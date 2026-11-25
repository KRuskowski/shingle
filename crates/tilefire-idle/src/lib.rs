use std::time::Duration;

pub type Timestamp = Duration;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DpmsState {
    #[default]
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LockFailurePolicy {
    #[default]
    FailClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub idle_timeout: Option<Duration>,
    pub dpms_timeout: Option<Duration>,
    pub lock_timeout: Option<Duration>,
    pub lock_failure_policy: LockFailurePolicy,
    pub disable_on_fullscreen: bool,
    pub disable_initially: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout: None,
            dpms_timeout: None,
            lock_timeout: None,
            lock_failure_policy: LockFailurePolicy::FailClosed,
            disable_on_fullscreen: true,
            disable_initially: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LockState {
    #[default]
    Unlocked,
    Pending,
    Locked,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputTarget {
    NormalClients,
    LockSurface,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    StartLockClient,
    RequestDpmsOff,
    RequestDpmsOn,
    NotifyIdle,
    NotifyResume,
}

#[derive(Clone, Debug)]
pub struct IdleCoordinator {
    config: Config,
    now: Timestamp,
    last_activity: Timestamp,
    idle_inhibited: bool,
    idle_notified: bool,
    lock_state: LockState,
    dpms_state: DpmsState,
    enabled: bool,
    fullscreen: bool,
}

impl IdleCoordinator {
    pub fn new(config: Config, now: Timestamp) -> Self {
        Self {
            config,
            now,
            last_activity: now,
            idle_inhibited: false,
            idle_notified: false,
            lock_state: LockState::Unlocked,
            dpms_state: DpmsState::On,
            enabled: !config.disable_initially,
            fullscreen: false,
        }
    }

    pub fn config(&self) -> Config {
        self.config
    }

    pub fn set_config(&mut self, config: Config) -> Vec<Command> {
        self.config = config;
        if self.fullscreen && self.config.disable_on_fullscreen {
            self.resume_after_suppression()
        } else {
            Vec::new()
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(
        &mut self,
        enabled: bool,
        now: Timestamp,
    ) -> Vec<Command> {
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

    pub fn toggle(&mut self, now: Timestamp) -> (bool, Vec<Command>) {
        let commands = self.set_enabled(!self.enabled, now);
        (self.enabled, commands)
    }

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

    pub fn now(&self) -> Timestamp {
        self.now
    }

    pub fn last_activity(&self) -> Timestamp {
        self.last_activity
    }

    pub fn lock_state(&self) -> LockState {
        self.lock_state
    }

    pub fn dpms_state(&self) -> DpmsState {
        self.dpms_state
    }

    pub fn is_idle(&self) -> bool {
        self.idle_notified
    }

    pub fn is_idle_inhibited(&self) -> bool {
        self.idle_inhibited
    }

    pub fn input_target(&self) -> InputTarget {
        match self.lock_state {
            LockState::Unlocked => InputTarget::NormalClients,
            LockState::Pending | LockState::Failed => InputTarget::None,
            LockState::Locked => InputTarget::LockSurface,
        }
    }

    pub fn normal_clients_input_allowed(&self) -> bool {
        self.input_target() == InputTarget::NormalClients
    }

    pub fn next_deadline(&self) -> Option<Timestamp> {
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
        if self.dpms_state == DpmsState::On
            && !matches!(
                self.lock_state,
                LockState::Pending | LockState::Failed
            )
        {
            Self::consider_deadline(
                &mut next,
                self.deadline(self.config.dpms_timeout),
            );
        }
        next
    }

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

    pub fn set_idle_inhibited(&mut self, inhibited: bool, now: Timestamp) {
        self.advance_to(now);
        if self.idle_inhibited != inhibited {
            self.idle_inhibited = inhibited;
            self.last_activity = self.now;
        }
    }

    pub fn request_lock(&mut self, now: Timestamp) -> Vec<Command> {
        self.advance_to(now);
        match self.lock_state {
            LockState::Unlocked | LockState::Failed => {
                self.lock_state = LockState::Pending;
                vec![Command::StartLockClient]
            }
            LockState::Pending | LockState::Locked => Vec::new(),
        }
    }

    pub fn lock_surface_ready(&mut self, now: Timestamp) {
        self.advance_to(now);
        if self.lock_state == LockState::Pending {
            self.lock_state = LockState::Locked;
        }
    }

    pub fn lock_client_failed(&mut self, now: Timestamp) {
        self.advance_to(now);
        if matches!(self.lock_state, LockState::Pending | LockState::Locked) {
            match self.config.lock_failure_policy {
                LockFailurePolicy::FailClosed => {
                    self.lock_state = LockState::Failed
                }
            }
        }
    }

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

    pub fn set_dpms(
        &mut self,
        state: DpmsState,
        now: Timestamp,
    ) -> Vec<Command> {
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

    pub fn tick(&mut self, now: Timestamp) -> Vec<Command> {
        self.advance_to(now);
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
            && !matches!(
                self.lock_state,
                LockState::Pending | LockState::Failed
            )
            && self.is_due(self.config.dpms_timeout);

        let mut commands = Vec::new();
        if idle_due {
            self.idle_notified = true;
            commands.push(Command::NotifyIdle);
        }
        if lock_due {
            self.lock_state = LockState::Pending;
            commands.push(Command::StartLockClient);
        }
        if dpms_due {
            self.dpms_state = DpmsState::Off;
            commands.push(Command::RequestDpmsOff);
        }
        commands
    }

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

    fn deadline(&self, timeout: Option<Duration>) -> Option<Timestamp> {
        timeout.map(|timeout| {
            self.last_activity.saturating_add(timeout).max(self.now)
        })
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
            *next =
                Some(next.map_or(candidate, |current| current.min(candidate)));
        }
    }
}
