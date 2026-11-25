//! Seat input devices and Wayfire's `input/list-devices` and
//! `input/configure-device` IPC methods.
//!
//! Shingle has three seat devices: the keyboard, pointer and touch input that
//! reach it from the nested host window and from STIPC. Every event from
//! those sources enters through the gated functions here, so a disabled
//! device delivers nothing to clients, bindings or compositor actions.
//! Virtual keyboards and pointers created by clients are separate devices,
//! as in Wayfire, and are not listed or gated.
//!
//! Disabling a device returns it to a neutral state before the gate closes,
//! the way libinput does when Wayfire suspends a device: held keys and
//! buttons are released with ordinary release events, depressed and latched
//! modifiers are cleared, active touch points are cancelled with
//! `wl_touch.cancel` and an active pointer gesture ends as cancelled.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::{
  Event,
  input::{self, Modifiers},
  state::CompositorState,
};

/// The kind of a seat input device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputDeviceKind {
  Keyboard,
  Pointer,
  Touch,
}

impl InputDeviceKind {
  fn index(self) -> usize {
    match self {
      Self::Keyboard => 0,
      Self::Pointer => 1,
      Self::Touch => 2,
    }
  }

  /// Returns the Wayfire `type` string for this kind.
  pub fn wayfire_type(self) -> &'static str {
    match self {
      Self::Keyboard => "keyboard",
      Self::Pointer => "pointer",
      Self::Touch => "touch",
    }
  }
}

/// Static metadata for one seat device.
struct DeviceInfo {
  id: i64,
  name: &'static str,
  kind: InputDeviceKind,
}

/// The seat devices, in the order `input/list-devices` reports them. IDs are
/// stable for the lifetime of the compositor.
const DEVICES: [DeviceInfo; 3] = [
  DeviceInfo {
    id: 1,
    name: "Shingle nested keyboard",
    kind: InputDeviceKind::Keyboard,
  },
  DeviceInfo {
    id: 2,
    name: "Shingle nested pointer",
    kind: InputDeviceKind::Pointer,
  },
  DeviceInfo {
    id: 3,
    name: "Shingle nested touch",
    kind: InputDeviceKind::Touch,
  },
];

/// Enabled flags and the input each device currently holds down.
pub(crate) struct InputDevices {
  enabled: [bool; 3],
  held_keys: BTreeSet<u32>,
  held_buttons: BTreeSet<u32>,
  held_touches: BTreeSet<i32>,
}

impl Default for InputDevices {
  fn default() -> Self {
    Self {
      enabled: [true; 3],
      held_keys: BTreeSet::new(),
      held_buttons: BTreeSet::new(),
      held_touches: BTreeSet::new(),
    }
  }
}

/// Returns whether the seat device of this kind is enabled.
pub(crate) fn enabled(state: &CompositorState, kind: InputDeviceKind) -> bool {
  state.input.devices.enabled[kind.index()]
}

/// Enables or disables a device. Disabling first releases everything the
/// device holds, so no client is left with stuck input. Returns whether the
/// state changed.
pub(crate) fn set_enabled(
  state: &mut CompositorState,
  kind: InputDeviceKind,
  enabled: bool,
) -> bool {
  if self::enabled(state, kind) == enabled {
    return false;
  }
  if !enabled {
    release_held(state, kind);
  }
  state.input.devices.enabled[kind.index()] = enabled;
  state.events.push(Event::InputDeviceChanged {
    device: kind,
    enabled,
  });
  true
}

