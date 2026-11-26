//! The active output and the pointer between outputs
//! (docs/design/multi-output.md, section 3).
//!
//! One output is active, as Wayfire's seat has one focused output: workspace
//! bindings, IPC without an output and new windows use it. It changes, as in
//! Wayfire, on a button press or touch down on another output, when a window
//! on another output is focused, through `oswitch` and through IPC that names
//! an output, never on pointer motion alone. A change emits
//! [`crate::Event::OutputFocused`] and the IPC event `output-gain-focus`, and
//! keyboard focus goes to the focused window of the output's current
//! workspace.
//!
//! The pointer stays in layout coordinates and is kept on the layout: a
//! point off every output is pulled to the closest point of one, as
//! `wlr_output_layout_closest_point` does, so it crosses between touching
//! outputs and cannot leave the layout.

use std::time::Duration;

use shingle_core::{OutputId, OutputIdentity, Rect};

use crate::{OutputHandle, state::CompositorState};

/// The multi-output settings that act on focus and input, from
/// [`crate::Config`].
#[derive(Clone, Debug)]
pub(crate) struct FocusSettings {
  /// Wayfire's `oswitch` bindings.
  pub(crate) oswitch: shingle_config::OswitchConfig,
  /// Wayfire's `wsets` bindings.
  pub(crate) wsets: shingle_config::WsetsConfig,
  /// The output each named absolute input device maps to.
  pub(crate) device_outputs: std::collections::BTreeMap<String, String>,
  /// How long an output that went keeps its claim to be focused again.
  pub(crate) focus_timeout: Duration,
}

impl FocusSettings {
  /// The settings `config` gives.
  pub(crate) fn new(config: &crate::Config) -> Self {
    Self {
      oswitch: config.oswitch.clone(),
      wsets: config.wsets.clone(),
      device_outputs: config.input_device_outputs.clone(),
      focus_timeout: Duration::from_millis(config.last_output_focus_timeout),
    }
  }
}

/// The active output that went away, which is focused again if it comes
/// back within the focus timeout, as Wayfire's preserve-output does.
#[derive(Clone, Debug)]
pub(crate) struct LostFocus {
  /// Its name.
  pub(crate) id: OutputId,
  /// Its EDID identity, if it has one.
  pub(crate) identity: Option<OutputIdentity>,
  /// When it went, by [`CompositorState::now`].
  pub(crate) at: Duration,
}

/// Makes `id` the active output without moving keyboard focus, and reports
/// the change. Returns whether the active output changed; an output that is
/// not there, or dark, is not made active.
pub(crate) fn set_active(state: &mut CompositorState, id: &OutputId) -> bool {
  if state.active_output == *id
    || state.outputs.get(id).is_none_or(|output| output.dark)
  {
    return false;
  }
  state.active_output = id.clone();
  state.core.set_active_output(Some(id.clone()));
  state.events.push(crate::Event::OutputFocused {
    output: OutputHandle::from(id),
  });
  crate::ipc::output_event(state, "output-gain-focus", id);
  true
}

/// Makes `id` the active output and gives keyboard focus to the focused
/// window of its current workspace, as Wayfire's `seat_t::focus_output`
/// refocuses. Returns whether the active output changed.
pub(crate) fn focus_output(state: &mut CompositorState, id: &OutputId) -> bool {
  if !set_active(state, id) {
    return false;
  }
  // Lock surfaces and exclusive layer surfaces keep the keyboard, which
  // sync_focus gives them first.
  if state.idle.lock_state() == shingle_idle::LockState::Unlocked {
    state.input.keyboard_focus = None;
  }
  crate::input::sync_focus(state);
  crate::shell::reconfigure_all(state);
  true
}

/// Focuses the output under `(x, y)` in layout coordinates, for a button
/// press or a touch down there. Returns whether the active output changed.
pub(crate) fn focus_output_at(
  state: &mut CompositorState,
  x: f64,
  y: f64,
) -> bool {
  match output_at(state, x, y) {
    Some(id) => focus_output(state, &id),
    None => false,
  }
}

/// The output under `(x, y)`, first in add order where outputs overlap;
/// dark outputs are skipped.
pub(crate) fn output_at(
  state: &CompositorState,
  x: f64,
  y: f64,
) -> Option<OutputId> {
  state
    .outputs
    .iter()
    .find(|output| !output.dark && contains(output.rect, x, y))
    .map(|output| output.id.clone())
}

/// Whether `(x, y)` lies in `rect`, whose right and bottom edges are
/// exclusive.
pub(crate) fn contains(rect: Rect, x: f64, y: f64) -> bool {
  x >= f64::from(rect.x)
    && y >= f64::from(rect.y)
    && x < f64::from(rect.x) + f64::from(rect.width)
    && y < f64::from(rect.y) + f64::from(rect.height)
}

