//! The nested backend: Shingle in windows inside another Wayland or X11
//! session, one window per output.
//!
//! This library is a thin winit host for [`shingle_frontend::Frontend`]: it
//! creates the windows and one GPU context they share, turns each window's
//! winit events into [`InputEvent`]s in its own output's pixels and evdev
//! codes, presents each output in its own window, and keeps the host-only
//! state (keyboard focus changes, button emulation and the pointer-warp
//! echo filter). `--outputs N` opens `N` windows, and
//! `stipc/create_wayland_output` and `stipc/destroy_wayland_output` open
//! and close more, as in a nested Wayfire (docs/design/multi-output.md,
//! section 7). The `shingle` binary (crates/shingle-bin) parses the
//! command line, selects this backend and calls [`run`].

mod clipboard;
mod host_keyboard;
mod waker;
mod windows;

use std::{
  error::Error,
  ffi::OsString,
  os::fd::AsFd,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use shingle::{
  Compositor, InputDeviceId, InputDeviceInfo, InputDeviceKind, OutputHandle,
  OutputInfo, SelectionSourceId,
};
use shingle_config::{Modifiers as ConfigModifiers, ShingleConfig};
use shingle_frontend::{
  Frontend,
  host::{Axis, AxisSource, Host, InputEvent, Touch, TouchPhase},
  render_target::RenderTarget,
};
use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, RenderError, RenderState, RenderTile,
  TextureTransform, WindowTarget, capture_frame,
};
use windows::{
  Focus, Windows, initial_window_size, output_scale, start_output_name,
  window_title,
};
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

/// One host window: the output it shows is its key in [`Windows`]. The
/// target holds the window's surface, so it is dropped first.
struct OutputWindow {
  target: WindowTarget,
  state: RenderState,
  window: Arc<Window>,
  /// Where the last warp put the host cursor, in window pixels, until the
  /// host echoes it back as a cursor motion.
  pending_warp: Option<(f64, f64)>,
}

/// The frontend's render target for one window: its surface and view
/// state on the one GPU context all windows share.
struct WindowOutput<'a> {
  gpu: &'a mut GpuContext,
  window: &'a mut OutputWindow,
}

impl RenderTarget for WindowOutput<'_> {
  fn set_scale_factor(&mut self, scale: f64) {
    self.window.state.set_scale_factor(scale);
  }

  fn set_output_transform(&mut self, transform: TextureTransform) {
    self.window.state.set_output_transform(transform);
  }

  fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    self.window.state.set_nearest_filter(nearest)
  }

  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    self.window.state.set_zoom(factor, origin_x, origin_y);
  }

  fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    self.gpu.set_gamma_ramp(ramp)
  }

  fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    self.gpu.set_inversion(inverted, preserve_hue);
  }

  fn set_tearing(&mut self, tearing: bool) {
    self.window.target.set_tearing(self.gpu, tearing);
  }

  fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError> {
    self.gpu.create_texture(
      width,
      height,
      rgba,
      self.window.state.texture_filter(),
    )
  }

  fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    self.gpu.update_texture(texture, rgba)
  }

  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    self
      .window
      .target
      .render(self.gpu, &self.window.state, tiles, clear)
  }

  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    capture_frame(self.gpu, &self.window.state, size, tiles, clear)
  }

  fn capture_scaled(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
    scale: f64,
  ) -> Option<Result<Vec<u8>, RenderError>> {
    Some(shingle_render::capture_frame_scaled(
      self.gpu,
      &self.window.state,
      size,
      scale,
      tiles,
      clear,
    ))
  }

  fn capture_into(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    buffer: &shingle_render::DmabufDescriptor<'_>,
  ) -> Result<(), RenderError> {
    shingle_render::capture_into_dmabuf(
      self.gpu,
      &self.window.state,
      buffer,
      tiles,
      clear,
    )
  }
}

