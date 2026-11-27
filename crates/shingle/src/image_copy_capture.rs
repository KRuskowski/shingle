//! ext-image-capture-source-v1 and ext-image-copy-capture-v1: captures
//! of outputs and single toplevels for xdg-desktop-portal-wlr 0.8, grim
//! 1.5, OBS and friends.
//!
//! A source names an output (`ext_output_image_capture_source_manager_v1`)
//! or an ext-foreign-toplevel-list handle
//! (`ext_foreign_toplevel_image_capture_source_manager_v1`, offered with
//! that list). A session tells its client the buffer it takes: the
//! output's buffer pixels, turned back by its transform, which every
//! frame's `transform` event names, as in wlroots; or the toplevel
//! surface's rectangle at its output's scale. It says so again whenever
//! that changes, and `stopped` when the source goes. Frames are copied by
//! crates/shingle/src/capture.rs, shared with wlr-screencopy: the first
//! at once, every later one once its source changed since the previous.
//! A window capture draws only that toplevel's surfaces and popups.
//!
//! Cursor sessions report where the pointer's hotspot is over the source
//! and give the cursor image to their capture session's frames, which
//! wait until the image changes, from the compositor's own copy of the
//! cursor's pixels (docs/design/screen-sharing.md).

use std::{collections::HashMap, sync::Arc};

use shingle_core::{OutputId, Rect, WindowId};
use wayland_protocols::ext::{
  image_capture_source::v1::server::{
    ext_foreign_toplevel_image_capture_source_manager_v1::{
      self as toplevel_sources, ExtForeignToplevelImageCaptureSourceManagerV1,
    },
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
    ext_output_image_capture_source_manager_v1::{
      self, ExtOutputImageCaptureSourceManagerV1,
    },
  },
  image_copy_capture::v1::server::{
    ext_image_copy_capture_cursor_session_v1::{
      self as cursor_session, ExtImageCopyCaptureCursorSessionV1,
    },
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
  },
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
  protocol::{wl_buffer::WlBuffer, wl_output},
};

use crate::{
  buffer::CAPTURE_SHM_FORMATS,
  capture::{self, CaptureFrame, Copy, DamageKey, Failure, Target},
  state::CompositorState,
};

/// What a source object names; `None` when its output or toplevel was
/// already gone, which stops every session at once.
#[derive(Clone, Debug)]
pub(crate) struct SourceData(Option<Target>);

/// What a capture session's frames show.
#[derive(Clone, Debug)]
enum Content {
  /// The source, with the cursor if the client asked to paint cursors.
  Image { target: Target, cursor: bool },
  /// The cursor image of a cursor session.
  Cursor(ObjectId),
}

struct Session {
  resource: ExtImageCopyCaptureSessionV1,
  content: Option<Content>,
  /// The buffer size last advertised.
  size: Option<(u32, u32)>,
  /// The frame that exists, at most one.
  frame: Option<ObjectId>,
  stopped: bool,
}

struct Frame {
  session: ObjectId,
  buffer: Option<WlBuffer>,
  captured: bool,
}

/// The cursor as a cursor session last reported it.
#[derive(Clone, Debug, Default, PartialEq)]
struct CursorSeen {
  inside: bool,
  position: Option<(i32, i32)>,
  hotspot: Option<(i32, i32)>,
}

struct CursorSession {
  resource: ExtImageCopyCaptureCursorSessionV1,
  target: Option<Target>,
  /// Its capture session, once the client asked for it.
  session: Option<ObjectId>,
  seen: CursorSeen,
  /// The cursor image its capture session's last frame showed.
  shown: Option<Arc<[u8]>>,
}

/// A cursor capture frame waiting for the image to change.
struct CursorWait {
  frame: ExtImageCopyCaptureFrameV1,
  buffer: WlBuffer,
  cursor_session: ObjectId,
}

