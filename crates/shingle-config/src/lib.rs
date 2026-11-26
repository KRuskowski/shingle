//! Shingle's configuration: the validated YAML model, imports from Wayfire
//! INI and i3-style files, and XKB keymap generation.
//!
//! [`ShingleConfig`] is the native format and the only one written back.
//! [`ShingleConfig::load`] picks the syntax from the file name or contents
//! ([`ConfigFormat`]), and every route into a `ShingleConfig` ends in
//! [`ShingleConfig::validate`], so a value returned by this crate is always
//! within the documented ranges. YAML is strict, but Wayfire and i3 files
//! are read as Wayfire reads its own: a value Shingle cannot use keeps its
//! default and becomes a [`ConfigWarning`], which
//! [`ShingleConfig::load_with_warnings`] returns with the configuration.
//! Most sections mirror a Wayfire plugin; field
//! docs name the Wayfire option each one is imported from, and Wayfire's
//! `metadata/*.xml` is the behavioural reference for those options.
//!
//! Window rules ([`WindowRule`]) and view matchers ([`ViewMatcher`]) use
//! Wayfire's syntax. [`ConfigWatcher`] implements automatic reloading.
//!
//! [`default_path`] falls back to the pre-rename Tilefire location when only
//! that file exists; see README.md#upgrading-from-tilefire.

#![warn(missing_docs)]

mod activator;
mod config_watcher;
mod i3;
mod import;
mod ini;
mod layout;
mod multi_output;
mod output;
mod plugins;
mod view_matcher;
mod wayfire;
mod wayfire_options;
mod window_rule;

pub use config_watcher::ConfigWatcher;
pub use layout::{
  LayoutOutput, closest_point, disjoint, layout, layout_warnings, logical_size,
  output_in_direction, output_relative, overlapping,
};
pub use multi_output::{
  InputDeviceConfig, OswitchConfig, OutputLayoutConfig, UnplugPolicy,
  WsetBinding, WsetsConfig,
};
pub use output::{OutputConfig, OutputMode, OutputPosition};
pub use plugins::PLUGINS;
pub use view_matcher::{ViewMatcher, ViewProperties};
pub use wayfire_options::{
  OptionValue, WayfireOption, WayfireValues, wayfire_option, wayfire_options,
};
pub use window_rule::{RuleAction, RuleEvent, WindowRule};

use std::{
  collections::BTreeMap,
  fs, io,
  path::{Path, PathBuf},
  time::Duration,
};

use serde::Deserialize;
use shingle_core::Gaps;
use thiserror::Error;

/// Source syntax accepted by [`ShingleConfig::parse`].
///
/// Imports are one way: a Wayfire or i3 file is read into a [`ShingleConfig`]
/// and can then be saved as YAML, but nothing writes INI or i3 syntax back.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConfigFormat {
  /// Shingle's native YAML, deserialized straight into [`ShingleConfig`]. Every
  /// section rejects unknown fields, so a typo is an error rather than a
  /// silently ignored key.
  #[default]
  Yaml,
  /// Wayfire's `wayfire.ini`, read line by line as wf-config reads it: an
  /// unescaped `#` starts a comment, `\#` is a literal `#` (WCM writes every
  /// hex colour that way), a line ending in `\` continues on the next, and
  /// names and values are trimmed. Recognised `[section] option` pairs are
  /// imported; whole sections of plugins Shingle does not have are reported
  /// once each, and other options are ignored. A value Shingle cannot use
  /// keeps its default and is reported with its line, as wf-config does, so
  /// any readable file loads.
  ///
  /// Import starts from Wayfire's defaults (data/wayfire-options.tsv, see
  /// [`wayfire_options`]), not from [`ShingleConfig::default`]: an empty
  /// file has a 3×3 workspace grid and exits on `ctrl+alt+backspace`.
  /// Shingle's bindings that no Wayfire option sets (moving and resizing
  /// tiles, `super+1` to `super+0`, reload and lock) are off unless the file
  /// sets them with Shingle's own names. `[core] plugins`, or Wayfire's
  /// default list when the file has none, becomes
  /// [`ShingleConfig::plugins`]: options of a plugin it leaves out are not
  /// applied, and the plugin is off. With `[autostart] autostart_wf_shell`
  /// (default true), `wf-panel` and `wf-background` are added to
  /// [`ShingleConfig::autostart`] unless an entry already runs them, as
  /// Wayfire's autostart plugin does. Every option the file sets is kept as
  /// written in [`ShingleConfig::wayfire_values`].
  ///
  /// Activators are Wayfire's `<super> <shift> KEY_Q` form with `|` between
  /// alternatives. Keys and buttons are bound; a modifier alone, touch
  /// gestures and hotspots are recognised and reported as not supported yet,
  /// and never bound as keys. Where a field takes one binding, the first key
  /// or button is used.
  Wayfire,
  /// An i3- or sway-style config. `bindsym`, `exec`, `gaps` and `output` are
  /// understood, Shingle-specific options are given as `set $option value` (for
  /// example `set $scale_spacing 40`), and any other `set $name value` becomes
  /// a variable substituted into later lines, longest name first. Unknown
  /// commands are ignored. `bindsym` recognises a fixed set of command strings
  /// such as `focus left`, `workspace 3`, `move container to workspace next`
  /// and `exec CMD`; any other `exec` binding becomes a [`CommandBinding`].
  /// `exec shingle-lock` binds the lock key instead.
  ///
  /// As with Wayfire files, a line whose value Shingle cannot use keeps the
  /// default and is reported with its line number.
  I3,
}

/// The complete compositor configuration.
///
/// Every section is `#[serde(default, deny_unknown_fields)]`: YAML may omit any
/// field, which then takes the default documented on it, but may not contain
/// unknown ones. A value returned by [`ShingleConfig::load`] or
/// [`ShingleConfig::parse`] has passed [`ShingleConfig::validate`]; one built
/// by hand or edited in place (as `shingle-wcm` does) must be validated again
/// before it is used or saved.
///
/// Unless a field says otherwise, durations are milliseconds, distances are
/// logical pixels and colours are `[r, g, b, a]` with every component in 0–1.
/// Field docs name the Wayfire option a value is imported from as `[section]
/// option`; Wayfire's `metadata/*.xml` describes what each option does there.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShingleConfig {
  /// The default output configuration: for every output without its own
  /// entry in `outputs`, and the size of the nested window and the headless
  /// output. A Wayfire `[output]` section without a name, or i3 `output *`.
  pub output: OutputConfig,
  /// One entry per named output, from Wayfire's `[output:NAME]` sections
  /// and i3's `output NAME` lines; see
  /// [`ShingleConfig::output_config`]. Default empty.
  pub outputs: Vec<OutputConfig>,
  /// What happens to the windows of an output that goes, and focus when it
  /// returns.
  pub output_layout: OutputLayoutConfig,
  /// Bindings that focus another output (`[oswitch]`).
  pub oswitch: OswitchConfig,
  /// Workspace set bindings (`[wsets]`).
  pub wsets: WsetsConfig,
  /// Per-device input settings by device name, from Wayfire's
  /// `[input-device:NAME]` sections and i3 `input NAME` lines. Default
  /// empty.
  pub input_devices: BTreeMap<String, InputDeviceConfig>,
  /// Colour shown where no surface covers the output. `[core]
  /// background_color`; default `[0.1, 0.1, 0.1, 1.0]`.
  pub background_color: [f64; 4],
  /// Config reload, dialog, decoration and buffer-size behaviour, mostly from
  /// Wayfire's `[workarounds]`.
  pub behavior: BehaviorConfig,
  /// Whether and when Shingle runs Xwayland for X11 clients, from
  /// Wayfire's `[core] xwayland` options.
  pub xwayland: XwaylandConfig,
  /// Protocols withheld from sandboxed clients (`[security-context-v1]`).
  pub security_context: SecurityContextConfig,
  /// Modifier-and-scroll output magnification (`[zoom]`).
  pub zoom: ZoomConfig,
  /// Modifier-and-scroll window opacity (`[alpha]`).
  pub alpha: AlphaConfig,
  /// The Alt+Escape switcher (`[fast-switcher]`).
  pub fast_switcher: FastSwitcherConfig,
  /// Open, close and minimize animations (`[animate]`).
  pub animate: AnimateConfig,
  /// The Scale window overview (`[scale]`).
  pub scale: ScaleConfig,
  /// Type-to-filter in Scale (`[scale-title-filter]`).
  pub scale_title_filter: ScaleTitleFilterConfig,
  /// The Expo workspace overview (`[expo]`).
  pub expo: ExpoConfig,
  /// Options of the `invert` and `wrot` effect plugins.
  pub effects: EffectsConfig,
  /// Where new floating windows appear (`[place]`).
  pub placement: PlacementConfig,
  /// Click-to-focus, from Wayfire's `[core]`.
  pub focus: FocusConfig,
  /// Checks on `xdg-activation-v1` tokens (`[xdg-activation]`).
  pub activation: ActivationConfig,
  /// Keyboard-shortcuts-inhibit policy (`[shortcuts-inhibit]`).
  pub shortcuts: ShortcutConfig,
  /// Snapping and edge behaviour during an interactive move. Despite the name
  /// these come from Wayfire's `[move]`; the `[grid]` slot bindings live in
  /// [`Keybindings`].
  pub grid: GridConfig,
  /// Multi-finger touch gestures (`[extra-gestures]`).
  pub extra_gestures: ExtraGesturesConfig,
  /// Size of the workspace grid and edge wraparound.
  pub workspaces: WorkspaceConfig,
  /// The sliding workspace-switch transition (`[vswitch]`).
  pub vswitch: VswitchConfig,
  /// Touchpad workspace swipes (`[vswipe]`).
  pub vswipe: VswipeConfig,
  /// Gaps, tiling policy, minimum tile size and drag previews, mostly from
  /// `[simple-tile]`.
  pub tiling: TilingConfig,
  /// Interactive resize limits (`[resize]`).
  pub resize: ResizeConfig,
  /// Idle, DPMS and lock timeouts (`[idle]`).
  pub idle: IdleConfig,
  /// Whether and where the tiling session is saved and restored.
  pub session: SessionConfig,
  /// Commands started once after the compositor is up, each through `/bin/sh
  /// -c` with `WAYLAND_DISPLAY` set. A command is skipped when a restored
  /// session already relaunched an application started by the same command
  /// line. Imported from every non-empty `[autostart]` value except
  /// `autostart_wf_shell`, and from i3 `exec` lines (`--no-startup-id` is
  /// dropped).
  pub autostart: Vec<String>,
  /// Wayfire `[window-rules]` entries in file order. When an event fires, every
  /// rule for that event whose matcher fits is applied in this order, so a
  /// later rule can override an earlier one.
  pub window_rules: Vec<WindowRule>,
  /// Keyboard bindings for built-in actions.
  pub keybindings: Keybindings,
  /// Bindings that run shell commands: Wayfire `[command]` `binding_NAME` and
  /// `command_NAME` pairs, and i3 `bindsym KEY exec CMD`.
  pub commands: Vec<CommandBinding>,
  /// Modifier-and-button bindings for move, resize and rotation.
  pub pointer: PointerBindings,
  /// Keymap, key repeat, pointer and cursor settings (`[input]`).
  pub input: InputConfig,
  /// Wayfire's `[core] plugins`: when set, only the plugins of
  /// [`PLUGINS`] it names are on, and [`ShingleConfig::plugin_enabled`]
  /// says which. A Wayfire import always sets it; default unset, which
  /// turns every plugin on.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub plugins: Option<Vec<String>>,
  /// The options a Wayfire file set, as written, for
  /// `wayfire/get-config-option`. Empty for other formats; never
  /// written to YAML.
  #[serde(skip)]
  pub wayfire_values: WayfireValues,
  /// The format this configuration was parsed from: set by
  /// [`ShingleConfig::parse_with_warnings`], and [`ConfigFormat::Yaml`]
  /// for [`ShingleConfig::default`] and configurations built by hand.
  /// [`ShingleConfig::wayfire_config_file`] reads it. Never written to
  /// YAML.
  #[serde(skip)]
  pub format: ConfigFormat,
}

impl Default for ShingleConfig {
  fn default() -> Self {
    Self {
      output: OutputConfig::default(),
      outputs: Vec::new(),
      output_layout: OutputLayoutConfig::default(),
      oswitch: OswitchConfig::default(),
      wsets: WsetsConfig::default(),
      input_devices: BTreeMap::new(),
      background_color: [0.1, 0.1, 0.1, 1.0],
      behavior: BehaviorConfig::default(),
      xwayland: XwaylandConfig::default(),
      security_context: SecurityContextConfig::default(),
      zoom: ZoomConfig::default(),
      alpha: AlphaConfig::default(),
      fast_switcher: FastSwitcherConfig::default(),
      animate: AnimateConfig::default(),
      scale: ScaleConfig::default(),
      scale_title_filter: ScaleTitleFilterConfig::default(),
      expo: ExpoConfig::default(),
      effects: EffectsConfig::default(),
      placement: PlacementConfig::default(),
      focus: FocusConfig::default(),
      activation: ActivationConfig::default(),
      shortcuts: ShortcutConfig::default(),
      grid: GridConfig::default(),
      extra_gestures: ExtraGesturesConfig::default(),
      workspaces: WorkspaceConfig::default(),
      vswitch: VswitchConfig::default(),
      vswipe: VswipeConfig::default(),
      tiling: TilingConfig::default(),
      resize: ResizeConfig::default(),
      idle: IdleConfig::default(),
      session: SessionConfig::default(),
      autostart: Vec::new(),
      window_rules: Vec::new(),
      keybindings: Keybindings::default(),
      commands: Vec::new(),
      pointer: PointerBindings::default(),
      input: InputConfig::default(),
      plugins: None,
      wayfire_values: WayfireValues::default(),
      format: ConfigFormat::Yaml,
    }
  }
}

