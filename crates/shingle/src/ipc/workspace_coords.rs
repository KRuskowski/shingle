//! Wayfire's coordinates for views on other workspaces.
//!
//! Wayfire keeps every view of a workspace set in one plane in which the
//! current workspace is the output's rectangle and workspace `(x, y)` lies
//! `(x - current.x) * width` and `(y - current.y) * height` away from it,
//! with no gap (wayfire:src/output/workspace-impl.cpp:501-516,
//! `view_visible_on`). Switching the workspace moves every view of the set
//! that is neither sticky nor carried by that difference (:596-617), and so
//! changes their IPC `geometry`. The plane is each output's own, with the
//! output's top-left corner at 0,0 (wayfire:src/output/workspace-impl.cpp:
//! 198-202, `get_relative_geometry`), so a view on an output at 1920,0 is
//! at 0,0 when it fills that output. Shingle keeps a window's rectangle in
//! its own workspace, laid over the output in layout coordinates, so IPC
//! takes the output's corner off and adds the workspace's offset to every
//! geometry it reports, and does the reverse to every geometry it is
//! given (docs/ipc.md).

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};
use shingle_core::{Rect, WindowId, WindowPresence, WorkspaceId, WsetId};

use crate::state::CompositorState;

/// What IPC adds to a rectangle in layout coordinates to put it in its
/// output's plane: how far its workspace lies from its set's current one,
/// less the corner of the output, in logical pixels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Offset {
  /// Horizontal distance; positive to the right.
  pub(crate) dx: i64,
  /// Vertical distance; positive downwards.
  pub(crate) dy: i64,
}

impl Offset {
  /// `rect` moved by the offset, saturated to `i32`.
  pub(crate) fn apply(self, rect: Rect) -> Rect {
    Rect::new(
      saturate(i64::from(rect.x) + self.dx),
      saturate(i64::from(rect.y) + self.dy),
      rect.width,
      rect.height,
    )
  }

  /// `rect` moved back by the offset, saturated to `i32`.
  pub(crate) fn remove(self, rect: Rect) -> Rect {
    Offset {
      dx: -self.dx,
      dy: -self.dy,
    }
    .apply(rect)
  }
}

