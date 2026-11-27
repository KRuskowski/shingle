//! The active output, the pointer between outputs and moving windows
//! between outputs and workspace sets (docs/design/multi-output.md, MO-4).

use std::collections::HashMap;

use serde_json::{Value, json};
use shingle_config::{RuleAction, UnplugPolicy, WindowRule};
use shingle_core::{OutputId, Rect, WindowId, WorkspaceId, WsetId};
use wayland_client::protocol::wl_output::{self, WlOutput};

use super::{MoveGeometry, OutputHandle, OutputInfo, OutputSwitch};
use crate::{
  Compositor, Config, Event, InputDeviceId, InputDeviceInfo, InputDeviceKind,
  test_client::{TestClient, Toplevel},
  tests::test_compositor_with,
};

const BTN_LEFT: u32 = 0x110;

fn id(name: &str) -> OutputId {
  OutputId::new(name)
}

fn handle(name: &str) -> OutputHandle {
  OutputHandle::new(name)
}

/// A compositor with HEADLESS-1 (1280x720) and HEADLESS-2 (800x600) to its
/// right.
fn two_outputs(config: Config) -> (Compositor, std::path::PathBuf) {
  let (mut compositor, path) = test_compositor_with(config);
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .expect("a second output");
  (compositor, path)
}

/// A mapped 64x64 toplevel and its window.
fn mapped(
  compositor: &mut Compositor,
  client: &mut TestClient,
  app_id: &str,
) -> (Toplevel, WindowId) {
  let toplevel = client.toplevel(compositor, app_id);
  client.attach_buffer(compositor, &toplevel.surface, 64, 64);
  client.roundtrip(compositor);
  let window = compositor.focused_window().expect("the toplevel maps");
  (toplevel, window)
}

fn ipc(compositor: &mut Compositor, method: &str, data: Value) -> Value {
  crate::ipc::respond(
    &mut compositor.state,
    &json!({"method": method, "data": data}),
  )
}

/// The IPC events queued so far, emptied.
fn ipc_events(compositor: &mut Compositor) -> Vec<Value> {
  compositor.drain_events();
  std::mem::take(&mut compositor.state.ipc_events)
}

/// The events named `name` among `events`.
fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
  events
    .iter()
    .filter(|event| event["event"] == name)
    .collect()
}

fn pointer(compositor: &mut Compositor) -> InputDeviceId {
  compositor.add_input_device(InputDeviceInfo {
    name: "test pointer".into(),
    vendor: None,
    product: None,
    kind: InputDeviceKind::Pointer,
  })
}

/// The output that shows `window`'s set, by name.
fn output_of(compositor: &Compositor, window: WindowId) -> Option<String> {
  let info = compositor.core().window_info(window).ok()?;
  let wset = compositor.core().wset(info.wset).ok()?;
  wset.output.map(|output| output.as_str().to_owned())
}

/// Two windows, `left` on HEADLESS-1 and `right` on HEADLESS-2, with
/// HEADLESS-1 active and `left` focused.
fn one_on_each(
  compositor: &mut Compositor,
  client: &mut TestClient,
) -> (Toplevel, WindowId, Toplevel, WindowId) {
  let (right_toplevel, right) = mapped(compositor, client, "right");
  compositor
    .move_window_to_output(right, &handle("HEADLESS-2"), MoveGeometry::Scale)
    .unwrap();
  let (left_toplevel, left) = mapped(compositor, client, "left");
  compositor.focus_window(left);
  compositor.focus_output(&handle("HEADLESS-1"));
  client.roundtrip(compositor);
  ipc_events(compositor);
  (left_toplevel, left, right_toplevel, right)
}

