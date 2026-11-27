//! The lock screen's options: swaylock's and swaylock-effects' config
//! format and command line, and the `lock` section of Shingle's YAML,
//! which `shingle-lock` reads and Shingle itself only uses for
//! [`LockConfig::command`] (docs/design/lock-screen.md).
//!
//! swaylock reads `$XDG_CONFIG_HOME/swaylock/config` (or the file `-C`
//! names) as one long option per line, without the leading `--`:
//! `name=value` for options with a value and the bare name for flags,
//! with `#` comments and blank lines skipped (swaylock's `main.c`,
//! `load_config`). [`LockOptions::apply`] takes each such option, from
//! the file, from the command line and from the YAML `lock.options` map
//! alike. Options swaylock or swaylock-effects know but `shingle-lock`
//! does not draw are ignored with a warning, as are unknown ones.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::Deserialize;

use crate::ConfigError;

/// An sRGB colour as swaylock writes it, `rrggbb[aa]`: red, green, blue
/// and alpha bytes.
pub type LockColor = [u8; 4];

/// The colours of the password input in one state, from swaylock's
/// indicator colours: the ring becomes the input's border, the inside its
/// background, the line the thin edge between them and the text the dots
/// and messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateColors {
  /// The border (`ring-*-color`).
  pub ring: LockColor,
  /// The background (`inside-*-color`).
  pub inside: LockColor,
  /// The edge between border and background (`line-*-color`).
  pub line: LockColor,
  /// The dots and messages (`text-*-color`).
  pub text: LockColor,
}

/// Which state the password input shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputState {
  /// Typing.
  Input,
  /// The password was cleared.
  Clear,
  /// PAM is checking the password.
  Verify,
  /// PAM refused it.
  Wrong,
}

/// The input's colours, by state, with swaylock's defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndicatorColors {
  /// While typing (`ring-color`, `inside-color`, `line-color`,
  /// `text-color`).
  pub input: StateColors,
  /// After clearing (`*-clear-color`).
  pub clear: StateColors,
  /// While verifying (`*-ver-color`).
  pub verify: StateColors,
  /// After a wrong password (`*-wrong-color`).
  pub wrong: StateColors,
  /// The highlight a key press draws on the border (`key-hl-color`).
  pub key_highlight: LockColor,
  /// The highlight a backspace draws (`bs-hl-color`).
  pub backspace_highlight: LockColor,
  /// The ends of a highlight (`separator-color`).
  pub separator: LockColor,
}

impl IndicatorColors {
  /// The colours of `state`.
  pub fn of(&self, state: InputState) -> StateColors {
    match state {
      InputState::Input => self.input,
      InputState::Clear => self.clear,
      InputState::Verify => self.verify,
      InputState::Wrong => self.wrong,
    }
  }
}

impl Default for IndicatorColors {
  /// swaylock's defaults (swaylock's `main.c`, `set_default_colors`).
  fn default() -> Self {
    let black = [0, 0, 0, 0xff];
    Self {
      input: StateColors {
        ring: [0x33, 0x7d, 0x00, 0xff],
        inside: [0x00, 0x00, 0x00, 0xc0],
        line: black,
        text: [0xe5, 0xa4, 0x45, 0xff],
      },
      clear: StateColors {
        ring: [0xe5, 0xa4, 0x45, 0xff],
        inside: [0x00, 0x00, 0x00, 0x00],
        line: black,
        text: black,
      },
      verify: StateColors {
        ring: [0x33, 0x00, 0xfa, 0xff],
        inside: [0x00, 0x72, 0xff, 0xc0],
        line: black,
        text: black,
      },
      wrong: StateColors {
        ring: [0x7d, 0x33, 0x00, 0xff],
        inside: [0xfa, 0x00, 0x00, 0xc0],
        line: black,
        text: black,
      },
      key_highlight: [0x33, 0xdb, 0x00, 0xff],
      backspace_highlight: [0xdb, 0x33, 0x00, 0xff],
      separator: black,
    }
  }
}

/// What the line colour follows (`line-uses-inside`, `line-uses-ring`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineSource {
  /// The `line-*-color` options.
  #[default]
  Own,
  /// The inside colour of the same state.
  Inside,
  /// The ring colour of the same state.
  Ring,
}

