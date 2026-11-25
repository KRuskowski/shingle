use std::{collections::HashMap, os::fd::AsFd};

use wayland_protocols::wp::primary_selection::zv1::server::{
  zwp_primary_selection_device_manager_v1::{
    self, ZwpPrimarySelectionDeviceManagerV1,
  },
  zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
  zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
  zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};
use wayland_protocols_wlr::data_control::v1::server as wlr_data_control;
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};
use wlr_data_control::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;

use crate::state::CompositorState;

pub(crate) struct SourceData;
struct DeviceData;
struct OfferData;

struct Source {
  resource: ZwpPrimarySelectionSourceV1,
  client: ClientId,
  mime_types: Vec<String>,
  control_source: Option<ZwlrDataControlSourceV1>,
}

struct Device {
  resource: ZwpPrimarySelectionDeviceV1,
  client: Client,
  focused: Option<ClientId>,
  selection: Option<ObjectId>,
}

#[derive(Default)]
pub(crate) struct PrimarySelection {
  sources: HashMap<ObjectId, Source>,
  devices: HashMap<ObjectId, Device>,
  offers: HashMap<ObjectId, ObjectId>,
  selection: Option<ObjectId>,
}

impl PrimarySelection {
  pub(crate) fn cleanup_clients(&mut self, clients: &[ClientId]) {
    self
      .sources
      .retain(|_, source| !clients.contains(&source.client));
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
}

pub(crate) fn selection(
  state: &CompositorState,
) -> Option<(ObjectId, Vec<String>)> {
  let id = state.primary_selection.selection.clone()?;
  let source = state.primary_selection.sources.get(&id)?;
  Some((id, source.mime_types.clone()))
}

pub(crate) fn set_control_selection(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  client: &Client,
  control_source: ZwlrDataControlSourceV1,
  mime_types: Vec<String>,
) -> Result<(), ()> {
  let resource: ZwpPrimarySelectionSourceV1 = client
    .create_resource::<_, SourceData, CompositorState>(handle, 1, SourceData)
    .map_err(|_| ())?;
  let id = resource.id();
  state.primary_selection.sources.insert(
    id.clone(),
    Source {
      resource,
      client: client.id(),
      mime_types,
      control_source: Some(control_source),
    },
  );
  if let Some(previous) = state.primary_selection.selection.replace(id)
    && let Some(previous) = state.primary_selection.sources.get(&previous)
  {
    previous.resource.cancelled();
    if let Some(control) = &previous.control_source {
      control.cancelled();
    }
  }
  Ok(())
}

pub(crate) fn receive_from(
  state: &CompositorState,
  source: &ObjectId,
  mime_type: String,
  fd: std::os::fd::OwnedFd,
) {
  let Some(source) = state.primary_selection.sources.get(source) else {
    return;
  };
  if !source.mime_types.contains(&mime_type) {
    return;
  }
  if let Some(control) = &source.control_source {
    control.send(mime_type, fd.as_fd());
  } else {
    source.resource.send(mime_type, fd.as_fd());
  }
}

pub(crate) fn remove_control_source(
  state: &mut CompositorState,
  control: &ObjectId,
) {
  let removed = state
    .primary_selection
    .sources
    .iter()
    .filter_map(|(id, source)| {
      (source.control_source.as_ref().map(Resource::id).as_ref()
        == Some(control))
      .then_some(id.clone())
    })
    .collect::<Vec<_>>();
  for id in removed {
    state.primary_selection.sources.remove(&id);
    state
      .primary_selection
      .offers
      .retain(|_, source| source != &id);
    if state.primary_selection.selection.as_ref() == Some(&id) {
      state.primary_selection.selection = None;
    }
  }
}

pub(crate) fn clear_selection(state: &mut CompositorState) {
  if let Some(previous) = state.primary_selection.selection.take()
    && let Some(previous) = state.primary_selection.sources.get(&previous)
  {
    previous.resource.cancelled();
    if let Some(control) = &previous.control_source {
      control.cancelled();
    }
  }
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
          source.id(),
          Source {
            resource: source,
            client: client.id(),
            mime_types: Vec::new(),
            control_source: None,
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
      && let Some(source) =
        state.primary_selection.sources.get_mut(&resource.id())
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
    let id = resource.id();
    state.primary_selection.sources.remove(&id);
    state
      .primary_selection
      .offers
      .retain(|_, source| source != &id);
    if state.primary_selection.selection == Some(id) {
      state.primary_selection.selection = None;
    }
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
      let source_id = source.as_ref().map(Resource::id);
      if source_id
        .as_ref()
        .is_some_and(|id| !state.primary_selection.sources.contains_key(id))
      {
        return;
      }
      if state.primary_selection.selection != source_id {
        if let Some(previous) =
          std::mem::replace(&mut state.primary_selection.selection, source_id)
          && let Some(previous) = state.primary_selection.sources.get(&previous)
        {
          previous.resource.cancelled();
          if let Some(control) = &previous.control_source {
            control.cancelled();
          }
        }
      } else {
        state.primary_selection.selection = source_id;
      }
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
