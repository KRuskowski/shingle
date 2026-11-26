//! The native host driven through a fake seat and fake libinput, with a
//! real frontend, compositor and Wayland client on a private socket.

use std::{
  cell::RefCell,
  collections::VecDeque,
  path::PathBuf,
  rc::Rc,
  time::{Duration, Instant},
};

use shingle::{Compositor, Config, InputDeviceId, InputDeviceKind, WindowId};
use shingle_config::ShingleConfig;
use shingle_frontend::{
  Frontend,
  host::{Gesture, TouchPhase},
  keyboard::Leds,
};
use shingle_session::SessionStore;

use super::{Backend, Rendering};
use crate::{
  display::Display,
  gpu::{RenderFailure, tests::FakeRenderer},
  hotplug::{DrmEvent, tests::FakeMonitor},
  input::{
    Capabilities, DeviceDescription, DeviceInput, DeviceKey, NativeEvent,
    ScrollAxis, ScrollSource, SendEvents, Setting,
    tests::{FakeInput, POINTER, device, key},
  },
  kms::{
    CommitFlags, Connection, FORMAT_XRGB8888, MODIFIER_LINEAR, fake::FakeKms,
    in_formats::tests::blob as in_formats_blob,
  },
  outputs::OutputPolicy,
  scanout::{ScanoutChoice, ScanoutStatus, tests::FakeImporter},
  session::{SeatEvent, tests::FakeSession},
  test_client::{Input, Presented, TestClient, Toplevel},
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
  /// The fake card behind the display.
  card: Rc<RefCell<FakeKms>>,
  /// Failures the renderer returns, one per frame, before drawing again.
  failures: Rc<RefCell<VecDeque<RenderFailure>>>,
  /// How many frames the renderers drew.
  frames: Rc<RefCell<u32>>,
  /// How many renderers the factory made after the first.
  recreated: Rc<RefCell<u32>>,
  /// The output of every frame drawn, in order.
  drawn: Rc<RefCell<Vec<String>>>,
  /// The renderers' GPU side.
  gpu: FakeGpu,
  client: TestClient,
  toplevel: Toplevel,
  base: PathBuf,
  /// The fake DRM hotplug monitor.
  monitor: FakeMonitor,
  /// The time every step runs at. It stands still, so a page flip left
  /// pending never times out (1 s) however slowly the test runs.
  now: Instant,
}

/// A rendering setup with fake renderers that share `failures` and
/// `frames`, counting recreations in `recreated`.
pub(crate) fn fake_rendering(
  size: (u32, u32),
  failures: &Rc<RefCell<VecDeque<RenderFailure>>>,
  frames: &Rc<RefCell<u32>>,
  recreated: &Rc<RefCell<u32>>,
) -> Rendering {
  let drawn = Rc::new(RefCell::new(Vec::new()));
  let gpu = FakeGpu::default();
  drawing_rendering(size, failures, frames, recreated, &drawn, &gpu)
}

/// The GPU side the fake renderers share, and the scanout buffers they
/// drew into.
#[derive(Clone, Default)]
struct FakeGpu {
  importer: Rc<RefCell<FakeImporter>>,
  drawn_into: Rc<RefCell<Vec<u64>>>,
}

/// [`fake_rendering`] that records the output of every frame in `drawn`,
/// with `gpu` as every renderer's GPU side.
fn drawing_rendering(
  size: (u32, u32),
  failures: &Rc<RefCell<VecDeque<RenderFailure>>>,
  frames: &Rc<RefCell<u32>>,
  recreated: &Rc<RefCell<u32>>,
  drawn: &Rc<RefCell<Vec<String>>>,
  gpu: &FakeGpu,
) -> Rendering {
  let renderer = FakeRenderer {
    size,
    failures: Rc::clone(failures),
    frames: Rc::clone(frames),
    drawn: Rc::clone(drawn),
    importer: Rc::clone(&gpu.importer),
    drawn_into: Rc::clone(&gpu.drawn_into),
  };
  let (failures, frames, recreated, drawn, gpu) = (
    Rc::clone(failures),
    Rc::clone(frames),
    Rc::clone(recreated),
    Rc::clone(drawn),
    gpu.clone(),
  );
  Rendering {
    renderer: Box::new(renderer),
    factory: Box::new(move |size| {
      *recreated.borrow_mut() += 1;
      Ok(Box::new(FakeRenderer {
        size,
        failures: Rc::clone(&failures),
        frames: Rc::clone(&frames),
        drawn: Rc::clone(&drawn),
        importer: Rc::clone(&gpu.importer),
        drawn_into: Rc::clone(&gpu.drawn_into),
      }))
    }),
  }
}

impl Fixture {
  fn new() -> Self {
    Self::with_config(test_config())
  }

  fn with_config(config: ShingleConfig) -> Self {
    Self::with_card(config, FakeKms::single_output())
  }

  fn with_card(config: ShingleConfig, card: FakeKms) -> Self {
    Self::with_scanout(config, card, None)
  }

  /// A fixture whose primary plane scans out XRGB8888 with LINEAR and
  /// whose renderers render to LINEAR, trying zero-copy as `choice` says.
  fn zero_copy(choice: ScanoutChoice) -> Self {
    let mut card = FakeKms::single_output();
    card.in_formats = Some(in_formats_blob(
      &[FORMAT_XRGB8888],
      &[(1, 0, MODIFIER_LINEAR)],
    ));
    Self::with_scanout(test_config(), card, Some(choice))
  }

