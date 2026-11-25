//! Command-line parsing for the `shingle` binary.

use std::{ffi::OsString, fmt, path::PathBuf};

use crate::backend::{Backend, UnknownBackend};

/// The text `--help` prints.
pub const HELP: &str = "\
Usage: shingle [OPTION]... [--spawn PROGRAM [ARG]...]

Shingle, a tiling Wayland compositor compatible with Wayfire.

Options:
  --backend BACKEND  run on BACKEND: nested, native or headless
  --headless         same as --backend headless
  --config PATH      read configuration from PATH instead of
                     $XDG_CONFIG_HOME/shingle/shingle.yaml
  --ipc PATH         serve Wayfire IPC on PATH and export it to
                     clients as WAYFIRE_SOCKET
  --spawn PROGRAM [ARG]...
                     start PROGRAM as a client; every later argument
                     is passed to it
  --allow-builtin-seat
                     let the native backend run on libseat's builtin
                     or noop backend, which cannot switch VT
  -h, --help         print this help and exit
  --version          print the version and exit

Without --backend or --headless the backend is SHINGLE_BACKEND if set,
otherwise nested (a window in the host session) when WAYLAND_DISPLAY,
WAYLAND_SOCKET or DISPLAY is set, otherwise native (libseat and libinput
from a VT login; it draws offscreen until KMS output is built).
";

/// What the command line asks for.
#[derive(Debug, Eq, PartialEq)]
pub enum Command {
  /// Run the compositor.
  Run(Options),
  /// Print [`HELP`] and exit.
  Help,
  /// Print the version and exit.
  Version,
}

/// The options of a compositor run.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Options {
  /// The backend from `--backend` or `--headless`; `None` selects it from
  /// the environment.
  pub backend: Option<Backend>,
  /// `--config`: the configuration file.
  pub config: Option<PathBuf>,
  /// `--ipc`: where to serve Wayfire IPC.
  pub ipc: Option<PathBuf>,
  /// `--spawn`: a program and its arguments to start as a client; empty
  /// without `--spawn`.
  pub spawn: Vec<OsString>,
  /// `--allow-builtin-seat`: let the native backend use a libseat backend
  /// that cannot switch VT.
  pub allow_builtin_seat: bool,
}

/// A command line that cannot be parsed.
#[derive(Debug, Eq, PartialEq)]
pub enum UsageError {
  /// An option that takes a value came without one.
  MissingValue(&'static str),
  /// An option that may appear once appeared twice.
  Repeated(&'static str),
  /// `--backend` and `--headless` name different backends.
  ConflictingBackends(Backend, Backend),
  /// `--backend` names no backend.
  UnknownBackend(UnknownBackend),
  /// `--spawn` came without a program.
  MissingProgram,
  /// An option that does not exist.
  UnknownOption(String),
  /// An argument that is not an option.
  UnexpectedArgument(String),
}

impl fmt::Display for UsageError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::MissingValue(option) => write!(formatter, "{option} needs a value"),
      Self::Repeated(option) => write!(formatter, "{option} given twice"),
      Self::ConflictingBackends(first, second) => write!(
        formatter,
        "conflicting backends: {first} and {second} were both requested"
      ),
      Self::UnknownBackend(error) => write!(formatter, "--backend: {error}"),
      Self::MissingProgram => formatter.write_str("--spawn needs a program"),
      Self::UnknownOption(option) => {
        write!(formatter, "unknown option '{option}'")
      }
      Self::UnexpectedArgument(argument) => {
        write!(formatter, "unexpected argument '{argument}'")
      }
    }
  }
}

impl std::error::Error for UsageError {}

