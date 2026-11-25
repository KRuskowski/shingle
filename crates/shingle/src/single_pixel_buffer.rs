use spb_server::wp_single_pixel_buffer_manager_v1::{
  self, WpSinglePixelBufferManagerV1,
};
use wayland_protocols::wp::single_pixel_buffer::v1::server as spb_server;
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::{
  buffer::Attachment,
  state::{BufferData, CompositorState},
};

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle
    .create_global::<CompositorState, WpSinglePixelBufferManagerV1, ()>(1, ());
}

impl GlobalDispatch<WpSinglePixelBufferManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!(
    "wp_single_pixel_buffer_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpSinglePixelBufferManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &WpSinglePixelBufferManagerV1,
    request: wp_single_pixel_buffer_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wp_single_pixel_buffer_manager_v1::Request::Destroy => {}
      wp_single_pixel_buffer_manager_v1::Request::CreateU32RgbaBuffer {
        id,
        r,
        g,
        b,
        a,
      } => {
        let buffer = data_init.init(id, BufferData);
        let rgba = [r, g, b, a].map(channel);
        state
          .buffers
          .insert(buffer.id(), Attachment::single_pixel(buffer, rgba));
      }
      _ => {}
    }
  }
}

fn channel(value: u32) -> u8 {
  ((u64::from(value) * 255 + u64::from(u32::MAX) / 2) / u64::from(u32::MAX))
    as u8
}
