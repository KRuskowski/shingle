//! The nested backend: Shingle in a window inside another Wayland or X11
//! session.
//!
//! This library is a thin winit host for [`shingle_frontend::Frontend`]: it
//! creates the window and renderer, turns winit events into
//! [`InputEvent`]s in output pixels and evdev codes, and keeps the host-only
//! state (keyboard focus changes, button emulation and the pointer-warp
//! echo filter). The `shingle` binary (crates/shingle-bin) parses the
//! command line, selects this backend and calls [`run`].

mod host_keyboard;

use std::{
  error::Error,
  ffi::OsString,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use shingle::{Compositor, InputDeviceId, InputDeviceInfo, InputDeviceKind};
use shingle_config::{Modifiers as ConfigModifiers, ShingleConfig};
use shingle_frontend::{
  Frontend,
  host::{Axis, AxisSource, Host, InputEvent, Touch, TouchPhase},
};
use shingle_render::Renderer;
use winit::{
  application::ApplicationHandler,
  dpi::{PhysicalPosition, PhysicalSize},
  event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta,
    WindowEvent,
  },
  event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
  platform::scancode::PhysicalKeyExtScancode,
  window::{Window, WindowAttributes, WindowId},
};

/// Translates a winit scroll. Line deltas are wheel clicks, ten pixels
/// each. Pixel deltas come from touchpads, which the host compositor
/// reports with a finger source, so they are fingers here too, and the end
/// of the gesture stops both axes.
fn scroll_axis(
  delta: MouseScrollDelta,
  phase: winit::event::TouchPhase,
) -> Axis {
  match delta {
    MouseScrollDelta::LineDelta(x, y) => Axis {
      horizontal: x as f64 * 10.0,
      vertical: y as f64 * 10.0,
      horizontal_steps: x as i32,
      vertical_steps: y as i32,
      source: AxisSource::Wheel,
      ..Axis::default()
    },
    MouseScrollDelta::PixelDelta(position) => {
      let ended = matches!(
        phase,
        winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled
      );
      Axis {
        horizontal: position.x,
        vertical: position.y,
        source: AxisSource::Finger,
        horizontal_stop: ended,
        vertical_stop: ended,
        ..Axis::default()
      }
    }
  }
}

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;
/// `BTN_RIGHT`.
const BTN_RIGHT: u32 = 0x111;
/// `BTN_MIDDLE`.
const BTN_MIDDLE: u32 = 0x112;

/// Software middle-button emulation and left-handed swapping for the host
/// pointer. On the native backend libinput does both.
#[derive(Default)]
struct MouseButtons {
  left: bool,
  right: bool,
  emulated_middle: bool,
}

impl MouseButtons {
  /// Translates host button `button` (an evdev code). Returns the button
  /// to report, or `None` to drop the event.
  fn translate(
    &mut self,
    button: u32,
    pressed: bool,
    middle_emulation: bool,
    left_handed: bool,
  ) -> Option<u32> {
    let physical = match button {
      BTN_LEFT => &mut self.left,
      BTN_RIGHT => &mut self.right,
      _ => return Some(button),
    };
    if pressed {
      if *physical {
        return None;
      }
      *physical = true;
      if middle_emulation && self.left && self.right && !self.emulated_middle {
        self.emulated_middle = true;
        return Some(BTN_MIDDLE);
      }
    } else {
      if !*physical {
        return None;
      }
      *physical = false;
      if self.emulated_middle {
        self.emulated_middle = false;
        return Some(BTN_MIDDLE);
      }
    }
    Some(match button {
      BTN_LEFT if left_handed => BTN_RIGHT,
      BTN_RIGHT if left_handed => BTN_LEFT,
      _ => button,
    })
  }
}

/// The seat devices nested feeds host input from: one each for the host
/// window's keyboard, pointer and touch input, which STIPC input also goes
/// through.
#[derive(Clone, Copy, Debug)]
struct SeatDevices {
  keyboard: InputDeviceId,
  pointer: InputDeviceId,
  touch: InputDeviceId,
}

