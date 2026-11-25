//! The Xwayland process: its command line, start, readiness, exit and the
//! restart rule.

use std::{
  ffi::{OsStr, OsString},
  io,
  os::{
    fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    unix::{net::UnixStream, process::CommandExt},
  },
  process::{Child, Command, ExitStatus},
  time::{Duration, Instant},
};

use rustix::{
  event::{PollFd, PollFlags, Timespec, poll},
  fs::{OFlags, fcntl_getfl, fcntl_setfl},
  io::{FdFlags, fcntl_setfd},
  pipe::{PipeFlags, pipe_with},
  process::{Pid, PidfdFlags, Signal, kill_process, pidfd_open},
};

use crate::sockets::DisplaySockets;

/// How long Xwayland must have run for Shingle to restart it after it
/// exits, wlroots' rule against crash loops (`handle_client_destroy`).
pub const MIN_RUN_FOR_RESTART: Duration = Duration::from_secs(5);

/// Seconds a lazily started Xwayland waits after its last X client before
/// it exits, as wlroots' `terminate_delay` in lazy mode.
pub const LAZY_TERMINATE_DELAY: u32 = 10;

/// The environment variable that names another Xwayland binary, like
/// wlroots' `WLR_XWAYLAND`.
pub const PROGRAM_VARIABLE: &str = "SHINGLE_XWAYLAND";

/// When Xwayland is started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartMode {
  /// Right away, and again whenever it exits (Wayfire's `xwayland = true`).
  Eager,
  /// On the first connection to the display (`xwayland = lazy`). The server
  /// exits [`LAZY_TERMINATE_DELAY`] seconds after its last X client.
  Lazy,
}

/// What to do after Xwayland exited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Restart {
  /// Start it again now.
  Now,
  /// Watch the display sockets and start it on the next connection.
  OnConnection,
  /// Leave the display without a server until Shingle restarts.
  Never,
}

/// The restart rule: a server that ran for more than
/// [`MIN_RUN_FOR_RESTART`] is restarted, eagerly or lazily by `mode`, and
/// one that exited sooner is not, as in wlroots.
pub fn restart_after_exit(mode: StartMode, ran_for: Duration) -> Restart {
  if ran_for <= MIN_RUN_FOR_RESTART {
    Restart::Never
  } else {
    match mode {
      StartMode::Eager => Restart::Now,
      StartMode::Lazy => Restart::OnConnection,
    }
  }
}

/// The program to run: `override_program` (the value of
/// [`PROGRAM_VARIABLE`]) when set and not empty, else `Xwayland` from
/// `PATH`.
pub fn program(override_program: Option<OsString>) -> OsString {
  override_program
    .filter(|program| !program.is_empty())
    .unwrap_or_else(|| "Xwayland".into())
}

/// The descriptors Xwayland is given, by number in the child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildFds {
  /// The two display sockets, for `-listenfd`.
  pub listen: [RawFd; 2],
  /// The write end of the readiness pipe, for `-displayfd`.
  pub displayfd: RawFd,
  /// The server end of the window manager connection, for `-wm`.
  pub wm: RawFd,
}

/// Xwayland's arguments after the program name, as wlroots builds them
/// (`xwayland/server.c`): `:N -rootless -core -terminate [DELAY]` and the
/// descriptors. `DELAY` is given only in lazy mode.
pub fn arguments(
  display: &str,
  mode: StartMode,
  fds: ChildFds,
) -> Vec<OsString> {
  let mut arguments: Vec<OsString> =
    vec![display.into(), "-rootless".into(), "-core".into()];
  arguments.push("-terminate".into());
  if mode == StartMode::Lazy {
    arguments.push(LAZY_TERMINATE_DELAY.to_string().into());
  }
  for fd in fds.listen {
    arguments.push("-listenfd".into());
    arguments.push(fd.to_string().into());
  }
  arguments.push("-displayfd".into());
  arguments.push(fds.displayfd.to_string().into());
  arguments.push("-wm".into());
  arguments.push(fds.wm.to_string().into());
  arguments
}

/// What the readiness pipe said.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
  /// Nothing complete yet.
  Pending,
  /// Xwayland wrote its display number: it accepts X clients now.
  Ready(u32),
  /// The pipe closed before a display number arrived, so Xwayland failed
  /// to start; its exit follows.
  Closed,
}

/// A started Xwayland and the compositor's ends of its connections.
#[derive(Debug)]
pub struct Spawned {
  /// The process.
  pub process: XwaylandProcess,
  /// The compositor end of Xwayland's Wayland connection, which it got as
  /// `WAYLAND_SOCKET`; the compositor adds it as a client.
  pub wayland: UnixStream,
}

