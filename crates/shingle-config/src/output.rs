//! Output configuration: Wayfire's `[output:NAME]` sections.
//!
//! [`OutputConfig`] holds one output's mode, position, scale, transform and
//! VRR switch. [`crate::ShingleConfig::output`] is the default for every
//! output, and [`crate::ShingleConfig::outputs`] holds one entry per named
//! output, as Wayfire keeps one section per connector.
//! [`crate::ShingleConfig::output_config`] picks the entry for a name.
//!
//! [`OutputMode`] parses and prints the `mode` option exactly as Wayfire's
//! wf-config does (`wf::option_type::from_string<output_config::mode_t>`,
//! wf-config/src/types.cpp).

use std::{fmt, str::FromStr};

use serde::Deserialize;
use shingle_core::Rect;

use crate::ConfigError;

/// How an output's mode is chosen: Wayfire's `[output:NAME] mode`.
///
/// Written as Wayfire writes it: `auto` (or `default`), `off`, `highres`,
/// `highrr`, `WIDTHxHEIGHT` or `WIDTHxHEIGHT@REFRESH`. A refresh below 1000
/// is in hertz and anything else in millihertz, so `@60` and `@60000` are
/// the same; Wayfire's parser takes whole numbers only, so `@59.94` is
/// invalid and `@59940` is the way to write it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputMode {
  /// The display's preferred mode. Wayfire's default.
  #[default]
  Auto,
  /// The output is not used.
  Off,
  /// The mode with the largest area.
  HighRes,
  /// The largest mode among those with the highest refresh rate.
  HighRr,
  /// This size, at this refresh rate or the closest one the display offers
  /// at that size. A size the display does not offer falls back to
  /// [`OutputMode::Auto`], as in Wayfire.
  Explicit {
    /// Width in pixels.
    width: u32,
    /// Height in pixels.
    height: u32,
    /// Refresh rate in millihertz, or zero for the preferred or fastest
    /// mode of that size.
    refresh_mhz: u32,
  },
}

impl OutputMode {
  /// The explicit size, if this mode names one.
  pub fn size(self) -> Option<(u32, u32)> {
    match self {
      Self::Explicit { width, height, .. } => Some((width, height)),
      _ => None,
    }
  }

  /// Parses a sway or i3 `output NAME mode` value: `WIDTHxHEIGHT`, with an
  /// optional `@RATE` in hertz that may have decimals and a `Hz` suffix,
  /// such as `2560x1440@59.951Hz`.
  pub fn from_sway(text: &str) -> Option<Self> {
    let (size, rate) = match text.split_once('@') {
      Some((size, rate)) => (size, Some(rate)),
      None => (text, None),
    };
    let (width, height) = size.split_once('x')?;
    let width: u32 = width.parse().ok()?;
    let height: u32 = height.parse().ok()?;
    let refresh_mhz = match rate {
      None => 0,
      Some(rate) => {
        let hertz: f64 = rate
          .strip_suffix("Hz")
          .or_else(|| rate.strip_suffix("hz"))
          .unwrap_or(rate)
          .parse()
          .ok()?;
        if !hertz.is_finite() || !(0.0..1_000_000.0).contains(&hertz) {
          return None;
        }
        (hertz * 1000.0).round() as u32
      }
    };
    let slow = refresh_mhz != 0 && refresh_mhz < 1000;
    (width > 0 && height > 0 && !slow).then_some(Self::Explicit {
      width,
      height,
      refresh_mhz,
    })
  }
}

/// `sscanf`'s `%d`: optional space, an optional sign and digits.
fn scan_int(text: &str) -> Option<(i64, &str)> {
  let text = text.trim_start();
  let digits_start = usize::from(text.starts_with(['+', '-']));
  let end = text[digits_start..]
    .find(|character: char| !character.is_ascii_digit())
    .map_or(text.len(), |end| end + digits_start);
  if end == digits_start {
    return None;
  }
  Some((text[..end].parse().ok()?, &text[end..]))
}

/// Wayfire's `sscanf(string, "%d x %d @ %d%c", ...)`: how many of the
/// three numbers were read, and whether something followed the refresh.
fn scan_resolution(text: &str) -> (Vec<i64>, bool) {
  let mut read = Vec::new();
  let Some((width, rest)) = scan_int(text) else {
    return (read, false);
  };
  read.push(width);
  let Some(rest) = rest.trim_start().strip_prefix('x') else {
    return (read, false);
  };
  let Some((height, rest)) = scan_int(rest) else {
    return (read, false);
  };
  read.push(height);
  let Some(rest) = rest.trim_start().strip_prefix('@') else {
    return (read, false);
  };
  let Some((refresh, rest)) = scan_int(rest) else {
    return (read, false);
  };
  read.push(refresh);
  (read, !rest.is_empty())
}

