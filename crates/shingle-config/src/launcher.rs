//! How `shingle-launcher` looks and runs (docs/design/launcher.md, section
//! 8). Wayfire has no launcher, so a Wayfire or i3 import keeps the
//! defaults.

use serde::Deserialize;

use crate::ConfigError;

/// Which Vulkan driver draws the launcher's panel.
#[derive(
  Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum LauncherRenderer {
  /// lavapipe, Mesa's CPU driver, when installed (it starts in a fifth of
  /// the time of a hardware GPU), else the Vulkan loader's default.
  #[default]
  Auto,
  /// lavapipe, or fail.
  Software,
  /// The Vulkan loader's default, normally the hardware GPU.
  Hardware,
}

impl LauncherRenderer {
  /// The name the config uses: `auto`, `software` or `hardware`.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::Software => "software",
      Self::Hardware => "hardware",
    }
  }
}

impl std::str::FromStr for LauncherRenderer {
  type Err = ConfigError;

  fn from_str(text: &str) -> Result<Self, ConfigError> {
    match text.trim() {
      "auto" => Ok(Self::Auto),
      "software" => Ok(Self::Software),
      "hardware" => Ok(Self::Hardware),
      other => Err(ConfigError::Invalid(format!(
        "launcher.renderer must be auto, software or hardware, not {other:?}"
      ))),
    }
  }
}

/// The launcher's colours, `[r, g, b, a]` in 0–1 like every colour of the
/// config.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LauncherColors {
  /// Over the rest of the output; default black at 0.45.
  pub backdrop: [f64; 4],
  /// The panel's background.
  pub panel: [f64; 4],
  /// The search field and hovered rows.
  pub field: [f64; 4],
  /// Names and typed text.
  pub text: [f64; 4],
  /// Hints, comments and counts.
  pub dim_text: [f64; 4],
  /// The caret, the field's border, favourite marks and drop targets.
  pub accent: [f64; 4],
  /// The selected row and category.
  pub selection: [f64; 4],
}

impl Default for LauncherColors {
  fn default() -> Self {
    Self {
      backdrop: [0.0, 0.0, 0.0, 0.45],
      panel: [0.11, 0.12, 0.15, 1.0],
      field: [0.16, 0.18, 0.22, 1.0],
      text: [0.92, 0.93, 0.95, 1.0],
      dim_text: [0.58, 0.62, 0.68, 1.0],
      accent: [0.33, 0.58, 0.93, 1.0],
      selection: [0.2, 0.31, 0.48, 1.0],
    }
  }
}

/// The `launcher` section.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LauncherConfig {
  /// The panel's width in logical pixels; 200–4000; default 760. It
  /// shrinks to fit a smaller output.
  pub width: u32,
  /// The panel's height in logical pixels; 150–4000; default 480.
  pub height: u32,
  /// The size of entry names; 8–48; default 16. Everything else scales
  /// with it.
  pub font_size: f64,
  /// Which Vulkan driver draws; default `auto`.
  pub renderer: LauncherRenderer,
  /// The terminal for entries with `Terminal=true`, run as `TERMINAL -e
  /// COMMAND`; empty (the default) picks `$TERMINAL`, then the first of
  /// foot, kitty, alacritty and xterm installed.
  pub terminal: String,
  /// The icon theme entries' icons are looked up in, falling back to
  /// `hicolor`; empty (the default) uses the desktop's: KDE's
  /// `kdeglobals`, else GTK's `settings.ini`.
  pub icon_theme: String,
  /// Colours.
  pub colors: LauncherColors,
}

impl Default for LauncherConfig {
  fn default() -> Self {
    Self {
      width: 760,
      height: 480,
      font_size: 16.0,
      renderer: LauncherRenderer::Auto,
      terminal: String::new(),
      icon_theme: String::new(),
      colors: LauncherColors::default(),
    }
  }
}

impl LauncherConfig {
  /// Checks the sizes and colours.
  pub fn validate(&self) -> Result<(), ConfigError> {
    let invalid = |message: &str| Err(ConfigError::Invalid(message.into()));
    if !(200..=4000).contains(&self.width) {
      return invalid("launcher.width must be between 200 and 4000");
    }
    if !(150..=4000).contains(&self.height) {
      return invalid("launcher.height must be between 150 and 4000");
    }
    if !self.font_size.is_finite() || !(8.0..=48.0).contains(&self.font_size) {
      return invalid("launcher.font_size must be between 8 and 48");
    }
    let colors = &self.colors;
    for color in [
      colors.backdrop,
      colors.panel,
      colors.field,
      colors.text,
      colors.dim_text,
      colors.accent,
      colors.selection,
    ] {
      if color.iter().any(|component| {
        !component.is_finite() || !(0.0..=1.0).contains(component)
      }) {
        return invalid("launcher.colors components must be between 0 and 1");
      }
    }
    Ok(())
  }
}