/// Everything `shingle-lock` takes from swaylock's options.
#[derive(Clone, Debug, PartialEq)]
pub struct LockOptions {
  /// Detach once the session is locked (`daemonize`, `-f`).
  pub daemonize: bool,
  /// Show the number of failed attempts (`show-failed-attempts`, `-F`).
  pub show_failed_attempts: bool,
  /// Show the time and date (`clock`).
  pub clock: bool,
  /// Capture every output as the background (`screenshot`, `-S`).
  pub screenshot: bool,
  /// Blur the background: radius in pixels and passes (`effect-blur`,
  /// `RxT`).
  pub blur: Option<(u32, u32)>,
  /// Darken the background towards the edges: base and factor
  /// (`effect-vignette`, `B:F`), as swaylock-effects computes it.
  pub vignette: Option<(f64, f64)>,
  /// The background colour, under the screenshot or alone (`color`,
  /// `-c`). Unlike swaylock's white, the default is a dark blue-black.
  pub color: LockColor,
  /// The font family, found with fontconfig (`font`); default
  /// `sans-serif`.
  pub font: String,
  /// The text size in pixels, `None` for the automatic size
  /// (`font-size`).
  pub font_size: Option<u32>,
  /// Show the password input at rest too, not only once typing starts
  /// (`indicator`).
  pub indicator_always: bool,
  /// Show the password input at all (cleared by `no-unlock-indicator`,
  /// `-u`).
  pub show_indicator: bool,
  /// The input's size: its height, in logical pixels; it is six times
  /// as wide (`indicator-radius`, default 50).
  pub indicator_radius: u32,
  /// The input's border width is half of this, in logical pixels
  /// (`indicator-thickness`, default 10).
  pub indicator_thickness: u32,
  /// The input's colours.
  pub colors: IndicatorColors,
  /// What the line colours follow.
  pub line_source: LineSource,
  /// Any key, pointer motion or touch unlocks without a password for
  /// this long after locking (`grace`, seconds).
  pub grace: Duration,
  /// Pointer motion does not end the grace period (`grace-no-mouse`).
  pub grace_no_mouse: bool,
  /// Touches do not end it (`grace-no-touch`).
  pub grace_no_touch: bool,
  /// The date line, a `strftime` format (`datestr`, default `%a, %x`).
  pub datestr: String,
  /// The time line, a `strftime` format (`timestr`, default `%T`).
  pub timestr: String,
  /// Enter with an empty password does nothing instead of trying it
  /// (`ignore-empty-password`, `-e`).
  pub ignore_empty_password: bool,
}

impl Default for LockOptions {
  fn default() -> Self {
    Self {
      daemonize: false,
      show_failed_attempts: false,
      clock: false,
      screenshot: false,
      blur: None,
      vignette: None,
      color: [0x1e, 0x22, 0x2e, 0xff],
      font: "sans-serif".into(),
      font_size: None,
      indicator_always: false,
      show_indicator: true,
      indicator_radius: 50,
      indicator_thickness: 10,
      colors: IndicatorColors::default(),
      line_source: LineSource::Own,
      grace: Duration::ZERO,
      grace_no_mouse: false,
      grace_no_touch: false,
      datestr: "%a, %x".into(),
      timestr: "%T".into(),
      ignore_empty_password: false,
    }
  }
}

/// What [`LockOptions::apply`] did with an option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
  /// It was taken.
  Set,
  /// A swaylock or swaylock-effects option `shingle-lock` does not
  /// draw; ignored.
  Unsupported,
  /// Not an option of either; ignored.
  Unknown,
}

/// swaylock's and swaylock-effects' options that `shingle-lock` knows and
/// ignores: images, keyboard layouts, Caps Lock, the other effects and
/// the indicator placement.
const UNSUPPORTED: &[&str] = &[
  "bs-hl-caps-lock-color",
  "caps-lock-bs-hl-color",
  "caps-lock-key-hl-color",
  "debug",
  "disable-caps-lock-text",
  "effect-compose",
  "effect-custom",
  "effect-greyscale",
  "effect-pixelate",
  "effect-scale",
  "fade-in",
  "hide-keyboard-layout",
  "image",
  "indicator-caps-lock",
  "indicator-idle-visible",
  "indicator-x-position",
  "indicator-y-position",
  "inside-caps-lock-color",
  "layout-bg-color",
  "layout-border-color",
  "layout-text-color",
  "line-caps-lock-color",
  "pass-char-keys",
  "ready-fd",
  "ring-caps-lock-color",
  "scaling",
  "show-keyboard-layout",
  "submit-on-touch",
  "text-caps-lock-color",
  "tiling",
  "trace",
];

