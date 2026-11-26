//! Session writes off the compositor thread.
//!
//! Saving a session syncs the file and its directory to disk, which took
//! 8 to 17 s on a busy disk (docs/status.md#sessions). [`SessionWriter`]
//! hands snapshots to a thread of its own, so the compositor only clones
//! the model. A snapshot submitted while another is waiting replaces it,
//! so a burst of layout changes costs one write of the newest state, and
//! since one thread writes them in turn, an older snapshot is never
//! written after a newer one. [`SessionWriter::finish`] writes the final
//! snapshot on the caller's thread once the writer thread has stopped,
//! so the exit stays durable.

use std::{
  collections::HashSet,
  io,
  mem::Discriminant,
  sync::{Arc, Condvar, Mutex, MutexGuard},
  thread::JoinHandle,
};

use shingle_core::{SessionSnapshot, ShingleError};

use crate::{SessionError, SessionStore};

/// Writes one snapshot; [`SessionStore::save_snapshot`] for real sessions.
pub type WriteFn<T> =
  Box<dyn FnMut(&T) -> Result<(), SessionError> + Send + 'static>;

/// Reports a failed background write. Called once per kind of error
/// until a write succeeds again; see [`SessionWriter::new`].
pub type ReportFn = Box<dyn FnMut(&SessionError) + Send + 'static>;

/// What the writer thread and the submitting thread share.
struct Queue<T> {
  /// The newest snapshot not yet taken by the writer thread.
  pending: Option<T>,
  /// The writer thread is writing a snapshot now.
  writing: bool,
  /// No more snapshots will be submitted; the thread exits once nothing
  /// is pending.
  closed: bool,
}

struct Shared<T> {
  queue: Mutex<Queue<T>>,
  changed: Condvar,
}

impl<T> Shared<T> {
  fn lock(&self) -> MutexGuard<'_, Queue<T>> {
    // A panic in the write function cannot leave the queue inconsistent,
    // since it is never held across a write.
    self
      .queue
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }
}

/// Writes session snapshots on a background thread, newest first and
/// never out of order; see the [module documentation](self).
///
/// Dropping the writer without [`SessionWriter::finish`] still writes a
/// snapshot that is waiting, and waits for it.
pub struct SessionWriter<T: Send + 'static = SessionSnapshot> {
  shared: Arc<Shared<T>>,
  thread: Option<JoinHandle<Worker<T>>>,
}

impl SessionWriter<SessionSnapshot> {
  /// A writer saving to `store` atomically, as
  /// [`SessionStore::save_snapshot`] does, which prints failed saves to
  /// stderr once per kind of error.
  ///
  /// # Errors
  ///
  /// If the thread cannot be started.
  pub fn for_store(store: SessionStore) -> io::Result<Self> {
    Self::new(
      Box::new(move |snapshot| store.save_snapshot(snapshot)),
      Box::new(|error| {
        eprintln!(
          "shingle: failed to save session: {}",
          crate::error_chain(error)
        )
      }),
    )
  }
}