#[derive(Default)]
pub(crate) struct ImageCopyCapture {
  sessions: HashMap<ObjectId, Session>,
  frames: HashMap<ObjectId, Frame>,
  cursor_sessions: HashMap<ObjectId, CursorSession>,
  cursor_waits: HashMap<ObjectId, CursorWait>,
}

impl ImageCopyCapture {
  /// How many sessions, frames and cursor sessions are tracked.
  pub(crate) fn totals(&self) -> (usize, usize, usize) {
    (
      self.sessions.len(),
      self.frames.len(),
      self.cursor_sessions.len(),
    )
  }
}

/// Offers the output source manager and the copy manager, and with
/// `toplevels` (ext-foreign-toplevel-list is offered) the toplevel source
/// manager.
pub(crate) fn create_globals(handle: &DisplayHandle, toplevels: bool) {
  handle
    .create_global::<CompositorState, ExtImageCopyCaptureManagerV1, ()>(1, ());
  handle
    .create_global::<CompositorState, ExtOutputImageCaptureSourceManagerV1, ()>(
      1,
      (),
    );
  if toplevels {
    handle.create_global::<
      CompositorState,
      ExtForeignToplevelImageCaptureSourceManagerV1,
      (),
    >(1, ());
  }
}

/// The cursor image shown over `target`, its rectangle in the layout and
/// the pointer's position there, while the pointer is on the source's
/// output.
fn cursor_over(
  state: &CompositorState,
  target: &Target,
) -> Option<(crate::BufferSnapshot, Rect)> {
  let output = match target {
    Target::Output(output, _) => output.clone(),
    Target::Window(window) => state.output_of_window(*window).id.clone(),
  };
  let cursor = state.cursor_on(&output)?;
  let buffer = cursor.buffer?;
  Some((buffer, cursor.geometry))
}

/// The source's rectangle in the layout and its buffer pixels per logical
/// pixel.
fn source_rect(
  state: &CompositorState,
  target: &Target,
) -> Option<(Rect, f64)> {
  match target {
    Target::Output(output, _) => {
      let output = state.outputs.get(output)?;
      Some((output.rect, output.scale))
    }
    Target::Window(window) => capture::window_rect(state, *window),
  }
}

/// What a cursor session reports now.
fn cursor_seen(state: &CompositorState, target: &Target) -> CursorSeen {
  let (Some((rect, scale)), Some((buffer, geometry))) =
    (source_rect(state, target), cursor_over(state, target))
  else {
    return CursorSeen::default();
  };
  if crate::surface_outputs::intersection(rect, geometry).is_none() {
    return CursorSeen::default();
  }
  let (x, y) = (state.input.pointer_x, state.input.pointer_y);
  let position = (
    ((x - f64::from(rect.x)) * scale).round() as i32,
    ((y - f64::from(rect.y)) * scale).round() as i32,
  );
  let per_logical = f64::from(buffer.width) / f64::from(geometry.width.max(1));
  let hotspot = (
    ((x - f64::from(geometry.x)) * per_logical).round() as i32,
    ((y - f64::from(geometry.y)) * per_logical).round() as i32,
  );
  CursorSeen {
    inside: true,
    position: Some(position),
    hotspot: Some(hotspot),
  }
}

/// The buffer size `content` takes now, or `None` when its source is
/// gone.
fn content_size(
  state: &CompositorState,
  content: &Content,
) -> Option<(u32, u32)> {
  match content {
    Content::Image { target, .. } => capture::geometry_of(state, target)
      .map(|geometry| (geometry.buffer.width, geometry.buffer.height)),
    Content::Cursor(id) => {
      let cursor = state.image_copy_capture.cursor_sessions.get(id)?;
      let target = cursor.target.as_ref()?;
      source_rect(state, target)?;
      // Without a cursor shown the last size stays, or a 1x1 buffer.
      Some(
        cursor_over(state, target)
          .map(|(buffer, _)| (buffer.width.max(1), buffer.height.max(1)))
          .unwrap_or((1, 1)),
      )
    }
  }
}

