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

pub struct ConfigWatcher {
    path: PathBuf,
    stamp: Option<FileStamp>,
    changed_at: Option<Instant>,
}

impl ConfigWatcher {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let stamp = file_stamp(&path);
        Self {
            path,
            stamp,
            changed_at: None,
        }
    }

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
