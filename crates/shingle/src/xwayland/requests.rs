//! What X clients ask of Shingle in their client messages, and what a
//! managed X window gets when it maps (docs/design/xwayland.md, sections
//! 2, 4 and 5).
//!
//! Client messages reach the same functions as their xdg counterparts:
//! `_NET_WM_STATE` and `WM_CHANGE_STATE` go to `set_fullscreen`,
//! `set_maximized` and `set_minimized`, `_NET_ACTIVE_WINDOW` to
//! `focus_window`, `_NET_WM_MOVERESIZE` to `Event::BeginMove` and
//! `Event::BeginResize`, and `_NET_CLOSE_WINDOW` to `close_window`. At map
//! time a window follows Wayfire: transients and fixed-size windows float
//! (the tile plugin's `can_tile_view`), a transient is centred on its
//! parent (`reposition_relative_to_parent`), the state the client asked
//! for before mapping is applied, and the ICCCM input model decides
//! whether the window takes the focus (`wayfire_xwayland_view::map`).

use shingle_core::{Rect, WindowId};
use shingle_xwayland::{MoveResize, StateChange, Window, WindowInfo};
use wayland_server::backend::ObjectId;

use super::windows::{self, XRequest};
use crate::{
  Event,
  shell::{self, ToplevelHandle},
  state::CompositorState,
};

/// Grants a `_NET_WM_STATE` or `WM_CHANGE_STATE` request. Before the X
/// window is a Shingle window, the change is kept as its initial state.
pub(crate) fn state(
  state: &mut CompositorState,
  window: Window,
  change: StateChange,
) {
  let Some(shingle) = windows::shingle_window(state, window) else {
    if let Some(entry) = state.xwayland.windows.get_mut(&window) {
      let initial = &mut entry.info.state;
      if let Some(fullscreen) = change.fullscreen {
        initial.fullscreen = fullscreen;
      }
      if let Some(maximized) = change.maximized {
        initial.maximized_vert = maximized;
        initial.maximized_horz = maximized;
      }
      if let Some(minimized) = change.minimized {
        initial.hidden = minimized;
      }
    }
    return;
  };
  if let Some(fullscreen) = change.fullscreen {
    let _ = shell::set_fullscreen(state, shingle, fullscreen);
  }
  if let Some(maximized) = change.maximized {
    let _ = shell::set_maximized(state, shingle, maximized);
  }
  if let Some(minimized) = change.minimized {
    let _ = shell::set_minimized(state, shingle, minimized);
  }
}

/// `_NET_ACTIVE_WINDOW`: focuses the window if it is mapped and not
/// focused already, as Wayfire's `on_request_activate` does.
pub(crate) fn activate(state: &mut CompositorState, window: Window) {
  let Some(shingle) = windows::shingle_window(state, window) else {
    return;
  };
  if crate::input::focused_window(state) == Some(shingle) {
    return;
  }
  crate::input::focus_window(state, shingle);
}

/// `_NET_WM_MOVERESIZE`: the interactive move or resize an xdg toplevel's
/// `move` and `resize` start.
pub(crate) fn move_resize(
  state: &mut CompositorState,
  window: Window,
  request: MoveResize,
) {
  let Some(shingle) = windows::shingle_window(state, window) else {
    return;
  };
  state.events.push(match request {
    MoveResize::Move => Event::BeginMove { window: shingle },
    MoveResize::Resize(edges) => Event::BeginResize {
      window: shingle,
      edges,
    },
  });
}

/// `_NET_CLOSE_WINDOW`: the same as `close-view`.
pub(crate) fn close(state: &mut CompositorState, window: Window) {
  if let Some(shingle) = windows::shingle_window(state, window) {
    shell::close_window(state, shingle);
  }
}

/// The X window and what the window manager knows of it, for the managed
/// X toplevel on `object`.
fn x_window<'a>(
  state: &'a CompositorState,
  object: &ObjectId,
) -> Option<(Window, &'a WindowInfo)> {
  let toplevel = state.surfaces.get(object)?.role.toplevel()?;
  let ToplevelHandle::X11(window) = toplevel.handle else {
    return None;
  };
  let entry = state.xwayland.windows.get(&window)?;
  Some((window, &entry.info))
}

/// What the window manager knows of the X window behind Shingle window
/// `window`, if it is one.
fn x_window_of(
  state: &CompositorState,
  window: WindowId,
) -> Option<&WindowInfo> {
  state.surfaces.values().find_map(|surface| {
    let toplevel = surface.role.toplevel()?;
    let ToplevelHandle::X11(x_window) = toplevel.handle else {
      return None;
    };
    if toplevel.window() != Some(window) {
      return None;
    }
    Some(&state.xwayland.windows.get(&x_window)?.info)
  })
}

/// Wayfire's `can_tile_view` for X toplevels: a window with a parent, or
/// with a fixed size (equal positive minimum and maximum), floats. Other
/// toplevels are not affected.
pub(crate) fn can_tile(state: &CompositorState, object: &ObjectId) -> bool {
  let Some(toplevel) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
  else {
    return true;
  };
  if !matches!(toplevel.handle, ToplevelHandle::X11(_)) {
    return true;
  }
  let (min, max) = (toplevel.min_size, toplevel.max_size);
  let fixed = min == max && min.0 > 0 && min.1 > 0;
  toplevel.parent.is_none() && !fixed
}

