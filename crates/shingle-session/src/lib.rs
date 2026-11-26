//! Session persistence and relaunching saved applications.
//!
//! [`SessionStore`] reads and atomically writes the `shingle_core::Shingle`
//! model as JSON. On restore, [`launch_saved`] starts one process per saved
//! launch group with `SHINGLE_LAUNCH_ID` in its environment. When that
//! client connects, the compositor reads the variable from
//! `/proc/<pid>/environ` (crates/shingle/src/launch.rs:12) and the core puts
//! the new window back into the saved tile, so tile order and sizes survive
//! logout and login. [`launch_autostart`] then runs the configured autostart
//! commands, skipping those the session already relaunched.
//!
//! Every process Shingle starts, here or elsewhere, gets its environment
//! from one [`ClientEnvironment`].
//!
//! Pre-rename Tilefire sessions and `TILEFIRE_LAUNCH_ID` are still read;
//! see README.md#upgrading-from-tilefire.

#![warn(missing_docs)]

mod environment;

use std::{
  collections::{BTreeMap, BTreeSet},
  ffi::{OsStr, OsString},
  fs, io,
  path::{Path, PathBuf},
  process::{Child, Command, Stdio},
};

pub use environment::ClientEnvironment;
pub use shingle_core::SessionSnapshot;

use shingle_core::{
  LaunchIdentity, NodeSnapshot, Shingle, ShingleError, WindowId,
};
use thiserror::Error;

