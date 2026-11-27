//! Idle, DPMS and screen-lock coordination, without Wayland or timers.
//!
//! [`IdleCoordinator`] is a pure state machine. The compositor feeds it the
//! current time with every call and polls it with
//! [`IdleCoordinator::tick`]; each call returns the [`Command`]s the
//! compositor must carry out (crates/shingle/src/state.rs:1036 turns them into
//! `Event`s). It never reads a clock or sleeps, which keeps it unit-testable.
//!
//! Three independent timeouts run from the last activity: the idle
//! notification, the automatic lock and DPMS off. All three are suspended
//! while a client holds an idle inhibitor, while the coordinator is disabled
//! (Wayfire's `idle/toggle`), and while a fullscreen view is shown if
//! [`Config::disable_on_fullscreen`] is set. This follows Wayfire's idle
//! plugin (/home/karl/dev/wayfire/plugins/single_plugins/idle.cpp), which
//! has the same toggle, fullscreen suppression and `disable_initially`, but
//! Shingle has no screensaver cube and adds the lock timeout.
//!
//! Locking fails closed: once a lock has been requested, normal clients get
//! no input until a lock surface is shown and then unlocked (see
//! [`IdleCoordinator::input_target`]). Once a lock client has asked for the
//! lock, no output shows a client until it is unlocked
//! ([`LockState::hides_clients`]).

#![warn(missing_docs)]

use std::time::Duration;

/// A point on the compositor's monotonic clock, as the time since an
/// arbitrary origin fixed by the caller. Shingle uses the time since the
/// compositor started. Only differences matter; times passed in that are
/// earlier than one already seen are treated as that earlier-seen time.
pub type Timestamp = Duration;

/// The power state the coordinator believes the outputs are in. It changes
/// as soon as the matching [`Command`] is issued, before the backend has
/// acted on it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DpmsState {
  /// Outputs are powered and displaying.
  #[default]
  On,
  /// Outputs are blanked to save power, by timeout or by a
  /// wlr-output-power-management client.
  Off,
}

/// What to do when the lock client dies or never shows a lock surface.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LockFailurePolicy {
  /// Enter [`LockState::Failed`]: the session stays locked and no client
  /// receives input until a new lock client locks and unlocks it. This is
  /// the only policy, and it is not configurable.
  #[default]
  FailClosed,
}

/// Timeouts and suppression rules, built from the `idle` section of the
/// Shingle config by `shingle_config::IdleConfig::as_idle_config`.
///
/// Every timeout counts from the most recent activity, not from the previous
/// timeout firing. `None` disables that timeout; the default disables all
/// three.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
  /// Inactivity before [`Command::NotifyIdle`], which the compositor emits
  /// as `Event::Idle`. Wayfire has no equivalent. Clients using
  /// ext-idle-notify-v1 have their own timeouts and do not depend on it.
  pub idle_timeout: Option<Duration>,
  /// Inactivity before [`Command::RequestDpmsOff`], Wayfire's
  /// `idle/dpms_timeout`. Does not fire while a lock is pending or has
  /// failed, so a broken lock never blanks the screen over the session.
  pub dpms_timeout: Option<Duration>,
  /// Inactivity before [`Command::StartLockClient`]. Wayfire leaves this to
  /// external tools such as swayidle.
  pub lock_timeout: Option<Duration>,
  /// How long a lock client the compositor started has to ask for the
  /// lock ([`LockState::Starting`]) before the session is locked without
  /// it ([`LockState::Pending`]). Default 5 s.
  pub lock_start_timeout: Duration,
  /// Applied by [`IdleCoordinator::lock_client_failed`].
  pub lock_failure_policy: LockFailurePolicy,
  /// Suspend all timeouts while a fullscreen view is shown, Wayfire's
  /// `idle/disable_on_fullscreen`. Defaults to `true`, as in Wayfire.
  pub disable_on_fullscreen: bool,
  /// Start with the coordinator disabled, as if `idle/toggle` had been
  /// pressed once, Wayfire's `idle/disable_initially`.
  pub disable_initially: bool,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      idle_timeout: None,
      dpms_timeout: None,
      lock_timeout: None,
      lock_start_timeout: Duration::from_secs(5),
      lock_failure_policy: LockFailurePolicy::FailClosed,
      disable_on_fullscreen: true,
      disable_initially: false,
    }
  }
}

