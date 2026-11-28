//! Where the launcher's files live, and writing them safely.

use std::{
  ffi::OsString,
  fs,
  io::{self, Write},
  path::{Path, PathBuf},
};

/// The launcher's files, from the XDG base directory variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
  /// The user's layout: `$XDG_DATA_HOME/shingle/launcher.yaml`.
  pub layout: PathBuf,
  /// Launch counts and times: `$XDG_STATE_HOME/shingle/launcher-history.json`.
  pub history: PathBuf,
  /// The scanned index: `$XDG_CACHE_HOME/shingle/launcher-index.json`.
  pub index_cache: PathBuf,
  /// Decoded icons: `$XDG_CACHE_HOME/shingle/launcher-icons`.
  pub icon_cache: PathBuf,
  /// dconf's user database, whose time says whether GNOME's settings
  /// changed: `$XDG_CONFIG_HOME/dconf/user`.
  pub dconf_database: PathBuf,
  /// The directory of the single-instance socket, `$XDG_RUNTIME_DIR`.
  pub runtime: Option<PathBuf>,
}

impl Paths {
  /// The paths for an environment looked up through `var`, with the XDG
  /// defaults below `$HOME` for unset or relative variables.
  pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Self {
    let home = var("HOME").map(PathBuf::from).unwrap_or_default();
    let base = |name: &str, fallback: &str| {
      absolute(var(name)).unwrap_or_else(|| home.join(fallback))
    };
    let shingle =
      |name: &str, fallback: &str| base(name, fallback).join("shingle");
    Self {
      layout: shingle("XDG_DATA_HOME", ".local/share").join("launcher.yaml"),
      history: shingle("XDG_STATE_HOME", ".local/state")
        .join("launcher-history.json"),
      index_cache: shingle("XDG_CACHE_HOME", ".cache")
        .join("launcher-index.json"),
      icon_cache: shingle("XDG_CACHE_HOME", ".cache").join("launcher-icons"),
      dconf_database: base("XDG_CONFIG_HOME", ".config").join("dconf/user"),
      runtime: absolute(var("XDG_RUNTIME_DIR")),
    }
  }
}

/// `value` as a path if it is set and absolute, as the XDG specification
/// requires of its variables.
pub fn absolute(value: Option<OsString>) -> Option<PathBuf> {
  value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// Writes `contents` to `path` so that a reader sees either the old file or
/// the new one: into `PATH.tmp` beside it, synced, then renamed over it.
/// Creates the parent directory.
pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  let mut temporary = path.as_os_str().to_owned();
  temporary.push(".tmp");
  let temporary = PathBuf::from(temporary);
  let mut file = fs::File::create(&temporary)?;
  file.write_all(contents)?;
  file.sync_all()?;
  drop(file);
  fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn paths_follow_the_xdg_variables() {
    let paths = Paths::from_env(|name| match name {
      "HOME" => Some("/home/u".into()),
      "XDG_DATA_HOME" => Some("/data".into()),
      "XDG_STATE_HOME" => Some("relative/is/ignored".into()),
      "XDG_RUNTIME_DIR" => Some("/run/user/7".into()),
      _ => None,
    });
    assert_eq!(paths.layout, Path::new("/data/shingle/launcher.yaml"));
    assert_eq!(
      paths.history,
      Path::new("/home/u/.local/state/shingle/launcher-history.json")
    );
    assert_eq!(
      paths.index_cache,
      Path::new("/home/u/.cache/shingle/launcher-index.json")
    );
    assert_eq!(
      paths.icon_cache,
      Path::new("/home/u/.cache/shingle/launcher-icons")
    );
    assert_eq!(
      paths.dconf_database,
      Path::new("/home/u/.config/dconf/user")
    );
    assert_eq!(paths.runtime.as_deref(), Some(Path::new("/run/user/7")));
  }

  #[test]
  fn atomic_writes_replace_the_file_and_leave_no_temporary() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a/b/file.yaml");
    write_atomically(&path, b"one").unwrap();
    write_atomically(&path, b"two").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"two");
    assert!(!directory.path().join("a/b/file.yaml.tmp").exists());
  }
}
