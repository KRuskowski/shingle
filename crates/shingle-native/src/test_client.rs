//! A minimal in-process Wayland client for native backend tests: it maps
//! `xdg_toplevel`s with `wl_shm` buffers on a compositor running on the
//! test thread and records the keyboard, pointer and touch events it gets,
//! with their timestamps, its frame callbacks and `wp_presentation`
//! feedback, and the output's name. It can also turn the output off and
//! on with wlr-output-power-management. Based on shingle-frontend's test
//! client.

use std::{os::unix::net::UnixStream, path::Path};

use shingle::Compositor;
use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
  },
};
use wayland_protocols::{
  wp::presentation_time::client::{wp_presentation, wp_presentation_feedback},
  xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wayland_protocols_wlr::output_power_management::v1::client::{
  zwlr_output_power_manager_v1, zwlr_output_power_v1,
};

/// `wp_presentation_feedback.presented`, with the halves joined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Presented {
  /// Seconds and nanoseconds of the presentation time.
  pub(crate) time: (u64, u32),
  pub(crate) refresh: u32,
  pub(crate) seq: u64,
  pub(crate) flags: u32,
}

/// User data that tells frame callbacks from sync callbacks.
pub(crate) struct FrameCallback;

/// What the client has received.
#[derive(Default)]
pub(crate) struct Events {
  globals: Vec<(u32, String, u32)>,
  /// Configured sizes, newest last, with the toplevel they were sent to.
  configures: Vec<(wayland_client::backend::ObjectId, i32, i32)>,
  synced: bool,
  /// Seat input, in order.
  pub(crate) input: Vec<Input>,
  /// The times of frame callbacks that fired, in milliseconds.
  pub(crate) frames: Vec<u32>,
  /// Presentation feedback received.
  pub(crate) presented: Vec<Presented>,
  /// Feedback discarded.
  pub(crate) discarded: usize,
  /// The name `wl_output` reported.
  pub(crate) output_name: Option<String>,
  /// The modes wlr-output-power reported.
  pub(crate) power_modes: Vec<u32>,
}

/// One seat event the client received.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Input {
  /// `wl_keyboard.key`, with the time in milliseconds.
  Key { time: u32, key: u32, pressed: bool },
  /// `wl_keyboard.modifiers`, depressed mask.
  Modifiers { depressed: u32 },
  /// `wl_pointer.motion`.
  Motion { time: u32, x: f64, y: f64 },
  /// `wl_pointer.button`.
  Button {
    time: u32,
    button: u32,
    pressed: bool,
  },
  /// `wl_pointer.axis`.
  Axis { time: u32, value: f64 },
  /// `wl_touch.down`.
  TouchDown { time: u32, id: i32, x: f64, y: f64 },
  /// `wl_touch.up`.
  TouchUp { time: u32, id: i32 },
}

/// A mapped toplevel; dropping it leaves the window in place.
pub(crate) struct Toplevel {
  pub(crate) surface: wl_surface::WlSurface,
  /// The size of its buffer.
  pub(crate) size: (i32, i32),
  _xdg_surface: xdg_surface::XdgSurface,
  _toplevel: xdg_toplevel::XdgToplevel,
}

