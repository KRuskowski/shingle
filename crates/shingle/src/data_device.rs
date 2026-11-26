//! `wl_data_device`: the clipboard and drag and drop.
//!
//! A selection's source is a [`Source`] with a [`SourceOrigin`]: a
//! client's `wl_data_source`, a wlr-data-control source
//! (crates/shingle/src/data_control.rs) or an X client through the
//! XWayland selection bridge (crates/shingle/src/xwayland/selection.rs),
//! which has no Wayland object. Sources are keyed by [`SourceKey`].
//!
//! A drag follows the surface under the pointer ([`sync_drag`]). Over a
//! Wayland surface it is offered through that client's `wl_data_device`;
//! over an X window the XWayland drag and drop bridge
//! (crates/shingle/src/xwayland/dnd.rs) speaks Xdnd to it, reading the
//! drag through [`drag_view`] and reporting the X window's answers with
//! [`x11_target_status`] and [`x11_target_finished`]. An X client's drag
//! is a drag too, with an [`SourceOrigin::X11Drag`] source
//! ([`start_x11_drag`]): while its button is held, the pointer stays on
//! the X surface it started on ([`pointer_grab`]), so that Xwayland keeps
//! seeing the motion and the release, as Wayfire's implicit grab does.

use std::{
  collections::HashMap,
  os::fd::{AsFd, OwnedFd},
  time::{Duration, Instant},
};

use wayland_protocols_wlr::data_control::v1::server as wlr_data_control;
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
  protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{self, WlDataDeviceManager},
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_surface::WlSurface,
  },
};
use wlr_data_control::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;

use shingle_core::Rect;

use crate::{
  shell::{self, Role},
  state::CompositorState,
};

#[cfg(test)]
mod icon_tests;

pub(crate) struct SourceData;
struct DeviceData;
struct OfferData;

/// Identifies a selection source: by its Wayland object, or by a number
/// for an X11 source, which has none.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SourceKey {
  /// A `wl_data_source`, a `zwp_primary_selection_source_v1` or a
  /// `zwlr_data_control_source_v1`.
  Object(ObjectId),
  /// An X client's selection, numbered in installation order.
  X11(u64),
}

impl SourceKey {
  /// Whether the source is an X client's.
  pub(crate) fn is_x11(&self) -> bool {
    matches!(self, Self::X11(_))
  }
}

/// Who offers a selection.
#[derive(Clone, Debug)]
enum SourceOrigin {
  /// A client's `wl_data_source`.
  Client(WlDataSource),
  /// A wlr-data-control client's source.
  Control(ZwlrDataControlSourceV1),
  /// An X client's selection, through the window manager.
  X11,
  /// An X client's drag, through the window manager.
  X11Drag,
}

struct Source {
  origin: SourceOrigin,
  /// The Wayland client behind it; `None` for an X11 source.
  client: Option<ClientId>,
  mime_types: Vec<String>,
  used: bool,
  actions: wl_data_device_manager::DndAction,
}

impl Source {
  /// Tells the source it is no longer the selection.
  fn cancel(&self) {
    match &self.origin {
      SourceOrigin::Client(resource) => resource.cancelled(),
      SourceOrigin::Control(control) => control.cancelled(),
      SourceOrigin::X11 | SourceOrigin::X11Drag => {}
    }
  }

  /// The `wl_data_source`, for drags.
  fn resource(&self) -> Option<&WlDataSource> {
    match &self.origin {
      SourceOrigin::Client(resource) => Some(resource),
      _ => None,
    }
  }
}

/// How long an X client's drag waits for its drop once the button is
/// released over a Wayland surface, before it is cancelled.
pub(crate) const X11_DROP_TIMEOUT: Duration = Duration::from_secs(2);

struct Drag {
  source: SourceKey,
  /// The surface the drag is over.
  current: Option<ObjectId>,
  /// The offer made to `current`'s client, if it is a Wayland one.
  offer: Option<ObjectId>,
  /// The version of that offer.
  offer_version: u32,
  accepted: Option<String>,
  action: wl_data_device_manager::DndAction,
  dropped: bool,
  /// The pointer position `current` last heard of, so that `motion` is
  /// sent only when it moves.
  position: (f64, f64),
  /// An X client's drag: the X surface that keeps the pointer while the
  /// button is held; `None` once it is released.
  grab: Option<ObjectId>,
  /// An X client's drag: when the button was released.
  released: Option<Instant>,
  /// A Wayland client's drag icon, the surface `start_drag` named.
  icon: Option<ObjectId>,
}