impl<T: PartialEq + Send + 'static> SessionWriter<T> {
  /// Starts the writer thread with `write` for each snapshot and `report`
  /// for failures.
  ///
  /// A snapshot equal to the last one written successfully is not written
  /// again. `report` is called for the first failure of each kind (the
  /// [`io::ErrorKind`] of an I/O error, otherwise the error variant); the
  /// same kind is not reported again until a write has succeeded.
  ///
  /// # Errors
  ///
  /// If the thread cannot be started.
  pub fn new(write: WriteFn<T>, report: ReportFn) -> io::Result<Self> {
    let shared = Arc::new(Shared {
      queue: Mutex::new(Queue {
        pending: None,
        writing: false,
        closed: false,
      }),
      changed: Condvar::new(),
    });
    let mut worker = Worker {
      write,
      report,
      reported: HashSet::new(),
      last_written: None,
    };
    let thread = std::thread::Builder::new()
      .name("session-writer".into())
      .spawn({
        let shared = Arc::clone(&shared);
        move || {
          worker.run(&shared);
          worker
        }
      })?;
    Ok(Self {
      shared,
      thread: Some(thread),
    })
  }

  /// Queues `snapshot` to be written and returns at once. A snapshot
  /// still waiting is replaced, since this one is newer.
  pub fn submit(&self, snapshot: T) {
    let mut queue = self.shared.lock();
    queue.pending = Some(snapshot);
    drop(queue);
    self.shared.changed.notify_all();
  }

  /// Waits until every submitted snapshot has been written or replaced.
  pub fn flush(&self) {
    let mut queue = self.shared.lock();
    while queue.pending.is_some() || queue.writing {
      queue = self
        .shared
        .changed
        .wait(queue)
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
  }

  /// Writes `snapshot` as the last one and stops the writer.
  ///
  /// A snapshot still waiting is dropped, since `snapshot` is newer; a
  /// write in progress is waited for. Then `snapshot` is written on the
  /// calling thread, so it lands after every earlier one and is on disk
  /// when this returns.
  ///
  /// # Errors
  ///
  /// The final write's error, which is returned rather than reported.
  pub fn finish(mut self, snapshot: T) -> Result<(), SessionError> {
    self.shared.lock().pending = None;
    match self.stop() {
      Some(mut worker) => (worker.write)(&snapshot),
      None => Err(SessionError::Io(io::Error::other(
        "the session writer thread panicked",
      ))),
    }
  }

  /// Closes the queue and joins the thread, which first writes what is
  /// still pending. Returns the thread's state, or `None` if it panicked.
  fn stop(&mut self) -> Option<Worker<T>> {
    let thread = self.thread.take()?;
    self.shared.lock().closed = true;
    self.shared.changed.notify_all();
    thread.join().ok()
  }
}

impl<T: Send + 'static> Drop for SessionWriter<T> {
  fn drop(&mut self) {
    if let Some(thread) = self.thread.take() {
      self.shared.lock().closed = true;
      self.shared.changed.notify_all();
      let _ = thread.join();
    }
  }
}

/// The writer thread's state.
struct Worker<T> {
  write: WriteFn<T>,
  report: ReportFn,
  /// Kinds of error reported since the last successful write.
  reported: HashSet<ErrorKind>,
  /// The last snapshot written successfully.
  last_written: Option<T>,
}

impl<T: PartialEq> Worker<T> {
  fn run(&mut self, shared: &Shared<T>) {
    loop {
      let mut queue = shared.lock();
      let snapshot = loop {
        if let Some(snapshot) = queue.pending.take() {
          break snapshot;
        }
        if queue.closed {
          return;
        }
        queue = shared
          .changed
          .wait(queue)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
      };
      queue.writing = true;
      drop(queue);
      self.write_one(snapshot);
      shared.lock().writing = false;
      shared.changed.notify_all();
    }
  }

  fn write_one(&mut self, snapshot: T) {
    if self.last_written.as_ref() == Some(&snapshot) {
      return;
    }
    match (self.write)(&snapshot) {
      Ok(()) => {
        self.reported.clear();
        self.last_written = Some(snapshot);
      }
      Err(error) => {
        // The file may no longer hold the last snapshot written.
        self.last_written = None;
        if self.reported.insert(ErrorKind::of(&error)) {
          (self.report)(&error);
        }
      }
    }
  }
}

/// What counts as the same error for reporting it once.
#[derive(Debug, Eq, Hash, PartialEq)]
enum ErrorKind {
  Io(io::ErrorKind),
  Core(Discriminant<ShingleError>),
  Session(Discriminant<SessionError>),
}

