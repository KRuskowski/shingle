use std::{
  collections::BTreeMap,
  time::{Duration, Instant},
};

use shingle_config::ExtraGesturesConfig;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GestureAction {
  BeginMove { x: f64, y: f64 },
  Close { x: f64, y: f64 },
}

struct TouchPoint {
  origin: (f64, f64),
  current: (f64, f64),
}

pub(crate) struct GestureState {
  config: ExtraGesturesConfig,
  touches: BTreeMap<u64, TouchPoint>,
  hold_started: Option<Instant>,
  move_triggered: bool,
  tap_started: Option<Instant>,
  tap_armed: Option<(Instant, (f64, f64))>,
  tap_cancelled: bool,
}

impl GestureState {
  pub(crate) fn new(config: ExtraGesturesConfig) -> Self {
    Self {
      config,
      touches: BTreeMap::new(),
      hold_started: None,
      move_triggered: false,
      tap_started: None,
      tap_armed: None,
      tap_cancelled: false,
    }
  }

  pub(crate) fn set_config(&mut self, config: ExtraGesturesConfig) {
    self.config = config;
    self.cancel();
  }

  pub(crate) fn touch_down(&mut self, id: u64, x: f64, y: f64, now: Instant) {
    if self.touches.is_empty() {
      self.reset_sequence();
    }
    self.touches.insert(
      id,
      TouchPoint {
        origin: (x, y),
        current: (x, y),
      },
    );
    self.tap_started.get_or_insert(now);
    if self.touches.len() > self.config.close_fingers as usize {
      self.tap_cancelled = true;
    }
    if self.touches.len() == self.config.close_fingers as usize
      && self.tap_started.is_some_and(|started| {
        now.duration_since(started) <= Duration::from_millis(150)
      })
    {
      self.tap_armed = self.center().map(|center| (now, center));
    }
    self.hold_started = (!self.move_triggered
      && self.touches.len() == self.config.move_fingers as usize)
      .then_some(now);
  }

  pub(crate) fn touch_motion(
    &mut self,
    id: u64,
    x: f64,
    y: f64,
    now: Instant,
  ) -> Option<GestureAction> {
    if let Some(touch) = self.touches.get_mut(&id) {
      touch.current = (x, y);
    }
    if self.tap_armed.is_some()
      && self.touches.len() == self.config.close_fingers as usize
      && let Some(center) = self.center()
    {
      self.tap_armed = self.tap_armed.map(|(armed, _)| (armed, center));
    }
    if self
      .touches
      .values()
      .any(|touch| distance(touch.origin, touch.current) > 50.0)
    {
      self.tap_cancelled = true;
    }
    if self
      .touches
      .values()
      .any(|touch| distance(touch.origin, touch.current) > 100.0)
    {
      self.hold_started = None;
    }
    self.poll(now)
  }

  pub(crate) fn poll(&mut self, now: Instant) -> Option<GestureAction> {
    let delay = Duration::from_millis(self.config.move_delay);
    if self.move_triggered
      || self.touches.len() != self.config.move_fingers as usize
      || self
        .hold_started
        .is_none_or(|started| now.duration_since(started) < delay)
    {
      return None;
    }
    self.move_triggered = true;
    self.hold_started = None;
    let (x, y) = self.center()?;
    Some(GestureAction::BeginMove { x, y })
  }

  pub(crate) fn touch_up(
    &mut self,
    id: u64,
    now: Instant,
  ) -> Option<GestureAction> {
    let (x, y) = self
      .tap_armed
      .map(|(_, center)| center)
      .or_else(|| self.center())?;
    let close = !self.move_triggered
      && !self.tap_cancelled
      && self.tap_armed.is_some_and(|(armed, _)| {
        now.duration_since(armed) <= Duration::from_millis(150)
      });
    self.touches.remove(&id);
    self.hold_started = None;
    let action = (close && self.touches.is_empty())
      .then_some(GestureAction::Close { x, y });
    if self.touches.is_empty() {
      self.reset_sequence();
    }
    action
  }

  pub(crate) fn cancel(&mut self) {
    self.touches.clear();
    self.reset_sequence();
  }

  fn center(&self) -> Option<(f64, f64)> {
    if self.touches.is_empty() {
      return None;
    }
    let (x, y) = self.touches.values().fold((0.0, 0.0), |(x, y), touch| {
      (x + touch.current.0, y + touch.current.1)
    });
    Some((x / self.touches.len() as f64, y / self.touches.len() as f64))
  }

  fn reset_sequence(&mut self) {
    self.hold_started = None;
    self.move_triggered = false;
    self.tap_started = None;
    self.tap_armed = None;
    self.tap_cancelled = false;
  }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
  (a.0 - b.0).hypot(a.1 - b.1)
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use super::{GestureAction, GestureState};

  fn config() -> shingle_config::ExtraGesturesConfig {
    shingle_config::ExtraGesturesConfig {
      move_fingers: 3,
      move_delay: 750,
      close_fingers: 2,
    }
  }

  #[test]
  fn hold_three_fingers_starts_move_once() {
    let start = Instant::now();
    let mut gestures = GestureState::new(config());
    for id in 0..3 {
      gestures.touch_down(id, 100.0 + id as f64, 200.0, start);
    }
    assert_eq!(gestures.poll(start + Duration::from_millis(749)), None);
    assert_eq!(
      gestures.poll(start + Duration::from_millis(750)),
      Some(GestureAction::BeginMove { x: 101.0, y: 200.0 })
    );
    assert_eq!(gestures.poll(start + Duration::from_secs(2)), None);
    for id in 0..3 {
      assert_eq!(gestures.touch_up(id, start + Duration::from_secs(2)), None);
    }
  }

  #[test]
  fn two_finger_tap_closes_at_the_touch_center() {
    let start = Instant::now();
    let mut gestures = GestureState::new(config());
    gestures.touch_down(1, 100.0, 100.0, start);
    gestures.touch_down(2, 200.0, 100.0, start + Duration::from_millis(50));
    assert_eq!(
      gestures.touch_up(1, start + Duration::from_millis(100)),
      None
    );
    assert_eq!(
      gestures.touch_up(2, start + Duration::from_millis(180)),
      Some(GestureAction::Close { x: 150.0, y: 100.0 })
    );
  }

  #[test]
  fn movement_and_late_release_cancel_tap_close() {
    let start = Instant::now();
    let mut gestures = GestureState::new(config());
    gestures.touch_down(1, 0.0, 0.0, start);
    gestures.touch_down(2, 10.0, 0.0, start);
    gestures.touch_motion(1, 100.0, 0.0, start);
    assert_eq!(gestures.touch_up(1, start), None);
    assert_eq!(gestures.touch_up(2, start), None);

    gestures.touch_down(1, 0.0, 0.0, start);
    gestures.touch_down(2, 10.0, 0.0, start);
    assert_eq!(gestures.touch_up(1, start), None);
    assert_eq!(
      gestures.touch_up(2, start + Duration::from_millis(151)),
      None
    );
  }
}
