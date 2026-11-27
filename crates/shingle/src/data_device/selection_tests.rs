//! Selections set through `wl_data_device` by the in-process test client.

use wayland_client::protocol::{
  wl_data_device_manager::WlDataDeviceManager, wl_seat::WlSeat,
};

use crate::{test_client::TestClient, tests::test_compositor};

#[test]
fn a_source_set_again_as_the_selection_is_no_error() {
  let (mut compositor, path) = test_compositor();
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = client.toplevel(&mut compositor, "copier");
  client.attach_buffer(&mut compositor, &window.surface, 100, 100);
  let seat = client.bind::<WlSeat>(5);
  let manager = client.bind::<WlDataDeviceManager>(3);
  let device = manager.get_data_device(&seat, &client.handle, ());
  let source = manager.create_data_source(&client.handle, ());
  source.offer("text/plain".into());
  device.set_selection(Some(&source), 1);
  // What GTK does on every copy, Firefox's among them.
  source.offer("SAVE_TARGETS".into());
  device.set_selection(Some(&source), 1);
  assert!(client.protocol_error(&mut compositor).is_none());
  let (_, mime_types) =
    super::selection(&compositor.state).expect("the source is the selection");
  assert!(mime_types.contains(&"text/plain".to_owned()));
  let _ = std::fs::remove_file(path);
}
