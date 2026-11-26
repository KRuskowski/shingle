//! Settings of single input devices: Wayfire's `[input:NAME]` sections,
//! which give one device its own libinput options in place of `[input]`'s,
//! and `[input-device:NAME]` sections, with the output an absolute device
//! maps to and a touch screen's calibration. YAML keeps both under
//! `input_devices.NAME`.
//!
//! A device takes the first entry that names it, in Wayfire's order
//! (wayfire:src/core/plugin.cpp, `get_input_device_section`): by the udev
//! properties of [`UDEV_PROPERTIES`], then by its name.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{ACCEL_PROFILES, CLICK_METHODS, ConfigError, InputConfig};

/// The udev properties a device is looked up by before its name, in the
/// order Wayfire tries them: the physical connection path, vendor, model
/// and serial, libinput's device group, and the unstable devpath.
pub const UDEV_PROPERTIES: [&str; 4] =
  ["ID_PATH", "ID_SERIAL", "LIBINPUT_DEVICE_GROUP", "DEVPATH"];

/// Settings of one input device. Every libinput option is unset by default
/// and then follows `[input]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputDeviceConfig {
  /// The output an absolute device (touch screen, tablet) maps to.
  /// `[input-device:NAME] output`; by default the output the device names
  /// itself, else the whole layout.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub output: Option<String>,
  /// A touch screen's libinput calibration matrix, the first two rows of
  /// a 3x3 matrix. `[input-device:NAME] calibration`, six numbers
  /// separated by spaces; by default the device's own. Native only.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub calibration: Option<[f32; 6]>,
  /// `[input:NAME] left_handed_mode`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub left_handed_mode: Option<bool>,
  /// `[input:NAME] middle_emulation`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub middle_emulation: Option<bool>,
  /// `[input:NAME] mouse_accel_profile`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub mouse_accel_profile: Option<String>,
  /// `[input:NAME] mouse_cursor_speed`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub mouse_cursor_speed: Option<f64>,
  /// `[input:NAME] mouse_natural_scroll`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub mouse_natural_scroll: Option<bool>,
  /// `[input:NAME] touchpad_accel_profile`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub touchpad_accel_profile: Option<String>,
  /// `[input:NAME] touchpad_cursor_speed`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub touchpad_cursor_speed: Option<f64>,
  /// `[input:NAME] tap_to_click`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tap_to_click: Option<bool>,
  /// `[input:NAME] click_method`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub click_method: Option<String>,
  /// `[input:NAME] scroll_method`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub scroll_method: Option<String>,
  /// `[input:NAME] natural_scroll`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub natural_scroll: Option<bool>,
  /// `[input:NAME] disable_touchpad_while_typing`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub disable_touchpad_while_typing: Option<bool>,
  /// `[input:NAME] disable_touchpad_while_mouse`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub disable_touchpad_while_mouse: Option<bool>,
  /// `[input:NAME] tap_and_drag`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tap_and_drag: Option<bool>,
  /// `[input:NAME] drag_lock`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub drag_lock: Option<bool>,
}

/// The options of `[input]` an `[input:NAME]` section can set for one
/// device: those the native backend hands libinput.
pub const DEVICE_OPTIONS: [&str; 15] = [
  "left_handed_mode",
  "middle_emulation",
  "mouse_accel_profile",
  "mouse_cursor_speed",
  "mouse_natural_scroll",
  "touchpad_accel_profile",
  "touchpad_cursor_speed",
  "tap_to_click",
  "click_method",
  "scroll_method",
  "natural_scroll",
  "disable_touchpad_while_typing",
  "disable_touchpad_while_mouse",
  "tap_and_drag",
  "drag_lock",
];

impl InputDeviceConfig {
  /// Whether this entry sets any libinput option of [`DEVICE_OPTIONS`].
  pub fn has_input_options(&self) -> bool {
    let empty = Self {
      output: self.output.clone(),
      calibration: self.calibration,
      ..Self::default()
    };
    *self != empty
  }

