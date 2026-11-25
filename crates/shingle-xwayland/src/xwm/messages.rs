//! What the EWMH and ICCCM client messages a window sends to the root ask
//! for, as wlroots' `xwm_handle_net_wm_state_message`,
//! `xwm_handle_net_wm_moveresize_message` and
//! `xwm_handle_wm_change_state_message` read them (xwayland/xwm.c).

use super::{
  atoms::{Atoms, Name},
  window::NetWmState,
};

/// `_NET_WM_STATE` actions (EWMH, "_NET_WM_STATE").
const REMOVE: u32 = 0;
const ADD: u32 = 1;
const TOGGLE: u32 = 2;

/// ICCCM `WM_STATE` values a `WM_CHANGE_STATE` message can ask for.
const NORMAL_STATE: u32 = 1;
const ICONIC_STATE: u32 = 3;

/// `_NET_WM_MOVERESIZE` directions (EWMH, "_NET_WM_MOVERESIZE").
const SIZE_TOPLEFT: u32 = 0;
const SIZE_TOP: u32 = 1;
const SIZE_TOPRIGHT: u32 = 2;
const SIZE_RIGHT: u32 = 3;
const SIZE_BOTTOMRIGHT: u32 = 4;
const SIZE_BOTTOM: u32 = 5;
const SIZE_BOTTOMLEFT: u32 = 6;
const SIZE_LEFT: u32 = 7;
const MOVE: u32 = 8;

/// Edges as `xdg_toplevel.resize_edge` numbers them, which are also
/// wlroots' `wlr_edges` bits.
pub mod edges {
  /// The top edge.
  pub const TOP: u32 = 1;
  /// The bottom edge.
  pub const BOTTOM: u32 = 2;
  /// The left edge.
  pub const LEFT: u32 = 4;
  /// The right edge.
  pub const RIGHT: u32 = 8;
}

/// What a window asks Shingle to change about its state. `None` leaves
/// that state alone.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StateChange {
  /// Fullscreen on or off.
  pub fullscreen: Option<bool>,
  /// Maximized (both directions) on or off.
  pub maximized: Option<bool>,
  /// Minimized (`_NET_WM_STATE_HIDDEN`, or `WM_CHANGE_STATE`) on or off.
  pub minimized: Option<bool>,
}

impl StateChange {
  /// Whether anything changes.
  pub fn is_empty(&self) -> bool {
    *self == Self::default()
  }
}

/// Applies one `_NET_WM_STATE` action to `value`; `None` for an unknown
/// action.
fn apply(action: u32, value: bool) -> Option<bool> {
  match action {
    REMOVE => Some(false),
    ADD => Some(true),
    TOGGLE => Some(!value),
    _ => None,
  }
}

/// The state after a `_NET_WM_STATE` client message with `data` (action,
/// first and second property), starting from `current`. Like wlroots,
/// maximized means both directions, and only fullscreen, the two
/// maximized directions, hidden and modal are understood.
pub fn net_wm_state(
  atoms: &Atoms,
  current: NetWmState,
  data: [u32; 5],
) -> NetWmState {
  let [action, first, second, ..] = data;
  let mut state = current;
  for property in [first, second] {
    let slot = if property == atoms.get(Name::NET_WM_STATE_FULLSCREEN) {
      &mut state.fullscreen
    } else if property == atoms.get(Name::NET_WM_STATE_MAXIMIZED_VERT) {
      &mut state.maximized_vert
    } else if property == atoms.get(Name::NET_WM_STATE_MAXIMIZED_HORZ) {
      &mut state.maximized_horz
    } else if property == atoms.get(Name::NET_WM_STATE_HIDDEN) {
      &mut state.hidden
    } else if property == atoms.get(Name::NET_WM_STATE_MODAL) {
      &mut state.modal
    } else {
      continue;
    };
    if let Some(value) = apply(action, *slot) {
      *slot = value;
    }
  }
  state
}

/// What changed between two states, as requests: fullscreen, maximized
/// in both directions, and hidden as minimized.
pub fn state_change(before: NetWmState, after: NetWmState) -> StateChange {
  let maximized =
    |state: NetWmState| state.maximized_vert && state.maximized_horz;
  let changed = |before: bool, after: bool| (before != after).then_some(after);
  StateChange {
    fullscreen: changed(before.fullscreen, after.fullscreen),
    maximized: changed(maximized(before), maximized(after)),
    minimized: changed(before.hidden, after.hidden),
  }
}

/// What a `WM_CHANGE_STATE` message with `state` asks: `IconicState`
/// minimizes, `NormalState` restores, anything else is ignored.
pub fn change_state(state: u32) -> Option<bool> {
  match state {
    ICONIC_STATE => Some(true),
    NORMAL_STATE => Some(false),
    _ => None,
  }
}

/// What a `_NET_WM_MOVERESIZE` message asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveResize {
  /// An interactive move.
  Move,
  /// An interactive resize from these [`edges`].
  Resize(u32),
}