/// Sends the buffer constraints for a `size` buffer: the shared-memory
/// formats and the size, then `done`.
fn send_constraints(session: &ExtImageCopyCaptureSessionV1, size: (u32, u32)) {
  for format in CAPTURE_SHM_FORMATS {
    session.shm_format(format);
  }
  session.buffer_size(size.0, size.1);
  session.done();
}

/// Tells sessions about new buffer sizes, stops those whose source went,
/// reports cursor movements and answers cursor frames whose image
/// changed.
pub(crate) fn sync(state: &mut CompositorState) {
  let ids = state
    .image_copy_capture
    .sessions
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for id in ids {
    let Some(session) = state.image_copy_capture.sessions.get(&id) else {
      continue;
    };
    if session.stopped {
      continue;
    }
    let size = session
      .content
      .as_ref()
      .and_then(|content| content_size(state, content));
    let session = state
      .image_copy_capture
      .sessions
      .get_mut(&id)
      .expect("looked up above");
    match size {
      None => stop(session),
      Some(size) if session.size != Some(size) => {
        session.size = Some(size);
        send_constraints(&session.resource, size);
      }
      Some(_) => {}
    }
  }
  sync_cursors(state);
}

fn stop(session: &mut Session) {
  session.stopped = true;
  session.resource.stopped();
}

fn sync_cursors(state: &mut CompositorState) {
  let ids = state
    .image_copy_capture
    .cursor_sessions
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for id in ids {
    let Some(cursor) = state.image_copy_capture.cursor_sessions.get(&id) else {
      continue;
    };
    let seen = cursor
      .target
      .as_ref()
      .map(|target| cursor_seen(state, target))
      .unwrap_or_default();
    let cursor = state
      .image_copy_capture
      .cursor_sessions
      .get_mut(&id)
      .expect("looked up above");
    report(cursor, seen);
  }
  let waits = state
    .image_copy_capture
    .cursor_waits
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for frame in waits {
    let Some(wait) = state.image_copy_capture.cursor_waits.get(&frame) else {
      continue;
    };
    if !wait.frame.is_alive() {
      state.image_copy_capture.cursor_waits.remove(&frame);
      continue;
    }
    let changed = image_of(state, &wait.cursor_session).is_some_and(|image| {
      state
        .image_copy_capture
        .cursor_sessions
        .get(&wait.cursor_session)
        .is_none_or(|cursor| {
          cursor
            .shown
            .as_ref()
            .is_none_or(|shown| !Arc::ptr_eq(shown, &image))
        })
    });
    if changed {
      let wait = state
        .image_copy_capture
        .cursor_waits
        .remove(&frame)
        .expect("looked up above");
      copy_cursor(state, &wait.frame, &wait.buffer, &wait.cursor_session);
    }
  }
}

/// Sends the cursor events that tell `cursor`'s client about `seen`.
fn report(cursor: &mut CursorSession, seen: CursorSeen) {
  if seen.inside != cursor.seen.inside {
    if seen.inside {
      cursor.resource.enter();
    } else {
      cursor.resource.leave();
    }
    cursor.seen = CursorSeen {
      inside: seen.inside,
      ..CursorSeen::default()
    };
  }
  if !seen.inside {
    return;
  }
  if seen.position != cursor.seen.position
    && let Some((x, y)) = seen.position
  {
    cursor.resource.position(x, y);
  }
  if seen.hotspot != cursor.seen.hotspot
    && let Some((x, y)) = seen.hotspot
  {
    cursor.resource.hotspot(x, y);
  }
  cursor.seen = seen;
}

