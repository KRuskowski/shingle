//! wayfire-shell through the in-process test client: every request and
//! event, hotspot timing, fullscreen across outputs, the menu key and its
//! IPC method, and the hotspot and fullscreen rules on their own.

use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use serde_json::{Value, json};
use shingle_core::{OutputId, WindowId, WorkspaceId};
use wayland_client::{
  Proxy,
  backend::ObjectId,
  protocol::{
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
  },
};

use super::protocol::client::{
  zwf_output_v2::ZwfOutputV2, zwf_shell_manager_v2::ZwfShellManagerV2,
};
use super::{Area, HotspotArea, HotspotEvent, HotspotState, edge_trigger};
use crate::{
  Compositor, Config, Event, InputDeviceId, InputDeviceInfo, InputDeviceKind,
  OutputHandle, OutputInfo,
  test_client::{TestClient, Toplevel},
  tests::test_compositor_with,
};

fn area(x: i64, y: i64, width: i64, height: i64) -> Area {
  Area {
    x,
    y,
    width,
    height,
  }
}

#[test]
fn edges_and_corners_trigger_on_the_last_row_or_column() {
  let output = area(1280, 0, 800, 600);
  assert_eq!(edge_trigger(output, 1), area(1280, 0, 800, 0));
  assert_eq!(edge_trigger(output, 2), area(1280, 599, 800, 0));
  assert_eq!(edge_trigger(output, 4), area(1280, 0, 0, 600));
  assert_eq!(edge_trigger(output, 8), area(2079, 0, 0, 600));
  assert_eq!(edge_trigger(output, 1 | 4), area(1280, 0, 0, 0));
  assert_eq!(edge_trigger(output, 2 | 8), area(2079, 599, 0, 0));
  // Wayfire takes any two bits as a corner and anything else as the
  // layout's origin.
  assert_eq!(edge_trigger(output, 1 | 2), area(1280, 599, 0, 0));
  assert_eq!(edge_trigger(output, 0), Area::default());
  assert_eq!(edge_trigger(output, 1 | 2 | 4), Area::default());
}

#[test]
fn a_hotspot_reaches_threshold_pixels_into_its_output() {
  let output = area(0, 0, 1280, 720);
  let top = HotspotArea::new(output, edge_trigger(output, 1), 10, 500);
  assert_eq!(top.bounds, area(0, 0, 1280, 10));
  assert!(top.contains(640.0, 0.0));
  assert!(top.contains(640.0, 9.5));
  assert!(!top.contains(640.0, 10.0));
  assert_eq!(top.proximity(640.0, 0.0), 0);
  assert_eq!(top.proximity(640.0, 6.7), 6);
  let bottom = HotspotArea::new(output, edge_trigger(output, 2), 10, 500);
  assert_eq!(bottom.bounds, area(0, 709, 1280, 11));
  assert_eq!(bottom.proximity(5.0, 715.0), 4);
  assert_eq!(bottom.proximity(5.0, 719.5), 0);
  // A corner is a disc of the threshold's radius.
  let corner = HotspotArea::new(output, edge_trigger(output, 1 | 4), 10, 0);
  assert!(corner.contains(5.0, 5.0));
  assert_eq!(corner.proximity(6.0, 8.0), 10);
  assert!(corner.contains(6.0, 8.0));
  assert!(!corner.contains(9.0, 9.0) || corner.proximity(9.0, 9.0) == 10);
  // A custom rectangle reaches out on every side, inside the output.
  let custom = HotspotArea::new(output, area(100, 100, 50, 20), 5, 0);
  assert_eq!(custom.bounds, area(95, 95, 60, 30));
  assert_eq!(custom.proximity(120.0, 110.0), 0);
  assert_eq!(custom.proximity(97.0, 110.0), 3);
  assert_eq!(custom.proximity(153.0, 124.0), 5);
  assert!(!custom.contains(160.0, 110.0));
}