  /// [`Fixture::with_card`], with the display set to `choice` and the
  /// renderers rendering to LINEAR if it is given.
  fn with_scanout(
    config: ShingleConfig,
    card: FakeKms,
    choice: Option<ScanoutChoice>,
  ) -> Self {
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
    let card = card.shared();
    let mut display = Display::prepare(
      Box::new(Rc::clone(&card)),
      OutputPolicy::from_config(&config),
    )
    .expect("the display passes its test");
    let gpu = FakeGpu::default();
    if let Some(choice) = choice {
      display.set_scanout_choice(choice);
      gpu.importer.borrow_mut().modifiers = Some(vec![MODIFIER_LINEAR]);
    }
    let failures = Rc::new(RefCell::new(VecDeque::new()));
    let frames = Rc::new(RefCell::new(0));
    let recreated = Rc::new(RefCell::new(0));
    let drawn = Rc::new(RefCell::new(Vec::new()));
    let size = display.size().unwrap_or(crate::OUTPUT_SIZE);
    let rendering =
      drawing_rendering(size, &failures, &frames, &recreated, &drawn, &gpu);
    let mut backend = Backend::new(
      frontend,
      Rc::clone(&session),
      FakeInput::default(),
      config.input,
      display,
      rendering,
    )
    .expect("the backend starts");
    let monitor = FakeMonitor::default();
    backend.set_device_monitor(Box::new(monitor.clone()));
    let compositor = backend.frontend_mut().compositor_mut();
    let mut client = TestClient::connect(compositor, &base);
    client.seat(compositor);
    let toplevel = client.toplevel(compositor, "native", "native");
    let mut fixture = Self {
      backend,
      session,
      card,
      failures,
      frames,
      recreated,
      drawn,
      gpu,
      client,
      toplevel,
      base,
      monitor,
      now: Instant::now(),
    };
    fixture.step();
    fixture
  }

