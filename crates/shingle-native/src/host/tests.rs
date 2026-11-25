//! The native host driven through a fake seat and fake libinput, with a
//! real frontend, compositor and Wayland client on a private socket.

use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};

use shingle::{Compositor, Config, InputDeviceId, InputDeviceKind};
use shingle_config::ShingleConfig;
use shingle_frontend::{
  Frontend,
  host::{Gesture, TouchPhase},
  keyboard::Leds,
};
use shingle_session::SessionStore;

use super::Backend;
use crate::{
  input::{
    Capabilities, DeviceDescription, DeviceInput, DeviceKey, NativeEvent,
    ScrollAxis, ScrollSource, SendEvents, Setting,
    tests::{FakeInput, POINTER, device, key},
  },
  session::{SeatEvent, tests::FakeSession},
  test_client::{Input, TestClient, Toplevel},
  tests::{base_path, configure_request, ipc_request, test_config},
};

const KEY_A: u32 = 30;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_CAPSLOCK: u32 = 58;
const KEY_F2: u32 = 60;
const BTN_LEFT: u32 = 0x110;
const SHIFT: u32 = 1;

const KEYBOARD: Capabilities = Capabilities {
  keyboard: true,
  pointer: false,
  touch: false,
  gesture: false,
};

const TOUCHSCREEN: Capabilities = Capabilities {
  keyboard: false,
  pointer: false,
  touch: true,
  gesture: false,
};

/// A native backend on a fake seat and fake libinput, with one client
/// window that has keyboard focus.
struct Fixture {
  backend: Backend<FakeSession, FakeInput>,
  session: Rc<RefCell<FakeSession>>,
  client: TestClient,
  _toplevel: Toplevel,
  base: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    Self::with_config(test_config())
  }

  fn with_config(config: ShingleConfig) -> Self {
    let base = base_path();
    let mut compositor =
      Compositor::with_socket_path(Config::from(config.clone()), &base)
        .expect("the temporary directory accepts a Wayland socket");
    compositor
      .enable_ipc(&base.with_extension("ipc"))
      .expect("the temporary directory accepts an IPC socket");
    let store = SessionStore::new(base.with_extension("session.json"));
    let frontend = Frontend::with_compositor(
      config.clone(),
      compositor,
      store,
      Vec::new(),
      Some(base.with_extension("yaml")),
    )
    .expect("the frontend starts");
    let session = Rc::new(RefCell::new(FakeSession::default()));
    let mut backend = Backend::new(
      frontend,
      Rc::clone(&session),
      FakeInput::default(),
      config.input,
      (1280, 720),
      None,
    )
    .expect("the backend starts");
    let compositor = backend.frontend_mut().compositor_mut();
    let mut client = TestClient::connect(compositor, &base);
    client.seat(compositor);
    let toplevel = client.toplevel(compositor, "native", "native");
    let mut fixture = Self {
      backend,
      session,
      client,
      _toplevel: toplevel,
      base,
    };
    fixture.step();
    fixture
  }

  /// Runs one turn of the loop and lets the client read what it got.
  fn step(&mut self) -> bool {
    let running = self.backend.step();
    self
      .client
      .roundtrip(self.backend.frontend_mut().compositor_mut());
    running
  }

  /// Queues libinput events and runs a turn.
  fn libinput(&mut self, events: impl IntoIterator<Item = NativeEvent>) {
    self.backend.input.pending.extend(events);
    self.step();
  }

  fn add(&mut self, device: DeviceDescription) {
    self.libinput([NativeEvent::Added(device)]);
  }

  fn input(&mut self, device: &str, time_usec: u64, input: DeviceInput) {
    self.libinput([NativeEvent::Input {
      device: key(device),
      time_usec,
      input,
    }]);
  }

  fn key(&mut self, device: &str, code: u32, pressed: bool) {
    self.input(device, 1_000, DeviceInput::Key { code, pressed });
  }

  /// Queues seat events for the next dispatch and runs a turn.
  fn seat(&mut self, events: Vec<SeatEvent>) {
    self.session.borrow_mut().script.push_back(events);
    self.step();
  }

  fn seat_device(&self, device: &str, kind: InputDeviceKind) -> InputDeviceId {
    self
      .backend
      .devices
      .seat_device(&key(device), kind)
      .expect("the device is registered")
  }

  /// Sends an IPC request and runs the loop until the frontend handled it.
  fn ipc(&mut self, request: &str) -> String {
    let path = self.base.with_extension("ipc");
    let backend = &mut self.backend;
    let reply = ipc_request(&path, request, || {
      backend
        .frontend_mut()
        .compositor_mut()
        .poll(Some(Duration::from_millis(5)))
        .expect("dispatch succeeds");
    });
    self.step();
    reply
  }

  fn list_devices(&mut self) -> Vec<serde_json::Value> {
    let reply = self.ipc(r#"{"method":"input/list-devices","data":{}}"#);
    serde_json::from_str(&reply).expect("a JSON list")
  }

  /// Takes what the client received since the last call.
  fn received(&mut self) -> Vec<Input> {
    std::mem::take(&mut self.client.events.input)
  }

  fn send_events(&self) -> &[(DeviceKey, SendEvents)] {
    &self.backend.input.send_events
  }
}