/// Errors from loading, saving or relaunching a session.
#[derive(Debug, Error)]
pub enum SessionError {
  /// The session file is not valid JSON, fails the snapshot's validation,
  /// or could not be written atomically.
  #[error(transparent)]
  Core(#[from] ShingleError),
  /// Reading the session file, other than it not existing, or creating its
  /// directory failed.
  #[error("failed to access session state")]
  Io(#[from] io::Error),
  /// Neither an absolute `XDG_STATE_HOME` nor a non-empty `HOME` is set.
  #[error("session state location is unavailable")]
  StateDirectoryUnavailable,
  /// A saved window's command could not be spawned, or it has no saved
  /// command (reported as `ErrorKind::NotFound`). Other windows are still
  /// launched.
  #[error("failed to launch window {window:?}: {source}")]
  Launch {
    /// The saved window that was not relaunched.
    window: WindowId,
    /// Why spawning failed.
    source: io::Error,
  },
  /// `/bin/sh` could not be spawned for an autostart command.
  #[error("failed to launch autostart command {command:?}: {source}")]
  Autostart {
    /// The command line as configured.
    command: String,
    /// Why spawning failed.
    source: io::Error,
  },
}

/// Picks the session file under `state`.
///
/// A session saved at the pre-rename Tilefire location is restored when no
/// Shingle session exists yet. Saving always targets the returned path, so a
/// legacy session keeps being used until it is moved by hand.
fn resolve_session_path(
  state: &Path,
  exists: impl Fn(&Path) -> bool,
) -> PathBuf {
  let current = state.join("shingle/session.json");
  let legacy = state.join("tilefire/session.json");
  if !exists(&current) && exists(&legacy) {
    legacy
  } else {
    current
  }
}

/// The XDG state directory from the values of `XDG_STATE_HOME` and `HOME`.
/// The base directory spec says an empty `XDG_STATE_HOME` means
/// `$HOME/.local/state` and a relative one is invalid and ignored; an
/// empty `HOME` counts as unset.
fn state_directory(
  xdg_state_home: Option<OsString>,
  home: Option<OsString>,
) -> Option<PathBuf> {
  xdg_state_home
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
    .or_else(|| {
      home
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state"))
    })
}

/// The location of one session file.
#[derive(Clone, Debug)]
pub struct SessionStore {
  path: PathBuf,
}

impl SessionStore {
  /// Uses `path` as given, as for `session.path` in the config. Nothing is
  /// read or created until [`Self::load`] or [`Self::save`].
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self { path: path.into() }
  }

  /// `$XDG_STATE_HOME/shingle/session.json`, falling back to
  /// `$HOME/.local/state` for the state directory. If that file does not
  /// exist but `tilefire/session.json` does, the Tilefire file is used for
  /// both loading and saving until it is moved by hand.
  ///
  /// An empty or relative `XDG_STATE_HOME` is ignored, as the XDG base
  /// directory spec asks.
  ///
  /// # Errors
  ///
  /// [`SessionError::StateDirectoryUnavailable`] if neither variable gives a
  /// directory.
  pub fn default_location() -> Result<Self, SessionError> {
    let state = state_directory(
      std::env::var_os("XDG_STATE_HOME"),
      std::env::var_os("HOME"),
    )
    .ok_or(SessionError::StateDirectoryUnavailable)?;
    Ok(Self::new(resolve_session_path(&state, |path| {
      path.exists()
    })))
  }

  /// The session file this store reads and writes.
  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Writes `session` atomically, creating the parent directories first. A
  /// failed save leaves the previous file intact.
  ///
  /// # Errors
  ///
  /// [`SessionError::Io`] if the directory cannot be created and
  /// [`SessionError::Core`] if the model fails validation or the write
  /// fails.
  pub fn save(&self, session: &Shingle) -> Result<(), SessionError> {
    self.save_snapshot(&session.snapshot())
  }

  /// Writes a snapshot taken earlier with `Shingle::snapshot`, as
  /// [`SessionStore::save`] does. Shutdown takes the snapshot first and
  /// writes it after the compositor has released its displays, because
  /// the write syncs to disk and can stall for many seconds on a busy one.
  ///
  /// # Errors
  ///
  /// As [`SessionStore::save`].
  pub fn save_snapshot(
    &self,
    snapshot: &SessionSnapshot,
  ) -> Result<(), SessionError> {
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent)?;
    }
    snapshot.save_atomic(&self.path)?;
    Ok(())
  }

  /// Reads and validates the saved session. Returns `Ok(None)` when the file
  /// does not exist, so a first start is not an error.
  ///
  /// Every saved window without a launch ID is given
  /// `shingle-restore-<window id>` in both the returned model and snapshot,
  /// so pass this snapshot, not a fresh one, to [`launch_saved`] for the
  /// relaunched clients to find their tiles again.
  ///
  /// A corrupt file (not UTF-8, not JSON, or not a valid snapshot) is
  /// renamed aside to `NAME.corrupt-SECONDS` next to it, with `.N` added
  /// if that name is taken, a warning naming both paths is printed to
  /// stderr, and `Ok(None)` is returned, so the compositor starts with an
  /// empty session and the next save writes a fresh file. The corrupt file
  /// is never deleted or overwritten.
  ///
  /// # Errors
  ///
  /// [`SessionError::Io`] if the file exists but cannot be read, or is
  /// corrupt and cannot be renamed aside.
  pub fn load(
    &self,
  ) -> Result<Option<(Shingle, SessionSnapshot)>, SessionError> {
    let json = match fs::read_to_string(&self.path) {
      Ok(json) => json,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return Ok(None);
      }
      Err(error) if error.kind() == io::ErrorKind::InvalidData => {
        return self.set_aside(&error.into());
      }
      Err(error) => return Err(error.into()),
    };
    let parsed = SessionSnapshot::from_json(&json).and_then(|mut snapshot| {
      assign_restore_ids(&mut snapshot);
      let session = Shingle::from_snapshot(snapshot.clone())?;
      Ok((session, snapshot))
    });
    match parsed {
      Ok(loaded) => Ok(Some(loaded)),
      Err(error) => self.set_aside(&error.into()),
    }
  }

  /// Renames the corrupt session file aside and reports it; see
  /// [`Self::load`].
  fn set_aside(
    &self,
    error: &SessionError,
  ) -> Result<Option<(Shingle, SessionSnapshot)>, SessionError> {
    let seconds = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_or(0, |elapsed| elapsed.as_secs());
    let aside = move_aside(&self.path, seconds)?;
    eprintln!(
      "shingle: session file {} is corrupt ({}); moved it to {} and \
       starting with an empty session",
      self.path.display(),
      error_chain(error),
      aside.display()
    );
    Ok(None)
  }
}

