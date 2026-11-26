use crate::ConfigError;

/// A boolean expression over view properties in Wayfire's view-matcher syntax
/// (Wayfire's `src/core/matcher.cpp`), as used by `tile_by_default`,
/// `animate/enabled_for`, the shortcuts-inhibit lists and window rules.
///
/// Grammar, loosest first: `A | B`, `A & B`, `!A`, `( A )`, and the atoms
/// `all`, `none`, `FIELD OP VALUE` and `PREDICATE [is|equals true|false]`.
/// `FIELD` is `app_id`, `title`, `role` or `type`; `OP` is `is` or `equals`
/// (exact, case-sensitive) or `contains` (substring). `role` is always
/// `TOPLEVEL`. `type` takes Wayfire's values (wayfire:src/core/
/// view-access-interface.cpp): `toplevel`, `x-or` for override-redirect X
/// windows, `unmanaged`, `background`, `panel` and `overlay` for layer-shell
/// surfaces, and `unknown`; the compositor says which applies to a view.
/// `PREDICATE` is one of `fullscreen`, `activated`, `minimized`, `focusable`,
/// `mapped`, `tiled-left`, `tiled-right`, `tiled-top`, `tiled-bottom`,
/// `maximized` or `floating`, compared with `true` or `false` as Wayfire
/// writes it (`focusable equals true`); a bare predicate, a Shingle
/// extension, tests for true. Values containing spaces or operator characters
/// must be double-quoted.
///
/// The matcher serializes back to the text it was parsed from. Parsing fails
/// with [`ConfigError::Invalid`] on unknown fields or operators, missing
/// operands, unbalanced parentheses, trailing tokens or an unterminated quote.
/// The default matches everything (`all`).
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ViewMatcher {
  text: String,
  expression: Expression,
}

#[derive(Clone, Debug)]
enum Expression {
  All,
  None,
  Not(Box<Self>),
  And(Box<Self>, Box<Self>),
  Or(Box<Self>, Box<Self>),
  Compare {
    field: Field,
    operator: Operator,
    value: String,
  },
  Predicate {
    field: PredicateField,
    expected: bool,
  },
}

#[derive(Clone, Copy, Debug)]
enum Field {
  AppId,
  Title,
  Role,
  Type,
}

#[derive(Clone, Copy, Debug)]
enum Operator {
  Is,
  Contains,
}

/// The view state a [`ViewMatcher`] is evaluated against. The compositor fills
/// it from the window's current state; the default is a mapped, focusable
/// `toplevel` with empty app ID and title and every other predicate false.
#[derive(Clone, Debug)]
pub struct ViewProperties<'a> {
  /// `xdg_toplevel` app ID.
  pub app_id: &'a str,
  /// `xdg_toplevel` title, empty if none was set.
  pub title: &'a str,
  /// Wayfire view type compared by `type`, such as `toplevel`.
  pub view_type: &'a str,
  /// The view is fullscreen.
  pub fullscreen: bool,
  /// The view is the focused view of its workspace.
  pub activated: bool,
  /// The view is minimized.
  pub minimized: bool,
  /// The view can take keyboard focus.
  pub focusable: bool,
  /// The view has a mapped buffer.
  pub mapped: bool,
  /// The view's left edge is constrained, as in `xdg_toplevel`'s `tiled_left`
  /// state. Shingle sets all four edges for any tiled view.
  pub tiled_left: bool,
  /// The view's right edge is constrained.
  pub tiled_right: bool,
  /// The view's top edge is constrained.
  pub tiled_top: bool,
  /// The view's bottom edge is constrained.
  pub tiled_bottom: bool,
  /// The view is maximized.
  pub maximized: bool,
  /// The view floats, that is it has floating geometry and is not maximized.
  pub floating: bool,
}

impl Default for ViewProperties<'_> {
  fn default() -> Self {
    Self {
      app_id: "",
      title: "",
      view_type: "toplevel",
      fullscreen: false,
      activated: false,
      minimized: false,
      focusable: true,
      mapped: true,
      tiled_left: false,
      tiled_right: false,
      tiled_top: false,
      tiled_bottom: false,
      maximized: false,
      floating: false,
    }
  }
}

