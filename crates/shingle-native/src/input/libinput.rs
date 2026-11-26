//! [`InputSource`] over libinput's udev backend.
//!
//! Compiled and checked on every host but only ever run on a machine whose
//! seat Shingle may take (see docs/native-testing.md); unit tests never
//! construct it, because creating it scans udev and opens evdev nodes.

use std::{
  cell::RefCell,
  collections::HashMap,
  io,
  os::fd::{AsFd, BorrowedFd, OwnedFd},
  path::Path,
  rc::Rc,
};

use input::ffi as sys;
use input::{
  AsRaw, Device, DeviceCapability, Libinput, LibinputInterface,
  event::{
    DeviceEvent, Event, EventTrait, GestureEvent, KeyboardEvent, PointerEvent,
    TouchEvent,
    gesture::{
      GestureEndEvent, GestureEventCoordinates, GestureEventTrait,
      GestureHoldEvent, GesturePinchEvent, GesturePinchEventTrait,
      GestureSwipeEvent,
    },
    keyboard::{KeyState, KeyboardEventTrait},
    pointer::{
      Axis as PointerAxis, ButtonState, PointerEventTrait, PointerScrollEvent,
    },
    touch::{TouchEventPosition, TouchEventSlot, TouchEventTrait},
  },
};
use shingle_frontend::{
  host::{Gesture, TouchPhase},
  keyboard::Leds,
};

use super::{
  AccelProfile, Capabilities, ClickMethod, DeviceDescription, DeviceInput,
  DeviceKey, InputSource, NativeEvent, ScrollAxis, ScrollMethod, ScrollSource,
  SendEvents, Setting,
};
use crate::session::Session;

/// Opens libinput's devices through the seat, remembering nothing: the
/// session maps each descriptor back to its seat device.
struct SeatInterface<S> {
  session: Rc<RefCell<S>>,
}

impl<S: Session + 'static> LibinputInterface for SeatInterface<S> {
  fn open_restricted(
    &mut self,
    path: &Path,
    _flags: i32,
  ) -> Result<OwnedFd, i32> {
    self
      .session
      .borrow_mut()
      .open_device(path)
      .map_err(|error| error.raw_os_error().unwrap_or(EIO))
  }

  fn close_restricted(&mut self, fd: OwnedFd) {
    if let Err(error) = self.session.borrow_mut().close_device(fd) {
      crate::diagnostics::log(&format!("closing an input device: {error}"));
    }
  }
}

/// `EIO`, for errors without an errno.
const EIO: i32 = 5;

/// libinput on the seat's udev devices.
pub struct LibinputSource {
  libinput: Libinput,
  /// The devices libinput has added, for configuration.
  devices: HashMap<DeviceKey, Device>,
}