impl ErrorKind {
  fn of(error: &SessionError) -> Self {
    match error {
      SessionError::Io(error) | SessionError::Core(ShingleError::Io(error)) => {
        Self::Io(error.kind())
      }
      SessionError::Core(error) => Self::Core(std::mem::discriminant(error)),
      error => Self::Session(std::mem::discriminant(error)),
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
  };

  use super::*;

  /// A write function that tells the test which snapshot it started and
  /// then waits until the test lets it finish with a result.
  struct SlowWriter {
    started: Receiver<u32>,
    finish: Sender<Result<(), SessionError>>,
    written: Arc<Mutex<Vec<u32>>>,
    reports: Arc<Mutex<Vec<String>>>,
  }

  impl SlowWriter {
    fn start() -> (Self, SessionWriter<u32>) {
      let (started_tx, started) = mpsc::channel();
      let (finish, finish_rx) = mpsc::channel::<Result<(), SessionError>>();
      let written = Arc::new(Mutex::new(Vec::new()));
      let reports = Arc::new(Mutex::new(Vec::new()));
      let log = Arc::clone(&written);
      let report_log = Arc::clone(&reports);
      let writer = SessionWriter::new(
        Box::new(move |snapshot: &u32| {
          started_tx.send(*snapshot).unwrap();
          let result = finish_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the test finishes every write");
          if result.is_ok() {
            log.lock().unwrap().push(*snapshot);
          }
          result
        }),
        Box::new(move |error| {
          report_log.lock().unwrap().push(error.to_string())
        }),
      )
      .unwrap();
      (
        Self {
          started,
          finish,
          written,
          reports,
        },
        writer,
      )
    }

    /// The next snapshot the writer started.
    fn next_started(&self) -> u32 {
      self.started.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    fn succeed(&self) {
      self.finish.send(Ok(())).unwrap();
    }

    fn fail(&self, kind: io::ErrorKind) {
      self.finish.send(Err(io::Error::from(kind).into())).unwrap();
    }

    fn written(&self) -> Vec<u32> {
      self.written.lock().unwrap().clone()
    }

    fn reports(&self) -> usize {
      self.reports.lock().unwrap().len()
    }

    /// No write starts within a short wait.
    fn nothing_started(&self) -> bool {
      self
        .started
        .recv_timeout(Duration::from_millis(50))
        .is_err()
    }
  }

  #[test]
  fn submit_returns_while_a_write_is_in_progress() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    // The write is held, yet submitting does not wait for it.
    let started = std::time::Instant::now();
    writer.submit(2);
    assert!(started.elapsed() < Duration::from_millis(50));
    slow.succeed();
    assert_eq!(slow.next_started(), 2);
    slow.succeed();
    writer.flush();
    assert_eq!(slow.written(), [1, 2]);
  }

  #[test]
  fn a_burst_during_a_write_is_written_once_as_the_newest() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    for snapshot in 2..=50 {
      writer.submit(snapshot);
    }
    slow.succeed();
    assert_eq!(slow.next_started(), 50);
    slow.succeed();
    writer.flush();
    assert!(slow.nothing_started());
    assert_eq!(slow.written(), [1, 50]);
  }