/// The session-lock state machine.
///
/// ```text
/// Unlocked --request_lock/lock timeout-------------> Starting
/// Unlocked, Starting, Failed --client_lock_requested-> Pending
/// Starting --lock_start_timeout---------------------> Pending
/// Pending  --lock_surface_ready---------------------> Locked
/// Locked   --unlock---------------------------------> Unlocked
/// Pending, Locked --lock_client_failed--------------> Failed
/// ```
///
/// There is no way from `Starting`, `Pending` or `Failed` back to
/// `Unlocked` except through `Locked`: only a lock client that showed a
/// lock surface can unlock. `request_lock` in `Failed` starts a new lock
/// client and stays `Failed`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LockState {
  /// Normal operation; clients get input.
  #[default]
  Unlocked,
  /// The lock timeout or the compositor started the lock client, which
  /// has not asked for the lock yet. No client gets input, but the
  /// outputs still show the clients, so that the lock client can capture
  /// them for its background, as swaylock started by swayidle can. Ends
  /// in [`LockState::Pending`] when the client asks or after
  /// [`Config::lock_start_timeout`].
  Starting,
  /// A lock client asked for the lock with an
  /// `ext_session_lock_manager_v1.lock` request, or the lock client
  /// started by the compositor did not ask in time, and no lock surface
  /// has been committed yet. No client gets input and none is shown.
  Pending,
  /// A lock surface is shown and receives all input.
  Locked,
  /// The lock client disconnected or destroyed its lock without unlocking.
  /// No client gets input or is shown until a new lock client takes over.
  Failed,
}

impl LockState {
  /// Whether no output may show any client: from the moment a lock
  /// client asked for the lock until it unlocks, also after it crashed.
  /// Only [`LockState::Unlocked`] and [`LockState::Starting`] show the
  /// clients.
  pub fn hides_clients(self) -> bool {
    matches!(self, Self::Pending | Self::Locked | Self::Failed)
  }
}

/// Which surfaces may receive seat input in the current [`LockState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputTarget {
  /// Ordinary focus rules apply ([`LockState::Unlocked`]).
  NormalClients,
  /// Only the session-lock surface ([`LockState::Locked`]).
  LockSurface,
  /// Nothing ([`LockState::Starting`], [`LockState::Pending`] or
  /// [`LockState::Failed`]).
  None,
}

/// An action the compositor must take. The coordinator has already updated
/// its own state as if the action succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
  /// Launch the configured lock client. Issued on entering
  /// [`LockState::Starting`] from the lock timeout or
  /// [`IdleCoordinator::request_lock`], and by `request_lock` in
  /// [`LockState::Failed`].
  StartLockClient,
  /// Blank the outputs.
  RequestDpmsOff,
  /// Power the outputs back on.
  RequestDpmsOn,
  /// The idle timeout passed. Issued once per idle period.
  NotifyIdle,
  /// Activity, unlocking or suppression ended an idle period that
  /// [`Command::NotifyIdle`] started.
  NotifyResume,
}

/// Tracks activity and turns timeouts into [`Command`]s.
///
/// Every method that takes `now` first advances the coordinator's clock to
/// it; the clock never goes backwards. Methods that return `Vec<Command>`
/// expect the caller to carry out every command in order.
#[derive(Clone, Debug)]
pub struct IdleCoordinator {
  config: Config,
  now: Timestamp,
  last_activity: Timestamp,
  idle_inhibited: bool,
  idle_notified: bool,
  lock_state: LockState,
  /// When [`LockState::Starting`] was entered.
  starting_since: Timestamp,
  dpms_state: DpmsState,
  enabled: bool,
  fullscreen: bool,
}

