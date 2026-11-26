//! Configuration of several outputs: what happens to an unplugged output's
//! windows, Wayfire's `oswitch` and `wsets` plugins, and which output an
//! absolute input device maps to.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{Binding, ConfigError, Modifier};

/// What happens to the workspace set of an output that goes away: an
/// unplugged monitor, a disabled head, `mode: off` on reload, a closed
/// nested window or a destroyed IPC output.
#[derive(
  Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum UnplugPolicy {
  /// Wayfire's default: every window moves to the active output, keeping
  /// its workspace, and the set is gone.
  Move,
  /// Wayfire with preserve-output: the set is parked with its windows,
  /// hidden, and shown again when the output returns.
  Park,
  /// The windows move as with `Move`, but the set is parked with a copy of
  /// its trees, and windows still away from it go back to their old places
  /// when the output returns.
  #[default]
  Return,
}

/// How Shingle treats outputs coming and going.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputLayoutConfig {
  /// What happens to an output's windows when it goes; default `return`.
  /// Imported as `park` when Wayfire's `[core] plugins` lists
  /// `preserve-output`; a Wayfire file without it keeps `return` rather
  /// than Wayfire's `move`. Outputs created over IPC always use `move`.
  pub unplug: UnplugPolicy,
  /// Milliseconds within which an output that returns is focused again if
  /// it was focused when it went. `[preserve-output]
  /// last_output_focus_timeout`; default 10000.
  pub last_output_focus_timeout: u64,
}

impl Default for OutputLayoutConfig {
  fn default() -> Self {
    Self {
      unplug: UnplugPolicy::Return,
      last_output_focus_timeout: 10_000,
    }
  }
}

/// Bindings that focus another output, Wayfire's `[oswitch]`
/// (metadata/oswitch.xml). Next and previous follow the order outputs were
/// added in and wrap; a direction picks the output whose centre lies that
/// way with the best overlap and distance ([`crate::output_in_direction`]).
/// The `_with_win` variants take the focused window along.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OswitchConfig {
  /// Focus the next output. `[oswitch] next_output`; default `super+o`.
  pub next_output: Binding,
  /// Focus the next output and move the focused window there. `[oswitch]
  /// next_output_with_win`; default `super+shift+o`.
  pub next_output_with_win: Binding,
  /// Focus the previous output. `[oswitch] prev_output`; disabled by
  /// default.
  pub prev_output: Binding,
  /// Focus the previous output with the focused window. `[oswitch]
  /// prev_output_with_win`; disabled by default.
  pub prev_output_with_win: Binding,
  /// Focus the output to the left. `[oswitch] left_output`; disabled by
  /// default.
  pub left_output: Binding,
  /// Focus the output to the right. `[oswitch] right_output`; disabled by
  /// default.
  pub right_output: Binding,
  /// Focus the output above. `[oswitch] above_output`; disabled by default.
  pub above_output: Binding,
  /// Focus the output below. `[oswitch] below_output`; disabled by default.
  pub below_output: Binding,
}

impl Default for OswitchConfig {
  fn default() -> Self {
    Self {
      next_output: Binding::new("o", vec![Modifier::Super]),
      next_output_with_win: Binding::new(
        "o",
        vec![Modifier::Super, Modifier::Shift],
      ),
      prev_output: Binding::disabled(),
      prev_output_with_win: Binding::disabled(),
      left_output: Binding::disabled(),
      right_output: Binding::disabled(),
      above_output: Binding::disabled(),
      below_output: Binding::disabled(),
    }
  }
}

/// A binding that acts on workspace set `wset`.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct WsetBinding {
  /// The set's index, from 1.
  pub wset: u32,
  /// The binding.
  pub binding: Binding,
}

/// Workspace set bindings, Wayfire's `[wsets]` (metadata/wsets.xml).
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WsetsConfig {
  /// Show set N on the active output, creating it if needed; the output
  /// showing it before gets a new empty set. `[wsets] wset_N`; none by
  /// default.
  pub wsets_bindings: Vec<WsetBinding>,
  /// Send the focused window to set N. `[wsets] send_to_wset_N`; none by
  /// default.
  pub send_window_bindings: Vec<WsetBinding>,
  /// Milliseconds the "Workspace set N" label is shown. `[wsets]
  /// label_duration`; default 2000.
  pub label_duration: u64,
}

impl Default for WsetsConfig {
  fn default() -> Self {
    Self {
      wsets_bindings: Vec::new(),
      send_window_bindings: Vec::new(),
      label_duration: 2000,
    }
  }
}

impl WsetsConfig {
  /// Checks that every binding names a set from 1.
  pub(crate) fn validate(&self) -> Result<(), ConfigError> {
    let bindings = self.wsets_bindings.iter().chain(&self.send_window_bindings);
    for binding in bindings {
      if binding.wset == 0 {
        return Err(ConfigError::Invalid(
          "wsets bindings name workspace sets from 1".into(),
        ));
      }
    }
    Ok(())
  }
}

