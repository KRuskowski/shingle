//! Input-script tests: a [`FakeHost`] and real Wayland clients drive a
//! [`Frontend`] around a compositor on a private socket, through the same
//! entry points the nested and native hosts use.

use std::{
  io::{Read, Write},
  os::unix::net::UnixStream,
  path::PathBuf,
  sync::atomic::{AtomicU32, Ordering},
  thread,
  time::{Duration, Instant},
};

use shingle::{
  Compositor, Config, InputDeviceId, InputDeviceInfo, InputDeviceKind,
};
use shingle_config::{ConfigFormat, ShingleConfig};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuTexture, OffscreenRenderer, RenderError, RenderRect, RenderTile,
  TextureTransform,
};
use shingle_session::SessionStore;
use wayland_client::Proxy;

use super::{Frontend, drag::DragState, switcher::SwitcherKind};
use crate::{
  host::{Axis, Host, InputEvent, Touch, TouchPhase},
  render_target::RenderTarget,
  test_client::{TestClient, Toplevel},
};

const KEY_ESC: u32 = 1;
const KEY_2: u32 = 3;
const KEY_TAB: u32 = 15;
const KEY_Q: u32 = 16;
const KEY_P: u32 = 25;
const KEY_ENTER: u32 = 28;
const KEY_A: u32 = 30;
const KEY_E: u32 = 18;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_F2: u32 = 60;
const KEY_RIGHT: u32 = 106;
const KEY_LEFTMETA: u32 = 125;
const BTN_LEFT: u32 = 0x110;

/// Records what the frontend asks of its host.
#[derive(Debug, Default)]
struct FakeHost {
  redraws: usize,
  warps: Vec<(f64, f64)>,
  /// Whether `switch_vt` succeeds.
  vt_switching: bool,
  vt_switches: Vec<u32>,
  device_changes: Vec<(InputDeviceId, bool)>,
  exits: usize,
}

impl Host for FakeHost {
  fn request_redraw(&mut self) {
    self.redraws += 1;
  }

  fn warp_pointer(&mut self, x: f64, y: f64) {
    self.warps.push((x, y));
  }

  fn switch_vt(&mut self, vt: u32) -> bool {
    self.vt_switches.push(vt);
    self.vt_switching
  }

  fn input_device_changed(&mut self, device: InputDeviceId, enabled: bool) {
    self.device_changes.push((device, enabled));
  }

  fn exit(&mut self) {
    self.exits += 1;
  }
}

/// A frontend on a private socket with a keyboard, a pointer and a touch
/// device, a fake host and one connected client.
struct Fixture {
  frontend: Frontend,
  host: FakeHost,
  client: TestClient,
  keyboard: InputDeviceId,
  pointer: InputDeviceId,
  touch: InputDeviceId,
  base: PathBuf,
}

impl Fixture {
  /// Starts a frontend with `config` on a 1000x800 output.
  fn new(mut config: ShingleConfig) -> Self {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
      "shingle-frontend-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let base = std::env::temp_dir().join(name);
    // Never pick up the user's config while the tests run.
    config.behavior.auto_reload = false;
    config.session.restore = false;
    // Never bind an X display in the shared /tmp.
    config.xwayland.mode = shingle_config::XwaylandMode::Disabled;
    let mut compositor =
      Compositor::with_socket_path(Config::from(config.clone()), &base)
        .expect("the temporary directory accepts a Wayland socket");
    let mut add = |name: &str, kind| {
      compositor.add_input_device(InputDeviceInfo {
        name: name.into(),
        vendor: None,
        product: None,
        kind,
      })
    };
    let keyboard = add("test keyboard", InputDeviceKind::Keyboard);
    let pointer = add("test pointer", InputDeviceKind::Pointer);
    let touch = add("test touch", InputDeviceKind::Touch);
    compositor
      .enable_ipc(&base.with_extension("ipc"))
      .expect("the temporary directory accepts an IPC socket");
    let store = SessionStore::new(base.with_extension("session.json"));
    let mut frontend = Frontend::with_compositor(
      config,
      compositor,
      store,
      Vec::new(),
      Some(base.with_extension("yaml")),
    )
    .expect("the frontend starts");
    frontend
      .set_output_size(1000, 800)
      .expect("the output accepts the size");
    let client = TestClient::connect(frontend.compositor_mut(), &base);
    Self {
      frontend,
      host: FakeHost::default(),
      client,
      keyboard,
      pointer,
      touch,
      base,
    }
  }

  /// Maps a toplevel and lets the frontend handle the result.
  fn window(&mut self, app_id: &str) -> Toplevel {
    let toplevel =
      self
        .client
        .toplevel(self.frontend.compositor_mut(), app_id, app_id);
    self.process();
    toplevel
  }

  /// Runs one turn of the frontend.
  fn process(&mut self) {
    self.client.roundtrip(self.frontend.compositor_mut());
    self.frontend.process(&mut self.host);
  }

  fn input(&mut self, device: InputDeviceId, event: InputEvent) {
    self
      .frontend
      .handle_input(&mut self.host, device, event, None);
    self.process();
  }

  fn key(&mut self, code: u32, pressed: bool) {
    self.input(self.keyboard, InputEvent::Key { code, pressed });
  }

  /// Taps `code` with `held` keys held.
  fn tap(&mut self, code: u32, held: &[u32]) {
    for modifier in held {
      self.key(*modifier, true);
    }
    self.key(code, true);
    self.key(code, false);
    for modifier in held.iter().rev() {
      self.key(*modifier, false);
    }
  }

  fn move_pointer(&mut self, x: f64, y: f64) {
    self.input(self.pointer, InputEvent::PointerMotionAbsolute { x, y });
  }

  fn button(&mut self, code: u32, pressed: bool) {
    self.input(self.pointer, InputEvent::PointerButton { code, pressed });
  }

