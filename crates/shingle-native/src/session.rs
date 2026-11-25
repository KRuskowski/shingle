//! The seat session: device access and VT switching through libseat.
//!
//! [`Session`] is what the rest of the native backend needs from the seat:
//! open and close device nodes, switch VT, and learn when the seat is
//! enabled or disabled. [`LibSeatSession`] implements it over libseat
//! (seatd, logind or builtin). Unit tests use fakes; nothing here may be
//! exercised on a host whose seat belongs to another session.
//!
//! The startup checks are pure functions: [`preflight`] refuses to start
//! from SSH before any seat is opened, [`wait_for_enable`] waits for the
//! first [`SeatEvent::Enable`] with a timeout, and [`check_backend`]
//! refuses libseat backends that cannot switch VT. Their errors are
//! [`NoSeat`], whose message names the cause.

use std::{
  cell::RefCell,
  collections::{HashMap, VecDeque},
  ffi::OsString,
  fmt, io,
  os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
  path::Path,
  rc::Rc,
  sync::Mutex,
  time::{Duration, Instant},
};

/// How long Shingle waits for the seat to be enabled at startup.
pub const ENABLE_TIMEOUT: Duration = Duration::from_secs(5);

/// A change of the seat's state, as libseat reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeatEvent {
  /// The seat is active: devices may be opened and used.
  Enable,
  /// The seat is about to be taken away, for example by a VT switch. The
  /// backend pauses and then calls [`Session::acknowledge_disable`].
  Disable,
}

/// What the native backend needs from the seat.
pub trait Session {
  /// The name of the seat, such as `seat0`, for libinput's seat
  /// assignment.
  fn seat_name(&self) -> &str;

  /// Opens device node `path` (an evdev or DRM node) through the seat.
  /// Only works while the seat is enabled.
  fn open_device(&mut self, path: &Path) -> io::Result<OwnedFd>;

  /// Closes a device opened with [`Session::open_device`].
  fn close_device(&mut self, fd: OwnedFd) -> io::Result<()>;

  /// Asks the seat to switch to VT (session) `vt`. Success means only that
  /// the request was sent; a [`SeatEvent::Disable`] follows if it happens.
  fn switch_vt(&mut self, vt: u32) -> io::Result<()>;

  /// Reads pending messages from the seat, waiting up to `timeout` for the
  /// first one (`None` does not wait), and returns the events they caused.
  fn dispatch(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<Vec<SeatEvent>>;

  /// Acknowledges a [`SeatEvent::Disable`] once the backend has stopped
  /// using its devices.
  fn acknowledge_disable(&mut self) -> io::Result<()>;

  /// The descriptor that becomes readable when [`Session::dispatch`] has
  /// work, if there is one.
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}

/// Why the native backend cannot get a seat. Every variant is reported as
/// `no seat: <cause>; run from a VT login or start seatd`, except
/// [`NoSeat::NoVtSwitching`], which names its own way out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoSeat {
  /// Started from an SSH login: this variable is set and `XDG_SEAT` is
  /// not.
  Ssh(&'static str),
  /// libseat could not open a seat, with its error.
  OpenFailed(String),
  /// The seat was not enabled within this long, as happens when the
  /// session is not on the active VT.
  NotEnabled(Duration),
  /// Reading from the seat failed while waiting for it.
  Lost(String),
  /// libseat opened the seat with this backend, which cannot switch VT,
  /// and `--allow-builtin-seat` was not given.
  NoVtSwitching(String),
}

impl fmt::Display for NoSeat {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    const ADVICE: &str = "run from a VT login or start seatd";
    match self {
      Self::Ssh(variable) => write!(
        formatter,
        "no seat: started over SSH ({variable} is set and XDG_SEAT is \
         not); {ADVICE}"
      ),
      Self::OpenFailed(error) => {
        write!(
          formatter,
          "no seat: libseat could not open one ({error}); \
           {ADVICE}"
        )
      }
      Self::NotEnabled(timeout) => write!(
        formatter,
        "no seat: the seat was not enabled within {} s, so this session \
         is not on the active VT; {ADVICE}",
        timeout.as_secs()
      ),
      Self::Lost(error) => {
        write!(
          formatter,
          "no seat: reading from the seat failed ({error}); \
           {ADVICE}"
        )
      }
      Self::NoVtSwitching(backend) => write!(
        formatter,
        "the libseat {backend} backend cannot switch VT, so there would be \
         no way back to a text console; start seatd or log in on a VT, or \
         pass --allow-builtin-seat to use it anyway"
      ),
    }
  }
}

impl std::error::Error for NoSeat {}