impl SeatDevices {
  /// Registers the three devices in the order and with the names that
  /// `input/list-devices` has always reported for nested, so they get IDs
  /// 1 to 3 on a new compositor.
  fn register(compositor: &mut Compositor) -> Self {
    let mut add = |name: &str, kind| {
      compositor.add_input_device(InputDeviceInfo {
        name: name.into(),
        vendor: None,
        product: None,
        kind,
      })
    };
    Self {
      keyboard: add("Shingle nested keyboard", InputDeviceKind::Keyboard),
      pointer: add("Shingle nested pointer", InputDeviceKind::Pointer),
      touch: add("Shingle nested touch", InputDeviceKind::Touch),
    }
  }

  /// Returns the device for host input of `kind`.
  fn get(self, kind: InputDeviceKind) -> InputDeviceId {
    match kind {
      InputDeviceKind::Keyboard => self.keyboard,
      InputDeviceKind::Pointer => self.pointer,
      InputDeviceKind::Touch => self.touch,
    }
  }
}

/// The [`Host`] side of the nested window: what the frontend asks of it
/// during one call, applied by [`App::finish`] afterwards.
#[derive(Default)]
struct WindowHost {
  window: Option<Arc<Window>>,
  /// A pointer warp the frontend asked for, in output pixels.
  warp: Option<(f64, f64)>,
  /// Where the last warp put the host cursor, in window pixels, until the
  /// host echoes it back as a cursor motion.
  pending_warp: Option<(f64, f64)>,
  exit: bool,
  /// The pointer device, whose button emulation resets when it is
  /// disabled.
  pointer: Option<InputDeviceId>,
  mouse_buttons: MouseButtons,
}

impl Host for WindowHost {
  fn request_redraw(&mut self) {
    if let Some(window) = &self.window {
      window.request_redraw();
    }
  }

  fn warp_pointer(&mut self, x: f64, y: f64) {
    self.warp = Some((x, y));
  }

  fn switch_vt(&mut self, _vt: u32) -> bool {
    false
  }

  fn input_device_changed(&mut self, device: InputDeviceId, enabled: bool) {
    if !enabled && Some(device) == self.pointer {
      self.mouse_buttons = MouseButtons::default();
    }
  }

  fn exit(&mut self) {
    self.exit = true;
  }
}

struct App {
  frontend: Frontend,
  devices: SeatDevices,
  host: WindowHost,
  renderer: Option<Renderer>,
  host_keyboard: host_keyboard::HostKeyboard,
  /// The window's size when it opens.
  window_size: (u32, u32),
}

/// The size the nested window opens at: an explicit configured mode of
/// the nested output, as a nested Wayfire sets a custom mode on its
/// window, else 1280x720.
fn initial_window_size(config: &ShingleConfig) -> (u32, u32) {
  config
    .output_config(shingle::OUTPUT_NAME)
    .mode
    .size()
    .unwrap_or((1280, 720))
}

impl App {
  fn new(
    config: ShingleConfig,
    spawn: Vec<OsString>,
    config_path: Option<PathBuf>,
    ipc_path: Option<&Path>,
  ) -> Result<Self, Box<dyn Error>> {
    let window_size = initial_window_size(&config);
    let mut frontend = Frontend::new(config, spawn, config_path, ipc_path)?;
    let devices = SeatDevices::register(frontend.compositor_mut());
    Ok(Self {
      frontend,
      devices,
      host: WindowHost {
        pointer: Some(devices.pointer),
        ..WindowHost::default()
      },
      renderer: None,
      host_keyboard: host_keyboard::HostKeyboard::default(),
      window_size,
    })
  }

  /// Carries out what the frontend asked of the host: warps the host
  /// cursor and ends the event loop.
  fn finish(&mut self, event_loop: &ActiveEventLoop) {
    if let Some((x, y)) = self.host.warp.take()
      && let Some(window) = &self.host.window
    {
      let size = window.inner_size();
      let point = self.frontend.compositor().unmap_output_point(
        x,
        y,
        size.width,
        size.height,
      );
      self.host.pending_warp = Some(point);
      if let Err(error) =
        window.set_cursor_position(PhysicalPosition::new(point.0, point.1))
      {
        eprintln!("shingle: pointer warp failed: {error}");
      }
    }
    if std::mem::take(&mut self.host.exit) {
      event_loop.exit();
    }
  }