/// A running Xwayland, watched through a pidfd. Dropping it terminates the
/// process and reaps it.
#[derive(Debug)]
pub struct XwaylandProcess {
  child: Option<Child>,
  pidfd: OwnedFd,
  displayfd: Option<OwnedFd>,
  ready_bytes: Vec<u8>,
  wm: Option<UnixStream>,
  started: Instant,
}

/// Starts `program` as the rootless Xwayland for `sockets`.
///
/// Two socket pairs carry its Wayland connection (`WAYLAND_SOCKET`) and
/// the window manager connection (`-wm`), and a pipe its readiness
/// (`-displayfd`). Only those descriptors and the display sockets are
/// inherited. Its stdout and stderr are Shingle's, so its log ends up in
/// Shingle's.
///
/// # Errors
///
/// When the pipes, sockets or the process cannot be created.
pub fn spawn(
  program: &OsStr,
  sockets: &DisplaySockets,
  mode: StartMode,
) -> io::Result<Spawned> {
  let (wayland, wayland_child) = UnixStream::pair()?;
  let (wm, wm_child) = UnixStream::pair()?;
  let (ready_read, ready_write) = pipe_with(PipeFlags::CLOEXEC)?;
  let flags = fcntl_getfl(&ready_read)?;
  fcntl_setfl(&ready_read, flags | OFlags::NONBLOCK)?;
  let [listen0, listen1] = sockets.listen_fds();
  let fds = ChildFds {
    listen: [listen0.as_raw_fd(), listen1.as_raw_fd()],
    displayfd: ready_write.as_raw_fd(),
    wm: wm_child.as_raw_fd(),
  };
  let inherited = [
    fds.listen[0],
    fds.listen[1],
    fds.displayfd,
    fds.wm,
    wayland_child.as_raw_fd(),
  ];
  let mut command = Command::new(program);
  command
    .args(arguments(&sockets.name(), mode, fds))
    .env("WAYLAND_SOCKET", wayland_child.as_raw_fd().to_string())
    .env_remove("WAYLAND_DISPLAY")
    .env_remove("DISPLAY");
  // SAFETY: the closure only makes fcntl system calls, which are
  // async-signal-safe, on descriptors that stay open in the parent until
  // spawn returns.
  unsafe {
    command.pre_exec(move || {
      for fd in inherited {
        // SAFETY: `fd` is open, see above.
        let fd = BorrowedFd::borrow_raw(fd);
        fcntl_setfd(fd, FdFlags::empty())?;
      }
      Ok(())
    });
  }
  let child = command.spawn()?;
  // The child has its copies; the parent's ends close here.
  drop((ready_write, wm_child, wayland_child));
  let pid = Pid::from_child(&child);
  let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
    Ok(pidfd) => pidfd,
    Err(error) => {
      let mut child = child;
      let _ = child.kill();
      let _ = child.wait();
      return Err(error.into());
    }
  };
  Ok(Spawned {
    process: XwaylandProcess {
      child: Some(child),
      pidfd,
      displayfd: Some(ready_read),
      ready_bytes: Vec::new(),
      wm: Some(wm),
      started: Instant::now(),
    },
    wayland,
  })
}

impl XwaylandProcess {
  /// The process ID.
  pub fn pid(&self) -> u32 {
    self.child.as_ref().map_or(0, Child::id)
  }

