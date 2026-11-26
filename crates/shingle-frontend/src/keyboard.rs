//! Keyboard state shared by every frontend, built on xkbcommon.
//!
//! Hosts feed evdev key codes into a [`Keyboard`] that interprets them with
//! Shingle's own keymap (`InputConfig::load_keymap`), the same keymap clients
//! receive. Each key yields its Wayfire name for ordinary bindings, its
//! keysym name for `xkb:` bindings, its text for Scale's filter and the
//! binding modifiers in effect, and [`Keyboard::modifiers`] gives the masks
//! for `Compositor::keyboard_modifiers`. Nothing here depends on the host's
//! keyboard layout.

mod names;
mod repeat;

use std::{collections::BTreeSet, fmt};

use shingle::Modifiers;
use shingle_config::Modifiers as ConfigModifiers;
use xkbcommon::xkb;

pub use names::key_name;
pub use repeat::KeyRepeat;

/// Offset between evdev key codes and XKB keycodes.
const EVDEV_OFFSET: u32 = 8;

/// A keymap that libxkbcommon could not compile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeymapError;

impl fmt::Display for KeymapError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("failed to compile XKB keymap")
  }
}

impl std::error::Error for KeymapError {}

/// What happened to a key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyState {
  /// The key went down.
  Pressed,
  /// The key went up.
  Released,
  /// A held key repeated. The keyboard state does not change.
  Repeated,
}

/// One key event, interpreted with Shingle's keymap.
///
/// Names, text and modifiers describe the state before the event, as XKB
/// does: pressing Super reports no modifiers, and releasing it reports
/// Super. [`Key::modifiers_changed`] tells the caller to send the new masks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Key {
  /// The evdev key code.
  pub code: u32,
  /// What happened to the key. A press of a key that is already down is
  /// reported as [`KeyState::Repeated`].
  pub state: KeyState,
  /// The Wayfire key name, from [`key_name`].
  pub name: Option<&'static str>,
  /// The keysym name for `xkb:` bindings, such as `a`, `A`, `Return` or
  /// `adiaeresis`, if the key produces exactly one keysym.
  pub symbol: Option<String>,
  /// The UTF-8 text a press or repeat produces, if any. It may be a control
  /// character.
  pub text: Option<String>,
  /// The binding modifiers held when the key event happened.
  pub modifiers: ConfigModifiers,
  /// Whether the event changed [`Keyboard::modifiers`].
  pub modifiers_changed: bool,
}

/// Which keyboard LEDs are lit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Leds {
  /// Caps Lock.
  pub caps_lock: bool,
  /// Num Lock.
  pub num_lock: bool,
  /// Scroll Lock.
  pub scroll_lock: bool,
}

/// XKB state for one seat keyboard.
pub struct Keyboard {
  keymap: xkb::Keymap,
  state: xkb::State,
  held: BTreeSet<u32>,
}

impl Keyboard {
  /// Compiles `keymap`, an XKB keymap in text format, with no key held, no
  /// modifier locked and the first layout active.
  pub fn new(keymap: &str) -> Result<Self, KeymapError> {
    let keymap = compile(keymap)?;
    let state = xkb::State::new(&keymap);
    Ok(Self {
      keymap,
      state,
      held: BTreeSet::new(),
    })
  }

  /// Switches to a new keymap. Keys that are held stay held in the new
  /// keymap and locked modifiers stay locked; the layout returns to the
  /// first one, as `Compositor::set_keymap` does.
  pub fn set_keymap(&mut self, keymap: &str) -> Result<(), KeymapError> {
    let locked = self.state.serialize_mods(xkb::STATE_MODS_LOCKED);
    self.keymap = compile(keymap)?;
    self.state = xkb::State::new(&self.keymap);
    self.state.update_mask(0, 0, locked, 0, 0, 0);
    for code in &self.held {
      self
        .state
        .update_key(xkb_keycode(*code), xkb::KeyDirection::Down);
    }
    Ok(())
  }