impl Drag {
  /// A drag from `source` that has not entered any surface yet.
  fn new(source: SourceKey, grab: Option<ObjectId>) -> Self {
    Self {
      source,
      current: None,
      offer: None,
      offer_version: 0,
      accepted: None,
      action: wl_data_device_manager::DndAction::Copy,
      dropped: false,
      position: (f64::NAN, f64::NAN),
      grab,
      released: None,
      icon: None,
    }
  }

  /// Whether the drag is accepted: a type and an action.
  fn is_accepted(&self) -> bool {
    self.accepted.is_some()
      && self.action != wl_data_device_manager::DndAction::empty()
  }
}

/// What the XWayland drag and drop bridge needs of the current drag.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DragView {
  /// The source.
  pub(crate) source: SourceKey,
  /// The source is an X client's drag.
  pub(crate) x11: bool,
  /// The source's MIME types.
  pub(crate) mime_types: Vec<String>,
  /// The source's actions, in `wl_data_device_manager` bits.
  pub(crate) actions: u32,
  /// The surface the drag is over.
  pub(crate) target: Option<ObjectId>,
  /// The target accepts the drag with this action, in
  /// `wl_data_device_manager` bits; 0 when it does not.
  pub(crate) action: u32,
  /// The drag was dropped.
  pub(crate) dropped: bool,
  /// An X client's drag: when its button was released.
  pub(crate) released: Option<Instant>,
}

#[derive(Clone)]
struct Device {
  resource: WlDataDevice,
  client: Client,
  focused: Option<ClientId>,
  selection: Option<SourceKey>,
}

#[derive(Default)]
pub(crate) struct DataDevices {
  sources: HashMap<SourceKey, Source>,
  devices: HashMap<ObjectId, Device>,
  offers: HashMap<ObjectId, SourceKey>,
  selection: Option<SourceKey>,
  drag: Option<Drag>,
  /// The number of the last X11 source.
  x11_sources: u64,
  /// An X client's drag ended: performed with this action, or not
  /// (`None`). The XWayland bridge tells the X source.
  pub(crate) x11_drag_ended: Option<Option<wl_data_device_manager::DndAction>>,
  /// The drag icon drawn at the end of the last turn, and where, so that
  /// the frontend redraws where it moves to and where it was.
  icon_drawn: Option<(ObjectId, Rect)>,
}

impl DataDevices {
  pub(crate) fn cleanup_clients(&mut self, clients: &[ClientId]) {
    self.sources.retain(|_, source| {
      source
        .client
        .as_ref()
        .is_none_or(|client| !clients.contains(client))
    });
    self
      .devices
      .retain(|_, device| !clients.contains(&device.client.id()));
    self
      .offers
      .retain(|_, source| self.sources.contains_key(source));
    if self
      .selection
      .as_ref()
      .is_some_and(|id| !self.sources.contains_key(id))
    {
      self.selection = None;
    }
  }

  /// Makes `key` the selection, cancelling the previous source unless it
  /// is the same. A replaced X11 source is forgotten, since no Wayland
  /// object keeps it.
  fn replace_selection(&mut self, key: Option<SourceKey>) {
    if self.selection == key {
      return;
    }
    if let Some(previous) = std::mem::replace(&mut self.selection, key) {
      if let Some(source) = self.sources.get(&previous) {
        source.cancel();
      }
      if previous.is_x11() {
        self.forget(&previous);
      }
    }
  }

  /// Removes a source and the offers of it.
  fn forget(&mut self, key: &SourceKey) {
    self.sources.remove(key);
    self.offers.retain(|_, source| source != key);
    if self.selection.as_ref() == Some(key) {
      self.selection = None;
    }
  }
}

pub(crate) fn selection(
  state: &CompositorState,
) -> Option<(SourceKey, Vec<String>)> {
  let id = state.data_devices.selection.clone()?;
  let source = state.data_devices.sources.get(&id)?;
  Some((id, source.mime_types.clone()))
}

pub(crate) fn set_control_selection(
  state: &mut CompositorState,
  client: &Client,
  control_source: ZwlrDataControlSourceV1,
  mime_types: Vec<String>,
) {
  let key = SourceKey::Object(control_source.id());
  state.data_devices.sources.insert(
    key.clone(),
    Source {
      origin: SourceOrigin::Control(control_source),
      client: Some(client.id()),
      mime_types,
      used: true,
      actions: wl_data_device_manager::DndAction::Copy,
    },
  );
  state.data_devices.replace_selection(Some(key));
}

/// Installs an X client's selection with `mime_types` as the Wayland
/// selection, replacing whatever was there, and returns its key.
pub(crate) fn set_x11_selection(
  state: &mut CompositorState,
  mime_types: Vec<String>,
) -> SourceKey {
  let data = &mut state.data_devices;
  data.x11_sources += 1;
  let key = SourceKey::X11(data.x11_sources);
  data.sources.insert(
    key.clone(),
    Source {
      origin: SourceOrigin::X11,
      client: None,
      mime_types,
      used: true,
      actions: wl_data_device_manager::DndAction::Copy,
    },
  );
  data.replace_selection(Some(key.clone()));
  key
}