impl ShingleConfig {
  /// Reads, parses and validates the configuration at `path`, or at
  /// [`default_path`] when `path` is `None`, dropping any warnings; see
  /// [`ShingleConfig::load_with_warnings`].
  ///
  /// # Errors
  ///
  /// As [`ShingleConfig::load_with_warnings`].
  pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
    Self::load_with_warnings(path).map(|(config, _)| config)
  }

  /// Reads, parses and validates the configuration at `path`, or at
  /// [`default_path`] when `path` is `None`, and returns it with the
  /// warnings about what a Wayfire or i3 file held that Shingle could not
  /// use.
  ///
  /// The format is chosen in this order: Wayfire when the extension is `.ini`
  /// or the first non-blank character is `[`, i3 when the extension is `.conf`
  /// or `.i3`, and YAML otherwise. A missing file at the default path yields
  /// [`ShingleConfig::default`], so a fresh install needs no config; a missing
  /// file at an explicit path is an error.
  ///
  /// # Errors
  ///
  /// [`ConfigError::Read`] if the file cannot be read, otherwise whatever
  /// [`ShingleConfig::parse_with_warnings`] returns.
  pub fn load_with_warnings(
    path: Option<&Path>,
  ) -> Result<(Self, Vec<ConfigWarning>), ConfigError> {
    let path = path.map(Path::to_owned).unwrap_or_else(default_path);
    let text = match fs::read_to_string(&path) {
      Ok(text) => text,
      Err(error)
        if error.kind() == io::ErrorKind::NotFound
          && path == default_path() =>
      {
        return Ok((Self::default(), Vec::new()));
      }
      Err(source) => {
        return Err(ConfigError::Read {
          path: path.clone(),
          source,
        });
      }
    };
    let format = if path.extension().is_some_and(|extension| extension == "ini")
      || text.trim_start().starts_with('[')
    {
      ConfigFormat::Wayfire
    } else if path
      .extension()
      .is_some_and(|extension| extension == "conf" || extension == "i3")
    {
      ConfigFormat::I3
    } else {
      ConfigFormat::Yaml
    };
    Self::parse_with_warnings(&text, format)
  }

  /// Parses `text` as `format` and validates the result, dropping any
  /// warnings; see [`ShingleConfig::parse_with_warnings`].
  ///
  /// # Errors
  ///
  /// As [`ShingleConfig::parse_with_warnings`].
  pub fn parse(text: &str, format: ConfigFormat) -> Result<Self, ConfigError> {
    Self::parse_with_warnings(text, format).map(|(config, _)| config)
  }

  /// Parses `text` as `format`, validates the result and returns it with the
  /// warnings, in line order. YAML never warns: anything it cannot use is an
  /// error. Wayfire and i3 files warn instead (see [`ConfigFormat`]), so
  /// they only fail on a bug. Plugins [`ShingleConfig::plugins`] leaves out
  /// are turned off ([`ShingleConfig::turn_off_unlisted_plugins`]) in every
  /// format.
  ///
  /// # Errors
  ///
  /// For YAML, [`ConfigError::Yaml`] for malformed YAML, a wrong type or an
  /// unknown field; [`ConfigError::Binding`] for an unparsable key binding;
  /// [`ConfigError::Invalid`] for an invalid colour, matcher or window rule,
  /// or any [`ShingleConfig::validate`] failure.
  pub fn parse_with_warnings(
    text: &str,
    format: ConfigFormat,
  ) -> Result<(Self, Vec<ConfigWarning>), ConfigError> {
    let (mut config, warnings) = match format {
      ConfigFormat::Yaml => (serde_yaml::from_str::<Self>(text)?, Vec::new()),
      ConfigFormat::Wayfire => wayfire::parse(text)?,
      ConfigFormat::I3 => i3::parse(text)?,
    };
    config.format = format;
    config.turn_off_unlisted_plugins();
    config.validate()?;
    Ok((config, warnings))
  }

  /// What Shingle's children get as `WAYFIRE_CONFIG_FILE` when this
  /// configuration was loaded from `path`: `path`, made absolute, when it
  /// is a Wayfire file, as Wayfire exports the file it reads
  /// (src/default-config-backend.cpp), and `None` for YAML and i3. WCM
  /// and scripts read the variable and edit the file as `wayfire.ini`, so
  /// it never names a file in another format.
  pub fn wayfire_config_file(&self, path: &Path) -> Option<PathBuf> {
    (self.format == ConfigFormat::Wayfire)
      .then(|| std::path::absolute(path).unwrap_or_else(|_| path.to_owned()))
  }

  /// Validates and serializes the configuration as native YAML. Matchers and
  /// window rules are written back as the text they were parsed from; bindings
  /// are written in the canonical `super+shift+key` form.
  ///
  /// # Errors
  ///
  /// [`ConfigError::Invalid`] if validation fails, or [`ConfigError::Yaml`] if
  /// serialization does.
  pub fn to_yaml(&self) -> Result<String, ConfigError> {
    self.validate()?;
    Ok(serde_yaml::to_string(self)?)
  }

  /// Validates the configuration and atomically replaces `path` with its YAML
  /// form.
  ///
  /// The YAML is written to a new hidden file `.NAME.PID.N.tmp` next to `path`
  /// (creating the directory if needed), synced, then renamed over `path`, so a
  /// reader such as a [`ConfigWatcher`] sees either the old or the new file,
  /// never a partial one. The temporary file is removed if writing or renaming
  /// fails. The directory is synced after the rename, so the new file
  /// survives a crash; if that sync fails, the error is returned although
  /// `path` already holds the new contents.
  ///
  /// # Errors
  ///
  /// [`ConfigError::Invalid`] if validation fails or `path` has no UTF-8 file
  /// name, and [`ConfigError::Write`] for any I/O failure, including when 100
  /// temporary names in a row already exist.
  pub fn save_yaml(&self, path: &Path) -> Result<(), ConfigError> {
    let yaml = self.to_yaml()?;
    let parent = path
      .parent()
      .filter(|parent| !parent.as_os_str().is_empty())
      .unwrap_or_else(|| Path::new("."));
    let file_name = path
      .file_name()
      .and_then(|name| name.to_str())
      .ok_or_else(|| {
        ConfigError::Invalid("config path must name a file".into())
      })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
      path: path.to_owned(),
      source,
    })?;

    let mut temporary = None;
    for attempt in 0..100 {
      let candidate = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        attempt
      ));
      match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&candidate)
      {
        Ok(file) => {
          temporary = Some((candidate, file));
          break;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
          return Err(ConfigError::Write {
            path: path.to_owned(),
            source,
          });
        }
      }
    }
    let (temporary_path, mut file) =
      temporary.ok_or_else(|| ConfigError::Write {
        path: path.to_owned(),
        source: io::Error::new(
          io::ErrorKind::AlreadyExists,
          "could not allocate a temporary config file",
        ),
      })?;
    let result = (|| {
      io::Write::write_all(&mut file, yaml.as_bytes())?;
      file.sync_all()?;
      drop(file);
      fs::rename(&temporary_path, path)?;
      // Make the rename itself durable, as Shingle::save_atomic does.
      fs::File::open(parent)?.sync_all()
    })();
    if let Err(source) = result {
      let _ = fs::remove_file(&temporary_path);
      return Err(ConfigError::Write {
        path: path.to_owned(),
        source,
      });
    }
    Ok(())
  }

  /// Checks the ranges and cross-field constraints serde cannot express. The
  /// limits are listed on the fields they apply to; the cross-field ones are
  /// that Expo workspace bindings and `assign_workspace` window rules must name
  /// a workspace inside the configured grid, and that `workspace_switch` and
  /// `workspace_move` hold exactly ten bindings each.
  ///
  /// # Errors
  ///
  /// The first violated constraint, as [`ConfigError::Invalid`] with a message
  /// naming the field.
  pub fn validate(&self) -> Result<(), ConfigError> {
    self.validate_fields()?;
    self.validate_references()
  }

  /// Every check of [`ShingleConfig::validate`] that concerns one field or
  /// fields set by the same option, which the importers run after each
  /// option.
  pub(crate) fn validate_fields(&self) -> Result<(), ConfigError> {
    if !(1..=20).contains(&self.extra_gestures.move_fingers)
      || !(1..=20).contains(&self.extra_gestures.close_fingers)
      || !(1..=10_000).contains(&self.extra_gestures.move_delay)
    {
      return Err(ConfigError::Invalid(
        concat!(
          "extra_gestures finger counts must be 1–20 and ",
          "move_delay must be 1–10000",
        )
        .into(),
      ));
    }
    if !self.effects.wrot_reset_radius.is_finite()
      || self.effects.wrot_reset_radius <= 0.0
    {
      return Err(ConfigError::Invalid(
        "effects.wrot_reset_radius must be finite and positive".into(),
      ));
    }
    if !matches!(
      self.placement.mode.as_str(),
      "center" | "cascade" | "maximize" | "random" | "pointer" | "none"
    ) {
      return Err(ConfigError::Invalid("unknown placement mode".into()));
    }
    self.validate_outputs()?;
    self.wsets.validate()?;
    multi_output::validate_input_devices(&self.input_devices)?;
    if self.behavior.max_buffer_size == 0 {
      return Err(ConfigError::Invalid(
        "behavior.max_buffer_size must be nonzero".into(),
      ));
    }
    if !matches!(
      self.behavior.preferred_decoration_mode.as_str(),
      "client" | "server"
    ) {
      return Err(ConfigError::Invalid(
        "behavior.preferred_decoration_mode must be client or server".into(),
      ));
    }
    if !self.input.mouse_scroll_speed.is_finite()
      || self.input.mouse_scroll_speed < 0.0
    {
      return Err(ConfigError::Invalid(
        "input.mouse_scroll_speed must be finite and non-negative".into(),
      ));
    }
    self.input.validate_devices()?;
    if !(1..=256).contains(&self.input.cursor_size) {
      return Err(ConfigError::Invalid(
        "input.cursor_size must be between 1 and 256".into(),
      ));
    }
    if !self.zoom.speed.is_finite() || self.zoom.speed < 0.0 {
      return Err(ConfigError::Invalid(
        "zoom.speed must be finite and non-negative".into(),
      ));
    }
    if !self.alpha.min_value.is_finite()
      || !(0.0..=1.0).contains(&self.alpha.min_value)
    {
      return Err(ConfigError::Invalid(
        "alpha.min_value must be between 0 and 1".into(),
      ));
    }
    if !self.fast_switcher.inactive_alpha.is_finite()
      || !(0.0..=1.0).contains(&self.fast_switcher.inactive_alpha)
    {
      return Err(ConfigError::Invalid(
        "fast_switcher.inactive_alpha must be between 0 and 1".into(),
      ));
    }
    if !self.scale.inactive_alpha.is_finite()
      || !(0.0..=1.0).contains(&self.scale.inactive_alpha)
    {
      return Err(ConfigError::Invalid(
        "scale.inactive_alpha must be between 0 and 1".into(),
      ));
    }
    if !self.scale.minimized_alpha.is_finite()
      || !(0.0..=1.0).contains(&self.scale.minimized_alpha)
    {
      return Err(ConfigError::Invalid(
        "scale.minimized_alpha must be between 0 and 1".into(),
      ));
    }
    if !self.expo.inactive_brightness.is_finite()
      || !(0.0..=1.0).contains(&self.expo.inactive_brightness)
    {
      return Err(ConfigError::Invalid(
        "expo.inactive_brightness must be between 0 and 1".into(),
      ));
    }
    if self.expo.background.iter().any(|component| {
      !component.is_finite() || !(0.0..=1.0).contains(component)
    }) {
      return Err(ConfigError::Invalid(
        "expo.background components must be between 0 and 1".into(),
      ));
    }
    if self.vswitch.background.iter().any(|component| {
      !component.is_finite() || !(0.0..=1.0).contains(component)
    }) {
      return Err(ConfigError::Invalid(
        "vswitch.background components must be between 0 and 1".into(),
      ));
    }
    let swipe = &self.vswipe;
    if swipe.background.iter().any(|component| {
      !component.is_finite() || !(0.0..=1.0).contains(component)
    }) {
      return Err(ConfigError::Invalid(
        "vswipe.background components must be between 0 and 1".into(),
      ));
    }
    let valid = (1..=20).contains(&swipe.fingers)
      && swipe.gap.is_finite()
      && swipe.gap >= 0.0
      && swipe.threshold.is_finite()
      && swipe.delta_threshold.is_finite()
      && swipe.speed_factor.is_finite()
      && swipe.speed_factor > 0.0
      && swipe.speed_cap.is_finite()
      && swipe.speed_cap >= 0.0;
    if !valid {
      return Err(ConfigError::Invalid(
        concat!(
          "vswipe.fingers must be 1–20, gap and speed_cap at least 0, ",
          "speed_factor above 0 and the thresholds finite",
        )
        .into(),
      ));
    }
    if !matches!(self.scale.title_overlay.as_str(), "never" | "mouse" | "all") {
      return Err(ConfigError::Invalid(
        "scale.title_overlay must be never, mouse, or all".into(),
      ));
    }
    if !matches!(
      self.scale.title_position.as_str(),
      "top" | "center" | "bottom"
    ) {
      return Err(ConfigError::Invalid(
        "scale.title_position must be top, center, or bottom".into(),
      ));
    }
    if !(1..=256).contains(&self.scale.title_font_size) {
      return Err(ConfigError::Invalid(
        "scale.title_font_size must be between 1 and 256".into(),
      ));
    }
    if !(1..=256).contains(&self.scale_title_filter.font_size) {
      return Err(ConfigError::Invalid(
        "scale_title_filter.font_size must be between 1 and 256".into(),
      ));
    }
    for color in [
      self.scale.title_bg_color,
      self.scale.title_text_color,
      self.scale_title_filter.bg_color,
      self.scale_title_filter.text_color,
    ] {
      if color.iter().any(|component| {
        !component.is_finite() || !(0.0..=1.0).contains(component)
      }) {
        return Err(ConfigError::Invalid(
          "scale title color components must be between 0 and 1".into(),
        ));
      }
    }
    if !matches!(
      self.animate.open_animation.as_str(),
      "none" | "fade" | "zoom"
    ) {
      return Err(ConfigError::Invalid(
        "animate.open_animation must be none, fade, or zoom".into(),
      ));
    }
    if !matches!(
      self.animate.close_animation.as_str(),
      "none" | "fade" | "zoom"
    ) {
      return Err(ConfigError::Invalid(
        "animate.close_animation must be none, fade, or zoom".into(),
      ));
    }
    if !matches!(
      self.animate.minimize_animation.as_str(),
      "none" | "fade" | "zoom"
    ) {
      return Err(ConfigError::Invalid(
        "animate.minimize_animation must be none, fade, or zoom".into(),
      ));
    }
    if !matches!(self.zoom.interpolation.as_str(), "linear" | "nearest") {
      return Err(ConfigError::Invalid(
        "zoom.interpolation must be linear or nearest".into(),
      ));
    }
    if self.background_color.iter().any(|component| {
      !component.is_finite() || !(0.0..=1.0).contains(component)
    }) {
      return Err(ConfigError::Invalid(
        "background_color components must be between 0 and 1".into(),
      ));
    }
    for color in [
      self.tiling.preview_base_color,
      self.tiling.preview_border_color,
    ] {
      if color.iter().any(|component| {
        !component.is_finite() || !(0.0..=1.0).contains(component)
      }) {
        return Err(ConfigError::Invalid(
          "preview color components must be between 0 and 1".into(),
        ));
      }
    }
    if self.workspaces.width == 0 || self.workspaces.height == 0 {
      return Err(ConfigError::Invalid(
        "workspaces.width and workspaces.height must be nonzero".into(),
      ));
    }
    if !matches!(
      self.workspaces.wraparound.as_str(),
      "false" | "true" | "on_end"
    ) {
      return Err(ConfigError::Invalid(
        "workspaces.wraparound must be false, true, or on_end".into(),
      ));
    }
    if self.tiling.minimum_extent == 0 {
      return Err(ConfigError::Invalid(
        "tiling.minimum_extent must be nonzero".into(),
      ));
    }
    if !self.resize.corner_threshold.is_finite()
      || !(0.0..=0.5).contains(&self.resize.corner_threshold)
    {
      return Err(ConfigError::Invalid(
        "resize.corner_threshold must be between 0 and 0.5".into(),
      ));
    }
    Ok(())
  }

  /// The constraints between fields: Expo workspace bindings and
  /// `assign_workspace` window rules name a workspace inside the grid, and
  /// `workspace_switch` and `workspace_move` hold ten bindings each.
  fn validate_references(&self) -> Result<(), ConfigError> {
    let workspace_count =
      u64::from(self.workspaces.width) * u64::from(self.workspaces.height);
    if self.expo.workspace_bindings.iter().any(|entry| {
      entry.workspace == 0 || entry.workspace as u64 > workspace_count
    }) {
      return Err(ConfigError::Invalid(
        "Expo workspace bindings must name a configured workspace".into(),
      ));
    }
    for rule in &self.window_rules {
      if let RuleAction::AssignWorkspace { x, y } = rule.action
        && (x >= self.workspaces.width || y >= self.workspaces.height)
      {
        return Err(ConfigError::Invalid(format!(
          "window rule workspace {x},{y} is outside the configured grid"
        )));
      }
    }
    for entry in self.vswitch.all_bindings() {
      if entry.workspace == 0 || entry.workspace as u64 > workspace_count {
        return Err(ConfigError::Invalid(
          "vswitch workspace bindings must name a configured workspace".into(),
        ));
      }
    }
    if self.keybindings.workspace_switch.len() != 10
      || self.keybindings.workspace_move.len() != 10
    {
      return Err(ConfigError::Invalid(
        "workspace_switch and workspace_move must each contain 10 bindings"
          .into(),
      ));
    }
    Ok(())
  }
}

/// Parses a colour in any of Wayfire's notations: `#RRGGBBAA` or `#RGBA` hex
/// as wf-config reads them, `#RRGGBB` hex with alpha 1, or four
/// whitespace-separated numbers `r g b a`. Surrounding whitespace is ignored.
///
/// # Errors
///
/// [`ConfigError::Invalid`] for any other length or syntax, or for a component
/// that is not finite or lies outside 0–1. Short `#RGB` hex is not accepted.
pub fn parse_color(text: &str) -> Result<[f64; 4], ConfigError> {
  let invalid = || ConfigError::Invalid(format!("invalid color {text:?}"));
  let text = text.trim();
  let color = if let Some(hex) = text.strip_prefix('#') {
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
      return Err(invalid());
    }
    let (digits, scale) = match hex.len() {
      4 => (1, 15.0),
      6 | 8 => (2, 255.0),
      _ => return Err(invalid()),
    };
    let mut color = [1.0; 4];
    for (component, chunk) in
      color.iter_mut().zip(hex.as_bytes().chunks(digits))
    {
      let chunk = std::str::from_utf8(chunk).map_err(|_| invalid())?;
      let value = u8::from_str_radix(chunk, 16).map_err(|_| invalid())?;
      *component = f64::from(value) / scale;
    }
    color
  } else {
    let components = text.split_whitespace().collect::<Vec<_>>();
    if components.len() != 4 {
      return Err(invalid());
    }
    let mut color = [0.0; 4];
    for (component, target) in components.into_iter().zip(&mut color) {
      *target = component.parse().map_err(|_| invalid())?;
    }
    color
  };
  if color
    .iter()
    .any(|component| !component.is_finite() || !(0.0..=1.0).contains(component))
  {
    return Err(invalid());
  }
  Ok(color)
}

/// Snapping during an interactive move, from Wayfire's `[move]` plugin
/// (metadata/move.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridConfig {
  /// Whether dragging a window to an output edge snaps it to that half, or to a
  /// quarter near a corner. `[move] enable_snap`; default true.
  pub enable_snap: bool,
  /// Distance from an output edge, in pixels, at which the pointer triggers an
  /// edge snap. `[move] snap_threshold`; default 10.
  pub snap_threshold: u32,
  /// Distance from an output corner, in pixels, at which an edge snap becomes a
  /// quarter snap. The pointer must still be within `snap_threshold` of an
  /// edge. `[move] quarter_snap_threshold`; default 50.
  pub quarter_snap_threshold: u32,
  /// Whether dragging a snapped window un-snaps it once it has moved
  /// `snap_off_threshold` pixels. `[move] enable_snap_off`; default true.
  pub enable_snap_off: bool,
  /// Pointer travel, in pixels, before a snapped window follows the drag. Only
  /// used when `enable_snap_off` is set. `[move] snap_off_threshold`; default
  /// 10.
  pub snap_off_threshold: u32,
  /// Milliseconds the pointer must rest on an output edge during a move before
  /// the workspace switches in that direction; negative disables it. `[move]
  /// workspace_switch_after`; default -1.
  pub workspace_switch_after: i64,
  /// Whether dialogs move together with their parent instead of independently.
  /// `[move] join_views`; default false.
  pub join_views: bool,
}

impl Default for GridConfig {
  fn default() -> Self {
    Self {
      enable_snap: true,
      snap_threshold: 10,
      quarter_snap_threshold: 50,
      enable_snap_off: true,
      snap_off_threshold: 10,
      workspace_switch_after: -1,
      join_views: false,
    }
  }
}