  /// Sends one IPC request and returns the raw reply, running the
  /// frontend until it arrives.
  fn ipc(&mut self, request: &str) -> String {
    let mut stream = UnixStream::connect(self.base.with_extension("ipc"))
      .expect("the IPC socket accepts");
    let length = u32::try_from(request.len()).expect("a short request");
    stream
      .write_all(&length.to_ne_bytes())
      .and_then(|()| stream.write_all(request.as_bytes()))
      .expect("the request is sent");
    stream
      .set_nonblocking(true)
      .expect("the stream is nonblocking");
    let mut reply = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
      self.process();
      let mut buffer = [0; 4096];
      match stream.read(&mut buffer) {
        Ok(read) => reply.extend_from_slice(&buffer[..read]),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(1));
        }
        Err(error) => panic!("reading the reply failed: {error}"),
      }
      if reply.len() >= 4 {
        let length = u32::from_ne_bytes(reply[..4].try_into().unwrap());
        if reply.len() >= 4 + length as usize {
          return String::from_utf8(reply[4..].to_vec())
            .expect("the reply is UTF-8");
        }
      }
    }
    panic!("no IPC reply to {request}");
  }

  /// Feeds evdev key `name` over `stipc/feed_key`, as Wayfire's test
  /// clients do.
  fn stipc_key(&mut self, name: &str, pressed: bool) {
    let request = format!(
      concat!(
        r#"{{"method":"stipc/feed_key","#,
        r#""data":{{"key":"{}","state":{}}}}}"#
      ),
      name, pressed
    );
    assert_eq!(self.ipc(&request), r#"{"result":"ok"}"#);
  }

  /// Taps `name` over STIPC with the `held` keys held.
  fn stipc_tap(&mut self, name: &str, held: &[&str]) {
    for modifier in held {
      self.stipc_key(modifier, true);
    }
    self.stipc_key(name, true);
    self.stipc_key(name, false);
    for modifier in held.iter().rev() {
      self.stipc_key(modifier, false);
    }
  }

  fn focused(&self) -> Option<TileWindowId> {
    self.frontend.compositor().focused_window()
  }

  fn workspace(&self) -> usize {
    self
      .frontend
      .compositor()
      .current_workspace_index()
      .expect("an output")
  }

  /// Tiled windows of the current workspace, left to right.
  fn tiles(&self) -> Vec<TileWindowId> {
    let mut windows = self
      .frontend
      .compositor()
      .switchable_windows()
      .into_iter()
      .filter_map(|window| {
        let layout = self
          .frontend
          .compositor()
          .core()
          .window_layout(window)
          .ok()?;
        Some((layout.geometry.x, window))
      })
      .collect::<Vec<_>>();
    windows.sort();
    windows.into_iter().map(|(_, window)| window).collect()
  }

  fn center(&self, window: TileWindowId) -> (f64, f64) {
    let geometry = self
      .frontend
      .compositor()
      .core()
      .window_layout(window)
      .expect("a tiled window")
      .geometry;
    (
      f64::from(geometry.x) + f64::from(geometry.width) / 2.0,
      f64::from(geometry.y) + f64::from(geometry.height) / 2.0,
    )
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    // A save still being written would leave the file behind.
    self.frontend.flush_session();
    for extension in ["session.json", "ipc"] {
      let _ = std::fs::remove_file(self.base.with_extension(extension));
    }
  }
}

fn three_windows() -> (Fixture, [TileWindowId; 3]) {
  let mut fixture = Fixture::new(ShingleConfig::default());
  for app_id in ["alpha", "bravo", "charlie"] {
    fixture.window(app_id);
  }
  let tiles = fixture.tiles();
  assert_eq!(tiles.len(), 3, "three windows tile side by side");
  (fixture, [tiles[0], tiles[1], tiles[2]])
}

/// Where Scale puts `window`.
fn scale_geometry(fixture: &Fixture, window: TileWindowId) -> RenderRect {
  fixture
    .frontend
    .scale
    .as_ref()
    .expect("Scale is open")
    .geometry[&window]
}

#[test]
fn scale_opens_moves_its_selection_and_focuses_it() {
  let (mut fixture, [.., charlie]) = three_windows();
  assert_eq!(fixture.focused(), Some(charlie));
  let redraws = fixture.host.redraws;
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let state = fixture.frontend.scale.as_ref().expect("Scale is open");
  assert_eq!(state.windows.len(), 3);
  assert_eq!(state.windows[state.selected], charlie);
  assert!(
    fixture.host.redraws > redraws,
    "opening Scale asks for a frame"
  );
  // Keys go to Scale, not to the focused client or its bindings.
  fixture.tap(KEY_RIGHT, &[]);
  let state = fixture.frontend.scale.as_ref().expect("Scale is open");
  let selected = state.windows[state.selected];
  assert_ne!(selected, charlie);
  fixture.tap(KEY_ENTER, &[]);
  assert!(fixture.frontend.scale.is_none());
  assert!(fixture.frontend.scale_exit.is_some(), "Scale animates out");
  assert_eq!(fixture.focused(), Some(selected));
}

#[test]
fn scale_selects_with_the_pointer_and_escape_restores_focus() {
  let (mut fixture, [alpha, _, charlie]) = three_windows();
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let target = scale_geometry(&fixture, alpha);
  fixture.move_pointer(
    f64::from(target.x) + f64::from(target.width) / 2.0,
    f64::from(target.y) + f64::from(target.height) / 2.0,
  );
  let state = fixture.frontend.scale.as_ref().expect("Scale is open");
  assert_eq!(state.windows[state.selected], alpha, "hover selects");
  fixture.tap(KEY_ESC, &[]);
  assert!(fixture.frontend.scale.is_none());
  assert_eq!(fixture.focused(), Some(charlie), "cancel keeps the focus");
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  fixture.move_pointer(f64::from(target.x) + 5.0, f64::from(target.y) + 5.0);
  fixture.button(BTN_LEFT, true);
  fixture.button(BTN_LEFT, false);
  assert!(fixture.frontend.scale.is_none());
  assert_eq!(fixture.focused(), Some(alpha), "a click commits");
}

