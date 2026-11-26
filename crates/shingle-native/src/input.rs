//! Input devices: libinput events as plain data, their translation into
//! the frontend's [`InputEvent`]s, and per-device configuration.
//!
//! [`InputSource`] is the native backend's view of libinput:
//! [`NativeEvent`]s out, configuration and send-events modes in.
//! [`LibinputSource`] implements it over libinput's udev backend, opening
//! devices through the [`Session`](crate::session::Session). Everything
//! else here is pure and unit-tested: [`Translator`] turns device events
//! into frontend input in output coordinates, and [`device_settings`]
//! derives the libinput configuration of a device from Wayfire's `[input]`
//! options the way Wayfire's pointing-device.cpp does.

mod libinput;

use std::{collections::HashMap, io, os::fd::BorrowedFd};

use shingle::OutputHandle;
use std::collections::BTreeMap;

use shingle::InputDeviceSections;
use shingle_config::{
  InputConfig, InputDeviceConfig, ShingleConfig, matching_device,
};
use shingle_core::Rect;
use shingle_frontend::{
  host::{Axis, AxisSource, Gesture, InputEvent, Touch, TouchPhase},
  keyboard::Leds,
};

pub use libinput::LibinputSource;

/// One libinput device, named by its kernel event node, such as `event5`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceKey(pub String);

/// The capabilities of a libinput device that Shingle uses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
  /// It has keys.
  pub keyboard: bool,
  /// It moves a pointer or has buttons.
  pub pointer: bool,
  /// It is a touchscreen.
  pub touch: bool,
  /// It reports touchpad gestures.
  pub gesture: bool,
}

/// A device libinput added.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceDescription {
  /// Its event node.
  pub key: DeviceKey,
  /// Its name, such as `Logitech USB Receiver`.
  pub name: String,
  /// The USB or Bluetooth vendor ID.
  pub vendor: u32,
  /// The product ID.
  pub product: u32,
  /// What it can do.
  pub capabilities: Capabilities,
  /// The number of fingers it can tap with. Wayfire treats a device with
  /// any as a touchpad.
  pub tap_fingers: u32,
  /// The udev properties of [`shingle_config::UDEV_PROPERTIES`] it has,
  /// with their values, which a per-device config entry may name.
  pub udev: Vec<(String, String)>,
}

impl DeviceDescription {
  /// Whether Wayfire's options for touchpads rather than mice apply.
  pub fn is_touchpad(&self) -> bool {
    self.tap_fingers > 0
  }

  /// The value of udev property `name`, if the device has it.
  pub fn udev_property(&self, name: &str) -> Option<String> {
    self
      .udev
      .iter()
      .find(|(property, _)| property == name)
      .map(|(_, value)| value.clone())
  }
}

/// The input options of every device: `[input]` and the per-device
/// entries (`input_devices`, Wayfire's `[input:NAME]` and
/// `[input-device:NAME]`), which a device finds by its udev properties or
/// its name ([`shingle_config::matching_device`]).
#[derive(Clone, Debug, Default)]
pub struct DeviceOptions {
  /// The `[input]` options.
  pub input: InputConfig,
  /// The per-device entries, by the name they are found under.
  pub devices: BTreeMap<String, InputDeviceConfig>,
}

impl DeviceOptions {
  /// The input options of `config`.
  pub fn from_config(config: &ShingleConfig) -> Self {
    Self {
      input: config.input.clone(),
      devices: config.input_devices.clone(),
    }
  }

  /// The entry whose libinput options `device` takes, with its name.
  pub fn input_entry(
    &self,
    device: &DeviceDescription,
  ) -> Option<(&str, &InputDeviceConfig)> {
    matching_device(
      &self.devices,
      &device.name,
      |name| device.udev_property(name),
      InputDeviceConfig::has_input_options,
    )
  }

  /// The entry whose output or calibration `device` takes, with its
  /// name.
  pub fn device_entry(
    &self,
    device: &DeviceDescription,
  ) -> Option<(&str, &InputDeviceConfig)> {
    matching_device(
      &self.devices,
      &device.name,
      |name| device.udev_property(name),
      |entry| entry.output.is_some() || entry.calibration.is_some(),
    )
  }

  /// The options `device` gets: `[input]`, with its entry's in their
  /// place.
  pub fn config_for(&self, device: &DeviceDescription) -> InputConfig {
    match self.input_entry(device) {
      Some((_, entry)) => entry.input_config(&self.input),
      None => self.input.clone(),
    }
  }