/// Touch gestures from Wayfire's `extra-gestures` plugin
/// (metadata/extra-gestures.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtraGesturesConfig {
  /// Fingers to press and hold on a window to start an interactive move. 1–20;
  /// default 3.
  pub move_fingers: u32,
  /// How long, in milliseconds, the fingers must be held before the move
  /// starts. 1–10000; default 750.
  pub move_delay: u64,
  /// Fingers to tap with to close a window. 1–20; the default of 20 effectively
  /// disables the gesture, as in Wayfire.
  pub close_fingers: u32,
}

impl Default for ExtraGesturesConfig {
  fn default() -> Self {
    Self {
      move_fingers: 3,
      move_delay: 750,
      close_fingers: 20,
    }
  }
}

/// Initial position of new floating windows (`[place]`).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlacementConfig {
  /// `center`, `cascade`, `maximize`, `random`, `pointer` (centred on the
  /// cursor, clamped to the workarea) or `none` (at the top left of the
  /// workarea, as Wayfire places views without its `place` plugin).
  /// Default `center`.
  pub mode: String,
}

impl Default for PlacementConfig {
  fn default() -> Self {
    Self {
      mode: "center".into(),
    }
  }
}

/// Click-to-focus, from Wayfire's `[core]` (metadata/core.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FocusConfig {
  /// Buttons that focus the view under the pointer when clicked. `[core]
  /// focus_buttons`; default left, middle and right.
  pub buttons: Vec<Binding>,
  /// Whether a click focuses even while keyboard modifiers are held; when
  /// false, click-to-focus only works with no modifiers pressed. `[core]
  /// focus_button_with_modifiers`; default false.
  pub button_with_modifiers: bool,
  /// Whether the click that changes focus is also delivered to the client.
  /// `[core] focus_buttons_passthrough`; default true.
  pub buttons_passthrough: bool,
}

impl Default for FocusConfig {
  fn default() -> Self {
    Self {
      buttons: ["btn_left", "btn_middle", "btn_right"]
        .into_iter()
        .map(|button| Binding::new(button, Vec::new()))
        .collect(),
      button_with_modifiers: false,
      buttons_passthrough: true,
    }
  }
}

/// Policy for `xdg-activation-v1` focus requests (metadata/xdg-activation.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivationConfig {
  /// Reject tokens created without a source surface. Without this any client
  /// can take focus at any time. `[xdg-activation] check_surface`; default
  /// false.
  pub check_surface: bool,
  /// Reject a token when a newer one has been created since. `[xdg-activation]
  /// only_last_request`; default false.
  pub only_last_request: bool,
  /// Seconds after creation after which a token is no longer honoured.
  /// `[xdg-activation] timeout`; default 30.
  pub timeout_seconds: u64,
}

impl Default for ActivationConfig {
  fn default() -> Self {
    Self {
      check_surface: false,
      only_last_request: false,
      timeout_seconds: 30,
    }
  }
}

/// `keyboard-shortcuts-inhibit-v1` policy (metadata/shortcuts-inhibit.xml). The
/// break-grab key is [`Keybindings::break_grab`].
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShortcutConfig {
  /// Views that inhibit compositor shortcuts while focused even though they
  /// never asked through the protocol. `[shortcuts-inhibit]
  /// inhibit_by_default`; default `none`.
  pub inhibit_by_default: ViewMatcher,
  /// Views whose inhibit requests are refused. `[shortcuts-inhibit]
  /// ignore_views`; default `none`.
  pub ignore_views: ViewMatcher,
}

impl Default for ShortcutConfig {
  fn default() -> Self {
    Self {
      inhibit_by_default: "none"
        .to_owned()
        .try_into()
        .expect("none is a valid matcher"),
      ignore_views: "none"
        .to_owned()
        .try_into()
        .expect("none is a valid matcher"),
    }
  }
}

/// Core switches and workarounds, mostly from Wayfire's `[workarounds]`
/// (metadata/workarounds.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BehaviorConfig {
  /// Reload the config file when it changes on disk, through [`ConfigWatcher`].
  /// `[workarounds] auto_reload_config`; default true.
  pub auto_reload: bool,
  /// Milliseconds a changed config file must stay unchanged before it is
  /// reloaded, so an editor's multi-step save is read once. `[workarounds]
  /// config_reload_delay`; default 20.
  pub config_reload_delay: u64,
  /// Treat every dialog as modal: its parent cannot be focused while it is
  /// open. When false the parent can take focus, though the dialog stays above
  /// it. `[workarounds] all_dialogs_modal`; default true.
  pub all_dialogs_modal: bool,
  /// Send the stdout and stderr of spawned commands to `/dev/null` instead of
  /// the compositor's log. `[workarounds] discard_command_output`; default
  /// true.
  pub discard_command_output: bool,
  /// When a layer-shell surface takes keyboard focus, keep the last focused
  /// toplevel in the activated state. `[workarounds]
  /// keep_last_toplevel_activated`; default true.
  pub keep_last_toplevel_activated: bool,
  /// Largest accepted shared-memory buffer width or height in pixels.
  pub max_buffer_size: u32,
  /// Preferred XDG decoration mode: `client` or `server`.
  pub preferred_decoration_mode: String,
  /// Whether clients must use the preferred decoration mode.
  pub force_preferred_decoration_mode: bool,
  /// Focus a popup's main surface for clients with popup-focus bugs.
  pub focus_main_surface_instead_of_popup: bool,
  /// Do not advertise primary selection; changes require a restart.
  pub disable_primary_selection: bool,
  /// Draw windows on every output they overlap instead of clipping them to
  /// their own output. `[workarounds] remove_output_limits`; default false.
  pub remove_output_limits: bool,
}

impl Default for BehaviorConfig {
  fn default() -> Self {
    Self {
      auto_reload: true,
      config_reload_delay: 20,
      all_dialogs_modal: true,
      discard_command_output: true,
      keep_last_toplevel_activated: true,
      max_buffer_size: 16384,
      preferred_decoration_mode: "client".into(),
      force_preferred_decoration_mode: false,
      focus_main_surface_instead_of_popup: false,
      disable_primary_selection: false,
      remove_output_limits: false,
    }
  }
}

/// When Shingle starts its Xwayland server, Wayfire's `[core] xwayland`.
///
/// In YAML it is written as Wayfire's values: `true`, `false` or `lazy`.
/// The mode is read once at startup; a reloaded config with another mode
/// takes effect only after a restart, as in Wayfire.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum XwaylandMode {
  /// `true`: start Xwayland with the compositor, before any client, and
  /// again whenever it exits after running for more than 5 seconds.
  Eager,
  /// `lazy`: bind the display at startup and start Xwayland on the first
  /// X11 connection. Shingle's default, so that sessions without X11
  /// clients run no X server. Wayfire's default is `true`, which the
  /// Wayfire importer keeps.
  #[default]
  Lazy,
  /// `false`: no X display, and `DISPLAY` is removed from every client's
  /// environment.
  Disabled,
}

impl XwaylandMode {
  /// The mode's Wayfire name: `true`, `lazy` or `false`.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Eager => "true",
      Self::Lazy => "lazy",
      Self::Disabled => "false",
    }
  }
}

impl std::fmt::Display for XwaylandMode {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

impl std::str::FromStr for XwaylandMode {
  type Err = ConfigError;

  /// Parses `true`, `lazy` or `false`, ignoring case and surrounding
  /// space.
  fn from_str(text: &str) -> Result<Self, Self::Err> {
    match text.trim().to_lowercase().as_str() {
      "true" => Ok(Self::Eager),
      "lazy" => Ok(Self::Lazy),
      "false" => Ok(Self::Disabled),
      _ => Err(ConfigError::Invalid(format!(
        "xwayland.mode must be true, false or lazy, not {text:?}"
      ))),
    }
  }
}

impl serde::Serialize for XwaylandMode {
  fn serialize<S: serde::Serializer>(
    &self,
    serializer: S,
  ) -> Result<S::Ok, S::Error> {
    match self {
      Self::Eager => serializer.serialize_bool(true),
      Self::Disabled => serializer.serialize_bool(false),
      Self::Lazy => serializer.serialize_str("lazy"),
    }
  }
}

impl<'de> Deserialize<'de> for XwaylandMode {
  fn deserialize<D: serde::Deserializer<'de>>(
    deserializer: D,
  ) -> Result<Self, D::Error> {
    /// A YAML boolean or string.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
      Bool(bool),
      Text(String),
    }
    match Raw::deserialize(deserializer)? {
      Raw::Bool(true) => Ok(Self::Eager),
      Raw::Bool(false) => Ok(Self::Disabled),
      Raw::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
  }
}

/// XWayland options, from Wayfire's `[core]`.
#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct XwaylandConfig {
  /// When Xwayland starts. `[core] xwayland`; default `lazy` (Wayfire's is
  /// `true`, and a Wayfire import without the option gets `true`).
  pub mode: XwaylandMode,
  /// A shell command run in the client environment each time Xwayland
  /// becomes ready, for example `xrdb -merge ~/.Xresources`. Empty runs
  /// nothing. `[core] xwayland_startup_script`; default empty.
  pub startup_script: String,
}

/// Controls protocol visibility for clients created by a sandbox engine.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityContextConfig {
  /// Comma-separated interface names withheld from sandboxed clients.
  /// `[security-context-v1] privileged_protocols`; the default lists
  /// foreign-toplevel management, screencopy, data control and shortcuts
  /// inhibit, matching Wayfire.
  pub privileged_protocols: String,
}

impl Default for SecurityContextConfig {
  fn default() -> Self {
    Self {
      privileged_protocols: concat!(
        "zwlr_foreign_toplevel_manager_v1,",
        "zwlr_screencopy_manager_v1,",
        "zwlr_data_control_manager_v1,",
        "zwp_keyboard_shortcuts_inhibit_manager_v1",
      )
      .into(),
    }
  }
}

/// Output magnification while a modifier is held and the wheel turns
/// (metadata/zoom.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZoomConfig {
  /// Modifiers that must be held, exactly, for scrolling to zoom: names such as
  /// `super`, `ctrl`, `alt` and `shift`, separated by spaces or `+`, with or
  /// without Wayfire's angle brackets. `[zoom] modifier`; default `super`.
  pub modifier: String,
  /// Zoom change per scroll unit, relative to the current level. Finite and
  /// non-negative; `[zoom] speed`; default 0.01.
  pub speed: f64,
  /// Duration of the eased approach to the target zoom level; 0 jumps
  /// immediately. `[zoom] smoothing_duration`; default 300.
  pub smoothing_ms: u64,
  /// Texture filter while zoomed: `linear` or `nearest`. Imported from
  /// Wayfire's numeric `[zoom] interpolation_method`, where 0 is linear and 1
  /// nearest. Default `linear`.
  pub interpolation: String,
}

impl Default for ZoomConfig {
  fn default() -> Self {
    Self {
      modifier: "super".into(),
      speed: 0.01,
      smoothing_ms: 300,
      interpolation: "linear".into(),
    }
  }
}

/// Window opacity adjusted by wheel while a modifier is held
/// (metadata/alpha.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlphaConfig {
  /// Modifiers that must be held, in the same syntax as
  /// [`ZoomConfig::modifier`]. `[alpha] modifier`; default `super alt`.
  pub modifier: String,
  /// Lowest opacity scrolling can reach, so a window cannot vanish. 0–1;
  /// `[alpha] min_value`; default 0.1.
  pub min_value: f32,
}

impl Default for AlphaConfig {
  fn default() -> Self {
    Self {
      modifier: "super alt".into(),
      min_value: 0.1,
    }
  }
}

/// The fast switcher, which cycles focus without an overview
/// (metadata/fast-switcher.xml). Its keys are
/// [`Keybindings::fast_switcher_next`] and `fast_switcher_previous`.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FastSwitcherConfig {
  /// Opacity of the windows not currently selected while switching. 0–1;
  /// `[fast-switcher] inactive_alpha`; default 0.7.
  pub inactive_alpha: f32,
}

impl Default for FastSwitcherConfig {
  fn default() -> Self {
    Self {
      inactive_alpha: 0.7,
    }
  }
}

/// Controls the interactive window overview known as Scale in Wayfire.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScaleConfig {
  /// Duration of thumbnail entry and layout transitions, in milliseconds.
  pub duration_ms: u64,
  /// Horizontal and vertical space between overview cells, in logical pixels.
  pub spacing: u32,
  /// Space reserved between the output edges and the overview grid.
  pub outer_margin: u32,
  /// Opacity multiplier for unselected, non-minimized windows.
  pub inactive_alpha: f32,
  /// Opacity multiplier for unselected minimized windows.
  pub minimized_alpha: f32,
  /// Whether thumbnails may grow beyond their original dimensions.
  pub allow_zoom: bool,
  /// Whether middle-clicking a thumbnail asks its application to close.
  pub middle_click_close: bool,
  /// Whether minimized windows are shown using their retained buffers.
  pub include_minimized: bool,
  /// Whether mapping a new window commits the selection and exits Scale.
  pub close_on_new_view: bool,
  /// Which thumbnails display titles: `never`, `mouse`, or `all`.
  pub title_overlay: String,
  /// Title font size in logical pixels.
  pub title_font_size: u32,
  /// Title placement within a thumbnail: `top`, `center`, or `bottom`.
  pub title_position: String,
  /// Title background color; rasterization produces pre-multiplied RGBA.
  pub title_bg_color: [f64; 4],
  /// Title text color.
  pub title_text_color: [f64; 4],
}

impl Default for ScaleConfig {
  fn default() -> Self {
    Self {
      duration_ms: 750,
      spacing: 50,
      outer_margin: 0,
      inactive_alpha: 0.75,
      minimized_alpha: 0.45,
      allow_zoom: false,
      middle_click_close: false,
      include_minimized: false,
      close_on_new_view: false,
      title_overlay: "all".into(),
      title_font_size: 16,
      title_position: "center".into(),
      title_bg_color: [0.1, 0.1, 0.1, 0.9],
      title_text_color: [0.8, 0.8, 0.8, 1.0],
    }
  }
}

/// Controls the workspace overview known as Expo in Wayfire.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExpoConfig {
  /// Output and workspace-cell background color.
  pub background: [f64; 4],
  /// Duration of workspace-wall entry and exit zooms, in milliseconds.
  pub duration_ms: u64,
  /// Gap around and between workspace cells, in logical pixels.
  pub offset: u32,
  /// Whether unmodified arrow and keypad keys move the selection.
  pub keyboard_interaction: bool,
  /// RGB multiplier for thumbnails on inactive workspaces.
  pub inactive_brightness: f32,
  /// Duration of selection brightness crossfades, in milliseconds.
  pub transition_length_ms: u64,
  /// Direct workspace selection shortcuts; workspace numbers start at one.
  pub workspace_bindings: Vec<ExpoWorkspaceBinding>,
}

/// A binding that, while Expo is open, selects a workspace and exits Expo. From
/// `[expo] select_workspace_N` or i3 `bindsym KEY expo select N`.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpoWorkspaceBinding {
  /// One-based workspace number, counted row by row across the grid. Must lie
  /// in `1..=width*height` of [`WorkspaceConfig`].
  pub workspace: usize,
  /// Key that selects the workspace.
  pub binding: Binding,
}

impl Default for ExpoConfig {
  fn default() -> Self {
    Self {
      background: [0.1, 0.1, 0.1, 1.0],
      duration_ms: 300,
      offset: 10,
      keyboard_interaction: true,
      inactive_brightness: 0.7,
      transition_length_ms: 200,
      workspace_bindings: Vec::new(),
    }
  }
}

/// Controls typing-to-filter and the filter overlay in Scale.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScaleTitleFilterConfig {
  /// Whether title and app-ID substring matching preserves letter case.
  pub case_sensitive: bool,
  /// Whether all outputs share one filter text, Wayfire's
  /// `scale-title-filter/share_filter` (plugins/scale/scale-title-filter.cpp,
  /// `get_active_filter`, which picks the global or the per-output filter).
  /// Shingle has one output, where both are the same, so it has no effect
  /// until multi-output support exists.
  pub share_filter: bool,
  /// Whether the current filter is displayed above the overview.
  pub overlay: bool,
  /// Filter overlay font size in logical pixels.
  pub font_size: u32,
  /// Filter overlay background color.
  pub bg_color: [f64; 4],
  /// Filter overlay text color.
  pub text_color: [f64; 4],
}

impl Default for ScaleTitleFilterConfig {
  fn default() -> Self {
    Self {
      case_sensitive: false,
      share_filter: false,
      overlay: true,
      font_size: 30,
      bg_color: [0.0, 0.0, 0.0, 0.5],
      text_color: [0.8, 0.8, 0.8, 0.8],
    }
  }
}

/// Window open, close and minimize animations (metadata/animate.xml). Only
/// `fade` and `zoom` are implemented; Wayfire's `fire`, `squeezimize`, `zap`
/// and `spin` are not.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnimateConfig {
  /// Views that are animated. `[animate] enabled_for`; default `type equals
  /// "toplevel"`. Shingle has no X11 views, so Wayfire's `x-or` clause is not
  /// needed.
  pub enabled_for: ViewMatcher,
  /// `none`, `fade` or `zoom`. `[animate] open_animation`; default `zoom`.
  pub open_animation: String,
  /// `none`, `fade` or `zoom`. `[animate] close_animation`; default `zoom`.
  pub close_animation: String,
  /// `none`, `fade` or `zoom`. `[animate] minimize_animation`; default `zoom`
  /// (Wayfire's default, `squeezimize`, is not implemented).
  pub minimize_animation: String,
  /// Length of every animation. `[animate] duration` (an easing suffix such as
  /// `400ms linear` is ignored, and `0.4s` is accepted); default 400.
  pub duration_ms: u64,
}

