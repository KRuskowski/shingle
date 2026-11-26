//! wlr-output-power-management: turning outputs off and on, each on its
//! own (docs/design/multi-output.md, section 8). An output a client turns
//! off draws nothing and gets no frames; when the last lit output goes off
//! that is idle DPMS, which turns every output off and, on waking, every
//! output on again.

use std::collections::HashMap;

use shingle_core::OutputId;

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
  /// Each control with the output it controls.
  controls: HashMap<ObjectId, (ZwlrOutputPowerV1, OutputId)>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrOutputPowerManagerV1, ()>(1, ());
}

/// Fails the controls of output `id`, which is gone or renamed.
pub(crate) fn output_removed(state: &mut CompositorState, id: &OutputId) {
  state.output_power.controls.retain(|_, (control, output)| {
    if output != id {
      return true;
    }
    control.failed();
    false
  });
}

/// Idle DPMS changed: waking turns every output on again; then every
/// control hears its output's mode.
pub(crate) fn sync(state: &mut CompositorState) {
  if state.idle.dpms_state() == DpmsState::On {
    for output in state.outputs.iter_mut() {
      output.powered = true;
    }
  }
  for (control, output) in state.output_power.controls.values() {
    control.mode(mode(state, output));
  }
}

/// Whether output `id` is lit: idle DPMS is on and no client turned it
/// off.
pub(crate) fn is_on(state: &CompositorState, id: &OutputId) -> bool {
  state.idle.dpms_state() == DpmsState::On
    && state.outputs.get(id).is_some_and(|output| output.powered)
}

/// Turns output `id` off or on for a client. Turning the last lit output
/// off is idle DPMS off, and turning one on while that is off wakes it.
fn set_power(state: &mut CompositorState, id: &OutputId, on: bool) {
  let Some(output) = state.outputs.get_mut(id) else {
    return;
  };
  output.powered = on;
  let lit = state
    .outputs
    .iter()
    .any(|output| output.powered && !output.dark);
  let wanted = if lit { DpmsState::On } else { DpmsState::Off };
  if wanted != state.idle.dpms_state() {
    let commands = state.idle.set_dpms(wanted, state.now());
    state.process_idle_commands(commands);
  }
  if let Some(output) = state.outputs.get_mut(id) {
    output.powered = on;
  }
  for (control, output) in state.output_power.controls.values() {
    control.mode(mode(state, output));
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
      zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
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
  let Some(id) = state
    .outputs
    .of_resource(&output)
    .map(|output| output.id.clone())
  else {
    control.failed();
    return;
  };
  control.mode(mode(state, &id));
  state
    .output_power
    .controls
    .insert(control.id(), (control, id));
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
        let on = match requested {
          zwlr_output_power_v1::Mode::Off => false,
          zwlr_output_power_v1::Mode::On => true,
          _ => {
            resource.post_error(
              zwlr_output_power_v1::Error::InvalidMode,
              "unknown output power mode",
            );
            return;
          }
        };
        let Some(id) = state
          .output_power
          .controls
          .get(&resource.id())
          .map(|(_, output)| output.clone())
        else {
          return;
        };
        set_power(state, &id, on);
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

fn mode(state: &CompositorState, id: &OutputId) -> zwlr_output_power_v1::Mode {
  if is_on(state, id) {
    zwlr_output_power_v1::Mode::On
  } else {
    zwlr_output_power_v1::Mode::Off
  }
}