fn release_held(state: &mut CompositorState, kind: InputDeviceKind) {
  match kind {
    InputDeviceKind::Keyboard => {
      let keys = std::mem::take(&mut state.input.devices.held_keys);
      for key in keys {
        input::keyboard_key(state, key, false);
      }
      let modifiers = state.input.modifiers;
      if modifiers.depressed != 0 || modifiers.latched != 0 {
        input::keyboard_modifiers(
          state,
          Modifiers {
            depressed: 0,
            latched: 0,
            ..modifiers
          },
        );
      }
    }
    InputDeviceKind::Pointer => {
      let buttons = std::mem::take(&mut state.input.devices.held_buttons);
      for button in buttons {
        input::pointer_button(state, button, false);
      }
      crate::pointer_gestures::cancel_active(state);
    }
    InputDeviceKind::Touch => {
      let touches = std::mem::take(&mut state.input.devices.held_touches);
      for id in touches {
        input::touch_cancel(state, id);
      }
    }
  }
}

/// Handles `input/list-devices`.
pub(crate) fn list_json(state: &CompositorState) -> Value {
  Value::Array(
    DEVICES
      .iter()
      .map(|device| {
        json!({
            "id": device.id,
            "name": device.name,
            "vendor": "unknown",
            "product": "unknown",
            "type": device.kind.wayfire_type(),
            "enabled": enabled(state, device.kind),
        })
      })
      .collect(),
  )
}

/// Handles `input/configure-device`, validating fields in Wayfire's order and
/// with its error messages.
pub(crate) fn configure(state: &mut CompositorState, data: &Value) -> Value {
  let id = match data.get("id") {
    None => return error("Missing \"id\""),
    Some(value) => match value.as_i64() {
      Some(id) => id,
      None => return wrong_type("id", "int64"),
    },
  };
  let enabled = match data.get("enabled") {
    None => return error("Missing \"enabled\""),
    Some(value) => match value.as_bool() {
      Some(enabled) => enabled,
      None => return wrong_type("enabled", "bool"),
    },
  };
  let Some(device) = DEVICES.iter().find(|device| device.id == id) else {
    return error("Unknown input device!");
  };
  set_enabled(state, device.kind, enabled);
  json!({"result": "ok"})
}

fn error(message: &str) -> Value {
  json!({"error": message})
}

fn wrong_type(field: &str, expected: &str) -> Value {
  error(&format!(
    "Field \"{field}\" does not have the correct type, expected {expected}"
  ))
}

/// Feeds a key from the seat keyboard. Returns whether the keyboard is
/// enabled and the key was passed on.
pub(crate) fn keyboard_key(
  state: &mut CompositorState,
  key: u32,
  pressed: bool,
) -> bool {
  if !enabled(state, InputDeviceKind::Keyboard) {
    return false;
  }
  if pressed {
    state.input.devices.held_keys.insert(key);
  } else {
    state.input.devices.held_keys.remove(&key);
  }
  input::keyboard_key(state, key, pressed);
  true
}

/// Feeds modifier state from the seat keyboard.
pub(crate) fn keyboard_modifiers(
  state: &mut CompositorState,
  modifiers: Modifiers,
) {
  if enabled(state, InputDeviceKind::Keyboard) {
    input::keyboard_modifiers(state, modifiers);
  }
}

/// Returns the keys the seat keyboard currently holds.
pub(crate) fn held_keys(state: &CompositorState) -> &BTreeSet<u32> {
  &state.input.devices.held_keys
}

/// Feeds absolute motion from the seat pointer.
pub(crate) fn pointer_motion(state: &mut CompositorState, x: f64, y: f64) {
  if enabled(state, InputDeviceKind::Pointer) {
    input::pointer_motion(state, x, y);
  }
}

/// Feeds relative motion from the seat pointer.
pub(crate) fn pointer_relative_motion(
  state: &mut CompositorState,
  dx: f64,
  dy: f64,
  dx_unaccel: f64,
  dy_unaccel: f64,
) {
  if enabled(state, InputDeviceKind::Pointer) {
    input::pointer_relative_motion(state, dx, dy, dx_unaccel, dy_unaccel);
  }
}

/// Feeds a button from the seat pointer.
pub(crate) fn pointer_button(
  state: &mut CompositorState,
  button: u32,
  pressed: bool,
) {
  if !enabled(state, InputDeviceKind::Pointer) {
    return;
  }
  if pressed {
    state.input.devices.held_buttons.insert(button);
  } else {
    state.input.devices.held_buttons.remove(&button);
  }
  input::pointer_button(state, button, pressed);
}