  /// Maps a point in window pixels, as winit reports it scaled by the host,
  /// to output pixels.
  fn map_point(&self, x: f64, y: f64) -> (f64, f64) {
    let Some(window) = &self.host.window else {
      return (x, y);
    };
    let host_scale = window.scale_factor();
    let size = window.inner_size();
    self.frontend.compositor().map_output_point(
      x * host_scale,
      y * host_scale,
      size.width,
      size.height,
    )
  }

  /// Feeds host keyboard and focus events to the frontend. Returns whether
  /// the event was a keyboard event.
  fn keyboard_event(&mut self, event: &WindowEvent) -> bool {
    let keyboard = self.devices.keyboard;
    match event {
      WindowEvent::Focused(focused) => {
        self.host_keyboard.focus_changed(*focused);
        if !*focused {
          self.frontend.handle_input(
            &mut self.host,
            keyboard,
            InputEvent::Reset,
            None,
          );
        }
        false
      }
      WindowEvent::ModifiersChanged(modifiers) => {
        let state = modifiers.state();
        let held = ConfigModifiers {
          super_key: state.super_key(),
          control: state.control_key(),
          alt: state.alt_key(),
          shift: state.shift_key(),
        };
        let host_keyboard = &mut self.host_keyboard;
        self
          .frontend
          .update_keyboard(&mut self.host, keyboard, |state| {
            host_keyboard.host_modifiers(state, held)
          });
        true
      }
      WindowEvent::KeyboardInput { event, .. } => {
        let Some(code) = event.physical_key.to_scancode() else {
          return true;
        };
        self.host_keyboard.key(code);
        // The frontend repeats keys itself, at Shingle's configured rate.
        if event.repeat {
          return true;
        }
        let pressed = event.state == ElementState::Pressed;
        self.frontend.handle_input(
          &mut self.host,
          keyboard,
          InputEvent::Key { code, pressed },
          None,
        );
        true
      }
      _ => false,
    }
  }

  /// Feeds one pointer or touch event to the frontend.
  fn input(&mut self, device: InputDeviceId, event: InputEvent) {
    self
      .frontend
      .handle_input(&mut self.host, device, event, None);
  }
}

impl ApplicationHandler for App {
  fn resumed(&mut self, event_loop: &ActiveEventLoop) {
    if self.host.window.is_some() {
      return;
    }
    let attributes = WindowAttributes::default()
      .with_title("Shingle")
      .with_inner_size(PhysicalSize::new(
        self.window_size.0,
        self.window_size.1,
      ));
    let window = match event_loop.create_window(attributes) {
      Ok(window) => Arc::new(window),
      Err(error) => {
        eprintln!("shingle: failed to create nested window: {error}");
        event_loop.exit();
        return;
      }
    };
    let size = window.inner_size();
    let renderer = match pollster::block_on(Renderer::new(
      window.clone(),
      (size.width, size.height),
    )) {
      Ok(renderer) => renderer,
      Err(error) => {
        eprintln!("shingle: failed to initialize wgpu: {error}");
        event_loop.exit();
        return;
      }
    };
    eprintln!("shingle: {}", renderer.gpu().dmabuf_summary());
    if let Err(error) = self.frontend.set_output_size(size.width, size.height) {
      eprintln!("shingle: failed to set output size: {error}");
      event_loop.exit();
      return;
    }
    // Clients start only now that linux-dmabuf can be offered: most look
    // for the global once (docs/design/dmabuf.md, section 2).
    self.frontend.enable_dmabuf(renderer.gpu());
    if let Err(error) = self.frontend.start_clients() {
      eprintln!("shingle: failed to start clients: {error}");
      event_loop.exit();
      return;
    }
    self.renderer = Some(renderer);
    self.host.window = Some(window);
  }