fn keys(input: &[Input]) -> Vec<(u32, bool)> {
  input
    .iter()
    .filter_map(|event| match event {
      Input::Key { key, pressed, .. } => Some((*key, *pressed)),
      _ => None,
    })
    .collect()
}

fn last_depressed(input: &[Input]) -> Option<u32> {
  input.iter().rev().find_map(|event| match event {
    Input::Modifiers { depressed } => Some(*depressed),
    _ => None,
  })
}

#[test]
fn keys_reach_clients_with_libinput_timestamps() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  fixture.received();
  fixture.input(
    "event1",
    123_456_789,
    DeviceInput::Key {
      code: KEY_A,
      pressed: true,
    },
  );
  fixture.input(
    "event1",
    123_556_789,
    DeviceInput::Key {
      code: KEY_A,
      pressed: false,
    },
  );
  let received = fixture.received();
  assert!(
    received.contains(&Input::Key {
      time: 123_456,
      key: KEY_A,
      pressed: true,
    }),
    "{received:?}"
  );
  assert!(received.contains(&Input::Key {
    time: 123_556,
    key: KEY_A,
    pressed: false,
  }));
}

#[test]
fn pointer_and_touch_events_reach_clients_with_libinput_timestamps() {
  let mut fixture = Fixture::new();
  fixture.add(device("event3", POINTER, 0));
  fixture.add(device("event5", TOUCHSCREEN, 0));
  fixture.input(
    "event3",
    7_000_000,
    DeviceInput::Motion {
      dx: 5.0,
      dy: 5.0,
      dx_unaccel: 5.0,
      dy_unaccel: 5.0,
    },
  );
  let received = fixture.received();
  let motion = received
    .iter()
    .find_map(|event| match event {
      Input::Motion { time, .. } => Some(*time),
      _ => None,
    })
    .expect("the window under the pointer gets motion");
  assert_eq!(motion, 7_000);
  fixture.input(
    "event3",
    7_100_000,
    DeviceInput::Button {
      code: BTN_LEFT,
      pressed: true,
    },
  );
  fixture.input(
    "event3",
    7_200_000,
    DeviceInput::Scroll {
      source: ScrollSource::Wheel,
      horizontal: None,
      vertical: Some(ScrollAxis {
        value: 15.0,
        v120: 120.0,
      }),
    },
  );
  fixture.input(
    "event5",
    7_300_000,
    DeviceInput::Touch {
      slot: 0,
      phase: TouchPhase::Down,
      position: Some((640.0, 360.0)),
    },
  );
  let received = fixture.received();
  assert!(received.contains(&Input::Button {
    time: 7_100,
    button: BTN_LEFT,
    pressed: true,
  }));
  assert!(
    received.contains(&Input::Axis {
      time: 7_200,
      value: 15.0,
    }),
    "natural scrolling is libinput's, the value passes unchanged: \
     {received:?}"
  );
  assert!(received.iter().any(|event| matches!(
    event,
    Input::TouchDown {
      time: 7_300,
      id: 0,
      ..
    }
  )));
}

#[test]
fn hotplugged_devices_appear_in_and_leave_list_devices() {
  let mut fixture = Fixture::new();
  assert!(fixture.list_devices().is_empty(), "no devices yet");
  let combo = device(
    "event4",
    Capabilities {
      keyboard: true,
      gesture: true,
      ..POINTER
    },
    2,
  );
  fixture.add(combo);
  let devices = fixture.list_devices();
  assert_eq!(devices.len(), 2, "{devices:?}");
  for (device, kind) in devices.iter().zip(["keyboard", "pointer"]) {
    assert_eq!(device["name"], "event4 device");
    assert_eq!(device["vendor"], 0x046d);
    assert_eq!(device["product"], 0xc52b);
    assert_eq!(device["type"], kind);
    assert_eq!(device["enabled"], true);
  }
  fixture.libinput([NativeEvent::Removed(key("event4"))]);
  assert!(fixture.list_devices().is_empty(), "unplugged");
}

#[test]
fn added_devices_are_configured_from_the_input_options() {
  let mut config = test_config();
  config.input.tap_to_click = false;
  config.input.disable_touchpad_while_mouse = true;
  let mut fixture = Fixture::with_config(config);
  fixture.add(device("event9", POINTER, 3));
  let (device, settings) = &fixture.backend.input.configured[0];
  assert_eq!(device, &key("event9"));
  assert!(settings.contains(&Setting::Tap(false)), "{settings:?}");
  assert_eq!(
    fixture.send_events(),
    [(key("event9"), SendEvents::DisabledOnExternalMouse)]
  );
}