/// Feeds scrolling from the seat pointer.
pub(crate) fn pointer_axis_detailed(
  state: &mut CompositorState,
  horizontal: f64,
  vertical: f64,
  horizontal_steps: i32,
  vertical_steps: i32,
) {
  if enabled(state, InputDeviceKind::Pointer) {
    input::pointer_axis_detailed(
      state,
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    );
  }
}

/// Starts a touch point on the seat touch device.
pub(crate) fn touch_down(state: &mut CompositorState, id: i32, x: f64, y: f64) {
  if enabled(state, InputDeviceKind::Touch) {
    state.input.devices.held_touches.insert(id);
    input::touch_down(state, id, x, y);
  }
}

/// Moves a touch point on the seat touch device.
pub(crate) fn touch_motion(
  state: &mut CompositorState,
  id: i32,
  x: f64,
  y: f64,
) {
  if enabled(state, InputDeviceKind::Touch) {
    input::touch_motion(state, id, x, y);
  }
}

/// Starts or moves a touch point, as STIPC `touch` does.
pub(crate) fn touch(state: &mut CompositorState, id: i32, x: f64, y: f64) {
  if enabled(state, InputDeviceKind::Touch) {
    state.input.devices.held_touches.insert(id);
    input::touch(state, id, x, y);
  }
}

/// Lifts a touch point on the seat touch device.
pub(crate) fn touch_up(state: &mut CompositorState, id: i32) {
  if enabled(state, InputDeviceKind::Touch) {
    state.input.devices.held_touches.remove(&id);
    input::touch_up(state, id);
  }
}

/// Cancels a touch point on the seat touch device.
pub(crate) fn touch_cancel(state: &mut CompositorState, id: i32) {
  if enabled(state, InputDeviceKind::Touch) {
    state.input.devices.held_touches.remove(&id);
    input::touch_cancel(state, id);
  }
}

/// Returns whether no device holds any key, button or touch point.
#[cfg(test)]
pub(crate) fn holds_nothing(state: &CompositorState) -> bool {
  let devices = &state.input.devices;
  devices.held_keys.is_empty()
    && devices.held_buttons.is_empty()
    && devices.held_touches.is_empty()
}

#[cfg(test)]
pub(crate) mod tests {
  use std::{
    ffi::OsStr,
    sync::{Arc, Mutex},
  };

  use serde_json::json;

  use super::*;

  /// Builds a compositor state without a Wayland display or clients.
  pub(crate) fn test_state() -> CompositorState {
    CompositorState::new(
      crate::Config::default(),
      shingle_core::Shingle::new(),
      Arc::new(Mutex::new(Vec::new())),
      OsStr::new("wayland-test"),
    )
    .expect("default config builds a compositor state")
  }

  fn device_events(
    state: &mut CompositorState,
  ) -> Vec<(InputDeviceKind, bool)> {
    state
      .events
      .drain(..)
      .filter_map(|event| match event {
        Event::InputDeviceChanged { device, enabled } => {
          Some((device, enabled))
        }
        _ => None,
      })
      .collect()
  }

  #[test]
  fn lists_three_devices_with_wayfire_fields() {
    let state = test_state();
    let devices = list_json(&state);
    assert_eq!(
      devices,
      json!([
        {"id": 1, "name": "Shingle nested keyboard", "vendor": "unknown",
         "product": "unknown", "type": "keyboard", "enabled": true},
        {"id": 2, "name": "Shingle nested pointer", "vendor": "unknown",
         "product": "unknown", "type": "pointer", "enabled": true},
        {"id": 3, "name": "Shingle nested touch", "vendor": "unknown",
         "product": "unknown", "type": "touch", "enabled": true},
      ])
    );
  }