impl LibinputSource {
  /// Creates a libinput context on `session`'s seat. The devices are
  /// added on the first [`InputSource::dispatch`].
  pub fn new<S: Session + 'static>(
    session: Rc<RefCell<S>>,
  ) -> io::Result<Self> {
    let seat = session.borrow().seat_name().to_owned();
    let mut libinput = Libinput::new_with_udev(SeatInterface { session });
    libinput.udev_assign_seat(&seat).map_err(|()| {
      io::Error::other(format!("libinput could not use seat {seat}"))
    })?;
    Ok(Self {
      libinput,
      devices: HashMap::new(),
    })
  }

  /// Takes every queued libinput event.
  fn drain(&mut self, output_size: (u32, u32)) -> Vec<NativeEvent> {
    let mut events = Vec::new();
    while let Some(event) = self.libinput.next() {
      if let Some(event) = self.convert(event, output_size) {
        events.push(event);
      }
    }
    events
  }

  fn convert(
    &mut self,
    event: Event,
    (width, height): (u32, u32),
  ) -> Option<NativeEvent> {
    let device = event.device();
    let key = DeviceKey(device.sysname().to_owned());
    let (time_usec, input) = match event {
      Event::Device(DeviceEvent::Added(_)) => {
        let description = describe(&device, key.clone());
        self.devices.insert(key, device);
        return Some(NativeEvent::Added(description));
      }
      Event::Device(DeviceEvent::Removed(_)) => {
        self.devices.remove(&key);
        return Some(NativeEvent::Removed(key));
      }
      Event::Keyboard(KeyboardEvent::Key(event)) => (
        event.time_usec(),
        DeviceInput::Key {
          code: event.key(),
          pressed: event.key_state() == KeyState::Pressed,
        },
      ),
      Event::Pointer(PointerEvent::Motion(event)) => (
        event.time_usec(),
        DeviceInput::Motion {
          dx: event.dx(),
          dy: event.dy(),
          dx_unaccel: event.dx_unaccelerated(),
          dy_unaccel: event.dy_unaccelerated(),
        },
      ),
      Event::Pointer(PointerEvent::MotionAbsolute(event)) => (
        event.time_usec(),
        DeviceInput::MotionAbsolute {
          x: event.absolute_x_transformed(width),
          y: event.absolute_y_transformed(height),
        },
      ),
      Event::Pointer(PointerEvent::Button(event)) => (
        event.time_usec(),
        DeviceInput::Button {
          code: event.button(),
          pressed: event.button_state() == ButtonState::Pressed,
        },
      ),
      Event::Pointer(PointerEvent::ScrollWheel(event)) => (
        event.time_usec(),
        DeviceInput::Scroll {
          source: ScrollSource::Wheel,
          horizontal: event.has_axis(PointerAxis::Horizontal).then(|| {
            ScrollAxis {
              value: event.scroll_value(PointerAxis::Horizontal),
              v120: event.scroll_value_v120(PointerAxis::Horizontal),
            }
          }),
          vertical: event.has_axis(PointerAxis::Vertical).then(|| ScrollAxis {
            value: event.scroll_value(PointerAxis::Vertical),
            v120: event.scroll_value_v120(PointerAxis::Vertical),
          }),
        },
      ),
      Event::Pointer(PointerEvent::ScrollFinger(event)) => (
        event.time_usec(),
        smooth_scroll(&event, ScrollSource::Finger),
      ),
      Event::Pointer(PointerEvent::ScrollContinuous(event)) => (
        event.time_usec(),
        smooth_scroll(&event, ScrollSource::Continuous),
      ),
      Event::Touch(TouchEvent::Down(event)) => (
        event.time_usec(),
        DeviceInput::Touch {
          slot: event.seat_slot(),
          phase: TouchPhase::Down,
          position: Some((
            event.x_transformed(width),
            event.y_transformed(height),
          )),
        },
      ),
      Event::Touch(TouchEvent::Motion(event)) => (
        event.time_usec(),
        DeviceInput::Touch {
          slot: event.seat_slot(),
          phase: TouchPhase::Motion,
          position: Some((
            event.x_transformed(width),
            event.y_transformed(height),
          )),
        },
      ),
      Event::Touch(TouchEvent::Up(event)) => (
        event.time_usec(),
        DeviceInput::Touch {
          slot: event.seat_slot(),
          phase: TouchPhase::Up,
          position: None,
        },
      ),
      Event::Touch(TouchEvent::Cancel(event)) => (
        event.time_usec(),
        DeviceInput::Touch {
          slot: event.seat_slot(),
          phase: TouchPhase::Cancel,
          position: None,
        },
      ),
      Event::Gesture(GestureEvent::Swipe(event)) => swipe(event),
      Event::Gesture(GestureEvent::Pinch(event)) => pinch(event),
      Event::Gesture(GestureEvent::Hold(event)) => hold(event),
      // Touch frames, the deprecated axis events (libinput sends the
      // scroll events as well), switches and tablets.
      _ => return None,
    };
    Some(NativeEvent::Input {
      device: key,
      time_usec,
      input,
    })
  }
}

fn smooth_scroll(
  event: &impl PointerScrollEvent,
  source: ScrollSource,
) -> DeviceInput {
  let axis = |axis| {
    event.has_axis(axis).then(|| ScrollAxis {
      value: event.scroll_value(axis),
      v120: 0.0,
    })
  };
  DeviceInput::Scroll {
    source,
    horizontal: axis(PointerAxis::Horizontal),
    vertical: axis(PointerAxis::Vertical),
  }
}

