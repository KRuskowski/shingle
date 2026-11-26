//! Moving windows between outputs and workspace sets, as Wayfire's
//! `wf::move_view_to_output` does (src/core/core.cpp;
//! docs/design/multi-output.md, section 4).
//!
//! The core moves the window and its dialogs into the target set's tree
//! ([`shingle_core::Shingle::move_window_to_wset`]); this module does the
//! rest: the workspace it lands on, its floating and snapped geometry, the
//! IPC events `view-set-output` and `view-wset-changed`, and focus.

use shingle_core::{OutputId, Rect, WindowId, WorkspaceId, WsetId};

use crate::{Error, state::CompositorState};

/// What happens to the floating geometry of a window that moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveGeometry {
  /// Wayfire's `VIEW_TO_OUTPUT_FLAG_RECONFIGURE`: the position relative to
  /// the output is scaled by the ratio of the outputs' sizes, and the
  /// rectangle is clamped into the target workarea. Dialogs move by their
  /// parent's offset.
  Scale,
  /// The position relative to the output is kept, as a window moved without
  /// reconfiguring keeps its output-relative geometry in Wayfire.
  Translate,
  /// The layout coordinates are kept, for a window dropped where the
  /// pointer put it.
  Keep,
}

/// The window whose parent chain `window` is in, as `oswitch` moves
/// `find_topmost_parent`: dialogs only move with their parent.
pub(crate) fn topmost_parent(
  state: &CompositorState,
  mut window: WindowId,
) -> WindowId {
  for _ in 0..64 {
    match state
      .core
      .window_info(window)
      .ok()
      .and_then(|info| info.parent)
    {
      Some(parent) if state.core.window_info(parent).is_ok() => {
        window = parent;
      }
      _ => break,
    }
  }
  window
}

/// Moves `window` and its dialogs to the workspace set output `output`
/// shows, see [`move_window_to_wset`].
pub(crate) fn move_window_to_output(
  state: &mut CompositorState,
  window: WindowId,
  output: &OutputId,
  geometry: MoveGeometry,
) -> Result<Vec<WindowId>, Error> {
  let wset = state.core.wset_of(output).ok_or_else(|| {
    Error::Core(shingle_core::ShingleError::OutputNotFound(
      output.as_str().into(),
    ))
  })?;
  move_window_to_wset(state, window, wset, geometry)
}

/// Moves `window` and its dialogs to workspace set `wset`: to the workspace
/// whose offset from `wset`'s current workspace is the window's offset from
/// its own set's current one, clamped into the grid, so a window on the
/// visible workspace lands on the visible workspace. Tiled windows are
/// appended to the target tree, floating ones get `geometry`, snapped ones
/// are snapped again in the target workarea and fullscreen ones fill the
/// target output. Sends `view-wset-changed` for every moved window and
/// `view-set-output` when its output changed, and focuses the window if
/// it lands on the active output's visible workspace. Returns the moved
/// windows; nothing moves when `window` is in `wset` already.
pub(crate) fn move_window_to_wset(
  state: &mut CompositorState,
  window: WindowId,
  wset: WsetId,
  geometry: MoveGeometry,
) -> Result<Vec<WindowId>, Error> {
  let info = state.core.window_info(window)?;
  let source = info.wset;
  if source == wset {
    return Ok(Vec::new());
  }
  let source_info = state.core.wset(source)?;
  let target_info = state.core.wset(wset)?;
  let workspace = landing_workspace(
    state,
    info.location.workspace,
    source_info.current_workspace,
    target_info.current_workspace,
  );
  let from = set_rect(state, source);
  let to = set_rect(state, wset);
  let before = floating_rect(state, window);
  let focused = crate::input::focused_window(state) == Some(window);
  let moved = state.core.move_window_to_wset(window, wset, workspace)?;
  if moved.is_empty() {
    return Ok(moved);
  }
  // Dialogs move by their parent's offset, as in move_view_to_output.
  let mut delta = None;
  for id in &moved {
    match delta {
      Some((dx, dy)) if *id != window && geometry == MoveGeometry::Scale => {
        if let Some(old) = floating_rect(state, *id) {
          let new = Rect::new(old.x + dx, old.y + dy, old.width, old.height);
          set_floating(state, *id, new);
        }
      }
      _ => {
        let new = reconfigure(state, *id, from, to, geometry);
        if *id == window
          && let (Some(old), Some(new)) = (before, new)
        {
          delta = Some((new.x - old.x, new.y - old.y));
        }
      }
    }
  }
  moved_events(state, &moved, source);
  state.events.push(crate::Event::LayoutChanged);
  crate::shell::reconfigure_all(state);
  let shown_on_active = target_info.output.as_ref()
    == Some(&state.active_output)
    && state
      .core
      .current_workspace(&state.active_output)
      .is_ok_and(|current| current == workspace);
  if shown_on_active {
    crate::input::focus_window(state, window);
  } else if focused {
    state.input.keyboard_focus = None;
    crate::input::sync_focus(state);
  }
  Ok(moved)
}

