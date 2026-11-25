use std::str::FromStr;

use shingle_core::Rect;

use crate::{ConfigError, ViewMatcher, ViewProperties};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct WindowRule {
  text: String,
  event: RuleEvent,
  matcher: ViewMatcher,
  pub action: RuleAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleEvent {
  Created,
  Maximized,
  Unmaximized,
  Minimized,
  Fullscreened,
}

#[derive(Clone, Debug)]
pub enum RuleAction {
  Maximize(bool),
  Minimize(bool),
  Snap(String),
  Alpha(f32),
  Sticky,
  AlwaysOnTop,
  AlwaysOnBottom,
  AssignWorkspace {
    x: u32,
    y: u32,
  },
  SetGeometry(Rect),
  SetGeometryPercent {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
  },
  SetGeometryX11 {
    width: Option<u32>,
    height: Option<u32>,
    x: Option<i32>,
    y: Option<i32>,
    x_from_right: bool,
    y_from_bottom: bool,
  },
  Move {
    x: i32,
    y: i32,
  },
  Resize {
    width: u32,
    height: u32,
  },
}

impl WindowRule {
  pub fn applies_to(&self, event: RuleEvent) -> bool {
    self.event == event
  }

  pub fn matches(&self, app_id: &str, title: Option<&str>) -> bool {
    self.matcher.matches(app_id, title)
  }

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
