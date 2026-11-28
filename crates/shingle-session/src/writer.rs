//! Session writes off the compositor thread.
//!
//! Saving a session syncs the file and its directory to disk, which took
//! 8 to 17 s on a busy disk (docs/status.md#sessions). [`SessionWriter`]
//! hands snapshots to a thread of its own, so the compositor only clones
//! the model. A snapshot submitted while another is waiting replaces it,
//! so a burst of layout changes costs one write of the newest state, and
//! since one thread writes them in turn, an older snapshot is never
//! written after a newer one.
//!
//! [`SessionWriter::finish`] hands over the final snapshot at exit. It is
//! written after every earlier one but not synced, and a sync still in
//! progress is given up for it, so the exit waits for no disk: the saves
//! before it were synced, the final one is atomic and survives a crash
//! of Shingle, and the kernel writes it back within its writeback delay
//! (docs/design/session-exit.md).

use std::{
  collections::HashSet,
  io,
  mem::Discriminant,
  sync::{Arc, Condvar, Mutex, MutexGuard},
  thread::JoinHandle,
  time::{Duration, Instant},
};

use shingle_core::{SessionSnapshot, ShingleError};

use crate::{SessionError, SessionStore, SyncCommand};

/// How long [`SessionWriter::finish`] waits for the final write.
pub const FINISH_LIMIT: Duration = Duration::from_secs(2);

/// Writes one snapshot as durably as asked;
/// [`SessionStore::save_snapshot_with`] for real sessions.
pub type WriteFn<T> = Box<
  dyn FnMut(&T, Durability<'_>) -> Result<(), SessionError> + Send + 'static,
>;

/// How durable a write must be.
#[derive(Clone, Copy)]
pub enum Durability<'a> {
  /// Synced to disk before and after the rename. Once `superseded`
  /// returns true, the final snapshot is waiting, and the write should
  /// give up a sync still in progress with an error of kind
  /// [`io::ErrorKind::Interrupted`].
  Synced {
    /// Whether the final snapshot is waiting for this write.
    superseded: &'a dyn Fn() -> bool,
  },
  /// Written and renamed into place without syncing: the final write.
  Unsynced,
}

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
  /// `pending` is the final snapshot from [`SessionWriter::finish`].
  last: bool,
  /// The final write's result, once it is done.
  last_result: Option<Result<(), SessionError>>,
  /// The writer thread has ended, normally or by a panic.
  done: bool,
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
/// never out of order. A snapshot that arrives while another waits
/// replaces it, so a burst of changes costs one write of the newest state.
///
/// Dropping the writer without [`SessionWriter::finish`] still writes a
/// snapshot that is waiting, synced, and waits for it.
pub struct SessionWriter<T: Send + 'static = SessionSnapshot> {
  shared: Arc<Shared<T>>,
  thread: Option<JoinHandle<Worker<T>>>,
}

impl SessionWriter<SessionSnapshot> {
  /// A writer saving to `store` atomically, synced by a child `sync`
  /// ([`SessionStore::save_snapshot_with`]), which prints failed saves to
  /// stderr once per kind of error.
  ///
  /// # Errors
  ///
  /// If the thread cannot be started.
  pub fn for_store(store: SessionStore) -> io::Result<Self> {
    Self::for_store_with(store, SyncCommand::default())
  }

