//! Output description, renaming and mode requests, through a real
//! compositor and the in-process test client.

use std::time::{Duration, Instant};

use shingle_core::{OutputId, Rect, WorkspaceId};
use wayland_client::{
  Proxy,
  protocol::wl_output::{self, WlOutput},
};
use wayland_protocols_wlr::output_management::v1::client::{
  zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
  zwlr_output_head_v1, zwlr_output_manager_v1::ZwlrOutputManagerV1,
};
use wayland_server::protocol::wl_output::Transform;

use super::*;
use crate::{
  Compositor, Config, OUTPUT_NAME,
  test_client::{ModeSeen, TestClient},
  tests::{test_compositor, test_compositor_with},
};

fn mode(width: u32, height: u32, hz: u32, preferred: bool) -> OutputModeInfo {
  OutputModeInfo {
    width,
    height,
    refresh_mhz: hz * 1000,
    preferred,
  }
}

/// A monitor on `DP-1` with three modes, the first current.
fn monitor() -> OutputInfo {
  OutputInfo {
    name: "DP-1".into(),
    make: "Dell Inc.".into(),
    model: "DELL U2720Q".into(),
    refresh_mhz: 60_000,
    modes: vec![
      mode(1920, 1080, 60, true),
      mode(2560, 1440, 144, false),
      mode(1280, 720, 60, false),
    ],
    current_mode: Some(0),
    ..OutputInfo::default()
  }
}

/// What a native backend does when it lights `info`: describe it and size
/// the output after its current mode.
fn light(compositor: &mut Compositor, info: OutputInfo) {
  let current = *info.current().expect("a current mode");
  compositor.set_output_info(info);
  let (width, height) =
    compositor.logical_output_size(current.width, current.height);
  let output = compositor.output();
  compositor
    .set_output(Rect::new(output.x, output.y, width, height))
    .expect("the mode size is valid");
}

fn mapped_window(
  compositor: &mut Compositor,
  client: &mut TestClient,
) -> shingle_core::WindowId {
  let toplevel = client.toplevel(compositor, "kept");
  client.attach_buffer(compositor, &toplevel.surface, 64, 64);
  compositor
    .focused_window()
    .expect("the toplevel is focused")
}

#[test]
fn a_new_name_replaces_the_output_and_keeps_the_tree() {
  let (mut compositor, path) = test_compositor();
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped_window(&mut compositor, &mut client);
  let old_global = client.global_name::<WlOutput>();
  let _old_output = client.bind::<WlOutput>(4);
  let _manager = client.bind::<ZwlrOutputManagerV1>(4);
  client.roundtrip(&mut compositor);
  light(&mut compositor, monitor());
  client.roundtrip(&mut compositor);

  // The old global is gone for clients and a new one is announced.
  assert_eq!(client.events.globals_removed, [old_global]);
  let new_global = client.global_name::<WlOutput>();
  assert_ne!(new_global, old_global);
  // The old head is finished and a head for DP-1 announced.
  let heads = &client.events.head_objects;
  assert_eq!(heads.len(), 2);
  assert!(heads[0].1, "the old head is finished");
  assert!(!heads[1].1);
  assert!(client.events.heads.iter().any(|event| matches!(
    event,
    zwlr_output_head_v1::Event::Name { name } if name == "DP-1"
  )));
  // The window kept its place; the core output has the connector's name.
  let dp1 = OutputId::new("DP-1");
  assert_eq!(compositor.core().outputs().collect::<Vec<_>>(), [&dp1]);
  let location = compositor.core().window_location(window).unwrap();
  assert_eq!(location.output, dp1);
  let tree = compositor.core().tree(&dp1, WorkspaceId::ORIGIN).unwrap();
  assert_eq!(tree.windows(), [window]);
  assert_eq!(compositor.output(), Rect::new(0, 0, 1920, 1080));

  // Binding the new global names the new output.
  client.events.output.clear();
  let _new_output = client.bind::<WlOutput>(4);
  client.roundtrip(&mut compositor);
  assert!(client.events.output.iter().any(|event| matches!(
    event,
    wl_output::Event::Name { name } if name == "DP-1"
  )));
  assert_eq!(
    compositor.state.outputs.bound().count(),
    1,
    "the old output is retired"
  );

  // The old global stays bindable for a while, as an inert output.
  let _late = client.bind_name::<WlOutput>(old_global, 4);
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.state.outputs.bound().count(), 1);
  assert_eq!(compositor.state.outputs.retired.len(), 1);
  let handle = compositor.display.handle();
  remove_retired(&mut compositor.state, &handle, Instant::now());
  assert_eq!(compositor.state.outputs.retired.len(), 1);
  let later = Instant::now() + RETIRED_GLOBAL_DELAY + Duration::from_secs(1);
  remove_retired(&mut compositor.state, &handle, later);
  assert!(compositor.state.outputs.retired.is_empty());
}