impl Default for AnimateConfig {
  fn default() -> Self {
    Self {
      enabled_for: "type equals \"toplevel\""
        .to_owned()
        .try_into()
        .expect("the default animation matcher is valid"),
      open_animation: "zoom".into(),
      close_animation: "zoom".into(),
      minimize_animation: "zoom".into(),
      duration_ms: 400,
    }
  }
}

/// Options of the colour-inversion and window-rotation effects.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsConfig {
  /// Invert only black and white, preserving hue, instead of inverting every
  /// channel. `[invert] preserve_hue`; default false.
  pub invert_preserve_hue: bool,
  /// Radius, in logical pixels, around a window's centre: while rotating,
  /// moving the pointer inside it resets the window's rotation to zero. Finite
  /// and positive; `[wrot] reset_radius`; default 25.
  pub wrot_reset_radius: f64,
}

impl Default for EffectsConfig {
  fn default() -> Self {
    Self {
      invert_preserve_hue: false,
      wrot_reset_radius: 25.0,
    }
  }
}

/// The workspace grid.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
  /// Columns of workspaces. `[core] vwidth`; must be nonzero; default 10
  /// (Wayfire's default is 3).
  pub width: u32,
  /// Rows of workspaces. `[core] vheight`; must be nonzero; default 1
  /// (Wayfire's default is 3).
  pub height: u32,
  /// Directional switching at the grid edge: `false` stops; `true` wraps within
  /// the same row or column, as Wayfire's vswitch does; `on_end` treats the
  /// grid as one row-major list, so moving right from the end of a row
  /// continues on the next row and past the last workspace to the first, while
  /// vertical moves do not wrap. `[vswitch] wraparound`; default `false`.
  pub wraparound: String,
}

impl Default for WorkspaceConfig {
  fn default() -> Self {
    Self {
      width: 10,
      height: 1,
      wraparound: "false".into(),
    }
  }
}

/// Controls Wayfire-style sliding transitions between workspaces.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VswitchConfig {
  /// Duration of the workspace-wall movement, in milliseconds.
  pub duration_ms: u64,
  /// Visual gap between workspaces during the transition.
  pub gap: u32,
  /// Color visible in the inter-workspace gap.
  pub background: [f64; 4],
  /// Bindings that switch to a workspace. `[vswitch] binding_N` (Wayfire's
  /// `workspace_bindings` list); default empty.
  pub workspace_bindings: Vec<WorkspaceBinding>,
  /// Bindings that carry the focused window to a workspace and follow it.
  /// `[vswitch] with_win_N` (`workspace_bindings_win`); default empty.
  pub workspace_bindings_win: Vec<WorkspaceBinding>,
  /// Bindings that send the focused window to a workspace without
  /// switching. `[vswitch] send_win_N` (`bindings_win`); default empty.
  pub bindings_win: Vec<WorkspaceBinding>,
}

impl VswitchConfig {
  /// Every entry of the three workspace binding lists.
  pub fn all_bindings(&self) -> impl Iterator<Item = &WorkspaceBinding> {
    self
      .workspace_bindings
      .iter()
      .chain(&self.workspace_bindings_win)
      .chain(&self.bindings_win)
  }
}

impl Default for VswitchConfig {
  fn default() -> Self {
    Self {
      duration_ms: 300,
      gap: 20,
      background: [0.1, 0.1, 0.1, 1.0],
      workspace_bindings: Vec::new(),
      workspace_bindings_win: Vec::new(),
      bindings_win: Vec::new(),
    }
  }
}

/// Touchpad workspace swipes from Wayfire's `vswipe` plugin
/// (metadata/vswipe.xml). The defaults are Wayfire's.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VswipeConfig {
  /// Swipe between columns. `[vswipe] enable_horizontal`; default true.
  pub enable_horizontal: bool,
  /// Swipe between rows. `[vswipe] enable_vertical`; default true.
  pub enable_vertical: bool,
  /// Swipe diagonally and across more than one workspace at a time.
  /// `[vswipe] enable_free_movement`; default false.
  pub enable_free_movement: bool,
  /// Ease the wall towards each new swipe position instead of jumping
  /// to it. `[vswipe] enable_smooth_transition`; default false.
  pub enable_smooth_transition: bool,
  /// Milliseconds of the smooth transition and of the slide to the
  /// target workspace after the fingers lift. `[vswipe] duration`;
  /// default 180.
  pub duration_ms: u64,
  /// Colour between and around the workspaces of the wall. `[vswipe]
  /// background`; components 0–1; default `[0.1, 0.1, 0.1, 1.0]`.
  pub background: [f64; 4],
  /// Fingers of the swipes that move the wall; others go to clients.
  /// `[vswipe] fingers`; 1–20; default 4.
  pub fingers: u32,
  /// Pixels between neighbouring workspaces of the wall. `[vswipe] gap`;
  /// at least 0; default 32.
  pub gap: f64,
  /// The fraction of a workspace a swipe must pass to move on to the
  /// next one; Wayfire clamps it to 0–1. `[vswipe] threshold`; default
  /// 0.35.
  pub threshold: f64,
  /// The motion in pixels of a swipe's last two updates that moves on
  /// to the next workspace however far the swipe went; Wayfire clamps it
  /// to 0–1000. `[vswipe] delta_threshold`; default 24.
  pub delta_threshold: f64,
  /// Pixels of finger motion per workspace. `[vswipe] speed_factor`;
  /// above 0; default 256.
  pub speed_factor: f64,
  /// The most one update moves the wall, in workspaces. `[vswipe]
  /// speed_cap`; at least 0; default 0.05.
  pub speed_cap: f64,
}

impl Default for VswipeConfig {
  fn default() -> Self {
    Self {
      enable_horizontal: true,
      enable_vertical: true,
      enable_free_movement: false,
      enable_smooth_transition: false,
      duration_ms: 180,
      background: [0.1, 0.1, 0.1, 1.0],
      fingers: 4,
      gap: 32.0,
      threshold: 0.35,
      delta_threshold: 24.0,
      speed_factor: 256.0,
      speed_cap: 0.05,
    }
  }
}

/// A binding for one workspace of the grid, from Wayfire's numbered
/// `[vswitch]` options.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
  /// One-based workspace number, counted row by row across the grid, as
  /// vswitch counts it. Must lie in `1..=width*height` of
  /// [`WorkspaceConfig`].
  pub workspace: usize,
  /// The key or button.
  pub binding: Binding,
}

/// Tiling policy and appearance, mostly from Wayfire's `[simple-tile]`
/// (metadata/simple-tile.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TilingConfig {
  /// Gaps around tiles. `[simple-tile] inner_gap_size` sets the gap between
  /// neighbours; `outer_horiz_gap_size` sets left and right and
  /// `outer_vert_gap_size` top and bottom. i3 `gaps
  /// inner|outer|horizontal|vertical|top|right|bottom|left N` sets the same
  /// values.
  pub gaps: Gaps,
  /// Smallest width or height, in logical pixels, a tile may be resized or
  /// split to. Must be nonzero; default 50. i3 `$minimum_extent`.
  pub minimum_extent: u32,
  /// When focus moves from a fullscreen tile to a neighbour, make the neighbour
  /// fullscreen instead of leaving fullscreen. `[simple-tile]
  /// keep_fullscreen_on_adjacent`; default true.
  pub keep_fullscreen_on_adjacent: bool,
  /// Views that are tiled when they first map; the rest float. `[simple-tile]
  /// tile_by_default`; default `all`.
  pub tile_by_default: ViewMatcher,
  /// Duration of the crossfade when a tile's geometry changes; 0 disables it.
  /// `[simple-tile] animation_duration`; default 0.
  pub animation_duration: u64,
  /// Fill colour of the drop preview drawn while dragging a tile.
  /// `[simple-tile] preview_base_color`.
  pub preview_base_color: [f64; 4],
  /// Border colour of the drop preview. `[simple-tile] preview_base_border`.
  pub preview_border_color: [f64; 4],
  /// Border width of the drop preview, in logical pixels. `[simple-tile]
  /// preview_border_width`; default 3.
  pub preview_border_width: u32,
}

impl Default for TilingConfig {
  fn default() -> Self {
    Self {
      gaps: Gaps::default(),
      minimum_extent: 50,
      keep_fullscreen_on_adjacent: true,
      tile_by_default: ViewMatcher::default(),
      animation_duration: 0,
      preview_base_color: [0.5, 0.5, 1.0, 0.5],
      preview_border_color: [0.25, 0.25, 0.5, 0.8],
      preview_border_width: 3,
    }
  }
}

/// Interactive resize behaviour (metadata/resize.xml).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResizeConfig {
  /// Narrowest width an interactive resize may produce; 0 leaves it to the
  /// client. `[resize] min_width`; default 0.
  pub min_width: u32,
  /// Lowest height an interactive resize may produce; 0 leaves it to the
  /// client. `[resize] min_height`; default 0.
  pub min_height: u32,
  /// Fraction of a window's width and height, measured from each edge, in which
  /// grabbing resizes that edge. On an axis where the grab falls between the
  /// two bands, that axis is not resized. 0–0.5; Shingle-specific; default
  /// 0.33.
  pub corner_threshold: f64,
}

impl Default for ResizeConfig {
  fn default() -> Self {
    Self {
      min_width: 0,
      min_height: 0,
      corner_threshold: 0.33,
    }
  }
}

/// Idle timeouts in whole seconds, converted to the idle state machine's
/// [`shingle_idle::Config`] by [`IdleConfig::as_idle_config`].
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdleConfig {
  /// Inactivity before the screensaver (idle) state. `None` disables it.
  /// Imported from `[idle] screensaver_timeout`, where a negative value means
  /// disabled. Unlike Wayfire (3600), the default is `None`.
  pub idle_seconds: Option<u64>,
  /// Inactivity before outputs are powered off. `None` disables it; `[idle]
  /// dpms_timeout`, negative meaning disabled. Default `None`.
  pub dpms_seconds: Option<u64>,
  /// Inactivity before the session is locked. `None` disables it; `[idle]
  /// lock_timeout` (not a stock Wayfire option), negative meaning disabled.
  /// Default `None`.
  pub lock_seconds: Option<u64>,
  /// Suppress the idle timeouts while a fullscreen window is shown. `[idle]
  /// disable_on_fullscreen`; default true.
  pub disable_on_fullscreen: bool,
  /// Start with the idle timeouts suspended, as if the idle toggle had been
  /// pressed once. `[idle] disable_initially`; default false.
  pub disable_initially: bool,
}

impl Default for IdleConfig {
  fn default() -> Self {
    Self {
      idle_seconds: None,
      dpms_seconds: None,
      lock_seconds: None,
      disable_on_fullscreen: true,
      disable_initially: false,
    }
  }
}

impl IdleConfig {
  /// Converts to the idle crate's configuration. Fields not configurable here,
  /// such as the lock-failure policy, keep their [`shingle_idle::Config`]
  /// defaults.
  pub fn as_idle_config(&self) -> shingle_idle::Config {
    shingle_idle::Config {
      idle_timeout: self.idle_seconds.map(Duration::from_secs),
      dpms_timeout: self.dpms_seconds.map(Duration::from_secs),
      lock_timeout: self.lock_seconds.map(Duration::from_secs),
      disable_on_fullscreen: self.disable_on_fullscreen,
      disable_initially: self.disable_initially,
      ..Default::default()
    }
  }
}

/// Persistence of the tiling session across restarts.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
  /// Restore the saved session at startup, relaunching its applications. This
  /// only controls loading at startup. `[session] restore` (a Shingle section);
  /// default true.
  pub restore: bool,
  /// Session file to use. `None` means the default location chosen by
  /// `shingle-session` (`$XDG_STATE_HOME/shingle/session.json`, falling back to
  /// a pre-rename Tilefire file if only that exists).
  pub path: Option<PathBuf>,
}

impl Default for SessionConfig {
  fn default() -> Self {
    Self {
      restore: true,
      path: None,
    }
  }
}

/// A key binding that runs a shell command.
///
/// Wayfire's `[command]` pairs `binding_NAME` (or `repeatable_binding_NAME`,
/// `release_binding_NAME`, `always_binding_NAME`) with `command_NAME`; a
/// binding without a matching command, or a command without a binding, is
/// dropped.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandBinding {
  /// Key combination that triggers the command.
  pub binding: Binding,
  /// Command line, run through `/bin/sh -c`.
  pub command: String,
  /// Run again on every key-repeat event while the key is held. From
  /// `repeatable_binding_NAME`; default false.
  #[serde(default)]
  pub repeatable: bool,
  /// Run when the key is released instead of when it is pressed. From
  /// `release_binding_NAME`; default false.
  #[serde(default)]
  pub on_release: bool,
  /// Run even while the session is locked. From `always_binding_NAME`; default
  /// false.
  #[serde(default)]
  pub always: bool,
}

impl CommandBinding {
  /// Decides whether a key event should run the command: `pressed` is false for
  /// a release, `repeat` marks key-repeat presses and `locked` is the session
  /// lock state. While locked only `always` bindings run; an `on_release`
  /// binding runs only on release, any other only on press, and repeats only
  /// when `repeatable`.
  pub fn should_run(&self, pressed: bool, repeat: bool, locked: bool) -> bool {
    if locked && !self.always {
      return false;
    }
    if self.on_release {
      !pressed
    } else {
      pressed && (self.repeatable || !repeat)
    }
  }
}