  /// A descriptor that becomes readable when the process exits.
  pub fn pidfd(&self) -> BorrowedFd<'_> {
    self.pidfd.as_fd()
  }

  /// The read end of the readiness pipe, until it reported readiness or
  /// closed.
  pub fn displayfd(&self) -> Option<BorrowedFd<'_>> {
    self.displayfd.as_ref().map(AsFd::as_fd)
  }

  /// How long ago the process was started.
  pub fn ran_for(&self) -> Duration {
    self.started.elapsed()
  }

  /// Reads what is available on the readiness pipe. After `Ready` or
  /// `Closed` the pipe is closed and [`XwaylandProcess::displayfd`] is
  /// `None`.
  ///
  /// # Errors
  ///
  /// When reading fails for another reason than an empty pipe.
  pub fn read_readiness(&mut self) -> io::Result<Readiness> {
    let Some(pipe) = &self.displayfd else {
      return Ok(Readiness::Closed);
    };
    let mut buffer = [0; 32];
    loop {
      match rustix::io::read(pipe, &mut buffer) {
        Ok(0) => {
          self.displayfd = None;
          return Ok(parse_readiness(&self.ready_bytes, true));
        }
        Ok(count) => {
          self.ready_bytes.extend_from_slice(&buffer[..count]);
          if let Readiness::Ready(display) =
            parse_readiness(&self.ready_bytes, false)
          {
            self.displayfd = None;
            return Ok(Readiness::Ready(display));
          }
        }
        Err(rustix::io::Errno::AGAIN) => return Ok(Readiness::Pending),
        Err(rustix::io::Errno::INTR) => {}
        Err(error) => return Err(error.into()),
      }
    }
  }

  /// Takes the compositor end of the window manager connection, once.
  pub fn take_wm_stream(&mut self) -> Option<UnixStream> {
    self.wm.take()
  }

  /// Reaps the process if it has exited.
  ///
  /// # Errors
  ///
  /// When `waitpid` fails.
  pub fn try_exit(&mut self) -> io::Result<Option<ExitStatus>> {
    let Some(child) = &mut self.child else {
      return Ok(None);
    };
    let status = child.try_wait()?;
    if status.is_some() {
      self.child = None;
    }
    Ok(status)
  }

  /// Asks the process to exit with SIGTERM, waits up to `grace` for it,
  /// then kills it with SIGKILL, and reaps it.
  pub fn terminate(&mut self, grace: Duration) {
    let Some(mut child) = self.child.take() else {
      return;
    };
    if let Ok(Some(_)) = child.try_wait() {
      return;
    }
    let pid = Pid::from_child(&child);
    let _ = kill_process(pid, Signal::TERM);
    if !wait_readable(self.pidfd.as_fd(), grace) {
      let _ = child.kill();
    }
    let _ = child.wait();
  }
}

impl Drop for XwaylandProcess {
  fn drop(&mut self) {
    self.terminate(Duration::from_secs(1));
  }
}

/// Parses the readiness pipe's bytes: a display number and a newline.
/// With `closed`, the pipe has reached its end, and a number without the
/// newline still counts.
pub fn parse_readiness(bytes: &[u8], closed: bool) -> Readiness {
  let text = String::from_utf8_lossy(bytes);
  let complete = match text.find('\n') {
    Some(end) => Some(&text[..end]),
    None if closed => Some(text.as_ref()),
    None => None,
  };
  match complete.map(|line| line.trim().parse::<u32>()) {
    Some(Ok(display)) => Readiness::Ready(display),
    Some(Err(_)) => Readiness::Closed,
    None => Readiness::Pending,
  }
}

