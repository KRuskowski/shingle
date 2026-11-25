//! Keeps the shared keyboard in step across host keyboard focus changes.
//!
//! The frontend's [`Keyboard`] sees only the key events the host window
//! receives. winit reports no releases for keys held when the window loses
//! focus and no presses for keys already held when it gains focus; it only
//! reports the host's modifiers. On focus loss the frontend forgets the
//! held keys (`InputEvent::Reset`). On focus gain, modifiers the host
//! reports as held are pressed with their usual left-hand key, and
//! released once the host stops reporting them, so a Super held while
//! focusing the window still counts.

use shingle_config::Modifiers as ConfigModifiers;
use shingle_frontend::keyboard::{KeyState, Keyboard};

/// Reads one modifier from a modifier set.
type ModifierHeld = fn(ConfigModifiers) -> bool;

/// Evdev codes pressed to stand in for a modifier the host reports held:
/// `KEY_LEFTSHIFT`, `KEY_LEFTCTRL`, `KEY_LEFTALT` and `KEY_LEFTMETA`.
const STAND_INS: [(ModifierHeld, u32); 4] = [
  (|modifiers| modifiers.shift, 42),
  (|modifiers| modifiers.control, 29),
  (|modifiers| modifiers.alt, 56),
  (|modifiers| modifiers.super_key, 125),
];

/// Focus-change bookkeeping for one host keyboard.
#[derive(Debug, Default)]
pub(crate) struct HostKeyboard {
  /// Whether the window gained focus and no key or modifier event has
  /// arrived since.
  focus_entered: bool,
  /// Stand-in keys pressed on focus gain and not yet released.
  stand_ins: Vec<u32>,
}

impl HostKeyboard {
  /// Handles a host focus change. On focus loss the stand-ins are gone
  /// with the rest of the held keys, which the host makes the frontend
  /// forget with `InputEvent::Reset`.
  pub(crate) fn focus_changed(&mut self, focused: bool) {
    self.focus_entered = focused;
    if !focused {
      self.stand_ins.clear();
    }
  }

  /// Notes a host key event for `code`, before it reaches `Keyboard::key`.
  /// A real event for a stand-in key takes it over.
  pub(crate) fn key(&mut self, code: u32) {
    self.focus_entered = false;
    self.stand_ins.retain(|stand_in| *stand_in != code);
  }

  /// Handles the host's modifier state. Returns whether `keyboard`'s
  /// modifiers changed.
  pub(crate) fn host_modifiers(
    &mut self,
    keyboard: &mut Keyboard,
    host: ConfigModifiers,
  ) -> bool {
    let before = keyboard.modifiers();
    if std::mem::take(&mut self.focus_entered) {
      let held = keyboard.binding_modifiers();
      for (modifier, code) in STAND_INS {
        if modifier(host) && !modifier(held) && !keyboard.is_held(code) {
          keyboard.key(code, KeyState::Pressed);
          self.stand_ins.push(code);
        }
      }
    }
    self.stand_ins.retain(|code| {
      let (modifier, _) = STAND_INS
        .iter()
        .find(|(_, stand_in)| stand_in == code)
        .expect("stand-ins come from STAND_INS");
      if modifier(host) {
        return true;
      }
      keyboard.key(*code, KeyState::Released);
      false
    });
    keyboard.modifiers() != before
  }
}

#[cfg(test)]
mod tests {
  use shingle_config::InputConfig;

  use super::*;

  const KEY_A: u32 = 30;
  const KEY_LEFTMETA: u32 = 125;
  const KEY_RIGHTMETA: u32 = 126;
  const SUPER: ConfigModifiers = ConfigModifiers {
    super_key: true,
    control: false,
    alt: false,
    shift: false,
  };

  fn keyboard() -> Keyboard {
    Keyboard::new(&InputConfig::default().keymap()).expect("keymap compiles")
  }

  #[test]
  fn focus_loss_drops_the_stand_ins() {
    let mut keyboard = keyboard();
    let mut host = HostKeyboard::default();
    host.focus_changed(true);
    assert!(host.host_modifiers(&mut keyboard, SUPER));
    host.focus_changed(false);
    keyboard.release_all();
    // The host still reports Super, but no focus was gained since.
    assert!(!host.host_modifiers(&mut keyboard, SUPER));
    assert_eq!(keyboard.binding_modifiers(), ConfigModifiers::default());
  }

  #[test]
  fn modifiers_held_on_focus_gain_count_until_the_host_releases_them() {
    let mut keyboard = keyboard();
    let mut host = HostKeyboard::default();
    host.focus_changed(true);
    assert!(host.host_modifiers(&mut keyboard, SUPER));
    assert_eq!(keyboard.binding_modifiers(), SUPER);
    // Releasing the right-hand Super, which the keyboard never saw.
    host.key(KEY_RIGHTMETA);
    keyboard.key(KEY_RIGHTMETA, KeyState::Released);
    assert_eq!(keyboard.binding_modifiers(), SUPER);
    assert!(host.host_modifiers(&mut keyboard, ConfigModifiers::default()));
    assert_eq!(keyboard.binding_modifiers(), ConfigModifiers::default());
  }

  #[test]
  fn a_real_release_of_the_stand_in_key_ends_it() {
    let mut keyboard = keyboard();
    let mut host = HostKeyboard::default();
    host.focus_changed(true);
    host.host_modifiers(&mut keyboard, SUPER);
    host.key(KEY_LEFTMETA);
    keyboard.key(KEY_LEFTMETA, KeyState::Released);
    assert_eq!(keyboard.binding_modifiers(), ConfigModifiers::default());
    assert!(!host.host_modifiers(&mut keyboard, ConfigModifiers::default()));
  }

  #[test]
  fn host_modifiers_after_focus_gain_do_not_press_keys() {
    let mut keyboard = keyboard();
    let mut host = HostKeyboard::default();
    host.focus_changed(true);
    host.key(KEY_A);
    keyboard.key(KEY_A, KeyState::Pressed);
    assert!(!host.host_modifiers(&mut keyboard, SUPER));
    assert_eq!(keyboard.binding_modifiers(), ConfigModifiers::default());

    // Without a focus change the host's modifiers are not used either, so
    // a layout that maps keys differently from the host's is respected.
    let mut host = HostKeyboard::default();
    assert!(!host.host_modifiers(&mut keyboard, SUPER));
  }
}
