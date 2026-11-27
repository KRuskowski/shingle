//! Captures shared by wlr-screencopy (crates/shingle/src/screencopy.rs)
//! and ext-image-copy-capture (crates/shingle/src/image_copy_capture.rs).
//!
//! A capture copies an output, a region of one, or a single toplevel into
//! a client's buffer. The buffer is in the source's buffer pixels: an
//! output's scaled by its scale and turned back by its transform, as
//! wlroots gives it (`capture_output_region` in types/wlr_screencopy_v1.c),
//! and a toplevel's scaled by the scale of the output it is on.
//!
//! Without a renderer the compositor composites the visible surfaces'
//! CPU pixels itself. A frontend that renders turns on frame capture
//! ([`crate::Compositor::set_frame_capture`]): a capture then emits
//! [`crate::Event::CaptureRequested`], the frontend draws what it shows
//! and answers with the source in logical pixels, and
//! [`crate::Compositor::finish_capture`] scales it into the client's
//! buffer (docs/design/dmabuf.md, section 6).
//!
//! Captures that may wait for damage (`copy_with_damage`, and every
//! ext-image-copy-capture frame after a session's first) wait while their
//! damage tracker is clean: from the moment a capture of the tracker was
//! answered until the source is damaged. A frontend reports an output's
//! damage when it draws a frame of it for anything but a capture
//! ([`crate::Compositor::output_damaged`]), which answers the waiting
//! captures from that very frame; without one, any turn in which clients
//! changed something damages every source
//! (docs/design/screen-sharing.md).

use std::{
  collections::{BTreeMap, HashMap},
  sync::Arc,
};

use shingle_core::{OutputId, Rect, WindowId};
use wayland_protocols::ext::image_copy_capture::v1::server::{
  ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
};
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::{
  self, ZwlrScreencopyFrameV1,
};
use wayland_server::{
  Resource,
  backend::{ClientId, ObjectId},
  protocol::{wl_buffer::WlBuffer, wl_output},
};

use crate::{
  DmabufBuffer, OutputHandle, SurfaceRole, buffer::Attachment,
  output::OutputState, state::CompositorState, surface_outputs::intersection,
};

/// Identifies a frame capture between [`crate::Event::CaptureRequested`]
/// and [`crate::Compositor::finish_capture`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureId(pub u64);

/// What a capture shows, as the frontend draws it.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureSource {
  /// A region of an output, as the output shows it.
  Output {
    /// The captured output.
    output: OutputHandle,
    /// The captured rectangle in the output's logical pixels, with (0, 0)
    /// at its top-left corner.
    region: Rect,
  },
  /// One toplevel alone: its surfaces, subsurfaces and popups, drawn
  /// over transparent black, without anything above or below it.
  Window {
    /// The captured window.
    window: WindowId,
    /// Its toplevel surface's rectangle in layout logical pixels, which
    /// the capture covers; popups beyond it are cut off.
    rect: Rect,
  },
}

/// A capture for the frontend to draw and answer with
/// [`crate::Compositor::finish_capture`].
#[derive(Clone, Debug)]
pub struct CaptureRequest {
  /// The capture to answer.
  pub id: CaptureId,
  /// What it shows. The frontend answers with the source's logical
  /// pixels: the output region's, or the window rectangle's, which the
  /// compositor scales into the client's buffer of physical pixels.
  pub source: CaptureSource,
  /// Whether the cursor is drawn into it. Without, the frontend leaves
  /// the cursor out.
  pub cursor: bool,
  /// The client's DMA-BUF, for a capture into one: the frontend draws
  /// into it, in the source's physical pixels as its output is drawn, and
  /// answers with [`crate::Compositor::finish_capture_into`]. `None` for
  /// a shared-memory buffer, which the compositor fills.
  pub dmabuf: Option<Arc<DmabufBuffer>>,
}

/// The DMA-BUFs captures may be copied into: what the renderer can draw
/// into, given by the frontend with
/// [`crate::Compositor::set_capture_dmabuf`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureDmabuf {
  /// The render node, as a `dev_t`.
  pub device: u64,
  /// DRM fourccs with the modifiers each can be rendered to with, the
  /// preferred first.
  pub formats: Vec<(u32, Vec<u64>)>,
}

