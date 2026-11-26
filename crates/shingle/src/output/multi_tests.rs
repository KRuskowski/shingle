//! Several outputs in one compositor: globals and xdg-outputs, the layout,
//! unplug policies, restored sessions, frames and IPC
//! (docs/design/multi-output.md, MO-2).

use serde_json::{Value, json};
use shingle_config::{OutputPosition, UnplugPolicy};
use shingle_core::{LaunchIdentity, OutputId, Rect, WindowId, WsetId};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_protocols::xdg::xdg_output::zv1::client::{
  zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1,
};
use wayland_protocols_wlr::output_management::v1::client::{
  zwlr_output_head_v1, zwlr_output_manager_v1::ZwlrOutputManagerV1,
};

use super::{OutputHandle, OutputInfo, OutputPlacement};
use crate::{
  Compositor, Config, Event,
  test_client::TestClient,
  tests::{test_compositor, test_compositor_with},
};

fn headless(name: &str) -> OutputInfo {
  OutputInfo {
    name: name.into(),
    ..OutputInfo::default()
  }
}

fn id(name: &str) -> OutputId {
  OutputId::new(name)
}

/// A compositor with HEADLESS-1 (1280x720) and HEADLESS-2 (800x600).
fn two_outputs(config: Config) -> (Compositor, std::path::PathBuf) {
  let (mut compositor, path) = test_compositor_with(config);
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .expect("a second output");
  (compositor, path)
}

fn rects(compositor: &Compositor) -> Vec<(String, Rect)> {
  compositor
    .outputs()
    .into_iter()
    .map(|output| (output.handle.name().to_owned(), output.rect))
    .collect()
}

/// A saved window, as a restored session has it, on `output`.
fn placeholder(compositor: &mut Compositor, output: &str) -> WindowId {
  compositor
    .state
    .core
    .attach_window(&id(output), LaunchIdentity::new("saved"))
    .expect("the output shows a set")
}

fn ipc(compositor: &mut Compositor, method: &str, data: Value) -> Value {
  crate::ipc::respond(
    &mut compositor.state,
    &json!({"method": method, "data": data}),
  )
}

fn config_with(unplug: UnplugPolicy) -> Config {
  Config {
    output_unplug: unplug,
    ..Config::default()
  }
}

/// An output's name, `wl_output` position and xdg-output rectangle.
type Seen = (String, (i32, i32), (i32, i32, i32, i32));

/// What a client sees of each `wl_output` global it binds, one at a time:
/// the name, the geometry's position and the xdg-output logical position
/// and size.
fn seen(compositor: &mut Compositor, client: &mut TestClient) -> Vec<Seen> {
  let manager = client.bind::<ZxdgOutputManagerV1>(3);
  client.roundtrip(compositor);
  let globals = client
    .events
    .globals
    .iter()
    .filter(|(_, interface, _)| interface == "wl_output")
    .map(|(name, ..)| *name)
    .filter(|name| !client.events.globals_removed.contains(name))
    .collect::<Vec<_>>();
  let mut seen = Vec::new();
  for global in globals {
    client.events.output.clear();
    client.events.xdg_output.clear();
    let output = client.bind_name::<WlOutput>(global, 4);
    manager.get_xdg_output(&output, &client.handle, ());
    client.roundtrip(compositor);
    let mut name = String::new();
    let mut position = (0, 0);
    for event in &client.events.output {
      match event {
        wl_output::Event::Name { name: value } => name.clone_from(value),
        wl_output::Event::Geometry { x, y, .. } => position = (*x, *y),
        _ => {}
      }
    }
    let mut logical = (0, 0, 0, 0);
    for event in &client.events.xdg_output {
      match event {
        zxdg_output_v1::Event::LogicalPosition { x, y } => {
          (logical.0, logical.1) = (*x, *y);
        }
        zxdg_output_v1::Event::LogicalSize { width, height } => {
          (logical.2, logical.3) = (*width, *height);
        }
        _ => {}
      }
    }
    seen.push((name, position, logical));
  }
  seen
}

#[test]
fn each_output_has_a_global_with_its_name_and_auto_position() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  client.roundtrip(&mut compositor);
  assert_eq!(
    seen(&mut compositor, &mut client),
    [
      ("HEADLESS-1".into(), (0, 0), (0, 0, 1280, 720)),
      ("HEADLESS-2".into(), (1280, 0), (1280, 0, 800, 600)),
    ]
  );
  let views = compositor.outputs();
  assert_eq!(
    views.iter().map(|view| view.ipc_id).collect::<Vec<_>>(),
    [1, 2]
  );
  assert_eq!(views[1].wset, Some(WsetId::new(2)));
  assert_eq!(
    compositor.output_at(1300.0, 10.0),
    Some(views[1].handle.clone())
  );
  assert_eq!(compositor.output_at(-1.0, 10.0), None);
  assert_eq!(compositor.active_output().name(), "HEADLESS-1");
  assert_eq!(compositor.primary_output().name(), "HEADLESS-1");
}

