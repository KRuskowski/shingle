//! Seat input devices and Wayfire's `input/list-devices` and
//! `input/configure-device` IPC methods.
//!
//! The backend registers every device it feeds input from with
//! `Compositor::add_input_device` and passes the returned
//! [`InputDeviceId`] with each event. Nested registers its keyboard,
//! pointer and touch input, the native backend one device per libinput
//! device capability. Every event from those devices enters through the
//! gated functions here, so a disabled device delivers nothing to clients,
//! bindings or compositor actions. Virtual keyboards and pointers created by
//! clients are separate devices, as in Wayfire, and are not listed or gated.
//! STIPC input goes through the first registered device of each kind.
//!
//! Each device tracks the keys, buttons and touch points it holds down.
//! Disabling or removing a device returns it to a neutral state before the
//! gate closes, the way libinput does when Wayfire suspends a device: held
//! keys and buttons are released with ordinary release events, active touch
//! points are cancelled with `wl_touch.cancel` and a pointer gesture the
//! device started ends as cancelled. A key, button or touch point that
//! another device still holds stays down for clients. Depressed and latched
//! modifiers are cleared once no keyboard holds any key.
//!
//! The same release happens for every device when the seat session becomes
//! inactive, for example on a VT switch, and no device delivers anything
//! until it is active again.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::{
  Event,
  input::{self, Modifiers},
  state::CompositorState,
};

/// The kind of a seat input device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputDeviceKind {
  /// Takes `keyboard_key` and `keyboard_modifiers`.
  Keyboard,
  /// Takes pointer motion, buttons, axes and touchpad gestures.
  Pointer,
  /// Takes `touch_*` events.
  Touch,
}

impl InputDeviceKind {
  /// Returns the Wayfire `type` string for this kind.
  pub fn wayfire_type(self) -> &'static str {
    match self {
      Self::Keyboard => "keyboard",
      Self::Pointer => "pointer",
      Self::Touch => "touch",
    }
  }
}

/// Identifies a registered seat input device. IDs count up from 1 in
/// registration order and are never reused while the compositor runs; they
/// are the `id` that `input/list-devices` reports.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InputDeviceId(u32);

impl InputDeviceId {
  /// Returns the numeric ID used over IPC.
  pub fn get(self) -> u32 {
    self.0
  }
}

/// Describes a seat input device to `Compositor::add_input_device`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputDeviceInfo {
  /// The device name, such as libinput's `Device::name`.
  pub name: String,
  /// The USB or Bluetooth vendor ID, if the device has one. IPC reports
  /// `"unknown"` without it, as Wayfire does for non-libinput devices.
  pub vendor: Option<u32>,
  /// The product ID, if the device has one.
  pub product: Option<u32>,
  /// What the device delivers. Input of another kind is dropped.
  pub kind: InputDeviceKind,
}

/// The keys, buttons and touch points one device holds down.
#[derive(Default)]
struct Held {
  keys: BTreeSet<u32>,
  buttons: BTreeSet<u32>,
  touches: BTreeSet<i32>,
}

impl Held {
  #[cfg(test)]
  fn is_empty(&self) -> bool {
    self.keys.is_empty() && self.buttons.is_empty() && self.touches.is_empty()
  }
}

/// One registered device.
struct Device {
  info: InputDeviceInfo,
  enabled: bool,
  held: Held,
}

/// The registered seat devices and the session state that gates them all.
pub(crate) struct InputDevices {
  devices: BTreeMap<InputDeviceId, Device>,
  next_id: u32,
  session_active: bool,
}

impl Default for InputDevices {
  fn default() -> Self {
    Self {
      devices: BTreeMap::new(),
      next_id: 1,
      session_active: true,
    }
  }
}

/// Registers a device, enabled, and returns its new ID.
pub(crate) fn add(
  state: &mut CompositorState,
  info: InputDeviceInfo,
) -> InputDeviceId {
  let devices = &mut state.input.devices;
  let id = InputDeviceId(devices.next_id);
  devices.next_id += 1;
  devices.devices.insert(
    id,
    Device {
      info,
      enabled: true,
      held: Held::default(),
    },
  );
  id
}

