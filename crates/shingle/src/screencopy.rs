//! wlr-screencopy-unstable-v1: output captures for `grim`,
//! xdg-desktop-portal-wlr and friends.
//!
//! A capture names an output, and a region capture a rectangle in that
//! output's logical coordinates, clipped to it. The client's buffer is in
//! the output's buffer pixels: scaled by its scale and turned back by its
//! transform, as wlroots' screencopy gives it (`capture_output_region` in
//! types/wlr_screencopy_v1.c), so `grim -o NAME` of a scale-2 output gets
//! twice as many pixels each way (docs/design/multi-output.md, section 8).
//! The cursor is drawn in only when the client asks for it with
//! `overlay_cursor`. `copy_with_damage` waits until the output changed
//! since that client's last capture of it, and reports the damage, as
//! wlroots does; the copying itself is shared with ext-image-copy-capture
//! in crates/shingle/src/capture.rs.

use std::collections::HashMap;

use shingle_core::Rect;
use wayland_protocols_wlr::screencopy::v1::server::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ObjectId,
  protocol::{wl_buffer::WlBuffer, wl_output, wl_shm},
};

use crate::{
  capture::{
    self, CaptureFrame, CaptureGeometry, Copy, DamageKey, Failure, Target,
  },
  state::CompositorState,
};

struct Frame {
  target: Target,
  geometry: CaptureGeometry,
  cursor: bool,
  used: bool,
}

#[derive(Default)]
pub(crate) struct Screencopy {
  frames: HashMap<ObjectId, Frame>,
}

impl Screencopy {
  /// How many frames are tracked.
  pub(crate) fn frames(&self) -> usize {
    self.frames.len()
  }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrScreencopyManagerV1, ()>(3, ());
}

/// Creates a frame capturing `region` (output-local logical pixels, the
/// whole output for `None`) of `output`, with the cursor if `cursor`, and
/// tells the client its buffer; it fails at once for an output that is
/// gone or a region that misses it.
fn create_frame(
  state: &mut CompositorState,
  frame: New<ZwlrScreencopyFrameV1>,
  data_init: &mut DataInit<'_, CompositorState>,
  output: &wl_output::WlOutput,
  region: Option<Rect>,
  cursor: bool,
) {
  let frame = data_init.init(frame, ());
  let Some((id, geometry)) =
    state.outputs.of_resource(output).and_then(|output| {
      CaptureGeometry::of_output(output, region)
        .map(|geometry| (output.id.clone(), geometry))
    })
  else {
    frame.failed();
    return;
  };
  let (width, height) = (geometry.buffer.width, geometry.buffer.height);
  frame.buffer(
    wl_shm::Format::Argb8888,
    width,
    height,
    width.saturating_mul(4),
  );
  if frame.version() >= 3 {
    // xdg-desktop-portal-wlr 0.7 fails a version 3 frame without a
    // DMA-BUF format.
    if let Some((fourcc, _)) = state
      .captures
      .dmabuf
      .as_ref()
      .and_then(|dmabuf| dmabuf.formats.first())
    {
      frame.linux_dmabuf(*fourcc, width, height);
    }
    frame.buffer_done();
  }
  state.screencopy.frames.insert(
    frame.id(),
    Frame {
      target: Target::Output(id, region),
      geometry,
      cursor,
      used: false,
    },
  );
}

