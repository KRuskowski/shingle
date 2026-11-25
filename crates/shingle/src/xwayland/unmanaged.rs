//! Unmanaged X windows: override-redirect menus, tooltips and splash
//! screens, and Wayfire's parented popups (docs/design/xwayland.md,
//! section 2).
//!
//! They are not Shingle windows. While their X window is mapped they are
//! drawn in the Unmanaged layer, above Top and below Overlay, at the
//! position the X client chose, as [`crate::SurfaceRole::Unmanaged`]
//! snapshots, later ones on top. They take pointer input, and keyboard
//! focus only as Wayfire's unmanaged views do: when they map, if they are
//! not override-redirect or wlroots' heuristic says they want it, and
//! their input model is not no-input. While one has the keyboard, the X
//! focus stays on the X toplevel that had it ([`super::focus`]).

use shingle_core::Rect;
use shingle_xwayland::Window;
use wayland_server::backend::ObjectId;

use super::shell::{UnmanagedRole, XwaylandRole};
use crate::{
  shell::Role,
  state::{CompositorState, Surface},
};

/// Starts drawing the unmanaged X window `window` on its surface `object`,
/// and focuses it now if it wants focus and already has a buffer, or else
/// on its first buffer ([`committed`]).
pub(crate) fn map(
  state: &mut CompositorState,
  window: Window,
  object: &ObjectId,
  from_commit: bool,
) {
  let Some(entry) = state.xwayland.windows.get(&window) else {
    return;
  };
  let keyboard = entry.info.unmanaged_accepts_keyboard();
  let focus = entry.info.unmanaged_focused_on_map();
  state.xwayland.sequence += 1;
  let sequence = state.xwayland.sequence;
  let Some(Surface {
    role: Role::Xwayland(role),
    ..
  }) = state.surfaces.get_mut(object)
  else {
    return;
  };
  if role.unmanaged.is_some() {
    return;
  }
  role.unmanaged = Some(UnmanagedRole {
    keyboard,
    focus_pending: focus,
    sequence,
  });
  if !from_commit {
    committed(state, object);
  }
}

/// Focuses an unmanaged window that waited for its first buffer.
pub(crate) fn committed(state: &mut CompositorState, object: &ObjectId) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let has_buffer = surface.buffer.is_some();
  let Role::Xwayland(role) = &mut surface.role else {
    return;
  };
  let Some(unmanaged) = &mut role.unmanaged else {
    return;
  };
  if !has_buffer || !std::mem::take(&mut unmanaged.focus_pending) {
    return;
  }
  crate::input::focus_surface(state, object.clone());
}

/// Stops drawing an unmanaged window; the keyboard focus, if it had it,
/// goes back to the focused window.
pub(crate) fn unmap(state: &mut CompositorState, object: &ObjectId) {
  let Some(Surface {
    role: Role::Xwayland(role),
    ..
  }) = state.surfaces.get_mut(object)
  else {
    return;
  };
  if role.unmanaged.take().is_some() {
    crate::input::sync_focus(state);
  }
}

/// Follows a property change of a mapped unmanaged window: whether it may
/// take keyboard focus.
pub(crate) fn changed(state: &mut CompositorState, window: Window) {
  let Some(entry) = state.xwayland.windows.get(&window) else {
    return;
  };
  let keyboard = entry.info.unmanaged_accepts_keyboard();
  let Some(object) = entry
    .surface
    .and_then(|surface| state.surface_ids.get(&surface))
    .cloned()
  else {
    return;
  };
  if let Some(Surface {
    role: Role::Xwayland(role),
    ..
  }) = state.surfaces.get_mut(&object)
    && let Some(unmanaged) = &mut role.unmanaged
  {
    unmanaged.keyboard = keyboard;
  }
  crate::input::sync_focus(state);
}

/// Where an unmanaged window is drawn: its X geometry, which is in
/// Shingle's logical coordinates.
pub(crate) fn geometry(
  state: &CompositorState,
  role: &XwaylandRole,
) -> Option<Rect> {
  role.unmanaged?;
  let info = &state.xwayland.windows.get(&role.window?)?.info;
  let geometry = info.geometry.nonzero();
  Some(Rect::new(
    geometry.x,
    geometry.y,
    geometry.width,
    geometry.height,
  ))
}

/// The unmanaged surfaces to draw, bottom to top.
pub(crate) fn drawn(state: &CompositorState) -> Vec<ObjectId> {
  let mut surfaces = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      surface.buffer.as_ref()?;
      let Role::Xwayland(role) = &surface.role else {
        return None;
      };
      role.window?;
      Some((role.unmanaged?.sequence, object.clone()))
    })
    .collect::<Vec<_>>();
  surfaces.sort_by_key(|(sequence, _)| *sequence);
  surfaces.into_iter().map(|(_, object)| object).collect()
}

/// The class and title of an unmanaged window, for its snapshot.
pub(crate) fn names(
  state: &CompositorState,
  role: &XwaylandRole,
) -> Option<(String, String)> {
  role.unmanaged?;
  let info = &state.xwayland.windows.get(&role.window?)?.info;
  Some((info.class.clone(), info.title.clone()))
}

/// Whether the keyboard focus is on an unmanaged X window.
pub(crate) fn has_keyboard_focus(state: &CompositorState) -> bool {
  state
    .input
    .keyboard_focus
    .as_ref()
    .and_then(|object| state.surfaces.get(object))
    .is_some_and(|surface| {
      matches!(&surface.role, Role::Xwayland(role) if role.unmanaged.is_some())
    })
}