#[test]
fn expo_moves_between_workspaces_with_keys_and_pointer() {
  // Without the zoom animation the cells are where they end up at once.
  let mut config = ShingleConfig::default();
  config.expo.duration_ms = 0;
  let mut fixture = Fixture::new(config);
  fixture.window("alpha");
  fixture.tap(KEY_E, &[KEY_LEFTMETA]);
  assert!(fixture.frontend.expo.is_some(), "Expo is open");
  fixture.tap(KEY_RIGHT, &[]);
  assert_eq!(fixture.frontend.expo.as_ref().expect("open").selected, 1);
  fixture.tap(KEY_ENTER, &[]);
  assert!(fixture.frontend.expo.is_none());
  assert_eq!(fixture.workspace(), 1);
  // A click on the third cell switches there.
  fixture.tap(KEY_E, &[KEY_LEFTMETA]);
  let cell = fixture.frontend.expo.as_ref().expect("open").cells[2];
  fixture.move_pointer(
    f64::from(cell.x) + f64::from(cell.width) / 2.0,
    f64::from(cell.y) + f64::from(cell.height) / 2.0,
  );
  fixture.button(BTN_LEFT, true);
  fixture.button(BTN_LEFT, false);
  assert!(fixture.frontend.expo.is_none());
  assert_eq!(fixture.workspace(), 2);
}

#[test]
fn switchers_cycle_until_their_modifier_is_released() {
  let (mut fixture, [_, bravo, charlie]) = three_windows();
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  let state = fixture.frontend.switcher.as_ref().expect("switching");
  assert_eq!(state.kind, SwitcherKind::Regular);
  assert_eq!(fixture.focused(), Some(charlie), "focus waits for release");
  fixture.key(KEY_LEFTALT, false);
  assert!(fixture.frontend.switcher.is_none());
  let first = fixture.focused().expect("a focused window");
  assert_ne!(first, charlie);
  // The fast switcher dims the others while Alt is held.
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_ESC, &[]);
  let state = fixture.frontend.switcher.as_ref().expect("switching");
  assert_eq!(state.kind, SwitcherKind::Fast);
  let selected = state.windows[state.selected];
  let dimmed = [bravo, charlie, first]
    .into_iter()
    .filter(|window| *window != selected)
    .filter(|window| {
      fixture.frontend.compositor().window_opacity(*window) < Some(1.0)
    })
    .count();
  assert!(dimmed > 0, "unselected windows are dimmed");
  fixture.key(KEY_LEFTALT, false);
  assert!(fixture.frontend.switcher.is_none());
  assert_eq!(fixture.focused(), Some(selected));
  for window in [bravo, charlie, first] {
    assert_eq!(
      fixture.frontend.compositor().window_opacity(window),
      Some(1.0),
      "opacity is restored"
    );
  }
}

#[test]
fn a_move_drag_drops_one_tile_onto_another() {
  let (mut fixture, [alpha, bravo, charlie]) = three_windows();
  let (x, y) = fixture.center(alpha);
  fixture.move_pointer(x, y);
  fixture.key(KEY_LEFTMETA, true);
  fixture.button(BTN_LEFT, true);
  assert!(matches!(
    fixture.frontend.drag,
    Some(DragState::Move { source, .. }) if source == alpha
  ));
  let (x, y) = fixture.center(charlie);
  fixture.move_pointer(x + 100.0, y);
  assert!(
    fixture.frontend.drop_preview.is_some(),
    "a drop preview shows"
  );
  fixture.button(BTN_LEFT, false);
  fixture.key(KEY_LEFTMETA, false);
  assert!(fixture.frontend.drag.is_none());
  assert!(fixture.frontend.drop_preview.is_none());
  assert_eq!(fixture.tiles(), [bravo, charlie, alpha]);
}

#[test]
fn a_workspace_switch_animates_and_moves_the_focus() {
  let (mut fixture, _) = three_windows();
  fixture.tap(KEY_2, &[KEY_LEFTMETA]);
  assert_eq!(fixture.workspace(), 1);
  assert!(
    fixture.frontend.workspace_transition.is_some(),
    "vswitch slides to the new workspace"
  );
  assert_eq!(fixture.focused(), None, "the new workspace is empty");
}

#[test]
fn held_keys_repeat_at_the_configured_rate() {
  let mut config = ShingleConfig::default();
  config.input.repeat_delay = 20;
  config.input.repeat_rate = 1000;
  let mut fixture = Fixture::new(config);
  for app_id in ["alpha", "bravo", "charlie", "delta"] {
    fixture.window(app_id);
  }
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let selected = |fixture: &Fixture| {
    fixture
      .frontend
      .scale
      .as_ref()
      .expect("Scale is open")
      .selected
  };
  let start = selected(&fixture);
  fixture.key(KEY_TAB, true);
  assert_eq!(selected(&fixture), (start + 1) % 4);
  assert!(
    fixture.frontend.next_deadline().is_some(),
    "repeat is armed"
  );
  thread::sleep(Duration::from_millis(30));
  fixture.process();
  assert_eq!(selected(&fixture), (start + 2) % 4, "one repeat");
  fixture.key(KEY_TAB, false);
  assert!(fixture.frontend.next_deadline().is_none());
  thread::sleep(Duration::from_millis(30));
  fixture.process();
  assert_eq!(selected(&fixture), (start + 2) % 4, "released keys stop");
  // Modifiers do not repeat.
  fixture.key(KEY_LEFTMETA, true);
  assert!(fixture.frontend.next_deadline().is_none());
}