#[test]
fn the_new_name_brings_its_configured_placement() {
  let mut config = Config::default();
  config.output_placements.insert(
    "DP-1".into(),
    OutputPlacement {
      position: shingle_config::OutputPosition::Fixed { x: 100, y: 0 },
      width: 1,
      height: 1,
      scale: 2.0,
      transform: Transform::_90,
      refresh_mhz: 0,
    },
  );
  let (mut compositor, _path) = test_compositor_with(config);
  assert_eq!(compositor.output(), Rect::new(0, 0, 1280, 720));
  compositor.set_output_info(monitor());
  assert_eq!(compositor.output_scale(), 2.0);
  assert_eq!(compositor.output_transform(), Transform::_90);
  // 1920x1080 at scale 2, turned a quarter.
  assert_eq!(compositor.output(), Rect::new(100, 0, 540, 960));
  // A config reload keeps the size of the mode and places it again.
  let mut reloaded = Config::default();
  reloaded.output_placements.insert(
    "DP-1".into(),
    OutputPlacement {
      position: shingle_config::OutputPosition::Fixed { x: 0, y: 50 },
      width: 1,
      height: 1,
      scale: 1.0,
      transform: Transform::Normal,
      refresh_mhz: 0,
    },
  );
  compositor.apply_config(reloaded).unwrap();
  assert_eq!(compositor.output(), Rect::new(0, 50, 1920, 1080));
}

#[test]
fn a_headless_config_entry_sizes_the_headless_output() {
  let mut config = Config::default();
  config.output_placements.insert(
    OUTPUT_NAME.into(),
    OutputPlacement {
      position: shingle_config::OutputPosition::Fixed { x: 0, y: 0 },
      width: 800,
      height: 600,
      scale: 1.0,
      transform: Transform::Normal,
      refresh_mhz: 0,
    },
  );
  let (compositor, _path) = test_compositor_with(config);
  assert_eq!(compositor.output(), Rect::new(0, 0, 800, 600));
}

#[test]
fn an_explicit_configured_mode_sizes_headless_and_sets_its_refresh() {
  let mut yaml = shingle_config::ShingleConfig::default();
  yaml.output.mode = "1920x1080@75".parse().unwrap();
  let (mut compositor, _path) = test_compositor_with(Config::from(yaml));
  assert_eq!(compositor.output(), Rect::new(0, 0, 1920, 1080));
  assert_eq!(compositor.output_info().refresh_mhz, 75_000);
  let mut reloaded = shingle_config::ShingleConfig::default();
  reloaded.outputs.push(shingle_config::OutputConfig {
    mode: "800x600@50".parse().unwrap(),
    ..shingle_config::OutputConfig::named(OUTPUT_NAME)
  });
  compositor.apply_config(Config::from(reloaded)).unwrap();
  assert_eq!(compositor.output(), Rect::new(0, 0, 800, 600));
  assert_eq!(compositor.output_info().refresh_mhz, 50_000);
  // A mode is in physical pixels; the scale divides it.
  let mut scaled = shingle_config::ShingleConfig::default();
  scaled.output.mode = "1920x1080".parse().unwrap();
  scaled.output.scale = 2.0;
  compositor.apply_config(Config::from(scaled)).unwrap();
  assert_eq!(compositor.output(), Rect::new(0, 0, 960, 540));
  assert_eq!(compositor.output_info().refresh_mhz, 50_000, "kept");
}

/// A client bound to output management, with its manager.
fn manager_client(
  compositor: &mut Compositor,
  path: &std::path::Path,
) -> (TestClient, ZwlrOutputManagerV1) {
  let mut client = TestClient::connect(compositor, path);
  let manager = client.bind::<ZwlrOutputManagerV1>(4);
  client.roundtrip(compositor);
  (client, manager)
}