/// Options that take a value; every other option is a flag.
const WITH_VALUE: &[&str] = &[
  "bs-hl-color",
  "color",
  "datestr",
  "effect-blur",
  "effect-vignette",
  "font",
  "font-size",
  "grace",
  "indicator-radius",
  "indicator-thickness",
  "inside-clear-color",
  "inside-color",
  "inside-ver-color",
  "inside-wrong-color",
  "key-hl-color",
  "line-clear-color",
  "line-color",
  "line-ver-color",
  "line-wrong-color",
  "ring-clear-color",
  "ring-color",
  "ring-ver-color",
  "ring-wrong-color",
  "separator-color",
  "text-clear-color",
  "text-color",
  "text-ver-color",
  "text-wrong-color",
  "timestr",
  // Unsupported, but their value must be skipped on the command line.
  "bs-hl-caps-lock-color",
  "caps-lock-bs-hl-color",
  "caps-lock-key-hl-color",
  "effect-compose",
  "effect-custom",
  "effect-pixelate",
  "effect-scale",
  "fade-in",
  "image",
  "indicator-x-position",
  "indicator-y-position",
  "inside-caps-lock-color",
  "layout-bg-color",
  "layout-border-color",
  "layout-text-color",
  "line-caps-lock-color",
  "ready-fd",
  "ring-caps-lock-color",
  "scaling",
  "text-caps-lock-color",
];

/// The long name of swaylock's short option `flag`, if it has one.
pub fn short_option(flag: char) -> Option<&'static str> {
  Some(match flag {
    'c' => "color",
    'd' => "debug",
    'e' => "ignore-empty-password",
    'f' => "daemonize",
    'F' => "show-failed-attempts",
    'i' => "image",
    'k' => "show-keyboard-layout",
    'K' => "hide-keyboard-layout",
    'l' => "indicator-caps-lock",
    'L' => "disable-caps-lock-text",
    'n' => "line-uses-inside",
    'r' => "line-uses-ring",
    's' => "scaling",
    'S' => "screenshot",
    't' => "tiling",
    'u' => "no-unlock-indicator",
    _ => return None,
  })
}

/// Whether option `name` takes a value.
pub fn takes_value(name: &str) -> bool {
  WITH_VALUE.contains(&name)
}

impl LockOptions {
  /// Applies one option, `name` without its `--`, with `value` for
  /// options that take one. A flag may also be given `true` or `false`
  /// (also `yes`, `no`, `1`, `0`), which the YAML `lock.options` map
  /// uses to turn one off again.
  ///
  /// # Errors
  ///
  /// A message naming the option for a missing or malformed value; the
  /// option is then left as it was.
  pub fn apply(
    &mut self,
    name: &str,
    value: Option<&str>,
  ) -> Result<Applied, String> {
    let name = name.trim().trim_start_matches("--");
    if UNSUPPORTED.contains(&name) {
      return Ok(Applied::Unsupported);
    }
    if takes_value(name) {
      let value = value
        .map(str::trim)
        .ok_or_else(|| format!("{name} needs a value"))?;
      self.apply_value(name, value)?;
      return Ok(Applied::Set);
    }
    let on = match value.map(str::trim) {
      None | Some("") => true,
      Some(text) => parse_bool(text)
        .ok_or_else(|| format!("{name} is a flag, not {text:?}"))?,
    };
    match name {
      "daemonize" => self.daemonize = on,
      "show-failed-attempts" => self.show_failed_attempts = on,
      "clock" => self.clock = on,
      "screenshot" | "screenshots" => self.screenshot = on,
      "indicator" => self.indicator_always = on,
      "no-unlock-indicator" => self.show_indicator = !on,
      "grace-no-mouse" => self.grace_no_mouse = on,
      "grace-no-touch" => self.grace_no_touch = on,
      "ignore-empty-password" => self.ignore_empty_password = on,
      "line-uses-inside" => {
        self.line_source = if on {
          LineSource::Inside
        } else {
          LineSource::Own
        }
      }
      "line-uses-ring" => {
        self.line_source = if on {
          LineSource::Ring
        } else {
          LineSource::Own
        }
      }
      _ => return Ok(Applied::Unknown),
    }
    Ok(Applied::Set)
  }