/// The [`Host`] side of the nested windows: what the frontend asks of
/// them during one call, applied by [`App::finish`] afterwards.
#[derive(Default)]
struct WindowHost {
  /// The outputs a frame was asked for.
  redraw: Vec<OutputHandle>,
  /// A pointer warp the frontend asked for: the output and the position
  /// on it, in its logical pixels.
  warp: Option<(OutputHandle, f64, f64)>,
  /// Outputs `stipc/create_wayland_output` added, whose windows are to
  /// be opened.
  opening: Vec<OutputHandle>,
  /// Outputs `stipc/destroy_wayland_output` removed, whose windows are to
  /// be closed.
  closing: Vec<OutputHandle>,
  exit: bool,
  /// The pointer device, whose button emulation resets when it is
  /// disabled.
  pointer: Option<InputDeviceId>,
  mouse_buttons: MouseButtons,
}

impl Host for WindowHost {
  fn request_redraw(&mut self, output: &OutputHandle) {
    if !self.redraw.contains(output) {
      self.redraw.push(output.clone());
    }
  }

  fn warp_pointer(&mut self, output: &OutputHandle, x: f64, y: f64) {
    self.warp = Some((output.clone(), x, y));
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

  fn open_output(&mut self, output: &OutputHandle) {
    self.opening.push(output.clone());
  }

  fn close_output(&mut self, output: &OutputHandle) {
    self.closing.push(output.clone());
  }
}

struct App {
  frontend: Frontend,
  devices: SeatDevices,
  host: WindowHost,
  /// The windows, by the output each shows; dropped before the GPU.
  windows: Windows<WindowId, OutputWindow>,
  /// The window the host pointer was last in, whose output relative
  /// motion is mapped by.
  pointer_window: Option<WindowId>,
  /// Which windows have keyboard focus.
  focus: Focus<WindowId>,
  /// The GPU every window draws with, created with the first window.
  gpu: Option<GpuContext>,
  host_keyboard: host_keyboard::HostKeyboard,
  /// The configuration, for the sizes new windows open at; replaced on
  /// every reload.
  config: ShingleConfig,
  /// How many windows to open at start.
  outputs: u32,
  /// The clipboard and primary selection shared with the host, while the
  /// bridge runs.
  clipboard: Option<clipboard::host::HostClipboard<SelectionSourceId>>,
  /// Wakes the loop when the compositor has work; without it, clients
  /// and IPC wait for the 16 ms poll.
  waker: Option<waker::Waker>,
}

impl App {
  fn new(
    config: ShingleConfig,
    spawn: Vec<OsString>,
    config_path: Option<PathBuf>,
    ipc_path: Option<&Path>,
    outputs: u32,
    host_socket: Option<&Path>,
  ) -> Result<Self, Box<dyn Error>> {
    let mut frontend =
      Frontend::new(config.clone(), spawn, config_path, ipc_path)?;
    let devices = SeatDevices::register(frontend.compositor_mut());
    frontend.compositor_mut().set_host_outputs(true);
    let clipboard = clipboard::share_with_host(
      frontend.compositor(),
      config.behavior.host_clipboard,
      host_socket,
    );
    Ok(Self {
      frontend,
      devices,
      host: WindowHost {
        pointer: Some(devices.pointer),
        ..WindowHost::default()
      },
      windows: Windows::default(),
      pointer_window: None,
      focus: Focus::default(),
      gpu: None,
      host_keyboard: host_keyboard::HostKeyboard::default(),
      config,
      outputs: outputs.max(1),
      clipboard,
      waker: None,
    })
  }

  /// Runs the host clipboard bridge's half of a turn: what the host sent
  /// before the compositor's turn, and nested's changes and reads after
  /// it (`after`). A failed connection stops the bridge, and the host's
  /// selections leave nested.
  fn share_clipboard(&mut self, after: bool) {
    let Some(clipboard) = &mut self.clipboard else {
      return;
    };
    let compositor = self.frontend.compositor_mut();
    let result = if after {
      clipboard.sync(compositor)
    } else {
      clipboard.dispatch(compositor)
    };
    if let Err(error) = result {
      eprintln!(
        "shingle: the connection to the host's clipboard failed: {error}"
      );
      clipboard.abandon(compositor);
      self.clipboard = None;
    } else if clipboard.idle() {
      self.clipboard = None;
    }
  }