fn live_modes(client: &TestClient) -> Vec<ModeSeen> {
  client
    .events
    .mode_objects
    .iter()
    .filter(|(_, seen)| !seen.finished)
    .map(|(_, seen)| seen.clone())
    .collect()
}

#[test]
fn heads_list_the_backends_modes() {
  let (mut compositor, path) = test_compositor();
  light(&mut compositor, monitor());
  let (mut client, _manager) = manager_client(&mut compositor, &path);
  let modes = live_modes(&client);
  let sizes: Vec<_> =
    modes.iter().map(|mode| (mode.size, mode.refresh)).collect();
  assert_eq!(
    sizes,
    [
      ((1920, 1080), 60_000),
      ((2560, 1440), 144_000),
      ((1280, 720), 60_000)
    ]
  );
  assert_eq!(
    modes.iter().map(|mode| mode.preferred).collect::<Vec<_>>(),
    [true, false, false]
  );
  let first = client.events.mode_objects[0].0.id();
  assert_eq!(client.events.current_modes.last(), Some(&first));

  // A new list replaces the mode objects.
  let mut info = monitor();
  info.modes.truncate(1);
  compositor.set_output_info(info);
  client.roundtrip(&mut compositor);
  assert!(
    client.events.mode_objects[..3]
      .iter()
      .all(|(_, s)| s.finished)
  );
  assert_eq!(live_modes(&client).len(), 1);
}

#[test]
fn without_a_mode_list_the_head_has_the_rectangle_as_its_mode() {
  let (mut compositor, path) = test_compositor();
  let (client, _manager) = manager_client(&mut compositor, &path);
  assert_eq!(
    live_modes(&client),
    [ModeSeen {
      size: (1280, 720),
      refresh: 60_000,
      preferred: true,
      finished: false,
    }]
  );
}

/// Sends a configuration of the one head: `configure` sets its properties,
/// then it is applied or, if `test`, tested.
fn configure(
  client: &mut TestClient,
  compositor: &mut Compositor,
  manager: &ZwlrOutputManagerV1,
  test: bool,
  configure: impl FnOnce(&ZwlrOutputConfigurationHeadV1, &TestClient),
) {
  let head = client
    .events
    .head_objects
    .iter()
    .rev()
    .find(|(_, finished)| !finished)
    .map(|(head, _)| head.clone())
    .expect("a head");
  let configuration = manager.create_configuration(
    client.events.output_serial,
    &client.handle,
    (),
  );
  let configuration_head = configuration.enable_head(&head, &client.handle, ());
  configure(&configuration_head, client);
  if test {
    configuration.test();
  } else {
    configuration.apply();
  }
  client.roundtrip(compositor);
}

#[test]
fn mode_changes_wait_for_the_backend() {
  let (mut compositor, path) = test_compositor();
  light(&mut compositor, monitor());
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let fast = client.events.mode_objects[1].0.clone();

  // A test goes to the backend, which refuses it.
  configure(&mut client, &mut compositor, &manager, true, |head, _| {
    head.set_mode(&fast);
  });
  let requests = compositor.take_output_requests();
  assert_eq!(
    requests,
    [OutputRequest {
      id: requests[0].id,
      mode: RequestedMode::Listed(1),
      test_only: true,
    }]
  );
  assert!(client.events.configuration_results.is_empty());
  compositor.finish_output_request(requests[0].id, false);
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["failed"]);

  // An applied change is answered after the backend made it.
  configure(&mut client, &mut compositor, &manager, false, |head, _| {
    head.set_mode(&fast);
    head.set_position(10, 20);
  });
  let requests = compositor.take_output_requests();
  assert_eq!(requests[0].mode, RequestedMode::Listed(1));
  assert!(!requests[0].test_only);
  light(
    &mut compositor,
    OutputInfo {
      current_mode: Some(1),
      refresh_mhz: 144_000,
      ..monitor()
    },
  );
  compositor.finish_output_request(requests[0].id, true);
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["failed", "succeeded"]);
  assert_eq!(compositor.output(), Rect::new(10, 20, 2560, 1440));
  let fast_id = fast.id();
  assert_eq!(client.events.current_modes.last(), Some(&fast_id));

  // A custom mode is the backend's to match; the current one is not.
  configure(&mut client, &mut compositor, &manager, false, |head, _| {
    head.set_custom_mode(1280, 720, 0);
  });
  let requests = compositor.take_output_requests();
  assert_eq!(
    requests[0].mode,
    RequestedMode::Custom {
      width: 1280,
      height: 720,
      refresh_mhz: 0
    }
  );
  compositor.finish_output_request(requests[0].id, false);
  client.roundtrip(&mut compositor);
  configure(&mut client, &mut compositor, &manager, false, |head, _| {
    head.set_custom_mode(2560, 1440, 144_000);
    head.set_scale(2.0);
  });
  assert!(compositor.take_output_requests().is_empty());
  assert_eq!(
    client.events.configuration_results.last(),
    Some(&"succeeded")
  );
  assert_eq!(compositor.output(), Rect::new(10, 20, 1280, 720));
}