  /// The libinput configuration of `device`: [`device_settings`] of its
  /// options and, for a touch device, its entry's calibration matrix, as
  /// Wayfire calibrates touch screens.
  pub fn settings(&self, device: &DeviceDescription) -> Vec<Setting> {
    let mut settings = device_settings(&self.config_for(device), device);
    if device.capabilities.touch
      && let Some(matrix) = self
        .device_entry(device)
        .and_then(|(_, entry)| entry.calibration)
    {
      settings.push(Setting::Calibration(matrix));
    }
    settings
  }

  /// The send-events mode of `device` while it is enabled.
  pub fn enabled_mode(&self, device: &DeviceDescription) -> SendEvents {
    enabled_mode(&self.config_for(device), device)
  }

  /// The name `device`'s output mapping is found under: its entry's that
  /// has an output, else its own.
  pub fn output_key<'a>(&'a self, device: &'a DeviceDescription) -> &'a str {
    self
      .device_entry(device)
      .filter(|(_, entry)| entry.output.is_some())
      .map_or(&device.name, |(name, _)| name)
  }

  /// The sections `input/list-devices` reports for `device`.
  pub fn sections(&self, device: &DeviceDescription) -> InputDeviceSections {
    InputDeviceSections {
      input: Some(match self.input_entry(device) {
        Some((name, _)) => format!("input:{name}"),
        None => "input".into(),
      }),
      input_device: self
        .device_entry(device)
        .map(|(name, _)| format!("input-device:{name}")),
    }
  }
}

/// Where a scroll comes from, as libinput reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollSource {
  /// A wheel, in clicks.
  Wheel,
  /// Fingers on a touchpad.
  Finger,
  /// A continuous source, such as button scrolling.
  Continuous,
}

/// One axis of a scroll event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollAxis {
  /// libinput's scroll value: degrees for wheels (15 per click), pointer
  /// units otherwise. 0 on a finger or continuous source ends the scroll.
  pub value: f64,
  /// For wheels, the v120 value: 120 per logical click.
  pub v120: f64,
}

/// What a device did, in libinput's terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeviceInput {
  /// A key, by evdev code.
  Key {
    /// The evdev code.
    code: u32,
    /// Whether it went down.
    pressed: bool,
  },
  /// Relative pointer motion.
  Motion {
    /// Accelerated horizontal motion.
    dx: f64,
    /// Accelerated vertical motion.
    dy: f64,
    /// Unaccelerated horizontal motion.
    dx_unaccel: f64,
    /// Unaccelerated vertical motion.
    dy_unaccel: f64,
  },
  /// Absolute pointer motion, transformed to output pixels.
  MotionAbsolute {
    /// Horizontal position.
    x: f64,
    /// Vertical position.
    y: f64,
  },
  /// A pointer button, by evdev code.
  Button {
    /// The evdev code.
    code: u32,
    /// Whether it went down.
    pressed: bool,
  },
  /// Scrolling on one or both axes. An axis is `None` if the event does
  /// not have it.
  Scroll {
    /// Where the scroll comes from.
    source: ScrollSource,
    /// The horizontal axis.
    horizontal: Option<ScrollAxis>,
    /// The vertical axis.
    vertical: Option<ScrollAxis>,
  },
  /// A touch point, by seat slot, in output pixels. Up and cancel carry
  /// no position.
  Touch {
    /// libinput's seat slot.
    slot: u32,
    /// What happened.
    phase: TouchPhase,
    /// The position, for down and motion.
    position: Option<(f64, f64)>,
  },
  /// A touchpad swipe.
  Swipe(Gesture),
  /// A touchpad pinch.
  Pinch(Gesture),
  /// A touchpad hold.
  Hold(Gesture),
}

/// An event from libinput.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeEvent {
  /// A device appeared, at startup, on hotplug or on resume.
  Added(DeviceDescription),
  /// A device went away, on unplug or on suspend.
  Removed(DeviceKey),
  /// A device did something at `time_usec` (`CLOCK_MONOTONIC`).
  Input {
    /// The device.
    device: DeviceKey,
    /// libinput's timestamp in microseconds.
    time_usec: u64,
    /// What happened.
    input: DeviceInput,
  },
}

/// A libinput send-events mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendEvents {
  /// The device sends events.
  Enabled,
  /// The device sends none, as for a device IPC disabled.
  Disabled,
  /// A touchpad sends none while an external mouse is connected.
  DisabledOnExternalMouse,
}

/// An acceleration profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccelProfile {
  /// The device's default profile.
  DeviceDefault,
  /// Constant acceleration.
  Flat,
  /// Acceleration that grows with speed.
  Adaptive,
}

/// How a touchpad click picks its button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClickMethod {
  /// The device's default method.
  DeviceDefault,
  /// No software click method.
  None,
  /// Areas at the bottom of the touchpad.
  ButtonAreas,
  /// The number of fingers on the touchpad.
  Clickfinger,
}