/// Clears the selection if an X client's source holds it.
pub(crate) fn clear_x11_selection(state: &mut CompositorState) {
  if state
    .data_devices
    .selection
    .as_ref()
    .is_some_and(SourceKey::is_x11)
  {
    state.data_devices.replace_selection(None);
  }
}

/// Asks `source` for `mime_type` in `fd`. An X11 source's request goes to
/// the window manager on its next flush. Nothing is sent for a type the
/// source does not offer; dropping `fd` ends the reader's transfer.
pub(crate) fn receive_from(
  state: &mut CompositorState,
  source: &SourceKey,
  mime_type: String,
  fd: OwnedFd,
) {
  let Some(found) = state.data_devices.sources.get(source) else {
    return;
  };
  if !found.mime_types.contains(&mime_type) {
    return;
  }
  match &found.origin {
    SourceOrigin::Client(resource) => resource.send(mime_type, fd.as_fd()),
    SourceOrigin::Control(control) => control.send(mime_type, fd.as_fd()),
    SourceOrigin::X11 => state.xwayland.selection_receives.push((
      shingle_xwayland::Selection::Clipboard,
      mime_type,
      fd,
    )),
    SourceOrigin::X11Drag => state.xwayland.selection_receives.push((
      shingle_xwayland::Selection::Dnd,
      mime_type,
      fd,
    )),
  }
}

pub(crate) fn remove_control_source(
  state: &mut CompositorState,
  control: &ObjectId,
) {
  let key = SourceKey::Object(control.clone());
  if state
    .data_devices
    .sources
    .get(&key)
    .is_some_and(|source| matches!(source.origin, SourceOrigin::Control(_)))
  {
    state.data_devices.forget(&key);
  }
}

pub(crate) fn clear_selection(state: &mut CompositorState) {
  state.data_devices.replace_selection(None);
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WlDataDeviceManager, ()>(4, ());
}

pub(crate) fn sync_selection(
  state: &mut CompositorState,
  handle: &DisplayHandle,
) {
  let focused = state
    .input
    .keyboard_focus
    .as_ref()
    .and_then(|id| state.surfaces.get(id))
    .map(|surface| surface.client.clone());
  let data = &mut state.data_devices;
  for device in data.devices.values_mut() {
    let focused_client = focused
      .as_ref()
      .filter(|client| *client == &device.client.id())
      .cloned();
    if device.focused == focused_client && device.selection == data.selection {
      continue;
    }
    device.focused = focused_client.clone();
    device.selection = data.selection.clone();
    if focused_client.is_none() {
      device.resource.selection(None);
      continue;
    }
    let Some(source_id) = data.selection.clone() else {
      device.resource.selection(None);
      continue;
    };
    let Some(source) = data.sources.get(&source_id) else {
      device.resource.selection(None);
      continue;
    };
    let Ok(offer) = device
      .client
      .create_resource::<WlDataOffer, OfferData, CompositorState>(
        handle,
        device.resource.version(),
        OfferData,
      )
    else {
      continue;
    };
    device.resource.data_offer(&offer);
    for mime_type in &source.mime_types {
      offer.offer(mime_type.clone());
    }
    device.resource.selection(Some(&offer));
    data.offers.insert(offer.id(), source_id);
  }
}

/// Whether `object` is an X window's surface, which Xdnd reaches through
/// the window manager rather than a `wl_data_device`.
fn is_x11(state: &CompositorState, object: &ObjectId) -> bool {
  state
    .surfaces
    .get(object)
    .is_some_and(|surface| matches!(surface.role, Role::Xwayland(_)))
}

/// Sends `wl_data_device.leave` to the client of `object`, if it has a
/// device.
fn leave(state: &CompositorState, object: &ObjectId) {
  let Some(surface) = state.surfaces.get(object) else {
    return;
  };
  for device in state.data_devices.devices.values() {
    if device.client.id() == surface.client {
      device.resource.leave();
    }
  }
}