  fn apply_value(&mut self, name: &str, value: &str) -> Result<(), String> {
    let color =
      || parse_lock_color(value).map_err(|error| format!("{name}: {error}"));
    let colors = &mut self.colors;
    match name {
      "color" => self.color = color()?,
      "font" => {
        if value.is_empty() {
          return Err("font cannot be empty".into());
        }
        self.font = value.to_owned();
      }
      "font-size" => {
        let size = parse_number::<u32>(name, value)?;
        self.font_size = (size > 0).then_some(size);
      }
      "indicator-radius" => {
        self.indicator_radius = parse_number::<u32>(name, value)?.max(1)
      }
      "indicator-thickness" => {
        self.indicator_thickness = parse_number(name, value)?
      }
      "effect-blur" => self.blur = Some(parse_blur(value)?),
      "effect-vignette" => self.vignette = Some(parse_vignette(value)?),
      "grace" => {
        let seconds = parse_number::<f64>(name, value)?;
        if !seconds.is_finite() || seconds < 0.0 {
          return Err(format!("grace must be 0 or more seconds, not {value}"));
        }
        self.grace = Duration::from_secs_f64(seconds);
      }
      "datestr" => self.datestr = value.to_owned(),
      "timestr" => self.timestr = value.to_owned(),
      "key-hl-color" => colors.key_highlight = color()?,
      "bs-hl-color" => colors.backspace_highlight = color()?,
      "separator-color" => colors.separator = color()?,
      _ => {
        let (part, state) = state_color(name).expect("a value option");
        let target = match state {
          InputState::Input => &mut colors.input,
          InputState::Clear => &mut colors.clear,
          InputState::Verify => &mut colors.verify,
          InputState::Wrong => &mut colors.wrong,
        };
        let value = color()?;
        match part {
          "ring" => target.ring = value,
          "inside" => target.inside = value,
          "line" => target.line = value,
          _ => target.text = value,
        }
      }
    }
    Ok(())
  }

  /// The colours of `state` with [`LockOptions::line_source`] applied.
  pub fn state_colors(&self, state: InputState) -> StateColors {
    let mut colors = self.colors.of(state);
    match self.line_source {
      LineSource::Own => {}
      LineSource::Inside => colors.line = colors.inside,
      LineSource::Ring => colors.line = colors.ring,
    }
    colors
  }

  /// Applies every option of a swaylock config file's `text`, as
  /// swaylock reads it; returns a warning per option ignored or
  /// malformed, with its 1-based line. A malformed value leaves that
  /// option as it was.
  pub fn apply_file(&mut self, text: &str) -> Vec<(usize, String)> {
    let mut warnings = Vec::new();
    for (index, line) in text.lines().enumerate() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      let (name, value) = match line.split_once('=') {
        Some((name, value)) => (name.trim(), Some(value)),
        None => (line, None),
      };
      if let Some(warning) = self.apply_reporting(name, value) {
        warnings.push((index + 1, warning));
      }
    }
    warnings
  }

  /// [`LockOptions::apply`], with what it ignored or refused as a
  /// warning.
  pub fn apply_reporting(
    &mut self,
    name: &str,
    value: Option<&str>,
  ) -> Option<String> {
    match self.apply(name, value) {
      Ok(Applied::Set) => None,
      Ok(Applied::Unsupported) => {
        Some(format!("{name} is not supported by shingle-lock; ignored"))
      }
      Ok(Applied::Unknown) => Some(format!("unknown option {name}; ignored")),
      Err(error) => Some(format!("{error}; ignored")),
    }
  }

  /// Applies swaylock-style command-line `arguments` (without the
  /// program name): long options as `--name`, `--name=value` or `--name
  /// value`, swaylock's short options, and bundles of short flags such
  /// as `-fF`. `--config`/`-C` and its path, `--help`, `--version` and
  /// `--shingle-config` and its path are skipped; the caller handles
  /// them. Returns a warning per argument ignored or malformed.
  pub fn apply_arguments(&mut self, arguments: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
      let argument = &arguments[index];
      index += 1;
      let skipped_with_value = ["-C", "--config", "--shingle-config"];
      if skipped_with_value.contains(&argument.as_str()) {
        index += 1;
        continue;
      }
      if ["-h", "--help", "-v", "--version"].contains(&argument.as_str())
        || argument.starts_with("--config=")
        || argument.starts_with("--shingle-config=")
      {
        continue;
      }
      let names: Vec<String> = if let Some(long) = argument.strip_prefix("--") {
        vec![long.to_owned()]
      } else if let Some(short) = argument.strip_prefix('-')
        && !short.is_empty()
      {
        let mut names = Vec::new();
        for flag in short.chars() {
          match short_option(flag) {
            Some(name) => names.push(name.to_owned()),
            None => warnings.push(format!("unknown option -{flag}; ignored")),
          }
        }
        names
      } else {
        warnings.push(format!("unexpected argument {argument:?}; ignored"));
        continue;
      };
      for name in names {
        let (name, inline) = match name.split_once('=') {
          Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
          None => (name, None),
        };
        let value = if inline.is_none() && takes_value(&name) {
          let value = arguments.get(index).cloned();
          index += 1;
          value
        } else {
          inline
        };
        if let Some(warning) = self.apply_reporting(&name, value.as_deref()) {
          warnings.push(warning);
        }
      }
    }
    warnings
  }
}