/// How a touchpad scrolls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollMethod {
  /// The device's default method.
  DeviceDefault,
  /// Not at all.
  None,
  /// With two fingers.
  TwoFinger,
  /// Along the edge.
  Edge,
  /// By moving while a button is held.
  OnButtonDown,
}

/// One libinput configuration call for a device.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Setting {
  /// Swap the left and right buttons.
  LeftHanded(bool),
  /// Emulate the middle button with left and right together.
  MiddleEmulation(bool),
  /// Pointer speed, from -1 to 1.
  AccelSpeed(f64),
  /// Acceleration profile.
  AccelProfile(AccelProfile),
  /// Tap to click.
  Tap(bool),
  /// Click method.
  ClickMethod(ClickMethod),
  /// Scroll method.
  ScrollMethod(ScrollMethod),
  /// Disable while typing.
  DisableWhileTyping(bool),
  /// Tap and drag.
  TapDrag(bool),
  /// Drag lock.
  DragLock(bool),
  /// Natural scrolling, where the device supports it.
  NaturalScroll(bool),
  /// A touch device's calibration matrix, where it supports one.
  Calibration([f32; 6]),
}

/// Returns the libinput configuration `config` gives `device`, in the
/// order Wayfire applies it (wayfire/src/core/seat/pointing-device.cpp,
/// `update_options`). Only pointers are configured. Touchpads (devices
/// that can tap) get the `touchpad_*` options and the tap, click, scroll,
/// typing and drag options; other pointers get the `mouse_*` ones.
///
/// An acceleration profile of `none` sets nothing: Wayfire passes
/// libinput's `ACCEL_PROFILE_NONE`, which libinput rejects, so the
/// device's profile stays as it was.
pub fn device_settings(
  config: &InputConfig,
  device: &DeviceDescription,
) -> Vec<Setting> {
  if !device.capabilities.pointer {
    return Vec::new();
  }
  let mut settings = vec![
    Setting::LeftHanded(config.left_handed_mode),
    Setting::MiddleEmulation(config.middle_emulation),
  ];
  let profile = |name: &str| match name {
    "flat" => Some(AccelProfile::Flat),
    "adaptive" => Some(AccelProfile::Adaptive),
    "default" => Some(AccelProfile::DeviceDefault),
    _ => None,
  };
  if device.is_touchpad() {
    settings.push(Setting::AccelSpeed(config.touchpad_cursor_speed));
    settings.extend(
      profile(&config.touchpad_accel_profile).map(Setting::AccelProfile),
    );
    settings.push(Setting::Tap(config.tap_to_click));
    settings.push(Setting::ClickMethod(match config.click_method.as_str() {
      "none" => ClickMethod::None,
      "button-areas" => ClickMethod::ButtonAreas,
      "clickfinger" => ClickMethod::Clickfinger,
      _ => ClickMethod::DeviceDefault,
    }));
    settings.push(Setting::ScrollMethod(match config.scroll_method.as_str() {
      "none" => ScrollMethod::None,
      "two-finger" => ScrollMethod::TwoFinger,
      "edge" => ScrollMethod::Edge,
      "on-button-down" => ScrollMethod::OnButtonDown,
      _ => ScrollMethod::DeviceDefault,
    }));
    settings.push(Setting::DisableWhileTyping(
      config.disable_touchpad_while_typing,
    ));
    settings.push(Setting::TapDrag(config.tap_and_drag));
    settings.push(Setting::DragLock(config.drag_lock));
    settings.push(Setting::NaturalScroll(config.natural_scroll));
  } else {
    settings.push(Setting::AccelSpeed(config.mouse_cursor_speed));
    settings
      .extend(profile(&config.mouse_accel_profile).map(Setting::AccelProfile));
    settings.push(Setting::NaturalScroll(config.mouse_natural_scroll));
  }
  settings
}

/// The send-events mode of an enabled `device`: touchpads ignore their
/// input while a mouse is plugged in if `disable_touchpad_while_mouse` is
/// set, as Wayfire configures them; everything else is
/// [`SendEvents::Enabled`].
pub fn enabled_mode(
  config: &InputConfig,
  device: &DeviceDescription,
) -> SendEvents {
  if device.capabilities.pointer
    && device.is_touchpad()
    && config.disable_touchpad_while_mouse
  {
    SendEvents::DisabledOnExternalMouse
  } else {
    SendEvents::Enabled
  }
}

/// What the native backend needs from libinput.
pub trait InputSource {
  /// Reads pending device input and returns the events, with absolute
  /// positions transformed to an output of `output_size` pixels.
  fn dispatch(
    &mut self,
    output_size: (u32, u32),
  ) -> io::Result<Vec<NativeEvent>>;

