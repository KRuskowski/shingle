//! Wayfire's vswipe: a touchpad swipe drags the workspace wall, and
//! lifting the fingers slides it to a workspace.
//!
//! A port of wayfire:plugins/single_plugins/vswipe.cpp and
//! vswipe-processing.hpp at the commit docs/design/wayfire-gaps.md pins.
//! [`Swipe`] holds one swipe's state and does the arithmetic; the frontend
//! decides which swipes it gets, starts it, switches the workspace it
//! chooses and draws the wall at [`Swipe::viewport`].
//!
//! Positions are in workspaces. The wall's offset (Wayfire's
//! `smooth_delta`) is positive when the fingers moved right or down,
//! which shows the workspace to the left or above.

use std::time::{Duration, Instant};

use shingle_config::VswipeConfig;

/// How far, in workspaces, a swipe must move before its direction is
/// decided. Wayfire's `initial_direction_threshold`.
const INITIAL_DIRECTION_THRESHOLD: f64 = 0.05;

/// How far a swipe locked to one axis must move along the other to turn
/// diagonal with free movement. Wayfire's `secondary_direction_threshold`.
const SECONDARY_DIRECTION_THRESHOLD: f64 = 0.3;

/// The steepest slope, either way, a diagonal swipe may start with:
/// tan(60°). Wayfire's `diagonal_threshold`.
const DIAGONAL_THRESHOLD: f64 = 1.73;

/// The axes a swipe moves the wall along.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Axes {
  /// Along the row.
  pub horizontal: bool,
  /// Along the column.
  pub vertical: bool,
}

impl Axes {
  /// Not decided yet.
  pub const NONE: Self = Self {
    horizontal: false,
    vertical: false,
  };
  /// Horizontal only.
  pub const HORIZONTAL: Self = Self {
    horizontal: true,
    vertical: false,
  };
  /// Vertical only.
  pub const VERTICAL: Self = Self {
    horizontal: false,
    vertical: true,
  };
  /// Both: free movement.
  pub const DIAGONAL: Self = Self {
    horizontal: true,
    vertical: true,
  };
}

/// One value moving from `start` to `end` over the swipe's duration,
/// as Wayfire's `timed_transition_t`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Transition {
  start: f64,
  end: f64,
}

impl Transition {
  fn at(self, progress: f64) -> f64 {
    self.start + (self.end - self.start) * progress
  }
}

/// What [`Swipe::update`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Update {
  /// The direction is not decided yet; nothing moved.
  Pending,
  /// This update decided the direction and moved the wall. The frontend
  /// starts the swipe now, or drops it if it cannot.
  Started,
  /// The wall moved.
  Moved,
}

/// One touchpad swipe of vswipe, from its first update to the end of
/// the slide after the fingers lift.
#[derive(Clone, Debug)]
pub struct Swipe {
  /// Whether the fingers are still down.
  swiping: bool,
  direction: Axes,
  /// Absolute motion before the direction was decided.
  initial_deltas: (f64, f64),
  /// Motion since the start, in workspaces.
  delta_sum: (f64, f64),
  /// The raw motion of the update before the last.
  delta_prev: (f64, f64),
  /// The raw motion of the last update.
  delta_last: (f64, f64),
  /// The workspace the swipe started on, and the grid.
  workspace: (i32, i32),
  grid: (i32, i32),
  /// The wall's offset on each axis.
  dx: Transition,
  dy: Transition,
  /// When the offsets started moving towards their ends; `None` before
  /// the first update that moved them.
  started: Option<Instant>,
  duration: Duration,
}

impl Swipe {
  /// A swipe that begins on `workspace` (column, row) of a `grid`
  /// (columns, rows), as Wayfire's `on_swipe_begin` sets it up.
  pub fn begin(workspace: (i32, i32), grid: (i32, i32)) -> Self {
    Self {
      swiping: true,
      direction: Axes::NONE,
      initial_deltas: (0.0, 0.0),
      delta_sum: (0.0, 0.0),
      delta_prev: (0.0, 0.0),
      delta_last: (0.0, 0.0),
      workspace,
      grid,
      dx: Transition::default(),
      dy: Transition::default(),
      started: None,
      duration: Duration::ZERO,
    }
  }