  /// [`SessionWriter::for_store`] with `command` syncing each write.
  ///
  /// # Errors
  ///
  /// If the thread cannot be started.
  pub fn for_store_with(
    store: SessionStore,
    command: SyncCommand,
  ) -> io::Result<Self> {
    store.remove_given_up_writes();
    Self::new(
      Box::new(move |snapshot, durability| {
        store.save_snapshot_with(snapshot, durability, &command)
      }),
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
        last: false,
        last_result: None,
        done: false,
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
          let _done = Done(Arc::clone(&shared));
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

  /// Writes `snapshot` as the last one, unsynced, and stops the writer,
  /// waiting at most [`FINISH_LIMIT`]; see [`SessionWriter::finish_within`].
  ///
  /// # Errors
  ///
  /// As [`SessionWriter::finish_within`].
  pub fn finish(self, snapshot: T) -> Result<(), SessionError> {
    self.finish_within(snapshot, FINISH_LIMIT)
  }

  /// Writes `snapshot` as the last one and stops the writer, waiting at
  /// most `limit`.
  ///
  /// A snapshot still waiting is dropped, since `snapshot` is newer. A
  /// write in progress is told it is superseded and gives up its sync,
  /// leaving the file as the last completed write left it. Then the
  /// writer thread writes `snapshot` without syncing ([`Durability`]), so
  /// it lands after every earlier one without waiting for the disk.
  ///
  /// # Errors
  ///
  /// The final write's error, which is returned rather than reported, or
  /// one of kind [`io::ErrorKind::TimedOut`] when the writer has not
  /// finished within `limit`; the thread is then left to end with the
  /// process.
  pub fn finish_within(
    mut self,
    snapshot: T,
    limit: Duration,
  ) -> Result<(), SessionError> {
    let deadline = Instant::now() + limit;
    let mut queue = self.shared.lock();
    queue.pending = Some(snapshot);
    queue.last = true;
    queue.closed = true;
    self.shared.changed.notify_all();
    while !queue.done {
      let Some(left) = deadline.checked_duration_since(Instant::now()) else {
        drop(queue);
        // Detached: joining would wait for the disk after all.
        drop(self.thread.take());
        return Err(SessionError::Io(io::Error::new(
          io::ErrorKind::TimedOut,
          format!("the final session write took over {limit:?}"),
        )));
      };
      queue = self
        .shared
        .changed
        .wait_timeout(queue, left)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .0;
    }
    let result = queue.last_result.take();
    drop(queue);
    let panicked = self
      .thread
      .take()
      .is_some_and(|thread| thread.join().is_err());
    match result {
      Some(result) if !panicked => result,
      _ => Err(SessionError::Io(io::Error::other(
        "the session writer thread panicked",
      ))),
    }
  }
}

/// Marks the writer thread done when it ends, also by a panic.
struct Done<T>(Arc<Shared<T>>);

impl<T> Drop for Done<T> {
  fn drop(&mut self) {
    self.0.lock().done = true;
    self.0.changed.notify_all();
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
      let (snapshot, last) = loop {
        if let Some(snapshot) = queue.pending.take() {
          break (snapshot, queue.last);
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
      if last {
        let result = self.write_last(snapshot);
        let mut queue = shared.lock();
        queue.last_result = Some(result);
        queue.writing = false;
        drop(queue);
        shared.changed.notify_all();
        return;
      }
      let superseded = || shared.lock().last;
      self.write_one(snapshot, &superseded);
      shared.lock().writing = false;
      shared.changed.notify_all();
    }
  }

  fn write_one(&mut self, snapshot: T, superseded: &dyn Fn() -> bool) {
    if self.last_written.as_ref() == Some(&snapshot) {
      return;
    }
    match (self.write)(&snapshot, Durability::Synced { superseded }) {
      Ok(()) => {
        self.reported.clear();
        self.last_written = Some(snapshot);
      }
      Err(error) => {
        // The file may no longer hold the last snapshot written.
        self.last_written = None;
        // A write given up for the final one is not a failure.
        if !(superseded() && interrupted(&error))
          && self.reported.insert(ErrorKind::of(&error))
        {
          (self.report)(&error);
        }
      }
    }
  }

  /// Writes the final snapshot even if it was written before, since the
  /// file may have changed since.
  fn write_last(&mut self, snapshot: T) -> Result<(), SessionError> {
    (self.write)(&snapshot, Durability::Unsynced)
  }
}

/// Whether `error` is a sync given up for a newer write.
fn interrupted(error: &SessionError) -> bool {
  matches!(
    error,
    SessionError::Io(error) | SessionError::Core(ShingleError::Io(error))
      if error.kind() == io::ErrorKind::Interrupted
  )
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
    /// Each snapshot written, and whether it was synced.
    written: Arc<Mutex<Vec<(u32, bool)>>>,
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
        Box::new(move |snapshot: &u32, durability: Durability<'_>| {
          started_tx.send(*snapshot).unwrap();
          let deadline = std::time::Instant::now() + Duration::from_secs(10);
          let result = loop {
            if let Ok(result) = finish_rx.recv_timeout(Duration::from_millis(5))
            {
              break result;
            }
            // A sync in progress gives up for the final write.
            if let Durability::Synced { superseded } = durability
              && superseded()
            {
              break Err(io::Error::from(io::ErrorKind::Interrupted).into());
            }
            assert!(
              std::time::Instant::now() < deadline,
              "the test finishes every write"
            );
          };
          if result.is_ok() {
            let synced = matches!(durability, Durability::Synced { .. });
            log.lock().unwrap().push((*snapshot, synced));
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

    /// The snapshots written.
    fn written(&self) -> Vec<u32> {
      self
        .synced()
        .into_iter()
        .map(|(snapshot, _)| snapshot)
        .collect()
    }

    /// The snapshots written, and whether each was synced.
    fn synced(&self) -> Vec<(u32, bool)> {
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
      Box::new(move |snapshot: &u32, _: Durability<'_>| {
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

  // The exit waits for no sync: the write in progress gives up its sync
  // without a report, one waiting is dropped, and the final snapshot is
  // written after them, unsynced.
  #[test]
  fn finish_gives_up_the_sync_in_progress_and_writes_last_unsynced() {
    let (slow, writer) = SlowWriter::start();
    writer.submit(1);
    assert_eq!(slow.next_started(), 1);
    slow.succeed();
    writer.flush();
    writer.submit(2);
    assert_eq!(slow.next_started(), 2);
    // Waiting behind 2, and replaced by the final snapshot.
    writer.submit(3);
    let finished = std::thread::spawn(move || writer.finish(4));
    assert_eq!(slow.next_started(), 4);
    slow.succeed();
    finished.join().unwrap().unwrap();
    assert_eq!(slow.synced(), [(1, true), (4, false)]);
    assert_eq!(slow.reports(), 0);
  }

  #[test]
  fn finish_gives_up_on_a_final_write_that_hangs() {
    let (slow, writer) = SlowWriter::start();
    let started = std::time::Instant::now();
    let error = writer
      .finish_within(1, Duration::from_millis(100))
      .unwrap_err();
    assert!(
      matches!(&error, SessionError::Io(error)
        if error.kind() == io::ErrorKind::TimedOut),
      "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // The detached thread ends once its write does.
    assert_eq!(slow.next_started(), 1);
    slow.succeed();
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
    assert_eq!(slow.synced(), [(4, true), (4, false)]);
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

  // A sync that hangs, as on this host's busy disk, neither loses the
  // last completed save if Shingle crashes during it, nor holds up the
  // exit, whose final snapshot is on disk afterwards.
  #[test]
  fn a_hanging_sync_keeps_the_last_save_and_lets_the_exit_through() {
    let directory = std::env::temp_dir()
      .join(format!("shingle-session-hang-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state/session.json");
    let hold = directory.join("hold");
    let script =
      format!("while [ -e {} ]; do sleep 0.02; done", hold.display());
    let store = SessionStore::new(&path);
    let writer = SessionWriter::for_store_with(
      store.clone(),
      SyncCommand::new("sh", ["-c", &script, "sh"]),
    )
    .unwrap();
    let mut core = shingle_core::Shingle::new();
    let mut snapshot = |name: &str| {
      core
        .add_output(
          shingle_core::OutputId::new(name),
          shingle_core::Rect::new(0, 0, 640, 480),
        )
        .unwrap();
      core.snapshot()
    };
    let saved = snapshot("SAVED-1");
    writer.submit(saved.clone());
    writer.flush();
    // Every sync from now on hangs until `hold` is removed.
    std::fs::write(&hold, b"").unwrap();
    writer.submit(snapshot("HANGING-1"));
    let temporaries = || {
      std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_name() != "session.json")
        .count()
    };
    let started = std::time::Instant::now();
    while temporaries() == 0 {
      assert!(started.elapsed() < Duration::from_secs(10));
      std::thread::sleep(Duration::from_millis(5));
    }
    // A crash now finds the last completed save.
    assert_eq!(store.load().unwrap().unwrap().1, saved);
    let last = snapshot("LAST-1");
    let started = std::time::Instant::now();
    writer.finish(last.clone()).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(store.load().unwrap().unwrap().1, last);
    // The given-up write's file stays while its sync may still run, and
    // goes when the next writer starts.
    assert_eq!(temporaries(), 1);
    std::fs::remove_file(&hold).unwrap();
    drop(SessionWriter::for_store(store.clone()).unwrap());
    assert_eq!(temporaries(), 0);
    assert_eq!(store.load().unwrap().unwrap().1, last);
    std::fs::remove_dir_all(directory).unwrap();
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
