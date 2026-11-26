//! The Wayland half of the drag and drop bridge (docs/design/xwayland.md,
//! section 5, and the XW-6 entry).
//!
//! - **Wayland to X.** [`DragBridge::sync`] reports a Wayland client's
//!   drag to the window manager each turn: its start with the source's
//!   types and actions, the X window under the pointer and the pointer's
//!   position while it moves, the drop and the end. The window manager
//!   speaks Xdnd to the X window; its verdict and its `XdndFinished` come
//!   back as events ([`apply`]) for `data_device`, which tells the Wayland
//!   source. X targets' data requests reach the source through the
//!   selection bridge's sends for `Selection::Dnd`.
//! - **X to Wayland.** When an X client takes `XdndSelection` while a
//!   button is held on one of its windows, [`apply`] starts a drag with an
//!   X11 source in `data_device`, which offers it to Wayland surfaces like
//!   any other. [`DragBridge::sync`] keeps the window manager's proxy
//!   mapped while the drag is not over an X window and hands it the
//!   Wayland target's verdict; the X client's types, its drop and its
//!   leaving come back as events. A drop that does not come within
//!   [`data_device::X11_DROP_TIMEOUT`] of the release is cancelled.

use shingle_xwayland::{
  DndAction, DndActions, Window, X11Connection, Xwm, XwmError, XwmEvent,
};
use wayland_server::{
  backend::ObjectId, protocol::wl_data_device_manager::DndAction as WlAction,
};

use crate::{
  data_device::{self, SourceKey},
  shell::Role,
  state::CompositorState,
};

/// The Xdnd action for `wl_data_device_manager` action bits: the first of
/// copy, move and ask.
pub(crate) fn xdnd_action(bits: u32) -> DndAction {
  DndActions::from_bits(bits).proposed()
}

/// The `wl_data_device_manager` bits for an Xdnd action.
pub(crate) fn wl_bits(action: DndAction) -> u32 {
  match action {
    DndAction::None => 0,
    DndAction::Copy => WlAction::Copy.bits(),
    DndAction::Move => WlAction::Move.bits(),
    DndAction::Ask => WlAction::Ask.bits(),
  }
}

/// The X window whose surface is `object`: a managed or unmanaged X
/// window that takes pointer input.
pub(crate) fn x_window(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Window> {
  let surface = state.surfaces.get(object)?;
  let Role::Xwayland(role) = &surface.role else {
    return None;
  };
  role.window.filter(|_| role.accepts_pointer())
}

/// Applies a window manager event about drag and drop.
pub(crate) fn apply(state: &mut CompositorState, event: XwmEvent) {
  match event {
    XwmEvent::DragOwnerChanged { owner } => owner_changed(state, owner),
    XwmEvent::DragStatus { accepted, action } => {
      data_device::x11_target_status(state, accepted, wl_bits(action));
    }
    XwmEvent::DragFinished { performed, .. } => {
      data_device::x11_target_finished(state, performed);
    }
    XwmEvent::DragOffered { mime_types } => {
      data_device::set_x11_drag_types(state, mime_types);
    }
    XwmEvent::DragSourceAction { action } => {
      data_device::set_x11_drag_actions(state, wl_bits(action));
    }
    XwmEvent::DragLeft => data_device::x11_source_left(state),
    XwmEvent::DragDropped => {
      data_device::drop_x11_drag(state);
    }
    _ => {}
  }
}

/// An X client took `XdndSelection`, or no one has it now. A drag starts
/// when a button is held on an X surface; any X drag before it ends.
fn owner_changed(state: &mut CompositorState, owner: Option<Window>) {
  data_device::end_x11_drag(state, None);
  // The window manager has forgotten the drag already.
  state.data_devices.x11_drag_ended = None;
  if owner.is_none() {
    return;
  }
  if let Some(origin) = crate::input::x_drag_origin(state) {
    data_device::start_x11_drag(state, origin);
  }
}

/// What the bridge told the window manager about the current drag.
#[derive(Debug, Default)]
pub(crate) struct DragBridge {
  /// The Wayland drag reported with `drag_start`.
  wayland: Option<SourceKey>,
  /// Its drop was reported.
  dropped: bool,
}

/// What [`DragBridge::sync`] asks of the window manager this turn, which
/// [`DragBridge::plan`] works out from the drag alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Plan {
  /// End the Wayland drag reported before.
  pub(crate) end: bool,
  /// Start a Wayland drag with these types and action bits.
  pub(crate) start: Option<(Vec<String>, u32)>,
  /// The Wayland drag is over this X window, or none.
  pub(crate) motion: Option<Option<Window>>,
  /// Drop the Wayland drag.
  pub(crate) drop: bool,
  /// End the X drag: performed with these action bits, or not.
  pub(crate) x11_end: Option<Option<u32>>,
  /// Update the X drag: map the proxy, and the verdict's action bits.
  pub(crate) x11: Option<(bool, u32)>,
  /// The X drag waited too long for its drop and is cancelled.
  pub(crate) x11_timed_out: bool,
}

