//! Finding an entry's icon file: the freedesktop Icon Theme Specification
//! 0.13's lookup through a theme, its parents, `hicolor` and the pixmaps
//! directories, and which theme the desktop chose.

use std::{
  collections::{HashMap, HashSet},
  ffi::OsString,
  fs,
  path::{Path, PathBuf},
  process::{Command, Stdio},
};

use crate::files::absolute;

/// The theme every lookup falls back to.
pub const FALLBACK_THEME: &str = "hicolor";
/// The file types searched, in the specification's order.
const EXTENSIONS: [&str; 3] = ["png", "svg", "xpm"];

/// How a theme directory's icons fit sizes (`Type` in `index.theme`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeKind {
  Fixed,
  Scalable,
  Threshold,
}

/// One directory of a theme, as its `index.theme` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ThemeDirectory {
  path: String,
  size: u32,
  scale: u32,
  kind: SizeKind,
  min_size: u32,
  max_size: u32,
  threshold: u32,
}

impl ThemeDirectory {
  /// `DirectoryMatchesSize` of the specification.
  fn matches(&self, size: u32, scale: u32) -> bool {
    if self.scale != scale {
      return false;
    }
    match self.kind {
      SizeKind::Fixed => self.size == size,
      SizeKind::Scalable => (self.min_size..=self.max_size).contains(&size),
      SizeKind::Threshold => {
        self.size.saturating_sub(self.threshold) <= size
          && size <= self.size + self.threshold
      }
    }
  }

  /// `DirectorySizeDistance` of the specification, in device pixels.
  fn distance(&self, size: u32, scale: u32) -> u32 {
    let wanted = size * scale;
    let (low, high) = match self.kind {
      SizeKind::Fixed => {
        return (self.size * self.scale).abs_diff(wanted);
      }
      SizeKind::Scalable => (self.min_size, self.max_size),
      SizeKind::Threshold => (
        self.size.saturating_sub(self.threshold),
        self.size + self.threshold,
      ),
    };
    // Below the range, the distance to its low end; above, to its high
    // end; inside, none.
    (low * self.scale).saturating_sub(wanted)
      + wanted.saturating_sub(high * self.scale)
  }
}

/// A theme's `index.theme`: its parents and directories.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Theme {
  inherits: Vec<String>,
  directories: Vec<ThemeDirectory>,
}

impl Theme {
  /// Parses the text of an `index.theme`.
  fn parse(text: &str) -> Self {
    let mut groups: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut group = String::new();
    for line in text.lines() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      if let Some(name) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
      {
        group = name.to_owned();
        continue;
      }
      if let Some((key, value)) = line.split_once('=') {
        groups
          .entry(group.clone())
          .or_default()
          .entry(key.trim().to_owned())
          .or_insert_with(|| value.trim().to_owned());
      }
    }
    let list = |value: Option<&String>| {
      value
        .map(|value| {
          value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    };
    let main = groups.get("Icon Theme").cloned().unwrap_or_default();
    let mut names = list(main.get("Directories"));
    names.extend(list(main.get("ScaledDirectories")));
    let mut seen = HashSet::new();
    let directories = names
      .into_iter()
      .filter(|name| seen.insert(name.clone()))
      .filter_map(|name| {
        let keys = groups.get(&name)?;
        let number = |key: &str| keys.get(key)?.parse::<u32>().ok();
        let size = number("Size")?;
        let kind = match keys.get("Type").map(String::as_str) {
          Some("Fixed") => SizeKind::Fixed,
          Some("Scalable") => SizeKind::Scalable,
          _ => SizeKind::Threshold,
        };
        Some(ThemeDirectory {
          path: name,
          size,
          scale: number("Scale").unwrap_or(1).max(1),
          kind,
          min_size: number("MinSize").unwrap_or(size),
          max_size: number("MaxSize").unwrap_or(size),
          threshold: number("Threshold").unwrap_or(2),
        })
      })
      .collect();
    Self {
      inherits: list(main.get("Inherits")),
      directories,
    }
  }
}

/// Where icons are looked for, from the environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IconPaths {
  /// The base directories of themes, most important first:
  /// `$HOME/.icons`, `$XDG_DATA_HOME/icons`, each `$XDG_DATA_DIRS/icons`.
  pub base_dirs: Vec<PathBuf>,
  /// The directories of unthemed icons: each `$XDG_DATA_DIRS/pixmaps`,
  /// then `/usr/share/pixmaps`.
  pub pixmap_dirs: Vec<PathBuf>,
}