#[derive(Clone, Copy, Debug)]
enum PredicateField {
  Fullscreen,
  Activated,
  Minimized,
  Focusable,
  Mapped,
  TiledLeft,
  TiledRight,
  TiledTop,
  TiledBottom,
  Maximized,
  Floating,
}

impl PredicateField {
  fn parse(text: &str) -> Option<Self> {
    Some(match text {
      "fullscreen" => Self::Fullscreen,
      "activated" => Self::Activated,
      "minimized" => Self::Minimized,
      "focusable" => Self::Focusable,
      "mapped" => Self::Mapped,
      "tiled-left" => Self::TiledLeft,
      "tiled-right" => Self::TiledRight,
      "tiled-top" => Self::TiledTop,
      "tiled-bottom" => Self::TiledBottom,
      "maximized" => Self::Maximized,
      "floating" => Self::Floating,
      _ => return None,
    })
  }

  fn value(self, view: &ViewProperties<'_>) -> bool {
    match self {
      Self::Fullscreen => view.fullscreen,
      Self::Activated => view.activated,
      Self::Minimized => view.minimized,
      Self::Focusable => view.focusable,
      Self::Mapped => view.mapped,
      Self::TiledLeft => view.tiled_left,
      Self::TiledRight => view.tiled_right,
      Self::TiledTop => view.tiled_top,
      Self::TiledBottom => view.tiled_bottom,
      Self::Maximized => view.maximized,
      Self::Floating => view.floating,
    }
  }
}

impl ViewMatcher {
  /// Evaluates the expression against `view`.
  pub fn matches_view(&self, view: &ViewProperties<'_>) -> bool {
    self.expression.matches(view)
  }

  /// The matcher's source text.
  pub fn as_str(&self) -> &str {
    &self.text
  }

  pub(crate) fn from_tokens(tokens: &[String]) -> Result<Self, ConfigError> {
    Self::parse(tokens.join(" "), tokens)
  }

  fn parse(text: String, tokens: &[String]) -> Result<Self, ConfigError> {
    let mut parser = Parser {
      tokens,
      position: 0,
      text: &text,
    };
    let expression = parser.parse_or()?;
    if parser.position != tokens.len() {
      return Err(parser.invalid());
    }
    Ok(Self { text, expression })
  }
}

impl Default for ViewMatcher {
  fn default() -> Self {
    Self {
      text: "all".into(),
      expression: Expression::All,
    }
  }
}

impl Expression {
  fn matches(&self, view: &ViewProperties<'_>) -> bool {
    match self {
      Self::All => true,
      Self::None => false,
      Self::Not(expression) => !expression.matches(view),
      Self::And(first, second) => first.matches(view) && second.matches(view),
      Self::Or(first, second) => first.matches(view) || second.matches(view),
      Self::Compare {
        field,
        operator,
        value,
      } => {
        let actual = match field {
          Field::AppId => view.app_id,
          Field::Title => view.title,
          Field::Role => "TOPLEVEL",
          Field::Type => view.view_type,
        };
        match operator {
          Operator::Is => actual == value,
          Operator::Contains => actual.contains(value),
        }
      }
      Self::Predicate { field, expected } => field.value(view) == *expected,
    }
  }
}

impl TryFrom<String> for ViewMatcher {
  type Error = ConfigError;

  fn try_from(text: String) -> Result<Self, Self::Error> {
    let tokens = tokenize(&text)?;
    Self::parse(text, &tokens)
  }
}

impl From<ViewMatcher> for String {
  fn from(matcher: ViewMatcher) -> Self {
    matcher.text
  }
}

struct Parser<'a> {
  tokens: &'a [String],
  position: usize,
  text: &'a str,
}