/// The variables an SSH server sets in a login, in the order they are
/// reported.
const SSH_VARIABLES: [&str; 3] = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// Refuses to start from an SSH login, before any seat is opened.
///
/// An SSH login has no seat: logind assigns `XDG_SEAT` only to sessions on
/// a seat, such as a VT login. Opening a seat from there would either
/// fail or, with seatd, take the devices of whatever session is on the
/// active VT. So a set `SSH_CONNECTION`, `SSH_CLIENT` or `SSH_TTY` without
/// `XDG_SEAT` is [`NoSeat::Ssh`]. `env` looks up variables by name; empty
/// values count as unset.
pub fn preflight(env: impl Fn(&str) -> Option<OsString>) -> Result<(), NoSeat> {
  let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
  if set("XDG_SEAT") {
    return Ok(());
  }
  match SSH_VARIABLES.into_iter().find(|name| set(name)) {
    Some(variable) => Err(NoSeat::Ssh(variable)),
    None => Ok(()),
  }
}

/// Waits until `session` reports [`SeatEvent::Enable`], for at most
/// `timeout` by `now`.
///
/// A [`SeatEvent::Disable`] that arrives first is acknowledged, since
/// nothing has been opened yet. Running out of time is
/// [`NoSeat::NotEnabled`].
pub fn wait_for_enable(
  session: &mut dyn Session,
  timeout: Duration,
  now: &dyn Fn() -> Instant,
) -> Result<(), NoSeat> {
  let deadline = now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(now());
    let events = session
      .dispatch(Some(remaining))
      .map_err(|error| NoSeat::Lost(error.to_string()))?;
    for event in events {
      match event {
        SeatEvent::Enable => return Ok(()),
        SeatEvent::Disable => session
          .acknowledge_disable()
          .map_err(|error| NoSeat::Lost(error.to_string()))?,
      }
    }
    if remaining.is_zero() {
      return Err(NoSeat::NotEnabled(timeout));
    }
  }
}

/// The libseat backends that cannot switch VT: `builtin` (in-process
/// seatd, for root without a seat daemon) and `noop`.
const NO_VT_BACKENDS: [&str; 2] = ["builtin", "noop"];

/// Refuses a libseat `backend` that cannot switch VT unless
/// `allow_builtin` (`--allow-builtin-seat`). An unknown backend (`None`) is
/// allowed: libseat names the backend only in its log.
pub fn check_backend(
  backend: Option<&str>,
  allow_builtin: bool,
) -> Result<(), NoSeat> {
  match backend {
    Some(name) if NO_VT_BACKENDS.contains(&name) && !allow_builtin => {
      Err(NoSeat::NoVtSwitching(name.to_owned()))
    }
    _ => Ok(()),
  }
}

/// Returns the backend a libseat log line names, if it is the line that
/// libseat writes after opening a seat: `[libseat/libseat.c:NN] Seat
/// opened with backend 'NAME'`.
pub fn opened_backend(line: &str) -> Option<&str> {
  const PREFIX: &str = "Seat opened with backend '";
  let start = line.find(PREFIX)? + PREFIX.len();
  let rest = &line[start..];
  let end = rest.find('\'')?;
  Some(&rest[..end])
}

/// The backend the last libseat "Seat opened" log line named.
static OPENED_BACKEND: Mutex<Option<String>> = Mutex::new(None);

/// Receives libseat's log through the `log` crate: records the backend it
/// opened the seat with and passes errors and information on to the native
/// log.
struct LibseatLogger;

impl log::Log for LibseatLogger {
  fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
    metadata.target().starts_with("libseat")
  }

  fn log(&self, record: &log::Record<'_>) {
    if !self.enabled(record.metadata()) {
      return;
    }
    let line = record.args().to_string();
    if let Some(backend) = opened_backend(&line)
      && let Ok(mut opened) = OPENED_BACKEND.lock()
    {
      *opened = Some(backend.to_owned());
    }
    if record.level() <= log::Level::Info {
      crate::diagnostics::log(&format!("libseat: {line}"));
    }
  }

  fn flush(&self) {}
}

static LOGGER: LibseatLogger = LibseatLogger;

/// The seat, through libseat.
///
/// Compiled and checked on every host but only ever run on a machine
/// whose seat Shingle may take (see docs/native-testing.md); unit tests
/// never construct it.
pub struct LibSeatSession {
  seat: libseat::Seat,
  events: Rc<RefCell<VecDeque<SeatEvent>>>,
  /// The libseat device behind each descriptor handed out, for closing.
  devices: HashMap<RawFd, libseat::Device>,
  name: String,
  /// libseat's connection descriptor, which lives as long as the seat.
  fd: Option<RawFd>,
}