  /// Interprets one key event and updates the state.
  ///
  /// A press of a key that is already down counts as a repeat, and a
  /// release of a key that is not down leaves the state alone, so host
  /// events lost on focus changes cannot leave modifiers stuck.
  pub fn key(&mut self, code: u32, state: KeyState) -> Key {
    let keycode = xkb_keycode(code);
    let state = match state {
      KeyState::Pressed if self.held.contains(&code) => KeyState::Repeated,
      state => state,
    };
    let symbol = self.state.key_get_one_sym(keycode);
    let text = (state != KeyState::Released)
      .then(|| self.state.key_get_utf8(keycode))
      .filter(|text| !text.is_empty());
    let modifiers = self.binding_modifiers();
    let before = self.modifiers();
    match state {
      KeyState::Pressed => {
        self.held.insert(code);
        self.state.update_key(keycode, xkb::KeyDirection::Down);
      }
      KeyState::Released => {
        if self.held.remove(&code) {
          self.state.update_key(keycode, xkb::KeyDirection::Up);
        }
      }
      KeyState::Repeated => {}
    }
    Key {
      code,
      state,
      name: key_name(code),
      symbol: (symbol.raw() != xkb::keysyms::KEY_NoSymbol)
        .then(|| xkb::keysym_get_name(symbol)),
      text,
      modifiers,
      modifiers_changed: self.modifiers() != before,
    }
  }

  /// Returns the depressed, latched and locked modifier masks and the
  /// active layout, for `Compositor::keyboard_modifiers`.
  pub fn modifiers(&self) -> Modifiers {
    Modifiers {
      depressed: self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
      latched: self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
      locked: self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
      group: self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
    }
  }

  /// Returns the effective binding modifiers: Shift, Control, Alt (`Mod1`)
  /// and Super (`Mod4`), whether depressed, latched or locked.
  pub fn binding_modifiers(&self) -> ConfigModifiers {
    let active = |name| {
      self
        .state
        .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
    };
    ConfigModifiers {
      super_key: active(xkb::MOD_NAME_LOGO),
      control: active(xkb::MOD_NAME_CTRL),
      alt: active(xkb::MOD_NAME_ALT),
      shift: active(xkb::MOD_NAME_SHIFT),
    }
  }

  /// Adopts locked modifiers and the active layout that changed outside
  /// this keyboard, for example through `wayfire/set-keyboard-state` or the
  /// compositor's initial Caps Lock and Num Lock. Held keys stay held.
  /// Returns whether anything changed.
  pub fn sync_locked(&mut self, locked: u32, group: u32) -> bool {
    let current = self.modifiers();
    if current.locked == locked && current.group == group {
      return false;
    }
    self.state.update_mask(
      current.depressed,
      current.latched,
      locked,
      0,
      0,
      group,
    );
    true
  }

  /// Forgets every held key, for when the host loses keyboard focus and
  /// will not report their releases. Locked modifiers and the layout stay.
  /// Returns the codes that were held.
  pub fn release_all(&mut self) -> Vec<u32> {
    let Modifiers { locked, group, .. } = self.modifiers();
    self.state = xkb::State::new(&self.keymap);
    self.state.update_mask(0, 0, locked, 0, 0, group);
    std::mem::take(&mut self.held).into_iter().collect()
  }

  /// Returns the LEDs the state lights, by the keymap's standard LED
  /// names.
  pub fn leds(&self) -> Leds {
    Leds {
      caps_lock: self.state.led_name_is_active(xkb::LED_NAME_CAPS),
      num_lock: self.state.led_name_is_active(xkb::LED_NAME_NUM),
      scroll_lock: self.state.led_name_is_active(xkb::LED_NAME_SCROLL),
    }
  }

  /// Returns whether key `code` is down.
  pub fn is_held(&self, code: u32) -> bool {
    self.held.contains(&code)
  }

  /// Returns whether every key that is down is a modifier key
  /// ([`shingle::modifier_of_key`]), as Wayfire's `has_only_modifiers`.
  pub fn holds_only_modifiers(&self) -> bool {
    self
      .held
      .iter()
      .all(|code| shingle::modifier_of_key(*code).is_some())
  }

  /// Returns whether key `code` repeats in the keymap. Modifiers usually
  /// do not.
  pub fn key_repeats(&self, code: u32) -> bool {
    self.keymap.key_repeats(xkb_keycode(code))
  }
}