#[test]
fn configure_device_disables_libinput_and_releases_held_keys() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  let keyboard = fixture.seat_device("event1", InputDeviceKind::Keyboard);
  fixture.key("event1", KEY_LEFTSHIFT, true);
  assert_eq!(last_depressed(&fixture.received()), Some(SHIFT));

  let reply = fixture.ipc(&configure_request(keyboard, false));
  assert_eq!(reply, r#"{"result":"ok"}"#);
  let received = fixture.received();
  assert_eq!(keys(&received), [(KEY_LEFTSHIFT, false)], "{received:?}");
  assert_eq!(
    fixture.send_events().last(),
    Some(&(key("event1"), SendEvents::Disabled))
  );
  // libinput's own release on the way to disabled is not handled again.
  fixture.key("event1", KEY_LEFTSHIFT, false);
  assert!(keys(&fixture.received()).is_empty());

  // Re-enabled, the keyboard state no longer holds Shift.
  fixture.ipc(&configure_request(keyboard, true));
  assert_eq!(last_depressed(&fixture.received()), Some(0));
  assert_eq!(
    fixture.send_events().last(),
    Some(&(key("event1"), SendEvents::Enabled))
  );
  fixture.key("event1", KEY_A, true);
  assert_eq!(keys(&fixture.received()), [(KEY_A, true)]);
}

#[test]
fn a_combined_device_keeps_sending_while_one_kind_is_enabled() {
  let mut fixture = Fixture::new();
  fixture.add(device(
    "event4",
    Capabilities {
      keyboard: true,
      ..POINTER
    },
    0,
  ));
  let keyboard = fixture.seat_device("event4", InputDeviceKind::Keyboard);
  let pointer = fixture.seat_device("event4", InputDeviceKind::Pointer);
  fixture.ipc(&configure_request(keyboard, false));
  assert_eq!(
    fixture.send_events().last(),
    Some(&(key("event4"), SendEvents::Enabled)),
    "the pointer still works"
  );
  fixture.ipc(&configure_request(pointer, false));
  assert_eq!(
    fixture.send_events().last(),
    Some(&(key("event4"), SendEvents::Disabled))
  );
}

#[test]
fn ctrl_alt_f2_asks_the_seat_to_switch_without_telling_the_client() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  fixture.received();
  for code in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F2] {
    fixture.key("event1", code, true);
  }
  assert_eq!(fixture.session.borrow().vt_switches, [2]);
  let received = keys(&fixture.received());
  assert!(!received.contains(&(KEY_F2, true)), "{received:?}");
}

#[test]
fn a_refused_vt_switch_passes_the_key_on() {
  let mut fixture = Fixture::new();
  fixture.session.borrow_mut().refuse_switch = true;
  fixture.add(device("event1", KEYBOARD, 0));
  for code in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F2] {
    fixture.key("event1", code, true);
  }
  assert_eq!(fixture.session.borrow().vt_switches, [2]);
  assert!(keys(&fixture.received()).contains(&(KEY_F2, true)));
}

#[test]
fn a_vt_switch_releases_held_input_and_switching_back_restores_it() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  fixture.add(device("event3", POINTER, 0));
  let keyboard = fixture.seat_device("event1", InputDeviceKind::Keyboard);
  let pointer = fixture.seat_device("event3", InputDeviceKind::Pointer);
  fixture.key("event1", KEY_A, true);
  fixture.input(
    "event3",
    2_000,
    DeviceInput::Button {
      code: BTN_LEFT,
      pressed: true,
    },
  );
  fixture.received();

  // libinput's releases during the suspend are dropped; the frontend
  // releases everything itself.
  fixture.backend.input.on_suspend = vec![
    NativeEvent::Input {
      device: key("event1"),
      time_usec: 3_000,
      input: DeviceInput::Key {
        code: KEY_A,
        pressed: false,
      },
    },
    NativeEvent::Removed(key("event1")),
    NativeEvent::Removed(key("event3")),
  ];
  fixture.seat(vec![SeatEvent::Disable]);
  assert!(!fixture.backend.active());
  assert!(fixture.backend.input.suspended);
  assert_eq!(fixture.session.borrow().acknowledged, 1);
  let received = fixture.received();
  assert_eq!(keys(&received), [(KEY_A, false)], "{received:?}");
  assert!(
    received.iter().any(|event| matches!(
      event,
      Input::Button {
        button: BTN_LEFT,
        pressed: false,
        ..
      }
    )),
    "{received:?}"
  );
  assert_eq!(fixture.list_devices().len(), 2, "the devices stay listed");

  // Nothing is read from libinput while paused.
  fixture.backend.input.pending.push_back(NativeEvent::Input {
    device: key("event1"),
    time_usec: 4_000,
    input: DeviceInput::Key {
      code: KEY_A,
      pressed: true,
    },
  });
  fixture.step();
  assert!(keys(&fixture.received()).is_empty());
  fixture.backend.input.pending.clear();

  // Switching back reopens the devices with their old ids and settings.
  let configured = fixture.backend.input.configured.len();
  fixture.backend.input.on_resume = vec![
    NativeEvent::Added(device("event1", KEYBOARD, 0)),
    NativeEvent::Added(device("event3", POINTER, 0)),
  ];
  fixture.seat(vec![SeatEvent::Enable]);
  assert!(fixture.backend.active());
  assert_eq!(fixture.backend.input.resumes, 1);
  assert_eq!(
    fixture.seat_device("event1", InputDeviceKind::Keyboard),
    keyboard
  );
  assert_eq!(
    fixture.seat_device("event3", InputDeviceKind::Pointer),
    pointer
  );
  assert_eq!(fixture.backend.input.configured.len(), configured + 1);
  assert!(!fixture.backend.input.leds.is_empty(), "LEDs are resent");
  // A release of a key held before the switch is not passed on, a new
  // press is.
  fixture.key("event1", KEY_A, false);
  assert!(keys(&fixture.received()).is_empty());
  fixture.key("event1", KEY_A, true);
  assert_eq!(keys(&fixture.received()), [(KEY_A, true)]);
}

