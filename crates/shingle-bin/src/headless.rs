//! The headless backend: the compositor without display or input devices,
//! serving clients and IPC until SIGINT, SIGTERM or a quit request.

use std::{
  collections::BTreeMap,
  error::Error,
  ffi::OsString,
  path::Path,
  process::Child,
  time::{Duration, Instant},
};

use shingle::{
  ClientEnvironment, Compositor, Config, InputDeviceId, InputDeviceInfo,
  InputDeviceKind, OutputInfo,
};
use shingle_config::ShingleConfig;
use shingle_session::{
  SessionSnapshot, SessionStore, launch_autostart, launch_program, launch_saved,
};

/// How many outputs headless Shingle starts with: `--outputs`, else
/// `WLR_HEADLESS_OUTPUTS` as wlroots' headless backend reads it, else 1.
///
/// # Errors
///
/// When `WLR_HEADLESS_OUTPUTS` is set but is not a positive number.
pub fn output_count(
  option: Option<u32>,
  variable: Option<&str>,
) -> Result<u32, crate::cli::UsageError> {
  match (option, variable) {
    (Some(count), _) => Ok(count),
    (None, Some(text)) => crate::cli::output_count(text),
    (None, None) => Ok(1),
  }
}

/// Runs Shingle headless with `config` and `outputs` outputs, named
/// `HEADLESS-1` to `HEADLESS-N` and each sized and placed by its
/// `[output:HEADLESS-N]` entry, and saves the session when it ends.
///
/// With `ipc_path`, Wayfire IPC is served there, and every client finds it
/// as `WAYFIRE_SOCKET`. `spawn`, if not empty, is a program and its
/// arguments to start as a client.
pub fn run(
  yaml: ShingleConfig,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
  outputs: u32,
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
  add_outputs(&mut compositor, outputs)?;
  // Every output is there, so a restored session's sets are settled
  // before any client connects.
  compositor.restore_outputs();
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

/// Adds `HEADLESS-2` to `HEADLESS-count` beside the compositor's first
/// output, each at the size its configuration entry gives.
fn add_outputs(
  compositor: &mut Compositor,
  count: u32,
) -> Result<(), shingle::Error> {
  for number in 2..=count {
    let name = format!("HEADLESS-{number}");
    let physical = compositor.configured_physical_size(&name);
    compositor.add_output(
      OutputInfo {
        name,
        ..OutputInfo::default()
      },
      physical,
    )?;
  }
  Ok(())
}

/// Serves clients until shutdown, reporting a frame for each output at
/// its refresh rate as wlroots' headless backend does for Wayfire, so that
/// clients waiting for frame callbacks or FIFO barriers keep drawing.
/// Without it, a nested Shingle in a headless one drew one frame only.
/// Outputs created over IPC are paced as soon as they appear. Events are
/// drained each turn, which is when IPC watchers get them.
fn run_with_frames(compositor: &mut Compositor) -> Result<(), shingle::Error> {
  let mut pacer = FramePacer::default();
  while !compositor.shutdown_requested() {
    let now = Instant::now();
    let outputs = compositor
      .outputs()
      .into_iter()
      .filter(|output| !output.dark)
      .map(|output| (output.handle.name().to_owned(), output.info.refresh_mhz))
      .collect::<Vec<_>>();
    for name in pacer.due(now, &outputs) {
      compositor.frame_now(&shingle::OutputHandle::new(name));
    }
    let wake = pacer.next_wake().unwrap_or(now + frame_interval(0));
    compositor.poll(Some(wake.saturating_duration_since(Instant::now())))?;
    compositor.drain_events();
  }
  Ok(())
}

/// When each output's next frame is due.
#[derive(Debug, Default)]
struct FramePacer {
  next: BTreeMap<String, Instant>,
}

impl FramePacer {
  /// The outputs, given by name and refresh rate in millihertz, whose
  /// frame is due at `now`; each gets its next deadline one interval
  /// later, or one interval after `now` if it fell behind. A new output's
  /// first frame is one interval away, and outputs that are gone are
  /// forgotten.
  fn due(&mut self, now: Instant, outputs: &[(String, u32)]) -> Vec<String> {
    self
      .next
      .retain(|name, _| outputs.iter().any(|(output, _)| output == name));
    let mut due = Vec::new();
    for (name, refresh_mhz) in outputs {
      let interval = frame_interval(*refresh_mhz);
      let next = self.next.entry(name.clone()).or_insert(now + interval);
      if now >= *next {
        due.push(name.clone());
        *next = (*next + interval).max(now);
      }
    }
    due
  }

  /// The earliest deadline, if any output is paced.
  fn next_wake(&self) -> Option<Instant> {
    self.next.values().min().copied()
  }
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
  fn the_output_count_prefers_the_option() {
    assert_eq!(output_count(Some(3), Some("2")).unwrap(), 3);
    assert_eq!(output_count(None, Some("2")).unwrap(), 2);
    assert_eq!(output_count(None, None).unwrap(), 1);
    assert!(output_count(None, Some("0")).is_err());
  }

  #[test]
  fn each_output_is_paced_at_its_own_rate() {
    let start = Instant::now();
    let outputs = [("A".to_owned(), 60_000), ("B".to_owned(), 30_000)];
    let mut pacer = FramePacer::default();
    assert!(pacer.due(start, &outputs).is_empty());
    let count = |pacer: &mut FramePacer, name: &str| {
      let mut frames = 0;
      for step in 1..=1000 {
        let now = start + Duration::from_millis(step);
        frames += pacer
          .due(now, &outputs)
          .iter()
          .filter(|due| due.as_str() == name)
          .count();
      }
      frames
    };
    let mut first = FramePacer::default();
    first.due(start, &outputs);
    assert_eq!(count(&mut first, "A"), 60);
    let mut second = FramePacer::default();
    second.due(start, &outputs);
    assert_eq!(count(&mut second, "B"), 30);
    // An output that is gone is forgotten; a new one starts one frame on.
    let later = start + Duration::from_secs(2);
    let replaced = [("C".to_owned(), 60_000)];
    assert!(pacer.due(later, &replaced).is_empty());
    assert_eq!(
      pacer.next_wake(),
      Some(later + frame_interval(60_000)),
      "only C is paced"
    );
  }

  #[test]
  fn extra_outputs_are_headless_2_onwards_side_by_side() {
    let path = std::env::temp_dir()
      .join(format!("shingle-headless-outputs-{}", std::process::id()));
    let mut compositor =
      Compositor::with_socket_path(Config::default(), &path).unwrap();
    add_outputs(&mut compositor, 3).unwrap();
    let outputs = compositor
      .outputs()
      .into_iter()
      .map(|output| (output.handle.name().to_owned(), output.rect.x))
      .collect::<Vec<_>>();
    assert_eq!(
      outputs,
      [
        ("HEADLESS-1".to_owned(), 0),
        ("HEADLESS-2".to_owned(), 1280),
        ("HEADLESS-3".to_owned(), 2560),
      ]
    );
    drop(compositor);
    let _ = std::fs::remove_file(path);
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
      outputs = snapshot.wsets.len();
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