  #[test]
  fn configure_toggles_one_device_and_reports_it() {
    let mut state = test_state();
    let reply = configure(&mut state, &json!({"id": 2, "enabled": false}));
    assert_eq!(reply, json!({"result": "ok"}));
    assert!(enabled(&state, InputDeviceKind::Keyboard));
    assert!(!enabled(&state, InputDeviceKind::Pointer));
    assert!(enabled(&state, InputDeviceKind::Touch));
    assert_eq!(list_json(&state)[1]["enabled"], json!(false));
    assert_eq!(
      device_events(&mut state),
      vec![(InputDeviceKind::Pointer, false)]
    );
    configure(&mut state, &json!({"id": 2, "enabled": false}));
    assert!(device_events(&mut state).is_empty());
    configure(&mut state, &json!({"id": 2, "enabled": true}));
    assert!(enabled(&state, InputDeviceKind::Pointer));
    assert_eq!(
      device_events(&mut state),
      vec![(InputDeviceKind::Pointer, true)]
    );
  }

  #[test]
  fn configure_rejects_bad_requests_without_changing_state() {
    let mut state = test_state();
    let cases = [
      (json!({"enabled": false}), "Missing \"id\""),
      (
        json!({"id": "1", "enabled": false}),
        "Field \"id\" does not have the correct type, expected int64",
      ),
      (
        json!({"id": 1.5, "enabled": false}),
        "Field \"id\" does not have the correct type, expected int64",
      ),
      (json!({"id": 1}), "Missing \"enabled\""),
      (
        json!({"id": 1, "enabled": 0}),
        "Field \"enabled\" does not have the correct type, expected bool",
      ),
      (
        json!({"id": 1, "enabled": "false"}),
        "Field \"enabled\" does not have the correct type, expected bool",
      ),
      (json!({"id": 4, "enabled": false}), "Unknown input device!"),
      (json!({"id": 0, "enabled": false}), "Unknown input device!"),
      (json!({"id": -1, "enabled": false}), "Unknown input device!"),
      (json!(null), "Missing \"id\""),
    ];
    for (data, message) in cases {
      assert_eq!(
        configure(&mut state, &data),
        json!({"error": message}),
        "{data}"
      );
    }
    for device in &DEVICES {
      assert!(enabled(&state, device.kind));
    }
    assert!(device_events(&mut state).is_empty());
  }

  #[test]
  fn disabled_pointer_ignores_motion_until_enabled() {
    let mut state = test_state();
    pointer_motion(&mut state, 10.0, 20.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (10.0, 20.0));
    set_enabled(&mut state, InputDeviceKind::Pointer, false);
    pointer_motion(&mut state, 30.0, 40.0);
    pointer_relative_motion(&mut state, 5.0, 5.0, 5.0, 5.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (10.0, 20.0));
    set_enabled(&mut state, InputDeviceKind::Pointer, true);
    pointer_relative_motion(&mut state, 5.0, 6.0, 5.0, 6.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (15.0, 26.0));
  }

  #[test]
  fn client_virtual_pointer_motion_is_not_gated() {
    let mut state = test_state();
    set_enabled(&mut state, InputDeviceKind::Pointer, false);
    input::pointer_motion(&mut state, 30.0, 40.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (30.0, 40.0));
  }

  #[test]
  fn disabled_keyboard_ignores_keys_and_modifiers() {
    let mut state = test_state();
    set_enabled(&mut state, InputDeviceKind::Keyboard, false);
    state.ipc_events.clear();
    assert!(!keyboard_key(&mut state, 30, true));
    assert!(held_keys(&state).is_empty());
    let modifiers = Modifiers {
      depressed: 64,
      ..state.input.modifiers
    };
    keyboard_modifiers(&mut state, modifiers);
    assert_eq!(state.input.modifiers.depressed, 0);
    assert!(state.ipc_events.is_empty());
    set_enabled(&mut state, InputDeviceKind::Keyboard, true);
    assert!(keyboard_key(&mut state, 30, true));
    assert_eq!(held_keys(&state).iter().copied().collect::<Vec<_>>(), [30]);
  }

