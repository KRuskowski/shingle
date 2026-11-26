//! Outputs coming and going: adding one with its workspace set, removing
//! one under `output_layout.unplug`, and settling a restored session's
//! sets once the backend has added its outputs
//! (docs/design/multi-output.md, sections 5 and 6).

use shingle_config::UnplugPolicy;
use shingle_core::{OutputId, Rect, ShingleError, WsetId};

use super::{
  OutputHandle, OutputInfo, OutputKind, claim_parked, create_global, moved,
  relayout, relayout_and_notify, retire_global,
};
use crate::{Error, state::CompositorState};

/// Adds an output of `physical` pixels described by `info`, placed and
/// scaled by its configuration entry, and shows on it the parked set that
/// belongs to it or a new one. If every other output is dark, they are
/// removed afterwards, so their windows join the new output.
pub(crate) fn add(
  state: &mut CompositorState,
  mut info: OutputInfo,
  physical: (u32, u32),
  kind: OutputKind,
) -> Result<OutputId, Error> {
  if info.name.is_empty() {
    return Err(Error::Core(ShingleError::InvalidOutputId));
  }
  let id = OutputId::new(info.name.clone());
  if let Some(existing) = state.outputs.get(&id) {
    if existing.dark {
      return revive(state, &id, info, physical).map(|()| id);
    }
    return Err(Error::OutputExists(info.name));
  }
  // A backend that adds an output that was turned off replugged it, or
  // turned it on: it takes its old place in the add order.
  let disabled = state.outputs.take_disabled(&id);
  let placement = state.placements.get(&info.name);
  if !placement.scale.is_finite() || placement.scale <= 0.0 {
    return Err(Error::InvalidOutputScale);
  }
  let (width, height) = crate::output_transform::logical_size(
    physical.0,
    physical.1,
    placement.scale,
    placement.transform,
  );
  let rect = Rect::new(0, 0, width, height);
  rect.validate()?;
  if info.modes.is_empty() && placement.refresh_mhz > 0 {
    info.refresh_mhz = placement.refresh_mhz;
  }
  let dark = if state.outputs.iter().all(|output| output.dark) {
    state
      .outputs
      .iter()
      .map(|output| output.id.clone())
      .collect::<Vec<_>>()
  } else {
    Vec::new()
  };
  state.core.add_output(id.clone(), rect)?;
  state.outputs.push(
    info,
    rect,
    placement.scale,
    placement.transform,
    placement.position,
    kind,
  );
  if let Some(off) = disabled {
    state.outputs.move_to(&id, off.index);
  }
  create_global(state, &id);
  for other in relayout(state) {
    if other != id {
      moved(state, &other);
    }
  }
  let output = state.outputs.get(&id).expect("the output was just added");
  let (rect, identity) = (output.rect, output.identity());
  state.core.set_output_workarea(&id, rect)?;
  if let Some(wset) = state.core.wset_of(&id) {
    state
      .core
      .attach_wset(wset, Some(id.clone()), identity.clone())?;
  }
  claim_parked(state, &id, identity);
  crate::output_management::output_added(state, &id);
  state.events.push(crate::Event::OutputAdded {
    output: OutputHandle::from(&id),
  });
  crate::ipc::output_event(state, "output-added", &id);
  for dark in dark {
    remove(state, &dark)?;
  }
  super::focus::output_returned(state, &id);
  crate::shell::reconfigure_all(state);
  Ok(id)
}

/// Lights the dark output `id` again: the backend added the output of
/// that name back after it reported the last one gone, as when the only
/// monitor is plugged in again. The output keeps its workspace set,
/// global and IPC id, takes the new description and size, and gets frames
/// again.
fn revive(
  state: &mut CompositorState,
  id: &OutputId,
  info: OutputInfo,
  physical: (u32, u32),
) -> Result<(), Error> {
  let Some(output) = state.outputs.get_mut(id) else {
    return Ok(());
  };
  output.dark = false;
  let modes_changed = output.info.modes != info.modes;
  let (scale, transform, rect) = (output.scale, output.transform, output.rect);
  state.set_info_of(id, info);
  if modes_changed {
    crate::output_management::modes_changed(state, id);
  }
  let (width, height) = crate::output_transform::logical_size(
    physical.0, physical.1, scale, transform,
  );
  state.set_rect_of(id, Rect::new(rect.x, rect.y, width, height), false)?;
  crate::shell::reconfigure_all(state);
  Ok(())
}