/// Unregisters a device after releasing everything it holds. Returns
/// whether the device was registered.
pub(crate) fn remove(state: &mut CompositorState, id: InputDeviceId) -> bool {
  if !state.input.devices.devices.contains_key(&id) {
    return false;
  }
  release_held(state, id);
  state.input.devices.devices.remove(&id);
  true
}

/// Returns the registration of a device.
pub(crate) fn info(
  state: &CompositorState,
  id: InputDeviceId,
) -> Option<&InputDeviceInfo> {
  state
    .input
    .devices
    .devices
    .get(&id)
    .map(|device| &device.info)
}

/// Returns whether a device is registered and enabled.
pub(crate) fn enabled(state: &CompositorState, id: InputDeviceId) -> bool {
  state
    .input
    .devices
    .devices
    .get(&id)
    .is_some_and(|device| device.enabled)
}

/// Returns whether input of `kind` from a device reaches the compositor:
/// the device is registered, of that kind and enabled, and the session is
/// active.
pub(crate) fn delivers(
  state: &CompositorState,
  id: InputDeviceId,
  kind: InputDeviceKind,
) -> bool {
  session_active(state)
    && state
      .input
      .devices
      .devices
      .get(&id)
      .is_some_and(|device| device.enabled && device.info.kind == kind)
}

/// Returns the device STIPC input of `kind` goes through: the registered
/// device of that kind with the lowest ID.
pub(crate) fn stipc_device(
  state: &CompositorState,
  kind: InputDeviceKind,
) -> Option<InputDeviceId> {
  state
    .input
    .devices
    .devices
    .iter()
    .find(|(_, device)| device.info.kind == kind)
    .map(|(id, _)| *id)
}

/// Returns whether the seat session is active.
pub(crate) fn session_active(state: &CompositorState) -> bool {
  state.input.devices.session_active
}

/// Marks the seat session active or inactive. Deactivating first releases
/// everything every device holds. Returns whether the state changed.
pub(crate) fn set_session_active(
  state: &mut CompositorState,
  active: bool,
) -> bool {
  if session_active(state) == active {
    return false;
  }
  if !active {
    let ids: Vec<_> = state.input.devices.devices.keys().copied().collect();
    for id in ids {
      release_held(state, id);
    }
  }
  state.input.devices.session_active = active;
  true
}

/// Enables or disables a device. Disabling first releases everything the
/// device holds, so no client is left with stuck input. Returns whether the
/// state changed; an unknown device changes nothing.
pub(crate) fn set_enabled(
  state: &mut CompositorState,
  id: InputDeviceId,
  enabled: bool,
) -> bool {
  match state.input.devices.devices.get(&id) {
    Some(device) if device.enabled != enabled => {}
    _ => return false,
  }
  if !enabled {
    release_held(state, id);
  }
  if let Some(device) = state.input.devices.devices.get_mut(&id) {
    device.enabled = enabled;
  }
  state.events.push(Event::InputDeviceChanged { id, enabled });
  true
}

/// Returns whether any device holds something `held` selects.
fn any_holds(state: &CompositorState, held: impl Fn(&Held) -> bool) -> bool {
  state
    .input
    .devices
    .devices
    .values()
    .any(|device| held(&device.held))
}