impl DragBridge {
  /// Works out what to tell the window manager, and remembers it.
  pub(crate) fn plan(&mut self, state: &mut CompositorState) -> Plan {
    let mut plan = Plan::default();
    let view = data_device::drag_view(state);
    let wayland = view.as_ref().filter(|view| !view.x11);
    if self.wayland.is_some()
      && self.wayland.as_ref() != wayland.map(|view| &view.source)
    {
      plan.end = true;
      self.wayland = None;
    }
    if let Some(view) = wayland {
      if self.wayland.is_none() {
        plan.start = Some((view.mime_types.clone(), view.actions));
        self.wayland = Some(view.source.clone());
        self.dropped = false;
      }
      if view.dropped {
        plan.drop = !std::mem::replace(&mut self.dropped, true);
      } else {
        let window = view
          .target
          .as_ref()
          .and_then(|target| x_window(state, target));
        plan.motion = Some(window);
      }
    }
    if let Some(view) = view.as_ref().filter(|view| view.x11) {
      if view.released.is_some_and(|released| {
        !view.dropped && released.elapsed() >= data_device::X11_DROP_TIMEOUT
      }) {
        plan.x11_timed_out = true;
        data_device::end_x11_drag(state, None);
      } else {
        let over_x = view
          .target
          .as_ref()
          .is_some_and(|target| x_window(state, target).is_some());
        plan.x11 = Some((!over_x, view.action));
      }
    }
    if let Some(ended) = state.data_devices.x11_drag_ended.take() {
      plan.x11_end = Some(ended.map(|action| action.bits()));
      plan.x11 = None;
    }
    plan
  }

  /// Tells the window manager about the drag this turn.
  pub(crate) fn sync(
    &mut self,
    state: &mut CompositorState,
    xwm: &mut Xwm<X11Connection>,
  ) -> Result<(), XwmError> {
    let plan = self.plan(state);
    if plan.end || plan.start.is_some() {
      xwm.drag_end()?;
    }
    if let Some((mime_types, actions)) = plan.start {
      xwm.drag_start(mime_types, DndActions::from_bits(actions))?;
    }
    if let Some(window) = plan.motion {
      let (x, y) = (state.input.pointer_x, state.input.pointer_y);
      // Positions are in root coordinates, which are Shingle's.
      let events =
        xwm.drag_motion(window, x.floor() as i32, y.floor() as i32, NOW)?;
      for event in events {
        apply(state, event);
      }
    }
    if plan.drop {
      xwm.drag_drop(NOW)?;
    }
    if let Some(ended) = plan.x11_end {
      xwm.x11_drag_end(ended.map(xdnd_action))?;
    }
    if let Some((proxy, action)) = plan.x11 {
      xwm.x11_drag(proxy, action != 0, xdnd_action(action))?;
    }
    Ok(())
  }

  /// Forgets what was reported, when the window manager goes.
  pub(crate) fn clear(&mut self) {
    *self = Self::default();
  }
}

/// Xdnd's times are X server times, which Shingle does not know; the
/// window manager sends `CurrentTime`, which the server accepts for the
/// conversion that follows a drop.
const NOW: u32 = 0;

/// Ends an X client's drag, when the window manager goes.
pub(crate) fn forget_x11(state: &mut CompositorState) {
  data_device::end_x11_drag(state, None);
  state.data_devices.x11_drag_ended = None;
}

#[cfg(test)]
mod tests;
