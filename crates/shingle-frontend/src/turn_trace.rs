//! Per-turn timing of the frontend, for finding what blocks its loop.
//!
//! With `SHINGLE_TRACE_TURNS=MS` set, every turn of the frontend (one
//! [`Frontend::process`](crate::Frontend::process) or one
//! [`Frontend::render`](crate::Frontend::render)) that takes longer than
//! `MS` milliseconds is logged to stderr with the time each of its phases
//! took, when it began (`CLOCK_MONOTONIC` seconds, the clock of
//! Python's `time.monotonic`) and how long the loop was idle before it:
//!
//! ```text
//! shingle: turn render 63.2 ms at 1234.567890 after 2.1 ms idle: tiles
//! 1.0, draw 60.1
//! ```
//!
//! (on one line).
//!
//! Without the variable nothing is measured.

use std::time::{Duration, Instant};

/// The variable that turns the trace on, with the threshold in
/// milliseconds.
pub const TRACE_VARIABLE: &str = "SHINGLE_TRACE_TURNS";

/// Measures turns and their phases; see the module documentation.
#[derive(Debug, Default)]
pub struct TurnTrace {
  /// Turns at least this long are logged; `None` measures nothing.
  threshold: Option<Duration>,
  /// The turn being measured.
  turn: Option<Turn>,
  /// When the last turn ended.
  last_end: Option<Instant>,
  /// How many turns were begun inside the current one and not ended.
  inner: u32,
}

#[derive(Debug)]
struct Turn {
  kind: &'static str,
  started: Instant,
  /// `CLOCK_MONOTONIC` at the start.
  clock: Duration,
  /// The end of the last phase, where the next begins.
  mark: Instant,
  phases: Vec<(&'static str, Duration)>,
  idle: Option<Duration>,
}

impl TurnTrace {
  /// The trace `SHINGLE_TRACE_TURNS` asks for, off without it. A value
  /// that is not a number of milliseconds traces turns over 10 ms.
  pub fn from_env() -> Self {
    Self::new(
      std::env::var(TRACE_VARIABLE)
        .ok()
        .map(|value| parse_threshold(&value)),
    )
  }

  /// A trace logging turns longer than `threshold`, or off with `None`.
  pub fn new(threshold: Option<Duration>) -> Self {
    Self {
      threshold,
      ..Self::default()
    }
  }

  /// Whether turns are measured.
  pub fn enabled(&self) -> bool {
    self.threshold.is_some()
  }

  /// Starts a turn of `kind` now.
  pub fn begin(&mut self, kind: &'static str) {
    if self.enabled() {
      self.begin_at(kind, Instant::now(), monotonic_now());
    }
  }

  /// Ends the current phase of the turn now, as `phase`.
  pub fn mark(&mut self, phase: &'static str) {
    if self.enabled() {
      self.mark_at(phase, Instant::now());
    }
  }

  /// Ends the turn now and logs it to stderr if it was long.
  pub fn end(&mut self) {
    if self.enabled()
      && let Some(line) = self.end_at(Instant::now())
    {
      eprintln!("{line}");
    }
  }

  fn begin_at(&mut self, kind: &'static str, now: Instant, clock: Duration) {
    // A turn inside another, such as a render the host runs from within
    // a turn, is part of the outer one.
    if self.threshold.is_none() {
      return;
    }
    if self.turn.is_some() {
      self.inner += 1;
      return;
    }
    self.turn = Some(Turn {
      kind,
      started: now,
      clock,
      mark: now,
      phases: Vec::new(),
      idle: self.last_end.map(|end| now.saturating_duration_since(end)),
    });
  }

  fn mark_at(&mut self, phase: &'static str, now: Instant) {
    if let Some(turn) = &mut self.turn {
      turn
        .phases
        .push((phase, now.saturating_duration_since(turn.mark)));
      turn.mark = now;
    }
  }