fn compile(keymap: &str) -> Result<xkb::Keymap, KeymapError> {
  let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
  xkb::Keymap::new_from_string(
    &context,
    keymap.to_owned(),
    xkb::KEYMAP_FORMAT_TEXT_V1,
    xkb::KEYMAP_COMPILE_NO_FLAGS,
  )
  .ok_or(KeymapError)
}

fn xkb_keycode(code: u32) -> xkb::Keycode {
  xkb::Keycode::new(code.saturating_add(EVDEV_OFFSET))
}

#[cfg(test)]
mod tests {
  use shingle_config::{Binding, InputConfig, Keybindings, Modifier};

  use super::*;

  const KEY_ESC: u32 = 1;
  const KEY_1: u32 = 2;
  const KEY_TAB: u32 = 15;
  const KEY_Q: u32 = 16;
  const KEY_Y: u32 = 21;
  const KEY_ENTER: u32 = 28;
  const KEY_A: u32 = 30;
  const KEY_LEFTSHIFT: u32 = 42;
  const KEY_Z: u32 = 44;
  const KEY_LEFTALT: u32 = 56;
  const KEY_SPACE: u32 = 57;
  const KEY_CAPSLOCK: u32 = 58;
  const KEY_PAGEUP: u32 = 104;
  const KEY_LEFTMETA: u32 = 125;
  const SHIFT: u32 = 1;
  const LOCK: u32 = 2;
  const CONTROL: u32 = 4;
  const MOD2: u32 = 16;
  const MOD4: u32 = 64;

  fn keyboard(layout: &str, variant: &str, options: &str) -> Keyboard {
    let config = InputConfig {
      layout: layout.into(),
      variant: variant.into(),
      options: options.into(),
      ..InputConfig::default()
    };
    Keyboard::new(&config.keymap().unwrap()).expect("the keymap compiles")
  }

  fn tap(keyboard: &mut Keyboard, code: u32) -> Key {
    let key = keyboard.key(code, KeyState::Pressed);
    keyboard.key(code, KeyState::Released);
    key
  }

  const SUPER: ConfigModifiers = ConfigModifiers {
    super_key: true,
    control: false,
    alt: false,
    shift: false,
  };

  #[test]
  fn evdev_codes_match_wayfire_shortcut_names() {
    for (code, expected) in [
      (105, "left"),
      (106, "right"),
      (103, "up"),
      (108, "down"),
      (82, "kp0"),
      (71, "kp7"),
      (96, "kpenter"),
      (78, "kpplus"),
      (98, "kpslash"),
      (41, "grave"),
      (KEY_ESC, "escape"),
      (KEY_1, "1"),
      (KEY_A, "a"),
      (KEY_ENTER, "enter"),
      (KEY_LEFTMETA, "leftmeta"),
      (656, "macro1"),
      (700, "kbd_lcd_menu5"),
    ] {
      assert_eq!(key_name(code), Some(expected), "code {code}");
    }
    assert_eq!(key_name(0), None);
    assert_eq!(key_name(0x100), None);
    assert_eq!(key_name(u32::MAX), None);

    let keys = Keybindings::default();
    let super_alt = ConfigModifiers { alt: true, ..SUPER };
    assert!(
      keys
        .workspace_next
        .matches(super_alt, key_name(106).unwrap())
    );
    assert!(keys.grid_slot_top_left[0].matches(SUPER, key_name(71).unwrap()));
    assert!(keys.workspace_switch[0].matches(SUPER, key_name(KEY_1).unwrap()));
  }

  #[test]
  fn keysym_names_follow_the_keymap_and_shift() {
    let mut us = keyboard("us", "", "");
    assert_eq!(tap(&mut us, KEY_A).symbol.as_deref(), Some("a"));
    assert_eq!(tap(&mut us, KEY_ENTER).symbol.as_deref(), Some("Return"));
    assert_eq!(tap(&mut us, KEY_PAGEUP).symbol.as_deref(), Some("Prior"));
    assert_eq!(tap(&mut us, KEY_SPACE).symbol.as_deref(), Some("space"));
    us.key(KEY_LEFTSHIFT, KeyState::Pressed);
    let shifted = tap(&mut us, KEY_A);
    assert_eq!(shifted.symbol.as_deref(), Some("A"));
    assert_eq!(shifted.text.as_deref(), Some("A"));
    assert_eq!(tap(&mut us, KEY_1).symbol.as_deref(), Some("exclam"));

    let binding = Binding::new("xkb:A", vec![Modifier::Super]);
    assert!(binding.matches_xkb(SUPER, "A"));
    assert!(!binding.matches_xkb(ConfigModifiers::default(), "A"));
    assert!(!binding.matches_xkb(SUPER, "a"));
    assert!(!binding.matches(ConfigModifiers::default(), "a"));
  }

