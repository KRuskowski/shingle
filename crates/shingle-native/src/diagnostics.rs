//! Diagnostics of the native backend.
//!
//! Messages go to standard error and, once [`open_log`] has run, to
//! `$XDG_STATE_HOME/shingle/native.log`, because standard error cannot be
//! seen once the seat has put the VT into graphics mode. A panic hook
//! writes panics there too.

use std::{
  ffi::OsString,
  fs::{File, OpenOptions},
  io::{self, Write},
  path::{Path, PathBuf},
  sync::Mutex,
};

static LOG: Mutex<Option<File>> = Mutex::new(None);

/// The native log file: `native.log` under `XDG_STATE_HOME/shingle`, or
/// under `HOME/.local/state/shingle` without it. `env` looks up variables
/// by name; empty values count as unset.
pub fn log_path(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
  let set = |name: &str| env(name).filter(|value| !value.is_empty());
  let state = set("XDG_STATE_HOME").map(PathBuf::from).or_else(|| {
    set("HOME").map(|home| Path::new(&home).join(".local/state"))
  })?;
  Some(state.join("shingle/native.log"))
}

/// Appends to the log at `path` from now on, creating it and its
/// directory, and installs a panic hook that logs panics before the
/// default hook runs.
pub fn open_log(path: &Path) -> io::Result<()> {
  if let Some(directory) = path.parent() {
    std::fs::create_dir_all(directory)?;
  }
  let file = OpenOptions::new().create(true).append(true).open(path)?;
  if let Ok(mut log) = LOG.lock() {
    *log = Some(file);
  }
  let default = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |info| {
    write_file(&format!("shingle: {info}"));
    default(info);
  }));
  Ok(())
}

/// Reports `message` on standard error and in the log, as one line
/// prefixed with `shingle: `.
pub fn log(message: &str) {
  let line = format!("shingle: {message}");
  eprintln!("{line}");
  write_file(&line);
}

fn write_file(line: &str) {
  if let Ok(mut log) = LOG.lock()
    && let Some(file) = log.as_mut()
  {
    let _ = writeln!(file, "{line}");
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_log_lives_in_the_state_directory() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
      move |name: &str| {
        pairs
          .iter()
          .find(|(key, _)| *key == name)
          .map(|(_, value)| OsString::from(value))
      }
    };
    assert_eq!(
      log_path(env(&[("XDG_STATE_HOME", "/s"), ("HOME", "/h")])),
      Some(PathBuf::from("/s/shingle/native.log"))
    );
    assert_eq!(
      log_path(env(&[("XDG_STATE_HOME", ""), ("HOME", "/h")])),
      Some(PathBuf::from("/h/.local/state/shingle/native.log"))
    );
    assert_eq!(log_path(env(&[])), None);
  }
}
