//! wlr-screencopy-unstable-v1: output captures for `grim` and friends.
//!
//! Without a renderer the compositor composites the visible surfaces'
//! CPU pixels itself. A frontend that renders turns on frame capture
//! ([`crate::Compositor::set_frame_capture`]): `copy` then emits
//! [`crate::Event::CaptureRequested`], the frontend draws the frame as it
//! shows it, and [`crate::Compositor::finish_capture`] writes it into the
//! client's buffer (docs/design/dmabuf.md, section 6).

use std::collections::HashMap;

use shingle_core::Rect;
use wayland_protocols_wlr::screencopy::v1::server::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ObjectId,
  protocol::{wl_output, wl_shm},
};

use crate::{buffer::Attachment, state::CompositorState};

/// Identifies a frame capture between [`crate::Event::CaptureRequested`]
/// and [`crate::Compositor::finish_capture`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureId(pub u64);

struct Frame {
  region: Rect,
  used: bool,
}

/// A copy waiting for the frontend's frame.
struct PendingCapture {
  frame: ZwlrScreencopyFrameV1,
  buffer: wayland_server::protocol::wl_buffer::WlBuffer,
  attachment: Attachment,
}

#[derive(Default)]
pub(crate) struct Screencopy {
  frames: HashMap<ObjectId, Frame>,
  /// Whether copies go to the frontend instead of the CPU composite.
  pub(crate) frame_capture: bool,
  captures: HashMap<CaptureId, PendingCapture>,
  next_capture: u64,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrScreencopyManagerV1, ()>(3, ());
}

fn create_frame(
  state: &mut CompositorState,
  frame: New<ZwlrScreencopyFrameV1>,
  data_init: &mut DataInit<'_, CompositorState>,
  region: Rect,
) {
  let frame = data_init.init(frame, ());
  if region.width == 0 || region.height == 0 {
    frame.failed();
    return;
  }
  frame.buffer(
    wl_shm::Format::Argb8888,
    region.width,
    region.height,
    region.width.saturating_mul(4),
  );
  if frame.version() >= 3 {
    frame.buffer_done();
  }
  state.screencopy.frames.insert(
    frame.id(),
    Frame {
      region,
      used: false,
    },
  );
}

fn copy_frame(
  state: &mut CompositorState,
  resource: &ZwlrScreencopyFrameV1,
  buffer: &wayland_server::protocol::wl_buffer::WlBuffer,
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
  if attachment.width != frame.region.width
    || attachment.height != frame.region.height
  {
    resource.post_error(
      zwlr_screencopy_frame_v1::Error::InvalidBuffer,
      "frame buffer dimensions do not match the capture",
    );
    return;
  }
  let region = frame.region;
  if state.screencopy.frame_capture {
    let id = CaptureId(state.screencopy.next_capture);
    state.screencopy.next_capture += 1;
    state.screencopy.captures.insert(
      id,
      PendingCapture {
        frame: resource.clone(),
        buffer: buffer.clone(),
        attachment,
      },
    );
    state
      .events
      .push(crate::Event::CaptureRequested { id, region });
    return;
  }
  let rgba = composite(state, region);
  answer(state, resource, &attachment, Ok(rgba));
}

/// Answers the capture `id` with the frontend's `result`: tightly packed
/// RGBA rows of the region, or why there are none. Captures whose frame
/// or buffer is gone, or that were answered already, are ignored or
/// failed.
pub(crate) fn finish(
  state: &mut CompositorState,
  id: CaptureId,
  result: Result<Vec<u8>, String>,
) {
  let Some(capture) = state.screencopy.captures.remove(&id) else {
    return;
  };
  if !capture.frame.is_alive() {
    return;
  }
  if !capture.buffer.is_alive() {
    capture.frame.failed();
    return;
  }
  answer(state, &capture.frame, &capture.attachment, result);
}

/// Writes `rgba` into `attachment` and sends `ready`, or `failed` when
/// there are no pixels or they do not fit.
fn answer(
  state: &CompositorState,
  resource: &ZwlrScreencopyFrameV1,
  attachment: &Attachment,
  rgba: Result<Vec<u8>, String>,
) {
  match rgba.and_then(|rgba| attachment.write_rgba(&rgba)) {
    Ok(()) => {
      let now = state.now();
      let seconds = now.as_secs();
      resource.flags(zwlr_screencopy_frame_v1::Flags::empty());
      resource.ready(
        (seconds >> 32) as u32,
        seconds as u32,
        now.subsec_nanos(),
      );
    }
    Err(error) => {
      eprintln!("shingle: screencopy failed: {error}");
      resource.failed();
    }
  }
}

