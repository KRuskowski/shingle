//! The arithmetic of the minimal style's movement: easing curves, a
//! value easing towards a target that can be retargeted halfway, the
//! wrong password's damped shake and the verifying outline's breathing.
//! Everything is a pure function of time, so that a frame drawn at any
//! moment shows where the animation is then, whatever the frame rate.

use std::{
  f32::consts::TAU,
  time::{Duration, Instant},
};

/// Ease-out cubic: fast at first, settling softly.
pub fn ease_out(t: f32) -> f32 {
  let t = t.clamp(0.0, 1.0);
  1.0 - (1.0 - t).powi(3)
}

/// Ease-in-out cubic, for a value leaving as gently as it arrived.
pub fn ease_in_out(t: f32) -> f32 {
  let t = t.clamp(0.0, 1.0);
  if t < 0.5 {
    4.0 * t * t * t
  } else {
    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
  }
}

/// How far `now` is through the span of `length` starting at `start`,
/// from 0 to 1.
pub fn progress(start: Instant, length: Duration, now: Instant) -> f32 {
  if length.is_zero() {
    return 1.0;
  }
  (now.saturating_duration_since(start).as_secs_f32() / length.as_secs_f32())
    .clamp(0.0, 1.0)
}

/// A value easing out ([`ease_out`]) from `from` to `to` over `length`
/// from `start`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tween {
  from: f32,
  to: f32,
  start: Instant,
  length: Duration,
}

impl Tween {
  /// A value resting at `value`.
  pub fn at_rest(value: f32, now: Instant) -> Self {
    Self {
      from: value,
      to: value,
      start: now,
      length: Duration::ZERO,
    }
  }

  /// The value at `now`.
  pub fn value(&self, now: Instant) -> f32 {
    let t = progress(self.start, self.length, now);
    self.from + (self.to - self.from) * ease_out(t)
  }

  /// Where it is heading.
  pub fn target(&self) -> f32 {
    self.to
  }

  /// Heads for `to` from wherever it is at `at`, over `length`; a value
  /// already heading there keeps its course.
  pub fn retarget(&mut self, to: f32, at: Instant, length: Duration) {
    if self.to == to {
      return;
    }
    *self = Self {
      from: self.value(at),
      to,
      start: at,
      length,
    };
  }

  /// Whether it is still moving at `now`.
  pub fn moving(&self, now: Instant) -> bool {
    self.from != self.to && now < self.start + self.length
  }
}

/// The wrong password's shake: a sideways sine of `amplitude` pixels at
/// `frequency` per second, damped with time constant `damping`, over in
/// `length`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shake {
  /// The first swing's size, in logical pixels.
  pub amplitude: f32,
  /// Swings per second.
  pub frequency: f32,
  /// The damping's time constant.
  pub damping: Duration,
  /// When it is still enough to stop.
  pub length: Duration,
}

impl Shake {
  /// The sideways offset `elapsed` after it began; 0 once over.
  pub fn offset(&self, elapsed: Duration) -> f32 {
    if elapsed >= self.length {
      return 0.0;
    }
    let t = elapsed.as_secs_f32();
    let decay = (-t / self.damping.as_secs_f32()).exp();
    self.amplitude * decay * (TAU * self.frequency * t).sin()
  }
}

/// The verifying outline's opacity `elapsed` after verifying began: a
/// slow cosine from 1 down to `low` and back, once per `period`.
pub fn breathing(elapsed: Duration, period: Duration, low: f32) -> f32 {
  let phase = elapsed.as_secs_f32() / period.as_secs_f32().max(1e-3);
  let wave = 0.5 + 0.5 * (TAU * phase).cos();
  low + (1.0 - low) * wave
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
  }

  #[test]
  fn curves_start_at_zero_and_end_at_one() {
    for curve in [ease_out, ease_in_out] {
      assert_eq!(curve(0.0), 0.0);
      assert_eq!(curve(1.0), 1.0);
      assert_eq!(curve(-1.0), 0.0);
      assert_eq!(curve(2.0), 1.0);
      let mut last = 0.0;
      for step in 1..=20 {
        let value = curve(step as f32 / 20.0);
        assert!(value >= last, "monotonic");
        last = value;
      }
    }
    // Ease-out is past half way at a quarter; in-out at half is half.
    assert!(ease_out(0.25) > 0.5);
    assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
  }

  #[test]
  fn a_tween_eases_and_can_be_retargeted_halfway() {
    let start = Instant::now();
    let mut tween = Tween::at_rest(0.0, start);
    assert!(!tween.moving(start));
    tween.retarget(1.0, start, ms(400));
    assert!(tween.moving(start + ms(100)));
    let half = tween.value(start + ms(200));
    assert!(half > 0.8 && half < 1.0, "{half}");
    assert_eq!(tween.value(start + ms(400)), 1.0);
    assert!(!tween.moving(start + ms(400)));
    // Retargeting to the same value keeps the course.
    let before = tween;
    tween.retarget(1.0, start + ms(100), ms(50));
    assert_eq!(tween, before);
    // Turning back halfway starts where it was, with no jump.
    tween.retarget(0.0, start + ms(200), ms(400));
    assert!((tween.value(start + ms(200)) - half).abs() < 1e-6);
    assert_eq!(tween.value(start + ms(600)), 0.0);
    assert_eq!(tween.target(), 0.0);
  }

  #[test]
  fn the_shake_is_small_damped_and_short() {
    let shake = Shake {
      amplitude: 6.0,
      frequency: 7.0,
      damping: ms(140),
      length: ms(560),
    };
    let samples = (0..560).map(|t| shake.offset(ms(t))).collect::<Vec<_>>();
    let biggest = samples.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
    assert!(biggest > 3.0 && biggest <= 6.0, "{biggest}");
    // Both ways, and much smaller towards the end.
    assert!(samples.iter().any(|x| *x > 1.0));
    assert!(samples.iter().any(|x| *x < -1.0));
    let late = samples[450..].iter().fold(0.0_f32, |a, b| a.max(b.abs()));
    assert!(late < 0.3, "{late}");
    assert_eq!(shake.offset(ms(560)), 0.0);
    assert_eq!(shake.offset(Duration::ZERO), 0.0);
  }

  #[test]
  fn breathing_is_a_slow_soft_wave() {
    let period = ms(1600);
    assert_eq!(breathing(Duration::ZERO, period, 0.35), 1.0);
    assert!((breathing(ms(800), period, 0.35) - 0.35).abs() < 1e-5);
    assert!((breathing(ms(1600), period, 0.35) - 1.0).abs() < 1e-5);
    // Never a jump between two frames at 60 Hz.
    for step in 0..200 {
      let a = breathing(ms(step * 16), period, 0.35);
      let b = breathing(ms(step * 16 + 16), period, 0.35);
      assert!((a - b).abs() < 0.05);
    }
  }

  #[test]
  fn progress_is_clamped() {
    let start = Instant::now();
    assert_eq!(progress(start, ms(100), start), 0.0);
    assert_eq!(progress(start, ms(100), start + ms(50)), 0.5);
    assert_eq!(progress(start, ms(100), start + ms(500)), 1.0);
    assert_eq!(progress(start, Duration::ZERO, start), 1.0);
  }
}
