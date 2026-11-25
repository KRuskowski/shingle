use std::{
  fs,
  os::unix::fs::MetadataExt,
  path::{Path, PathBuf},
  time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
  modified: SystemTime,
  length: u64,
  device: u64,
  inode: u64,
  changed_seconds: i64,
  changed_nanoseconds: i64,
}

/// Polls one config file for changes and reports when it has been stable long
/// enough to reload, implementing Wayfire's `auto_reload_config` and
/// `config_reload_delay`.
///
/// A change is any difference in modification time, length, device, inode or
/// ctime, so an editor that saves by renaming a new file over the old one is
/// noticed too, and so is the file appearing or disappearing. There is no
/// inotify: the host calls [`ConfigWatcher::poll`] from its loop.
pub struct ConfigWatcher {
  path: PathBuf,
  stamp: Option<FileStamp>,
  changed_at: Option<Instant>,
}

impl ConfigWatcher {
  /// Watches `path`, taking its current state as already loaded.
  pub fn new(path: impl Into<PathBuf>) -> Self {
    let path = path.into();
    let stamp = file_stamp(&path);
    Self {
      path,
      stamp,
      changed_at: None,
    }
  }

  /// Records any change since the last poll and returns true once, when the
  /// file has gone `delay` without a further change. Each new change restarts
  /// the delay, so a burst of writes yields one reload. `now` is passed in so
  /// tests can control time.
  pub fn poll(&mut self, now: Instant, delay: Duration) -> bool {
    let stamp = file_stamp(&self.path);
    if stamp != self.stamp {
      self.stamp = stamp;
      self.changed_at = Some(now);
    }
    if self
      .changed_at
      .is_some_and(|changed| now.duration_since(changed) >= delay)
    {
      self.changed_at = None;
      return true;
    }
    false
  }

  /// Takes the file's current state as loaded and drops any pending reload, for
  /// example after the host reloaded or saved the file itself.
  pub fn mark_loaded(&mut self) {
    self.stamp = file_stamp(&self.path);
    self.changed_at = None;
  }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
  let metadata = fs::metadata(path).ok()?;
  Some(FileStamp {
    modified: metadata.modified().ok()?,
    length: metadata.len(),
    device: metadata.dev(),
    inode: metadata.ino(),
    changed_seconds: metadata.ctime(),
    changed_nanoseconds: metadata.ctime_nsec(),
  })
}
