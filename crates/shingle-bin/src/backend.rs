//! Backend selection.
//!
//! Shingle picks its backend the way Wayfire does, which leaves it to
//! wlroots (wayfire/src/main.cpp:422 calls `wlr_backend_autocreate`). In
//! wlroots' backend/backend.c, `WLR_BACKENDS` names the backends explicitly
//! and an unknown name is an error; otherwise `WAYLAND_DISPLAY` or
//! `WAYLAND_SOCKET` selects the Wayland backend, then `DISPLAY` the X11
//! one, and only with neither does it open a session for DRM and libinput.
//! A nested backend that fails to start is an error, not a reason to try
//! the next one. Shingle follows that order, with `--backend` (or
//! `--headless`) before `SHINGLE_BACKEND` before the display variables. One
//! difference: an empty variable counts as unset here, where wlroots checks
//! only that `getenv` returns non-NULL, so a stray `WAYLAND_DISPLAY=` in a
//! TTY login does not make Shingle try to nest.

use std::{
  ffi::{OsStr, OsString},
  fmt,
  str::FromStr,
};

/// The environment variable that overrides automatic selection, like
/// wlroots' `WLR_BACKENDS` but naming one backend.
pub const BACKEND_VARIABLE: &str = "SHINGLE_BACKEND";

/// The variables that mean Shingle runs inside a host session, in the
/// order wlroots checks them.
const HOST_VARIABLES: [&str; 3] =
  ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"];

/// How Shingle reaches its display and input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
  /// A window in a host Wayland or X11 session (winit).
  Nested,
  /// DRM/KMS and libinput on a seat, started from a TTY.
  Native,
  /// No display or input devices; clients and IPC only.
  Headless,
  /// Headless, but drawing with the GPU into an offscreen target, for
  /// GPU clients and the GPU harness.
  Offscreen,
}

impl Backend {
  /// The names `--backend` and `SHINGLE_BACKEND` accept.
  pub const NAMES: &str = "nested, native, headless or offscreen";

  /// The name of the backend, as `--backend` takes it.
  pub fn name(self) -> &'static str {
    match self {
      Self::Nested => "nested",
      Self::Native => "native",
      Self::Headless => "headless",
      Self::Offscreen => "offscreen",
    }
  }
}

impl fmt::Display for Backend {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(self.name())
  }
}

/// A backend name that is not one of [`Backend::NAMES`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownBackend(pub String);

impl fmt::Display for UnknownBackend {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      formatter,
      "unknown backend '{}' (expected {})",
      self.0,
      Backend::NAMES
    )
  }
}

impl std::error::Error for UnknownBackend {}

impl FromStr for Backend {
  type Err = UnknownBackend;

  fn from_str(name: &str) -> Result<Self, Self::Err> {
    match name {
      "nested" => Ok(Self::Nested),
      "native" => Ok(Self::Native),
      "headless" => Ok(Self::Headless),
      "offscreen" => Ok(Self::Offscreen),
      _ => Err(UnknownBackend(name.to_owned())),
    }
  }
}

/// Why a backend was selected, for the startup message and for error
/// messages that explain the choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reason {
  /// `--backend` or `--headless` on the command line.
  Argument,
  /// [`BACKEND_VARIABLE`] in the environment.
  Variable,
  /// This host-session variable is set.
  HostSession(&'static str),
  /// No host-session variable is set.
  NoHostSession,
}

impl fmt::Display for Reason {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Argument => formatter.write_str("requested on the command line"),
      Self::Variable => write!(formatter, "{BACKEND_VARIABLE} is set"),
      Self::HostSession(variable) => write!(formatter, "{variable} is set"),
      Self::NoHostSession => formatter
        .write_str("none of WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is set"),
    }
  }
}

/// The selected backend and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
  /// The backend to run.
  pub backend: Backend,
  /// Why it was selected.
  pub reason: Reason,
}

/// Selection failed because [`BACKEND_VARIABLE`] names no backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidVariable(pub UnknownBackend);

impl fmt::Display for InvalidVariable {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{BACKEND_VARIABLE}: {}", self.0)
  }
}

impl std::error::Error for InvalidVariable {}

/// Selects the backend from the one `requested` on the command line, if
/// any, and the environment, which `env` looks up by name.
///
/// `requested` wins, then [`BACKEND_VARIABLE`], then the first of
/// `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` and `DISPLAY` that is set selects
/// [`Backend::Nested`]; with none of them it is [`Backend::Native`].
/// Empty values count as unset. A [`BACKEND_VARIABLE`] that names no
/// backend is an error rather than falling back to detection, as an
/// unknown name in `WLR_BACKENDS` is in wlroots.
pub fn select(
  requested: Option<Backend>,
  env: impl Fn(&str) -> Option<OsString>,
) -> Result<Selection, InvalidVariable> {
  let set = |name: &str| env(name).filter(|value| !value.is_empty());
  if let Some(backend) = requested {
    return Ok(Selection {
      backend,
      reason: Reason::Argument,
    });
  }
  if let Some(value) = set(BACKEND_VARIABLE) {
    let backend = value
      .to_str()
      .ok_or_else(|| UnknownBackend(lossy(&value)))
      .and_then(str::parse)
      .map_err(InvalidVariable)?;
    return Ok(Selection {
      backend,
      reason: Reason::Variable,
    });
  }
  let reason = HOST_VARIABLES
    .into_iter()
    .find(|name| set(name).is_some())
    .map_or(Reason::NoHostSession, Reason::HostSession);
  let backend = match reason {
    Reason::NoHostSession => Backend::Native,
    _ => Backend::Nested,
  };
  Ok(Selection { backend, reason })
}