#[test]
fn an_output_added_later_is_announced_to_bound_clients() {
  let (mut compositor, path) = test_compositor();
  let mut client = TestClient::connect(&mut compositor, &path);
  let _manager = client.bind::<ZwlrOutputManagerV1>(4);
  client.roundtrip(&mut compositor);
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .unwrap();
  client.roundtrip(&mut compositor);
  let outputs = client
    .events
    .globals
    .iter()
    .filter(|(_, interface, _)| interface == "wl_output")
    .count();
  assert_eq!(outputs, 2);
  let heads = client
    .events
    .heads
    .iter()
    .filter_map(|event| match event {
      zwlr_output_head_v1::Event::Name { name } => Some(name.clone()),
      _ => None,
    })
    .collect::<Vec<_>>();
  assert_eq!(heads, ["HEADLESS-1", "HEADLESS-2"]);
  assert!(matches!(
    compositor.drain_events().as_slice(),
    [.., Event::OutputAdded { output }] if output.name() == "HEADLESS-2"
  ));
  // Removing it finishes its head and removes its global.
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  // The global is disabled at once and destroyed a few seconds later.
  assert_eq!(client.events.globals_removed.len(), 1);
  let finished = client
    .events
    .head_objects
    .iter()
    .filter(|(_, finished)| *finished)
    .count();
  assert_eq!(finished, 1);
}

#[test]
fn fixed_outputs_come_first_and_auto_ones_follow_them() {
  let mut config = Config::default();
  config.output_placements.insert(
    "HEADLESS-2".into(),
    OutputPlacement {
      position: OutputPosition::Fixed { x: -800, y: 100 },
      width: 800,
      height: 600,
      scale: 1.0,
      transform: wayland_server::protocol::wl_output::Transform::Normal,
      refresh_mhz: 0,
    },
  );
  let (mut compositor, _path) = two_outputs(config);
  // HEADLESS-1 is automatic and starts at the right edge of the fixed one.
  assert_eq!(
    rects(&compositor),
    [
      ("HEADLESS-1".into(), Rect::new(0, 100, 1280, 720)),
      ("HEADLESS-2".into(), Rect::new(-800, 100, 800, 600)),
    ]
  );
  compositor
    .add_output(headless("HEADLESS-3"), (640, 480))
    .unwrap();
  assert_eq!(rects(&compositor)[2].1, Rect::new(1280, 100, 640, 480));
  // A wider first output pushes the automatic outputs after it along.
  compositor
    .set_output_size(&OutputHandle::new("HEADLESS-1"), (1920, 1080))
    .unwrap();
  assert_eq!(rects(&compositor)[2].1, Rect::new(1920, 100, 640, 480));
  // Removing it moves the third one into its place.
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-1"))
    .unwrap();
  assert_eq!(
    rects(&compositor),
    [
      ("HEADLESS-2".into(), Rect::new(-800, 100, 800, 600)),
      ("HEADLESS-3".into(), Rect::new(0, 100, 640, 480)),
    ]
  );
}

#[test]
fn names_are_unique_among_outputs() {
  let (mut compositor, _path) = two_outputs(Config::default());
  assert!(matches!(
    compositor.add_output(headless("HEADLESS-2"), (10, 10)),
    Err(crate::Error::OutputExists(name)) if name == "HEADLESS-2"
  ));
  assert!(compositor.add_output(headless(""), (10, 10)).is_err());
  assert_eq!(compositor.outputs().len(), 2);
}

#[test]
fn move_takes_the_windows_to_the_active_output_for_good() {
  let (mut compositor, _path) = two_outputs(config_with(UnplugPolicy::Move));
  let window = placeholder(&mut compositor, "HEADLESS-2");
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("HEADLESS-1"));
  assert_eq!(info.away_from, None);
  assert_eq!(compositor.core().wsets().count(), 1);
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .unwrap();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("HEADLESS-1"), "nothing returns");
}

