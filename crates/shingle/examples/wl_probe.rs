//! A Wayland probe client for Shingle's XWayland tests: one `xdg_toplevel`
//! with a white `wl_shm` buffer at every configured size, which reports
//! its configures and keyboard focus as JSON lines on stdout, so that the
//! harness can check focus moving between X and Wayland windows
//! (docs/design/xwayland.md, section 7).
//!
//! It runs only inside tools/xwayland_test/run.py's,
//! tools/gpu_test/run.py's, tools/session_test/run.py's or
//! tools/output_test/run.py's namespace, which set
//! `SHINGLE_XWAYLAND_TEST_NAMESPACE=1`, `SHINGLE_GPU_TEST_NAMESPACE=1`,
//! `SHINGLE_SESSION_TEST_NAMESPACE=1` or `SHINGLE_OUTPUT_TEST_NAMESPACE=1`,
//! so that it never opens a window on the host's Wayland session.
//!
//! ```text
//! wl_probe [APP_ID] [--copy TEXT] [--delay-ms N] [--pattern] [--fifo]
//!          [--damage] [--animate FRAMES] [--presentation]
//!          [--report-outputs] [--layer OUTPUT] [--fullscreen]
//!          [--decoration server|client] [--drag TEXT] [--drop]
//! ```
//!
//! With `--drag TEXT`, a press of the first button on the probe starts a
//! drag (`wl_data_device.start_drag`) of a source offering
//! `text/plain;charset=utf-8` with the copy action, which answers each
//! `send` with TEXT. It reports `drag_started`, the source's `drag_target`
//! (mime, or null), `drag_action` (action bits), `drag_drop_performed`,
//! `drag_finished`, `cancelled` and `send` (mime). With `--drop`, the
//! probe accepts drags that offer `text/plain;charset=utf-8` with the
//! copy action; it reports `drop_enter` (types), `drop_leave` and `drop`,
//! reads the data and reports it as `dropped` (text) before it calls
//! `finish`, as XW-6's checks use them (docs/design/xwayland.md).
//!
//! With `--decoration MODE`, the probe asks for server-side or
//! client-side decorations through `zxdg_decoration_manager_v1` before
//! its first commit, and reports each mode the compositor configures
//! (`decoration`, mode `server` or `client`), as WG-6's checks of
//! server-side frames use it (docs/design/wayfire-gaps.md).
//!
//! With `--fullscreen`, once mapped the probe asks to be fullscreen on
//! the output the compositor chooses (`xdg_toplevel.set_fullscreen`
//! without an output) and reports `fullscreen`, as WG-4's checks of
//! wayfire-shell's fullscreen events use it
//! (docs/design/wayfire-gaps.md).
//!
//! With `--report-outputs` the probe binds every `wl_output` and reports
//! the outputs its surface enters and leaves (`output-enter`,
//! `output-leave` with the output's name), its preferred fractional scale
//! in 120ths (`preferred-scale`) and its preferred buffer scale
//! (`buffer-scale`), as MO-3 checks them (docs/design/multi-output.md).
//!
//! With `--layer OUTPUT` the probe maps a wlr-layer-shell panel instead of
//! a toplevel: anchored to the top edge of the output named OUTPUT, 30
//! pixels high with an exclusive zone of 30. It reports each configure
//! (`layer-configure`, width and height) and, when the compositor closes
//! it, `layer-closed`, and then exits with status 0.
//!
//! With `--pattern` the buffer is not white but four quadrants: red top
//! left, green top right, blue bottom left and white bottom right, so
//! that a capture shows where and which way up the buffer was drawn
//! (docs/design/dmabuf.md, DB-2).
//!
//! With `--fifo`, once the toplevel is mapped the probe sends three
//! commits in one flush: buffer `a` with a `wp_fifo_v1` barrier, then
//! buffers `b` and `c`, each waiting for the barrier. `b` is replaced
//! before it is shown, so the compositor must release it without drawing
//! it, and `c` is shown at the next frame. The probe reports `fifo` when
//! it has sent them and `release` (buffer) for each buffer given back
//! (docs/design/dmabuf.md, DB-3).
//!
//! With `--damage`, once the toplevel is mapped the probe commits a
//! 1000x1000 buffer (`damage`) from a pool sealed against shrinking,
//! fully damaged, waits for its release, draws a 10x10 red square at
//! (500, 500) into it and commits it again with only that square
//! damaged. It reports `damage` (step `full`, then `square`) after each
//! commit, and `release` for each release, so that the harness can read
//! from Shingle's buffer trace how many bytes each commit uploaded
//! (docs/design/dmabuf.md, DB-6).
//!
//! With `--animate FRAMES`, once mapped the probe draws FRAMES full-size
//! frames at the size last drawn, each a different grey, fully damaged,
//! from two buffers of a sealed pool, one per frame callback and each
//! only once it was released. It then reports `animated` (frames,
//! milliseconds), for the harness's CPU measurement (DB-6). With
//! `--presentation` too, each frame asks for `wp_presentation` feedback,
//! and the probe reports `presented` (output, the name of its
//! `sync_output`; seq; msec, the presentation time in milliseconds) or
//! `discarded` for each, so that the harness can count the frames each
//! output presents and check that feedback names the output the window
//! is on (docs/design/multi-output.md, MO-5).
//!
//! With `--copy`, the probe also makes a `wl_data_source` offering
//! `text/plain;charset=utf-8` the clipboard, and answers each `send` with
//! TEXT after waiting N milliseconds on a thread of its own, so that the
//! harness can check that a slow Wayland writer stalls only its own
//! transfer (docs/design/xwayland.md, XW-5).
//!
//! Events: `configure` (width, height, activated), `enter`, `leave`,
//! `key` (key, pressed), `copied`, `send` (mime), `sent`, `cancelled`,
//! `fifo`, `fullscreen`, `release` (buffer), `damage` (step), `animated`
//! (frames, milliseconds), `presented` (output, seq, msec), `discarded`,
//! `output-enter` and `output-leave` (output),
//! `preferred-scale` (scale), `buffer-scale` (factor), `layer-configure`
//! (width, height), `layer-closed`, `decoration` (mode), `close` and
//! `error`.

