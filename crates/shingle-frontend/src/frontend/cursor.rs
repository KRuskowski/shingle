//! The cursor: drawn into the frame and redrawn wherever it moves, or
//! left to a hardware cursor plane of the host (docs/design/native-backend.md,
//! NB-10).
//!
//! The compositor places the cursor ([`shingle::Compositor::cursor_on`]).
//! While the frontend draws it, every move or change of it asks for a frame
//! of the outputs it left and entered ([`Frontend::track_cursor`]). A host
//! with a cursor plane asks [`Frontend::cursor_plane`] what the plane should
//! show and, when it shows the cursor, turns the frontend's own drawing of
//! it off for that output ([`Frontend::set_hardware_cursor`]); then moving
//! the pointer draws no frame at all.

use std::sync::Arc;

use shingle::{OutputHandle, SnapshotContent, SurfaceId};
use shingle_core::Rect;
use wayland_server::protocol::wl_output;

use super::Frontend;

/// The cursor image a cursor plane shows.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorImage {
  /// Width in pixels, which is the output's pixels too.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// Tightly packed premultiplied RGBA rows, as the compositor keeps
  /// cursor buffers. The same `Arc` while the image does not change.
  pub rgba: Arc<[u8]>,
  /// Where its top-left corner is, in the output's physical pixels; it
  /// may be negative or beyond the output when the cursor is at an edge.
  pub position: (i32, i32),
}

/// What an output's cursor plane should show.
#[derive(Clone, Debug, PartialEq)]
pub enum CursorPlane {
  /// Nothing: the pointer is on another output, the cursor is hidden or
  /// the output shows nothing.
  Hidden,
  /// This image.
  Image(CursorImage),
  /// The frontend must draw the cursor into the frame, for this reason.
  Software(String),
}

impl Frontend {
  /// What a cursor plane of `output` that takes images up to `max` pixels
  /// should show. The plane shows the cursor's pixels one to one at the
  /// output's scale, so anything that the frontend would draw differently
  /// keeps the cursor in software: zoom, inverted colours, a gamma ramp, a
  /// rotated or flipped output, a cursor buffer with a viewport, a
  /// transform or another scale than the output, one that is a DMA-BUF,
  /// and one larger than the plane.
  pub fn cursor_plane(
    &self,
    output: &OutputHandle,
    max: (u32, u32),
  ) -> CursorPlane {
    let Some(view) = self.compositor.output_view(output) else {
      return CursorPlane::Hidden;
    };
    let Some(cursor) = self.compositor.cursor_on(output) else {
      return CursorPlane::Hidden;
    };
    let Some(buffer) = &cursor.buffer else {
      return CursorPlane::Hidden;
    };
    let software = |reason: &str| CursorPlane::Software(reason.to_owned());
    if self.zoom_of(output).0 != 1.0 {
      return software("the output is zoomed");
    }
    if self.inverted {
      return software("colours are inverted");
    }
    if self.compositor.gamma_set_on(output) {
      return software("a gamma ramp is set");
    }
    if view.transform != wl_output::Transform::Normal {
      return software("the output is rotated or flipped");
    }
    if buffer.source.is_some() {
      return software("the cursor surface has a viewport");
    }
    if buffer.transform != wl_output::Transform::Normal {
      return software("the cursor buffer is transformed");
    }
    if f64::from(buffer.scale) != view.scale {
      return CursorPlane::Software(format!(
        "the cursor's buffer scale {} is not the output's scale {}",
        buffer.scale, view.scale
      ));
    }
    let SnapshotContent::Pixels(rgba) = &buffer.content else {
      return software("the cursor buffer is not in shared memory");
    };
    if buffer.width > max.0 || buffer.height > max.1 {
      return CursorPlane::Software(format!(
        "the cursor ({}x{}) is larger than the cursor plane ({}x{})",
        buffer.width, buffer.height, max.0, max.1
      ));
    }
    let physical = |layout: i32, origin: i32| {
      (f64::from(layout - origin) * view.scale).round() as i32
    };
    CursorPlane::Image(CursorImage {
      width: buffer.width,
      height: buffer.height,
      rgba: Arc::clone(rgba),
      position: (
        physical(cursor.geometry.x, view.rect.x),
        physical(cursor.geometry.y, view.rect.y),
      ),
    })
  }

  /// Makes the frontend leave the cursor out of `output`'s frames, which
  /// a cursor plane shows instead, or draw it again. Changing it asks for
  /// a frame of the output, which adds or removes the drawn cursor.
  pub fn set_hardware_cursor(&mut self, output: &OutputHandle, on: bool) {
    let changed = if on {
      self.hardware_cursors.insert(output.clone())
    } else {
      self.hardware_cursors.remove(output)
    };
    if changed {
      self.request_redraw_on(output);
    }
  }

  /// Whether `output`'s cursor is on a cursor plane rather than drawn.
  pub fn hardware_cursor(&self, output: &OutputHandle) -> bool {
    self.hardware_cursors.contains(output)
  }

  /// Asks for a frame of the outputs the drawn cursor left and entered
  /// when it moved or became another surface, since the frontend draws it
  /// into the frame. An output with a cursor plane needs none.
  pub(super) fn track_cursor(&mut self) {
    let output = self.compositor.pointer_output();
    let now = self
      .compositor
      .cursor_on(&output)
      .map(|cursor| DrawnCursor {
        output: output.clone(),
        surface: cursor.id,
        geometry: cursor.geometry,
      });
    if now == self.drawn_cursor {
      return;
    }
    let before = std::mem::replace(&mut self.drawn_cursor, now);
    let outputs = before
      .into_iter()
      .chain(self.drawn_cursor.clone())
      .map(|cursor| cursor.output)
      .filter(|output| !self.hardware_cursors.contains(output))
      .collect::<Vec<_>>();
    for output in outputs {
      self.request_redraw_on(&output);
    }
  }
}

/// Where the cursor was when [`Frontend::track_cursor`] last looked.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DrawnCursor {
  output: OutputHandle,
  surface: SurfaceId,
  geometry: Rect,
}