#[test]
fn reset_forgets_held_keys_and_stops_repeat() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.key(KEY_LEFTMETA, true);
  fixture.key(KEY_A, true);
  assert!(fixture.frontend.next_deadline().is_some());
  assert!(fixture.frontend.keyboard.binding_modifiers().super_key);
  fixture.input(fixture.keyboard, InputEvent::Reset);
  assert!(fixture.frontend.next_deadline().is_none());
  assert!(!fixture.frontend.keyboard.binding_modifiers().super_key);
}

#[test]
fn ctrl_alt_function_keys_switch_vt_when_the_host_can() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.tap(KEY_F2, &[KEY_LEFTCTRL, KEY_LEFTALT]);
  assert_eq!(fixture.host.vt_switches, [2], "the host is asked");
  fixture.host.vt_switching = true;
  fixture.tap(KEY_F2, &[KEY_LEFTCTRL, KEY_LEFTALT]);
  assert_eq!(fixture.host.vt_switches, [2, 2]);
  fixture.tap(KEY_F2, &[]);
  assert_eq!(fixture.host.vt_switches, [2, 2], "F2 alone is a key");
}

#[test]
fn pausing_the_session_ends_held_state_and_resuming_redraws() {
  let (mut fixture, [alpha, ..]) = three_windows();
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert!(fixture.frontend.switcher.is_some());
  fixture.key(KEY_A, true);
  assert!(fixture.frontend.next_deadline().is_some(), "A repeats");
  fixture
    .frontend
    .set_session_active(&mut fixture.host, false);
  fixture.process();
  assert!(fixture.frontend.switcher.is_none(), "the switcher ends");
  assert!(fixture.frontend.next_deadline().is_none(), "repeat stops");
  assert!(!fixture.frontend.keyboard.binding_modifiers().alt);
  // Keys pressed while paused are tracked but reach nobody.
  fixture.key(KEY_LEFTMETA, true);
  fixture.key(KEY_LEFTMETA, false);

  // A drag in progress is abandoned where the window is.
  fixture.frontend.set_session_active(&mut fixture.host, true);
  let (x, y) = fixture.center(alpha);
  fixture.move_pointer(x, y);
  fixture.key(KEY_LEFTMETA, true);
  fixture.button(BTN_LEFT, true);
  assert!(fixture.frontend.drag.is_some());
  let tiles = fixture.tiles();
  fixture
    .frontend
    .set_session_active(&mut fixture.host, false);
  assert!(fixture.frontend.drag.is_none());
  assert_eq!(fixture.tiles(), tiles, "nothing is dropped");
  let redraws = fixture.host.redraws;
  fixture.frontend.set_session_active(&mut fixture.host, true);
  assert!(fixture.host.redraws > redraws, "resuming asks for a frame");
}

#[test]
fn keyboard_leds_follow_caps_lock() {
  const KEY_CAPSLOCK: u32 = 58;
  let mut fixture = Fixture::new(ShingleConfig::default());
  assert!(!fixture.frontend.keyboard_leds().caps_lock);
  fixture.tap(KEY_CAPSLOCK, &[]);
  assert!(fixture.frontend.keyboard_leds().caps_lock);
}

#[test]
fn touch_taps_commit_scale() {
  let (mut fixture, [alpha, ..]) = three_windows();
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let target = scale_geometry(&fixture, alpha);
  let touch = |phase| {
    InputEvent::Touch(Touch {
      id: 7,
      phase,
      x: f64::from(target.x) + 5.0,
      y: f64::from(target.y) + 5.0,
    })
  };
  fixture.input(fixture.touch, touch(TouchPhase::Down));
  assert!(fixture.frontend.scale.is_none());
  assert_eq!(fixture.focused(), Some(alpha));
  fixture.input(fixture.touch, touch(TouchPhase::Up));
}

#[test]
fn zoom_follows_the_wheel_with_its_modifier() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.move_pointer(100.0, 200.0);
  let redraws = fixture.host.redraws;
  fixture.key(KEY_LEFTMETA, true);
  fixture.input(
    fixture.pointer,
    InputEvent::PointerAxis(Axis {
      vertical: -10.0,
      vertical_steps: -1,
      ..Axis::default()
    }),
  );
  fixture.key(KEY_LEFTMETA, false);
  assert!(fixture.frontend.zoom_target > 1.0, "scrolling up zooms in");
  assert_eq!(fixture.frontend.zoom_origin, (100.0, 200.0));
  assert!(fixture.host.redraws > redraws);
}

#[test]
fn disabling_a_device_drops_its_input_and_tells_the_host() {
  let (mut fixture, [alpha, ..]) = three_windows();
  let (x, y) = fixture.center(alpha);
  let request = format!(
    concat!(
      r#"{{"method":"input/configure-device","#,
      r#""data":{{"id":{},"enabled":false}}}}"#
    ),
    fixture.pointer.get()
  );
  assert_eq!(fixture.ipc(&request), r#"{"result":"ok"}"#);
  assert_eq!(fixture.host.device_changes, [(fixture.pointer, false)]);
  fixture.move_pointer(x, y);
  assert_eq!(
    (fixture.frontend.pointer_x, fixture.frontend.pointer_y),
    (0.0, 0.0),
    "motion is dropped"
  );
}

#[test]
fn quit_saves_and_asks_the_host_to_exit() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.window("alpha");
  assert_eq!(fixture.host.exits, 0);
  fixture.tap(KEY_Q, &[KEY_LEFTMETA, KEY_LEFTSHIFT]);
  assert!(fixture.host.exits > 0, "the host is asked to exit");
  fixture.frontend.flush_session();
  assert!(fixture.base.with_extension("session.json").exists());
}

/// Parses a YAML config, the way `shingle.yaml` is read.
fn yaml_config(text: &str) -> ShingleConfig {
  ShingleConfig::parse(text, ConfigFormat::Yaml).expect("the config parses")
}

