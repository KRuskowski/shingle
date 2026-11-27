//! Wayfire's animation durations: a length and an easing curve, as
//! wf-config parses the `animation` option type and evaluates its curves
//! (wf-config:src/duration.cpp).

use std::fmt;

use serde::Deserialize;

use crate::ConfigError;

/// An easing curve of a wf-config `animation` option: how an animation's
/// linear progress, 0 to 1 over its length, becomes the fraction of the
/// way from its start to its end ([`Easing::apply`]). Written as Wayfire
/// writes it, `linear`, `circle`, `sigmoid`, `easeOutElastic` or
/// `cubic-bezier X1 Y1 X2 Y2`.
#[derive(
  Clone, Copy, Debug, Default, Deserialize, PartialEq, serde::Serialize,
)]
#[serde(try_from = "String", into = "String")]
pub enum Easing {
  /// `x` (wf-config:src/duration.cpp:28).
  Linear,
  /// A quarter circle, `sqrt(2x - x²)` (wf-config:src/duration.cpp:30), the
  /// curve of a duration written without one.
  #[default]
  Circle,
  /// A logistic curve through (0.5, 0.5), `(1 + e⁻⁶) / (1 + e^(6 - 12x))`
  /// (wf-config:src/duration.cpp:33-35). It starts at 0.0025, not 0.
  Sigmoid,
  /// An overshooting spring that settles at 1
  /// (wf-config:src/duration.cpp:302-329).
  EaseOutElastic,
  /// The CSS cubic Bézier through (0, 0), (x1, y1), (x2, y2) and (1, 1),
  /// solved for `x` with ten Newton steps (wf-config:src/duration.cpp:37-57).
  CubicBezier([f64; 4]),
}

impl Easing {
  /// The eased value of linear progress `x`, as the wf-config curve of the
  /// same name computes it. `x` is not clamped; animations pass 0..=1.
  pub fn apply(self, x: f64) -> f64 {
    match self {
      Self::Linear => x,
      Self::Circle => (2.0 * x - x * x).sqrt(),
      Self::Sigmoid => {
        let max = 1.0 + (-6.0f64).exp();
        max / (1.0 + (-12.0 * x + 6.0).exp())
      }
      Self::EaseOutElastic => ease_out_elastic(x),
      Self::CubicBezier([x1, y1, x2, y2]) => cubic_bezier(x1, y1, x2, y2, x),
    }
  }

  /// Parses a curve as wf-config reads one after the length
  /// (wf-config:src/duration.cpp:381-402): one of the names, or
  /// `cubic-bezier` followed by up to four numbers, missing ones taking
  /// 0, 0, 1 and 1.
  fn parse(words: &[&str]) -> Option<Self> {
    let (name, rest) = words.split_first()?;
    let curve = match *name {
      "linear" => Self::Linear,
      "circle" => Self::Circle,
      "sigmoid" => Self::Sigmoid,
      "easeOutElastic" => Self::EaseOutElastic,
      "cubic-bezier" => {
        if rest.len() > 4 {
          return None;
        }
        let mut points = [0.0, 0.0, 1.0, 1.0];
        for (point, word) in points.iter_mut().zip(rest) {
          *point = word.parse().ok().filter(|value: &f64| value.is_finite())?;
        }
        return Some(Self::CubicBezier(points));
      }
      _ => return None,
    };
    rest.is_empty().then_some(curve)
  }
}

impl fmt::Display for Easing {
  /// The name wf-config writes back (wf-config:src/duration.cpp:394-398).
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Linear => formatter.write_str("linear"),
      Self::Circle => formatter.write_str("circle"),
      Self::Sigmoid => formatter.write_str("sigmoid"),
      Self::EaseOutElastic => formatter.write_str("easeOutElastic"),
      Self::CubicBezier([x1, y1, x2, y2]) => {
        write!(formatter, "cubic-bezier {x1:.6} {y1:.6} {x2:.6} {y2:.6}")
      }
    }
  }
}

impl TryFrom<String> for Easing {
  type Error = ConfigError;

  fn try_from(value: String) -> Result<Self, Self::Error> {
    let words = value.split_whitespace().collect::<Vec<_>>();
    Self::parse(&words).ok_or_else(|| {
      ConfigError::Invalid(format!("invalid easing curve {value:?}"))
    })
  }
}

