//! Where `shingle-lock`'s settings come from, in this order, each over
//! the one before: the defaults, the swaylock config (the file `-C`
//! names, else the one swaylock reads, unless `lock.read_swaylock_config`
//! is off), the `lock.options` of Shingle's config, and the command line.
//! Shingle's config is the file `--shingle-config` names, else the one
//! Shingle passed in `SHINGLE_CONFIG_FILE`, else Shingle's default path.
//! Options that do not apply warn and are ignored.

use std::{
  path::{Path, PathBuf},
  time::Duration,
};

use shingle_config::{DriftConfig, LockConfig, LockOptions};

/// What `shingle-lock` runs with.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
  /// swaylock's options.
  pub options: LockOptions,
  /// The background's movement.
  pub drift: DriftConfig,
  /// How long the input stays without a key.
  pub input_timeout: Duration,
}

/// What the command line asks for.
#[derive(Debug, PartialEq)]
pub enum Action {
  /// Lock with these settings; the warnings are printed first.
  Run(Box<Settings>, Vec<String>),
  /// Print the usage.
  Help,
  /// Print the version.
  Version,
}

/// What [`resolve`] reads from the system.
pub struct Sources<'a> {
  /// Looks up an environment variable.
  pub var: &'a dyn Fn(&str) -> Option<String>,
  /// Whether a file exists.
  pub exists: &'a dyn Fn(&Path) -> bool,
  /// Reads a text file.
  pub read: &'a dyn Fn(&Path) -> std::io::Result<String>,
  /// Loads the `lock` section of the Shingle config at a path.
  pub lock_section: &'a dyn Fn(&Path) -> Result<LockConfig, String>,
  /// Shingle's default config path.
  pub default_shingle_config: PathBuf,
}

impl Sources<'_> {
  /// The real system.
  pub fn system() -> Sources<'static> {
    Sources {
      var: &|name| std::env::var(name).ok(),
      exists: &|path| path.is_file(),
      read: &|path| std::fs::read_to_string(path),
      lock_section: &|path| {
        shingle_config::ShingleConfig::load(Some(path))
          .map(|config| config.lock)
          .map_err(|error| error.to_string())
      },
      default_shingle_config: shingle_config::default_path(),
    }
  }
}

/// The value after `flag` or in `--flag=VALUE`, the last one given.
fn flag_value(
  arguments: &[String],
  names: &[&str],
) -> Result<Option<String>, String> {
  let mut found = None;
  let mut index = 0;
  while index < arguments.len() {
    let argument = &arguments[index];
    if names.contains(&argument.as_str()) {
      let value = arguments
        .get(index + 1)
        .ok_or_else(|| format!("{argument} needs a path"))?;
      found = Some(value.clone());
      index += 2;
      continue;
    }
    for name in names.iter().filter(|name| name.starts_with("--")) {
      if let Some(value) = argument.strip_prefix(&format!("{name}=")) {
        found = Some(value.to_owned());
      }
    }
    index += 1;
  }
  match found {
    Some(value) if value.is_empty() => {
      Err("a config path cannot be empty".into())
    }
    found => Ok(found),
  }
}

/// Resolves `arguments` (without the program name) against `sources`.
///
/// # Errors
///
/// A message for a config file named on the command line that cannot be
/// read, or a flag without its path.
pub fn resolve(
  arguments: &[String],
  sources: &Sources<'_>,
) -> Result<Action, String> {
  for argument in arguments {
    match argument.as_str() {
      "-h" | "--help" => return Ok(Action::Help),
      "-v" | "--version" => return Ok(Action::Version),
      _ => {}
    }
  }
  let mut warnings = Vec::new();
  let shingle_config = flag_value(arguments, &["--shingle-config"])?
    .map(PathBuf::from)
    .or_else(|| (sources.var)("SHINGLE_CONFIG_FILE").map(PathBuf::from))
    .or_else(|| {
      let path = sources.default_shingle_config.clone();
      (sources.exists)(&path).then_some(path)
    });
  let section = match &shingle_config {
    Some(path) => (sources.lock_section)(path).unwrap_or_else(|error| {
      warnings.push(format!(
        "{}: {error}; using the default lock section",
        path.display()
      ));
      LockConfig::default()
    }),
    None => LockConfig::default(),
  };
  let mut options = LockOptions::default();
  let named = flag_value(arguments, &["-C", "--config"])?.map(PathBuf::from);
  let swaylock = match named {
    Some(path) => {
      let text = (sources.read)(&path).map_err(|error| {
        format!("failed to read config {}: {error}", path.display())
      })?;
      Some((path, text))
    }
    None if section.read_swaylock_config => section
      .swaylock_config
      .clone()
      .filter(|path| (sources.exists)(path))
      .or_else(|| {
        shingle_config::swaylock_config_path(sources.var, sources.exists)
      })
      .and_then(|path| match (sources.read)(&path) {
        Ok(text) => Some((path, text)),
        Err(error) => {
          warnings.push(format!("{}: {error}; not read", path.display()));
          None
        }
      }),
    None => None,
  };
  if let Some((path, text)) = swaylock {
    for (line, warning) in options.apply_file(&text) {
      warnings.push(format!("{}:{line}: {warning}", path.display()));
    }
  }
  if let Err(error) = section.apply_to(&mut options) {
    warnings.push(format!("{error}; the lock section's options are skipped"));
  }
  warnings.extend(options.apply_arguments(arguments));
  Ok(Action::Run(
    Box::new(Settings {
      options,
      drift: section.drift.clone(),
      input_timeout: Duration::from_secs_f64(section.input_timeout),
    }),
    warnings,
  ))
}

/// The usage `--help` prints.
pub const HELP: &str = "\
Usage: shingle-lock [OPTIONS]