/// Follows the drag to the surface under the pointer: leaves the last
/// one, and offers the drag to a Wayland one's `wl_data_device` with
/// `enter`, or sends it `motion`. An X window's surface gets nothing here;
/// the XWayland bridge speaks Xdnd to it. An X client's drag is offered
/// only once its types are known, and stops following the pointer when
/// its button is released.
pub(crate) fn sync_drag(state: &mut CompositorState, handle: &DisplayHandle) {
  let Some(drag) = &state.data_devices.drag else {
    return;
  };
  let (source_id, previous) = (drag.source.clone(), drag.current.clone());
  if drag.dropped || drag.released.is_some() {
    return;
  }
  let Some(source) = state.data_devices.sources.get(&source_id) else {
    state.data_devices.drag = None;
    return;
  };
  let waiting_for_types = matches!(source.origin, SourceOrigin::X11Drag)
    && source.mime_types.is_empty();
  let target = crate::input::drag_target(state)
    .filter(|target| !waiting_for_types || is_x11(state, target));
  let position = (state.input.pointer_x, state.input.pointer_y);
  if previous == target {
    let Some(drag) = &mut state.data_devices.drag else {
      return;
    };
    if drag.offer.is_none() || drag.position == position {
      return;
    }
    drag.position = position;
    if let Some(target) = target {
      motion(state, &target);
    }
    return;
  }
  if let Some(previous) = &previous {
    leave(state, previous);
  }
  let entered = target
    .as_ref()
    .and_then(|target| enter(state, handle, &source_id, target));
  if let Some(drag) = &mut state.data_devices.drag {
    drag.current = target;
    drag.offer = entered.as_ref().map(|(offer, _)| offer.clone());
    drag.offer_version = entered.map_or(0, |(_, version)| version);
    drag.accepted = None;
    drag.action = wl_data_device_manager::DndAction::Copy;
    drag.position = position;
  }
}

/// Offers the drag from `source_id` to the client of the Wayland surface
/// `target` with `wl_data_device.enter`; returns the offer and its
/// version, or `None` for a client without a device or an X window.
fn enter(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  source_id: &SourceKey,
  target: &ObjectId,
) -> Option<(ObjectId, u32)> {
  if is_x11(state, target) {
    return None;
  }
  let surface = state.surfaces.get(target)?;
  let device = state
    .data_devices
    .devices
    .values()
    .find(|device| device.client.id() == surface.client)
    .cloned()?;
  let offer = device
    .client
    .create_resource::<WlDataOffer, OfferData, CompositorState>(
      handle,
      device.resource.version(),
      OfferData,
    )
    .ok()?;
  let source = state.data_devices.sources.get(source_id)?;
  device.resource.data_offer(&offer);
  for mime_type in &source.mime_types {
    offer.offer(mime_type.clone());
  }
  if offer.version() >= 3 {
    offer.source_actions(source.actions);
    offer.action(wl_data_device_manager::DndAction::Copy);
  }
  let geometry =
    crate::shell::surface_geometry(state, target).unwrap_or_default();
  let surface_resource = surface.resource.clone();
  let serial = state.serial();
  device.resource.enter(
    serial,
    &surface_resource,
    state.input.pointer_x - f64::from(geometry.x),
    state.input.pointer_y - f64::from(geometry.y),
    Some(&offer),
  );
  state
    .data_devices
    .offers
    .insert(offer.id(), source_id.clone());
  Some((offer.id(), offer.version()))
}

/// Sends `wl_data_device.motion` to the client of `target`.
fn motion(state: &CompositorState, target: &ObjectId) {
  let Some(surface) = state.surfaces.get(target) else {
    return;
  };
  let geometry =
    crate::shell::surface_geometry(state, target).unwrap_or_default();
  let time = crate::input::timestamp(state);
  for device in state.data_devices.devices.values() {
    if device.client.id() == surface.client {
      device.resource.motion(
        time,
        state.input.pointer_x - f64::from(geometry.x),
        state.input.pointer_y - f64::from(geometry.y),
      );
    }
  }
}

/// Drops a Wayland client's drag when the button is released: on a
/// Wayland surface with `wl_data_device.drop`, and the source learns
/// whether the drop was performed. Over an X window that accepted it the
/// drop is performed too, and the XWayland bridge sends `XdndDrop`. An X
/// client's drag is dropped by its `XdndDrop` instead ([`drop_x11_drag`]).
pub(crate) fn drop_drag(state: &mut CompositorState) {
  let Some(drag) = &mut state.data_devices.drag else {
    return;
  };
  if drag.dropped || drag.grab.is_some() || drag.released.is_some() {
    return;
  }
  drag.dropped = true;
  let Some(drag) = &state.data_devices.drag else {
    return;
  };
  let Some(source) = state
    .data_devices
    .sources
    .get(&drag.source)
    .and_then(Source::resource)
    .cloned()
  else {
    return;
  };
  let accepted = drag.is_accepted();
  if let Some(target) = &drag.current
    && !is_x11(state, target)
    && let Some(surface) = state.surfaces.get(target)
  {
    for device in state.data_devices.devices.values() {
      if device.client.id() == surface.client {
        device.resource.drop();
      }
    }
  }
  if accepted {
    source.dnd_drop_performed();
  } else {
    source.cancelled();
  }
}