use std::{
  io::Write,
  os::{
    fd::{AsFd, OwnedFd},
    unix::fs::FileExt,
  },
};

use wayland_client::{
  Connection, Dispatch, QueueHandle, WEnum,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device,
    wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard,
    wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
  },
};
use wayland_protocols::{
  wp::{
    fifo::v1::client::{wp_fifo_manager_v1, wp_fifo_v1},
    fractional_scale::v1::client::{
      wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
    },
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
  },
  xdg::{
    decoration::zv1::client::{
      zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
    },
    shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
  },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
  zwlr_layer_shell_v1, zwlr_layer_surface_v1,
};

/// The variables the test harnesses set inside their namespaces.
const NAMESPACE_VARIABLES: [&str; 4] = [
  "SHINGLE_XWAYLAND_TEST_NAMESPACE",
  "SHINGLE_GPU_TEST_NAMESPACE",
  "SHINGLE_SESSION_TEST_NAMESPACE",
  "SHINGLE_OUTPUT_TEST_NAMESPACE",
];

/// The size drawn before the first configure that names one.
const DEFAULT_SIZE: (i32, i32) = (320, 240);

/// The labels of the buffers `--fifo` commits, in order.
const FIFO_BUFFERS: [&str; 3] = ["a", "b", "c"];

/// The size of `--damage`'s buffer.
const DAMAGE_SIZE: i32 = 1000;

/// `--damage`'s square: x, y and side.
const DAMAGE_SQUARE: (i32, i32, i32) = (500, 500, 10);

/// The labels of `--animate`'s two buffers.
const ANIMATE_BUFFERS: [&str; 2] = ["animate-0", "animate-1"];

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
  /// Commits two buffers behind a FIFO barrier once mapped.
  fifo: bool,
  /// Commits a large buffer and then a small damage of it once mapped.
  damage: bool,
  /// Draws this many full-size frames once mapped.
  animate: Option<u32>,
  /// Asks for presentation feedback of each animated frame.
  presentation: bool,
  /// Reports the outputs the surface enters and leaves and its scales.
  report_outputs: bool,
  /// Maps a layer-shell panel on the output of this name instead of a
  /// toplevel.
  layer: Option<String>,
  /// Asks to be fullscreen once mapped.
  fullscreen: bool,
  /// The decoration mode to ask for: server-side when true.
  decoration: Option<bool>,
  /// Text to drag on a press of the first button.
  drag: Option<String>,
  /// Accepts text drags and reports what is dropped.
  drop: bool,
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