/// Parses the arguments after the program name.
///
/// Options that take a value accept it as the next argument or after `=`
/// (`--config=PATH`). `--spawn` ends option parsing: everything after it is
/// the program and its arguments. `--help` and `--version` win over
/// anything that follows them.
pub fn parse(
  arguments: impl IntoIterator<Item = OsString>,
) -> Result<Command, UsageError> {
  let mut options = Options::default();
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    let Some(text) = argument.to_str() else {
      return Err(UsageError::UnexpectedArgument(
        argument.to_string_lossy().into_owned(),
      ));
    };
    let (name, inline) = match text.split_once('=') {
      Some((name, value)) if name.starts_with("--") => {
        (name, Some(OsString::from(value)))
      }
      _ => (text, None),
    };
    let mut value = |option: &'static str| {
      inline
        .clone()
        .or_else(|| arguments.next())
        .filter(|value| !value.is_empty())
        .ok_or(UsageError::MissingValue(option))
    };
    match name {
      "-h" | "--help" if inline.is_none() => return Ok(Command::Help),
      "--version" if inline.is_none() => return Ok(Command::Version),
      "--headless" if inline.is_none() => {
        set_backend(&mut options.backend, Backend::Headless)?;
      }
      "--backend" => {
        let name = value("--backend")?;
        let backend = name
          .to_str()
          .ok_or_else(|| UnknownBackend(name.to_string_lossy().into_owned()))
          .and_then(str::parse)
          .map_err(UsageError::UnknownBackend)?;
        set_backend(&mut options.backend, backend)?;
      }
      "--allow-builtin-seat" if inline.is_none() => {
        options.allow_builtin_seat = true;
      }
      "--config" => set_once(&mut options.config, "--config", value)?,
      "--ipc" => set_once(&mut options.ipc, "--ipc", value)?,
      "--spawn" if inline.is_none() => {
        options.spawn.extend(arguments);
        if options.spawn.is_empty() {
          return Err(UsageError::MissingProgram);
        }
        break;
      }
      _ if text.starts_with('-') => {
        return Err(UsageError::UnknownOption(text.to_owned()));
      }
      _ => return Err(UsageError::UnexpectedArgument(text.to_owned())),
    }
  }
  Ok(Command::Run(options))
}

/// Records `backend`, which must agree with any backend already requested.
fn set_backend(
  current: &mut Option<Backend>,
  backend: Backend,
) -> Result<(), UsageError> {
  match *current {
    Some(previous) if previous != backend => {
      Err(UsageError::ConflictingBackends(previous, backend))
    }
    _ => {
      *current = Some(backend);
      Ok(())
    }
  }
}

