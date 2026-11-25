use std::collections::HashMap;

use wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    WEnum,
    backend::{ClientId, ObjectId},
    protocol::wl_output::WlOutput,
};

use crate::{DpmsState, state::CompositorState};

#[derive(Default)]
pub(crate) struct OutputPower {
    controls: HashMap<ObjectId, ZwlrOutputPowerV1>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle
        .create_global::<CompositorState, ZwlrOutputPowerManagerV1, ()>(1, ());
}

pub(crate) fn sync(state: &mut CompositorState) {
    let mode = mode(state.idle.dpms_state());
    for control in state.output_power.controls.values() {
        control.mode(mode);
    }
}

impl GlobalDispatch<ZwlrOutputPowerManagerV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("zwlr_output_power_manager_v1");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::Destroy => {}
            zwlr_output_power_manager_v1::Request::GetOutputPower {
                id,
                output,
            } => {
                let control = data_init.init(id, ());
                create_control(state, output, control);
            }
            _ => {}
        }
    }
}

fn create_control(
    state: &mut CompositorState,
    output: WlOutput,
    control: ZwlrOutputPowerV1,
) {
    if !state.outputs.contains_key(&output.id()) {
        control.failed();
        return;
    }
    control.mode(mode(state.idle.dpms_state()));
    state.output_power.controls.insert(control.id(), control);
}

impl Dispatch<ZwlrOutputPowerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_power_v1::Request::Destroy => {
                state.output_power.controls.remove(&resource.id());
            }
            zwlr_output_power_v1::Request::SetMode { mode: requested } => {
                let WEnum::Value(requested) = requested else {
                    resource.post_error(
                        zwlr_output_power_v1::Error::InvalidMode,
                        "unknown output power mode",
                    );
                    return;
                };
                let dpms = match requested {
                    zwlr_output_power_v1::Mode::Off => DpmsState::Off,
                    zwlr_output_power_v1::Mode::On => DpmsState::On,
                    _ => {
                        resource.post_error(
                            zwlr_output_power_v1::Error::InvalidMode,
                            "unknown output power mode",
                        );
                        return;
                    }
                };
                let commands = state.idle.set_dpms(dpms, state.now());
                state.process_idle_commands(commands);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputPowerV1,
        _data: &(),
    ) {
        state.output_power.controls.remove(&resource.id());
    }
}

fn mode(state: DpmsState) -> zwlr_output_power_v1::Mode {
    match state {
        DpmsState::Off => zwlr_output_power_v1::Mode::Off,
        DpmsState::On => zwlr_output_power_v1::Mode::On,
    }
}