fn release_held(state: &mut CompositorState, id: InputDeviceId) {
  let Some(device) = state.input.devices.devices.get_mut(&id) else {
    return;
  };
  let kind = device.info.kind;
  let held = std::mem::take(&mut device.held);
  for key in held.keys {
    if !any_holds(state, |held| held.keys.contains(&key)) {
      input::keyboard_key(state, key, false);
    }
  }
  if kind == InputDeviceKind::Keyboard
    && !any_holds(state, |held| !held.keys.is_empty())
  {
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
  for button in held.buttons {
    if !any_holds(state, |held| held.buttons.contains(&button)) {
      input::pointer_button(state, button, false);
    }
  }
  if kind == InputDeviceKind::Pointer {
    crate::pointer_gestures::cancel_active(state, id);
  }
  for touch in held.touches {
    if !any_holds(state, |held| held.touches.contains(&touch)) {
      input::touch_cancel(state, touch);
    }
  }
}

/// Returns the hold sets of a device that delivers input of `kind`.
fn delivering_held(
  state: &mut CompositorState,
  id: InputDeviceId,
  kind: InputDeviceKind,
) -> Option<&mut Held> {
  if !delivers(state, id, kind) {
    return None;
  }
  state
    .input
    .devices
    .devices
    .get_mut(&id)
    .map(|device| &mut device.held)
}

/// Handles `input/list-devices`.
pub(crate) fn list_json(state: &CompositorState) -> Value {
  let unknown_or =
    |value: Option<u32>| value.map_or(json!("unknown"), |v| json!(v));
  Value::Array(
    state
      .input
      .devices
      .devices
      .iter()
      .map(|(id, device)| {
        json!({
            "id": id.get(),
            "name": device.info.name,
            "vendor": unknown_or(device.info.vendor),
            "product": unknown_or(device.info.product),
            "type": device.info.kind.wayfire_type(),
            "enabled": device.enabled,
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
  let Some(id) = u32::try_from(id)
    .ok()
    .map(InputDeviceId)
    .filter(|id| state.input.devices.devices.contains_key(id))
  else {
    return error("Unknown input device!");
  };
  set_enabled(state, id, enabled);
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

/// Feeds a key from a seat keyboard. Returns whether the keyboard delivers
/// and the key was passed on. A release is only sent to clients once no
/// other device holds the key.
pub(crate) fn keyboard_key(
  state: &mut CompositorState,
  id: InputDeviceId,
  key: u32,
  pressed: bool,
) -> bool {
  let Some(held) = delivering_held(state, id, InputDeviceKind::Keyboard) else {
    return false;
  };
  if pressed {
    held.keys.insert(key);
  } else {
    held.keys.remove(&key);
    if any_holds(state, |held| held.keys.contains(&key)) {
      return true;
    }
  }
  input::keyboard_key(state, key, pressed);
  true
}

/// Feeds modifier state from a seat keyboard.
pub(crate) fn keyboard_modifiers(
  state: &mut CompositorState,
  id: InputDeviceId,
  modifiers: Modifiers,
) {
  if delivers(state, id, InputDeviceKind::Keyboard) {
    input::keyboard_modifiers(state, modifiers);
  }
}

/// Returns the keys a device currently holds.
pub(crate) fn held_keys(
  state: &CompositorState,
  id: InputDeviceId,
) -> BTreeSet<u32> {
  state
    .input
    .devices
    .devices
    .get(&id)
    .map(|device| device.held.keys.clone())
    .unwrap_or_default()
}

/// Feeds absolute motion from a seat pointer.
pub(crate) fn pointer_motion(
  state: &mut CompositorState,
  id: InputDeviceId,
  x: f64,
  y: f64,
) {
  if delivers(state, id, InputDeviceKind::Pointer) {
    input::pointer_motion(state, x, y);
  }
}

/// Feeds relative motion from a seat pointer.
pub(crate) fn pointer_relative_motion(
  state: &mut CompositorState,
  id: InputDeviceId,
  dx: f64,
  dy: f64,
  dx_unaccel: f64,
  dy_unaccel: f64,
) {
  if delivers(state, id, InputDeviceKind::Pointer) {
    input::pointer_relative_motion(state, dx, dy, dx_unaccel, dy_unaccel);
  }
}

/// Feeds a button from a seat pointer. A release is only sent to clients
/// once no other device holds the button.
pub(crate) fn pointer_button(
  state: &mut CompositorState,
  id: InputDeviceId,
  button: u32,
  pressed: bool,
) {
  let Some(held) = delivering_held(state, id, InputDeviceKind::Pointer) else {
    return;
  };
  if pressed {
    held.buttons.insert(button);
  } else {
    held.buttons.remove(&button);
    if any_holds(state, |held| held.buttons.contains(&button)) {
      return;
    }
  }
  input::pointer_button(state, button, pressed);
}

/// Feeds scrolling from a seat pointer.
pub(crate) fn pointer_axis_detailed(
  state: &mut CompositorState,
  id: InputDeviceId,
  horizontal: f64,
  vertical: f64,
  horizontal_steps: i32,
  vertical_steps: i32,
) {
  if delivers(state, id, InputDeviceKind::Pointer) {
    input::pointer_axis_detailed(
      state,
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    );
  }
}

/// Starts a touch point on a seat touch device.
pub(crate) fn touch_down(
  state: &mut CompositorState,
  id: InputDeviceId,
  touch: i32,
  x: f64,
  y: f64,
) {
  if let Some(held) = delivering_held(state, id, InputDeviceKind::Touch) {
    held.touches.insert(touch);
    input::touch_down(state, touch, x, y);
  }
}

/// Moves a touch point on a seat touch device.
pub(crate) fn touch_motion(
  state: &mut CompositorState,
  id: InputDeviceId,
  touch: i32,
  x: f64,
  y: f64,
) {
  if delivers(state, id, InputDeviceKind::Touch) {
    input::touch_motion(state, touch, x, y);
  }
}

/// Starts or moves a touch point, as STIPC `touch` does.
pub(crate) fn touch(
  state: &mut CompositorState,
  id: InputDeviceId,
  touch: i32,
  x: f64,
  y: f64,
) {
  if let Some(held) = delivering_held(state, id, InputDeviceKind::Touch) {
    held.touches.insert(touch);
    input::touch(state, touch, x, y);
  }
}

/// Lifts a touch point on a seat touch device.
pub(crate) fn touch_up(
  state: &mut CompositorState,
  id: InputDeviceId,
  touch: i32,
) {
  if let Some(held) = delivering_held(state, id, InputDeviceKind::Touch) {
    held.touches.remove(&touch);
    if !any_holds(state, |held| held.touches.contains(&touch)) {
      input::touch_up(state, touch);
    }
  }
}

/// Cancels a touch point on a seat touch device.
pub(crate) fn touch_cancel(
  state: &mut CompositorState,
  id: InputDeviceId,
  touch: i32,
) {
  if let Some(held) = delivering_held(state, id, InputDeviceKind::Touch) {
    held.touches.remove(&touch);
    if !any_holds(state, |held| held.touches.contains(&touch)) {
      input::touch_cancel(state, touch);
    }
  }
}

/// Returns whether no device holds any key, button or touch point.
#[cfg(test)]
pub(crate) fn holds_nothing(state: &CompositorState) -> bool {
  !any_holds(state, |held| !held.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
  use std::{
    ffi::OsStr,
    sync::{Arc, Mutex},
  };

  use serde_json::json;

  use super::*;

  /// The nested keyboard's ID in [`test_state`].
  pub(crate) const KEYBOARD: InputDeviceId = InputDeviceId(1);
  /// The nested pointer's ID in [`test_state`].
  pub(crate) const POINTER: InputDeviceId = InputDeviceId(2);
  /// The nested touch device's ID in [`test_state`].
  pub(crate) const TOUCH: InputDeviceId = InputDeviceId(3);

  /// Returns the registration nested uses for a device of `kind`.
  pub(crate) fn nested_info(kind: InputDeviceKind) -> InputDeviceInfo {
    InputDeviceInfo {
      name: format!("Shingle nested {}", kind.wayfire_type()),
      vendor: None,
      product: None,
      kind,
    }
  }

  /// Builds a compositor state without a Wayland display or clients and
  /// with no seat devices.
  pub(crate) fn bare_state() -> CompositorState {
    CompositorState::new(
      crate::Config::default(),
      shingle_core::Shingle::new(),
      Arc::new(Mutex::new(Vec::new())),
      OsStr::new("wayland-test"),
    )
    .expect("default config builds a compositor state")
  }

  /// Builds a compositor state without a Wayland display or clients, with
  /// nested's three seat devices registered as [`KEYBOARD`], [`POINTER`]
  /// and [`TOUCH`].
  pub(crate) fn test_state() -> CompositorState {
    let mut state = bare_state();
    for kind in [
      InputDeviceKind::Keyboard,
      InputDeviceKind::Pointer,
      InputDeviceKind::Touch,
    ] {
      add(&mut state, nested_info(kind));
    }
    state
  }

  fn keyboard_info(name: &str) -> InputDeviceInfo {
    InputDeviceInfo {
      name: name.into(),
      vendor: Some(0x046d),
      product: Some(0xc31c),
      kind: InputDeviceKind::Keyboard,
    }
  }

  fn device_events(state: &mut CompositorState) -> Vec<(InputDeviceId, bool)> {
    state
      .events
      .drain(..)
      .filter_map(|event| match event {
        Event::InputDeviceChanged { id, enabled } => Some((id, enabled)),
        _ => None,
      })
      .collect()
  }

  fn held(state: &CompositorState, id: InputDeviceId) -> &Held {
    &state.input.devices.devices[&id].held
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
  fn ids_count_up_and_are_not_reused() {
    let mut state = bare_state();
    assert_eq!(list_json(&state), json!([]));
    let first = add(&mut state, keyboard_info("first"));
    let second = add(&mut state, keyboard_info("second"));
    assert_eq!((first.get(), second.get()), (1, 2));
    assert!(remove(&mut state, first));
    assert!(!remove(&mut state, first));
    let third = add(&mut state, keyboard_info("third"));
    assert_eq!(third.get(), 3);
    assert_eq!(info(&state, first), None);
    assert_eq!(
      info(&state, third).map(|info| info.name.as_str()),
      Some("third")
    );
    assert_eq!(
      list_json(&state),
      json!([
        {"id": 2, "name": "second", "vendor": 1133, "product": 49948,
         "type": "keyboard", "enabled": true},
        {"id": 3, "name": "third", "vendor": 1133, "product": 49948,
         "type": "keyboard", "enabled": true},
      ])
    );
  }

  #[test]
  fn configure_toggles_one_device_and_reports_it() {
    let mut state = test_state();
    let reply = configure(&mut state, &json!({"id": 2, "enabled": false}));
    assert_eq!(reply, json!({"result": "ok"}));
    assert!(enabled(&state, KEYBOARD));
    assert!(!enabled(&state, POINTER));
    assert!(enabled(&state, TOUCH));
    assert_eq!(list_json(&state)[1]["enabled"], json!(false));
    assert_eq!(device_events(&mut state), vec![(POINTER, false)]);
    configure(&mut state, &json!({"id": 2, "enabled": false}));
    assert!(device_events(&mut state).is_empty());
    configure(&mut state, &json!({"id": 2, "enabled": true}));
    assert!(enabled(&state, POINTER));
    assert_eq!(device_events(&mut state), vec![(POINTER, true)]);
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
      (
        json!({"id": i64::from(u32::MAX) + 2, "enabled": false}),
        "Unknown input device!",
      ),
      (json!(null), "Missing \"id\""),
    ];
    for (data, message) in cases {
      assert_eq!(
        configure(&mut state, &data),
        json!({"error": message}),
        "{data}"
      );
    }
    for id in [KEYBOARD, POINTER, TOUCH] {
      assert!(enabled(&state, id));
    }
    assert!(device_events(&mut state).is_empty());
  }

  #[test]
  fn configure_rejects_a_removed_device() {
    let mut state = test_state();
    remove(&mut state, POINTER);
    assert_eq!(
      configure(&mut state, &json!({"id": 2, "enabled": false})),
      json!({"error": "Unknown input device!"})
    );
    assert!(!set_enabled(&mut state, POINTER, false));
    assert!(device_events(&mut state).is_empty());
  }

  #[test]
  fn disabled_pointer_ignores_motion_until_enabled() {
    let mut state = test_state();
    pointer_motion(&mut state, POINTER, 10.0, 20.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (10.0, 20.0));
    set_enabled(&mut state, POINTER, false);
    pointer_motion(&mut state, POINTER, 30.0, 40.0);
    pointer_relative_motion(&mut state, POINTER, 5.0, 5.0, 5.0, 5.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (10.0, 20.0));
    set_enabled(&mut state, POINTER, true);
    pointer_relative_motion(&mut state, POINTER, 5.0, 6.0, 5.0, 6.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (15.0, 26.0));
  }

  #[test]
  fn input_from_unknown_or_mismatched_devices_is_dropped() {
    let mut state = test_state();
    pointer_motion(&mut state, KEYBOARD, 30.0, 40.0);
    pointer_motion(&mut state, InputDeviceId(9), 30.0, 40.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (0.0, 0.0));
    assert!(!keyboard_key(&mut state, POINTER, 30, true));
    touch_down(&mut state, KEYBOARD, 1, 0.0, 0.0);
    assert!(holds_nothing(&state));
  }

  #[test]
  fn client_virtual_pointer_motion_is_not_gated() {
    let mut state = test_state();
    set_enabled(&mut state, POINTER, false);
    input::pointer_motion(&mut state, 30.0, 40.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (30.0, 40.0));
  }

  #[test]
  fn disabled_keyboard_ignores_keys_and_modifiers() {
    let mut state = test_state();
    set_enabled(&mut state, KEYBOARD, false);
    state.ipc_events.clear();
    assert!(!keyboard_key(&mut state, KEYBOARD, 30, true));
    assert!(held_keys(&state, KEYBOARD).is_empty());
    let modifiers = Modifiers {
      depressed: 64,
      ..state.input.modifiers
    };
    keyboard_modifiers(&mut state, KEYBOARD, modifiers);
    assert_eq!(state.input.modifiers.depressed, 0);
    assert!(state.ipc_events.is_empty());
    set_enabled(&mut state, KEYBOARD, true);
    assert!(keyboard_key(&mut state, KEYBOARD, 30, true));
    assert_eq!(
      held_keys(&state, KEYBOARD).into_iter().collect::<Vec<_>>(),
      [30]
    );
  }

  #[test]
  fn disabling_keyboard_releases_held_keys_and_modifiers() {
    let mut state = test_state();
    keyboard_key(&mut state, KEYBOARD, 125, true);
    keyboard_key(&mut state, KEYBOARD, 30, true);
    keyboard_modifiers(
      &mut state,
      KEYBOARD,
      Modifiers {
        depressed: 64,
        latched: 1,
        locked: 2,
        group: 0,
      },
    );
    state.ipc_events.clear();
    set_enabled(&mut state, KEYBOARD, false);
    assert!(held_keys(&state, KEYBOARD).is_empty());
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
    set_enabled(&mut state, KEYBOARD, false);
    assert!(state.ipc_events.is_empty());
  }

  #[test]
  fn disabling_pointer_releases_held_buttons() {
    let mut state = test_state();
    pointer_button(&mut state, POINTER, 0x110, true);
    pointer_button(&mut state, POINTER, 0x111, true);
    pointer_button(&mut state, POINTER, 0x111, false);
    assert_eq!(
      held(&state, POINTER)
        .buttons
        .iter()
        .copied()
        .collect::<Vec<_>>(),
      [0x110]
    );
    set_enabled(&mut state, POINTER, false);
    assert!(held(&state, POINTER).buttons.is_empty());
    pointer_button(&mut state, POINTER, 0x112, true);
    assert!(held(&state, POINTER).buttons.is_empty());
  }

  #[test]
  fn disabling_pointer_cancels_active_gesture() {
    use crate::pointer_gestures::{hold_begin, is_active, pinch_begin};
    let mut state = test_state();
    crate::pointer_gestures::swipe_begin(&mut state, POINTER, 3);
    assert!(is_active(&state));
    set_enabled(&mut state, POINTER, false);
    assert!(!is_active(&state));
    pinch_begin(&mut state, POINTER, 2);
    hold_begin(&mut state, POINTER, 2);
    assert!(!is_active(&state));
    set_enabled(&mut state, POINTER, true);
    hold_begin(&mut state, POINTER, 2);
    assert!(is_active(&state));
  }

  #[test]
  fn disabling_another_pointer_keeps_the_gesture() {
    use crate::pointer_gestures::{is_active, swipe_begin, swipe_end};
    let mut state = test_state();
    let touchpad = add(&mut state, nested_info(InputDeviceKind::Pointer));
    swipe_begin(&mut state, touchpad, 3);
    set_enabled(&mut state, POINTER, false);
    assert!(is_active(&state));
    // Only the device that started the gesture can end it.
    set_enabled(&mut state, POINTER, true);
    swipe_end(&mut state, POINTER, false);
    assert!(is_active(&state));
    assert!(remove(&mut state, touchpad));
    assert!(!is_active(&state));
  }

  #[test]
  fn disabling_touch_cancels_held_touches() {
    let mut state = test_state();
    touch_down(&mut state, TOUCH, 1, 5.0, 5.0);
    touch(&mut state, TOUCH, 2, 6.0, 6.0);
    touch_up(&mut state, TOUCH, 2);
    assert_eq!(
      held(&state, TOUCH)
        .touches
        .iter()
        .copied()
        .collect::<Vec<_>>(),
      [1]
    );
    set_enabled(&mut state, TOUCH, false);
    assert!(held(&state, TOUCH).touches.is_empty());
    touch_down(&mut state, TOUCH, 3, 5.0, 5.0);
    touch(&mut state, TOUCH, 4, 5.0, 5.0);
    assert!(held(&state, TOUCH).touches.is_empty());
  }

  #[test]
  fn disabling_one_device_keeps_the_others_held() {
    let mut state = test_state();
    keyboard_key(&mut state, KEYBOARD, 30, true);
    pointer_button(&mut state, POINTER, 0x110, true);
    touch_down(&mut state, TOUCH, 1, 0.0, 0.0);
    set_enabled(&mut state, POINTER, false);
    assert_eq!(held_keys(&state, KEYBOARD).len(), 1);
    assert_eq!(held(&state, TOUCH).touches.len(), 1);
  }

  #[test]
  fn two_keyboards_hold_keys_independently() {
    let mut state = test_state();
    let second = add(&mut state, keyboard_info("second"));
    keyboard_key(&mut state, KEYBOARD, 30, true);
    keyboard_key(&mut state, KEYBOARD, 42, true);
    keyboard_key(&mut state, second, 31, true);
    keyboard_key(&mut state, second, 42, true);
    let modifiers = Modifiers {
      depressed: 1,
      ..state.input.modifiers
    };
    keyboard_modifiers(&mut state, second, modifiers);
    set_enabled(&mut state, KEYBOARD, false);
    assert!(held_keys(&state, KEYBOARD).is_empty());
    assert_eq!(
      held_keys(&state, second).into_iter().collect::<Vec<_>>(),
      [31, 42]
    );
    // The second keyboard still holds keys, so its modifiers stay.
    assert_eq!(state.input.modifiers.depressed, 1);
    assert!(!keyboard_key(&mut state, KEYBOARD, 32, true));
    assert!(keyboard_key(&mut state, second, 32, true));
    assert_eq!(held_keys(&state, second).len(), 3);
    set_enabled(&mut state, second, false);
    assert!(holds_nothing(&state));
    assert_eq!(state.input.modifiers.depressed, 0);
  }

  #[test]
  fn a_key_two_keyboards_hold_stays_down_until_both_let_go() {
    let mut state = test_state();
    let second = add(&mut state, keyboard_info("second"));
    keyboard_key(&mut state, KEYBOARD, 30, true);
    keyboard_key(&mut state, second, 30, true);
    keyboard_key(&mut state, KEYBOARD, 30, false);
    assert_eq!(
      held_keys(&state, second).into_iter().collect::<Vec<_>>(),
      [30]
    );
    keyboard_key(&mut state, KEYBOARD, 30, true);
    set_enabled(&mut state, second, false);
    assert_eq!(held_keys(&state, KEYBOARD).len(), 1);
  }

  #[test]
  fn removing_a_device_releases_what_it_holds() {
    let mut state = test_state();
    let second = add(&mut state, keyboard_info("second"));
    keyboard_key(&mut state, second, 30, true);
    let modifiers = Modifiers {
      depressed: 4,
      ..state.input.modifiers
    };
    keyboard_modifiers(&mut state, second, modifiers);
    pointer_button(&mut state, POINTER, 0x110, true);
    state.ipc_events.clear();
    assert!(remove(&mut state, second));
    assert_eq!(state.input.modifiers.depressed, 0);
    assert!(
      state
        .ipc_events
        .iter()
        .any(|event| { event["event"] == "keyboard-modifier-state-changed" })
    );
    assert!(!keyboard_key(&mut state, second, 30, true));
    assert!(held_keys(&state, second).is_empty());
    assert_eq!(held(&state, POINTER).buttons.len(), 1);
    assert!(remove(&mut state, POINTER));
    assert!(holds_nothing(&state));
    // Removal is not an IPC change.
    assert!(device_events(&mut state).is_empty());
    assert_eq!(list_json(&state).as_array().map(Vec::len), Some(2));
  }

  #[test]
  fn stipc_uses_the_first_device_of_each_kind() {
    let mut state = bare_state();
    assert_eq!(stipc_device(&state, InputDeviceKind::Keyboard), None);
    let pointer = add(&mut state, nested_info(InputDeviceKind::Pointer));
    let first = add(&mut state, keyboard_info("first"));
    add(&mut state, keyboard_info("second"));
    assert_eq!(stipc_device(&state, InputDeviceKind::Keyboard), Some(first));
    assert_eq!(
      stipc_device(&state, InputDeviceKind::Pointer),
      Some(pointer)
    );
    assert_eq!(stipc_device(&state, InputDeviceKind::Touch), None);
  }

  #[test]
  fn deactivating_the_session_releases_all_held_input() {
    let mut state = test_state();
    let second = add(&mut state, keyboard_info("second"));
    keyboard_key(&mut state, KEYBOARD, 30, true);
    keyboard_key(&mut state, second, 31, true);
    keyboard_modifiers(
      &mut state,
      KEYBOARD,
      Modifiers {
        depressed: 64,
        latched: 1,
        locked: 2,
        group: 0,
      },
    );
    pointer_button(&mut state, POINTER, 0x110, true);
    touch_down(&mut state, TOUCH, 1, 0.0, 0.0);
    crate::pointer_gestures::swipe_begin(&mut state, POINTER, 3);
    assert!(set_session_active(&mut state, false));
    assert!(holds_nothing(&state));
    assert_eq!(state.input.modifiers.depressed, 0);
    assert_eq!(state.input.modifiers.latched, 0);
    assert_eq!(state.input.modifiers.locked, 2);
    assert!(!crate::pointer_gestures::is_active(&state));
    assert!(!set_session_active(&mut state, false));
  }

  #[test]
  fn inactive_session_drops_input_until_resumed() {
    let mut state = test_state();
    pointer_motion(&mut state, POINTER, 10.0, 20.0);
    set_session_active(&mut state, false);
    assert!(!keyboard_key(&mut state, KEYBOARD, 30, true));
    pointer_motion(&mut state, POINTER, 30.0, 40.0);
    pointer_button(&mut state, POINTER, 0x110, true);
    touch_down(&mut state, TOUCH, 1, 0.0, 0.0);
    crate::pointer_gestures::hold_begin(&mut state, POINTER, 2);
    assert!(holds_nothing(&state));
    assert!(!crate::pointer_gestures::is_active(&state));
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (10.0, 20.0));
    assert!(set_session_active(&mut state, true));
    assert!(keyboard_key(&mut state, KEYBOARD, 30, true));
    pointer_motion(&mut state, POINTER, 30.0, 40.0);
    assert_eq!((state.input.pointer_x, state.input.pointer_y), (30.0, 40.0));
  }

  #[test]
  fn session_activity_does_not_change_ipc_device_state() {
    let mut state = test_state();
    set_enabled(&mut state, TOUCH, false);
    device_events(&mut state);
    set_session_active(&mut state, false);
    assert_eq!(list_json(&state)[0]["enabled"], json!(true));
    assert!(enabled(&state, KEYBOARD));
    assert!(!delivers(&state, KEYBOARD, InputDeviceKind::Keyboard));
    set_session_active(&mut state, true);
    assert!(delivers(&state, KEYBOARD, InputDeviceKind::Keyboard));
    assert!(!delivers(&state, TOUCH, InputDeviceKind::Touch));
    assert!(device_events(&mut state).is_empty());
  }
}