impl Parser<'_> {
  fn parse_or(&mut self) -> Result<Expression, ConfigError> {
    let mut expression = self.parse_and()?;
    while self.consume("|") {
      expression =
        Expression::Or(Box::new(expression), Box::new(self.parse_and()?));
    }
    Ok(expression)
  }

  fn parse_and(&mut self) -> Result<Expression, ConfigError> {
    let mut expression = self.parse_unary()?;
    while self.consume("&") {
      expression =
        Expression::And(Box::new(expression), Box::new(self.parse_unary()?));
    }
    Ok(expression)
  }

  fn parse_unary(&mut self) -> Result<Expression, ConfigError> {
    if self.consume("!") {
      return Ok(Expression::Not(Box::new(self.parse_unary()?)));
    }
    if self.consume("(") {
      let expression = self.parse_or()?;
      if !self.consume(")") {
        return Err(self.invalid());
      }
      return Ok(expression);
    }
    if self.consume("all") {
      return Ok(Expression::All);
    }
    if self.consume("none") {
      return Ok(Expression::None);
    }
    let Some(name) = self.next() else {
      return Err(self.invalid());
    };
    if let Some(field) = PredicateField::parse(name) {
      let expected = if self.consume("is") || self.consume("equals") {
        match self.next() {
          Some("true") => true,
          Some("false") => false,
          _ => return Err(self.invalid()),
        }
      } else {
        true
      };
      return Ok(Expression::Predicate { field, expected });
    }
    let field = match name {
      "app_id" => Field::AppId,
      "title" => Field::Title,
      "role" => Field::Role,
      "type" => Field::Type,
      _ => return Err(self.invalid()),
    };
    let operator = match self.next() {
      Some("is" | "equals") => Operator::Is,
      Some("contains") => Operator::Contains,
      _ => return Err(self.invalid()),
    };
    let Some(value) = self.next().map(str::to_owned) else {
      return Err(self.invalid());
    };
    Ok(Expression::Compare {
      field,
      operator,
      value,
    })
  }

  fn next(&mut self) -> Option<&str> {
    let token = self.tokens.get(self.position)?;
    self.position += 1;
    Some(token)
  }

  fn consume(&mut self, expected: &str) -> bool {
    if self
      .tokens
      .get(self.position)
      .is_some_and(|token| token == expected)
    {
      self.position += 1;
      true
    } else {
      false
    }
  }

  fn invalid(&self) -> ConfigError {
    ConfigError::Invalid(format!("invalid view matcher {:?}", self.text))
  }
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
      "unterminated quoted value in view matcher {text:?}"
    )));
  }
  flush(&mut tokens, &mut token, &mut started);
  Ok(tokens)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn matches(text: &str, view: &ViewProperties<'_>) -> bool {
    ViewMatcher::try_from(text.to_owned())
      .unwrap()
      .matches_view(view)
  }

  #[test]
  fn wayfire_default_enabled_for_parses_and_matches() {
    let text = concat!(
      "(type equals \"toplevel\" | ",
      "(type equals \"x-or\" & focusable equals true))"
    );
    assert!(matches(text, &ViewProperties::default()));
    let override_redirect = ViewProperties {
      view_type: "x-or",
      ..ViewProperties::default()
    };
    assert!(matches(text, &override_redirect));
    assert!(!matches(
      text,
      &ViewProperties {
        focusable: false,
        ..override_redirect
      }
    ));
    assert!(!matches(
      text,
      &ViewProperties {
        view_type: "panel",
        ..ViewProperties::default()
      }
    ));
  }

  #[test]
  fn predicates_compare_with_is_and_equals() {
    let fullscreen = ViewProperties {
      fullscreen: true,
      ..ViewProperties::default()
    };
    for text in ["fullscreen", "fullscreen is true", "fullscreen equals true"] {
      assert!(matches(text, &fullscreen), "{text}");
      assert!(!matches(text, &ViewProperties::default()), "{text}");
    }
    assert!(matches(
      "fullscreen equals false",
      &ViewProperties::default()
    ));
    for text in ["fullscreen equals", "fullscreen equals maybe"] {
      assert!(ViewMatcher::try_from(text.to_owned()).is_err(), "{text}");
    }
  }

  #[test]
  fn wayfire_view_types_are_compared() {
    for kind in ["x-or", "unmanaged", "background", "panel", "overlay"] {
      let view = ViewProperties {
        view_type: kind,
        ..ViewProperties::default()
      };
      assert!(matches(&format!("type is \"{kind}\""), &view), "{kind}");
      assert!(!matches("type is \"toplevel\"", &view), "{kind}");
    }
  }

  #[test]
  fn equals_uses_actual_view_type() {
    let matcher =
      ViewMatcher::try_from("type equals \"toplevel\"".to_owned()).unwrap();
    assert!(matcher.matches_view(&ViewProperties::default()));
    assert!(!matcher.matches_view(&ViewProperties {
      view_type: "popup",
      ..ViewProperties::default()
    }));
  }
}