/// Parses `wl_probe [APP_ID] [--copy TEXT] [--delay-ms N] [--pattern]
/// [--fifo] [--damage] [--animate FRAMES] [--presentation]
/// [--report-outputs] [--layer OUTPUT] [--fullscreen]
/// [--decoration server|client]`.
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
      "--report-outputs" => options.report_outputs = true,
      "--presentation" => options.presentation = true,
      "--layer" => {
        options.layer =
          Some(arguments.next().ok_or("--layer needs an output")?.clone());
      }
      "--fifo" => options.fifo = true,
      "--fullscreen" => options.fullscreen = true,
      "--decoration" => {
        options.decoration = Some(match arguments.next().map(String::as_str) {
          Some("server") => true,
          Some("client") => false,
          _ => return Err("--decoration needs server or client".into()),
        });
      }
      "--damage" => options.damage = true,
      "--drag" => {
        options.drag =
          Some(arguments.next().ok_or("--drag needs a text")?.clone());
      }
      "--drop" => options.drop = true,
      "--animate" => {
        let value = arguments.next().ok_or("--animate needs a number")?;
        options.animate = Some(
          value
            .parse()
            .map_err(|_| format!("bad --animate {value:?}"))?,
        );
      }
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
  fifo_manager: Option<wp_fifo_manager_v1::WpFifoManagerV1>,
  fractional_manager:
    Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
  layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
  decoration_manager:
    Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
  /// Every `wl_output` bound, with its name once it was told.
  outputs: Vec<(wl_output::WlOutput, Option<String>)>,
  /// Whether the toplevel's surface reports outputs and scales.
  report_outputs: bool,
  /// `wp_presentation`, bound with `--presentation`.
  presentation: Option<wp_presentation::WpPresentation>,
  /// Whether `--presentation` asked for it, which binds the outputs too.
  wants_presentation: bool,
  /// The `sync_output` of the feedback being reported.
  sync_output: Option<String>,
  keyboard: Option<wl_keyboard::WlKeyboard>,
  /// The text the selection offers and how long each send waits.
  copy: Option<(String, u64)>,
  /// Whether buffers show the quadrant pattern.
  pattern: bool,
  surface: Option<wl_surface::WlSurface>,
  /// The size of the pending configure.
  pending: (i32, i32),
  /// The size last drawn, if anything was.
  drawn: Option<(i32, i32)>,
  /// How many buffers were drawn for configures.
  draws: u32,
  /// Whether the pending configure says activated.
  activated: bool,
  closed: bool,
  /// Labelled buffers released since they were last committed.
  released: Vec<&'static str>,
  /// Whether the last frame callback is done.
  frame_done: bool,
  /// The pointer, with `--drag` or `--drop`.
  pointer: Option<wl_pointer::WlPointer>,
  /// The data device, with `--drag` or `--drop`.
  device: Option<wl_data_device::WlDataDevice>,
  /// The text `--drag` drags.
  drag_text: Option<String>,
  /// The source of the drag started, if any.
  drag_source: Option<wl_data_source::WlDataSource>,
  /// Whether `--drop` accepts drags.
  drop_target: bool,
  /// The types each data offer announced.
  offer_types: Vec<(wl_data_offer::WlDataOffer, Vec<String>)>,
  /// The offer of the drag over the probe.
  drag_offer: Option<wl_data_offer::WlDataOffer>,
}

impl Probe {
  /// A pool holding `count` white or patterned buffers of `width` by
  /// `height`, one after the other, or None after reporting an error.
  fn pool(
    &self,
    handle: &QueueHandle<Self>,
    width: i32,
    height: i32,
    count: i32,
  ) -> Option<wl_shm_pool::WlShmPool> {
    let shm = self.shm.as_ref()?;
    let Ok(fd) = rustix::fs::memfd_create(
      "wl-probe-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    ) else {
      report("error", &[("text", "\"memfd_create failed\"".into())]);
      return None;
    };
    let fd: OwnedFd = fd;
    let mut file = std::fs::File::from(fd);
    let bytes = buffer_bytes(width, height, self.pattern);
    for _ in 0..count {
      let _ = file.write_all(&bytes);
    }
    Some(shm.create_pool(file.as_fd(), width * height * 4 * count, handle, ()))
  }

  /// A pool of `size` bytes in a memfd sealed against shrinking, and the
  /// file to draw into it through.
  fn sealed_pool(
    &self,
    handle: &QueueHandle<Self>,
    size: i32,
  ) -> Result<(wl_shm_pool::WlShmPool, std::fs::File), String> {
    let shm = self.shm.as_ref().ok_or("wl_shm is missing")?;
    let fd = rustix::fs::memfd_create(
      "wl-probe-sealed",
      rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|error| error.to_string())?;
    rustix::fs::ftruncate(&fd, size as u64)
      .map_err(|error| error.to_string())?;
    rustix::fs::fcntl_add_seals(
      &fd,
      rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
    )
    .map_err(|error| error.to_string())?;
    let pool = shm.create_pool(fd.as_fd(), size, handle, ());
    Ok((pool, std::fs::File::from(fd)))
  }

  /// Attaches and commits a white or patterned buffer of `width` by
  /// `height`.
  fn draw(&mut self, handle: &QueueHandle<Self>, width: i32, height: i32) {
    let Some(surface) = self.surface.clone() else {
      return;
    };
    let Some(pool) = self.pool(handle, width, height, 1) else {
      return;
    };
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Xrgb8888,
      handle,
      (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width, height);
    surface.commit();
    pool.destroy();
    self.drawn = Some((width, height));
    self.draws += 1;
  }