#[test]
fn stipc_keys_fire_key_bindings_from_the_config() {
  let config = yaml_config(concat!(
    "keybindings:\n",
    "  expo_toggle: super+x\n",
    "expo:\n",
    "  duration_ms: 0\n",
  ));
  let mut fixture = Fixture::new(config);
  fixture.window("alpha");
  fixture.stipc_tap("KEY_X", &["KEY_LEFTMETA"]);
  assert!(fixture.frontend.expo.is_some(), "the YAML binding fired");
  // Keys reach Expo like keys from the host keyboard.
  fixture.stipc_tap("KEY_ESC", &[]);
  assert!(fixture.frontend.expo.is_none());
  // Super was released: a bare P does nothing, and the default Super+P
  // opens Scale.
  fixture.stipc_tap("KEY_P", &[]);
  assert!(fixture.frontend.scale.is_none(), "no modifier is stuck");
  fixture.stipc_tap("KEY_P", &["KEY_LEFTMETA"]);
  assert!(fixture.frontend.scale.is_some());
}

#[test]
fn stipc_keys_fire_xkb_command_bindings_in_shingles_layout() {
  let marker = std::env::temp_dir()
    .join(format!("shingle-frontend-stipc-xkb-{}", std::process::id()));
  let _ = std::fs::remove_file(&marker);
  let config = yaml_config(&format!(
    concat!(
      "input:\n",
      "  layout: de\n",
      "commands:\n",
      "  - binding: super+xkb:z\n",
      "    command: touch {}\n",
    ),
    marker.display()
  ));
  let mut fixture = Fixture::new(config);
  // On a German layout the key labelled Z is the physical Y key.
  fixture.stipc_tap("KEY_Z", &["KEY_LEFTMETA"]);
  fixture.stipc_tap("KEY_Y", &["KEY_LEFTMETA"]);
  let deadline = Instant::now() + Duration::from_secs(5);
  while !marker.exists() && Instant::now() < deadline {
    fixture.process();
    thread::sleep(Duration::from_millis(10));
  }
  assert!(marker.exists(), "super+xkb:z ran its command");
  let _ = std::fs::remove_file(&marker);
}

#[test]
fn stipc_keys_fire_ipc_bindings_once() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  let reply = fixture.ipc(concat!(
    r#"{"method":"command/register-binding","data":{"#,
    r#""binding":"KEY_F9","call-method":"invert/toggle","call-data":{}}}"#
  ));
  assert!(reply.contains(r#""result":"ok""#), "{reply}");
  fixture.stipc_tap("KEY_F9", &[]);
  fixture.process();
  assert!(fixture.frontend.inverted, "the IPC binding toggled once");
  fixture.stipc_tap("KEY_F9", &[]);
  fixture.process();
  assert!(!fixture.frontend.inverted);
}

#[test]
fn stipc_keys_are_dropped_while_the_session_is_inactive() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.window("alpha");
  fixture.frontend.compositor_mut().set_session_active(false);
  fixture.stipc_tap("KEY_P", &["KEY_LEFTMETA"]);
  assert!(fixture.frontend.scale.is_none());
  fixture.frontend.compositor_mut().set_session_active(true);
  fixture.stipc_tap("KEY_P", &["KEY_LEFTMETA"]);
  assert!(fixture.frontend.scale.is_some());
}

/// Records the tiles of every frame drawn into an offscreen renderer.
struct RecordingTarget {
  renderer: OffscreenRenderer,
  frames: Vec<Vec<(RenderRect, bool)>>,
  /// The alpha of every tile of every frame.
  alphas: Vec<Vec<f32>>,
  /// How long each frame takes to draw, to simulate a slow GPU.
  delay: Duration,
  /// The size of every screencopy capture drawn.
  captures: Vec<(u32, u32)>,
}

impl RenderTarget for RecordingTarget {
  fn set_scale_factor(&mut self, scale: f64) {
    self.renderer.set_scale_factor(scale);
  }

  fn set_output_transform(&mut self, transform: TextureTransform) {
    self.renderer.set_output_transform(transform);
  }

  fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    self.renderer.set_nearest_filter(nearest)
  }

  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    self.renderer.set_zoom(factor, origin_x, origin_y);
  }

  fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    self.renderer.set_gamma_ramp(ramp)
  }

  fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    self.renderer.set_inversion(inverted, preserve_hue);
  }

  fn set_tearing(&mut self, tearing: bool) {
    RenderTarget::set_tearing(&mut self.renderer, tearing);
  }

  fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError> {
    self.renderer.create_texture(width, height, rgba)
  }

  fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    RenderTarget::update_texture(&self.renderer, texture, rgba)
  }

  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    self.frames.push(
      tiles
        .iter()
        .map(|tile| (tile.geometry, tile.texture.is_some()))
        .collect(),
    );
    self
      .alphas
      .push(tiles.iter().map(|tile| tile.color[3]).collect());
    thread::sleep(self.delay);
    RenderTarget::render(&mut self.renderer, tiles, clear)
  }

  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    self.captures.push(size);
    RenderTarget::capture(&mut self.renderer, tiles, clear, size)
  }
}

/// An offscreen recording target, or `None` without a GPU adapter.
fn recording_target() -> Option<RecordingTarget> {
  match pollster::block_on(OffscreenRenderer::new(1000, 800)) {
    Ok(renderer) => Some(RecordingTarget {
      renderer,
      frames: Vec::new(),
      alphas: Vec::new(),
      delay: Duration::ZERO,
      captures: Vec::new(),
    }),
    Err(RenderError::RequestAdapter(error)) => {
      eprintln!("skipping scene test, no GPU adapter: {error}");
      None
    }
    Err(error) => panic!("offscreen renderer failed: {error}"),
  }
}