impl LibSeatSession {
  /// Opens the seat. libseat picks the backend (seatd, logind, then
  /// builtin) unless `LIBSEAT_BACKEND` names one.
  pub fn open() -> Result<Self, NoSeat> {
    if log::set_logger(&LOGGER).is_ok() {
      log::set_max_level(log::LevelFilter::Info);
    }
    let events = Rc::new(RefCell::new(VecDeque::new()));
    let queue = Rc::clone(&events);
    let mut seat = libseat::Seat::open(move |_seat, event| {
      queue.borrow_mut().push_back(match event {
        libseat::SeatEvent::Enable => SeatEvent::Enable,
        libseat::SeatEvent::Disable => SeatEvent::Disable,
      });
    })
    .map_err(|errno| {
      NoSeat::OpenFailed(io::Error::from_raw_os_error(errno.0).to_string())
    })?;
    let name = seat.name().to_owned();
    let fd = seat.get_fd().ok().map(|fd| fd.as_raw_fd());
    Ok(Self {
      seat,
      events,
      devices: HashMap::new(),
      name,
      fd,
    })
  }

  /// The backend libseat opened the seat with, from its log, if it said.
  pub fn backend(&self) -> Option<String> {
    OPENED_BACKEND.lock().ok().and_then(|opened| opened.clone())
  }
}

fn errno_error(errno: impl Into<i32>) -> io::Error {
  io::Error::from_raw_os_error(errno.into())
}

impl Session for LibSeatSession {
  fn seat_name(&self) -> &str {
    &self.name
  }

  fn open_device(&mut self, path: &Path) -> io::Result<OwnedFd> {
    // libseat-rs panics on paths that are not UTF-8.
    if path.to_str().is_none() {
      return Err(io::ErrorKind::InvalidInput.into());
    }
    let device = self
      .seat
      .open_device(&path)
      .map_err(|errno| errno_error(errno.0))?;
    let raw = device.as_fd().as_raw_fd();
    self.devices.insert(raw, device);
    // SAFETY: libseat hands the descriptor over; closing it is ours, after
    // libseat_close_device, as in wlroots. `libseat::Device` never closes
    // it.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
  }

  fn close_device(&mut self, fd: OwnedFd) -> io::Result<()> {
    let result = match self.devices.remove(&fd.as_raw_fd()) {
      Some(device) => self
        .seat
        .close_device(device)
        .map_err(|errno| errno_error(errno.0)),
      None => Ok(()),
    };
    drop(fd);
    result
  }

  fn switch_vt(&mut self, vt: u32) -> io::Result<()> {
    let session = i32::try_from(vt)
      .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    self
      .seat
      .switch_session(session)
      .map_err(|errno| errno_error(errno.0))
  }

  fn dispatch(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<Vec<SeatEvent>> {
    let millis = timeout.map_or(0, |timeout| {
      i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX)
    });
    self
      .seat
      .dispatch(millis)
      .map_err(|errno| errno_error(errno.0))?;
    Ok(self.events.borrow_mut().drain(..).collect())
  }

  fn acknowledge_disable(&mut self) -> io::Result<()> {
    self.seat.disable().map_err(|errno| errno_error(errno.0))
  }

  fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
    // SAFETY: the descriptor belongs to the seat, which outlives the
    // borrow of `self`.
    self.fd.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) })
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use std::cell::Cell;

  use super::*;

  /// A seat that replays scripted events and records what it is asked.
  #[derive(Default)]
  pub(crate) struct FakeSession {
    /// Events returned by successive dispatches, one batch each.
    pub(crate) script: VecDeque<Vec<SeatEvent>>,
    pub(crate) dispatches: usize,
    pub(crate) acknowledged: usize,
    pub(crate) vt_switches: Vec<u32>,
    /// Whether `switch_vt` fails.
    pub(crate) refuse_switch: bool,
    pub(crate) opened: Vec<String>,
    pub(crate) closed: usize,
  }

  impl Session for FakeSession {
    fn seat_name(&self) -> &str {
      "seat0"
    }

    fn open_device(&mut self, path: &Path) -> io::Result<OwnedFd> {
      self.opened.push(path.display().to_string());
      Ok(std::fs::File::open("/dev/null")?.into())
    }

    fn close_device(&mut self, _fd: OwnedFd) -> io::Result<()> {
      self.closed += 1;
      Ok(())
    }

    fn switch_vt(&mut self, vt: u32) -> io::Result<()> {
      self.vt_switches.push(vt);
      if self.refuse_switch {
        return Err(io::ErrorKind::PermissionDenied.into());
      }
      Ok(())
    }

    fn dispatch(
      &mut self,
      _timeout: Option<Duration>,
    ) -> io::Result<Vec<SeatEvent>> {
      self.dispatches += 1;
      Ok(self.script.pop_front().unwrap_or_default())
    }

    fn acknowledge_disable(&mut self) -> io::Result<()> {
      self.acknowledged += 1;
      Ok(())
    }

    fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
      None
    }
  }

  fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let pairs: Vec<(String, OsString)> = pairs
      .iter()
      .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
      .collect();
    move |name| {
      pairs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
    }
  }

  #[test]
  fn ssh_logins_without_a_seat_are_refused() {
    let refused = preflight(env(&[("SSH_CONNECTION", "10.0.0.1 22 10.0.0.2")]));
    assert_eq!(refused, Err(NoSeat::Ssh("SSH_CONNECTION")));
    let message = refused.unwrap_err().to_string();
    assert!(
      message.starts_with("no seat: started over SSH"),
      "{message}"
    );
    assert!(
      message.ends_with("run from a VT login or start seatd"),
      "{message}"
    );
    assert_eq!(
      preflight(env(&[("SSH_TTY", "/dev/pts/3")])),
      Err(NoSeat::Ssh("SSH_TTY"))
    );
  }

  #[test]
  fn vt_logins_and_seated_ssh_sessions_pass() {
    assert_eq!(preflight(env(&[("TERM", "linux")])), Ok(()));
    assert_eq!(
      preflight(env(&[("SSH_CLIENT", "x"), ("XDG_SEAT", "seat0")])),
      Ok(())
    );
    assert_eq!(preflight(env(&[("SSH_CONNECTION", "")])), Ok(()));
  }

  /// A clock that advances by `step` on every reading.
  fn ticking(step: Duration) -> impl Fn() -> Instant {
    let start = Instant::now();
    let ticks = Cell::new(0u32);
    move || {
      ticks.set(ticks.get() + 1);
      start + step * ticks.get()
    }
  }

  #[test]
  fn startup_waits_for_the_first_enable() {
    let mut session = FakeSession {
      script: [vec![], vec![SeatEvent::Disable], vec![SeatEvent::Enable]]
        .into(),
      ..FakeSession::default()
    };
    let now = ticking(Duration::from_millis(100));
    assert_eq!(wait_for_enable(&mut session, ENABLE_TIMEOUT, &now), Ok(()));
    assert_eq!(session.dispatches, 3);
    assert_eq!(session.acknowledged, 1, "an early disable is acknowledged");
  }

  #[test]
  fn a_seat_that_never_enables_is_no_seat() {
    let mut session = FakeSession::default();
    let now = ticking(Duration::from_secs(1));
    let error =
      wait_for_enable(&mut session, ENABLE_TIMEOUT, &now).unwrap_err();
    assert_eq!(error, NoSeat::NotEnabled(ENABLE_TIMEOUT));
    assert!(error.to_string().contains("within 5 s"), "{error}");
    assert!(session.dispatches <= 6, "it gives up at the deadline");
  }

  #[test]
  fn backends_without_vt_switching_need_the_option() {
    assert_eq!(
      check_backend(Some("builtin"), false),
      Err(NoSeat::NoVtSwitching("builtin".into()))
    );
    assert!(check_backend(Some("noop"), false).is_err());
    assert_eq!(check_backend(Some("builtin"), true), Ok(()));
    assert_eq!(check_backend(Some("logind"), false), Ok(()));
    assert_eq!(check_backend(Some("seatd"), false), Ok(()));
    assert_eq!(check_backend(None, false), Ok(()));
    let message = NoSeat::NoVtSwitching("builtin".into()).to_string();
    assert!(message.contains("--allow-builtin-seat"), "{message}");
  }

  #[test]
  fn the_backend_comes_from_libseats_log_line() {
    assert_eq!(
      opened_backend(
        "[libseat/libseat.c:83] Seat opened with backend 'logind'"
      ),
      Some("logind")
    );
    assert_eq!(
      opened_backend("Seat opened with backend 'seatd'"),
      Some("seatd")
    );
    assert_eq!(opened_backend("Backend 'seatd' failed to open seat"), None);
    assert_eq!(opened_backend("Seat opened with backend 'x"), None);
  }

  #[test]
  fn libseat_errors_name_their_cause() {
    let open = NoSeat::OpenFailed("No such file or directory".into());
    assert_eq!(
      open.to_string(),
      "no seat: libseat could not open one (No such file or directory); \
       run from a VT login or start seatd"
    );
  }
}
