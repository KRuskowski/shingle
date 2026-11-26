//! The headless backend: the compositor without display or input devices,
//! serving clients and IPC until SIGINT, SIGTERM or a quit request.

use std::{
  error::Error,
  ffi::OsString,
  path::Path,
  process::Child,
  time::{Duration, Instant},
};

use shingle::{
  ClientEnvironment, Compositor, Config, InputDeviceId, InputDeviceInfo,
  InputDeviceKind,
};
use shingle_config::ShingleConfig;
use shingle_session::{
  SessionSnapshot, SessionStore, launch_autostart, launch_program, launch_saved,
};

/// Runs Shingle headless with `config` and saves the session when it ends.
///
/// With `ipc_path`, Wayfire IPC is served there, and every client finds it
/// as `WAYFIRE_SOCKET`. `spawn`, if not empty, is a program and its
/// arguments to start as a client.
pub fn run(
  yaml: ShingleConfig,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
) -> Result<(), Box<dyn Error>> {
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
  register_stipc_devices(&mut compositor);
  if let Some(path) = ipc_path {
    compositor.enable_ipc(path)?;
  }
  let discard_output = yaml.behavior.discard_command_output;
  let environment = compositor.client_environment();
  let mut session_children = snapshot
    .as_ref()
    .map(|snapshot| {
      let report = launch_saved(snapshot, &environment, discard_output);
      for error in &report.failed {
        eprintln!("shingle: session restore launch failed: {error}");
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
    &environment,
    discard_output,
  );
  for error in &autostart.failed {
    eprintln!("shingle: {error}");
  }
  session_children.extend(autostart.children);
  println!(
    "WAYLAND_DISPLAY={}",
    compositor.socket_name().to_string_lossy()
  );
  if let Some(display) = compositor.x_display() {
    println!("DISPLAY={display}");
  }
  if let Some(child) = spawn_client(&spawn, &environment)? {
    session_children.push(child);
  }
  let _session_children = session_children;
  run_with_frames(&mut compositor)?;
  shut_down(compositor, |snapshot| store.save_snapshot(snapshot))?;
  Ok(())
}

/// Ends a compositor whose loop has finished: takes the session snapshot,
/// drops the compositor, which stops Xwayland, removes the X display's
/// lock and sockets and closes the Wayland and IPC sockets, and only then
/// hands the snapshot to `save`.
///
/// Saving syncs the file and its directory to disk, which took more than
/// 10 s on a busy disk, so the display is freed first: a SIGKILL sent
/// because the exit took too long then leaves nothing behind.
fn shut_down<E>(
  compositor: Compositor,
  save: impl FnOnce(&SessionSnapshot) -> Result<(), E>,
) -> Result<(), E> {
  let snapshot = compositor.core().snapshot();
  drop(compositor);
  save(&snapshot)
}

/// Serves clients until shutdown, reporting a frame at the output's
/// refresh rate as wlroots' headless backend does for Wayfire, so that
/// clients waiting for frame callbacks or FIFO barriers keep drawing.
/// Without it, a nested Shingle in a headless one drew one frame only.
fn run_with_frames(compositor: &mut Compositor) -> Result<(), shingle::Error> {
  let interval = frame_interval(compositor.output_info().refresh_mhz);
  let mut next = Instant::now() + interval;
  while !compositor.shutdown_requested() {
    let now = Instant::now();
    if now >= next {
      compositor.frame();
      next = (next + interval).max(now);
    }
    compositor.poll(Some(next.saturating_duration_since(Instant::now())))?;
  }
  Ok(())
}

/// The time between frames at `refresh_mhz` (millihertz); 60 Hz when it
/// is not known.
pub(crate) fn frame_interval(refresh_mhz: u32) -> Duration {
  let refresh_mhz = if refresh_mhz == 0 {
    60_000
  } else {
    refresh_mhz
  };
  Duration::from_nanos(1_000_000_000_000 / u64::from(refresh_mhz))
}

/// Registers a keyboard, a pointer and a touch device for STIPC input
/// (`stipc/feed_key`, `feed_button`, `move_cursor`, `touch`), which
/// Wayfire's stipc plugin creates as headless input devices on any
/// backend. Headless and offscreen have no other devices.
pub(crate) fn register_stipc_devices(
  compositor: &mut Compositor,
) -> Vec<InputDeviceId> {
  [
    ("Shingle headless keyboard", InputDeviceKind::Keyboard),
    ("Shingle headless pointer", InputDeviceKind::Pointer),
    ("Shingle headless touch", InputDeviceKind::Touch),
  ]
  .into_iter()
  .map(|(name, kind)| {
    compositor.add_input_device(InputDeviceInfo {
      name: name.into(),
      vendor: None,
      product: None,
      kind,
    })
  })
  .collect()
}

/// Starts `spawn`'s program, if any, with its arguments in the client
/// `environment`, as the nested backend does.
fn spawn_client(
  spawn: &[OsString],
  environment: &ClientEnvironment,
) -> std::io::Result<Option<Child>> {
  let Some((program, arguments)) = spawn.split_first() else {
    return Ok(None);
  };
  launch_program(program, arguments, environment).map(Some)
}

#[cfg(test)]
mod tests {
  use std::ffi::OsStr;

  use super::*;

  /// The environment of a compositor on `wayland-9` without IPC.
  fn environment() -> ClientEnvironment {
    ClientEnvironment::new(OsStr::new("wayland-9"), None, None)
  }

  #[test]
  fn frames_follow_the_refresh_rate() {
    assert_eq!(frame_interval(60_000), Duration::from_nanos(16_666_666));
    assert_eq!(frame_interval(0), frame_interval(60_000));
    assert_eq!(frame_interval(144_000), Duration::from_nanos(6_944_444));
  }

  #[test]
  fn stipc_devices_are_registered() {
    let path = std::env::temp_dir()
      .join(format!("shingle-headless-test-{}", std::process::id()));
    let mut compositor =
      Compositor::with_socket_path(Config::default(), &path).unwrap();
    let ids = register_stipc_devices(&mut compositor);
    let kinds = ids
      .iter()
      .map(|id| compositor.input_device(*id).unwrap().kind)
      .collect::<Vec<_>>();
    assert_eq!(
      kinds,
      [
        InputDeviceKind::Keyboard,
        InputDeviceKind::Pointer,
        InputDeviceKind::Touch
      ]
    );
    drop(compositor);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn shutdown_frees_the_displays_before_the_session_is_saved() {
    let directory = std::env::temp_dir()
      .join(format!("shingle-headless-shutdown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let config = Config {
      xwayland: shingle::XwaylandSettings {
        mode: shingle_config::XwaylandMode::Lazy,
        socket_base: directory.clone(),
        program: Some("/nonexistent/shingle-test-xwayland".into()),
        ..shingle::XwaylandSettings::default()
      },
      ..Config::default()
    };
    let socket = directory.join("wayland");
    let compositor = Compositor::with_socket_path(config, &socket).unwrap();
    assert!(directory.join(".X0-lock").exists());
    assert!(socket.exists());
    let mut outputs = 0;
    shut_down(compositor, |snapshot| {
      // What a SIGKILL during a slow save would leave behind.
      assert!(!directory.join(".X0-lock").exists());
      assert!(!directory.join(".X11-unix/X0").exists());
      assert!(!socket.exists());
      outputs = snapshot.outputs.len();
      Ok::<(), ()>(())
    })
    .unwrap();
    assert_eq!(outputs, 1);
    std::fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn nothing_to_spawn() {
    assert!(spawn_client(&[], &environment()).unwrap().is_none());
  }

  #[test]
  fn spawned_client_gets_the_socket_and_arguments() {
    let spawn = [
      "sh",
      "-c",
      concat!(
        r#"test "$WAYLAND_DISPLAY" = wayland-9 && test "$0" = x"#,
        r#" && test -z "${DISPLAY+set}${WAYFIRE_SOCKET+set}""#,
      ),
      "x",
    ]
    .map(OsString::from);
    let mut child = spawn_client(&spawn, &environment())
      .expect("sh starts")
      .expect("a child");
    assert!(child.wait().expect("sh exits").success());
  }

  #[test]
  fn missing_program_is_an_error() {
    let spawn = [OsString::from("/nonexistent/shingle-test-program")];
    assert!(spawn_client(&spawn, &environment()).is_err());
  }
}
