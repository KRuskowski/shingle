//! The primary selection protocol, with sources of the same origins as
//! `wl_data_device`'s clipboard (crates/shingle/src/data_device.rs).

use std::{
  collections::HashMap,
  os::fd::{AsFd, OwnedFd},
};

use wayland_protocols::wp::primary_selection::zv1::server::{
  zwp_primary_selection_device_manager_v1::{
    self, ZwpPrimarySelectionDeviceManagerV1,
  },
  zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
  zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
  zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{data_device::SourceKey, state::CompositorState};

pub(crate) struct SourceData;
struct DeviceData;
struct OfferData;

/// Who offers the primary selection.
#[derive(Clone, Debug)]
enum SourceOrigin {
  /// A client's `zwp_primary_selection_source_v1`.
  Client(ZwpPrimarySelectionSourceV1),
  /// A wlr-data-control or ext-data-control client's source.
  Control(crate::data_control::ControlSource),
  /// An X client, through the window manager.
  X11,
  /// The host session, through the nested backend.
  Host,
}

struct Source {
  origin: SourceOrigin,
  /// The Wayland client behind it; `None` for an X11 or host source.
  client: Option<ClientId>,
  mime_types: Vec<String>,
}

impl Source {
  /// Tells the source it is no longer the selection.
  fn cancel(&self) {
    match &self.origin {
      SourceOrigin::Client(resource) => resource.cancelled(),
      SourceOrigin::Control(control) => control.cancelled(),
      SourceOrigin::X11 | SourceOrigin::Host => {}
    }
  }
}

struct Device {
  resource: ZwpPrimarySelectionDeviceV1,
  client: Client,
  focused: Option<ClientId>,
  selection: Option<SourceKey>,
}

#[derive(Default)]
pub(crate) struct PrimarySelection {
  sources: HashMap<SourceKey, Source>,
  devices: HashMap<ObjectId, Device>,
  offers: HashMap<ObjectId, SourceKey>,
  selection: Option<SourceKey>,
  /// The number of the last X11 source.
  x11_sources: u64,
  /// The number of the last host source.
  host_sources: u64,
}

impl PrimarySelection {
  /// How many sources, devices and offers are tracked.
  pub(crate) fn totals(&self) -> (usize, usize, usize) {
    (self.sources.len(), self.devices.len(), self.offers.len())
  }

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
  /// is the same. A replaced X11 or host source is forgotten.
  fn replace_selection(&mut self, key: Option<SourceKey>) {
    if self.selection == key {
      return;
    }
    if let Some(previous) = std::mem::replace(&mut self.selection, key) {
      if let Some(source) = self.sources.get(&previous) {
        source.cancel();
      }
      if previous.is_compositor_owned() {
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
  let id = state.primary_selection.selection.clone()?;
  let source = state.primary_selection.sources.get(&id)?;
  Some((id, source.mime_types.clone()))
}

pub(crate) fn set_control_selection(
  state: &mut CompositorState,
  client: &Client,
  control_source: crate::data_control::ControlSource,
  mime_types: Vec<String>,
) {
  let key = SourceKey::Object(control_source.id());
  state.primary_selection.sources.insert(
    key.clone(),
    Source {
      origin: SourceOrigin::Control(control_source),
      client: Some(client.id()),
      mime_types,
    },
  );
  state.primary_selection.replace_selection(Some(key));
}

/// Installs an X client's primary selection with `mime_types`, replacing
/// whatever was there, and returns its key.
pub(crate) fn set_x11_selection(
  state: &mut CompositorState,
  mime_types: Vec<String>,
) -> SourceKey {
  let data = &mut state.primary_selection;
  data.x11_sources += 1;
  let key = SourceKey::X11(data.x11_sources);
  data.sources.insert(
    key.clone(),
    Source {
      origin: SourceOrigin::X11,
      client: None,
      mime_types,
    },
  );
  data.replace_selection(Some(key.clone()));
  key
}

/// Installs the host session's primary selection with `mime_types`,
/// replacing whatever was there, and returns its key.
pub(crate) fn set_host_selection(
  state: &mut CompositorState,
  mime_types: Vec<String>,
) -> SourceKey {
  let data = &mut state.primary_selection;
  data.host_sources += 1;
  let key = SourceKey::Host(data.host_sources);
  data.sources.insert(
    key.clone(),
    Source {
      origin: SourceOrigin::Host,
      client: None,
      mime_types,
    },
  );
  data.replace_selection(Some(key.clone()));
  key
}

/// Clears the primary selection if the host session's source holds it.
pub(crate) fn clear_host_selection(state: &mut CompositorState) {
  if state
    .primary_selection
    .selection
    .as_ref()
    .is_some_and(SourceKey::is_host)
  {
    state.primary_selection.replace_selection(None);
  }
}

/// Clears the primary selection if an X client's source holds it.
pub(crate) fn clear_x11_selection(state: &mut CompositorState) {
  if state
    .primary_selection
    .selection
    .as_ref()
    .is_some_and(SourceKey::is_x11)
  {
    state.primary_selection.replace_selection(None);
  }
}

/// Asks `source` for `mime_type` in `fd`; see
/// [`crate::data_device::receive_from`].
pub(crate) fn receive_from(
  state: &mut CompositorState,
  source: &SourceKey,
  mime_type: String,
  fd: OwnedFd,
) {
  let Some(found) = state.primary_selection.sources.get(source) else {
    return;
  };
  if !found.mime_types.contains(&mime_type) {
    return;
  }
  match &found.origin {
    SourceOrigin::Client(resource) => resource.send(mime_type, fd.as_fd()),
    SourceOrigin::Control(control) => control.send(mime_type, fd.as_fd()),
    SourceOrigin::X11 => state.xwayland.selection_receives.push((
      shingle_xwayland::Selection::Primary,
      mime_type,
      fd,
    )),
    SourceOrigin::Host => crate::host_selection::request(
      state,
      crate::host_selection::SelectionKind::Primary,
      source.clone(),
      mime_type,
      fd,
    ),
  }
}

pub(crate) fn remove_control_source(
  state: &mut CompositorState,
  control: &ObjectId,
) {
  let key = SourceKey::Object(control.clone());
  if state
    .primary_selection
    .sources
    .get(&key)
    .is_some_and(|source| matches!(source.origin, SourceOrigin::Control(_)))
  {
    state.primary_selection.forget(&key);
  }
}

pub(crate) fn clear_selection(state: &mut CompositorState) {
  state.primary_selection.replace_selection(None);
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle
    .create_global::<CompositorState, ZwpPrimarySelectionDeviceManagerV1, ()>(
      1,
      (),
    );
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
  let data = &mut state.primary_selection;
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
    let Ok(offer): Result<ZwpPrimarySelectionOfferV1, _> = device
      .client
      .create_resource::<_, OfferData, CompositorState>(
      handle,
      device.resource.version(),
      OfferData,
    ) else {
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

impl GlobalDispatch<ZwpPrimarySelectionDeviceManagerV1, ()>
  for CompositorState
{
  crate::security_context::protocol_can_view!(
    "zwp_primary_selection_device_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpPrimarySelectionDeviceManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &ZwpPrimarySelectionDeviceManagerV1,
    request: zwp_primary_selection_device_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwp_primary_selection_device_manager_v1::Request::CreateSource { id } => {
        let source = data_init.init(id, SourceData);
        state.primary_selection.sources.insert(
          SourceKey::Object(source.id()),
          Source {
            origin: SourceOrigin::Client(source),
            client: Some(client.id()),
            mime_types: Vec::new(),
          },
        );
      }
      zwp_primary_selection_device_manager_v1::Request::GetDevice {
        id,
        ..
      } => {
        let device = data_init.init(id, DeviceData);
        state.primary_selection.devices.insert(
          device.id(),
          Device {
            resource: device,
            client: client.clone(),
            focused: None,
            selection: None,
          },
        );
      }
      zwp_primary_selection_device_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, SourceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpPrimarySelectionSourceV1,
    request: zwp_primary_selection_source_v1::Request,
    _data: &SourceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_primary_selection_source_v1::Request::Offer { mime_type } =
      request
      && let Some(source) = state
        .primary_selection
        .sources
        .get_mut(&SourceKey::Object(resource.id()))
      && !source.mime_types.contains(&mime_type)
    {
      source.mime_types.push(mime_type);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpPrimarySelectionSourceV1,
    _data: &SourceData,
  ) {
    let id = SourceKey::Object(resource.id());
    state.primary_selection.forget(&id);
  }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, DeviceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpPrimarySelectionDeviceV1,
    request: zwp_primary_selection_device_v1::Request,
    _data: &DeviceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_primary_selection_device_v1::Request::SetSelection {
      source,
      ..
    } = request
    {
      let source_id =
        source.as_ref().map(|source| SourceKey::Object(source.id()));
      if source_id
        .as_ref()
        .is_some_and(|id| !state.primary_selection.sources.contains_key(id))
      {
        return;
      }
      state.primary_selection.replace_selection(source_id);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpPrimarySelectionDeviceV1,
    _data: &DeviceData,
  ) {
    state.primary_selection.devices.remove(&resource.id());
  }
}

impl Dispatch<ZwpPrimarySelectionOfferV1, OfferData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpPrimarySelectionOfferV1,
    request: zwp_primary_selection_offer_v1::Request,
    _data: &OfferData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_primary_selection_offer_v1::Request::Receive { mime_type, fd } =
      request
      && let Some(source_id) =
        state.primary_selection.offers.get(&resource.id()).cloned()
    {
      receive_from(state, &source_id, mime_type, fd);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpPrimarySelectionOfferV1,
    _data: &OfferData,
  ) {
    state.primary_selection.offers.remove(&resource.id());
  }
}