  /// `base` with the options this entry sets in their place, the options
  /// the device gets.
  pub fn input_config(&self, base: &InputConfig) -> InputConfig {
    let mut config = base.clone();
    let flag = |value: Option<bool>, field: &mut bool| {
      if let Some(value) = value {
        *field = value;
      }
    };
    flag(self.left_handed_mode, &mut config.left_handed_mode);
    flag(self.middle_emulation, &mut config.middle_emulation);
    flag(self.mouse_natural_scroll, &mut config.mouse_natural_scroll);
    flag(self.tap_to_click, &mut config.tap_to_click);
    flag(self.natural_scroll, &mut config.natural_scroll);
    flag(
      self.disable_touchpad_while_typing,
      &mut config.disable_touchpad_while_typing,
    );
    flag(
      self.disable_touchpad_while_mouse,
      &mut config.disable_touchpad_while_mouse,
    );
    flag(self.tap_and_drag, &mut config.tap_and_drag);
    flag(self.drag_lock, &mut config.drag_lock);
    if let Some(speed) = self.mouse_cursor_speed {
      config.mouse_cursor_speed = speed;
    }
    if let Some(speed) = self.touchpad_cursor_speed {
      config.touchpad_cursor_speed = speed;
    }
    for (value, field) in [
      (&self.mouse_accel_profile, &mut config.mouse_accel_profile),
      (
        &self.touchpad_accel_profile,
        &mut config.touchpad_accel_profile,
      ),
      (&self.click_method, &mut config.click_method),
      (&self.scroll_method, &mut config.scroll_method),
    ] {
      if let Some(value) = value {
        field.clone_from(value);
      }
    }
    config
  }

  /// Checks the options it sets against the values `[input]` allows.
  fn validate(&self, name: &str) -> Result<(), ConfigError> {
    let choices = [
      (
        "mouse_accel_profile",
        &self.mouse_accel_profile,
        &ACCEL_PROFILES[..],
      ),
      (
        "touchpad_accel_profile",
        &self.touchpad_accel_profile,
        &ACCEL_PROFILES[..],
      ),
      ("click_method", &self.click_method, &CLICK_METHODS[..]),
      (
        "scroll_method",
        &self.scroll_method,
        &crate::SCROLL_METHODS[..],
      ),
    ];
    for (option, value, allowed) in choices {
      if let Some(value) = value
        && !allowed.contains(&value.as_str())
      {
        return Err(ConfigError::Invalid(format!(
          "input_devices.{name}.{option} must be one of {}, not {value:?}",
          allowed.join(", ")
        )));
      }
    }
    for (option, speed) in [
      ("mouse_cursor_speed", self.mouse_cursor_speed),
      ("touchpad_cursor_speed", self.touchpad_cursor_speed),
    ] {
      if speed.is_some_and(|speed| !(-1.0..=1.0).contains(&speed)) {
        return Err(ConfigError::Invalid(format!(
          "input_devices.{name}.{option} must be between -1 and 1"
        )));
      }
    }
    if self.output.as_deref() == Some("") {
      return Err(ConfigError::Invalid(format!(
        "input_devices.{name}.output must name an output"
      )));
    }
    if self
      .calibration
      .is_some_and(|matrix| matrix.iter().any(|value| !value.is_finite()))
    {
      return Err(ConfigError::Invalid(format!(
        "input_devices.{name}.calibration needs six finite numbers"
      )));
    }
    Ok(())
  }
}

/// Reads Wayfire's `calibration` text, six numbers separated by spaces.
/// Wayfire reads them with `sscanf("%f %f %f %f %f %f")` and ignores
/// anything after the sixth; an empty value is no calibration.
///
/// # Errors
///
/// [`ConfigError::Invalid`] if the text does not start with six numbers.
pub fn parse_calibration(value: &str) -> Result<Option<[f32; 6]>, ConfigError> {
  if value.trim().is_empty() {
    return Ok(None);
  }
  let numbers = value
    .split_whitespace()
    .take(6)
    .map(str::parse::<f32>)
    .collect::<Result<Vec<_>, _>>()
    .ok()
    .and_then(|numbers| <[f32; 6]>::try_from(numbers).ok())
    .filter(|matrix| matrix.iter().all(|value| value.is_finite()));
  numbers.map(Some).ok_or_else(|| {
    ConfigError::Invalid(format!(
      "calibration needs six numbers separated by spaces, not {value:?}"
    ))
  })
}