impl IconPaths {
  /// The paths of an environment looked up through `var`.
  pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Self {
    let home = var("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = absolute(var("XDG_DATA_HOME"))
      .unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = var("XDG_DATA_DIRS")
      .filter(|value| !value.is_empty())
      .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let data_dirs = std::env::split_paths(&data_dirs)
      .filter(|path| path.is_absolute())
      .collect::<Vec<_>>();
    let mut base_dirs = Vec::new();
    if home.is_absolute() {
      base_dirs.push(home.join(".icons"));
    }
    base_dirs.push(data_home.join("icons"));
    base_dirs.extend(data_dirs.iter().map(|path| path.join("icons")));
    let mut pixmap_dirs = data_dirs
      .iter()
      .map(|path| path.join("pixmaps"))
      .collect::<Vec<_>>();
    let system = PathBuf::from("/usr/share/pixmaps");
    if !pixmap_dirs.contains(&system) {
      pixmap_dirs.push(system);
    }
    Self {
      base_dirs,
      pixmap_dirs,
    }
  }
}

/// Looks icons up in one theme, caching what it reads: themes, and the
/// file names of each directory, so that a lookup costs no `stat` calls
/// once the directories were listed.
pub struct IconLookup {
  paths: IconPaths,
  theme: String,
  themes: HashMap<String, Option<Theme>>,
  listings: HashMap<PathBuf, Option<HashSet<OsString>>>,
}

impl IconLookup {
  /// A lookup in `theme` (falling back to `hicolor`) under `paths`.
  pub fn new(paths: IconPaths, theme: &str) -> Self {
    Self {
      paths,
      theme: theme.to_owned(),
      themes: HashMap::new(),
      listings: HashMap::new(),
    }
  }

  /// The theme looked in first.
  pub fn theme(&self) -> &str {
    &self.theme
  }

  /// The file of the icon `icon` (an `Icon` value) closest to `size`
  /// logical pixels at integer `scale`, or `None`. An absolute path is
  /// taken as it is; a name with a `.png`, `.svg` or `.xpm` extension is
  /// looked up without it, as many desktop files expect.
  pub fn find(&mut self, icon: &str, size: u32, scale: u32) -> Option<PathBuf> {
    if icon.is_empty() {
      return None;
    }
    let path = Path::new(icon);
    if path.is_absolute() {
      return path.is_file().then(|| path.to_owned());
    }
    if icon.contains('/') {
      return None;
    }
    let name = EXTENSIONS
      .iter()
      .find_map(|extension| icon.strip_suffix(&format!(".{extension}")))
      .unwrap_or(icon);
    let scale = scale.max(1);
    let mut visited = HashSet::new();
    let theme = self.theme.clone();
    self
      .find_in(name, size, scale, &theme, &mut visited)
      .or_else(|| self.find_in(name, size, scale, FALLBACK_THEME, &mut visited))
      .or_else(|| self.fallback(name))
  }

  /// `FindIconHelper`: the theme, then its parents, depth first, each
  /// theme once.
  fn find_in(
    &mut self,
    name: &str,
    size: u32,
    scale: u32,
    theme: &str,
    visited: &mut HashSet<String>,
  ) -> Option<PathBuf> {
    if !visited.insert(theme.to_owned()) {
      return None;
    }
    let parsed = self.load_theme(theme)?;
    if let Some(found) = self.lookup(name, size, scale, theme, &parsed) {
      return Some(found);
    }
    for parent in &parsed.inherits {
      if let Some(found) = self.find_in(name, size, scale, parent, visited) {
        return Some(found);
      }
    }
    None
  }

