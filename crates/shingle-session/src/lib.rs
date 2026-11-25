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
//! Pre-rename Tilefire sessions and `TILEFIRE_LAUNCH_ID` are still read;
//! see README.md#upgrading-from-tilefire.

#![warn(missing_docs)]

use std::{
  collections::{BTreeMap, BTreeSet},
  ffi::OsStr,
  fs, io,
  path::{Path, PathBuf},
  process::{Child, Command, Stdio},
};

use shingle_core::{
  LaunchIdentity, NodeSnapshot, SessionSnapshot, Shingle, ShingleError,
  WindowId,
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
  /// Neither `XDG_STATE_HOME` nor `HOME` is set.
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
  /// An empty `XDG_STATE_HOME` is used as is, which makes the path
  /// relative, rather than ignored as the XDG base directory spec asks.
  ///
  /// # Errors
  ///
  /// [`SessionError::StateDirectoryUnavailable`] if neither variable is set.
  pub fn default_location() -> Result<Self, SessionError> {
    let state = std::env::var_os("XDG_STATE_HOME")
      .map(PathBuf::from)
      .or_else(|| {
        std::env::var_os("HOME")
          .map(|home| PathBuf::from(home).join(".local/state"))
      })
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
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent)?;
    }
    session.save_atomic(&self.path)?;
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
  /// # Errors
  ///
  /// [`SessionError::Io`] if the file exists but cannot be read and
  /// [`SessionError::Core`] if it is not a valid snapshot. A corrupt file is
  /// left in place, not renamed or deleted, and since the frontend
  /// propagates this error it then refuses to start until the file is
  /// removed or `session.restore` is turned off.
  pub fn load(
    &self,
  ) -> Result<Option<(Shingle, SessionSnapshot)>, SessionError> {
    let json = match fs::read_to_string(&self.path) {
      Ok(json) => json,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return Ok(None);
      }
      Err(error) => return Err(error.into()),
    };
    let mut snapshot = SessionSnapshot::from_json(&json)?;
    assign_restore_ids(&mut snapshot);
    let session = Shingle::from_snapshot(snapshot.clone())?;
    Ok(Some((session, snapshot)))
  }

  /// [`Self::load`] followed by [`RestoreReport::launch`].
  ///
  /// The relaunched clients connect to the `WAYLAND_DISPLAY` of the calling
  /// process, which inside a nested compositor is the host session, not
  /// Shingle. The frontend therefore calls [`Self::load`] and
  /// [`launch_saved`] with the compositor's own socket instead.
  ///
  /// # Errors
  ///
  /// As for [`Self::load`]. Launch failures do not fail the call; they are
  /// collected in [`RestoreReport::failed`].
  pub fn restore(&self) -> Result<Option<RestoreReport>, SessionError> {
    let Some((session, snapshot)) = self.load()? else {
      return Ok(None);
    };
    Ok(Some(RestoreReport::launch(session, &snapshot)))
  }
}

/// A loaded session and the clients started for it.
pub struct RestoreReport {
  /// The restored model, with every saved window waiting for its client.
  pub session: Shingle,
  /// Processes that were spawned. Their windows have not mapped yet.
  pub launched: Vec<LaunchedWindow>,
  /// [`SessionError::Launch`] for each launch group that failed to spawn.
  pub failed: Vec<SessionError>,
}

impl RestoreReport {
  /// Runs [`launch_saved`] for `snapshot` against this process's
  /// `WAYLAND_DISPLAY`, discarding the clients' output. See
  /// [`SessionStore::restore`] for why that is rarely the right display.
  pub fn launch(session: Shingle, snapshot: &SessionSnapshot) -> Self {
    let display = std::env::var_os("WAYLAND_DISPLAY");
    let report = launch_saved(snapshot, display.as_deref(), true);
    Self {
      session,
      launched: report.launched,
      failed: report.failed,
    }
  }
}

/// The result of [`launch_saved`].
pub struct LaunchReport {
  /// Processes that were spawned, one per launch group, in tile order.
  pub launched: Vec<LaunchedWindow>,
  /// [`SessionError::Launch`] for each launch group that failed to spawn.
  pub failed: Vec<SessionError>,
}

/// Runs `command` through `/bin/sh -c` with `WAYLAND_DISPLAY` set to
/// `display`, for autostart, `command` bindings and IPC `run`.
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
  display: &OsStr,
  discard_output: bool,
) -> io::Result<Child> {
  let mut process = Command::new("/bin/sh");
  process
    .arg("-c")
    .arg(command)
    .env("WAYLAND_DISPLAY", display);
  if discard_output {
    process.stdout(Stdio::null()).stderr(Stdio::null());
  }
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
  display: &OsStr,
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
    match launch_command(command, display, discard_output) {
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
/// window has none) and, when `display` is given, `WAYLAND_DISPLAY`. Launch
/// IDs are only filled in by [`SessionStore::load`].
///
/// Spawn failures are collected, not returned early, so one missing program
/// does not stop the rest of the session.
pub fn launch_saved(
  snapshot: &SessionSnapshot,
  display: Option<&OsStr>,
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
    match launch_window(id, identity, display, discard_output) {
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
  display: Option<&OsStr>,
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
  if let Some(display) = display {
    command.env("WAYLAND_DISPLAY", display);
  }
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
  use super::*;

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
