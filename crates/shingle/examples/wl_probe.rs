//! A Wayland probe client for Shingle's XWayland tests: one `xdg_toplevel`
//! with a white `wl_shm` buffer at every configured size, which reports
//! its configures and keyboard focus as JSON lines on stdout, so that the
//! harness can check focus moving between X and Wayland windows
//! (docs/design/xwayland.md, section 7).
//!
//! It runs only inside tools/xwayland_test/run.py's or
//! tools/gpu_test/run.py's namespace, which set
//! `SHINGLE_XWAYLAND_TEST_NAMESPACE=1` or `SHINGLE_GPU_TEST_NAMESPACE=1`,
//! so that it never opens a window on the host's Wayland session.
//!
//! ```text
//! wl_probe [APP_ID] [--copy TEXT] [--delay-ms N] [--pattern]
//! ```
//!
//! With `--pattern` the buffer is not white but four quadrants: red top
//! left, green top right, blue bottom left and white bottom right, so
//! that a capture shows where and which way up the buffer was drawn
//! (docs/design/dmabuf.md, DB-2).
//!
//! With `--copy`, the probe also makes a `wl_data_source` offering
//! `text/plain;charset=utf-8` the clipboard, and answers each `send` with
//! TEXT after waiting N milliseconds on a thread of its own, so that the
//! harness can check that a slow Wayland writer stalls only its own
//! transfer (docs/design/xwayland.md, XW-5).
//!
//! Events: `configure` (width, height, activated), `enter`, `leave`,
//! `key` (key, pressed), `copied`, `send` (mime), `sent`, `cancelled`,
//! `close` and `error`.

use std::{
  io::Write,
  os::fd::{AsFd, OwnedFd},
};

use wayland_client::{
  Connection, Dispatch, QueueHandle, WEnum,
  protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager,
    wl_data_offer, wl_data_source, wl_keyboard, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
  },
};
use wayland_protocols::xdg::shell::client::{
  xdg_surface, xdg_toplevel, xdg_wm_base,
};

/// The variables the test harnesses set inside their namespaces.
const NAMESPACE_VARIABLES: [&str; 2] = [
  "SHINGLE_XWAYLAND_TEST_NAMESPACE",
  "SHINGLE_GPU_TEST_NAMESPACE",
];

/// The size drawn before the first configure that names one.
const DEFAULT_SIZE: (i32, i32) = (320, 240);

/// The MIME type the probe's selection offers.
const COPY_TYPE: &str = "text/plain;charset=utf-8";

/// The command line.
#[derive(Debug, Default, Eq, PartialEq)]
struct Options {
  app_id: Option<String>,
  /// Text to offer as the clipboard.
  copy: Option<String>,
  /// How long each `send` waits before writing.
  delay_ms: u64,
  /// Draws the quadrant pattern instead of white.
  pattern: bool,
}

/// The XRGB8888 bytes (B, G, R, X) of pixel (`x`, `y`) of a `width` by
/// `height` quadrant pattern.
fn pattern_pixel(x: i32, y: i32, width: i32, height: i32) -> [u8; 4] {
  match (x < width / 2, y < height / 2) {
    (true, true) => [0, 0, 0xff, 0xff],
    (false, true) => [0, 0xff, 0, 0xff],
    (true, false) => [0xff, 0, 0, 0xff],
    (false, false) => [0xff; 4],
  }
}

/// The bytes of a `width` by `height` XRGB8888 buffer: white, or the
/// quadrant pattern.
fn buffer_bytes(width: i32, height: i32, pattern: bool) -> Vec<u8> {
  if !pattern {
    return vec![0xff; (width * height * 4) as usize];
  }
  (0..height)
    .flat_map(|y| {
      (0..width).flat_map(move |x| pattern_pixel(x, y, width, height))
    })
    .collect()
}