#[test]
fn a_click_on_another_output_focuses_it_and_its_window() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  let device = pointer(&mut compositor);
  compositor.pointer_motion(device, 1300.0, 10.0);
  assert_eq!(
    compositor.active_output(),
    handle("HEADLESS-1"),
    "motion alone does not change the active output"
  );
  assert_eq!(compositor.focused_window(), Some(left));
  compositor.pointer_button(device, BTN_LEFT, true);
  compositor.pointer_button(device, BTN_LEFT, false);
  assert_eq!(compositor.active_output(), handle("HEADLESS-2"));
  assert_eq!(compositor.focused_window(), Some(right));
  let events = compositor.drain_events();
  assert!(events.iter().any(|event| matches!(
    event,
    Event::OutputFocused { output } if *output == handle("HEADLESS-2")
  )));
  let events = std::mem::take(&mut compositor.state.ipc_events);
  let gained = named(&events, "output-gain-focus");
  assert_eq!(gained.len(), 1, "{events:?}");
  assert_eq!(gained[0]["output"]["name"], json!("HEADLESS-2"));
  assert_eq!(gained[0]["output"]["id"], json!(2));
  let focused = ipc(
    &mut compositor,
    "window-rules/get-focused-output",
    json!({}),
  );
  assert_eq!(focused["info"]["name"], json!("HEADLESS-2"));
  let _ = std::fs::remove_file(path);
}

#[test]
fn focusing_a_window_focuses_its_output() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  ipc(
    &mut compositor,
    "window-rules/focus-view",
    json!({"id": right.0}),
  );
  assert_eq!(compositor.active_output(), handle("HEADLESS-2"));
  assert_eq!(compositor.focused_window(), Some(right));
  compositor.focus_window(left);
  assert_eq!(compositor.active_output(), handle("HEADLESS-1"));
  // Focusing the output that is active already changes nothing.
  ipc_events(&mut compositor);
  assert!(!compositor.focus_output(&handle("HEADLESS-1")));
  assert!(named(&ipc_events(&mut compositor), "output-gain-focus").is_empty());
  let _ = std::fs::remove_file(path);
}

#[test]
fn the_pointer_stays_on_the_layout() {
  let (mut compositor, path) = two_outputs(Config::default());
  let device = pointer(&mut compositor);
  compositor.pointer_motion(device, 5000.0, -50.0);
  let (x, y) = (
    compositor.state.input.pointer_x,
    compositor.state.input.pointer_y,
  );
  assert!(x < 2080.0 && x > 2079.99, "{x}");
  assert_eq!(y, 0.0);
  // Below HEADLESS-2, which is shorter, the point goes to its bottom edge.
  compositor.pointer_motion(device, 1500.0, 700.0);
  let y = compositor.state.input.pointer_y;
  assert!(y < 600.0 && y > 599.99, "{y}");
  assert_eq!(compositor.clamp_to_layout(100.0, 100.0), (100.0, 100.0));
  let _ = std::fs::remove_file(path);
}

