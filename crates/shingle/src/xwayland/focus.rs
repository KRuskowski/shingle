//! What the X window manager hears of Shingle's keyboard focus and
//! stacking once per turn (docs/design/xwayland.md, sections 2 and 5).
//!
//! Keyboard focus reaches an X window's `wl_surface` as for any client;
//! Xwayland then hands keys to the X focus window, which the window
//! manager sets with [`shingle_xwayland::Xwm::set_focus`]. Stacking
//! follows Shingle's, so that X's idea of what is on top matches what is
//! drawn.

use std::collections::HashMap;

use shingle_core::{StackingMode, WindowId};
use shingle_xwayland::Window;

use crate::{
  shell::{Role, ToplevelHandle},
  state::CompositorState,
};

/// The X focus for Shingle's keyboard focus: the managed X window that
/// has it, or no X window at all (`None`) when the keyboard is on a
/// Wayland surface or nowhere.
pub(crate) fn target(state: &CompositorState) -> Option<Window> {
  let surface = state
    .input
    .keyboard_focus
    .as_ref()
    .and_then(|object| state.surfaces.get(object))?;
  match &surface.role {
    Role::Xwayland(role) if role.toplevel.is_some() => role.window,
    _ => None,
  }
}

/// The managed X windows in Shingle's stacking order, bottom to top:
/// windows on other workspaces and outputs first, then those of the
/// current workspace, each tree by stacking mode (always-on-bottom,
/// normal, always-on-top) and stack order, as they are drawn.
pub(crate) fn stacking(state: &CompositorState) -> Vec<Window> {
  let x_windows = state
    .surfaces
    .values()
    .filter_map(|surface| {
      let toplevel = surface.role.toplevel()?;
      let ToplevelHandle::X11(window) = toplevel.handle else {
        return None;
      };
      Some((toplevel.window()?, window))
    })
    .collect::<HashMap<WindowId, Window>>();
  if x_windows.is_empty() {
    return Vec::new();
  }
  let current = state.core.current_workspace(&state.output_id).ok();
  let mut trees = Vec::new();
  for output in state.core.outputs() {
    for workspace in state.core.workspaces(output).unwrap_or_default() {
      let is_current = *output == state.output_id && Some(workspace) == current;
      if let Ok(tree) = state.core.tree(output, workspace) {
        trees.push((is_current, tree));
      }
    }
  }
  // The current workspace last, so on top.
  trees.sort_by_key(|(is_current, _)| *is_current);
  let mut order = Vec::new();
  for (_, tree) in trees {
    for mode in [
      StackingMode::AlwaysOnBottom,
      StackingMode::Normal,
      StackingMode::AlwaysOnTop,
    ] {
      for window in tree.stack_order() {
        let in_mode = tree
          .leaf_state(*window)
          .is_ok_and(|leaf| leaf.stacking == mode);
        if in_mode && let Some(x_window) = x_windows.get(window) {
          order.push(*x_window);
        }
      }
    }
  }
  order
}