impl CaptureDmabuf {
  /// Whether `buffer` has one of the formats and modifiers.
  pub(crate) fn accepts(&self, buffer: &DmabufBuffer) -> bool {
    buffer.planes.len() == 1
      && self.formats.iter().any(|(fourcc, modifiers)| {
        *fourcc == buffer.format && modifiers.contains(&buffer.modifier)
      })
  }
}

impl CaptureRequest {
  /// The size in logical pixels the frontend answers with.
  pub fn size(&self) -> (u32, u32) {
    match &self.source {
      CaptureSource::Output { region, .. } => (region.width, region.height),
      CaptureSource::Window { rect, .. } => (rect.width, rect.height),
    }
  }
}

/// What damages a capture: an output, or a window.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Damaged {
  Output(OutputId),
  Window(WindowId),
}

/// Who tracks damage between captures: a screencopy client of one
/// output, as wlroots tracks it, or one ext-image-copy-capture session.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DamageKey {
  /// A client's captures of an output, with or without the cursor: a
  /// portal's streams of one output with either cursor mode come from one
  /// client, and each needs its own first frame.
  Screencopy(ClientId, OutputId, bool),
  Session(ObjectId),
}

/// Why a capture failed, as ext-image-copy-capture tells it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Failure {
  Unknown,
  BufferConstraints,
  Stopped,
}

/// The protocol object a capture answers.
#[derive(Clone, Debug)]
pub(crate) enum CaptureFrame {
  Screencopy {
    frame: ZwlrScreencopyFrameV1,
    with_damage: bool,
  },
  Ext {
    frame: ExtImageCopyCaptureFrameV1,
    transform: wl_output::Transform,
  },
}

impl CaptureFrame {
  pub(crate) fn id(&self) -> ObjectId {
    match self {
      Self::Screencopy { frame, .. } => frame.id(),
      Self::Ext { frame, .. } => frame.id(),
    }
  }

  fn is_alive(&self) -> bool {
    match self {
      Self::Screencopy { frame, .. } => frame.is_alive(),
      Self::Ext { frame, .. } => frame.is_alive(),
    }
  }

  pub(crate) fn fail(&self, reason: Failure) {
    match self {
      Self::Screencopy { frame, .. } => frame.failed(),
      Self::Ext { frame, .. } => frame.failed(match reason {
        Failure::Unknown => {
          ext_image_copy_capture_frame_v1::FailureReason::Unknown
        }
        Failure::BufferConstraints => {
          ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints
        }
        Failure::Stopped => {
          ext_image_copy_capture_frame_v1::FailureReason::Stopped
        }
      }),
    }
  }

  /// Sends the metadata and `ready` for a `width` by `height` buffer
  /// copied at `now`, with all of it damaged.
  fn ready(&self, now: std::time::Duration, (width, height): (u32, u32)) {
    let seconds = now.as_secs();
    let (high, low) = ((seconds >> 32) as u32, seconds as u32);
    let (width, height) = (width as i32, height as i32);
    match self {
      Self::Screencopy { frame, with_damage } => {
        frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
        if *with_damage && frame.version() >= 2 {
          frame.damage(0, 0, width as u32, height as u32);
        }
        frame.ready(high, low, now.subsec_nanos());
      }
      Self::Ext { frame, transform } => {
        frame.transform(*transform);
        frame.damage(0, 0, width, height);
        frame.presentation_time(high, low, now.subsec_nanos());
        frame.ready();
      }
    }
  }
}

/// Where the pixels of a capture's buffer are: the source's origin in the
/// layout, the captured part in its logical and in its buffer pixels, and
/// how the two map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CaptureGeometry {
  /// The source's top-left corner in the layout.
  origin: (i32, i32),
  /// The source's logical size, after the transform.
  logical: (u32, u32),
  /// The source's size in buffer pixels, before the transform.
  physical: (u32, u32),
  /// The source's transform.
  transform: wl_output::Transform,
  /// The captured part, in source-local logical pixels.
  pub(crate) region: Rect,
  /// The captured part, in the source's buffer pixels; the client's
  /// buffer has its size.
  pub(crate) buffer: Rect,
}

impl CaptureGeometry {
  /// The capture of `region` (source-local logical pixels, the whole
  /// source for `None`) of a source at `origin` with logical size
  /// `logical`, `physical` buffer pixels and `transform`; `None` when the
  /// region misses the source.
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
  pub(crate) fn of_output(
    output: &OutputState,
    region: Option<Rect>,
  ) -> Option<Self> {
    Self::new(
      (output.rect.x, output.rect.y),
      (output.rect.width, output.rect.height),
      output.physical_size(),
      output.transform,
      region,
    )
  }