/// Settings of one input device, Wayfire's `[input-device:NAME]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputDeviceConfig {
  /// The output an absolute device (touch screen, tablet) maps to.
  /// `[input-device:NAME] output`; by default the output the device names
  /// itself, else the whole layout.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub output: Option<String>,
}

/// Checks that no device maps to an empty output name.
pub(crate) fn validate_input_devices(
  devices: &BTreeMap<String, InputDeviceConfig>,
) -> Result<(), ConfigError> {
  for (name, device) in devices {
    if name.is_empty() {
      return Err(ConfigError::Invalid(
        "input_devices needs device names".into(),
      ));
    }
    if device.output.as_deref() == Some("") {
      return Err(ConfigError::Invalid(format!(
        "input_devices.{name}.output must name an output"
      )));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConfigFormat, ShingleConfig};

  fn text(binding: &Binding) -> String {
    String::from(binding.clone())
  }

  #[test]
  fn defaults_follow_the_design_and_wayfire() {
    let config = ShingleConfig::default();
    assert_eq!(config.output_layout.unplug, UnplugPolicy::Return);
    assert_eq!(config.output_layout.last_output_focus_timeout, 10_000);
    assert_eq!(text(&config.oswitch.next_output), "super+o");
    assert_eq!(text(&config.oswitch.next_output_with_win), "super+shift+o");
    assert!(config.oswitch.below_output.key.is_empty());
    assert!(config.wsets.wsets_bindings.is_empty());
    assert_eq!(config.wsets.label_duration, 2000);
    assert!(config.input_devices.is_empty());
    assert!(!config.behavior.remove_output_limits);
  }

  #[test]
  fn sections_round_trip_through_yaml() {
    let config = ShingleConfig::parse(
      concat!(
        "output_layout:\n",
        "  unplug: park\n",
        "  last_output_focus_timeout: 500\n",
        "oswitch:\n",
        "  left_output: super+left\n",
        "wsets:\n",
        "  wsets_bindings:\n",
        "    - wset: 2\n",
        "      binding: super+f2\n",
        "  send_window_bindings:\n",
        "    - wset: 2\n",
        "      binding: super+shift+f2\n",
        "  label_duration: 750\n",
        "input_devices:\n",
        "  Wacom Pen:\n",
        "    output: DP-2\n",
        "behavior:\n",
        "  remove_output_limits: true\n",
      ),
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(config.output_layout.unplug, UnplugPolicy::Park);
    assert_eq!(config.output_layout.last_output_focus_timeout, 500);
    assert_eq!(text(&config.oswitch.left_output), "super+left");
    assert_eq!(text(&config.oswitch.next_output), "super+o");
    assert_eq!(config.wsets.wsets_bindings[0].wset, 2);
    assert_eq!(
      text(&config.wsets.send_window_bindings[0].binding),
      "super+shift+f2"
    );
    assert_eq!(config.wsets.label_duration, 750);
    assert_eq!(
      config.input_devices["Wacom Pen"].output.as_deref(),
      Some("DP-2")
    );
    assert!(config.behavior.remove_output_limits);
    let yaml = config.to_yaml().unwrap();
    let again = ShingleConfig::parse(&yaml, ConfigFormat::Yaml).unwrap();
    assert_eq!(again.output_layout, config.output_layout);
    assert_eq!(again.oswitch, config.oswitch);
    assert_eq!(again.wsets, config.wsets);
    assert_eq!(again.input_devices, config.input_devices);
  }

  #[test]
  fn invalid_values_are_refused() {
    let error = |yaml: &str| {
      ShingleConfig::parse(yaml, ConfigFormat::Yaml)
        .unwrap_err()
        .to_string()
    };
    assert!(!error("output_layout:\n  unplug: keep\n").is_empty());
    assert!(
      error(
        "wsets:\n  wsets_bindings:\n    - wset: 0\n      binding: super+a\n"
      )
      .contains("from 1")
    );
    assert!(
      error("input_devices:\n  pen:\n    output: \"\"\n")
        .contains("pen.output")
    );
  }

  #[test]
  fn wayfire_sections_are_imported() {
    let config = ShingleConfig::parse(
      concat!(
        "[core]\n",
        "plugins = ipc wsets preserve-output oswitch\n",
        "[preserve-output]\n",
        "last_output_focus_timeout = 2500\n",
        "[oswitch]\n",
        "next_output = <super> KEY_TAB\n",
        "prev_output_with_win = <super> <shift> KEY_P\n",
        "left_output = <super> <ctrl> KEY_LEFT\n",
        "right_output = <super> <ctrl> KEY_RIGHT\n",
        "above_output = <super> <ctrl> KEY_UP\n",
        "below_output = <super> <ctrl> KEY_DOWN\n",
        "[wsets]\n",
        "wset_1 = <super> KEY_F1\n",
        "wset_3 = <super> KEY_F3\n",
        "send_to_wset_3 = <super> <shift> KEY_F3\n",
        "label_duration = 500ms linear\n",
        "[input-device:Wacom Pen]\n",
        "output = HDMI-A-1\n",
        "[workarounds]\n",
        "remove_output_limits = true\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(config.output_layout.unplug, UnplugPolicy::Park);
    assert_eq!(config.output_layout.last_output_focus_timeout, 2500);
    assert_eq!(text(&config.oswitch.next_output), "super+tab");
    assert_eq!(text(&config.oswitch.next_output_with_win), "super+shift+o");
    assert_eq!(text(&config.oswitch.prev_output_with_win), "super+shift+p");
    for (binding, key) in [
      (&config.oswitch.left_output, "left"),
      (&config.oswitch.right_output, "right"),
      (&config.oswitch.above_output, "up"),
      (&config.oswitch.below_output, "down"),
    ] {
      assert_eq!(text(binding), format!("super+ctrl+{key}"));
    }
    let sets = |bindings: &[WsetBinding]| {
      bindings
        .iter()
        .map(|binding| (binding.wset, text(&binding.binding)))
        .collect::<Vec<_>>()
    };
    assert_eq!(
      sets(&config.wsets.wsets_bindings),
      [(1, "super+f1".to_owned()), (3, "super+f3".to_owned())]
    );
    assert_eq!(
      sets(&config.wsets.send_window_bindings),
      [(3, "super+shift+f3".to_owned())]
    );
    assert_eq!(config.wsets.label_duration, 500);
    assert_eq!(
      config.input_devices["Wacom Pen"].output.as_deref(),
      Some("HDMI-A-1")
    );
    assert!(config.behavior.remove_output_limits);
  }

  #[test]
  fn wayfire_without_preserve_output_keeps_the_default() {
    for text in ["[core]\nplugins = ipc oswitch\n", "[core]\nvwidth = 3\n"] {
      let config = ShingleConfig::parse(text, ConfigFormat::Wayfire).unwrap();
      assert_eq!(config.output_layout.unplug, UnplugPolicy::Return);
    }
    let error = ShingleConfig::parse(
      "[wsets]\nwset_0 = <super> KEY_F1\n",
      ConfigFormat::Wayfire,
    )
    .unwrap_err();
    assert!(error.to_string().contains("from 1"), "{error}");
  }

  #[test]
  fn i3_lines_are_imported() {
    let config = ShingleConfig::parse(
      concat!(
        "set $output_unplug move\n",
        "set $last_output_focus_timeout 3000\n",
        "set $remove_output_limits yes\n",
        "set $wsets_label_duration 900\n",
        "bindsym Mod4+o focus output next\n",
        "bindsym Mod4+Shift+o move container to output next\n",
        "bindsym Mod4+i focus output prev\n",
        "bindsym Mod4+Shift+i move container to output prev\n",
        "bindsym Mod4+Ctrl+Left focus output left\n",
        "bindsym Mod4+Ctrl+Right focus output right\n",
        "bindsym Mod4+Ctrl+Up focus output up\n",
        "bindsym Mod4+Ctrl+Down focus output down\n",
        "bindsym Mod4+F2 wset 2\n",
        "bindsym Mod4+Shift+F2 move container to wset 2\n",
        "input \"1386:890:Wacom_Pen\" map_to_output DP-3\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(config.output_layout.unplug, UnplugPolicy::Move);
    assert_eq!(config.output_layout.last_output_focus_timeout, 3000);
    assert!(config.behavior.remove_output_limits);
    assert_eq!(config.wsets.label_duration, 900);
    let oswitch = &config.oswitch;
    assert_eq!(text(&oswitch.next_output), "super+o");
    assert_eq!(text(&oswitch.next_output_with_win), "super+shift+o");
    assert_eq!(text(&oswitch.prev_output), "super+i");
    assert_eq!(text(&oswitch.prev_output_with_win), "super+shift+i");
    assert_eq!(text(&oswitch.left_output), "super+ctrl+left");
    assert_eq!(text(&oswitch.right_output), "super+ctrl+right");
    assert_eq!(text(&oswitch.above_output), "super+ctrl+up");
    assert_eq!(text(&oswitch.below_output), "super+ctrl+down");
    assert_eq!(config.wsets.wsets_bindings[0].wset, 2);
    assert_eq!(
      text(&config.wsets.send_window_bindings[0].binding),
      "super+shift+f2"
    );
    assert_eq!(
      config.input_devices["1386:890:Wacom_Pen"].output.as_deref(),
      Some("DP-3")
    );
    assert!(
      ShingleConfig::parse("set $output_unplug keep\n", ConfigFormat::I3)
        .is_err()
    );
  }
}