  /// `LookupIcon`: a directory matching the size, else the closest.
  fn lookup(
    &mut self,
    name: &str,
    size: u32,
    scale: u32,
    theme_name: &str,
    theme: &Theme,
  ) -> Option<PathBuf> {
    let mut closest: Option<(u32, PathBuf)> = None;
    for directory in &theme.directories {
      let matches = directory.matches(size, scale);
      let distance = directory.distance(size, scale);
      if !matches && closest.as_ref().is_some_and(|(best, _)| distance >= *best)
      {
        continue;
      }
      let Some(found) = self.file_in(theme_name, &directory.path, name) else {
        continue;
      };
      if matches {
        return Some(found);
      }
      closest = Some((distance, found));
    }
    // A directory that matches anywhere beats the closest; the loop above
    // returns the first one in order, as the specification's first pass.
    closest.map(|(_, path)| path)
  }

  /// `NAME.png`, `NAME.svg` or `NAME.xpm` in `subdirectory` of `theme`
  /// under any base
  /// directory, the first base directory winning.
  fn file_in(
    &mut self,
    theme: &str,
    subdirectory: &str,
    name: &str,
  ) -> Option<PathBuf> {
    let bases = self.paths.base_dirs.clone();
    for base in bases {
      let directory = base.join(theme).join(subdirectory);
      if let Some(found) = self.file_named(&directory, name) {
        return Some(found);
      }
    }
    None
  }

  /// `NAME.EXTENSION` in `directory` for the first extension present.
  fn file_named(&mut self, directory: &Path, name: &str) -> Option<PathBuf> {
    let listing = self
      .listings
      .entry(directory.to_owned())
      .or_insert_with(|| {
        fs::read_dir(directory)
          .ok()
          .map(|read| read.flatten().map(|item| item.file_name()).collect())
      })
      .as_ref()?;
    EXTENSIONS.iter().find_map(|extension| {
      let file = OsString::from(format!("{name}.{extension}"));
      listing.contains(&file).then(|| directory.join(file))
    })
  }

  /// `LookupFallbackIcon`: the pixmaps directories.
  fn fallback(&mut self, name: &str) -> Option<PathBuf> {
    let directories = self.paths.pixmap_dirs.clone();
    directories
      .iter()
      .find_map(|directory| self.file_named(directory, name))
  }

  /// The theme `name` from the first base directory that has its
  /// `index.theme`.
  fn load_theme(&mut self, name: &str) -> Option<Theme> {
    if let Some(theme) = self.themes.get(name) {
      return theme.clone();
    }
    let theme = self.paths.base_dirs.iter().find_map(|base| {
      let text =
        fs::read_to_string(base.join(name).join("index.theme")).ok()?;
      Some(Theme::parse(&text))
    });
    self.themes.insert(name.to_owned(), theme.clone());
    theme
  }
}

/// The icon theme to use: `configured` when it is not empty, else the
/// desktop's setting, else `hicolor`. Under KDE (by
/// `$XDG_CURRENT_DESKTOP`) `kdeglobals`' `[Icons] Theme` comes before
/// GTK's `gtk-icon-theme-name` (GTK 4's `settings.ini`, then GTK 3's);
/// elsewhere GTK's comes first. GNOME's own setting, which `gnome` reads
/// (`org.gnome.desktop.interface icon-theme`, see [`gnome_theme`]), comes
/// first under GNOME, where GTK takes it over `settings.ini`, and after
/// the files elsewhere. `read` reads a file under `$XDG_CONFIG_HOME`
/// (default `~/.config`).
pub fn theme_name(
  configured: &str,
  var: impl Fn(&str) -> Option<OsString>,
  read: impl Fn(&Path) -> Option<String>,
  gnome: impl FnOnce() -> Option<String>,
) -> String {
  let configured = configured.trim();
  if !configured.is_empty() {
    return configured.to_owned();
  }
  let home = var("HOME").map(PathBuf::from).unwrap_or_default();
  let config =
    absolute(var("XDG_CONFIG_HOME")).unwrap_or_else(|| home.join(".config"));
  let desktop = |name: &str| {
    var("XDG_CURRENT_DESKTOP").is_some_and(|value| {
      value
        .to_string_lossy()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case(name))
    })
  };
  let kde_first = desktop("KDE");
  let gnome_first = !kde_first && desktop("GNOME");
  let kde = || {
    read(&config.join("kdeglobals"))
      .and_then(|text| ini_value(&text, "Icons", "Theme"))
  };
  let gtk = || {
    ["gtk-4.0", "gtk-3.0"].into_iter().find_map(|version| {
      read(&config.join(version).join("settings.ini"))
        .and_then(|text| ini_value(&text, "Settings", "gtk-icon-theme-name"))
    })
  };
  let gnome = || gnome().filter(|theme| !theme.trim().is_empty());
  let found = if kde_first {
    kde().or_else(gtk).or_else(gnome)
  } else if gnome_first {
    gnome().or_else(gtk).or_else(kde)
  } else {
    gtk().or_else(kde).or_else(gnome)
  };
  found.unwrap_or_else(|| FALLBACK_THEME.to_owned())
}