impl IdleCoordinator {
  /// Starts unlocked with DPMS on and `now` as the last activity. The
  /// coordinator is disabled if [`Config::disable_initially`] is set.
  pub fn new(config: Config, now: Timestamp) -> Self {
    Self {
      config,
      now,
      last_activity: now,
      idle_inhibited: false,
      idle_notified: false,
      lock_state: LockState::Unlocked,
      starting_since: now,
      dpms_state: DpmsState::On,
      enabled: !config.disable_initially,
      fullscreen: false,
    }
  }

  /// The configuration last set.
  pub fn config(&self) -> Config {
    self.config
  }

  /// Replaces the configuration, for a config reload. Does not advance the
  /// clock or reset the activity time, except that a fullscreen view under
  /// [`Config::disable_on_fullscreen`] resumes from idle and DPMS off as
  /// [`IdleCoordinator::set_fullscreen`] would. [`Config::disable_initially`]
  /// is ignored here; it only applies to [`IdleCoordinator::new`].
  pub fn set_config(&mut self, config: Config) -> Vec<Command> {
    self.config = config;
    if self.fullscreen && self.config.disable_on_fullscreen {
      self.resume_after_suppression()
    } else {
      Vec::new()
    }
  }

  /// `false` while `idle/toggle` has suspended all timeouts.
  pub fn is_enabled(&self) -> bool {
    self.enabled
  }

  /// Enables or suspends all timeouts. A change counts as activity.
  /// Disabling also ends any idle period and turns DPMS back on. Has no
  /// effect on the lock state.
  pub fn set_enabled(&mut self, enabled: bool, now: Timestamp) -> Vec<Command> {
    self.advance_to(now);
    if self.enabled == enabled {
      return Vec::new();
    }
    self.enabled = enabled;
    self.last_activity = self.now;
    if enabled {
      Vec::new()
    } else {
      self.resume_after_suppression()
    }
  }

  /// Wayfire's `idle/toggle` activator: flips [`Self::is_enabled`] and
  /// returns the new value with the commands from [`Self::set_enabled`].
  pub fn toggle(&mut self, now: Timestamp) -> (bool, Vec<Command>) {
    let commands = self.set_enabled(!self.enabled, now);
    (self.enabled, commands)
  }

  /// Records whether a fullscreen view is shown. A change counts as
  /// activity. Under [`Config::disable_on_fullscreen`], entering fullscreen
  /// suspends all timeouts, ends any idle period and turns DPMS back on,
  /// like Wayfire's `update_fullscreen` in idle.cpp.
  pub fn set_fullscreen(
    &mut self,
    fullscreen: bool,
    now: Timestamp,
  ) -> Vec<Command> {
    self.advance_to(now);
    if self.fullscreen == fullscreen {
      return Vec::new();
    }
    self.fullscreen = fullscreen;
    self.last_activity = self.now;
    if fullscreen && self.config.disable_on_fullscreen {
      self.resume_after_suppression()
    } else {
      Vec::new()
    }
  }

  /// The latest time passed to any method.
  pub fn now(&self) -> Timestamp {
    self.now
  }

  /// When the timeouts last restarted: the last input, suppression change,
  /// inhibitor change or unlock. ext-idle-notify-v1 notifications also
  /// count from here (crates/shingle/src/idle_notify.rs:42).
  pub fn last_activity(&self) -> Timestamp {
    self.last_activity
  }

  /// See [`LockState`] for the transitions.
  pub fn lock_state(&self) -> LockState {
    self.lock_state
  }

  /// See [`DpmsState`].
  pub fn dpms_state(&self) -> DpmsState {
    self.dpms_state
  }

