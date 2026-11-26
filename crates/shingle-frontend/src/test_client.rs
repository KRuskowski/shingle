//! A minimal in-process Wayland client for frontend tests: it maps
//! `xdg_toplevel`s with `wl_shm` buffers on a compositor running on the
//! test thread.

use std::{
  os::{fd::OwnedFd, unix::net::UnixStream},
  path::Path,
};

use shingle::Compositor;
use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
  },
};
use wayland_protocols::{
  wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
  },
  wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_swipe_v1::{self, ZwpPointerGestureSwipeV1},
    zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
  },
  xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wayland_protocols_wlr::screencopy::v1::client::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// The client's side of wayfire-shell-unstable-v2, which no protocol
/// crate has.
#[allow(missing_docs, clippy::all, dead_code, non_upper_case_globals)]
#[allow(non_camel_case_types, unused_imports)]
pub(crate) mod wayfire_shell {
  use wayland_client;
  use wayland_client::protocol::*;

  pub(crate) mod __interfaces {
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::__interfaces::*;
    wayland_scanner::generate_interfaces!(
      "../shingle/protocols/wayfire-shell-unstable-v2.xml"
    );
  }
  use self::__interfaces::*;

  wayland_scanner::generate_client_code!(
    "../shingle/protocols/wayfire-shell-unstable-v2.xml"
  );
}

use wayfire_shell::{
  zwf_output_v2::{self, ZwfOutputV2},
  zwf_shell_manager_v2::ZwfShellManagerV2,
};

/// What the client has received.
#[derive(Default)]
pub(crate) struct Events {
  globals: Vec<(u32, String, u32)>,
  /// Configured sizes, newest last, with the toplevel they were sent to.
  configures: Vec<(wayland_client::backend::ObjectId, i32, i32)>,
  /// Screencopy outcomes, `ready` or `failed`, in order.
  pub(crate) captures: Vec<&'static str>,
  /// The `wl_buffer`s that received `release`, in order.
  pub(crate) releases: Vec<wayland_client::backend::ObjectId>,
  /// The frame callbacks that are done, by number, in order.
  frames_done: Vec<u32>,
  /// `zwf_output_v2` events, by name, in order.
  shell: Vec<&'static str>,
  /// Swipe gesture events, as `begin FINGERS`, `update DX DY` and `end
  /// CANCELLED`, in order.
  swipes: Vec<String>,
  synced: bool,
}

/// The user data of a frame callback: the number the test gave it.
pub(crate) struct FrameCallback(u32);

/// A screencopy copy in flight, with the memory its buffer lives in.
pub(crate) struct Capture {
  _frame: ZwlrScreencopyFrameV1,
  _buffer: wl_buffer::WlBuffer,
  fd: OwnedFd,
  size: usize,
}

impl Capture {
  /// The buffer's bytes: ARGB8888, so B, G, R, A per pixel.
  pub(crate) fn bytes(&self) -> Vec<u8> {
    let mut bytes = vec![0; self.size];
    let read = rustix::io::pread(&self.fd, &mut bytes, 0).expect("pread");
    bytes.truncate(read);
    bytes
  }
}

/// A mapped toplevel; dropping it leaves the window in place.
pub(crate) struct Toplevel {
  pub(crate) surface: wl_surface::WlSurface,
  _xdg_surface: xdg_surface::XdgSurface,
  _toplevel: xdg_toplevel::XdgToplevel,
}

/// A connected client with its event queue.
pub(crate) struct TestClient {
  connection: Connection,
  queue: EventQueue<Events>,
  handle: QueueHandle<Events>,
  registry: wl_registry::WlRegistry,
  events: Events,
}

impl TestClient {
  /// Connects to the compositor's socket and reads its globals.
  pub(crate) fn connect(compositor: &mut Compositor, socket: &Path) -> Self {
    let stream =
      UnixStream::connect(socket).expect("the test socket accepts clients");
    let connection =
      Connection::from_socket(stream).expect("the client connects");
    let queue = connection.new_event_queue();
    let handle = queue.handle();
    let registry = connection.display().get_registry(&handle, ());
    let mut client = Self {
      connection,
      queue,
      handle,
      registry,
      events: Events::default(),
    };
    client.roundtrip(compositor);
    client
  }