  /// Runs one turn of the loop and lets the client read what it got.
  fn step(&mut self) -> bool {
    let running = self.backend.step_at(self.now);
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

  /// Whether a flip waits for its event.
  fn flip_pending(&self) -> bool {
    self.card.borrow().flip_pending()
  }

  /// Completes the pending page flip at `time` and runs a turn.
  fn flip(&mut self, time: Duration, sequence: u32) {
    self.card.borrow_mut().complete_flip(time, sequence);
    self.step();
  }

  /// The client asks for a frame callback and feedback and commits a new
  /// buffer; then a turn runs.
  fn client_frame(&mut self) {
    let surface = self.toplevel.surface.clone();
    let size = self.toplevel.size;
    let compositor = self.backend.frontend_mut().compositor_mut();
    self.client.frame(compositor, &surface, size);
    self.step();
  }

  /// The flags of every accepted commit that was not a test.
  fn commit_flags(&self) -> Vec<CommitFlags> {
    let card = self.card.borrow();
    card
      .real_commits()
      .into_iter()
      .map(|(_, flags)| flags)
      .collect()
  }

  /// Delivers a DRM hotplug event and runs a turn.
  fn hotplug(&mut self) {
    self
      .monitor
      .events
      .borrow_mut()
      .push(DrmEvent::Hotplug(None));
    self.step();
  }

  /// The client's window and the core output it is on, with the windows
  /// of that output's current workspace.
  fn window_place(&self) -> (WindowId, String, Vec<WindowId>) {
    let compositor = self.backend.frontend().compositor();
    let window = compositor.focused_window().expect("the window is focused");
    let core = compositor.core();
    let output = core.window_location(window).unwrap().output.clone();
    let workspace = core.current_workspace(&output).unwrap();
    let windows = core.tree(&output, workspace).unwrap().windows();
    (window, output.0, windows)
  }

  /// The CRTC's `ACTIVE` as the card has it now.
  fn crtc_active(&self) -> u64 {
    let card = self.card.borrow();
    card.value(card.crtc_id.0, "ACTIVE")
  }
}

const FLIP: CommitFlags =
  CommitFlags(CommitFlags::NONBLOCK.0 | CommitFlags::PAGE_FLIP_EVENT.0);
const MODESET: CommitFlags = CommitFlags::ALLOW_MODESET;
/// `wp_presentation_feedback.kind` `vsync | hw_clock | hw_completion`.
const FLIP_FEEDBACK: u32 = 0x1 | 0x2 | 0x4;
/// `wp_presentation_feedback.kind` `zero_copy`.
const ZERO_COPY: u32 = 0x8;

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

#[test]
fn the_first_frame_is_drawn_after_a_tested_modeset() {
  let fixture = Fixture::new();
  let card = fixture.card.borrow();
  let flags: Vec<_> = card.commits.iter().map(|(_, flags, _)| *flags).collect();
  let test = CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET;
  // prepare's test, start's test and modeset, then the window's frame.
  assert_eq!(flags, vec![test, test, MODESET, FLIP]);
  drop(card);
  assert_eq!(*fixture.frames.borrow(), 1);
  assert!(fixture.flip_pending());
}

#[test]
fn clients_see_the_connector_as_their_output() {
  let mut fixture = Fixture::new();
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.output(compositor);
  assert_eq!(
    fixture.client.events.output_name.as_deref(),
    Some("HDMI-A-1")
  );
  let info = fixture.backend.frontend().compositor().output_info();
  assert_eq!(info.refresh_mhz, 60_000);
  assert_eq!(info.physical_size_mm, (600, 340));
}

#[test]
fn frame_callbacks_and_presentation_carry_the_page_flip_time() {
  let mut fixture = Fixture::new();
  fixture.flip(Duration::from_secs(1), 1);
  fixture.client_frame();
  assert!(fixture.flip_pending(), "the commit was drawn and flipped");
  assert!(
    fixture.client.events.frames.is_empty(),
    "no frame callback before the flip completes"
  );
  assert!(fixture.client.events.presented.is_empty());
  fixture.flip(Duration::new(1234, 567_891_000), 42);
  assert_eq!(fixture.client.events.frames, [1_234_567]);
  assert_eq!(
    fixture.client.events.presented,
    [Presented {
      time: (1234, 567_891_000),
      refresh: 16_666_666,
      seq: 42,
      flags: FLIP_FEEDBACK,
    }]
  );
}

#[test]
fn frames_wait_for_the_previous_flip() {
  let mut fixture = Fixture::new();
  fixture.flip(Duration::from_secs(1), 1);
  fixture.client_frame();
  let drawn = *fixture.frames.borrow();
  let flips = fixture.commit_flags().len();
  // The client commits again while the flip is pending.
  fixture.client_frame();
  fixture.step();
  assert_eq!(*fixture.frames.borrow(), drawn, "nothing drawn meanwhile");
  assert_eq!(fixture.commit_flags().len(), flips, "nothing committed");
  assert!(fixture.client.events.frames.is_empty());
  // Both callbacks fire with the flip, as the compositor sends every
  // pending callback of a visible surface when a frame is shown.
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(fixture.client.events.frames, [2_000, 2_000]);
  assert_eq!(
    *fixture.frames.borrow(),
    drawn + 1,
    "the next frame follows"
  );
  assert!(fixture.flip_pending());
  fixture.flip(Duration::from_secs(3), 3);
  assert_eq!(fixture.client.events.frames.len(), 2, "none was asked for");
  assert_eq!(fixture.client.events.presented.len(), 2);
}

#[test]
fn output_power_management_turns_the_crtc_off_and_on() {
  let mut fixture = Fixture::new();
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  let output = fixture.client.output(compositor);
  // The first frame's flip is still pending, so the change waits for it.
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.set_power(compositor, &output, false);
  fixture.step();
  assert_eq!(fixture.crtc_active(), 1, "not while a flip is pending");
  fixture.flip(Duration::from_secs(1), 1);
  assert_eq!(fixture.crtc_active(), 0);
  assert!(!fixture.backend.display().dpms_on());
  assert_eq!(fixture.client.events.power_modes.last(), Some(&0));
  let drawn = *fixture.frames.borrow();
  fixture.client_frame();
  assert_eq!(
    *fixture.frames.borrow(),
    drawn,
    "nothing is drawn while off"
  );
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.set_power(compositor, &output, true);
  fixture.step();
  assert_eq!(fixture.crtc_active(), 1);
  assert!(fixture.backend.display().dpms_on());
  let flags = fixture.commit_flags();
  assert_eq!(
    flags[flags.len() - 2..],
    [MODESET, FLIP],
    "on, then a frame"
  );
  assert_eq!(fixture.client.events.power_modes.last(), Some(&1));
}

#[test]
fn a_vt_switch_with_a_flip_pending_resumes_with_a_fresh_modeset() {
  let mut fixture = Fixture::new();
  assert!(fixture.flip_pending());
  fixture.seat(vec![SeatEvent::Disable]);
  assert_eq!(
    fixture.backend.display().state(),
    crate::pacing::State::Paused
  );
  // The flip completes while another session has the display; it is
  // stale and presents nothing.
  fixture.flip(Duration::from_secs(1), 1);
  let before = fixture.card.borrow().commits.len();
  fixture.client_frame();
  assert_eq!(
    fixture.card.borrow().commits.len(),
    before,
    "nothing while away"
  );
  assert!(fixture.client.events.presented.is_empty());
  fixture.seat(vec![SeatEvent::Enable]);
  let card = fixture.card.borrow();
  let flags: Vec<_> = card.commits[before..]
    .iter()
    .map(|(_, flags, _)| *flags)
    .collect();
  drop(card);
  assert_eq!(
    flags,
    vec![CommitFlags::TEST_ONLY | MODESET, MODESET, FLIP],
    "a tested modeset, then the first frame"
  );
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(fixture.client.events.frames, [2_000]);
}

#[test]
fn a_lost_gpu_is_replaced_and_frames_continue() {
  let mut fixture = Fixture::new();
  fixture.flip(Duration::from_secs(1), 1);
  fixture
    .failures
    .borrow_mut()
    .push_back(RenderFailure::Lost("test".into()));
  fixture.client_frame();
  assert_eq!(*fixture.recreated.borrow(), 1, "a new GPU context");
  assert!(!fixture.flip_pending(), "the failed frame was not flipped");
  fixture.step();
  assert!(fixture.flip_pending(), "the new renderer draws the frame");
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(fixture.client.events.frames, [2_000]);
  assert!(fixture.backend.fatal().is_none());
}

#[test]
fn a_gpu_that_keeps_timing_out_is_replaced() {
  let mut fixture = Fixture::new();
  fixture.flip(Duration::from_secs(1), 1);
  fixture
    .failures
    .borrow_mut()
    .extend([RenderFailure::Timeout, RenderFailure::Timeout]);
  fixture.client_frame();
  fixture.step();
  assert_eq!(
    *fixture.recreated.borrow(),
    0,
    "two slow frames are retried"
  );
  fixture.step();
  assert!(fixture.flip_pending(), "the third attempt drew");
  fixture.flip(Duration::from_secs(2), 2);
  fixture
    .failures
    .borrow_mut()
    .extend(std::iter::repeat_n(RenderFailure::Timeout, 3));
  fixture.client_frame();
  fixture.step();
  fixture.step();
  assert_eq!(
    *fixture.recreated.borrow(),
    1,
    "three in a row count as hung"
  );
}

#[test]
fn a_third_gpu_loss_within_a_minute_saves_and_exits() {
  let mut fixture = Fixture::new();
  fixture.flip(Duration::from_secs(1), 1);
  fixture
    .failures
    .borrow_mut()
    .extend([0, 1, 2].map(|n| RenderFailure::Lost(format!("loss {n}"))));
  fixture.client_frame();
  let mut running = true;
  for _ in 0..3 {
    running = fixture.step();
  }
  assert!(!running, "the loop ends");
  assert_eq!(*fixture.recreated.borrow(), 2);
  let fatal = fixture.backend.fatal().expect("a fatal failure").clone();
  assert_eq!(fatal.0, "the GPU failed 3 times within 60 s");
  let Fixture { backend, base, .. } = fixture;
  backend.finish();
  assert!(base.with_extension("session.json").exists(), "saved");
}

#[test]
fn no_connected_output_runs_clients_without_drawing() {
  let mut card = FakeKms::single_output();
  card.disconnect();
  let mut fixture = Fixture::with_card(test_config(), card);
  fixture.client_frame();
  assert!(
    fixture.card.borrow().commits.is_empty(),
    "nothing is committed"
  );
  assert_eq!(*fixture.frames.borrow(), 0);
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.output(compositor);
  assert_eq!(
    fixture.client.events.output_name.as_deref(),
    Some("HEADLESS-1")
  );
  // A monitor plugged in on another VT is lit on the way back.
  fixture.seat(vec![SeatEvent::Disable]);
  fixture.card.borrow_mut().connection = Connection::Connected;
  fixture.seat(vec![SeatEvent::Enable]);
  assert_eq!(fixture.commit_flags(), vec![MODESET, FLIP]);
  let info = fixture.backend.frontend().compositor().output_info();
  assert_eq!(info.name, "HDMI-A-1");
}

#[test]
fn traced_flips_are_logged_with_their_sequence_and_time() {
  let info = shingle::PresentInfo {
    time: Duration::new(12, 3_000),
    refresh_ns: 16_666_666,
    seq: 7,
    flags: shingle::PresentationFlags::Vsync,
  };
  assert_eq!(super::flip_line(&info), "page flip seq=7 at 12.000003000 s");
  let torn = shingle::PresentInfo {
    flags: shingle::PresentationFlags::HwCompletion,
    ..info
  };
  assert_eq!(
    super::flip_line(&torn),
    "page flip seq=7 at 12.000003000 s async"
  );
  let mut fixture = Fixture::new();
  fixture.backend.set_trace_flips(true);
  fixture.flip(Duration::from_secs(1), 1);
  assert_eq!(fixture.client.events.frames.len(), 0, "none was asked for");
}

#[test]
fn a_monitor_plugged_in_later_is_lit_without_a_vt_switch() {
  let mut card = FakeKms::single_output();
  card.disconnect();
  let mut fixture = Fixture::with_card(test_config(), card);
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.output(compositor);
  assert_eq!(
    fixture.client.events.output_name.as_deref(),
    Some("HEADLESS-1")
  );
  fixture.card.borrow_mut().connection = Connection::Connected;
  fixture.hotplug();
  assert_eq!(fixture.commit_flags(), vec![MODESET, FLIP]);
  // The output global was replaced; binding it again gives the connector.
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.output(compositor);
  assert_eq!(
    fixture.client.events.output_name.as_deref(),
    Some("HDMI-A-1")
  );
  let (_, output, _) = fixture.window_place();
  assert_eq!(output, "HDMI-A-1", "the core output follows the connector");
}

#[test]
fn unplugging_the_lit_monitor_moves_to_the_next_with_the_tree_intact() {
  let mut card = FakeKms::single_output();
  let mut dp = crate::kms::fake::connector_info(9, "DP-2");
  dp.modes = vec![crate::kms::fake::mode(2560, 1440, 60, true)];
  card.add_connector(dp);
  let mut fixture = Fixture::with_card(test_config(), card);
  let (window, output, windows) = fixture.window_place();
  assert_eq!(
    (output.as_str(), windows.clone()),
    ("HDMI-A-1", vec![window])
  );
  fixture.flip(Duration::from_secs(1), 1);
  fixture.card.borrow_mut().disconnect();
  fixture.hotplug();
  let compositor = fixture.backend.frontend().compositor();
  assert_eq!(compositor.output_info().name, "DP-2");
  assert_eq!(compositor.output().width, 2560);
  assert_eq!(fixture.window_place(), (window, "DP-2".into(), windows));
  assert!(fixture.flip_pending(), "the new output shows a frame");
}

#[test]
fn with_every_monitor_unplugged_clients_live_and_the_picture_returns() {
  let mut fixture = Fixture::new();
  fixture.flip(Duration::from_secs(1), 1);
  let place = fixture.window_place();
  fixture.card.borrow_mut().disconnect();
  fixture.hotplug();
  let commits = fixture.card.borrow().commits.len();
  let frames = *fixture.frames.borrow();
  // The client keeps running and committing; nothing is drawn or
  // committed, and the compositor keeps its output and windows.
  fixture.client_frame();
  fixture.step();
  assert_eq!(fixture.card.borrow().commits.len(), commits);
  assert_eq!(*fixture.frames.borrow(), frames);
  let compositor = fixture.backend.frontend().compositor();
  assert_eq!(compositor.output_info().name, "HDMI-A-1");
  assert_eq!(compositor.output().width, 1280);
  assert_eq!(fixture.window_place(), place);
  fixture.card.borrow_mut().connection = Connection::Connected;
  fixture.hotplug();
  assert!(fixture.flip_pending(), "the picture is back");
  assert_eq!(*fixture.frames.borrow(), frames + 1);
  assert_eq!(fixture.window_place(), place);
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(fixture.client.events.frames.len(), 1, "the frame callback");
}

#[test]
fn output_management_modes_are_tested_before_they_are_set() {
  let mut fixture = Fixture::new();
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  let manager = fixture.client.output_manager(compositor);
  let sizes: Vec<_> = fixture.client.events.modes.iter().map(|m| m.1).collect();
  assert_eq!(sizes, [(1280, 720), (1920, 1080), (1024, 768)]);
  // A test of a mode the driver refuses fails and commits nothing.
  fixture.card.borrow_mut().reject_width(1024);
  let before = fixture.commit_flags();
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.request_mode(compositor, &manager, 2, true);
  fixture.step();
  assert_eq!(fixture.client.events.configurations, ["failed"]);
  assert_eq!(fixture.commit_flags(), before);
  // Applying a mode waits for the pending flip, then tests and sets it.
  assert!(fixture.flip_pending());
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.request_mode(compositor, &manager, 1, false);
  fixture.step();
  assert_eq!(fixture.client.events.configurations, ["failed"], "waiting");
  let tests = fixture.card.borrow().commits.len();
  fixture.flip(Duration::from_secs(1), 1);
  assert_eq!(
    fixture.client.events.configurations,
    ["failed", "succeeded"]
  );
  let card = fixture.card.borrow();
  let flags: Vec<_> = card.commits[tests..].iter().map(|c| c.1).collect();
  drop(card);
  let test = CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET;
  assert_eq!(&flags[..3], [test, test, MODESET]);
  let compositor = fixture.backend.frontend().compositor();
  assert_eq!(compositor.output().width, 1920);
  assert_eq!(compositor.output_info().current_mode, Some(1));
  assert_eq!(fixture.backend.output_size, (1920, 1080));
}

#[test]
fn a_hotplug_during_a_pending_flip_waits_for_the_flip() {
  let mut card = FakeKms::single_output();
  let mut dp = crate::kms::fake::connector_info(9, "DP-2");
  dp.modes = vec![crate::kms::fake::mode(2560, 1440, 60, true)];
  card.add_connector(dp);
  let mut fixture = Fixture::with_card(test_config(), card);
  assert!(fixture.flip_pending());
  fixture.card.borrow_mut().disconnect();
  fixture.hotplug();
  let compositor = fixture.backend.frontend().compositor();
  assert_eq!(compositor.output_info().name, "HDMI-A-1", "not yet");
  fixture.flip(Duration::from_secs(1), 1);
  let compositor = fixture.backend.frontend().compositor();
  assert_eq!(compositor.output_info().name, "DP-2");
  assert!(fixture.flip_pending(), "DP-2 shows a frame");
}

/// A card with HDMI-A-1 (1280x720) and DP-1 (1920x1080) on CRTCs of their
/// own, both connected unless `dp_connected` is false, with DP-1's CRTC.
fn two_monitors(dp_connected: bool) -> (FakeKms, crate::kms::CrtcId) {
  let mut card = FakeKms::single_output();
  let mut dp = crate::kms::fake::connector_info(20, "DP-1");
  dp.modes = vec![crate::kms::fake::mode(1920, 1080, 60, true)];
  if !dp_connected {
    dp.connection = Connection::Disconnected;
  }
  let crtc = card.add_pipe(dp);
  (card, crtc)
}

impl Fixture {
  /// The compositor's outputs, by name, in add order.
  fn output_names(&self) -> Vec<String> {
    self
      .backend
      .frontend()
      .compositor()
      .outputs()
      .into_iter()
      .map(|view| view.handle.name().to_owned())
      .collect()
  }