  /// `true` between [`Command::NotifyIdle`] and [`Command::NotifyResume`].
  /// Activity while locked does not end an idle period; unlocking does.
  pub fn is_idle(&self) -> bool {
    self.idle_notified
  }

  /// Whether a client's idle inhibitor is active.
  pub fn is_idle_inhibited(&self) -> bool {
    self.idle_inhibited
  }

  /// Where seat input may go; see [`InputTarget`].
  pub fn input_target(&self) -> InputTarget {
    match self.lock_state {
      LockState::Unlocked => InputTarget::NormalClients,
      LockState::Starting | LockState::Pending | LockState::Failed => {
        InputTarget::None
      }
      LockState::Locked => InputTarget::LockSurface,
    }
  }

  /// Shorthand for `input_target() == InputTarget::NormalClients`.
  pub fn normal_clients_input_allowed(&self) -> bool {
    self.input_target() == InputTarget::NormalClients
  }

  /// The earliest time at which [`Self::tick`] may issue a command, or
  /// `None` when every timeout is disabled, suspended or already fired. A
  /// deadline already in the past is reported as [`Self::now`]. It may be
  /// earlier than strictly needed (a DPMS deadline that coincides with the
  /// lock timeout is reported although `tick` then skips DPMS), so a caller
  /// sleeping until it just ticks once more.
  pub fn next_deadline(&self) -> Option<Timestamp> {
    // The start deadline runs whatever suppresses the idle timeouts.
    if self.lock_state == LockState::Starting {
      return Some(self.start_deadline().max(self.now));
    }
    if self.idle_inhibited
      || !self.enabled
      || (self.fullscreen && self.config.disable_on_fullscreen)
    {
      return None;
    }

    let mut next = None;
    if self.lock_state == LockState::Unlocked && !self.idle_notified {
      Self::consider_deadline(
        &mut next,
        self.deadline(self.config.idle_timeout),
      );
    }
    if self.lock_state == LockState::Unlocked {
      Self::consider_deadline(
        &mut next,
        self.deadline(self.config.lock_timeout),
      );
    }
    if self.dpms_state == DpmsState::On && !self.lock_unsettled() {
      Self::consider_deadline(
        &mut next,
        self.deadline(self.config.dpms_timeout),
      );
    }
    next
  }

  /// Records user input: restarts all timeouts, turns DPMS back on and, if
  /// unlocked, ends an idle period. Input to a lock surface counts too.
  pub fn activity(&mut self, now: Timestamp) -> Vec<Command> {
    self.advance_to(now);
    self.last_activity = self.now;

    let mut commands = Vec::new();
    if self.dpms_state == DpmsState::Off {
      self.dpms_state = DpmsState::On;
      commands.push(Command::RequestDpmsOn);
    }
    if self.lock_state == LockState::Unlocked && self.idle_notified {
      self.idle_notified = false;
      commands.push(Command::NotifyResume);
    }
    commands
  }

  /// Records whether any mapped surface holds a zwp-idle-inhibit-v1
  /// inhibitor. A change counts as activity. Like fullscreen suppression,
  /// an inhibitor appearing ends any idle period and turns DPMS back on;
  /// Wayfire uses one inhibitor mechanism for both (src/core/idle.cpp,
  /// `idle_inhibitor_t`).
  pub fn set_idle_inhibited(
    &mut self,
    inhibited: bool,
    now: Timestamp,
  ) -> Vec<Command> {
    self.advance_to(now);
    if self.idle_inhibited == inhibited {
      return Vec::new();
    }
    self.idle_inhibited = inhibited;
    self.last_activity = self.now;
    if inhibited {
      self.resume_after_suppression()
    } else {
      Vec::new()
    }
  }

