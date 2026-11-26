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
///
/// [`ClientEnvironment::with_desktop`] adds what Wayfire exports beyond
/// that: `XDG_CURRENT_DESKTOP`, `XCURSOR_THEME`, `XCURSOR_SIZE` and
/// `WAYFIRE_CONFIG_FILE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientEnvironment {
  /// Each variable with the value children get, or `None` to remove it, in
  /// the order they are applied.
  variables: Vec<(OsString, Option<OsString>)>,
}

/// `[input] cursor_size`'s default, at which Shingle's own cursor follows
/// an inherited `XCURSOR_SIZE` (crates/shingle/src/cursor_shape.rs).
const DEFAULT_CURSOR_SIZE: u32 = 24;

/// The configuration behind the variables
/// [`ClientEnvironment::with_desktop`] adds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopVariables<'a> {
  /// `[input] cursor_theme`; `default` leaves `XCURSOR_THEME` alone.
  pub cursor_theme: &'a str,
  /// `[input] cursor_size`.
  pub cursor_size: u32,
  /// The Wayfire file Shingle's configuration was read from, if it was.
  pub wayfire_config_file: Option<&'a Path>,
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

  /// Adds the variables Wayfire exports to every child beyond its sockets,
  /// from `desktop` and `inherited`, which looks up a variable of the
  /// environment Shingle itself was started with (`std::env::var_os`):
  ///
  /// - `XDG_CURRENT_DESKTOP` is `wayfire` when `inherited` has none, as
  ///   Wayfire sets it at start (src/main.cpp), so that
  ///   xdg-desktop-portal reads `wayfire-portals.conf`. One the login or
  ///   the session file set, even to an empty value, is kept.
  /// - `XCURSOR_THEME` is [`DesktopVariables::cursor_theme`] unless that
  ///   is `default`, which leaves the inherited theme, as Wayfire's
  ///   `init_xcursor` does (src/core/seat/cursor.cpp).
  /// - `XCURSOR_SIZE` is [`DesktopVariables::cursor_size`]. Shingle draws
  ///   its own cursor at an inherited `XCURSOR_SIZE` while the configured
  ///   size is the default 24, so then a numeric inherited size is kept,
  ///   and children draw theirs at the size Shingle draws.
  /// - `WAYFIRE_CONFIG_FILE` is [`DesktopVariables::wayfire_config_file`],
  ///   and removed without one, so a host Wayfire's file never reaches
  ///   WCM or scripts started from Shingle.
  pub fn with_desktop(
    mut self,
    desktop: &DesktopVariables<'_>,
    inherited: impl Fn(&str) -> Option<OsString>,
  ) -> Self {
    let mut set = |name: &str, value: Option<&OsStr>| {
      self
        .variables
        .push((OsString::from(name), value.map(OsStr::to_owned)));
    };
    if inherited("XDG_CURRENT_DESKTOP").is_none() {
      set("XDG_CURRENT_DESKTOP", Some(OsStr::new("wayfire")));
    }
    if desktop.cursor_theme != "default" {
      set("XCURSOR_THEME", Some(OsStr::new(desktop.cursor_theme)));
    }
    let inherited_size = inherited("XCURSOR_SIZE")
      .and_then(|size| size.to_str()?.parse::<u32>().ok());
    if desktop.cursor_size != DEFAULT_CURSOR_SIZE || inherited_size.is_none() {
      let size = desktop.cursor_size.to_string();
      set("XCURSOR_SIZE", Some(OsStr::new(&size)));
    }
    set(
      "WAYFIRE_CONFIG_FILE",
      desktop.wayfire_config_file.map(Path::as_os_str),
    );
    self
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

  /// What `with_desktop` adds to a bare environment, for `desktop` and
  /// the inherited variables `inherited`.
  fn desktop_changes(
    desktop: &DesktopVariables<'_>,
    inherited: &[(&str, &str)],
  ) -> Vec<(OsString, Option<OsString>)> {
    let base = ClientEnvironment::new(OsStr::new("wayland-1"), None, None);
    let skip = base.variables().len();
    let lookup = |name: &str| {
      inherited
        .iter()
        .find(|(variable, _)| *variable == name)
        .map(|(_, value)| OsString::from(value))
    };
    base.with_desktop(desktop, lookup).variables()[skip..].to_vec()
  }

  /// The configuration's defaults: theme `default`, size 24, no Wayfire
  /// file.
  const DEFAULTS: DesktopVariables<'static> = DesktopVariables {
    cursor_theme: "default",
    cursor_size: 24,
    wayfire_config_file: None,
  };

  #[test]
  fn defaults_set_wayfire_s_variables_into_a_bare_environment() {
    assert_eq!(
      desktop_changes(&DEFAULTS, &[]),
      [
        ("XDG_CURRENT_DESKTOP".into(), Some("wayfire".into())),
        ("XCURSOR_SIZE".into(), Some("24".into())),
        ("WAYFIRE_CONFIG_FILE".into(), None),
      ]
    );
  }

  #[test]
  fn an_inherited_current_desktop_is_kept_even_when_empty() {
    for value in ["GNOME", "sway:wlroots", ""] {
      let changes =
        desktop_changes(&DEFAULTS, &[("XDG_CURRENT_DESKTOP", value)]);
      assert!(
        changes
          .iter()
          .all(|(name, _)| name != "XDG_CURRENT_DESKTOP"),
        "{value:?}: {changes:?}"
      );
    }
  }

  #[test]
  fn a_configured_cursor_theme_and_size_replace_the_inherited_ones() {
    let desktop = DesktopVariables {
      cursor_theme: "Adwaita",
      cursor_size: 32,
      ..DEFAULTS
    };
    let inherited = [("XCURSOR_THEME", "breeze"), ("XCURSOR_SIZE", "48")];
    let changes = desktop_changes(&desktop, &inherited);
    assert!(
      changes.contains(&("XCURSOR_THEME".into(), Some("Adwaita".into())))
    );
    assert!(changes.contains(&("XCURSOR_SIZE".into(), Some("32".into()))));
  }

  #[test]
  fn the_default_theme_leaves_the_inherited_theme_alone() {
    for inherited in [&[][..], &[("XCURSOR_THEME", "breeze")][..]] {
      let changes = desktop_changes(&DEFAULTS, inherited);
      assert!(
        changes.iter().all(|(name, _)| name != "XCURSOR_THEME"),
        "{changes:?}"
      );
    }
  }

  #[test]
  fn the_default_size_keeps_a_numeric_inherited_size_only() {
    let size = |inherited: &[(&str, &str)]| {
      desktop_changes(&DEFAULTS, inherited)
        .into_iter()
        .find(|(name, _)| name == "XCURSOR_SIZE")
        .map(|(_, value)| value)
    };
    // Shingle draws its own cursor at 48 then, so children do too.
    assert_eq!(size(&[("XCURSOR_SIZE", "48")]), None);
    assert_eq!(size(&[("XCURSOR_SIZE", "big")]), Some(Some("24".into())));
    assert_eq!(size(&[]), Some(Some("24".into())));
  }

  #[test]
  fn the_wayfire_config_file_replaces_a_host_one_or_removes_it() {
    let host = [("WAYFIRE_CONFIG_FILE", "/home/host/.config/wayfire.ini")];
    let file = |desktop: &DesktopVariables<'_>| {
      desktop_changes(desktop, &host)
        .into_iter()
        .find(|(name, _)| name == "WAYFIRE_CONFIG_FILE")
        .map(|(_, value)| value)
    };
    assert_eq!(file(&DEFAULTS), Some(None));
    let ini = Path::new("/tmp/test/wayfire.ini");
    let desktop = DesktopVariables {
      wayfire_config_file: Some(ini),
      ..DEFAULTS
    };
    assert_eq!(file(&desktop), Some(Some(ini.as_os_str().to_owned())));
  }

  #[test]
  fn a_spawned_child_sees_wayfire_s_variables() {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(
      "printf '%s|%s|%s|%s' \"$XDG_CURRENT_DESKTOP\" \
       \"${XCURSOR_THEME-unset}\" \"$XCURSOR_SIZE\" \
       \"${WAYFIRE_CONFIG_FILE-unset}\"",
    );
    command
      .env_remove("XDG_CURRENT_DESKTOP")
      .env_remove("XCURSOR_THEME")
      .env("WAYFIRE_CONFIG_FILE", "/host/wayfire.ini");
    let desktop = DesktopVariables {
      cursor_size: 40,
      ..DEFAULTS
    };
    ClientEnvironment::new(OsStr::new("wayland-9"), None, None)
      .with_desktop(&desktop, |_| None)
      .apply(&mut command);
    let output = command.output().expect("sh runs");
    assert_eq!(
      String::from_utf8_lossy(&output.stdout),
      "wayfire|unset|40|unset"
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