/// The request for a `_NET_WM_MOVERESIZE` direction: codes 0 to 7 resize
/// from an edge or corner, 8 moves. The keyboard variants (9, 10) and
/// cancel (11) are ignored, as wlroots ignores them.
pub fn moveresize(direction: u32) -> Option<MoveResize> {
  use edges::{BOTTOM, LEFT, RIGHT, TOP};
  let edges = match direction {
    SIZE_TOPLEFT => TOP | LEFT,
    SIZE_TOP => TOP,
    SIZE_TOPRIGHT => TOP | RIGHT,
    SIZE_RIGHT => RIGHT,
    SIZE_BOTTOMRIGHT => BOTTOM | RIGHT,
    SIZE_BOTTOM => BOTTOM,
    SIZE_BOTTOMLEFT => BOTTOM | LEFT,
    SIZE_LEFT => LEFT,
    MOVE => return Some(MoveResize::Move),
    _ => return None,
  };
  Some(MoveResize::Resize(edges))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::xwm::atoms::NAMES;

  fn atoms() -> Atoms {
    Atoms::from_values((1000..1000 + NAMES.len() as u32).collect()).unwrap()
  }

  #[test]
  fn net_wm_state_adds_removes_and_toggles() {
    let atoms = atoms();
    let fullscreen = atoms.get(Name::NET_WM_STATE_FULLSCREEN);
    let hidden = atoms.get(Name::NET_WM_STATE_HIDDEN);
    let start = NetWmState::default();
    let added = net_wm_state(&atoms, start, [ADD, fullscreen, 0, 1, 0]);
    assert!(added.fullscreen);
    assert_eq!(
      state_change(start, added),
      StateChange {
        fullscreen: Some(true),
        ..StateChange::default()
      }
    );
    let removed = net_wm_state(&atoms, added, [REMOVE, fullscreen, 0, 1, 0]);
    assert_eq!(removed, start);
    let toggled =
      net_wm_state(&atoms, added, [TOGGLE, fullscreen, hidden, 1, 0]);
    assert!(!toggled.fullscreen && toggled.hidden);
    assert_eq!(
      state_change(added, toggled),
      StateChange {
        fullscreen: Some(false),
        minimized: Some(true),
        ..StateChange::default()
      }
    );
    // Adding what is already set changes nothing.
    assert!(
      state_change(
        added,
        net_wm_state(&atoms, added, [ADD, fullscreen, 0, 1, 0])
      )
      .is_empty()
    );
  }

  #[test]
  fn maximized_needs_both_directions() {
    let atoms = atoms();
    let vert = atoms.get(Name::NET_WM_STATE_MAXIMIZED_VERT);
    let horz = atoms.get(Name::NET_WM_STATE_MAXIMIZED_HORZ);
    let start = NetWmState::default();
    let one = net_wm_state(&atoms, start, [ADD, vert, 0, 1, 0]);
    assert!(state_change(start, one).is_empty());
    let both = net_wm_state(&atoms, one, [ADD, horz, 0, 1, 0]);
    assert_eq!(state_change(start, both).maximized, Some(true));
    let together = net_wm_state(&atoms, start, [ADD, vert, horz, 1, 0]);
    assert_eq!(state_change(start, together).maximized, Some(true));
    let unmax = net_wm_state(&atoms, both, [REMOVE, horz, 0, 1, 0]);
    assert_eq!(state_change(both, unmax).maximized, Some(false));
  }

  #[test]
  fn unknown_actions_and_atoms_are_ignored() {
    let atoms = atoms();
    let fullscreen = atoms.get(Name::NET_WM_STATE_FULLSCREEN);
    let sticky = atoms.get(Name::NET_WM_STATE_STICKY);
    let start = NetWmState::default();
    assert_eq!(net_wm_state(&atoms, start, [7, fullscreen, 0, 1, 0]), start);
    assert_eq!(net_wm_state(&atoms, start, [ADD, sticky, 0, 1, 0]), start);
    // Modal is tracked but is no request.
    let modal = atoms.get(Name::NET_WM_STATE_MODAL);
    let state = net_wm_state(&atoms, start, [ADD, modal, 0, 1, 0]);
    assert!(state.modal);
    assert!(state_change(start, state).is_empty());
  }

  #[test]
  fn wm_change_state_minimizes_or_restores() {
    assert_eq!(change_state(3), Some(true));
    assert_eq!(change_state(1), Some(false));
    assert_eq!(change_state(0), None);
    assert_eq!(change_state(2), None);
  }

  #[test]
  fn moveresize_maps_directions_to_edges() {
    use edges::*;
    let expected = [
      (0, TOP | LEFT),
      (1, TOP),
      (2, TOP | RIGHT),
      (3, RIGHT),
      (4, BOTTOM | RIGHT),
      (5, BOTTOM),
      (6, BOTTOM | LEFT),
      (7, LEFT),
    ];
    for (direction, edges) in expected {
      assert_eq!(moveresize(direction), Some(MoveResize::Resize(edges)));
    }
    assert_eq!(moveresize(8), Some(MoveResize::Move));
    // Keyboard move and resize, cancel and unknown codes.
    for direction in [9, 10, 11, 12, u32::MAX] {
      assert_eq!(moveresize(direction), None);
    }
    // The bits are xdg_toplevel's resize_edge values.
    assert_eq!(TOP | LEFT, 5);
    assert_eq!(BOTTOM | RIGHT, 10);
  }
}