/// `shingle-launcher` for a bare `rofi -show MODE` whose modes are all
/// ones the launcher covers (`drun`, `run`, `combi`), as Wayfire configs
/// bind rofi as their launcher; `None` for anything else: a path such as
/// `/usr/bin/rofi`, `-dmenu`, other modes such as `window` or `ssh`, and
/// commands with shell syntax, which are kept as written.
pub(crate) fn swap_rofi(command: &str) -> Option<String> {
  if command.contains(|c: char| ";|&$`()<>\\'\"".contains(c)) {
    return None;
  }
  let words: Vec<&str> = command.split_whitespace().collect();
  if words.first() != Some(&"rofi") || words.contains(&"-dmenu") {
    return None;
  }
  let covered = |modes: &str| {
    modes
      .split(',')
      .all(|mode| matches!(mode, "drun" | "run" | "combi"))
  };
  let mut shown = false;
  for pair in words.windows(2) {
    match pair[0] {
      "-show" => {
        if !covered(pair[1]) {
          return None;
        }
        shown = true;
      }
      "-modi" | "-modes" | "-combi-modi" | "-combi-modes"
        if !covered(pair[1]) =>
      {
        return None;
      }
      _ => {}
    }
  }
  shown.then(|| "shingle-launcher".to_owned())
}

#[cfg(test)]
mod tests {
  use crate::{ConfigFormat, ShingleConfig};

  use super::*;

  #[test]
  fn the_section_is_optional_and_partial() {
    let (config, _) = ShingleConfig::parse_with_warnings(
      "launcher:\n  width: 900\n  renderer: hardware\n  icon_theme: \
       breeze\n  colors:\n    accent: [1, 0, 1, 1]\n",
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(config.launcher.width, 900);
    assert_eq!(config.launcher.height, 480);
    assert_eq!(config.launcher.renderer, LauncherRenderer::Hardware);
    assert_eq!(config.launcher.icon_theme, "breeze");
    assert_eq!(config.launcher.colors.accent, [1.0, 0.0, 1.0, 1.0]);
    assert_eq!(config.launcher.colors.panel, [0.11, 0.12, 0.15, 1.0]);
    let (config, _) =
      ShingleConfig::parse_with_warnings("", ConfigFormat::Yaml).unwrap();
    assert_eq!(config.launcher, LauncherConfig::default());
  }

  #[test]
  fn the_sample_config_binds_and_themes_the_launcher() {
    let sample = include_str!("../../../data/shingle.yaml");
    let (config, _) =
      ShingleConfig::parse_with_warnings(sample, ConfigFormat::Yaml).unwrap();
    assert_eq!(config.launcher, LauncherConfig::default());
    let launcher = config
      .commands
      .iter()
      .find(|command| command.command == "shingle-launcher")
      .expect("a launcher binding");
    assert_eq!(String::from(launcher.binding.clone()), "super+space");
  }

  #[test]
  fn renderer_names_round_trip() {
    for renderer in [
      LauncherRenderer::Auto,
      LauncherRenderer::Software,
      LauncherRenderer::Hardware,
    ] {
      assert_eq!(
        renderer.as_str().parse::<LauncherRenderer>().ok(),
        Some(renderer)
      );
    }
    assert!(" gpu ".parse::<LauncherRenderer>().is_err());
  }

  #[test]
  fn bad_values_are_refused() {
    for text in [
      "launcher:\n  width: 10\n",
      "launcher:\n  font_size: 100\n",
      "launcher:\n  colors:\n    text: [2, 0, 0, 1]\n",
      "launcher:\n  renderer: gpu\n",
      "launcher:\n  colour: x\n",
    ] {
      assert!(
        ShingleConfig::parse_with_warnings(text, ConfigFormat::Yaml).is_err(),
        "{text}"
      );
    }
  }

  #[test]
  fn a_wayfire_import_keeps_the_defaults_and_runs_the_launcher_for_rofi() {
    let (config, _) = ShingleConfig::parse_with_warnings(
      "[command]\nbinding_launcher = <super> KEY_D\n\
       command_launcher = rofi -show drun\n",
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(config.launcher, LauncherConfig::default());
    let commands = config
      .commands
      .iter()
      .map(|command| command.command.as_str())
      .collect::<Vec<_>>();
    assert_eq!(commands, ["shingle-launcher"]);
  }

  #[test]
  fn rofi_as_a_launcher_becomes_shingle_launcher() {
    for command in [
      "rofi -show drun",
      "rofi -show run",
      "rofi -show combi -combi-modi drun,run",
      "rofi -show drun -show-icons -theme Arc",
      "  rofi   -show drun ",
    ] {
      assert_eq!(
        swap_rofi(command).as_deref(),
        Some("shingle-launcher"),
        "{command}"
      );
    }
  }

  #[test]
  fn other_rofi_commands_are_kept() {
    for command in [
      "/usr/bin/rofi -show drun",
      "rofi -show window",
      "rofi -show ssh",
      "rofi -show drun -modi drun,window",
      "rofi -dmenu",
      "rofi",
      "rofi -show drun; notify-send done",
      "echo a | rofi -dmenu",
      "rofi -show drun -theme '~/x.rasi'",
      "rofi-wayland -show drun",
      "kitty",
    ] {
      assert_eq!(swap_rofi(command), None, "{command}");
    }
  }
}