fn swipe(event: GestureSwipeEvent) -> (u64, DeviceInput) {
  let gesture = match &event {
    GestureSwipeEvent::Begin(begin) => Gesture::Begin {
      fingers: fingers(begin.finger_count()),
    },
    GestureSwipeEvent::Update(update) => Gesture::Update {
      dx: update.dx(),
      dy: update.dy(),
      scale: 1.0,
      rotation: 0.0,
    },
    GestureSwipeEvent::End(end) => Gesture::End {
      cancelled: end.cancelled(),
    },
    _ => Gesture::End { cancelled: true },
  };
  (event.time_usec(), DeviceInput::Swipe(gesture))
}

fn pinch(event: GesturePinchEvent) -> (u64, DeviceInput) {
  let gesture = match &event {
    GesturePinchEvent::Begin(begin) => Gesture::Begin {
      fingers: fingers(begin.finger_count()),
    },
    GesturePinchEvent::Update(update) => Gesture::Update {
      dx: update.dx(),
      dy: update.dy(),
      scale: update.scale(),
      rotation: update.angle_delta(),
    },
    GesturePinchEvent::End(end) => Gesture::End {
      cancelled: end.cancelled(),
    },
    _ => Gesture::End { cancelled: true },
  };
  (event.time_usec(), DeviceInput::Pinch(gesture))
}

fn hold(event: GestureHoldEvent) -> (u64, DeviceInput) {
  let gesture = match &event {
    GestureHoldEvent::Begin(begin) => Gesture::Begin {
      fingers: fingers(begin.finger_count()),
    },
    GestureHoldEvent::End(end) => Gesture::End {
      cancelled: end.cancelled(),
    },
    _ => Gesture::End { cancelled: true },
  };
  (event.time_usec(), DeviceInput::Hold(gesture))
}

fn fingers(count: i32) -> u32 {
  u32::try_from(count).unwrap_or(0)
}

/// What Shingle needs to know about a libinput device.
fn describe(device: &Device, key: DeviceKey) -> DeviceDescription {
  DeviceDescription {
    key,
    name: device.name().into_owned(),
    vendor: device.id_vendor(),
    product: device.id_product(),
    capabilities: Capabilities {
      keyboard: device.has_capability(DeviceCapability::Keyboard),
      pointer: device.has_capability(DeviceCapability::Pointer),
      touch: device.has_capability(DeviceCapability::Touch),
      gesture: device.has_capability(DeviceCapability::Gesture),
    },
    tap_fingers: device.config_tap_finger_count(),
    udev: udev_properties(device),
  }
}

/// The udev properties a per-device config entry may name, with their
/// values, in Wayfire's order.
fn udev_properties(device: &Device) -> Vec<(String, String)> {
  // SAFETY: libinput's udev backend created the device from its own udev
  // context, which the returned handle references.
  let Some(udev) = (unsafe { device.udev_device() }) else {
    return Vec::new();
  };
  shingle_config::UDEV_PROPERTIES
    .iter()
    .filter_map(|name| {
      let value = udev.property_value(name)?.to_str()?;
      Some(((*name).to_owned(), value.to_owned()))
    })
    .collect()
}

/// Applies one setting; a device that does not support it says so, which
/// is not an error.
fn apply(device: &mut Device, setting: Setting) {
  let _ = match setting {
    Setting::LeftHanded(enabled) => device.config_left_handed_set(enabled),
    Setting::MiddleEmulation(enabled) => {
      device.config_middle_emulation_set_enabled(enabled)
    }
    Setting::AccelSpeed(speed) => device.config_accel_set_speed(speed),
    Setting::AccelProfile(profile) => {
      let profile = match profile {
        AccelProfile::Flat => Some(input::AccelProfile::Flat),
        AccelProfile::Adaptive => Some(input::AccelProfile::Adaptive),
        AccelProfile::DeviceDefault => device.config_accel_default_profile(),
      };
      match profile {
        Some(profile) => device.config_accel_set_profile(profile),
        None => Ok(()),
      }
    }
    Setting::Tap(enabled) => device.config_tap_set_enabled(enabled),
    Setting::ClickMethod(method) => match method {
      ClickMethod::ButtonAreas => {
        device.config_click_set_method(input::ClickMethod::ButtonAreas)
      }
      ClickMethod::Clickfinger => {
        device.config_click_set_method(input::ClickMethod::Clickfinger)
      }
      ClickMethod::DeviceDefault => {
        match device.config_click_default_method() {
          Some(method) => device.config_click_set_method(method),
          None => set_no_click_method(device),
        }
      }
      ClickMethod::None => set_no_click_method(device),
    },
    Setting::ScrollMethod(method) => {
      let method = match method {
        ScrollMethod::None => Some(input::ScrollMethod::NoScroll),
        ScrollMethod::TwoFinger => Some(input::ScrollMethod::TwoFinger),
        ScrollMethod::Edge => Some(input::ScrollMethod::Edge),
        ScrollMethod::OnButtonDown => Some(input::ScrollMethod::OnButtonDown),
        ScrollMethod::DeviceDefault => device.config_scroll_default_method(),
      };
      match method {
        Some(method) => device.config_scroll_set_method(method),
        None => Ok(()),
      }
    }
    Setting::DisableWhileTyping(enabled) => {
      device.config_dwt_set_enabled(enabled)
    }
    Setting::TapDrag(enabled) => device.config_tap_set_drag_enabled(enabled),
    Setting::DragLock(enabled) => {
      device.config_tap_set_drag_lock_enabled(if enabled {
        input::DragLockState::EnabledTimeout
      } else {
        input::DragLockState::Disabled
      })
    }
    Setting::NaturalScroll(enabled) => {
      if device.config_scroll_has_natural_scroll() {
        device.config_scroll_set_natural_scroll_enabled(enabled)
      } else {
        Ok(())
      }
    }
    Setting::Calibration(matrix) => {
      if device.config_calibration_has_matrix() {
        device.config_calibration_set_matrix(matrix)
      } else {
        Ok(())
      }
    }
  };
}

