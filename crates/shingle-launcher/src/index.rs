//! Application discovery: desktop entries from the XDG data directories and
//! commands on `$PATH`, and the cache of what was found.

use std::{
  collections::HashSet,
  ffi::OsString,
  fs,
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
  desktop::{
    DesktopEntry, ExecContext, locale_candidates, parse_exec, shell_join,
    shell_quote,
  },
  files::{absolute, write_atomically},
};

/// The version of the cache format; a cache of another version is ignored.
pub const CACHE_VERSION: u32 = 1;

/// Where an entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
  /// A desktop entry file.
  Application,
  /// An executable on `$PATH`.
  Command,
  /// A shortcut the user added in the launcher.
  Shortcut,
}

/// One thing the launcher can start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
  /// The desktop file ID (`firefox.desktop`), `path:NAME` for a command
  /// or `shortcut:SLUG` for a shortcut.
  pub id: String,
  /// Where it came from.
  pub kind: EntryKind,
  /// The name shown, before the user's renames.
  pub name: String,
  /// `GenericName`, or empty.
  pub generic_name: String,
  /// `Comment`, or empty.
  pub comment: String,
  /// `Keywords`.
  pub keywords: Vec<String>,
  /// The program's file name, matched at a lower weight.
  pub program: String,
  /// The `/bin/sh` command line that starts it.
  pub command: String,
  /// Whether it runs in a terminal.
  pub terminal: bool,
  /// Its default category's shown name; empty for commands on `$PATH`.
  pub category: String,
}

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
  /// [`CACHE_VERSION`] when read from a cache.
  pub version: u32,
  /// Desktop entries sorted by name, then commands sorted by name.
  pub entries: Vec<Entry>,
}

/// Where a scan looks, from the environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchPaths {
  /// The `applications` directories, most important first:
  /// `$XDG_DATA_HOME`'s, then each of `$XDG_DATA_DIRS`.
  pub application_dirs: Vec<PathBuf>,
  /// The directories of `$PATH`, in order.
  pub path_dirs: Vec<PathBuf>,
  /// The desktops of `$XDG_CURRENT_DESKTOP`, for `OnlyShowIn` and
  /// `NotShowIn`.
  pub desktops: Vec<String>,
  /// Locale keys to try, most specific first.
  pub locales: Vec<String>,
}