/// Moves `path` to `PATH.corrupt-SECONDS`, or `PATH.corrupt-SECONDS.N` for
/// the first free `N`, without replacing an existing file, and returns the
/// new path.
fn move_aside(path: &Path, seconds: u64) -> io::Result<PathBuf> {
  let mut base = path.as_os_str().to_owned();
  base.push(format!(".corrupt-{seconds}"));
  for attempt in 0..1000_u32 {
    let mut candidate = base.clone();
    if attempt > 0 {
      candidate.push(format!(".{attempt}"));
    }
    let candidate = PathBuf::from(candidate);
    // A hard link fails if the name exists, so nothing is overwritten.
    match fs::hard_link(path, &candidate) {
      Ok(()) => {
        fs::remove_file(path)?;
        return Ok(candidate);
      }
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
      // File systems without hard links: check first, then rename.
      Err(_) if !candidate.exists() => {
        fs::rename(path, &candidate)?;
        return Ok(candidate);
      }
      Err(_) => {}
    }
  }
  Err(io::Error::new(
    io::ErrorKind::AlreadyExists,
    "no free name to move the corrupt session file to",
  ))
}

/// `error` with its sources, for a one-line message. A source whose text the
/// message already ends with, as for errors that display their source, is
/// not repeated.
fn error_chain(error: &dyn std::error::Error) -> String {
  let mut text = error.to_string();
  let mut source = error.source();
  while let Some(cause) = source {
    let cause_text = cause.to_string();
    if !text.ends_with(&cause_text) {
      text.push_str(": ");
      text.push_str(&cause_text);
    }
    source = cause.source();
  }
  text
}

/// The result of [`launch_saved`].
pub struct LaunchReport {
  /// Processes that were spawned, one per launch group, in tile order.
  pub launched: Vec<LaunchedWindow>,
  /// [`SessionError::Launch`] for each launch group that failed to spawn.
  pub failed: Vec<SessionError>,
}

/// Runs `command` through `/bin/sh -c` in `environment`, for autostart,
/// `command` bindings, IPC binding commands and IPC `run`.
///
/// The child inherits stdin and, unless `discard_output` is set, stdout and
/// stderr. It is not waited for; the caller owns the returned [`Child`] and
/// must reap it.
///
/// # Errors
///
/// Only when `/bin/sh` cannot be spawned. A command that fails inside the
/// shell still returns `Ok`.
pub fn launch_command(
  command: &str,
  environment: &ClientEnvironment,
  discard_output: bool,
) -> io::Result<Child> {
  let mut process = Command::new("/bin/sh");
  process.arg("-c").arg(command);
  environment.apply(&mut process);
  if discard_output {
    process.stdout(Stdio::null()).stderr(Stdio::null());
  }
  process.spawn()
}

/// Runs `program` with `arguments`, without a shell, in `environment`: the
/// `--spawn` client and the lock client.
///
/// The child inherits stdin, stdout and stderr. It is not waited for; the
/// caller owns the returned [`Child`] and must reap it.
///
/// # Errors
///
/// When `program` cannot be spawned.
pub fn launch_program(
  program: &OsStr,
  arguments: &[OsString],
  environment: &ClientEnvironment,
) -> io::Result<Child> {
  let mut process = Command::new(program);
  process.args(arguments);
  environment.apply(&mut process);
  process.spawn()
}

/// The result of [`launch_autostart`].
pub struct AutostartReport {
  /// The shells started, in command order.
  pub children: Vec<Child>,
  /// [`SessionError::Autostart`] for each command that could not start.
  pub failed: Vec<SessionError>,
}

