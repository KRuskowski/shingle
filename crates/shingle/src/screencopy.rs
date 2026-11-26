//! wlr-screencopy-unstable-v1: output captures for `grim` and friends.
//!
//! A capture names an output, and a region capture a rectangle in that
//! output's logical coordinates, clipped to it. The client's buffer is in
//! the output's buffer pixels: scaled by its scale and turned back by its
//! transform, as wlroots' screencopy gives it (`capture_output_region` in
//! types/wlr_screencopy_v1.c), so `grim -o NAME` of a scale-2 output gets
//! twice as many pixels each way (docs/design/multi-output.md, section 8).
//!
//! Without a renderer the compositor composites the visible surfaces'
//! CPU pixels of that output itself. A frontend that renders turns on
//! frame capture ([`crate::Compositor::set_frame_capture`]): `copy` then
//! emits [`crate::Event::CaptureRequested`], the frontend draws the
//! output's frame as it shows it and answers with the region in logical
//! pixels, and [`crate::Compositor::finish_capture`] scales it into the
//! client's buffer (docs/design/dmabuf.md, section 6).

use std::collections::HashMap;

use shingle_core::{OutputId, Rect};
use wayland_protocols_wlr::screencopy::v1::server::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ObjectId,
  protocol::{wl_output, wl_shm},
};

use crate::{
  buffer::Attachment, output::OutputState, state::CompositorState,
  surface_outputs::intersection,
};

/// Identifies a frame capture between [`crate::Event::CaptureRequested`]
/// and [`crate::Compositor::finish_capture`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureId(pub u64);

/// Where the pixels of a capture's buffer are: the output, the captured
/// part in its logical and in its buffer pixels, and how the two map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureGeometry {
  /// The output's top-left corner in the layout.
  origin: (i32, i32),
  /// The output's logical size, after the transform.
  logical: (u32, u32),
  /// The output's size in buffer pixels, before the transform.
  physical: (u32, u32),
  /// The output's transform.
  transform: wl_output::Transform,
  /// The captured part, in output-local logical pixels.
  pub(crate) region: Rect,
  /// The captured part, in the output's buffer pixels; the client's
  /// buffer has its size.
  pub(crate) buffer: Rect,
}

impl CaptureGeometry {
  /// The capture of `region` (output-local logical pixels, the whole
  /// output for `None`) of an output at `origin` with logical size
  /// `logical`, `physical` buffer pixels and `transform`; `None` when the
  /// region misses the output.
  pub(crate) fn new(
    origin: (i32, i32),
    logical: (u32, u32),
    physical: (u32, u32),
    transform: wl_output::Transform,
    region: Option<Rect>,
  ) -> Option<Self> {
    let whole = Rect::new(0, 0, logical.0, logical.1);
    let region = match region {
      Some(region) => intersection(whole, region)?,
      None => whole,
    };
    let mut geometry = Self {
      origin,
      logical,
      physical,
      transform,
      region,
      buffer: Rect::new(0, 0, 0, 0),
    };
    // The region's corners in the transformed buffer space, then turned
    // back into the buffer, as wlr_box_transform does.
    let (scale_x, scale_y) = geometry.scales();
    let corners = [
      (f64::from(region.x), f64::from(region.y)),
      (
        f64::from(region.x) + f64::from(region.width),
        f64::from(region.y) + f64::from(region.height),
      ),
    ]
    .map(|(x, y)| {
      let (width, height) = geometry.transformed();
      let (x, y) = crate::output_transform::unmap_point(
        (x * scale_x).round(),
        (y * scale_y).round(),
        physical.0,
        physical.1,
        width,
        height,
        transform,
      );
      (x.round() as i64, y.round() as i64)
    });
    let left = corners[0].0.min(corners[1].0).max(0);
    let top = corners[0].1.min(corners[1].1).max(0);
    let right = corners[0].0.max(corners[1].0).min(i64::from(physical.0));
    let bottom = corners[0].1.max(corners[1].1).min(i64::from(physical.1));
    if right <= left || bottom <= top {
      return None;
    }
    geometry.buffer = Rect::new(
      left as i32,
      top as i32,
      (right - left) as u32,
      (bottom - top) as u32,
    );
    Some(geometry)
  }

