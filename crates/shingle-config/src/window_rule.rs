use std::str::FromStr;

use shingle_core::Rect;

use crate::{ConfigError, ViewMatcher, ViewProperties};

/// One rule in Wayfire's `window-rules` syntax: `on EVENT if MATCHER then
/// ACTION`.
///
/// `EVENT` is a [`RuleEvent`], `MATCHER` a [`ViewMatcher`] expression and
/// `ACTION` a [`RuleAction`]. Double quotes group a value containing spaces or
/// the operator characters `()&|!`, with `\` escaping inside quotes. The rule
/// serializes back to the exact text it was parsed from. Behaviour follows
/// Wayfire's `plugins/window-rules/window-rules.cpp`; actions are applied in
/// `crates/shingle/src/window_rules.rs`.
///
/// Parsing fails with [`ConfigError::Invalid`] for an unknown event or action,
/// wrong argument counts, unparsable numbers, a zero `resize` or `set geometry`
/// size, a non-finite alpha or an unterminated quote.
/// [`ShingleConfig::validate`](crate::ShingleConfig::validate) additionally
/// rejects `assign_workspace` outside the workspace grid.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct WindowRule {
  text: String,
  event: RuleEvent,
  matcher: ViewMatcher,
  /// What the rule does when its event fires and its matcher fits.
  pub action: RuleAction,
}

/// The view event a [`WindowRule`] reacts to, the word after `on`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleEvent {
  /// `created`: the window has just joined the layout, after `tile_by_default`
  /// and placement have been applied.
  Created,
  /// `maximized`: the view became maximized.
  Maximized,
  /// `unmaximized`: the view left the maximized state.
  Unmaximized,
  /// `minimized`: the view was minimized.
  Minimized,
  /// `fullscreened`: the view entered fullscreen.
  Fullscreened,
}

/// What a [`WindowRule`] does, the words after `then`.
///
/// Actions that fail, for example because the view is gone, are skipped
/// silently, and events caused by an action do not trigger further rules.
/// Geometry is in logical pixels, relative to the output unless stated
/// otherwise.
#[derive(Clone, Debug)]
pub enum RuleAction {
  /// `maximize` (true) or `unmaximize` (false).
  Maximize(bool),
  /// `minimize` (true) or `unminimize` (false).
  Minimize(bool),
  /// `snap SLOT`: snap to a grid slot, one of `top`, `top_right`, `right`,
  /// `bottom_right`, `bottom`, `bottom_left`, `left`, `top_left` or `center`
  /// (validated at parse time).
  Snap(String),
  /// `set alpha A`: window opacity. Any finite value parses; it is clamped to
  /// 0–1 when applied.
  Alpha(f32),
  /// `set sticky`: show the view on every workspace.
  Sticky,
  /// `set always_on_top`.
  AlwaysOnTop,
  /// `set always_on_bottom`.
  AlwaysOnBottom,
  /// `assign_workspace X Y`: move the view to the workspace at column `x`, row
  /// `y` of the grid.
  AssignWorkspace {
    /// Zero-based workspace column.
    x: u32,
    /// Zero-based workspace row.
    y: u32,
  },
  /// `set geometry X Y W H`: float the view at this rectangle in output
  /// coordinates. Width and height must be nonzero.
  SetGeometry(Rect),
  /// `set geometry_ppt X Y W H`: float the view at a rectangle given in percent
  /// of the output size, measured from the output's origin. Values are clamped
  /// to 0–100 when applied.
  SetGeometryPercent {
    /// Left edge, percent of the output width.
    x: i32,
    /// Top edge, percent of the output height.
    y: i32,
    /// Width, percent of the output width.
    width: i32,
    /// Height, percent of the output height.
    height: i32,
  },
  /// `set geometry SPEC` with an X11 geometry string `[W][xH][{+-}X[{+-}Y]]`,
  /// such as `800x600+10-20`. Omitted parts keep the view's current floating
  /// geometry; offsets are relative to the workarea of the current workspace.
  SetGeometryX11 {
    /// Width, if given; never zero.
    width: Option<u32>,
    /// Height, if given; never zero.
    height: Option<u32>,
    /// Horizontal offset, if given; always non-negative, with the side given by
    /// `x_from_right`.
    x: Option<i32>,
    /// Vertical offset, if given; always non-negative, with the side given by
    /// `y_from_bottom`.
    y: Option<i32>,
    /// `x` was written with `-`: it is the distance from the workarea's right
    /// edge to the view's right edge, clamped so the view does not start left
    /// of the workarea.
    x_from_right: bool,
    /// `y` was written with `-`: it is the distance from the workarea's bottom
    /// edge to the view's bottom edge, clamped likewise.
    y_from_bottom: bool,
  },
  /// `move X Y`: float the view at this position, keeping its size.
  Move {
    /// New left edge in output coordinates.
    x: i32,
    /// New top edge in output coordinates.
    y: i32,
  },
  /// `resize W H`: float the view at this size, keeping its position. Both must
  /// be nonzero.
  Resize {
    /// New width.
    width: u32,
    /// New height.
    height: u32,
  },
}