/// Sets a path option that may be given once, from `value`.
fn set_once(
  slot: &mut Option<PathBuf>,
  option: &'static str,
  mut value: impl FnMut(&'static str) -> Result<OsString, UsageError>,
) -> Result<(), UsageError> {
  if slot.is_some() {
    return Err(UsageError::Repeated(option));
  }
  *slot = Some(PathBuf::from(value(option)?));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parsed(arguments: &[&str]) -> Result<Command, UsageError> {
    parse(arguments.iter().map(OsString::from))
  }

  fn options(arguments: &[&str]) -> Options {
    match parsed(arguments) {
      Ok(Command::Run(options)) => options,
      other => panic!("{arguments:?} parsed as {other:?}"),
    }
  }

  #[test]
  fn no_arguments_select_from_the_environment() {
    assert_eq!(options(&[]), Options::default());
  }

  #[test]
  fn all_options() {
    assert_eq!(
      options(&[
        "--backend",
        "nested",
        "--config",
        "data/shingle.yaml",
        "--ipc",
        "/run/ipc",
        "--spawn",
        "foot",
        "-e",
        "--config",
      ]),
      Options {
        backend: Some(Backend::Nested),
        config: Some("data/shingle.yaml".into()),
        ipc: Some("/run/ipc".into()),
        spawn: ["foot", "-e", "--config"].map(OsString::from).to_vec(),
        allow_builtin_seat: false,
      }
    );
  }

  #[test]
  fn values_after_equals() {
    assert_eq!(
      options(&["--backend=native", "--config=a=b.yaml", "--ipc=/x"]),
      Options {
        backend: Some(Backend::Native),
        config: Some("a=b.yaml".into()),
        ipc: Some("/x".into()),
        spawn: Vec::new(),
        allow_builtin_seat: false,
      }
    );
  }

  #[test]
  fn builtin_seat_is_allowed_by_its_option() {
    let options = options(&["--backend", "native", "--allow-builtin-seat"]);
    assert!(options.allow_builtin_seat);
    assert_eq!(
      parsed(&["--allow-builtin-seat=yes"]),
      Err(UsageError::UnknownOption("--allow-builtin-seat=yes".into()))
    );
  }

  #[test]
  fn headless_is_backend_headless() {
    assert_eq!(options(&["--headless"]).backend, Some(Backend::Headless));
    assert_eq!(
      options(&["--headless", "--backend", "headless"]).backend,
      Some(Backend::Headless)
    );
  }

  #[test]
  fn conflicting_backends_are_rejected() {
    assert_eq!(
      parsed(&["--headless", "--backend", "nested"]),
      Err(UsageError::ConflictingBackends(
        Backend::Headless,
        Backend::Nested
      ))
    );
    assert_eq!(
      parsed(&["--backend=native", "--backend=nested"]),
      Err(UsageError::ConflictingBackends(
        Backend::Native,
        Backend::Nested
      ))
    );
  }

  #[test]
  fn unknown_backend_is_rejected() {
    let error = parsed(&["--backend", "drm"]).unwrap_err();
    assert_eq!(
      error.to_string(),
      "--backend: unknown backend 'drm' (expected nested, native or headless)"
    );
  }

  #[test]
  fn missing_values_are_rejected() {
    for (arguments, option) in [
      (&["--backend"][..], "--backend"),
      (&["--config"][..], "--config"),
      (&["--ipc"][..], "--ipc"),
      (&["--config="][..], "--config"),
    ] {
      assert_eq!(
        parsed(arguments),
        Err(UsageError::MissingValue(option)),
        "{arguments:?}"
      );
    }
    assert_eq!(
      UsageError::MissingValue("--ipc").to_string(),
      "--ipc needs a value"
    );
  }

  #[test]
  fn repeated_paths_are_rejected() {
    assert_eq!(
      parsed(&["--config", "a", "--config=b"]),
      Err(UsageError::Repeated("--config"))
    );
    assert_eq!(
      parsed(&["--ipc", "a", "--ipc", "b"]),
      Err(UsageError::Repeated("--ipc"))
    );
  }

  #[test]
  fn spawn_needs_a_program() {
    assert_eq!(parsed(&["--spawn"]), Err(UsageError::MissingProgram));
  }

  #[test]
  fn unknown_options_and_arguments_are_rejected() {
    assert_eq!(
      parsed(&["--nested"]),
      Err(UsageError::UnknownOption("--nested".into()))
    );
    assert_eq!(
      parsed(&["--headless=yes"]),
      Err(UsageError::UnknownOption("--headless=yes".into()))
    );
    assert_eq!(
      parsed(&["foot"]),
      Err(UsageError::UnexpectedArgument("foot".into()))
    );
    assert_eq!(
      UsageError::UnknownOption("-x".into()).to_string(),
      "unknown option '-x'"
    );
  }

  #[test]
  fn help_and_version_win() {
    assert_eq!(parsed(&["-h"]), Ok(Command::Help));
    assert_eq!(
      parsed(&["--headless", "--help", "--bogus"]),
      Ok(Command::Help)
    );
    assert_eq!(parsed(&["--version"]), Ok(Command::Version));
  }

  #[test]
  fn help_after_spawn_belongs_to_the_program() {
    assert_eq!(
      options(&["--spawn", "foot", "--help"]).spawn,
      ["foot", "--help"].map(OsString::from).to_vec()
    );
  }

  #[test]
  fn non_utf8_paths_are_kept() {
    use std::os::unix::ffi::OsStringExt;
    let path = OsString::from_vec(vec![b'a', 0xff]);
    let command = parse([OsString::from("--config"), path.clone()]);
    let Ok(Command::Run(options)) = command else {
      panic!("{command:?}");
    };
    assert_eq!(options.config, Some(PathBuf::from(path)));
  }

  #[test]
  fn help_fits_in_80_columns() {
    assert!(HELP.lines().all(|line| line.len() <= 80));
  }
}