/// Keyboard bindings for built-in actions.
///
/// Each field is one [`Binding`], where [`Binding::disabled`] (written `none`
/// or empty) turns the action off; `Vec` fields accept several alternatives, as
/// Wayfire activators separated by `|` do. Defaults are given per field, with
/// `super` written for the logo key. Directional and last-workspace
/// bindings follow Wayfire's vswitch naming: `move` carries the focused
/// window to the target workspace and follows it, `send` moves the window
/// but stays. The numbered `workspace_move` is the exception: it sends the
/// window and stays, as i3's `move container to workspace N` does.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keybindings {
  /// Focus the tile to the left. `[simple-tile] key_focus_left`; default
  /// `super+h`.
  pub focus_left: Binding,
  /// Focus the tile to the right. `[simple-tile] key_focus_right`; default
  /// `super+l`.
  pub focus_right: Binding,
  /// Focus the tile above. `[simple-tile] key_focus_above`; default `super+k`.
  pub focus_up: Binding,
  /// Focus the tile below. `[simple-tile] key_focus_below`; default `super+j`.
  pub focus_down: Binding,
  /// Start the regular switcher, moving forward. `[switcher] next_view`;
  /// default `alt+tab`.
  pub focus_next: Binding,
  /// Start the regular switcher, moving backward. `[switcher] prev_view`;
  /// default `alt+shift+tab`.
  pub focus_previous: Binding,
  /// Toggle fullscreen on the focused window. `[wm-actions] toggle_fullscreen`;
  /// default `super+f`.
  pub toggle_fullscreen: Binding,
  /// Toggle whether the focused window shows on every workspace. `[wm-actions]
  /// toggle_sticky`; default `super+s`.
  pub toggle_sticky: Binding,
  /// Toggle Scale for the current workspace. `[scale] toggle`; default
  /// `super+p`.
  pub scale_toggle: Binding,
  /// Toggle Scale for all workspaces. `[scale] toggle_all`; disabled by
  /// default.
  pub scale_toggle_all: Binding,
  /// Toggle Expo. `[expo] toggle`; default `super+e`.
  pub expo_toggle: Binding,
  /// Start the fast switcher, moving forward. `[fast-switcher] activate`;
  /// default `alt+escape`.
  pub fast_switcher_next: Binding,
  /// Start the fast switcher, moving backward. `[fast-switcher]
  /// activate_backward`; default `alt+shift+escape`.
  pub fast_switcher_previous: Binding,
  /// Toggle colour inversion of the output. `[invert] toggle`; default
  /// `super+i`.
  pub invert_toggle: Binding,
  /// Reset the rotation of every window. `[wrot] reset`; default
  /// `super+ctrl+r`.
  pub wrot_reset: Binding,
  /// Reset the focused window's rotation. `[wrot] reset-one`; default
  /// `super+r`.
  pub wrot_reset_one: Binding,
  /// Toggle always-on-top for the focused window. `[wm-actions]
  /// toggle_always_on_top`; disabled by default.
  pub always_on_top: Binding,
  /// Toggle always-on-bottom for the focused window. `[wm-actions]
  /// toggle_always_on_bottom`; disabled by default.
  pub always_on_bottom: Binding,
  /// Minimize the focused window. `[wm-actions] minimize`; default `super+n`.
  pub minimize: Binding,
  /// Raise the focused window to the top of its stacking layer. `[wm-actions]
  /// bring_to_front`; default `super+b`.
  pub bring_to_front: Binding,
  /// Lower the focused window to the bottom of its stacking layer.
  /// `[wm-actions] send_to_back`; default `super+shift+b`.
  pub send_to_back: Binding,
  /// Toggle show-desktop, minimizing every window and restoring them on the
  /// next press. `[wm-actions] toggle_showdesktop`; default `super+d`.
  pub show_desktop: Binding,
  /// Toggle maximize on the focused window. `[wm-actions] toggle_maximize` or
  /// `[simple-tile] key_toggle_maximized`; default `super+m`.
  pub toggle_maximized: Binding,
  /// Toggle the focused window between tiled and floating. `[simple-tile]
  /// key_toggle`; i3 `floating toggle`; default `super+t`.
  pub toggle_tiling: Binding,
  /// Swap the focused tile with its left neighbour. `[simple-tile]
  /// key_move_left`; default `super+shift+h`.
  pub move_left: Binding,
  /// Swap the focused tile with its right neighbour. `[simple-tile]
  /// key_move_right`; default `super+shift+l`.
  pub move_right: Binding,
  /// Swap the focused tile with the one above. `[simple-tile] key_move_above`;
  /// default `super+shift+k`.
  pub move_up: Binding,
  /// Swap the focused tile with the one below. `[simple-tile] key_move_below`;
  /// default `super+shift+j`.
  pub move_down: Binding,
  /// Move the focused tile's split edge left. `[simple-tile] key_resize_left`;
  /// default `super+ctrl+h`.
  pub resize_left: Binding,
  /// Move the focused tile's split edge right. `[simple-tile]
  /// key_resize_right`; default `super+ctrl+l`.
  pub resize_right: Binding,
  /// Move the focused tile's split edge up. `[simple-tile] key_resize_above`;
  /// default `super+ctrl+k`.
  pub resize_up: Binding,
  /// Move the focused tile's split edge down. `[simple-tile] key_resize_below`;
  /// default `super+ctrl+j`.
  pub resize_down: Binding,
  /// Snap the focused window to the top-left quarter. `[grid] slot_tl`; default
  /// `super+kp7`.
  pub grid_slot_top_left: Vec<Binding>,
  /// Snap the focused window to the top half. `[grid] slot_t`; default
  /// `super+kp8`.
  pub grid_slot_top: Vec<Binding>,
  /// Snap the focused window to the top-right quarter. `[grid] slot_tr`;
  /// default `super+kp9`.
  pub grid_slot_top_right: Vec<Binding>,
  /// Snap the focused window to the left half. `[grid] slot_l`; default
  /// `super+left` and `super+kp4`.
  pub grid_slot_left: Vec<Binding>,
  /// Snap the focused window to the whole workarea. `[grid] slot_c`; default
  /// `super+up` and `super+kp5`.
  pub grid_slot_center: Vec<Binding>,
  /// Snap the focused window to the right half. `[grid] slot_r`; default
  /// `super+right` and `super+kp6`.
  pub grid_slot_right: Vec<Binding>,
  /// Snap the focused window to the bottom-left quarter. `[grid] slot_bl`;
  /// default `super+kp1`.
  pub grid_slot_bottom_left: Vec<Binding>,
  /// Snap the focused window to the bottom half. `[grid] slot_b`; default
  /// `super+kp2`.
  pub grid_slot_bottom: Vec<Binding>,
  /// Snap the focused window to the bottom-right quarter. `[grid] slot_br`;
  /// default `super+kp3`.
  pub grid_slot_bottom_right: Vec<Binding>,
  /// Restore a snapped window's geometry from before it was snapped. `[grid]
  /// restore`; default `super+down` and `super+kp0`.
  pub grid_restore: Vec<Binding>,
  /// Suspend or resume the idle timeouts. `[idle] toggle`; disabled by default.
  pub idle_toggle: Binding,
  /// End a client's keyboard-shortcuts inhibition so compositor bindings work
  /// again. Checked before the inhibitor, so it always reaches the compositor.
  /// `[shortcuts-inhibit] break_grab`; disabled by default.
  pub break_grab: Binding,
  /// Lock the session. Imported from `[idle] key_lock` or `[simple-tile]
  /// key_lock` (Shingle names, not stock Wayfire), or i3 `bindsym KEY exec
  /// shingle-lock`; default `super+f8`.
  pub lock: Binding,
  /// Ask the focused window to close. `[core] close_top_view`, whose first
  /// alternative lands here and the rest in `close_alternatives`; default
  /// `super+q`.
  pub close: Binding,
  /// Further bindings that close the focused window. Default `alt+f4`.
  pub close_alternatives: Vec<Binding>,
  /// Reload the configuration file. i3 `reload`; no Wayfire equivalent; default
  /// `super+shift+r`.
  pub reload: Binding,
  /// Quit the compositor, saving the session. `[core] exit`; default
  /// `super+shift+q`.
  pub quit: Binding,
  /// Switch to workspace 1 to 10, in order; exactly ten entries. Defaults
  /// `super+1` … `super+9`, `super+0`. i3 `workspace N`.
  pub workspace_switch: Vec<Binding>,
  /// Send the focused window to workspace 1 to 10 without following it, as
  /// i3 does; exactly ten entries. Defaults `super+shift+1` …
  /// `super+shift+0`. i3 `move container to workspace N`. Wayfire's
  /// following `[vswitch] with_win_N` is `vswitch.workspace_bindings_win`.
  pub workspace_move: Vec<Binding>,
  /// Switch to the workspace to the right. `[vswitch] binding_right`; default
  /// `super+alt+right`.
  pub workspace_next: Binding,
  /// Switch to the workspace to the left. `[vswitch] binding_left`; default
  /// `super+alt+left`.
  pub workspace_previous: Binding,
  /// Switch to the workspace above. `[vswitch] binding_up`; default
  /// `super+alt+up`.
  pub workspace_up: Binding,
  /// Switch to the workspace below. `[vswitch] binding_down`; default
  /// `super+alt+down`.
  pub workspace_down: Binding,
  /// Carry the focused window right and follow it. `[vswitch] with_win_right`;
  /// default `super+alt+shift+right`.
  pub workspace_move_next: Binding,
  /// Carry the focused window left and follow it. `[vswitch] with_win_left`;
  /// default `super+alt+shift+left`.
  pub workspace_move_previous: Binding,
  /// Carry the focused window up and follow it. `[vswitch] with_win_up`;
  /// default `super+alt+shift+up`.
  pub workspace_move_up: Binding,
  /// Carry the focused window down and follow it. `[vswitch] with_win_down`;
  /// default `super+alt+shift+down`.
  pub workspace_move_down: Binding,
  /// Send the focused window right without switching. `[vswitch]
  /// send_win_right`; disabled by default.
  pub workspace_send_next: Binding,
  /// Send the focused window left without switching. `[vswitch] send_win_left`;
  /// disabled by default.
  pub workspace_send_previous: Binding,
  /// Send the focused window up without switching. `[vswitch] send_win_up`;
  /// disabled by default.
  pub workspace_send_up: Binding,
  /// Send the focused window down without switching. `[vswitch] send_win_down`;
  /// disabled by default.
  pub workspace_send_down: Binding,
  /// Switch back to the previously active workspace. `[vswitch] binding_last`;
  /// i3 `workspace back_and_forth`; default `super+grave`.
  pub workspace_last: Binding,
  /// Carry the focused window to the previous workspace and follow it.
  /// `[vswitch] with_win_last`; default `super+shift+grave`.
  pub workspace_move_last: Binding,
  /// Send the focused window to the previous workspace without switching.
  /// `[vswitch] send_win_last`; disabled by default.
  pub workspace_send_last: Binding,
  /// Tell shell clients such as wf-panel to open or close their menu
  /// (`zwf_output_v2.toggle_menu`). `[wayfire-shell] toggle_menu`;
  /// default `super`, the logo key pressed and released alone.
  pub toggle_menu: Binding,
}

impl Default for Keybindings {
  fn default() -> Self {
    let workspace_switch = (0..10)
      .map(|index| {
        Binding::new(((index + 1) % 10).to_string(), vec![Modifier::Super])
      })
      .collect();
    let workspace_move = (0..10)
      .map(|index| {
        Binding::new(
          ((index + 1) % 10).to_string(),
          vec![Modifier::Super, Modifier::Shift],
        )
      })
      .collect();
    let grid = |key: &str| Binding::new(key, vec![Modifier::Super]);
    Self {
      focus_left: Binding::new("h", vec![Modifier::Super]),
      focus_right: Binding::new("l", vec![Modifier::Super]),
      focus_up: Binding::new("k", vec![Modifier::Super]),
      focus_down: Binding::new("j", vec![Modifier::Super]),
      focus_next: Binding::new("tab", vec![Modifier::Alt]),
      focus_previous: Binding::new("tab", vec![Modifier::Alt, Modifier::Shift]),
      toggle_fullscreen: Binding::new("f", vec![Modifier::Super]),
      toggle_sticky: Binding::new("s", vec![Modifier::Super]),
      scale_toggle: Binding::new("p", vec![Modifier::Super]),
      scale_toggle_all: Binding::disabled(),
      expo_toggle: Binding::new("e", vec![Modifier::Super]),
      fast_switcher_next: Binding::new("escape", vec![Modifier::Alt]),
      fast_switcher_previous: Binding::new(
        "escape",
        vec![Modifier::Alt, Modifier::Shift],
      ),
      invert_toggle: Binding::new("i", vec![Modifier::Super]),
      wrot_reset: Binding::new("r", vec![Modifier::Super, Modifier::Control]),
      wrot_reset_one: Binding::new("r", vec![Modifier::Super]),
      always_on_top: Binding::disabled(),
      always_on_bottom: Binding::disabled(),
      minimize: Binding::new("n", vec![Modifier::Super]),
      bring_to_front: Binding::new("b", vec![Modifier::Super]),
      send_to_back: Binding::new("b", vec![Modifier::Super, Modifier::Shift]),
      show_desktop: Binding::new("d", vec![Modifier::Super]),
      toggle_maximized: Binding::new("m", vec![Modifier::Super]),
      toggle_tiling: Binding::new("t", vec![Modifier::Super]),
      move_left: Binding::new("h", vec![Modifier::Super, Modifier::Shift]),
      move_right: Binding::new("l", vec![Modifier::Super, Modifier::Shift]),
      move_up: Binding::new("k", vec![Modifier::Super, Modifier::Shift]),
      move_down: Binding::new("j", vec![Modifier::Super, Modifier::Shift]),
      resize_left: Binding::new("h", vec![Modifier::Super, Modifier::Control]),
      resize_right: Binding::new("l", vec![Modifier::Super, Modifier::Control]),
      resize_up: Binding::new("k", vec![Modifier::Super, Modifier::Control]),
      resize_down: Binding::new("j", vec![Modifier::Super, Modifier::Control]),
      grid_slot_top_left: vec![grid("kp7")],
      grid_slot_top: vec![grid("kp8")],
      grid_slot_top_right: vec![grid("kp9")],
      grid_slot_left: vec![grid("left"), grid("kp4")],
      grid_slot_center: vec![grid("up"), grid("kp5")],
      grid_slot_right: vec![grid("right"), grid("kp6")],
      grid_slot_bottom_left: vec![grid("kp1")],
      grid_slot_bottom: vec![grid("kp2")],
      grid_slot_bottom_right: vec![grid("kp3")],
      grid_restore: vec![grid("down"), grid("kp0")],
      idle_toggle: Binding::disabled(),
      break_grab: Binding::disabled(),
      lock: Binding::new("f8", vec![Modifier::Super]),
      close: Binding::new("q", vec![Modifier::Super]),
      close_alternatives: vec![Binding::new("f4", vec![Modifier::Alt])],
      reload: Binding::new("r", vec![Modifier::Super, Modifier::Shift]),
      quit: Binding::new("q", vec![Modifier::Super, Modifier::Shift]),
      workspace_switch,
      workspace_move,
      workspace_next: Binding::new(
        "right",
        vec![Modifier::Super, Modifier::Alt],
      ),
      workspace_previous: Binding::new(
        "left",
        vec![Modifier::Super, Modifier::Alt],
      ),
      workspace_up: Binding::new("up", vec![Modifier::Super, Modifier::Alt]),
      workspace_down: Binding::new(
        "down",
        vec![Modifier::Super, Modifier::Alt],
      ),
      workspace_move_next: Binding::new(
        "right",
        vec![Modifier::Super, Modifier::Alt, Modifier::Shift],
      ),
      workspace_move_previous: Binding::new(
        "left",
        vec![Modifier::Super, Modifier::Alt, Modifier::Shift],
      ),
      workspace_move_up: Binding::new(
        "up",
        vec![Modifier::Super, Modifier::Alt, Modifier::Shift],
      ),
      workspace_move_down: Binding::new(
        "down",
        vec![Modifier::Super, Modifier::Alt, Modifier::Shift],
      ),
      workspace_send_next: Binding::disabled(),
      workspace_send_previous: Binding::disabled(),
      workspace_send_up: Binding::disabled(),
      workspace_send_down: Binding::disabled(),
      workspace_last: Binding::new("grave", vec![Modifier::Super]),
      workspace_move_last: Binding::new(
        "grave",
        vec![Modifier::Super, Modifier::Shift],
      ),
      workspace_send_last: Binding::disabled(),
      toggle_menu: Binding::modifiers_alone(vec![Modifier::Super]),
    }
  }
}

/// Modifier-and-button bindings that start pointer grabs. The key part of each
/// [`Binding`] is a button name such as `btn_left`.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PointerBindings {
  /// Start an interactive move of the window under the pointer. `[move]
  /// activate` or `[simple-tile] button_move`; default `super+btn_left`.
  pub move_button: Binding,
  /// Start an interactive resize of the window under the pointer, from the edge
  /// chosen by [`ResizeConfig::corner_threshold`]. `[resize] activate` or
  /// `[simple-tile] button_resize`; default `super+btn_right`.
  pub resize_button: Binding,
  /// Start a resize that keeps the window's aspect ratio. `[resize]
  /// activate_preserve_aspect`; disabled by default.
  pub resize_preserve_aspect_button: Binding,
  /// Start rotating the window under the pointer in 2D. `[wrot] activate`;
  /// default `super+ctrl+btn_right`.
  pub wrot_activate: Binding,
}

impl Default for PointerBindings {
  fn default() -> Self {
    Self {
      move_button: Binding::new("btn_left", vec![Modifier::Super]),
      resize_button: Binding::new("btn_right", vec![Modifier::Super]),
      resize_preserve_aspect_button: Binding::disabled(),
      wrot_activate: Binding::new(
        "btn_right",
        vec![Modifier::Super, Modifier::Control],
      ),
    }
  }
}

/// Keyboard, pointer and cursor settings from Wayfire's `[input]`
/// (metadata/input.xml). The XKB fields feed [`InputConfig::keymap`].
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
  /// XKB rules name. `[input] xkb_rules`; default `evdev`.
  pub rules: String,
  /// Path of a complete XKB keymap file; when non-empty it replaces the
  /// generated keymap. `[input] xkb_file` (Shingle-specific); default empty.
  pub xkb_file: String,
  /// XKB keyboard model. `[input] xkb_model`; default `pc105`.
  pub model: String,
  /// Comma-separated XKB layouts, the first being group 1. `[input]
  /// xkb_layout`; default `us`.
  pub layout: String,
  /// Comma-separated XKB variants, matched to `layout` by position; an empty
  /// entry means the layout's default variant. `[input] xkb_variant`; default
  /// empty.
  pub variant: String,
  /// Comma-separated XKB options such as `ctrl:nocaps`. `[input] xkb_options`;
  /// default empty.
  pub options: String,
  /// Key repeats per second once `repeat_delay` has passed. `[input]
  /// kb_repeat_rate`; default 40.
  pub repeat_rate: u32,
  /// Milliseconds a key must be held before it starts repeating. `[input]
  /// kb_repeat_delay`; default 400.
  pub repeat_delay: u32,
  /// Num Lock state at startup. `[input] kb_numlock_default_state`; default
  /// false.
  pub numlock_default: bool,
  /// Caps Lock state at startup. `[input] kb_capslock_default_state`; default
  /// false.
  pub capslock_default: bool,
  /// Multiplier for wheel scroll distances. Finite and non-negative; `[input]
  /// mouse_scroll_speed`; default 1.
  pub mouse_scroll_speed: f64,
  /// Invert wheel scrolling direction. `[input] mouse_natural_scroll`; default
  /// false.
  pub mouse_natural_scroll: bool,
  /// Swap the left and right mouse buttons. `[input] left_handed_mode`; default
  /// false.
  pub left_handed_mode: bool,
  /// Emulate a middle click when left and right are pressed together. `[input]
  /// middle_emulation`; default false.
  pub middle_emulation: bool,
  /// libinput acceleration profile for mice and other pointers that are not
  /// touchpads: `default` (the device's default), `none`, `adaptive` or
  /// `flat`. `[input] mouse_accel_profile`; default `default`. Native only.
  pub mouse_accel_profile: String,
  /// libinput pointer speed for mice, from -1 to 1. `[input]
  /// mouse_cursor_speed`; default 0. Native only.
  pub mouse_cursor_speed: f64,
  /// libinput acceleration profile for touchpads; the values of
  /// `mouse_accel_profile`. `[input] touchpad_accel_profile`; default
  /// `default`. Native only.
  pub touchpad_accel_profile: String,
  /// libinput pointer speed for touchpads, from -1 to 1. `[input]
  /// touchpad_cursor_speed`; default 0. Native only.
  pub touchpad_cursor_speed: f64,
  /// Tapping a touchpad clicks. `[input] tap_to_click`; default true. Native
  /// only.
  pub tap_to_click: bool,
  /// How a touchpad click picks its button: `default`, `none`,
  /// `button-areas` or `clickfinger`. `[input] click_method`; default
  /// `default`. Native only.
  pub click_method: String,
  /// How a touchpad scrolls: `default`, `none`, `two-finger`, `edge` or
  /// `on-button-down`. `[input] scroll_method`; default `default`. Native
  /// only.
  pub scroll_method: String,
  /// Natural (inverted) scrolling on touchpads. `[input] natural_scroll`;
  /// default false. Native only.
  pub natural_scroll: bool,
  /// Ignore a touchpad while typing. `[input] disable_touchpad_while_typing`;
  /// default false. Native only.
  pub disable_touchpad_while_typing: bool,
  /// Ignore a touchpad while an external mouse is plugged in. `[input]
  /// disable_touchpad_while_mouse`; default false. Native only.
  pub disable_touchpad_while_mouse: bool,
  /// Tap and drag on touchpads. `[input] tap_and_drag`; default true. Native
  /// only.
  pub tap_and_drag: bool,
  /// Keep a tap-and-drag going briefly after the finger lifts. `[input]
  /// drag_lock`; default false. Native only.
  pub drag_lock: bool,
  /// XCursor theme for the compositor's own cursors and `wp-cursor-shape-v1`
  /// shapes. `[input] cursor_theme`; default `default`.
  pub cursor_theme: String,
  /// XCursor size. 1–256; `[input] cursor_size`; default 24.
  pub cursor_size: u32,
  /// Milliseconds within which a modifier pressed and released alone
  /// triggers a modifier-alone binding such as `toggle_menu`; 0 never
  /// cancels it. `[input] modifier_binding_timeout`; default 400.
  pub modifier_binding_timeout: u32,
  /// Enable the text-input-v3 protocol for editable client surfaces.
  pub enable_text_input_v3: bool,
}