#[test]
fn park_hides_the_set_until_its_output_returns() {
  let (mut compositor, _path) = two_outputs(config_with(UnplugPolicy::Park));
  let window = placeholder(&mut compositor, "HEADLESS-2");
  let wset = compositor.core().wset_of(&id("HEADLESS-2")).unwrap();
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.wset, wset);
  assert_eq!(compositor.core().wset(wset).unwrap().output, None);
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .unwrap();
  assert_eq!(compositor.core().wset_of(&id("HEADLESS-2")), Some(wset));
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("HEADLESS-2"));
  // It got a new IPC id, as a replugged Wayfire output does.
  assert_eq!(compositor.outputs()[1].ipc_id, 3);
}

#[test]
fn return_moves_the_windows_and_brings_them_back() {
  let (mut compositor, _path) = two_outputs(Config::default());
  let window = placeholder(&mut compositor, "HEADLESS-2");
  let wset = compositor.core().wset_of(&id("HEADLESS-2")).unwrap();
  let empty = placeholder(&mut compositor, "HEADLESS-1");
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("HEADLESS-1"));
  assert_eq!(info.away_from, Some(wset));
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .unwrap();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("HEADLESS-2"));
  assert_eq!((info.wset, info.away_from), (wset, None));
  let other = compositor.core().window_info(empty).unwrap();
  assert_eq!(other.location.output, id("HEADLESS-1"));
}

#[test]
fn an_empty_set_is_not_kept_for_an_output_that_goes() {
  let (mut compositor, _path) = two_outputs(Config::default());
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  assert_eq!(compositor.core().wsets().count(), 1);
}

#[test]
fn the_active_output_passes_to_the_next_one() {
  let (mut compositor, _path) = two_outputs(Config::default());
  compositor
    .add_output(headless("HEADLESS-3"), (800, 600))
    .unwrap();
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-1"))
    .unwrap();
  assert_eq!(compositor.active_output().name(), "HEADLESS-2");
  assert_eq!(compositor.primary_output().name(), "HEADLESS-2");
  assert_eq!(compositor.core().active_output(), Some(&id("HEADLESS-2")));
}

#[test]
fn the_last_output_goes_dark_and_a_new_one_takes_its_windows() {
  let (mut compositor, _path) = test_compositor();
  let window = placeholder(&mut compositor, "HEADLESS-1");
  let handle = OutputHandle::new("HEADLESS-1");
  compositor.remove_output(&handle).unwrap();
  let view = compositor.output_view(&handle).expect("it stays");
  assert!(view.dark);
  assert!(compositor.visible_surfaces_on(&handle).is_empty());
  compositor
    .add_output(
      OutputInfo {
        name: "DP-1".into(),
        make: "Dell Inc.".into(),
        ..OutputInfo::default()
      },
      (1920, 1080),
    )
    .unwrap();
  assert!(compositor.output_view(&handle).is_none());
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("DP-1"));
  assert_eq!(compositor.active_output().name(), "DP-1");
  assert_eq!(compositor.output(), Rect::new(0, 0, 1920, 1080));
}

#[test]
fn a_returning_monitor_is_found_by_its_identity_on_another_connector() {
  let (mut compositor, _path) = test_compositor();
  let monitor = |name: &str| OutputInfo {
    name: name.into(),
    make: "Dell Inc.".into(),
    model: "U2720Q".into(),
    serial: Some("ABC".into()),
    ..OutputInfo::default()
  };
  compositor
    .add_output(monitor("DP-1"), (1920, 1080))
    .unwrap();
  let window = placeholder(&mut compositor, "DP-1");
  compositor
    .remove_output(&OutputHandle::new("DP-1"))
    .unwrap();
  compositor
    .add_output(monitor("DP-2"), (1920, 1080))
    .unwrap();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.output, id("DP-2"));
}