  fn window_event(
    &mut self,
    event_loop: &ActiveEventLoop,
    window_id: WindowId,
    event: WindowEvent,
  ) {
    if self.host.window.as_ref().map(|window| window.id()) != Some(window_id) {
      return;
    }
    // Keyboard state is tracked even while the keyboard device is disabled,
    // so modifiers are correct again the moment it is re-enabled.
    if self.keyboard_event(&event) {
      self.finish(event_loop);
      return;
    }
    if window_event_device(&event).is_some_and(|kind| {
      !self
        .frontend
        .compositor()
        .input_device_enabled(self.devices.get(kind))
    }) {
      return;
    }
    match event {
      WindowEvent::CloseRequested => {
        self.frontend.shutdown();
        event_loop.exit();
      }
      WindowEvent::Resized(size) => {
        if let Some(renderer) = self.renderer.as_mut() {
          renderer.resize((size.width, size.height));
        }
        if let Err(error) =
          self.frontend.output_resized(size.width, size.height)
        {
          eprintln!("shingle: failed to resize output: {error}");
          event_loop.exit();
        }
      }
      WindowEvent::RedrawRequested => {
        if let Some(renderer) = self.renderer.as_mut()
          && let Err(error) = self.frontend.render(&mut self.host, renderer)
        {
          eprintln!("shingle: render failed: {error}");
          event_loop.exit();
        }
      }
      WindowEvent::CursorMoved { position, .. } => {
        if self.host.pending_warp.is_some_and(|pending| {
          (position.x - pending.0).abs() <= 1.0
            && (position.y - pending.1).abs() <= 1.0
        }) {
          self.host.pending_warp = None;
          return;
        }
        let (x, y) = self.map_point(position.x, position.y);
        self.input(
          self.devices.pointer,
          InputEvent::PointerMotionAbsolute { x, y },
        );
      }
      WindowEvent::MouseInput { state, button, .. } => {
        let code = match button {
          MouseButton::Left => BTN_LEFT,
          MouseButton::Right => BTN_RIGHT,
          MouseButton::Middle => BTN_MIDDLE,
          MouseButton::Back => 0x113,
          MouseButton::Forward => 0x114,
          MouseButton::Other(code) => u32::from(code),
        };
        let pressed = state == ElementState::Pressed;
        if let Some(code) = self.host.mouse_buttons.translate(
          code,
          pressed,
          self.frontend.middle_emulation(),
          self.frontend.left_handed_mode(),
        ) {
          self.input(
            self.devices.pointer,
            InputEvent::PointerButton { code, pressed },
          );
        }
      }
      WindowEvent::Touch(touch) => {
        let (x, y) = self.map_point(touch.location.x, touch.location.y);
        let phase = match touch.phase {
          winit::event::TouchPhase::Started => TouchPhase::Down,
          winit::event::TouchPhase::Moved => TouchPhase::Motion,
          winit::event::TouchPhase::Ended => TouchPhase::Up,
          winit::event::TouchPhase::Cancelled => TouchPhase::Cancel,
        };
        self.input(
          self.devices.touch,
          InputEvent::Touch(Touch {
            id: touch.id,
            phase,
            x,
            y,
          }),
        );
      }
      WindowEvent::MouseWheel { delta, phase, .. } => {
        let axis = scroll_axis(delta, phase);
        self.input(self.devices.pointer, InputEvent::PointerAxis(axis));
      }
      _ => {}
    }
    self.finish(event_loop);
  }

  fn device_event(
    &mut self,
    event_loop: &ActiveEventLoop,
    _device_id: DeviceId,
    event: DeviceEvent,
  ) {
    let DeviceEvent::MouseMotion { delta: (dx, dy) } = event else {
      return;
    };
    let (dx, dy) = if let Some(window) = &self.host.window {
      let host_scale = window.scale_factor();
      let size = window.inner_size();
      self.frontend.compositor().map_output_vector(
        dx * host_scale,
        dy * host_scale,
        size.width,
        size.height,
      )
    } else {
      (dx, dy)
    };
    self.input(
      self.devices.pointer,
      InputEvent::PointerMotion {
        dx,
        dy,
        dx_unaccel: dx,
        dy_unaccel: dy,
      },
    );
    self.finish(event_loop);
  }

  fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    self.frontend.process(&mut self.host);
    // A reload sizes the output from the config; the window decides.
    if self.frontend.take_reloaded_config().is_some()
      && let Some(window) = &self.host.window
    {
      let size = window.inner_size();
      if let Err(error) = self.frontend.output_resized(size.width, size.height)
      {
        eprintln!("shingle: resizing the output after a reload: {error}");
      }
    }
    let exiting = self.host.exit;
    self.finish(event_loop);
    if exiting {
      return;
    }
    let poll = Instant::now() + Duration::from_millis(16);
    let wake = self
      .frontend
      .next_deadline()
      .map_or(poll, |deadline| deadline.min(poll));
    event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
  }

  fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
    self.frontend.shutdown();
  }
}

/// Returns the seat device a host window event comes from, if it is input.
fn window_event_device(event: &WindowEvent) -> Option<InputDeviceKind> {
  match event {
    WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) => {
      Some(InputDeviceKind::Keyboard)
    }
    WindowEvent::CursorMoved { .. }
    | WindowEvent::MouseInput { .. }
    | WindowEvent::MouseWheel { .. }
    | WindowEvent::PinchGesture { .. }
    | WindowEvent::PanGesture { .. }
    | WindowEvent::DoubleTapGesture { .. }
    | WindowEvent::RotationGesture { .. } => Some(InputDeviceKind::Pointer),
    WindowEvent::Touch(_) => Some(InputDeviceKind::Touch),
    _ => None,
  }
}