#[test]
fn a_hotspot_enters_after_its_timeout_and_leaves_once() {
  let output = area(0, 0, 1280, 720);
  let start = Instant::now();
  let at = |millis| start + Duration::from_millis(millis);
  let mut hotspot = HotspotState::new(Some(HotspotArea::new(
    output,
    edge_trigger(output, 1),
    10,
    300,
  )));
  assert!(hotspot.input_at(640.0, 2.0, at(0)).is_empty());
  assert!(hotspot.tick(at(299)).is_empty());
  // Moving inside does not restart the timer.
  assert!(hotspot.input_at(600.0, 3.0, at(200)).is_empty());
  assert_eq!(
    hotspot.tick(at(300)),
    [HotspotEvent::Enter, HotspotEvent::Proximity(3)]
  );
  assert!(hotspot.tick(at(900)).is_empty(), "enter comes once");
  assert_eq!(
    hotspot.input_at(600.0, 7.0, at(910)),
    [HotspotEvent::Proximity(7)]
  );
  assert_eq!(
    hotspot.input_at(600.0, 300.0, at(920)),
    [HotspotEvent::Leave]
  );
  assert!(hotspot.input_at(600.0, 400.0, at(930)).is_empty());
  // Leaving before the timeout stops the timer; coming back restarts it.
  assert!(hotspot.input_at(10.0, 0.0, at(1000)).is_empty());
  assert!(hotspot.input_at(10.0, 50.0, at(1100)).is_empty());
  assert!(hotspot.tick(at(1400)).is_empty());
  assert!(hotspot.input_at(10.0, 0.0, at(1500)).is_empty());
  assert!(hotspot.tick(at(1799)).is_empty());
  assert_eq!(hotspot.tick(at(1800))[0], HotspotEvent::Enter);
  // Its output going makes it leave, and it never triggers again.
  assert_eq!(hotspot.output_gone(), [HotspotEvent::Leave]);
  assert!(hotspot.input_at(10.0, 0.0, at(2000)).is_empty());
  assert!(hotspot.tick(at(9000)).is_empty());
}

/// A compositor with HEADLESS-1 (1280x720) and HEADLESS-2 (800x600) to
/// its right, a keyboard and a pointer.
struct Fixture {
  compositor: Compositor,
  client: TestClient,
  keyboard: InputDeviceId,
  pointer: InputDeviceId,
  outputs: HashMap<String, WlOutput>,
  _path: std::path::PathBuf,
}

impl Fixture {
  fn new(config: Config) -> Self {
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
    let device = |compositor: &mut Compositor, kind| {
      compositor.add_input_device(InputDeviceInfo {
        name: format!("test {kind:?}"),
        vendor: None,
        product: None,
        kind,
      })
    };
    let keyboard = device(&mut compositor, InputDeviceKind::Keyboard);
    let pointer = device(&mut compositor, InputDeviceKind::Pointer);
    let mut client = TestClient::connect(&mut compositor, &path);
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
      client.roundtrip(&mut compositor);
      let name = client.events.output.iter().find_map(|event| match event {
        wl_output::Event::Name { name } => Some(name.clone()),
        _ => None,
      });
      outputs.insert(name.expect("a named output"), output);
    }
    Self {
      compositor,
      client,
      keyboard,
      pointer,
      outputs,
      _path: path,
    }
  }

  fn shell(&mut self, version: u32) -> ZwfShellManagerV2 {
    let shell = self.client.bind::<ZwfShellManagerV2>(version);
    self.client.roundtrip(&mut self.compositor);
    shell
  }

  fn shell_output(
    &mut self,
    shell: &ZwfShellManagerV2,
    name: &str,
  ) -> ZwfOutputV2 {
    let output =
      shell.get_wf_output(&self.outputs[name], &self.client.handle, ());
    self.client.roundtrip(&mut self.compositor);
    output
  }

  fn roundtrip(&mut self) {
    self.client.roundtrip(&mut self.compositor);
  }

  /// The wayfire-shell events `object` received, in order.
  fn events_of(&self, object: &ObjectId) -> Vec<String> {
    self
      .client
      .events
      .wayfire_shell
      .iter()
      .filter(|(id, _)| id == object)
      .map(|(_, event)| event.clone())
      .collect()
  }

  fn clear(&mut self) {
    self.client.events.wayfire_shell.clear();
  }

  /// A mapped toplevel, focused, on the current workspace of `output`.
  fn window_on(&mut self, output: &str) -> (Toplevel, WindowId) {
    // It maps on the active output, so that output is made active first.
    crate::output::focus_output(
      &mut self.compositor.state,
      &OutputId::new(output),
    );
    let toplevel = self.client.toplevel(&mut self.compositor, output);
    self
      .client
      .attach_buffer(&mut self.compositor, &toplevel.surface, 64, 64);
    let window = self.compositor.focused_window().expect("it maps");
    let target = self
      .compositor
      .core()
      .wset_of(&OutputId::new(output))
      .unwrap();
    if self.compositor.core().window_info(window).unwrap().wset != target {
      self
        .compositor
        .state
        .core
        .move_window_to_wset(window, target, WorkspaceId::ORIGIN)
        .unwrap();
      crate::shell::reconfigure_all(&mut self.compositor.state);
    }
    self.compositor.focus_window(window);
    self.roundtrip();
    (toplevel, window)
  }

  fn ipc(&mut self, method: &str, data: Value) -> Value {
    let reply = crate::ipc::respond(
      &mut self.compositor.state,
      &json!({"method": method, "data": data}),
    );
    self.roundtrip();
    reply
  }

  fn stipc_key(&mut self, key: &str, pressed: bool) {
    let reply =
      self.ipc("stipc/feed_key", json!({"key": key, "state": pressed}));
    assert_eq!(reply["result"], "ok", "{reply}");
  }
}