/// Parses `wl_probe [APP_ID] [--copy TEXT] [--delay-ms N]`.
fn parse(arguments: &[String]) -> Result<Options, String> {
  let mut options = Options::default();
  let mut arguments = arguments.iter();
  while let Some(argument) = arguments.next() {
    match argument.as_str() {
      "--copy" => {
        options.copy =
          Some(arguments.next().ok_or("--copy needs a text")?.clone());
      }
      "--delay-ms" => {
        let value = arguments.next().ok_or("--delay-ms needs a number")?;
        options.delay_ms = value
          .parse()
          .map_err(|_| format!("bad --delay-ms {value:?}"))?;
      }
      "--pattern" => options.pattern = true,
      other if other.starts_with("--") => {
        return Err(format!("unknown option {other}"));
      }
      other if options.app_id.is_none() => {
        options.app_id = Some(other.to_owned());
      }
      other => return Err(format!("unexpected argument {other}")),
    }
  }
  Ok(options)
}

/// Prints one JSON line: `{"event": NAME, FIELDS}`. Values are already
/// JSON.
fn report(name: &str, fields: &[(&str, String)]) {
  let mut line = format!("{{\"event\": \"{name}\"");
  for (key, value) in fields {
    line.push_str(&format!(", \"{key}\": {value}"));
  }
  line.push('}');
  let mut stdout = std::io::stdout().lock();
  let _ = writeln!(stdout, "{line}");
  let _ = stdout.flush();
}

/// The probe's globals, objects and what it was last told.
#[derive(Default)]
struct Probe {
  compositor: Option<wl_compositor::WlCompositor>,
  shm: Option<wl_shm::WlShm>,
  wm_base: Option<xdg_wm_base::XdgWmBase>,
  seat: Option<wl_seat::WlSeat>,
  data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
  keyboard: Option<wl_keyboard::WlKeyboard>,
  /// The text the selection offers and how long each send waits.
  copy: Option<(String, u64)>,
  /// Whether buffers show the quadrant pattern.
  pattern: bool,
  surface: Option<wl_surface::WlSurface>,
  /// The size of the pending configure.
  pending: (i32, i32),
  /// Whether the pending configure says activated.
  activated: bool,
  closed: bool,
}

impl Probe {
  /// Attaches and commits a white or patterned buffer of `width` by
  /// `height`.
  fn draw(&self, handle: &QueueHandle<Self>, width: i32, height: i32) {
    let (Some(shm), Some(surface)) = (&self.shm, &self.surface) else {
      return;
    };
    let stride = width * 4;
    let size = stride * height;
    let Ok(fd) = rustix::fs::memfd_create(
      "wl-probe-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    ) else {
      report("error", &[("text", "\"memfd_create failed\"".into())]);
      return;
    };
    let fd: OwnedFd = fd;
    let mut file = std::fs::File::from(fd);
    let _ = file.write_all(&buffer_bytes(width, height, self.pattern));
    let pool = shm.create_pool(file.as_fd(), size, handle, ());
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      stride,
      wl_shm::Format::Xrgb8888,
      handle,
      (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width, height);
    surface.commit();
    pool.destroy();
  }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
  fn event(
    probe: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    let wl_registry::Event::Global {
      name, interface, ..
    } = event
    else {
      return;
    };
    match interface.as_str() {
      "wl_compositor" => {
        probe.compositor = Some(registry.bind(name, 4, handle, ()));
      }
      "wl_shm" => probe.shm = Some(registry.bind(name, 1, handle, ())),
      "xdg_wm_base" => probe.wm_base = Some(registry.bind(name, 1, handle, ())),
      "wl_seat" => probe.seat = Some(registry.bind(name, 1, handle, ())),
      "wl_data_device_manager" => {
        probe.data_device_manager = Some(registry.bind(name, 3, handle, ()));
      }
      _ => {}
    }
  }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
  fn event(
    _: &mut Self,
    wm_base: &xdg_wm_base::XdgWmBase,
    event: xdg_wm_base::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_wm_base::Event::Ping { serial } = event {
      wm_base.pong(serial);
    }
  }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
  fn event(
    probe: &mut Self,
    xdg_surface: &xdg_surface::XdgSurface,
    event: xdg_surface::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    let xdg_surface::Event::Configure { serial } = event else {
      return;
    };
    xdg_surface.ack_configure(serial);
    let (width, height) = probe.pending;
    let width = if width > 0 { width } else { DEFAULT_SIZE.0 };
    let height = if height > 0 { height } else { DEFAULT_SIZE.1 };
    report(
      "configure",
      &[
        ("width", width.to_string()),
        ("height", height.to_string()),
        ("activated", probe.activated.to_string()),
      ],
    );
    probe.draw(handle, width, height);
  }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &xdg_toplevel::XdgToplevel,
    event: xdg_toplevel::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      xdg_toplevel::Event::Configure {
        width,
        height,
        states,
      } => {
        probe.pending = (width, height);
        let activated = xdg_toplevel::State::Activated as u32;
        probe.activated = states.chunks_exact(4).any(|state| {
          u32::from_ne_bytes(state.try_into().unwrap()) == activated
        });
      }
      xdg_toplevel::Event::Close => {
        report("close", &[]);
        probe.closed = true;
      }
      _ => {}
    }
  }
}