  /// Closes every device for a seat pause and returns the events that
  /// caused, such as the removals.
  fn suspend(&mut self, output_size: (u32, u32)) -> Vec<NativeEvent>;

  /// Opens the devices again after a pause and returns the events that
  /// caused, such as the additions.
  fn resume(&mut self, output_size: (u32, u32))
  -> io::Result<Vec<NativeEvent>>;

  /// Applies `settings` to `device`. Settings the device does not support
  /// are skipped.
  fn configure(&mut self, device: &DeviceKey, settings: &[Setting]);

  /// Sets `device`'s send-events mode.
  fn set_send_events(&mut self, device: &DeviceKey, mode: SendEvents);

  /// Lights `leds` on every keyboard.
  fn set_leds(&mut self, leds: Leds);

  /// The descriptor that becomes readable when there is input to
  /// dispatch.
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}

/// The smallest step inside the output's right and bottom edges, as
/// wlroots' `wlr_box_closest_point` uses.
const EDGE: f64 = 1.0 / 65536.0;

/// Turns [`DeviceInput`] into the frontend's [`InputEvent`]s, keeping the
/// pointer position, the touch positions and the fractional wheel clicks
/// between events.
#[derive(Debug, Default)]
pub struct Translator {
  /// The pointer position in output coordinates.
  pub pointer: (f64, f64),
  /// Where each touch point last was, by seat slot.
  touches: HashMap<u32, (f64, f64)>,
  /// Unsent fractions of a wheel click, in v120 units, per device and
  /// axis (horizontal, vertical).
  wheel: HashMap<DeviceKey, (f64, f64)>,
}

/// The outputs as the translation of one device's input sees them.
pub struct OutputGeometry<'a> {
  /// Pulls a point in layout coordinates onto the closest point of an
  /// output, so the pointer stays on the layout
  /// (`Compositor::clamp_to_layout`).
  pub clamp: &'a dyn Fn(f64, f64) -> (f64, f64),
  /// Maps an absolute position of the device, as libinput transformed
  /// it, to layout coordinates: onto the output it is mapped to, undoing
  /// that output's scale and transform, or else onto the whole layout.
  pub map: &'a dyn Fn(f64, f64) -> (f64, f64),
  /// The output the device is mapped to (`input_devices.NAME.output`),
  /// with its top-left corner in the layout. Its absolute events name it,
  /// with positions in its coordinates.
  pub output: Option<(OutputHandle, (f64, f64))>,
  /// Whether a client constrains the pointer, which then does not move.
  pub constrained: bool,
}

impl OutputGeometry<'_> {
  /// The output a position in layout coordinates is reported on, and the
  /// position as reported.
  fn reported(&self, (x, y): (f64, f64)) -> (f64, f64, Option<OutputHandle>) {
    match &self.output {
      Some((output, (left, top))) => (x - left, y - top, Some(output.clone())),
      None => (x, y, None),
    }
  }
}

impl Translator {
  /// Translates `input` from `device`.
  ///
  /// Relative motion is always passed on as [`InputEvent::PointerMotion`]
  /// for relative-pointer clients and, unless the pointer is constrained,
  /// moves the pointer by the accelerated delta, kept inside the output,
  /// as [`InputEvent::PointerMotionAbsolute`]. Wheel scrolls pass whole
  /// clicks as steps and keep fractions of high-resolution wheels for
  /// later; finger and continuous scrolls have no steps and stop on a zero
  /// value, as libinput ends them.
  pub fn translate(
    &mut self,
    device: &DeviceKey,
    input: DeviceInput,
    output: &OutputGeometry<'_>,
  ) -> Vec<InputEvent> {
    match input {
      DeviceInput::Key { code, pressed } => {
        vec![InputEvent::Key { code, pressed }]
      }
      DeviceInput::Motion {
        dx,
        dy,
        dx_unaccel,
        dy_unaccel,
      } => {
        let mut events = vec![InputEvent::PointerMotion {
          dx,
          dy,
          dx_unaccel,
          dy_unaccel,
        }];
        if !output.constrained {
          let (x, y) = self.pointer;
          self.pointer = (output.clamp)(x + dx, y + dy);
          let (x, y) = self.pointer;
          events.push(InputEvent::PointerMotionAbsolute { x, y, output: None });
        }
        events
      }
      DeviceInput::MotionAbsolute { x, y } => {
        let (x, y) = (output.map)(x, y);
        self.pointer = (output.clamp)(x, y);
        let (x, y, output) = output.reported(self.pointer);
        vec![InputEvent::PointerMotionAbsolute { x, y, output }]
      }
      DeviceInput::Button { code, pressed } => {
        vec![InputEvent::PointerButton { code, pressed }]
      }
      DeviceInput::Scroll {
        source,
        horizontal,
        vertical,
      } => vec![InputEvent::PointerAxis(
        self.scroll(device, source, horizontal, vertical),
      )],
      DeviceInput::Touch {
        slot,
        phase,
        position,
      } => {
        let position = match position {
          Some((x, y)) => {
            let point = (output.map)(x, y);
            self.touches.insert(slot, point);
            point
          }
          None => self.touches.get(&slot).copied().unwrap_or_default(),
        };
        if matches!(phase, TouchPhase::Up | TouchPhase::Cancel) {
          self.touches.remove(&slot);
        }
        let (x, y, output) = output.reported(position);
        vec![InputEvent::Touch(Touch {
          id: u64::from(slot),
          phase,
          x,
          y,
          output,
        })]
      }
      DeviceInput::Swipe(gesture) => vec![InputEvent::Swipe(gesture)],
      DeviceInput::Pinch(gesture) => vec![InputEvent::Pinch(gesture)],
      DeviceInput::Hold(gesture) => vec![InputEvent::Hold(gesture)],
    }
  }