/// Gives `icon` the drag icon role, which a surface without a role or one
/// that was an icon before may take, and moves it back to the pointer.
/// Returns whether it took the role.
fn take_icon_role(state: &mut CompositorState, icon: &WlSurface) -> bool {
  let Some(surface) = state.surfaces.get_mut(&icon.id()) else {
    return true;
  };
  match surface.role {
    Role::None | Role::DragIcon { .. } => {
      surface.role = Role::DragIcon { offset: (0, 0) };
      true
    }
    _ => false,
  }
}

/// The drag icon to draw: the icon surface of the current drag while it
/// is not dropped and the surface has a buffer. wlroots ends the drag and
/// its icon on the drop.
pub(crate) fn drag_icon(state: &CompositorState) -> Option<ObjectId> {
  let drag = state.data_devices.drag.as_ref()?;
  if drag.dropped {
    return None;
  }
  let icon = drag.icon.as_ref()?;
  state
    .surfaces
    .get(icon)
    .is_some_and(|surface| {
      surface.buffer.is_some() && matches!(surface.role, Role::DragIcon { .. })
    })
    .then(|| icon.clone())
}

/// Whether `object` is the drag icon or one of its subsurfaces, which take
/// no input, so that the drag sees the surface under it.
pub(crate) fn in_drag_icon(state: &CompositorState, object: &ObjectId) -> bool {
  let mut current = object.clone();
  for _ in 0..crate::surface_outputs::MAX_DEPTH {
    match state.surfaces.get(&current).map(|surface| &surface.role) {
      Some(Role::DragIcon { .. }) => return true,
      Some(Role::Subsurface { parent, .. }) => current = parent.clone(),
      _ => return false,
    }
  }
  false
}

/// Asks the frontend to redraw where the drag icon moved to, and where it
/// was when it moved or went, with a commit event: the icon follows the
/// pointer without committing, and a cursor on a cursor plane draws no
/// frame for the motion.
pub(crate) fn sync_icon(state: &mut CompositorState) {
  let now = drag_icon(state).and_then(|object| {
    let rect = shell::surface_geometry(state, &object)?;
    Some((object, rect))
  });
  if now == state.data_devices.icon_drawn {
    return;
  }
  let before = std::mem::replace(&mut state.data_devices.icon_drawn, now);
  let mut damaged = Vec::new();
  if let Some((object, _)) = &state.data_devices.icon_drawn {
    damaged.extend(state.snapshot(object));
  }
  if let Some((object, rect)) = before
    && let Some(mut snapshot) = state.snapshot(&object)
  {
    snapshot.geometry = rect;
    damaged.push(snapshot);
  }
  for snapshot in damaged {
    state
      .events
      .push(crate::Event::SurfaceCommitted(Box::new(snapshot)));
  }
}

/// The current drag, for the XWayland bridge.
pub(crate) fn drag_view(state: &CompositorState) -> Option<DragView> {
  let drag = state.data_devices.drag.as_ref()?;
  let source = state.data_devices.sources.get(&drag.source)?;
  let action = if drag.is_accepted() {
    drag.action.bits()
  } else {
    0
  };
  Some(DragView {
    source: drag.source.clone(),
    x11: matches!(source.origin, SourceOrigin::X11Drag),
    mime_types: source.mime_types.clone(),
    actions: source.actions.bits(),
    target: drag.current.clone(),
    action,
    dropped: drag.dropped,
    released: drag.released,
  })
}

/// The X window a Wayland drag is over accepts it with `action` (bits), or
/// does not, or the drag left it: the source learns of it as from a
/// Wayland target, with the first of its types as the accepted one, since
/// Xdnd names none. A verdict that comes after the drag moved on to
/// another surface only takes back an acceptance.
pub(crate) fn x11_target_status(
  state: &mut CompositorState,
  accepted: bool,
  action: u32,
) {
  let Some(drag) = &state.data_devices.drag else {
    return;
  };
  if drag.dropped || drag.source.is_x11() {
    return;
  }
  let over_x = drag
    .current
    .as_ref()
    .is_some_and(|target| is_x11(state, target));
  if !over_x && accepted {
    return;
  }
  let Some(found) = state.data_devices.sources.get(&drag.source) else {
    return;
  };
  let mime_type = found.mime_types.first().cloned().filter(|_| accepted);
  let Some(source) = found.resource().cloned() else {
    return;
  };
  let action = wl_data_device_manager::DndAction::from_bits_truncate(action);
  if over_x && let Some(drag) = &mut state.data_devices.drag {
    drag.accepted.clone_from(&mime_type);
    drag.action = action;
  }
  source.target(mime_type);
  if source.version() >= 3 {
    source.action(action);
  }
}