  /// The axes the swipe moves along; [`Axes::NONE`] until decided.
  pub fn direction(&self) -> Axes {
    self.direction
  }

  /// The workspace the swipe began on.
  pub fn workspace(&self) -> (i32, i32) {
    self.workspace
  }

  /// Whether the fingers are still down.
  pub fn swiping(&self) -> bool {
    self.swiping
  }

  /// How far the offsets have come, from 0 to 1, linearly: Shingle's
  /// animations ignore Wayfire's easing curves.
  fn progress(&self, now: Instant) -> f64 {
    match self.started {
      Some(started) if !self.duration.is_zero() => {
        (now.saturating_duration_since(started).as_secs_f64()
          / self.duration.as_secs_f64())
        .min(1.0)
      }
      _ => 1.0,
    }
  }

  /// Whether the offsets are still moving, as Wayfire's
  /// `duration_t::running`.
  pub fn running(&self, now: Instant) -> bool {
    self.progress(now) < 1.0
  }

  /// Whether the swipe is over: the fingers lifted and the slide ended.
  /// Wayfire's `post_frame` finishes the swipe then.
  pub fn finished(&self, now: Instant) -> bool {
    !self.swiping && !self.running(now)
  }

  /// Restarts the offsets' clock, as Wayfire's `smooth_delta.start()`.
  fn restart(&mut self, now: Instant, config: &VswipeConfig) {
    self.started = Some(now);
    self.duration = Duration::from_millis(config.duration_ms);
  }

  /// The wall's offsets now, in workspaces.
  pub fn offset(&self, now: Instant) -> (f64, f64) {
    let progress = self.progress(now);
    (self.dx.at(progress), self.dy.at(progress))
  }

  /// The ends the offsets move to.
  pub fn offset_end(&self) -> (f64, f64) {
    (self.dx.end, self.dy.end)
  }

  /// The wall position shown now: the column and row, fractional, at
  /// the output's top-left corner. Wayfire's `on_frame` interpolates
  /// between the start workspace and its neighbour along each axis of
  /// the direction; the other axis stays on the start workspace.
  pub fn viewport(&self, now: Instant) -> (f64, f64) {
    let (dx, dy) = self.offset(now);
    let (x, y) = (f64::from(self.workspace.0), f64::from(self.workspace.1));
    (
      if self.direction.horizontal { x - dx } else { x },
      if self.direction.vertical { y - dy } else { y },
    )
  }

  /// Handles one update of the swipe, by (`dx`, `dy`) pixels of finger
  /// motion, as Wayfire's `on_swipe_update`.
  pub fn update(
    &mut self,
    dx: f64,
    dy: f64,
    config: &VswipeConfig,
    now: Instant,
  ) -> Update {
    if !self.swiping {
      return Update::Pending;
    }
    let factor = config.speed_factor;
    self.delta_sum.0 += dx / factor;
    self.delta_sum.1 += dy / factor;
    let mut outcome = Update::Moved;
    if self.direction == Axes::NONE {
      self.initial_deltas.0 += (dx / factor).abs();
      self.initial_deltas.1 += (dy / factor).abs();
      self.direction = direction(self.initial_deltas, self.grid, config);
      if self.direction == Axes::NONE {
        return Update::Pending;
      }
      outcome = Update::Started;
    } else if self.direction != Axes::DIAGONAL && config.enable_free_movement {
      // Consider promoting the swipe to diagonal movement.
      let other = if self.direction == Axes::HORIZONTAL {
        self.delta_sum.1
      } else {
        self.delta_sum.0
      };
      if other.abs() > SECONDARY_DIRECTION_THRESHOLD {
        self.direction = Axes::DIAGONAL;
      }
    }
    self.delta_prev = self.delta_last;
    let progress = self.progress(now);
    let step = |delta: f64, total: Transition, workspace: i32, count: i32| {
      let processed = process_delta(
        delta / factor,
        total.at(progress),
        workspace,
        count,
        config.speed_cap,
        config.enable_free_movement,
      );
      let end = total.end + processed;
      let start = if config.enable_smooth_transition {
        total.at(progress)
      } else {
        end
      };
      Transition { start, end }
    };
    if self.direction.horizontal {
      self.dx = step(dx, self.dx, self.workspace.0, self.grid.0);
    }
    if self.direction.vertical {
      self.dy = step(dy, self.dy, self.workspace.1, self.grid.1);
    }
    self.delta_last = (dx, dy);
    self.restart(now, config);
    outcome
  }