impl WindowRule {
  /// Whether this rule reacts to `event`.
  pub fn applies_to(&self, event: RuleEvent) -> bool {
    self.event == event
  }

  /// Matches the rule's [`ViewMatcher`] against a view known only by app ID and
  /// title; see [`ViewMatcher::matches`].
  pub fn matches(&self, app_id: &str, title: Option<&str>) -> bool {
    self.matcher.matches(app_id, title)
  }

  /// Matches the rule's [`ViewMatcher`] against a view's full state.
  pub fn matches_view(&self, view: &ViewProperties<'_>) -> bool {
    self.matcher.matches_view(view)
  }
}

impl TryFrom<String> for WindowRule {
  type Error = ConfigError;

  fn try_from(text: String) -> Result<Self, Self::Error> {
    let tokens = tokenize(&text)?;
    let invalid =
      || ConfigError::Invalid(format!("invalid window rule {text:?}"));
    if tokens.len() < 6 || tokens[0] != "on" || tokens[2] != "if" {
      return Err(invalid());
    }
    let event = match tokens[1].as_str() {
      "created" => RuleEvent::Created,
      "maximized" => RuleEvent::Maximized,
      "unmaximized" => RuleEvent::Unmaximized,
      "minimized" => RuleEvent::Minimized,
      "fullscreened" => RuleEvent::Fullscreened,
      _ => return Err(invalid()),
    };
    let then = tokens
      .iter()
      .position(|token| token == "then")
      .ok_or_else(invalid)?;
    let matcher = ViewMatcher::from_tokens(&tokens[3..then])?;
    let action = parse_action(&tokens[then + 1..]).ok_or_else(invalid)?;
    Ok(Self {
      text,
      event,
      matcher,
      action,
    })
  }
}

impl From<WindowRule> for String {
  fn from(rule: WindowRule) -> Self {
    rule.text
  }
}

fn parse_action(tokens: &[String]) -> Option<RuleAction> {
  match tokens.first()?.as_str() {
    "maximize" if tokens.len() == 1 => Some(RuleAction::Maximize(true)),
    "unmaximize" if tokens.len() == 1 => Some(RuleAction::Maximize(false)),
    "minimize" if tokens.len() == 1 => Some(RuleAction::Minimize(true)),
    "unminimize" if tokens.len() == 1 => Some(RuleAction::Minimize(false)),
    "snap"
      if tokens.len() == 2
        && matches!(
          tokens[1].as_str(),
          "top"
            | "top_right"
            | "right"
            | "bottom_right"
            | "bottom"
            | "bottom_left"
            | "left"
            | "top_left"
            | "center"
        ) =>
    {
      Some(RuleAction::Snap(tokens[1].clone()))
    }
    "assign_workspace" if tokens.len() == 3 => {
      Some(RuleAction::AssignWorkspace {
        x: parse(&tokens[1])?,
        y: parse(&tokens[2])?,
      })
    }
    "move" if tokens.len() == 3 => Some(RuleAction::Move {
      x: parse(&tokens[1])?,
      y: parse(&tokens[2])?,
    }),
    "resize" if tokens.len() == 3 => {
      let width = parse(&tokens[1])?;
      let height = parse(&tokens[2])?;
      (width > 0 && height > 0).then_some(RuleAction::Resize { width, height })
    }
    "set" if tokens.len() == 3 && tokens[1] == "alpha" => {
      let alpha = parse(&tokens[2])?;
      f32::is_finite(alpha).then_some(RuleAction::Alpha(alpha))
    }
    "set" if tokens.len() == 2 => match tokens[1].as_str() {
      "sticky" => Some(RuleAction::Sticky),
      "always_on_top" => Some(RuleAction::AlwaysOnTop),
      "always_on_bottom" => Some(RuleAction::AlwaysOnBottom),
      _ => None,
    },
    "set" if tokens.len() == 3 && tokens[1] == "geometry" => {
      parse_x11_geometry(&tokens[2])
    }
    "set" if tokens.len() == 6 && tokens[1] == "geometry" => {
      let x = parse(&tokens[2])?;
      let y = parse(&tokens[3])?;
      let width = parse(&tokens[4])?;
      let height = parse(&tokens[5])?;
      (width > 0 && height > 0)
        .then_some(RuleAction::SetGeometry(Rect::new(x, y, width, height)))
    }
    "set" if tokens.len() == 6 && tokens[1] == "geometry_ppt" => {
      Some(RuleAction::SetGeometryPercent {
        x: parse(&tokens[2])?,
        y: parse(&tokens[3])?,
        width: parse(&tokens[4])?,
        height: parse(&tokens[5])?,
      })
    }
    _ => None,
  }
}