#[test]
fn the_global_is_version_3() {
  let fixture = Fixture::new(Config::default());
  let global = fixture
    .client
    .events
    .globals
    .iter()
    .find(|(_, interface, _)| interface == "zwf_shell_manager_v2")
    .map(|(_, _, version)| *version);
  assert_eq!(global, Some(3));
}

#[test]
fn hotspots_enter_after_their_timeout_and_leave() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-2");
  let handle = fixture.client.handle.clone();
  // The left edge of HEADLESS-2 (x 1280 to 2080), 10 pixels deep.
  let edge = output.create_hotspot(4, 10, 0, &handle, ());
  let custom =
    output.create_custom_hotspot(100, 100, 50, 20, 5, 0, &handle, ());
  let slow = output.create_hotspot(1, 10, 60_000, &handle, ());
  fixture.roundtrip();
  let (pointer, x) = (fixture.pointer, 1280.0);
  fixture.compositor.pointer_motion(pointer, x + 3.0, 300.0);
  // The move is seen at the end of the turn, and a zero timeout fires at
  // the next one.
  fixture.roundtrip();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&edge.id()), ["enter", "proximity 3"]);
  assert!(fixture.events_of(&slow.id()).is_empty());
  fixture.compositor.pointer_motion(pointer, x + 1.0, 300.0);
  fixture.roundtrip();
  assert_eq!(
    fixture.events_of(&edge.id()),
    ["enter", "proximity 3", "proximity 1"]
  );
  fixture.clear();
  fixture.compositor.pointer_motion(pointer, x + 300.0, 300.0);
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&edge.id()), ["leave"]);
  // The custom rectangle is in output coordinates.
  fixture.compositor.pointer_motion(pointer, x + 97.0, 110.0);
  fixture.roundtrip();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&custom.id()), ["enter", "proximity 3"]);
  // Another output's area is not the hotspot's.
  fixture.clear();
  fixture.compositor.pointer_motion(pointer, 3.0, 300.0);
  fixture.roundtrip();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&custom.id()), ["leave"]);
  assert!(fixture.events_of(&edge.id()).is_empty());
}

#[test]
fn a_hotspot_waits_for_its_timeout() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-1");
  let top = output.create_hotspot(1, 10, 80, &fixture.client.handle, ());
  fixture.roundtrip();
  fixture
    .compositor
    .pointer_motion(fixture.pointer, 500.0, 2.0);
  fixture.roundtrip();
  assert!(fixture.events_of(&top.id()).is_empty());
  // The hotspot's timer wakes the loop by itself.
  let start = Instant::now();
  while fixture.events_of(&top.id()).is_empty()
    && start.elapsed() < Duration::from_secs(5)
  {
    fixture
      .compositor
      .poll(Some(Duration::from_millis(200)))
      .unwrap();
    fixture.roundtrip();
  }
  let waited = start.elapsed();
  assert_eq!(fixture.events_of(&top.id()), ["enter", "proximity 2"]);
  assert!(waited >= Duration::from_millis(70), "{waited:?}");
}

#[test]
fn version_2_hotspots_get_no_proximity() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(2);
  let output = fixture.shell_output(&shell, "HEADLESS-1");
  let top = output.create_hotspot(1, 10, 0, &fixture.client.handle, ());
  fixture.roundtrip();
  fixture
    .compositor
    .pointer_motion(fixture.pointer, 500.0, 2.0);
  fixture.roundtrip();
  fixture.roundtrip();
  fixture
    .compositor
    .pointer_motion(fixture.pointer, 500.0, 50.0);
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&top.id()), ["enter", "leave"]);
}