impl Default for InputConfig {
  fn default() -> Self {
    Self {
      rules: "evdev".into(),
      xkb_file: String::new(),
      model: "pc105".into(),
      layout: "us".into(),
      variant: String::new(),
      options: String::new(),
      repeat_rate: 40,
      repeat_delay: 400,
      numlock_default: false,
      capslock_default: false,
      mouse_scroll_speed: 1.0,
      mouse_natural_scroll: false,
      left_handed_mode: false,
      middle_emulation: false,
      mouse_accel_profile: "default".into(),
      mouse_cursor_speed: 0.0,
      touchpad_accel_profile: "default".into(),
      touchpad_cursor_speed: 0.0,
      tap_to_click: true,
      click_method: "default".into(),
      scroll_method: "default".into(),
      natural_scroll: false,
      disable_touchpad_while_typing: false,
      disable_touchpad_while_mouse: false,
      tap_and_drag: true,
      drag_lock: false,
      cursor_theme: "default".into(),
      cursor_size: 24,
      modifier_binding_timeout: 400,
      enable_text_input_v3: true,
    }
  }
}

/// The values of `[input] mouse_accel_profile` and
/// `touchpad_accel_profile`.
pub const ACCEL_PROFILES: [&str; 4] = ["default", "none", "adaptive", "flat"];

/// The values of `[input] click_method`.
pub const CLICK_METHODS: [&str; 4] =
  ["default", "none", "button-areas", "clickfinger"];

/// The values of `[input] scroll_method`.
pub const SCROLL_METHODS: [&str; 5] =
  ["default", "none", "two-finger", "edge", "on-button-down"];

impl InputConfig {
  /// Checks the libinput device options against the values Wayfire's
  /// metadata/input.xml allows.
  fn validate_devices(&self) -> Result<(), ConfigError> {
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
      ("scroll_method", &self.scroll_method, &SCROLL_METHODS[..]),
    ];
    for (name, value, allowed) in choices {
      if !allowed.contains(&value.as_str()) {
        return Err(ConfigError::Invalid(format!(
          "input.{name} must be one of {}, not {value:?}",
          allowed.join(", ")
        )));
      }
    }
    for (name, speed) in [
      ("mouse_cursor_speed", self.mouse_cursor_speed),
      ("touchpad_cursor_speed", self.touchpad_cursor_speed),
    ] {
      if !(-1.0..=1.0).contains(&speed) {
        return Err(ConfigError::Invalid(format!(
          "input.{name} must be between -1 and 1"
        )));
      }
    }
    Ok(())
  }

  /// Returns the XKB keymap text for clients: the contents of `xkb_file` when
  /// it is set, otherwise [`InputConfig::keymap`].
  ///
  /// # Errors
  ///
  /// [`ConfigError::Read`] if `xkb_file` is set but cannot be read; the
  /// file's contents are not compiled or checked here. Otherwise as for
  /// [`InputConfig::keymap`].
  pub fn load_keymap(&self) -> Result<String, ConfigError> {
    if self.xkb_file.is_empty() {
      return self.keymap();
    }
    let path = PathBuf::from(&self.xkb_file);
    fs::read_to_string(&path)
      .map_err(|source| ConfigError::Read { path, source })
  }

  /// Compiles the XKB keymap that `rules`, `model`, `layout`, `variant` and
  /// `options` name, as Wayfire does with `xkb_map_new_from_names` in
  /// `reload_input_options` (src/core/seat/keyboard.cpp), and returns it as
  /// keymap text with no includes. Spaces around the comma-separated
  /// layouts, variants and options are ignored.
  ///
  /// # Errors
  ///
  /// [`ConfigError::Invalid`] if libxkbcommon cannot compile the names, for
  /// example for an unknown layout.
  pub fn keymap(&self) -> Result<String, ConfigError> {
    use xkbcommon::xkb;
    let list = |value: &str| {
      value
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(",")
    };
    let options = list(&self.options);
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
      &context,
      self.rules.trim(),
      self.model.trim(),
      &list(&self.layout),
      &list(&self.variant),
      (!options.is_empty()).then_some(options),
      xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| {
      ConfigError::Invalid(format!(
        "no XKB keymap for rules {:?}, model {:?}, layout {:?}, variant {:?} \
         and options {:?}",
        self.rules, self.model, self.layout, self.variant, self.options
      ))
    })?;
    Ok(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
  }
}

/// A key or button with the exact set of modifiers that must be held, or
/// modifiers pressed and released alone.
///
/// The text form, used in YAML, is modifiers and key joined by `+`, such as
/// `super+shift+q`; modifier names are case-insensitive (`super`, `super_key`,
/// `logo`, `ctrl`, `control`, `alt`, `shift`). Keys are lower-cased Wayfire key
/// names without the `KEY_` prefix (`q`, `enter`, `kp5`, `btn_left`). A key
/// written `xkb:NAME` matches the case-sensitive XKB keysym name instead, so
/// layout-dependent symbols such as `xkb:XF86AudioMute` can be bound. An empty
/// string or `none` is the disabled binding.
///
/// Modifiers without a key, such as `super`, are a modifier-alone binding,
/// Wayfire's `<super>`: it triggers when the modifiers are pressed and
/// released with no other key, button or touch in between, within
/// [`InputConfig::modifier_binding_timeout`]
/// ([`Binding::matches_modifiers_alone`]). Only `keybindings.toggle_menu`
/// and commands act on one; elsewhere it never matches.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Binding {
  /// Lower-case key or button name, `xkb:` followed by a keysym name, or empty
  /// for a disabled binding.
  pub key: String,
  /// Modifiers that must be held. Order does not matter, and no other modifier
  /// may be held.
  pub modifiers: Vec<Modifier>,
}

impl Binding {
  /// Creates a binding from an already normalized key name. The key is not
  /// lower-cased or checked.
  pub fn new(key: impl Into<String>, modifiers: Vec<Modifier>) -> Self {
    Self {
      key: key.into(),
      modifiers,
    }
  }

  /// The binding that never matches, written `none` in YAML.
  pub fn disabled() -> Self {
    Self::new("", Vec::new())
  }

  /// The modifier-alone binding of `modifiers`, such as Wayfire's
  /// `<super>`. Without modifiers it is the disabled binding.
  pub fn modifiers_alone(modifiers: Vec<Modifier>) -> Self {
    Self::new("", modifiers)
  }

  /// Whether this is a modifier-alone binding: modifiers without a key.
  pub fn is_modifiers_alone(&self) -> bool {
    self.key.is_empty() && !self.modifiers.is_empty()
  }

  /// Whether a modifier-alone binding triggers for `modifiers`, the
  /// modifiers held when the last of them was released, that one
  /// included. Key bindings never match here.
  pub fn matches_modifiers_alone(&self, modifiers: Modifiers) -> bool {
    self.is_modifiers_alone() && self.modifiers_match(modifiers)
  }

  /// Whether `key`, a Wayfire key name, pressed with exactly `modifiers`
  /// triggers this binding. A disabled binding never matches, and an `xkb:`
  /// binding only matches through [`Binding::matches_xkb`].
  pub fn matches(&self, modifiers: Modifiers, key: &str) -> bool {
    !self.key.is_empty() && self.key == key && self.modifiers_match(modifiers)
  }

  /// Matches the case-sensitive name produced by an XKB keysym.
  pub fn matches_xkb(&self, modifiers: Modifiers, symbol: &str) -> bool {
    self
      .key
      .strip_prefix("xkb:")
      .is_some_and(|expected| !expected.is_empty() && expected == symbol)
      && self.modifiers_match(modifiers)
  }

  fn modifiers_match(&self, modifiers: Modifiers) -> bool {
    let modifiers = modifiers.as_slice();
    self.modifiers.len() == modifiers.len()
      && self
        .modifiers
        .iter()
        .all(|modifier| modifiers.contains(modifier))
  }
}

impl From<Binding> for String {
  fn from(binding: Binding) -> Self {
    let mut parts = binding
      .modifiers
      .iter()
      .map(|modifier| match modifier {
        Modifier::Super => "super",
        Modifier::Control => "ctrl",
        Modifier::Alt => "alt",
        Modifier::Shift => "shift",
      })
      .collect::<Vec<_>>();
    if !binding.key.is_empty() || parts.is_empty() {
      parts.push(&binding.key);
    }
    parts.join("+")
  }
}

impl TryFrom<String> for Binding {
  type Error = ConfigError;

  fn try_from(value: String) -> Result<Self, Self::Error> {
    if value.trim().is_empty() || value.trim() == "none" {
      return Ok(Self::disabled());
    }
    if let Some(modifiers) = value
      .split('+')
      .map(modifier_named)
      .collect::<Option<Vec<_>>>()
    {
      return Ok(Self::modifiers_alone(modifiers));
    }
    let mut parts = value.split('+');
    let key = parts
      .next_back()
      .filter(|key| !key.is_empty())
      .ok_or_else(|| ConfigError::Binding(value.clone()))?;
    let key = if key
      .get(..4)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case("xkb:"))
    {
      if key.len() == 4 {
        return Err(ConfigError::Binding(value.clone()));
      }
      format!("xkb:{}", &key[4..])
    } else {
      key.to_lowercase()
    };
    let modifiers = parts
      .map(|part| {
        modifier_named(part).ok_or_else(|| ConfigError::Binding(value.clone()))
      })
      .collect::<Result<Vec<_>, _>>()?;
    Ok(Self { key, modifiers })
  }
}

/// The modifier a binding's text names, case-insensitively.
fn modifier_named(name: &str) -> Option<Modifier> {
  Some(match name.to_lowercase().as_str() {
    "super" | "super_key" | "logo" => Modifier::Super,
    "ctrl" | "control" => Modifier::Control,
    "alt" => Modifier::Alt,
    "shift" => Modifier::Shift,
    _ => return None,
  })
}

/// A modifier a [`Binding`] can require. There is no distinction between left
/// and right keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Modifier {
  /// The logo (Windows) key, XKB `Mod4`.
  Super,
  /// Either Control key.
  Control,
  /// Either Alt key, XKB `Mod1`.
  Alt,
  /// Either Shift key.
  Shift,
}

/// The modifiers currently held, as reported by the keyboard state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
  /// The logo key is held.
  pub super_key: bool,
  /// A Control key is held.
  pub control: bool,
  /// An Alt key is held.
  pub alt: bool,
  /// A Shift key is held.
  pub shift: bool,
}

impl Modifiers {
  /// The held modifiers as a list, in the order control, alt, shift, super.
  /// Despite the name this allocates a `Vec`.
  pub fn as_slice(self) -> Vec<Modifier> {
    let mut modifiers = Vec::new();
    if self.control {
      modifiers.push(Modifier::Control);
    }
    if self.alt {
      modifiers.push(Modifier::Alt);
    }
    if self.shift {
      modifiers.push(Modifier::Shift);
    }
    if self.super_key {
      modifiers.push(Modifier::Super);
    }
    modifiers
  }
}

/// Something in a Wayfire or i3 file that Shingle read past: a value it
/// cannot use, which kept its default, a part of a value it does not
/// support, or a plugin section it does not have. Its `Display` form is
/// `line N: MESSAGE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigWarning {
  /// The 1-based line the warning is about, or `None` for the whole file.
  /// A value continued over several lines reports its first line.
  pub line: Option<usize>,
  /// What was left out and why, naming the section and option.
  pub message: String,
}

impl std::fmt::Display for ConfigWarning {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self.line {
      Some(line) => write!(formatter, "line {line}: {}", self.message),
      None => formatter.write_str(&self.message),
    }
  }
}