#[test]
fn devices_unplugged_on_another_vt_are_gone_after_the_switch_back() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  fixture.add(device("event3", POINTER, 0));
  fixture.seat(vec![SeatEvent::Disable]);
  fixture.backend.input.on_resume =
    vec![NativeEvent::Added(device("event1", KEYBOARD, 0))];
  fixture.seat(vec![SeatEvent::Enable]);
  let devices = fixture.list_devices();
  assert_eq!(devices.len(), 1, "{devices:?}");
  assert_eq!(devices[0]["type"], "keyboard");
}

#[test]
fn a_device_disabled_before_a_switch_stays_disabled_after_it() {
  let mut fixture = Fixture::new();
  fixture.add(device("event3", POINTER, 0));
  let pointer = fixture.seat_device("event3", InputDeviceKind::Pointer);
  fixture.ipc(&configure_request(pointer, false));
  fixture.seat(vec![SeatEvent::Disable]);
  fixture.backend.input.on_resume =
    vec![NativeEvent::Added(device("event3", POINTER, 0))];
  fixture.seat(vec![SeatEvent::Enable]);
  assert_eq!(
    fixture.send_events().last(),
    Some(&(key("event3"), SendEvents::Disabled))
  );
  assert_eq!(fixture.list_devices()[0]["enabled"], false);
}

#[test]
fn caps_lock_lights_the_keyboard_leds() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  assert_eq!(fixture.backend.input.leds.last(), Some(&Leds::default()));
  fixture.key("event1", KEY_CAPSLOCK, true);
  fixture.key("event1", KEY_CAPSLOCK, false);
  assert_eq!(
    fixture.backend.input.leds.last(),
    Some(&Leds {
      caps_lock: true,
      ..Leds::default()
    })
  );
}

#[test]
fn input_of_unknown_devices_and_kinds_is_dropped() {
  let mut fixture = Fixture::new();
  fixture.add(device("event3", POINTER, 0));
  fixture.received();
  // A key from a device libinput never added, and one from a mouse,
  // which has no keyboard seat device.
  fixture.key("event7", KEY_A, true);
  fixture.key("event3", KEY_A, true);
  assert!(keys(&fixture.received()).is_empty());
  // Gestures belong to the pointer.
  assert_eq!(
    crate::udev::input_kind(&DeviceInput::Swipe(Gesture::Begin { fingers: 3 })),
    InputDeviceKind::Pointer
  );
}

#[test]
fn sigterm_saves_the_session_and_closes_the_seat() {
  let mut fixture = Fixture::new();
  fixture.add(device("event1", KEYBOARD, 0));
  // SAFETY: raise() directs the signal at this thread, which the
  // compositor's signal source has blocked, so it stays pending for the
  // signalfd instead of terminating the test process.
  assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
  let mut running = true;
  for _ in 0..100 {
    running = fixture.step();
    if !running {
      break;
    }
    std::thread::sleep(Duration::from_millis(5));
  }
  assert!(!running, "SIGTERM ends the loop");
  let Fixture {
    backend,
    session,
    base,
    ..
  } = fixture;
  backend.finish();
  assert!(base.with_extension("session.json").exists(), "saved");
  assert_eq!(
    Rc::strong_count(&session),
    1,
    "the backend let go of the seat"
  );
}