  /// Exchanges messages until the compositor has handled every request
  /// sent so far and the client has read the replies.
  pub(crate) fn roundtrip(&mut self, compositor: &mut Compositor) {
    self.events.synced = false;
    self.connection.display().sync(&self.handle, ());
    for _ in 0..100 {
      self.queue.flush().expect("the client flushes");
      compositor
        .dispatch_pending()
        .expect("the compositor dispatches");
      if let Some(guard) = self.queue.prepare_read() {
        // WouldBlock only means that nothing has arrived yet.
        let _ = guard.read();
      }
      self
        .queue
        .dispatch_pending(&mut self.events)
        .expect("the client dispatches");
      if self.events.synced {
        return;
      }
    }
    panic!("the round trip did not finish");
  }

  fn bind<I>(&self, version: u32) -> I
  where
    I: Proxy + 'static,
    Events: Dispatch<I, ()>,
  {
    let (name, _, _) = self
      .events
      .globals
      .iter()
      .find(|(_, interface, _)| interface == I::interface().name)
      .unwrap_or_else(|| panic!("no {} global", I::interface().name));
    self.registry.bind(*name, version, &self.handle, ())
  }

  /// Creates an `xdg_toplevel` with `app_id` and `title` and maps it with
  /// a buffer of the size it was configured to, or 200x150.
  pub(crate) fn toplevel(
    &mut self,
    compositor: &mut Compositor,
    app_id: &str,
    title: &str,
  ) -> Toplevel {
    let wl_compositor = self.bind::<wl_compositor::WlCompositor>(4);
    let wm_base = self.bind::<xdg_wm_base::XdgWmBase>(1);
    let surface = wl_compositor.create_surface(&self.handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &self.handle, ());
    let toplevel = xdg_surface.get_toplevel(&self.handle, ());
    toplevel.set_app_id(app_id.into());
    toplevel.set_title(title.into());
    surface.commit();
    self.roundtrip(compositor);
    let (width, height) = self
      .events
      .configures
      .iter()
      .rev()
      .find(|(id, ..)| *id == toplevel.id())
      .map(|(_, width, height)| (*width, *height))
      .filter(|(width, height)| *width > 0 && *height > 0)
      .unwrap_or((200, 150));
    self.attach_buffer(compositor, &surface, width, height);
    Toplevel {
      surface,
      _xdg_surface: xdg_surface,
      _toplevel: toplevel,
    }
  }

  /// Binds the seat's pointer and a swipe gesture for it, so that the
  /// client records the swipes it gets while it has the pointer.
  pub(crate) fn listen_for_swipes(&mut self, compositor: &mut Compositor) {
    let seat = self.bind::<wl_seat::WlSeat>(5);
    let pointer = seat.get_pointer(&self.handle, ());
    let gestures = self.bind::<ZwpPointerGesturesV1>(1);
    gestures.get_swipe_gesture(&pointer, &self.handle, ());
    self.roundtrip(compositor);
  }

  /// The swipe events received, in order.
  pub(crate) fn swipes(&self) -> &[String] {
    &self.events.swipes
  }

  /// A `zwf_output_v2` of version 3 for the `nth` `wl_output` global
  /// announced, counted from 0.
  pub(crate) fn shell_output(
    &mut self,
    compositor: &mut Compositor,
    nth: usize,
  ) -> ZwfOutputV2 {
    let shell = self.bind::<ZwfShellManagerV2>(3);
    let name = self
      .events
      .globals
      .iter()
      .filter(|(_, interface, _)| interface == "wl_output")
      .nth(nth)
      .map(|(name, ..)| *name)
      .expect("the output is announced");
    let output: wl_output::WlOutput =
      self.registry.bind(name, 4, &self.handle, ());
    let shell_output = shell.get_wf_output(&output, &self.handle, ());
    self.roundtrip(compositor);
    shell_output
  }

