//! Wayfire's activator syntax (wf-config:src/types.cpp), split into the
//! kinds of binding it can hold.
//!
//! An activator is alternatives separated by `|`. Each one is a key or
//! button with modifiers (`<super> <shift> KEY_Q`, `<alt> BTN_LEFT`), a
//! modifier alone (`<super>`), a touch gesture (`swipe up 3`, `edge-swipe
//! left-up 2`, `pinch in 4`), a hotspot (`hotspot top-left 10x10 500`) or
//! `none`/`disabled`. Shingle binds keys and buttons only; the other kinds
//! are recognised so that they can be reported and are never mistaken for
//! a key. wf-config keeps anything else as an "extension"; Shingle reports
//! it the same way.

use crate::{Binding, ConfigError, Modifier, wayfire::normalize_key};

/// One alternative of an activator.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Alternative {
  /// `none` or `disabled`.
  Disabled,
  /// A key or button Shingle can bind.
  Binding(Binding),
  /// Modifiers pressed and released alone, such as `<super>`.
  Modifiers(Vec<Modifier>),
  /// A touch gesture, as written.
  Gesture(String),
  /// A screen-edge hotspot, as written.
  Hotspot(String),
  /// Text that is none of the above.
  Unknown(String),
}

impl Alternative {
  /// Why Shingle cannot bind this alternative, or `None` if it can.
  pub(crate) fn unsupported(&self) -> Option<String> {
    let (text, kind) = match self {
      Self::Disabled | Self::Binding(_) => return None,
      Self::Modifiers(modifiers) => {
        let names = modifiers
          .iter()
          .map(|modifier| format!("<{}>", modifier_name(*modifier)))
          .collect::<Vec<_>>()
          .join(" ");
        return Some(format!(
          "{names:?} is a modifier-alone binding, which Shingle supports \
           only for wayfire-shell/toggle_menu and commands; it is ignored"
        ));
      }
      Self::Gesture(text) => (text, "a touch gesture"),
      Self::Hotspot(text) => (text, "a hotspot"),
      Self::Unknown(text) => (text, "not a key, button, gesture or hotspot"),
    };
    Some(if matches!(self, Self::Unknown(_)) {
      format!("{text:?} is {kind}; it is ignored")
    } else {
      format!(
        "{text:?} is {kind}, which Shingle does not support yet; it is ignored"
      )
    })
  }
}

fn modifier_name(modifier: Modifier) -> &'static str {
  match modifier {
    Modifier::Super => "super",
    Modifier::Control => "ctrl",
    Modifier::Alt => "alt",
    Modifier::Shift => "shift",
  }
}

/// Splits an activator into its alternatives. An empty value has none.
///
/// # Errors
///
/// [`ConfigError::Binding`] when an alternative is empty, as in `KEY_A |`,
/// which wf-config rejects as a whole.
pub(crate) fn parse(value: &str) -> Result<Vec<Alternative>, ConfigError> {
  if value.chars().all(char::is_whitespace) {
    return Ok(Vec::new());
  }
  value
    .split('|')
    .map(|text| {
      if text.chars().all(char::is_whitespace) {
        Err(ConfigError::Binding(value.to_owned()))
      } else {
        Ok(alternative(text.trim()))
      }
    })
    .collect()
}

fn alternative(text: &str) -> Alternative {
  let compact = text.split_whitespace().collect::<String>();
  if compact == "none" || compact == "disabled" {
    return Alternative::Disabled;
  }
  let words = text.split_whitespace().collect::<Vec<_>>();
  match words[0] {
    "swipe" | "edge-swipe" | "pinch" => {
      return if gesture(&words) {
        Alternative::Gesture(words.join(" "))
      } else {
        Alternative::Unknown(text.to_owned())
      };
    }
    "hotspot" => {
      return if hotspot(&words) {
        Alternative::Hotspot(words.join(" "))
      } else {
        Alternative::Unknown(text.to_owned())
      };
    }
    _ => {}
  }
  let tokens = text
    .split(|character: char| {
      "<>".contains(character) || character.is_whitespace()
    })
    .filter(|token| !token.is_empty())
    .collect::<Vec<_>>();
  let mut modifiers = Vec::new();
  for token in &tokens[..tokens.len() - 1] {
    match modifier(token) {
      Some(modifier) => modifiers.push(modifier),
      None => return Alternative::Unknown(text.to_owned()),
    }
  }
  let last = tokens[tokens.len() - 1];
  if let Some(modifier) = modifier(last) {
    modifiers.push(modifier);
    return Alternative::Modifiers(modifiers);
  }
  let key = normalize_key(last);
  if key == "xkb:" {
    return Alternative::Unknown(text.to_owned());
  }
  Alternative::Binding(Binding::new(key, modifiers))
}

fn modifier(token: &str) -> Option<Modifier> {
  Some(match token.to_lowercase().as_str() {
    "super" | "logo" => Modifier::Super,
    "ctrl" | "control" => Modifier::Control,
    "alt" => Modifier::Alt,
    "shift" => Modifier::Shift,
    _ => return None,
  })
}