/// Lists an output of `physical` pixels described by `info` that the
/// backend has but does not light, as a disabled output-management head
/// at the end of the add order. Does nothing if an output of that name
/// is there or listed already.
pub(crate) fn add_disabled(
  state: &mut CompositorState,
  info: OutputInfo,
  physical: (u32, u32),
) -> Result<(), Error> {
  if info.name.is_empty() {
    return Err(Error::Core(ShingleError::InvalidOutputId));
  }
  let id = OutputId::new(info.name.clone());
  if state.outputs.get(&id).is_some() || state.outputs.disabled(&id).is_some() {
    return Ok(());
  }
  let placement = state.placements.get(&info.name);
  let index = state.outputs.iter().count();
  state.outputs.disabled.push(super::DisabledOutput {
    info,
    physical,
    kind: OutputKind::Backend,
    position: placement.position,
    scale: placement.scale,
    transform: placement.transform,
    index,
  });
  crate::output_management::output_added(state, &id);
  Ok(())
}

/// Turns output `id` off for the backend: it is removed under
/// `output_layout.unplug` and stays listed as a disabled head. The last
/// output that is not dark is only marked dark, as [`remove`] keeps it.
pub(crate) fn disable(
  state: &mut CompositorState,
  id: &OutputId,
) -> Result<(), Error> {
  let (Some(output), Some(index)) =
    (state.outputs.get(id), state.outputs.index_of(id))
  else {
    return Err(Error::Core(ShingleError::OutputNotFound(
      id.as_str().into(),
    )));
  };
  let last = !state
    .outputs
    .iter()
    .any(|other| other.id != *id && !other.dark);
  if !last {
    let off = super::DisabledOutput::new(output, index);
    state.outputs.disabled.push(off);
  }
  remove(state, id)
}

/// Removes output `id`. Its workspace set is handled by
/// `output_layout.unplug` (always `move` for an output created over IPC),
/// the active output passes to the next one in add order if it was the
/// active one, and clients see its global go. The last output that is not
/// dark is kept instead, marked dark: it stays described, draws nothing and
/// gets no frames until another output arrives.
pub(crate) fn remove(
  state: &mut CompositorState,
  id: &OutputId,
) -> Result<(), Error> {
  let Some(output) = state.outputs.get(id) else {
    // A turned-off output whose monitor or window is gone now.
    if state.outputs.take_disabled(id).is_some() {
      crate::output_management::output_removed(state, id);
      return Ok(());
    }
    return Err(Error::Core(ShingleError::OutputNotFound(
      id.as_str().into(),
    )));
  };
  let kind = output.kind;
  if !state
    .outputs
    .iter()
    .any(|other| other.id != *id && !other.dark)
  {
    if let Some(output) = state.outputs.get_mut(id) {
      output.dark = true;
    }
    return Ok(());
  }
  let policy = match kind {
    OutputKind::Virtual => UnplugPolicy::Move,
    OutputKind::Backend => state.unplug,
  };
  let removed = crate::ipc::output_json(state, id);
  if state.active_output == *id {
    super::focus::output_lost(state, id);
    let next = next_output(state, id);
    super::focus::set_active(state, &next);
  }
  if let Some(wset) = state.core.wset_of(id) {
    let target = state.core.wset_of(&state.active_output);
    unplug_wset(state, wset, target, policy)?;
  }
  crate::layer_shell::output_removed(state, id);
  crate::shell::lock_output_removed(state, id);
  crate::output_power::output_removed(state, id);
  crate::gamma_control::output_removed(state, id);
  crate::wayfire_shell::output_removed(state, id);
  retire_global(state, id);
  crate::output_management::output_removed(state, id);
  state.outputs.remove(id);
  relayout_and_notify(state);
  crate::layer_shell::update_workarea(state);
  state.events.push(crate::Event::OutputRemoved {
    output: OutputHandle::from(id),
  });
  state.ipc_events.push(serde_json::json!({
    "event": "output-removed",
    "output": removed,
  }));
  crate::shell::reconfigure_all(state);
  crate::shell::finish_lock(state);
  crate::input::sync_focus(state);
  Ok(())
}

