//! The registry of hotplugged input devices.
//!
//! libinput's udev backend watches udev for input devices on the seat and
//! reports them as added and removed ([`NativeEvent::Added`],
//! [`NativeEvent::Removed`]); a second udev monitor would only race it
//! (docs/design/native-backend.md, "Input"). [`DeviceRegistry`] keeps the
//! seat devices of the compositor in step with those reports.
//!
//! A Shingle seat device has exactly one kind, so a libinput device with
//! several capabilities (a keyboard with a touchpad, say) becomes one seat
//! device per kind, as wlroots makes one `wlr_input_device` per type.
//! Gestures belong to the pointer.
//!
//! libinput removes every device when it is suspended for a seat pause
//! and adds them again on resume. The registry keeps the seat devices of a
//! suspended device, so a device that comes back keeps its IPC id and its
//! enabled state; devices that do not come back are removed after the
//! resume.
//!
//! [`NativeEvent::Added`]: crate::input::NativeEvent::Added
//! [`NativeEvent::Removed`]: crate::input::NativeEvent::Removed

use std::collections::{BTreeMap, BTreeSet};

use shingle::{Compositor, InputDeviceId, InputDeviceInfo, InputDeviceKind};

use crate::input::{DeviceDescription, DeviceInput, DeviceKey};

/// The seat device kind that input of this sort belongs to.
pub fn input_kind(input: &DeviceInput) -> InputDeviceKind {
  match input {
    DeviceInput::Key { .. } => InputDeviceKind::Keyboard,
    DeviceInput::Touch { .. } => InputDeviceKind::Touch,
    DeviceInput::Motion { .. }
    | DeviceInput::MotionAbsolute { .. }
    | DeviceInput::Button { .. }
    | DeviceInput::Scroll { .. }
    | DeviceInput::Swipe(_)
    | DeviceInput::Pinch(_)
    | DeviceInput::Hold(_) => InputDeviceKind::Pointer,
  }
}

/// The seat device kinds of `device`, in registration order.
pub fn device_kinds(device: &DeviceDescription) -> Vec<InputDeviceKind> {
  let capabilities = device.capabilities;
  let mut kinds = Vec::new();
  if capabilities.keyboard {
    kinds.push(InputDeviceKind::Keyboard);
  }
  if capabilities.pointer || capabilities.gesture {
    kinds.push(InputDeviceKind::Pointer);
  }
  if capabilities.touch {
    kinds.push(InputDeviceKind::Touch);
  }
  kinds
}

/// What [`DeviceRegistry::add`] did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Added {
  /// The device is new and got these seat devices.
  New(Vec<InputDeviceId>),
  /// The device came back after a resume and kept these seat devices.
  Returned(Vec<InputDeviceId>),
  /// The device has nothing Shingle uses, such as a lid switch or a
  /// tablet.
  Ignored,
}

struct Entry {
  description: DeviceDescription,
  seat: Vec<(InputDeviceKind, InputDeviceId)>,
  /// libinput removed it for a seat pause.
  suspended: bool,
  /// Keys the device holds down.
  held: BTreeSet<u32>,
}

/// The libinput devices on the seat and their seat devices.
#[derive(Default)]
pub struct DeviceRegistry {
  devices: BTreeMap<DeviceKey, Entry>,
}

