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
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm,
    wl_shm_pool, wl_surface,
  },
};
use wayland_protocols::xdg::shell::client::{
  xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols_wlr::screencopy::v1::client::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// What the client has received.
#[derive(Default)]
pub(crate) struct Events {
  globals: Vec<(u32, String, u32)>,
  /// Configured sizes, newest last, with the toplevel they were sent to.
  configures: Vec<(wayland_client::backend::ObjectId, i32, i32)>,
  /// Screencopy outcomes, `ready` or `failed`, in order.
  pub(crate) captures: Vec<&'static str>,
  synced: bool,
}

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
  _surface: wl_surface::WlSurface,
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
      _surface: surface,
      _xdg_surface: xdg_surface,
      _toplevel: toplevel,
    }
  }

  /// Asks wlr-screencopy for the `width` by `height` region at (`x`, `y`)
  /// and copies it into a new shm buffer.
  pub(crate) fn capture(
    &mut self,
    compositor: &mut Compositor,
    (x, y, width, height): (i32, i32, i32, i32),
  ) -> Capture {
    let manager = self.bind::<ZwlrScreencopyManagerV1>(3);
    let output = self.bind::<wl_output::WlOutput>(1);
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

  /// The screencopy outcomes so far.
  pub(crate) fn captures(&self) -> &[&'static str] {
    &self.events.captures
  }

  /// Attaches and commits a `width` by `height` shm buffer on `surface`.
  pub(crate) fn attach_buffer(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
    width: i32,
    height: i32,
  ) {
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
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width, height);
    surface.commit();
    self.roundtrip(compositor);
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

ignore_events!(
  wl_compositor::WlCompositor,
  wl_surface::WlSurface,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_buffer::WlBuffer,
  wl_output::WlOutput,
  ZwlrScreencopyManagerV1,
);