/// A connected client with its event queue.
pub(crate) struct TestClient {
  connection: Connection,
  queue: EventQueue<Events>,
  handle: QueueHandle<Events>,
  registry: wl_registry::WlRegistry,
  pub(crate) events: Events,
  seat: Option<wl_seat::WlSeat>,
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
      seat: None,
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
      size: (width, height),
      _xdg_surface: xdg_surface,
      _toplevel: toplevel,
    }
  }

  /// Binds the seat and asks for its keyboard, pointer and touch.
  pub(crate) fn seat(&mut self, compositor: &mut Compositor) {
    let seat = self.bind::<wl_seat::WlSeat>(7);
    seat.get_keyboard(&self.handle, ());
    seat.get_pointer(&self.handle, ());
    seat.get_touch(&self.handle, ());
    self.seat = Some(seat);
    self.roundtrip(compositor);
  }

  /// Binds `wl_output`, whose name event arrives in the round trip.
  pub(crate) fn output(
    &mut self,
    compositor: &mut Compositor,
  ) -> wl_output::WlOutput {
    let output = self.bind::<wl_output::WlOutput>(4);
    self.roundtrip(compositor);
    output
  }

  /// Asks for a frame callback and presentation feedback on `surface`,
  /// then commits a new `width` by `height` buffer.
  pub(crate) fn frame(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
    size: (i32, i32),
  ) {
    let presentation = self.bind::<wp_presentation::WpPresentation>(1);
    surface.frame(&self.handle, FrameCallback);
    presentation.feedback(surface, &self.handle, ());
    self.attach_buffer(compositor, surface, size.0, size.1);
  }

  /// Sets the output's power mode through wlr-output-power-management.
  pub(crate) fn set_power(
    &mut self,
    compositor: &mut Compositor,
    output: &wl_output::WlOutput,
    on: bool,
  ) {
    let manager =
      self.bind::<zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1>(1);
    let power = manager.get_output_power(output, &self.handle, ());
    power.set_mode(if on {
      zwlr_output_power_v1::Mode::On
    } else {
      zwlr_output_power_v1::Mode::Off
    });
    self.roundtrip(compositor);
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
      "shingle-native-test-buffer",
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

impl Dispatch<wl_callback::WlCallback, FrameCallback> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    _: &FrameCallback,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { callback_data } = event {
      events.frames.push(callback_data);
    }
  }
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
    match event {
      wp_presentation_feedback::Event::Presented {
        tv_sec_hi,
        tv_sec_lo,
        tv_nsec,
        refresh,
        seq_hi,
        seq_lo,
        flags,
      } => events.presented.push(Presented {
        time: ((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo), tv_nsec),
        refresh,
        seq: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
        flags: match flags {
          WEnum::Value(flags) => flags.bits(),
          WEnum::Unknown(bits) => bits,
        },
      }),
      wp_presentation_feedback::Event::Discarded => events.discarded += 1,
      _ => {}
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
    if let wl_output::Event::Name { name } = event {
      events.output_name = Some(name);
    }
  }
}

impl Dispatch<zwlr_output_power_v1::ZwlrOutputPowerV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &zwlr_output_power_v1::ZwlrOutputPowerV1,
    event: zwlr_output_power_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zwlr_output_power_v1::Event::Mode {
      mode: WEnum::Value(mode),
    } = event
    {
      events.power_modes.push(mode as u32);
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
    if let xdg_toplevel::Event::Configure { width, height, .. } = event {
      events.configures.push((toplevel.id(), width, height));
    }
  }
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
    match event {
      wl_keyboard::Event::Key {
        time, key, state, ..
      } => events.input.push(Input::Key {
        time,
        key,
        pressed: state == WEnum::Value(wl_keyboard::KeyState::Pressed),
      }),
      wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
        events.input.push(Input::Modifiers {
          depressed: mods_depressed,
        })
      }
      _ => {}
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
    match event {
      wl_pointer::Event::Motion {
        time,
        surface_x,
        surface_y,
      } => events.input.push(Input::Motion {
        time,
        x: surface_x,
        y: surface_y,
      }),
      wl_pointer::Event::Button {
        time,
        button,
        state,
        ..
      } => events.input.push(Input::Button {
        time,
        button,
        pressed: state == WEnum::Value(wl_pointer::ButtonState::Pressed),
      }),
      wl_pointer::Event::Axis { time, value, .. } => {
        events.input.push(Input::Axis { time, value })
      }
      _ => {}
    }
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
    match event {
      wl_touch::Event::Down { time, id, x, y, .. } => {
        events.input.push(Input::TouchDown { time, id, x, y })
      }
      wl_touch::Event::Up { time, id, .. } => {
        events.input.push(Input::TouchUp { time, id })
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
  wl_seat::WlSeat,
  wp_presentation::WpPresentation,
  zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
);
