//! The compositor's clock for protocol timestamps.
//!
//! Input events, presentation feedback and commit timing all use
//! `CLOCK_MONOTONIC`, the clock `wp_presentation` advertises and libinput
//! stamps its events with, so timestamps from the backend and from the
//! compositor itself can be compared.

use std::time::Duration;

/// Returns the current `CLOCK_MONOTONIC` time.
pub(crate) fn monotonic_now() -> Duration {
  let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
  Duration::new(time.tv_sec.max(0) as u64, time.tv_nsec.max(0) as u32)
}

/// Returns `time` in milliseconds, wrapped to 32 bits as Wayland input
/// events carry it.
pub(crate) fn millis(time: Duration) -> u32 {
  time.as_millis() as u32
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn monotonic_now_does_not_go_backwards() {
    let first = monotonic_now();
    let second = monotonic_now();
    assert!(second >= first);
    assert!(first > Duration::ZERO);
  }

  #[test]
  fn millis_wraps_to_32_bits() {
    assert_eq!(millis(Duration::from_micros(123_456_789)), 123_456);
    let wrapped = Duration::from_millis(u64::from(u32::MAX) + 6);
    assert_eq!(millis(wrapped), 5);
  }
}