/// Why a configuration could not be loaded, parsed, validated or saved.
#[derive(Debug, Error)]
pub enum ConfigError {
  /// The file could not be read.
  #[error("failed to read config {path}: {source}")]
  Read {
    /// File that was being read.
    path: PathBuf,
    /// Underlying I/O error.
    source: io::Error,
  },
  /// The file or its temporary copy could not be written, synced or renamed.
  #[error("failed to write config {path}: {source}")]
  Write {
    /// Destination path; the failure may concern its temporary copy.
    path: PathBuf,
    /// Underlying I/O error.
    source: io::Error,
  },
  /// YAML syntax error, wrong type or unknown field.
  #[error("invalid YAML: {0}")]
  Yaml(#[from] serde_yaml::Error),
  /// A value is malformed or outside its range. The message names the field.
  /// Wayfire and i3 imports report such values as [`ConfigWarning`]s
  /// instead.
  #[error("invalid config: {0}")]
  Invalid(String),
  /// A key binding could not be parsed, for example an unknown modifier or a
  /// missing key. Carries the original binding text.
  #[error("invalid keybinding: {0}")]
  Binding(String),
}

/// Returns the configuration file used when no path is given.
///
/// This is `$XDG_CONFIG_HOME/shingle/shingle.yaml`, falling back to
/// `~/.config`. A configuration left at the pre-rename Tilefire location is
/// used when the Shingle file does not exist yet, so upgrading does not
/// silently discard it.
pub fn default_path() -> PathBuf {
  let config_home = std::env::var_os("XDG_CONFIG_HOME")
    .map(PathBuf::from)
    .or_else(|| {
      std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
    })
    .unwrap_or_else(|| PathBuf::from("."));
  resolve_default_path(&config_home, |path| path.exists())
}

/// Picks the Shingle configuration path under `config_home`, or the legacy
/// Tilefire path when only that one exists.
fn resolve_default_path(
  config_home: &Path,
  exists: impl Fn(&Path) -> bool,
) -> PathBuf {
  let current = config_home.join("shingle/shingle.yaml");
  let legacy = config_home.join("tilefire/tilefire.yaml");
  if !exists(&current) && exists(&legacy) {
    legacy
  } else {
    current
  }
}

/// The warnings of importing `text`, as `line N: MESSAGE`, for tests.
#[cfg(test)]
pub(crate) fn warnings_of(text: &str, format: ConfigFormat) -> Vec<String> {
  ShingleConfig::parse_with_warnings(text, format)
    .expect("the import succeeds")
    .1
    .iter()
    .map(ToString::to_string)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_survey_findings_b1_and_b2_are_fixed() {
    // Each value the survey (docs/design/wayfire-gaps.md, B1 and B2) found
    // refused or misread, in a file of its own.
    let parse = |text: &str| {
      ShingleConfig::parse_with_warnings(text, ConfigFormat::Wayfire).unwrap()
    };
    let (colour, warnings) = parse("[core]\nbackground_color = \\#101010FF\n");
    assert_eq!(colour.background_color[0], 16.0 / 255.0);
    assert!(warnings.is_empty(), "{warnings:?}");
    let (matcher, warnings) = parse(concat!(
      "[animate]\nenabled_for = (type equals \"toplevel\" | ",
      "(type equals \"x-or\" & focusable equals true))\n",
    ));
    assert!(matcher.animate.enabled_for.as_str().contains("x-or"));
    assert!(warnings.is_empty(), "{warnings:?}");
    let (animation, warnings) =
      parse("[animate]\nminimize_animation = squeezimize\n");
    assert_eq!(animation.animate.minimize_animation, "zoom");
    assert_eq!(warnings.len(), 1);
    let (modifier, warnings) =
      parse("[core]\nplugins = scale\n[scale]\ntoggle = <super>\n");
    assert_eq!(modifier.keybindings.scale_toggle, Binding::disabled());
    assert!(warnings[0].message.contains("modifier-alone"));
    let (pinch, warnings) = parse(concat!(
      "[core]\nplugins = scale\n",
      "[scale]\ntoggle = <super> KEY_P | pinch in 4\n"
    ));
    assert_eq!(String::from(pinch.keybindings.scale_toggle), "super+p");
    assert!(warnings[0].message.contains("touch gesture"));
    let (joined, warnings) = parse("[core]\nplugins = \\\n  preserve-output\n");
    assert_eq!(joined.output_layout.unplug, UnplugPolicy::Park);
    assert!(warnings.is_empty(), "{warnings:?}");
  }

  #[test]
  fn modifiers_alone_are_a_binding_of_their_own() {
    let parse = |text: &str| Binding::try_from(text.to_owned()).unwrap();
    let menu = parse("super");
    assert!(menu.is_modifiers_alone());
    assert_eq!(menu, Binding::modifiers_alone(vec![Modifier::Super]));
    assert_eq!(String::from(menu.clone()), "super");
    let both = parse("Ctrl+ALT");
    assert_eq!(String::from(both.clone()), "ctrl+alt");
    assert!(!parse("super+q").is_modifiers_alone());
    assert!(!Binding::disabled().is_modifiers_alone());
    assert_eq!(String::from(Binding::disabled()), "");
    assert!(Binding::try_from("super+".to_owned()).is_err());
    let held = |super_key, alt| Modifiers {
      super_key,
      alt,
      ..Modifiers::default()
    };
    assert!(menu.matches_modifiers_alone(held(true, false)));
    assert!(!menu.matches_modifiers_alone(held(true, true)));
    assert!(both.matches_modifiers_alone(Modifiers {
      control: true,
      alt: true,
      ..Modifiers::default()
    }));
    // It is never a key, and a key binding never matches here.
    assert!(!menu.matches(held(true, false), ""));
    assert!(!menu.matches(held(false, false), "super"));
    assert!(!parse("super+q").matches_modifiers_alone(held(true, false)));
    let yaml: Keybindings =
      serde_yaml::from_str("toggle_menu: shift+super\n").unwrap();
    assert_eq!(
      yaml.toggle_menu,
      Binding::modifiers_alone(vec![Modifier::Shift, Modifier::Super])
    );
    assert_eq!(
      Keybindings::default().toggle_menu,
      Binding::modifiers_alone(vec![Modifier::Super])
    );
    assert_eq!(InputConfig::default().modifier_binding_timeout, 400);
  }

  #[test]
  fn colours_parse_in_every_wayfire_notation() {
    let parse = |text| parse_color(text).unwrap();
    assert_eq!(parse("#FF000080"), [1.0, 0.0, 0.0, 128.0 / 255.0]);
    assert_eq!(parse(" #00ff00 "), [0.0, 1.0, 0.0, 1.0]);
    assert_eq!(parse("#F00F"), [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(parse("0.5 0.25 0 1"), [0.5, 0.25, 0.0, 1.0]);
    for text in ["#F00", "#+1+1+1", "#ééé", "aéééb", "1 2 3", "#GG0000"] {
      assert!(parse_color(text).is_err(), "{text}");
    }
  }

  #[test]
  fn only_a_wayfire_file_is_exported_as_wayfire_config_file() {
    let path = Path::new("/home/user/.config/wayfire.ini");
    let parse = |text, format| ShingleConfig::parse(text, format).unwrap();
    let wayfire = parse("[core]\nvwidth = 2\n", ConfigFormat::Wayfire);
    assert_eq!(wayfire.format, ConfigFormat::Wayfire);
    assert_eq!(wayfire.wayfire_config_file(path), Some(path.to_owned()));
    for (text, format) in [("{}", ConfigFormat::Yaml), ("", ConfigFormat::I3)] {
      let config = parse(text, format);
      assert_eq!(config.format, format);
      assert_eq!(config.wayfire_config_file(path), None, "{format:?}");
    }
    assert_eq!(ShingleConfig::default().format, ConfigFormat::Yaml);
  }

  #[test]
  fn a_relative_wayfire_config_file_is_made_absolute() {
    let config = ShingleConfig::parse("", ConfigFormat::Wayfire).unwrap();
    let exported = config
      .wayfire_config_file(Path::new("wayfire.ini"))
      .expect("a Wayfire file is exported");
    assert!(exported.is_absolute(), "{exported:?}");
    assert_eq!(
      exported,
      std::env::current_dir().unwrap().join("wayfire.ini")
    );
  }

  #[test]
  fn load_records_the_format_it_detected() {
    let directory = std::env::temp_dir()
      .join(format!("shingle-config-format-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let ini = directory.join("wayfire.ini");
    std::fs::write(&ini, "[core]\nvwidth = 2\n").unwrap();
    // A Wayfire file is recognised by its first `[` whatever its name.
    let unnamed = directory.join("config");
    std::fs::write(&unnamed, "[core]\n").unwrap();
    let yaml = directory.join("shingle.yaml");
    std::fs::write(&yaml, "workspaces: {width: 2}\n").unwrap();
    let format = |path: &Path| ShingleConfig::load(Some(path)).unwrap().format;
    assert_eq!(format(&ini), ConfigFormat::Wayfire);
    assert_eq!(format(&unnamed), ConfigFormat::Wayfire);
    assert_eq!(format(&yaml), ConfigFormat::Yaml);
    std::fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn default_path_prefers_shingle_location() {
    let home = Path::new("/cfg");
    let path = resolve_default_path(home, |_| true);
    assert_eq!(path, home.join("shingle/shingle.yaml"));
    let path = resolve_default_path(home, |_| false);
    assert_eq!(path, home.join("shingle/shingle.yaml"));
  }

  #[test]
  fn default_path_falls_back_to_legacy_tilefire_location() {
    let home = Path::new("/cfg");
    let legacy = home.join("tilefire/tilefire.yaml");
    let path = resolve_default_path(home, |path| path == legacy);
    assert_eq!(path, legacy);
  }

  #[test]
  fn vswitch_imports_and_validates_transition_options() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[vswitch]\n",
        "duration = 250ms circle\n",
        "gap = 24\n",
        "background = \\#11223380\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.vswitch.duration_ms, 250);
    assert_eq!(wayfire.vswitch.gap, 24);
    assert_eq!(wayfire.vswitch.background[3], 128.0 / 255.0);

    let i3 = ShingleConfig::parse(
      concat!(
        "set $vswitch_duration_ms 125\n",
        "set $vswitch_gap 16\n",
        "set $vswitch_background 0.1 0.2 0.3 0.4\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.vswitch.duration_ms, 125);
    assert_eq!(i3.vswitch.gap, 16);
    assert_eq!(i3.vswitch.background[1], 0.2);
    assert!(
      ShingleConfig::parse(
        "vswitch:\n  background: [0, 0, 0, 2]\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }

  #[test]
  fn expo_imports_and_validates_wayfire_i3_and_yaml() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[expo]\n",
        "toggle = <super> KEY_E\n",
        "background = \\#11223380\n",
        "duration = 300ms circle\n",
        "transition_length = 200\n",
        "offset = 14\n",
        "keyboard_interaction = false\n",
        "inactive_brightness = 0.6\n",
        "select_workspace_2 = <super> KEY_2\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.keybindings.expo_toggle.key, "e");
    assert_eq!(wayfire.expo.background[3], 128.0 / 255.0);
    assert_eq!(wayfire.expo.duration_ms, 300);
    assert_eq!(wayfire.expo.transition_length_ms, 200);
    assert_eq!(wayfire.expo.offset, 14);
    assert!(!wayfire.expo.keyboard_interaction);
    assert_eq!(wayfire.expo.inactive_brightness, 0.6);
    assert_eq!(wayfire.expo.workspace_bindings.len(), 1);
    assert_eq!(wayfire.expo.workspace_bindings[0].workspace, 2);
    assert_eq!(wayfire.expo.workspace_bindings[0].binding.key, "2");

    let i3 = ShingleConfig::parse(
      concat!(
        "set $expo_background 0.1 0.2 0.3 0.4\n",
        "set $expo_duration_ms 250\n",
        "set $expo_transition_length_ms 150\n",
        "set $expo_offset 12\n",
        "set $expo_keyboard_interaction no\n",
        "set $expo_inactive_brightness 0.5\n",
        "bindsym Mod4+e expo toggle\n",
        "bindsym Mod4+2 expo select 2\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.keybindings.expo_toggle.key, "e");
    assert_eq!(i3.expo.background[1], 0.2);
    assert_eq!(i3.expo.duration_ms, 250);
    assert_eq!(i3.expo.transition_length_ms, 150);
    assert_eq!(i3.expo.offset, 12);
    assert!(!i3.expo.keyboard_interaction);
    assert_eq!(i3.expo.inactive_brightness, 0.5);
    assert_eq!(i3.expo.workspace_bindings.len(), 1);
    assert_eq!(i3.expo.workspace_bindings[0].workspace, 2);
    assert_eq!(i3.expo.workspace_bindings[0].binding.key, "2");

    for yaml in [
      "expo:\n  inactive_brightness: 1.1\n",
      "expo:\n  inactive_brightness: nan\n",
      "expo:\n  background: [0, 0, 0, 2]\n",
      concat!(
        "workspaces:\n  width: 1\n  height: 1\n",
        "expo:\n  workspace_bindings:\n",
        "    - workspace: 2\n      binding: super+2\n",
      ),
      concat!(
        "expo:\n  workspace_bindings:\n",
        "    - workspace: 0\n      binding: super+0\n",
      ),
    ] {
      assert!(ShingleConfig::parse(yaml, ConfigFormat::Yaml).is_err());
    }
  }

  #[test]
  fn scale_imports_from_wayfire_and_i3() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = scale\n",
        "[scale]\n",
        "toggle = <super> KEY_O\n",
        "toggle_all = <super> <shift> KEY_O\n",
        "spacing = 24\n",
        "outer_margin = 12\n",
        "inactive_alpha = 0.6\n",
        "allow_zoom = true\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.keybindings.scale_toggle.key, "o");
    assert_eq!(
      wayfire.keybindings.scale_toggle.modifiers,
      [Modifier::Super]
    );
    assert_eq!(wayfire.keybindings.scale_toggle_all.key, "o");
    assert_eq!(
      wayfire.keybindings.scale_toggle_all.modifiers,
      [Modifier::Super, Modifier::Shift]
    );
    assert_eq!(wayfire.scale.spacing, 24);
    assert_eq!(wayfire.scale.outer_margin, 12);
    assert_eq!(wayfire.scale.inactive_alpha, 0.6);
    assert!(wayfire.scale.allow_zoom);

    let i3 = ShingleConfig::parse(
      concat!(
        "set $scale_spacing 30\n",
        "set $scale_outer_margin 15\n",
        "set $scale_inactive_alpha 0.5\n",
        "set $scale_allow_zoom yes\n",
        "bindsym Mod4+o scale toggle\n",
        "bindsym Mod4+Shift+o scale toggle_all\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.keybindings.scale_toggle.key, "o");
    assert_eq!(i3.keybindings.scale_toggle.modifiers, [Modifier::Super]);
    assert_eq!(i3.keybindings.scale_toggle_all.key, "o");
    assert_eq!(
      i3.keybindings.scale_toggle_all.modifiers,
      [Modifier::Super, Modifier::Shift]
    );
    assert_eq!(i3.scale.spacing, 30);
    assert_eq!(i3.scale.outer_margin, 15);
    assert_eq!(i3.scale.inactive_alpha, 0.5);
    assert!(i3.scale.allow_zoom);
  }

  #[test]
  fn scale_imports_interaction_and_minimized_options() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = scale\n",
        "[scale]\n",
        "middle_click_close = true\n",
        "include_minimized = true\n",
        "minimized_alpha = 0.4\n",
        "close_on_new_view = true\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert!(wayfire.scale.middle_click_close);
    assert!(wayfire.scale.include_minimized);
    assert!(wayfire.scale.close_on_new_view);
    assert_eq!(wayfire.scale.minimized_alpha, 0.4);
    let i3 = ShingleConfig::parse(
      concat!(
        "set $scale_middle_click_close yes\n",
        "set $scale_include_minimized yes\n",
        "set $scale_minimized_alpha 0.4\n",
        "set $scale_close_on_new_view yes\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert!(i3.scale.middle_click_close);
    assert!(i3.scale.include_minimized);
    assert!(i3.scale.close_on_new_view);
    assert_eq!(i3.scale.minimized_alpha, 0.4);
  }

  #[test]
  fn save_yaml_syncs_the_directory_after_the_rename() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir()
      .join(format!("shingle-config-sync-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("shingle.yaml");
    ShingleConfig::default().save_yaml(&path).unwrap();
    // Without read permission the directory cannot be opened to sync it,
    // although files in it can still be created and renamed.
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o300)).unwrap();
    let readable = fs::File::open(&directory).is_ok();
    let result = ShingleConfig::default().save_yaml(&path);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&directory).unwrap();
    if readable {
      // Running as root: permissions do not apply.
      return;
    }
    let Err(ConfigError::Write { source, .. }) = result else {
      panic!("the directory was not opened for syncing: {result:?}");
    };
    assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
  }

  #[test]
  fn unknown_zoom_interpolation_names_its_line() {
    let warnings = warnings_of(
      "[zoom]\n\ninterpolation_method = 5\n",
      ConfigFormat::Wayfire,
    );
    assert_eq!(
      warnings,
      [concat!(
        "line 3: [zoom] interpolation_method: unknown zoom interpolation; ",
        "keeping the default"
      )]
    );
  }

  #[test]
  fn scale_imports_duration() {
    let wayfire = ShingleConfig::parse(
      "[core]\nplugins = scale\n[scale]\nduration = 250ms\n",
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.scale.duration_ms, 250);
    let i3 =
      ShingleConfig::parse("set $scale_duration_ms 125\n", ConfigFormat::I3)
        .unwrap();
    assert_eq!(i3.scale.duration_ms, 125);
  }

  #[test]
  fn scale_imports_and_validates_title_options() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = scale\n",
        "[scale]\n",
        "title_overlay = mouse\n",
        "title_font_size = 20\n",
        "title_position = bottom\n",
        "bg_color = \\#11223380\n",
        "text_color = \\#ddeeff\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.scale.title_overlay, "mouse");
    assert_eq!(wayfire.scale.title_font_size, 20);
    assert_eq!(wayfire.scale.title_position, "bottom");
    assert_eq!(wayfire.scale.title_bg_color[3], 128.0 / 255.0);
    assert_eq!(wayfire.scale.title_text_color[0], 221.0 / 255.0);

    let i3 = ShingleConfig::parse(
      concat!(
        "set $scale_title_overlay all\n",
        "set $scale_title_font_size 18\n",
        "set $scale_title_position top\n",
        "set $scale_title_bg_color 0.1 0.2 0.3 0.4\n",
        "set $scale_title_text_color 0.8 0.7 0.6 1\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.scale.title_overlay, "all");
    assert_eq!(i3.scale.title_font_size, 18);
    assert_eq!(i3.scale.title_position, "top");
    assert_eq!(i3.scale.title_bg_color[1], 0.2);
    assert_eq!(i3.scale.title_text_color[2], 0.6);

    for yaml in [
      "scale:\n  title_overlay: sometimes\n",
      "scale:\n  title_position: left\n",
      "scale:\n  title_font_size: 0\n",
      "scale:\n  title_bg_color: [0, 0, 0, 2]\n",
    ] {
      assert!(ShingleConfig::parse(yaml, ConfigFormat::Yaml).is_err());
    }
  }

  #[test]
  fn wayfire_and_i3_normalize_arrow_and_keypad_aliases() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[grid]\n",
        "slot_tl = <super> KEY_KP7\n",
        "[vswitch]\n",
        "binding_right = <super> <alt> KEY_RIGHT\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.keybindings.grid_slot_top_left[0].key, "kp7");
    assert_eq!(wayfire.keybindings.workspace_next.key, "right");

    let i3 = ShingleConfig::parse(
      concat!(
        "bindsym Mod4+KP_7 snap top_left\n",
        "bindsym Mod4+Alt+Right workspace next\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.keybindings.grid_slot_top_left[0].key, "kp7");
    assert_eq!(i3.keybindings.workspace_next.key, "right");
  }

  #[test]
  fn scale_filter_imports_and_validates_options() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = scale scale-title-filter\n",
        "[scale-title-filter]\n",
        "case_sensitive = true\n",
        "share_filter = true\n",
        "overlay = false\n",
        "font_size = 32\n",
        "bg_color = \\#11223380\n",
        "text_color = \\#ddeeff\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert!(wayfire.scale_title_filter.case_sensitive);
    assert!(wayfire.scale_title_filter.share_filter);
    assert!(!wayfire.scale_title_filter.overlay);
    assert_eq!(wayfire.scale_title_filter.font_size, 32);
    assert_eq!(wayfire.scale_title_filter.bg_color[2], 0.2);

    let i3 = ShingleConfig::parse(
      concat!(
        "set $scale_filter_case_sensitive yes\n",
        "set $scale_filter_share yes\n",
        "set $scale_filter_overlay no\n",
        "set $scale_filter_font_size 28\n",
        "set $scale_filter_bg_color 0.1 0.2 0.3 0.4\n",
        "set $scale_filter_text_color 0.8 0.7 0.6 1\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert!(i3.scale_title_filter.case_sensitive);
    assert!(i3.scale_title_filter.share_filter);
    assert!(!i3.scale_title_filter.overlay);
    assert_eq!(i3.scale_title_filter.font_size, 28);
    assert_eq!(i3.scale_title_filter.text_color[1], 0.7);

    assert!(
      ShingleConfig::parse(
        "scale_title_filter:\n  font_size: 0\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }

  #[test]
  fn popup_focus_and_primary_selection_workarounds_are_imported() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[workarounds]\n",
        "focus_main_surface_instead_of_popup = true\n",
        "disable_primary_selection = true\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert!(wayfire.behavior.focus_main_surface_instead_of_popup);
    assert!(wayfire.behavior.disable_primary_selection);
    let i3 = ShingleConfig::parse(
      concat!(
        "set $focus_main_surface_instead_of_popup yes\n",
        "set $disable_primary_selection yes\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert!(i3.behavior.focus_main_surface_instead_of_popup);
    assert!(i3.behavior.disable_primary_selection);
    assert!(!ShingleConfig::default().behavior.disable_primary_selection);
  }

  #[test]
  fn security_context_privileged_protocols_are_imported() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = security-context-v1\n",
        "[security-context-v1]\n",
        "privileged_protocols = zwlr_screencopy_manager_v1,",
        " zwp_virtual_keyboard_manager_v1\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(
      wayfire.security_context.privileged_protocols,
      "zwlr_screencopy_manager_v1, zwp_virtual_keyboard_manager_v1"
    );
    let i3 = ShingleConfig::parse(
      "set $security_privileged_protocols zwlr_screencopy_manager_v1\n",
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(
      i3.security_context.privileged_protocols,
      "zwlr_screencopy_manager_v1"
    );
    let defaults = SecurityContextConfig::default().privileged_protocols;
    for protocol in [
      "zwlr_foreign_toplevel_manager_v1",
      "zwlr_screencopy_manager_v1",
      "zwlr_data_control_manager_v1",
      "zwp_keyboard_shortcuts_inhibit_manager_v1",
    ] {
      assert!(defaults.split(',').any(|name| name == protocol));
    }
    let shipped = ShingleConfig::parse(
      include_str!("../../../data/shingle.yaml"),
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(
      shipped
        .security_context
        .privileged_protocols
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>(),
      defaults.split(',').collect::<Vec<_>>()
    );
  }

  #[test]
  fn text_input_v3_enablement_is_imported() {
    let wayfire = ShingleConfig::parse(
      "[input-method-v1]\nenable_text_input_v3 = false\n",
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert!(!wayfire.input.enable_text_input_v3);
    let i3 =
      ShingleConfig::parse("set $enable_text_input_v3 no\n", ConfigFormat::I3)
        .unwrap();
    assert!(!i3.input.enable_text_input_v3);
    assert!(ShingleConfig::default().input.enable_text_input_v3);
  }

  #[test]
  fn decoration_modes_are_imported_and_validated() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\n",
        "preferred_decoration_mode = server\n",
        "[workarounds]\n",
        "force_preferred_decoration_mode = true\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.behavior.preferred_decoration_mode, "server");
    assert!(wayfire.behavior.force_preferred_decoration_mode);

    let i3 = ShingleConfig::parse(
      concat!(
        "set $preferred_decoration_mode server\n",
        "set $force_preferred_decoration_mode yes\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.behavior.preferred_decoration_mode, "server");
    assert!(i3.behavior.force_preferred_decoration_mode);
    assert!(
      ShingleConfig::parse(
        "behavior:\n  preferred_decoration_mode: automatic\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }

  #[test]
  fn xwayland_defaults_to_lazy_without_a_startup_script() {
    let config = ShingleConfig::default();
    assert_eq!(config.xwayland.mode, XwaylandMode::Lazy);
    assert_eq!(config.xwayland.startup_script, "");
    let yaml =
      ShingleConfig::parse("output:\n  width: 800\n", ConfigFormat::Yaml)
        .unwrap();
    assert_eq!(yaml.xwayland.mode, XwaylandMode::Lazy);
  }

  #[test]
  fn xwayland_mode_takes_wayfire_values_in_yaml() {
    let parse = |text: &str| {
      ShingleConfig::parse(text, ConfigFormat::Yaml).map(|c| c.xwayland)
    };
    assert_eq!(
      parse("xwayland:\n  mode: true\n").unwrap().mode,
      XwaylandMode::Eager
    );
    assert_eq!(
      parse("xwayland:\n  mode: false\n").unwrap().mode,
      XwaylandMode::Disabled
    );
    assert_eq!(
      parse("xwayland:\n  mode: lazy\n").unwrap().mode,
      XwaylandMode::Lazy
    );
    assert_eq!(
      parse("xwayland:\n  mode: \"true\"\n").unwrap().mode,
      XwaylandMode::Eager
    );
    let script = parse("xwayland:\n  startup_script: xrdb -merge x\n").unwrap();
    assert_eq!(script.startup_script, "xrdb -merge x");
    assert!(parse("xwayland:\n  mode: sometimes\n").is_err());
    assert!(parse("xwayland:\n  mode: 1\n").is_err());
    assert!(parse("xwayland:\n  modes: lazy\n").is_err());
  }

  #[test]
  fn xwayland_mode_round_trips_through_yaml() {
    for mode in [
      XwaylandMode::Eager,
      XwaylandMode::Lazy,
      XwaylandMode::Disabled,
    ] {
      let mut config = ShingleConfig::default();
      config.xwayland.mode = mode;
      config.xwayland.startup_script = "echo ready".into();
      let text = serde_yaml::to_string(&config).unwrap();
      let parsed = ShingleConfig::parse(&text, ConfigFormat::Yaml).unwrap();
      assert_eq!(parsed.xwayland.mode, mode);
      assert_eq!(parsed.xwayland.startup_script, "echo ready");
      assert_eq!(mode.to_string().parse::<XwaylandMode>().unwrap(), mode);
    }
    assert_eq!(
      " LAZY ".parse::<XwaylandMode>().unwrap(),
      XwaylandMode::Lazy
    );
  }

  #[test]
  fn wayfire_import_keeps_wayfire_xwayland_default_and_values() {
    let import = |text: &str| {
      ShingleConfig::parse(text, ConfigFormat::Wayfire).map(|c| c.xwayland)
    };
    let default = import("[core]\nvwidth = 2\n").unwrap();
    assert_eq!(default.mode, XwaylandMode::Eager);
    assert_eq!(default.startup_script, "");
    let lazy = import(concat!(
      "[core]\n",
      "xwayland = lazy\n",
      "xwayland_startup_script = xrdb -merge ~/.Xresources\n",
    ))
    .unwrap();
    assert_eq!(lazy.mode, XwaylandMode::Lazy);
    assert_eq!(lazy.startup_script, "xrdb -merge ~/.Xresources");
    assert_eq!(
      import("[core]\nxwayland = false\n").unwrap().mode,
      XwaylandMode::Disabled
    );
    assert_eq!(
      import("[core]\nxwayland = TRUE\n").unwrap().mode,
      XwaylandMode::Eager
    );
    assert_eq!(
      import("[core]\nxwayland = maybe\n").unwrap().mode,
      XwaylandMode::Eager
    );
    assert_eq!(
      warnings_of("[core]\nxwayland = maybe\n", ConfigFormat::Wayfire),
      [concat!(
        "line 2: [core] xwayland: must be true, false or lazy, not ",
        "\"maybe\"; keeping the default"
      )]
    );
    // Other sections' options of the same name are not XWayland's.
    assert_eq!(
      import("[other]\nxwayland = false\n").unwrap().mode,
      XwaylandMode::Eager
    );
  }

  #[test]
  fn wayfire_import_reads_libinput_device_options() {
    let defaults = InputConfig::default();
    assert_eq!(defaults.mouse_accel_profile, "default");
    assert_eq!(defaults.touchpad_cursor_speed, 0.0);
    assert!(defaults.tap_to_click && defaults.tap_and_drag);
    assert!(!defaults.natural_scroll && !defaults.drag_lock);
    let input = ShingleConfig::parse(
      concat!(
        "[input]\n",
        "mouse_accel_profile = flat\n",
        "mouse_cursor_speed = -0.5\n",
        "touchpad_accel_profile = adaptive\n",
        "touchpad_cursor_speed = 0.25\n",
        "tap_to_click = false\n",
        "click_method = clickfinger\n",
        "scroll_method = two-finger\n",
        "natural_scroll = true\n",
        "disable_touchpad_while_typing = true\n",
        "disable_touchpad_while_mouse = true\n",
        "tap_and_drag = false\n",
        "drag_lock = true\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap()
    .input;
    assert_eq!(input.mouse_accel_profile, "flat");
    assert_eq!(input.mouse_cursor_speed, -0.5);
    assert_eq!(input.touchpad_accel_profile, "adaptive");
    assert_eq!(input.touchpad_cursor_speed, 0.25);
    assert!(!input.tap_to_click);
    assert_eq!(input.click_method, "clickfinger");
    assert_eq!(input.scroll_method, "two-finger");
    assert!(input.natural_scroll);
    assert!(input.disable_touchpad_while_typing);
    assert!(input.disable_touchpad_while_mouse);
    assert!(!input.tap_and_drag);
    assert!(input.drag_lock);
  }

  #[test]
  fn libinput_device_options_are_validated() {
    for line in [
      "mouse_accel_profile = fast",
      "touchpad_accel_profile = Flat",
      "click_method = areas",
      "scroll_method = three-finger",
      "mouse_cursor_speed = 1.5",
      "touchpad_cursor_speed = -2",
    ] {
      let text = format!("[input]\n{line}\n");
      let (config, warnings) =
        ShingleConfig::parse_with_warnings(&text, ConfigFormat::Wayfire)
          .unwrap();
      assert_eq!(warnings.len(), 1, "{line}");
      assert_eq!(warnings[0].line, Some(2), "{line}");
      assert_eq!(
        config.input.mouse_accel_profile,
        InputConfig::default().mouse_accel_profile
      );
    }
    let yaml = "input:\n  scroll_method: on-button-down\n";
    let input = ShingleConfig::parse(yaml, ConfigFormat::Yaml)
      .unwrap()
      .input;
    assert_eq!(input.scroll_method, "on-button-down");
  }

  #[test]
  fn maximum_buffer_size_is_imported_and_validated() {
    let wayfire = ShingleConfig::parse(
      "[workarounds]\nmax_buffer_size = 4096\n",
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.behavior.max_buffer_size, 4096);
    let i3 =
      ShingleConfig::parse("set $max_buffer_size 4096\n", ConfigFormat::I3)
        .unwrap();
    assert_eq!(i3.behavior.max_buffer_size, 4096);
    assert!(
      ShingleConfig::parse(
        "behavior:\n  max_buffer_size: 0\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }

  #[test]
  fn scale_rejects_invalid_alpha() {
    for field in ["inactive_alpha", "minimized_alpha"] {
      for value in ["-0.1", "1.1", "nan"] {
        let yaml = format!("scale:\n  {field}: {value}\n");
        assert!(ShingleConfig::parse(&yaml, ConfigFormat::Yaml).is_err());
      }
    }
  }

  #[test]
  fn vswipe_imports_every_option_and_validates() {
    let text = concat!(
      "[core]\nplugins = vswipe\n[vswipe]\n",
      "enable_horizontal = false\nenable_vertical = false\n",
      "enable_free_movement = true\nenable_smooth_transition = true\n",
      "duration = 250ms circle\nbackground = \\#FF000080\n",
      "fingers = 3\ngap = 12.5\nthreshold = 0.5\n",
      "delta_threshold = 30\nspeed_factor = 128\nspeed_cap = 0.1\n",
    );
    let (config, warnings) =
      ShingleConfig::parse_with_warnings(text, ConfigFormat::Wayfire).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(config.plugin_enabled("vswipe"));
    assert_eq!(
      config.vswipe,
      VswipeConfig {
        enable_horizontal: false,
        enable_vertical: false,
        enable_free_movement: true,
        enable_smooth_transition: true,
        duration_ms: 250,
        background: [1.0, 0.0, 0.0, 128.0 / 255.0],
        fingers: 3,
        gap: 12.5,
        threshold: 0.5,
        delta_threshold: 30.0,
        speed_factor: 128.0,
        speed_cap: 0.1,
      }
    );
    // Wayfire's defaults are Shingle's.
    let wayfire =
      ShingleConfig::parse("[core]\nplugins = vswipe\n", ConfigFormat::Wayfire)
        .unwrap();
    assert_eq!(wayfire.vswipe, VswipeConfig::default());
    // Without vswipe in the list its section is not read, and the plugin
    // is off.
    let unlisted = ShingleConfig::parse(
      "[core]\nplugins = vswitch\n[vswipe]\nfingers = 3\n",
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert!(!unlisted.plugin_enabled("vswipe"));
    assert_eq!(unlisted.vswipe.fingers, 4);
    // Bad values warn and keep the default.
    for line in [
      "fingers = 0",
      "speed_factor = 0",
      "speed_cap = -1",
      "gap = -2",
      "threshold = nan",
      "background = 2 0 0 1",
    ] {
      let text = format!("[core]\nplugins = vswipe\n[vswipe]\n{line}\n");
      let (config, warnings) =
        ShingleConfig::parse_with_warnings(&text, ConfigFormat::Wayfire)
          .unwrap();
      assert_eq!(warnings.len(), 1, "{line}: {warnings:?}");
      assert_eq!(config.vswipe, VswipeConfig::default(), "{line}");
    }
    let yaml = "vswipe:\n  fingers: 3\n  enable_free_movement: true\n";
    let yaml = ShingleConfig::parse(yaml, ConfigFormat::Yaml).unwrap();
    assert_eq!(yaml.vswipe.fingers, 3);
    assert!(yaml.vswipe.enable_free_movement);
    assert!(
      ShingleConfig::parse("vswipe:\n  speed_factor: 0\n", ConfigFormat::Yaml)
        .is_err()
    );
  }

  #[test]
  fn extra_gestures_import_and_validate() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = extra-gestures\n",
        "[extra-gestures]\n",
        "move_fingers = 4\n",
        "move_delay = 500\n",
        "close_fingers = 2\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.extra_gestures.move_fingers, 4);
    assert_eq!(wayfire.extra_gestures.move_delay, 500);
    assert_eq!(wayfire.extra_gestures.close_fingers, 2);

    let i3 = ShingleConfig::parse(
      concat!(
        "set $gesture_move_fingers 5\n",
        "set $gesture_move_delay 600\n",
        "set $gesture_close_fingers 3\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.extra_gestures.move_fingers, 5);
    assert_eq!(i3.extra_gestures.move_delay, 600);
    assert_eq!(i3.extra_gestures.close_fingers, 3);

    let yaml = ShingleConfig::parse(
      concat!(
        "extra_gestures:\n",
        "  move_fingers: 2\n",
        "  move_delay: 200\n",
        "  close_fingers: 4\n",
      ),
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(yaml.extra_gestures.move_fingers, 2);
    assert_eq!(yaml.extra_gestures.move_delay, 200);
    assert_eq!(yaml.extra_gestures.close_fingers, 4);
    assert!(
      ShingleConfig::parse(
        "extra_gestures:\n  move_delay: 0\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }

  #[test]
  fn xkb_keymap_supports_multiple_layouts_and_variants() {
    let input = InputConfig {
      layout: "us, de,cz".into(),
      variant: ",dvorak,".into(),
      ..Default::default()
    };
    use xkbcommon::xkb;
    let keymap = input.keymap().unwrap();
    assert!(!keymap.contains("include"));
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
      &context,
      keymap,
      xkb::KEYMAP_FORMAT_TEXT_V1,
      xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("the keymap compiles");
    let layouts = (0..keymap.num_layouts())
      .map(|index| keymap.layout_get_name(index).to_owned())
      .collect::<Vec<_>>();
    assert_eq!(layouts, ["English (US)", "German (Dvorak)", "Czech"]);
  }

  #[test]
  fn xkb_keymap_follows_rules_and_model() {
    let default = InputConfig::default().keymap().unwrap();
    let base = InputConfig {
      rules: "base".into(),
      ..Default::default()
    }
    .keymap()
    .unwrap();
    let chromebook = InputConfig {
      model: "chromebook".into(),
      ..Default::default()
    }
    .keymap()
    .unwrap();
    assert_ne!(base, default, "the rules choose the keycodes");
    assert_ne!(chromebook, default, "the model adds symbols");
  }

  #[test]
  fn xkb_command_bindings_preserve_symbol_case() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[core]\nplugins = command xkb-bindings\n",
        "[command]\n",
        "binding_symbol = <super> xkb:XF86AudioPlay\n",
        "command_symbol = playerctl play-pause\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.commands[0].binding.key, "xkb:XF86AudioPlay");

    let i3 = ShingleConfig::parse(
      "bindsym Mod4+xkb:Return exec foot\n",
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.commands[0].binding.key, "xkb:Return");

    let yaml = ShingleConfig::parse(
      concat!(
        "commands:\n",
        "  - binding: super+xkb:A\n",
        "    command: true\n",
      ),
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(yaml.commands[0].binding.key, "xkb:A");
    assert!(
      ShingleConfig::parse(
        "commands:\n  - binding: xkb:\n    command: true\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }

  #[test]
  fn wrot_options_import_and_validate() {
    let wayfire = ShingleConfig::parse(
      concat!(
        "[wrot]\n",
        "activate = <alt> BTN_LEFT\n",
        "reset = <ctrl> KEY_F1\n",
        "reset-one = <shift> KEY_F2\n",
        "reset_radius = 40\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(wayfire.pointer.wrot_activate.key, "btn_left");
    assert_eq!(wayfire.keybindings.wrot_reset.key, "f1");
    assert_eq!(wayfire.keybindings.wrot_reset_one.key, "f2");
    assert_eq!(wayfire.effects.wrot_reset_radius, 40.0);

    let i3 = ShingleConfig::parse(
      concat!(
        "bindsym Mod4+w wrot activate\n",
        "bindsym Mod4+r wrot reset\n",
        "bindsym Mod4+o wrot reset-one\n",
        "set $wrot_reset_radius 30\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(i3.pointer.wrot_activate.key, "w");
    assert_eq!(i3.keybindings.wrot_reset.key, "r");
    assert_eq!(i3.keybindings.wrot_reset_one.key, "o");
    assert_eq!(i3.effects.wrot_reset_radius, 30.0);

    let yaml = ShingleConfig::parse(
      concat!(
        "effects:\n  wrot_reset_radius: 20\n",
        "pointer:\n  wrot_activate: alt+btn_left\n",
        "keybindings:\n  wrot_reset: ctrl+f1\n  wrot_reset_one: shift+f2\n",
      ),
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(yaml.effects.wrot_reset_radius, 20.0);
    assert_eq!(yaml.pointer.wrot_activate.key, "btn_left");
    assert_eq!(yaml.keybindings.wrot_reset.key, "f1");
    assert_eq!(yaml.keybindings.wrot_reset_one.key, "f2");
    assert!(
      ShingleConfig::parse(
        "effects:\n  wrot_reset_radius: 0\n",
        ConfigFormat::Yaml,
      )
      .is_err()
    );
  }
}
