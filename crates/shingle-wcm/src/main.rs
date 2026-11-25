mod app;
mod ui;

use std::path::PathBuf;

use anyhow::{Result, bail};
use gloomy_core::{HeadlessConfig, HeadlessRenderer};
use shingle_config::{ShingleConfig, default_path};
use ui::{Page, UiModel};

fn main() -> Result<()> {
  let Some(options) = Options::parse()? else {
    print_help();
    return Ok(());
  };
  let source = options.source.unwrap_or_else(default_path);
  let config = ShingleConfig::load(Some(&source))?;
  let destination = options.destination.unwrap_or_else(|| {
    if source
      .extension()
      .is_some_and(|extension| extension == "yaml" || extension == "yml")
    {
      source.clone()
    } else {
      default_path()
    }
  });

  match options.mode {
    Mode::Window => app::run(config, source, destination),
    Mode::Check => {
      println!("Valid configuration: {}", source.display());
      Ok(())
    }
    Mode::Export => {
      config.save_yaml(&destination)?;
      println!("Saved YAML: {}", destination.display());
      Ok(())
    }
    Mode::Screenshot(path) => {
      let mut widget = ui::build(&UiModel {
        config: &config,
        source: &source,
        destination: &destination,
        page: options.page.unwrap_or(Page::Keybindings),
        capture_action: None,
        status: "Ready",
        dirty: false,
      });
      let mut renderer = HeadlessRenderer::new(HeadlessConfig {
        width: 1100,
        height: 850,
        ..HeadlessConfig::default()
      })?;
      renderer.save_screenshot(&mut widget, &path, None, None)?;
      println!("Saved screenshot: {}", path.display());
      Ok(())
    }
  }
}

#[derive(Default)]
struct Options {
  source: Option<PathBuf>,
  destination: Option<PathBuf>,
  mode: Mode,
  page: Option<Page>,
}

#[derive(Default)]
enum Mode {
  #[default]
  Window,
  Check,
  Export,
  Screenshot(PathBuf),
}

impl Options {
  fn parse() -> Result<Option<Self>> {
    let mut options = Self::default();
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
      match argument.to_str() {
        Some("--config") => {
          options.source = Some(next_path(&mut arguments, "--config")?)
        }
        Some("--output") => {
          options.destination = Some(next_path(&mut arguments, "--output")?)
        }
        Some("--check") => options.mode = Mode::Check,
        Some("--export-yaml") => options.mode = Mode::Export,
        Some("--page") => {
          let name = next_path(&mut arguments, "--page")?;
          options.page =
            Some(Page::named(&name.to_string_lossy()).ok_or_else(|| {
              anyhow::anyhow!("unknown WCM page: {}", name.display())
            })?);
        }
        Some("--screenshot") => {
          options.mode =
            Mode::Screenshot(next_path(&mut arguments, "--screenshot")?)
        }
        Some("--help" | "-h") => return Ok(None),
        _ => bail!("unknown argument: {}", argument.to_string_lossy()),
      }
    }
    Ok(Some(options))
  }
}

fn next_path(
  arguments: &mut impl Iterator<Item = std::ffi::OsString>,
  flag: &str,
) -> Result<PathBuf> {
  Ok(PathBuf::from(
    arguments
      .next()
      .ok_or_else(|| anyhow::anyhow!("{flag} needs a path"))?,
  ))
}

fn print_help() {
  println!(
    "shingle-wcm [OPTIONS]\n\
         \n\
         --config PATH       Load YAML, Wayfire INI, or i3 config\n\
         --output PATH       YAML destination (defaults to source YAML or \
         Shingle default)\n\
         --check             Validate the source without opening a window\n\
         --export-yaml       Convert and save YAML without opening a window\n\
         --page NAME         Screenshot page: general, tiling, keys, \
         commands, rules, input, idle\n\
         --screenshot PATH   Render the selected page headlessly to a PNG \
         (default: keys)\n\
         --help              Show this help"
  );
}