fn copy_frame(
  state: &mut CompositorState,
  client: &Client,
  resource: &ZwlrScreencopyFrameV1,
  buffer: &WlBuffer,
  with_damage: bool,
) {
  let Some(frame) = state.screencopy.frames.get_mut(&resource.id()) else {
    return;
  };
  if frame.used {
    resource.post_error(
      zwlr_screencopy_frame_v1::Error::AlreadyUsed,
      "frame has already been copied",
    );
    return;
  }
  frame.used = true;
  let Some(attachment) = state.buffers.get(&buffer.id()).cloned() else {
    resource.failed();
    return;
  };
  if attachment.width != frame.geometry.buffer.width
    || attachment.height != frame.geometry.buffer.height
  {
    resource.post_error(
      zwlr_screencopy_frame_v1::Error::InvalidBuffer,
      "frame buffer dimensions do not match the capture",
    );
    return;
  }
  match attachment.shm_format() {
    Some(format) if crate::buffer::CAPTURE_SHM_FORMATS.contains(&format) => {}
    Some(_) => {
      resource.post_error(
        zwlr_screencopy_frame_v1::Error::InvalidBuffer,
        "frame buffer format does not match the capture",
      );
      return;
    }
    None => {
      // Only the format `linux_dmabuf` named, with a modifier the
      // renderer draws into.
      let announced = state.captures.dmabuf.as_ref().is_some_and(|dmabuf| {
        attachment.dmabuf_buffer().is_some_and(|buffer| {
          dmabuf.accepts(buffer)
            && dmabuf.formats.first().map(|(fourcc, _)| *fourcc)
              == Some(buffer.format)
        })
      });
      if !announced {
        CaptureFrame::Screencopy {
          frame: resource.clone(),
          with_damage,
        }
        .fail(Failure::BufferConstraints);
        return;
      }
    }
  }
  let (target, geometry, cursor) =
    (frame.target.clone(), frame.geometry, frame.cursor);
  let Target::Output(output, _) = &target else {
    return;
  };
  if state.outputs.get(output).is_none() {
    resource.failed();
    return;
  }
  let key = with_damage
    .then(|| DamageKey::Screencopy(client.id(), output.clone(), cursor));
  capture::start(
    state,
    Copy {
      frame: CaptureFrame::Screencopy {
        frame: resource.clone(),
        with_damage,
      },
      buffer: buffer.clone(),
      attachment,
      geometry,
      target,
      cursor,
      key,
    },
  );
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for CompositorState {
  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwlrScreencopyManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }

  fn can_view(client: Client, _: &()) -> bool {
    crate::security_context::can_view(client, "zwlr_screencopy_manager_v1")
  }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwlrScreencopyManagerV1,
    request: zwlr_screencopy_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_screencopy_manager_v1::Request::CaptureOutput {
        frame,
        overlay_cursor,
        output,
      } => {
        let cursor = overlay_cursor != 0;
        create_frame(state, frame, data_init, &output, None, cursor);
      }
      zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
        frame,
        overlay_cursor,
        output,
        x,
        y,
        width,
        height,
      } => {
        let region = Rect::new(x, y, width.max(0) as u32, height.max(0) as u32);
        let cursor = overlay_cursor != 0;
        create_frame(state, frame, data_init, &output, Some(region), cursor);
      }
      zwlr_screencopy_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    resource: &ZwlrScreencopyFrameV1,
    request: zwlr_screencopy_frame_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
        copy_frame(state, client, resource, &buffer, false);
      }
      zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
        copy_frame(state, client, resource, &buffer, true);
      }
      zwlr_screencopy_frame_v1::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZwlrScreencopyFrameV1,
    _data: &(),
  ) {
    let id = resource.id();
    state.screencopy.frames.remove(&id);
    capture::forget_frame(state, &id);
  }
}

#[cfg(test)]
mod tests {
  use std::os::fd::{AsFd, OwnedFd};

  use wayland_client::protocol::{wl_buffer, wl_shm};
  use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{Event, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
  };

  use crate::{
    CaptureId, CaptureRequest, CaptureSource, Compositor,
    Event as CompositorEvent, test_client::TestClient, tests::test_compositor,
  };

  /// A client with a screencopy manager.
  struct Capturer {
    client: TestClient,
    manager: ZwlrScreencopyManagerV1,
  }

  impl Capturer {
    fn new(compositor: &mut Compositor, socket: &std::path::Path) -> Self {
      let client = TestClient::connect(compositor, socket);
      let manager = client.bind::<ZwlrScreencopyManagerV1>(3);
      Self { client, manager }
    }

    /// Captures a region of the output and copies it into a new shm
    /// buffer of the advertised size. Returns the frame, its buffer and
    /// the memfd behind it.
    fn copy(
      &mut self,
      compositor: &mut Compositor,
      region: (i32, i32, i32, i32),
    ) -> (ZwlrScreencopyFrameV1, wl_buffer::WlBuffer, OwnedFd) {
      self.copy_with(compositor, region, false)
    }