/// The output after `id` in add order, wrapping, that is not dark, as
/// Wayfire's `get_next_output` chooses it.
fn next_output(state: &CompositorState, id: &OutputId) -> OutputId {
  let outputs = state.outputs.iter().collect::<Vec<_>>();
  let start = outputs
    .iter()
    .position(|output| output.id == *id)
    .unwrap_or(0);
  (1..=outputs.len())
    .map(|step| outputs[(start + step) % outputs.len()])
    .find(|output| output.id != *id && !output.dark)
    .map_or_else(|| id.clone(), |output| output.id.clone())
}

/// Detaches `wset` from its output under `policy`: `move` and `return`
/// move its windows into `target`, reconfigured for the target output as
/// Wayfire's `transfer_views` moves them, `return` keeping a copy of its
/// trees for their return, and `park` keeps them in the set. A set that
/// nothing can return to is destroyed.
fn unplug_wset(
  state: &mut CompositorState,
  wset: WsetId,
  target: Option<WsetId>,
  policy: UnplugPolicy,
) -> Result<(), Error> {
  if let Some(target) = target.filter(|target| *target != wset)
    && policy != UnplugPolicy::Park
  {
    let from = shown_rect(state, wset);
    let to = shown_rect(state, target);
    let moved =
      state
        .core
        .transfer_wset(wset, target, policy == UnplugPolicy::Return)?;
    super::moving::transferred(state, &moved, wset, from, to);
  }
  // Parking keeps the set's home and identity.
  state.core.attach_wset(wset, None, None)?;
  if policy != UnplugPolicy::Park && !referenced(state, wset) {
    state.core.destroy_wset(wset)?;
  }
  Ok(())
}

/// The rectangle of the output showing `wset`, else its workarea.
fn shown_rect(state: &CompositorState, wset: WsetId) -> Rect {
  state
    .core
    .wset(wset)
    .map(|info| {
      info
        .output
        .and_then(|id| state.outputs.get(&id).map(|output| output.rect))
        .unwrap_or(info.workarea)
    })
    .unwrap_or(state.active().rect)
}

/// Whether a parked `wset` still holds windows, a copy of its trees or
/// windows that are away from it.
pub(super) fn referenced(state: &CompositorState, wset: WsetId) -> bool {
  state
    .core
    .wset(wset)
    .is_ok_and(|info| info.keeps_return_copy)
    || state.core.window_ids().any(|window| {
      state
        .core
        .window_info(window)
        .is_ok_and(|info| info.wset == wset || info.away_from == Some(wset))
    })
}

/// Settles a restored session once the backend has added its outputs:
/// every parked set that holds windows and that no output took is handled
/// as if its output had just been unplugged, and the saved active output
/// is focused again if it is there. Runs once.
pub(crate) fn restore(state: &mut CompositorState) {
  if state.outputs_restored {
    return;
  }
  state.outputs_restored = true;
  if let Some(saved) = state.core.active_output().cloned()
    && state.outputs.get(&saved).is_some_and(|output| !output.dark)
  {
    state.active_output = saved;
  }
  let parked = state
    .core
    .wsets()
    .filter(|wset| wset.output.is_none())
    .map(|wset| wset.id)
    .collect::<Vec<_>>();
  let target = state.core.wset_of(&state.active_output);
  for wset in parked {
    let holds_windows = state.core.window_ids().any(|window| {
      state
        .core
        .window_info(window)
        .is_ok_and(|info| info.wset == wset)
    });
    let result = if holds_windows {
      unplug_wset(state, wset, target, state.unplug)
    } else if state.unplug != UnplugPolicy::Park && !referenced(state, wset) {
      state.core.destroy_wset(wset).map_err(Error::Core)
    } else {
      Ok(())
    };
    if let Err(error) = result {
      eprintln!(
        "shingle: restoring workspace set {} failed: {error}",
        wset.0
      );
    }
  }
  crate::shell::reconfigure_all(state);
  crate::input::sync_focus(state);
}

/// Parks every workspace set of a restored model, keeping each set's home
/// and identity, so that outputs take their sets as they are added.
pub(crate) fn park_restored(core: &mut shingle_core::Shingle) {
  let shown = core
    .wsets()
    .filter(|wset| wset.output.is_some())
    .map(|wset| wset.id)
    .collect::<Vec<_>>();
  for wset in shown {
    let _ = core.attach_wset(wset, None, None);
  }
}