/// The part and state of a per-state colour option such as
/// `ring-ver-color`.
fn state_color(name: &str) -> Option<(&'static str, InputState)> {
  let name = name.strip_suffix("-color")?;
  let (part, state) = match name.split_once('-') {
    Some((part, state)) => (part, Some(state)),
    None => (name, None),
  };
  let part = ["ring", "inside", "line", "text"]
    .into_iter()
    .find(|candidate| *candidate == part)?;
  let state = match state {
    None => InputState::Input,
    Some("clear") => InputState::Clear,
    Some("ver") => InputState::Verify,
    Some("wrong") => InputState::Wrong,
    Some(_) => return None,
  };
  Some((part, state))
}

fn parse_bool(text: &str) -> Option<bool> {
  match text.to_ascii_lowercase().as_str() {
    "true" | "yes" | "1" | "on" => Some(true),
    "false" | "no" | "0" | "off" => Some(false),
    _ => None,
  }
}

fn parse_number<T: std::str::FromStr>(
  name: &str,
  value: &str,
) -> Result<T, String> {
  value
    .parse()
    .map_err(|_| format!("{name} must be a number, not {value:?}"))
}

/// swaylock's colour: `rrggbb` or `rrggbbaa`, with an optional `#` or
/// `0x`.
///
/// # Errors
///
/// A message for anything else.
pub fn parse_lock_color(text: &str) -> Result<LockColor, String> {
  let text = text.trim();
  let hex = text
    .strip_prefix('#')
    .or_else(|| text.strip_prefix("0x"))
    .unwrap_or(text);
  let invalid = || format!("invalid colour {text:?}; expected rrggbb[aa]");
  if (hex.len() != 6 && hex.len() != 8)
    || !hex.chars().all(|c| c.is_ascii_hexdigit())
  {
    return Err(invalid());
  }
  let mut color = [0xff; 4];
  for (index, byte) in color.iter_mut().enumerate().take(hex.len() / 2) {
    *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
      .map_err(|_| invalid())?;
  }
  Ok(color)
}

/// swaylock-effects' `effect-blur`: `RADIUSxTIMES`.
fn parse_blur(value: &str) -> Result<(u32, u32), String> {
  let invalid = || format!("effect-blur must be RADIUSxTIMES, not {value:?}");
  let (radius, times) = value.split_once('x').ok_or_else(invalid)?;
  let radius = radius.trim().parse::<u32>().map_err(|_| invalid())?;
  let times = times.trim().parse::<u32>().map_err(|_| invalid())?;
  if radius > 200 || times > 20 {
    return Err(format!("effect-blur {value} is too large"));
  }
  Ok((radius, times))
}

/// swaylock-effects' `effect-vignette`: `BASE:FACTOR`, each from 0 to 1.
fn parse_vignette(value: &str) -> Result<(f64, f64), String> {
  let invalid =
    || format!("effect-vignette must be BASE:FACTOR, not {value:?}");
  let (base, factor) = value.split_once(':').ok_or_else(invalid)?;
  let base = base.trim().parse::<f64>().map_err(|_| invalid())?;
  let factor = factor.trim().parse::<f64>().map_err(|_| invalid())?;
  if !base.is_finite() || !factor.is_finite() {
    return Err(invalid());
  }
  // swaylock-effects clamps base to 0..1 and factor to 0..1-base.
  let base = base.clamp(0.0, 1.0);
  Ok((base, factor.clamp(0.0, 1.0 - base)))
}

/// A value of the YAML `lock.options` map: a flag's state, a number or
/// text, all given to [`LockOptions::apply`] as text.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum LockValue {
  /// `true` sets a flag, `false` clears it.
  Flag(bool),
  /// A number, such as `grace: 5`.
  Number(f64),
  /// Anything else, such as `effect-blur: 7x5`.
  Text(String),
}

impl LockValue {
  /// The value as [`LockOptions::apply`] takes it.
  pub fn as_text(&self) -> String {
    match self {
      Self::Flag(flag) => flag.to_string(),
      Self::Number(number) => number.to_string(),
      Self::Text(text) => text.clone(),
    }
  }
}

/// The slow movement of the blurred background (docs/design/lock-screen.md).
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DriftConfig {
  /// Whether the background moves at all; default true. It only moves
  /// with a screenshot.
  pub enabled: bool,
  /// Frames per second while it moves; default 8, at most 30.
  pub fps: f64,
  /// How far it moves from its rest position, in logical pixels;
  /// default 12.
  pub amplitude: f64,
  /// How long one cycle takes, in seconds; default 30.
  pub period: f64,
}

