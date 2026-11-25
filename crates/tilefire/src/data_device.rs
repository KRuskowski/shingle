use std::{collections::HashMap, os::fd::AsFd};

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

use crate::state::CompositorState;

pub(crate) struct SourceData;
struct DeviceData;
struct OfferData;

struct Source {
    resource: WlDataSource,
    client: ClientId,
    mime_types: Vec<String>,
    used: bool,
    actions: wl_data_device_manager::DndAction,
    control_source: Option<wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
}

struct Drag {
    source: ObjectId,
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
    selection: Option<ObjectId>,
}

#[derive(Default)]
pub(crate) struct DataDevices {
    sources: HashMap<ObjectId, Source>,
    devices: HashMap<ObjectId, Device>,
    offers: HashMap<ObjectId, ObjectId>,
    selection: Option<ObjectId>,
    drag: Option<Drag>,
}

impl DataDevices {
    pub(crate) fn cleanup_clients(&mut self, clients: &[ClientId]) {
        self.sources
            .retain(|_, source| !clients.contains(&source.client));
        self.devices
            .retain(|_, device| !clients.contains(&device.client.id()));
        self.offers
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
    let id = state.data_devices.selection.clone()?;
    let source = state.data_devices.sources.get(&id)?;
    Some((id, source.mime_types.clone()))
}

pub(crate) fn set_control_selection(
    state: &mut CompositorState,
    handle: &DisplayHandle,
    client: &Client,
    control_source: wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    mime_types: Vec<String>,
) -> Result<(), ()> {
    let resource = client
        .create_resource::<WlDataSource, SourceData, CompositorState>(
            handle, 1, SourceData,
        )
        .map_err(|_| ())?;
    let id = resource.id();
    state.data_devices.sources.insert(
        id.clone(),
        Source {
            resource,
            client: client.id(),
            mime_types,
            used: true,
            actions: wl_data_device_manager::DndAction::Copy,
            control_source: Some(control_source),
        },
    );
    if let Some(previous) = state.data_devices.selection.replace(id)
        && let Some(previous) = state.data_devices.sources.get(&previous)
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
    let Some(source) = state.data_devices.sources.get(source) else {
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
        .data_devices
        .sources
        .iter()
        .filter_map(|(id, source)| {
            (source.control_source.as_ref().map(Resource::id).as_ref()
                == Some(control))
            .then_some(id.clone())
        })
        .collect::<Vec<_>>();
    for id in removed {
        state.data_devices.sources.remove(&id);
        state.data_devices.offers.retain(|_, source| source != &id);
        if state.data_devices.selection.as_ref() == Some(&id) {
            state.data_devices.selection = None;
        }
    }
}

pub(crate) fn clear_selection(state: &mut CompositorState) {
    if let Some(previous) = state.data_devices.selection.take()
        && let Some(previous) = state.data_devices.sources.get(&previous)
    {
        previous.resource.cancelled();
        if let Some(control) = &previous.control_source {
            control.cancelled();
        }
    }
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
        if device.focused == focused_client
            && device.selection == data.selection
        {
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
    let Some((source_id, previous, dropped)) =
        state.data_devices.drag.as_ref().map(|drag| {
            (drag.source.clone(), drag.current.clone(), drag.dropped)
        })
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
            let geometry = crate::shell::surface_geometry(state, &target)
                .unwrap_or_default();
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
        let time = state.now().as_millis() as u32;
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
    let source = state.data_devices.sources[&drag.source].resource.clone();
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
                    source.id(),
                    Source {
                        resource: source,
                        client: client.id(),
                        mime_types: Vec::new(),
                        used: false,
                        actions: wl_data_device_manager::DndAction::Copy,
                        control_source: None,
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
                if let Some(source) =
                    state.data_devices.sources.get_mut(&resource.id())
                    && !source.mime_types.contains(&mime_type)
                {
                    source.mime_types.push(mime_type);
                }
            }
            wl_data_source::Request::SetActions { dnd_actions } => {
                if let WEnum::Value(actions) = dnd_actions
                    && let Some(source) =
                        state.data_devices.sources.get_mut(&resource.id())
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
        let id = resource.id();
        state.data_devices.sources.remove(&id);
        state.data_devices.offers.retain(|_, source| source != &id);
        if state.data_devices.selection == Some(id.clone()) {
            state.data_devices.selection = None;
        }
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
            let source_id = source.id();
            let Some(source) = state.data_devices.sources.get_mut(&source_id)
            else {
                return;
            };
            if source.used {
                source.resource.post_error(
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
            let source_id = source.as_ref().map(Resource::id);
            if let Some(source_id) = &source_id {
                let Some(source) =
                    state.data_devices.sources.get_mut(source_id)
                else {
                    return;
                };
                if source.used {
                    source.resource.post_error(
                        wl_data_device::Error::UsedSource,
                        "data source has already been used",
                    );
                    return;
                }
                source.used = true;
            }
            if state.data_devices.selection != source_id {
                if let Some(previous) = std::mem::replace(
                    &mut state.data_devices.selection,
                    source_id,
                ) && let Some(previous) =
                    state.data_devices.sources.get(&previous)
                {
                    previous.resource.cancelled();
                    if let Some(control) = &previous.control_source {
                        control.cancelled();
                    }
                }
            } else {
                state.data_devices.selection = source_id;
            }
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
                    if let Some(source) =
                        state.data_devices.sources.get(&drag.source)
                    {
                        source.resource.target(mime_type);
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
                    source.resource.action(drag.action);
                }
            }
            wl_data_offer::Request::Finish => {
                if let Some(drag) = &state.data_devices.drag
                    && drag.offer.as_ref() == Some(&resource.id())
                    && drag.dropped
                    && drag.accepted.is_some()
                    && drag.action != wl_data_device_manager::DndAction::empty()
                    && let Some(source) =
                        state.data_devices.sources.get(&drag.source)
                {
                    source.resource.dnd_finished();
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
