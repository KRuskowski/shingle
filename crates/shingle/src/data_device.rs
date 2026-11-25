//! `wl_data_device`: the clipboard and drag and drop.
//!
//! A selection's source is a [`Source`] with a [`SourceOrigin`]: a
//! client's `wl_data_source`, a wlr-data-control source
//! (crates/shingle/src/data_control.rs) or an X client through the
//! XWayland selection bridge (crates/shingle/src/xwayland/selection.rs),
//! which has no Wayland object. Sources are keyed by [`SourceKey`].

use std::{
  collections::HashMap,
  os::fd::{AsFd, OwnedFd},
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
  },
};
use wlr_data_control::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;

use crate::state::CompositorState;

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
  /// An X client, through the window manager.
  X11,
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
      SourceOrigin::X11 => {}
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

struct Drag {
  source: SourceKey,
  current: Option<ObjectId>,
  offer: Option<ObjectId>,
  accepted: Option<String>,
  action: wl_data_device_manager::DndAction,
  dropped: bool,
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

pub(crate) fn sync_drag(state: &mut CompositorState, handle: &DisplayHandle) {
  let Some((source_id, previous, dropped)) = state
    .data_devices
    .drag
    .as_ref()
    .map(|drag| (drag.source.clone(), drag.current.clone(), drag.dropped))
  else {
    return;
  };
  if dropped {
    return;
  }
  if !state.data_devices.sources.contains_key(&source_id) {
    state.data_devices.drag = None;
    return;
  }
  let target = state.input.pointer_focus.clone();
  if previous != target {
    if let Some(previous) = previous
      && let Some(surface) = state.surfaces.get(&previous)
    {
      for device in state.data_devices.devices.values() {
        if device.client.id() == surface.client {
          device.resource.leave();
        }
      }
    }
    let mut new_offer = None;
    if let Some(target) = target.clone() {
      let Some(surface) = state.surfaces.get(&target) else {
        return;
      };
      let Some(device) = state
        .data_devices
        .devices
        .values()
        .find(|device| device.client.id() == surface.client)
        .cloned()
      else {
        return;
      };
      let Ok(offer) = device
        .client
        .create_resource::<WlDataOffer, OfferData, CompositorState>(
          handle,
          device.resource.version(),
          OfferData,
        )
      else {
        return;
      };
      let source = &state.data_devices.sources[&source_id];
      device.resource.data_offer(&offer);
      for mime_type in &source.mime_types {
        offer.offer(mime_type.clone());
      }
      if offer.version() >= 3 {
        offer.source_actions(source.actions);
        offer.action(wl_data_device_manager::DndAction::Copy);
      }
      let geometry =
        crate::shell::surface_geometry(state, &target).unwrap_or_default();
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
      new_offer = Some(offer.id());
    }
    if let Some(drag) = &mut state.data_devices.drag {
      drag.current = target;
      drag.offer = new_offer;
      drag.accepted = None;
      drag.action = wl_data_device_manager::DndAction::Copy;
    }
  } else if let Some(target) = target {
    let Some(surface) = state.surfaces.get(&target) else {
      return;
    };
    let geometry =
      crate::shell::surface_geometry(state, &target).unwrap_or_default();
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
}

pub(crate) fn drop_drag(state: &mut CompositorState) {
  let Some(drag) = &mut state.data_devices.drag else {
    return;
  };
  if drag.dropped {
    return;
  }
  drag.dropped = true;
  let Some(source) = state
    .data_devices
    .sources
    .get(&drag.source)
    .and_then(Source::resource)
    .cloned()
  else {
    return;
  };
  let accepted = drag.accepted.is_some()
    && drag.action != wl_data_device_manager::DndAction::empty();
  if let Some(target) = &drag.current
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
    _resource: &WlDataDevice,
    request: wl_data_device::Request,
    _data: &DeviceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_data_device::Request::StartDrag {
      source: Some(source),
      ..
    } = request
    {
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
      state.data_devices.drag = Some(Drag {
        source: source_id,
        current: None,
        offer: None,
        accepted: None,
        action: wl_data_device_manager::DndAction::Copy,
        dropped: false,
      });
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
          drag.accepted = mime_type.clone();
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
          && drag.accepted.is_some()
          && drag.action != wl_data_device_manager::DndAction::empty()
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
  }
}
