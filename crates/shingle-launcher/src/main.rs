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
  index::{SearchPaths, load_cache, save_cache, scan},
  launch::pick_terminal,
  layout::LayoutStore,
  model::{Key, Model, Modifiers, Storage},
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
  --screenshot PATH   Draw the panel to a PNG without Wayland and exit
  --query TEXT        With --screenshot, type TEXT first
  --help              Show this help
  --version           Show the version

Files: $XDG_DATA_HOME/shingle/launcher.yaml (the layout),
$XDG_STATE_HOME/shingle/launcher-history.json and
$XDG_CACHE_HOME/shingle/launcher-index.json.";

#[derive(Debug, Default, PartialEq)]
struct Arguments {
  config: Option<PathBuf>,
  trace: bool,
  timing: bool,
  screenshot: Option<PathBuf>,
  query: String,
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
    return screenshot(&path, &mut model, renderer, &theme);
  }
  let options = client::Options {
    trace: arguments.trace,
    timing: arguments.timing,
    started,
    index_cache: paths.index_cache.clone(),
    search_paths,
    terminal,
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

/// Draws the panel of `model` alone to a PNG at `path`.
fn screenshot(
  path: &Path,
  model: &mut Model,
  mut renderer: Renderer,
  theme: &Theme,
) -> Result<(), String> {
  let size = (theme.width, theme.height);
  renderer.resize(size.0 as u32, size.1 as u32, 1.0);
  model.page = view::page_size(theme, size);
  let mut frame = {
    let measure = renderer.measurer();
    view::build(model, theme, size, &measure)
  };
  let mut pixels = vec![0_u8; size.0 as usize * size.1 as usize * 4];
  let stride = size.0 as usize * 4;
  renderer.draw(&mut frame, theme.panel, |row, bytes| {
    let start = row * stride;
    for (target, source) in pixels[start..start + stride]
      .chunks_exact_mut(4)
      .zip(bytes.chunks_exact(4))
    {
      target.copy_from_slice(&[source[2], source[1], source[0], 255]);
    }
  })?;
  image::RgbaImage::from_raw(size.0 as u32, size.1 as u32, pixels)
    .ok_or("bad image size")?
    .save(path)
    .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
  println!("Saved screenshot: {}", path.display());
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

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
      }
    );
    assert!(matches!(arguments(&["--help"]), Ok(Action::Help)));
    assert!(arguments(&["--bogus"]).is_err());
    assert!(arguments(&["--screenshot"]).is_err());
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