/// GNOME's icon theme setting through the `gsettings` binary, cached in
/// the file `cache` while the dconf database `database` keeps its time,
/// so that a start runs `gsettings` only after the setting may have
/// changed. Outside GNOME (`!gnome`) a missing database means GNOME's
/// settings were never written, and the schema's default (Adwaita) is not
/// taken for a choice. `run` runs `gsettings` ([`run_gsettings`]).
pub fn gnome_theme(
  gnome: bool,
  database: &Path,
  cache: &Path,
  run: impl FnOnce() -> Option<String>,
) -> Option<String> {
  let stamp = fs::metadata(database)
    .and_then(|metadata| metadata.modified())
    .ok()
    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    .map(|duration| duration.as_nanos());
  if stamp.is_none() && !gnome {
    return None;
  }
  let stamp = stamp.unwrap_or_default().to_string();
  if let Some(cached) = fs::read_to_string(cache)
    .ok()
    .and_then(|text| cached_theme(&text, &stamp))
  {
    return cached;
  }
  let theme = run().as_deref().and_then(parse_gsettings);
  let text = format!("{stamp}\n{}\n", theme.as_deref().unwrap_or(""));
  let _ = crate::files::write_atomically(cache, text.as_bytes());
  theme
}

/// The theme a [`gnome_theme`] cache file holds for the database time
/// `stamp`: `Some(None)` when it recorded none, `None` when it is of
/// another time.
fn cached_theme(text: &str, stamp: &str) -> Option<Option<String>> {
  let (time, theme) = text.split_once('\n')?;
  (time == stamp).then(|| {
    let theme = theme.trim();
    (!theme.is_empty()).then(|| theme.to_owned())
  })
}

/// The string `gsettings get` printed, in GVariant text form: `'name'`.
fn parse_gsettings(output: &str) -> Option<String> {
  let output = output.trim();
  let value = output
    .strip_prefix('\'')
    .and_then(|rest| rest.strip_suffix('\''))
    .or_else(|| {
      output
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    })?;
  (!value.is_empty() && !value.contains(['\'', '"', '\\']))
    .then(|| value.to_owned())
}