  #[test]
  fn writes_follow_the_order_of_submission() {
    let written = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&written);
    let writer = SessionWriter::new(
      Box::new(move |snapshot: &u32| {
        std::thread::sleep(Duration::from_micros(300));
        log.lock().unwrap().push(*snapshot);
        Ok(())
      }),
      Box::new(|_| {}),
    )
    .unwrap();
    for snapshot in 1..=300 {
      writer.submit(snapshot);
      if snapshot % 7 == 0 {
        std::thread::sleep(Duration::from_micros(200));
      }
    }
    writer.flush();
    let written = written.lock().unwrap().clone();
    assert!(
      written.windows(2).all(|pair| pair[0] < pair[1]),
      "{written:?}"
    );
    assert_eq!(written.last(), Some(&300));
    assert!(written.len() < 300, "bursts were coalesced: {written:?}");
  }

  #[test]
  fn an_unchanged_snapshot_is_not_written_again() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(7);
    assert_eq!(slow.next_started(), 7);
    slow.succeed();
    writer.flush();
    writer.submit(7);
    writer.flush();
    assert!(slow.nothing_started());
    assert_eq!(slow.written(), [7]);
  }

  #[test]
  fn finish_waits_for_the_write_in_progress_and_writes_last() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    // Waiting behind 1, and replaced by the final snapshot.
    writer.submit(2);
    let shared = Arc::clone(&writer.shared);
    let finished = std::thread::spawn(move || writer.finish(3));
    // finish has dropped 2 by the time it closes the queue.
    while !shared.lock().closed {
      std::thread::yield_now();
    }
    assert_eq!(shared.lock().pending, None);
    slow.succeed();
    assert_eq!(slow.next_started(), 3);
    slow.succeed();
    finished.join().unwrap().unwrap();
    assert_eq!(slow.written(), [1, 3]);
  }

  #[test]
  fn finish_writes_even_an_unchanged_snapshot() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(4);
    assert_eq!(slow.next_started(), 4);
    slow.succeed();
    writer.flush();
    let finished = std::thread::spawn(move || writer.finish(4));
    assert_eq!(slow.next_started(), 4);
    slow.succeed();
    finished.join().unwrap().unwrap();
    assert_eq!(slow.written(), [4, 4]);
  }

  #[test]
  fn finish_returns_the_final_error_without_reporting_it() {
    let (slow, writer) = SlowWriter::start();
    let finished = std::thread::spawn(move || writer.finish(1));
    assert_eq!(slow.next_started(), 1);
    slow.fail(io::ErrorKind::StorageFull);
    let error = finished.join().unwrap().unwrap_err();
    assert!(
      matches!(&error, SessionError::Io(error)
        if error.kind() == io::ErrorKind::StorageFull),
      "{error:?}"
    );
    assert_eq!(slow.reports(), 0);
  }

  #[test]
  fn drop_writes_the_waiting_snapshot() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    writer.submit(2);
    let dropped = std::thread::spawn(move || drop(writer));
    slow.succeed();
    assert_eq!(slow.next_started(), 2);
    slow.succeed();
    dropped.join().unwrap();
    assert_eq!(slow.written(), [1, 2]);
  }

  #[test]
  fn errors_are_reported_once_per_kind_until_a_write_succeeds() {
    let (slow, writer) = SlowWriter::start();
    let fail = |snapshot, kind| {
      writer.submit(snapshot);
      assert_eq!(slow.next_started(), snapshot);
      match kind {
        Some(kind) => slow.fail(kind),
        None => slow.succeed(),
      }
      writer.flush();
      slow.reports()
    };
    assert_eq!(fail(1, Some(io::ErrorKind::StorageFull)), 1);
    assert_eq!(fail(2, Some(io::ErrorKind::StorageFull)), 1);
    assert_eq!(fail(3, Some(io::ErrorKind::StorageFull)), 1);
    assert_eq!(fail(4, Some(io::ErrorKind::PermissionDenied)), 2);
    assert_eq!(fail(5, Some(io::ErrorKind::StorageFull)), 2);
    assert_eq!(fail(6, None), 2);
    assert_eq!(fail(7, Some(io::ErrorKind::StorageFull)), 3);
  }

  #[test]
  fn a_failed_snapshot_is_written_again_when_resubmitted() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    slow.succeed();
    writer.submit(2);
    assert_eq!(slow.next_started(), 2);
    slow.fail(io::ErrorKind::StorageFull);
    // Back to 1, which the file may no longer hold.
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    slow.succeed();
    writer.flush();
    assert_eq!(slow.written(), [1, 1]);
  }

  #[test]
  fn error_kinds_tell_io_kinds_and_variants_apart() {
    let io = |kind| SessionError::Io(io::Error::from(kind));
    let core_io =
      SessionError::Core(ShingleError::Io(io::Error::other("disk")));
    assert_eq!(
      ErrorKind::of(&io(io::ErrorKind::Other)),
      ErrorKind::of(&core_io)
    );
    assert_ne!(
      ErrorKind::of(&io(io::ErrorKind::Other)),
      ErrorKind::of(&io(io::ErrorKind::StorageFull))
    );
    assert_eq!(
      ErrorKind::of(&SessionError::Core(ShingleError::MalformedSnapshot(
        "a".into()
      ))),
      ErrorKind::of(&SessionError::Core(ShingleError::MalformedSnapshot(
        "b".into()
      )))
    );
    assert_ne!(
      ErrorKind::of(&SessionError::Core(ShingleError::InvalidPersistencePath)),
      ErrorKind::of(&SessionError::StateDirectoryUnavailable)
    );
  }

  #[test]
  fn the_store_writer_saves_atomically_to_its_file() {
    let directory = std::env::temp_dir()
      .join(format!("shingle-session-writer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let path = directory.join("nested/session.json");
    let writer = SessionWriter::for_store(SessionStore::new(&path)).unwrap();
    let snapshot = shingle_core::Shingle::new().snapshot();
    writer.submit(snapshot.clone());
    writer.flush();
    assert!(path.exists());
    std::fs::remove_file(&path).unwrap();
    writer.finish(snapshot.clone()).unwrap();
    let (_, loaded) = SessionStore::new(&path).load().unwrap().unwrap();
    assert_eq!(loaded, snapshot);
    // Nothing but the session file is left: no temporary files.
    assert_eq!(
      std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
      1
    );
    std::fs::remove_dir_all(directory).unwrap();
  }
}