  /// Opens the window that shows `output` and gives the output the
  /// window's size. The first window also creates the GPU context, on an
  /// adapter that can present to it.
  fn open_window(
    &mut self,
    event_loop: &ActiveEventLoop,
    output: &OutputHandle,
  ) -> Result<(), Box<dyn Error>> {
    let size = initial_window_size(&self.config, output.name());
    let attributes = WindowAttributes::default()
      .with_title(window_title(output.name()))
      .with_inner_size(PhysicalSize::new(size.0, size.1));
    let window = Arc::new(event_loop.create_window(attributes)?);
    let size = window.inner_size();
    let scale =
      output_scale(&self.config, output.name(), window.scale_factor());
    let target = match &self.gpu {
      Some(gpu) => {
        let surface =
          WindowTarget::create_surface(gpu.instance(), window.clone())?;
        WindowTarget::new(gpu, surface, (size.width, size.height))?
      }
      None => {
        let instance = GpuContext::create_instance();
        let surface = WindowTarget::create_surface(&instance, window.clone())?;
        let gpu = pollster::block_on(GpuContext::new(
          instance,
          GpuOptions {
            compatible_surface: Some(&surface),
            dmabuf: true,
            ..Default::default()
          },
        ))?;
        eprintln!("shingle: {}", gpu.dmabuf_summary());
        let target =
          WindowTarget::new(&gpu, surface, (size.width, size.height))?;
        self.gpu = Some(gpu);
        target
      }
    };
    // The output takes the window's scale factor unless configured
    // otherwise, so that its logical pixels are the host's.
    let compositor = self.frontend.compositor_mut();
    compositor.set_output_scale_on(output, scale)?;
    compositor.set_output_size(output, (size.width, size.height))?;
    let window_id = window.id();
    self.windows.insert(
      window_id,
      output.clone(),
      OutputWindow {
        target,
        state: RenderState::default(),
        window,
        pending_warp: None,
      },
    );
    Ok(())
  }

  /// Opens the windows of the outputs nested starts with, `HEADLESS-1` to
  /// `HEADLESS-N`, and starts the clients.
  fn start(
    &mut self,
    event_loop: &ActiveEventLoop,
  ) -> Result<(), Box<dyn Error>> {
    let first = self.frontend.compositor().primary_output();
    self.open_window(event_loop, &first)?;
    for number in 2..=self.outputs {
      let name = start_output_name(number);
      let size = initial_window_size(&self.config, &name);
      let output = self.frontend.compositor_mut().add_output(
        OutputInfo {
          name,
          ..OutputInfo::default()
        },
        size,
      )?;
      self.open_window(event_loop, &output)?;
    }
    // Clients start only now that linux-dmabuf can be offered: most look
    // for the global once (docs/design/dmabuf.md, section 2).
    if let Some(gpu) = &self.gpu {
      self.frontend.attach_gpu(gpu);
    }
    self.frontend.start_clients()
  }

  /// Carries out what the frontend asked of the host: asks the windows
  /// for frames, warps the host cursor, opens and closes windows and ends
  /// the event loop. Requests for outputs without a window are dropped.
  fn finish(&mut self, event_loop: &ActiveEventLoop) {
    for output in std::mem::take(&mut self.host.redraw) {
      if let Some(window) = self.windows.showing(&output) {
        window.window.request_redraw();
      }
    }
    if let Some((output, x, y)) = self.host.warp.take() {
      self.warp(&output, x, y);
    }
    // Closing first, so that a window closed and opened again in one
    // turn is open afterwards.
    for output in std::mem::take(&mut self.host.closing) {
      self.windows.remove_showing(&output);
    }
    for output in std::mem::take(&mut self.host.opening) {
      if let Err(error) = self.open_window(event_loop, &output) {
        eprintln!(
          "shingle: failed to open a window for {}: {error}",
          output.name()
        );
        if let Err(error) =
          self.frontend.compositor_mut().remove_output(&output)
        {
          eprintln!("shingle: removing {}: {error}", output.name());
        }
      }
    }
    if std::mem::take(&mut self.host.exit) {
      event_loop.exit();
    }
  }

