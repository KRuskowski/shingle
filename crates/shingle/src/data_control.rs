//! Clipboard managers' access to the selections: wlr-data-control-unstable-v1
//! (`zwlr_data_control_manager_v1`) and its standard successor
//! ext-data-control-v1 (`ext_data_control_manager_v1`), which newer
//! clipboard managers prefer, as Wayfire offers both.
//!
//! The two protocols are the same requests and events under other names,
//! except that ext-data-control always has the primary selection, where
//! wlr-data-control has it from version 2. Their sources, devices and
//! offers are kept together here, as [`ControlSource`], `Device` and
//! `Offer` values of either protocol, and a source of one is a selection
//! like any other for clients of the other.

use std::{collections::HashMap, os::fd::BorrowedFd};

use wayland_protocols::ext::data_control::v1::server::{
  ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
  ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
  ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
  ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
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

/// A data-control client's source, of either protocol.
#[derive(Clone, Debug)]
pub(crate) enum ControlSource {
  Wlr(ZwlrDataControlSourceV1),
  Ext(ExtDataControlSourceV1),
}

impl ControlSource {
  pub(crate) fn id(&self) -> ObjectId {
    match self {
      Self::Wlr(source) => source.id(),
      Self::Ext(source) => source.id(),
    }
  }

  /// Tells the source it is no longer the selection.
  pub(crate) fn cancelled(&self) {
    match self {
      Self::Wlr(source) => source.cancelled(),
      Self::Ext(source) => source.cancelled(),
    }
  }

  /// Asks the source to write `mime_type` into `fd`.
  pub(crate) fn send(&self, mime_type: String, fd: BorrowedFd<'_>) {
    match self {
      Self::Wlr(source) => source.send(mime_type, fd),
      Self::Ext(source) => source.send(mime_type, fd),
    }
  }
}

/// A data-control device, of either protocol.
#[derive(Clone, Debug)]
enum DeviceResource {
  Wlr(ZwlrDataControlDeviceV1),
  Ext(ExtDataControlDeviceV1),
}

/// A data-control offer, of either protocol.
#[derive(Clone, Debug)]
enum Offer {
  Wlr(ZwlrDataControlOfferV1),
  Ext(ExtDataControlOfferV1),
}

impl Offer {
  fn id(&self) -> ObjectId {
    match self {
      Self::Wlr(offer) => offer.id(),
      Self::Ext(offer) => offer.id(),
    }
  }

  fn offer(&self, mime_type: String) {
    match self {
      Self::Wlr(offer) => offer.offer(mime_type),
      Self::Ext(offer) => offer.offer(mime_type),
    }
  }
}

impl DeviceResource {
  /// Whether the device has the primary selection.
  fn has_primary(&self) -> bool {
    match self {
      Self::Wlr(device) => device.version() >= 2,
      Self::Ext(_) => true,
    }
  }

  /// Introduces a new offer to the device's `client`.
  fn create_offer(
    &self,
    client: &Client,
    handle: &DisplayHandle,
  ) -> Option<Offer> {
    match self {
      Self::Wlr(device) => {
        let offer = client
          .create_resource::<ZwlrDataControlOfferV1, (), CompositorState>(
            handle,
            device.version(),
            (),
          )
          .ok()?;
        device.data_offer(&offer);
        Some(Offer::Wlr(offer))
      }
      Self::Ext(device) => {
        let offer = client
          .create_resource::<ExtDataControlOfferV1, (), CompositorState>(
            handle,
            device.version(),
            (),
          )
          .ok()?;
        device.data_offer(&offer);
        Some(Offer::Ext(offer))
      }
    }
  }

  fn selection(&self, offer: Option<&Offer>) {
    match (self, offer) {
      (Self::Wlr(device), Some(Offer::Wlr(offer))) => {
        device.selection(Some(offer));
      }
      (Self::Wlr(device), _) => device.selection(None),
      (Self::Ext(device), Some(Offer::Ext(offer))) => {
        device.selection(Some(offer));
      }
      (Self::Ext(device), _) => device.selection(None),
    }
  }

  fn primary_selection(&self, offer: Option<&Offer>) {
    match (self, offer) {
      (Self::Wlr(device), Some(Offer::Wlr(offer))) => {
        device.primary_selection(Some(offer));
      }
      (Self::Wlr(device), _) => device.primary_selection(None),
      (Self::Ext(device), Some(Offer::Ext(offer))) => {
        device.primary_selection(Some(offer));
      }
      (Self::Ext(device), _) => device.primary_selection(None),
    }
  }

  /// Ends the client for setting a source that was used already.
  fn used_source(&self) {
    let message = "data source has already been used";
    match self {
      Self::Wlr(device) => device
        .post_error(zwlr_data_control_device_v1::Error::UsedSource, message),
      Self::Ext(device) => device
        .post_error(ext_data_control_device_v1::Error::UsedSource, message),
    }
  }
}

struct Source {
  resource: ControlSource,
  client: ClientId,
  mime_types: Vec<String>,
  used: bool,
}

struct Device {
  resource: DeviceResource,
  client: Client,
  /// The selection last announced; `None` before the first, which the
  /// protocol sends at once, even when there is no selection.
  selection: Option<Option<SourceKey>>,
  /// The same for the primary selection, where the device has it. Its
  /// first event tells clients such as wl-copy that primary is supported.
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
    destroy_source(state, &source);
  }
  state
    .data_control
    .devices
    .retain(|_, device| !clients.contains(&device.client.id()));
  let alive = |state: &CompositorState, offer: &ObjectId| {
    state
      .data_control
      .offer_clients
      .get(offer)
      .is_some_and(|client| !clients.contains(client))
  };
  let offers = std::mem::take(&mut state.data_control.offers);
  state.data_control.offers = offers
    .into_iter()
    .filter(|(offer, _)| alive(state, offer))
    .collect();
  let offers = std::mem::take(&mut state.data_control.primary_offers);
  state.data_control.primary_offers = offers
    .into_iter()
    .filter(|(offer, _)| alive(state, offer))
    .collect();
  state
    .data_control
    .offer_clients
    .retain(|_, client| !clients.contains(client));
}

