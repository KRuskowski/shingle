//! Page-flip pacing: at most one commit in flight, frames only between
//! flips.
//!
//! [`FrameScheduler`] is the state machine of docs/design/native-backend.md
//! (section 2, "Page-flip pacing"): `Idle`, `Rendering`, `FlipPending` and
//! `Paused`. A frame starts only when idle, a commit moves it to
//! `FlipPending`, and only the page-flip event makes it idle again, so no
//! frame callback can be sent before the previous flip completed. A flip
//! that never completes times out after [`FLIP_TIMEOUT`]; after
//! [`MAX_FLIP_TIMEOUTS`] in a row the backend gives up.

use std::time::{Duration, Instant};

/// How long a commit may wait for its page flip.
pub const FLIP_TIMEOUT: Duration = Duration::from_secs(1);

/// How many flips in a row may time out before Shingle saves and exits.
pub const MAX_FLIP_TIMEOUTS: u32 = 3;

/// Where the output is in its frame cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
  /// Nothing in flight: a frame may start.
  Idle,
  /// A frame is being drawn and committed.
  Rendering,
  /// A commit waits for its page flip, sent at this time.
  FlipPending {
    /// When the commit was made.
    since: Instant,
  },
  /// The seat is disabled, the display is off or there is no output:
  /// nothing is drawn.
  Paused,
}

/// A flip that did not complete in time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlipTimeout {
  /// How many flips in a row have timed out, this one included.
  pub consecutive: u32,
}

impl FlipTimeout {
  /// Whether Shingle should give up and exit.
  pub fn fatal(self) -> bool {
    self.consecutive >= MAX_FLIP_TIMEOUTS
  }
}

/// The page-flip state machine of one output.
#[derive(Clone, Debug)]
pub struct FrameScheduler {
  state: State,
  timeouts: u32,
}

impl Default for FrameScheduler {
  fn default() -> Self {
    Self {
      state: State::Paused,
      timeouts: 0,
    }
  }
}

impl FrameScheduler {
  /// The current state.
  pub fn state(&self) -> State {
    self.state
  }

  /// Whether a frame may start now.
  pub fn can_render(&self) -> bool {
    self.state == State::Idle
  }

  /// Whether a commit waits for its flip.
  pub fn flip_pending(&self) -> bool {
    matches!(self.state, State::FlipPending { .. })
  }

  /// Starts a frame. Returns false, changing nothing, unless idle.
  pub fn begin_frame(&mut self) -> bool {
    if self.state != State::Idle {
      return false;
    }
    self.state = State::Rendering;
    true
  }

  /// The frame was committed at `now` and its flip is awaited.
  pub fn committed(&mut self, now: Instant) {
    if self.state == State::Rendering {
      self.state = State::FlipPending { since: now };
    }
  }

  /// The frame was not committed after all; another may start.
  pub fn abandon(&mut self) {
    if self.state == State::Rendering {
      self.state = State::Idle;
    }
  }

  /// A page flip arrived. Returns whether one was awaited; a stale flip,
  /// such as one completing after a pause, changes nothing.
  pub fn page_flip(&mut self) -> bool {
    if !self.flip_pending() {
      return false;
    }
    self.state = State::Idle;
    self.timeouts = 0;
    true
  }

  /// Stops drawing and forgets a pending flip, whose event may never
  /// arrive.
  pub fn pause(&mut self) {
    self.state = State::Paused;
  }

  /// Drawing may start again, after a modeset.
  pub fn resume(&mut self) {
    self.state = State::Idle;
  }

  /// When the pending flip times out, if one is pending.
  pub fn deadline(&self) -> Option<Instant> {
    match self.state {
      State::FlipPending { since } => Some(since + FLIP_TIMEOUT),
      _ => None,
    }
  }

  /// Checks the pending flip at `now`. A flip pending for
  /// [`FLIP_TIMEOUT`] or longer is dropped, leaving the scheduler idle,
  /// and reported with the number of timeouts in a row.
  pub fn check_timeout(&mut self, now: Instant) -> Option<FlipTimeout> {
    let deadline = self.deadline()?;
    if now < deadline {
      return None;
    }
    self.state = State::Idle;
    self.timeouts += 1;
    Some(FlipTimeout {
      consecutive: self.timeouts,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn idle() -> FrameScheduler {
    let mut scheduler = FrameScheduler::default();
    scheduler.resume();
    scheduler
  }

  #[test]
  fn it_starts_paused_until_the_first_modeset() {
    let scheduler = FrameScheduler::default();
    assert_eq!(scheduler.state(), State::Paused);
    assert!(!scheduler.can_render());
  }

  #[test]
  fn one_frame_at_a_time() {
    let now = Instant::now();
    let mut scheduler = idle();
    assert!(scheduler.begin_frame());
    assert!(!scheduler.begin_frame(), "a frame is being drawn");
    scheduler.committed(now);
    assert_eq!(scheduler.state(), State::FlipPending { since: now });
    assert!(!scheduler.can_render(), "no frame before the flip");
    assert!(!scheduler.begin_frame());
    assert!(scheduler.page_flip());
    assert!(scheduler.can_render());
    assert!(!scheduler.page_flip(), "a second event is stale");
  }

  #[test]
  fn an_abandoned_frame_frees_the_slot() {
    let mut scheduler = idle();
    scheduler.begin_frame();
    scheduler.abandon();
    assert!(scheduler.can_render());
    scheduler.committed(Instant::now());
    assert_eq!(scheduler.state(), State::Idle, "nothing was being drawn");
  }

  #[test]
  fn a_pause_drops_the_pending_flip() {
    let mut scheduler = idle();
    scheduler.begin_frame();
    scheduler.committed(Instant::now());
    scheduler.pause();
    assert_eq!(scheduler.state(), State::Paused);
    assert!(!scheduler.page_flip(), "its event no longer counts");
    assert_eq!(scheduler.deadline(), None);
    scheduler.resume();
    assert!(scheduler.can_render());
  }

  #[test]
  fn flips_time_out_and_count_until_one_completes() {
    let start = Instant::now();
    let mut scheduler = idle();
    for round in 1..=MAX_FLIP_TIMEOUTS {
      let committed = start + FLIP_TIMEOUT * 2 * round;
      scheduler.begin_frame();
      scheduler.committed(committed);
      assert_eq!(scheduler.deadline(), Some(committed + FLIP_TIMEOUT));
      let early = committed + FLIP_TIMEOUT - Duration::from_millis(1);
      assert_eq!(scheduler.check_timeout(early), None);
      let timeout = scheduler.check_timeout(committed + FLIP_TIMEOUT).unwrap();
      assert_eq!(timeout.consecutive, round);
      assert_eq!(timeout.fatal(), round == MAX_FLIP_TIMEOUTS);
      assert!(scheduler.can_render());
    }
    scheduler.begin_frame();
    scheduler.committed(start);
    assert!(scheduler.page_flip());
    scheduler.begin_frame();
    scheduler.committed(start);
    let timeout = scheduler.check_timeout(start + FLIP_TIMEOUT).unwrap();
    assert_eq!(timeout.consecutive, 1, "a completed flip resets the count");
  }
}