/// The X window a Wayland drag was dropped on is done (`XdndFinished`):
/// the source gets `dnd_finished`, or `cancelled` when the window did not
/// take the data.
pub(crate) fn x11_target_finished(
  state: &mut CompositorState,
  performed: bool,
) {
  let Some(drag) = &state.data_devices.drag else {
    return;
  };
  if !drag.dropped {
    return;
  }
  let Some(source) = state
    .data_devices
    .sources
    .get(&drag.source)
    .and_then(Source::resource)
    .cloned()
  else {
    return;
  };
  if performed && source.version() >= 3 {
    source.dnd_finished();
  } else if !performed {
    source.cancelled();
  }
}

/// Starts an X client's drag, which the XWayland bridge saw take
/// `XdndSelection` while a button is held on `grab`, an X surface. Its
/// types come with its `XdndEnter` ([`set_x11_drag_types`]). A drag from
/// a Wayland client that is not dropped yet wins; one that was dropped
/// and whose source lives on is replaced, as a new `start_drag` would
/// replace it.
pub(crate) fn start_x11_drag(state: &mut CompositorState, grab: ObjectId) {
  if state
    .data_devices
    .drag
    .as_ref()
    .is_some_and(|drag| !drag.source.is_x11() && !drag.dropped)
  {
    return;
  }
  forget_x11_drag(state);
  let data = &mut state.data_devices;
  data.x11_sources += 1;
  let key = SourceKey::X11(data.x11_sources);
  data.sources.insert(
    key.clone(),
    Source {
      origin: SourceOrigin::X11Drag,
      client: None,
      mime_types: Vec::new(),
      used: true,
      actions: wl_data_device_manager::DndAction::Copy,
    },
  );
  data.drag = Some(Drag::new(key, Some(grab)));
}

/// The source of the X client's drag.
fn x11_drag_source(state: &mut CompositorState) -> Option<&mut Source> {
  let drag = state.data_devices.drag.as_ref()?;
  let key = drag.source.clone();
  state
    .data_devices
    .sources
    .get_mut(&key)
    .filter(|source| matches!(source.origin, SourceOrigin::X11Drag))
}

/// The X client's drag offers `mime_types`. A Wayland surface it is over
/// gets the drag again with them.
pub(crate) fn set_x11_drag_types(
  state: &mut CompositorState,
  mime_types: Vec<String>,
) {
  let Some(source) = x11_drag_source(state) else {
    return;
  };
  source.mime_types = mime_types;
  reenter(state);
}

/// The X client's drag proposes `actions` (bits); copy is always among
/// them, as every Xdnd source supports it.
pub(crate) fn set_x11_drag_actions(state: &mut CompositorState, actions: u32) {
  let Some(source) = x11_drag_source(state) else {
    return;
  };
  source.actions =
    wl_data_device_manager::DndAction::from_bits_truncate(actions)
      | wl_data_device_manager::DndAction::Copy;
  reenter(state);
}

/// Leaves the surface the drag is over, so that [`sync_drag`] offers it
/// again with what changed.
fn reenter(state: &mut CompositorState) {
  let Some(drag) = &mut state.data_devices.drag else {
    return;
  };
  if drag.dropped || drag.released.is_some() {
    return;
  }
  if let Some(previous) = drag.current.take() {
    drag.offer = None;
    drag.accepted = None;
    leave(state, &previous);
  }
}

/// The X client's drag was dropped on a Wayland surface (`XdndDrop`):
/// the surface's client gets `wl_data_device.drop` if it accepted the
/// drag, and the drag waits for its `finish`. Otherwise the drag ends
/// unperformed. Returns whether it was dropped.
pub(crate) fn drop_x11_drag(state: &mut CompositorState) -> bool {
  let Some(drag) = &state.data_devices.drag else {
    return false;
  };
  if !drag.source.is_x11() || drag.dropped {
    return false;
  }
  let target = drag.current.clone().filter(|_| drag.is_accepted());
  let (version, action) = (drag.offer_version, drag.action);
  let Some(target) = target.filter(|target| !is_x11(state, target)) else {
    end_x11_drag(state, None);
    return false;
  };
  if let Some(drag) = &mut state.data_devices.drag {
    drag.dropped = true;
    drag.grab = None;
  }
  if let Some(surface) = state.surfaces.get(&target) {
    for device in state.data_devices.devices.values() {
      if device.client.id() == surface.client {
        device.resource.drop();
      }
    }
  }
  // A target older than version 3 never sends finish.
  if version < 3 {
    end_x11_drag(state, Some(action));
  }
  true
}