  /// Lifts the fingers, as Wayfire's `on_swipe_end`: picks the workspace
  /// to slide to, starts the slide there and returns it. Wayfire ignores
  /// whether libinput cancelled the swipe, and so does this.
  pub fn end(&mut self, config: &VswipeConfig, now: Instant) -> (i32, i32) {
    self.swiping = false;
    let move_threshold = config.threshold.clamp(0.0, 1.0);
    let fast_threshold = config.delta_threshold.clamp(0.0, 1000.0);
    let mut target_delta = (0, 0);
    let mut target = self.workspace;
    if self.direction.horizontal {
      target_delta.0 = finish_target(
        self.dx.end,
        self.workspace.0,
        self.grid.0,
        self.delta_prev.0 + self.delta_last.0,
        move_threshold,
        fast_threshold,
        config.enable_free_movement,
      );
      target.0 -= target_delta.0;
    }
    if self.direction.vertical {
      target_delta.1 = finish_target(
        self.dy.end,
        self.workspace.1,
        self.grid.1,
        self.delta_prev.1 + self.delta_last.1,
        move_threshold,
        fast_threshold,
        config.enable_free_movement,
      );
      target.1 -= target_delta.1;
    }
    let (x, y) = self.offset(now);
    self.dx = Transition {
      start: x,
      end: f64::from(target_delta.0),
    };
    self.dy = Transition {
      start: y,
      end: f64::from(target_delta.1),
    };
    self.restart(now, config);
    target
  }
}

/// Whether accumulated absolute motion `deltas` is diagonal: not too
/// steep either way, and far enough. Wayfire's `is_diagonal`.
fn is_diagonal(deltas: (f64, f64)) -> bool {
  let slope = deltas.0 / deltas.1;
  // A NaN or infinite slope (no vertical motion) is not diagonal.
  let diagonal =
    slope.clamp(1.0 / DIAGONAL_THRESHOLD, DIAGONAL_THRESHOLD) == slope;
  diagonal
    && deltas.0 * deltas.0 + deltas.1 * deltas.1
      >= INITIAL_DIRECTION_THRESHOLD * INITIAL_DIRECTION_THRESHOLD
}

/// The direction accumulated absolute motion `deltas` decides on a
/// `grid`, or [`Axes::NONE`]. Wayfire's `calculate_direction`: free
/// movement takes a diagonal swipe on any grid; one axis needs more
/// than one workspace along it and that axis enabled.
pub fn direction(
  deltas: (f64, f64),
  grid: (i32, i32),
  config: &VswipeConfig,
) -> Axes {
  let horizontal =
    deltas.0 > INITIAL_DIRECTION_THRESHOLD && deltas.0 > deltas.1;
  let vertical = deltas.1 > INITIAL_DIRECTION_THRESHOLD && deltas.1 > deltas.0;
  if is_diagonal(deltas) && config.enable_free_movement {
    Axes::DIAGONAL
  } else if horizontal && grid.0 > 1 && config.enable_horizontal {
    Axes::HORIZONTAL
  } else if vertical && grid.1 > 1 && config.enable_vertical {
    Axes::VERTICAL
  } else {
    Axes::NONE
  }
}