Locks the session with ext-session-lock-v1 and unlocks it with PAM.
It takes swaylock's and swaylock-effects' options, on the command line
as --NAME[=VALUE] or in a swaylock config as NAME[=VALUE], among them:

  -C, --config <PATH>            Read this swaylock config
      --shingle-config <PATH>    Read the lock section of this Shingle
                                 config
  -f, --daemonize                Detach once the session is locked
  -S, --screenshot               Blur a capture of every output
      --effect-blur <RxT>        Blur radius and passes
      --effect-vignette <B:F>    Darken the edges
      --clock                    Show the time and date
      --timestr, --datestr <F>   strftime formats of the clock
  -c, --color <RRGGBB[AA]>       Background colour
      --font <NAME>              Font family
      --indicator                Show the password input at rest too
  -u, --no-unlock-indicator      Never show the password input
      --indicator-radius <N>     The input's height
      --indicator-thickness <N>  Twice the input's border width
      --ring-color, --inside-color, --line-color, --text-color and
      their -clear, -ver and -wrong forms, --key-hl-color,
      --bs-hl-color, --separator-color <RRGGBB[AA]>
  -F, --show-failed-attempts     Count failed attempts
  -e, --ignore-empty-password    Enter without a password does nothing
      --grace <SECONDS>          Unlock without a password at first
      --grace-no-mouse, --grace-no-touch
  -h, --help                     Print this help
  -v, --version                  Print the version

The swaylock config is $XDG_CONFIG_HOME/swaylock/config,
~/.swaylock/config or /etc/swaylock/config unless -C names one; the
lock section of Shingle's config adds options and the background's
drift. See docs/design/lock-screen.md.";

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;

  struct Fake {
    files: BTreeMap<PathBuf, String>,
    env: BTreeMap<&'static str, String>,
    section: Result<LockConfig, String>,
  }

  impl Fake {
    fn new() -> Self {
      Self {
        files: BTreeMap::new(),
        env: BTreeMap::from([("HOME", "/home/invented".to_owned())]),
        section: Ok(LockConfig::default()),
      }
    }

    fn resolve(&self, arguments: &[&str]) -> Result<Action, String> {
      let arguments =
        arguments.iter().map(|a| a.to_string()).collect::<Vec<_>>();
      let var = |name: &str| self.env.get(name).cloned();
      let exists = |path: &Path| self.files.contains_key(path);
      let read = |path: &Path| {
        self
          .files
          .get(path)
          .cloned()
          .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
      };
      let lock_section = |_: &Path| self.section.clone();
      resolve(
        &arguments,
        &Sources {
          var: &var,
          exists: &exists,
          read: &read,
          lock_section: &lock_section,
          default_shingle_config: PathBuf::from("/invented/shingle.yaml"),
        },
      )
    }
  }

  fn run(action: Result<Action, String>) -> (Settings, Vec<String>) {
    match action.unwrap() {
      Action::Run(settings, warnings) => (*settings, warnings),
      other => panic!("{other:?}"),
    }
  }

  #[test]
  fn the_swaylock_config_then_the_section_then_the_command_line() {
    let mut fake = Fake::new();
    fake.files.insert(
      "/home/invented/.config/swaylock/config".into(),
      "clock\ncolor=112233\neffect-blur=5x2\nno-such\n".into(),
    );
    fake
      .files
      .insert("/invented/shingle.yaml".into(), String::new());
    let mut section = LockConfig::default();
    section.options.insert(
      "color".into(),
      shingle_config::LockValue::Text("445566".into()),
    );
    section.drift.fps = 3.0;
    section.input_timeout = 2.0;
    fake.section = Ok(section);
    let (settings, warnings) = run(fake.resolve(&["--effect-blur=9x1"]));
    assert!(settings.options.clock);
    assert_eq!(settings.options.color, [0x44, 0x55, 0x66, 0xff]);
    assert_eq!(settings.options.blur, Some((9, 1)));
    assert_eq!(settings.drift.fps, 3.0);
    assert_eq!(settings.input_timeout, Duration::from_secs(2));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
      warnings[0].starts_with(
        "/home/invented/.config/swaylock/config:4: unknown option"
      )
    );
  }

  #[test]
  fn a_named_config_must_exist_and_the_section_can_turn_swaylock_off() {
    let mut fake = Fake::new();
    assert!(fake.resolve(&["-C", "/missing"]).is_err());
    assert!(fake.resolve(&["--config"]).is_err());
    fake.files.insert("/named".into(), "screenshot\n".into());
    let (settings, _) = run(fake.resolve(&["--config=/named"]));
    assert!(settings.options.screenshot);
    fake.files.insert(
      "/home/invented/.config/swaylock/config".into(),
      "clock\n".into(),
    );
    fake
      .env
      .insert("SHINGLE_CONFIG_FILE", "/elsewhere.yaml".into());
    fake.section = Ok(LockConfig {
      read_swaylock_config: false,
      ..LockConfig::default()
    });
    let (settings, _) = run(fake.resolve(&[]));
    assert!(!settings.options.clock, "the swaylock config is not read");
  }

  #[test]
  fn a_broken_shingle_config_warns_and_help_wins() {
    let mut fake = Fake::new();
    fake
      .env
      .insert("SHINGLE_CONFIG_FILE", "/broken.yaml".into());
    fake.section = Err("invalid YAML".into());
    let (settings, warnings) = run(fake.resolve(&[]));
    assert_eq!(settings.options, LockOptions::default());
    assert!(warnings[0].contains("/broken.yaml: invalid YAML"));
    assert_eq!(
      fake.resolve(&["-C", "/missing", "--help"]),
      Ok(Action::Help)
    );
    assert_eq!(fake.resolve(&["-v"]), Ok(Action::Version));
  }
}