impl DeviceRegistry {
  /// Records that libinput added `device`. A new device's kinds are
  /// registered with `seat` under its name, vendor and product; a
  /// suspended device with the same event node gets its seat devices
  /// back.
  pub fn add(
    &mut self,
    seat: &mut Compositor,
    device: DeviceDescription,
  ) -> Added {
    if let Some(entry) = self.devices.get_mut(&device.key)
      && entry.suspended
      && entry.description.capabilities == device.capabilities
    {
      entry.suspended = false;
      entry.description = device;
      return Added::Returned(entry.seat.iter().map(|(_, id)| *id).collect());
    }
    // An unexpected second addition, or a different device on the node.
    self.remove(seat, &device.key);
    let kinds = device_kinds(&device);
    if kinds.is_empty() {
      return Added::Ignored;
    }
    let ids: Vec<_> = kinds
      .into_iter()
      .map(|kind| {
        let id = seat.add_input_device(InputDeviceInfo {
          name: device.name.clone(),
          vendor: Some(device.vendor),
          product: Some(device.product),
          kind,
        });
        (kind, id)
      })
      .collect();
    let added = ids.iter().map(|(_, id)| *id).collect();
    self.devices.insert(
      device.key.clone(),
      Entry {
        description: device,
        seat: ids,
        suspended: false,
        held: BTreeSet::new(),
      },
    );
    Added::New(added)
  }

  /// Records that libinput removed `device` outside a pause: its seat
  /// devices are unregistered. Returns them and the keys it still held.
  pub fn remove(
    &mut self,
    seat: &mut Compositor,
    device: &DeviceKey,
  ) -> Option<(Vec<InputDeviceId>, Vec<u32>)> {
    let entry = self.devices.remove(device)?;
    let ids: Vec<_> = entry.seat.iter().map(|(_, id)| *id).collect();
    for id in &ids {
      seat.remove_input_device(*id);
    }
    Some((ids, entry.held.into_iter().collect()))
  }

  /// Marks every device as removed for a seat pause, keeping its seat
  /// devices, and forgets the keys they hold: the pause releases them.
  pub fn suspend(&mut self) {
    for entry in self.devices.values_mut() {
      entry.suspended = true;
      entry.held.clear();
    }
  }

  /// Unregisters the devices that did not come back after a resume, as
  /// when they were unplugged during the pause. Returns their event
  /// nodes.
  pub fn remove_suspended(&mut self, seat: &mut Compositor) -> Vec<DeviceKey> {
    let gone: Vec<DeviceKey> = self
      .devices
      .iter()
      .filter(|(_, entry)| entry.suspended)
      .map(|(key, _)| key.clone())
      .collect();
    for key in &gone {
      self.remove(seat, key);
    }
    gone
  }

  /// The seat device of `kind` for `device`, if it has one and is not
  /// suspended.
  pub fn seat_device(
    &self,
    device: &DeviceKey,
    kind: InputDeviceKind,
  ) -> Option<InputDeviceId> {
    let entry = self.devices.get(device).filter(|entry| !entry.suspended)?;
    entry
      .seat
      .iter()
      .find(|(entry_kind, _)| *entry_kind == kind)
      .map(|(_, id)| *id)
  }

  /// The libinput device a seat device belongs to.
  pub fn owner(&self, id: InputDeviceId) -> Option<&DeviceDescription> {
    self
      .devices
      .values()
      .find(|entry| entry.seat.iter().any(|(_, seat_id)| *seat_id == id))
      .map(|entry| &entry.description)
  }

  /// Records a key event of `device`. Returns false for a release of a
  /// key the device does not hold, such as libinput's own release of a
  /// key Shingle already released when IPC disabled the device or the
  /// seat paused, so it is not handled twice.
  pub fn track_key(
    &mut self,
    device: &DeviceKey,
    code: u32,
    pressed: bool,
  ) -> bool {
    let Some(entry) = self.devices.get_mut(device) else {
      return false;
    };
    if pressed {
      entry.held.insert(code);
      true
    } else {
      entry.held.remove(&code)
    }
  }

  /// Forgets and returns the keys `device` holds.
  pub fn take_held_keys(&mut self, device: &DeviceKey) -> Vec<u32> {
    self
      .devices
      .get_mut(device)
      .map(|entry| std::mem::take(&mut entry.held).into_iter().collect())
      .unwrap_or_default()
  }