  /// Sends `--fifo`'s three commits ([`FIFO_BUFFERS`]) at the size last
  /// drawn. They go out in one flush, so the compositor reads them in one
  /// dispatch, before any frame can clear the barrier.
  fn fifo_commits(
    &self,
    handle: &QueueHandle<Self>,
    fifo: &wp_fifo_v1::WpFifoV1,
  ) -> Result<(), String> {
    let (Some(surface), Some((width, height))) = (&self.surface, self.drawn)
    else {
      return Err("nothing was drawn before the FIFO commits".into());
    };
    let pool = self
      .pool(handle, width, height, FIFO_BUFFERS.len() as i32)
      .ok_or("no wl_shm pool for the FIFO commits")?;
    for (index, label) in FIFO_BUFFERS.into_iter().enumerate() {
      let buffer = pool.create_buffer(
        width * height * 4 * index as i32,
        width,
        height,
        width * 4,
        wl_shm::Format::Xrgb8888,
        handle,
        label,
      );
      surface.attach(Some(&buffer), 0, 0);
      surface.damage_buffer(0, 0, width, height);
      if index == 0 {
        fifo.set_barrier();
      } else {
        fifo.wait_barrier();
      }
      surface.commit();
    }
    pool.destroy();
    Ok(())
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
      name,
      interface,
      version,
    } = event
    else {
      return;
    };
    match interface.as_str() {
      "wl_compositor" => {
        // Version 6 has `preferred_buffer_scale`.
        let version = if probe.report_outputs {
          version.min(6)
        } else {
          4
        };
        probe.compositor = Some(registry.bind(name, version, handle, ()));
      }
      "wl_output" if probe.report_outputs || probe.wants_presentation => {
        let output = registry.bind(name, version.min(4), handle, ());
        probe.outputs.push((output, None));
      }
      "wp_fractional_scale_manager_v1" => {
        probe.fractional_manager = Some(registry.bind(name, 1, handle, ()));
      }
      "zwlr_layer_shell_v1" => {
        probe.layer_shell =
          Some(registry.bind(name, version.min(4), handle, ()));
      }
      "zxdg_decoration_manager_v1" => {
        probe.decoration_manager = Some(registry.bind(name, 1, handle, ()));
      }
      "wl_shm" => probe.shm = Some(registry.bind(name, 1, handle, ())),
      "xdg_wm_base" => probe.wm_base = Some(registry.bind(name, 1, handle, ())),
      "wl_seat" => probe.seat = Some(registry.bind(name, 1, handle, ())),
      "wl_data_device_manager" => {
        probe.data_device_manager = Some(registry.bind(name, 3, handle, ()));
      }
      "wp_fifo_manager_v1" => {
        probe.fifo_manager = Some(registry.bind(name, 1, handle, ()));
      }
      "wp_presentation" if probe.wants_presentation => {
        probe.presentation = Some(registry.bind(name, 1, handle, ()));
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
    let wl_seat::Event::Capabilities {
      capabilities: WEnum::Value(capabilities),
    } = event
    else {
      return;
    };
    if capabilities.contains(wl_seat::Capability::Keyboard)
      && probe.keyboard.is_none()
    {
      probe.keyboard = Some(seat.get_keyboard(handle, ()));
    }
    if capabilities.contains(wl_seat::Capability::Pointer)
      && probe.pointer.is_none()
      && (probe.drag_text.is_some() || probe.drop_target)
    {
      probe.pointer = Some(seat.get_pointer(handle, ()));
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

impl Dispatch<wl_pointer::WlPointer, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_pointer::WlPointer,
    event: wl_pointer::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    let wl_pointer::Event::Button {
      serial,
      button: 0x110,
      state: WEnum::Value(wl_pointer::ButtonState::Pressed),
      ..
    } = event
    else {
      return;
    };
    let (Some(text), None) = (&probe.drag_text, &probe.drag_source) else {
      return;
    };
    let (Some(manager), Some(device), Some(surface)) =
      (&probe.data_device_manager, &probe.device, &probe.surface)
    else {
      return;
    };
    let source = manager.create_data_source(handle, ());
    source.offer(COPY_TYPE.into());
    source.set_actions(wl_data_device_manager::DndAction::Copy);
    device.start_drag(Some(&source), surface, None, serial);
    report("drag_started", &[("text", format!("{text:?}"))]);
    probe.drag_source = Some(source);
  }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for Probe {
  fn event(
    probe: &mut Self,
    source: &wl_data_source::WlDataSource,
    event: wl_data_source::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let dragged = probe.drag_source.as_ref() == Some(source);
    match event {
      wl_data_source::Event::Send { mime_type, fd } => {
        report("send", &[("mime", format!("{mime_type:?}"))]);
        let copy = probe.copy.clone();
        let text = if dragged {
          probe.drag_text.clone().map(|text| (text, 0))
        } else {
          copy
        };
        let Some((text, delay_ms)) = text else {
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
      wl_data_source::Event::Cancelled => {
        report("cancelled", &[]);
        if dragged {
          source.destroy();
          probe.drag_source = None;
        }
      }
      wl_data_source::Event::Target { mime_type } => report(
        "drag_target",
        &[(
          "mime",
          mime_type.map_or("null".into(), |mime| format!("{mime:?}")),
        )],
      ),
      wl_data_source::Event::Action {
        dnd_action: WEnum::Value(action),
      } => report("drag_action", &[("action", action.bits().to_string())]),
      wl_data_source::Event::DndDropPerformed => {
        report("drag_drop_performed", &[]);
      }
      wl_data_source::Event::DndFinished => {
        report("drag_finished", &[]);
        source.destroy();
        probe.drag_source = None;
      }
      _ => {}
    }
  }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_data_device::WlDataDevice,
    event: wl_data_device::Event,
    _: &(),
    connection: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if !probe.drop_target {
      return;
    }
    match event {
      wl_data_device::Event::Enter { serial, id, .. } => {
        let Some(offer) = id else {
          return;
        };
        let types = probe
          .offer_types
          .iter()
          .find(|(known, _)| *known == offer)
          .map(|(_, types)| types.clone())
          .unwrap_or_default();
        report("drop_enter", &[("types", format!("{types:?}"))]);
        if types.iter().any(|mime| mime == COPY_TYPE) {
          offer.accept(serial, Some(COPY_TYPE.into()));
          offer.set_actions(
            wl_data_device_manager::DndAction::Copy,
            wl_data_device_manager::DndAction::Copy,
          );
        } else {
          offer.accept(serial, None);
        }
        probe.drag_offer = Some(offer);
      }
      wl_data_device::Event::Leave => {
        report("drop_leave", &[]);
        if let Some(offer) = probe.drag_offer.take() {
          probe.offer_types.retain(|(known, _)| *known != offer);
          offer.destroy();
        }
      }
      wl_data_device::Event::Drop => {
        report("drop", &[]);
        let Some(offer) = probe.drag_offer.take() else {
          return;
        };
        let Ok((read, write)) = rustix::pipe::pipe() else {
          report("error", &[("text", "\"pipe\"".into())]);
          return;
        };
        offer.receive(COPY_TYPE.into(), write.as_fd());
        drop(write);
        let _ = connection.flush();
        // The data comes from the compositor, which does not wait for
        // this probe, so reading it here cannot deadlock.
        let mut text = String::new();
        let mut file = std::fs::File::from(read);
        let read = std::io::Read::read_to_string(&mut file, &mut text);
        report(
          "dropped",
          &[(
            "text",
            match read {
              Ok(_) => format!("{text:?}"),
              Err(_) => "null".into(),
            },
          )],
        );
        offer.finish();
        offer.destroy();
        probe.offer_types.retain(|(known, _)| *known != offer);
      }
      _ => {}
    }
  }

  wayland_client::event_created_child!(Probe, wl_data_device::WlDataDevice, [
    wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
  ]);
}

impl Dispatch<wl_buffer::WlBuffer, &'static str> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    label: &&'static str,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_buffer::Event::Release = event {
      probe.released.push(label);
      // Animation frames would be hundreds of lines.
      if !ANIMATE_BUFFERS.contains(label) {
        report("release", &[("buffer", format!("{label:?}"))]);
      }
    }
  }
}

impl Dispatch<wl_callback::WlCallback, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { .. } = event {
      probe.frame_done = true;
    }
  }
}

