//! Wayfire's animation clock and the `animate` plugin's effects, as pure
//! functions of progress: what each effect does to a view at a given point
//! of its animation, with no GPU or compositor state. [`Timing`] is the
//! length and curve every animation of the frontend runs on.

use std::time::{Duration, Instant};

use shingle_config::Easing;
use shingle_render::animation_progress;

/// `easing`'s value at linear progress `linear`, exactly 1 once `linear`
/// reaches 1, as wf-config's `duration_t::progress` returns 1 for a
/// finished animation (wf-config:src/duration.cpp:145-159).
pub(crate) fn ease(easing: Easing, linear: f64) -> f64 {
  if linear >= 1.0 {
    return 1.0;
  }
  easing.apply(linear.max(0.0))
}

/// The length and curve of an animation, as a wf-config `animation` option
/// gives them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
  /// How long the animation runs.
  pub duration: Duration,
  /// How its progress is eased.
  pub easing: Easing,
}

impl Timing {
  /// `ms` milliseconds on `easing`.
  pub fn new(ms: u64, easing: Easing) -> Self {
    Self {
      duration: Duration::from_millis(ms),
      easing,
    }
  }

  /// `duration` on the linear curve.
  pub fn linear(duration: Duration) -> Self {
    Self {
      duration,
      easing: Easing::Linear,
    }
  }

  /// `duration` on the circle curve, which wf-config gives durations
  /// read from an integer option (wf-config:src/duration.cpp:28-31, the
  /// default of `duration_t`'s constructor).
  pub fn circle(duration: Duration) -> Self {
    Self {
      duration,
      easing: Easing::Circle,
    }
  }

  /// Linear progress at `now` of an animation started at `started`, in
  /// 0..=1: whether it has ended is `linear_progress(...) >= 1`.
  pub fn linear_progress(&self, started: Instant, now: Instant) -> f32 {
    animation_progress(started, now, self.duration)
  }

  /// Eased progress at `now` of an animation started at `started`, for
  /// interpolating; exactly 1 once it ended.
  pub fn progress(&self, started: Instant, now: Instant) -> f32 {
    eased_progress(started, now, self.duration, self.easing)
  }
}

/// The eased progress of an animation of `duration` started at `started`:
/// [`animation_progress`] through `easing`. Only for interpolating; whether
/// an animation has ended is a question for the linear progress, since an
/// overshooting curve passes 1 before the end.
pub(crate) fn eased_progress(
  started: Instant,
  now: Instant,
  duration: Duration,
  easing: Easing,
) -> f32 {
  ease(
    easing,
    f64::from(animation_progress(started, now, duration)),
  ) as f32
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn easing_ends_at_exactly_one() {
    assert_eq!(ease(Easing::Sigmoid, 1.0), 1.0);
    assert_eq!(ease(Easing::EaseOutElastic, 2.0), 1.0);
    assert_eq!(ease(Easing::Circle, 0.5), 0.75f64.sqrt());
    let start = Instant::now();
    let duration = Duration::from_millis(400);
    assert_eq!(
      eased_progress(start, start + duration / 2, duration, Easing::Circle),
      0.75f64.sqrt() as f32
    );
    assert_eq!(
      eased_progress(start, start + duration, duration, Easing::Sigmoid),
      1.0
    );
  }
}