/// The X client left the proxy (`XdndLeave`). After the button was
/// released that is how it refuses a drop no one accepted, and the drag
/// ends; while the button is held, the drag only moved over an X window.
pub(crate) fn x11_source_left(state: &mut CompositorState) {
  if state.data_devices.drag.as_ref().is_some_and(|drag| {
    drag.source.is_x11() && !drag.dropped && drag.released.is_some()
  }) {
    end_x11_drag(state, None);
  }
}

/// Ends the X client's drag: a Wayland surface it is over and was not
/// dropped on gets `leave`, and the bridge tells the X source whether it
/// was performed, with which action.
pub(crate) fn end_x11_drag(
  state: &mut CompositorState,
  performed: Option<wl_data_device_manager::DndAction>,
) {
  let Some(drag) = &state.data_devices.drag else {
    return;
  };
  if !drag.source.is_x11() {
    return;
  }
  if !drag.dropped
    && let Some(target) = drag.current.clone()
  {
    leave(state, &target);
  }
  forget_x11_drag(state);
  state.data_devices.x11_drag_ended = Some(performed);
}

/// Forgets an X client's drag and its source without telling anyone.
fn forget_x11_drag(state: &mut CompositorState) {
  let data = &mut state.data_devices;
  if let Some(drag) = data.drag.take_if(|drag| drag.source.is_x11()) {
    data.forget(&drag.source);
  }
}

/// Moves the release of an X client's drag `by` into the past, for the
/// timeout's tests.
#[cfg(test)]
pub(crate) fn backdate_release(state: &mut CompositorState, by: Duration) {
  if let Some(drag) = &mut state.data_devices.drag
    && let Some(released) = &mut drag.released
  {
    *released -= by;
  }
}

/// The X surface that keeps the pointer during an X client's drag.
pub(crate) fn pointer_grab(state: &CompositorState) -> Option<ObjectId> {
  state.data_devices.drag.as_ref()?.grab.clone()
}

/// Every button was released: an X client's drag stops holding the
/// pointer. Over an X window, X finishes the drag by itself and it ends
/// here; elsewhere it waits for the X client's `XdndDrop` or `XdndLeave`.
/// Returns whether the pointer was held.
pub(crate) fn release_grab(state: &mut CompositorState) -> bool {
  let Some(drag) = &mut state.data_devices.drag else {
    return false;
  };
  if drag.grab.take().is_none() {
    return false;
  }
  drag.released = Some(Instant::now());
  if crate::input::drag_target(state)
    .is_some_and(|target| is_x11(state, &target))
  {
    end_x11_drag(state, None);
  }
  true
}

fn select_action(
  source: &Source,
  supported: wl_data_device_manager::DndAction,
  preferred: wl_data_device_manager::DndAction,
) -> wl_data_device_manager::DndAction {
  let matching = source.actions & supported;
  if matching.contains(preferred) {
    return preferred;
  }
  for action in [
    wl_data_device_manager::DndAction::Copy,
    wl_data_device_manager::DndAction::Move,
    wl_data_device_manager::DndAction::Ask,
  ] {
    if matching.contains(action) {
      return action;
    }
  }
  wl_data_device_manager::DndAction::empty()
}

impl GlobalDispatch<WlDataDeviceManager, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wl_data_device_manager");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WlDataDeviceManager>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WlDataDeviceManager, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &WlDataDeviceManager,
    request: wl_data_device_manager::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_data_device_manager::Request::CreateDataSource { id } => {
        let source = data_init.init(id, SourceData);
        state.data_devices.sources.insert(
          SourceKey::Object(source.id()),
          Source {
            origin: SourceOrigin::Client(source),
            client: Some(client.id()),
            mime_types: Vec::new(),
            used: false,
            actions: wl_data_device_manager::DndAction::Copy,
          },
        );
      }
      wl_data_device_manager::Request::GetDataDevice { id, .. } => {
        let device = data_init.init(id, DeviceData);
        state.data_devices.devices.insert(
          device.id(),
          Device {
            resource: device,
            client: client.clone(),
            focused: None,
            selection: None,
          },
        );
      }
      wl_data_device_manager::Request::Release => {}
      _ => {}
    }
  }
}

impl Dispatch<WlDataSource, SourceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlDataSource,
    request: wl_data_source::Request,
    _data: &SourceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_data_source::Request::Offer { mime_type } => {
        let key = SourceKey::Object(resource.id());
        if let Some(source) = state.data_devices.sources.get_mut(&key)
          && !source.mime_types.contains(&mime_type)
        {
          source.mime_types.push(mime_type);
        }
      }
      wl_data_source::Request::SetActions { dnd_actions } => {
        if let WEnum::Value(actions) = dnd_actions
          && let Some(source) = state
            .data_devices
            .sources
            .get_mut(&SourceKey::Object(resource.id()))
        {
          source.actions = actions;
        }
      }
      wl_data_source::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlDataSource,
    _data: &SourceData,
  ) {
    let id = SourceKey::Object(resource.id());
    state.data_devices.forget(&id);
    if state
      .data_devices
      .drag
      .as_ref()
      .is_some_and(|drag| drag.source == id)
    {
      state.data_devices.drag = None;
    }
  }
}