/// Runs each autostart command with [`launch_command`], skipping blank ones
/// and, when a session was `restored`, those that relaunched a saved window.
///
/// A command counts as relaunched when a saved window's command is either
/// `sh -c <command>` (or `/bin/sh -c`) or, with no whitespace inside any
/// argument, the arguments joined by single spaces, compared after trimming.
/// So `foot --server` matches a saved `["foot", "--server"]`, but quoting or
/// a different path to the program does not match and the command runs a
/// second time.
pub fn launch_autostart(
  commands: &[String],
  restored: Option<&SessionSnapshot>,
  environment: &ClientEnvironment,
  discard_output: bool,
) -> AutostartReport {
  let mut report = AutostartReport {
    children: Vec::new(),
    failed: Vec::new(),
  };
  for command in commands {
    if command.trim().is_empty()
      || restored.is_some_and(|snapshot| was_restored(command, snapshot))
    {
      continue;
    }
    match launch_command(command, environment, discard_output) {
      Ok(child) => report.children.push(child),
      Err(source) => report.failed.push(SessionError::Autostart {
        command: command.clone(),
        source,
      }),
    }
  }
  report
}

fn was_restored(command: &str, snapshot: &SessionSnapshot) -> bool {
  snapshot.windows.iter().any(|window| {
    let saved = &window.identity.command;
    let shell = saved.len() == 3
      && matches!(saved[0].as_str(), "/bin/sh" | "sh")
      && saved[1] == "-c"
      && saved[2].trim() == command.trim();
    let direct = saved
      .iter()
      .all(|argument| !argument.chars().any(char::is_whitespace))
      && saved.join(" ") == command.trim();
    shell || direct
  })
}

/// Spawns the saved clients of `snapshot` in tile order: output by output,
/// workspace by workspace, depth first through each tree.
///
/// Windows that share a `launch_group` (by default the PID of the process
/// that created them) are launched once, for the first window in tile
/// order. Each child gets the saved arguments without a shell, the saved
/// working directory, a null stdin, `SHINGLE_LAUNCH_ID` (empty if the saved
/// window has none) and `environment`, the compositor's client
/// environment. Launch IDs are only filled in by [`SessionStore::load`].
///
/// Spawn failures are collected, not returned early, so one missing program
/// does not stop the rest of the session.
pub fn launch_saved(
  snapshot: &SessionSnapshot,
  environment: &ClientEnvironment,
  discard_output: bool,
) -> LaunchReport {
  let identities = snapshot
    .windows
    .iter()
    .map(|window| (window.id, window.identity.clone()))
    .collect::<BTreeMap<_, _>>();
  let mut launched = Vec::new();
  let mut failed = Vec::new();
  let mut launched_groups = BTreeSet::new();
  for id in tile_order(snapshot) {
    let Some(identity) = identities.get(&id) else {
      continue;
    };
    if identity
      .launch_group
      .as_ref()
      .is_some_and(|group| !launched_groups.insert(group.clone()))
    {
      continue;
    }
    match launch_window(id, identity, environment, discard_output) {
      Ok(child) => launched.push(LaunchedWindow { id, child }),
      Err(error) => failed.push(error),
    }
  }
  LaunchReport { launched, failed }
}

/// A process started for a saved window.
pub struct LaunchedWindow {
  /// The first saved window of the process's launch group. Other windows in
  /// the group are expected from the same process.
  pub id: WindowId,
  /// The spawned process, which the caller must reap.
  pub child: Child,
}

fn assign_restore_ids(snapshot: &mut SessionSnapshot) {
  for window in &mut snapshot.windows {
    if window.identity.launch_id.is_none() {
      window.identity.launch_id =
        Some(format!("shingle-restore-{}", window.id.0));
    }
  }
}

fn tile_order(snapshot: &SessionSnapshot) -> Vec<WindowId> {
  let mut order = Vec::new();
  for output in &snapshot.outputs {
    for workspace in &output.workspaces {
      for child in &workspace.root.children {
        collect_order(&child.node, &mut order);
      }
    }
  }
  order
}

fn collect_order(node: &NodeSnapshot, order: &mut Vec<WindowId>) {
  match node {
    NodeSnapshot::Split(split) => {
      for child in &split.children {
        collect_order(&child.node, order);
      }
    }
    NodeSnapshot::Leaf { window, .. } => order.push(*window),
  }
}