/// `(x, y)` pulled onto the closest output that is not dark
/// ([`shingle_config::closest_point`]).
pub(crate) fn clamp(state: &CompositorState, x: f64, y: f64) -> (f64, f64) {
  let rects = state
    .outputs
    .iter()
    .filter(|output| !output.dark)
    .map(|output| output.rect)
    .collect::<Vec<_>>();
  if rects.is_empty() {
    return (x, y);
  }
  shingle_config::closest_point(&rects, x, y)
}

/// Remembers that the active output `id` goes away, so that it is focused
/// again if it returns within the timeout.
pub(crate) fn output_lost(state: &mut CompositorState, id: &OutputId) {
  let identity = state.outputs.get(id).and_then(|output| output.identity());
  state.lost_focus = Some(LostFocus {
    id: id.clone(),
    identity,
    at: state.now(),
  });
}

/// Focuses output `id`, which was just added, again if it is the active
/// output that went within the focus timeout, matched by EDID identity or
/// else by name.
pub(crate) fn output_returned(state: &mut CompositorState, id: &OutputId) {
  let Some(lost) = state.lost_focus.clone() else {
    return;
  };
  let identity = state.outputs.get(id).and_then(|output| output.identity());
  let same = match (&lost.identity, &identity) {
    (Some(lost), Some(new)) => lost == new,
    _ => lost.id == *id,
  };
  if !same {
    return;
  }
  state.lost_focus = None;
  if state.now().saturating_sub(lost.at) <= state.focus_settings.focus_timeout {
    focus_output(state, id);
  }
}

/// Warps the pointer to the centre of output `id` unless it is inside it
/// already, as Wayfire's `ensure_pointer` does after `oswitch`, and tells
/// the host with [`crate::Event::PointerWarp`].
pub(crate) fn ensure_pointer(state: &mut CompositorState, id: &OutputId) {
  let Some(rect) = state.outputs.get(id).map(|output| output.rect) else {
    return;
  };
  if contains(rect, state.input.pointer_x, state.input.pointer_y) {
    return;
  }
  let x = f64::from(rect.x) + f64::from(rect.width) / 2.0;
  let y = f64::from(rect.y) + f64::from(rect.height) / 2.0;
  crate::input::pointer_warp(state, x, y);
  state.events.push(crate::Event::PointerWarp {
    x: state.input.pointer_x,
    y: state.input.pointer_y,
    output: Some(OutputHandle::from(id)),
  });
}

/// Which output an `oswitch` binding goes to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputSwitch {
  /// The next output in add order, wrapping (`next_output`).
  Next,
  /// The previous output in add order, wrapping (`prev_output`).
  Previous,
  /// The output to the left (`left_output`).
  Left,
  /// The output to the right (`right_output`).
  Right,
  /// The output above (`above_output`).
  Above,
  /// The output below (`below_output`).
  Below,
}

/// The output `switch` goes to from the active one, among the outputs that
/// are not dark, as oswitch's `get_output_relative` and
/// `get_output_in_direction` choose it. The active output itself when none
/// qualifies.
pub(crate) fn switch_target(
  state: &CompositorState,
  switch: OutputSwitch,
) -> OutputId {
  let outputs = state
    .outputs
    .iter()
    .filter(|output| !output.dark)
    .collect::<Vec<_>>();
  let Some(current) = outputs
    .iter()
    .position(|output| output.id == state.active_output)
  else {
    return state.active_output.clone();
  };
  let rects = outputs.iter().map(|output| output.rect).collect::<Vec<_>>();
  let index = match switch {
    OutputSwitch::Next => {
      shingle_config::output_relative(outputs.len(), current, 1)
    }
    OutputSwitch::Previous => {
      shingle_config::output_relative(outputs.len(), current, -1)
    }
    OutputSwitch::Left => {
      shingle_config::output_in_direction(&rects, current, -1, 0)
    }
    OutputSwitch::Right => {
      shingle_config::output_in_direction(&rects, current, 1, 0)
    }
    OutputSwitch::Above => {
      shingle_config::output_in_direction(&rects, current, 0, -1)
    }
    OutputSwitch::Below => {
      shingle_config::output_in_direction(&rects, current, 0, 1)
    }
  };
  outputs[index].id.clone()
}

/// Runs an `oswitch` binding: focuses the output `switch` names and warps
/// the pointer into it. With `with_window`, the focused window's topmost
/// parent goes along first, as `move_view_to_output` with `RECONFIGURE`
/// moves it ([`crate::output::move_window_to_output`]). Returns the output
/// that is active afterwards.
pub(crate) fn oswitch(
  state: &mut CompositorState,
  switch: OutputSwitch,
  with_window: bool,
) -> OutputId {
  let target = switch_target(state, switch);
  if with_window
    && target != state.active_output
    && let Some(window) = crate::input::focused_window(state)
  {
    let window = crate::output::topmost_parent(state, window);
    if let Err(error) = crate::output::move_window_to_output(
      state,
      window,
      &target,
      crate::output::MoveGeometry::Scale,
    ) {
      eprintln!("shingle: moving a window to {target:?} failed: {error}");
    }
  }
  focus_output(state, &target);
  ensure_pointer(state, &target);
  state.active_output.clone()
}