/// `value` with invalid UTF-8 replaced, for error messages.
fn lossy(value: &OsStr) -> String {
  value.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use super::*;

  const ALL: [Backend; 4] = [
    Backend::Nested,
    Backend::Native,
    Backend::Headless,
    Backend::Offscreen,
  ];

  /// An environment lookup over `pairs`.
  fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
      .iter()
      .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
      .collect();
    move |name| map.get(name).cloned()
  }

  fn selected(requested: Option<Backend>, pairs: &[(&str, &str)]) -> Selection {
    select(requested, env(pairs)).expect("selection succeeds")
  }

  #[test]
  fn wayland_display_selects_nested() {
    assert_eq!(
      selected(None, &[("WAYLAND_DISPLAY", "wayland-1")]),
      Selection {
        backend: Backend::Nested,
        reason: Reason::HostSession("WAYLAND_DISPLAY"),
      }
    );
  }

  #[test]
  fn wayland_socket_selects_nested() {
    assert_eq!(
      selected(None, &[("WAYLAND_SOCKET", "3")]).reason,
      Reason::HostSession("WAYLAND_SOCKET")
    );
  }

  #[test]
  fn x11_display_selects_nested() {
    assert_eq!(
      selected(None, &[("DISPLAY", ":0")]),
      Selection {
        backend: Backend::Nested,
        reason: Reason::HostSession("DISPLAY"),
      }
    );
  }

  #[test]
  fn wayland_is_reported_before_x11() {
    let both = [("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-1")];
    assert_eq!(
      selected(None, &both).reason,
      Reason::HostSession("WAYLAND_DISPLAY")
    );
  }

  #[test]
  fn no_host_session_selects_native() {
    assert_eq!(
      selected(None, &[("XDG_SESSION_TYPE", "tty")]),
      Selection {
        backend: Backend::Native,
        reason: Reason::NoHostSession,
      }
    );
  }

  #[test]
  fn empty_host_variables_count_as_unset() {
    let empty = [
      ("WAYLAND_DISPLAY", ""),
      ("WAYLAND_SOCKET", ""),
      ("DISPLAY", ""),
    ];
    assert_eq!(selected(None, &empty).backend, Backend::Native);
  }

  #[test]
  fn argument_overrides_environment() {
    let host = [
      ("WAYLAND_DISPLAY", "wayland-1"),
      (BACKEND_VARIABLE, "nested"),
    ];
    for backend in ALL {
      assert_eq!(
        selected(Some(backend), &host),
        Selection {
          backend,
          reason: Reason::Argument,
        }
      );
    }
  }

  #[test]
  fn argument_wins_over_an_invalid_variable() {
    let invalid = [(BACKEND_VARIABLE, "drm")];
    assert_eq!(
      selected(Some(Backend::Headless), &invalid).backend,
      Backend::Headless
    );
  }

  #[test]
  fn variable_overrides_host_session() {
    let host = [
      ("WAYLAND_DISPLAY", "wayland-1"),
      (BACKEND_VARIABLE, "headless"),
    ];
    assert_eq!(
      selected(None, &host),
      Selection {
        backend: Backend::Headless,
        reason: Reason::Variable,
      }
    );
  }

  #[test]
  fn offscreen_is_never_selected_automatically() {
    for pairs in [
      &[("WAYLAND_DISPLAY", "wayland-1")][..],
      &[("DISPLAY", ":0")][..],
      &[][..],
    ] {
      assert_ne!(selected(None, pairs).backend, Backend::Offscreen);
    }
    assert_eq!(
      selected(None, &[(BACKEND_VARIABLE, "offscreen")]).backend,
      Backend::Offscreen
    );
  }

  #[test]
  fn empty_variable_is_ignored() {
    let host = [("DISPLAY", ":1"), (BACKEND_VARIABLE, "")];
    assert_eq!(selected(None, &host).backend, Backend::Nested);
  }

  #[test]
  fn unknown_variable_is_an_error_without_fallback() {
    let host = [("WAYLAND_DISPLAY", "wayland-1"), (BACKEND_VARIABLE, "drm")];
    let error = select(None, env(&host)).unwrap_err();
    assert_eq!(error, InvalidVariable(UnknownBackend("drm".into())));
    assert_eq!(
      error.to_string(),
      "SHINGLE_BACKEND: unknown backend 'drm' \
       (expected nested, native, headless or offscreen)"
    );
  }

  #[test]
  fn non_utf8_variable_is_an_error() {
    use std::os::unix::ffi::OsStringExt;
    let value = OsString::from_vec(vec![b'n', 0xff]);
    let error = select(None, move |name| {
      (name == BACKEND_VARIABLE).then(|| value.clone())
    })
    .unwrap_err();
    assert_eq!(error.0, UnknownBackend("n\u{fffd}".into()));
  }

  #[test]
  fn names_round_trip() {
    for backend in ALL {
      assert_eq!(backend.name().parse(), Ok(backend));
      assert_eq!(backend.to_string(), backend.name());
    }
    assert_eq!(
      "Nested".parse::<Backend>(),
      Err(UnknownBackend("Nested".into()))
    );
  }

  #[test]
  fn reasons_name_the_variable() {
    assert_eq!(Reason::HostSession("DISPLAY").to_string(), "DISPLAY is set");
    assert_eq!(Reason::Variable.to_string(), "SHINGLE_BACKEND is set");
    assert_eq!(
      Reason::NoHostSession.to_string(),
      "none of WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is set"
    );
  }
}