  /// Asks wlr-screencopy for the `width` by `height` region at (`x`, `y`)
  /// of the first output and copies it into a new shm buffer.
  pub(crate) fn capture(
    &mut self,
    compositor: &mut Compositor,
    region: (i32, i32, i32, i32),
  ) -> Capture {
    self.capture_on(compositor, 0, region)
  }

  /// [`TestClient::capture`] of the `nth` `wl_output` global announced,
  /// counted from 0.
  pub(crate) fn capture_on(
    &mut self,
    compositor: &mut Compositor,
    nth: usize,
    (x, y, width, height): (i32, i32, i32, i32),
  ) -> Capture {
    let manager = self.bind::<ZwlrScreencopyManagerV1>(3);
    let name = self
      .events
      .globals
      .iter()
      .filter(|(_, interface, _)| interface == "wl_output")
      .nth(nth)
      .map(|(name, ..)| *name)
      .expect("that many outputs");
    let output = self.registry.bind::<wl_output::WlOutput, _, _>(
      name,
      1,
      &self.handle,
      (),
    );
    let frame = manager.capture_output_region(
      0,
      &output,
      x,
      y,
      width,
      height,
      &self.handle,
      (),
    );
    let size = width * height * 4;
    let fd = rustix::fs::memfd_create(
      "shingle-frontend-test-capture",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, size as u64).expect("ftruncate succeeds");
    let shm = self.bind::<wl_shm::WlShm>(1);
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), size, &self.handle, ());
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    );
    frame.copy(&buffer);
    self.roundtrip(compositor);
    Capture {
      _frame: frame,
      _buffer: buffer,
      fd,
      size: size as usize,
    }
  }

  /// A `width` by `height` XRGB8888 linear DMA-BUF made with
  /// `create_immed`, from a memfd: what a fake importer accepts.
  pub(crate) fn dmabuf_buffer(
    &mut self,
    compositor: &mut Compositor,
    width: i32,
    height: i32,
  ) -> wl_buffer::WlBuffer {
    let dmabuf = self.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>(5);
    let fd = rustix::fs::memfd_create(
      "shingle-frontend-test-dmabuf",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, (width * height * 4) as u64)
      .expect("ftruncate succeeds");
    let params = dmabuf.create_params(&self.handle, ());
    params.add(rustix::fd::AsFd::as_fd(&fd), 0, 0, (width * 4) as u32, 0, 0);
    let buffer = params.create_immed(
      width,
      height,
      u32::from_le_bytes(*b"XR24"),
      zwp_linux_buffer_params_v1::Flags::empty(),
      &self.handle,
      (),
    );
    params.destroy();
    self.roundtrip(compositor);
    buffer
  }

  /// Attaches `buffer` to `surface`, damages it all and commits.
  pub(crate) fn commit_buffer(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
    buffer: &wl_buffer::WlBuffer,
  ) {
    surface.attach(Some(buffer), 0, 0);
    surface.damage(0, 0, i32::MAX, i32::MAX);
    surface.commit();
    self.roundtrip(compositor);
  }

  /// The buffers released so far.
  pub(crate) fn releases(&self) -> &[wayland_client::backend::ObjectId] {
    &self.events.releases
  }

  /// Asks for a frame callback numbered `number` on `surface` and
  /// commits.
  pub(crate) fn request_frame(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
    number: u32,
  ) {
    surface.frame(&self.handle, FrameCallback(number));
    surface.commit();
    self.roundtrip(compositor);
  }

  /// The numbers of the frame callbacks that are done, in order.
  pub(crate) fn frames_done(&self) -> &[u32] {
    &self.events.frames_done
  }

  /// The screencopy outcomes so far.
  pub(crate) fn captures(&self) -> &[&'static str] {
    &self.events.captures
  }

  /// The `zwf_output_v2` events so far, by name.
  pub(crate) fn shell_events(&self) -> &[&'static str] {
    &self.events.shell
  }

  /// Attaches and commits a `width` by `height` shm buffer on `surface`.
  pub(crate) fn attach_buffer(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
    width: i32,
    height: i32,
  ) {
    let buffer = self.shm_buffer(width, height);
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width, height);
    surface.commit();
    self.roundtrip(compositor);
  }

  /// A `width` by `height` ARGB8888 shm buffer in a pool of its own.
  pub(crate) fn shm_buffer(
    &mut self,
    width: i32,
    height: i32,
  ) -> wl_buffer::WlBuffer {
    let shm = self.bind::<wl_shm::WlShm>(1);
    let size = width * height * 4;
    let fd = rustix::fs::memfd_create(
      "shingle-frontend-test-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, size as u64).expect("ftruncate succeeds");
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), size, &self.handle, ());
    pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    )
  }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    {
      events.globals.push((name, interface, version));
    }
  }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Events {
  fn event(
    events: &mut Self,
    buffer: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_buffer::Event::Release = event {
      events.releases.push(buffer.id());
    }
  }
}