/// Offers both managers: wlr-data-control at version 2 and
/// ext-data-control at version 1.
pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrDataControlManagerV1, ()>(2, ());
  handle.create_global::<CompositorState, ExtDataControlManagerV1, ()>(1, ());
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
    if device.resource.has_primary()
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
) -> Option<Offer> {
  let offer = device.resource.create_offer(&device.client, handle)?;
  for mime_type in mime_types {
    offer.offer(mime_type.clone());
  }
  Some(offer)
}

fn create_source(
  state: &mut CompositorState,
  client: &Client,
  resource: ControlSource,
) {
  state.data_control.sources.insert(
    resource.id(),
    Source {
      resource,
      client: client.id(),
      mime_types: Vec::new(),
      used: false,
    },
  );
}

fn create_device(
  state: &mut CompositorState,
  client: &Client,
  resource: DeviceResource,
) {
  let id = match &resource {
    DeviceResource::Wlr(device) => device.id(),
    DeviceResource::Ext(device) => device.id(),
  };
  state.data_control.devices.insert(
    id,
    Device {
      resource,
      client: client.clone(),
      selection: None,
      primary_selection: None,
    },
  );
}

fn add_mime_type(state: &mut CompositorState, source: &ObjectId, mime: String) {
  if let Some(source) = state.data_control.sources.get_mut(source)
    && !source.mime_types.contains(&mime)
  {
    source.mime_types.push(mime);
  }
}

fn destroy_source(state: &mut CompositorState, source: &ObjectId) {
  state.data_control.sources.remove(source);
  data_device::remove_control_source(state, source);
  crate::primary_selection::remove_control_source(state, source);
}

/// Makes the source `source`, or nothing for `None`, the selection, or
/// with `primary` the primary selection, for `device` of `client`.
fn set_selection(
  state: &mut CompositorState,
  client: &Client,
  device: &DeviceResource,
  source: Option<ObjectId>,
  primary: bool,
) {
  let Some(source) = source else {
    if primary {
      crate::primary_selection::clear_selection(state);
    } else {
      data_device::clear_selection(state);
    }
    return;
  };
  let Some(control) = state.data_control.sources.get_mut(&source) else {
    return;
  };
  if control.used {
    device.used_source();
    return;
  }
  control.used = true;
  let resource = control.resource.clone();
  let mime_types = control.mime_types.clone();
  if primary {
    crate::primary_selection::set_control_selection(
      state, client, resource, mime_types,
    );
  } else {
    data_device::set_control_selection(state, client, resource, mime_types);
  }
}

