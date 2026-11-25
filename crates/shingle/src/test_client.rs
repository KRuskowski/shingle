//! A minimal in-process Wayland client for protocol tests.
//!
//! The client and the compositor run on the test thread. Requests are
//! flushed, the compositor dispatches them, and the client reads its
//! replies without blocking, until a `wl_display.sync` round trip ends.

use std::{os::unix::net::UnixStream, path::Path};

use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
  },
};
use wayland_protocols::{
  wp::presentation_time::client::{wp_presentation, wp_presentation_feedback},
  xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1},
};
use wayland_protocols_wlr::output_management::v1::client::{
  zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
  zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
  zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::Compositor;

/// Everything the test client has received, in order per object type.
#[derive(Default)]
pub(crate) struct Events {
  pub(crate) globals: Vec<(u32, String, u32)>,
  pub(crate) output: Vec<wl_output::Event>,
  pub(crate) xdg_output: Vec<zxdg_output_v1::Event>,
  pub(crate) heads: Vec<zwlr_output_head_v1::Event>,
  pub(crate) modes: Vec<zwlr_output_mode_v1::Event>,
  pub(crate) frame_done: Vec<u32>,
  pub(crate) feedback: Vec<wp_presentation_feedback::Event>,
  synced: bool,
}

/// Marks the `wl_callback` of a round trip.
pub(crate) struct Sync;

/// Marks a `wl_surface.frame` callback.
pub(crate) struct Frame;

/// A connected test client with its event queue.
pub(crate) struct TestClient {
  connection: Connection,
  queue: EventQueue<Events>,
  pub(crate) handle: QueueHandle<Events>,
  registry: wl_registry::WlRegistry,
  pub(crate) events: Events,
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
    self.connection.display().sync(&self.handle, Sync);
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

  /// Binds the first global of interface `I` at `version`.
  pub(crate) fn bind<I>(&self, version: u32) -> I
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

  /// Creates a surface with an attached and committed 1x1 shm buffer and
  /// makes it the pointer's cursor, which keeps it visible without a shell.
  pub(crate) fn cursor_surface(
    &mut self,
    compositor: &mut Compositor,
  ) -> wl_surface::WlSurface {
    let wl_compositor = self.bind::<wl_compositor::WlCompositor>(4);
    let shm = self.bind::<wl_shm::WlShm>(1);
    let seat = self.bind::<wl_seat::WlSeat>(5);
    let pointer = seat.get_pointer(&self.handle, ());
    let fd = rustix::fs::memfd_create(
      "shingle-test-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, 4).expect("ftruncate succeeds");
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), 4, &self.handle, ());
    let buffer = pool.create_buffer(
      0,
      1,
      1,
      4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    );
    let surface = wl_compositor.create_surface(&self.handle, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    pointer.set_cursor(0, Some(&surface), 0, 0);
    self.roundtrip(compositor);
    surface
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

impl Dispatch<wl_callback::WlCallback, Sync> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    _: wl_callback::Event,
    _: &Sync,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.synced = true;
  }
}

impl Dispatch<wl_callback::WlCallback, Frame> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    _: &Frame,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { callback_data } = event {
      events.frame_done.push(callback_data);
    }
  }
}

impl Dispatch<wl_output::WlOutput, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_output::WlOutput,
    event: wl_output::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.output.push(event);
  }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &zxdg_output_v1::ZxdgOutputV1,
    event: zxdg_output_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.xdg_output.push(event);
  }
}

impl Dispatch<ZwlrOutputHeadV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrOutputHeadV1,
    event: zwlr_output_head_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.heads.push(event);
  }

  wayland_client::event_created_child!(Events, ZwlrOutputHeadV1, [
    zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
  ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrOutputModeV1,
    event: zwlr_output_mode_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.modes.push(event);
  }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Events {
  fn event(
    _: &mut Self,
    _: &ZwlrOutputManagerV1,
    _: zwlr_output_manager_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }

  wayland_client::event_created_child!(Events, ZwlrOutputManagerV1, [
    zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
  ]);
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wp_presentation_feedback::WpPresentationFeedback,
    event: wp_presentation_feedback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.feedback.push(event);
  }
}

/// Implements `Dispatch` for objects whose events the tests ignore.
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
  wl_seat::WlSeat,
  wl_pointer::WlPointer,
  zxdg_output_manager_v1::ZxdgOutputManagerV1,
  wp_presentation::WpPresentation,
);