fn launch_window(
  id: WindowId,
  identity: &LaunchIdentity,
  environment: &ClientEnvironment,
  discard_output: bool,
) -> Result<Child, SessionError> {
  let Some((program, args)) = identity.command.split_first() else {
    return Err(SessionError::Launch {
      window: id,
      source: io::Error::new(
        io::ErrorKind::NotFound,
        "no saved launch command",
      ),
    });
  };
  let mut command = Command::new(program);
  command.args(args).stdin(Stdio::null()).env(
    "SHINGLE_LAUNCH_ID",
    identity.launch_id.as_deref().unwrap_or(""),
  );
  environment.apply(&mut command);
  if let Some(directory) = &identity.working_directory {
    command.current_dir(directory);
  }
  if discard_output {
    command.stdout(Stdio::null()).stderr(Stdio::null());
  }
  command
    .spawn()
    .map_err(|source| SessionError::Launch { window: id, source })
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicU32, Ordering};

  use shingle_core::{OutputId, Rect};

  use super::*;

  /// A fresh directory under the temporary directory.
  fn temp_dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
      "shingle-session-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
  }

  /// A shell command that writes the client variables to `path`.
  fn dump_to(path: &Path) -> String {
    format!(
      "printf '%s|%s|%s|%s' \"${{DISPLAY-unset}}\" \
       \"${{WAYFIRE_SOCKET-unset}}\" \"$WAYLAND_DISPLAY\" \
       \"${{SHINGLE_LAUNCH_ID-unset}}\" > {}",
      path.display()
    )
  }

  /// An environment as a nested Shingle serving IPC builds it.
  fn environment() -> ClientEnvironment {
    ClientEnvironment::new(
      OsStr::new("wayland-7"),
      None,
      Some(Path::new("/run/shingle-ipc")),
    )
  }

  fn wait_and_read(mut child: Child, path: &Path) -> String {
    assert!(child.wait().unwrap().success());
    fs::read_to_string(path).unwrap()
  }

  #[test]
  fn launch_command_runs_in_the_client_environment() {
    let directory = temp_dir();
    let out = directory.join("out");
    let child = launch_command(&dump_to(&out), &environment(), true).unwrap();
    assert_eq!(
      wait_and_read(child, &out),
      "unset|/run/shingle-ipc|wayland-7|unset"
    );
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn launch_program_runs_in_the_client_environment() {
    let directory = temp_dir();
    let out = directory.join("out");
    let arguments = ["-c".into(), dump_to(&out).into()];
    let child =
      launch_program(OsStr::new("/bin/sh"), &arguments, &environment())
        .unwrap();
    assert_eq!(
      wait_and_read(child, &out),
      "unset|/run/shingle-ipc|wayland-7|unset"
    );
    assert!(
      launch_program(
        OsStr::new("/nonexistent/shingle-test-program"),
        &[],
        &environment()
      )
      .is_err()
    );
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn autostart_and_saved_clients_run_in_the_client_environment() {
    let directory = temp_dir();
    let autostart = directory.join("autostart");
    let report =
      launch_autostart(&[dump_to(&autostart)], None, &environment(), true);
    assert!(report.failed.is_empty());
    let child = report.children.into_iter().next().unwrap();
    assert_eq!(
      wait_and_read(child, &autostart),
      "unset|/run/shingle-ipc|wayland-7|unset"
    );

    let saved = directory.join("saved");
    let output = OutputId::from("HEADLESS-1");
    let mut core = Shingle::new();
    core
      .add_output(output.clone(), Rect::new(0, 0, 640, 480))
      .unwrap();
    let mut identity = LaunchIdentity::new("saved");
    identity.command = vec!["/bin/sh".into(), "-c".into(), dump_to(&saved)];
    identity.launch_id = Some("restore-1".into());
    core.attach_window(&output, identity).unwrap();
    let report = launch_saved(&core.snapshot(), &environment(), true);
    assert!(report.failed.is_empty());
    let child = report.launched.into_iter().next().unwrap().child;
    assert_eq!(
      wait_and_read(child, &saved),
      "unset|/run/shingle-ipc|wayland-7|restore-1"
    );
    fs::remove_dir_all(directory).unwrap();
  }

  /// The files in `directory` whose names start with `prefix`.
  fn files_starting(directory: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files = fs::read_dir(directory)
      .unwrap()
      .map(|entry| entry.unwrap().path())
      .filter(|path| {
        path
          .file_name()
          .and_then(|name| name.to_str())
          .is_some_and(|name| name.starts_with(prefix))
      })
      .collect::<Vec<_>>();
    files.sort();
    files
  }

  #[test]
  fn a_corrupt_session_is_set_aside_and_the_session_starts_empty() {
    let directory = temp_dir();
    let path = directory.join("session.json");
    let store = SessionStore::new(&path);
    for contents in [&b"{ not json"[..], b"{\"version\": 999}", b"\xff\xfe"] {
      fs::write(&path, contents).unwrap();
      assert!(store.load().unwrap().is_none());
      assert!(!path.exists(), "the corrupt file is moved away");
    }
    // Three corrupt files, none overwritten, none deleted.
    let aside = files_starting(&directory, "session.json.corrupt-");
    assert_eq!(aside.len(), 3, "{aside:?}");
    let mut contents = aside
      .iter()
      .map(|path| fs::read(path).unwrap())
      .collect::<Vec<_>>();
    contents.sort();
    let mut expected = vec![
      b"{ not json".to_vec(),
      b"{\"version\": 999}".to_vec(),
      vec![0xff, 0xfe],
    ];
    expected.sort();
    assert_eq!(contents, expected);
    // A good session saved afterwards loads again.
    store.save(&Shingle::new()).unwrap();
    assert!(store.load().unwrap().is_some());
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn save_snapshot_writes_the_model_as_it_was_when_taken() {
    let directory = temp_dir();
    let store = SessionStore::new(directory.join("state/session.json"));
    let mut core = Shingle::new();
    let first = OutputId::new("FIRST-1");
    core
      .add_output(first.clone(), Rect::new(0, 0, 640, 480))
      .unwrap();
    let snapshot = core.snapshot();
    core
      .add_output(OutputId::new("SECOND-1"), Rect::new(640, 0, 640, 480))
      .unwrap();
    store.save_snapshot(&snapshot).unwrap();
    let (loaded, _) = store.load().unwrap().expect("a saved session");
    assert_eq!(loaded.outputs().collect::<Vec<_>>(), [&first]);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn corrupt_session_messages_name_each_cause_once() {
    let json = SessionSnapshot::from_json("{ x").unwrap_err();
    let parse = std::error::Error::source(&json).unwrap().to_string();
    assert_eq!(json.to_string(), parse, "the core error shows its source");
    let error = SessionError::from(json);
    assert_eq!(error_chain(&error), parse);
    let error = SessionError::from(io::Error::new(
      io::ErrorKind::InvalidData,
      "stream did not contain valid UTF-8",
    ));
    assert_eq!(
      error_chain(&error),
      "failed to access session state: stream did not contain valid UTF-8"
    );
  }

  #[test]
  fn empty_or_relative_xdg_state_home_falls_back_to_home() {
    let home = Some(OsString::from("/home/user"));
    let fallback = Some(PathBuf::from("/home/user/.local/state"));
    for ignored in ["", "relative/state"] {
      assert_eq!(
        state_directory(Some(ignored.into()), home.clone()),
        fallback,
        "{ignored:?}"
      );
    }
    assert_eq!(state_directory(None, home.clone()), fallback);
    assert_eq!(
      state_directory(Some("/state".into()), home),
      Some(PathBuf::from("/state"))
    );
    assert_eq!(state_directory(Some("".into()), Some("".into())), None);
    assert_eq!(state_directory(None, None), None);
  }

  #[test]
  fn session_path_prefers_shingle_location() {
    let state = Path::new("/state");
    let path = resolve_session_path(state, |_| true);
    assert_eq!(path, state.join("shingle/session.json"));
    let path = resolve_session_path(state, |_| false);
    assert_eq!(path, state.join("shingle/session.json"));
  }

  #[test]
  fn session_path_falls_back_to_legacy_tilefire_location() {
    let state = Path::new("/state");
    let legacy = state.join("tilefire/session.json");
    let path = resolve_session_path(state, |path| path == legacy);
    assert_eq!(path, legacy);
  }
}