impl SearchPaths {
  /// The search paths of an environment looked up through `var`.
  pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Self {
    let home = var("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = absolute(var("XDG_DATA_HOME"))
      .unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = var("XDG_DATA_DIRS")
      .filter(|value| !value.is_empty())
      .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut application_dirs = vec![data_home.join("applications")];
    application_dirs.extend(
      std::env::split_paths(&data_dirs)
        .filter(|path| path.is_absolute())
        .map(|path| path.join("applications")),
    );
    let path_dirs = var("PATH")
      .map(|path| {
        std::env::split_paths(&path)
          .filter(|path| path.is_absolute())
          .collect()
      })
      .unwrap_or_default();
    let desktops = var("XDG_CURRENT_DESKTOP")
      .map(|value| {
        value
          .to_string_lossy()
          .split(':')
          .filter(|desktop| !desktop.is_empty())
          .map(str::to_owned)
          .collect()
      })
      .unwrap_or_default();
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
      .into_iter()
      .find_map(|name| var(name).filter(|value| !value.is_empty()))
      .map(|value| locale_candidates(&value.to_string_lossy()))
      .unwrap_or_default();
    Self {
      application_dirs,
      path_dirs,
      desktops,
      locales: locale,
    }
  }
}

/// Scans `paths`: every desktop entry, then every command on `$PATH` that
/// no desktop entry's program already names.
pub fn scan(paths: &SearchPaths) -> Index {
  let mut entries = scan_applications(paths);
  entries.sort_by(by_name);
  let programs = entries
    .iter()
    .map(|entry| entry.program.clone())
    .collect::<HashSet<_>>();
  let mut commands = scan_commands(&paths.path_dirs)
    .into_iter()
    .filter(|entry| !programs.contains(&entry.program))
    .collect::<Vec<_>>();
  commands.sort_by(by_name);
  entries.extend(commands);
  Index {
    version: CACHE_VERSION,
    entries,
  }
}

fn by_name(a: &Entry, b: &Entry) -> std::cmp::Ordering {
  a.name
    .to_lowercase()
    .cmp(&b.name.to_lowercase())
    .then_with(|| a.id.cmp(&b.id))
}

/// The desktop entries of every applications directory, the first
/// directory winning for each desktop file ID.
fn scan_applications(paths: &SearchPaths) -> Vec<Entry> {
  let mut seen = HashSet::new();
  let mut entries = Vec::new();
  for directory in &paths.application_dirs {
    let mut files = Vec::new();
    desktop_files(directory, directory, &mut files);
    files.sort();
    for (id, path) in files {
      if !seen.insert(id.clone()) {
        continue;
      }
      let Ok(text) = fs::read_to_string(&path) else {
        continue;
      };
      if let Some(entry) = application(&id, &path, &text, paths) {
        entries.push(entry);
      }
    }
  }
  entries
}

/// Every `*.desktop` file below `directory`, with its desktop file ID (the
/// path below `root` with `/` replaced by `-`).
fn desktop_files(
  root: &Path,
  directory: &Path,
  files: &mut Vec<(String, PathBuf)>,
) {
  let Ok(read) = fs::read_dir(directory) else {
    return;
  };
  for item in read.flatten() {
    let path = item.path();
    let Ok(kind) = item.file_type() else {
      continue;
    };
    let is_dir = kind.is_dir() || (kind.is_symlink() && path.is_dir());
    if is_dir {
      desktop_files(root, &path, files);
    } else if path
      .extension()
      .is_some_and(|extension| extension == "desktop")
      && let Ok(relative) = path.strip_prefix(root)
    {
      let id = relative.to_string_lossy().replace('/', "-");
      files.push((id, path));
    }
  }
}

/// The entry of the desktop file `id` at `path` with contents `text`, or
/// `None` when the specification says not to show it.
pub fn application(
  id: &str,
  path: &Path,
  text: &str,
  paths: &SearchPaths,
) -> Option<Entry> {
  let desktop = DesktopEntry::parse(text);
  if desktop.raw("Type") != Some("Application")
    || desktop.boolean("NoDisplay")
    || desktop.boolean("Hidden")
  {
    return None;
  }
  let shown_in = |key| {
    desktop
      .list(key, &[])
      .iter()
      .any(|desktop| paths.desktops.contains(desktop))
  };
  if desktop.raw("OnlyShowIn").is_some() && !shown_in("OnlyShowIn") {
    return None;
  }
  if shown_in("NotShowIn") {
    return None;
  }
  if let Some(try_exec) = desktop.string("TryExec")
    && !try_exec.is_empty()
    && find_executable(&try_exec, &paths.path_dirs).is_none()
  {
    return None;
  }
  let name = desktop.localized("Name", &paths.locales)?;
  let icon = desktop.string("Icon");
  let path_text = path.to_string_lossy();
  let arguments = parse_exec(
    &desktop.string("Exec")?,
    &ExecContext {
      name: &name,
      path: &path_text,
      icon: icon.as_deref(),
    },
  )?;
  let mut command = format!("exec {}", shell_join(&arguments));
  if let Some(directory) =
    desktop.string("Path").filter(|path| !path.is_empty())
  {
    command = format!("cd {} && {command}", shell_quote(&directory));
  }
  Some(Entry {
    id: id.to_owned(),
    kind: EntryKind::Application,
    generic_name: desktop
      .localized("GenericName", &paths.locales)
      .unwrap_or_default(),
    comment: desktop
      .localized("Comment", &paths.locales)
      .unwrap_or_default(),
    keywords: desktop.list("Keywords", &paths.locales),
    program: program_name(&arguments),
    command,
    terminal: desktop.boolean("Terminal"),
    category: main_category(&desktop.list("Categories", &[])).to_owned(),
    name,
  })
}

/// The file name of the program `arguments` run, looking past `env` and
/// its variable assignments.
fn program_name(arguments: &[String]) -> String {
  let base = |argument: &String| {
    Path::new(argument)
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
      .unwrap_or_default()
  };
  let mut rest = arguments.iter();
  let first = rest.next().map(base).unwrap_or_default();
  if first != "env" {
    return first;
  }
  while let Some(argument) = rest.next() {
    if argument == "-u" || argument == "--unset" {
      rest.next();
    } else if !argument.contains('=') && !argument.starts_with('-') {
      return base(argument);
    }
  }
  first
}

/// `name` as an executable: itself when it is a path, else the first match
/// in `path_dirs`.
pub fn find_executable(name: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
  if name.contains('/') {
    let path = PathBuf::from(name);
    return is_executable(&path).then_some(path);
  }
  path_dirs
    .iter()
    .map(|directory| directory.join(name))
    .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
  fs::metadata(path).is_ok_and(|metadata| {
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
  })
}

/// Every executable file of `path_dirs` by name, the first directory
/// winning.
fn scan_commands(path_dirs: &[PathBuf]) -> Vec<Entry> {
  let mut seen = HashSet::new();
  let mut entries = Vec::new();
  for directory in path_dirs {
    let Ok(read) = fs::read_dir(directory) else {
      continue;
    };
    for item in read.flatten() {
      let Some(name) = item.file_name().to_str().map(str::to_owned) else {
        continue;
      };
      if name.starts_with('.') || seen.contains(&name) {
        continue;
      }
      if !is_executable(&item.path()) {
        continue;
      }
      seen.insert(name.clone());
      entries.push(Entry {
        id: format!("path:{name}"),
        kind: EntryKind::Command,
        command: shell_quote(&name),
        program: name.clone(),
        name,
        generic_name: String::new(),
        comment: String::new(),
        keywords: Vec::new(),
        terminal: false,
        category: String::new(),
      });
    }
  }
  entries
}

/// The shown name of the first freedesktop main category in `categories`,
/// or `Other`.
pub fn main_category(categories: &[String]) -> &'static str {
  categories
    .iter()
    .find_map(|category| {
      Some(match category.as_str() {
        "AudioVideo" | "Audio" | "Video" => "Multimedia",
        "Development" => "Development",
        "Education" => "Education",
        "Game" => "Games",
        "Graphics" => "Graphics",
        "Network" => "Internet",
        "Office" => "Office",
        "Science" => "Science",
        "Settings" => "Settings",
        "System" => "System",
        "Utility" => "Accessories",
        _ => return None,
      })
    })
    .unwrap_or("Other")
}