impl FromStr for OutputMode {
  type Err = ConfigError;

  /// Parses a mode as wf-config does. Like Wayfire, text after the height
  /// that does not start with `@` is ignored, and anything after the
  /// refresh rate makes the value invalid. Unlike Wayfire, a zero width or
  /// height is refused, and so is `mirror`, which Shingle does not support;
  /// the Wayfire importer turns `mirror` into [`OutputMode::Off`].
  fn from_str(text: &str) -> Result<Self, Self::Err> {
    let invalid =
      || ConfigError::Invalid(format!("invalid output mode {text:?}"));
    match text {
      "off" => return Ok(Self::Off),
      "auto" | "default" => return Ok(Self::Auto),
      "highres" => return Ok(Self::HighRes),
      "highrr" => return Ok(Self::HighRr),
      _ => {}
    }
    let (read, trailing) = scan_resolution(text);
    if read.len() < 2 || trailing {
      return Err(invalid());
    }
    let number = |value: i64| u32::try_from(value).ok().filter(|v| *v > 0);
    let width = number(read[0]).ok_or_else(invalid)?;
    let height = number(read[1]).ok_or_else(invalid)?;
    let mut refresh = read
      .get(2)
      .map_or(Some(0), |refresh| u32::try_from(*refresh).ok())
      .filter(|refresh| i32::try_from(*refresh).is_ok())
      .ok_or_else(invalid)?;
    // Wayfire: "Ensure refresh rate in mHz".
    if refresh < 1000 {
      refresh *= 1000;
    }
    Ok(Self::Explicit {
      width,
      height,
      refresh_mhz: refresh,
    })
  }
}

impl fmt::Display for OutputMode {
  /// Writes the mode so that [`OutputMode::from_str`] reads it back: the
  /// refresh in hertz when it is a whole number of hertz, else in
  /// millihertz.
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Auto => formatter.write_str("auto"),
      Self::Off => formatter.write_str("off"),
      Self::HighRes => formatter.write_str("highres"),
      Self::HighRr => formatter.write_str("highrr"),
      Self::Explicit {
        width,
        height,
        refresh_mhz: 0,
      } => write!(formatter, "{width}x{height}"),
      Self::Explicit {
        width,
        height,
        refresh_mhz,
      } if refresh_mhz % 1000 == 0 => {
        write!(formatter, "{width}x{height}@{}", refresh_mhz / 1000)
      }
      Self::Explicit {
        width,
        height,
        refresh_mhz,
      } => write!(formatter, "{width}x{height}@{refresh_mhz}"),
    }
  }
}

impl serde::Serialize for OutputMode {
  fn serialize<S: serde::Serializer>(
    &self,
    serializer: S,
  ) -> Result<S::Ok, S::Error> {
    serializer.collect_str(self)
  }
}

impl<'de> Deserialize<'de> for OutputMode {
  fn deserialize<D: serde::Deserializer<'de>>(
    deserializer: D,
  ) -> Result<Self, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.trim().parse().map_err(serde::de::Error::custom)
  }
}

/// One output's mode, position, scale, transform and VRR switch.
///
/// [`crate::ShingleConfig::output`] is the default for every output, and
/// also sets the size of the nested window and the headless output.
/// [`crate::ShingleConfig::outputs`] holds one entry per Wayfire
/// `[output:NAME]` section or i3 `output NAME` line, keyed by `name`.
///
/// Wayfire's `[output:NAME]` supplies `mode`, `position = X,Y`, `scale`,
/// `transform` and `vrr`; i3 supplies `output NAME mode WxH scale S
/// transform T pos X Y`.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
  /// The connector this entry is for, such as `DP-1` or `eDP-1`, or
  /// `HEADLESS-1` for the nested and headless output. Required in
  /// `outputs`, and not allowed on the default `output`.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  /// How the mode is chosen. `[output:NAME] mode`; default `auto`. On a
  /// native output this picks the KMS mode (`off` leaves the output dark);
  /// in nested and headless an explicit size replaces `width` and
  /// `height`, and its refresh rate is the one clients are told.
  pub mode: OutputMode,
  /// Horizontal position of the output's top-left corner in the global layout.
  /// `[output:NAME] position`; default 0.
  pub x: i32,
  /// Vertical position of the output's top-left corner in the global layout.
  /// Default 0.
  pub y: i32,
  /// Width of the nested window and the headless output in the untransformed
  /// orientation, before `scale` is applied, unless `mode` names a size.
  /// The compositor swaps width and height for 90° and 270° transforms. Must
  /// be nonzero; default 1280. A native output takes its size from its mode.
  pub width: u32,
  /// Height, like `width`. Must be nonzero; default 720.
  pub height: u32,
  /// Scale factor advertised to clients through `wl_output` and
  /// fractional-scale. `[output:NAME] scale`. Must be finite and positive;
  /// default 1.
  pub scale: f64,
  /// `wl_output` transform: `normal`, `90`, `180`, `270`, `flipped` or
  /// `90_flipped`, `180_flipped`, `270_flipped` as Wayfire spells them.
  /// `flipped-90` and `flipped90` style spellings are also accepted. Rotations
  /// are counter-clockwise, as in the Wayland protocol. Default `normal`.
  pub transform: String,
  /// Variable refresh rate (adaptive sync). `[output:NAME] vrr`; default
  /// false. Read now and applied by the native backend's VRR support
  /// (NB-10).
  pub vrr: bool,
}