  /// Moves the host cursor to `(x, y)` on `output`, in its logical pixels.
  fn warp(&mut self, output: &OutputHandle, x: f64, y: f64) {
    let compositor = self.frontend.compositor();
    let Some(window) = self.windows.showing_mut(output) else {
      return;
    };
    let size = window.window.inner_size();
    let Some(point) =
      compositor.unmap_point_on(output, x, y, size.width, size.height)
    else {
      return;
    };
    window.pending_warp = Some(point);
    if let Err(error) = window
      .window
      .set_cursor_position(PhysicalPosition::new(point.0, point.1))
    {
      eprintln!("shingle: pointer warp failed: {error}");
    }
  }

  /// A point in window `id`, in the physical pixels winit reports
  /// pointer and touch positions in, as a position on the output the
  /// window shows, relative to that output's top-left corner, so that the
  /// frontend maps the window's pointer and touch to its own output.
  fn output_point(
    &self,
    id: WindowId,
    x: f64,
    y: f64,
  ) -> Option<(f64, f64, Option<OutputHandle>)> {
    let output = self.windows.output_of(id)?;
    let size = self.windows.get(id)?.window.inner_size();
    let (x, y) = window_point(self.frontend.compositor(), output, x, y, size)?;
    Some((x, y, Some(output.clone())))
  }

  /// Gives the output of window `id` the window's scale factor, after the
  /// window moved to a monitor of another scale, unless the config sets
  /// the output's scale ([`output_scale`]). The physical size
  /// follows with the `Resized` event winit sends next.
  fn rescale_output(&mut self, id: WindowId, scale: f64) {
    let Some(output) = self.windows.output_of(id).cloned() else {
      return;
    };
    let scale = output_scale(&self.config, output.name(), scale);
    if let Err(error) = self.frontend.output_rescaled_on(&output, scale) {
      eprintln!("shingle: rescaling {}: {error}", output.name());
    }
  }