  /// Asks for the session to be locked by the compositor, from a lock
  /// binding or the compositor API, which starts the lock client
  /// ([`Command::StartLockClient`]). From [`LockState::Unlocked`] it
  /// enters [`LockState::Starting`]; in [`LockState::Failed`] it stays
  /// failed, so the outputs stay dark while the new client starts; when a
  /// lock is already starting, pending or held it does nothing.
  pub fn request_lock(&mut self, now: Timestamp) -> Vec<Command> {
    self.advance_to(now);
    match self.lock_state {
      LockState::Unlocked => {
        self.enter_starting();
        vec![Command::StartLockClient]
      }
      LockState::Failed => vec![Command::StartLockClient],
      LockState::Starting | LockState::Pending | LockState::Locked => {
        Vec::new()
      }
    }
  }

  /// A client asked for the lock (`ext_session_lock_manager_v1.lock`):
  /// from [`LockState::Unlocked`], [`LockState::Starting`] or
  /// [`LockState::Failed`] it enters [`LockState::Pending`], which hides
  /// every client. Ignored while a lock is pending or held; the
  /// compositor refuses a second lock before calling this.
  pub fn client_lock_requested(&mut self, now: Timestamp) {
    self.advance_to(now);
    if matches!(
      self.lock_state,
      LockState::Unlocked | LockState::Starting | LockState::Failed
    ) {
      self.lock_state = LockState::Pending;
    }
  }

  /// The lock client committed a lock surface: [`LockState::Pending`]
  /// becomes [`LockState::Locked`]. Ignored in any other state.
  pub fn lock_surface_ready(&mut self, now: Timestamp) {
    self.advance_to(now);
    if self.lock_state == LockState::Pending {
      self.lock_state = LockState::Locked;
    }
  }

  /// The lock client went away without unlocking, or its lock object was
  /// destroyed while pending or locked. Applies
  /// [`Config::lock_failure_policy`]; ignored when unlocked or already
  /// failed.
  pub fn lock_client_failed(&mut self, now: Timestamp) {
    self.advance_to(now);
    if matches!(self.lock_state, LockState::Pending | LockState::Locked) {
      match self.config.lock_failure_policy {
        LockFailurePolicy::FailClosed => self.lock_state = LockState::Failed,
      }
    }
  }

  /// The lock client sent `unlock_and_destroy` after authenticating, or the
  /// compositor API unlocked. Only
  /// valid from [`LockState::Locked`]; in any other state it is ignored,
  /// so a client cannot unlock a lock that never showed a surface. Counts
  /// as activity, turns DPMS on and ends an idle period.
  pub fn unlock(&mut self, now: Timestamp) -> Vec<Command> {
    self.advance_to(now);
    if self.lock_state != LockState::Locked {
      return Vec::new();
    }

    self.lock_state = LockState::Unlocked;
    self.last_activity = self.now;

    let mut commands = Vec::new();
    if self.dpms_state == DpmsState::Off {
      self.dpms_state = DpmsState::On;
      commands.push(Command::RequestDpmsOn);
    }
    if self.idle_notified {
      self.idle_notified = false;
      commands.push(Command::NotifyResume);
    }
    commands
  }

  /// Sets the output power state on request, from
  /// wlr-output-power-management or the compositor API, and returns the
  /// matching DPMS command if it changed. Does not count as activity, so
  /// turning DPMS on by hand does not delay the next timeout.
  pub fn set_dpms(&mut self, state: DpmsState, now: Timestamp) -> Vec<Command> {
    self.advance_to(now);
    if self.dpms_state == state {
      return Vec::new();
    }

    self.dpms_state = state;
    vec![match state {
      DpmsState::On => Command::RequestDpmsOn,
      DpmsState::Off => Command::RequestDpmsOff,
    }]
  }