/// Asks the source behind `offer` for `mime_type`, into `fd`.
fn receive(
  state: &mut CompositorState,
  offer: &ObjectId,
  mime_type: String,
  fd: std::os::fd::OwnedFd,
) {
  if let Some(source) = state.data_control.offers.get(offer).cloned() {
    data_device::receive_from(state, &source, mime_type, fd);
  } else if let Some(source) =
    state.data_control.primary_offers.get(offer).cloned()
  {
    crate::primary_selection::receive_from(state, &source, mime_type, fd);
  }
}

fn forget_offer(state: &mut CompositorState, offer: &ObjectId) {
  state.data_control.offers.remove(offer);
  state.data_control.primary_offers.remove(offer);
  state.data_control.offer_clients.remove(offer);
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
        let source = ControlSource::Wlr(data_init.init(id, ()));
        create_source(state, client, source);
      }
      zwlr_data_control_manager_v1::Request::GetDataDevice { id, .. } => {
        let device = DeviceResource::Wlr(data_init.init(id, ()));
        create_device(state, client, device);
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
        add_mime_type(state, &resource.id(), mime_type);
      }
      zwlr_data_control_source_v1::Request::Destroy => {
        destroy_source(state, &resource.id());
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
    let device = DeviceResource::Wlr(resource.clone());
    match request {
      zwlr_data_control_device_v1::Request::SetSelection { source } => {
        let source = source.map(|source| source.id());
        set_selection(state, client, &device, source, false);
      }
      zwlr_data_control_device_v1::Request::SetPrimarySelection { source } => {
        let source = source.map(|source| source.id());
        set_selection(state, client, &device, source, true);
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
        receive(state, &resource.id(), mime_type, fd);
      }
      zwlr_data_control_offer_v1::Request::Destroy => {
        forget_offer(state, &resource.id());
      }
      _ => {}
    }
  }
}

impl GlobalDispatch<ExtDataControlManagerV1, ()> for CompositorState {
  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtDataControlManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }

  fn can_view(client: Client, _: &()) -> bool {
    crate::security_context::can_view(client, "ext_data_control_manager_v1")
  }
}

