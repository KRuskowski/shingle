use std::collections::HashMap;

use wayland_protocols_wlr::data_control::v1::server::{
  zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
  zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
  zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
  zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{
  data_device::{self, SourceKey},
  state::CompositorState,
};

struct Source {
  resource: ZwlrDataControlSourceV1,
  client: ClientId,
  mime_types: Vec<String>,
  used: bool,
}

struct Device {
  resource: ZwlrDataControlDeviceV1,
  client: Client,
  /// The selection last announced; `None` before the first, which the
  /// protocol sends at once, even when there is no selection.
  selection: Option<Option<SourceKey>>,
  /// The same for the primary selection, from version 2 on. Its first
  /// event tells clients such as wl-copy that primary is supported.
  primary_selection: Option<Option<SourceKey>>,
}

#[derive(Default)]
pub(crate) struct DataControl {
  sources: HashMap<ObjectId, Source>,
  devices: HashMap<ObjectId, Device>,
  offers: HashMap<ObjectId, SourceKey>,
  primary_offers: HashMap<ObjectId, SourceKey>,
  offer_clients: HashMap<ObjectId, ClientId>,
}

impl DataControl {
  /// How many sources, devices and offers (both selections') are
  /// tracked.
  pub(crate) fn totals(&self) -> (usize, usize, usize) {
    (
      self.sources.len(),
      self.devices.len(),
      self.offers.len() + self.primary_offers.len(),
    )
  }
}

pub(crate) fn cleanup_clients(
  state: &mut CompositorState,
  clients: &[ClientId],
) {
  let sources = state
    .data_control
    .sources
    .iter()
    .filter(|(_, source)| clients.contains(&source.client))
    .map(|(id, _)| id.clone())
    .collect::<Vec<_>>();
  for source in sources {
    state.data_control.sources.remove(&source);
    data_device::remove_control_source(state, &source);
    crate::primary_selection::remove_control_source(state, &source);
  }
  state
    .data_control
    .devices
    .retain(|_, device| !clients.contains(&device.client.id()));
  state.data_control.offers.retain(|offer, _| {
    state
      .data_control
      .offer_clients
      .get(offer)
      .is_some_and(|client| !clients.contains(client))
  });
  state.data_control.primary_offers.retain(|offer, _| {
    state
      .data_control
      .offer_clients
      .get(offer)
      .is_some_and(|client| !clients.contains(client))
  });
  state
    .data_control
    .offer_clients
    .retain(|_, client| !clients.contains(client));
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrDataControlManagerV1, ()>(2, ());
}

pub(crate) fn sync(state: &mut CompositorState, handle: &DisplayHandle) {
  let current = data_device::selection(state);
  let primary = crate::primary_selection::selection(state);
  let current_key = current.as_ref().map(|(id, _)| id.clone());
  let primary_key = primary.as_ref().map(|(id, _)| id.clone());
  for device in state.data_control.devices.values_mut() {
    if device.selection.as_ref() != Some(&current_key) {
      device.selection = Some(current_key.clone());
      let offer = current.as_ref().and_then(|(source, mime_types)| {
        let offer = offer(device, handle, mime_types)?;
        state.data_control.offers.insert(offer.id(), source.clone());
        Some(offer)
      });
      if let Some(offer) = &offer {
        state
          .data_control
          .offer_clients
          .insert(offer.id(), device.client.id());
      }
      device.resource.selection(offer.as_ref());
    }
    if device.resource.version() >= 2
      && device.primary_selection.as_ref() != Some(&primary_key)
    {
      device.primary_selection = Some(primary_key.clone());
      let offer = primary.as_ref().and_then(|(source, mime_types)| {
        let offer = offer(device, handle, mime_types)?;
        state
          .data_control
          .primary_offers
          .insert(offer.id(), source.clone());
        Some(offer)
      });
      if let Some(offer) = &offer {
        state
          .data_control
          .offer_clients
          .insert(offer.id(), device.client.id());
      }
      device.resource.primary_selection(offer.as_ref());
    }
  }
}

/// Introduces a new offer of `mime_types` to `device`'s client.
fn offer(
  device: &Device,
  handle: &DisplayHandle,
  mime_types: &[String],
) -> Option<ZwlrDataControlOfferV1> {
  let offer = device
    .client
    .create_resource::<ZwlrDataControlOfferV1, (), CompositorState>(
      handle,
      device.resource.version(),
      (),
    )
    .ok()?;
  device.resource.data_offer(&offer);
  for mime_type in mime_types {
    offer.offer(mime_type.clone());
  }
  Some(offer)
}

impl GlobalDispatch<ZwlrDataControlManagerV1, ()> for CompositorState {
  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwlrDataControlManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }

  fn can_view(client: Client, _: &()) -> bool {
    crate::security_context::can_view(client, "zwlr_data_control_manager_v1")
  }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &ZwlrDataControlManagerV1,
    request: zwlr_data_control_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_data_control_manager_v1::Request::CreateDataSource { id } => {
        let source = data_init.init(id, ());
        state.data_control.sources.insert(
          source.id(),
          Source {
            resource: source,
            client: client.id(),
            mime_types: Vec::new(),
            used: false,
          },
        );
      }
      zwlr_data_control_manager_v1::Request::GetDataDevice { id, .. } => {
        let device = data_init.init(id, ());
        state.data_control.devices.insert(
          device.id(),
          Device {
            resource: device,
            client: client.clone(),
            selection: None,
            primary_selection: None,
          },
        );
      }
      zwlr_data_control_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrDataControlSourceV1,
    request: zwlr_data_control_source_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_data_control_source_v1::Request::Offer { mime_type } => {
        if let Some(source) = state.data_control.sources.get_mut(&resource.id())
          && !source.mime_types.contains(&mime_type)
        {
          source.mime_types.push(mime_type);
        }
      }
      zwlr_data_control_source_v1::Request::Destroy => {
        state.data_control.sources.remove(&resource.id());
        data_device::remove_control_source(state, &resource.id());
        crate::primary_selection::remove_control_source(state, &resource.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    resource: &ZwlrDataControlDeviceV1,
    request: zwlr_data_control_device_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_data_control_device_v1::Request::SetSelection { source } => {
        if let Some(source) = source {
          let Some(control) = state.data_control.sources.get_mut(&source.id())
          else {
            return;
          };
          if control.used {
            resource.post_error(
              zwlr_data_control_device_v1::Error::UsedSource,
              "data source has already been used",
            );
            return;
          }
          control.used = true;
          let control_resource = control.resource.clone();
          let mime_types = control.mime_types.clone();
          data_device::set_control_selection(
            state,
            client,
            control_resource,
            mime_types,
          );
        } else {
          data_device::clear_selection(state);
        }
      }
      zwlr_data_control_device_v1::Request::SetPrimarySelection { source } => {
        if let Some(source) = source {
          let Some(control) = state.data_control.sources.get_mut(&source.id())
          else {
            return;
          };
          if control.used {
            resource.post_error(
              zwlr_data_control_device_v1::Error::UsedSource,
              "data source has already been used",
            );
            return;
          }
          control.used = true;
          let control_resource = control.resource.clone();
          let mime_types = control.mime_types.clone();
          crate::primary_selection::set_control_selection(
            state,
            client,
            control_resource,
            mime_types,
          );
        } else {
          crate::primary_selection::clear_selection(state);
        }
      }
      zwlr_data_control_device_v1::Request::Destroy => {
        state.data_control.devices.remove(&resource.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrDataControlOfferV1,
    request: zwlr_data_control_offer_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_data_control_offer_v1::Request::Receive { mime_type, fd } => {
        if let Some(source) =
          state.data_control.offers.get(&resource.id()).cloned()
        {
          data_device::receive_from(state, &source, mime_type, fd);
        } else if let Some(source) = state
          .data_control
          .primary_offers
          .get(&resource.id())
          .cloned()
        {
          crate::primary_selection::receive_from(state, &source, mime_type, fd);
        }
      }
      zwlr_data_control_offer_v1::Request::Destroy => {
        state.data_control.offers.remove(&resource.id());
        state.data_control.primary_offers.remove(&resource.id());
        state.data_control.offer_clients.remove(&resource.id());
      }
      _ => {}
    }
  }
}
