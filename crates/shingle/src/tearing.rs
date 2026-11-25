use std::collections::HashMap;

use wayland_protocols::wp::tearing_control::v1::server::{
  wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
  wp_tearing_control_v1::{self, WpTearingControlV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
};

use crate::state::CompositorState;

struct TearingControl {
  surface: ObjectId,
}

#[derive(Default)]
pub(crate) struct TearingControls {
  objects: HashMap<ObjectId, TearingControl>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpTearingControlManagerV1, ()>(1, ());
}

impl GlobalDispatch<WpTearingControlManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wp_tearing_control_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpTearingControlManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpTearingControlManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpTearingControlManagerV1,
    request: wp_tearing_control_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wp_tearing_control_manager_v1::Request::GetTearingControl {
        id,
        surface,
      } => {
        let control = data_init.init(id, ());
        let surface = Resource::id(&surface);
        if state
          .tearing_controls
          .objects
          .values()
          .any(|control| control.surface == surface)
        {
          resource.post_error(
            wp_tearing_control_manager_v1::Error::TearingControlExists,
            "wl_surface already has a tearing control",
          );
          return;
        }
        state
          .tearing_controls
          .objects
          .insert(Resource::id(&control), TearingControl { surface });
      }
      wp_tearing_control_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<WpTearingControlV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpTearingControlV1,
    request: wp_tearing_control_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let Some(surface) = state
      .tearing_controls
      .objects
      .get(&Resource::id(resource))
      .map(|control| control.surface.clone())
    else {
      return;
    };
    let tearing = match request {
      wp_tearing_control_v1::Request::SetPresentationHint {
        hint: WEnum::Value(wp_tearing_control_v1::PresentationHint::Async),
      } => true,
      wp_tearing_control_v1::Request::SetPresentationHint {
        hint: WEnum::Value(wp_tearing_control_v1::PresentationHint::Vsync),
      } => false,
      wp_tearing_control_v1::Request::Destroy => false,
      _ => return,
    };
    if let Some(surface) = state.surfaces.get_mut(&surface) {
      surface.pending.tearing = Some(tearing);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WpTearingControlV1,
    _data: &(),
  ) {
    state
      .tearing_controls
      .objects
      .remove(&Resource::id(resource));
  }
}