  /// Forgets `device`'s wheel fractions, when it goes away.
  pub fn forget(&mut self, device: &DeviceKey) {
    self.wheel.remove(device);
  }

  /// Forgets every touch point and wheel fraction, for a seat pause.
  pub fn reset(&mut self) {
    self.touches.clear();
    self.wheel.clear();
  }

  fn scroll(
    &mut self,
    device: &DeviceKey,
    source: ScrollSource,
    horizontal: Option<ScrollAxis>,
    vertical: Option<ScrollAxis>,
  ) -> Axis {
    let value = |axis: Option<ScrollAxis>| axis.map_or(0.0, |axis| axis.value);
    let stop = |axis: Option<ScrollAxis>| {
      source != ScrollSource::Wheel
        && axis.is_some_and(|axis| axis.value == 0.0)
    };
    let mut axis = Axis {
      horizontal: value(horizontal),
      vertical: value(vertical),
      source: match source {
        ScrollSource::Wheel => AxisSource::Wheel,
        ScrollSource::Finger => AxisSource::Finger,
        ScrollSource::Continuous => AxisSource::Continuous,
      },
      horizontal_stop: stop(horizontal),
      vertical_stop: stop(vertical),
      ..Axis::default()
    };
    if source == ScrollSource::Wheel {
      let pending = self.wheel.entry(device.clone()).or_default();
      axis.horizontal_steps = whole_clicks(&mut pending.0, horizontal);
      axis.vertical_steps = whole_clicks(&mut pending.1, vertical);
    }
    axis
  }
}

/// Adds `axis`'s v120 value to `pending` and takes out the whole clicks.
fn whole_clicks(pending: &mut f64, axis: Option<ScrollAxis>) -> i32 {
  let Some(axis) = axis else {
    return 0;
  };
  // A change of direction drops what was left of the other one.
  if *pending != 0.0 && pending.signum() != axis.v120.signum() {
    *pending = 0.0;
  }
  *pending += axis.v120;
  let clicks = (*pending / 120.0).trunc();
  *pending -= clicks * 120.0;
  clicks as i32
}

/// Keeps `(x, y)` inside `bounds`, as the host keeps the pointer on an
/// output when the compositor has none to clamp it to.
pub fn clamp(bounds: Rect, x: f64, y: f64) -> (f64, f64) {
  let left = f64::from(bounds.x);
  let top = f64::from(bounds.y);
  let right = left + f64::from(bounds.width.max(1)) - EDGE;
  let bottom = top + f64::from(bounds.height.max(1)) - EDGE;
  (x.clamp(left, right), y.clamp(top, bottom))
}

#[cfg(test)]
pub(crate) mod tests {
  use std::collections::VecDeque;

  use super::*;

  /// libinput as a script: dispatches return queued events, and every
  /// request is recorded.
  #[derive(Default)]
  pub(crate) struct FakeInput {
    pub(crate) pending: VecDeque<NativeEvent>,
    /// What `suspend` returns.
    pub(crate) on_suspend: Vec<NativeEvent>,
    /// What `resume` returns.
    pub(crate) on_resume: Vec<NativeEvent>,
    pub(crate) suspended: bool,
    pub(crate) suspends: usize,
    pub(crate) resumes: usize,
    pub(crate) configured: Vec<(DeviceKey, Vec<Setting>)>,
    pub(crate) send_events: Vec<(DeviceKey, SendEvents)>,
    pub(crate) leds: Vec<Leds>,
    pub(crate) output_sizes: Vec<(u32, u32)>,
  }