  /// The capture of a window whose toplevel surface is at `rect`, on an
  /// output of `scale`.
  pub(crate) fn of_window(rect: Rect, scale: f64) -> Option<Self> {
    Self::new(
      (rect.x, rect.y),
      (rect.width, rect.height),
      window_buffer_size(rect, scale),
      wl_output::Transform::Normal,
      None,
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

  /// Whether each buffer pixel is the logical pixel at the same place,
  /// so that the frontend's pixels are the buffer's as they are.
  fn is_identity(&self) -> bool {
    self.transform == wl_output::Transform::Normal
      && self.physical == self.logical
      && self.buffer == self.region
  }

  /// The source-local logical point at the centre of pixel (`x`, `y`) of
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
  pub(crate) fn scale_logical(
    &self,
    region: Vec<u8>,
  ) -> Result<Vec<u8>, String> {
    let (width, height) = (self.region.width, self.region.height);
    if region.len() != width as usize * height as usize * 4 {
      return Err(format!(
        "{} bytes for a {width}x{height} capture",
        region.len()
      ));
    }
    if self.is_identity() {
      return Ok(region);
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

/// The buffer size of a window capture of `rect` at `scale`.
pub(crate) fn window_buffer_size(rect: Rect, scale: f64) -> (u32, u32) {
  let size = |logical: u32| {
    ((f64::from(logical) * scale).round() as u32).max(u32::from(logical > 0))
  };
  (size(rect.width), size(rect.height))
}

/// What a capture is of, as the compositor resolves it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
  /// `region` of an output, the whole output for `None`.
  Output(OutputId, Option<Rect>),
  Window(WindowId),
}

impl Target {
  fn damaged(&self) -> Damaged {
    match self {
      Self::Output(output, _) => Damaged::Output(output.clone()),
      Self::Window(window) => Damaged::Window(*window),
    }
  }
}

/// The rectangle of window `window`'s toplevel surface in the layout and
/// the scale of the output it is on, while it is mapped with a buffer.
pub(crate) fn window_rect(
  state: &CompositorState,
  window: WindowId,
) -> Option<(Rect, f64)> {
  let snapshot = state.window_surfaces(window).into_iter().next()?;
  let rect = snapshot.geometry;
  (rect.width > 0 && rect.height > 0)
    .then(|| (rect, state.output_of_window(window).scale))
}

/// The geometry a capture of `target` has now, or `None` when the source
/// is gone or the region misses it.
pub(crate) fn geometry_of(
  state: &CompositorState,
  target: &Target,
) -> Option<CaptureGeometry> {
  match target {
    Target::Output(output, region) => {
      CaptureGeometry::of_output(state.outputs.get(output)?, *region)
    }
    Target::Window(window) => {
      let (rect, scale) = window_rect(state, *window)?;
      CaptureGeometry::of_window(rect, scale)
    }
  }
}

/// A capture between its copy request and its answer.
struct Pending {
  frame: CaptureFrame,
  buffer: WlBuffer,
  attachment: Attachment,
  geometry: CaptureGeometry,
  target: Target,
  cursor: bool,
  key: Option<DamageKey>,
}

/// The captures of every protocol.
#[derive(Default)]
pub(crate) struct Captures {
  /// Whether copies go to the frontend instead of the CPU composite.
  pub(crate) frame_capture: bool,
  /// The DMA-BUFs copies may go into, with frame capture.
  pub(crate) dmabuf: Option<CaptureDmabuf>,
  next: u64,
  /// Captures waiting for damage, oldest first.
  waiting: BTreeMap<CaptureId, Pending>,
  /// Captures the frontend was asked to draw.
  requested: HashMap<CaptureId, Pending>,
  /// Damage trackers with nothing new since their last answered capture,
  /// with what damages them.
  clean: HashMap<DamageKey, Damaged>,
}

impl Captures {
  /// How many captures wait for damage and for the frontend.
  pub(crate) fn totals(&self) -> (usize, usize) {
    (self.waiting.len(), self.requested.len())
  }
}

/// What a client asked to copy.
pub(crate) struct Copy {
  pub(crate) frame: CaptureFrame,
  pub(crate) buffer: WlBuffer,
  pub(crate) attachment: Attachment,
  pub(crate) geometry: CaptureGeometry,
  pub(crate) target: Target,
  pub(crate) cursor: bool,
  /// The damage tracker, for a copy that waits until its source changed
  /// since the tracker's last capture; `None` copies at once.
  pub(crate) key: Option<DamageKey>,
}

/// Starts `copy`: at once, or once its source is damaged.
pub(crate) fn start(state: &mut CompositorState, copy: Copy) {
  let id = CaptureId(state.captures.next);
  state.captures.next += 1;
  let pending = Pending {
    frame: copy.frame,
    buffer: copy.buffer,
    attachment: copy.attachment,
    geometry: copy.geometry,
    target: copy.target,
    cursor: copy.cursor,
    key: copy.key,
  };
  if pending
    .key
    .as_ref()
    .is_some_and(|key| state.captures.clean.contains_key(key))
  {
    state.captures.waiting.insert(id, pending);
    return;
  }
  if let Some(request) = issue(state, id, pending) {
    state.events.push(crate::Event::CaptureRequested(request));
  }
}

/// Draws capture `id` now: on the CPU, answering it at once, or by the
/// frontend, returning what it is to draw.
fn issue(
  state: &mut CompositorState,
  id: CaptureId,
  pending: Pending,
) -> Option<CaptureRequest> {
  // The source may have changed while the capture waited.
  let Some(geometry) = geometry_of(state, &pending.target) else {
    pending.frame.fail(Failure::Stopped);
    return None;
  };
  if (geometry.buffer.width, geometry.buffer.height)
    != (
      pending.geometry.buffer.width,
      pending.geometry.buffer.height,
    )
  {
    pending.frame.fail(Failure::BufferConstraints);
    return None;
  }
  let pending = Pending {
    geometry,
    ..pending
  };
  let dmabuf = pending.attachment.dmabuf_buffer().cloned();
  if dmabuf.is_some() && !state.captures.frame_capture {
    // Only a renderer writes DMA-BUFs.
    pending.frame.fail(Failure::BufferConstraints);
    return None;
  }
  if !state.captures.frame_capture {
    let rgba = composite(state, &pending);
    answer(state, pending, Ok(rgba));
    return None;
  }
  let source = match &pending.target {
    Target::Output(output, _) => CaptureSource::Output {
      output: OutputHandle::from(output),
      region: pending.geometry.region,
    },
    Target::Window(window) => CaptureSource::Window {
      window: *window,
      rect: Rect::new(
        pending.geometry.origin.0,
        pending.geometry.origin.1,
        pending.geometry.logical.0,
        pending.geometry.logical.1,
      ),
    },
  };
  let request = CaptureRequest {
    id,
    source,
    cursor: pending.cursor,
    dmabuf,
  };
  state.captures.requested.insert(id, pending);
  Some(request)
}

/// Marks `damaged`, and everything on it, as changed, and draws the
/// captures that waited for it; returns those the frontend is to draw.
pub(crate) fn damage(
  state: &mut CompositorState,
  damaged: &[Damaged],
) -> Vec<CaptureRequest> {
  let hit = |state: &CompositorState, source: &Damaged| {
    damaged.iter().any(|damaged| match (damaged, source) {
      (Damaged::Output(output), Damaged::Window(window)) => {
        state.output_of_window(*window).id == *output
      }
      (damaged, source) => damaged == source,
    })
  };
  let clean = std::mem::take(&mut state.captures.clean);
  state.captures.clean = clean
    .into_iter()
    .filter(|(_, source)| !hit(state, source))
    .collect();
  let released = state
    .captures
    .waiting
    .iter()
    .filter(|(_, pending)| hit(state, &pending.target.damaged()))
    .map(|(id, _)| *id)
    .collect::<Vec<_>>();
  released
    .into_iter()
    .filter_map(|id| {
      let pending = state.captures.waiting.remove(&id)?;
      issue(state, id, pending)
    })
    .collect()
}

/// Damages every output and window when frame capture is off and clients
/// changed something this turn, and fails captures whose frame, buffer or
/// source is gone.
pub(crate) fn sync(state: &mut CompositorState, changed: bool) {
  let waiting = std::mem::take(&mut state.captures.waiting);
  for (id, pending) in waiting {
    if !pending.frame.is_alive() {
      continue;
    }
    if !pending.buffer.is_alive() {
      pending.frame.fail(Failure::Unknown);
      continue;
    }
    if geometry_of(state, &pending.target).is_none() {
      pending.frame.fail(Failure::Stopped);
      continue;
    }
    state.captures.waiting.insert(id, pending);
  }
  if changed && !state.captures.frame_capture {
    let mut damaged = state
      .outputs
      .iter()
      .map(|output| Damaged::Output(output.id.clone()))
      .collect::<Vec<_>>();
    damaged.extend(state.captures.waiting.values().filter_map(|pending| {
      match pending.target {
        Target::Window(window) => Some(Damaged::Window(window)),
        Target::Output(..) => None,
      }
    }));
    damage(state, &damaged);
  }
}

/// Forgets the captures of `frame`, which was destroyed.
pub(crate) fn forget_frame(state: &mut CompositorState, frame: &ObjectId) {
  state
    .captures
    .waiting
    .retain(|_, pending| pending.frame.id() != *frame);
  state
    .captures
    .requested
    .retain(|_, pending| pending.frame.id() != *frame);
}

/// Forgets damage tracker `key`, whose session or client is gone.
pub(crate) fn forget_key(state: &mut CompositorState, key: &DamageKey) {
  state.captures.clean.remove(key);
}

/// Forgets the screencopy damage trackers of the clients that left.
pub(crate) fn cleanup_clients(
  state: &mut CompositorState,
  clients: &[ClientId],
) {
  state.captures.clean.retain(|key, _| match key {
    DamageKey::Screencopy(client, ..) => !clients.contains(client),
    DamageKey::Session(_) => true,
  });
}

/// Answers the capture `id` with the frontend's `result`: tightly packed
/// RGBA rows of the source in logical pixels, which are scaled into the
/// buffer, or why there are none. Captures whose frame or buffer is gone,
/// or that were answered already, are ignored or failed.
pub(crate) fn finish(
  state: &mut CompositorState,
  id: CaptureId,
  result: Result<Vec<u8>, String>,
) {
  let Some(pending) = state.captures.requested.remove(&id) else {
    return;
  };
  let result = result.and_then(|region| pending.geometry.scale_logical(region));
  answer(state, pending, result);
}

/// Answers the capture `id` into a DMA-BUF with what the frontend did:
/// drew into it, or why it could not.
pub(crate) fn finish_into(
  state: &mut CompositorState,
  id: CaptureId,
  result: Result<(), String>,
) {
  let Some(pending) = state.captures.requested.remove(&id) else {
    return;
  };
  if !pending.frame.is_alive() {
    return;
  }
  match result {
    Ok(()) if pending.buffer.is_alive() => ready(state, pending),
    Ok(()) => pending.frame.fail(Failure::Unknown),
    Err(error) => {
      eprintln!("shingle: capture into a DMA-BUF failed: {error}");
      pending.frame.fail(Failure::Unknown);
    }
  }
}

/// Sends `ready` for a capture whose buffer holds its pixels, and marks
/// its damage tracker clean.
fn ready(state: &mut CompositorState, pending: Pending) {
  if let Some(key) = pending.key {
    state.captures.clean.insert(key, pending.target.damaged());
  }
  let size = (
    pending.geometry.buffer.width,
    pending.geometry.buffer.height,
  );
  pending.frame.ready(state.now(), size);
}

/// Writes `rgba` into the capture's buffer and sends `ready`, or `failed`
/// when there are no pixels or they do not fit.
fn answer(
  state: &mut CompositorState,
  pending: Pending,
  rgba: Result<Vec<u8>, String>,
) {
  if !pending.frame.is_alive() {
    return;
  }
  if !pending.buffer.is_alive() {
    pending.frame.fail(Failure::Unknown);
    return;
  }
  match rgba.and_then(|rgba| pending.attachment.write_capture(&rgba)) {
    Ok(()) => ready(state, pending),
    Err(error) => {
      eprintln!("shingle: capture failed: {error}");
      pending.frame.fail(Failure::Unknown);
    }
  }
}

/// The capture's buffer composited on the CPU from the surfaces of its
/// source: those drawn on its output over the background colour, or the
/// window's own over transparent black.
fn composite(state: &CompositorState, pending: &Pending) -> Vec<u8> {
  let (background, surfaces) = match &pending.target {
    Target::Output(output, _) => (
      [
        color_byte(state.background_color[0]),
        color_byte(state.background_color[1]),
        color_byte(state.background_color[2]),
        255,
      ],
      state.visible_surfaces_on(output),
    ),
    Target::Window(window) => ([0; 4], state.window_surfaces(*window)),
  };
  let surfaces = surfaces
    .into_iter()
    .filter(|surface| pending.cursor || surface.role != SurfaceRole::Cursor)
    .filter_map(|surface| {
      let buffer = surface.buffer?;
      // Content without CPU pixels is left to the frontend's capture.
      let pixels = buffer.rgba()?;
      let opacity = (surface.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
      (opacity > 0).then_some((surface.geometry, buffer, pixels, opacity))
    })
    .collect::<Vec<_>>();
  pending.geometry.fill(|x, y| {
    let mut destination = background;
    for (rect, buffer, pixels, opacity) in &surfaces {
      let Some(source_index) = sample_index(buffer, *rect, x, y) else {
        continue;
      };
      let source = &pixels[source_index..source_index + 4];
      destination = blend(source, *opacity, destination);
    }
    destination
  })
}

/// `source`, a premultiplied RGBA pixel as Wayland buffers hold them,
/// drawn at `opacity` (0 to 255) over `destination`.
fn blend(source: &[u8], opacity: u32, destination: [u8; 4]) -> [u8; 4] {
  let alpha = (u32::from(source[3]) * opacity + 127) / 255;
  if alpha == 255 {
    return [source[0], source[1], source[2], 255];
  }
  if alpha == 0 {
    return destination;
  }
  let mut blended = destination;
  for channel in 0..3 {
    blended[channel] = ((u32::from(source[channel]) * opacity
      + u32::from(destination[channel]) * (255 - alpha)
      + 127)
      / 255)
      .min(255) as u8;
  }
  blended[3] = (alpha + (u32::from(destination[3]) * (255 - alpha) + 127) / 255)
    .min(255) as u8;
  blended
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

#[cfg(test)]
mod tests {
  use shingle_core::Rect;
  use wayland_server::protocol::wl_output::Transform;

  use super::{CaptureGeometry, blend, window_buffer_size};

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
    let rgba = geometry.scale_logical(region).unwrap();
    assert_eq!(rgba.len(), 6 * 4 * 4);
    assert_eq!(rgba[..8], [0, 0, 0, 255, 0, 0, 0, 255]);
    assert_eq!(rgba[8..12], [1, 0, 0, 255]);
    assert_eq!(rgba[6 * 4 * 2..6 * 4 * 2 + 4], [3, 0, 0, 255]);
    assert!(geometry.scale_logical(vec![0; 4]).is_err());
  }

  #[test]
  fn an_unscaled_capture_keeps_the_frontends_pixels() {
    let geometry = CaptureGeometry::new(
      (0, 0),
      (4, 2),
      (4, 2),
      Transform::Normal,
      Some(Rect::new(1, 0, 2, 2)),
    )
    .unwrap();
    assert!(geometry.is_identity());
    let region = (0..16).collect::<Vec<u8>>();
    assert_eq!(geometry.scale_logical(region.clone()).unwrap(), region);
    let turned =
      CaptureGeometry::new((0, 0), (2, 4), (4, 2), Transform::_90, None)
        .unwrap();
    assert!(!turned.is_identity());
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

  #[test]
  fn a_window_capture_covers_its_rectangle_at_its_outputs_scale() {
    let rect = Rect::new(100, 50, 30, 20);
    assert_eq!(window_buffer_size(rect, 1.5), (45, 30));
    let geometry = CaptureGeometry::of_window(rect, 2.0).unwrap();
    assert_eq!(geometry.buffer, Rect::new(0, 0, 60, 40));
    assert_eq!(geometry.region, Rect::new(0, 0, 30, 20));
    let (x, y) = geometry.layout_point(0, 0);
    assert!((x - 100.25).abs() < 1e-9 && (y - 50.25).abs() < 1e-9);
  }

  #[test]
  fn blending_keeps_coverage_over_transparent_black() {
    // Opaque over anything is the source.
    assert_eq!(blend(&[10, 20, 30, 255], 255, [0; 4]), [10, 20, 30, 255]);
    // Half over transparent black is half as bright and half covered.
    assert_eq!(blend(&[200, 0, 0, 255], 128, [0; 4]), [100, 0, 0, 128]);
    // Half over an opaque background stays opaque.
    assert_eq!(
      blend(&[128, 128, 128, 128], 255, [0, 0, 0, 255]),
      [128, 128, 128, 255]
    );
    assert_eq!(blend(&[1, 2, 3, 0], 255, [9, 9, 9, 9]), [9, 9, 9, 9]);
  }
}