/// Reconfigures windows that a transfer moved from the set shown in `from`
/// into the set shown in `to`, as Wayfire's `transfer_views` moves views
/// with `RECONFIGURE`, and sends their IPC events.
pub(crate) fn transferred(
  state: &mut CompositorState,
  moved: &[WindowId],
  source: WsetId,
  from: Rect,
  to: Rect,
) {
  for window in moved {
    reconfigure(state, *window, from, to, MoveGeometry::Scale);
  }
  moved_events(state, moved, source);
}

/// Gives the windows of `wset`, which was shown in `from` and is shown in
/// `to` now, their places relative to the new output, as Wayfire keeps
/// views' output-relative geometry when an output shows another set.
pub(crate) fn moved_set(
  state: &mut CompositorState,
  wset: WsetId,
  from: Rect,
  to: Rect,
) {
  if from == to {
    return;
  }
  let windows = state
    .core
    .window_ids()
    .filter(|window| {
      state
        .core
        .window_info(*window)
        .is_ok_and(|info| info.wset == wset)
    })
    .collect::<Vec<_>>();
  for window in windows {
    reconfigure(state, window, from, to, MoveGeometry::Translate);
  }
}

/// Sends `view-wset-changed` for each of `moved`, which left set `source`,
/// and `view-set-output` for those whose output changed with it.
pub(crate) fn moved_events(
  state: &mut CompositorState,
  moved: &[WindowId],
  source: WsetId,
) {
  let source_output = state.core.wset(source).ok().and_then(|info| info.output);
  for window in moved {
    let Ok(info) = state.core.window_info(*window) else {
      continue;
    };
    let output = state.core.wset(info.wset).ok().and_then(|info| info.output);
    crate::ipc::view_wset_changed(state, *window, source, info.wset);
    if output != source_output {
      crate::ipc::view_set_output(state, *window, output.as_ref());
    }
  }
}

/// The workspace a window on `workspace` of a set showing `source_current`
/// lands on in a set showing `target_current`: the same offset from the
/// current workspace, clamped into the grid.
fn landing_workspace(
  state: &CompositorState,
  workspace: WorkspaceId,
  source_current: WorkspaceId,
  target_current: WorkspaceId,
) -> WorkspaceId {
  let clamp = |target: u32, window: u32, source: u32, size: u32| {
    let value = i64::from(target) + i64::from(window) - i64::from(source);
    value.clamp(0, i64::from(size.max(1)) - 1) as u32
  };
  WorkspaceId::new(
    clamp(
      target_current.x,
      workspace.x,
      source_current.x,
      state.workspace_width,
    ),
    clamp(
      target_current.y,
      workspace.y,
      source_current.y,
      state.workspace_height,
    ),
  )
}

/// The rectangle of the output showing `wset`, else the set's workarea.
fn set_rect(state: &CompositorState, wset: WsetId) -> Rect {
  let Ok(info) = state.core.wset(wset) else {
    return state.active().rect;
  };
  info
    .output
    .and_then(|output| state.outputs.get(&output).map(|output| output.rect))
    .unwrap_or(info.workarea)
}

/// A window's floating rectangle, if it floats.
fn floating_rect(state: &CompositorState, window: WindowId) -> Option<Rect> {
  state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| info.state.floating)
}