/// Checks that every entry has a name and valid values.
pub(crate) fn validate(
  devices: &BTreeMap<String, InputDeviceConfig>,
) -> Result<(), ConfigError> {
  for (name, device) in devices {
    if name.is_empty() {
      return Err(ConfigError::Invalid(
        "input_devices needs device names".into(),
      ));
    }
    device.validate(name)?;
  }
  Ok(())
}

/// The first entry of `devices` that names a device and satisfies `wanted`,
/// with the name it was found under: by the device's udev `properties`
/// (`property(name)` gives a property's value) in the order of
/// [`UDEV_PROPERTIES`], then by `device_name`.
///
/// Wayfire looks up `[input:NAME]` and `[input-device:NAME]` sections
/// independently; `wanted` picks which half of an entry is looked for,
/// such as [`InputDeviceConfig::has_input_options`].
pub fn matching_device<'a>(
  devices: &'a BTreeMap<String, InputDeviceConfig>,
  device_name: &str,
  property: impl Fn(&str) -> Option<String>,
  wanted: impl Fn(&InputDeviceConfig) -> bool,
) -> Option<(&'a str, &'a InputDeviceConfig)> {
  UDEV_PROPERTIES
    .iter()
    .filter_map(|name| property(name))
    .chain(std::iter::once(device_name.to_owned()))
    .find_map(|key| {
      devices
        .get_key_value(key.as_str())
        .filter(|(_, device)| wanted(device))
    })
    .map(|(key, device)| (key.as_str(), device))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConfigFormat, ShingleConfig};

  fn devices(
    entries: &[(&str, InputDeviceConfig)],
  ) -> BTreeMap<String, InputDeviceConfig> {
    entries
      .iter()
      .map(|(name, device)| ((*name).to_owned(), device.clone()))
      .collect()
  }

  fn speed(value: f64) -> InputDeviceConfig {
    InputDeviceConfig {
      mouse_cursor_speed: Some(value),
      ..InputDeviceConfig::default()
    }
  }

  #[test]
  fn devices_match_by_udev_properties_then_by_name() {
    let all = devices(&[
      ("Logitech USB Receiver", speed(0.1)),
      ("pci-0000:00:14.0-usb-0:2:1.0", speed(0.2)),
      ("Logitech_USB_Receiver", speed(0.3)),
    ]);
    let properties = |name: &str| match name {
      "ID_PATH" => Some("pci-0000:00:14.0-usb-0:2:1.0".to_owned()),
      "ID_SERIAL" => Some("Logitech_USB_Receiver".to_owned()),
      _ => None,
    };
    let found = matching_device(
      &all,
      "Logitech USB Receiver",
      properties,
      InputDeviceConfig::has_input_options,
    );
    assert_eq!(
      found.map(|(key, _)| key),
      Some("pci-0000:00:14.0-usb-0:2:1.0")
    );
    let serial = |name: &str| {
      (name == "ID_SERIAL").then(|| "Logitech_USB_Receiver".to_owned())
    };
    let found =
      matching_device(&all, "Logitech USB Receiver", serial, |_| true);
    assert_eq!(found.map(|(key, _)| key), Some("Logitech_USB_Receiver"));
    let found =
      matching_device(&all, "Logitech USB Receiver", |_| None, |_| true);
    assert_eq!(found.map(|(key, _)| key), Some("Logitech USB Receiver"));
    assert!(matching_device(&all, "Other Mouse", |_| None, |_| true).is_none());
  }

  #[test]
  fn each_half_of_an_entry_is_looked_up_on_its_own() {
    let output = InputDeviceConfig {
      output: Some("DP-1".into()),
      ..InputDeviceConfig::default()
    };
    let all = devices(&[("Pen", speed(0.5)), ("usb-pen-path", output)]);
    let path = |name: &str| (name == "ID_PATH").then(|| "usb-pen-path".into());
    let options =
      matching_device(&all, "Pen", path, InputDeviceConfig::has_input_options);
    assert_eq!(options.map(|(key, _)| key), Some("Pen"));
    let mapped =
      matching_device(&all, "Pen", path, |device| device.output.is_some());
    assert_eq!(mapped.map(|(key, _)| key), Some("usb-pen-path"));
  }

  #[test]
  fn an_entry_overrides_only_what_it_sets() {
    let base = InputConfig {
      tap_to_click: true,
      mouse_cursor_speed: -0.5,
      click_method: "clickfinger".into(),
      ..InputConfig::default()
    };
    let device = InputDeviceConfig {
      tap_to_click: Some(false),
      scroll_method: Some("edge".into()),
      ..InputDeviceConfig::default()
    };
    let config = device.input_config(&base);
    assert!(!config.tap_to_click);
    assert_eq!(config.scroll_method, "edge");
    assert_eq!(config.mouse_cursor_speed, -0.5);
    assert_eq!(config.click_method, "clickfinger");
    assert!(device.has_input_options());
    assert!(
      !InputDeviceConfig {
        output: Some("DP-1".into()),
        calibration: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
        ..InputDeviceConfig::default()
      }
      .has_input_options()
    );
  }

  #[test]
  fn every_device_option_is_read_from_wayfire_text() {
    let values = [
      ("left_handed_mode", "true"),
      ("middle_emulation", "1"),
      ("mouse_accel_profile", "flat"),
      ("mouse_cursor_speed", "0.25"),
      ("mouse_natural_scroll", "true"),
      ("touchpad_accel_profile", "adaptive"),
      ("touchpad_cursor_speed", "-0.5"),
      ("tap_to_click", "false"),
      ("click_method", "clickfinger"),
      ("scroll_method", "edge"),
      ("natural_scroll", "true"),
      ("disable_touchpad_while_typing", "true"),
      ("disable_touchpad_while_mouse", "true"),
      ("tap_and_drag", "false"),
      ("drag_lock", "true"),
    ];
    assert_eq!(values.len(), DEVICE_OPTIONS.len());
    let mut text = String::from("[input:Device]\n");
    for (key, value) in values {
      assert!(DEVICE_OPTIONS.contains(&key), "{key}");
      text += &format!("{key} = {value}\n");
    }
    let (config, warnings) =
      ShingleConfig::parse_with_warnings(&text, ConfigFormat::Wayfire).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let config =
      config.input_devices["Device"].input_config(&InputConfig::default());
    assert!(config.left_handed_mode && config.middle_emulation);
    assert_eq!(config.mouse_accel_profile, "flat");
    assert_eq!(config.mouse_cursor_speed, 0.25);
    assert!(config.mouse_natural_scroll);
    assert_eq!(config.touchpad_accel_profile, "adaptive");
    assert_eq!(config.touchpad_cursor_speed, -0.5);
    assert!(!config.tap_to_click);
    assert_eq!(config.click_method, "clickfinger");
    assert_eq!(config.scroll_method, "edge");
    assert!(config.natural_scroll);
    assert!(config.disable_touchpad_while_typing);
    assert!(config.disable_touchpad_while_mouse);
    assert!(!config.tap_and_drag && config.drag_lock);
  }

  #[test]
  fn calibration_takes_six_numbers() {
    assert_eq!(
      parse_calibration("0 1 0 -1 0 1").unwrap(),
      Some([0.0, 1.0, 0.0, -1.0, 0.0, 1.0])
    );
    assert_eq!(
      parse_calibration(" 1.5 0 0 0 1 0 trailing").unwrap(),
      Some([1.5, 0.0, 0.0, 0.0, 1.0, 0.0])
    );
    assert_eq!(parse_calibration("").unwrap(), None);
    assert!(parse_calibration("1 0 0 0 1").is_err());
    assert!(parse_calibration("1 0 0 0 1 x").is_err());
  }

  #[test]
  fn yaml_entries_round_trip_and_are_checked() {
    let config = ShingleConfig::parse(
      concat!(
        "input_devices:\n",
        "  Logitech USB Receiver:\n",
        "    mouse_cursor_speed: 0.3\n",
        "    mouse_accel_profile: flat\n",
        "  Touch Screen:\n",
        "    output: DP-1\n",
        "    calibration: [0, 1, 0, -1, 0, 1]\n",
      ),
      ConfigFormat::Yaml,
    )
    .unwrap();
    let mouse = &config.input_devices["Logitech USB Receiver"];
    assert_eq!(mouse.mouse_cursor_speed, Some(0.3));
    assert_eq!(mouse.mouse_accel_profile.as_deref(), Some("flat"));
    let touch = &config.input_devices["Touch Screen"];
    assert_eq!(touch.calibration, Some([0.0, 1.0, 0.0, -1.0, 0.0, 1.0]));
    let yaml = config.to_yaml().unwrap();
    let again = ShingleConfig::parse(&yaml, ConfigFormat::Yaml).unwrap();
    assert_eq!(again.input_devices, config.input_devices);
    let error = |yaml: &str| {
      ShingleConfig::parse(yaml, ConfigFormat::Yaml)
        .unwrap_err()
        .to_string()
    };
    assert!(
      error("input_devices:\n  m:\n    click_method: tap\n")
        .contains("m.click_method")
    );
    assert!(
      error("input_devices:\n  m:\n    mouse_cursor_speed: 2\n")
        .contains("between -1 and 1")
    );
    assert!(!error("input_devices:\n  m:\n    xkb_layout: de\n").is_empty());
  }

  #[test]
  fn wayfire_device_sections_are_imported() {
    let (config, warnings) = ShingleConfig::parse_with_warnings(
      concat!(
        "[input]\n",
        "mouse_cursor_speed = -0.2\n",
        "tap_to_click = true\n",
        "[input:Logitech USB Receiver]\n",
        "mouse_cursor_speed = 0.4\n",
        "mouse_accel_profile = flat\n",
        "xkb_layout = de\n",
        "[input:SynPS/2 Synaptics TouchPad]\n",
        "tap_to_click = false\n",
        "click_method = sideways\n",
        "[input-device:ELAN Touchscreen]\n",
        "output = eDP-1\n",
        "calibration = 0 1 0 -1 0 1\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(config.input.mouse_cursor_speed, -0.2);
    let mouse = &config.input_devices["Logitech USB Receiver"];
    assert_eq!(mouse.mouse_cursor_speed, Some(0.4));
    assert_eq!(mouse.mouse_accel_profile.as_deref(), Some("flat"));
    let touchpad = &config.input_devices["SynPS/2 Synaptics TouchPad"];
    assert_eq!(touchpad.tap_to_click, Some(false));
    assert_eq!(touchpad.click_method, None);
    let screen = &config.input_devices["ELAN Touchscreen"];
    assert_eq!(screen.output.as_deref(), Some("eDP-1"));
    assert_eq!(screen.calibration, Some([0.0, 1.0, 0.0, -1.0, 0.0, 1.0]));
    let texts = warnings
      .iter()
      .map(|warning| warning.to_string())
      .collect::<Vec<_>>();
    // A keyboard option per device and a bad value each warn; nothing
    // says the sections belong to a missing plugin.
    assert!(
      texts.iter().any(|text| text.contains("xkb_layout")),
      "{texts:?}"
    );
    assert!(
      texts.iter().any(|text| text.contains("click_method")),
      "{texts:?}"
    );
    assert!(
      !texts.iter().any(|text| text.contains("does not have")),
      "{texts:?}"
    );
    assert_eq!(texts.len(), 2, "{texts:?}");
  }
}
