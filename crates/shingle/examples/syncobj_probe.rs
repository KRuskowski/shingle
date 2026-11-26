//! A Wayland probe client for DB-5's stalled-acquire check
//! (docs/design/dmabuf.md, DB-5): one client with two toplevels, of
//! which one waits forever on a `linux-drm-syncobj-v1` acquire point
//! while the other keeps drawing.
//!
//! It runs only inside tools/gpu_test/run.py's namespace, which sets
//! `SHINGLE_GPU_TEST_NAMESPACE=1`, so that it never opens a window on the
//! host's Wayland session. It opens the render node, never a card node,
//! for its timelines, and `/dev/udmabuf` for its DMA-BUFs.
//!
//! ```text
//! syncobj_probe [--seconds N] [--render-node PATH]
//! ```
//!
//! Both toplevels map with `wl_shm` buffers: `syncobj-stalled` with the
//! four-quadrant pattern of `wl_probe --pattern`, `syncobj-live` in red.
//! Then `syncobj-stalled` gets a syncobj surface and commits two linear
//! XRGB8888 DMA-BUFs with frame callbacks, with acquire points 1 and 2 on
//! a timeline nobody signals and release points 1 and 2 on another. For N
//! seconds (3 by default) `syncobj-live` redraws on every frame callback,
//! alternating red and blue. The probe then reports how many frames each
//! surface got, which DMA-BUFs came back and which release points were
//! signalled, destroys `syncobj-stalled` and reports the same again
//! after a second.
//!
//! Events, as JSON lines on stdout: `mapped`, `stalled`, `live` (frames,
//! stalled_frames, released, signalled), `destroyed` (released,
//! signalled) and `error`.

use std::{
  io::Write,
  os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
  time::{Duration, Instant},
};

use drm::control::{Device as _, syncobj};
use wayland_client::{
  Connection, Dispatch, QueueHandle,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
  },
};
use wayland_protocols::{
  wp::{
    linux_dmabuf::zv1::client::{
      zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
    },
    linux_drm_syncobj::v1::client::{
      wp_linux_drm_syncobj_manager_v1, wp_linux_drm_syncobj_surface_v1,
      wp_linux_drm_syncobj_timeline_v1,
    },
  },
  xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

/// The variable the GPU harness sets inside its namespace.
const NAMESPACE_VARIABLE: &str = "SHINGLE_GPU_TEST_NAMESPACE";
/// The size drawn before a configure names one.
const DEFAULT_SIZE: (i32, i32) = (320, 240);
/// The DMA-BUFs' size and format: 64 by 64 linear XRGB8888.
const DMABUF_SIZE: i32 = 64;
const XR24: u32 = u32::from_le_bytes(*b"XR24");
/// `_IOW('u', 0x42, struct udmabuf_create)`, from linux/udmabuf.h.
const UDMABUF_CREATE: libc::c_ulong = 0x4018_7542;
/// `UDMABUF_FLAGS_CLOEXEC`.
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// The command line.
#[derive(Debug, Eq, PartialEq)]
struct Options {
  seconds: u64,
  render_node: String,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      seconds: 3,
      render_node: "/dev/dri/renderD128".into(),
    }
  }
}