fn composite(state: &CompositorState, region: Rect) -> Vec<u8> {
  let background = [
    srgb_channel(state.background_color[0]),
    srgb_channel(state.background_color[1]),
    srgb_channel(state.background_color[2]),
    255,
  ];
  let mut rgba = vec![0; region.width as usize * region.height as usize * 4];
  for pixel in rgba.as_chunks_mut::<4>().0 {
    *pixel = background;
  }
  for surface in state.visible_surfaces() {
    let Some(buffer) = surface.buffer else {
      continue;
    };
    // Content without CPU pixels is left to the frontend's frame capture.
    let Some(pixels) = buffer.rgba() else {
      continue;
    };
    let opacity = (surface.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
    if opacity == 0 {
      continue;
    }
    let intersection = intersection(region, surface.geometry);
    let Some(intersection) = intersection else {
      continue;
    };
    for y in 0..intersection.height {
      for x in 0..intersection.width {
        let Some(source_index) = sample_index(
          &buffer,
          surface.geometry,
          f64::from(intersection.x) + f64::from(x),
          f64::from(intersection.y) + f64::from(y),
        ) else {
          continue;
        };
        let source = &pixels[source_index..source_index + 4];
        let destination_x = intersection.x - region.x + x as i32;
        let destination_y = intersection.y - region.y + y as i32;
        let destination_index = (destination_y as usize
          * region.width as usize
          + destination_x as usize)
          * 4;
        let destination = &mut rgba[destination_index..destination_index + 4];
        let alpha = (u32::from(source[3]) * opacity + 127) / 255;
        if alpha == 255 && opacity == 255 {
          destination.copy_from_slice(source);
        } else if alpha > 0 {
          for channel in 0..3 {
            destination[channel] = ((u32::from(source[channel]) * opacity
              + u32::from(destination[channel]) * (255 - alpha)
              + 127)
              / 255)
              .min(255) as u8;
          }
          destination[3] = 255;
        }
      }
    }
  }
  rgba
}

fn sample_index(
  buffer: &crate::BufferSnapshot,
  geometry: Rect,
  x: f64,
  y: f64,
) -> Option<usize> {
  if geometry.width == 0
    || geometry.height == 0
    || buffer.width == 0
    || buffer.height == 0
  {
    return None;
  }
  let u = (x - f64::from(geometry.x) + 0.5) / f64::from(geometry.width);
  let v = (y - f64::from(geometry.y) + 0.5) / f64::from(geometry.height);
  if !u.is_finite()
    || !v.is_finite()
    || !(0.0..1.0).contains(&u)
    || !(0.0..1.0).contains(&v)
  {
    return None;
  }
  let (u, v) = match buffer.transform {
    wl_output::Transform::Normal => (u, v),
    wl_output::Transform::Flipped => (1.0 - u, v),
    wl_output::Transform::_90 => (v, 1.0 - u),
    wl_output::Transform::_180 => (1.0 - u, 1.0 - v),
    wl_output::Transform::_270 => (1.0 - v, u),
    wl_output::Transform::Flipped90 => (v, u),
    wl_output::Transform::Flipped180 => (u, 1.0 - v),
    wl_output::Transform::Flipped270 => (1.0 - v, 1.0 - u),
    _ => (u, v),
  };
  let source = buffer.source.unwrap_or([
    0.0,
    0.0,
    f64::from(buffer.width),
    f64::from(buffer.height),
  ]);
  let source_x = (source[0] + u * source[2]).floor();
  let source_y = (source[1] + v * source[3]).floor();
  if !source_x.is_finite()
    || !source_y.is_finite()
    || source_x < 0.0
    || source_y < 0.0
    || source_x >= f64::from(buffer.width)
    || source_y >= f64::from(buffer.height)
  {
    return None;
  }
  Some((source_y as usize * buffer.width as usize + source_x as usize) * 4)
}

fn srgb_channel(value: f64) -> u8 {
  let value = value.clamp(0.0, 1.0);
  let encoded = if value <= 0.0031308 {
    value * 12.92
  } else {
    1.055 * value.powf(1.0 / 2.4) - 0.055
  };
  (encoded * 255.0).round() as u8
}

fn intersection(first: Rect, second: Rect) -> Option<Rect> {
  let left = first.x.max(second.x);
  let top = first.y.max(second.y);
  let right = (first.x.saturating_add(first.width as i32))
    .min(second.x.saturating_add(second.width as i32));
  let bottom = (first.y.saturating_add(first.height as i32))
    .min(second.y.saturating_add(second.height as i32));
  if right <= left || bottom <= top {
    return None;
  }
  Some(Rect::new(
    left,
    top,
    (right - left) as u32,
    (bottom - top) as u32,
  ))
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
      zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, .. } => {
        create_frame(state, frame, data_init, state.primary().rect);
      }
      zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
        frame,
        x,
        y,
        width,
        height,
        ..
      } => {
        let region = Rect::new(x, y, width.max(0) as u32, height.max(0) as u32);
        let region = intersection(state.primary().rect, region)
          .unwrap_or(Rect::new(x, y, 0, 0));
        create_frame(state, frame, data_init, region);
      }
      zwlr_screencopy_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrScreencopyFrameV1,
    request: zwlr_screencopy_frame_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_screencopy_frame_v1::Request::Copy { buffer }
      | zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
        copy_frame(state, resource, &buffer);
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
    state
      .screencopy
      .captures
      .retain(|_, capture| capture.frame.id() != id);
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
    CaptureId, Compositor, Event as CompositorEvent, test_client::TestClient,
    tests::test_compositor,
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
      frame.copy(&buffer);
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
        CompositorEvent::CaptureRequested { id, region } => Some((id, region)),
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
    // The default background, 0.1 linear, is 89 in sRGB, as BGRA.
    assert_eq!(read(&fd, 8), [89, 89, 89, 255, 89, 89, 89, 255]);
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
}