  /// Whether libinput should send `device`'s events: while any of its seat
  /// devices is enabled. A keyboard with a touchpad whose keyboard IPC
  /// disabled keeps sending; Shingle drops the keys.
  pub fn wants_events(&self, seat: &Compositor, device: &DeviceKey) -> bool {
    self.devices.get(device).is_some_and(|entry| {
      entry
        .seat
        .iter()
        .any(|(_, id)| seat.input_device_enabled(*id))
    })
  }

  /// The devices with a keyboard, for LED updates.
  pub fn keyboards(&self) -> impl Iterator<Item = &DeviceKey> {
    self
      .devices
      .iter()
      .filter(|(_, entry)| {
        !entry.suspended
          && entry
            .seat
            .iter()
            .any(|(kind, _)| *kind == InputDeviceKind::Keyboard)
      })
      .map(|(key, _)| key)
  }

  /// The number of libinput devices, suspended or not.
  pub fn len(&self) -> usize {
    self.devices.len()
  }

  /// Whether there are no devices.
  pub fn is_empty(&self) -> bool {
    self.devices.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::input::{
    Capabilities,
    tests::{POINTER, device, key},
  };

  /// A compositor on a private socket, for its device registry.
  fn seat() -> Compositor {
    crate::tests::compositor()
  }

  fn kinds(seat: &Compositor, ids: &[InputDeviceId]) -> Vec<InputDeviceKind> {
    ids
      .iter()
      .map(|id| seat.input_device(*id).expect("registered").kind)
      .collect()
  }

  const KEYBOARD: Capabilities = Capabilities {
    keyboard: true,
    pointer: false,
    touch: false,
    gesture: false,
  };

  #[test]
  fn a_combined_device_gets_one_seat_device_per_kind() {
    let mut seat = seat();
    let mut registry = DeviceRegistry::default();
    let combo = device(
      "event4",
      Capabilities {
        keyboard: true,
        gesture: true,
        ..POINTER
      },
      2,
    );
    let Added::New(ids) = registry.add(&mut seat, combo) else {
      panic!("a new device");
    };
    assert_eq!(
      kinds(&seat, &ids),
      [InputDeviceKind::Keyboard, InputDeviceKind::Pointer]
    );
    let info = seat.input_device(ids[0]).expect("registered");
    assert_eq!(info.name, "event4 device");
    assert_eq!((info.vendor, info.product), (Some(0x046d), Some(0xc52b)));
    assert_eq!(
      registry.seat_device(&key("event4"), InputDeviceKind::Pointer),
      Some(ids[1])
    );
    assert_eq!(
      registry.seat_device(&key("event4"), InputDeviceKind::Touch),
      None
    );
    assert_eq!(registry.owner(ids[1]).map(|d| &d.key), Some(&key("event4")));
  }

  #[test]
  fn hotplug_adds_and_removes_seat_devices() {
    let mut seat = seat();
    let mut registry = DeviceRegistry::default();
    let Added::New(mouse) =
      registry.add(&mut seat, device("event3", POINTER, 0))
    else {
      panic!("a new device");
    };
    registry.add(&mut seat, device("event1", KEYBOARD, 0));
    registry.track_key(&key("event1"), 30, true);
    let (ids, held) = registry
      .remove(&mut seat, &key("event1"))
      .expect("it was there");
    assert_eq!(ids.len(), 1);
    assert_eq!(held, [30], "keys held at unplug are reported");
    assert!(seat.input_device(ids[0]).is_none(), "unregistered");
    assert!(seat.input_device(mouse[0]).is_some());
    assert!(registry.remove(&mut seat, &key("event1")).is_none());
    // Plugged in again, it is a new device with a new id.
    let Added::New(again) =
      registry.add(&mut seat, device("event1", KEYBOARD, 0))
    else {
      panic!("a new device");
    };
    assert_ne!(again, ids);
  }

  #[test]
  fn switches_and_tablets_are_ignored() {
    let mut seat = seat();
    let mut registry = DeviceRegistry::default();
    let lid = device("event0", Capabilities::default(), 0);
    assert_eq!(registry.add(&mut seat, lid), Added::Ignored);
    assert!(registry.is_empty());
  }

  #[test]
  fn devices_keep_their_ids_across_a_pause() {
    let mut seat = seat();
    let mut registry = DeviceRegistry::default();
    let Added::New(mouse) =
      registry.add(&mut seat, device("event3", POINTER, 0))
    else {
      panic!("a new device");
    };
    let Added::New(keyboard) =
      registry.add(&mut seat, device("event1", KEYBOARD, 0))
    else {
      panic!("a new device");
    };
    registry.track_key(&key("event1"), 30, true);
    registry.suspend();
    assert_eq!(
      registry.seat_device(&key("event3"), InputDeviceKind::Pointer),
      None,
      "suspended devices take no input"
    );
    assert_eq!(
      registry.add(&mut seat, device("event3", POINTER, 0)),
      Added::Returned(mouse.clone())
    );
    // The keyboard was unplugged on the other VT.
    assert_eq!(registry.remove_suspended(&mut seat), [key("event1")]);
    assert!(seat.input_device(keyboard[0]).is_none());
    assert!(seat.input_device(mouse[0]).is_some());
    assert_eq!(
      registry.seat_device(&key("event3"), InputDeviceKind::Pointer),
      Some(mouse[0])
    );
  }

  #[test]
  fn releases_of_keys_not_held_are_dropped() {
    let mut seat = seat();
    let mut registry = DeviceRegistry::default();
    registry.add(&mut seat, device("event1", KEYBOARD, 0));
    let keyboard = key("event1");
    assert!(registry.track_key(&keyboard, 30, true));
    assert!(registry.track_key(&keyboard, 31, true));
    assert_eq!(registry.take_held_keys(&keyboard), [30, 31]);
    assert!(
      !registry.track_key(&keyboard, 30, false),
      "already released"
    );
    assert!(registry.track_key(&keyboard, 32, true));
    registry.suspend();
    assert!(
      !registry.track_key(&keyboard, 32, false),
      "the pause released it"
    );
    assert!(
      !registry.track_key(&key("event9"), 30, true),
      "unknown device"
    );
  }

  #[test]
  fn libinput_events_stop_only_when_every_kind_is_disabled() {
    let mut seat = seat();
    let mut registry = DeviceRegistry::default();
    let combo = device(
      "event4",
      Capabilities {
        keyboard: true,
        ..POINTER
      },
      0,
    );
    let Added::New(ids) = registry.add(&mut seat, combo) else {
      panic!("a new device");
    };
    let combo = key("event4");
    assert!(registry.wants_events(&seat, &combo));
    crate::tests::disable(&mut seat, ids[0]);
    assert!(
      registry.wants_events(&seat, &combo),
      "the pointer still works"
    );
    crate::tests::disable(&mut seat, ids[1]);
    assert!(!registry.wants_events(&seat, &combo));
    assert_eq!(registry.keyboards().collect::<Vec<_>>(), [&combo]);
  }

  #[test]
  fn input_goes_to_the_seat_device_of_its_kind() {
    use shingle_frontend::host::{Gesture, TouchPhase};
    let cases = [
      (
        DeviceInput::Key {
          code: 1,
          pressed: true,
        },
        InputDeviceKind::Keyboard,
      ),
      (
        DeviceInput::Button {
          code: 0x110,
          pressed: true,
        },
        InputDeviceKind::Pointer,
      ),
      (
        DeviceInput::Pinch(Gesture::Begin { fingers: 2 }),
        InputDeviceKind::Pointer,
      ),
      (
        DeviceInput::Touch {
          slot: 0,
          phase: TouchPhase::Down,
          position: Some((1.0, 1.0)),
        },
        InputDeviceKind::Touch,
      ),
    ];
    for (input, kind) in cases {
      assert_eq!(input_kind(&input), kind, "{input:?}");
    }
  }
}