impl Dispatch<wl_callback::WlCallback, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    _: wl_callback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.synced = true;
  }
}

impl Dispatch<wl_callback::WlCallback, FrameCallback> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    _: wl_callback::Event,
    callback: &FrameCallback,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.frames_done.push(callback.0);
  }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Events {
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

impl Dispatch<xdg_surface::XdgSurface, ()> for Events {
  fn event(
    _: &mut Self,
    xdg_surface: &xdg_surface::XdgSurface,
    event: xdg_surface::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_surface::Event::Configure { serial } = event {
      xdg_surface.ack_configure(serial);
    }
  }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Events {
  fn event(
    events: &mut Self,
    toplevel: &xdg_toplevel::XdgToplevel,
    event: xdg_toplevel::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_toplevel::Event::Configure { width, height, .. } = event {
      events.configures.push((toplevel.id(), width, height));
    }
  }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrScreencopyFrameV1,
    event: zwlr_screencopy_frame_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_screencopy_frame_v1::Event::Ready { .. } => {
        events.captures.push("ready");
      }
      zwlr_screencopy_frame_v1::Event::Failed => {
        events.captures.push("failed");
      }
      _ => {}
    }
  }
}

macro_rules! ignore_events {
  ($($interface:ty),* $(,)?) => {
    $(
      impl Dispatch<$interface, ()> for Events {
        fn event(
          _: &mut Self,
          _: &$interface,
          _: <$interface as Proxy>::Event,
          _: &(),
          _: &Connection,
          _: &QueueHandle<Self>,
        ) {
        }
      }
    )*
  };
}

impl Dispatch<ZwfOutputV2, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwfOutputV2,
    event: zwf_output_v2::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.shell.push(match event {
      zwf_output_v2::Event::EnterFullscreen => "enter_fullscreen",
      zwf_output_v2::Event::LeaveFullscreen => "leave_fullscreen",
      zwf_output_v2::Event::ToggleMenu => "toggle_menu",
    });
  }
}

impl Dispatch<ZwpPointerGestureSwipeV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwpPointerGestureSwipeV1,
    event: zwp_pointer_gesture_swipe_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.swipes.push(match event {
      zwp_pointer_gesture_swipe_v1::Event::Begin { fingers, .. } => {
        format!("begin {fingers}")
      }
      zwp_pointer_gesture_swipe_v1::Event::Update { dx, dy, .. } => {
        format!("update {dx} {dy}")
      }
      zwp_pointer_gesture_swipe_v1::Event::End { cancelled, .. } => {
        format!("end {cancelled}")
      }
      _ => return,
    });
  }
}

ignore_events!(
  ZwfShellManagerV2,
  wl_seat::WlSeat,
  wl_pointer::WlPointer,
  ZwpPointerGesturesV1,
  wl_compositor::WlCompositor,
  wl_surface::WlSurface,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_output::WlOutput,
  ZwlrScreencopyManagerV1,
  zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
  zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
);
