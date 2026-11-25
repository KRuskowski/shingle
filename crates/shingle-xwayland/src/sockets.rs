//! The X display: its lock file and listening sockets.
//!
//! Shingle binds the display itself before Xwayland runs, as wlroots'
//! `xwayland/sockets.c` does, so `DISPLAY` is known from the start and a
//! lazy server can be started by the first connection.

use std::{
  fs::{self, DirBuilder, OpenOptions},
  io::{self, Read, Write},
  os::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
  },
  path::{Path, PathBuf},
};

use rustix::{
  io::Errno,
  net::{
    AddressFamily, SocketAddrUnix, SocketFlags, SocketType, bind, listen,
    socket_with,
  },
  process::{Pid, getpid, test_kill_process},
};

/// The highest display number tried, as in wlroots.
pub const MAX_DISPLAY: u32 = 32;

/// A bound X display `:N`: the lock file `.XN-lock` and two listening
/// sockets, `.X11-unix/XN` in the filesystem and the abstract socket of the
/// same name.
///
/// Both sockets are close-on-exec; the server process clears that flag only
/// in its own child. Dropping the value removes the socket file and the
/// lock.
#[derive(Debug)]
pub struct DisplaySockets {
  display: u32,
  lock_path: PathBuf,
  socket_path: PathBuf,
  filesystem: OwnedFd,
  abstract_socket: OwnedFd,
}

impl DisplaySockets {
  /// Binds the first free display under `/tmp`, the location every X
  /// client looks in.
  ///
  /// # Errors
  ///
  /// When `/tmp/.X11-unix` cannot be created or used, or when displays 0
  /// to [`MAX_DISPLAY`] are all taken.
  pub fn bind() -> io::Result<Self> {
    Self::bind_in(Path::new("/tmp"))
  }

  /// Binds the first free display with `base` in place of `/tmp`: the lock
  /// is `base/.XN-lock` and the sockets are `base/.X11-unix/XN` and the
  /// abstract socket named by that path. Tests use a temporary directory.
  ///
  /// # Errors
  ///
  /// As [`DisplaySockets::bind`].
  pub fn bind_in(base: &Path) -> io::Result<Self> {
    Self::bind_with(base, process_exists)
  }

  /// [`DisplaySockets::bind_in`] with `exists` deciding whether the PID in
  /// an existing lock file still runs.
  fn bind_with(base: &Path, exists: impl Fn(i32) -> bool) -> io::Result<Self> {
    let socket_dir = base.join(".X11-unix");
    ensure_socket_dir(&socket_dir)?;
    for display in 0..=MAX_DISPLAY {
      let lock_path = base.join(format!(".X{display}-lock"));
      if !take_lock(&lock_path, &exists)? {
        continue;
      }
      let socket_path = socket_dir.join(format!("X{display}"));
      match bind_sockets(&socket_path) {
        Ok((filesystem, abstract_socket)) => {
          return Ok(Self {
            display,
            lock_path,
            socket_path,
            filesystem,
            abstract_socket,
          });
        }
        Err(_) => {
          let _ = fs::remove_file(&lock_path);
        }
      }
    }
    Err(io::Error::new(
      io::ErrorKind::AddrInUse,
      format!("no free X display from :0 to :{MAX_DISPLAY}"),
    ))
  }

  /// The display number `N`.
  pub fn display(&self) -> u32 {
    self.display
  }

  /// The `DISPLAY` value, `:N`.
  pub fn name(&self) -> String {
    format!(":{}", self.display)
  }

  /// The lock file.
  pub fn lock_path(&self) -> &Path {
    &self.lock_path
  }

  /// The filesystem socket.
  pub fn socket_path(&self) -> &Path {
    &self.socket_path
  }

  /// The two listening descriptors, filesystem first, which Xwayland gets
  /// as `-listenfd` and a lazy server watches for the first connection.
  pub fn listen_fds(&self) -> [BorrowedFd<'_>; 2] {
    [self.filesystem.as_fd(), self.abstract_socket.as_fd()]
  }
}

impl Drop for DisplaySockets {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.socket_path);
    let _ = fs::remove_file(&self.lock_path);
  }
}

