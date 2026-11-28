//! Syncing session writes to disk from a child process.
//!
//! fsync waits for the disk without regard to signals, for many seconds
//! on a busy one, and a process cannot finish exiting while any of its
//! threads is in it: SIGTERM then left Shingle in fsync for 75 to 80 s
//! (docs/status.md#sessions). So the session writer has a child process,
//! coreutils `sync FILE`, do each fsync and waits for that child, which
//! it can stop doing at any moment; the child finishes on its own and
//! nothing of Shingle waits for it.

use std::{
  ffi::{OsStr, OsString},
  io,
  path::Path,
  process::{Command, Stdio},
  time::Duration,
};

use shingle_core::{SyncTarget, sync_in_place};

/// How often a waiting write looks at its child and at whether it was
/// superseded.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The command that fsyncs the path appended to it: `sync --` by default,
/// which coreutils' `sync` runs as an fsync of each file named.
#[derive(Clone, Debug)]
pub struct SyncCommand {
  program: OsString,
  arguments: Vec<OsString>,
}

impl Default for SyncCommand {
  fn default() -> Self {
    Self::new("sync", ["--"])
  }
}

impl SyncCommand {
  /// `program` with `arguments`, to which the path to sync is appended.
  pub fn new(
    program: impl Into<OsString>,
    arguments: impl IntoIterator<Item = impl Into<OsString>>,
  ) -> Self {
    Self {
      program: program.into(),
      arguments: arguments.into_iter().map(Into::into).collect(),
    }
  }

  /// Syncs `target` by running the command on its path and waiting for
  /// it, or in this thread if the program does not exist.
  ///
  /// # Errors
  ///
  /// The spawn's error, a failure exit of the child, or, as soon as
  /// `superseded` returns true while the child still runs, an error of
  /// kind [`io::ErrorKind::Interrupted`]; the child is then left to
  /// finish by itself.
  pub fn sync(
    &self,
    target: SyncTarget<'_>,
    superseded: &dyn Fn() -> bool,
  ) -> io::Result<()> {
    let path = target.path();
    let spawned = Command::new(&self.program)
      .args(&self.arguments)
      .arg(path)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn();
    let mut child = match spawned {
      Ok(child) => child,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return sync_in_place(target);
      }
      Err(error) => return Err(error),
    };
    loop {
      if let Some(status) = child.try_wait()? {
        return if status.success() {
          Ok(())
        } else {
          Err(io::Error::other(failure(&self.program, path, status)))
        };
      }
      if superseded() {
        return Err(io::Error::new(
          io::ErrorKind::Interrupted,
          "a newer session write superseded this one",
        ));
      }
      std::thread::sleep(POLL_INTERVAL);
    }
  }
}

fn failure(
  program: &OsStr,
  path: &Path,
  status: std::process::ExitStatus,
) -> String {
  format!(
    "{} {} failed: {status}",
    program.to_string_lossy(),
    path.display()
  )
}

#[cfg(test)]
mod tests {
  use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
  };

  use super::*;

  fn temp_file(name: &str) -> PathBuf {
    let path = std::env::temp_dir()
      .join(format!("shingle-sync-{name}-{}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    path
  }

  fn shell(script: &str) -> SyncCommand {
    // `sh -c SCRIPT sh PATH` runs SCRIPT with the path as $1.
    SyncCommand::new("sh", ["-c", script, "sh"])
  }

  #[test]
  fn the_default_command_syncs_a_file_and_a_directory() {
    let path = temp_file("default");
    let command = SyncCommand::default();
    command.sync(SyncTarget::File(&path), &|| false).unwrap();
    command
      .sync(SyncTarget::Directory(path.parent().unwrap()), &|| false)
      .unwrap();
    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn the_child_gets_the_path_and_its_failure_is_an_error() {
    let path = temp_file("failure");
    let seen = path.with_extension("seen");
    let script = format!("echo \"$1\" > {}; exit 3", seen.display());
    let error = shell(&script)
      .sync(SyncTarget::Directory(&path), &|| false)
      .unwrap_err();
    assert!(error.to_string().contains("exit status: 3"), "{error}");
    let named = std::fs::read_to_string(&seen).unwrap();
    assert_eq!(named.trim(), path.to_str().unwrap());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(seen).unwrap();
  }

  #[test]
  fn a_missing_program_syncs_in_this_thread() {
    let path = temp_file("missing");
    SyncCommand::new("/nonexistent/shingle-sync", Vec::<OsString>::new())
      .sync(SyncTarget::File(&path), &|| false)
      .unwrap();
    std::fs::remove_file(path).unwrap();
  }

  // A sync that hangs, as fsync on a busy disk does, is given up as soon
  // as a newer write supersedes it; the child is not waited for.
  #[test]
  fn a_hanging_sync_is_given_up_once_superseded() {
    let path = temp_file("hanging");
    let release = path.with_extension("hold");
    std::fs::write(&release, b"").unwrap();
    let script =
      format!("while [ -e {} ]; do sleep 0.02; done", release.display());
    let superseded = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| {
      scope.spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        superseded.store(true, Ordering::Relaxed);
      });
      let error = shell(&script)
        .sync(SyncTarget::Directory(&path), &|| {
          superseded.load(Ordering::Relaxed)
        })
        .unwrap_err();
      assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    });
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    // The child ends once released.
    std::fs::remove_file(release).unwrap();
    std::fs::remove_file(path).unwrap();
  }
}