#[test]
fn a_removed_output_ends_its_hotspots_and_fullscreen() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-2");
  let edge = output.create_hotspot(4, 10, 0, &fixture.client.handle, ());
  fixture.roundtrip();
  let (_toplevel, window) = fixture.window_on("HEADLESS-2");
  fixture.compositor.toggle_fullscreen(window).unwrap();
  fixture
    .compositor
    .pointer_motion(fixture.pointer, 1281.0, 300.0);
  fixture.roundtrip();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&edge.id()), ["enter", "proximity 1"]);
  assert_eq!(fixture.events_of(&output.id()), ["enter_fullscreen"]);
  fixture.clear();
  fixture
    .compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&edge.id()), ["leave"]);
  assert_eq!(fixture.events_of(&output.id()), ["leave_fullscreen"]);
  // Hotspots made on it now do nothing.
  let late = output.create_hotspot(4, 10, 0, &fixture.client.handle, ());
  fixture
    .compositor
    .pointer_motion(fixture.pointer, 1.0, 300.0);
  fixture.roundtrip();
  fixture.roundtrip();
  assert!(fixture.events_of(&late.id()).is_empty());
}

#[test]
fn fullscreen_is_announced_on_its_own_output_only() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let first = fixture.shell_output(&shell, "HEADLESS-1");
  let second = fixture.shell_output(&shell, "HEADLESS-2");
  let (_right, window) = fixture.window_on("HEADLESS-2");
  fixture.compositor.toggle_fullscreen(window).unwrap();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&second.id()), ["enter_fullscreen"]);
  assert!(fixture.events_of(&first.id()).is_empty());
  // HEADLESS-1 neither draws it nor hides its own top layer for it.
  let drawn = |fixture: &Fixture, output: &str| {
    fixture
      .compositor
      .state
      .visible_surfaces_on(&OutputId::new(output))
      .into_iter()
      .map(|snapshot| snapshot.window)
      .collect::<Vec<_>>()
  };
  assert!(!drawn(&fixture, "HEADLESS-1").contains(&Some(window)));
  assert!(drawn(&fixture, "HEADLESS-2").contains(&Some(window)));
  // Focus moving to a window on the other output keeps HEADLESS-2's
  // fullscreen view above its panels.
  let (_left, other) = fixture.window_on("HEADLESS-1");
  assert_eq!(fixture.compositor.focused_window(), Some(other));
  assert_eq!(fixture.events_of(&second.id()), ["enter_fullscreen"]);
  assert!(fixture.events_of(&first.id()).is_empty());
  // A shell output made now hears of it at once.
  let late = fixture.shell_output(&shell, "HEADLESS-2");
  assert_eq!(fixture.events_of(&late.id()), ["enter_fullscreen"]);
  fixture.compositor.toggle_fullscreen(window).unwrap();
  fixture.roundtrip();
  assert_eq!(
    fixture.events_of(&second.id()),
    ["enter_fullscreen", "leave_fullscreen"]
  );
  assert_eq!(
    fixture.events_of(&late.id()),
    ["enter_fullscreen", "leave_fullscreen"]
  );
  assert!(fixture.events_of(&first.id()).is_empty());
  // The window on HEADLESS-1 going fullscreen is HEADLESS-1's.
  fixture.compositor.toggle_fullscreen(other).unwrap();
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&first.id()), ["enter_fullscreen"]);
  // Minimizing it ends it.
  fixture.compositor.minimize_focused();
  fixture.roundtrip();
  assert_eq!(
    fixture.events_of(&first.id()),
    ["enter_fullscreen", "leave_fullscreen"]
  );
}

#[test]
fn a_client_fullscreen_request_is_announced() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-1");
  let (toplevel, _window) = fixture.window_on("HEADLESS-1");
  toplevel.toplevel.set_fullscreen(None);
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&output.id()), ["enter_fullscreen"]);
  toplevel.toplevel.unset_fullscreen();
  fixture.roundtrip();
  assert_eq!(
    fixture.events_of(&output.id()),
    ["enter_fullscreen", "leave_fullscreen"]
  );
}

#[test]
fn toggle_menu_reaches_the_outputs_shell_clients_from_version_2() {
  let mut fixture = Fixture::new(Config::default());
  let new = fixture.shell(3);
  let old = fixture.shell(1);
  let first = fixture.shell_output(&new, "HEADLESS-1");
  let second = fixture.shell_output(&new, "HEADLESS-2");
  let first_old = fixture.shell_output(&old, "HEADLESS-1");
  fixture.compositor.toggle_menu(None);
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&first.id()), ["toggle_menu"]);
  assert!(fixture.events_of(&second.id()).is_empty());
  assert!(fixture.events_of(&first_old.id()).is_empty());
  fixture.clear();
  fixture
    .compositor
    .toggle_menu(Some(&OutputHandle::new("HEADLESS-2")));
  fixture.roundtrip();
  assert_eq!(fixture.events_of(&second.id()), ["toggle_menu"]);
  assert!(fixture.events_of(&first.id()).is_empty());
}