impl Dispatch<wl_seat::WlSeat, ()> for Probe {
  fn event(
    probe: &mut Self,
    seat: &wl_seat::WlSeat,
    event: wl_seat::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    if let wl_seat::Event::Capabilities {
      capabilities: WEnum::Value(capabilities),
    } = event
      && capabilities.contains(wl_seat::Capability::Keyboard)
      && probe.keyboard.is_none()
    {
      probe.keyboard = Some(seat.get_keyboard(handle, ()));
    }
  }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Probe {
  fn event(
    _: &mut Self,
    _: &wl_keyboard::WlKeyboard,
    event: wl_keyboard::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wl_keyboard::Event::Enter { .. } => report("enter", &[]),
      wl_keyboard::Event::Leave { .. } => report("leave", &[]),
      wl_keyboard::Event::Key { key, state, .. } => report(
        "key",
        &[
          ("key", key.to_string()),
          (
            "pressed",
            matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed))
              .to_string(),
          ),
        ],
      ),
      _ => {}
    }
  }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_data_source::WlDataSource,
    event: wl_data_source::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wl_data_source::Event::Send { mime_type, fd } => {
        report("send", &[("mime", format!("{mime_type:?}"))]);
        let Some((text, delay_ms)) = probe.copy.clone() else {
          return;
        };
        // The slow writer: only this thread waits.
        std::thread::spawn(move || {
          std::thread::sleep(std::time::Duration::from_millis(delay_ms));
          let mut file = std::fs::File::from(fd);
          let _ = file.write_all(text.as_bytes());
          drop(file);
          report("sent", &[]);
        });
      }
      wl_data_source::Event::Cancelled => report("cancelled", &[]),
      _ => {}
    }
  }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Probe {
  fn event(
    _: &mut Self,
    _: &wl_data_device::WlDataDevice,
    _: wl_data_device::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }

  wayland_client::event_created_child!(Probe, wl_data_device::WlDataDevice, [
    wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
  ]);
}

/// Objects whose events the probe ignores.
macro_rules! ignore_events {
  ($($interface:ty),*) => {
    $(
      impl Dispatch<$interface, ()> for Probe {
        fn event(
          _: &mut Self,
          _: &$interface,
          _: <$interface as wayland_client::Proxy>::Event,
          _: &(),
          _: &Connection,
          _: &QueueHandle<Self>,
        ) {
        }
      }
    )*
  };
}

ignore_events!(
  wl_compositor::WlCompositor,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_buffer::WlBuffer,
  wl_surface::WlSurface,
  wl_data_device_manager::WlDataDeviceManager,
  wl_data_offer::WlDataOffer
);