/// The cached index at `path`, or `None` when it is missing, unreadable or
/// of another [`CACHE_VERSION`].
pub fn load_cache(path: &Path) -> Option<Index> {
  let bytes = fs::read(path).ok()?;
  let index: Index = serde_json::from_slice(&bytes).ok()?;
  (index.version == CACHE_VERSION).then_some(index)
}

/// Writes `index` to the cache at `path`.
pub fn save_cache(path: &Path, index: &Index) -> std::io::Result<()> {
  let bytes = serde_json::to_vec(index).map_err(std::io::Error::other)?;
  write_atomically(path, &bytes)
}

#[cfg(test)]
mod tests {
  use std::os::unix::fs::PermissionsExt;

  use super::*;

  fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
  }

  fn executable(path: &Path) {
    write(path, "#!/bin/sh\n");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
  }

  fn app(name: &str, exec: &str, extra: &str) -> String {
    format!(
      "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n{extra}"
    )
  }

  struct Fixture {
    _directory: tempfile::TempDir,
    paths: SearchPaths,
    user: PathBuf,
    system: PathBuf,
    bin: PathBuf,
  }

  fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_owned();
    let paths = SearchPaths::from_env(|name| match name {
      "HOME" => Some(root.join("home").into()),
      "XDG_DATA_DIRS" => Some(root.join("system").into()),
      "PATH" => Some(root.join("bin").into()),
      "XDG_CURRENT_DESKTOP" => Some("Shingle:wayfire".into()),
      _ => None,
    });
    Fixture {
      user: root.join("home/.local/share/applications"),
      system: root.join("system/applications"),
      bin: root.join("bin"),
      paths,
      _directory: directory,
    }
  }

  #[test]
  fn search_paths_follow_the_environment() {
    let paths = SearchPaths::from_env(|name| match name {
      "HOME" => Some("/home/u".into()),
      "XDG_DATA_DIRS" => Some("/a:relative:/b".into()),
      "PATH" => Some("/usr/bin:bin:/bin".into()),
      "LANG" => Some("de_DE.UTF-8".into()),
      "LC_MESSAGES" => Some("fr_FR.UTF-8".into()),
      _ => None,
    });
    assert_eq!(
      paths.application_dirs,
      [
        PathBuf::from("/home/u/.local/share/applications"),
        PathBuf::from("/a/applications"),
        PathBuf::from("/b/applications"),
      ]
    );
    assert_eq!(
      paths.path_dirs,
      [PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
    );
    assert_eq!(paths.locales, ["fr_FR", "fr"]);
    let defaults = SearchPaths::from_env(|_| None);
    assert_eq!(
      defaults.application_dirs[1..],
      [
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications")
      ]
    );
  }

  #[test]
  fn the_user_directory_overrides_and_hides_system_entries() {
    let fixture = fixture();
    write(
      &fixture.system.join("editor.desktop"),
      &app(
        "System Editor",
        "editor %F",
        "Categories=Utility;TextEditor;",
      ),
    );
    write(
      &fixture.user.join("editor.desktop"),
      &app("My Editor", "editor --mine", "Categories=Development;"),
    );
    write(
      &fixture.system.join("gone.desktop"),
      &app("Gone", "gone", ""),
    );
    write(
      &fixture.user.join("gone.desktop"),
      &app("Gone", "gone", "Hidden=true"),
    );
    write(
      &fixture.system.join("kde/konsole.desktop"),
      &app("Konsole", "konsole", "Terminal=false\nCategories=System;"),
    );
    let index = scan(&fixture.paths);
    let ids = index
      .entries
      .iter()
      .map(|entry| (entry.id.as_str(), entry.name.as_str()))
      .collect::<Vec<_>>();
    assert_eq!(
      ids,
      [
        ("kde-konsole.desktop", "Konsole"),
        ("editor.desktop", "My Editor")
      ]
    );
    let editor = &index.entries[1];
    assert_eq!(editor.command, "exec editor --mine");
    assert_eq!(editor.category, "Development");
    assert_eq!(index.entries[0].category, "System");
  }

  #[test]
  fn entries_are_filtered_as_the_specification_says() {
    let fixture = fixture();
    executable(&fixture.bin.join("present"));
    let cases = [
      (
        "link.desktop",
        "[Desktop Entry]\nType=Link\nName=L\nURL=x\n",
      ),
      (
        "nodisplay.desktop",
        &app("N", "n", "NoDisplay=true") as &str,
      ),
      ("gnome.desktop", &app("G", "g", "OnlyShowIn=GNOME;")),
      (
        "shingle.desktop",
        &app("S", "s", "OnlyShowIn=GNOME;Shingle;"),
      ),
      ("notwayfire.desktop", &app("W", "w", "NotShowIn=wayfire;")),
      ("missing.desktop", &app("M", "m", "TryExec=absent")),
      ("tried.desktop", &app("T", "t", "TryExec=present")),
      (
        "noexec.desktop",
        "[Desktop Entry]\nType=Application\nName=X\n",
      ),
    ];
    for (file, text) in cases {
      write(&fixture.system.join(file), text);
    }
    let index = scan(&fixture.paths);
    let ids = index
      .entries
      .iter()
      .filter(|entry| entry.kind == EntryKind::Application)
      .map(|entry| entry.id.as_str())
      .collect::<Vec<_>>();
    assert_eq!(ids, ["shingle.desktop", "tried.desktop"]);
  }

  #[test]
  fn commands_on_path_skip_programs_of_desktop_entries() {
    let fixture = fixture();
    executable(&fixture.bin.join("firefox"));
    executable(&fixture.bin.join("htop"));
    write(&fixture.bin.join("readme"), "not executable");
    write(
      &fixture.system.join("firefox.desktop"),
      &app("Firefox", "/usr/lib/firefox/firefox %u", "Path=/tmp/a b"),
    );
    write(
      &fixture.system.join("env.desktop"),
      &app("With Env", "env A=1 -u B htop", ""),
    );
    let index = scan(&fixture.paths);
    let firefox = &index.entries[0];
    assert_eq!(firefox.program, "firefox");
    assert_eq!(
      firefox.command,
      "cd '/tmp/a b' && exec /usr/lib/firefox/firefox"
    );
    assert_eq!(index.entries[1].program, "htop");
    assert_eq!(index.entries.len(), 2, "{:?}", index.entries);
  }

  #[test]
  fn commands_are_listed_once_with_the_first_directory_winning() {
    let fixture = fixture();
    let second = fixture.bin.parent().unwrap().join("bin2");
    executable(&fixture.bin.join("tool"));
    executable(&second.join("tool"));
    executable(&second.join("other tool"));
    let paths = SearchPaths {
      path_dirs: vec![fixture.bin.clone(), second],
      ..fixture.paths.clone()
    };
    let index = scan(&paths);
    let commands = index
      .entries
      .iter()
      .map(|entry| (entry.id.as_str(), entry.command.as_str()))
      .collect::<Vec<_>>();
    assert_eq!(
      commands,
      [("path:other tool", "'other tool'"), ("path:tool", "tool")]
    );
  }

  #[test]
  fn main_categories_have_readable_names() {
    let list =
      |text: &str| text.split(';').map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(main_category(&list("GTK;Network;WebBrowser")), "Internet");
    assert_eq!(main_category(&list("Audio;Player")), "Multimedia");
    assert_eq!(main_category(&list("Qt;KDE")), "Other");
    assert_eq!(main_category(&[]), "Other");
  }

  #[test]
  fn the_cache_round_trips_and_rejects_other_versions() {
    let fixture = fixture();
    executable(&fixture.bin.join("tool"));
    let index = scan(&fixture.paths);
    let path = fixture.bin.parent().unwrap().join("cache/index.json");
    save_cache(&path, &index).unwrap();
    assert_eq!(load_cache(&path), Some(index.clone()));
    let mut old = index;
    old.version = CACHE_VERSION + 1;
    save_cache(&path, &old).unwrap();
    assert_eq!(load_cache(&path), None);
    fs::write(&path, b"{not json").unwrap();
    assert_eq!(load_cache(&path), None);
  }
}