/// Parses `syncobj_probe [--seconds N] [--render-node PATH]`. Only a
/// render node is accepted.
fn parse(arguments: &[String]) -> Result<Options, String> {
  let mut options = Options::default();
  let mut arguments = arguments.iter();
  while let Some(argument) = arguments.next() {
    match argument.as_str() {
      "--seconds" => {
        let value = arguments.next().ok_or("--seconds needs a number")?;
        options.seconds = value
          .parse()
          .map_err(|_| format!("bad --seconds {value:?}"))?;
      }
      "--render-node" => {
        options.render_node = arguments
          .next()
          .ok_or("--render-node needs a path")?
          .clone();
      }
      other => return Err(format!("unexpected argument {other}")),
    }
  }
  let name = options.render_node.rsplit('/').next().unwrap_or_default();
  if !name.starts_with("renderD") {
    return Err(format!("{} is not a render node", options.render_node));
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

/// A JSON list of strings.
fn json_list(items: &[String]) -> String {
  let quoted = items
    .iter()
    .map(|item| format!("\"{item}\""))
    .collect::<Vec<_>>();
  format!("[{}]", quoted.join(", "))
}

/// The XRGB8888 bytes of pixel (`x`, `y`) of `wl_probe --pattern`'s
/// quadrants: red, green, blue and white.
fn pattern_pixel(x: i32, y: i32, width: i32, height: i32) -> [u8; 4] {
  match (x < width / 2, y < height / 2) {
    (true, true) => [0, 0, 0xff, 0xff],
    (false, true) => [0, 0xff, 0, 0xff],
    (true, false) => [0xff, 0, 0, 0xff],
    (false, false) => [0xff; 4],
  }
}

/// What a toplevel draws with `wl_shm`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fill {
  Pattern,
  Red,
  Blue,
}

/// The bytes of a `width` by `height` XRGB8888 buffer with `fill`.
fn buffer_bytes(width: i32, height: i32, fill: Fill) -> Vec<u8> {
  (0..height)
    .flat_map(|y| {
      (0..width).flat_map(move |x| match fill {
        Fill::Pattern => pattern_pixel(x, y, width, height),
        Fill::Red => [0, 0, 0xff, 0xff],
        Fill::Blue => [0xff, 0, 0, 0xff],
      })
    })
    .collect()
}

/// The render node the probe makes its timelines on.
struct Node(OwnedFd);

impl AsFd for Node {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.0.as_fd()
  }
}

impl drm::Device for Node {}

impl drm::control::Device for Node {}

impl Node {
  /// Whether `point` of `handle` has signalled.
  fn signalled(&self, handle: syncobj::Handle, point: u64) -> bool {
    self
      .syncobj_timeline_wait(&[handle], &[point], 0, true, true, false)
      .is_ok()
  }
}

/// A linear DMA-BUF of `size` bytes from a sealed memfd, through
/// `/dev/udmabuf`.
fn udmabuf(size: usize) -> Result<OwnedFd, String> {
  let memfd = rustix::fs::memfd_create(
    "syncobj-probe",
    rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
  )
  .map_err(|error| format!("memfd_create: {error}"))?;
  rustix::fs::ftruncate(&memfd, size as u64)
    .map_err(|error| format!("ftruncate: {error}"))?;
  rustix::fs::fcntl_add_seals(&memfd, rustix::fs::SealFlags::SHRINK)
    .map_err(|error| format!("sealing: {error}"))?;
  let device = rustix::fs::open(
    "/dev/udmabuf",
    rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  )
  .map_err(|error| format!("opening /dev/udmabuf: {error}"))?;
  #[repr(C)]
  struct Create {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
  }
  let create = Create {
    memfd: memfd.as_raw_fd() as u32,
    flags: UDMABUF_FLAGS_CLOEXEC,
    offset: 0,
    size: size as u64,
  };
  // SAFETY: UDMABUF_CREATE reads one `struct udmabuf_create`, which
  // `Create` matches, and returns a new fd or -1.
  let fd = unsafe { libc::ioctl(device.as_raw_fd(), UDMABUF_CREATE, &create) };
  if fd < 0 {
    return Err(format!(
      "UDMABUF_CREATE: {}",
      std::io::Error::last_os_error()
    ));
  }
  // SAFETY: the ioctl returned a new fd that nothing else owns.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// One toplevel and what it was last told.
#[derive(Default)]
struct Window {
  surface: Option<wl_surface::WlSurface>,
  xdg_surface: Option<xdg_surface::XdgSurface>,
  toplevel: Option<xdg_toplevel::XdgToplevel>,
  /// The size of the pending configure.
  pending: (i32, i32),
  /// The size last drawn, if anything was.
  drawn: Option<(i32, i32)>,
  draws: u32,
  /// Frame callbacks done.
  frames: u32,
  /// Configures are acknowledged but no longer drawn.
  frozen: bool,
}

/// Which toplevel an object belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Which {
  Live,
  Stalled,
}

/// The probe's globals, windows and what came back.
#[derive(Default)]
struct Probe {
  compositor: Option<wl_compositor::WlCompositor>,
  shm: Option<wl_shm::WlShm>,
  wm_base: Option<xdg_wm_base::XdgWmBase>,
  dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
  syncobj: Option<wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1>,
  live: Window,
  stalled: Window,
  /// Alternates the live window's colour.
  blue: bool,
  /// The labels of released DMA-BUFs.
  released: Vec<String>,
}