/// The cursor image a cursor session shows now, as tightly packed RGBA
/// rows of its buffer.
fn image_of(state: &CompositorState, cursor: &ObjectId) -> Option<Arc<[u8]>> {
  let cursor = state.image_copy_capture.cursor_sessions.get(cursor)?;
  let target = cursor.target.as_ref()?;
  if !cursor_seen(state, target).inside {
    return None;
  }
  cursor_over(state, target)?.0.rgba()
}

/// Copies the cursor image into `buffer` and answers `frame`.
fn copy_cursor(
  state: &mut CompositorState,
  frame: &ExtImageCopyCaptureFrameV1,
  buffer: &WlBuffer,
  cursor_session: &ObjectId,
) {
  let image = image_of(state, cursor_session);
  let attachment = state.buffers.get(&buffer.id()).cloned();
  let (Some(image), Some(attachment)) = (image, attachment) else {
    frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
    return;
  };
  if let Err(error) = attachment.write_capture(&image) {
    eprintln!("shingle: cursor capture failed: {error}");
    frame.failed(
      ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
    );
    return;
  }
  if let Some(cursor) = state
    .image_copy_capture
    .cursor_sessions
    .get_mut(cursor_session)
  {
    cursor.shown = Some(image);
  }
  let now = state.now();
  let seconds = now.as_secs();
  frame.transform(wl_output::Transform::Normal);
  frame.damage(0, 0, attachment.width as i32, attachment.height as i32);
  frame.presentation_time(
    (seconds >> 32) as u32,
    seconds as u32,
    now.subsec_nanos(),
  );
  frame.ready();
}

/// Creates a capture session showing `content`, and tells its client the
/// buffer it takes, or that it stopped.
fn create_session(
  state: &mut CompositorState,
  resource: ExtImageCopyCaptureSessionV1,
  content: Option<Content>,
) {
  let size = content
    .as_ref()
    .and_then(|content| content_size(state, content));
  let mut session = Session {
    resource,
    content,
    size,
    frame: None,
    stopped: false,
  };
  match size {
    Some(size) => send_constraints(&session.resource, size),
    None => stop(&mut session),
  }
  state
    .image_copy_capture
    .sessions
    .insert(session.resource.id(), session);
}

fn capture_frame(
  state: &mut CompositorState,
  resource: &ExtImageCopyCaptureFrameV1,
) {
  let Some(frame) = state.image_copy_capture.frames.get_mut(&resource.id())
  else {
    return;
  };
  if frame.captured {
    resource.post_error(
      ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
      "the frame was captured already",
    );
    return;
  }
  let Some(buffer) = frame.buffer.clone() else {
    resource.post_error(
      ext_image_copy_capture_frame_v1::Error::NoBuffer,
      "capture without an attached buffer",
    );
    return;
  };
  frame.captured = true;
  let session_id = frame.session.clone();
  let fail = |reason| {
    CaptureFrame::Ext {
      frame: resource.clone(),
      transform: wl_output::Transform::Normal,
    }
    .fail(reason)
  };
  let Some(session) = state.image_copy_capture.sessions.get(&session_id) else {
    fail(Failure::Stopped);
    return;
  };
  if session.stopped {
    fail(Failure::Stopped);
    return;
  }
  let (Some(content), Some(size)) = (session.content.clone(), session.size)
  else {
    fail(Failure::Stopped);
    return;
  };
  let Some(attachment) = state.buffers.get(&buffer.id()).cloned() else {
    fail(Failure::Unknown);
    return;
  };
  let shm = attachment
    .shm_format()
    .is_some_and(|format| CAPTURE_SHM_FORMATS.contains(&format));
  if (attachment.width, attachment.height) != size || !shm {
    fail(Failure::BufferConstraints);
    return;
  }
  match content {
    Content::Image { target, cursor } => {
      let Some(geometry) = capture::geometry_of(state, &target) else {
        fail(Failure::Stopped);
        return;
      };
      let transform = match &target {
        Target::Output(output, _) => state
          .outputs
          .get(output)
          .map_or(wl_output::Transform::Normal, |output| output.transform),
        Target::Window(_) => wl_output::Transform::Normal,
      };
      capture::start(
        state,
        Copy {
          frame: CaptureFrame::Ext {
            frame: resource.clone(),
            transform,
          },
          buffer,
          attachment,
          geometry,
          target,
          cursor,
          key: Some(DamageKey::Session(session_id)),
        },
      );
    }
    Content::Cursor(cursor_session) => {
      let image = image_of(state, &cursor_session);
      let shown = state
        .image_copy_capture
        .cursor_sessions
        .get(&cursor_session)
        .and_then(|cursor| cursor.shown.clone());
      let unchanged = match (&image, &shown) {
        (Some(image), Some(shown)) => Arc::ptr_eq(image, shown),
        // No cursor over the source: the session is paused.
        (None, _) => true,
        (Some(_), None) => false,
      };
      if unchanged {
        state.image_copy_capture.cursor_waits.insert(
          resource.id(),
          CursorWait {
            frame: resource.clone(),
            buffer,
            cursor_session,
          },
        );
      } else {
        copy_cursor(state, resource, &buffer, &cursor_session);
      }
    }
  }
}