  /// Completes the flip pending on `crtc` and runs a turn.
  fn flip_on(&mut self, crtc: crate::kms::CrtcId, time: Duration, seq: u32) {
    self.card.borrow_mut().complete_flip_on(crtc, time, seq);
    self.step();
  }

  /// Completes every pending flip and runs a turn.
  fn flip_all(&mut self, time: Duration) {
    let pending: Vec<_> = self.card.borrow().pending.iter().copied().collect();
    for crtc in pending {
      self.card.borrow_mut().complete_flip_on(crtc, time, 1);
    }
    self.step();
  }

  /// Delivers a DRM hotplug event and completes flips until the hotplug,
  /// which waits for them, was followed.
  fn hotplug_settled(&mut self, time: Duration) {
    self.hotplug();
    for _ in 0..3 {
      self.flip_all(time);
    }
  }

  /// `ACTIVE` of `crtc` as the card has it now.
  fn active_of(&self, crtc: crate::kms::CrtcId) -> u64 {
    self.card.borrow().value(crtc.0, "ACTIVE")
  }
}

#[test]
fn two_monitors_are_two_outputs_each_paced_by_its_own_flips() {
  let (card, dp) = two_monitors(true);
  let mut fixture = Fixture::with_card(test_config(), card);
  assert_eq!(fixture.output_names(), ["HDMI-A-1", "DP-1"]);
  let compositor = fixture.backend.frontend().compositor();
  let view = compositor.output_view(&shingle::OutputHandle::new("DP-1"));
  assert_eq!(
    view.map(|view| view.rect),
    Some(shingle_core::Rect::new(1280, 0, 1920, 1080)),
    "placed automatically to the right"
  );
  let mut drawn = fixture.drawn.borrow().clone();
  drawn.sort();
  assert_eq!(drawn, ["DP-1", "HDMI-A-1"], "a first frame on each");
  let hdmi = fixture.card.borrow().crtc_id;
  assert_eq!(
    fixture.card.borrow().pending.len(),
    2,
    "two flips in flight"
  );
  // The window is on HDMI-A-1: DP-1's flip sends it nothing.
  fixture.client_frame();
  fixture.flip_on(dp, Duration::from_secs(2), 11);
  assert!(fixture.client.events.frames.is_empty());
  fixture.flip_on(hdmi, Duration::from_secs(3), 20);
  assert_eq!(fixture.client.events.frames, [3_000]);
  assert_eq!(fixture.client.events.presented[0].seq, 20);
  assert_eq!(
    fixture.drawn.borrow().last().map(String::as_str),
    Some("HDMI-A-1"),
    "the committed frame is drawn on HDMI-A-1"
  );
}

#[test]
fn monitors_plugged_in_and_out_add_and_remove_outputs() {
  let (card, _) = two_monitors(false);
  let mut fixture = Fixture::with_card(test_config(), card);
  assert_eq!(fixture.output_names(), ["HDMI-A-1"]);
  let (window, _, _) = fixture.window_place();
  fixture
    .card
    .borrow_mut()
    .set_connection("DP-1", Connection::Connected);
  fixture.hotplug_settled(Duration::from_secs(1));
  assert_eq!(fixture.output_names(), ["HDMI-A-1", "DP-1"]);
  assert!(fixture.drawn.borrow().iter().any(|name| name == "DP-1"));
  // Unplugging HDMI-A-1 moves its window to DP-1 ...
  fixture.card.borrow_mut().disconnect();
  fixture.hotplug_settled(Duration::from_secs(2));
  assert_eq!(fixture.output_names(), ["DP-1"]);
  let compositor = fixture.backend.frontend().compositor();
  let place = compositor.core().window_location(window).unwrap();
  assert_eq!(place.output.0, "DP-1");
  // ... and replugging it brings the window back, under `return`.
  fixture.card.borrow_mut().connection = Connection::Connected;
  fixture.hotplug_settled(Duration::from_secs(3));
  assert_eq!(fixture.output_names(), ["DP-1", "HDMI-A-1"]);
  let compositor = fixture.backend.frontend().compositor();
  let place = compositor.core().window_location(window).unwrap();
  assert_eq!(place.output.0, "HDMI-A-1");
}

#[test]
fn a_hotplug_waits_for_every_flip_and_starts_no_frame_meanwhile() {
  let (card, dp) = two_monitors(true);
  let mut fixture = Fixture::with_card(test_config(), card);
  let hdmi = fixture.card.borrow().crtc_id;
  fixture.flip_on(hdmi, Duration::from_secs(1), 1);
  fixture.card.borrow_mut().disconnect();
  fixture.hotplug();
  let drawn = fixture.drawn.borrow().len();
  fixture.client_frame();
  assert_eq!(fixture.output_names(), ["HDMI-A-1", "DP-1"], "not yet");
  assert_eq!(fixture.drawn.borrow().len(), drawn, "no frame meanwhile");
  fixture.flip_on(dp, Duration::from_secs(2), 2);
  assert_eq!(fixture.output_names(), ["DP-1"]);
}

#[test]
fn a_monitor_without_a_free_crtc_is_a_disabled_head() {
  let mut card = FakeKms::single_output();
  card.add_connector(crate::kms::fake::connector_info(9, "DP-2"));
  let mut fixture = Fixture::with_card(test_config(), card);
  let compositor = fixture.backend.frontend().compositor();
  assert_eq!(fixture.output_names(), ["HDMI-A-1"]);
  assert_eq!(
    compositor.disabled_outputs(),
    [shingle::OutputHandle::new("DP-2")]
  );
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  let _manager = fixture.client.output_manager(compositor);
  assert_eq!(
    fixture.client.heads(),
    std::collections::BTreeMap::from([
      ("DP-2".to_owned(), false),
      ("HDMI-A-1".to_owned(), true)
    ])
  );
  // Unplugging HDMI-A-1 frees the CRTC for DP-2.
  fixture.flip(Duration::from_secs(1), 1);
  fixture.card.borrow_mut().disconnect();
  fixture.hotplug();
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.roundtrip(compositor);
  assert_eq!(fixture.output_names(), ["DP-2"]);
  assert!(
    fixture
      .backend
      .frontend()
      .compositor()
      .disabled_outputs()
      .is_empty()
  );
  assert_eq!(
    fixture.client.heads(),
    std::collections::BTreeMap::from([("DP-2".to_owned(), true)])
  );
}

#[test]
fn output_management_turns_one_monitor_off_and_on_again() {
  let (card, dp) = two_monitors(true);
  let mut fixture = Fixture::with_card(test_config(), card);
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  let manager = fixture.client.output_manager(compositor);
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture
    .client
    .configure_heads(compositor, &manager, &["DP-1"]);
  fixture.step();
  assert!(
    fixture.client.events.configurations.is_empty(),
    "waiting for the flips"
  );
  fixture.flip_all(Duration::from_secs(1));
  assert_eq!(fixture.client.events.configurations, ["succeeded"]);
  assert_eq!(fixture.output_names(), ["HDMI-A-1"]);
  assert_eq!(fixture.active_of(dp), 0, "DP-1's CRTC is off");
  assert_eq!(
    fixture.client.heads(),
    std::collections::BTreeMap::from([
      ("DP-1".to_owned(), false),
      ("HDMI-A-1".to_owned(), true)
    ])
  );
  // A hotplug leaves it off, as output management turned it off.
  fixture.hotplug_settled(Duration::from_secs(2));
  assert_eq!(fixture.output_names(), ["HDMI-A-1"]);
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.configure_heads(compositor, &manager, &[]);
  fixture.flip_all(Duration::from_secs(3));
  fixture.step();
  assert_eq!(
    fixture.client.events.configurations,
    ["succeeded", "succeeded"]
  );
  assert_eq!(fixture.output_names(), ["HDMI-A-1", "DP-1"]);
  assert_eq!(fixture.active_of(dp), 1);
}

#[test]
fn power_management_turns_one_monitor_off() {
  let (card, dp) = two_monitors(true);
  let mut fixture = Fixture::with_card(test_config(), card);
  let hdmi = fixture.card.borrow().crtc_id;
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  let output = fixture.client.output_named(compositor, "DP-1");
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.set_power(compositor, &output, false);
  fixture.step();
  assert_eq!(fixture.active_of(dp), 1, "not while its flip is pending");
  fixture.flip_on(dp, Duration::from_secs(1), 1);
  assert_eq!(fixture.active_of(dp), 0);
  assert_eq!(fixture.active_of(hdmi), 1, "HDMI-A-1 stays on");
  // HDMI-A-1 keeps drawing.
  fixture.flip_on(hdmi, Duration::from_secs(1), 1);
  fixture.client_frame();
  assert!(fixture.card.borrow().pending.contains(&hdmi));
  let compositor = fixture.backend.frontend_mut().compositor_mut();
  fixture.client.set_power(compositor, &output, true);
  fixture.step();
  assert_eq!(fixture.active_of(dp), 1);
}

#[test]
fn absolute_devices_map_to_their_output_or_the_whole_layout() {
  let (card, _) = two_monitors(true);
  let mut config = test_config();
  config.input_devices.insert(
    "event7 device".into(),
    shingle_config::InputDeviceConfig {
      output: Some("DP-1".into()),
    },
  );
  let mut fixture = Fixture::with_card(config, card);
  fixture.add(device("event7", POINTER, 0));
  fixture.add(device("event8", POINTER, 0));
  // libinput transforms to the first output's 1280x720 pixels: the middle
  // of the device is the middle of DP-1.
  let middle = DeviceInput::MotionAbsolute { x: 640.0, y: 360.0 };
  fixture.input("event7", 1_000, middle);
  assert_eq!(fixture.backend.translator.pointer, (1280.0 + 960.0, 540.0));
  // An unmapped device spans the layout, 3200x1080.
  fixture.input("event8", 2_000, middle);
  assert_eq!(fixture.backend.translator.pointer, (1600.0, 540.0));
  // Relative motion crosses from one output to the other.
  fixture.input(
    "event8",
    3_000,
    DeviceInput::Motion {
      dx: -400.0,
      dy: 0.0,
      dx_unaccel: -400.0,
      dy_unaccel: 0.0,
    },
  );
  assert_eq!(fixture.backend.translator.pointer, (1200.0, 540.0));
}

/// The client's presentation flags of every frame presented so far.
fn presented_flags(fixture: &Fixture) -> Vec<u32> {
  let presented = &fixture.client.events.presented;
  presented.iter().map(|presented| presented.flags).collect()
}

#[test]
fn frames_are_drawn_into_gbm_buffers_and_presented_as_zero_copy() {
  let mut fixture = Fixture::zero_copy(ScanoutChoice::Auto);
  let test = CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET;
  let flags: Vec<_> = fixture
    .card
    .borrow()
    .commits
    .iter()
    .map(|(_, flags, _)| *flags)
    .collect();
  // prepare's test, start's test and modeset, the tested blocking flip to
  // a cleared GBM buffer, then the window's frame.
  let blocking = CommitFlags::default();
  assert_eq!(
    flags,
    vec![test, test, MODESET, CommitFlags::TEST_ONLY, blocking, FLIP]
  );
  assert_eq!(
    fixture.backend.display().scanout_status("HDMI-A-1"),
    Some(ScanoutStatus::ZeroCopy(MODIFIER_LINEAR))
  );
  assert_eq!(fixture.gpu.importer.borrow().prepared.len(), 1);
  assert_eq!(*fixture.gpu.drawn_into.borrow(), [2]);
  assert!(fixture.card.borrow().buffers.is_empty(), "no dumb buffers");
  fixture.flip(Duration::from_secs(1), 1);
  fixture.client_frame();
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(*fixture.gpu.drawn_into.borrow(), [2, 3]);
  assert_eq!(presented_flags(&fixture), [FLIP_FEEDBACK | ZERO_COPY]);
}

#[test]
fn shingle_scanout_copy_reads_frames_back() {
  let mut fixture = Fixture::zero_copy(ScanoutChoice::Copy);
  assert_eq!(
    fixture.backend.display().scanout_status("HDMI-A-1"),
    Some(ScanoutStatus::Copy(
      "SHINGLE_SCANOUT=copy forces copy scanout".into()
    ))
  );
  fixture.flip(Duration::from_secs(1), 1);
  fixture.client_frame();
  fixture.flip(Duration::from_secs(2), 2);
  assert!(fixture.gpu.drawn_into.borrow().is_empty());
  assert!(fixture.gpu.importer.borrow().prepared.is_empty());
  assert_eq!(presented_flags(&fixture), [FLIP_FEEDBACK]);
}

#[test]
fn buffers_the_renderer_cannot_draw_into_fall_back_to_copy() {
  let mut fixture = Fixture::zero_copy(ScanoutChoice::Auto);
  fixture.flip(Duration::from_secs(1), 1);
  fixture
    .failures
    .borrow_mut()
    .push_back(RenderFailure::Scanout("cannot import".into()));
  fixture.client_frame();
  assert!(!fixture.flip_pending(), "the failed frame was not flipped");
  assert_eq!(*fixture.recreated.borrow(), 0, "no GPU loss");
  // The next turn falls back and draws the frame by readback.
  fixture.step();
  assert_eq!(
    fixture.backend.display().scanout_status("HDMI-A-1"),
    Some(ScanoutStatus::Copy("cannot import".into()))
  );
  assert!(fixture.flip_pending());
  assert_eq!(fixture.card.borrow().buffers.len(), 2, "copy buffers");
  assert!(fixture.card.borrow().framebuffers.is_empty());
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(presented_flags(&fixture), [FLIP_FEEDBACK]);
}

#[test]
fn a_lost_gpu_keeps_drawing_into_the_gbm_buffers() {
  let mut fixture = Fixture::zero_copy(ScanoutChoice::Auto);
  fixture.flip(Duration::from_secs(1), 1);
  fixture
    .failures
    .borrow_mut()
    .push_back(RenderFailure::Lost("test".into()));
  fixture.client_frame();
  assert_eq!(*fixture.recreated.borrow(), 1);
  fixture.step();
  fixture.flip(Duration::from_secs(2), 2);
  assert_eq!(
    *fixture.gpu.drawn_into.borrow(),
    [2, 3],
    "the new renderer draws into the same buffers"
  );
  assert_eq!(
    fixture.backend.display().scanout_status("HDMI-A-1"),
    Some(ScanoutStatus::ZeroCopy(MODIFIER_LINEAR))
  );
  assert_eq!(presented_flags(&fixture), [FLIP_FEEDBACK | ZERO_COPY]);
}

impl Fixture {
  /// Completes every pending flip, one turn at a time, until none is
  /// pending, starting at sequence 100.
  fn settle(&mut self) {
    let mut sequence = 100;
    self.step();
    while self.flip_pending() {
      sequence += 1;
      self.flip(Duration::from_millis(u64::from(sequence)), sequence);
    }
  }