#[test]
fn the_scene_draws_tiles_and_then_scale_thumbnails_with_titles() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.scale.duration_ms = 0;
  config.animate.open_animation = "none".into();
  let mut fixture = Fixture::new(config);
  let alpha = {
    fixture.window("alpha");
    fixture.window("bravo");
    fixture.tiles()[0]
  };
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  let tiled = fixture
    .tiles()
    .into_iter()
    .map(|window| {
      let geometry = fixture
        .frontend
        .compositor()
        .core()
        .window_layout(window)
        .expect("tiled")
        .geometry;
      (
        RenderRect {
          x: geometry.x,
          y: geometry.y,
          width: geometry.width,
          height: geometry.height,
        },
        true,
      )
    })
    .collect::<Vec<_>>();
  assert_eq!(target.frames[0], tiled, "one textured tile per window");
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let thumbnail = scale_geometry(&fixture, alpha);
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  let frame = target.frames.last().expect("a frame");
  assert!(
    frame.contains(&(thumbnail, true)),
    "Alpha is drawn at its thumbnail: {frame:?}"
  );
  // Two thumbnails, then a title texture inside each.
  assert_eq!(frame.len(), 4, "{frame:?}");
  let inside = |inner: RenderRect, outer: RenderRect| {
    inner.x >= outer.x
      && inner.y >= outer.y
      && inner.x + inner.width as i32 <= outer.x + outer.width as i32
      && inner.y + inner.height as i32 <= outer.y + outer.height as i32
  };
  for (title, textured) in &frame[2..] {
    assert!(*textured);
    assert!(
      frame[..2]
        .iter()
        .any(|(thumbnail, _)| inside(*title, *thumbnail)),
      "a title is inside a thumbnail: {frame:?}"
    );
  }
}

/// Draws frames the way a host does, for as long as the frontend asks for
/// them.
fn render_until_idle(fixture: &mut Fixture, target: &mut RecordingTarget) {
  for _ in 0..20 {
    let redraws = fixture.host.redraws;
    fixture
      .frontend
      .render(&mut fixture.host, target)
      .expect("the frame renders");
    if fixture.host.redraws == redraws {
      return;
    }
  }
  panic!("the frontend keeps asking for frames");
}

/// A frame that takes longer than the rest of an animation must not be
/// its last: the settled state is drawn after it.
#[test]
fn a_slow_frame_does_not_end_the_open_animation_early() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "zoom".into();
  config.animate.duration_ms = 300;
  let mut fixture = Fixture::new(config);
  fixture.window("alpha");
  let alpha = fixture.tiles()[0];
  let geometry = fixture
    .frontend
    .compositor()
    .core()
    .window_layout(alpha)
    .expect("tiled")
    .geometry;
  let settled = RenderRect {
    x: geometry.x,
    y: geometry.y,
    width: geometry.width,
    height: geometry.height,
  };
  target.delay = Duration::from_millis(400);
  render_until_idle(&mut fixture, &mut target);
  assert_eq!(
    target.frames.last().expect("a frame"),
    &[(settled, true)],
    "the last frame shows the window at its full size: {:?}",
    target.frames
  );
  assert_eq!(target.alphas.last().expect("a frame"), &[1.0]);
  assert!(fixture.frontend.open_animations.is_empty());
}

#[test]
fn a_slow_frame_does_not_end_the_workspace_slide_early() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.vswitch.duration_ms = 300;
  let mut fixture = Fixture::new(config);
  fixture.window("alpha");
  render_until_idle(&mut fixture, &mut target);
  fixture.tap(KEY_2, &[KEY_LEFTMETA]);
  assert!(fixture.frontend.workspace_transition.is_some());
  target.frames.clear();
  target.delay = Duration::from_millis(400);
  render_until_idle(&mut fixture, &mut target);
  assert!(fixture.frontend.workspace_transition.is_none());
  // At the end of the slide the old workspace is entirely off screen.
  let output = fixture.frontend.compositor().output();
  let last = target.frames.last().expect("a frame");
  assert!(
    last.iter().all(|(rect, _)| {
      rect.x >= output.x + output.width as i32
        || rect.x + rect.width as i32 <= output.x
        || rect.y >= output.y + output.height as i32
        || rect.y + rect.height as i32 <= output.y
    }),
    "the last frame shows the settled workspace: {:?}",
    target.frames
  );
}

/// The first pixel of an ARGB8888 capture as (r, g, b).
fn first_pixel(bytes: &[u8]) -> (u8, u8, u8) {
  (bytes[2], bytes[1], bytes[0])
}

#[test]
fn screencopy_captures_the_frame_as_drawn_including_inversion() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(ShingleConfig::default());
  let capture = fixture
    .client
    .capture(fixture.frontend.compositor_mut(), (10, 20, 4, 3));
  assert!(
    fixture.client.captures().is_empty(),
    "nothing before a frame"
  );
  fixture.frontend.process(&mut fixture.host);
  assert!(fixture.host.redraws > 0, "a capture asks for a frame");
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  fixture.process();
  assert_eq!(fixture.client.captures(), ["ready"]);
  assert_eq!(
    target.captures,
    [(1000, 800)],
    "one draw at the output size"
  );
  // The background, 0.1 linear, is 89 in sRGB.
  let (r, g, b) = first_pixel(&capture.bytes());
  assert!(r.abs_diff(89) <= 2 && r == g && g == b, "{:?}", (r, g, b));
  fixture.frontend.toggle_inversion();
  let inverted = fixture
    .client
    .capture(fixture.frontend.compositor_mut(), (0, 0, 2, 2));
  fixture.frontend.process(&mut fixture.host);
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  fixture.process();
  assert_eq!(fixture.client.captures(), ["ready", "ready"]);
  // 0.9 linear is 243 in sRGB.
  let (r, g, b) = first_pixel(&inverted.bytes());
  assert!(r.abs_diff(243) <= 2 && r == g && g == b, "{:?}", (r, g, b));
}