#[test]
fn a_restored_session_waits_for_its_outputs() {
  let mut saved = shingle_core::Shingle::new();
  for name in ["HEADLESS-1", "HEADLESS-2", "DP-9"] {
    saved
      .add_output(id(name), Rect::new(0, 0, 640, 480))
      .unwrap();
  }
  let windows = ["HEADLESS-1", "HEADLESS-2", "DP-9"].map(|name| {
    saved
      .attach_window(&id(name), LaunchIdentity::new(name))
      .unwrap()
  });
  saved.set_active_output(Some(id("HEADLESS-2")));
  let path = std::env::temp_dir()
    .join(format!("shingle-test-multi-restore-{}", std::process::id()));
  let socket = wayland_server::ListeningSocket::bind_absolute(path.clone())
    .expect("the temporary directory accepts a test socket");
  let mut compositor = Compositor::with_socket(
    Config::default(),
    saved,
    socket,
    path.file_name().unwrap().to_owned(),
  )
  .unwrap();
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .unwrap();
  compositor.restore_outputs();
  let output = |window| {
    compositor
      .core()
      .window_info(window)
      .unwrap()
      .location
      .output
  };
  assert_eq!(output(windows[0]), id("HEADLESS-1"));
  assert_eq!(output(windows[1]), id("HEADLESS-2"));
  // DP-9 is not there: under `return` its window joins the active output,
  // which the session saved, and its set waits for DP-9.
  assert_eq!(compositor.active_output().name(), "HEADLESS-2");
  assert_eq!(output(windows[2]), id("HEADLESS-2"));
  let info = compositor.core().window_info(windows[2]).unwrap();
  let parked = info.away_from.unwrap();
  assert_eq!(
    compositor.core().wset(parked).unwrap().home,
    Some(id("DP-9"))
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn frames_release_the_callbacks_of_their_own_output() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let toplevel = client.toplevel(&mut compositor, "away");
  client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
  let window = compositor.focused_window().unwrap();
  let target = compositor.core().wset_of(&id("HEADLESS-2")).unwrap();
  compositor
    .state
    .core
    .move_window_to_wset(window, target, shingle_core::WorkspaceId::ORIGIN)
    .unwrap();
  toplevel
    .surface
    .frame(&client.handle, crate::test_client::Frame);
  toplevel.surface.commit();
  client.roundtrip(&mut compositor);
  client.events.frame_done.clear();
  compositor.frame_now(&OutputHandle::new("HEADLESS-1"));
  client.roundtrip(&mut compositor);
  assert!(client.events.frame_done.is_empty(), "not on HEADLESS-1");
  let on_second = compositor
    .visible_surfaces_on(&OutputHandle::new("HEADLESS-2"))
    .iter()
    .any(|surface| surface.window == Some(window));
  assert!(on_second);
  compositor.frame_now(&OutputHandle::new("HEADLESS-2"));
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.frame_done.len(), 1);
}

#[test]
fn ipc_reports_every_output_and_set_with_real_ids() {
  let (mut compositor, _path) = two_outputs(Config::default());
  let outputs = ipc(&mut compositor, "window-rules/list-outputs", json!({}));
  assert_eq!(outputs[1]["id"], json!(2));
  assert_eq!(outputs[1]["name"], json!("HEADLESS-2"));
  assert_eq!(
    outputs[1]["geometry"],
    json!({"x": 1280, "y": 0, "width": 800, "height": 600})
  );
  assert_eq!(outputs[1]["wset-index"], json!(2));
  let info = ipc(
    &mut compositor,
    "window-rules/output-info",
    json!({"id": 2}),
  );
  assert_eq!(info, outputs[1]);
  let missing = ipc(
    &mut compositor,
    "window-rules/output-info",
    json!({"id": 9}),
  );
  assert_eq!(missing, json!({"error": "output not found"}));
  let focused = ipc(
    &mut compositor,
    "window-rules/get-focused-output",
    json!({}),
  );
  assert_eq!(focused["info"]["id"], json!(1));
  // A parked set has no output.
  let window = placeholder(&mut compositor, "HEADLESS-2");
  compositor.state.unplug = UnplugPolicy::Park;
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  let wsets = ipc(&mut compositor, "window-rules/list-wsets", json!({}));
  assert_eq!(wsets[1]["index"], json!(2));
  assert_eq!(wsets[1]["output-id"], json!(-1));
  assert_eq!(wsets[1]["output-name"], json!(""));
  let wset = ipc(&mut compositor, "window-rules/wset-info", json!({"id": 2}));
  assert_eq!(wset, wsets[1]);
  let missing =
    ipc(&mut compositor, "window-rules/wset-info", json!({"id": 7}));
  assert_eq!(missing, json!({"error": "workspace set not found"}));
  let view = crate::ipc::view_json(&compositor.state, window);
  assert_eq!(view["output-id"], json!(-1));
  assert_eq!(view["output-name"], json!("null"));
  assert_eq!(view["wset-index"], json!(2));
}

