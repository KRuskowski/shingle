//! How the launcher looks (docs/design/launcher.md, section 8).

use shingle_config::LauncherConfig;

/// An RGBA colour with components from 0 to 1.
pub type Color = (f32, f32, f32, f32);

/// Sizes and colours of the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
  /// The panel's width in logical pixels.
  pub width: f32,
  /// The panel's height in logical pixels.
  pub height: f32,
  /// The size of entry names; the rest scales with it.
  pub font_size: f32,
  /// Over the rest of the output, premultiplied when drawn.
  pub backdrop: Color,
  /// The panel's background.
  pub panel: Color,
  /// The search field and hovered rows.
  pub field: Color,
  /// Names and typed text.
  pub text: Color,
  /// Hints, comments and counts.
  pub dim_text: Color,
  /// The caret, the field's border, favourite marks and drop targets.
  pub accent: Color,
  /// The selected row and category.
  pub selection: Color,
}

impl Default for Theme {
  /// A dark slate panel with a blue accent.
  fn default() -> Self {
    Self {
      width: 760.0,
      height: 480.0,
      font_size: 16.0,
      backdrop: (0.0, 0.0, 0.0, 0.45),
      panel: (0.11, 0.12, 0.15, 1.0),
      field: (0.16, 0.18, 0.22, 1.0),
      text: (0.92, 0.93, 0.95, 1.0),
      dim_text: (0.58, 0.62, 0.68, 1.0),
      accent: (0.33, 0.58, 0.93, 1.0),
      selection: (0.2, 0.31, 0.48, 1.0),
    }
  }
}

impl Theme {
  /// The theme of Shingle's `launcher` section.
  pub fn from_config(config: &LauncherConfig) -> Self {
    let color = |value: [f64; 4]| {
      (
        value[0] as f32,
        value[1] as f32,
        value[2] as f32,
        value[3] as f32,
      )
    };
    let colors = &config.colors;
    Self {
      width: config.width as f32,
      height: config.height as f32,
      font_size: config.font_size as f32,
      backdrop: color(colors.backdrop),
      panel: color(colors.panel),
      field: color(colors.field),
      text: color(colors.text),
      dim_text: color(colors.dim_text),
      accent: color(colors.accent),
      selection: color(colors.selection),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_default_config_is_the_default_theme() {
    assert_eq!(
      Theme::from_config(&LauncherConfig::default()),
      Theme::default()
    );
    let mut config = LauncherConfig {
      width: 1000,
      ..LauncherConfig::default()
    };
    config.colors.accent = [1.0, 0.0, 1.0, 1.0];
    let theme = Theme::from_config(&config);
    assert_eq!(theme.width, 1000.0);
    assert_eq!(theme.accent, (1.0, 0.0, 1.0, 1.0));
  }
}