impl Default for DriftConfig {
  fn default() -> Self {
    Self {
      enabled: true,
      fps: 8.0,
      amplitude: 12.0,
      period: 30.0,
    }
  }
}

/// The `lock` section: what Shingle runs to lock, and the lock screen's
/// options on top of the swaylock config. Wayfire and i3 files keep the
/// defaults.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LockConfig {
  /// The program the lock binding and the lock timeout run, without
  /// arguments; default `shingle-lock`. The `SHINGLE_LOCK` environment
  /// variable overrides it.
  pub command: String,
  /// Whether `shingle-lock` reads the swaylock config first; default
  /// true.
  pub read_swaylock_config: bool,
  /// The swaylock config to read; default the one swaylock reads,
  /// `$XDG_CONFIG_HOME/swaylock/config`, `~/.swaylock/config` or
  /// `/etc/swaylock/config`.
  pub swaylock_config: Option<PathBuf>,
  /// swaylock options by their long name, applied after the swaylock
  /// config and before the command line: `screenshot: true`,
  /// `effect-blur: 7x5`, `ring-color: 337d00`. `false` turns a flag off.
  pub options: BTreeMap<String, LockValue>,
  /// The background's movement.
  pub drift: DriftConfig,
  /// Seconds without a key before the password input fades out;
  /// default 5.
  pub input_timeout: f64,
}

impl Default for LockConfig {
  fn default() -> Self {
    Self {
      command: "shingle-lock".into(),
      read_swaylock_config: true,
      swaylock_config: None,
      options: BTreeMap::new(),
      drift: DriftConfig::default(),
      input_timeout: 5.0,
    }
  }
}

impl LockConfig {
  /// Applies [`LockConfig::options`] to `options`.
  ///
  /// # Errors
  ///
  /// [`ConfigError::Invalid`] naming the first option that is unknown,
  /// unsupported or malformed: YAML never warns.
  pub fn apply_to(&self, options: &mut LockOptions) -> Result<(), ConfigError> {
    for (name, value) in &self.options {
      let value = value.as_text();
      match options.apply(name, Some(&value)) {
        Ok(Applied::Set) => {}
        Ok(Applied::Unsupported) => {
          return Err(ConfigError::Invalid(format!(
            "lock.options.{name} is not supported by shingle-lock"
          )));
        }
        Ok(Applied::Unknown) => {
          return Err(ConfigError::Invalid(format!(
            "lock.options.{name} is not a swaylock option"
          )));
        }
        Err(error) => {
          return Err(ConfigError::Invalid(format!("lock.options: {error}")));
        }
      }
    }
    Ok(())
  }

  /// Checks the section: a command, every option valid, and drift and
  /// timeout values in range.
  ///
  /// # Errors
  ///
  /// [`ConfigError::Invalid`] naming the field.
  pub fn validate(&self) -> Result<(), ConfigError> {
    if self.command.trim().is_empty() {
      return Err(ConfigError::Invalid("lock.command cannot be empty".into()));
    }
    self.apply_to(&mut LockOptions::default())?;
    let drift = &self.drift;
    if !(drift.fps.is_finite() && drift.fps > 0.0 && drift.fps <= 30.0) {
      return Err(ConfigError::Invalid(
        "lock.drift.fps must be above 0 and at most 30".into(),
      ));
    }
    if !(drift.amplitude.is_finite()
      && (0.0..=200.0).contains(&drift.amplitude))
    {
      return Err(ConfigError::Invalid(
        "lock.drift.amplitude must be from 0 to 200 pixels".into(),
      ));
    }
    if !(drift.period.is_finite() && drift.period >= 1.0) {
      return Err(ConfigError::Invalid(
        "lock.drift.period must be at least 1 second".into(),
      ));
    }
    if !(self.input_timeout.is_finite() && self.input_timeout > 0.0) {
      return Err(ConfigError::Invalid(
        "lock.input_timeout must be above 0 seconds".into(),
      ));
    }
    Ok(())
  }
}

/// The swaylock config swaylock itself would read: the first that exists
/// of `$XDG_CONFIG_HOME/swaylock/config` (or `$HOME/.config/...`),
/// `$HOME/.swaylock/config` and `/etc/swaylock/config` (swaylock's
/// `main.c`, `get_config_path`). `var` looks up environment variables
/// and `exists` checks a path.
pub fn swaylock_config_path(
  var: impl Fn(&str) -> Option<String>,
  exists: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
  let home = var("HOME").filter(|home| !home.is_empty());
  let config_home = var("XDG_CONFIG_HOME")
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    .or_else(|| {
      home
        .as_ref()
        .map(|home| PathBuf::from(home).join(".config"))
    });
  config_home
    .map(|path| path.join("swaylock/config"))
    .into_iter()
    .chain(home.map(|home| PathBuf::from(home).join(".swaylock/config")))
    .chain([PathBuf::from("/etc/swaylock/config")])
    .find(|path| exists(path))
}