  /// The capture of `region` of `output`.
  fn of_output(output: &OutputState, region: Option<Rect>) -> Option<Self> {
    Self::new(
      (output.rect.x, output.rect.y),
      (output.rect.width, output.rect.height),
      output.physical_size(),
      output.transform,
      region,
    )
  }

  /// The buffer size after the transform, which the logical size scales
  /// to.
  fn transformed(&self) -> (u32, u32) {
    if crate::output_transform::is_quarter_turn(self.transform) {
      (self.physical.1, self.physical.0)
    } else {
      self.physical
    }
  }

  /// Buffer pixels per logical pixel, each way.
  fn scales(&self) -> (f64, f64) {
    let (width, height) = self.transformed();
    (
      f64::from(width) / f64::from(self.logical.0.max(1)),
      f64::from(height) / f64::from(self.logical.1.max(1)),
    )
  }

  /// The output-local logical point at the centre of pixel (`x`, `y`) of
  /// the capture's buffer.
  pub(crate) fn local_point(&self, x: u32, y: u32) -> (f64, f64) {
    let (width, height) = self.transformed();
    let (x, y) = crate::output_transform::map_point(
      f64::from(self.buffer.x) + f64::from(x) + 0.5,
      f64::from(self.buffer.y) + f64::from(y) + 0.5,
      self.physical.0,
      self.physical.1,
      width,
      height,
      self.transform,
    );
    let (scale_x, scale_y) = self.scales();
    (x / scale_x, y / scale_y)
  }

  /// The layout point at the centre of pixel (`x`, `y`) of the buffer.
  fn layout_point(&self, x: u32, y: u32) -> (f64, f64) {
    let (x, y) = self.local_point(x, y);
    (x + f64::from(self.origin.0), y + f64::from(self.origin.1))
  }

  /// RGBA rows of the buffer, each pixel taken from `pixel` at its
  /// layout point.
  fn fill(&self, mut pixel: impl FnMut(f64, f64) -> [u8; 4]) -> Vec<u8> {
    let (width, height) = (self.buffer.width, self.buffer.height);
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
      for x in 0..width {
        let (layout_x, layout_y) = self.layout_point(x, y);
        rgba.extend_from_slice(&pixel(layout_x, layout_y));
      }
    }
    rgba
  }

  /// The buffer's RGBA rows from `region`, the captured region's RGBA rows
  /// in logical pixels, as a frontend draws them: each buffer pixel takes
  /// the logical pixel it lies in; pixels outside are transparent black.
  pub(crate) fn scale_logical(&self, region: &[u8]) -> Result<Vec<u8>, String> {
    let (width, height) = (self.region.width, self.region.height);
    if region.len() != width as usize * height as usize * 4 {
      return Err(format!(
        "{} bytes for a {width}x{height} capture",
        region.len()
      ));
    }
    let mut rgba = Vec::with_capacity(
      self.buffer.width as usize * self.buffer.height as usize * 4,
    );
    for y in 0..self.buffer.height {
      for x in 0..self.buffer.width {
        let (local_x, local_y) = self.local_point(x, y);
        let column = (local_x - f64::from(self.region.x)).floor();
        let row = (local_y - f64::from(self.region.y)).floor();
        if column < 0.0
          || row < 0.0
          || column >= f64::from(width)
          || row >= f64::from(height)
        {
          rgba.extend_from_slice(&[0; 4]);
          continue;
        }
        let index = (row as usize * width as usize + column as usize) * 4;
        rgba.extend_from_slice(&region[index..index + 4]);
      }
    }
    Ok(rgba)
  }
}

