//! The environment of the processes Shingle starts.

use std::{
  ffi::{OsStr, OsString},
  path::Path,
  process::Command,
};

/// What Shingle sets and removes in the environment of every process it
/// starts: restored and autostarted clients, `--spawn`, command bindings,
/// IPC `stipc/run` and binding commands, and the lock client. Everything
/// else is inherited from Shingle.
///
/// A nested Shingle runs inside a host session whose `DISPLAY`,
/// `WAYFIRE_SOCKET` and possibly `WAYLAND_SOCKET` it inherits. None of them
/// may reach its children: X11 clients would open on the host's Xwayland,
/// IPC scripts would drive the host compositor, and libwayland prefers
/// `WAYLAND_SOCKET` over `WAYLAND_DISPLAY`. Shingle never changes its own
/// environment for this, because the nested backend still connects to the
/// host through it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientEnvironment {
  /// Each variable with the value children get, or `None` to remove it, in
  /// the order they are applied.
  variables: Vec<(OsString, Option<OsString>)>,
}

impl ClientEnvironment {
  /// The environment for clients of Shingle's Wayland socket
  /// `wayland_display` (a name in `XDG_RUNTIME_DIR` or an absolute path).
  ///
  /// - `WAYLAND_DISPLAY` is `wayland_display`, and `WAYLAND_SOCKET` is
  ///   removed.
  /// - `DISPLAY` is `x_display` when Shingle runs its own X server, and is
  ///   removed otherwise.
  /// - `WAYFIRE_SOCKET` is `ipc_socket` when Shingle serves Wayfire IPC, as
  ///   Wayfire's IPC plugin sets it before autostart runs, and is removed
  ///   otherwise.
  /// - `_JAVA_AWT_WM_NONREPARENTING` is `1`, which Wayfire sets for every
  ///   child (src/core/core.cpp:539) so Java's AWT draws correctly under a
  ///   non-reparenting window manager.
  pub fn new(
    wayland_display: &OsStr,
    x_display: Option<&OsStr>,
    ipc_socket: Option<&Path>,
  ) -> Self {
    let set = |name: &str, value: Option<&OsStr>| {
      (OsString::from(name), value.map(OsStr::to_owned))
    };
    Self {
      variables: vec![
        set("WAYLAND_DISPLAY", Some(wayland_display)),
        set("WAYLAND_SOCKET", None),
        set("DISPLAY", x_display),
        set("WAYFIRE_SOCKET", ipc_socket.map(Path::as_os_str)),
        set("_JAVA_AWT_WM_NONREPARENTING", Some(OsStr::new("1"))),
      ],
    }
  }

  /// Every variable this environment changes, with its value or `None` for
  /// a removed one, in the order they are applied.
  pub fn variables(&self) -> &[(OsString, Option<OsString>)] {
    &self.variables
  }

  /// The value `name` is set to, or `None` if it is removed or left as
  /// inherited.
  pub fn value(&self, name: &str) -> Option<&OsStr> {
    self
      .variables
      .iter()
      .find(|(variable, _)| variable == name)
      .and_then(|(_, value)| value.as_deref())
  }

  /// Applies this environment to `command`, overriding anything set on it
  /// before.
  pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
    for (name, value) in &self.variables {
      match value {
        Some(value) => command.env(name, value),
        None => command.env_remove(name),
      };
    }
    command
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The variables `command` changes, as `(name, value)` strings.
  fn changes(command: &Command) -> Vec<(String, Option<String>)> {
    let mut changes = command
      .get_envs()
      .map(|(name, value)| {
        (
          name.to_string_lossy().into_owned(),
          value.map(|value| value.to_string_lossy().into_owned()),
        )
      })
      .collect::<Vec<_>>();
    changes.sort();
    changes
  }

  #[test]
  fn without_x_or_ipc_display_and_wayfire_socket_are_removed() {
    let environment =
      ClientEnvironment::new(OsStr::new("wayland-3"), None, None);
    assert_eq!(
      environment.variables(),
      [
        ("WAYLAND_DISPLAY".into(), Some("wayland-3".into())),
        ("WAYLAND_SOCKET".into(), None),
        ("DISPLAY".into(), None),
        ("WAYFIRE_SOCKET".into(), None),
        ("_JAVA_AWT_WM_NONREPARENTING".into(), Some("1".into())),
      ]
    );
    assert_eq!(
      environment.value("WAYLAND_DISPLAY"),
      Some(OsStr::new("wayland-3"))
    );
    assert_eq!(environment.value("DISPLAY"), None);
    assert_eq!(environment.value("HOME"), None);
  }

  #[test]
  fn shingle_s_own_x_display_and_ipc_socket_are_exported() {
    let environment = ClientEnvironment::new(
      OsStr::new("/tmp/test/wayland"),
      Some(OsStr::new(":2")),
      Some(Path::new("/run/user/1000/shingle-ipc")),
    );
    assert_eq!(environment.value("DISPLAY"), Some(OsStr::new(":2")));
    assert_eq!(
      environment.value("WAYFIRE_SOCKET"),
      Some(OsStr::new("/run/user/1000/shingle-ipc"))
    );
    assert_eq!(
      environment.value("WAYLAND_DISPLAY"),
      Some(OsStr::new("/tmp/test/wayland"))
    );
    assert_eq!(environment.value("WAYLAND_SOCKET"), None);
    assert_eq!(
      environment.value("_JAVA_AWT_WM_NONREPARENTING"),
      Some(OsStr::new("1"))
    );
  }

  #[test]
  fn apply_overrides_what_the_host_session_set() {
    let mut command = Command::new("true");
    // What a nested Shingle would otherwise pass on from the host.
    command
      .env("DISPLAY", ":1")
      .env("WAYFIRE_SOCKET", "/tmp/wayfire-wayland-1.socket")
      .env("WAYLAND_SOCKET", "5")
      .env("WAYLAND_DISPLAY", "wayland-1")
      .env("KEPT", "yes");
    ClientEnvironment::new(OsStr::new("wayland-2"), None, None)
      .apply(&mut command);
    assert_eq!(
      changes(&command),
      [
        ("DISPLAY".into(), None),
        ("KEPT".into(), Some("yes".into())),
        ("WAYFIRE_SOCKET".into(), None),
        ("WAYLAND_DISPLAY".into(), Some("wayland-2".into())),
        ("WAYLAND_SOCKET".into(), None),
        ("_JAVA_AWT_WM_NONREPARENTING".into(), Some("1".into())),
      ]
    );
  }

  #[test]
  fn a_spawned_child_sees_exactly_the_client_environment() {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(
      "printf '%s|%s|%s|%s|%s' \"${DISPLAY-unset}\" \
       \"${WAYFIRE_SOCKET-unset}\" \"${WAYLAND_SOCKET-unset}\" \
       \"$WAYLAND_DISPLAY\" \"$_JAVA_AWT_WM_NONREPARENTING\"",
    );
    command
      .env("DISPLAY", ":1")
      .env("WAYFIRE_SOCKET", "/host")
      .env("WAYLAND_SOCKET", "5");
    ClientEnvironment::new(
      OsStr::new("wayland-9"),
      None,
      Some(Path::new("/ipc")),
    )
    .apply(&mut command);
    let output = command.output().expect("sh runs");
    assert_eq!(
      String::from_utf8_lossy(&output.stdout),
      "unset|/ipc|unset|wayland-9|1"
    );
  }
}