/// How far one update of `delta` workspaces moves the offset
/// `accumulated` of a swipe that started on `workspace` of `count`
/// along the axis: capped at `speed_cap`, and slowed down like a
/// rubber band beyond the grid's edges or, without free movement, the
/// neighbouring workspace. Wayfire's `vswipe_process_delta`.
pub fn process_delta(
  delta: f64,
  accumulated: f64,
  workspace: i32,
  count: i32,
  speed_cap: f64,
  free_movement: bool,
) -> f64 {
  let workspace = f64::from(workspace);
  // The slowdown below must be applied differently for going out of
  // bounds.
  let mut offset = if free_movement {
    0.0_f64.copysign(accumulated)
  } else {
    accumulated
  };
  if workspace - accumulated < 0.0 {
    offset = (accumulated - accumulated.floor()) + 1.0;
  }
  if workspace - accumulated > f64::from(count) - 1.0 {
    offset = (accumulated - accumulated.ceil()) - 1.0;
  }
  // An ease-in of the whole swiped distance slows the current delta
  // down, for a rubber band's resistance when going too far.
  let ease = 1.0 - (offset.abs() - 0.025).powi(4);
  // Moving further towards the limit slows down to almost nothing;
  // reversing is easier.
  let floor = if delta.is_sign_negative() == offset.is_sign_negative() {
    0.005
  } else {
    0.2
  };
  delta.clamp(-speed_cap, speed_cap) * ease.clamp(floor, 1.0)
}

/// The number of workspaces to move, positive towards lower indices, for
/// a swipe that ended at offset `accumulated` on `workspace` of `count`,
/// with `last_deltas` pixels of motion in its last two updates. Past
/// `move_threshold` of a workspace, or with a flick faster than
/// `fast_threshold`, it moves on; it never leaves the grid, and without
/// free movement moves at most one. Wayfire's `vswipe_finish_target`.
pub fn finish_target(
  accumulated: f64,
  workspace: i32,
  count: i32,
  last_deltas: f64,
  move_threshold: f64,
  fast_threshold: f64,
  free_movement: bool,
) -> i32 {
  let mut target = 0;
  if accumulated > 0.0 {
    target = accumulated.floor() as i32;
    if accumulated - f64::from(target) > move_threshold
      || ((!free_movement || target == 0) && last_deltas > fast_threshold)
    {
      target += 1;
    }
    if workspace - target < 0 {
      target = workspace;
    }
  } else if accumulated < 0.0 {
    target = accumulated.ceil() as i32;
    if accumulated - f64::from(target) < -move_threshold
      || ((!free_movement || target == 0) && last_deltas < -fast_threshold)
    {
      target -= 1;
    }
    if workspace - target > count - 1 {
      target = workspace - count + 1;
    }
  }
  if !free_movement {
    target = target.clamp(-1, 1);
  }
  target
}

#[cfg(test)]
mod tests {
  use super::*;

  fn config() -> VswipeConfig {
    VswipeConfig::default()
  }

  fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
  }

  /// Feeds `steps` updates of (`dx`, `dy`) at `now`.
  fn swipe(
    swipe: &mut Swipe,
    steps: usize,
    (dx, dy): (f64, f64),
    config: &VswipeConfig,
    now: Instant,
  ) -> Vec<Update> {
    (0..steps)
      .map(|_| swipe.update(dx, dy, config, now))
      .collect()
  }

  #[test]
  fn the_direction_locks_once_past_the_initial_threshold() {
    let config = config();
    let now = Instant::now();
    let mut horizontal = Swipe::begin((1, 1), (3, 3));
    // 10 px is 0.039 of a workspace, under 0.05.
    assert_eq!(horizontal.update(10.0, 2.0, &config, now), Update::Pending);
    assert_eq!(horizontal.direction(), Axes::NONE);
    assert_eq!(horizontal.offset(now), (0.0, 0.0));
    assert_eq!(horizontal.update(10.0, 2.0, &config, now), Update::Started);
    assert_eq!(horizontal.direction(), Axes::HORIZONTAL);
    assert_eq!(horizontal.update(10.0, 50.0, &config, now), Update::Moved);
    // Locked: vertical motion now moves nothing.
    assert_eq!(horizontal.direction(), Axes::HORIZONTAL);
    assert_eq!(horizontal.offset(now).1, 0.0);
    assert!(horizontal.offset(now).0 > 0.0);
    let mut vertical = Swipe::begin((1, 1), (3, 3));
    swipe(&mut vertical, 2, (-1.0, -10.0), &config, now);
    assert_eq!(vertical.direction(), Axes::VERTICAL);
    assert!(vertical.offset(now).1 < 0.0);
  }

  #[test]
  fn disabled_axes_and_single_rows_never_lock() {
    let now = Instant::now();
    let mut config = config();
    config.enable_horizontal = false;
    let mut swipe_off = Swipe::begin((0, 0), (3, 3));
    assert!(
      swipe(&mut swipe_off, 10, (20.0, 0.0), &config, now)
        .iter()
        .all(|update| *update == Update::Pending)
    );
    // A grid one workspace wide has nowhere to go horizontally.
    let config = VswipeConfig::default();
    let mut single = Swipe::begin((0, 0), (1, 3));
    swipe(&mut single, 10, (20.0, 0.0), &config, now);
    assert_eq!(single.direction(), Axes::NONE);
    let mut column = Swipe::begin((0, 0), (1, 3));
    swipe(&mut column, 2, (0.0, 20.0), &config, now);
    assert_eq!(column.direction(), Axes::VERTICAL);
  }

  #[test]
  fn free_movement_goes_diagonal_at_once_or_later() {
    let now = Instant::now();
    let mut config = config();
    config.enable_free_movement = true;
    let mut diagonal = Swipe::begin((1, 1), (3, 3));
    swipe(&mut diagonal, 2, (10.0, 10.0), &config, now);
    assert_eq!(diagonal.direction(), Axes::DIAGONAL);
    // Locked horizontal first, then 0.3 of a workspace vertically
    // promotes it.
    let mut promoted = Swipe::begin((1, 1), (3, 3));
    swipe(&mut promoted, 2, (20.0, 0.0), &config, now);
    assert_eq!(promoted.direction(), Axes::HORIZONTAL);
    swipe(&mut promoted, 3, (0.0, 25.0), &config, now);
    assert_eq!(promoted.direction(), Axes::HORIZONTAL);
    swipe(&mut promoted, 1, (0.0, 25.0), &config, now);
    assert_eq!(promoted.direction(), Axes::DIAGONAL);
    // Without free movement a diagonal start picks the larger axis.
    let config = VswipeConfig::default();
    let mut locked = Swipe::begin((1, 1), (3, 3));
    swipe(&mut locked, 2, (10.0, 9.0), &config, now);
    assert_eq!(locked.direction(), Axes::HORIZONTAL);
    swipe(&mut locked, 10, (0.0, 30.0), &config, now);
    assert_eq!(locked.direction(), Axes::HORIZONTAL);
  }

  #[test]
  fn deltas_are_capped_and_scaled_by_the_speed_factor() {
    let now = Instant::now();
    let mut config = config();
    let mut slow = Swipe::begin((1, 0), (3, 1));
    // 12.8 px of 256 is 0.05 of a workspace, not yet past the initial
    // threshold, so only the second update moves.
    let updates = swipe(&mut slow, 2, (-12.8, 0.0), &config, now);
    assert_eq!(updates, [Update::Pending, Update::Started]);
    let expected = process_delta(-0.05, 0.0, 1, 3, 0.05, false);
    assert!(close(slow.offset_end().0, expected));
    config.speed_factor = 128.0;
    let mut fast = Swipe::begin((1, 0), (3, 1));
    fast.update(-6.4, 0.0, &config, now);
    fast.update(-100.0, 0.0, &config, now);
    // 100 px of 128 is capped at 0.05 workspaces.
    let end = fast.offset_end().0;
    assert!(close(end, expected), "{end}");
  }

  /// The slowdown factor of `process_delta` for a small delta in the
  /// direction of `delta` at `accumulated` on workspace 1 of 3.
  fn slowdown(delta: f64, accumulated: f64) -> f64 {
    process_delta(delta, accumulated, 1, 3, 1.0, false) / delta
  }

  #[test]
  fn going_beyond_the_neighbour_or_the_edge_is_a_rubber_band() {
    // Near the start the delta passes almost unchanged.
    assert!(slowdown(0.01, 0.0) > 0.99);
    // Close to a whole workspace away it slows to almost nothing...
    let far = process_delta(0.01, 1.0, 1, 3, 1.0, false) / 0.01;
    assert!(far < 0.1, "{far}");
    // ...but going back is easier.
    let back = process_delta(-0.01, 1.0, 1, 3, 1.0, false) / -0.01;
    assert!(back > 0.19, "{back}");
    // At the left edge (workspace 0) any motion right is resisted.
    let edge = process_delta(0.01, 0.5, 0, 3, 1.0, true) / 0.01;
    assert!(edge < 0.1, "{edge}");
    // In the middle of the grid free movement is not resisted.
    let free = process_delta(0.01, 0.98, 1, 3, 1.0, true) / 0.01;
    assert!(free > 0.99, "{free}");
  }

  #[test]
  fn the_finish_target_follows_wayfire() {
    // Past the threshold, or not.
    assert_eq!(finish_target(0.4, 1, 3, 0.0, 0.35, 24.0, false), 1);
    assert_eq!(finish_target(0.3, 1, 3, 0.0, 0.35, 24.0, false), 0);
    assert_eq!(finish_target(-0.4, 1, 3, 0.0, 0.35, 24.0, false), -1);
    // A flick goes on however short the swipe.
    assert_eq!(finish_target(0.1, 1, 3, 30.0, 0.35, 24.0, false), 1);
    assert_eq!(finish_target(-0.1, 1, 3, -30.0, 0.35, 24.0, false), -1);
    // A flick the other way does not.
    assert_eq!(finish_target(0.1, 1, 3, -30.0, 0.35, 24.0, false), 0);
    // Never past the grid's edges.
    assert_eq!(finish_target(0.9, 0, 3, 0.0, 0.35, 24.0, false), 0);
    assert_eq!(finish_target(-0.9, 2, 3, 0.0, 0.35, 24.0, false), 0);
    // Free movement crosses several workspaces; without it, one.
    assert_eq!(finish_target(1.6, 2, 3, 0.0, 0.35, 24.0, true), 2);
    assert_eq!(finish_target(1.6, 2, 3, 0.0, 0.35, 24.0, false), 1);
    // With free movement a flick only counts below one workspace.
    assert_eq!(finish_target(1.1, 2, 3, 30.0, 0.35, 24.0, true), 1);
  }

  #[test]
  fn a_long_swipe_moves_on_and_a_short_one_returns() {
    let config = config();
    let now = Instant::now();
    let mut long = Swipe::begin((1, 0), (3, 1));
    // Twenty capped updates to the left: almost a workspace.
    swipe(&mut long, 20, (-12.8, 0.0), &config, now);
    assert!(long.offset_end().0 < -0.35);
    // Slow last updates, so no flick.
    long.update(-1.0, 0.0, &config, now);
    long.update(-1.0, 0.0, &config, now);
    assert_eq!(long.end(&config, now), (2, 0));
    assert_eq!(long.offset_end(), (-1.0, 0.0));
    let mut short = Swipe::begin((1, 0), (3, 1));
    swipe(&mut short, 4, (-12.8, 0.0), &config, now);
    short.update(-1.0, 0.0, &config, now);
    short.update(-1.0, 0.0, &config, now);
    assert_eq!(short.end(&config, now), (1, 0));
    assert_eq!(short.offset_end(), (0.0, 0.0));
    // libinput's cancellation does not change that: Wayfire ignores it.
  }

  #[test]
  fn the_slide_starts_where_the_wall_is_and_ends_after_the_duration() {
    let config = config();
    let start = Instant::now();
    let mut swipe_state = Swipe::begin((1, 0), (3, 1));
    swipe(&mut swipe_state, 20, (-12.8, 0.0), &config, start);
    let before = swipe_state.offset(start).0;
    assert_eq!(swipe_state.viewport(start), (1.0 - before, 0.0));
    swipe_state.end(&config, start);
    assert!(!swipe_state.swiping());
    // The slide starts at the wall's position and runs for 180 ms.
    assert!(close(swipe_state.offset(start).0, before));
    let middle = start + Duration::from_millis(90);
    assert!(close(swipe_state.offset(middle).0, (before - 1.0) / 2.0));
    assert!(swipe_state.running(middle));
    assert!(!swipe_state.finished(middle));
    let end = start + Duration::from_millis(180);
    assert_eq!(swipe_state.viewport(end), (2.0, 0.0));
    assert!(swipe_state.finished(end));
  }

  #[test]
  fn a_smooth_transition_eases_towards_each_new_position() {
    let mut config = config();
    config.enable_smooth_transition = true;
    let start = Instant::now();
    let mut smooth = Swipe::begin((1, 0), (3, 1));
    smooth.update(-12.8, 0.0, &config, start);
    smooth.update(-12.8, 0.0, &config, start);
    let end = smooth.offset_end().0;
    assert!(end < 0.0);
    // The wall is still where it was and gets there over the duration.
    assert_eq!(smooth.offset(start).0, 0.0);
    let middle = start + Duration::from_millis(90);
    assert!(close(smooth.offset(middle).0, end / 2.0));
    // The next update starts from where the wall is now.
    smooth.update(-12.8, 0.0, &config, middle);
    assert!(close(smooth.offset(middle).0, end / 2.0));
    assert!(smooth.offset_end().0 < end);
    // Without it the wall jumps to each new position.
    let immediate_config = VswipeConfig::default();
    let mut immediate = Swipe::begin((1, 0), (3, 1));
    immediate.update(-12.8, 0.0, &immediate_config, start);
    immediate.update(-12.8, 0.0, &immediate_config, start);
    assert_eq!(immediate.offset(start).0, immediate.offset_end().0);
  }

  #[test]
  fn a_zero_duration_finishes_at_once() {
    let mut config = config();
    config.duration_ms = 0;
    let now = Instant::now();
    let mut swipe_state = Swipe::begin((0, 0), (2, 1));
    swipe(&mut swipe_state, 20, (-12.8, 0.0), &config, now);
    assert!(!swipe_state.finished(now), "the fingers are down");
    assert_eq!(swipe_state.end(&config, now), (1, 0));
    assert!(swipe_state.finished(now));
    assert_eq!(swipe_state.viewport(now), (1.0, 0.0));
  }

  #[test]
  fn diagonal_swipes_move_both_axes_and_several_workspaces() {
    let mut config = config();
    config.enable_free_movement = true;
    let now = Instant::now();
    let mut diagonal = Swipe::begin((2, 2), (3, 3));
    swipe(&mut diagonal, 60, (12.8, 12.8), &config, now);
    diagonal.update(0.0, 0.0, &config, now);
    diagonal.update(0.0, 0.0, &config, now);
    let (dx, dy) = diagonal.offset_end();
    assert!(dx > 1.35 && dy > 1.35, "{dx} {dy}");
    assert_eq!(diagonal.end(&config, now), (0, 0));
  }
}