/// Whether a process with `pid` exists. A process of another user, which
/// `kill(pid, 0)` refuses with `EPERM`, exists too.
fn process_exists(pid: i32) -> bool {
  let Some(pid) = Pid::from_raw(pid.max(0)) else {
    return false;
  };
  !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

/// Creates the socket directory with mode 1777 when it is missing. An
/// existing directory is used as it is.
fn ensure_socket_dir(path: &Path) -> io::Result<()> {
  match DirBuilder::new().mode(0o1777).create(path) {
    Ok(()) => {
      // The umask may have cleared bits from the mode above.
      fs::set_permissions(path, fs::Permissions::from_mode(0o1777))
    }
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
      if fs::metadata(path)?.is_dir() {
        Ok(())
      } else {
        Err(io::Error::new(
          io::ErrorKind::AlreadyExists,
          format!("{} is not a directory", path.display()),
        ))
      }
    }
    Err(error) => Err(error),
  }
}

/// The contents of a lock file for `pid`: the PID right-aligned in ten
/// columns and a newline, as the X server writes it.
pub fn lock_contents(pid: i32) -> String {
  format!("{pid:>10}\n")
}

/// Parses a lock file. `None` means it is malformed, and the display is
/// treated as taken.
pub fn parse_lock(contents: &[u8]) -> Option<i32> {
  if contents.len() != 11 || contents[10] != b'\n' {
    return None;
  }
  std::str::from_utf8(&contents[..10])
    .ok()?
    .trim_start()
    .parse::<i32>()
    .ok()
    .filter(|pid| *pid > 0)
}

/// Tries to take the lock at `path`. Returns `false` when another live
/// process holds it or it cannot be read. A lock whose process is gone is
/// removed and taken.
fn take_lock(path: &Path, exists: &impl Fn(i32) -> bool) -> io::Result<bool> {
  // One retry after removing a stale lock, as wlroots does.
  for _ in 0..2 {
    match OpenOptions::new()
      .write(true)
      .create_new(true)
      .mode(0o444)
      .open(path)
    {
      Ok(mut file) => {
        let written = file
          .write_all(lock_contents(getpid().as_raw_nonzero().get()).as_bytes());
        if written.is_err() {
          let _ = fs::remove_file(path);
          return Ok(false);
        }
        return Ok(true);
      }
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
      Err(error) => return Err(error),
    }
    let mut contents = Vec::new();
    let read =
      fs::File::open(path).and_then(|mut file| file.read_to_end(&mut contents));
    if read.is_err() {
      return Ok(false);
    }
    match parse_lock(&contents) {
      Some(pid) if !exists(pid) => {
        if fs::remove_file(path).is_err() {
          return Ok(false);
        }
      }
      _ => return Ok(false),
    }
  }
  Ok(false)
}

/// Binds and listens on the filesystem socket at `path`, after removing
/// anything left there, and on the abstract socket named by `path`.
fn bind_sockets(path: &Path) -> io::Result<(OwnedFd, OwnedFd)> {
  let _ = fs::remove_file(path);
  let filesystem = listening_socket(&SocketAddrUnix::new(path)?)?;
  let name = path.as_os_str().as_encoded_bytes();
  let abstract_socket =
    match listening_socket(&SocketAddrUnix::new_abstract_name(name)?) {
      Ok(socket) => socket,
      Err(error) => {
        let _ = fs::remove_file(path);
        return Err(error);
      }
    };
  Ok((filesystem, abstract_socket))
}

/// A close-on-exec Unix stream socket bound to `address` with a backlog of
/// one, as wlroots uses.
fn listening_socket(address: &SocketAddrUnix) -> io::Result<OwnedFd> {
  let socket = socket_with(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::CLOEXEC,
    None,
  )?;
  bind(&socket, address)?;
  listen(&socket, 1)?;
  Ok(socket)
}

#[cfg(test)]
mod tests {
  use std::os::unix::net::UnixStream;

  use super::*;