fn parse_x11_geometry(text: &str) -> Option<RuleAction> {
  if text.is_empty() {
    return None;
  }
  let mut width = None;
  let mut height = None;
  let mut x = None;
  let mut y = None;
  let mut x_from_right = false;
  let mut y_from_bottom = false;
  let mut rest = text;
  if !rest.starts_with(['x', '+', '-']) {
    let end = rest.find(['x', '+', '-']).unwrap_or(rest.len());
    let parsed = parse(&rest[..end])?;
    if parsed == 0 {
      return None;
    }
    width = Some(parsed);
    rest = &rest[end..];
  }
  if let Some(after_x) = rest.strip_prefix('x') {
    let end = after_x.find(['+', '-']).unwrap_or(after_x.len());
    let parsed = parse(&after_x[..end])?;
    if parsed == 0 {
      return None;
    }
    height = Some(parsed);
    rest = &after_x[end..];
  }
  if !rest.is_empty() {
    let y_separator = rest[1..].find(['+', '-']).map(|index| index + 1);
    let (x_text, y_text) =
      y_separator.map_or((rest, ""), |index| (&rest[..index], &rest[index..]));
    let (x_value, negative) = parse_x11_offset(x_text)?;
    x = Some(x_value);
    x_from_right = negative;
    if !y_text.is_empty() {
      let (y_value, negative) = parse_x11_offset(y_text)?;
      y = Some(y_value);
      y_from_bottom = negative;
    }
  }
  if width.is_none() && height.is_none() && x.is_none() && y.is_none() {
    return None;
  }
  Some(RuleAction::SetGeometryX11 {
    width,
    height,
    x,
    y,
    x_from_right,
    y_from_bottom,
  })
}

fn parse_x11_offset(text: &str) -> Option<(i32, bool)> {
  let (sign, value) = text.split_at_checked(1)?;
  let negative = match sign {
    "+" => false,
    "-" => true,
    _ => return None,
  };
  let value: u32 = parse(value)?;
  Some((i32::try_from(value).ok()?, negative))
}

fn parse<T: FromStr>(token: &str) -> Option<T> {
  token.parse().ok()
}

fn tokenize(text: &str) -> Result<Vec<String>, ConfigError> {
  let mut tokens = Vec::new();
  let mut token = String::new();
  let mut started = false;
  let mut quoted = false;
  let mut escaped = false;
  fn flush(tokens: &mut Vec<String>, token: &mut String, started: &mut bool) {
    if *started {
      tokens.push(std::mem::take(token));
      *started = false;
    }
  }
  for character in text.chars() {
    if escaped {
      token.push(character);
      escaped = false;
    } else if character == '\\' && quoted {
      escaped = true;
    } else if character == '"' {
      quoted = !quoted;
      started = true;
    } else if !quoted
      && (character.is_whitespace() || "()&|!".contains(character))
    {
      flush(&mut tokens, &mut token, &mut started);
      if !character.is_whitespace() {
        tokens.push(character.to_string());
      }
    } else {
      token.push(character);
      started = true;
    }
  }
  if quoted || escaped {
    return Err(ConfigError::Invalid(format!(
      "unterminated quoted value in window rule {text:?}"
    )));
  }
  flush(&mut tokens, &mut token, &mut started);
  Ok(tokens)
}
