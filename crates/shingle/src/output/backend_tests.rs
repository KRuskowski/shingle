//! Outputs of a backend that lists modes, as native lights them: heads
//! turned on and off and mode changes across heads as one request,
//! connected monitors listed as disabled heads, and the dark last output
//! lit again (docs/design/multi-output.md, MO-7).

use std::collections::BTreeMap;

use shingle_core::{OutputId, Rect};
use wayland_client::Proxy;
use wayland_protocols_wlr::output_management::v1::client::{
  zwlr_output_manager_v1::ZwlrOutputManagerV1,
};

use super::{
  HeadRequest, HeadState, OutputHandle, OutputInfo, OutputModeInfo,
  RequestedMode,
};
use crate::{
  Compositor, Config, OutputRequest, test_client::TestClient,
  tests::test_compositor_with,
};

fn mode(width: u32, height: u32, preferred: bool) -> OutputModeInfo {
  OutputModeInfo {
    width,
    height,
    refresh_mhz: 60_000,
    preferred,
  }
}

/// A monitor on connector `name` with a 1920x1080 and a 1280x720 mode,
/// the first current unless `current` is `None`.
fn monitor(name: &str, current: Option<usize>) -> OutputInfo {
  OutputInfo {
    name: name.into(),
    make: "Test".into(),
    model: name.into(),
    refresh_mhz: 60_000,
    modes: vec![mode(1920, 1080, true), mode(1280, 720, false)],
    current_mode: current,
    ..OutputInfo::default()
  }
}

/// A compositor lit as native lights two monitors: `HEADLESS-1` renamed
/// to DP-1, and DP-2 added beside it, both at 1920x1080.
fn two_monitors() -> (Compositor, std::path::PathBuf) {
  let (mut compositor, path) = test_compositor_with(Config::default());
  compositor.set_output_info(monitor("DP-1", Some(0)));
  let first = compositor.primary_output();
  compositor.set_output_size(&first, (1920, 1080)).unwrap();
  compositor
    .add_output(monitor("DP-2", Some(0)), (1920, 1080))
    .expect("a second monitor");
  (compositor, path)
}

fn manager_client(
  compositor: &mut Compositor,
  path: &std::path::Path,
) -> (TestClient, ZwlrOutputManagerV1) {
  let mut client = TestClient::connect(compositor, path);
  let manager = client.bind::<ZwlrOutputManagerV1>(4);
  client.roundtrip(compositor);
  (client, manager)
}

/// The live heads, by name, with whether each is enabled.
fn heads(client: &TestClient) -> BTreeMap<String, bool> {
  client
    .events
    .head_objects
    .iter()
    .filter(|(_, finished)| !finished)
    .map(|(head, _)| {
      (
        client.events.head_names[&head.id()].clone(),
        client.events.head_enabled[&head.id()],
      )
    })
    .collect()
}

/// What a configuration asks of one head.
#[derive(Clone, Copy)]
enum Ask {
  Off,
  On,
  Custom(i32, i32),
  At(i32, i32),
}

/// Sends a configuration of every live head, each as `asks` says (the
/// others enabled unchanged), and tests or applies it.
fn configure(
  compositor: &mut Compositor,
  client: &mut TestClient,
  manager: &ZwlrOutputManagerV1,
  asks: &[(&str, Ask)],
  test: bool,
) {
  let configuration = manager.create_configuration(
    client.events.output_serial,
    &client.handle,
    (),
  );
  let live = client
    .events
    .head_objects
    .iter()
    .filter(|(_, finished)| !finished)
    .map(|(head, _)| head.clone())
    .collect::<Vec<_>>();
  for head in live {
    let name = client.events.head_names[&head.id()].clone();
    let ask = asks
      .iter()
      .find(|(asked, _)| *asked == name)
      .map_or(Ask::On, |(_, ask)| *ask);
    if let Ask::Off = ask {
      configuration.disable_head(&head);
      continue;
    }
    let configured = configuration.enable_head(&head, &client.handle, ());
    match ask {
      Ask::Custom(width, height) => {
        configured.set_custom_mode(width, height, 0)
      }
      Ask::At(x, y) => configured.set_position(x, y),
      Ask::Off | Ask::On => {}
    }
  }
  if test {
    configuration.test();
  } else {
    configuration.apply();
  }
  client.events.configuration_results.clear();
  client.roundtrip(compositor);
}

fn names(compositor: &Compositor) -> Vec<String> {
  compositor
    .outputs()
    .into_iter()
    .map(|output| output.handle.name().to_owned())
    .collect()
}

fn head(name: &str, state: HeadState) -> HeadRequest {
  HeadRequest {
    output: OutputHandle::new(name),
    state,
  }
}

#[test]
fn mode_changes_of_two_heads_are_one_request() {
  let (mut compositor, path) = two_monitors();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let custom = Ask::Custom(1280, 720);
  configure(
    &mut compositor,
    &mut client,
    &manager,
    &[("DP-1", custom), ("DP-2", custom)],
    true,
  );
  let requests = compositor.take_output_requests();
  let small = RequestedMode::Custom {
    width: 1280,
    height: 720,
    refresh_mhz: 0,
  };
  assert_eq!(
    requests,
    [OutputRequest {
      id: requests[0].id,
      heads: vec![
        head("DP-1", HeadState::On(Some(small))),
        head("DP-2", HeadState::On(Some(small))),
      ],
      test_only: true,
    }]
  );
  assert!(client.events.configuration_results.is_empty(), "waiting");
  compositor.finish_output_request(requests[0].id, true);
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["succeeded"]);
  let _ = std::fs::remove_file(path);
}