/// Sets `LIBINPUT_CONFIG_CLICK_METHOD_NONE`, which the `input` crate has
/// no variant for.
fn set_no_click_method(device: &mut Device) -> input::DeviceConfigResult {
  // SAFETY: the device pointer is valid while `device` is borrowed.
  let status = unsafe {
    sys::libinput_device_config_click_set_method(
      device.as_raw_mut(),
      sys::libinput_config_click_method_LIBINPUT_CONFIG_CLICK_METHOD_NONE,
    )
  };
  if status == sys::libinput_config_status_LIBINPUT_CONFIG_STATUS_SUCCESS {
    Ok(())
  } else {
    Err(input::DeviceConfigError::Unsupported)
  }
}

impl InputSource for LibinputSource {
  fn dispatch(
    &mut self,
    output_size: (u32, u32),
  ) -> io::Result<Vec<NativeEvent>> {
    self.libinput.dispatch()?;
    Ok(self.drain(output_size))
  }

  fn suspend(&mut self, output_size: (u32, u32)) -> Vec<NativeEvent> {
    self.libinput.suspend();
    self.drain(output_size)
  }

  fn resume(
    &mut self,
    output_size: (u32, u32),
  ) -> io::Result<Vec<NativeEvent>> {
    self
      .libinput
      .resume()
      .map_err(|()| io::Error::other("libinput could not resume"))?;
    self.libinput.dispatch()?;
    Ok(self.drain(output_size))
  }

  fn configure(&mut self, device: &DeviceKey, settings: &[Setting]) {
    if let Some(device) = self.devices.get_mut(device) {
      for setting in settings {
        apply(device, *setting);
      }
    }
  }

  fn set_send_events(&mut self, device: &DeviceKey, mode: SendEvents) {
    let Some(device) = self.devices.get_mut(device) else {
      return;
    };
    let mode = match mode {
      SendEvents::Enabled => input::SendEventsMode::ENABLED,
      SendEvents::Disabled => input::SendEventsMode::DISABLED,
      SendEvents::DisabledOnExternalMouse => {
        input::SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE
      }
    };
    if let Err(error) = device.config_send_events_set_mode(mode) {
      crate::diagnostics::log(&format!(
        "{}: send-events mode not supported ({error:?})",
        device.name()
      ));
    }
  }

  fn set_leds(&mut self, leds: Leds) {
    let mut mask = input::Led::empty();
    if leds.caps_lock {
      mask |= input::Led::CAPSLOCK;
    }
    if leds.num_lock {
      mask |= input::Led::NUMLOCK;
    }
    if leds.scroll_lock {
      mask |= input::Led::SCROLLLOCK;
    }
    for device in self.devices.values_mut() {
      if device.has_capability(DeviceCapability::Keyboard) {
        device.led_update(mask);
      }
    }
  }

  fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
    Some(self.libinput.as_fd())
  }
}