/// Runs Shingle nested in a window of the current host session until it
/// quits, the window is closed or a SIGINT or SIGTERM arrives.
///
/// `config` was loaded from `config_path`, which config reloads read again.
/// With `ipc_path`, Wayfire IPC is served there, and every client finds it
/// as `WAYFIRE_SOCKET`. `spawn`, if not empty, is a program and its
/// arguments to start as a client.
pub fn run(
  config: ShingleConfig,
  config_path: Option<PathBuf>,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
) -> Result<(), Box<dyn Error>> {
  // Connect to the host first, so that no client is started when there is
  // no session to show it in.
  let event_loop = EventLoop::new()?;
  let mut app = App::new(config, spawn, config_path, ipc_path)?;
  event_loop.run_app(&mut app)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::atomic::{AtomicU32, Ordering},
  };

  use shingle::{Config, Event};
  use winit::event::TouchPhase;

  use super::*;

  #[test]
  fn touchpad_scrolls_are_fingers_that_stop_and_wheels_are_wheels() {
    let wheel =
      scroll_axis(MouseScrollDelta::LineDelta(0.0, 2.0), TouchPhase::Moved);
    assert_eq!(wheel.source, AxisSource::Wheel);
    assert_eq!((wheel.vertical, wheel.vertical_steps), (20.0, 2));
    let pixels = PhysicalPosition::new(0.0, 7.5);
    let finger =
      scroll_axis(MouseScrollDelta::PixelDelta(pixels), TouchPhase::Moved);
    assert_eq!(finger.source, AxisSource::Finger);
    assert_eq!((finger.vertical, finger.vertical_steps), (7.5, 0));
    assert!(!finger.vertical_stop && !finger.horizontal_stop);
    let end = PhysicalPosition::new(0.0, 0.0);
    let ended =
      scroll_axis(MouseScrollDelta::PixelDelta(end), TouchPhase::Ended);
    assert!(ended.vertical_stop && ended.horizontal_stop);
  }

  /// The `input/list-devices` reply of the nested backend before the device
  /// registry, recorded byte for byte from a real run of `master` at
  /// 7189b50.
  const NESTED_LIST_DEVICES: &str = concat!(
    r#"[{"enabled":true,"id":1,"name":"Shingle nested keyboard","#,
    r#""product":"unknown","type":"keyboard","vendor":"unknown"},"#,
    r#"{"enabled":true,"id":2,"name":"Shingle nested pointer","#,
    r#""product":"unknown","type":"pointer","vendor":"unknown"},"#,
    r#"{"enabled":true,"id":3,"name":"Shingle nested touch","#,
    r#""product":"unknown","type":"touch","vendor":"unknown"}]"#,
  );

  /// A compositor with its Wayland and IPC sockets in the temporary
  /// directory, set up the way `App::new` sets up the nested one.
  struct IpcFixture {
    compositor: Compositor,
    devices: SeatDevices,
    ipc: PathBuf,
  }

  impl IpcFixture {
    fn new() -> Self {
      static NEXT: AtomicU32 = AtomicU32::new(0);
      let name = format!(
        "shingle-nested-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
      );
      let base = std::env::temp_dir().join(name);
      let mut compositor =
        Compositor::with_socket_path(Config::default(), &base)
          .expect("the temporary directory accepts a Wayland socket");
      let devices = SeatDevices::register(&mut compositor);
      let ipc = base.with_extension("ipc");
      compositor
        .enable_ipc(&ipc)
        .expect("the temporary directory accepts an IPC socket");
      Self {
        compositor,
        devices,
        ipc,
      }
    }

    /// Sends one IPC request and returns the raw reply.
    fn request(&mut self, request: &str) -> String {
      let mut stream =
        UnixStream::connect(&self.ipc).expect("the IPC socket accepts");
      let length = u32::try_from(request.len()).expect("a short request");
      stream
        .write_all(&length.to_ne_bytes())
        .and_then(|()| stream.write_all(request.as_bytes()))
        .expect("the request is sent");
      stream
        .set_nonblocking(true)
        .expect("the stream is nonblocking");
      let mut reply = Vec::new();
      let deadline = Instant::now() + Duration::from_secs(2);
      while Instant::now() < deadline {
        self
          .compositor
          .poll(Some(Duration::from_millis(10)))
          .expect("dispatch succeeds");
        let mut buffer = [0; 4096];
        match stream.read(&mut buffer) {
          Ok(read) => reply.extend_from_slice(&buffer[..read]),
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
          Err(error) => panic!("reading the reply failed: {error}"),
        }
        if reply.len() >= 4 {
          let length = u32::from_ne_bytes(reply[..4].try_into().unwrap());
          if reply.len() >= 4 + length as usize {
            return String::from_utf8(reply[4..].to_vec())
              .expect("the reply is UTF-8");
          }
        }
      }
      panic!("no IPC reply to {request}");
    }
  }

  #[test]
  fn nested_list_devices_reply_is_unchanged() {
    let mut fixture = IpcFixture::new();
    let reply = fixture.request(r#"{"method":"input/list-devices","data":{}}"#);
    assert_eq!(reply, NESTED_LIST_DEVICES);
  }

  #[test]
  fn seat_devices_get_ids_one_to_three_with_their_kind() {
    let fixture = IpcFixture::new();
    let devices = fixture.devices;
    let kinds = [
      InputDeviceKind::Keyboard,
      InputDeviceKind::Pointer,
      InputDeviceKind::Touch,
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
      let id = devices.get(kind);
      assert_eq!(id.get() as usize, index + 1);
      let info = fixture.compositor.input_device(id).expect("registered");
      assert_eq!(info.kind, kind);
      assert!(fixture.compositor.input_device_enabled(id));
    }
  }

  #[test]
  fn configure_device_events_carry_the_nested_device_id() {
    let mut fixture = IpcFixture::new();
    fixture.request(
      r#"{"method":"input/configure-device","data":{"id":3,"enabled":false}}"#,
    );
    let changes: Vec<_> = fixture
      .compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::InputDeviceChanged { id, enabled } => Some((id, enabled)),
        _ => None,
      })
      .collect();
    assert_eq!(changes, [(fixture.devices.touch, false)]);
    assert_eq!(
      fixture
        .compositor
        .input_device(fixture.devices.touch)
        .map(|info| info.kind),
      Some(InputDeviceKind::Touch)
    );
  }

  #[test]
  fn nested_configure_device_replies_are_unchanged() {
    let mut fixture = IpcFixture::new();
    let cases = [
      (r#"{"id":2,"enabled":false}"#, r#"{"result":"ok"}"#),
      (
        r#"{"id":4,"enabled":false}"#,
        r#"{"error":"Unknown input device!"}"#,
      ),
      (
        r#"{"id":0,"enabled":true}"#,
        r#"{"error":"Unknown input device!"}"#,
      ),
      (r#"{"enabled":true}"#, r#"{"error":"Missing \"id\""}"#),
    ];
    for (data, expected) in cases {
      let request =
        format!(r#"{{"method":"input/configure-device","data":{data}}}"#);
      assert_eq!(fixture.request(&request), expected, "{data}");
    }
    let reply = fixture.request(r#"{"method":"input/list-devices","data":{}}"#);
    assert_eq!(
      reply,
      NESTED_LIST_DEVICES.replacen(
        r#""enabled":true,"id":2"#,
        r#""enabled":false,"id":2"#,
        1
      )
    );
  }

  #[test]
  fn host_input_events_map_to_their_seat_device() {
    let device_id = DeviceId::dummy();
    let cases = [
      (
        WindowEvent::ModifiersChanged(Default::default()),
        Some(InputDeviceKind::Keyboard),
      ),
      (
        WindowEvent::CursorMoved {
          device_id,
          position: winit::dpi::PhysicalPosition::new(1.0, 2.0),
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::MouseInput {
          device_id,
          state: ElementState::Pressed,
          button: MouseButton::Left,
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::MouseWheel {
          device_id,
          delta: MouseScrollDelta::LineDelta(0.0, 1.0),
          phase: TouchPhase::Moved,
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::PinchGesture {
          device_id,
          delta: 0.1,
          phase: TouchPhase::Started,
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::Touch(winit::event::Touch {
          device_id,
          phase: TouchPhase::Started,
          location: winit::dpi::PhysicalPosition::new(1.0, 2.0),
          force: None,
          id: 0,
        }),
        Some(InputDeviceKind::Touch),
      ),
      (WindowEvent::CloseRequested, None),
      (WindowEvent::Resized(PhysicalSize::new(10, 10)), None),
      (WindowEvent::Focused(true), None),
    ];
    for (event, device) in cases {
      assert_eq!(window_event_device(&event), device, "{event:?}");
    }
  }

  #[test]
  fn left_handed_swaps_real_buttons() {
    let mut buttons = MouseButtons::default();
    assert_eq!(
      buttons.translate(BTN_LEFT, true, false, true),
      Some(BTN_RIGHT)
    );
    assert_eq!(
      buttons.translate(BTN_RIGHT, true, false, true),
      Some(BTN_LEFT)
    );
    assert_eq!(
      buttons.translate(BTN_LEFT, false, false, true),
      Some(BTN_RIGHT)
    );
    assert_eq!(buttons.translate(0x113, true, false, true), Some(0x113));
  }

  #[test]
  fn middle_emulation_balances_presses_and_releases() {
    let mut buttons = MouseButtons::default();
    assert_eq!(
      buttons.translate(BTN_LEFT, true, true, false),
      Some(BTN_LEFT)
    );
    assert_eq!(
      buttons.translate(BTN_RIGHT, true, true, false),
      Some(BTN_MIDDLE)
    );
    assert_eq!(
      buttons.translate(BTN_LEFT, false, true, false),
      Some(BTN_MIDDLE)
    );
    assert_eq!(
      buttons.translate(BTN_RIGHT, false, true, false),
      Some(BTN_RIGHT)
    );
    assert!(!buttons.left && !buttons.right && !buttons.emulated_middle);
  }

  #[test]
  fn disabled_emulation_preserves_independent_buttons() {
    let mut buttons = MouseButtons::default();
    for button in [BTN_LEFT, BTN_RIGHT] {
      assert_eq!(buttons.translate(button, true, false, false), Some(button));
    }
    assert!(!buttons.emulated_middle);
  }

  #[test]
  fn the_window_opens_at_an_explicit_configured_mode() {
    let mut config = ShingleConfig::default();
    assert_eq!(super::initial_window_size(&config), (1280, 720));
    config.output.width = 1920;
    assert_eq!(
      super::initial_window_size(&config),
      (1280, 720),
      "only a mode sets the window size"
    );
    config.output.mode = "1600x900@75".parse().unwrap();
    assert_eq!(super::initial_window_size(&config), (1600, 900));
    config.outputs.push(shingle_config::OutputConfig {
      mode: "800x600".parse().unwrap(),
      ..shingle_config::OutputConfig::named(shingle::OUTPUT_NAME)
    });
    assert_eq!(super::initial_window_size(&config), (800, 600));
  }
}