/// Floats a new X toplevel that may not tile. A transient is centred on
/// its parent and kept inside the workarea, as Wayfire's
/// `reposition_relative_to_parent` does; any other window floats where
/// placement puts it.
pub(crate) fn float(
  state: &mut CompositorState,
  object: &ObjectId,
  window: WindowId,
) {
  let Some(size) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
    .filter(|toplevel| toplevel.parent.is_some())
    .map(|toplevel| toplevel.floating_geometry)
  else {
    let _ = shell::toggle_tiling(state, window);
    return;
  };
  // Out of the tree first, so that the parent has its own tile again.
  let _ = shell::set_floating_geometry(state, window, size);
  if let Some(geometry) = transient_geometry(state, object, window) {
    let _ = shell::set_floating_geometry(state, window, geometry);
  }
}

/// Where a transient X toplevel floats: its size, centred on its parent's
/// current geometry and clamped into the parent's workarea. `None` for
/// windows without a parent.
fn transient_geometry(
  state: &CompositorState,
  object: &ObjectId,
  window: WindowId,
) -> Option<Rect> {
  let toplevel = state.surfaces.get(object)?.role.toplevel()?;
  let parent = toplevel.parent?;
  let size = toplevel.floating_geometry;
  let parent_object = state.surfaces.iter().find_map(|(object, surface)| {
    surface
      .role
      .toplevel()
      .is_some_and(|candidate| candidate.window() == Some(parent))
      .then(|| object.clone())
  })?;
  let parent_geometry = shell::surface_geometry(state, &parent_object)?;
  let workarea = state
    .core
    .window_location(window)
    .ok()
    .and_then(|location| {
      state.core.tree(&location.output, location.workspace).ok()
    })
    .map_or(state.active().rect, |tree| tree.workarea());
  Some(centred(parent_geometry, size, workarea))
}

/// `size`'s width and height centred on `parent`, then shrunk and moved
/// into `workarea`.
fn centred(parent: Rect, size: Rect, workarea: Rect) -> Rect {
  let half = |outer: u32, inner: u32| (i64::from(outer) - i64::from(inner)) / 2;
  let x = i64::from(parent.x) + half(parent.width, size.width);
  let y = i64::from(parent.y) + half(parent.height, size.height);
  let clamp = |value: i64| value.clamp(i32::MIN.into(), i32::MAX.into());
  let geometry = shingle_xwayland::xwm::policy::clamp(
    shingle_xwayland::Geometry::new(
      clamp(x) as i32,
      clamp(y) as i32,
      size.width,
      size.height,
    ),
    windows::geometry(workarea),
  );
  Rect::new(geometry.x, geometry.y, geometry.width, geometry.height)
}

/// Finishes mapping a new X toplevel. It gets the `_NET_WM_STATE` it
/// asked for before it mapped, as Wayfire's `handle_map_request` does:
/// fullscreen, and maximized when either direction is set; hidden
/// minimizes it. A globally active `NORMAL` or `DIALOG` window is offered
/// the focus with `WM_TAKE_FOCUS`, as Wayfire's `map` does.
pub(crate) fn mapped(
  state: &mut CompositorState,
  object: &ObjectId,
  window: WindowId,
) {
  let Some((x_window, info)) = x_window(state, object) else {
    return;
  };
  let initial = info.state;
  if info.offered_focus_on_map() {
    state.xwayland.requests.push(XRequest::OfferFocus(x_window));
  }
  if initial.fullscreen {
    let _ = shell::set_fullscreen(state, window, true);
  }
  if initial.maximized_vert || initial.maximized_horz {
    let _ = shell::set_maximized(state, window, true);
  }
  if initial.hidden {
    let _ = shell::set_minimized(state, window, true);
  }
}

/// Whether a newly mapped window takes the keyboard focus. X windows
/// follow their input model: passive and locally active ones do;
/// globally active ones are at most offered it ([`mapped`]) and no-input
/// ones are not focused. Every other window is.
pub(crate) fn focus_on_map(state: &CompositorState, window: WindowId) -> bool {
  x_window_of(state, window).is_none_or(WindowInfo::focused_on_map)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn transients_are_centred_on_their_parent_inside_the_workarea() {
    let workarea = Rect::new(0, 0, 1280, 720);
    let parent = Rect::new(0, 0, 640, 720);
    assert_eq!(
      centred(parent, Rect::new(0, 0, 200, 100), workarea),
      Rect::new(220, 310, 200, 100)
    );
    // Bigger than its parent: still centred, and kept on screen.
    assert_eq!(
      centred(parent, Rect::new(0, 0, 1000, 100), workarea),
      Rect::new(0, 310, 1000, 100)
    );
    // Bigger than the workarea: shrunk to fit.
    assert_eq!(
      centred(parent, Rect::new(0, 0, 2000, 2000), workarea),
      workarea
    );
  }
}
