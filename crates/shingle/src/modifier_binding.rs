//! Modifier-alone bindings, such as Wayfire's `toggle_menu = <super>`:
//! modifiers pressed and released with nothing else in between, within
//! `input/modifier_binding_timeout` (wayfire:src/core/seat/keyboard.cpp,
//! `handle_keyboard_key`, and `seat_t::impl::break_mod_bindings`).

use std::time::{Duration, Instant};

use shingle_config::{Modifier, Modifiers};

/// The binding modifier evdev key `code` is, if it is a modifier key:
/// either logo, Control, Alt or Shift key.
pub fn modifier_of_key(code: u32) -> Option<Modifier> {
  Some(match code {
    125 | 126 => Modifier::Super,
    29 | 97 => Modifier::Control,
    56 | 100 => Modifier::Alt,
    42 | 54 => Modifier::Shift,
    _ => return None,
  })
}

/// `modifiers` with `modifier` held too.
fn with(mut modifiers: Modifiers, modifier: Modifier) -> Modifiers {
  match modifier {
    Modifier::Super => modifiers.super_key = true,
    Modifier::Control => modifiers.control = true,
    Modifier::Alt => modifiers.alt = true,
    Modifier::Shift => modifiers.shift = true,
  }
  modifiers
}

/// Follows one keyboard's keys for modifier-alone bindings, as Wayfire's
/// keyboard does: a modifier key pressed while only modifier keys are
/// held, and no pointer button or touch point is down, starts a candidate;
/// any other key press, a button, scrolling or a touch ends it; and its
/// release within the timeout triggers the modifiers held with it.
#[derive(Clone, Debug, Default)]
pub struct ModifierBindingTracker {
  /// The modifier key that started the candidate, and when.
  pending: Option<(u32, Instant)>,
}

impl ModifierBindingTracker {
  /// A press of evdev key `code` at `now`. `only_modifiers` says that no
  /// key other than modifier keys is held besides this one and that no
  /// pointer button or touch point is down.
  pub fn press(&mut self, code: u32, only_modifiers: bool, now: Instant) {
    self.pending = (modifier_of_key(code).is_some() && only_modifiers)
      .then_some((code, now));
  }

  /// A release of evdev key `code` at `now`, with `held` the modifiers
  /// held just before it. Returns the modifiers a modifier-alone binding
  /// must match, those held with the released key's own, when a candidate
  /// was running for less than `timeout_ms` milliseconds; 0 never
  /// cancels, as in Wayfire. Every release ends the candidate.
  pub fn release(
    &mut self,
    code: u32,
    held: Modifiers,
    timeout_ms: u32,
    now: Instant,
  ) -> Option<Modifiers> {
    let (_, started) = self.pending.take()?;
    let elapsed = now.saturating_duration_since(started);
    if timeout_ms != 0 && elapsed >= Duration::from_millis(timeout_ms.into()) {
      return None;
    }
    Some(match modifier_of_key(code) {
      Some(modifier) => with(held, modifier),
      None => held,
    })
  }

  /// Ends the candidate, as a pointer button, scrolling, a touch or a
  /// tablet tool does in Wayfire.
  pub fn interrupt(&mut self) {
    self.pending = None;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SUPER: u32 = 125;
  const SHIFT: u32 = 42;
  const KEY_A: u32 = 30;

  fn logo() -> Modifiers {
    Modifiers {
      super_key: true,
      ..Modifiers::default()
    }
  }

  fn after(start: Instant, millis: u64) -> Instant {
    start + Duration::from_millis(millis)
  }

  #[test]
  fn a_modifier_pressed_alone_triggers_on_release() {
    let start = Instant::now();
    let mut tracker = ModifierBindingTracker::default();
    tracker.press(SUPER, true, start);
    assert_eq!(
      tracker.release(SUPER, logo(), 400, after(start, 100)),
      Some(logo())
    );
    // Only once.
    assert_eq!(tracker.release(SUPER, logo(), 400, after(start, 150)), None);
    // The released key counts even when `held` no longer has it.
    tracker.press(SUPER, true, start);
    assert_eq!(
      tracker.release(SUPER, Modifiers::default(), 400, after(start, 10)),
      Some(logo())
    );
  }

  #[test]
  fn another_key_in_between_cancels() {
    let start = Instant::now();
    let mut tracker = ModifierBindingTracker::default();
    tracker.press(SUPER, true, start);
    tracker.press(KEY_A, false, after(start, 50));
    assert_eq!(tracker.release(KEY_A, logo(), 400, after(start, 60)), None);
    assert_eq!(tracker.release(SUPER, logo(), 400, after(start, 70)), None);
    // A modifier pressed while another key is held does not start one.
    tracker.press(SUPER, false, start);
    assert_eq!(tracker.release(SUPER, logo(), 400, after(start, 10)), None);
    // Nor does a key that is no modifier.
    tracker.press(KEY_A, true, start);
    assert_eq!(tracker.release(KEY_A, logo(), 400, after(start, 10)), None);
  }

  #[test]
  fn holding_too_long_cancels_unless_the_timeout_is_zero() {
    let start = Instant::now();
    let mut tracker = ModifierBindingTracker::default();
    tracker.press(SUPER, true, start);
    assert_eq!(tracker.release(SUPER, logo(), 400, after(start, 400)), None);
    tracker.press(SUPER, true, start);
    assert_eq!(
      tracker.release(SUPER, logo(), 400, after(start, 399)),
      Some(logo())
    );
    tracker.press(SUPER, true, start);
    assert_eq!(
      tracker.release(SUPER, logo(), 0, after(start, 60_000)),
      Some(logo())
    );
  }

  #[test]
  fn buttons_and_touches_interrupt() {
    let start = Instant::now();
    let mut tracker = ModifierBindingTracker::default();
    tracker.press(SUPER, true, start);
    tracker.interrupt();
    assert_eq!(tracker.release(SUPER, logo(), 400, after(start, 10)), None);
  }

  #[test]
  fn the_last_modifier_pressed_carries_the_others() {
    let start = Instant::now();
    let mut tracker = ModifierBindingTracker::default();
    tracker.press(SUPER, true, start);
    tracker.press(SHIFT, true, after(start, 10));
    let both = Modifiers {
      super_key: true,
      shift: true,
      ..Modifiers::default()
    };
    assert_eq!(
      tracker.release(SHIFT, both, 400, after(start, 20)),
      Some(both)
    );
    assert_eq!(modifier_of_key(97), Some(Modifier::Control));
    assert_eq!(modifier_of_key(100), Some(Modifier::Alt));
    assert_eq!(modifier_of_key(KEY_A), None);
  }
}