#[test]
fn ipc_creates_and_destroys_headless_outputs() {
  let (mut compositor, path) = test_compositor();
  let mut client = TestClient::connect(&mut compositor, &path);
  client.roundtrip(&mut compositor);
  let reply = ipc(
    &mut compositor,
    "wayfire/create-headless-output",
    json!({"width": 640, "height": 480}),
  );
  assert_eq!(reply["result"], json!("ok"));
  assert_eq!(reply["output"]["name"], json!("HEADLESS-2"));
  assert_eq!(reply["output"]["id"], json!(2));
  assert_eq!(
    reply["output"]["geometry"],
    json!({"x": 1280, "y": 0, "width": 640, "height": 480})
  );
  client.roundtrip(&mut compositor);
  let names = seen(&mut compositor, &mut client)
    .into_iter()
    .map(|(name, ..)| name)
    .collect::<Vec<_>>();
  assert_eq!(names, ["HEADLESS-1", "HEADLESS-2"]);
  let events = std::mem::take(&mut compositor.state.ipc_events);
  assert!(events.iter().any(|event| event["event"] == "output-added"
    && event["output"]["name"] == "HEADLESS-2"));
  assert_eq!(
    ipc(&mut compositor, "wayfire/create-headless-output", json!({})),
    json!({"error": "Missing \"width\""})
  );
  // Its windows move to the active output when it goes, whatever the
  // policy, as preserve-output skips headless outputs.
  let window = placeholder(&mut compositor, "HEADLESS-2");
  assert_eq!(
    ipc(
      &mut compositor,
      "wayfire/destroy-headless-output",
      json!({"output": "HEADLESS-1"})
    ),
    json!({"error":
      "Output is not a headless output created from an IPC command!"})
  );
  assert_eq!(
    ipc(
      &mut compositor,
      "wayfire/destroy-headless-output",
      json!({})
    ),
    json!({"error": "Missing `output` or `output-id`!"})
  );
  assert_eq!(
    ipc(
      &mut compositor,
      "wayfire/destroy-headless-output",
      json!({"output-id": 2})
    ),
    json!({"result": "ok"})
  );
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(
    (info.location.output, info.away_from),
    (id("HEADLESS-1"), None)
  );
  assert_eq!(compositor.core().wsets().count(), 1);
  let events = std::mem::take(&mut compositor.state.ipc_events);
  assert!(events.iter().any(
    |event| event["event"] == "output-removed" && event["output"]["id"] == 2
  ));
  // Names and ids are not reused.
  let reply = ipc(
    &mut compositor,
    "wayfire/create-headless-output",
    json!({"width": 640, "height": 480}),
  );
  assert_eq!(reply["output"]["name"], json!("HEADLESS-3"));
  assert_eq!(reply["output"]["id"], json!(3));
}

#[test]
fn layout_and_workspace_events_carry_wayfires_payloads() {
  let (mut compositor, _path) = two_outputs(Config {
    workspace_width: 3,
    ..Config::default()
  });
  compositor.drain_events();
  compositor.state.ipc_events.clear();
  compositor
    .set_output_size(&OutputHandle::new("HEADLESS-1"), (1000, 700))
    .unwrap();
  compositor.switch_workspace(2).unwrap();
  compositor.drain_events();
  let events = std::mem::take(&mut compositor.state.ipc_events);
  let layout = events
    .iter()
    .filter(|event| event["event"] == "output-layout-changed")
    .collect::<Vec<_>>();
  assert_eq!(layout.len(), 1, "one event for the batch");
  assert_eq!(
    layout[0]["configuration"][1],
    json!({
        "name": "HEADLESS-2",
        "output-id": 2,
        "source": "self",
        "depth": 8,
        "scale": 1.0,
        "vrr": false,
        "transform": "normal",
        "mirror-from": "",
        "position": {"x": 1000, "y": 0},
        "mode": {"width": 800, "height": 600, "refresh": 60000},
    })
  );
  let workspace = events
    .iter()
    .find(|event| event["event"] == "wset-workspace-changed")
    .unwrap();
  assert_eq!(workspace["previous-workspace"], json!({"x": 0, "y": 0}));
  assert_eq!(workspace["new-workspace"], json!({"x": 2, "y": 0}));
  assert_eq!(workspace["output"], json!(1));
  assert_eq!(workspace["wset"], json!(1));
  assert_eq!(workspace["output-data"]["name"], json!("HEADLESS-1"));
  assert_eq!(workspace["wset-data"]["index"], json!(1));
}

#[test]
fn a_returning_output_reports_its_new_set() {
  let (mut compositor, _path) = two_outputs(Config::default());
  placeholder(&mut compositor, "HEADLESS-2");
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  compositor.state.ipc_events.clear();
  compositor
    .add_output(headless("HEADLESS-2"), (800, 600))
    .unwrap();
  let events = std::mem::take(&mut compositor.state.ipc_events);
  let changed = events
    .iter()
    .find(|event| event["event"] == "output-wset-changed")
    .expect("the output took its parked set");
  assert_eq!(changed["new-wset"], json!(2));
  assert_eq!(changed["output"], json!(3));
  assert_eq!(changed["new-wset-data"]["output-name"], json!("HEADLESS-2"));
}