  /// Advances to `now` and fires every timeout that is due, in the order
  /// idle, lock, DPMS. Each fires once per activity period. When the lock
  /// and DPMS timeouts fall due on the same tick only the lock fires; DPMS
  /// follows on a later tick once the lock is held, so the screen is not
  /// blanked before the lock surface is up.
  pub fn tick(&mut self, now: Timestamp) -> Vec<Command> {
    self.advance_to(now);
    if self.lock_state == LockState::Starting
      && self.now >= self.start_deadline()
    {
      self.lock_state = LockState::Pending;
    }
    if self.idle_inhibited
      || !self.enabled
      || (self.fullscreen && self.config.disable_on_fullscreen)
    {
      return Vec::new();
    }

    let idle_due = self.lock_state == LockState::Unlocked
      && !self.idle_notified
      && self.is_due(self.config.idle_timeout);
    let lock_due = self.lock_state == LockState::Unlocked
      && self.is_due(self.config.lock_timeout);
    let dpms_due = self.dpms_state == DpmsState::On
      && !lock_due
      && !self.lock_unsettled()
      && self.is_due(self.config.dpms_timeout);

    let mut commands = Vec::new();
    if idle_due {
      self.idle_notified = true;
      commands.push(Command::NotifyIdle);
    }
    if lock_due {
      self.enter_starting();
      commands.push(Command::StartLockClient);
    }
    if dpms_due {
      self.dpms_state = DpmsState::Off;
      commands.push(Command::RequestDpmsOff);
    }
    commands
  }

  /// Same as [`Self::tick`].
  pub fn poll(&mut self, now: Timestamp) -> Vec<Command> {
    self.tick(now)
  }

  fn resume_after_suppression(&mut self) -> Vec<Command> {
    self.last_activity = self.now;
    let mut commands = Vec::new();
    if self.dpms_state == DpmsState::Off {
      self.dpms_state = DpmsState::On;
      commands.push(Command::RequestDpmsOn);
    }
    if self.idle_notified {
      self.idle_notified = false;
      commands.push(Command::NotifyResume);
    }
    commands
  }

  fn advance_to(&mut self, now: Timestamp) {
    self.now = self.now.max(now);
  }

  fn enter_starting(&mut self) {
    self.lock_state = LockState::Starting;
    self.starting_since = self.now;
  }

  /// When a [`LockState::Starting`] lock becomes [`LockState::Pending`].
  fn start_deadline(&self) -> Timestamp {
    self
      .starting_since
      .saturating_add(self.config.lock_start_timeout)
  }

  /// Whether a lock is on its way but not shown: no DPMS timeout then,
  /// so a broken lock never blanks the screen over the session.
  fn lock_unsettled(&self) -> bool {
    matches!(
      self.lock_state,
      LockState::Starting | LockState::Pending | LockState::Failed
    )
  }

  fn deadline(&self, timeout: Option<Duration>) -> Option<Timestamp> {
    timeout
      .map(|timeout| self.last_activity.saturating_add(timeout).max(self.now))
  }

  fn is_due(&self, timeout: Option<Duration>) -> bool {
    timeout.is_some_and(|timeout| {
      self.now >= self.last_activity.saturating_add(timeout)
    })
  }