#[test]
fn oswitch_moves_focus_and_a_window_and_warps_the_pointer() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  let active = compositor.switch_output(OutputSwitch::Next, false);
  assert_eq!(active, handle("HEADLESS-2"));
  assert_eq!(compositor.focused_window(), Some(right));
  let warp =
    compositor
      .drain_events()
      .into_iter()
      .find_map(|event| match event {
        Event::PointerWarp { x, y, output } => Some((x, y, output)),
        _ => None,
      });
  assert_eq!(warp, Some((1680.0, 300.0, Some(handle("HEADLESS-2")))));
  // Next wraps; with the window, the focused window goes along.
  compositor.state.ipc_events.clear();
  let active = compositor.switch_output(OutputSwitch::Next, true);
  assert_eq!(active, handle("HEADLESS-1"));
  assert_eq!(output_of(&compositor, right).as_deref(), Some("HEADLESS-1"));
  assert_eq!(output_of(&compositor, left).as_deref(), Some("HEADLESS-1"));
  assert_eq!(compositor.focused_window(), Some(right));
  let events = ipc_events(&mut compositor);
  let set_output = named(&events, "view-set-output");
  assert_eq!(set_output.len(), 1, "{events:?}");
  assert_eq!(set_output[0]["output"]["name"], json!("HEADLESS-1"));
  assert_eq!(set_output[0]["view"]["id"], json!(right.0));
  let wset = named(&events, "view-wset-changed");
  assert_eq!(wset[0]["old-wset"]["index"], json!(2));
  assert_eq!(wset[0]["new-wset"]["index"], json!(1));
  // Directions follow the layout: HEADLESS-2 is to the right.
  assert_eq!(
    compositor.switch_output(OutputSwitch::Left, false),
    handle("HEADLESS-1")
  );
  assert_eq!(
    compositor.switch_output(OutputSwitch::Right, false),
    handle("HEADLESS-2")
  );
  assert_eq!(
    compositor.switch_output(OutputSwitch::Below, false),
    handle("HEADLESS-2")
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn configure_view_moves_windows_with_wayfires_geometry() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_tiled_toplevel, tiled) = mapped(&mut compositor, &mut client, "tiled");
  let (_floating_toplevel, floating) =
    mapped(&mut compositor, &mut client, "floating");
  crate::shell::set_floating_geometry(
    &mut compositor.state,
    floating,
    Rect::new(100, 50, 300, 200),
  )
  .unwrap();
  let (_full_toplevel, full) = mapped(&mut compositor, &mut client, "full");
  compositor.set_fullscreen(full, true).unwrap();
  ipc_events(&mut compositor);
  for window in [tiled, floating, full] {
    let reply = ipc(
      &mut compositor,
      "window-rules/configure-view",
      json!({"id": window.0, "output_id": 2}),
    );
    assert_eq!(reply, json!({"result": "ok"}));
    assert_eq!(
      output_of(&compositor, window).as_deref(),
      Some("HEADLESS-2")
    );
  }
  let core = compositor.core();
  let target = core.wset_of(&id("HEADLESS-2")).unwrap();
  assert_eq!(
    core
      .wset_tree(target, WorkspaceId::ORIGIN)
      .unwrap()
      .windows(),
    [tiled, floating, full]
  );
  assert_eq!(
    core.window_layout(tiled).unwrap().geometry,
    Rect::new(1280, 0, 400, 600),
    "the tiled window shares the target tree with the fullscreen one's tile"
  );
  // 100 * 800 / 1280 and 50 * 600 / 720 from the new output's corner.
  assert_eq!(
    core.window_info(floating).unwrap().state.floating,
    Some(Rect::new(1342, 41, 300, 200))
  );
  let view = ipc(
    &mut compositor,
    "window-rules/view-info",
    json!({"id": full.0}),
  );
  // In the second output's own plane, as Wayfire's view geometries are.
  assert_eq!(
    view["info"]["geometry"],
    json!({"x": 0, "y": 0, "width": 800, "height": 600})
  );
  assert_eq!(view["info"]["output-name"], json!("HEADLESS-2"));
  assert_eq!(view["info"]["fullscreen"], json!(true));
  let events = ipc_events(&mut compositor);
  assert_eq!(named(&events, "view-set-output").len(), 3);
  assert_eq!(named(&events, "view-wset-changed").len(), 3);
  let missing = ipc(
    &mut compositor,
    "window-rules/configure-view",
    json!({"id": tiled.0, "output_id": 9}),
  );
  assert_eq!(missing, json!({"error": "output not found"}));
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_geometry_with_an_output_places_the_window_there() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "placed");
  let reply = ipc(
    &mut compositor,
    "window-rules/configure-view",
    json!({
      "id": window.0,
      "output-id": 2,
      "geometry": {"x": 20, "y": 20, "width": 200, "height": 100},
    }),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  assert_eq!(
    output_of(&compositor, window).as_deref(),
    Some("HEADLESS-2")
  );
  // The geometry is in the output's plane, from its corner at 1280,0.
  assert_eq!(
    compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .floating,
    Some(Rect::new(1300, 20, 200, 100))
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn vswitch_and_activators_take_an_output() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  let reply = ipc(
    &mut compositor,
    "vswitch/set-workspace",
    json!({"x": 2, "y": 0, "output-id": 2, "view-id": right.0}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  let core = compositor.core();
  assert_eq!(
    core.current_workspace(&id("HEADLESS-2")).unwrap(),
    WorkspaceId::new(2, 0)
  );
  assert_eq!(
    core.window_location(right).unwrap().workspace,
    WorkspaceId::new(2, 0)
  );
  assert_eq!(
    core.current_workspace(&id("HEADLESS-1")).unwrap(),
    WorkspaceId::ORIGIN
  );
  assert_eq!(compositor.active_output(), handle("HEADLESS-1"));
  let other = ipc(
    &mut compositor,
    "vswitch/set-workspace",
    json!({"x": 1, "y": 0, "output-id": 2, "view-id": left.0}),
  );
  assert_eq!(
    other,
    json!({"error": "view belongs to a different output"})
  );
  // send-view moves a window within its own set, on any output.
  let sent = ipc(
    &mut compositor,
    "vswitch/send-view",
    json!({"x": 1, "y": 0, "view-id": right.0}),
  );
  assert_eq!(sent, json!({"result": "ok"}));
  assert_eq!(output_of(&compositor, right).as_deref(), Some("HEADLESS-2"));
  // An activator with an output acts on it, as the active output.
  let toggled = ipc(&mut compositor, "scale/toggle", json!({"output-id": 2}));
  assert_eq!(toggled, json!({"result": "ok"}));
  assert_eq!(compositor.active_output(), handle("HEADLESS-2"));
  assert!(
    compositor
      .drain_events()
      .iter()
      .any(|event| matches!(event, Event::ToggleScale(false)))
  );
  let bound = ipc(
    &mut compositor,
    "vswitch/binding_right",
    json!({"output_id": 1}),
  );
  assert_eq!(bound, json!({"result": "ok"}));
  assert_eq!(compositor.active_output(), handle("HEADLESS-1"));
  assert_eq!(
    compositor
      .core()
      .current_workspace(&id("HEADLESS-1"))
      .unwrap(),
    WorkspaceId::new(1, 0)
  );
  let missing = ipc(&mut compositor, "expo/toggle", json!({"output-id": 7}));
  assert_eq!(missing, json!({"error": "output id not found!"}));
  let _ = std::fs::remove_file(path);
}

#[test]
fn simple_tile_layouts_reach_any_set() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  let workspace = json!({"x": 0, "y": 0});
  let layout = ipc(
    &mut compositor,
    "simple-tile/get-layout",
    json!({"wset-index": 2, "workspace": workspace}),
  );
  assert_eq!(
    layout["layout"]["vertical-split"][0]["view-id"],
    json!(right.0)
  );
  // A view of another set joins the set the layout is for.
  let applied = ipc(
    &mut compositor,
    "simple-tile/set-layout",
    json!({
      "wset-index": 2,
      "workspace": workspace,
      "layout": {"vertical-split": [
        {"view-id": right.0, "weight": 1},
        {"view-id": left.0, "weight": 1},
      ]},
    }),
  );
  assert_eq!(applied, json!({"result": "ok"}));
  assert_eq!(output_of(&compositor, left).as_deref(), Some("HEADLESS-2"));
  let events = ipc_events(&mut compositor);
  assert_eq!(named(&events, "view-wset-changed").len(), 1, "{events:?}");
  let missing = ipc(
    &mut compositor,
    "simple-tile/get-layout",
    json!({"wset-index": 9, "workspace": workspace}),
  );
  assert_eq!(missing, json!({"error": "wset-index not found"}));
  let _ = std::fs::remove_file(path);
}

#[test]
fn wsets_methods_show_sets_and_send_windows_to_them() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  let wset_of = |compositor: &Compositor, output: &str| {
    compositor.core().wset_of(&id(output)).map(WsetId::get)
  };
  // HEADLESS-1 takes set 2 from HEADLESS-2, which gets a new empty set.
  let reply = ipc(
    &mut compositor,
    "wsets/set-output-wset",
    json!({"output-id": 1, "wset-index": 2}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  assert_eq!(wset_of(&compositor, "HEADLESS-1"), Some(2));
  assert_eq!(wset_of(&compositor, "HEADLESS-2"), Some(3));
  assert_eq!(output_of(&compositor, right).as_deref(), Some("HEADLESS-1"));
  assert_eq!(output_of(&compositor, left), None, "set 1 is parked");
  let events = ipc_events(&mut compositor);
  let changed = named(&events, "output-wset-changed");
  assert_eq!(changed.len(), 2, "{events:?}");
  assert_eq!(changed[0]["new-wset"], json!(2));
  assert_eq!(changed[1]["new-wset"], json!(3));
  // And HEADLESS-2 takes set 1 back: the sets are swapped, and the empty
  // set 3 is gone.
  let reply = ipc(
    &mut compositor,
    "wsets/set-output-wset",
    json!({"output-id": 2, "wset-index": 1}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  assert_eq!(wset_of(&compositor, "HEADLESS-2"), Some(1));
  assert_eq!(output_of(&compositor, left).as_deref(), Some("HEADLESS-2"));
  assert!(compositor.core().wset(WsetId(3)).is_err());
  // A window sent to a set nobody shows is out of sight.
  let sent = ipc(
    &mut compositor,
    "wsets/send-view-to-wset",
    json!({"view-id": right.0, "wset-index": 5}),
  );
  assert_eq!(sent, json!({"result": "ok"}));
  let view = ipc(
    &mut compositor,
    "window-rules/view-info",
    json!({"id": right.0}),
  );
  assert_eq!(view["info"]["wset-index"], json!(5));
  assert_eq!(view["info"]["output-id"], json!(-1));
  let missing = ipc(
    &mut compositor,
    "wsets/set-output-wset",
    json!({"output-id": 9, "wset-index": 1}),
  );
  assert_eq!(missing, json!({"error": "output not found"}));
  let missing = ipc(
    &mut compositor,
    "wsets/send-view-to-wset",
    json!({"view-id": 999, "wset-index": 1}),
  );
  assert_eq!(missing, json!({"error": "view not found"}));
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_start_on_output_rule_moves_a_new_window() {
  let rule = WindowRule::try_from(
    "on created if app_id is \"second\" then start_on_output HEADLESS-2"
      .to_owned(),
  )
  .unwrap();
  assert!(matches!(rule.action, RuleAction::StartOnOutput(_)));
  let config = Config {
    window_rules: vec![rule],
    ..Config::default()
  };
  let (mut compositor, path) = two_outputs(config);
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_first_toplevel, first) = mapped(&mut compositor, &mut client, "first");
  let (_second_toplevel, _) = mapped(&mut compositor, &mut client, "second");
  let second = compositor
    .core()
    .window_ids()
    .find(|window| *window != first)
    .unwrap();
  assert_eq!(output_of(&compositor, first).as_deref(), Some("HEADLESS-1"));
  assert_eq!(
    output_of(&compositor, second).as_deref(),
    Some("HEADLESS-2")
  );
  let _ = std::fs::remove_file(path);
}

/// Binds every `wl_output` global the client sees, by output name.
fn bind_outputs(
  compositor: &mut Compositor,
  client: &mut TestClient,
) -> HashMap<String, WlOutput> {
  let globals = client
    .events
    .globals
    .iter()
    .filter(|(_, interface, _)| interface == "wl_output")
    .map(|(name, ..)| *name)
    .collect::<Vec<_>>();
  let mut outputs = HashMap::new();
  for global in globals {
    client.events.output.clear();
    let output = client.bind_name::<WlOutput>(global, 4);
    client.roundtrip(compositor);
    let name = client.events.output.iter().find_map(|event| match event {
      wl_output::Event::Name { name } => Some(name.clone()),
      _ => None,
    });
    outputs.insert(name.expect("a version 4 output is named"), output);
  }
  outputs
}

#[test]
fn set_fullscreen_on_an_output_moves_the_window_there() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  client.roundtrip(&mut compositor);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "video");
  toplevel
    .toplevel
    .set_fullscreen(Some(&outputs["HEADLESS-2"]));
  client.roundtrip(&mut compositor);
  assert_eq!(
    output_of(&compositor, window).as_deref(),
    Some("HEADLESS-2")
  );
  let info = compositor.core().window_info(window).unwrap();
  assert!(info.state.fullscreen);
  let configure = client.events.last_configure(&toplevel.toplevel);
  assert_eq!(configure, Some((800, 600)));
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_dialog_maps_on_its_parents_output() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (parent_toplevel, parent) = mapped(&mut compositor, &mut client, "main");
  compositor
    .move_window_to_output(parent, &handle("HEADLESS-2"), MoveGeometry::Scale)
    .unwrap();
  compositor.focus_output(&handle("HEADLESS-1"));
  let dialog = client.toplevel(&mut compositor, "dialog");
  dialog.toplevel.set_parent(Some(&parent_toplevel.toplevel));
  client.attach_buffer(&mut compositor, &dialog.surface, 64, 64);
  client.roundtrip(&mut compositor);
  let dialog_window = compositor
    .core()
    .window_ids()
    .find(|window| *window != parent)
    .unwrap();
  assert_eq!(
    output_of(&compositor, dialog_window).as_deref(),
    Some("HEADLESS-2")
  );
  // A dialog moves with its parent.
  compositor
    .move_window_to_output(
      dialog_window,
      &handle("HEADLESS-1"),
      MoveGeometry::Scale,
    )
    .unwrap();
  assert_eq!(
    output_of(&compositor, parent).as_deref(),
    Some("HEADLESS-1")
  );
  assert_eq!(
    output_of(&compositor, dialog_window).as_deref(),
    Some("HEADLESS-1")
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn an_unplug_brings_floating_windows_onto_the_active_output() {
  let config = Config {
    output_unplug: UnplugPolicy::Move,
    ..Config::default()
  };
  let (mut compositor, path) = two_outputs(config);
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "floater");
  compositor
    .move_window_to_output(window, &handle("HEADLESS-2"), MoveGeometry::Scale)
    .unwrap();
  crate::shell::set_floating_geometry(
    &mut compositor.state,
    window,
    Rect::new(1680, 300, 200, 100),
  )
  .unwrap();
  ipc_events(&mut compositor);
  compositor.remove_output(&handle("HEADLESS-2")).unwrap();
  assert_eq!(
    output_of(&compositor, window).as_deref(),
    Some("HEADLESS-1")
  );
  // 400 * 1280 / 800 and 300 * 720 / 600 from HEADLESS-1's corner.
  assert_eq!(
    compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .floating,
    Some(Rect::new(640, 360, 200, 100))
  );
  let events = ipc_events(&mut compositor);
  assert_eq!(named(&events, "view-set-output").len(), 1, "{events:?}");
  assert_eq!(named(&events, "view-wset-changed").len(), 1);
  let _ = std::fs::remove_file(path);
}

#[test]
fn an_active_output_that_returns_soon_is_focused_again() {
  let (mut compositor, path) = two_outputs(Config::default());
  compositor.focus_output(&handle("HEADLESS-2"));
  ipc_events(&mut compositor);
  compositor.remove_output(&handle("HEADLESS-2")).unwrap();
  assert_eq!(compositor.active_output(), handle("HEADLESS-1"));
  let events = ipc_events(&mut compositor);
  assert_eq!(
    named(&events, "output-gain-focus")[0]["output"]["name"],
    json!("HEADLESS-1")
  );
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .unwrap();
  assert_eq!(compositor.active_output(), handle("HEADLESS-2"));
  // Not after the timeout.
  compositor.state.focus_settings.focus_timeout = std::time::Duration::ZERO;
  compositor.remove_output(&handle("HEADLESS-2")).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(5));
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .unwrap();
  assert_eq!(compositor.active_output(), handle("HEADLESS-1"));
  let _ = std::fs::remove_file(path);
}

#[test]
fn stipc_keys_run_oswitch_without_a_frontend() {
  let (mut compositor, path) = two_outputs(Config::default());
  for (name, kind) in [
    ("stipc keyboard", InputDeviceKind::Keyboard),
    ("stipc pointer", InputDeviceKind::Pointer),
  ] {
    compositor.add_input_device(InputDeviceInfo {
      name: name.into(),
      vendor: None,
      product: None,
      kind,
    });
  }
  let feed = |compositor: &mut Compositor, key: &str, state: bool| {
    ipc(
      compositor,
      "stipc/feed_key",
      json!({"key": key, "state": state}),
    );
  };
  feed(&mut compositor, "KEY_LEFTMETA", true);
  feed(&mut compositor, "KEY_O", true);
  feed(&mut compositor, "KEY_O", false);
  feed(&mut compositor, "KEY_LEFTMETA", false);
  assert_eq!(compositor.active_output(), handle("HEADLESS-2"));
  // stipc/move_cursor and a click focus the output under the pointer.
  ipc(
    &mut compositor,
    "stipc/move_cursor",
    json!({"x": 100, "y": 100}),
  );
  ipc(
    &mut compositor,
    "stipc/feed_button",
    json!({"combo": "BTN_LEFT", "mode": "full"}),
  );
  assert_eq!(compositor.active_output(), handle("HEADLESS-1"));
  let _ = std::fs::remove_file(path);
}

#[test]
fn selecting_a_wset_asks_for_its_label_on_both_outputs() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  one_on_each(&mut compositor, &mut client);
  let labels = |compositor: &mut Compositor| {
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::WsetSelected { output, wset } => {
          Some((output.name().to_owned(), wset))
        }
        _ => None,
      })
      .collect::<Vec<_>>()
  };
  compositor.show_wset(&handle("HEADLESS-1"), 2).unwrap();
  assert_eq!(
    labels(&mut compositor),
    [("HEADLESS-1".to_owned(), 2), ("HEADLESS-2".to_owned(), 3)],
    "HEADLESS-2 lost set 2 and shows a new one"
  );
  compositor.show_wset(&handle("HEADLESS-1"), 2).unwrap();
  assert_eq!(
    labels(&mut compositor),
    [("HEADLESS-1".to_owned(), 2)],
    "the label is shown even when the set stays"
  );
  let _ = std::fs::remove_file(path);
}

