//! The headless backend: the compositor without display or input devices,
//! serving clients and IPC until SIGINT, SIGTERM or a quit request.

use std::{error::Error, ffi::OsString, path::Path, process::Child};

use shingle::{ClientEnvironment, Compositor, Config};
use shingle_config::ShingleConfig;
use shingle_session::{
  SessionStore, launch_autostart, launch_program, launch_saved,
};

/// Runs Shingle headless with `config` and saves the session when it ends.
///
/// With `ipc_path`, Wayfire IPC is served there, and every client finds it
/// as `WAYFIRE_SOCKET`. `spawn`, if not empty, is a program and its
/// arguments to start as a client.
pub fn run(
  yaml: ShingleConfig,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
) -> Result<(), Box<dyn Error>> {
  let store = if let Some(path) = &yaml.session.path {
    SessionStore::new(path)
  } else {
    SessionStore::default_location()?
  };
  let restored = if yaml.session.restore {
    store.load()?
  } else {
    None
  };
  let keymap = yaml.input.load_keymap()?;
  let config = Config::from(yaml.clone());
  let (mut compositor, snapshot) = if let Some((core, snapshot)) = restored {
    (Compositor::with_core(config, core)?, Some(snapshot))
  } else {
    (Compositor::new(config)?, None)
  };
  compositor.set_keymap(&keymap)?;
  if let Some(path) = ipc_path {
    compositor.enable_ipc(path)?;
  }
  let discard_output = yaml.behavior.discard_command_output;
  let environment = compositor.client_environment();
  let mut session_children = snapshot
    .as_ref()
    .map(|snapshot| {
      let report = launch_saved(snapshot, &environment, discard_output);
      for error in &report.failed {
        eprintln!("shingle: session restore launch failed: {error}");
      }
      report
        .launched
        .into_iter()
        .map(|launched| launched.child)
        .collect::<Vec<_>>()
    })
    .unwrap_or_default();
  let autostart = launch_autostart(
    &yaml.autostart,
    snapshot.as_ref(),
    &environment,
    discard_output,
  );
  for error in &autostart.failed {
    eprintln!("shingle: {error}");
  }
  session_children.extend(autostart.children);
  println!(
    "WAYLAND_DISPLAY={}",
    compositor.socket_name().to_string_lossy()
  );
  if let Some(child) = spawn_client(&spawn, &environment)? {
    session_children.push(child);
  }
  let _session_children = session_children;
  compositor.run()?;
  store.save(compositor.core())?;
  Ok(())
}

/// Starts `spawn`'s program, if any, with its arguments in the client
/// `environment`, as the nested backend does.
fn spawn_client(
  spawn: &[OsString],
  environment: &ClientEnvironment,
) -> std::io::Result<Option<Child>> {
  let Some((program, arguments)) = spawn.split_first() else {
    return Ok(None);
  };
  launch_program(program, arguments, environment).map(Some)
}

#[cfg(test)]
mod tests {
  use std::ffi::OsStr;

  use super::*;

  /// The environment of a compositor on `wayland-9` without IPC.
  fn environment() -> ClientEnvironment {
    ClientEnvironment::new(OsStr::new("wayland-9"), None, None)
  }

  #[test]
  fn nothing_to_spawn() {
    assert!(spawn_client(&[], &environment()).unwrap().is_none());
  }

  #[test]
  fn spawned_client_gets_the_socket_and_arguments() {
    let spawn = [
      "sh",
      "-c",
      concat!(
        r#"test "$WAYLAND_DISPLAY" = wayland-9 && test "$0" = x"#,
        r#" && test -z "${DISPLAY+set}${WAYFIRE_SOCKET+set}""#,
      ),
      "x",
    ]
    .map(OsString::from);
    let mut child = spawn_client(&spawn, &environment())
      .expect("sh starts")
      .expect("a child");
    assert!(child.wait().expect("sh exits").success());
  }

  #[test]
  fn missing_program_is_an_error() {
    let spawn = [OsString::from("/nonexistent/shingle-test-program")];
    assert!(spawn_client(&spawn, &environment()).is_err());
  }
}
