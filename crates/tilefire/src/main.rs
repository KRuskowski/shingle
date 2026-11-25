use std::{env, error::Error, path::PathBuf, process::ExitCode};

use tilefire::{Compositor, Config};
use tilefire_config::TilefireConfig;
use tilefire_session::{SessionStore, launch_autostart, launch_saved};

fn run() -> Result<(), Box<dyn Error>> {
    let mut headless = false;
    let mut config_path = None;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--headless" => headless = true,
            "--config" => {
                config_path = Some(PathBuf::from(
                    args.next().ok_or("missing --config path")?,
                ));
            }
            _ => {
                eprintln!("usage: tilefire --headless [--config PATH]");
                return Err("invalid arguments".into());
            }
        }
    }
    if !headless {
        eprintln!("usage: tilefire --headless [--config PATH]");
        return Err("invalid arguments".into());
    }
    let yaml = TilefireConfig::load(config_path.as_deref())?;
    let store = if let Some(path) = &yaml.session.path {
        SessionStore::new(path)
    } else {
        SessionStore::default_location()?
    };
    let restored = if yaml.session.restore {
        store.load()?
    } else {
        None
    };
    let keymap = yaml.input.load_keymap()?;
    let config = Config::from(yaml.clone());
    let (mut compositor, snapshot) = if let Some((core, snapshot)) = restored {
        (Compositor::with_core(config, core)?, Some(snapshot))
    } else {
        (Compositor::new(config)?, None)
    };
    compositor.set_keymap(&keymap)?;
    let discard_output = yaml.behavior.discard_command_output;
    let mut session_children = snapshot
        .as_ref()
        .map(|snapshot| {
            let report = launch_saved(
                snapshot,
                Some(compositor.socket_name()),
                discard_output,
            );
            for error in &report.failed {
                eprintln!("tilefire: session restore launch failed: {error}");
            }
            report
                .launched
                .into_iter()
                .map(|launched| launched.child)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let autostart = launch_autostart(
        &yaml.autostart,
        snapshot.as_ref(),
        compositor.socket_name(),
        discard_output,
    );
    for error in &autostart.failed {
        eprintln!("tilefire: {error}");
    }
    session_children.extend(autostart.children);
    let _session_children = session_children;
    println!(
        "WAYLAND_DISPLAY={}",
        compositor.socket_name().to_string_lossy()
    );
    compositor.run()?;
    store.save(compositor.core())?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tilefire: {error}");
            ExitCode::FAILURE
        }
    }
}