  #[test]
  fn a_german_keymap_changes_symbols_but_not_names() {
    let mut de = keyboard("de", "", "");
    let y = tap(&mut de, KEY_Y);
    assert_eq!(y.name, Some("y"));
    assert_eq!(y.symbol.as_deref(), Some("z"));
    assert_eq!(y.text.as_deref(), Some("z"));
    let z = tap(&mut de, KEY_Z);
    assert_eq!(z.name, Some("z"));
    assert_eq!(z.symbol.as_deref(), Some("y"));
    assert_eq!(tap(&mut de, 39).symbol.as_deref(), Some("odiaeresis"));
    assert_eq!(tap(&mut de, 39).text.as_deref(), Some("ö"));
  }

  #[test]
  fn modifier_keys_update_masks_after_the_event() {
    let mut keyboard = keyboard("us", "", "");
    let press = keyboard.key(KEY_LEFTMETA, KeyState::Pressed);
    assert_eq!(press.name, Some("leftmeta"));
    assert_eq!(press.modifiers, ConfigModifiers::default());
    assert!(press.modifiers_changed);
    assert_eq!(keyboard.binding_modifiers(), SUPER);
    assert_eq!(keyboard.modifiers().depressed, MOD4);

    let q = keyboard.key(KEY_Q, KeyState::Pressed);
    assert_eq!(q.modifiers, SUPER);
    assert!(!q.modifiers_changed);
    keyboard.key(KEY_Q, KeyState::Released);

    let release = keyboard.key(KEY_LEFTMETA, KeyState::Released);
    assert_eq!(release.modifiers, SUPER);
    assert!(release.modifiers_changed);
    assert_eq!(keyboard.modifiers(), Modifiers::default());
  }

  #[test]
  fn caps_lock_locks_and_unlocks() {
    let mut keyboard = keyboard("us", "", "");
    assert!(tap(&mut keyboard, KEY_CAPSLOCK).modifiers_changed);
    assert_eq!(keyboard.modifiers().locked, LOCK);
    assert_eq!(keyboard.modifiers().depressed, 0);
    assert_eq!(tap(&mut keyboard, KEY_A).text.as_deref(), Some("A"));
    tap(&mut keyboard, KEY_CAPSLOCK);
    assert_eq!(keyboard.modifiers().locked, 0);
  }

  #[test]
  fn leds_follow_locks_from_keys_and_from_outside() {
    let mut keyboard = keyboard("us", "", "");
    assert_eq!(keyboard.leds(), Leds::default());
    tap(&mut keyboard, KEY_CAPSLOCK);
    assert_eq!(
      keyboard.leds(),
      Leds {
        caps_lock: true,
        ..Leds::default()
      }
    );
    // Num Lock set through IPC, Caps Lock cleared.
    keyboard.sync_locked(MOD2, 0);
    assert_eq!(
      keyboard.leds(),
      Leds {
        num_lock: true,
        ..Leds::default()
      }
    );
  }

  #[test]
  fn keymap_options_decide_the_modifiers() {
    let mut keyboard = keyboard("de", "", "ctrl:nocaps");
    keyboard.key(KEY_CAPSLOCK, KeyState::Pressed);
    assert!(keyboard.binding_modifiers().control);
    assert_eq!(keyboard.modifiers().depressed, CONTROL);
    assert_eq!(keyboard.modifiers().locked, 0);

    let mut swapped = self::keyboard("us", "", "altwin:swap_alt_win");
    swapped.key(KEY_LEFTALT, KeyState::Pressed);
    assert_eq!(swapped.binding_modifiers(), SUPER);
  }

