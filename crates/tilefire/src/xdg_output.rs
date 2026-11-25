use wayland_protocols::xdg::xdg_output::zv1::server::{
    zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::state::CompositorState;

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, ZxdgOutputManagerV1, ()>(3, ());
}

pub(crate) fn update(state: &mut CompositorState) {
    for output in state.xdg_outputs.values() {
        send_metadata(output, state.output);
    }
}

fn send_metadata(resource: &ZxdgOutputV1, output: tilefire_core::Rect) {
    resource.logical_position(output.x, output.y);
    resource.logical_size(output.width as i32, output.height as i32);
    if resource.version() >= 2 {
        resource.name(crate::OUTPUT_NAME.into());
        resource.description("Tilefire output".into());
    }
    resource.done();
}

impl GlobalDispatch<ZxdgOutputManagerV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("zxdg_output_manager_v1");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZxdgOutputManagerV1,
        request: zxdg_output_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zxdg_output_manager_v1::Request::GetXdgOutput { id, .. } => {
                let output = data_init.init(id, ());
                send_metadata(&output, state.output);
                state.xdg_outputs.insert(output.id(), output);
            }
            zxdg_output_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZxdgOutputV1,
        request: zxdg_output_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_output_v1::Request::Destroy = request {}
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &ZxdgOutputV1,
        _data: &(),
    ) {
        state.xdg_outputs.remove(&resource.id());
    }
}
