//! Running what the user chose, with the environment Shingle gives its own
//! children (docs/design/launcher.md, section 9).

use std::{
  ffi::{OsStr, OsString},
  io,
  path::{Path, PathBuf},
};

use shingle_session::{ClientEnvironment, launch_command};

use crate::{desktop::shell_quote, index::find_executable, model::Launch};

/// Terminals tried, in order, when neither the config nor `$TERMINAL` names
/// one.
const TERMINALS: [&str; 4] = ["foot", "kitty", "alacritty", "xterm"];

/// The terminal for entries with `Terminal=true`: `configured` when set,
/// else `$TERMINAL`, else the first of foot, kitty, alacritty and xterm
/// on `path_dirs`.
pub fn pick_terminal(
  configured: &str,
  environment_terminal: Option<OsString>,
  path_dirs: &[PathBuf],
) -> Option<String> {
  if !configured.trim().is_empty() {
    return Some(configured.trim().to_owned());
  }
  if let Some(terminal) = environment_terminal
    .and_then(|value| value.into_string().ok())
    .filter(|value| !value.trim().is_empty())
  {
    return Some(terminal);
  }
  TERMINALS
    .into_iter()
    .find(|name| find_executable(name, path_dirs).is_some())
    .map(str::to_owned)
}

/// The `/bin/sh` command line for `launch`: its command, or for a terminal
/// entry `TERMINAL -e sh -c COMMAND`. `None` when it needs a terminal and
/// there is none.
pub fn command_line(launch: &Launch, terminal: Option<&str>) -> Option<String> {
  if !launch.terminal {
    return Some(launch.command.clone());
  }
  let terminal = terminal?;
  Some(format!(
    "exec {terminal} -e sh -c {}",
    shell_quote(&launch.command)
  ))
}

/// The client environment of an environment looked up through `var`: the
/// launcher's own `WAYLAND_DISPLAY`, `DISPLAY` and `WAYFIRE_SOCKET` pass on,
/// and `WAYLAND_SOCKET` (the launcher's own connection, when it was given
/// one) never does.
pub fn client_environment(
  var: impl Fn(&str) -> Option<OsString>,
) -> ClientEnvironment {
  let display = var("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
  let x_display = var("DISPLAY");
  let ipc = var("WAYFIRE_SOCKET").map(PathBuf::from);
  ClientEnvironment::new(
    &display,
    x_display.as_deref().map(OsStr::new),
    ipc.as_deref().map(Path::new),
  )
}

/// Runs `line` through `/bin/sh -c` in `environment` without waiting for
/// it; the launcher exits next, and the child is reparented.
pub fn spawn(line: &str, environment: &ClientEnvironment) -> io::Result<u32> {
  let child = launch_command(line, environment, false)?;
  Ok(child.id())
}

#[cfg(test)]
mod tests {
  use std::{fs, os::unix::fs::PermissionsExt};

  use super::*;

  fn launch(command: &str, terminal: bool) -> Launch {
    Launch {
      id: None,
      command: command.to_owned(),
      terminal,
    }
  }

  #[test]
  fn terminals_come_from_the_config_the_environment_or_path() {
    let directory = tempfile::tempdir().unwrap();
    let kitty = directory.path().join("kitty");
    fs::write(&kitty, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&kitty, fs::Permissions::from_mode(0o755)).unwrap();
    let dirs = [directory.path().to_owned()];
    assert_eq!(
      pick_terminal("wezterm start", Some("xterm".into()), &dirs).as_deref(),
      Some("wezterm start")
    );
    assert_eq!(
      pick_terminal(" ", Some("konsole".into()), &dirs).as_deref(),
      Some("konsole")
    );
    assert_eq!(pick_terminal("", None, &dirs).as_deref(), Some("kitty"));
    assert_eq!(pick_terminal("", None, &[]), None);
  }

  #[test]
  fn terminal_entries_run_inside_the_terminal() {
    assert_eq!(
      command_line(&launch("exec htop", false), None).as_deref(),
      Some("exec htop")
    );
    assert_eq!(
      command_line(&launch("exec htop -d 5", true), Some("foot")).as_deref(),
      Some("exec foot -e sh -c 'exec htop -d 5'")
    );
    assert_eq!(command_line(&launch("htop", true), None), None);
  }

  #[test]
  fn the_environment_passes_the_sockets_and_drops_wayland_socket() {
    let environment = client_environment(|name| match name {
      "WAYLAND_DISPLAY" => Some("wayland-7".into()),
      "WAYFIRE_SOCKET" => Some("/run/user/1/ipc".into()),
      "WAYLAND_SOCKET" => Some("12".into()),
      _ => None,
    });
    assert_eq!(
      environment.value("WAYLAND_DISPLAY"),
      Some(OsStr::new("wayland-7"))
    );
    assert_eq!(
      environment.value("WAYFIRE_SOCKET"),
      Some(OsStr::new("/run/user/1/ipc"))
    );
    assert_eq!(environment.value("WAYLAND_SOCKET"), None);
    assert!(
      environment
        .variables()
        .iter()
        .any(|(name, value)| name == "WAYLAND_SOCKET" && value.is_none())
    );
    assert!(
      environment
        .variables()
        .iter()
        .any(|(name, value)| name == "DISPLAY" && value.is_none())
    );
  }

  #[test]
  fn spawned_commands_get_the_environment() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("out");
    let environment = client_environment(|name| {
      (name == "WAYLAND_DISPLAY").then(|| "wayland-9".into())
    });
    let line = format!(
      "echo \"$WAYLAND_DISPLAY\" > {}",
      shell_quote(&out.to_string_lossy())
    );
    spawn(&line, &environment).unwrap();
    let deadline =
      std::time::Instant::now() + std::time::Duration::from_secs(5);
    while fs::read_to_string(&out).map_or(true, |text| !text.ends_with('\n')) {
      assert!(std::time::Instant::now() < deadline);
      std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(&out).unwrap(), "wayland-9\n");
  }
}
