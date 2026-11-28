//! `shingle-launcher`: shows the launcher on the focused output, launches
//! what is chosen and exits (docs/design/launcher.md).

mod client;

use std::{
  ffi::OsString,
  os::unix::net::UnixListener,
  path::{Path, PathBuf},
  process::ExitCode,
  time::{Instant, SystemTime, UNIX_EPOCH},
};

use shingle_config::{LauncherConfig, ShingleConfig, default_path};
use shingle_launcher::{
  files::Paths,
  history::History,
  icon_cache::{IconStore, resolve},
  icons::{IconLookup, IconPaths, gnome_theme, run_gsettings, theme_name},
  index::{SearchPaths, load_cache, save_cache, scan},
  launch::pick_terminal,
  layout::LayoutStore,
  model::{Key, Model, Modifiers, Row, Storage},
  render::{Renderer, RendererChoice},
  theme::Theme,
  view,
};

const HELP: &str = "\
shingle-launcher [OPTIONS]

Shows the application launcher on the focused output. Type to search,
Enter launches; Ctrl+D favourites, F2 renames, Ctrl+N adds a shortcut
from the typed text, Ctrl+M moves to a category, Ctrl+H hides. Running it
again while it is open closes it.

  --config PATH       Read the launcher section of this Shingle config
                      (default: Shingle's own, $XDG_CONFIG_HOME/shingle/
                      shingle.yaml)
  --trace             Print one JSON line per frame and per launch
  --timing            Print how long the first frame took
  --screenshot PATH   Draw the panel to a PNG (a PPM for a .ppm path)
                      without Wayland and exit
  --query TEXT        With --screenshot, type TEXT first
  --scale FACTOR      With --screenshot, draw at this scale (default 1)
  --help              Show this help
  --version           Show the version

Files: $XDG_DATA_HOME/shingle/launcher.yaml (the layout),
$XDG_STATE_HOME/shingle/launcher-history.json,
$XDG_CACHE_HOME/shingle/launcher-index.json and the decoded icons in
$XDG_CACHE_HOME/shingle/launcher-icons.";

#[derive(Debug, Default, PartialEq)]
struct Arguments {
  config: Option<PathBuf>,
  trace: bool,
  timing: bool,
  screenshot: Option<PathBuf>,
  query: String,
  scale: Option<f32>,
}

enum Action {
  Run(Arguments),
  Help,
  Version,
}

fn parse(
  arguments: impl IntoIterator<Item = OsString>,
) -> Result<Action, String> {
  let mut parsed = Arguments::default();
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    let mut value = |name: &str| {
      arguments
        .next()
        .ok_or_else(|| format!("{name} needs a value"))
    };
    match argument.to_str() {
      Some("--config") => {
        parsed.config = Some(PathBuf::from(value("--config")?))
      }
      Some("--trace") => parsed.trace = true,
      Some("--timing") => parsed.timing = true,
      Some("--screenshot") => {
        parsed.screenshot = Some(PathBuf::from(value("--screenshot")?))
      }
      Some("--query") => {
        parsed.query = value("--query")?.to_string_lossy().into_owned()
      }
      Some("--scale") => {
        let text = value("--scale")?.to_string_lossy().into_owned();
        let scale = text
          .parse::<f32>()
          .ok()
          .filter(|scale| (0.5..=8.0).contains(scale))
          .ok_or_else(|| format!("--scale {text}: not between 0.5 and 8"))?;
        parsed.scale = Some(scale);
      }
      Some("--help" | "-h") => return Ok(Action::Help),
      Some("--version" | "-V") => return Ok(Action::Version),
      _ => {
        return Err(format!(
          "unknown argument {}; see --help",
          argument.to_string_lossy()
        ));
      }
    }
  }
  Ok(Action::Run(parsed))
}

fn main() -> ExitCode {
  let started = Instant::now();
  let arguments = match parse(std::env::args_os().skip(1)) {
    Ok(Action::Run(arguments)) => arguments,
    Ok(Action::Help) => {
      println!("{HELP}");
      return ExitCode::SUCCESS;
    }
    Ok(Action::Version) => {
      println!("shingle-launcher {}", env!("CARGO_PKG_VERSION"));
      return ExitCode::SUCCESS;
    }
    Err(error) => {
      eprintln!("shingle-launcher: {error}");
      return ExitCode::from(2);
    }
  };
  match run(arguments, started) {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("shingle-launcher: {error}");
      ExitCode::FAILURE
    }
  }
}

