//! Wayfire's `wsets` plugin (plugins/single_plugins/wsets.cpp;
//! docs/design/multi-output.md, section 2): showing workspace set N on an
//! output and sending a window to set N, by binding or by IPC
//! (`wsets/set-output-wset`, `wsets/send-view-to-wset`).

use shingle_core::{OutputId, WindowId, WsetId};

use super::moving::{MoveGeometry, move_window_to_wset};
use crate::{Error, state::CompositorState};

/// Shows workspace set `wset` on output `output`, as `select_workspace`
/// does: the set is created if it does not exist, an output that showed it
/// gets a new empty set, and the set `output` showed until then is parked,
/// or destroyed if nothing is left in it. The windows of both sets are told
/// their new output.
pub(crate) fn select_wset(
  state: &mut CompositorState,
  output: &OutputId,
  wset: WsetId,
) -> Result<(), Error> {
  let Some(rect) = state.outputs.get(output).map(|output| output.rect) else {
    return Err(Error::Core(shingle_core::ShingleError::OutputNotFound(
      output.as_str().into(),
    )));
  };
  state.core.ensure_wset(wset, rect)?;
  let previous = state.core.wset_of(output);
  if previous == Some(wset) {
    return Ok(());
  }
  let elsewhere = state.core.wset(wset)?.output;
  // Where the set's windows were placed: its output, else its workarea.
  let from = elsewhere
    .as_ref()
    .and_then(|other| state.outputs.get(other).map(|output| output.rect))
    .unwrap_or(state.core.wset(wset)?.workarea);
  let identity = state.outputs.get(output).and_then(|state| state.identity());
  state
    .core
    .attach_wset(wset, Some(output.clone()), identity)?;
  let mut changed = vec![(output.clone(), wset)];
  if let Some(other) = elsewhere {
    // The output that showed the set gets a new, empty one.
    let other_rect =
      state.outputs.get(&other).map_or(rect, |output| output.rect);
    let fresh = state.core.create_wset(other_rect)?;
    let identity = state.outputs.get(&other).and_then(|state| state.identity());
    state
      .core
      .attach_wset(fresh, Some(other.clone()), identity)?;
    changed.push((other, fresh));
  }
  crate::layer_shell::update_workarea(state);
  super::moving::moved_set(state, wset, from, rect);
  // The windows of `wset` and of the set `output` showed changed outputs;
  // a fresh set has none.
  for changed_set in [Some(wset), previous].into_iter().flatten() {
    output_changed_events(state, changed_set);
  }
  for (id, now) in &changed {
    crate::ipc::output_wset_changed(state, id, *now);
  }
  if let Some(previous) = previous
    && state
      .core
      .wset(previous)
      .is_ok_and(|info| info.output.is_none())
    && !super::hotplug::referenced(state, previous)
  {
    state.core.destroy_wset(previous)?;
  }
  if changed.iter().any(|(id, _)| *id == state.active_output) {
    state.input.keyboard_focus = None;
  }
  crate::input::sync_focus(state);
  crate::shell::reconfigure_all(state);
  state.events.push(crate::Event::LayoutChanged);
  Ok(())
}

/// Sends `window` and its dialogs to workspace set `wset`, created if it
/// does not exist, as `send_window_to` does: the window keeps its place
/// relative to the output, and goes out of sight if the set is parked.
pub(crate) fn send_to_wset(
  state: &mut CompositorState,
  window: WindowId,
  wset: WsetId,
) -> Result<(), Error> {
  let rect = state.active().rect;
  state.core.ensure_wset(wset, rect)?;
  move_window_to_wset(state, window, wset, MoveGeometry::Translate)?;
  crate::input::sync_focus(state);
  Ok(())
}

/// Sends `view-set-output` for every window of `wset`, whose output just
/// changed.
fn output_changed_events(state: &mut CompositorState, wset: WsetId) {
  let output = state.core.wset(wset).ok().and_then(|info| info.output);
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
    crate::ipc::view_set_output(state, window, output.as_ref());
  }
}