impl Probe {
  fn window(&mut self, which: Which) -> &mut Window {
    match which {
      Which::Live => &mut self.live,
      Which::Stalled => &mut self.stalled,
    }
  }

  /// Draws `which` at its pending size with `wl_shm`, with a frame
  /// callback for the live window.
  fn draw(&mut self, handle: &QueueHandle<Self>, which: Which) {
    let fill = match which {
      Which::Stalled => Fill::Pattern,
      Which::Live if self.blue => Fill::Blue,
      Which::Live => Fill::Red,
    };
    self.blue = !self.blue;
    let Some(shm) = self.shm.clone() else {
      return;
    };
    let window = self.window(which);
    let Some(surface) = window.surface.clone() else {
      return;
    };
    let (width, height) = window.pending;
    let Ok(fd) = rustix::fs::memfd_create(
      "syncobj-probe-shm",
      rustix::fs::MemfdFlags::CLOEXEC,
    ) else {
      report("error", &[("text", "\"memfd_create failed\"".into())]);
      return;
    };
    let mut file = std::fs::File::from(fd);
    let _ = file.write_all(&buffer_bytes(width, height, fill));
    let pool = shm.create_pool(file.as_fd(), width * height * 4, handle, ());
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Xrgb8888,
      handle,
      String::from("shm"),
    );
    pool.destroy();
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width, height);
    if which == Which::Live {
      surface.frame(handle, which);
    }
    surface.commit();
    let window = self.window(which);
    window.drawn = Some((width, height));
    window.draws += 1;
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
      "zwp_linux_dmabuf_v1" => {
        probe.dmabuf = Some(registry.bind(name, 4, handle, ()));
      }
      "wp_linux_drm_syncobj_manager_v1" => {
        probe.syncobj = Some(registry.bind(name, 1, handle, ()));
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

impl Dispatch<xdg_surface::XdgSurface, Which> for Probe {
  fn event(
    probe: &mut Self,
    xdg_surface: &xdg_surface::XdgSurface,
    event: xdg_surface::Event,
    which: &Which,
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    let xdg_surface::Event::Configure { serial } = event else {
      return;
    };
    xdg_surface.ack_configure(serial);
    let window = probe.window(*which);
    if window.frozen {
      return;
    }
    if window.pending.0 <= 0 || window.pending.1 <= 0 {
      window.pending = DEFAULT_SIZE;
    }
    if window.drawn != Some(window.pending) {
      probe.draw(handle, *which);
    }
  }
}

impl Dispatch<xdg_toplevel::XdgToplevel, Which> for Probe {
  fn event(
    probe: &mut Self,
    _: &xdg_toplevel::XdgToplevel,
    event: xdg_toplevel::Event,
    which: &Which,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_toplevel::Event::Configure { width, height, .. } = event {
      let window = probe.window(*which);
      if width > 0 && height > 0 {
        window.pending = (width, height);
      }
    }
  }
}

impl Dispatch<wl_callback::WlCallback, Which> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    which: &Which,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { .. } = event {
      probe.window(*which).frames += 1;
    }
  }
}

impl Dispatch<wl_buffer::WlBuffer, String> for Probe {
  fn event(
    probe: &mut Self,
    buffer: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    label: &String,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_buffer::Event::Release = event {
      if label != "shm" {
        probe.released.push(label.clone());
      }
      buffer.destroy();
    }
  }
}

