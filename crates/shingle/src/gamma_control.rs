use std::{fs::File, io::Read, os::fd::OwnedFd};

use wayland_protocols_wlr::gamma_control::v1::server::{
  zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
  zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
  protocol::wl_output::WlOutput,
};

use crate::state::CompositorState;

const RAMP_SIZE: usize = 256;
const TABLE_BYTES: usize = RAMP_SIZE * 3 * 2;

#[derive(Default)]
pub(crate) struct GammaControl {
  active: Option<ActiveControl>,
  pub(crate) ramp: Vec<u16>,
}

struct ActiveControl {
  id: ObjectId,
  client: ClientId,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrGammaControlManagerV1, ()>(1, ());
}

pub(crate) fn ramp(state: &CompositorState) -> &[u16] {
  if state.gamma_control.ramp.len() == RAMP_SIZE * 3 {
    &state.gamma_control.ramp
  } else {
    identity_ramp()
  }
}

pub(crate) fn cleanup_client(state: &mut CompositorState, client: &ClientId) {
  if state
    .gamma_control
    .active
    .as_ref()
    .is_some_and(|active| active.client == *client)
  {
    restore(state);
  }
}

fn restore(state: &mut CompositorState) {
  state.gamma_control.active = None;
  state.gamma_control.ramp = identity_ramp().to_vec();
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwlr_gamma_control_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwlrGammaControlManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &ZwlrGammaControlManagerV1,
    request: zwlr_gamma_control_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_gamma_control_manager_v1::Request::Destroy => {}
      zwlr_gamma_control_manager_v1::Request::GetGammaControl {
        id,
        output,
      } => {
        let control = data_init.init(id, ());
        create_control(state, client, output, control);
      }
      _ => {}
    }
  }
}

fn create_control(
  state: &mut CompositorState,
  client: &Client,
  output: WlOutput,
  control: ZwlrGammaControlV1,
) {
  if !state.outputs.contains_key(&output.id())
    || state.gamma_control.active.is_some()
  {
    control.failed();
    return;
  }
  control.gamma_size(RAMP_SIZE as u32);
  state.gamma_control.active = Some(ActiveControl {
    id: control.id(),
    client: client.id(),
  });
}

impl Dispatch<ZwlrGammaControlV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrGammaControlV1,
    request: zwlr_gamma_control_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_gamma_control_v1::Request::Destroy => {
        if state
          .gamma_control
          .active
          .as_ref()
          .is_some_and(|active| active.id == resource.id())
        {
          restore(state);
        }
      }
      zwlr_gamma_control_v1::Request::SetGamma { fd } => {
        if state
          .gamma_control
          .active
          .as_ref()
          .is_none_or(|active| active.id != resource.id())
        {
          resource.failed();
          return;
        }
        match read_ramp(fd) {
          Ok(ramp) => state.gamma_control.ramp = ramp,
          Err(error) => resource
            .post_error(zwlr_gamma_control_v1::Error::InvalidGamma, error),
        }
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwlrGammaControlV1,
    _data: &(),
  ) {
    if state
      .gamma_control
      .active
      .as_ref()
      .is_some_and(|active| active.id == resource.id())
    {
      restore(state);
    }
  }
}

fn read_ramp(fd: OwnedFd) -> Result<Vec<u16>, String> {
  let mut file = File::from(fd);
  let mut bytes = vec![0; TABLE_BYTES];
  file.read_exact(&mut bytes).map_err(|error| {
    format!("failed to read complete RGB gamma table: {error}")
  })?;
  let mut trailing = [0];
  if file
    .read(&mut trailing)
    .map_err(|error| error.to_string())?
    != 0
  {
    return Err("gamma table has trailing data".into());
  }
  Ok(
    bytes
      .as_chunks::<2>()
      .0
      .iter()
      .map(|bytes| u16::from_ne_bytes(*bytes))
      .collect(),
  )
}

fn identity_ramp() -> &'static [u16] {
  static IDENTITY: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
  IDENTITY.get_or_init(|| {
    (0..RAMP_SIZE * 3)
      .map(|index| {
        ((index % RAMP_SIZE) as u32 * u32::from(u16::MAX) / 255) as u16
      })
      .collect()
  })
}