/// The name of the single-instance socket for the display the launcher
/// runs on.
fn socket_path(runtime: &Path, display: Option<OsString>) -> PathBuf {
  let display = display
    .as_deref()
    .map(Path::new)
    .and_then(Path::file_name)
    .map(|name| name.to_string_lossy().into_owned())
    .unwrap_or_else(|| "wayland-0".to_owned());
  runtime.join(format!("shingle-launcher-{display}.sock"))
}

/// The `launcher` section of the config at `path`, or of Shingle's own;
/// the defaults when there is none or it does not load.
fn launcher_config(path: Option<&Path>) -> LauncherConfig {
  let path = path.map(Path::to_owned).unwrap_or_else(default_path);
  if !path.exists() {
    return LauncherConfig::default();
  }
  match ShingleConfig::load(Some(&path)) {
    Ok(config) => config.launcher,
    Err(error) => {
      eprintln!(
        "shingle-launcher: {}: {error}; using the default look",
        path.display()
      );
      LauncherConfig::default()
    }
  }
}

fn now() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_secs())
    .unwrap_or_default()
}

fn run(arguments: Arguments, started: Instant) -> Result<(), String> {
  let var = |name: &str| std::env::var_os(name);
  let paths = Paths::from_env(var);
  let search_paths = SearchPaths::from_env(var);
  let config = launcher_config(arguments.config.as_deref());
  let theme = Theme::from_config(&config);
  let choice = RendererChoice::from(config.renderer);
  let terminal =
    pick_terminal(&config.terminal, var("TERMINAL"), &search_paths.path_dirs);
  let icon_paths = IconPaths::from_env(var);
  let icon_theme = theme_name(
    &config.icon_theme,
    var,
    |path| std::fs::read_to_string(path).ok(),
    || {
      let gnome = var("XDG_CURRENT_DESKTOP").is_some_and(|value| {
        value
          .to_string_lossy()
          .split(':')
          .any(|desktop| desktop.eq_ignore_ascii_case("GNOME"))
      });
      gnome_theme(
        gnome,
        &paths.dconf_database,
        &paths.icon_cache.join("gnome-icon-theme"),
        run_gsettings,
      )
    },
  );

  let socket = paths
    .runtime
    .as_deref()
    .map(|runtime| socket_path(runtime, var("WAYLAND_DISPLAY")));
  let mut listener = None;
  if arguments.screenshot.is_none()
    && let Some(socket) = &socket
  {
    if client::close_running(socket) {
      eprintln!("shingle-launcher: closed the open launcher");
      return Ok(());
    }
    let _ = std::fs::remove_file(socket);
    listener = UnixListener::bind(socket).ok();
  }
  // Connect first, so that a missing compositor fails before the GPU
  // starts.
  let connection = match &arguments.screenshot {
    Some(_) => None,
    None => Some(
      wayland_client::Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to Wayland: {error}"))?,
    ),
  };

  let (index, scanned) = match load_cache(&paths.index_cache) {
    Some(index) => (index, false),
    None => (scan(&search_paths), true),
  };
  if scanned && let Err(error) = save_cache(&paths.index_cache, &index) {
    eprintln!(
      "shingle-launcher: cannot write {}: {error}",
      paths.index_cache.display()
    );
  }
  let store = LayoutStore::new(paths.layout.clone());
  let (layout, warning) = store.load();
  if let Some(warning) = warning {
    eprintln!("shingle-launcher: {warning}");
  }
  let history = History::load(&paths.history);
  let mut model = Model::new(
    index,
    layout,
    history,
    Storage {
      layout: Some(store),
      history: Some(paths.history.clone()),
    },
    now(),
  );
  let renderer = Renderer::new(choice)?;

  if let Some(path) = arguments.screenshot {
    for character in arguments.query.chars() {
      model.key(Key::Text(character.to_string()), Modifiers::NONE);
    }
    let icons = IconLookup::new(icon_paths, &icon_theme);
    let scale = arguments.scale.unwrap_or(1.0);
    return screenshot(&path, &mut model, renderer, &theme, icons, scale);
  }
  let options = client::Options {
    trace: arguments.trace,
    timing: arguments.timing,
    started,
    index_cache: paths.index_cache.clone(),
    search_paths,
    terminal,
    icon_paths,
    icon_theme,
    icon_cache: paths.icon_cache.clone(),
  };
  let result = client::run(
    connection.expect("connected"),
    model,
    renderer,
    theme,
    options,
    listener,
  );
  if let Some(socket) = socket {
    let _ = std::fs::remove_file(socket);
  }
  result
}