  fn consider_deadline(
    next: &mut Option<Timestamp>,
    candidate: Option<Timestamp>,
  ) {
    if let Some(candidate) = candidate {
      *next = Some(next.map_or(candidate, |current| current.min(candidate)));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn seconds(seconds: u64) -> Timestamp {
    Duration::from_secs(seconds)
  }

  /// A coordinator that went idle after 10 s and turned DPMS off after 20 s.
  fn idle_and_off() -> IdleCoordinator {
    let mut idle = IdleCoordinator::new(
      Config {
        idle_timeout: Some(seconds(10)),
        dpms_timeout: Some(seconds(20)),
        ..Config::default()
      },
      seconds(0),
    );
    idle.tick(seconds(10));
    idle.tick(seconds(20));
    assert_eq!(idle.dpms_state(), DpmsState::Off);
    assert!(idle.is_idle());
    idle
  }

  #[test]
  fn an_idle_inhibitor_resumes_like_fullscreen_suppression() {
    let mut fullscreen = idle_and_off();
    let suppressed = fullscreen.set_fullscreen(true, seconds(30));
    let mut inhibited = idle_and_off();
    let resumed = inhibited.set_idle_inhibited(true, seconds(30));
    for idle in [&fullscreen, &inhibited] {
      assert_eq!(idle.dpms_state(), DpmsState::On);
      assert!(!idle.is_idle());
      assert_eq!(idle.next_deadline(), None);
    }
    assert_eq!(suppressed, [Command::RequestDpmsOn, Command::NotifyResume]);
    assert_eq!(resumed, suppressed);
    assert_eq!(inhibited.set_idle_inhibited(true, seconds(31)), []);
    assert_eq!(inhibited.set_idle_inhibited(false, seconds(32)), []);
  }
  fn locking() -> IdleCoordinator {
    IdleCoordinator::new(
      Config {
        lock_timeout: Some(seconds(10)),
        dpms_timeout: Some(seconds(12)),
        lock_start_timeout: seconds(5),
        ..Config::default()
      },
      seconds(0),
    )
  }

  #[test]
  fn the_lock_timeout_starts_the_client_with_the_clients_still_shown() {
    let mut idle = locking();
    assert_eq!(idle.tick(seconds(10)), [Command::StartLockClient]);
    assert_eq!(idle.lock_state(), LockState::Starting);
    assert!(!idle.lock_state().hides_clients());
    assert_eq!(idle.input_target(), InputTarget::None);
    assert_eq!(idle.next_deadline(), Some(seconds(15)));
    // No DPMS while the lock is on its way.
    assert_eq!(idle.tick(seconds(13)), []);
    assert_eq!(idle.dpms_state(), DpmsState::On);
    idle.client_lock_requested(seconds(14));
    assert_eq!(idle.lock_state(), LockState::Pending);
    assert!(idle.lock_state().hides_clients());
    idle.lock_surface_ready(seconds(14));
    assert_eq!(idle.lock_state(), LockState::Locked);
    assert!(idle.lock_state().hides_clients());
  }

  #[test]
  fn a_lock_client_that_does_not_ask_in_time_leaves_the_session_pending() {
    let mut idle = locking();
    assert_eq!(idle.request_lock(seconds(1)), [Command::StartLockClient]);
    assert_eq!(idle.request_lock(seconds(2)), []);
    // Suppression does not hold the start deadline back.
    idle.set_idle_inhibited(true, seconds(3));
    assert_eq!(idle.next_deadline(), Some(seconds(6)));
    idle.tick(seconds(5));
    assert_eq!(idle.lock_state(), LockState::Starting);
    idle.tick(seconds(6));
    assert_eq!(idle.lock_state(), LockState::Pending);
    assert_eq!(idle.unlock(seconds(7)), []);
    assert_eq!(idle.lock_state(), LockState::Pending);
  }

  #[test]
  fn a_failed_lock_stays_dark_while_a_new_client_starts() {
    let mut idle = locking();
    idle.client_lock_requested(seconds(1));
    idle.lock_surface_ready(seconds(1));
    idle.lock_client_failed(seconds(2));
    assert_eq!(idle.lock_state(), LockState::Failed);
    assert!(idle.lock_state().hides_clients());
    assert_eq!(idle.request_lock(seconds(3)), [Command::StartLockClient]);
    assert_eq!(idle.lock_state(), LockState::Failed);
    idle.client_lock_requested(seconds(4));
    assert_eq!(idle.lock_state(), LockState::Pending);
    idle.lock_surface_ready(seconds(4));
    idle.unlock(seconds(5));
    assert_eq!(idle.lock_state(), LockState::Unlocked);
  }

  #[test]
  fn only_unlocked_and_starting_show_the_clients() {
    for (state, hides) in [
      (LockState::Unlocked, false),
      (LockState::Starting, false),
      (LockState::Pending, true),
      (LockState::Locked, true),
      (LockState::Failed, true),
    ] {
      assert_eq!(state.hides_clients(), hides, "{state:?}");
    }
  }
}