impl Dispatch<WlDataDevice, DeviceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlDataDevice,
    request: wl_data_device::Request,
    _data: &DeviceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_data_device::Request::StartDrag {
      source: Some(source),
      icon,
      ..
    } = request
    {
      if let Some(icon) = &icon
        && !take_icon_role(state, icon)
      {
        resource.post_error(
          wl_data_device::Error::Role,
          "the drag icon surface has another role",
        );
        return;
      }
      let source_id = SourceKey::Object(source.id());
      let Some(source) = state.data_devices.sources.get_mut(&source_id) else {
        return;
      };
      let Some(resource) = source.resource().cloned() else {
        return;
      };
      if source.used {
        resource.post_error(
          wl_data_device::Error::UsedSource,
          "data source has already been used",
        );
        return;
      }
      source.used = true;
      forget_x11_drag(state);
      let mut drag = Drag::new(source_id, None);
      drag.icon = icon.map(|icon| icon.id());
      state.data_devices.drag = Some(drag);
      return;
    }
    if let wl_data_device::Request::SetSelection { source, .. } = request {
      let source_id =
        source.as_ref().map(|source| SourceKey::Object(source.id()));
      if let Some(source_id) = &source_id {
        let Some(found) = state.data_devices.sources.get_mut(source_id) else {
          return;
        };
        if found.used {
          if let Some(source) = &source {
            source.post_error(
              wl_data_device::Error::UsedSource,
              "data source has already been used",
            );
          }
          return;
        }
        found.used = true;
      }
      state.data_devices.replace_selection(source_id);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlDataDevice,
    _data: &DeviceData,
  ) {
    state.data_devices.devices.remove(&resource.id());
  }
}

impl Dispatch<WlDataOffer, OfferData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlDataOffer,
    request: wl_data_offer::Request,
    _data: &OfferData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_data_offer::Request::Accept { mime_type, .. } => {
        if let Some(drag) = &mut state.data_devices.drag
          && drag.offer.as_ref() == Some(&resource.id())
        {
          drag.accepted.clone_from(&mime_type);
          if let Some(source) = state
            .data_devices
            .sources
            .get(&drag.source)
            .and_then(Source::resource)
          {
            source.target(mime_type);
          }
        }
      }
      wl_data_offer::Request::SetActions {
        dnd_actions,
        preferred_action,
      } => {
        let (WEnum::Value(supported), WEnum::Value(preferred)) =
          (dnd_actions, preferred_action)
        else {
          return;
        };
        if let Some(drag) = &mut state.data_devices.drag
          && drag.offer.as_ref() == Some(&resource.id())
        {
          let source = &state.data_devices.sources[&drag.source];
          drag.action = select_action(source, supported, preferred);
          resource.action(drag.action);
          if let Some(source) = source.resource() {
            source.action(drag.action);
          }
        }
      }
      wl_data_offer::Request::Finish => {
        if let Some(drag) = &state.data_devices.drag
          && drag.offer.as_ref() == Some(&resource.id())
          && drag.dropped
          && drag.is_accepted()
          && drag.source.is_x11()
        {
          let action = drag.action;
          end_x11_drag(state, Some(action));
        } else if let Some(drag) = &state.data_devices.drag
          && drag.offer.as_ref() == Some(&resource.id())
          && drag.dropped
          && drag.is_accepted()
          && let Some(source) = state
            .data_devices
            .sources
            .get(&drag.source)
            .and_then(Source::resource)
        {
          source.dnd_finished();
        }
      }
      wl_data_offer::Request::Receive { mime_type, fd } => {
        if let Some(source_id) =
          state.data_devices.offers.get(&resource.id()).cloned()
        {
          receive_from(state, &source_id, mime_type, fd);
        }
      }
      wl_data_offer::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlDataOffer,
    _data: &OfferData,
  ) {
    state.data_devices.offers.remove(&resource.id());
    // An X client's drop whose offer goes without finish was not
    // performed, as wlroots' data_offer_destroy decides.
    if state.data_devices.drag.as_ref().is_some_and(|drag| {
      drag.source.is_x11()
        && drag.dropped
        && drag.offer.as_ref() == Some(&resource.id())
    }) {
      end_x11_drag(state, None);
    }
  }
}