#[test]
fn without_a_mode_list_a_custom_mode_resizes_the_output() {
  let (mut compositor, path) = test_compositor();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  configure(&mut client, &mut compositor, &manager, true, |head, _| {
    head.set_custom_mode(1920, 1080, 75_000);
  });
  assert_eq!(client.events.configuration_results, ["succeeded"]);
  assert_eq!(compositor.output(), Rect::new(0, 0, 1280, 720));
  configure(&mut client, &mut compositor, &manager, false, |head, _| {
    head.set_custom_mode(1920, 1080, 75_000);
  });
  assert!(compositor.take_output_requests().is_empty());
  assert_eq!(
    client.events.configuration_results.last(),
    Some(&"succeeded")
  );
  assert_eq!(compositor.output(), Rect::new(0, 0, 1920, 1080));
  assert_eq!(compositor.output_info().refresh_mhz, 75_000);
  assert_eq!(
    live_modes(&client)[0],
    ModeSeen {
      size: (1920, 1080),
      refresh: 75_000,
      preferred: true,
      finished: false,
    }
  );
}

#[test]
fn turning_the_only_output_off_fails() {
  let (mut compositor, path) = test_compositor();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let head = client.events.head_objects[0].0.clone();
  let configuration = manager.create_configuration(
    client.events.output_serial,
    &client.handle,
    (),
  );
  configuration.disable_head(&head);
  configuration.apply();
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.configuration_results, ["failed"]);
  assert!(compositor.take_output_requests().is_empty());
}

#[test]
fn a_session_saved_under_a_connector_name_joins_the_one_output() {
  let mut saved = shingle_core::Shingle::new();
  let dp1 = OutputId::new("DP-1");
  saved
    .add_output(dp1.clone(), Rect::new(0, 0, 2560, 1440))
    .unwrap();
  let window = saved
    .attach_window(&dp1, shingle_core::LaunchIdentity::new("saved"))
    .unwrap();
  let path = std::env::temp_dir()
    .join(format!("shingle-test-restore-{}", std::process::id()));
  let socket = wayland_server::ListeningSocket::bind_absolute(path.clone())
    .expect("the temporary directory accepts a test socket");
  let mut compositor = Compositor::with_socket(
    Config::default(),
    saved,
    socket,
    path.file_name().unwrap().to_owned(),
  )
  .unwrap();
  let headless = OutputId::new(OUTPUT_NAME);
  assert_eq!(compositor.core().outputs().collect::<Vec<_>>(), [&headless]);
  // The saved set waits, parked, until the backend has added its outputs.
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(compositor.core().wset(info.wset).unwrap().output, None);
  compositor.restore_outputs();
  // Under the default `return`, the window joins the one output and
  // remembers its set.
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, headless);
  let parked = info.away_from.expect("the window is away from its set");
  assert_eq!(
    compositor.core().wset(parked).unwrap().home,
    Some(dp1.clone())
  );
  assert_eq!(
    compositor.core().output_workarea(&headless).unwrap(),
    Rect::new(0, 0, 1280, 720)
  );
  // When the output is DP-1 after all, the window goes back to its set.
  compositor.set_output_info(monitor());
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, dp1);
  assert_eq!(info.wset, parked);
  assert_eq!(info.away_from, None);
  assert_eq!(compositor.core().wsets().count(), 1);
  let _ = std::fs::remove_file(path);
}
