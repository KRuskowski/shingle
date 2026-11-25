//! Binding helpers shared by keyboard, pointer and wheel dispatch.

use shingle_config::{
  Binding, Keybindings, Modifier, Modifiers as ConfigModifiers,
};

/// Returns the grid slot whose binding matches `key` with `modifiers`.
pub fn grid_slot(
  bindings: &Keybindings,
  modifiers: ConfigModifiers,
  key: &str,
) -> Option<&'static str> {
  for (slot, bindings) in [
    ("top_left", &bindings.grid_slot_top_left),
    ("top", &bindings.grid_slot_top),
    ("top_right", &bindings.grid_slot_top_right),
    ("left", &bindings.grid_slot_left),
    ("center", &bindings.grid_slot_center),
    ("right", &bindings.grid_slot_right),
    ("bottom_left", &bindings.grid_slot_bottom_left),
    ("bottom", &bindings.grid_slot_bottom),
    ("bottom_right", &bindings.grid_slot_bottom_right),
  ] {
    if bindings
      .iter()
      .any(|binding| binding.matches(modifiers, key))
    {
      return Some(slot);
    }
  }
  None
}

/// Returns the modifiers a binding requires.
pub fn binding_modifiers(binding: &Binding) -> ConfigModifiers {
  ConfigModifiers {
    super_key: binding.modifiers.contains(&Modifier::Super),
    control: binding.modifiers.contains(&Modifier::Control),
    alt: binding.modifiers.contains(&Modifier::Alt),
    shift: binding.modifiers.contains(&Modifier::Shift),
  }
}

/// Returns whether no modifier is held.
pub fn modifiers_empty(modifiers: ConfigModifiers) -> bool {
  modifiers == ConfigModifiers::default()
}

/// Returns whether every modifier in `expected` is held in `actual`.
pub fn modifiers_held(
  expected: ConfigModifiers,
  actual: ConfigModifiers,
) -> bool {
  (!expected.super_key || actual.super_key)
    && (!expected.control || actual.control)
    && (!expected.alt || actual.alt)
    && (!expected.shift || actual.shift)
}

/// Matches a Wayfire modifier string such as `<super> <alt>` or
/// `ctrl+shift` exactly against the held modifiers. Unknown names and an
/// empty string never match.
pub fn modifier_matches(text: &str, actual: ConfigModifiers) -> bool {
  let normalized = text.replace("><", "> <").replace(['<', '>'], "");
  let mut expected = ConfigModifiers::default();
  let mut found = false;
  for modifier in normalized.split_whitespace() {
    for modifier in modifier.split(['+', '|']) {
      match modifier {
        "super" | "mod4" | "logo" => expected.super_key = true,
        "ctrl" | "control" => expected.control = true,
        "alt" | "mod1" => expected.alt = true,
        "shift" => expected.shift = true,
        _ => return false,
      }
      found = true;
    }
  }
  found && actual == expected
}

#[cfg(test)]
mod tests {
  use super::*;

  const SUPER: ConfigModifiers = ConfigModifiers {
    super_key: true,
    control: false,
    alt: false,
    shift: false,
  };

  #[test]
  fn grid_slot_finds_the_matching_slot() {
    let keys = Keybindings::default();
    assert_eq!(grid_slot(&keys, SUPER, "kp7"), Some("top_left"));
    assert_eq!(grid_slot(&keys, SUPER, "left"), Some("left"));
    assert_eq!(grid_slot(&keys, ConfigModifiers::default(), "kp7"), None);
    assert_eq!(grid_slot(&keys, SUPER, "kp0"), None);
  }

  #[test]
  fn binding_modifiers_lists_the_required_modifiers() {
    let binding = Binding::new("a", vec![Modifier::Super, Modifier::Shift]);
    let expected = ConfigModifiers {
      shift: true,
      ..SUPER
    };
    assert_eq!(binding_modifiers(&binding), expected);
    assert!(modifiers_empty(binding_modifiers(&Binding::disabled())));
    assert!(!modifiers_empty(expected));
  }

  #[test]
  fn modifiers_held_allows_extra_modifiers() {
    let both = ConfigModifiers { alt: true, ..SUPER };
    assert!(modifiers_held(SUPER, both));
    assert!(!modifiers_held(both, SUPER));
    assert!(modifiers_held(ConfigModifiers::default(), SUPER));
  }

  #[test]
  fn modifier_strings_match_exactly() {
    let both = ConfigModifiers { alt: true, ..SUPER };
    assert!(modifier_matches("<super>", SUPER));
    assert!(modifier_matches("<super><alt>", both));
    assert!(modifier_matches("mod4+mod1", both));
    assert!(!modifier_matches("<super>", both));
    assert!(!modifier_matches("", ConfigModifiers::default()));
    assert!(!modifier_matches("<hyper>", SUPER));
  }
}