impl Dispatch<ExtDataControlManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &ExtDataControlManagerV1,
    request: ext_data_control_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_data_control_manager_v1::Request::CreateDataSource { id } => {
        let source = ControlSource::Ext(data_init.init(id, ()));
        create_source(state, client, source);
      }
      ext_data_control_manager_v1::Request::GetDataDevice { id, .. } => {
        let device = DeviceResource::Ext(data_init.init(id, ()));
        create_device(state, client, device);
      }
      ext_data_control_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ExtDataControlSourceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtDataControlSourceV1,
    request: ext_data_control_source_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_data_control_source_v1::Request::Offer { mime_type } => {
        add_mime_type(state, &resource.id(), mime_type);
      }
      ext_data_control_source_v1::Request::Destroy => {
        destroy_source(state, &resource.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    resource: &ExtDataControlDeviceV1,
    request: ext_data_control_device_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let device = DeviceResource::Ext(resource.clone());
    match request {
      ext_data_control_device_v1::Request::SetSelection { source } => {
        let source = source.map(|source| source.id());
        set_selection(state, client, &device, source, false);
      }
      ext_data_control_device_v1::Request::SetPrimarySelection { source } => {
        let source = source.map(|source| source.id());
        set_selection(state, client, &device, source, true);
      }
      ext_data_control_device_v1::Request::Destroy => {
        state.data_control.devices.remove(&resource.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtDataControlOfferV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtDataControlOfferV1,
    request: ext_data_control_offer_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_data_control_offer_v1::Request::Receive { mime_type, fd } => {
        receive(state, &resource.id(), mime_type, fd);
      }
      ext_data_control_offer_v1::Request::Destroy => {
        forget_offer(state, &resource.id());
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Write};

  use wayland_client::protocol::wl_seat::WlSeat;
  use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
  };
  use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
  };

  use crate::{test_client::TestClient, tests::test_compositor};

  fn pipe() -> (std::fs::File, std::fs::File) {
    let (read, write) = rustix::pipe::pipe().expect("pipe");
    (std::fs::File::from(read), std::fs::File::from(write))
  }

  #[test]
  fn an_ext_device_announces_both_selections_at_once() {
    let (mut compositor, socket) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &socket);
    let manager = client.bind::<ExtDataControlManagerV1>(1);
    let seat = client.bind::<WlSeat>(5);
    let _device = manager.get_data_device(&seat, &client.handle, ());
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.ext_control_selections, [None]);
    assert_eq!(client.events.ext_control_primary, [None]);
  }

  #[test]
  fn ext_and_wlr_clients_share_the_clipboard_both_ways() {
    let (mut compositor, socket) = test_compositor();
    let mut ext = TestClient::connect(&mut compositor, &socket);
    let ext_manager = ext.bind::<ExtDataControlManagerV1>(1);
    let ext_seat = ext.bind::<WlSeat>(5);
    let ext_device = ext_manager.get_data_device(&ext_seat, &ext.handle, ());
    let mut wlr = TestClient::connect(&mut compositor, &socket);
    let wlr_manager = wlr.bind::<ZwlrDataControlManagerV1>(2);
    let wlr_seat = wlr.bind::<WlSeat>(5);
    let wlr_device: ZwlrDataControlDeviceV1 =
      wlr_manager.get_data_device(&wlr_seat, &wlr.handle, ());
    ext.roundtrip(&mut compositor);
    wlr.roundtrip(&mut compositor);
    // An ext-data-control source becomes the clipboard.
    let source = ext_manager.create_data_source(&ext.handle, ());
    source.offer("text/plain".into());
    ext_device.set_selection(Some(&source));
    ext.roundtrip(&mut compositor);
    wlr.roundtrip(&mut compositor);
    let offer = wlr
      .events
      .control_selections
      .last()
      .cloned()
      .flatten()
      .expect("the wlr client is offered the ext selection");
    assert_eq!(
      wlr.events.offer_types[&wayland_client::Proxy::id(&offer)],
      ["text/plain"]
    );
    let (mut read, write) = pipe();
    offer.receive("text/plain".into(), std::os::fd::AsFd::as_fd(&write));
    drop(write);
    wlr.roundtrip(&mut compositor);
    ext.roundtrip(&mut compositor);
    let (mime_type, fd) = ext.events.control_sends.pop().expect("a send");
    assert_eq!(mime_type, "text/plain");
    std::fs::File::from(fd).write_all(b"shared").unwrap();
    let mut text = String::new();
    read.read_to_string(&mut text).unwrap();
    assert_eq!(text, "shared");
    // The ext device sees its own selection too, as an offer.
    assert!(matches!(
      ext.events.ext_control_selections.last(),
      Some(Some(_))
    ));
    // A wlr source replaces it and cancels the ext source.
    let wlr_source = wlr_manager.create_data_source(&wlr.handle, ());
    wlr_source.offer("text/html".into());
    wlr_device.set_selection(Some(&wlr_source));
    wlr.roundtrip(&mut compositor);
    ext.roundtrip(&mut compositor);
    assert_eq!(
      ext.events.control_cancelled,
      [wayland_client::Proxy::id(&source)]
    );
    let offer = ext
      .events
      .ext_control_selections
      .last()
      .cloned()
      .flatten()
      .expect("the ext client is offered the wlr selection");
    assert_eq!(
      ext.events.offer_types[&wayland_client::Proxy::id(&offer)],
      ["text/html"]
    );
  }

  #[test]
  fn an_ext_source_used_twice_is_a_protocol_error() {
    let (mut compositor, socket) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &socket);
    let manager = client.bind::<ExtDataControlManagerV1>(1);
    let seat = client.bind::<WlSeat>(5);
    let device = manager.get_data_device(&seat, &client.handle, ());
    let source = manager.create_data_source(&client.handle, ());
    device.set_selection(Some(&source));
    device.set_primary_selection(Some(&source));
    let error = client.protocol_error(&mut compositor);
    assert_eq!(
      error.map(|error| error.code),
      Some(ext_data_control_device_v1::Error::UsedSource as u32)
    );
  }
}