impl Probe {
  /// The name of `output`, if it was told.
  fn output_name(&self, output: &wl_output::WlOutput) -> String {
    self
      .outputs
      .iter()
      .find(|(candidate, _)| candidate == output)
      .and_then(|(_, name)| name.clone())
      .unwrap_or_else(|| "?".into())
  }
}

impl Dispatch<wl_output::WlOutput, ()> for Probe {
  fn event(
    probe: &mut Self,
    output: &wl_output::WlOutput,
    event: wl_output::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_output::Event::Name { name } = event
      && let Some((_, known)) = probe
        .outputs
        .iter_mut()
        .find(|(candidate, _)| candidate == output)
    {
      *known = Some(name);
    }
  }
}

impl Dispatch<wl_surface::WlSurface, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_surface::WlSurface,
    event: wl_surface::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if !probe.report_outputs {
      return;
    }
    match event {
      wl_surface::Event::Enter { output } => report(
        "output-enter",
        &[("output", format!("{:?}", probe.output_name(&output)))],
      ),
      wl_surface::Event::Leave { output } => report(
        "output-leave",
        &[("output", format!("{:?}", probe.output_name(&output)))],
      ),
      wl_surface::Event::PreferredBufferScale { factor } => {
        report("buffer-scale", &[("factor", factor.to_string())]);
      }
      _ => {}
    }
  }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for Probe {
  fn event(
    _: &mut Self,
    _: &wp_fractional_scale_v1::WpFractionalScaleV1,
    event: wp_fractional_scale_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
      report("preferred-scale", &[("scale", scale.to_string())]);
    }
  }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Probe {
  fn event(
    probe: &mut Self,
    layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    event: zwlr_layer_surface_v1::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_layer_surface_v1::Event::Configure {
        serial,
        width,
        height,
      } => {
        layer.ack_configure(serial);
        report(
          "layer-configure",
          &[("width", width.to_string()), ("height", height.to_string())],
        );
        probe.draw(handle, width.max(1) as i32, height.max(1) as i32);
      }
      zwlr_layer_surface_v1::Event::Closed => {
        report("layer-closed", &[]);
        probe.closed = true;
      }
      _ => {}
    }
  }
}

