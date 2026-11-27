//! swaylock's `--daemonize`: the command returns once the session is
//! locked, while the lock screen runs on in the background.
//!
//! swaylock forks after the lock is held. `shingle-lock` has GPU driver
//! threads by then, which a fork would not copy, so it forks first,
//! before it connects or opens the GPU: the parent waits on a pipe until
//! the child reports the lock held and exits 0, or exits 1 if the child
//! ends or fails before that.

use std::{
  fs::File,
  io::{Read, Write},
  os::fd::{FromRawFd, OwnedFd},
};

/// What the child keeps to report the lock held.
pub struct Notifier(Option<File>);

impl Notifier {
  /// A notifier that reports nothing, without `--daemonize`.
  pub fn none() -> Self {
    Self(None)
  }

  /// Tells the waiting parent that the session is locked; only the first
  /// call does anything.
  pub fn locked(&mut self) {
    if let Some(mut pipe) = self.0.take() {
      let _ = pipe.write_all(b"L");
    }
  }
}

/// What the parent concludes from what came through the pipe.
pub fn parent_status(read: &[u8]) -> u8 {
  if read.first() == Some(&b'L') { 0 } else { 1 }
}

/// Forks. In the parent this waits for the child's report and returns
/// the status to exit with; in the child, now in a session of its own,
/// it returns the notifier.
///
/// # Errors
///
/// When the pipe or the fork fails.
pub fn daemonize() -> std::io::Result<Result<Notifier, u8>> {
  let mut fds = [0; 2];
  // SAFETY: `fds` has room for the two descriptors pipe2 writes.
  if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
    return Err(std::io::Error::last_os_error());
  }
  // SAFETY: pipe2 returned two descriptors that nothing else owns.
  let (read, write) =
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
  // SAFETY: Called before this process starts any thread, so the child
  // gets the whole process.
  match unsafe { libc::fork() } {
    -1 => Err(std::io::Error::last_os_error()),
    0 => {
      drop(read);
      // SAFETY: setsid has no memory effects; failing is harmless.
      unsafe { libc::setsid() };
      Ok(Ok(Notifier(Some(File::from(write)))))
    }
    _ => {
      drop(write);
      let mut buffer = Vec::new();
      let _ = File::from(read).take(1).read_to_end(&mut buffer);
      Ok(Err(parent_status(&buffer)))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_parent_succeeds_only_on_the_report() {
    assert_eq!(parent_status(b"L"), 0);
    assert_eq!(parent_status(b""), 1);
    assert_eq!(parent_status(b"x"), 1);
    let mut nothing = Notifier::none();
    nothing.locked();
  }
}
