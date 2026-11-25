//! XWayland for Shingle: the X display sockets, the supervised `Xwayland`
//! process and Shingle's own X window manager.
//!
//! Nothing here depends on `wayland-server`. The `shingle` crate owns one
//! [`XwaylandServer`], adds the Wayland connection [`XwaylandServer::start`]
//! returns as a client, watches the descriptors this crate exposes in its
//! event loop and connects an [`Xwm`] when the server is ready. The design
//! is docs/design/xwayland.md.
//!
//! The lifecycle:
//!
//! 1. [`DisplaySockets::bind`] takes a free display `:N` when Shingle
//!    starts, so `DISPLAY` is known before any client runs.
//! 2. In [`StartMode::Eager`] the server starts right away; in
//!    [`StartMode::Lazy`] on the first connection to either display socket.
//! 3. When the readiness pipe reports the display number, the window
//!    manager connects ([`Xwm::start`]).
//! 4. When the process exits, [`XwaylandServer::reap`] applies the restart
//!    rule ([`restart_after_exit`]).

#![warn(missing_docs)]

pub mod server;
pub mod sockets;
pub mod xwm;

use std::{
  ffi::OsString, io, os::unix::net::UnixStream, process::ExitStatus,
  time::Duration,
};

pub use server::{
  Readiness, Restart, StartMode, XwaylandProcess, restart_after_exit,
};
pub use sockets::DisplaySockets;
pub use xwm::{X11Connection, XConnection, Xwm, XwmError, XwmEvent};

/// How a server process ended, and what happens next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Exited {
  /// Its exit status.
  pub status: ExitStatus,
  /// How long it ran.
  pub ran_for: Duration,
  /// The restart rule's decision.
  pub restart: Restart,
}

/// The X display and the server process behind it.
#[derive(Debug)]
pub struct XwaylandServer {
  // Declared first so that it is dropped, and the process stopped, before
  // the sockets and lock are removed.
  process: Option<XwaylandProcess>,
  sockets: DisplaySockets,
  mode: StartMode,
  program: OsString,
  waiting_for_connection: bool,
}

impl XwaylandServer {
  /// A server for the bound `sockets` that runs `program` (see
  /// [`server::program`]). Nothing is started yet; in lazy mode the
  /// server waits for the first connection.
  pub fn new(
    sockets: DisplaySockets,
    mode: StartMode,
    program: OsString,
  ) -> Self {
    Self {
      sockets,
      mode,
      program,
      process: None,
      waiting_for_connection: mode == StartMode::Lazy,
    }
  }

  /// The display sockets.
  pub fn sockets(&self) -> &DisplaySockets {
    &self.sockets
  }

  /// The `DISPLAY` value, `:N`.
  pub fn display_name(&self) -> String {
    self.sockets.name()
  }

  /// The start mode.
  pub fn mode(&self) -> StartMode {
    self.mode
  }

  /// The running process, if any.
  pub fn process(&self) -> Option<&XwaylandProcess> {
    self.process.as_ref()
  }

  /// The running process, if any.
  pub fn process_mut(&mut self) -> Option<&mut XwaylandProcess> {
    self.process.as_mut()
  }

  /// Whether the display sockets should be watched so that the next
  /// connection starts the server.
  pub fn waiting_for_connection(&self) -> bool {
    self.waiting_for_connection
  }

  /// Starts the server and returns the compositor end of its Wayland
  /// connection. A running server is left alone and yields an error.
  ///
  /// # Errors
  ///
  /// When the server already runs or cannot be spawned. After a failed
  /// spawn the server stays stopped and does not wait for connections.
  pub fn start(&mut self) -> io::Result<UnixStream> {
    if self.process.is_some() {
      return Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Xwayland is already running",
      ));
    }
    self.waiting_for_connection = false;
    let spawned = server::spawn(&self.program, &self.sockets, self.mode)?;
    self.process = Some(spawned.process);
    Ok(spawned.wayland)
  }

  /// Reaps the process if it exited and applies the restart rule. With
  /// [`Restart::OnConnection`] the server waits for a connection again;
  /// with [`Restart::Now`] the caller starts it.
  ///
  /// # Errors
  ///
  /// When `waitpid` fails.
  pub fn reap(&mut self) -> io::Result<Option<Exited>> {
    let Some(process) = &mut self.process else {
      return Ok(None);
    };
    let ran_for = process.ran_for();
    let Some(status) = process.try_exit()? else {
      return Ok(None);
    };
    self.process = None;
    let restart = restart_after_exit(self.mode, ran_for);
    self.waiting_for_connection = restart == Restart::OnConnection;
    Ok(Some(Exited {
      status,
      ran_for,
      restart,
    }))
  }

  /// Stops the server, if it runs, with SIGTERM and then SIGKILL after
  /// `grace`, and stops waiting for connections. The sockets stay bound
  /// until the value is dropped.
  pub fn stop(&mut self, grace: Duration) {
    self.waiting_for_connection = false;
    if let Some(mut process) = self.process.take() {
      process.terminate(grace);
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{fs, path::PathBuf, time::Instant};

  use super::*;

  /// A directory with a display and a fake server running `body`.
  fn fixture(name: &str, body: &str) -> (PathBuf, DisplaySockets, OsString) {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!(
      "shingle-xwayland-lib-{name}-{}",
      std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let sockets = DisplaySockets::bind_in(&directory).unwrap();
    let program = directory.join("fake-xwayland");
    fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    (directory, sockets, program.into_os_string())
  }

  /// Waits until `server` reaps its process.
  fn wait_exit(server: &mut XwaylandServer) -> Exited {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      if let Some(exited) = server.reap().unwrap() {
        return exited;
      }
      assert!(Instant::now() < deadline, "the fake server did not exit");
      std::thread::sleep(Duration::from_millis(10));
    }
  }

  #[test]
  fn lazy_mode_waits_for_a_connection_first() {
    let (directory, sockets, program) = fixture("lazy", "exit 0");
    let mut server = XwaylandServer::new(sockets, StartMode::Lazy, program);
    assert!(server.waiting_for_connection());
    assert!(server.process().is_none());
    assert_eq!(server.display_name(), ":0");
    let _wayland = server.start().unwrap();
    assert!(!server.waiting_for_connection());
    assert!(server.start().is_err());
    // A quick exit is not restarted, not even lazily.
    let exited = wait_exit(&mut server);
    assert_eq!(exited.restart, Restart::Never);
    assert!(exited.status.success());
    assert!(!server.waiting_for_connection());
    assert!(server.reap().unwrap().is_none());
    drop(server);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn eager_mode_does_not_wait_for_connections() {
    let (directory, sockets, program) = fixture("eager", "exec sleep 60");
    let mut server = XwaylandServer::new(sockets, StartMode::Eager, program);
    assert!(!server.waiting_for_connection());
    let _wayland = server.start().unwrap();
    assert!(server.process().unwrap().pid() > 0);
    assert!(server.reap().unwrap().is_none());
    server.stop(Duration::from_secs(5));
    assert!(server.process().is_none());
    drop(server);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn dropping_the_server_removes_the_display() {
    let (directory, sockets, program) = fixture("drop", "exec sleep 60");
    let lock = sockets.lock_path().to_owned();
    let socket = sockets.socket_path().to_owned();
    let mut server = XwaylandServer::new(sockets, StartMode::Eager, program);
    let _wayland = server.start().unwrap();
    drop(server);
    assert!(!lock.exists());
    assert!(!socket.exists());
    fs::remove_dir_all(directory).unwrap();
  }
}