/// `swipe DIRECTION FINGERS`, `edge-swipe DIRECTION FINGERS` or `pinch
/// in|out FINGERS`, as wf-config's `parse_gesture` accepts them.
fn gesture(words: &[&str]) -> bool {
  if words.len() != 3 {
    return false;
  }
  let direction = match words[0] {
    "pinch" => matches!(words[1], "in" | "out"),
    _ => swipe_direction(words[1]),
  };
  // wf-config reads the finger count with atoi, so any text passes.
  direction && !words[2].is_empty()
}

fn swipe_direction(text: &str) -> bool {
  let single = |part: &str| matches!(part, "up" | "down" | "left" | "right");
  match text.split_once('-') {
    None => single(text),
    Some((first, second)) => {
      let vertical = |part: &str| matches!(part, "up" | "down");
      single(first) && single(second) && vertical(first) != vertical(second)
    }
  }
}

/// `hotspot EDGE[-EDGE] ALONGxAWAY TIMEOUT`.
fn hotspot(words: &[&str]) -> bool {
  let edge = |part: &str| matches!(part, "top" | "bottom" | "left" | "right");
  let edges = match words.get(1).map(|word| word.split_once('-')) {
    Some(None) => edge(words[1]),
    Some(Some((first, second))) => edge(first) && edge(second),
    None => false,
  };
  let size = words
    .get(2)
    .and_then(|word| word.split_once('x'))
    .is_some_and(|(along, away)| {
      along.parse::<i32>().is_ok() && away.parse::<i32>().is_ok()
    });
  let timeout = words.get(3).is_some_and(|word| word.parse::<i32>().is_ok());
  words.len() == 4 && edges && size && timeout
}

#[cfg(test)]
mod tests {
  use super::*;

  fn one(text: &str) -> Alternative {
    let mut alternatives = parse(text).unwrap();
    assert_eq!(alternatives.len(), 1, "{text}");
    alternatives.remove(0)
  }

  #[test]
  fn keys_and_buttons_keep_their_modifiers() {
    assert_eq!(
      one("<super> <shift> KEY_Q"),
      Alternative::Binding(Binding::new(
        "q",
        vec![Modifier::Super, Modifier::Shift]
      ))
    );
    assert_eq!(
      one("<super><ctrl>KEY_KP_5"),
      Alternative::Binding(Binding::new(
        "kp5",
        vec![Modifier::Super, Modifier::Control]
      ))
    );
    assert_eq!(
      one("<alt> BTN_LEFT"),
      Alternative::Binding(Binding::new("btn_left", vec![Modifier::Alt]))
    );
    assert_eq!(
      one("<super> xkb:XF86AudioMute"),
      Alternative::Binding(Binding::new(
        "xkb:XF86AudioMute",
        vec![Modifier::Super]
      ))
    );
  }

  #[test]
  fn alternatives_split_at_bars() {
    assert_eq!(
      parse("<super> KEY_P | pinch in 4 | hotspot top-left 10x10 500").unwrap(),
      vec![
        Alternative::Binding(Binding::new("p", vec![Modifier::Super])),
        Alternative::Gesture("pinch in 4".into()),
        Alternative::Hotspot("hotspot top-left 10x10 500".into()),
      ]
    );
    assert!(parse("").unwrap().is_empty());
    assert!(parse("  ").unwrap().is_empty());
    assert!(parse("KEY_A |").is_err());
    assert!(parse("| KEY_A").is_err());
  }

  #[test]
  fn none_and_disabled_disable() {
    assert_eq!(one("none"), Alternative::Disabled);
    assert_eq!(one(" disabled "), Alternative::Disabled);
    assert_eq!(one("dis abled"), Alternative::Disabled);
  }

  #[test]
  fn a_modifier_alone_is_its_own_kind() {
    assert_eq!(
      one("<super>"),
      Alternative::Modifiers(vec![Modifier::Super])
    );
    assert_eq!(
      one("<ctrl> <alt>"),
      Alternative::Modifiers(vec![Modifier::Control, Modifier::Alt])
    );
    assert!(
      one("<super>")
        .unsupported()
        .unwrap()
        .contains("modifier-alone")
    );
  }

  #[test]
  fn gestures_are_recognised_and_never_keys() {
    for text in [
      "swipe up 3",
      "swipe left-up 4",
      "edge-swipe right 2",
      "pinch in 4",
      "pinch out 3",
    ] {
      assert_eq!(one(text), Alternative::Gesture(text.into()));
      assert!(one(text).unsupported().unwrap().contains("touch gesture"));
    }
    for text in ["swipe up-down 3", "pinch sideways 4", "swipe up"] {
      assert_eq!(one(text), Alternative::Unknown(text.into()), "{text}");
    }
  }

  #[test]
  fn hotspots_are_recognised() {
    for text in ["hotspot top-left 10x10 500", "hotspot bottom 1x2 0"] {
      assert_eq!(one(text), Alternative::Hotspot(text.into()));
    }
    for text in ["hotspot middle 10x10 500", "hotspot top 10 500"] {
      assert_eq!(one(text), Alternative::Unknown(text.into()));
    }
  }

  #[test]
  fn unknown_modifiers_are_not_keys() {
    assert_eq!(
      one("<hyper> KEY_A"),
      Alternative::Unknown("<hyper> KEY_A".into())
    );
    assert_eq!(one("xkb:"), Alternative::Unknown("xkb:".into()));
    assert!(one("<hyper> KEY_A").unsupported().is_some());
    assert!(one("KEY_A").unsupported().is_none());
  }
}