/// Implements `Dispatch` for objects whose events the probe ignores.
macro_rules! ignore_events {
  ($($interface:ty),* $(,)?) => {
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
  wl_surface::WlSurface,
  zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
  zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
  wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
  wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
  wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
);

/// A new timeline on `node`: its handle and the protocol object.
fn timeline(
  node: &Node,
  manager: &wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
  handle: &QueueHandle<Probe>,
) -> Result<
  (
    syncobj::Handle,
    wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
  ),
  String,
> {
  let syncobj = node
    .create_syncobj(false)
    .map_err(|error| format!("creating a syncobj: {error}"))?;
  let fd = node
    .syncobj_to_fd(syncobj, false)
    .map_err(|error| format!("exporting a syncobj: {error}"))?;
  Ok((syncobj, manager.import_timeline(fd.as_fd(), handle, ())))
}

/// Creates a toplevel for `which` with `app_id`.
fn toplevel(probe: &mut Probe, handle: &QueueHandle<Probe>, which: Which) {
  let (Some(compositor), Some(wm_base)) = (&probe.compositor, &probe.wm_base)
  else {
    return;
  };
  let surface = compositor.create_surface(handle, ());
  let xdg_surface = wm_base.get_xdg_surface(&surface, handle, which);
  let toplevel = xdg_surface.get_toplevel(handle, which);
  let app_id = match which {
    Which::Live => "syncobj-live",
    Which::Stalled => "syncobj-stalled",
  };
  toplevel.set_app_id(app_id.into());
  toplevel.set_title(app_id.into());
  surface.commit();
  let window = probe.window(which);
  window.surface = Some(surface);
  window.xdg_surface = Some(xdg_surface);
  window.toplevel = Some(toplevel);
}

/// Dispatches until `deadline`, without blocking past it.
fn dispatch_until(
  connection: &Connection,
  queue: &mut wayland_client::EventQueue<Probe>,
  probe: &mut Probe,
  deadline: Instant,
) -> Result<(), String> {
  while Instant::now() < deadline {
    queue.flush().map_err(|error| error.to_string())?;
    queue
      .dispatch_pending(probe)
      .map_err(|error| error.to_string())?;
    if let Some(guard) = queue.prepare_read() {
      let fd = connection.as_fd();
      let mut fds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
      )];
      let timeout = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 20_000_000,
      };
      if matches!(rustix::event::poll(&mut fds, Some(&timeout)), Ok(n) if n > 0)
      {
        match guard.read() {
          Ok(_) => {}
          // Readable, but only part of a message (or of its fds) has
          // arrived, so nothing could be dispatched yet: not an error.
          Err(wayland_client::backend::WaylandError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock => {}
          Err(error) => return Err(error.to_string()),
        }
      }
    }
    queue
      .dispatch_pending(probe)
      .map_err(|error| error.to_string())?;
  }
  Ok(())
}

/// Maps both toplevels, stalls one, keeps the other drawing, and reports.
fn run(options: &Options) -> Result<(), String> {
  let node = Node(
    rustix::fs::open(
      options.render_node.as_str(),
      rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
      rustix::fs::Mode::empty(),
    )
    .map_err(|error| format!("opening {}: {error}", options.render_node))?,
  );
  let connection =
    Connection::connect_to_env().map_err(|error| error.to_string())?;
  let mut queue = connection.new_event_queue();
  let handle = queue.handle();
  connection.display().get_registry(&handle, ());
  let mut probe = Probe::default();
  queue
    .roundtrip(&mut probe)
    .map_err(|error| error.to_string())?;
  let (Some(dmabuf), Some(manager)) =
    (probe.dmabuf.clone(), probe.syncobj.clone())
  else {
    return Err("zwp_linux_dmabuf_v1 or the syncobj manager is missing".into());
  };
  toplevel(&mut probe, &handle, Which::Live);
  toplevel(&mut probe, &handle, Which::Stalled);
  // Mapping the second toplevel retiles the first; wait until a round
  // trip brings no more draws.
  loop {
    let draws = probe.live.draws + probe.stalled.draws;
    queue
      .roundtrip(&mut probe)
      .map_err(|error| error.to_string())?;
    if probe.live.draws + probe.stalled.draws == draws
      && probe.live.drawn.is_some()
      && probe.stalled.drawn.is_some()
    {
      break;
    }
  }
  report("mapped", &[]);
  // The stalled toplevel: two DMA-BUFs behind acquire points nobody
  // signals.
  let (_acquire_handle, acquire) = timeline(&node, &manager, &handle)?;
  let (release_handle, release) = timeline(&node, &manager, &handle)?;
  let surface = probe.stalled.surface.clone().ok_or("no stalled surface")?;
  let syncobj_surface = manager.get_surface(&surface, &handle, ());
  probe.stalled.frozen = true;
  let size = (DMABUF_SIZE * DMABUF_SIZE * 4) as usize;
  let mut fds = Vec::new();
  for point in [1u32, 2] {
    let fd = udmabuf(size)?;
    let params = dmabuf.create_params(&handle, ());
    params.add(fd.as_fd(), 0, 0, (DMABUF_SIZE * 4) as u32, 0, 0);
    let buffer = params.create_immed(
      DMABUF_SIZE,
      DMABUF_SIZE,
      XR24,
      zwp_linux_buffer_params_v1::Flags::empty(),
      &handle,
      format!("dmabuf-{point}"),
    );
    params.destroy();
    syncobj_surface.set_acquire_point(&acquire, 0, point);
    syncobj_surface.set_release_point(&release, 0, point);
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, DMABUF_SIZE, DMABUF_SIZE);
    surface.frame(&handle, Which::Stalled);
    surface.commit();
    fds.push(fd);
  }
  queue
    .roundtrip(&mut probe)
    .map_err(|error| error.to_string())?;
  report("stalled", &[]);
  // The live toplevel redraws on every frame callback meanwhile.
  let start = probe.live.frames;
  probe.draw(&handle, Which::Live);
  let deadline = Instant::now() + Duration::from_secs(options.seconds);
  while Instant::now() < deadline {
    let frames = probe.live.frames;
    dispatch_until(
      &connection,
      &mut queue,
      &mut probe,
      (Instant::now() + Duration::from_millis(50)).min(deadline),
    )?;
    if probe.live.frames != frames {
      probe.draw(&handle, Which::Live);
    }
  }
  let signalled = |node: &Node| {
    [1, 2]
      .map(|point| node.signalled(release_handle, point).to_string())
      .join(", ")
  };
  report(
    "live",
    &[
      ("frames", (probe.live.frames - start).to_string()),
      ("stalled_frames", probe.stalled.frames.to_string()),
      ("released", json_list(&probe.released)),
      ("signalled", format!("[{}]", signalled(&node))),
    ],
  );
  syncobj_surface.destroy();
  if let Some(toplevel) = probe.stalled.toplevel.take() {
    toplevel.destroy();
  }
  if let Some(xdg_surface) = probe.stalled.xdg_surface.take() {
    xdg_surface.destroy();
  }
  surface.destroy();
  dispatch_until(
    &connection,
    &mut queue,
    &mut probe,
    Instant::now() + Duration::from_secs(1),
  )?;
  report(
    "destroyed",
    &[
      ("released", json_list(&probe.released)),
      ("signalled", format!("[{}]", signalled(&node))),
    ],
  );
  Ok(())
}