  /// A fresh directory under the system temporary directory.
  fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir()
      .join(format!("shingle-xwayland-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
  }

  /// Writes a lock for `pid` at display `display` under `base`.
  fn write_lock(base: &Path, display: u32, pid: i32) {
    fs::write(base.join(format!(".X{display}-lock")), lock_contents(pid))
      .unwrap();
  }

  #[test]
  fn lock_contents_match_the_x_server_format() {
    assert_eq!(lock_contents(42), "        42\n");
    assert_eq!(parse_lock(b"        42\n"), Some(42));
    assert_eq!(parse_lock(b"42\n"), None);
    assert_eq!(parse_lock(b"        42 "), None);
    assert_eq!(parse_lock(b"     hello\n"), None);
    assert_eq!(parse_lock(b"         0\n"), None);
  }

  #[test]
  fn binds_display_zero_in_an_empty_directory() {
    let base = temp_dir("empty");
    let sockets = DisplaySockets::bind_in(&base).unwrap();
    assert_eq!(sockets.display(), 0);
    assert_eq!(sockets.name(), ":0");
    let mode = fs::metadata(base.join(".X11-unix")).unwrap().permissions();
    assert_eq!(mode.mode() & 0o7777, 0o1777);
    let lock = fs::read(sockets.lock_path()).unwrap();
    assert_eq!(parse_lock(&lock), Some(std::process::id() as i32));
    // A client can connect to the filesystem socket.
    UnixStream::connect(sockets.socket_path()).unwrap();
    drop(sockets);
    assert!(!base.join(".X0-lock").exists());
    assert!(!base.join(".X11-unix/X0").exists());
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn skips_displays_locked_by_live_processes() {
    let base = temp_dir("live");
    write_lock(&base, 0, 100);
    write_lock(&base, 1, 101);
    let sockets = DisplaySockets::bind_with(&base, |_| true).unwrap();
    assert_eq!(sockets.display(), 2);
    // The other locks are left alone.
    assert!(base.join(".X0-lock").exists());
    drop(sockets);
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn replaces_a_stale_lock() {
    let base = temp_dir("stale");
    write_lock(&base, 0, 100);
    write_lock(&base, 1, 101);
    let sockets = DisplaySockets::bind_with(&base, |pid| pid != 100).unwrap();
    assert_eq!(sockets.display(), 0);
    let lock = fs::read(base.join(".X0-lock")).unwrap();
    assert_eq!(parse_lock(&lock), Some(std::process::id() as i32));
    drop(sockets);
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn a_malformed_lock_counts_as_taken() {
    let base = temp_dir("malformed");
    fs::write(base.join(".X0-lock"), "garbage").unwrap();
    let sockets = DisplaySockets::bind_with(&base, |_| false).unwrap();
    assert_eq!(sockets.display(), 1);
    assert!(base.join(".X0-lock").exists());
    drop(sockets);
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn a_leftover_socket_file_is_replaced() {
    let base = temp_dir("leftover");
    fs::create_dir(base.join(".X11-unix")).unwrap();
    fs::write(base.join(".X11-unix/X0"), "").unwrap();
    let sockets = DisplaySockets::bind_in(&base).unwrap();
    assert_eq!(sockets.display(), 0);
    UnixStream::connect(sockets.socket_path()).unwrap();
    drop(sockets);
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn fails_when_every_display_is_taken() {
    let base = temp_dir("full");
    for display in 0..=MAX_DISPLAY {
      write_lock(&base, display, 100);
    }
    let error = DisplaySockets::bind_with(&base, |_| true).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn a_socket_path_that_is_a_file_fails() {
    let base = temp_dir("notdir");
    fs::write(base.join(".X11-unix"), "").unwrap();
    assert!(DisplaySockets::bind_in(&base).is_err());
    fs::remove_dir_all(base).unwrap();
  }

  #[test]
  fn live_processes_exist() {
    assert!(process_exists(std::process::id() as i32));
    assert!(!process_exists(0));
    assert!(!process_exists(-1));
  }

  #[test]
  fn the_abstract_socket_is_bound_too() {
    let base = temp_dir("abstract");
    let sockets = DisplaySockets::bind_in(&base).unwrap();
    let name = sockets.socket_path().as_os_str().as_encoded_bytes();
    let address = SocketAddrUnix::new_abstract_name(name).unwrap();
    let client = socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .unwrap();
    rustix::net::connect(&client, &address).unwrap();
    // A second compositor gets the next display.
    let second = DisplaySockets::bind_in(&base).unwrap();
    assert_eq!(second.display(), 1);
    drop((sockets, second));
    fs::remove_dir_all(base).unwrap();
  }
}