impl Default for OutputConfig {
  fn default() -> Self {
    Self {
      name: None,
      mode: OutputMode::Auto,
      x: 0,
      y: 0,
      width: 1280,
      height: 720,
      scale: 1.0,
      transform: "normal".into(),
      vrr: false,
    }
  }
}

/// The transform spellings [`OutputConfig::transform`] accepts.
const TRANSFORMS: [&str; 14] = [
  "normal",
  "90",
  "180",
  "270",
  "flipped",
  "90_flipped",
  "180_flipped",
  "270_flipped",
  "flipped-90",
  "flipped-180",
  "flipped-270",
  "flipped90",
  "flipped180",
  "flipped270",
];

impl OutputConfig {
  /// A default entry for the output `name`.
  pub fn named(name: impl Into<String>) -> Self {
    Self {
      name: Some(name.into()),
      ..Self::default()
    }
  }

  /// The untransformed size: the explicit mode's size if `mode` names one,
  /// else `width` by `height`.
  pub fn size(&self) -> (u32, u32) {
    self.mode.size().unwrap_or((self.width, self.height))
  }

  /// The output's position and untransformed size as a layout rectangle.
  pub fn rect(&self) -> Rect {
    let (width, height) = self.size();
    Rect::new(self.x, self.y, width, height)
  }

  /// Checks the ranges documented on each field; `label` names the entry
  /// in the error.
  pub(crate) fn validate(&self, label: &str) -> Result<(), ConfigError> {
    if self.width == 0 || self.height == 0 {
      return Err(ConfigError::Invalid(format!(
        "{label} width and height must be nonzero"
      )));
    }
    if !self.scale.is_finite() || self.scale <= 0.0 {
      return Err(ConfigError::Invalid(format!(
        "{label}.scale must be finite and positive"
      )));
    }
    if !TRANSFORMS.contains(&self.transform.as_str()) {
      return Err(ConfigError::Invalid(format!(
        "unknown {label} transform {:?}",
        self.transform
      )));
    }
    Ok(())
  }
}

impl crate::ShingleConfig {
  /// The configuration of the output named `name`: its entry in
  /// [`crate::ShingleConfig::outputs`], else the default
  /// [`crate::ShingleConfig::output`].
  pub fn output_config(&self, name: &str) -> &OutputConfig {
    self
      .outputs
      .iter()
      .find(|output| output.name.as_deref() == Some(name))
      .unwrap_or(&self.output)
  }

  /// The entry for `name` in `outputs`, added with defaults if missing.
  pub(crate) fn named_output(&mut self, name: &str) -> &mut OutputConfig {
    let index = match self
      .outputs
      .iter()
      .position(|output| output.name.as_deref() == Some(name))
    {
      Some(index) => index,
      None => {
        self.outputs.push(OutputConfig::named(name));
        self.outputs.len() - 1
      }
    };
    &mut self.outputs[index]
  }