fn main() {
  if std::env::var_os(NAMESPACE_VARIABLE).is_none_or(|value| value != "1") {
    eprintln!(
      "syncobj_probe: refusing to run outside the GPU harness's namespace \
       ({NAMESPACE_VARIABLE} is not 1)"
    );
    std::process::exit(2);
  }
  let arguments = std::env::args().skip(1).collect::<Vec<_>>();
  let options = match parse(&arguments) {
    Ok(options) => options,
    Err(error) => {
      eprintln!("syncobj_probe: {error}");
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
  use super::{Fill, Options, buffer_bytes, json_list, parse};

  fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
  }

  #[test]
  fn options_parse() {
    assert_eq!(parse(&[]), Ok(Options::default()));
    let options = parse(&arguments(&[
      "--seconds",
      "5",
      "--render-node",
      "/dev/dri/renderD129",
    ]))
    .expect("the options parse");
    assert_eq!(options.seconds, 5);
    assert_eq!(options.render_node, "/dev/dri/renderD129");
    assert!(parse(&arguments(&["--seconds", "soon"])).is_err());
    assert!(parse(&arguments(&["--stall"])).is_err());
  }

  #[test]
  fn card_nodes_are_refused() {
    let card = parse(&arguments(&["--render-node", "/dev/dri/card0"]));
    assert!(card.is_err());
  }

  #[test]
  fn fills_have_their_colours() {
    assert_eq!(buffer_bytes(1, 1, Fill::Red), [0, 0, 0xff, 0xff]);
    assert_eq!(buffer_bytes(1, 1, Fill::Blue), [0xff, 0, 0, 0xff]);
    let pattern = buffer_bytes(2, 2, Fill::Pattern);
    assert_eq!(&pattern[..4], [0, 0, 0xff, 0xff]);
    assert_eq!(&pattern[12..], [0xff; 4]);
  }

  #[test]
  fn json_lists_quote_their_items() {
    assert_eq!(json_list(&[]), "[]");
    assert_eq!(json_list(&["a".into(), "b".into()]), "[\"a\", \"b\"]");
  }
}