impl From<Easing> for String {
  fn from(value: Easing) -> Self {
    value.to_string()
  }
}

/// A wf-config `animation` value: a length in milliseconds and its curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationDuration {
  /// The length; wf-config's animations last at least 1 ms
  /// (wf-config:src/duration.cpp:112).
  pub ms: u64,
  /// The curve.
  pub easing: Easing,
}

impl AnimationDuration {
  /// Parses a Wayfire `animation` value as wf-config does
  /// (wf-config:src/duration.cpp:354-420): an integer alone is
  /// milliseconds with the `circle` curve; otherwise a number, `ms` or `s`
  /// (separated or not), and an optional curve, `circle` without one.
  /// Fractions of a millisecond are cut off. Shingle refuses negative
  /// lengths, which Wayfire would run for 1 ms.
  pub fn parse(value: &str) -> Result<Self, ConfigError> {
    let invalid =
      || ConfigError::Invalid(format!("invalid animation duration {value:?}"));
    let words = value.split_whitespace().collect::<Vec<_>>();
    let (first, rest) = words.split_first().ok_or_else(invalid)?;
    // Format 1: an integer and nothing else, which wf-config reads when
    // the whole value is the integer as it would print it.
    if words.len() == 1
      && let Ok(ms) = first.parse::<u64>()
      && ms.to_string() == value
    {
      return Ok(Self {
        ms,
        easing: Easing::Circle,
      });
    }
    // Format 2: `N <s|ms> [easing]`; `>>` reads the number up to the
    // first character that cannot continue it, so `400ms` is `400 ms`.
    let split = first
      .find(|c: char| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
      .unwrap_or(first.len());
    let (number, suffix) = first.split_at(split);
    let (suffix, curve) = if suffix.is_empty() {
      let (suffix, curve) = rest.split_first().ok_or_else(invalid)?;
      (*suffix, curve)
    } else {
      (suffix, rest)
    };
    let number = number.parse::<f64>().map_err(|_| invalid())?;
    let factor = match suffix {
      "ms" => 1.0,
      "s" => 1000.0,
      _ => return Err(invalid()),
    };
    let easing = if curve.is_empty() {
      Easing::Circle
    } else {
      Easing::parse(curve).ok_or_else(invalid)?
    };
    let ms = (number * factor).trunc();
    if !ms.is_finite() || ms < 0.0 || ms > i32::MAX as f64 {
      return Err(invalid());
    }
    Ok(Self {
      ms: ms as u64,
      easing,
    })
  }
}

impl fmt::Display for AnimationDuration {
  /// As wf-config writes the value, `400ms circle`
  /// (wf-config:src/duration.cpp:423-426).
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}ms {}", self.ms, self.easing)
  }
}

/// wf-config's `ease_out_elastic`, with its single-precision constants
/// (wf-config:src/duration.cpp:302-329).
fn ease_out_elastic(x: f64) -> f64 {
  let d = 1.0f32;
  let p = d * 0.6f32;
  if x == 0.0 {
    return 0.0;
  }
  let x = x / f64::from(d);
  if x == 1.0 {
    return 1.0;
  }
  let a = 1.0f64;
  let s = f64::from(p * 0.25f32);
  let p = f64::from(p);
  let tau = 2.0 * (-1.0f64).acos();
  a * 2.0f64.powf(-10.0 * x) * ((x * f64::from(d) - s) * tau / p).sin() + 1.0
}

fn bezier(t: f64, p0: f64, p1: f64, p2: f64, p3: f64) -> f64 {
  let u = 1.0 - t;
  u * u * u * p0 + 3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t * p3
}

