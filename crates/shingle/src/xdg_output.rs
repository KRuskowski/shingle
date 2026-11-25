use wayland_protocols::xdg::xdg_output::zv1::server::{
  zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
  zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  protocol::wl_output::WlOutput,
};

use crate::state::CompositorState;

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZxdgOutputManagerV1, ()>(3, ());
}

/// Sends the current logical geometry to every xdg-output. The name is
/// sent only when an xdg-output is created, and the description is updated
/// only from version 3, where the protocol lets it change.
///
/// From version 3, `zxdg_output_v1.done` is deprecated: clients apply the
/// details on the next `wl_output.done` of the output, as wlroots sends it
/// after them. Without it, Xwayland never applied the logical size and
/// its root window stayed 0x0.
pub(crate) fn update(state: &mut CompositorState) {
  for output in state.xdg_outputs.values() {
    send_metadata(state, output, false);
  }
}

fn send_metadata(
  state: &CompositorState,
  resource: &ZxdgOutputV1,
  created: bool,
) {
  let output = state.output;
  resource.logical_position(output.x, output.y);
  resource.logical_size(output.width as i32, output.height as i32);
  if created && resource.version() >= 2 {
    resource.name(state.output_info.name.clone());
  }
  if (created && resource.version() >= 2) || resource.version() >= 3 {
    resource.description(state.output_info.description());
  }
  if resource.version() >= 3 {
    if let Some(output) = resource.data::<WlOutput>()
      && output.is_alive()
      && output.version() >= 2
    {
      output.done();
    }
  } else {
    resource.done();
  }
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
      zxdg_output_manager_v1::Request::GetXdgOutput { id, output } => {
        let output = data_init.init(id, output);
        send_metadata(state, &output, true);
        state.xdg_outputs.insert(output.id(), output);
      }
      zxdg_output_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZxdgOutputV1, WlOutput> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZxdgOutputV1,
    request: zxdg_output_v1::Request,
    _data: &WlOutput,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zxdg_output_v1::Request::Destroy = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZxdgOutputV1,
    _data: &WlOutput,
  ) {
    state.xdg_outputs.remove(&resource.id());
  }
}

#[cfg(test)]
mod tests {
  use wayland_client::protocol::wl_output::{Event, WlOutput};
  use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1,
  };

  use crate::{test_client::TestClient, tests::test_compositor};

  /// The events of a version `version` xdg-output, and the `wl_output`
  /// events after its `get_xdg_output`.
  fn details(version: u32) -> (Vec<zxdg_output_v1::Event>, Vec<Event>) {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let output = client.bind::<WlOutput>(4);
    let manager = client.bind::<ZxdgOutputManagerV1>(version);
    client.roundtrip(&mut compositor);
    client.events.output.clear();
    manager.get_xdg_output(&output, &client.handle, ());
    client.roundtrip(&mut compositor);
    (
      std::mem::take(&mut client.events.xdg_output),
      std::mem::take(&mut client.events.output),
    )
  }

  #[test]
  fn version_3_details_end_with_wl_output_done() {
    let (xdg, output) = details(3);
    assert!(
      xdg.iter().any(|event| matches!(
        event,
        zxdg_output_v1::Event::LogicalSize { .. }
      ))
    );
    assert!(
      !xdg
        .iter()
        .any(|event| matches!(event, zxdg_output_v1::Event::Done))
    );
    assert!(matches!(output.last(), Some(Event::Done)));
  }

  #[test]
  fn older_versions_end_with_xdg_output_done() {
    let (xdg, output) = details(2);
    assert!(matches!(xdg.last(), Some(zxdg_output_v1::Event::Done)));
    assert!(output.is_empty());
  }
}