  /// Feeds host keyboard and focus events of window `id` to the frontend.
  /// Returns whether the event was a keyboard event.
  fn keyboard_event(&mut self, id: WindowId, event: &WindowEvent) -> bool {
    let keyboard = self.devices.keyboard;
    match event {
      WindowEvent::Focused(focused) => {
        if let Some(focused) = self.focus.changed(id, *focused) {
          self.host_keyboard.focus_changed(focused);
          if !focused {
            self.frontend.handle_input(
              &mut self.host,
              keyboard,
              InputEvent::Reset,
              None,
            );
          }
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

  /// Closes window `id`: its output goes under `output_layout.unplug`.
  /// Closing the last window quits, as it always has.
  fn close_window(&mut self, event_loop: &ActiveEventLoop, id: WindowId) {
    if self.windows.len() <= 1 {
      self.frontend.shutdown();
      event_loop.exit();
      return;
    }
    let Some((output, _window)) = self.windows.remove(id) else {
      return;
    };
    if self.pointer_window == Some(id) {
      self.pointer_window = None;
    }
    self.focus.changed(id, false);
    if let Err(error) = self.frontend.compositor_mut().remove_output(&output) {
      eprintln!("shingle: removing {}: {error}", output.name());
    }
  }

  /// Gives every window's output the window's scale and size again.
  fn resize_outputs(&mut self) {
    let sizes = self
      .windows
      .iter()
      .map(|(_, output, window)| {
        let window = &window.window;
        let scale =
          output_scale(&self.config, output.name(), window.scale_factor());
        (output.clone(), scale, window.inner_size())
      })
      .collect::<Vec<_>>();
    for (output, scale, size) in sizes {
      if let Err(error) = self.frontend.output_rescaled_on(&output, scale) {
        eprintln!(
          "shingle: rescaling {} after a reload: {error}",
          output.name()
        );
      }
      if let Err(error) =
        self
          .frontend
          .output_resized_on(&output, size.width, size.height)
      {
        eprintln!(
          "shingle: resizing {} after a reload: {error}",
          output.name()
        );
      }
    }
  }
}

impl ApplicationHandler for App {
  fn resumed(&mut self, event_loop: &ActiveEventLoop) {
    if self.gpu.is_some() {
      return;
    }
    if let Err(error) = self.start(event_loop) {
      eprintln!("shingle: failed to start nested: {error}");
      event_loop.exit();
    }
  }

  fn window_event(
    &mut self,
    event_loop: &ActiveEventLoop,
    window_id: WindowId,
    event: WindowEvent,
  ) {
    if self.windows.get(window_id).is_none() {
      return;
    }
    // Keyboard state is tracked even while the keyboard device is disabled,
    // so modifiers are correct again the moment it is re-enabled.
    if self.keyboard_event(window_id, &event) {
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
      WindowEvent::CloseRequested => self.close_window(event_loop, window_id),
      WindowEvent::Resized(size) => {
        let Some((output, window)) = self.windows.get_mut(window_id) else {
          return;
        };
        let output = output.clone();
        if let Some(gpu) = &self.gpu {
          window.target.resize(gpu, (size.width, size.height));
        }
        if let Err(error) =
          self
            .frontend
            .output_resized_on(&output, size.width, size.height)
        {
          eprintln!("shingle: failed to resize {}: {error}", output.name());
          event_loop.exit();
        }
      }
      WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
        self.rescale_output(window_id, scale_factor);
      }
      WindowEvent::RedrawRequested => {
        let (Some(gpu), Some((output, window))) =
          (self.gpu.as_mut(), self.windows.get_mut(window_id))
        else {
          return;
        };
        let output = output.clone();
        let mut target = WindowOutput { gpu, window };
        let result = self.frontend.render(&mut self.host, &output, &mut target);
        if let Err(error) = result {
          eprintln!("shingle: render of {} failed: {error}", output.name());
          event_loop.exit();
        }
      }
      WindowEvent::CursorEntered { .. } => {
        self.pointer_window = Some(window_id);
      }
      WindowEvent::CursorMoved { position, .. } => {
        self.pointer_window = Some(window_id);
        if let Some((_, window)) = self.windows.get_mut(window_id)
          && window.pending_warp.is_some_and(|pending| {
            (position.x - pending.0).abs() <= 1.0
              && (position.y - pending.1).abs() <= 1.0
          })
        {
          window.pending_warp = None;
          return;
        }
        if let Some((x, y, output)) =
          self.output_point(window_id, position.x, position.y)
        {
          self.input(
            self.devices.pointer,
            InputEvent::PointerMotionAbsolute { x, y, output },
          );
        }
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
        let Some((x, y, output)) =
          self.output_point(window_id, touch.location.x, touch.location.y)
        else {
          return;
        };
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
            output,
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
    // Relative motion is mapped by the window the pointer is in.
    let mapped = self.pointer_window.and_then(|id| {
      let output = self.windows.output_of(id)?;
      let window = &self.windows.get(id)?.window;
      let host_scale = window.scale_factor();
      let size = window.inner_size();
      self.frontend.compositor().map_vector_on(
        output,
        dx * host_scale,
        dy * host_scale,
        size.width,
        size.height,
      )
    });
    let (dx, dy) = mapped.unwrap_or((dx, dy));
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
    // Host selections first, so that the compositor's turn flushes them
    // to nested's clients; nested's own changes after it, so that the
    // host hears of them in the same turn.
    self.share_clipboard(false);
    self.frontend.process(&mut self.host);
    self.share_clipboard(true);
    // A reload sizes the outputs from the config; the windows decide.
    if let Some(config) = self.frontend.take_reloaded_config() {
      self.config = config;
      self.resize_outputs();
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
    // This turn dispatched the compositor, so its descriptor is worth
    // watching again.
    if let Some(waker) = &self.waker {
      waker.arm();
    }
  }

  fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
    self.frontend.shutdown();
  }
}

/// Maps `(x, y)` in physical pixels of a host window of `size` physical
/// pixels showing `output` to that output's logical pixels, relative to
/// its top-left corner. winit reports pointer and touch positions in
/// physical pixels, and the output's logical size is the window's
/// physical size divided by its scale factor, so this undoes the scale.
fn window_point(
  compositor: &Compositor,
  output: &OutputHandle,
  x: f64,
  y: f64,
  size: PhysicalSize<u32>,
) -> Option<(f64, f64)> {
  compositor.map_point_on(output, x, y, size.width, size.height)
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

/// Runs Shingle nested in windows of the current host session until it
/// quits, its last window is closed or a SIGINT or SIGTERM arrives.
///
/// `config` was loaded from `config_path`, which config reloads read again.
/// With `ipc_path`, Wayfire IPC is served there, and every client finds it
/// as `WAYFIRE_SOCKET`. `spawn`, if not empty, is a program and its
/// arguments to start as a client. Nested starts with `outputs` windows,
/// one per output, `HEADLESS-1` to `HEADLESS-N`;
/// `stipc/create_wayland_output` opens more and
/// `stipc/destroy_wayland_output` closes them. Closing a window removes
/// its output under `output_layout.unplug`.
pub fn run(
  config: ShingleConfig,
  config_path: Option<PathBuf>,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
  outputs: u32,
) -> Result<(), Box<dyn Error>> {
  // Connect to the host first, so that no client is started when there is
  // no session to show it in.
  let event_loop = EventLoop::new()?;
  let host_socket =
    clipboard::host::host_socket(|name| std::env::var(name).ok());
  let mut app = App::new(
    config,
    spawn,
    config_path,
    ipc_path,
    outputs,
    host_socket.as_deref(),
  )?;
  let proxy = event_loop.create_proxy();
  match waker::Waker::start(app.frontend.compositor().as_fd(), move || {
    proxy.send_event(()).is_ok()
  }) {
    Ok(waker) => app.waker = Some(waker),
    Err(error) => eprintln!(
      "shingle: clients wait for the 16 ms poll, no waker thread: {error}"
    ),
  }
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

  /// The scale factors of the hosts nested is checked on: none, a
  /// fractional one and an integer one.
  const HOST_SCALES: [f64; 3] = [1.0, 1.5, 2.0];

  /// Gives the fixture's output a 3840x2160 host window of `scale`, as
  /// `App::open_window` does.
  fn scaled_output(fixture: &mut IpcFixture, scale: f64) -> OutputHandle {
    let output = fixture.compositor.primary_output();
    fixture
      .compositor
      .set_output_scale_on(&output, scale)
      .expect("a valid scale");
    fixture
      .compositor
      .set_output_size(&output, (3840, 2160))
      .expect("a valid size");
    output
  }

  #[test]
  fn the_output_takes_the_host_windows_scale_and_logical_size() {
    let mut fixture = IpcFixture::new();
    for scale in HOST_SCALES {
      let output = scaled_output(&mut fixture, scale);
      let view = fixture.compositor.output_view(&output).expect("an output");
      assert_eq!(view.scale, scale);
      let logical = ((3840.0 / scale) as u32, (2160.0 / scale) as u32);
      assert_eq!((view.rect.width, view.rect.height), logical, "{scale}");
      assert_eq!(
        fixture.compositor.physical_size_of(&output),
        Some((3840, 2160))
      );
    }
  }

  #[test]
  fn the_pointer_lands_where_the_host_cursor_is_at_any_host_scale() {
    let mut fixture = IpcFixture::new();
    let window = PhysicalSize::new(3840, 2160);
    for scale in HOST_SCALES {
      let output = scaled_output(&mut fixture, scale);
      // winit reports CursorMoved in physical pixels: the host cursor at
      // physical (1500, 900) is at logical (1500, 900) / scale.
      let point =
        window_point(&fixture.compositor, &output, 1500.0, 900.0, window)
          .expect("the output exists");
      let close = |a: (f64, f64), b: (f64, f64)| {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
      };
      assert!(close(point, (1500.0 / scale, 900.0 / scale)), "{scale}");
      // And back, as a pointer warp places the host cursor.
      let back = fixture
        .compositor
        .unmap_point_on(&output, point.0, point.1, 3840, 2160)
        .expect("the output exists");
      assert!(close(back, (1500.0, 900.0)), "{scale}");
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
}
