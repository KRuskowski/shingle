//! Key repeat for compositor bindings, as a timer the frontend polls.
//!
//! libinput reports no repeats, and the nested host drops winit's, so the
//! frontend repeats keys itself with the configured `repeat_rate` and
//! `repeat_delay`, the values clients get in `wl_keyboard.repeat_info`. As
//! in Wayland clients, only the most recently pressed key that repeats in
//! the keymap repeats, until it is released.

use std::time::{Duration, Instant};

/// The repeat timer of one keyboard.
#[derive(Clone, Debug)]
pub struct KeyRepeat {
  /// Time from a press to its first repeat.
  delay: Duration,
  /// Time between repeats, or `None` if repeat is off.
  interval: Option<Duration>,
  /// The repeating key and when it repeats next.
  active: Option<(u32, Instant)>,
}

impl KeyRepeat {
  /// Creates a timer that repeats `rate` times a second after `delay_ms`
  /// milliseconds. A rate of 0 turns repeat off, as in
  /// `wl_keyboard.repeat_info`.
  pub fn new(rate: u32, delay_ms: u32) -> Self {
    let mut repeat = Self {
      delay: Duration::ZERO,
      interval: None,
      active: None,
    };
    repeat.configure(rate, delay_ms);
    repeat
  }

  /// Changes the rate and delay. A key that is repeating stops.
  pub fn configure(&mut self, rate: u32, delay_ms: u32) {
    self.delay = Duration::from_millis(u64::from(delay_ms));
    self.interval =
      (rate > 0).then(|| Duration::from_secs(1) / rate.min(1_000_000));
    self.active = None;
  }

  /// Notes that key `code` went down at `now`. A key that `repeats` takes
  /// over from the key repeating so far; any other key, such as a
  /// modifier, leaves it repeating.
  pub fn press(&mut self, code: u32, repeats: bool, now: Instant) {
    if repeats && self.interval.is_some() {
      self.active = Some((code, now + self.delay));
    }
  }

  /// Notes that key `code` went up. Repeat stops if it was repeating.
  pub fn release(&mut self, code: u32) {
    if self.active.is_some_and(|(active, _)| active == code) {
      self.active = None;
    }
  }

  /// Stops any repeat, for example when the host loses keyboard focus.
  pub fn cancel(&mut self) {
    self.active = None;
  }

  /// Returns the key to repeat once if its next repeat is due at `now`.
  /// At most one repeat is reported per call; after a stall the next one
  /// is a full interval away instead of a burst.
  pub fn poll(&mut self, now: Instant) -> Option<u32> {
    let interval = self.interval?;
    let (code, due) = self.active.as_mut()?;
    if now < *due {
      return None;
    }
    *due += interval;
    if *due <= now {
      *due = now + interval;
    }
    Some(*code)
  }

  /// Returns when the next repeat is due, if a key is repeating.
  pub fn deadline(&self) -> Option<Instant> {
    self.active.map(|(_, due)| due)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const KEY_A: u32 = 30;
  const KEY_B: u32 = 48;
  const KEY_LEFTSHIFT: u32 = 42;

  fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
  }

  #[test]
  fn a_held_key_repeats_after_the_delay_at_the_rate() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(40, 400);
    repeat.press(KEY_A, true, start);
    assert_eq!(repeat.deadline(), Some(start + ms(400)));
    assert_eq!(repeat.poll(start + ms(399)), None);
    assert_eq!(repeat.poll(start + ms(400)), Some(KEY_A));
    assert_eq!(repeat.poll(start + ms(410)), None);
    assert_eq!(repeat.deadline(), Some(start + ms(425)));
    assert_eq!(repeat.poll(start + ms(425)), Some(KEY_A));
  }

  #[test]
  fn release_stops_only_the_repeating_key() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(40, 400);
    repeat.press(KEY_A, true, start);
    repeat.release(KEY_B);
    assert_eq!(repeat.poll(start + ms(400)), Some(KEY_A));
    repeat.release(KEY_A);
    assert_eq!(repeat.poll(start + ms(1000)), None);
    assert_eq!(repeat.deadline(), None);
  }

  #[test]
  fn the_last_repeating_key_pressed_repeats() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(40, 400);
    repeat.press(KEY_A, true, start);
    repeat.press(KEY_B, true, start + ms(100));
    assert_eq!(repeat.poll(start + ms(450)), None);
    assert_eq!(repeat.poll(start + ms(500)), Some(KEY_B));
    // Releasing the earlier key does not stop the later one.
    repeat.release(KEY_A);
    assert_eq!(repeat.poll(start + ms(525)), Some(KEY_B));
  }

  #[test]
  fn a_key_that_does_not_repeat_leaves_the_repeat_alone() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(40, 400);
    repeat.press(KEY_A, true, start);
    repeat.press(KEY_LEFTSHIFT, false, start + ms(100));
    assert_eq!(repeat.poll(start + ms(400)), Some(KEY_A));
    let mut idle = KeyRepeat::new(40, 400);
    idle.press(KEY_LEFTSHIFT, false, start);
    assert_eq!(idle.deadline(), None);
  }

  #[test]
  fn rate_zero_turns_repeat_off() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(0, 400);
    repeat.press(KEY_A, true, start);
    assert_eq!(repeat.deadline(), None);
    assert_eq!(repeat.poll(start + ms(10_000)), None);
  }

  #[test]
  fn a_stall_yields_one_repeat_then_a_full_interval() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(40, 400);
    repeat.press(KEY_A, true, start);
    assert_eq!(repeat.poll(start + ms(2000)), Some(KEY_A));
    assert_eq!(repeat.poll(start + ms(2000)), None);
    assert_eq!(repeat.deadline(), Some(start + ms(2025)));
  }

  #[test]
  fn cancel_and_configure_stop_the_repeat() {
    let start = Instant::now();
    let mut repeat = KeyRepeat::new(40, 400);
    repeat.press(KEY_A, true, start);
    repeat.cancel();
    assert_eq!(repeat.deadline(), None);
    repeat.press(KEY_A, true, start);
    repeat.configure(25, 200);
    assert_eq!(repeat.deadline(), None);
    repeat.press(KEY_A, true, start);
    assert_eq!(repeat.poll(start + ms(200)), Some(KEY_A));
    assert_eq!(repeat.deadline(), Some(start + ms(240)));
  }
}