/// `--layer OUTPUT`: a panel on the output called OUTPUT, served until the
/// compositor closes it.
fn run_layer(
  queue: &mut wayland_client::EventQueue<Probe>,
  probe: &mut Probe,
  output: &str,
) -> Result<(), String> {
  let handle = queue.handle();
  let (Some(compositor), Some(shell)) = (&probe.compositor, &probe.layer_shell)
  else {
    return Err("wl_compositor or zwlr_layer_shell_v1 is missing".into());
  };
  let Some((target, _)) = probe
    .outputs
    .iter()
    .find(|(_, name)| name.as_deref() == Some(output))
  else {
    return Err(format!("no output called {output}"));
  };
  let surface = compositor.create_surface(&handle, ());
  let layer = shell.get_layer_surface(
    &surface,
    Some(target),
    zwlr_layer_shell_v1::Layer::Top,
    "wl-probe".into(),
    &handle,
    (),
  );
  layer.set_anchor(
    zwlr_layer_surface_v1::Anchor::Top
      | zwlr_layer_surface_v1::Anchor::Left
      | zwlr_layer_surface_v1::Anchor::Right,
  );
  layer.set_size(0, 30);
  layer.set_exclusive_zone(30);
  probe.surface = Some(surface.clone());
  surface.commit();
  while !probe.closed {
    queue
      .blocking_dispatch(probe)
      .map_err(|error| error.to_string())?;
  }
  layer.destroy();
  surface.destroy();
  queue.roundtrip(probe).map_err(|error| error.to_string())?;
  Ok(())
}

/// Dispatches until `done(probe)` holds or the toplevel is closed.
fn dispatch_until(
  queue: &mut wayland_client::EventQueue<Probe>,
  probe: &mut Probe,
  done: impl Fn(&Probe) -> bool,
) -> Result<(), String> {
  while !done(probe) && !probe.closed {
    queue
      .blocking_dispatch(probe)
      .map_err(|error| error.to_string())?;
  }
  Ok(())
}

/// Waits until the toplevel is mapped and drawn at its final size: the
/// first draw, then round trips until one brings no new configure.
fn wait_mapped(
  queue: &mut wayland_client::EventQueue<Probe>,
  probe: &mut Probe,
) -> Result<(), String> {
  dispatch_until(queue, probe, |probe| probe.drawn.is_some())?;
  loop {
    let draws = probe.draws;
    queue.roundtrip(probe).map_err(|error| error.to_string())?;
    if probe.draws == draws {
      return Ok(());
    }
  }
}

/// `--damage`: a full commit of a 1000x1000 buffer, then a 10x10 one.
fn damage_commits(
  queue: &mut wayland_client::EventQueue<Probe>,
  probe: &mut Probe,
) -> Result<(), String> {
  let handle = queue.handle();
  let surface = probe.surface.clone().ok_or("no surface")?;
  let size = DAMAGE_SIZE * DAMAGE_SIZE * 4;
  let (pool, file) = probe.sealed_pool(&handle, size)?;
  file
    .write_all_at(&vec![0xff; size as usize], 0)
    .map_err(|error| error.to_string())?;
  let buffer = pool.create_buffer(
    0,
    DAMAGE_SIZE,
    DAMAGE_SIZE,
    DAMAGE_SIZE * 4,
    wl_shm::Format::Xrgb8888,
    &handle,
    "damage",
  );
  probe.released.clear();
  surface.attach(Some(&buffer), 0, 0);
  surface.damage_buffer(0, 0, DAMAGE_SIZE, DAMAGE_SIZE);
  surface.commit();
  queue.flush().map_err(|error| error.to_string())?;
  report("damage", &[("step", "\"full\"".into())]);
  dispatch_until(queue, probe, |probe| probe.released.contains(&"damage"))?;
  probe.released.clear();
  let (x, y, side) = DAMAGE_SQUARE;
  let red = [0, 0, 0xff, 0xff].repeat(side as usize);
  for row in y..y + side {
    let offset = (row * DAMAGE_SIZE + x) * 4;
    file
      .write_all_at(&red, offset as u64)
      .map_err(|error| error.to_string())?;
  }
  surface.attach(Some(&buffer), 0, 0);
  surface.damage_buffer(x, y, side, side);
  surface.commit();
  queue.flush().map_err(|error| error.to_string())?;
  report("damage", &[("step", "\"square\"".into())]);
  dispatch_until(queue, probe, |probe| probe.released.contains(&"damage"))?;
  pool.destroy();
  Ok(())
}