/// The target of `source`, if it still names something.
fn target_of(source: &ExtImageCaptureSourceV1) -> Option<Target> {
  source.data::<SourceData>().and_then(|data| data.0.clone())
}

impl GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ()>
  for CompositorState
{
  crate::security_context::protocol_can_view!(
    "ext_output_image_capture_source_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtOutputImageCaptureSourceManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ExtOutputImageCaptureSourceManagerV1,
    request: ext_output_image_capture_source_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let ext_output_image_capture_source_manager_v1::Request::CreateSource {
      source,
      output,
    } = request
    {
      let output: Option<OutputId> = state
        .outputs
        .of_resource(&output)
        .map(|output| output.id.clone());
      data_init.init(
        source,
        SourceData(output.map(|output| Target::Output(output, None))),
      );
    }
  }
}

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()>
  for CompositorState
{
  crate::security_context::protocol_can_view!(
    "ext_foreign_toplevel_image_capture_source_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()>
  for CompositorState
{
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ExtForeignToplevelImageCaptureSourceManagerV1,
    request: toplevel_sources::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let toplevel_sources::Request::CreateSource {
      source,
      toplevel_handle,
    } = request
    {
      let window: Option<WindowId> =
        crate::ext_toplevel::window_of(state, &toplevel_handle);
      data_init.init(source, SourceData(window.map(Target::Window)));
    }
  }
}

impl Dispatch<ExtImageCaptureSourceV1, SourceData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ExtImageCaptureSourceV1,
    _request: ext_image_capture_source_v1::Request,
    _data: &SourceData,
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }
}

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!(
    "ext_image_copy_capture_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtImageCopyCaptureManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtImageCopyCaptureManagerV1,
    request: ext_image_copy_capture_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_image_copy_capture_manager_v1::Request::CreateSession {
        session,
        source,
        options,
      } => {
        let options = match options {
          wayland_server::WEnum::Value(options) => options,
          wayland_server::WEnum::Unknown(_) => {
            resource.post_error(
              ext_image_copy_capture_manager_v1::Error::InvalidOption,
              "unknown capture options",
            );
            return;
          }
        };
        let cursor = options
          .contains(ext_image_copy_capture_manager_v1::Options::PaintCursors);
        let session = data_init.init(session, ());
        let content =
          target_of(&source).map(|target| Content::Image { target, cursor });
        create_session(state, session, content);
      }
      ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
        session,
        source,
        ..
      } => {
        let resource = data_init.init(session, ());
        state.image_copy_capture.cursor_sessions.insert(
          resource.id(),
          CursorSession {
            resource,
            target: target_of(&source),
            session: None,
            seen: CursorSeen::default(),
            shown: None,
          },
        );
        sync_cursors(state);
      }
      ext_image_copy_capture_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtImageCopyCaptureCursorSessionV1,
    request: cursor_session::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let cursor_session::Request::GetCaptureSession { session } = request {
      let Some(cursor) = state
        .image_copy_capture
        .cursor_sessions
        .get_mut(&resource.id())
      else {
        return;
      };
      if cursor.session.is_some() {
        resource.post_error(
          cursor_session::Error::DuplicateSession,
          "the capture session was asked for already",
        );
        return;
      }
      let session = data_init.init(session, ());
      cursor.session = Some(session.id());
      let content = cursor
        .target
        .is_some()
        .then(|| Content::Cursor(resource.id()));
      create_session(state, session, content);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtImageCopyCaptureCursorSessionV1,
    _data: &(),
  ) {
    let id = resource.id();
    state.image_copy_capture.cursor_sessions.remove(&id);
    // Its capture session stops: nothing is left to show.
    for session in state.image_copy_capture.sessions.values_mut() {
      let shows = matches!(
        &session.content,
        Some(Content::Cursor(cursor)) if *cursor == id
      );
      if shows && !session.stopped {
        stop(session);
      }
    }
  }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtImageCopyCaptureSessionV1,
    request: ext_image_copy_capture_session_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } =
      request
    {
      let session_id = resource.id();
      if state
        .image_copy_capture
        .sessions
        .get(&session_id)
        .is_some_and(|session| session.frame.is_some())
      {
        resource.post_error(
          ext_image_copy_capture_session_v1::Error::DuplicateFrame,
          "the session's previous frame still exists",
        );
        return;
      }
      let frame = data_init.init(frame, ());
      if let Some(session) =
        state.image_copy_capture.sessions.get_mut(&session_id)
      {
        session.frame = Some(frame.id());
      }
      state.image_copy_capture.frames.insert(
        frame.id(),
        Frame {
          session: session_id,
          buffer: None,
          captured: false,
        },
      );
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtImageCopyCaptureSessionV1,
    _data: &(),
  ) {
    let id = resource.id();
    state.image_copy_capture.sessions.remove(&id);
    capture::forget_key(state, &DamageKey::Session(id));
  }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtImageCopyCaptureFrameV1,
    request: ext_image_copy_capture_frame_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let captured = state
      .image_copy_capture
      .frames
      .get(&resource.id())
      .is_some_and(|frame| frame.captured);
    match request {
      ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => {
        if captured {
          resource.post_error(
            ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
            "attach_buffer after capture",
          );
          return;
        }
        if let Some(frame) =
          state.image_copy_capture.frames.get_mut(&resource.id())
        {
          frame.buffer = Some(buffer);
        }
      }
      ext_image_copy_capture_frame_v1::Request::DamageBuffer {
        x,
        y,
        width,
        height,
      } => {
        if captured {
          resource.post_error(
            ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
            "damage_buffer after capture",
          );
        } else if x < 0 || y < 0 || width <= 0 || height <= 0 {
          resource.post_error(
            ext_image_copy_capture_frame_v1::Error::InvalidBufferDamage,
            "buffer damage must be inside the buffer and not empty",
          );
        }
        // Every capture copies the whole buffer, so the client's damage
        // needs no record.
      }
      ext_image_copy_capture_frame_v1::Request::Capture => {
        capture_frame(state, resource);
      }
      ext_image_copy_capture_frame_v1::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtImageCopyCaptureFrameV1,
    _data: &(),
  ) {
    let id = resource.id();
    if let Some(frame) = state.image_copy_capture.frames.remove(&id)
      && let Some(session) =
        state.image_copy_capture.sessions.get_mut(&frame.session)
      && session.frame.as_ref() == Some(&id)
    {
      session.frame = None;
    }
    state.image_copy_capture.cursor_waits.remove(&id);
    capture::forget_frame(state, &id);
  }
}

#[cfg(test)]
mod tests;