  /// Checks the default output and every named one: names are present,
  /// not empty and unique, and each entry is in range.
  pub(crate) fn validate_outputs(&self) -> Result<(), ConfigError> {
    if self.output.name.is_some() {
      return Err(ConfigError::Invalid(
        "output.name is not allowed; put named outputs in outputs".into(),
      ));
    }
    self.output.validate("output")?;
    for (index, output) in self.outputs.iter().enumerate() {
      let Some(name) = output.name.as_deref().filter(|name| !name.is_empty())
      else {
        return Err(ConfigError::Invalid(format!(
          "outputs[{index}] needs a name"
        )));
      };
      if self.outputs[..index]
        .iter()
        .any(|other| other.name.as_deref() == Some(name))
      {
        return Err(ConfigError::Invalid(format!(
          "output {name} is configured twice"
        )));
      }
      output.validate(&format!("output {name}"))?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConfigFormat, ShingleConfig};

  fn explicit(width: u32, height: u32, refresh_mhz: u32) -> OutputMode {
    OutputMode::Explicit {
      width,
      height,
      refresh_mhz,
    }
  }

  #[test]
  fn modes_parse_as_wf_config_does() {
    let parse = |text: &str| text.parse::<OutputMode>().ok();
    assert_eq!(parse("auto"), Some(OutputMode::Auto));
    assert_eq!(parse("default"), Some(OutputMode::Auto));
    assert_eq!(parse("off"), Some(OutputMode::Off));
    assert_eq!(parse("highres"), Some(OutputMode::HighRes));
    assert_eq!(parse("highrr"), Some(OutputMode::HighRr));
    assert_eq!(parse("1920x1080"), Some(explicit(1920, 1080, 0)));
    assert_eq!(
      parse("1920 x 1080 @ 144"),
      Some(explicit(1920, 1080, 144_000))
    );
    // Below 1000 is hertz, the rest millihertz.
    assert_eq!(parse("1920x1080@60"), Some(explicit(1920, 1080, 60_000)));
    assert_eq!(parse("1920x1080@59940"), Some(explicit(1920, 1080, 59_940)));
    // sscanf stops at a missing '@' or refresh and keeps what it read.
    assert_eq!(parse("1920x1080@"), Some(explicit(1920, 1080, 0)));
    assert_eq!(parse("1920x1080 junk"), Some(explicit(1920, 1080, 0)));
    // Something after the refresh, decimals included, is invalid.
    assert_eq!(parse("1920x1080@59.94"), None);
    assert_eq!(parse("1920x1080@60Hz"), None);
    assert_eq!(parse("1920"), None);
    assert_eq!(parse("1920x-1"), None);
    assert_eq!(parse("1920x1080@-60"), None);
    assert_eq!(parse("0x0"), None);
    assert_eq!(parse("Auto"), None);
    assert_eq!(parse("mirror eDP-1"), None);
  }

  #[test]
  fn modes_print_so_that_they_parse_back() {
    for mode in [
      OutputMode::Auto,
      OutputMode::Off,
      OutputMode::HighRes,
      OutputMode::HighRr,
      explicit(2560, 1440, 0),
      explicit(2560, 1440, 144_000),
      explicit(1920, 1080, 59_940),
    ] {
      assert_eq!(mode.to_string().parse::<OutputMode>().unwrap(), mode);
    }
    assert_eq!(explicit(1920, 1080, 60_000).to_string(), "1920x1080@60");
  }

  #[test]
  fn sway_modes_take_decimal_hertz() {
    assert_eq!(
      OutputMode::from_sway("2560x1440@59.951Hz"),
      Some(explicit(2560, 1440, 59_951))
    );
    assert_eq!(
      OutputMode::from_sway("1920x1080@60"),
      Some(explicit(1920, 1080, 60_000))
    );
    assert_eq!(
      OutputMode::from_sway("1920x1080"),
      Some(explicit(1920, 1080, 0))
    );
    assert_eq!(OutputMode::from_sway("1920x1080@fast"), None);
    assert_eq!(OutputMode::from_sway("big"), None);
  }

  #[test]
  fn an_explicit_mode_sets_the_size() {
    let mut output = OutputConfig::default();
    assert_eq!(output.size(), (1280, 720));
    output.mode = explicit(1920, 1080, 60_000);
    assert_eq!(output.rect(), Rect::new(0, 0, 1920, 1080));
    output.mode = OutputMode::HighRes;
    assert_eq!(output.size(), (1280, 720));
  }

  #[test]
  fn named_outputs_round_trip_through_yaml() {
    let config = ShingleConfig::parse(
      concat!(
        "output:\n",
        "  scale: 1.5\n",
        "outputs:\n",
        "  - name: DP-1\n",
        "    mode: 2560x1440@144\n",
        "    vrr: true\n",
        "  - name: HDMI-A-1\n",
        "    mode: \"off\"\n",
      ),
      ConfigFormat::Yaml,
    )
    .unwrap();
    assert_eq!(config.output.scale, 1.5);
    let dp = config.output_config("DP-1");
    assert_eq!(dp.mode, explicit(2560, 1440, 144_000));
    assert!(dp.vrr);
    assert_eq!(dp.scale, 1.0, "named entries do not inherit the default");
    assert_eq!(config.output_config("HDMI-A-1").mode, OutputMode::Off);
    assert_eq!(config.output_config("eDP-1"), &config.output);
    let yaml = config.to_yaml().unwrap();
    let again = ShingleConfig::parse(&yaml, ConfigFormat::Yaml).unwrap();
    assert_eq!(again.outputs, config.outputs);
  }

  #[test]
  fn output_entries_are_validated() {
    let error = |yaml: &str| {
      ShingleConfig::parse(yaml, ConfigFormat::Yaml)
        .unwrap_err()
        .to_string()
    };
    assert!(error("outputs:\n  - mode: auto\n").contains("needs a name"));
    assert!(
      error("outputs:\n  - name: DP-1\n  - name: DP-1\n")
        .contains("configured twice")
    );
    assert!(error("output:\n  name: DP-1\n").contains("not allowed"));
    assert!(
      error("outputs:\n  - name: DP-1\n    scale: 0\n").contains("DP-1.scale")
    );
    assert!(error("output:\n  mode: 1920x1080@59.94\n").contains("mode"));
    assert!(
      error("outputs:\n  - name: DP-1\n    transform: sideways\n")
        .contains("transform")
    );
  }

  #[test]
  fn each_wayfire_output_section_keeps_its_own_entry() {
    let config = ShingleConfig::parse(
      concat!(
        "[output:DP-1]\n",
        "mode = 2560x1440@144\n",
        "position = 0,0\n",
        "vrr = true\n",
        "[output:HDMI-A-1]\n",
        "mode = 1920x1080@60000\n",
        "position = 2560, 0\n",
        "scale = 1.5\n",
        "transform = 90\n",
        "[output:eDP-1]\n",
        "mode = mirror DP-1\n",
        "[output:DP-2]\n",
        "mode = highrr\n",
      ),
      ConfigFormat::Wayfire,
    )
    .unwrap();
    let names: Vec<_> = config
      .outputs
      .iter()
      .map(|output| output.name.as_deref().unwrap())
      .collect();
    assert_eq!(names, ["DP-1", "HDMI-A-1", "eDP-1", "DP-2"]);
    let dp = config.output_config("DP-1");
    assert_eq!(dp.mode, explicit(2560, 1440, 144_000));
    assert!(dp.vrr);
    let hdmi = config.output_config("HDMI-A-1");
    assert_eq!(hdmi.mode, explicit(1920, 1080, 60_000));
    assert_eq!((hdmi.x, hdmi.y, hdmi.scale), (2560, 0, 1.5));
    assert_eq!(hdmi.transform, "90");
    assert_eq!(config.output_config("eDP-1").mode, OutputMode::Off);
    assert_eq!(config.output_config("DP-2").mode, OutputMode::HighRr);
    assert_eq!(
      config.output,
      OutputConfig::default(),
      "the default is kept"
    );
  }

  #[test]
  fn an_unnamed_wayfire_output_section_is_the_default() {
    let config = ShingleConfig::parse(
      "[output]\nmode = 800x600\nscale = 2\n",
      ConfigFormat::Wayfire,
    )
    .unwrap();
    assert_eq!(config.output.size(), (800, 600));
    assert_eq!(config.output.scale, 2.0);
    assert!(config.outputs.is_empty());
    let error = ShingleConfig::parse(
      "[output:DP-1]\nmode = 1920x1080@59.94\n",
      ConfigFormat::Wayfire,
    )
    .unwrap_err();
    assert!(error.to_string().contains("line 2"), "{error}");
  }

  #[test]
  fn i3_output_lines_fill_named_entries() {
    let config = ShingleConfig::parse(
      concat!(
        "output * scale 2\n",
        "output DP-1 mode 2560x1440@59.951Hz pos 0 0 adaptive_sync on\n",
        "output HDMI-A-1 resolution --custom 1920x1080 transform 270\n",
        "output HDMI-A-1 position 2560 0\n",
        "output eDP-1 disable\n",
      ),
      ConfigFormat::I3,
    )
    .unwrap();
    assert_eq!(config.output.scale, 2.0);
    let dp = config.output_config("DP-1");
    assert_eq!(dp.mode, explicit(2560, 1440, 59_951));
    assert!(dp.vrr);
    let hdmi = config.output_config("HDMI-A-1");
    assert_eq!(hdmi.mode, explicit(1920, 1080, 0));
    assert_eq!((hdmi.x, hdmi.transform.as_str()), (2560, "270"));
    assert_eq!(config.output_config("eDP-1").mode, OutputMode::Off);
    assert!(
      ShingleConfig::parse("output DP-1 mode big\n", ConfigFormat::I3).is_err()
    );
  }
}