#[test]
fn turning_a_monitor_off_goes_to_the_backend_and_its_head_stays() {
  let (mut compositor, path) = two_monitors();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  configure(
    &mut compositor,
    &mut client,
    &manager,
    &[("DP-2", Ask::Off), ("DP-1", Ask::At(100, 0))],
    false,
  );
  let requests = compositor.take_output_requests();
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].heads, [head("DP-2", HeadState::Off)]);
  assert!(!requests[0].test_only);
  assert_eq!(
    names(&compositor),
    ["DP-1", "DP-2"],
    "not before the backend"
  );
  // The backend turned the CRTC off, then tells the compositor.
  compositor
    .disable_output(&OutputHandle::new("DP-2"))
    .unwrap();
  compositor.finish_output_request(requests[0].id, true);
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["succeeded"]);
  assert_eq!(names(&compositor), ["DP-1"]);
  assert_eq!(
    heads(&client),
    BTreeMap::from([("DP-1".into(), true), ("DP-2".into(), false)])
  );
  assert_eq!(compositor.disabled_outputs(), [OutputHandle::new("DP-2")]);
  assert_eq!(compositor.output().x, 100, "the rest of it applied");
  let _ = std::fs::remove_file(path);
}

#[test]
fn turning_a_disabled_monitor_on_goes_to_the_backend() {
  let (mut compositor, path) = two_monitors();
  compositor
    .disable_output(&OutputHandle::new("DP-2"))
    .unwrap();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  assert!(!heads(&client)["DP-2"]);
  configure(
    &mut compositor,
    &mut client,
    &manager,
    &[("DP-2", Ask::On)],
    false,
  );
  let requests = compositor.take_output_requests();
  assert_eq!(requests[0].heads, [head("DP-2", HeadState::On(None))]);
  // The backend lit it and adds it; it takes its old place.
  compositor
    .add_output(monitor("DP-2", Some(0)), (1920, 1080))
    .unwrap();
  compositor.finish_output_request(requests[0].id, true);
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["succeeded"]);
  assert_eq!(names(&compositor), ["DP-1", "DP-2"]);
  assert!(heads(&client)["DP-2"]);
  assert!(compositor.disabled_outputs().is_empty());
  // A refusal by the backend fails the configuration and changes nothing.
  configure(
    &mut compositor,
    &mut client,
    &manager,
    &[("DP-1", Ask::Custom(1280, 720))],
    false,
  );
  let requests = compositor.take_output_requests();
  compositor.finish_output_request(requests[0].id, false);
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["failed"]);
  let _ = std::fs::remove_file(path);
}

#[test]
fn turning_every_monitor_off_fails_without_asking_the_backend() {
  let (mut compositor, path) = two_monitors();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  configure(
    &mut compositor,
    &mut client,
    &manager,
    &[("DP-1", Ask::Off), ("DP-2", Ask::Off)],
    false,
  );
  assert_eq!(client.events.configuration_results, ["failed"]);
  assert!(compositor.take_output_requests().is_empty());
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_connected_monitor_that_is_not_lit_is_a_disabled_head() {
  let (mut compositor, path) = two_monitors();
  compositor
    .add_disabled_output(monitor("HDMI-A-1", None), (1920, 1080))
    .unwrap();
  // Listing it twice, or an output that is there, changes nothing.
  compositor
    .add_disabled_output(monitor("HDMI-A-1", None), (1920, 1080))
    .unwrap();
  compositor
    .add_disabled_output(monitor("DP-1", None), (1920, 1080))
    .unwrap();
  let (mut client, _manager) = manager_client(&mut compositor, &path);
  assert_eq!(
    heads(&client),
    BTreeMap::from([
      ("DP-1".into(), true),
      ("DP-2".into(), true),
      ("HDMI-A-1".into(), false),
    ])
  );
  assert_eq!(names(&compositor), ["DP-1", "DP-2"]);
  // Unplugged, its head goes.
  compositor
    .remove_output(&OutputHandle::new("HDMI-A-1"))
    .unwrap();
  client.roundtrip(&mut compositor);
  assert!(!heads(&client).contains_key("HDMI-A-1"));
  assert!(compositor.disabled_outputs().is_empty());
  let _ = std::fs::remove_file(path);
}

#[test]
fn the_dark_last_output_is_lit_again_with_its_windows() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  compositor.set_output_info(monitor("DP-1", Some(0)));
  let mut client = TestClient::connect(&mut compositor, &path);
  let toplevel = client.toplevel(&mut compositor, "kept");
  client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
  client.roundtrip(&mut compositor);
  let window = compositor.focused_window().expect("the window maps");
  let dp1 = OutputHandle::new("DP-1");
  let ipc_id = compositor.output_view(&dp1).unwrap().ipc_id;
  compositor.remove_output(&dp1).unwrap();
  assert!(compositor.output_view(&dp1).unwrap().dark);
  let mut replugged = monitor("DP-1", Some(1));
  replugged.serial = Some("new".into());
  compositor
    .add_output(replugged.clone(), (1280, 720))
    .unwrap();
  let view = compositor.output_view(&dp1).unwrap();
  assert!(!view.dark);
  assert_eq!(view.ipc_id, ipc_id, "the same output");
  assert_eq!(view.info, replugged);
  assert_eq!(view.rect, Rect::new(0, 0, 1280, 720));
  let location = compositor.core().window_location(window).unwrap();
  assert_eq!(location.output, OutputId::new("DP-1"));
  // Disabling the last output keeps it, dark, and lists no head.
  compositor.disable_output(&dp1).unwrap();
  assert!(compositor.output_view(&dp1).unwrap().dark);
  assert!(compositor.disabled_outputs().is_empty());
  let _ = std::fs::remove_file(path);
}