    /// [`Capturer::copy`] with `copy_with_damage` if `with_damage`.
    fn copy_with(
      &mut self,
      compositor: &mut Compositor,
      region: (i32, i32, i32, i32),
      with_damage: bool,
    ) -> (ZwlrScreencopyFrameV1, wl_buffer::WlBuffer, OwnedFd) {
      let (x, y, width, height) = region;
      let output = self
        .client
        .bind::<wayland_client::protocol::wl_output::WlOutput>(1);
      let frame = self.manager.capture_output_region(
        0,
        &output,
        x,
        y,
        width,
        height,
        &self.client.handle,
        (),
      );
      self.client.roundtrip(compositor);
      let size = width * height * 4;
      let fd = rustix::fs::memfd_create(
        "shingle-screencopy-test",
        rustix::fs::MemfdFlags::CLOEXEC,
      )
      .expect("memfd_create succeeds");
      rustix::fs::ftruncate(&fd, size as u64).expect("ftruncate succeeds");
      let shm = self.client.bind::<wl_shm::WlShm>(1);
      let pool = shm.create_pool(fd.as_fd(), size, &self.client.handle, ());
      let buffer = pool.create_buffer(
        0,
        width,
        height,
        width * 4,
        wl_shm::Format::Argb8888,
        &self.client.handle,
        (),
      );
      if with_damage {
        frame.copy_with_damage(&buffer);
      } else {
        frame.copy(&buffer);
      }
      self.client.roundtrip(compositor);
      (frame, buffer, fd)
    }