/// Maps the toplevel, sets the selection with `--copy`, and serves events
/// until it is closed.
fn run(options: &Options) -> Result<(), String> {
  let app_id = options.app_id.as_deref().unwrap_or("wl-probe");
  let connection =
    Connection::connect_to_env().map_err(|error| error.to_string())?;
  let mut queue = connection.new_event_queue();
  let handle = queue.handle();
  connection.display().get_registry(&handle, ());
  let mut probe = Probe {
    copy: options.copy.clone().map(|text| (text, options.delay_ms)),
    pattern: options.pattern,
    ..Probe::default()
  };
  queue
    .roundtrip(&mut probe)
    .map_err(|error| error.to_string())?;
  let (Some(compositor), Some(wm_base)) = (&probe.compositor, &probe.wm_base)
  else {
    return Err("wl_compositor or xdg_wm_base is missing".into());
  };
  let surface = compositor.create_surface(&handle, ());
  let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, ());
  let toplevel = xdg_surface.get_toplevel(&handle, ());
  toplevel.set_app_id(app_id.into());
  toplevel.set_title(app_id.into());
  surface.commit();
  probe.surface = Some(surface);
  // Kept alive for as long as the probe runs.
  let mut _selection = None;
  if probe.copy.is_some() {
    let (Some(manager), Some(seat)) = (&probe.data_device_manager, &probe.seat)
    else {
      return Err("wl_data_device_manager or wl_seat is missing".into());
    };
    let device = manager.get_data_device(seat, &handle, ());
    let source = manager.create_data_source(&handle, ());
    source.offer(COPY_TYPE.into());
    device.set_selection(Some(&source), 0);
    queue
      .roundtrip(&mut probe)
      .map_err(|error| error.to_string())?;
    report("copied", &[]);
    _selection = Some((device, source));
  }
  while !probe.closed {
    queue
      .blocking_dispatch(&mut probe)
      .map_err(|error| error.to_string())?;
  }
  Ok(())
}

fn main() {
  let inside = NAMESPACE_VARIABLES
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|value| value == "1"));
  if !inside {
    eprintln!(
      "wl_probe: refusing to run outside a test harness's namespace \
       (neither {} nor {} is 1)",
      NAMESPACE_VARIABLES[0], NAMESPACE_VARIABLES[1]
    );
    std::process::exit(2);
  }
  let arguments = std::env::args().skip(1).collect::<Vec<_>>();
  let options = match parse(&arguments) {
    Ok(options) => options,
    Err(error) => {
      eprintln!("wl_probe: {error}");
      std::process::exit(2);
    }
  };
  if let Err(error) = run(&options) {
    report("error", &[("text", format!("{error:?}"))]);
    std::process::exit(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
  }

  #[test]
  fn options_parse() {
    assert_eq!(parse(&[]).unwrap(), Options::default());
    assert_eq!(
      parse(&arguments(&["slow", "--copy", "hi", "--delay-ms", "1500"]))
        .unwrap(),
      Options {
        app_id: Some("slow".into()),
        copy: Some("hi".into()),
        delay_ms: 1500,
        pattern: false,
      }
    );
    assert!(parse(&arguments(&["--copy"])).is_err());
    assert!(parse(&arguments(&["--delay-ms", "soon"])).is_err());
    assert!(parse(&arguments(&["--paste"])).is_err());
    assert!(parse(&arguments(&["--pattern"])).unwrap().pattern);
    assert!(parse(&arguments(&["a", "b"])).is_err());
  }

  #[test]
  fn the_pattern_has_four_quadrants() {
    let red = [0, 0, 0xff, 0xff];
    assert_eq!(pattern_pixel(0, 0, 4, 2), red);
    assert_eq!(pattern_pixel(1, 0, 4, 2), red);
    assert_eq!(pattern_pixel(2, 0, 4, 2), [0, 0xff, 0, 0xff]);
    assert_eq!(pattern_pixel(0, 1, 4, 2), [0xff, 0, 0, 0xff]);
    assert_eq!(pattern_pixel(3, 1, 4, 2), [0xff; 4]);
    let bytes = buffer_bytes(4, 2, true);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[..4], red);
    assert_eq!(bytes[28..], [0xff; 4]);
    assert_eq!(buffer_bytes(2, 1, false), [0xff; 8]);
  }
}
