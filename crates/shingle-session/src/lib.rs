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

#[derive(Debug, Error)]
pub enum SessionError {
  #[error(transparent)]
  Core(#[from] ShingleError),
  #[error("failed to access session state")]
  Io(#[from] io::Error),
  #[error("session state location is unavailable")]
  StateDirectoryUnavailable,
  #[error("failed to launch window {window:?}: {source}")]
  Launch { window: WindowId, source: io::Error },
  #[error("failed to launch autostart command {command:?}: {source}")]
  Autostart { command: String, source: io::Error },
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

#[derive(Clone, Debug)]
pub struct SessionStore {
  path: PathBuf,
}

impl SessionStore {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self { path: path.into() }
  }

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

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn save(&self, session: &Shingle) -> Result<(), SessionError> {
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent)?;
    }
    session.save_atomic(&self.path)?;
    Ok(())
  }

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

  pub fn restore(&self) -> Result<Option<RestoreReport>, SessionError> {
    let Some((session, snapshot)) = self.load()? else {
      return Ok(None);
    };
    Ok(Some(RestoreReport::launch(session, &snapshot)))
  }
}

pub struct RestoreReport {
  pub session: Shingle,
  pub launched: Vec<LaunchedWindow>,
  pub failed: Vec<SessionError>,
}

impl RestoreReport {
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

pub struct LaunchReport {
  pub launched: Vec<LaunchedWindow>,
  pub failed: Vec<SessionError>,
}

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

pub struct AutostartReport {
  pub children: Vec<Child>,
  pub failed: Vec<SessionError>,
}

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

pub struct LaunchedWindow {
  pub id: WindowId,
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