  /// Moves the pointer by (`dx`, `dy`) through device `event3`.
  fn move_pointer(&mut self, dx: f64, dy: f64) {
    self.input(
      "event3",
      8_000_000,
      DeviceInput::Motion {
        dx,
        dy,
        dx_unaccel: dx,
        dy_unaccel: dy,
      },
    );
  }

  /// The last commit the card accepted that was not a test.
  fn last_commit(&self) -> (crate::kms::AtomicRequest, CommitFlags) {
    let card = self.card.borrow();
    card.real_commits().last().cloned().expect("a commit")
  }
}

/// A card whose CRTC has a cursor plane, and that plane.
fn cursor_card() -> (FakeKms, crate::kms::PlaneId) {
  let mut card = FakeKms::single_output();
  let crtc = card.crtc_id;
  let cursor = card.add_cursor_plane(crtc);
  (card, cursor)
}

fn hdmi() -> shingle::OutputHandle {
  shingle::OutputHandle::new("HDMI-A-1")
}

#[test]
fn pointer_motion_redraws_the_drawn_cursor() {
  let mut fixture = Fixture::new();
  fixture.add(device("event3", POINTER, 0));
  fixture.settle();
  assert!(!fixture.backend.frontend().hardware_cursor(&hdmi()));
  let drawn = *fixture.frames.borrow();
  fixture.move_pointer(5.0, 5.0);
  assert_eq!(
    *fixture.frames.borrow(),
    drawn + 1,
    "the default arrow is drawn into the frame, so it needs one"
  );
}

#[test]
fn on_a_cursor_plane_the_cursor_moves_without_frames() {
  let (card, cursor) = cursor_card();
  let mut fixture = Fixture::with_card(test_config(), card);
  fixture.add(device("event3", POINTER, 0));
  fixture.settle();
  assert!(fixture.backend.frontend().hardware_cursor(&hdmi()));
  let (_, flags) = fixture.last_commit();
  assert_eq!(flags, FLIP);
  let drawn = *fixture.frames.borrow();
  let x_property = fixture.card.borrow_mut().property_id("CRTC_X");
  let x = |request: &crate::kms::AtomicRequest| {
    request.get(crate::kms::Object::Plane(cursor), x_property)
  };
  let commits = fixture.card.borrow().real_commits().len();
  fixture.move_pointer(5.0, 7.0);
  assert_eq!(*fixture.frames.borrow(), drawn, "no frame is drawn");
  let real = fixture.card.borrow().real_commits();
  assert_eq!(real.len(), commits + 1, "one commit");
  let (request, flags) = real.last().cloned().unwrap();
  assert_eq!(flags, FLIP);
  assert!(
    request
      .entries()
      .iter()
      .all(|(object, ..)| *object == crate::kms::Object::Plane(cursor)),
    "a flip of the cursor plane alone: {request:?}"
  );
  let geometry = fixture
    .backend
    .frontend()
    .compositor()
    .cursor_on(&hdmi())
    .expect("the default arrow")
    .geometry;
  assert_eq!(
    x(&request),
    Some(geometry.x as u64),
    "the plane follows the cursor"
  );
  // Its flip presents nothing, and the next motion follows it.
  let presented = fixture.client.events.presented.len();
  fixture.settle();
  assert_eq!(fixture.client.events.presented.len(), presented);
  fixture.move_pointer(1.0, 0.0);
  assert_eq!(*fixture.frames.borrow(), drawn);
  assert_eq!(fixture.card.borrow().real_commits().len(), commits + 2);
}

#[test]
fn the_cursor_plane_override_draws_the_cursor() {
  let (card, _) = cursor_card();
  let mut fixture = Fixture::with_card(test_config(), card);
  fixture
    .backend
    .display
    .set_features(crate::features::PlaneFeatures {
      no_cursor_plane: Some(
        "SHINGLE_CURSOR_PLANE=0 turns cursor planes off".into(),
      ),
      ..Default::default()
    });
  fixture.add(device("event3", POINTER, 0));
  fixture.settle();
  assert!(!fixture.backend.frontend().hardware_cursor(&hdmi()));
  let drawn = *fixture.frames.borrow();
  fixture.move_pointer(5.0, 5.0);
  assert_eq!(*fixture.frames.borrow(), drawn + 1, "drawn into a frame");
}

#[test]
fn inverted_colours_take_the_cursor_back_into_the_frames() {
  let (card, cursor) = cursor_card();
  let mut fixture = Fixture::with_card(test_config(), card);
  fixture.add(device("event3", POINTER, 0));
  fixture.settle();
  assert!(fixture.backend.frontend().hardware_cursor(&hdmi()));
  let drawn = *fixture.frames.borrow();
  fixture.ipc(r#"{"method":"invert/toggle","data":{}}"#);
  assert!(!fixture.backend.frontend().hardware_cursor(&hdmi()));
  assert_eq!(
    *fixture.frames.borrow(),
    drawn + 1,
    "a frame with the cursor"
  );
  let (request, _) = fixture.last_commit();
  let fb_id = fixture.card.borrow_mut().property_id("FB_ID");
  assert_eq!(
    request.get(crate::kms::Object::Plane(cursor), fb_id),
    Some(0),
    "the same flip turns the plane off"
  );
}

#[test]
fn a_tearing_client_gets_async_flips() {
  let mut card = FakeKms::single_output();
  card.async_flips = true;
  let mut fixture = Fixture::with_card(test_config(), card);
  fixture.settle();
  let surface = fixture.toplevel.surface.clone();
  let _control = fixture.client.tear(&surface);
  fixture.client_frame();
  let (_, flags) = fixture.last_commit();
  assert_eq!(
    flags,
    FLIP | CommitFlags::PAGE_FLIP_ASYNC,
    "its frame flips at once"
  );
  fixture.flip(Duration::from_secs(5), 5);
  let presented = fixture.client.events.presented.last().copied().unwrap();
  assert_eq!(presented.flags, 0x2 | 0x4, "no vsync flag");
}

#[test]
fn vrr_from_the_config_reaches_the_crtc_and_presentation() {
  let mut card = FakeKms::single_output();
  card.vrr = true;
  card.vrr_capable = true;
  let mut config = test_config();
  config.output.vrr = true;
  let mut fixture = Fixture::with_card(config, card);
  let crtc = fixture.card.borrow().crtc_id;
  assert_eq!(fixture.card.borrow().value(crtc.0, "VRR_ENABLED"), 1);
  fixture.settle();
  fixture.client_frame();
  fixture.flip(Duration::from_secs(5), 5);
  let presented = fixture.client.events.presented.last().copied().unwrap();
  assert_eq!(presented.refresh, 0, "no constant refresh rate");
}

#[test]
fn zero_copy_frames_hand_their_fence_to_the_plane_until_it_refuses() {
  let mut fixture = Fixture::zero_copy(ScanoutChoice::Auto);
  fixture.gpu.importer.borrow_mut().fences = true;
  fixture.settle();
  fixture.client_frame();
  let in_fence = fixture.card.borrow_mut().property_id("IN_FENCE_FD");
  let primary = crate::kms::Object::Plane(fixture.card.borrow().plane_id);
  let (request, _) = fixture.last_commit();
  assert!(request.get(primary, in_fence).is_some(), "{request:?}");
  fixture.settle();
  // The card refuses fences: the frame goes out without, and later ones
  // are drawn without.
  fixture.card.borrow_mut().reject =
    Box::new(move |request, _| request.get(primary, in_fence).map(|_| 22));
  fixture.client_frame();
  let (request, _) = fixture.last_commit();
  assert_eq!(request.get(primary, in_fence), None);
  assert!(fixture.backend.display().fences_allowed().is_err());
  fixture.settle();
  fixture.client_frame();
  assert!(!fixture.gpu.importer.borrow().fences_allowed);
  let commits = fixture.card.borrow().commits.len();
  let refused = fixture.card.borrow().commits[commits - 1].2;
  assert!(refused, "the last frame was not refused: no fence tried");
}