/// Waits up to `timeout` for `fd` to become readable.
fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
  let deadline = Instant::now() + timeout;
  loop {
    let left = deadline.saturating_duration_since(Instant::now());
    let timespec = Timespec {
      tv_sec: left.as_secs() as i64,
      tv_nsec: i64::from(left.subsec_nanos()),
    };
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    match poll(&mut fds, Some(&timespec)) {
      Ok(count) => return count > 0,
      Err(rustix::io::Errno::INTR) if !left.is_zero() => {}
      Err(_) => return false,
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{fs, io::Read, path::PathBuf};

  use super::*;

  const FDS: ChildFds = ChildFds {
    listen: [3, 4],
    displayfd: 5,
    wm: 6,
  };

  #[test]
  fn eager_arguments_match_wlroots() {
    assert_eq!(
      arguments(":2", StartMode::Eager, FDS),
      [
        ":2",
        "-rootless",
        "-core",
        "-terminate",
        "-listenfd",
        "3",
        "-listenfd",
        "4",
        "-displayfd",
        "5",
        "-wm",
        "6",
      ]
    );
  }

  #[test]
  fn lazy_arguments_add_the_terminate_delay() {
    let arguments = arguments(":0", StartMode::Lazy, FDS);
    assert_eq!(arguments[3..5], ["-terminate", "10"]);
    assert_eq!(arguments.len(), 13);
  }

  #[test]
  fn restart_rule_depends_on_run_time_and_mode() {
    let short = Duration::from_secs(2);
    let exact = MIN_RUN_FOR_RESTART;
    let long = Duration::from_secs(6);
    assert_eq!(restart_after_exit(StartMode::Eager, short), Restart::Never);
    assert_eq!(restart_after_exit(StartMode::Lazy, short), Restart::Never);
    assert_eq!(restart_after_exit(StartMode::Eager, exact), Restart::Never);
    assert_eq!(restart_after_exit(StartMode::Eager, long), Restart::Now);
    assert_eq!(
      restart_after_exit(StartMode::Lazy, long),
      Restart::OnConnection
    );
  }

  #[test]
  fn program_is_xwayland_unless_overridden() {
    assert_eq!(program(None), "Xwayland");
    assert_eq!(program(Some("".into())), "Xwayland");
    assert_eq!(program(Some("/opt/Xwayland".into())), "/opt/Xwayland");
  }

  #[test]
  fn readiness_needs_a_display_number_and_newline() {
    assert_eq!(parse_readiness(b"", false), Readiness::Pending);
    assert_eq!(parse_readiness(b"3", false), Readiness::Pending);
    assert_eq!(parse_readiness(b"3\n", false), Readiness::Ready(3));
    assert_eq!(parse_readiness(b"3", true), Readiness::Ready(3));
    assert_eq!(parse_readiness(b"", true), Readiness::Closed);
    assert_eq!(parse_readiness(b"x\n", false), Readiness::Closed);
  }

  /// A directory for one test's display and fake server.
  fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
      "shingle-xwayland-server-{name}-{}",
      std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
  }

  /// Writes an executable bash script standing in for Xwayland. Bash,
  /// unlike dash, redirects descriptors above 9.
  fn fake_server(directory: &std::path::Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("fake-xwayland");
    fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
  }

  /// Polls `process` until it reports something other than `Pending`.
  fn wait_ready(process: &mut XwaylandProcess) -> Readiness {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let readiness = process.read_readiness().unwrap();
      if readiness != Readiness::Pending || Instant::now() > deadline {
        return readiness;
      }
      std::thread::sleep(Duration::from_millis(10));
    }
  }

  #[test]
  fn a_started_server_gets_its_descriptors_and_reports_ready() {
    let directory = temp_dir("ready");
    let sockets = DisplaySockets::bind_in(&directory).unwrap();
    let out = directory.join("out");
    // Records its arguments and environment, checks that each descriptor
    // it was given is open, writes the display number and waits for the
    // window manager connection to close.
    let script = format!(
      concat!(
        "echo \"$@\" > {out}\n",
        "echo \"socket=$WAYLAND_SOCKET display=${{DISPLAY-unset}}\" >> {out}\n",
        "for fd in $7 $9 ${{11}} ${{13}} $WAYLAND_SOCKET; do\n",
        "  test -e /proc/self/fd/$fd && echo open $fd >> {out}\n",
        "done\n",
        "echo 7 >&${{11}}\n",
        "cat <&${{13}} >/dev/null\n",
      ),
      out = out.display()
    );
    let program = fake_server(&directory, &script);
    let Spawned {
      mut process,
      wayland,
    } = spawn(program.as_os_str(), &sockets, StartMode::Lazy).unwrap();
    assert!(process.pid() > 0);
    assert_eq!(wait_ready(&mut process), Readiness::Ready(7));
    assert!(process.displayfd().is_none());
    assert_eq!(process.try_exit().unwrap(), None);
    let wm = process.take_wm_stream().unwrap();
    assert!(process.take_wm_stream().is_none());
    drop(wm);
    assert!(wait_readable(process.pidfd(), Duration::from_secs(10)));
    assert!(process.try_exit().unwrap().unwrap().success());
    drop(wayland);
    let mut text = String::new();
    fs::File::open(&out)
      .unwrap()
      .read_to_string(&mut text)
      .unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert!(lines[0].starts_with(":0 -rootless -core -terminate 10 -listenfd"));
    assert!(lines[1].ends_with("display=unset"));
    assert_eq!(lines.len(), 7, "{text}");
    drop(process);
    drop(sockets);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_failed_start_closes_the_pipe() {
    let directory = temp_dir("fail");
    let sockets = DisplaySockets::bind_in(&directory).unwrap();
    let program = fake_server(&directory, "exit 3");
    let mut process = spawn(program.as_os_str(), &sockets, StartMode::Eager)
      .unwrap()
      .process;
    assert_eq!(wait_ready(&mut process), Readiness::Closed);
    assert!(wait_readable(process.pidfd(), Duration::from_secs(10)));
    assert_eq!(process.try_exit().unwrap().unwrap().code(), Some(3));
    assert_eq!(process.try_exit().unwrap(), None);
    drop(process);
    drop(sockets);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn terminate_stops_a_running_server() {
    let directory = temp_dir("terminate");
    let sockets = DisplaySockets::bind_in(&directory).unwrap();
    let program = fake_server(&directory, "exec sleep 60");
    let mut process = spawn(program.as_os_str(), &sockets, StartMode::Eager)
      .unwrap()
      .process;
    let started = Instant::now();
    process.terminate(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(process.try_exit().unwrap(), None);
    drop(process);
    drop(sockets);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_missing_program_is_an_error() {
    let directory = temp_dir("missing");
    let sockets = DisplaySockets::bind_in(&directory).unwrap();
    let program = OsStr::new("/nonexistent/shingle-xwayland-test");
    assert!(spawn(program, &sockets, StartMode::Eager).is_err());
    drop(sockets);
    fs::remove_dir_all(directory).unwrap();
  }
}