/// wf-config's `get_cubic_bezier` (wf-config:src/duration.cpp:37-57).
fn cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
  let mut t = x;
  for _ in 0..10 {
    let f = bezier(t, 0.0, x1, x2, 1.0) - x;
    let df = 3.0 * (1.0 - t) * (1.0 - t) * x1
      + 6.0 * (1.0 - t) * t * (x2 - x1)
      + 3.0 * t * t * (1.0 - x2);
    if f.abs() < 1e-6 {
      break;
    }
    t -= f / df;
  }
  bezier(t, 0.0, y1, y2, 1.0)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn close(actual: f64, expected: f64) {
    assert!(
      (actual - expected).abs() < 1e-9,
      "{actual} is not {expected}"
    );
  }

  #[test]
  fn the_curves_follow_wf_config_formulas() {
    for x in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
      close(Easing::Linear.apply(x), x);
      close(Easing::Circle.apply(x), (2.0 * x - x * x).sqrt());
      close(
        Easing::Sigmoid.apply(x),
        (1.0 + (-6.0f64).exp()) / (1.0 + (6.0 - 12.0 * x).exp()),
      );
    }
    close(Easing::Circle.apply(0.5), 0.75f64.sqrt());
    close(Easing::Sigmoid.apply(0.5), (1.0 + (-6.0f64).exp()) / 2.0);
    close(Easing::Sigmoid.apply(1.0), 1.0);
    let start = Easing::Sigmoid.apply(0.0);
    assert!(start > 0.002 && start < 0.003, "{start}");
  }

  #[test]
  fn ease_out_elastic_overshoots_and_settles() {
    close(Easing::EaseOutElastic.apply(0.0), 0.0);
    close(Easing::EaseOutElastic.apply(1.0), 1.0);
    // 2^-1 * sin((0.1 - 0.15) * 2pi / 0.6) + 1, with 0.6 and 0.15 in
    // single precision as wf-config has them.
    let p = f64::from(0.6f32);
    let s = f64::from(0.6f32 * 0.25f32);
    let tau = std::f64::consts::TAU;
    close(
      Easing::EaseOutElastic.apply(0.1),
      0.5 * ((0.1 - s) * tau / p).sin() + 1.0,
    );
    // The spring passes beyond 1 before settling.
    assert!(Easing::EaseOutElastic.apply(0.2) > 1.0);
  }

  #[test]
  fn cubic_bezier_solves_for_x() {
    // The identity curve and CSS `ease`.
    let identity = Easing::CubicBezier([0.0, 0.0, 1.0, 1.0]);
    for x in [0.0, 0.3, 0.6, 1.0] {
      // Newton stops within 1e-6 of x.
      assert!((identity.apply(x) - x).abs() < 1e-6);
    }
    let ease = Easing::CubicBezier([0.25, 0.1, 0.25, 1.0]);
    assert!((ease.apply(0.5) - 0.8024033877399112).abs() < 1e-5);
    close(ease.apply(1.0), 1.0);
  }

  #[test]
  fn durations_parse_as_wf_config_reads_them() {
    let parse = |value| AnimationDuration::parse(value).expect(value);
    assert_eq!(
      parse("300"),
      AnimationDuration {
        ms: 300,
        easing: Easing::Circle
      }
    );
    assert_eq!(parse("400ms").easing, Easing::Circle);
    assert_eq!(parse("250ms linear").easing, Easing::Linear);
    assert_eq!(parse("2s linear").ms, 2000);
    assert_eq!(parse("0.4s").ms, 400);
    assert_eq!(parse("500 ms sigmoid").easing, Easing::Sigmoid);
    assert_eq!(parse("1.9ms easeOutElastic").ms, 1);
    assert_eq!(
      parse("300ms cubic-bezier 0.25 0.1").easing,
      Easing::CubicBezier([0.25, 0.1, 1.0, 1.0])
    );
    for invalid in [
      "",
      "fast",
      "300ms bounce",
      "300 minutes",
      "300ms linear extra",
      "-5ms",
      "300ms cubic-bezier 1 2 3 4 5",
    ] {
      assert!(AnimationDuration::parse(invalid).is_err(), "{invalid:?}");
    }
  }

  #[test]
  fn durations_and_curves_print_as_wf_config_writes_them() {
    assert_eq!(
      AnimationDuration::parse("0.25s").unwrap().to_string(),
      "250ms circle"
    );
    let curve = Easing::try_from("cubic-bezier 0.25 0.1 0.25 1".to_owned())
      .expect("valid");
    assert_eq!(
      curve.to_string(),
      "cubic-bezier 0.250000 0.100000 0.250000 1.000000"
    );
    assert_eq!(Easing::try_from(curve.to_string()).unwrap(), curve);
    assert!(Easing::try_from("bounce".to_owned()).is_err());
  }
}