  impl InputSource for FakeInput {
    fn dispatch(
      &mut self,
      output_size: (u32, u32),
    ) -> io::Result<Vec<NativeEvent>> {
      self.output_sizes.push(output_size);
      Ok(self.pending.drain(..).collect())
    }

    fn suspend(&mut self, _output_size: (u32, u32)) -> Vec<NativeEvent> {
      self.suspended = true;
      self.suspends += 1;
      std::mem::take(&mut self.on_suspend)
    }

    fn resume(
      &mut self,
      _output_size: (u32, u32),
    ) -> io::Result<Vec<NativeEvent>> {
      self.suspended = false;
      self.resumes += 1;
      Ok(std::mem::take(&mut self.on_resume))
    }

    fn configure(&mut self, device: &DeviceKey, settings: &[Setting]) {
      self.configured.push((device.clone(), settings.to_vec()));
    }

    fn set_send_events(&mut self, device: &DeviceKey, mode: SendEvents) {
      self.send_events.push((device.clone(), mode));
    }

    fn set_leds(&mut self, leds: Leds) {
      self.leds.push(leds);
    }

    fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
      None
    }
  }

  pub(crate) fn key(name: &str) -> DeviceKey {
    DeviceKey(name.into())
  }

  pub(crate) fn device(
    name: &str,
    capabilities: Capabilities,
    tap_fingers: u32,
  ) -> DeviceDescription {
    DeviceDescription {
      key: key(name),
      name: format!("{name} device"),
      vendor: 0x046d,
      product: 0xc52b,
      capabilities,
      tap_fingers,
      udev: Vec::new(),
    }
  }

  pub(crate) const POINTER: Capabilities = Capabilities {
    keyboard: false,
    pointer: true,
    touch: false,
    gesture: false,
  };

  fn identity(x: f64, y: f64) -> (f64, f64) {
    (x, y)
  }

  fn halved(x: f64, y: f64) -> (f64, f64) {
    (x / 2.0, y / 2.0)
  }

  fn bounded(x: f64, y: f64) -> (f64, f64) {
    clamp(Rect::new(0, 0, 1280, 720), x, y)
  }

  fn output(map: &dyn Fn(f64, f64) -> (f64, f64)) -> OutputGeometry<'_> {
    OutputGeometry {
      clamp: &bounded,
      map,
      output: None,
      constrained: false,
    }
  }

  #[test]
  fn relative_motion_moves_the_pointer_inside_the_output() {
    let mut translator = Translator::default();
    let mouse = key("event3");
    let motion = |dx, dy| DeviceInput::Motion {
      dx,
      dy,
      dx_unaccel: dx / 2.0,
      dy_unaccel: dy / 2.0,
    };
    let events =
      translator.translate(&mouse, motion(10.0, 5.0), &output(&identity));
    assert_eq!(
      events,
      [
        InputEvent::PointerMotion {
          dx: 10.0,
          dy: 5.0,
          dx_unaccel: 5.0,
          dy_unaccel: 2.5,
        },
        InputEvent::PointerMotionAbsolute {
          x: 10.0,
          y: 5.0,
          output: None,
        },
      ]
    );
    let events =
      translator.translate(&mouse, motion(5000.0, -50.0), &output(&identity));
    let InputEvent::PointerMotionAbsolute { x, y, output: None } = events[1]
    else {
      panic!("{events:?}");
    };
    assert!(x < 1280.0 && x > 1279.99, "{x}");
    assert_eq!(y, 0.0);
  }

  #[test]
  fn a_constrained_pointer_only_reports_relative_motion() {
    let mut translator = Translator::default();
    let map = identity;
    let geometry = OutputGeometry {
      constrained: true,
      ..output(&map)
    };
    let events = translator.translate(
      &key("event3"),
      DeviceInput::Motion {
        dx: 3.0,
        dy: 4.0,
        dx_unaccel: 3.0,
        dy_unaccel: 4.0,
      },
      &geometry,
    );
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], InputEvent::PointerMotion { .. }));
    assert_eq!(translator.pointer, (0.0, 0.0));
  }

  #[test]
  fn absolute_positions_and_touches_are_mapped_to_the_output() {
    let mut translator = Translator::default();
    let tablet = key("event7");
    assert_eq!(
      translator.translate(
        &tablet,
        DeviceInput::MotionAbsolute { x: 800.0, y: 600.0 },
        &output(&halved),
      ),
      [InputEvent::PointerMotionAbsolute {
        x: 400.0,
        y: 300.0,
        output: None,
      }]
    );
    let touch = |phase, position| DeviceInput::Touch {
      slot: 2,
      phase,
      position,
    };
    let down = translator.translate(
      &tablet,
      touch(TouchPhase::Down, Some((100.0, 50.0))),
      &output(&halved),
    );
    assert_eq!(
      down,
      [InputEvent::Touch(Touch {
        id: 2,
        phase: TouchPhase::Down,
        x: 50.0,
        y: 25.0,
        output: None,
      })]
    );
    // Up has no position of its own; it lifts where the point was.
    let up = translator.translate(
      &tablet,
      touch(TouchPhase::Up, None),
      &output(&halved),
    );
    assert_eq!(
      up,
      [InputEvent::Touch(Touch {
        id: 2,
        phase: TouchPhase::Up,
        x: 50.0,
        y: 25.0,
        output: None,
      })]
    );
  }

  #[test]
  fn a_device_mapped_to_an_output_reports_on_it() {
    let mut translator = Translator::default();
    let tablet = key("event7");
    let right = |x: f64, y: f64| (x + 1280.0, y);
    let layout = |x: f64, y: f64| (x.clamp(0.0, 3200.0), y.clamp(0.0, 1080.0));
    let geometry = OutputGeometry {
      clamp: &layout,
      map: &right,
      output: Some((OutputHandle::new("DP-1"), (1280.0, 0.0))),
      constrained: false,
    };
    let events = translator.translate(
      &tablet,
      DeviceInput::MotionAbsolute { x: 100.0, y: 50.0 },
      &geometry,
    );
    assert_eq!(
      events,
      [InputEvent::PointerMotionAbsolute {
        x: 100.0,
        y: 50.0,
        output: Some(OutputHandle::new("DP-1")),
      }]
    );
    assert_eq!(translator.pointer, (1380.0, 50.0), "in the layout");
    let down = translator.translate(
      &tablet,
      DeviceInput::Touch {
        slot: 0,
        phase: TouchPhase::Down,
        position: Some((10.0, 20.0)),
      },
      &geometry,
    );
    assert_eq!(
      down,
      [InputEvent::Touch(Touch {
        id: 0,
        phase: TouchPhase::Down,
        x: 10.0,
        y: 20.0,
        output: Some(OutputHandle::new("DP-1")),
      })]
    );
    // Relative motion crosses the layout freely.
    translator.translate(
      &key("event3"),
      DeviceInput::Motion {
        dx: -500.0,
        dy: 0.0,
        dx_unaccel: -500.0,
        dy_unaccel: 0.0,
      },
      &geometry,
    );
    assert_eq!(translator.pointer, (880.0, 50.0));
  }

  fn wheel(v120: f64) -> DeviceInput {
    DeviceInput::Scroll {
      source: ScrollSource::Wheel,
      horizontal: None,
      vertical: Some(ScrollAxis {
        value: v120 / 8.0,
        v120,
      }),
    }
  }

  fn axis(events: Vec<InputEvent>) -> Axis {
    match events.as_slice() {
      [InputEvent::PointerAxis(axis)] => *axis,
      other => panic!("{other:?}"),
    }
  }

  #[test]
  fn wheel_clicks_are_steps_and_fractions_add_up() {
    let mut translator = Translator::default();
    let mouse = key("event3");
    let click =
      axis(translator.translate(&mouse, wheel(120.0), &output(&identity)));
    assert_eq!(click.source, AxisSource::Wheel);
    assert_eq!((click.vertical, click.vertical_steps), (15.0, 1));
    assert!(!click.vertical_stop);
    let steps: Vec<i32> = (0..4)
      .map(|_| {
        axis(translator.translate(&mouse, wheel(60.0), &output(&identity)))
          .vertical_steps
      })
      .collect();
    assert_eq!(steps, [0, 1, 0, 1], "half clicks add up");
    let reverse =
      axis(translator.translate(&mouse, wheel(-60.0), &output(&identity)));
    assert_eq!(reverse.vertical_steps, 0);
    let back =
      axis(translator.translate(&mouse, wheel(-60.0), &output(&identity)));
    assert_eq!(back.vertical_steps, -1);
  }

  #[test]
  fn finger_scrolls_have_no_steps_and_stop_at_zero() {
    let mut translator = Translator::default();
    let pad = key("event9");
    let finger = |value| DeviceInput::Scroll {
      source: ScrollSource::Finger,
      horizontal: Some(ScrollAxis { value, v120: 0.0 }),
      vertical: None,
    };
    let moving =
      axis(translator.translate(&pad, finger(4.5), &output(&identity)));
    assert_eq!(moving.source, AxisSource::Finger);
    assert_eq!((moving.horizontal, moving.horizontal_steps), (4.5, 0));
    assert!(!moving.horizontal_stop && !moving.vertical_stop);
    let lifted =
      axis(translator.translate(&pad, finger(0.0), &output(&identity)));
    assert!(lifted.horizontal_stop);
    assert!(!lifted.vertical_stop, "the event has no vertical axis");
  }

  #[test]
  fn keys_buttons_and_gestures_pass_through() {
    let mut translator = Translator::default();
    let device = key("event1");
    let geometry = output(&identity);
    assert_eq!(
      translator.translate(
        &device,
        DeviceInput::Key {
          code: 30,
          pressed: true
        },
        &geometry
      ),
      [InputEvent::Key {
        code: 30,
        pressed: true
      }]
    );
    assert_eq!(
      translator.translate(
        &device,
        DeviceInput::Button {
          code: 0x110,
          pressed: false
        },
        &geometry
      ),
      [InputEvent::PointerButton {
        code: 0x110,
        pressed: false
      }]
    );
    let begin = Gesture::Begin { fingers: 3 };
    assert_eq!(
      translator.translate(&device, DeviceInput::Swipe(begin), &geometry),
      [InputEvent::Swipe(begin)]
    );
    let end = Gesture::End { cancelled: true };
    assert_eq!(
      translator.translate(&device, DeviceInput::Hold(end), &geometry),
      [InputEvent::Hold(end)]
    );
  }

  #[test]
  fn mice_get_the_mouse_options_in_wayfires_order() {
    let config = InputConfig {
      mouse_accel_profile: "flat".into(),
      mouse_cursor_speed: 0.5,
      mouse_natural_scroll: true,
      left_handed_mode: true,
      ..InputConfig::default()
    };
    assert_eq!(
      device_settings(&config, &device("event3", POINTER, 0)),
      [
        Setting::LeftHanded(true),
        Setting::MiddleEmulation(false),
        Setting::AccelSpeed(0.5),
        Setting::AccelProfile(AccelProfile::Flat),
        Setting::NaturalScroll(true),
      ]
    );
  }

  #[test]
  fn touchpads_get_the_touchpad_options() {
    let config = InputConfig {
      touchpad_accel_profile: "none".into(),
      touchpad_cursor_speed: -0.25,
      tap_to_click: false,
      click_method: "clickfinger".into(),
      scroll_method: "edge".into(),
      natural_scroll: true,
      disable_touchpad_while_typing: true,
      drag_lock: true,
      mouse_natural_scroll: false,
      ..InputConfig::default()
    };
    let touchpad = device(
      "event9",
      Capabilities {
        gesture: true,
        ..POINTER
      },
      3,
    );
    assert_eq!(
      device_settings(&config, &touchpad),
      [
        Setting::LeftHanded(false),
        Setting::MiddleEmulation(false),
        Setting::AccelSpeed(-0.25),
        Setting::Tap(false),
        Setting::ClickMethod(ClickMethod::Clickfinger),
        Setting::ScrollMethod(ScrollMethod::Edge),
        Setting::DisableWhileTyping(true),
        Setting::TapDrag(true),
        Setting::DragLock(true),
        Setting::NaturalScroll(true),
      ],
      "`none` leaves the profile alone"
    );
    let defaults = device_settings(&InputConfig::default(), &touchpad);
    assert!(
      defaults.contains(&Setting::AccelProfile(AccelProfile::DeviceDefault))
    );
    assert!(
      defaults.contains(&Setting::ClickMethod(ClickMethod::DeviceDefault))
    );
    assert!(
      defaults.contains(&Setting::ScrollMethod(ScrollMethod::DeviceDefault))
    );
  }

  #[test]
  fn keyboards_and_touchscreens_are_not_configured() {
    let keyboard = Capabilities {
      keyboard: true,
      ..Capabilities::default()
    };
    let touch = Capabilities {
      touch: true,
      ..Capabilities::default()
    };
    let config = InputConfig::default();
    assert!(
      device_settings(&config, &device("event1", keyboard, 0)).is_empty()
    );
    assert!(device_settings(&config, &device("event5", touch, 0)).is_empty());
  }

  #[test]
  fn only_touchpads_wait_for_the_mouse_to_go() {
    let config = InputConfig {
      disable_touchpad_while_mouse: true,
      ..InputConfig::default()
    };
    assert_eq!(
      enabled_mode(&config, &device("event9", POINTER, 2)),
      SendEvents::DisabledOnExternalMouse
    );
    assert_eq!(
      enabled_mode(&config, &device("event3", POINTER, 0)),
      SendEvents::Enabled
    );
    assert_eq!(
      enabled_mode(&InputConfig::default(), &device("event9", POINTER, 2)),
      SendEvents::Enabled
    );
  }
}