    /// Whether the frame events so far end with `ready`, `failed` or
    /// neither.
    fn outcome(&self) -> Option<&'static str> {
      self
        .client
        .events
        .screencopy
        .iter()
        .rev()
        .find_map(|event| match event {
          Event::Ready { .. } => Some("ready"),
          Event::Failed => Some("failed"),
          _ => None,
        })
    }
  }

  /// The capture requests among the compositor's events.
  fn requests(
    compositor: &mut Compositor,
  ) -> Vec<(CaptureId, shingle_core::Rect)> {
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        CompositorEvent::CaptureRequested(CaptureRequest {
          id,
          source: CaptureSource::Output { region, .. },
          ..
        }) => Some((id, region)),
        _ => None,
      })
      .collect()
  }

  fn read(fd: &OwnedFd, length: usize) -> Vec<u8> {
    let mut bytes = vec![0; length];
    let read = rustix::io::pread(fd, &mut bytes, 0).expect("pread");
    bytes.truncate(read);
    bytes
  }

  #[test]
  fn without_frame_capture_the_cpu_composite_answers_at_once() {
    let (mut compositor, socket) = test_compositor();
    let mut capturer = Capturer::new(&mut compositor, &socket);
    let (_frame, _buffer, fd) = capturer.copy(&mut compositor, (0, 0, 2, 1));
    assert_eq!(capturer.outcome(), Some("ready"));
    assert!(requests(&mut compositor).is_empty());
    // The default background, 0.1, is the byte 26, as Wayfire writes it,
    // in BGRA.
    assert_eq!(read(&fd, 8), [26, 26, 26, 255, 26, 26, 26, 255]);
  }

  #[test]
  fn frame_capture_waits_for_the_frontend_and_writes_its_pixels() {
    let (mut compositor, socket) = test_compositor();
    compositor.set_frame_capture(true);
    let mut capturer = Capturer::new(&mut compositor, &socket);
    let (_frame, _buffer, fd) = capturer.copy(&mut compositor, (10, 20, 2, 1));
    assert_eq!(capturer.outcome(), None, "nothing before the frame");
    let requests = requests(&mut compositor);
    assert_eq!(requests.len(), 1);
    let (id, region) = requests[0];
    assert_eq!(region, shingle_core::Rect::new(10, 20, 2, 1));
    compositor.finish_capture(id, Ok(vec![255, 0, 0, 255, 0, 0, 255, 128]));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(capturer.outcome(), Some("ready"));
    // RGBA in, ARGB8888 (BGRA bytes) out.
    assert_eq!(read(&fd, 8), [0, 0, 255, 255, 255, 0, 0, 128]);
    // A second answer to the same capture changes nothing.
    compositor.finish_capture(id, Err("again".into()));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(
      capturer
        .client
        .events
        .screencopy
        .iter()
        .filter(|event| matches!(event, Event::Ready { .. } | Event::Failed))
        .count(),
      1
    );
  }

  #[test]
  fn a_failed_or_misfit_frame_fails_the_capture() {
    let (mut compositor, socket) = test_compositor();
    compositor.set_frame_capture(true);
    let mut capturer = Capturer::new(&mut compositor, &socket);
    let _first = capturer.copy(&mut compositor, (0, 0, 2, 2));
    let (id, _) = requests(&mut compositor)[0];
    compositor.finish_capture(id, Err("the GPU timed out".into()));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(capturer.outcome(), Some("failed"));
    capturer.client.events.screencopy.clear();
    let _second = capturer.copy(&mut compositor, (0, 0, 2, 2));
    let (id, _) = requests(&mut compositor)[0];
    compositor.finish_capture(id, Ok(vec![0; 4]));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(capturer.outcome(), Some("failed"), "4 bytes for 2x2");
  }

  #[test]
  fn a_destroyed_frame_or_buffer_is_not_answered_with_pixels() {
    let (mut compositor, socket) = test_compositor();
    compositor.set_frame_capture(true);
    let mut capturer = Capturer::new(&mut compositor, &socket);
    let (frame, _buffer, _fd) = capturer.copy(&mut compositor, (0, 0, 1, 1));
    let (gone, _) = requests(&mut compositor)[0];
    frame.destroy();
    capturer.client.roundtrip(&mut compositor);
    compositor.finish_capture(gone, Ok(vec![0; 4]));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(capturer.outcome(), None);
    let (_frame, buffer, _fd) = capturer.copy(&mut compositor, (0, 0, 1, 1));
    let (id, _) = requests(&mut compositor)[0];
    buffer.destroy();
    capturer.client.roundtrip(&mut compositor);
    compositor.finish_capture(id, Ok(vec![0; 4]));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(capturer.outcome(), Some("failed"));
  }

  /// The names of the frame events so far, `damage` included.
  fn names(capturer: &Capturer) -> Vec<&'static str> {
    capturer
      .client
      .events
      .screencopy
      .iter()
      .filter_map(|event| match event {
        Event::Damage { .. } => Some("damage"),
        Event::Ready { .. } => Some("ready"),
        Event::Failed => Some("failed"),
        _ => None,
      })
      .collect()
  }

  #[test]
  fn copy_with_damage_waits_for_a_change_after_the_first_copy() {
    let (mut compositor, socket) = test_compositor();
    let mut capturer = Capturer::new(&mut compositor, &socket);
    let _first = capturer.copy_with(&mut compositor, (0, 0, 4, 4), true);
    assert_eq!(names(&capturer), ["damage", "ready"], "the first at once");
    capturer.client.events.screencopy.clear();
    let _second = capturer.copy_with(&mut compositor, (0, 0, 4, 4), true);
    assert!(names(&capturer).is_empty(), "nothing changed");
    // A plain copy never waits.
    let _plain = capturer.copy(&mut compositor, (0, 0, 4, 4));
    assert_eq!(names(&capturer), ["ready"]);
    capturer.client.events.screencopy.clear();
    let mut other = TestClient::connect(&mut compositor, &socket);
    let toplevel = other.toplevel(&mut compositor, "damage");
    other.attach_buffer(&mut compositor, &toplevel.surface, 8, 8);
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(names(&capturer), ["damage", "ready"]);
  }

  #[test]
  fn with_frame_capture_damage_comes_from_the_frontend() {
    let (mut compositor, socket) = test_compositor();
    compositor.set_frame_capture(true);
    let mut capturer = Capturer::new(&mut compositor, &socket);
    let _first = capturer.copy_with(&mut compositor, (0, 0, 2, 1), true);
    let (id, _) = requests(&mut compositor)[0];
    compositor.finish_capture(id, Ok(vec![0; 8]));
    capturer.client.roundtrip(&mut compositor);
    capturer.client.events.screencopy.clear();
    let _second = capturer.copy_with(&mut compositor, (0, 0, 2, 1), true);
    assert!(requests(&mut compositor).is_empty(), "it waits");
    let output = crate::OutputHandle::new(crate::OUTPUT_NAME);
    let released = compositor.output_damaged(&output);
    assert_eq!(released.len(), 1);
    assert!(!released[0].cursor, "overlay_cursor was 0");
    compositor.finish_capture(released[0].id, Ok(vec![0; 8]));
    capturer.client.roundtrip(&mut compositor);
    assert_eq!(names(&capturer), ["damage", "ready"]);
    // Damage of another output, or none, releases nothing.
    let _third = capturer.copy_with(&mut compositor, (0, 0, 2, 1), true);
    let other = crate::OutputHandle::new("HEADLESS-9");
    assert!(compositor.output_damaged(&other).is_empty());
  }
}