/// `gsettings get org.gnome.desktop.interface icon-theme`'s output, or
/// `None` when it is not installed or fails. It reads dconf's database
/// file and needs no D-Bus: the session bus is set to an address that
/// cannot connect and `DISPLAY` is removed, so that GIO starts no bus of
/// its own, and its stderr is dropped.
pub fn run_gsettings() -> Option<String> {
  let output = Command::new("gsettings")
    .args(["get", "org.gnome.desktop.interface", "icon-theme"])
    .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
    .env_remove("DISPLAY")
    .stdin(Stdio::null())
    .stderr(Stdio::null())
    .output()
    .ok()?;
  output
    .status
    .success()
    .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The value of `key` in `[group]` of an INI text, without quotes; `None`
/// when it is missing or empty.
fn ini_value(text: &str, group: &str, key: &str) -> Option<String> {
  let mut in_group = false;
  for line in text.lines() {
    let line = line.trim();
    if let Some(name) = line
      .strip_prefix('[')
      .and_then(|rest| rest.strip_suffix(']'))
    {
      in_group = name == group;
      continue;
    }
    if !in_group {
      continue;
    }
    if let Some((name, value)) = line.split_once('=')
      && name.trim() == key
    {
      let value = value.trim().trim_matches('"').trim();
      return (!value.is_empty()).then(|| value.to_owned());
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
  }

  const HICOLOR: &str = "\
[Icon Theme]
Name=Hicolor
Directories=48x48/apps,scalable/apps,16x16/apps
ScaledDirectories=48x48@2/apps

[48x48/apps]
Size=48
Type=Threshold

[48x48@2/apps]
Size=48
Scale=2
Type=Fixed

[scalable/apps]
Size=48
MinSize=8
MaxSize=512
Type=Scalable

[16x16/apps]
Size=16
Type=Fixed
";

  const CHILD: &str = "\
[Icon Theme]
Name=Child
Inherits=Parent
Directories=apps/32

[apps/32]
Size=32
Type=Fixed
";

  const PARENT: &str = "\
[Icon Theme]
Name=Parent
Directories=apps/64

[apps/64]
Size=64
Type=Fixed
";

  struct Fixture {
    _directory: tempfile::TempDir,
    paths: IconPaths,
    data: PathBuf,
    home: PathBuf,
  }

  fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_owned();
    let data = root.join("share");
    let home = root.join("home");
    let paths = IconPaths::from_env(|name| match name {
      "HOME" => Some(home.clone().into()),
      "XDG_DATA_DIRS" => Some(data.clone().into()),
      _ => None,
    });
    write(&data.join("icons/hicolor/index.theme"), HICOLOR);
    write(&data.join("icons/Child/index.theme"), CHILD);
    write(&data.join("icons/Parent/index.theme"), PARENT);
    Fixture {
      _directory: directory,
      paths,
      data,
      home,
    }
  }

  #[test]
  fn paths_follow_the_environment() {
    let paths = IconPaths::from_env(|name| match name {
      "HOME" => Some("/home/u".into()),
      "XDG_DATA_DIRS" => Some("/a:rel:/usr/share".into()),
      _ => None,
    });
    assert_eq!(
      paths.base_dirs,
      [
        PathBuf::from("/home/u/.icons"),
        PathBuf::from("/home/u/.local/share/icons"),
        PathBuf::from("/a/icons"),
        PathBuf::from("/usr/share/icons"),
      ]
    );
    assert_eq!(
      paths.pixmap_dirs,
      [
        PathBuf::from("/a/pixmaps"),
        PathBuf::from("/usr/share/pixmaps")
      ]
    );
  }

  #[test]
  fn directories_match_sizes_as_the_specification_says() {
    let theme = Theme::parse(HICOLOR);
    let by_path = |path: &str| {
      theme
        .directories
        .iter()
        .find(|directory| directory.path == path)
        .unwrap()
        .clone()
    };
    let threshold = by_path("48x48/apps");
    assert!(threshold.matches(46, 1) && threshold.matches(50, 1));
    assert!(!threshold.matches(45, 1) && !threshold.matches(48, 2));
    assert_eq!(threshold.distance(40, 1), 6);
    let scalable = by_path("scalable/apps");
    assert!(scalable.matches(28, 1));
    assert_eq!(scalable.distance(600, 1), 88);
    let fixed = by_path("48x48@2/apps");
    assert!(fixed.matches(48, 2) && !fixed.matches(48, 1));
    assert_eq!(fixed.distance(28, 2), 40);
    assert_eq!(theme.directories.len(), 4);
  }

  #[test]
  fn an_exact_directory_wins_then_the_closest() {
    let fixture = fixture();
    let hicolor = fixture.data.join("icons/hicolor");
    write(&hicolor.join("16x16/apps/small.png"), "");
    write(&hicolor.join("48x48/apps/both.png"), "");
    write(&hicolor.join("scalable/apps/both.svg"), "");
    write(&hicolor.join("48x48@2/apps/both.png"), "");
    let mut lookup = IconLookup::new(fixture.paths.clone(), "hicolor");
    // 28 is no fixed or threshold size: the scalable directory matches.
    assert_eq!(
      lookup.find("both", 28, 1),
      Some(hicolor.join("scalable/apps/both.svg"))
    );
    assert_eq!(
      lookup.find("both", 48, 1),
      Some(hicolor.join("48x48/apps/both.png"))
    );
    assert_eq!(
      lookup.find("both", 48, 2),
      Some(hicolor.join("48x48@2/apps/both.png"))
    );
    // Only a 16 pixel file: the closest is taken.
    assert_eq!(
      lookup.find("small", 28, 1),
      Some(hicolor.join("16x16/apps/small.png"))
    );
    assert_eq!(
      lookup.find("small.png", 28, 1),
      Some(hicolor.join("16x16/apps/small.png"))
    );
    assert_eq!(lookup.find("missing", 28, 1), None);
    assert_eq!(lookup.find("a/b", 28, 1), None);
  }

  #[test]
  fn a_theme_falls_back_to_its_parents_hicolor_and_pixmaps() {
    let fixture = fixture();
    let icons = fixture.data.join("icons");
    write(&icons.join("Child/apps/32/own.png"), "");
    write(&icons.join("Parent/apps/64/own.svg"), "");
    write(&icons.join("Parent/apps/64/inherited.png"), "");
    write(&icons.join("hicolor/48x48/apps/plain.png"), "");
    write(&fixture.data.join("pixmaps/old.png"), "");
    // A user's copy in ~/.icons comes first.
    write(&fixture.home.join(".icons/Child/apps/32/user.png"), "");
    write(&icons.join("Child/apps/32/user.png"), "");
    let mut lookup = IconLookup::new(fixture.paths.clone(), "Child");
    assert_eq!(lookup.theme(), "Child");
    assert_eq!(
      lookup.find("own", 32, 1),
      Some(icons.join("Child/apps/32/own.png"))
    );
    assert_eq!(
      lookup.find("inherited", 32, 1),
      Some(icons.join("Parent/apps/64/inherited.png"))
    );
    assert_eq!(
      lookup.find("plain", 32, 1),
      Some(icons.join("hicolor/48x48/apps/plain.png"))
    );
    assert_eq!(
      lookup.find("old", 32, 1),
      Some(fixture.data.join("pixmaps/old.png"))
    );
    assert_eq!(
      lookup.find("user", 32, 1),
      Some(fixture.home.join(".icons/Child/apps/32/user.png"))
    );
    let absolute = fixture.data.join("pixmaps/old.png");
    assert_eq!(
      lookup.find(absolute.to_str().unwrap(), 32, 1),
      Some(absolute.clone())
    );
    assert_eq!(lookup.find("/nowhere/x.png", 32, 1), None);
    // An unknown theme is hicolor.
    let mut unknown = IconLookup::new(fixture.paths.clone(), "Nope");
    assert_eq!(
      unknown.find("plain", 32, 1),
      Some(icons.join("hicolor/48x48/apps/plain.png"))
    );
  }

  #[test]
  fn a_theme_inheriting_itself_ends() {
    let fixture = fixture();
    write(
      &fixture.data.join("icons/Loop/index.theme"),
      "[Icon Theme]\nInherits=Loop\nDirectories=\n",
    );
    let mut lookup = IconLookup::new(fixture.paths.clone(), "Loop");
    assert_eq!(lookup.find("nothing", 32, 1), None);
  }

  #[test]
  fn the_theme_comes_from_the_config_then_the_desktop() {
    let files = |kde: Option<&str>, gtk3: Option<&str>, gtk4: Option<&str>| {
      let kde = kde.map(str::to_owned);
      let gtk3 = gtk3.map(str::to_owned);
      let gtk4 = gtk4.map(str::to_owned);
      move |path: &Path| match path.to_str()? {
        "/c/kdeglobals" => kde.clone(),
        "/c/gtk-3.0/settings.ini" => gtk3.clone(),
        "/c/gtk-4.0/settings.ini" => gtk4.clone(),
        _ => None,
      }
    };
    let env = |desktop: &'static str| {
      move |name: &str| match name {
        "XDG_CONFIG_HOME" => Some("/c".into()),
        "XDG_CURRENT_DESKTOP" => Some(desktop.into()),
        _ => None,
      }
    };
    let kde = Some("[General]\nTheme=x\n[Icons]\nTheme=breeze-dark\n");
    let gtk3 = Some("[Settings]\ngtk-icon-theme-name = \"Adwaita\"\n");
    let gtk4 = Some("[Settings]\ngtk-icon-theme-name=Papirus\n");
    let none = || None;
    assert_eq!(
      theme_name(" Mine ", env("KDE"), files(kde, gtk3, gtk4), none),
      "Mine"
    );
    assert_eq!(
      theme_name("", env("KDE"), files(kde, gtk3, gtk4), none),
      "breeze-dark"
    );
    assert_eq!(
      theme_name("", env("GNOME"), files(kde, gtk3, gtk4), none),
      "Papirus"
    );
    assert_eq!(
      theme_name("", env("GNOME"), files(kde, gtk3, None), none),
      "Adwaita"
    );
    assert_eq!(
      theme_name("", env("wayfire"), files(kde, None, None), none),
      "breeze-dark"
    );
    assert_eq!(
      theme_name("", env("KDE"), files(None, gtk3, None), none),
      "Adwaita"
    );
    assert_eq!(
      theme_name("", env("KDE"), files(None, None, None), none),
      "hicolor"
    );
    let gnome = || Some("Papirus-Dark".to_owned());
    // GNOME's setting wins under GNOME, where GTK follows it.
    assert_eq!(
      theme_name("", env("GNOME"), files(kde, gtk3, gtk4), gnome),
      "Papirus-Dark"
    );
    assert_eq!(
      theme_name("x", env("GNOME"), files(kde, gtk3, gtk4), gnome),
      "x"
    );
    // Elsewhere it comes after the files.
    assert_eq!(
      theme_name("", env("wayfire"), files(None, gtk3, None), gnome),
      "Adwaita"
    );
    assert_eq!(
      theme_name("", env("wayfire"), files(None, None, None), gnome),
      "Papirus-Dark"
    );
    assert_eq!(
      theme_name("", env("KDE"), files(None, None, None), || Some(" ".into())),
      "hicolor"
    );
  }

  #[test]
  fn gsettings_output_is_a_quoted_name() {
    assert_eq!(parse_gsettings("'breeze'\n"), Some("breeze".to_owned()));
    assert_eq!(parse_gsettings("\"Adwaita\""), Some("Adwaita".to_owned()));
    assert_eq!(parse_gsettings("''"), None);
    assert_eq!(parse_gsettings("breeze"), None);
    assert_eq!(parse_gsettings("'a'b'"), None);
  }

  #[test]
  fn gnome_theme_runs_gsettings_once_per_database_time() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("dconf/user");
    let cache = directory.path().join("cache/gnome-icon-theme");
    let runs = std::cell::Cell::new(0);
    let run = |output: &'static str| {
      let runs = &runs;
      move || {
        runs.set(runs.get() + 1);
        Some(output.to_owned())
      }
    };
    // Outside GNOME, no database means no setting, without a run.
    assert_eq!(gnome_theme(false, &database, &cache, run("'x'")), None);
    assert_eq!(runs.get(), 0);
    // Under GNOME the default counts; the answer is cached.
    assert_eq!(
      gnome_theme(true, &database, &cache, run("'Adwaita'")),
      Some("Adwaita".to_owned())
    );
    assert_eq!(
      gnome_theme(true, &database, &cache, run("'other'")),
      Some("Adwaita".to_owned())
    );
    assert_eq!(runs.get(), 1);
    // A database written since asks again, and its answer is cached too.
    write(&database, "gvdb");
    assert_eq!(
      gnome_theme(false, &database, &cache, run("'breeze'")),
      Some("breeze".to_owned())
    );
    assert_eq!(
      gnome_theme(false, &database, &cache, run("'other'")),
      Some("breeze".to_owned())
    );
    assert_eq!(runs.get(), 2);
    // A failed run is cached as no setting.
    fs::remove_file(&cache).unwrap();
    assert_eq!(gnome_theme(false, &database, &cache, || None), None);
    assert_eq!(gnome_theme(false, &database, &cache, run("'y'")), None);
    assert_eq!(runs.get(), 2);
  }
}