#[test]
fn the_ipc_method_toggles_the_named_or_active_outputs_menu() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let first = fixture.shell_output(&shell, "HEADLESS-1");
  let second = fixture.shell_output(&shell, "HEADLESS-2");
  let reply = fixture.ipc("list-methods", json!({}));
  assert!(
    reply["methods"]
      .as_array()
      .unwrap()
      .contains(&json!("wayfire-shell/toggle_menu"))
  );
  assert_eq!(
    fixture.ipc("wayfire-shell/toggle_menu", json!({}))["result"],
    "ok"
  );
  assert_eq!(fixture.events_of(&first.id()), ["toggle_menu"]);
  let reply = fixture.ipc("wayfire-shell/toggle_menu", json!({"output_id": 2}));
  assert_eq!(reply["result"], "ok");
  assert_eq!(fixture.events_of(&second.id()), ["toggle_menu"]);
  let reply = fixture.ipc("wayfire-shell/toggle_menu", json!({"output-id": 2}));
  assert_eq!(reply["result"], "ok");
  assert_eq!(fixture.events_of(&second.id()).len(), 2);
  // It does not focus the output it names.
  assert_eq!(
    fixture.compositor.state.active_output,
    OutputId::new("HEADLESS-1")
  );
  let reply = fixture.ipc("wayfire-shell/toggle_menu", json!({"output_id": 9}));
  assert_eq!(reply["error"], "output id not found!");
  let reply = fixture.ipc("wayfire-shell/toggle_menu", json!({"view_id": 99}));
  assert_eq!(reply["error"], "view id not found!");
  assert_eq!(fixture.events_of(&first.id()), ["toggle_menu"]);
}

#[test]
fn a_stipc_super_tap_toggles_the_menu_without_a_frontend() {
  let config = Config {
    toggle_menu: shingle_config::Binding::modifiers_alone(vec![
      shingle_config::Modifier::Super,
    ]),
    modifier_binding_timeout: 400,
    ..Config::default()
  };
  let mut fixture = Fixture::new(config);
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-1");
  let menus = |fixture: &Fixture| fixture.events_of(&output.id()).len();
  // Pressed and released alone.
  fixture.stipc_key("KEY_LEFTMETA", true);
  fixture.stipc_key("KEY_LEFTMETA", false);
  assert_eq!(menus(&fixture), 1);
  // With another key in between.
  fixture.stipc_key("KEY_LEFTMETA", true);
  fixture.stipc_key("KEY_A", true);
  fixture.stipc_key("KEY_A", false);
  fixture.stipc_key("KEY_LEFTMETA", false);
  assert_eq!(menus(&fixture), 1);
  // Pressed while another key is held.
  fixture.stipc_key("KEY_A", true);
  fixture.stipc_key("KEY_LEFTMETA", true);
  fixture.stipc_key("KEY_LEFTMETA", false);
  fixture.stipc_key("KEY_A", false);
  assert_eq!(menus(&fixture), 1);
  // With a button in between.
  fixture.stipc_key("KEY_LEFTMETA", true);
  let pointer = fixture.pointer;
  fixture.compositor.pointer_button(pointer, 0x110, true);
  fixture.compositor.pointer_button(pointer, 0x110, false);
  fixture.stipc_key("KEY_LEFTMETA", false);
  assert_eq!(menus(&fixture), 1);
  // With Shift, which `super` alone does not match.
  fixture.stipc_key("KEY_LEFTSHIFT", true);
  fixture.stipc_key("KEY_LEFTMETA", true);
  fixture.stipc_key("KEY_LEFTMETA", false);
  fixture.stipc_key("KEY_LEFTSHIFT", false);
  assert_eq!(menus(&fixture), 1);
  // Held too long.
  fixture.compositor.state.wayfire_shell.modifier_timeout = 30;
  fixture.stipc_key("KEY_LEFTMETA", true);
  std::thread::sleep(Duration::from_millis(40));
  fixture.stipc_key("KEY_LEFTMETA", false);
  assert_eq!(menus(&fixture), 1);
  fixture.stipc_key("KEY_RIGHTMETA", true);
  fixture.stipc_key("KEY_RIGHTMETA", false);
  assert_eq!(menus(&fixture), 2);
  // The key itself reaches clients as usual: the keyboard holds nothing.
  assert!(
    crate::input_device::held_keys(&fixture.compositor.state, fixture.keyboard)
      .is_empty()
  );
}