struct Frame {
  output: OutputId,
  geometry: CaptureGeometry,
  used: bool,
}

/// A copy waiting for the frontend's frame.
struct PendingCapture {
  frame: ZwlrScreencopyFrameV1,
  buffer: wayland_server::protocol::wl_buffer::WlBuffer,
  attachment: Attachment,
  geometry: CaptureGeometry,
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

/// Creates a frame capturing `region` (output-local logical pixels, the
/// whole output for `None`) of `output`, and tells the client its buffer;
/// it fails at once for an output that is gone or a region that misses
/// it.
fn create_frame(
  state: &mut CompositorState,
  frame: New<ZwlrScreencopyFrameV1>,
  data_init: &mut DataInit<'_, CompositorState>,
  output: &wl_output::WlOutput,
  region: Option<Rect>,
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
    frame.buffer_done();
  }
  state.screencopy.frames.insert(
    frame.id(),
    Frame {
      output: id,
      geometry,
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
  if attachment.width != frame.geometry.buffer.width
    || attachment.height != frame.geometry.buffer.height
  {
    resource.post_error(
      zwlr_screencopy_frame_v1::Error::InvalidBuffer,
      "frame buffer dimensions do not match the capture",
    );
    return;
  }
  let (output, geometry) = (frame.output.clone(), frame.geometry);
  if state.outputs.get(&output).is_none() {
    resource.failed();
    return;
  }
  if state.screencopy.frame_capture {
    let id = CaptureId(state.screencopy.next_capture);
    state.screencopy.next_capture += 1;
    state.screencopy.captures.insert(
      id,
      PendingCapture {
        frame: resource.clone(),
        buffer: buffer.clone(),
        attachment,
        geometry,
      },
    );
    state.events.push(crate::Event::CaptureRequested {
      id,
      output: crate::OutputHandle::from(&output),
      region: geometry.region,
    });
    return;
  }
  let rgba = composite(state, &output, &geometry);
  answer(state, resource, &attachment, Ok(rgba));
}

/// Answers the capture `id` with the frontend's `result`: tightly packed
/// RGBA rows of the region in logical pixels, which are scaled into the
/// buffer, or why there are none. Captures whose frame or buffer is gone,
/// or that were answered already, are ignored or failed.
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
  let result =
    result.and_then(|region| capture.geometry.scale_logical(&region));
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

/// The capture's buffer composited on the CPU from the surfaces drawn on
/// `output`, over the background colour.
fn composite(
  state: &CompositorState,
  output: &OutputId,
  geometry: &CaptureGeometry,
) -> Vec<u8> {
  let background = [
    color_byte(state.background_color[0]),
    color_byte(state.background_color[1]),
    color_byte(state.background_color[2]),
    255,
  ];
  let surfaces = state
    .visible_surfaces_on(output)
    .into_iter()
    .filter_map(|surface| {
      let buffer = surface.buffer?;
      // Content without CPU pixels is left to the frontend's capture.
      let pixels = buffer.rgba()?;
      let opacity = (surface.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
      (opacity > 0).then_some((surface.geometry, buffer, pixels, opacity))
    })
    .collect::<Vec<_>>();
  geometry.fill(|x, y| {
    let mut destination = background;
    for (rect, buffer, pixels, opacity) in &surfaces {
      let Some(source_index) = sample_index(buffer, *rect, x, y) else {
        continue;
      };
      let source = &pixels[source_index..source_index + 4];
      let alpha = (u32::from(source[3]) * opacity + 127) / 255;
      if alpha == 255 && *opacity == 255 {
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
    destination
  })
}

/// The index in `buffer`'s RGBA pixels of the texel shown at layout point
/// (`x`, `y`) by a surface at `geometry`, if the point is on it.
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
  let u = (x - f64::from(geometry.x)) / f64::from(geometry.width);
  let v = (y - f64::from(geometry.y)) / f64::from(geometry.height);
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

/// The byte of configured colour component `value`, which Wayfire writes
/// as it is (crates/shingle-render/src/color.rs).
fn color_byte(value: f64) -> u8 {
  (value.clamp(0.0, 1.0) * 255.0).round() as u8
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
        output,
        ..
      } => {
        create_frame(state, frame, data_init, &output, None);
      }
      zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
        frame,
        output,
        x,
        y,
        width,
        height,
        ..
      } => {
        let region = Rect::new(x, y, width.max(0) as u32, height.max(0) as u32);
        create_frame(state, frame, data_init, &output, Some(region));
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
        CompositorEvent::CaptureRequested { id, region, .. } => {
          Some((id, region))
        }
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
}

#[cfg(test)]
mod geometry_tests {
  use shingle_core::Rect;
  use wayland_server::protocol::wl_output::Transform;

  use super::CaptureGeometry;

  #[test]
  fn a_region_is_scaled_into_the_buffer() {
    let geometry = CaptureGeometry::new(
      (1280, 0),
      (640, 360),
      (1280, 720),
      Transform::Normal,
      Some(Rect::new(10, 20, 3, 2)),
    )
    .unwrap();
    assert_eq!(geometry.buffer, Rect::new(20, 40, 6, 4));
    let near = |(x, y): (f64, f64), (expected_x, expected_y): (f64, f64)| {
      (x - expected_x).abs() < 1e-9 && (y - expected_y).abs() < 1e-9
    };
    assert!(near(geometry.local_point(0, 0), (10.25, 20.25)));
    assert!(near(geometry.layout_point(5, 3), (1292.75, 21.75)));
    // Each logical pixel becomes two by two buffer pixels.
    let region = (0..6)
      .flat_map(|value| [value, 0, 0, 255])
      .collect::<Vec<_>>();
    let rgba = geometry.scale_logical(&region).unwrap();
    assert_eq!(rgba.len(), 6 * 4 * 4);
    assert_eq!(rgba[..8], [0, 0, 0, 255, 0, 0, 0, 255]);
    assert_eq!(rgba[8..12], [1, 0, 0, 255]);
    assert_eq!(rgba[6 * 4 * 2..6 * 4 * 2 + 4], [3, 0, 0, 255]);
    assert!(geometry.scale_logical(&[0; 4]).is_err());
  }

  #[test]
  fn a_turned_output_gives_a_buffer_turned_back() {
    // A 1280x720 panel turned by 90 degrees is 720x1280 in the layout.
    let whole = CaptureGeometry::new(
      (0, 0),
      (720, 1280),
      (1280, 720),
      Transform::_90,
      None,
    )
    .unwrap();
    assert_eq!(whole.buffer, Rect::new(0, 0, 1280, 720));
    let strip = CaptureGeometry::new(
      (0, 0),
      (720, 1280),
      (1280, 720),
      Transform::_90,
      Some(Rect::new(0, 0, 720, 10)),
    )
    .unwrap();
    assert_eq!((strip.buffer.width, strip.buffer.height), (10, 720));
    for (x, y) in [(0, 0), (9, 719), (5, 300)] {
      let (local_x, local_y) = strip.local_point(x, y);
      assert!((0.0..720.0).contains(&local_x), "{x},{y}: {local_x}");
      assert!((0.0..10.0).contains(&local_y), "{x},{y}: {local_y}");
    }
  }

  #[test]
  fn a_region_that_misses_the_output_has_no_capture() {
    let capture = |region| {
      CaptureGeometry::new(
        (0, 0),
        (100, 100),
        (100, 100),
        Transform::Normal,
        Some(region),
      )
    };
    assert!(capture(Rect::new(100, 0, 10, 10)).is_none());
    assert!(capture(Rect::new(0, 0, 0, 10)).is_none());
    assert_eq!(
      capture(Rect::new(-5, 95, 10, 10)).unwrap().region,
      Rect::new(0, 95, 5, 5)
    );
  }
}