/// Draws the panel of `model` alone at `scale` to a PNG at `path`, with
/// the icons of the rows shown looked up and decoded first.
fn screenshot(
  path: &Path,
  model: &mut Model,
  mut renderer: Renderer,
  theme: &Theme,
  mut lookup: IconLookup,
  scale: f32,
) -> Result<(), String> {
  let size = (theme.width, theme.height);
  let width = (size.0 * scale).round() as u32;
  let height = (size.1 * scale).round() as u32;
  renderer.resize(width, height, scale);
  model.page = view::page_size(theme, size);
  let icon_size = view::icon_size(theme);
  let pixels = (icon_size * scale).round().max(1.0) as u32;
  let mut icons = IconStore::new(pixels);
  let shown = model.rows().into_iter().skip(model.scroll).take(model.page);
  for row in shown {
    let (position, action) = match row {
      Row::Entry(position) => (position, None),
      Row::Action(position, action) => (position, Some(action)),
      Row::Run => continue,
    };
    let entry = &model.catalog.entries[position];
    let name = action
      .map(|action| entry.actions[action].icon.clone())
      .filter(|icon| !icon.is_empty())
      .unwrap_or_else(|| entry.icon.clone());
    if name.is_empty() || icons.knows(&name) {
      continue;
    }
    let icon = resolve(
      &mut lookup,
      &name,
      icon_size as u32,
      scale.ceil().max(1.0) as u32,
      pixels,
      None,
    );
    icons.icons.insert(name, icon);
  }
  let mut frame = {
    let measure = renderer.measurer();
    view::build(model, theme, size, &measure, &|icon| {
      icons.get(icon).is_some()
    })
  };
  let mut pixels = vec![0_u8; width as usize * height as usize * 4];
  let stride = width as usize * 4;
  renderer.draw(&mut frame, theme.panel, &icons, |row, bytes| {
    let start = row * stride;
    for (target, source) in pixels[start..start + stride]
      .chunks_exact_mut(4)
      .zip(bytes.chunks_exact(4))
    {
      target.copy_from_slice(&[source[2], source[1], source[0], 255]);
    }
  })?;
  let saved = if path.extension().is_some_and(|extension| extension == "ppm") {
    std::fs::write(path, ppm(width, height, &pixels))
      .map_err(|error| error.to_string())
  } else {
    image::RgbaImage::from_raw(width, height, pixels)
      .ok_or_else(|| "bad image size".to_owned())?
      .save(path)
      .map_err(|error| error.to_string())
  };
  saved.map_err(|error| format!("cannot write {}: {error}", path.display()))?;
  println!("Saved screenshot: {}", path.display());
  Ok(())
}

/// RGBA `pixels` as a binary PPM, which the harness compares with its
/// captures.
fn ppm(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
  let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
  for pixel in pixels.chunks_exact(4) {
    bytes.extend_from_slice(&pixel[..3]);
  }
  bytes
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn screenshots_can_be_ppm() {
    assert_eq!(
      ppm(2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]),
      b"P6\n2 1\n255\n\x01\x02\x03\x04\x05\x06"
    );
  }

  fn arguments(items: &[&str]) -> Result<Action, String> {
    parse(items.iter().map(OsString::from))
  }

  #[test]
  fn options_parse() {
    let Ok(Action::Run(parsed)) = arguments(&[
      "--trace",
      "--timing",
      "--screenshot",
      "a.png",
      "--query",
      "x",
      "--scale",
      "1.5",
    ]) else {
      panic!("not parsed");
    };
    assert_eq!(
      parsed,
      Arguments {
        config: None,
        trace: true,
        timing: true,
        screenshot: Some("a.png".into()),
        query: "x".into(),
        scale: Some(1.5),
      }
    );
    assert!(matches!(arguments(&["--help"]), Ok(Action::Help)));
    assert!(arguments(&["--bogus"]).is_err());
    assert!(arguments(&["--screenshot"]).is_err());
    assert!(arguments(&["--scale", "0"]).is_err());
    assert!(arguments(&["--scale", "x"]).is_err());
  }

  #[test]
  fn the_socket_is_named_after_the_display() {
    let runtime = Path::new("/run/user/1");
    assert_eq!(
      socket_path(runtime, Some("wayland-1".into())),
      Path::new("/run/user/1/shingle-launcher-wayland-1.sock")
    );
    assert_eq!(
      socket_path(runtime, Some("/tmp/x/shingle-0".into())),
      Path::new("/run/user/1/shingle-launcher-shingle-0.sock")
    );
    assert_eq!(
      socket_path(runtime, None),
      Path::new("/run/user/1/shingle-launcher-wayland-0.sock")
    );
  }
}
