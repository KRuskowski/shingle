//! A Wayland probe client for Shingle's XWayland tests: one `xdg_toplevel`
//! with a white `wl_shm` buffer at every configured size, which reports
//! its configures and keyboard focus as JSON lines on stdout, so that the
//! harness can check focus moving between X and Wayland windows
//! (docs/design/xwayland.md, section 7).
//!
//! It runs only inside tools/xwayland_test/run.py's namespace, which sets
//! `SHINGLE_XWAYLAND_TEST_NAMESPACE=1`, so that it never opens a window
//! on the host's Wayland session.
//!
//! ```text
//! wl_probe [APP_ID]
//! ```
//!
//! Events: `configure` (width, height, activated), `enter`, `leave`,
//! `key` (key, pressed), `close` and `error`.

use std::{
  io::Write,
  os::fd::{AsFd, OwnedFd},
};

use wayland_client::{
  Connection, Dispatch, QueueHandle, WEnum,
  protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
  },
};
use wayland_protocols::xdg::shell::client::{
  xdg_surface, xdg_toplevel, xdg_wm_base,
};

/// The variable the test harness sets inside its namespace.
const NAMESPACE_VARIABLE: &str = "SHINGLE_XWAYLAND_TEST_NAMESPACE";

/// The size drawn before the first configure that names one.
const DEFAULT_SIZE: (i32, i32) = (320, 240);

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
  keyboard: Option<wl_keyboard::WlKeyboard>,
  surface: Option<wl_surface::WlSurface>,
  /// The size of the pending configure.
  pending: (i32, i32),
  /// Whether the pending configure says activated.
  activated: bool,
  closed: bool,
}

impl Probe {
  /// Attaches and commits a white buffer of `width` by `height`.
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
    let _ = file.write_all(&vec![0xff; size as usize]);
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
  wl_surface::WlSurface
);

/// Maps the toplevel and serves events until it is closed.
fn run(app_id: &str) -> Result<(), String> {
  let connection =
    Connection::connect_to_env().map_err(|error| error.to_string())?;
  let mut queue = connection.new_event_queue();
  let handle = queue.handle();
  connection.display().get_registry(&handle, ());
  let mut probe = Probe::default();
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
  while !probe.closed {
    queue
      .blocking_dispatch(&mut probe)
      .map_err(|error| error.to_string())?;
  }
  Ok(())
}

fn main() {
  if std::env::var_os(NAMESPACE_VARIABLE).is_none_or(|value| value != "1") {
    eprintln!(
      "wl_probe: refusing to run outside tools/xwayland_test/run.py \
       ({NAMESPACE_VARIABLE}=1 is not set)"
    );
    std::process::exit(2);
  }
  let app_id = std::env::args().nth(1).unwrap_or_else(|| "wl-probe".into());
  if let Err(error) = run(&app_id) {
    report("error", &[("text", format!("{error:?}"))]);
    std::process::exit(1);
  }
}