/// A frontend offering linux-dmabuf through a fake importer whose texture
/// lives on `target`'s device, and a mapped toplevel, with animations off.
fn dmabuf_fixture(
  target: &RecordingTarget,
) -> (Fixture, crate::dmabuf::tests::FakeImport, Toplevel) {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.animate.close_animation = "none".into();
  config.tiling.animation_duration = 0;
  let mut fixture = Fixture::new(config);
  let texture = target
    .create_texture(1, 1, &[255; 4])
    .expect("the fake texture uploads");
  let fake = crate::dmabuf::tests::FakeImport::new(texture);
  fixture
    .frontend
    .set_dmabuf_importer(
      crate::dmabuf::tests::test_feedback(),
      Box::new(fake.clone()),
    )
    .expect("linux-dmabuf is offered");
  fixture.process();
  let toplevel = fixture.window("dmabuf");
  (fixture, fake, toplevel)
}

#[test]
fn dmabuf_surfaces_draw_cached_imports_and_release_after_the_gpu() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, fake, toplevel) = dmabuf_fixture(&target);
  let compositor = fixture.frontend.compositor_mut();
  let first = fixture.client.dmabuf_buffer(compositor, 64, 32);
  let second = fixture.client.dmabuf_buffer(compositor, 64, 32);
  assert_eq!(fake.imports.get(), 2, "each buffer is imported at create");
  let surface = toplevel.surface.clone();
  let show = |fixture: &mut Fixture, buffer, target: &mut RecordingTarget| {
    fixture.client.commit_buffer(
      fixture.frontend.compositor_mut(),
      &surface,
      buffer,
    );
    fixture.frontend.process(&mut fixture.host);
    fixture
      .frontend
      .render(&mut fixture.host, target)
      .expect("the frame renders");
  };
  show(&mut fixture, &first, &mut target);
  assert!(
    target
      .frames
      .last()
      .is_some_and(|frame| frame.iter().any(|tile| tile.1)),
    "the surface is drawn from its texture"
  );
  show(&mut fixture, &second, &mut target);
  show(&mut fixture, &first, &mut target);
  assert_eq!(fake.imports.get(), 2, "commits use the cache");
  // `second` was replaced, but the frame that drew it may still run.
  fixture.process();
  fixture.process();
  assert!(!fixture.client.releases().contains(&second.id()));
  fake.finish_work();
  fixture.process();
  fixture.process();
  assert!(fixture.client.releases().contains(&second.id()));
  assert!(
    !fixture.client.releases().contains(&first.id()),
    "first is shown"
  );
}

#[test]
fn dmabuf_textures_are_imported_again_after_a_gpu_loss() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, fake, toplevel) = dmabuf_fixture(&target);
  let compositor = fixture.frontend.compositor_mut();
  let buffer = fixture.client.dmabuf_buffer(compositor, 64, 32);
  fixture.client.commit_buffer(
    fixture.frontend.compositor_mut(),
    &toplevel.surface,
    &buffer,
  );
  fixture.frontend.process(&mut fixture.host);
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert_eq!(fake.imports.get(), 1);
  fixture.frontend.drop_gpu_resources();
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert_eq!(fake.imports.get(), 2, "imported again from the kept fds");
  assert!(
    target
      .frames
      .last()
      .is_some_and(|frame| frame.iter().any(|tile| tile.1))
  );
}

#[test]
fn a_surface_that_goes_from_dmabuf_to_shm_gets_an_uploaded_texture() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, _fake, toplevel) = dmabuf_fixture(&target);
  let compositor = fixture.frontend.compositor_mut();
  let buffer = fixture.client.dmabuf_buffer(compositor, 200, 150);
  fixture.client.commit_buffer(
    fixture.frontend.compositor_mut(),
    &toplevel.surface,
    &buffer,
  );
  fixture.frontend.process(&mut fixture.host);
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  fixture.client.attach_buffer(
    fixture.frontend.compositor_mut(),
    &toplevel.surface,
    200,
    150,
  );
  fixture.frontend.process(&mut fixture.host);
  // Writing shm pixels into the imported texture would fail validation.
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
}

/// A frontend that uploads shm buffers on `target`'s device, with
/// animations off.
fn shm_fixture(target: &RecordingTarget) -> Fixture {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.animate.close_animation = "none".into();
  config.tiling.animation_duration = 0;
  config.vswitch.duration_ms = 0;
  let mut fixture = Fixture::new(config);
  fixture.frontend.attach_gpu(target.renderer.gpu());
  fixture
}

/// Whether the last frame drew a textured tile.
fn drew_texture(target: &RecordingTarget) -> bool {
  target
    .frames
    .last()
    .is_some_and(|frame| frame.iter().any(|tile| tile.1))
}

#[test]
fn shm_commits_upload_their_damage_and_are_released_without_a_frame() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = shm_fixture(&target);
  let toplevel = fixture.window("shm");
  let surface = toplevel.surface.clone();
  let buffer = fixture.client.shm_buffer(100, 80);
  fixture.client.commit_buffer(
    fixture.frontend.compositor_mut(),
    &surface,
    &buffer,
  );
  fixture.frontend.process(&mut fixture.host);
  let (id, bytes) = *fixture.frontend.uploads.last().expect("an upload");
  assert_eq!(bytes, 100 * 80 * 4, "a new size is written in full");
  fixture.process();
  assert!(
    fixture.client.releases().contains(&buffer.id()),
    "released after its upload, before any frame"
  );
  // The client draws again into the same buffer and damages a square.
  surface.attach(Some(&buffer), 0, 0);
  surface.damage_buffer(10, 20, 5, 5);
  surface.commit();
  fixture.process();
  assert_eq!(fixture.frontend.uploads.last(), Some(&(id, 5 * 5 * 4)));
  fixture.process();
  assert_eq!(
    fixture
      .client
      .releases()
      .iter()
      .filter(|released| **released == buffer.id())
      .count(),
    2
  );
  // A commit without a new buffer uploads nothing.
  let uploads = fixture.frontend.uploads.len();
  surface.commit();
  fixture.process();
  assert_eq!(fixture.frontend.uploads.len(), uploads);
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert!(drew_texture(&target), "drawn from the uploaded texture");
}