#[test]
fn a_stipc_super_tap_does_nothing_with_the_binding_off() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-1");
  fixture.stipc_key("KEY_LEFTMETA", true);
  fixture.stipc_key("KEY_LEFTMETA", false);
  assert!(fixture.events_of(&output.id()).is_empty());
}

#[test]
fn inhibits_count_and_end_with_their_client() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.shell_output(&shell, "HEADLESS-2");
  let second = OutputHandle::new("HEADLESS-2");
  let first = OutputHandle::new("HEADLESS-1");
  let changes = |compositor: &mut Compositor| {
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::OutputInhibitChanged { output, inhibited } => {
          Some((output.name().to_owned(), inhibited))
        }
        _ => None,
      })
      .collect::<Vec<_>>()
  };
  changes(&mut fixture.compositor);
  output.inhibit_output();
  output.inhibit_output();
  fixture.roundtrip();
  assert!(fixture.compositor.output_inhibited(&second));
  assert!(!fixture.compositor.output_inhibited(&first));
  assert_eq!(
    changes(&mut fixture.compositor),
    [("HEADLESS-2".to_owned(), true)]
  );
  output.inhibit_output_done();
  fixture.roundtrip();
  assert!(fixture.compositor.output_inhibited(&second));
  output.inhibit_output_done();
  // An extra done is ignored.
  output.inhibit_output_done();
  fixture.roundtrip();
  assert!(!fixture.compositor.output_inhibited(&second));
  assert_eq!(
    changes(&mut fixture.compositor),
    [("HEADLESS-2".to_owned(), false)]
  );
  output.inhibit_output();
  fixture.roundtrip();
  assert!(fixture.compositor.output_inhibited(&second));
  // A client that goes takes its inhibits along.
  let Fixture {
    mut compositor,
    client,
    ..
  } = fixture;
  drop(client);
  for _ in 0..20 {
    compositor.poll(Some(Duration::from_millis(10))).unwrap();
    if !compositor.output_inhibited(&second) {
      break;
    }
  }
  assert!(!compositor.output_inhibited(&second));
  assert_eq!(
    changes(&mut compositor).last(),
    Some(&("HEADLESS-2".to_owned(), false))
  );
}

#[test]
fn interactive_move_moves_the_surfaces_view() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let (toplevel, window) = fixture.window_on("HEADLESS-1");
  let handle = fixture.client.handle.clone();
  let shell_surface = shell.get_wf_surface(&toplevel.surface, &handle, ());
  let compositor_global = fixture.client.bind::<WlCompositor>(4);
  let bare = compositor_global.create_surface(&handle, ());
  let bare_shell = shell.get_wf_surface(&bare, &handle, ());
  fixture.roundtrip();
  fixture.compositor.drain_events();
  shell_surface.interactive_move();
  bare_shell.interactive_move();
  fixture.roundtrip();
  let moves = fixture
    .compositor
    .drain_events()
    .into_iter()
    .filter_map(|event| match event {
      Event::BeginMove { window } => Some(window),
      _ => None,
    })
    .collect::<Vec<_>>();
  assert_eq!(moves, [window]);
}

#[test]
fn a_gone_wl_output_gives_an_inert_shell_output() {
  let mut fixture = Fixture::new(Config::default());
  let shell = fixture.shell(3);
  let output = fixture.outputs["HEADLESS-2"].clone();
  fixture
    .compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  fixture.roundtrip();
  let shell_output = shell.get_wf_output(&output, &fixture.client.handle, ());
  let hotspot =
    shell_output.create_hotspot(1, 10, 0, &fixture.client.handle, ());
  shell_output.inhibit_output();
  fixture.roundtrip();
  fixture.compositor.toggle_menu(None);
  fixture.compositor.pointer_motion(fixture.pointer, 5.0, 1.0);
  fixture.roundtrip();
  fixture.roundtrip();
  assert!(fixture.events_of(&shell_output.id()).is_empty());
  assert!(fixture.events_of(&hotspot.id()).is_empty());
  assert!(
    !fixture
      .compositor
      .output_inhibited(&OutputHandle::new("HEADLESS-1"))
  );
}