/// `command` with every bare `swaylock` word, the program as the shell
/// or swayidle would run it, replaced by `shingle-lock`, which takes the
/// same options; `None` when it has none. `/usr/bin/swaylock` and other
/// paths are kept, which is how a Wayfire config keeps swaylock
/// (docs/design/lock-screen.md).
pub fn swap_swaylock(command: &str) -> Option<String> {
  const WORD: &str = "swaylock";
  let before = |c: char| c.is_whitespace() || "'\";&|(".contains(c);
  let after = |c: char| c.is_whitespace() || "'\";&|)".contains(c);
  let mut out = String::with_capacity(command.len() + 8);
  let mut rest = command;
  let mut previous: Option<char> = None;
  let mut swapped = false;
  while !rest.is_empty() {
    if rest.starts_with(WORD)
      && previous.is_none_or(before)
      && rest[WORD.len()..].chars().next().is_none_or(after)
    {
      out.push_str("shingle-lock");
      rest = &rest[WORD.len()..];
      previous = Some('k');
      swapped = true;
      continue;
    }
    let c = rest.chars().next().expect("not empty");
    out.push(c);
    previous = Some(c);
    rest = &rest[c.len_utf8()..];
  }
  swapped.then_some(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bare_swaylock_commands_become_shingle_lock() {
    assert_eq!(
      swap_swaylock("swaylock -f -c 000000").as_deref(),
      Some("shingle-lock -f -c 000000")
    );
    assert_eq!(
      swap_swaylock(
        "swayidle -w timeout 300 'swaylock -f' before-sleep \"swaylock\""
      )
      .as_deref(),
      Some(
        "swayidle -w timeout 300 'shingle-lock -f' before-sleep \
         \"shingle-lock\""
      )
    );
    assert_eq!(swap_swaylock("/usr/bin/swaylock -f"), None);
    assert_eq!(swap_swaylock("swaylock-fancy"), None);
    assert_eq!(swap_swaylock("foot"), None);
    assert_eq!(
      swap_swaylock("pkill -0 x;swaylock").as_deref(),
      Some("pkill -0 x;shingle-lock")
    );
  }

  #[test]
  fn a_swaylock_config_carries_over() {
    let mut options = LockOptions::default();
    let warnings = options.apply_file(
      "# invented values\n\
       daemonize\n\
       show-failed-attempts\n\
       clock\n\
       screenshot\n\
       effect-blur=9x4\n\
       effect-vignette=0.4:0.3\n\
       color=102030\n\
       font=Invented Sans\n\
       font-size=21\n\
       indicator\n\
       indicator-radius=70\n\
       indicator-thickness=6\n\
       grace=2.5\n\
       grace-no-mouse\n\
       grace-no-touch\n\
       datestr=%d.%m.\n\
       timestr=%H:%M\n\
       ignore-empty-password\n",
    );
    assert_eq!(warnings, []);
    assert!(options.daemonize && options.show_failed_attempts);
    assert!(options.clock && options.screenshot && options.indicator_always);
    assert_eq!(options.blur, Some((9, 4)));
    assert_eq!(options.vignette, Some((0.4, 0.3)));
    assert_eq!(options.color, [0x10, 0x20, 0x30, 0xff]);
    assert_eq!(options.font, "Invented Sans");
    assert_eq!(options.font_size, Some(21));
    assert_eq!(
      (options.indicator_radius, options.indicator_thickness),
      (70, 6)
    );
    assert_eq!(options.grace, Duration::from_millis(2500));
    assert!(options.grace_no_mouse && options.grace_no_touch);
    assert_eq!(
      (options.datestr.as_str(), options.timestr.as_str()),
      ("%d.%m.", "%H:%M")
    );
    assert!(options.ignore_empty_password);
  }

  #[test]
  fn every_indicator_colour_lands_in_its_state() {
    let mut options = LockOptions::default();
    let mut text = String::new();
    let parts = ["ring", "inside", "line", "text"];
    let states = ["", "-clear", "-ver", "-wrong"];
    for (p, part) in parts.iter().enumerate() {
      for (s, state) in states.iter().enumerate() {
        text.push_str(&format!("{part}{state}-color={p:02x}{s:02x}00\n"));
      }
    }
    text.push_str("key-hl-color=aabbcc\nbs-hl-color=#ddeeff80\n");
    text.push_str("separator-color=0x123456\n");
    assert_eq!(options.apply_file(&text), []);
    let colors = options.colors;
    for (s, state) in [
      InputState::Input,
      InputState::Clear,
      InputState::Verify,
      InputState::Wrong,
    ]
    .into_iter()
    .enumerate()
    {
      let of = colors.of(state);
      let s = s as u8;
      assert_eq!(of.ring, [0, s, 0, 0xff], "{state:?}");
      assert_eq!(of.inside, [1, s, 0, 0xff]);
      assert_eq!(of.line, [2, s, 0, 0xff]);
      assert_eq!(of.text, [3, s, 0, 0xff]);
    }
    assert_eq!(colors.key_highlight, [0xaa, 0xbb, 0xcc, 0xff]);
    assert_eq!(colors.backspace_highlight, [0xdd, 0xee, 0xff, 0x80]);
    assert_eq!(colors.separator, [0x12, 0x34, 0x56, 0xff]);
    options.apply("line-uses-ring", None).unwrap();
    assert_eq!(
      options.state_colors(InputState::Wrong).line,
      [0, 3, 0, 0xff]
    );
  }

  #[test]
  fn unknown_unsupported_and_malformed_options_warn_and_are_ignored() {
    let mut options = LockOptions::default();
    let warnings = options.apply_file(
      "image=/tmp/invented.png\n\
       no-such-option\n\
       effect-blur=lots\n\
       color=nothex\n\
       clock=maybe\n\
       ring-color\n",
    );
    let lines = warnings.iter().map(|(line, _)| *line).collect::<Vec<_>>();
    assert_eq!(lines, [1, 2, 3, 4, 5, 6]);
    assert!(warnings[0].1.contains("not supported"), "{warnings:?}");
    assert!(warnings[1].1.contains("unknown option"));
    assert_eq!(options, LockOptions::default());
  }

  #[test]
  fn the_command_line_takes_long_short_and_bundled_options() {
    let mut options = LockOptions::default();
    let arguments = [
      "-fF",
      "--color",
      "abcdef",
      "--effect-blur=3x2",
      "-C",
      "/invented/config",
      "--clock",
      "-e",
      "-u",
      "--image",
      "/invented.png",
      "-Z",
      "stray",
    ]
    .map(String::from);
    let warnings = options.apply_arguments(&arguments);
    assert!(options.daemonize && options.show_failed_attempts);
    assert_eq!(options.color, [0xab, 0xcd, 0xef, 0xff]);
    assert_eq!(options.blur, Some((3, 2)));
    assert!(options.clock && options.ignore_empty_password);
    assert!(!options.show_indicator);
    assert_eq!(warnings.len(), 3, "{warnings:?}");
  }

  #[test]
  fn the_yaml_options_override_and_validate() {
    let config: LockConfig = serde_yaml::from_str(
      "options:\n  screenshot: true\n  effect-blur: 4x2\n  grace: 3\n  \
       clock: false\ndrift:\n  fps: 4\n",
    )
    .unwrap();
    config.validate().unwrap();
    let mut options = LockOptions {
      clock: true,
      ..LockOptions::default()
    };
    config.apply_to(&mut options).unwrap();
    assert!(options.screenshot && !options.clock);
    assert_eq!(options.blur, Some((4, 2)));
    assert_eq!(options.grace, Duration::from_secs(3));
    assert_eq!(config.drift.fps, 4.0);
    for bad in [
      "options:\n  no-such: 1\n",
      "options:\n  image: x\n",
      "options:\n  effect-blur: big\n",
      "drift:\n  fps: 0\n",
      "command: ''\n",
    ] {
      let config: LockConfig = serde_yaml::from_str(bad).unwrap();
      assert!(config.validate().is_err(), "{bad}");
    }
    assert!(serde_yaml::from_str::<LockConfig>("colour: red\n").is_err());
  }

  #[test]
  fn the_swaylock_config_is_found_where_swaylock_looks() {
    let var = |name: &str| match name {
      "HOME" => Some("/home/invented".to_owned()),
      _ => None,
    };
    let found =
      swaylock_config_path(var, |path| path.ends_with(".swaylock/config"));
    assert_eq!(
      found,
      Some(PathBuf::from("/home/invented/.swaylock/config"))
    );
    let found = swaylock_config_path(var, |_| true);
    assert_eq!(
      found,
      Some(PathBuf::from("/home/invented/.config/swaylock/config"))
    );
    assert_eq!(swaylock_config_path(var, |_| false), None);
  }
}