/// Gives a floating window `rect`.
fn set_floating(state: &mut CompositorState, window: WindowId, rect: Rect) {
  if state.core.set_floating(window, Some(rect)).is_ok() {
    crate::shell::sync_floating_geometry(state, window, rect);
  }
}

/// Gives a window that moved from the output at `from` to the one at `to`
/// its new geometry: floating windows by `geometry`, snapped windows the
/// same edges in their new workarea. Fullscreen, maximized and tiled
/// windows follow their output and tree by themselves. Returns the new
/// floating rectangle.
fn reconfigure(
  state: &mut CompositorState,
  window: WindowId,
  from: Rect,
  to: Rect,
  geometry: MoveGeometry,
) -> Option<Rect> {
  let info = state.core.window_info(window).ok()?;
  let workarea = state
    .core
    .wset_tree(info.wset, info.location.workspace)
    .map_or(to, |tree| tree.workarea());
  if info.state.snapped.is_some() {
    let edges = info.state.tiled_edges;
    let _ = crate::shell::snap_window_to_geometry(
      state,
      window,
      crate::shell::edges_geometry(workarea, edges),
      edges,
    );
    return None;
  }
  let old = info.state.floating?;
  let new = match geometry {
    MoveGeometry::Scale => clamp_rect(scale_rect(old, from, to), workarea),
    MoveGeometry::Translate => Rect::new(
      old.x + (to.x - from.x),
      old.y + (to.y - from.y),
      old.width,
      old.height,
    ),
    MoveGeometry::Keep => old,
  };
  if new != old {
    set_floating(state, window, new);
  }
  Some(new)
}

/// `rect`'s position relative to output `from`, scaled by the ratio of the
/// outputs' sizes and placed relative to `to`, its size kept, as
/// `move_view_to_output` scales `view_g`.
pub(crate) fn scale_rect(rect: Rect, from: Rect, to: Rect) -> Rect {
  let ratio = |to: u32, from: u32| {
    if from == 0 {
      1.0
    } else {
      f64::from(to) / f64::from(from)
    }
  };
  let x = f64::from(rect.x - from.x) * ratio(to.width, from.width);
  let y = f64::from(rect.y - from.y) * ratio(to.height, from.height);
  Rect::new(
    to.x.saturating_add(x as i32),
    to.y.saturating_add(y as i32),
    rect.width,
    rect.height,
  )
}

/// `rect` moved and, if larger, shrunk into `area`, as Wayfire's
/// `wf::clamp`.
pub(crate) fn clamp_rect(rect: Rect, area: Rect) -> Rect {
  let width = rect.width.min(area.width);
  let height = rect.height.min(area.height);
  let max_x = i64::from(area.x) + i64::from(area.width) - i64::from(width);
  let max_y = i64::from(area.y) + i64::from(area.height) - i64::from(height);
  let x = i64::from(rect.x).clamp(i64::from(area.x), max_x);
  let y = i64::from(rect.y).clamp(i64::from(area.y), max_y);
  Rect::new(x as i32, y as i32, width.max(1), height.max(1))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scaling_keeps_the_relative_place_on_another_output() {
    let from = Rect::new(0, 0, 1280, 720);
    let to = Rect::new(1280, 0, 2560, 1440);
    assert_eq!(
      scale_rect(Rect::new(100, 50, 300, 200), from, to),
      Rect::new(1480, 100, 300, 200)
    );
    let back = Rect::new(0, 0, 1280, 720);
    assert_eq!(
      scale_rect(Rect::new(1480, 100, 300, 200), to, back),
      Rect::new(100, 50, 300, 200)
    );
  }

  #[test]
  fn clamping_moves_and_shrinks_into_the_area() {
    let area = Rect::new(1280, 0, 1280, 720);
    assert_eq!(
      clamp_rect(Rect::new(2400, 600, 300, 200), area),
      Rect::new(2260, 520, 300, 200)
    );
    assert_eq!(
      clamp_rect(Rect::new(0, -50, 2000, 900), area),
      Rect::new(1280, 0, 1280, 720)
    );
    assert_eq!(
      clamp_rect(Rect::new(1300, 10, 100, 100), area),
      Rect::new(1300, 10, 100, 100)
    );
  }
}