/// The windows among the surfaces drawn on `output`.
fn windows_on(compositor: &Compositor, output: &str) -> Vec<WindowId> {
  compositor
    .visible_surfaces_on(&handle(output))
    .into_iter()
    .filter_map(|snapshot| snapshot.window)
    .collect()
}

/// Floats `window` across the boundary of HEADLESS-1 and HEADLESS-2,
/// with its centre on HEADLESS-1, so that it stays on the current
/// workspace.
fn straddle(compositor: &mut Compositor, window: WindowId) {
  let reply = ipc(
    compositor,
    "window-rules/configure-view",
    json!({
      "id": window.0,
      "geometry": {"x": 1170, "y": 20, "width": 200, "height": 100},
    }),
  );
  assert_eq!(reply, json!({"result": "ok"}));
}

#[test]
fn windows_are_drawn_only_on_their_own_output_by_default() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "wide");
  straddle(&mut compositor, window);
  client.roundtrip(&mut compositor);
  assert_eq!(windows_on(&compositor, "HEADLESS-1"), [window]);
  assert!(windows_on(&compositor, "HEADLESS-2").is_empty());
  let surfaces = compositor.window_surfaces(window);
  assert_eq!(surfaces.len(), 1);
  assert_eq!(surfaces[0].window, Some(window));
  assert!(compositor.window_surfaces(WindowId(999)).is_empty());
  let _ = std::fs::remove_file(path);
}