  #[test]
  fn disabling_keyboard_releases_held_keys_and_modifiers() {
    let mut state = test_state();
    keyboard_key(&mut state, 125, true);
    keyboard_key(&mut state, 30, true);
    keyboard_modifiers(
      &mut state,
      Modifiers {
        depressed: 64,
        latched: 1,
        locked: 2,
        group: 0,
      },
    );
    state.ipc_events.clear();
    set_enabled(&mut state, InputDeviceKind::Keyboard, false);
    assert!(held_keys(&state).is_empty());
    assert_eq!(
      state.input.modifiers,
      Modifiers {
        depressed: 0,
        latched: 0,
        locked: 2,
        group: 0,
      }
    );
    assert!(
      state
        .ipc_events
        .iter()
        .any(|event| { event["event"] == "keyboard-modifier-state-changed" })
    );
  }

  #[test]
  fn disabling_keyboard_without_held_modifiers_sends_no_update() {
    let mut state = test_state();
    state.ipc_events.clear();
    set_enabled(&mut state, InputDeviceKind::Keyboard, false);
    assert!(state.ipc_events.is_empty());
  }

  #[test]
  fn disabling_pointer_releases_held_buttons() {
    let mut state = test_state();
    pointer_button(&mut state, 0x110, true);
    pointer_button(&mut state, 0x111, true);
    pointer_button(&mut state, 0x111, false);
    assert_eq!(
      state
        .input
        .devices
        .held_buttons
        .iter()
        .copied()
        .collect::<Vec<_>>(),
      [0x110]
    );
    set_enabled(&mut state, InputDeviceKind::Pointer, false);
    assert!(state.input.devices.held_buttons.is_empty());
    pointer_button(&mut state, 0x112, true);
    assert!(state.input.devices.held_buttons.is_empty());
  }

  #[test]
  fn disabling_pointer_cancels_active_gesture() {
    let mut state = test_state();
    crate::pointer_gestures::swipe_begin(&mut state, 3);
    assert!(crate::pointer_gestures::is_active(&state));
    set_enabled(&mut state, InputDeviceKind::Pointer, false);
    assert!(!crate::pointer_gestures::is_active(&state));
    crate::pointer_gestures::pinch_begin(&mut state, 2);
    crate::pointer_gestures::hold_begin(&mut state, 2);
    assert!(!crate::pointer_gestures::is_active(&state));
    set_enabled(&mut state, InputDeviceKind::Pointer, true);
    crate::pointer_gestures::hold_begin(&mut state, 2);
    assert!(crate::pointer_gestures::is_active(&state));
  }

  #[test]
  fn disabling_touch_cancels_held_touches() {
    let mut state = test_state();
    touch_down(&mut state, 1, 5.0, 5.0);
    touch(&mut state, 2, 6.0, 6.0);
    touch_up(&mut state, 2);
    assert_eq!(
      state
        .input
        .devices
        .held_touches
        .iter()
        .copied()
        .collect::<Vec<_>>(),
      [1]
    );
    set_enabled(&mut state, InputDeviceKind::Touch, false);
    assert!(state.input.devices.held_touches.is_empty());
    touch_down(&mut state, 3, 5.0, 5.0);
    touch(&mut state, 4, 5.0, 5.0);
    assert!(state.input.devices.held_touches.is_empty());
  }

  #[test]
  fn disabling_one_device_keeps_the_others_held() {
    let mut state = test_state();
    keyboard_key(&mut state, 30, true);
    pointer_button(&mut state, 0x110, true);
    touch_down(&mut state, 1, 0.0, 0.0);
    set_enabled(&mut state, InputDeviceKind::Pointer, false);
    assert_eq!(held_keys(&state).len(), 1);
    assert_eq!(state.input.devices.held_touches.len(), 1);
  }
}