  /// Ends the turn at `now`; returns its log line if it took longer than
  /// the threshold.
  fn end_at(&mut self, now: Instant) -> Option<String> {
    let threshold = self.threshold?;
    if self.inner > 0 {
      self.inner -= 1;
      return None;
    }
    let turn = self.turn.take()?;
    self.last_end = Some(now);
    let total = now.saturating_duration_since(turn.started);
    (total > threshold).then(|| turn.line(total, now))
  }
}

impl Turn {
  fn line(&self, total: Duration, now: Instant) -> String {
    let mut phases = self
      .phases
      .iter()
      .map(|(name, time)| format!("{name} {}", milliseconds(*time)))
      .collect::<Vec<_>>();
    let rest = now.saturating_duration_since(self.mark);
    if !self.phases.is_empty() && !rest.is_zero() {
      phases.push(format!("rest {}", milliseconds(rest)));
    }
    let idle = self.idle.map_or(String::new(), |idle| {
      format!(" after {} ms idle", milliseconds(idle))
    });
    format!(
      "shingle: turn {} {} ms at {:.6}{idle}: {}",
      self.kind,
      milliseconds(total),
      self.clock.as_secs_f64(),
      phases.join(", ")
    )
  }
}

fn monotonic_now() -> Duration {
  let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
  Duration::new(
    u64::try_from(time.tv_sec).unwrap_or(0),
    u32::try_from(time.tv_nsec).unwrap_or(0),
  )
}

fn milliseconds(time: Duration) -> String {
  format!("{:.1}", time.as_secs_f64() * 1000.0)
}

/// The threshold `value` of `SHINGLE_TRACE_TURNS` names, 10 ms if it
/// names none.
fn parse_threshold(value: &str) -> Duration {
  value
    .trim()
    .parse::<f64>()
    .ok()
    .filter(|ms| ms.is_finite() && *ms >= 0.0)
    .map_or(Duration::from_millis(10), |ms| {
      Duration::from_secs_f64(ms / 1000.0)
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  const CLOCK: Duration = Duration::from_millis(1500);

  fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
  }

  #[test]
  fn thresholds_are_milliseconds_and_default_to_ten() {
    assert_eq!(parse_threshold("25"), ms(25));
    assert_eq!(parse_threshold(" 0.5 "), Duration::from_micros(500));
    assert_eq!(parse_threshold("yes"), ms(10));
    assert_eq!(parse_threshold("-3"), ms(10));
  }

  #[test]
  fn a_disabled_trace_measures_nothing() {
    let mut trace = TurnTrace::new(None);
    let start = Instant::now();
    trace.begin_at("process", start, CLOCK);
    trace.mark_at("dispatch", start + ms(50));
    assert!(trace.turn.is_none());
    assert_eq!(trace.end_at(start + ms(60)), None);
  }

  #[test]
  fn short_turns_are_not_logged() {
    let mut trace = TurnTrace::new(Some(ms(10)));
    let start = Instant::now();
    trace.begin_at("process", start, CLOCK);
    trace.mark_at("dispatch", start + ms(4));
    assert_eq!(trace.end_at(start + ms(10)), None);
  }

  #[test]
  fn long_turns_list_their_phases_and_the_idle_time_before() {
    let mut trace = TurnTrace::new(Some(ms(10)));
    let start = Instant::now();
    trace.begin_at("process", start, CLOCK);
    trace.end_at(start + ms(1));
    trace.begin_at("render", start + ms(5), CLOCK);
    trace.mark_at("tiles", start + ms(7));
    trace.mark_at("draw", start + ms(30));
    assert_eq!(
      trace.end_at(start + ms(31)).as_deref(),
      Some(
        "shingle: turn render 26.0 ms at 1.500000 after 4.0 ms idle: \
         tiles 2.0, draw 23.0, rest 1.0"
      )
    );
  }

  #[test]
  fn a_turn_begun_inside_another_belongs_to_it() {
    let mut trace = TurnTrace::new(Some(ms(0)));
    let start = Instant::now();
    trace.begin_at("process", start, CLOCK);
    trace.mark_at("dispatch", start + ms(2));
    trace.begin_at("render", start + ms(2), CLOCK);
    trace.mark_at("draw", start + ms(12));
    assert_eq!(trace.end_at(start + ms(12)), None);
    trace.mark_at("flush", start + ms(13));
    assert_eq!(
      trace.end_at(start + ms(13)).as_deref(),
      Some(
        "shingle: turn process 13.0 ms at 1.500000: dispatch 2.0, \
         draw 10.0, flush 1.0"
      )
    );
    assert_eq!(trace.end_at(start + ms(14)), None);
  }
}
