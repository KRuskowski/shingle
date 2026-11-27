use wayland_protocols::wp::pointer_warp::v1::server::wp_pointer_warp_v1::{
  self, WpPointerWarpV1,
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::{Event, shell, state::CompositorState};

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpPointerWarpV1, ()>(1, ());
}

impl GlobalDispatch<WpPointerWarpV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wp_pointer_warp_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpPointerWarpV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpPointerWarpV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &WpPointerWarpV1,
    request: wp_pointer_warp_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wp_pointer_warp_v1::Request::WarpPointer {
      surface,
      pointer,
      x,
      y,
      serial,
    } = request
    {
      let object = Resource::id(&surface);
      let valid = state.input.pointer_focus.as_ref() == Some(&object)
        && state.input.pointer_serial == serial
        && state.surfaces.get(&object).is_some_and(|surface| {
          state
            .input
            .pointers
            .get(&Resource::id(&pointer))
            .and_then(|pointer| pointer.client())
            .is_some_and(|client| client.id() == surface.client)
        });
      if !valid {
        return;
      }
      let Some(geometry) = shell::surface_rect(state, &object) else {
        return;
      };
      if x < 0.0
        || y < 0.0
        || x >= f64::from(geometry.width)
        || y >= f64::from(geometry.height)
      {
        return;
      }
      crate::input::pointer_warp(
        state,
        f64::from(geometry.x) + x,
        f64::from(geometry.y) + y,
      );
      let output = crate::output::output_at(
        state,
        state.input.pointer_x,
        state.input.pointer_y,
      )
      .map(|id| crate::OutputHandle::from(&id));
      state.events.push(Event::PointerWarp {
        x: state.input.pointer_x,
        y: state.input.pointer_y,
        output,
      });
    }
  }
}