fn saturate(value: i64) -> i32 {
  value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// The rectangle a workspace of set `wset` covers when it is current: its
/// output's, else the output it was last shown on, else its workarea, as
/// Wayfire falls back to the last output's geometry
/// (wayfire:src/output/workspace-impl.cpp:759).
pub(crate) fn workspace_frame(state: &CompositorState, wset: WsetId) -> Rect {
  let Ok(info) = state.core.wset(wset) else {
    return state.active().rect;
  };
  info
    .output
    .iter()
    .chain(info.home.iter())
    .find_map(|output| state.outputs.get(output))
    .map_or(info.workarea, |output| output.rect)
}

/// The offset of workspace `workspace` of set `wset`: its distance from
/// the set's current workspace, less the corner of the set's frame
/// ([`workspace_frame`]).
pub(crate) fn workspace_offset(
  state: &CompositorState,
  wset: WsetId,
  workspace: WorkspaceId,
) -> Offset {
  let Ok(info) = state.core.wset(wset) else {
    return Offset::default();
  };
  let frame = workspace_frame(state, wset);
  let current = info.current_workspace;
  Offset {
    dx: (i64::from(workspace.x) - i64::from(current.x))
      * i64::from(frame.width)
      - i64::from(frame.x),
    dy: (i64::from(workspace.y) - i64::from(current.y))
      * i64::from(frame.height)
      - i64::from(frame.y),
  }
}

/// The offset of `window`'s workspace; only the output's corner for a
/// sticky window, which Wayfire never moves on a switch.
pub(crate) fn window_offset(
  state: &CompositorState,
  window: WindowId,
) -> Offset {
  let Ok(info) = state.core.window_info(window) else {
    return Offset::default();
  };
  let workspace = if info.state.sticky {
    // Its own workspace is drawn as the current one.
    state
      .core
      .wset(info.wset)
      .map_or(info.location.workspace, |wset| wset.current_workspace)
  } else {
    info.location.workspace
  };
  workspace_offset(state, info.wset, workspace)
}

/// The workspace of set `wset` that IPC rectangle `rect` belongs to:
/// Wayfire's main workspace, the one holding its centre, or the closest
/// workspace of the grid when the centre is outside it
/// (wayfire:src/output/workspace-impl.cpp:478-492).
pub(crate) fn main_workspace(
  state: &CompositorState,
  wset: WsetId,
  rect: Rect,
) -> WorkspaceId {
  let Ok(info) = state.core.wset(wset) else {
    return WorkspaceId::ORIGIN;
  };
  let frame = workspace_frame(state, wset);
  // The rectangle is in the output's plane, whose current workspace
  // starts at 0,0.
  let axis = |start: i32, extent: u32, size: u32, current| {
    let centre = f64::from(start) + f64::from(extent) / 2.0;
    let steps = (centre / f64::from(size.max(1))).floor();
    (f64::from(current) + steps) as i64
  };
  let column = axis(rect.x, rect.width, frame.width, info.current_workspace.x);
  let row = axis(rect.y, rect.height, frame.height, info.current_workspace.y);
  let last = |count: u32| i64::from(count.max(1) - 1);
  WorkspaceId::new(
    column.clamp(0, last(state.workspace_width)) as u32,
    row.clamp(0, last(state.workspace_height)) as u32,
  )
}

/// Puts `window` where IPC rectangle `rect` says, as Wayfire's
/// `configure-view` and `stipc/layout_views` do by setting the view's
/// geometry (wayfire:plugins/ipc-rules/ipc-rules.cpp:302-310): floating,
/// on the workspace holding the centre, at `rect` less that workspace's
/// offset. A sticky window and a window of a parked set stay on their
/// workspace. Wayfire sends no `view-workspace-changed` for it, so the
/// move is not reported.
pub(crate) fn place_window(
  state: &mut CompositorState,
  window: WindowId,
  rect: Rect,
) -> Result<(), crate::Error> {
  let info = state.core.window_info(window)?;
  let shown = state.core.wset(info.wset)?.output.is_some();
  let target = if info.state.sticky || !shown {
    info.location.workspace
  } else {
    main_workspace(state, info.wset, rect)
  };
  let local = if info.state.sticky {
    window_offset(state, window).remove(rect)
  } else {
    workspace_offset(state, info.wset, target).remove(rect)
  };
  crate::shell::set_floating_geometry(state, window, local)?;
  if target != info.location.workspace {
    let index =
      target.y as usize * state.workspace_width as usize + target.x as usize;
    state.move_window_to_workspace_as(window, index, false)?;
  }
  Ok(())
}

/// The workspace offsets of the windows IPC has reported, so that a
/// geometry change that comes from a workspace switch alone is reported
/// with the geometry the window had before it.
#[derive(Debug, Default)]
pub(crate) struct OffsetTracker {
  offsets: BTreeMap<WindowId, Offset>,
}

impl OffsetTracker {
  /// The offset last reported for `window`, if any.
  pub(crate) fn reported(&self, window: WindowId) -> Option<Offset> {
    self.offsets.get(&window).copied()
  }

  /// Records `offset` as reported for `window`.
  pub(crate) fn record(&mut self, window: WindowId, offset: Offset) {
    self.offsets.insert(window, offset);
  }

  /// Forgets `window`.
  pub(crate) fn forget(&mut self, window: WindowId) {
    self.offsets.remove(&window);
  }
}

/// `view-geometry-changed` for every mapped window whose offset changed
/// since it was last reported and which has no configure of its own in
/// this batch (`skip`), oldest window first; Wayfire moves every view of
/// the set on a switch and reports each one
/// (wayfire:src/output/workspace-impl.cpp:612-617,
/// wayfire:src/view/view-impl.cpp:125-130). Windows seen for the first
/// time are recorded without an event.
pub(crate) fn offset_changes(
  state: &mut CompositorState,
  skip: &HashSet<WindowId>,
) -> Vec<Value> {
  let windows = state
    .core
    .window_ids()
    .filter(|window| {
      state
        .core
        .window_info(*window)
        .is_ok_and(|info| info.presence == WindowPresence::Mapped)
    })
    .collect::<std::collections::BTreeSet<_>>();
  let mut messages = Vec::new();
  for window in windows {
    let current = window_offset(state, window);
    let Some(reported) = state.ipc_offsets.reported(window) else {
      state.ipc_offsets.record(window, current);
      continue;
    };
    if reported == current || skip.contains(&window) {
      continue;
    }
    state.ipc_offsets.record(window, current);
    let view = super::view_json(state, window);
    let Some(rect) = super::rect_of(&view["geometry"]) else {
      continue;
    };
    // The geometry before the change is the same rectangle in its own
    // workspace, at the offset it had then.
    let old = reported.apply(current.remove(rect));
    messages.push(json!({
        "event": "view-geometry-changed",
        "old-geometry": super::geometry(old),
        "view": view,
    }));
  }
  messages
}

#[cfg(test)]
mod tests;
