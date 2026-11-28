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
//!
//! Drag icons, X windows of type `_NET_WM_WINDOW_TYPE_DND`, are kept the
//! same way ([`map_icon`]) but drawn above every layer
//! ([`drag_icons`]), as [`crate::SurfaceRole::DragIcon`] snapshots, and
//! take neither keyboard nor pointer input, as Wayfire's
//! `wayfire_dnd_xwayland_view` does: the X client moves the icon with the
//! pointer, and the drag must see what is under it.

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
    drag_icon: false,
  });
  if !from_commit {
    committed(state, object);
  }
}

/// Starts drawing the drag icon `object`, the surface of an X window of
/// type `_NET_WM_WINDOW_TYPE_DND`.
pub(crate) fn map_icon(state: &mut CompositorState, object: &ObjectId) {
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
    keyboard: false,
    focus_pending: false,
    sequence,
    drag_icon: true,
  });
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

/// Follows a change of a mapped unmanaged window or drag icon: whether it
/// may take keyboard focus, and, when it `moved`, a redraw where it is
/// now, which a commit event asks the frontend for.
pub(crate) fn changed(
  state: &mut CompositorState,
  window: Window,
  moved: bool,
) {
  let Some(entry) = state.xwayland.windows.get(&window) else {
    return;
  };
  let keyboard = entry.info.unmanaged_accepts_keyboard()
    && entry.info.kind() == shingle_xwayland::Kind::Unmanaged;
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
  if moved && let Some(snapshot) = state.snapshot(&object) {
    state
      .events
      .push(crate::Event::SurfaceCommitted(Box::new(snapshot)));
  }
}

/// Where an unmanaged window is drawn: its X geometry in logical
/// coordinates, which are X coordinates at X scale 1.
pub(crate) fn geometry(
  state: &CompositorState,
  role: &XwaylandRole,
) -> Option<Rect> {
  role.unmanaged?;
  let info = &state.xwayland.windows.get(&role.window?)?.info;
  let geometry = super::windows::from_x(state, info.geometry.nonzero());
  Some(Rect::new(
    geometry.x,
    geometry.y,
    geometry.width.max(1),
    geometry.height.max(1),
  ))
}

/// The unmanaged surfaces to draw in the Unmanaged layer, bottom to top.
pub(crate) fn drawn(state: &CompositorState) -> Vec<ObjectId> {
  in_order(state, false)
}

/// The drag icons to draw above every layer, bottom to top.
pub(crate) fn drag_icons(state: &CompositorState) -> Vec<ObjectId> {
  in_order(state, true)
}

/// The unmanaged surfaces with a buffer that are drag icons, or are not,
/// bottom to top.
fn in_order(state: &CompositorState, drag_icons: bool) -> Vec<ObjectId> {
  let mut surfaces = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      surface.buffer.as_ref()?;
      let Role::Xwayland(role) = &surface.role else {
        return None;
      };
      role.window?;
      let unmanaged = role.unmanaged?;
      (unmanaged.drag_icon == drag_icons)
        .then(|| (unmanaged.sequence, object.clone()))
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