#[test]
fn uploaded_textures_survive_a_filter_change_and_go_when_unmapped() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = shm_fixture(&target);
  let toplevel = fixture.window("shm");
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert!(drew_texture(&target));
  let id = fixture.frontend.uploads.last().expect("an upload").0;
  let uploads = fixture.frontend.uploads.len();
  fixture.frontend.zoom.interpolation = "nearest".into();
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert!(drew_texture(&target), "rebound, not lost");
  assert_eq!(fixture.frontend.uploads.len(), uploads, "nothing uploaded");
  assert!(fixture.frontend.textures.contains_key(&id));
  toplevel.surface.attach(None, 0, 0);
  toplevel.surface.commit();
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert!(
    !fixture.frontend.textures.contains_key(&id),
    "dropped once unmapped"
  );
}

#[test]
fn a_buffer_whose_upload_was_missed_is_uploaded_in_full_by_the_frame() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = shm_fixture(&target);
  let toplevel = fixture.window("shm");
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  let buffer = fixture.client.shm_buffer(40, 30);
  let surface = toplevel.surface.clone();
  surface.attach(Some(&buffer), 0, 0);
  surface.damage_buffer(0, 0, 1, 1);
  surface.commit();
  fixture.client.roundtrip(fixture.frontend.compositor_mut());
  fixture
    .frontend
    .compositor_mut()
    .dispatch_pending()
    .unwrap();
  // Its event is dropped, as if the frontend had missed it.
  fixture.frontend.compositor_mut().drain_events();
  let uploads = fixture.frontend.uploads.len();
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert_eq!(fixture.frontend.uploads.len(), uploads + 1);
  assert_eq!(fixture.frontend.uploads.last().unwrap().1, 40 * 30 * 4);
  fixture.process();
  assert!(fixture.client.releases().contains(&buffer.id()));
}

#[test]
fn an_uploaded_texture_stays_while_its_surface_is_hidden() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = shm_fixture(&target);
  let _toplevel = fixture.window("shm");
  let id = fixture.frontend.uploads.last().expect("an upload").0;
  fixture
    .frontend
    .compositor_mut()
    .switch_workspace(1)
    .expect("a second workspace");
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert!(!drew_texture(&target), "nothing on the other workspace");
  assert!(
    fixture.frontend.textures.contains_key(&id),
    "kept, since its buffer went back"
  );
  let uploads = fixture.frontend.uploads.len();
  fixture
    .frontend
    .compositor_mut()
    .switch_workspace(0)
    .expect("the first workspace");
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &mut target)
    .expect("the frame renders");
  assert!(drew_texture(&target), "shown again without a commit");
  assert_eq!(fixture.frontend.uploads.len(), uploads);
}

#[test]
fn shutdown_frees_the_x_display_and_saves_once() {
  static NEXT: AtomicU32 = AtomicU32::new(0);
  let directory = std::env::temp_dir().join(format!(
    "shingle-frontend-shutdown-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  ));
  let _ = std::fs::remove_dir_all(&directory);
  std::fs::create_dir_all(&directory).unwrap();
  let mut yaml = ShingleConfig::default();
  yaml.behavior.auto_reload = false;
  yaml.session.restore = false;
  let mut config = Config::from(yaml.clone());
  // A lazy server in a private directory, which never starts here.
  config.xwayland.mode = shingle_config::XwaylandMode::Lazy;
  config.xwayland.socket_base = directory.clone();
  config.xwayland.program = Some("/nonexistent/shingle-test-xwayland".into());
  let compositor =
    Compositor::with_socket_path(config, &directory.join("wayland")).unwrap();
  let session = directory.join("session.json");
  let mut frontend = Frontend::with_compositor(
    yaml,
    compositor,
    SessionStore::new(&session),
    Vec::new(),
    None,
  )
  .unwrap();
  assert!(directory.join(".X0-lock").exists());
  frontend.shutdown();
  // Freed while the compositor still exists, not only on its drop.
  assert!(!directory.join(".X0-lock").exists());
  assert!(!directory.join(".X11-unix/X0").exists());
  assert_eq!(frontend.compositor().x_display(), None);
  assert!(session.exists());
  // The last save was the one before Xwayland stopped.
  std::fs::remove_file(&session).unwrap();
  frontend.shutdown();
  frontend.save();
  assert!(!session.exists());
  drop(frontend);
  std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn shutdown_saves_the_mapped_windows() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.window("alpha");
  fixture.frontend.shutdown();
  let session = fixture.base.with_extension("session.json");
  let (core, _) = SessionStore::new(&session).load().unwrap().unwrap();
  let windows = core.snapshot().windows;
  assert_eq!(windows.len(), 1);
  assert_eq!(windows[0].identity.app_id, "alpha");
}

#[test]
fn layout_changes_are_saved_in_the_background() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  let session = fixture.base.with_extension("session.json");
  let saved = |fixture: &Fixture| {
    fixture.frontend.flush_session();
    let (core, _) = SessionStore::new(&session).load().unwrap().unwrap();
    core
      .snapshot()
      .windows
      .into_iter()
      .map(|window| window.identity.app_id)
      .collect::<Vec<_>>()
  };
  fixture.window("alpha");
  assert_eq!(saved(&fixture), ["alpha"]);
  fixture.window("bravo");
  assert_eq!(saved(&fixture), ["alpha", "bravo"]);
}