  #[test]
  fn repeats_and_unmatched_events_leave_the_state_alone() {
    let mut keyboard = keyboard("us", "", "");
    keyboard.key(KEY_LEFTSHIFT, KeyState::Pressed);
    let again = keyboard.key(KEY_LEFTSHIFT, KeyState::Pressed);
    assert_eq!(again.state, KeyState::Repeated);
    assert!(!again.modifiers_changed);
    let repeat = keyboard.key(KEY_A, KeyState::Repeated);
    assert_eq!(repeat.text.as_deref(), Some("A"));
    assert!(!keyboard.is_held(KEY_A));

    let stray = keyboard.key(KEY_TAB, KeyState::Released);
    assert_eq!(stray.state, KeyState::Released);
    assert_eq!(stray.text, None);
    assert!(!stray.modifiers_changed);

    keyboard.key(KEY_LEFTSHIFT, KeyState::Released);
    assert_eq!(keyboard.modifiers(), Modifiers::default());
  }

  #[test]
  fn sync_locked_adopts_external_layout_and_locks() {
    let mut keyboard = keyboard("us,de", "", "");
    assert!(!keyboard.sync_locked(0, 0));
    keyboard.key(KEY_LEFTSHIFT, KeyState::Pressed);
    assert!(keyboard.sync_locked(MOD2, 1));
    let modifiers = keyboard.modifiers();
    assert_eq!(modifiers.group, 1);
    assert_eq!(modifiers.locked, MOD2);
    assert_eq!(modifiers.depressed, SHIFT);
    assert_eq!(tap(&mut keyboard, KEY_Y).symbol.as_deref(), Some("Z"));
    keyboard.key(KEY_LEFTSHIFT, KeyState::Released);
    assert_eq!(keyboard.modifiers().depressed, 0);
  }

  #[test]
  fn release_all_keeps_locks_and_layout() {
    let mut keyboard = keyboard("us,de", "", "");
    tap(&mut keyboard, KEY_CAPSLOCK);
    keyboard.sync_locked(LOCK, 1);
    keyboard.key(KEY_LEFTMETA, KeyState::Pressed);
    keyboard.key(KEY_A, KeyState::Pressed);
    assert_eq!(keyboard.release_all(), vec![KEY_A, KEY_LEFTMETA]);
    let modifiers = keyboard.modifiers();
    assert_eq!(modifiers.depressed, 0);
    assert_eq!(modifiers.locked, LOCK);
    assert_eq!(modifiers.group, 1);
    assert!(
      !keyboard
        .key(KEY_LEFTMETA, KeyState::Released)
        .modifiers_changed
    );
  }

  #[test]
  fn set_keymap_keeps_held_keys_and_locks() {
    let mut keyboard = keyboard("us", "", "");
    keyboard.key(KEY_LEFTMETA, KeyState::Pressed);
    tap(&mut keyboard, KEY_CAPSLOCK);
    let de = InputConfig {
      layout: "de".into(),
      ..InputConfig::default()
    };
    keyboard
      .set_keymap(&de.keymap().unwrap())
      .expect("the keymap compiles");
    assert_eq!(keyboard.binding_modifiers(), SUPER);
    assert_eq!(keyboard.modifiers().locked, LOCK);
    assert_eq!(tap(&mut keyboard, KEY_Y).symbol.as_deref(), Some("Z"));
    keyboard.key(KEY_LEFTMETA, KeyState::Released);
    assert_eq!(keyboard.modifiers().depressed, 0);
  }

  #[test]
  fn invalid_keymaps_are_rejected() {
    assert_eq!(Keyboard::new("xkb_keymap {").err(), Some(KeymapError));
    let mut keyboard = keyboard("us", "", "");
    assert!(keyboard.set_keymap("nonsense").is_err());
    assert_eq!(tap(&mut keyboard, KEY_A).symbol.as_deref(), Some("a"));
  }

  #[test]
  fn letters_repeat_and_modifiers_do_not() {
    let keyboard = keyboard("us", "", "");
    assert!(keyboard.key_repeats(KEY_A));
    assert!(keyboard.key_repeats(KEY_ENTER));
    assert!(!keyboard.key_repeats(KEY_LEFTSHIFT));
    assert!(!keyboard.key_repeats(KEY_LEFTMETA));
  }
}