/// Rules of `lines`, each a window-rules value.
fn rules(lines: &[&str]) -> Vec<WindowRule> {
  lines
    .iter()
    .map(|line| WindowRule::try_from((*line).to_owned()).unwrap())
    .collect()
}

#[test]
fn rules_after_start_on_output_keep_the_window_tiled_on_that_output() {
  let config = Config {
    window_rules: rules(&[
      "on created if app_id is \"second\" then start_on_output HEADLESS-2",
      "on created if app_id is \"second\" then assign_workspace 0 0",
      "on created if app_id is \"second\" then resize 400 300",
      "on created if app_id is \"second\" then move 100 100",
    ]),
    ..Config::default()
  };
  let (mut compositor, path) = two_outputs(config);
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_first_toplevel, first) = mapped(&mut compositor, &mut client, "first");
  let (_second_toplevel, _) = mapped(&mut compositor, &mut client, "second");
  let second = compositor
    .core()
    .window_ids()
    .find(|window| *window != first)
    .unwrap();
  // assign_workspace takes the window's own output, not the focused one.
  assert_eq!(
    output_of(&compositor, second).as_deref(),
    Some("HEADLESS-2")
  );
  // The new window took the focus, and its output with it.
  assert_eq!(compositor.active_output(), handle("HEADLESS-2"));
  assert_eq!(compositor.focused_window(), Some(second));
  // Moved and resized, it stays in its tile, as under simple-tile.
  let info = compositor.core().window_info(second).unwrap();
  assert_eq!(info.state.floating, None);
  // Untiled, it floats where the rules put it, in its output's plane.
  let output = compositor.state.output_of_window(second).rect;
  crate::shell::toggle_tiling(&mut compositor.state, second).unwrap();
  let floating = compositor
    .core()
    .window_info(second)
    .unwrap()
    .state
    .floating;
  assert_eq!(
    floating,
    Some(Rect::new(output.x + 100, output.y + 100, 400, 300))
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_move_rule_keeps_a_floating_window_inside_the_workspace_grid() {
  let config = Config {
    window_rules: rules(&["on created if app_id is \"far\" then resize 20 10"]),
    ..Config::default()
  };
  let (mut compositor, path) = two_outputs(config);
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "far");
  crate::shell::toggle_tiling(&mut compositor.state, window).unwrap();
  let move_rule = rules(&["on created if app_id is \"far\" then move -50 -60"]);
  compositor.state.window_rules = move_rule;
  crate::window_rules::apply_created(
    &mut compositor.state,
    window,
    &shingle_core::LaunchIdentity {
      app_id: "far".into(),
      ..Default::default()
    },
  );
  let floating = compositor
    .core()
    .window_info(window)
    .unwrap()
    .state
    .floating;
  let output = compositor.state.output_of_window(window).rect;
  // Clamped to the grid's corner; resize gave it Wayfire's 40x30 minimum.
  assert_eq!(floating, Some(Rect::new(output.x, output.y, 40, 30)));
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_click_focuses_with_num_lock_on() {
  let (mut compositor, path) = two_outputs(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let (_left_toplevel, left, _right_toplevel, right) =
    one_on_each(&mut compositor, &mut client);
  let keyboard = compositor.add_input_device(InputDeviceInfo {
    name: "test keyboard".into(),
    vendor: None,
    product: None,
    kind: InputDeviceKind::Keyboard,
  });
  // Num Lock locked (Mod2), as `kb_numlock_default_state = true` leaves it.
  compositor.keyboard_modifiers(
    keyboard,
    crate::Modifiers {
      locked: 0x10,
      ..crate::Modifiers::default()
    },
  );
  assert_eq!(compositor.focused_window(), Some(left));
  let device = pointer(&mut compositor);
  compositor.pointer_motion(device, 1300.0, 10.0);
  compositor.pointer_button(device, BTN_LEFT, true);
  compositor.pointer_button(device, BTN_LEFT, false);
  assert_eq!(compositor.focused_window(), Some(right));
  let _ = std::fs::remove_file(path);
}