/// `--animate FRAMES`: full-size frames, one per frame callback.
fn animate(
  queue: &mut wayland_client::EventQueue<Probe>,
  probe: &mut Probe,
  frames: u32,
) -> Result<(), String> {
  let handle = queue.handle();
  let surface = probe.surface.clone().ok_or("no surface")?;
  let (width, height) = probe.drawn.ok_or("nothing was drawn")?;
  let frame_size = width * height * 4;
  let (pool, file) = probe.sealed_pool(&handle, frame_size * 2)?;
  let buffers = ANIMATE_BUFFERS
    .iter()
    .enumerate()
    .map(|(index, label)| {
      pool.create_buffer(
        frame_size * index as i32,
        width,
        height,
        width * 4,
        wl_shm::Format::Xrgb8888,
        &handle,
        *label,
      )
    })
    .collect::<Vec<_>>();
  probe.released = ANIMATE_BUFFERS.to_vec();
  probe.frame_done = true;
  let started = std::time::Instant::now();
  let mut pixels = vec![0; frame_size as usize];
  for frame in 0..frames {
    dispatch_until(queue, probe, |probe| {
      probe.frame_done && !probe.released.is_empty()
    })?;
    if probe.closed {
      break;
    }
    let label = probe.released.remove(0);
    let index = ANIMATE_BUFFERS.iter().position(|name| *name == label);
    let index = index.ok_or("an unknown buffer was released")?;
    pixels.fill((frame % 200) as u8 + 30);
    file
      .write_all_at(&pixels, (frame_size * index as i32) as u64)
      .map_err(|error| error.to_string())?;
    probe.frame_done = false;
    if let Some(presentation) = &probe.presentation {
      presentation.feedback(&surface, &handle, ());
    }
    surface.frame(&handle, ());
    surface.attach(Some(&buffers[index]), 0, 0);
    surface.damage_buffer(0, 0, width, height);
    surface.commit();
    queue.flush().map_err(|error| error.to_string())?;
  }
  report(
    "animated",
    &[
      ("frames", frames.to_string()),
      ("milliseconds", started.elapsed().as_millis().to_string()),
    ],
  );
  pool.destroy();
  Ok(())
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &wp_presentation_feedback::WpPresentationFeedback,
    event: wp_presentation_feedback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wp_presentation_feedback::Event::SyncOutput { output } => {
        probe.sync_output = Some(probe.output_name(&output));
      }
      wp_presentation_feedback::Event::Presented {
        tv_sec_hi,
        tv_sec_lo,
        tv_nsec,
        seq_hi,
        seq_lo,
        ..
      } => {
        let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
        let msec = seconds * 1000 + u64::from(tv_nsec) / 1_000_000;
        let seq = (u64::from(seq_hi) << 32) | u64::from(seq_lo);
        let output = probe.sync_output.take().unwrap_or_else(|| "?".into());
        report(
          "presented",
          &[
            ("output", format!("{output:?}")),
            ("seq", seq.to_string()),
            ("msec", msec.to_string()),
          ],
        );
      }
      wp_presentation_feedback::Event::Discarded => {
        probe.sync_output = None;
        report("discarded", &[]);
      }
      _ => {}
    }
  }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()>
  for Probe
{
  fn event(
    _: &mut Self,
    _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
    event: zxdg_toplevel_decoration_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
      let mode = match mode {
        WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide) => "server",
        _ => "client",
      };
      report("decoration", &[("mode", format!("\"{mode}\""))]);
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
  wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
  zwlr_layer_shell_v1::ZwlrLayerShellV1,
  wl_data_device_manager::WlDataDeviceManager,
  wp_fifo_manager_v1::WpFifoManagerV1,
  wp_fifo_v1::WpFifoV1,
  wp_presentation::WpPresentation,
  zxdg_decoration_manager_v1::ZxdgDecorationManagerV1
);

