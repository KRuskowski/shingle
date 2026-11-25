//! A minimal in-process Wayland client for protocol tests.
//!
//! The client and the compositor run on the test thread. Requests are
//! flushed, the compositor dispatches them, and the client reads its
//! replies without blocking, until a `wl_display.sync` round trip ends.

use std::{
  os::{fd::OwnedFd, unix::net::UnixStream},
  path::Path,
};

use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle,
  backend::ObjectId,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
  },
};
use wayland_protocols::{
  wp::{
    commit_timing::v1::client::{
      wp_commit_timer_v1, wp_commit_timing_manager_v1,
    },
    fifo::v1::client::{wp_fifo_manager_v1, wp_fifo_v1},
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
  },
  xdg::{
    shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
    xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1},
  },
};
use wayland_protocols_misc::{
  zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2,
    zwp_input_method_v2,
  },
  zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
  },
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
  /// Keymaps received by `wl_keyboard` objects, as text.
  pub(crate) keymaps: Vec<String>,
  /// Keymaps received by input-method keyboard grabs, as text.
  pub(crate) grab_keymaps: Vec<String>,
  /// `xdg_toplevel.configure` sizes, with the toplevel they were sent to.
  pub(crate) configures: Vec<(ObjectId, i32, i32)>,
  /// Every `xdg_toplevel.configure`, in order, with its state list.
  pub(crate) configure_states: Vec<(ObjectId, i32, i32, Vec<u32>)>,
  /// Toplevels that received `xdg_toplevel.close`, in order.
  pub(crate) closes: Vec<ObjectId>,
  /// Every `wl_pointer` event, in order.
  pub(crate) pointer: Vec<wl_pointer::Event>,
  /// Every `wl_touch` event, in order.
  pub(crate) touch: Vec<wl_touch::Event>,
  synced: bool,
}

impl Events {
  /// Returns the last size configured for `toplevel`.
  pub(crate) fn last_configure(
    &self,
    toplevel: &xdg_toplevel::XdgToplevel,
  ) -> Option<(i32, i32)> {
    self
      .configures
      .iter()
      .rev()
      .find(|(id, ..)| *id == toplevel.id())
      .map(|(_, width, height)| (*width, *height))
  }
}

/// An `xdg_toplevel` with its surface and `xdg_surface`.
pub(crate) struct Toplevel {
  pub(crate) surface: wl_surface::WlSurface,
  pub(crate) _xdg_surface: xdg_surface::XdgSurface,
  pub(crate) toplevel: xdg_toplevel::XdgToplevel,
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

  /// Creates an `xdg_toplevel` with `app_id` and commits it without a
  /// buffer. The initial configure is acknowledged during the round trip,
  /// so a buffer can be attached next.
  pub(crate) fn toplevel(
    &mut self,
    compositor: &mut Compositor,
    app_id: &str,
  ) -> Toplevel {
    let wl_compositor = self.bind::<wl_compositor::WlCompositor>(4);
    let wm_base = self.bind::<xdg_wm_base::XdgWmBase>(1);
    let surface = wl_compositor.create_surface(&self.handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &self.handle, ());
    let toplevel = xdg_surface.get_toplevel(&self.handle, ());
    toplevel.set_app_id(app_id.into());
    surface.commit();
    self.roundtrip(compositor);
    Toplevel {
      surface,
      _xdg_surface: xdg_surface,
      toplevel,
    }
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
      "shingle-test-buffer",
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
    surface.commit();
    self.roundtrip(compositor);
  }
}

/// Reads a keymap that a compositor shared as a file descriptor. `pread`
/// leaves the descriptor's offset, which the compositor shares, alone.
fn read_keymap(fd: &OwnedFd, size: u32) -> String {
  let mut bytes = vec![0; size as usize];
  let mut read = 0;
  while read < bytes.len() {
    let count = rustix::io::pread(fd, &mut bytes[read..], read as u64)
      .expect("the keymap is readable");
    if count == 0 {
      break;
    }
    read += count;
  }
  bytes.truncate(read);
  // The compositor may count a terminating null byte in `size`.
  while bytes.last() == Some(&0) {
    bytes.pop();
  }
  String::from_utf8(bytes).expect("the keymap is UTF-8")
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_keyboard::WlKeyboard,
    event: wl_keyboard::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_keyboard::Event::Keymap { fd, size, .. } = event {
      events.keymaps.push(read_keymap(&fd, size));
    }
  }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_pointer::WlPointer,
    event: wl_pointer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.pointer.push(event);
  }
}

impl Dispatch<wl_touch::WlTouch, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_touch::WlTouch,
    event: wl_touch::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.touch.push(event);
  }
}

impl
  Dispatch<zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2, ()>
  for Events
{
  fn event(
    events: &mut Self,
    _: &zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
    event: zwp_input_method_keyboard_grab_v2::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zwp_input_method_keyboard_grab_v2::Event::Keymap {
      fd, size, ..
    } = event
    {
      events.grab_keymaps.push(read_keymap(&fd, size));
    }
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
    match event {
      xdg_toplevel::Event::Configure {
        width,
        height,
        states,
      } => {
        events.configures.push((toplevel.id(), width, height));
        let states = states
          .as_chunks::<4>()
          .0
          .iter()
          .map(|bytes| u32::from_ne_bytes(*bytes))
          .collect();
        events
          .configure_states
          .push((toplevel.id(), width, height, states));
      }
      xdg_toplevel::Event::Close => events.closes.push(toplevel.id()),
      _ => {}
    }
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
  zxdg_output_manager_v1::ZxdgOutputManagerV1,
  wp_presentation::WpPresentation,
  zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
  zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
  zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
  zwp_input_method_v2::ZwpInputMethodV2,
  wp_fifo_manager_v1::WpFifoManagerV1,
  wp_fifo_v1::WpFifoV1,
  wp_commit_timing_manager_v1::WpCommitTimingManagerV1,
  wp_commit_timer_v1::WpCommitTimerV1,
);