impl Dispatch<wl_data_offer::WlDataOffer, ()> for Probe {
  fn event(
    probe: &mut Self,
    offer: &wl_data_offer::WlDataOffer,
    event: wl_data_offer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_data_offer::Event::Offer { mime_type } = event {
      match probe
        .offer_types
        .iter_mut()
        .find(|(known, _)| known == offer)
      {
        Some((_, types)) => types.push(mime_type),
        None => probe.offer_types.push((offer.clone(), vec![mime_type])),
      }
    }
  }
}

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
    report_outputs: options.report_outputs || options.layer.is_some(),
    wants_presentation: options.presentation,
    drag_text: options.drag.clone(),
    drop_target: options.drop,
    ..Probe::default()
  };
  queue
    .roundtrip(&mut probe)
    .map_err(|error| error.to_string())?;
  if probe.report_outputs || probe.wants_presentation {
    // The outputs' names.
    queue
      .roundtrip(&mut probe)
      .map_err(|error| error.to_string())?;
  }
  if let Some(output) = &options.layer {
    return run_layer(&mut queue, &mut probe, output);
  }
  let (Some(compositor), Some(wm_base)) = (&probe.compositor, &probe.wm_base)
  else {
    return Err("wl_compositor or xdg_wm_base is missing".into());
  };
  let surface = compositor.create_surface(&handle, ());
  // Kept alive for as long as the probe runs.
  let _fractional = probe
    .fractional_manager
    .as_ref()
    .filter(|_| options.report_outputs)
    .map(|manager| manager.get_fractional_scale(&surface, &handle, ()));
  let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, ());
  let toplevel = xdg_surface.get_toplevel(&handle, ());
  toplevel.set_app_id(app_id.into());
  toplevel.set_title(app_id.into());
  // Kept alive for as long as the probe runs.
  let _decoration = match options.decoration {
    Some(server) => {
      let manager = probe
        .decoration_manager
        .as_ref()
        .ok_or("zxdg_decoration_manager_v1 is missing")?;
      let decoration = manager.get_toplevel_decoration(&toplevel, &handle, ());
      decoration.set_mode(if server {
        zxdg_toplevel_decoration_v1::Mode::ServerSide
      } else {
        zxdg_toplevel_decoration_v1::Mode::ClientSide
      });
      Some(decoration)
    }
    None => None,
  };
  surface.commit();
  probe.surface = Some(surface.clone());
  if options.fullscreen {
    wait_mapped(&mut queue, &mut probe)?;
    toplevel.set_fullscreen(None);
    queue.flush().map_err(|error| error.to_string())?;
    report("fullscreen", &[]);
  }
  // Kept alive for as long as the probe runs.
  let mut _fifo = None;
  if options.fifo {
    let manager = probe
      .fifo_manager
      .clone()
      .ok_or("wp_fifo_manager_v1 is missing")?;
    let fifo = manager.get_fifo(&surface, &handle, ());
    // Mapping may bring a configure with the tiled size, which is drawn
    // too, so no draw can follow the FIFO commits.
    wait_mapped(&mut queue, &mut probe)?;
    probe.fifo_commits(&handle, &fifo)?;
    queue.flush().map_err(|error| error.to_string())?;
    report("fifo", &[]);
    _fifo = Some(fifo);
  }
  if options.damage {
    wait_mapped(&mut queue, &mut probe)?;
    damage_commits(&mut queue, &mut probe)?;
  }
  if let Some(frames) = options.animate {
    wait_mapped(&mut queue, &mut probe)?;
    animate(&mut queue, &mut probe, frames)?;
  }
  if options.drag.is_some() || options.drop {
    let (Some(manager), Some(seat)) = (&probe.data_device_manager, &probe.seat)
    else {
      return Err("wl_data_device_manager or wl_seat is missing".into());
    };
    probe.device = Some(manager.get_data_device(seat, &handle, ()));
  }
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
       (none of {} is 1)",
      NAMESPACE_VARIABLES.join(", ")
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
        fifo: false,
        damage: false,
        animate: None,
        presentation: false,
        report_outputs: false,
        layer: None,
        fullscreen: false,
        decoration: None,
        drag: None,
        drop: false,
      }
    );
    let options = parse(&arguments(&["--drag", "text", "--drop"])).unwrap();
    assert_eq!(options.drag.as_deref(), Some("text"));
    assert!(options.drop);
    assert!(parse(&arguments(&["--drag"])).is_err());
    assert!(parse(&arguments(&["--fullscreen"])).unwrap().fullscreen);
    assert_eq!(
      parse(&arguments(&["--decoration", "server"]))
        .unwrap()
        .decoration,
      Some(true)
    );
    assert_eq!(
      parse(&arguments(&["--decoration", "client"]))
        .unwrap()
        .decoration,
      Some(false)
    );
    assert!(parse(&arguments(&["--decoration", "both"])).is_err());
    assert!(
      parse(&arguments(&["--report-outputs"]))
        .unwrap()
        .report_outputs
    );
    assert_eq!(
      parse(&arguments(&["--layer", "HEADLESS-2"])).unwrap().layer,
      Some("HEADLESS-2".into())
    );
    assert!(parse(&arguments(&["--layer"])).is_err());
    assert!(parse(&arguments(&["--copy"])).is_err());
    assert!(parse(&arguments(&["--delay-ms", "soon"])).is_err());
    assert!(parse(&arguments(&["--paste"])).is_err());
    assert!(parse(&arguments(&["--pattern"])).unwrap().pattern);
    assert!(parse(&arguments(&["--fifo"])).unwrap().fifo);
    assert!(parse(&arguments(&["--damage"])).unwrap().damage);
    assert_eq!(
      parse(&arguments(&["--animate", "60"])).unwrap().animate,
      Some(60)
    );
    assert!(parse(&arguments(&["--presentation"])).unwrap().presentation);
    assert!(parse(&arguments(&["--animate", "many"])).is_err());
    assert!(parse(&arguments(&["--animate"])).is_err());
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
