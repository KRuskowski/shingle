//! The native host: the event loop that runs [`Frontend`] on a seat.
//!
//! [`Backend`] owns the frontend, the [`Session`], the [`InputSource`] and
//! the [`DeviceRegistry`]. Each [`Backend::step`] dispatches the seat,
//! feeds libinput's events to the frontend with their timestamps, runs the
//! frontend's turn, carries out what the frontend asked of its
//! [`NativeHost`] and draws a frame if one is due. [`Backend::run`] repeats
//! that, sleeping in `poll(2)` on the seat, libinput and compositor
//! descriptors in between, until the frontend asks to exit, and then saves
//! the session and closes every device.
//!
//! A seat pause ([`SeatEvent::Disable`], for example a VT switch) follows
//! docs/design/native-backend.md, "Pause": stop drawing, suspend libinput,
//! pause the frontend (which releases held input) and acknowledge. A
//! resume reopens the devices, which keep their seat device ids, resumes
//! the frontend, resends the keyboard LEDs and draws again.

use std::{
  cell::RefCell,
  error::Error,
  io,
  os::fd::{AsFd, BorrowedFd},
  rc::Rc,
  time::{Duration, Instant},
};

use shingle::{InputDeviceId, InputDeviceKind};
use shingle_config::InputConfig;
use shingle_frontend::{
  Frontend,
  host::{Host, InputEvent},
  keyboard::Leds,
  render_target::RenderTarget,
};

use crate::{
  diagnostics,
  input::{
    DeviceInput, InputSource, NativeEvent, OutputGeometry, SendEvents,
    Translator, device_settings, enabled_mode,
  },
  session::{SeatEvent, Session},
  udev::{Added, DeviceRegistry, input_kind},
};

/// How long the loop sleeps at most, for the frontend's config reload,
/// drag and gesture timers and child reaping, as nested does.
const TICK: Duration = Duration::from_millis(16);

/// What the frontend asks of the native host during one call, carried out
/// by the [`Backend`] afterwards.
pub struct NativeHost<S> {
  session: Rc<RefCell<S>>,
  /// A frame was requested.
  redraw: bool,
  /// The frontend warped the pointer here.
  warp: Option<(f64, f64)>,
  /// The frontend asked the loop to end.
  exit: bool,
  /// Seat devices IPC enabled or disabled.
  device_changes: Vec<(InputDeviceId, bool)>,
}

impl<S> NativeHost<S> {
  /// A host for the seat `session`.
  pub fn new(session: Rc<RefCell<S>>) -> Self {
    Self {
      session,
      redraw: false,
      warp: None,
      exit: false,
      device_changes: Vec::new(),
    }
  }
}

impl<S: Session> Host for NativeHost<S> {
  fn request_redraw(&mut self) {
    self.redraw = true;
  }

  fn warp_pointer(&mut self, x: f64, y: f64) {
    self.warp = Some((x, y));
  }

  /// Asks libseat to switch VT. A refusal is logged and the key handled
  /// like any other.
  fn switch_vt(&mut self, vt: u32) -> bool {
    match self.session.borrow_mut().switch_vt(vt) {
      Ok(()) => true,
      Err(error) => {
        diagnostics::log(&format!("switching to VT {vt} failed: {error}"));
        false
      }
    }
  }

  fn input_device_changed(&mut self, device: InputDeviceId, enabled: bool) {
    self.device_changes.push((device, enabled));
  }

  fn exit(&mut self) {
    self.exit = true;
  }
}

/// The native backend's state and event loop.
pub struct Backend<S: Session, I: InputSource> {
  frontend: Frontend,
  host: NativeHost<S>,
  input: I,
  devices: DeviceRegistry,
  translator: Translator,
  /// The `[input]` options for device configuration.
  config: InputConfig,
  /// The output in pixels, for absolute and touch positions.
  output_size: (u32, u32),
  /// Whether the seat is enabled.
  active: bool,
  renderer: Option<Box<dyn RenderTarget>>,
  frame_interval: Duration,
  next_frame: Instant,
  /// The LEDs last sent to the keyboards, or `None` to send them again.
  leds: Option<Leds>,
  /// Whether the missing-devices warning was given.
  warned_no_devices: bool,
}

impl<S: Session, I: InputSource> Backend<S, I> {
  /// Sets up the frontend for the native backend on an enabled seat: the
  /// compositor draws its own cursor, libinput applies natural
  /// scrolling, the output is `output_size` pixels and the pointer starts
  /// in its middle. With no `renderer`, frames are reported without
  /// drawing.
  pub fn new(
    mut frontend: Frontend,
    session: Rc<RefCell<S>>,
    input: I,
    config: InputConfig,
    output_size: (u32, u32),
    renderer: Option<Box<dyn RenderTarget>>,
  ) -> Result<Self, shingle::Error> {
    let compositor = frontend.compositor_mut();
    compositor.set_default_cursor(true);
    compositor.set_device_natural_scroll(true);
    frontend.set_output_size(output_size.0, output_size.1)?;
    let output = frontend.compositor().output();
    let mut translator = Translator::default();
    translator.pointer = (
      f64::from(output.x) + f64::from(output.width) / 2.0,
      f64::from(output.y) + f64::from(output.height) / 2.0,
    );
    let refresh_mhz = frontend.compositor().output_info().refresh_mhz.max(1);
    Ok(Self {
      frontend,
      host: NativeHost::new(session),
      input,
      devices: DeviceRegistry::default(),
      translator,
      config,
      output_size,
      active: true,
      renderer,
      frame_interval: Duration::from_nanos(
        1_000_000_000_000 / u64::from(refresh_mhz),
      ),
      next_frame: Instant::now(),
      leds: None,
      warned_no_devices: false,
    })
  }

  /// The frontend, for queries.
  pub fn frontend(&self) -> &Frontend {
    &self.frontend
  }

  /// The frontend, for setup and tests.
  pub fn frontend_mut(&mut self) -> &mut Frontend {
    &mut self.frontend
  }

  /// Whether the seat is enabled.
  pub fn active(&self) -> bool {
    self.active
  }

  /// Runs one turn of the loop without waiting. Returns false once the
  /// frontend asked to exit.
  pub fn step(&mut self) -> bool {
    self.dispatch_session();
    self.dispatch_input();
    self.frontend.process(&mut self.host);
    self.apply_host_requests();
    self.update_leds();
    self.render_if_due(Instant::now());
    !self.host.exit
  }

  /// Runs until the frontend asks to exit (the quit binding, IPC, SIGINT
  /// or SIGTERM), then saves the session and closes every device.
  pub fn run(mut self) -> Result<(), Box<dyn Error>> {
    while self.step() {
      self.wait(self.timeout(Instant::now()))?;
    }
    self.finish();
    Ok(())
  }

  /// Saves the session, then closes libinput's devices and, as the last
  /// owner drops it, the seat.
  pub fn finish(mut self) {
    self.frontend.shutdown();
    diagnostics::log("native backend stopped");
    let Self {
      input, frontend, ..
    } = self;
    drop(input);
    drop(frontend);
  }

  /// How long the loop may sleep after a turn at `now`: until key repeat,
  /// the next frame if one is wanted, or [`TICK`].
  fn timeout(&self, now: Instant) -> Duration {
    let mut wake = now + TICK;
    if let Some(deadline) = self.frontend.next_deadline() {
      wake = wake.min(deadline);
    }
    if self.active && self.host.redraw {
      wake = wake.min(self.next_frame);
    }
    wake.saturating_duration_since(now)
  }

  /// Sleeps until the seat, libinput or the compositor has work, or for
  /// `timeout`.
  fn wait(&self, timeout: Duration) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let session = self.host.session.borrow();
    let mut fds: Vec<BorrowedFd<'_>> = vec![self.frontend.compositor().as_fd()];
    fds.extend(session.poll_fd());
    if self.active {
      fds.extend(self.input.poll_fd());
    }
    let mut poll_fds: Vec<PollFd<'_>> = fds
      .iter()
      .map(|fd| PollFd::new(fd, PollFlags::IN))
      .collect();
    let timeout = Timespec {
      tv_sec: timeout.as_secs() as i64,
      tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    match poll(&mut poll_fds, Some(&timeout)) {
      Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
      Err(error) => Err(error.into()),
    }
  }

  fn dispatch_session(&mut self) {
    let events = match self.host.session.borrow_mut().dispatch(None) {
      Ok(events) => events,
      Err(error) => {
        diagnostics::log(&format!("the seat failed: {error}"));
        self.host.exit = true;
        return;
      }
    };
    for event in events {
      match event {
        SeatEvent::Disable => self.pause(),
        SeatEvent::Enable => self.resume(),
      }
    }
  }

  /// Pauses for a disabled seat and acknowledges it.
  fn pause(&mut self) {
    if self.active {
      self.active = false;
      // The events of the suspend are removals and libinput's releases
      // of held keys; the devices stay registered, and the frontend
      // releases everything at once.
      let _ = self.input.suspend(self.output_size);
      self.devices.suspend();
      self.translator.reset();
      self.frontend.set_session_active(&mut self.host, false);
      diagnostics::log("seat disabled; paused");
    }
    if let Err(error) = self.host.session.borrow_mut().acknowledge_disable() {
      diagnostics::log(&format!("acknowledging the seat pause: {error}"));
    }
  }

  /// Resumes for an enabled seat.
  fn resume(&mut self) {
    if self.active {
      return;
    }
    match self.input.resume(self.output_size) {
      Ok(events) => self.handle_events(events, false),
      Err(error) => {
        diagnostics::log(&format!("resuming input failed: {error}"))
      }
    }
    for device in self
      .devices
      .remove_suspended(self.frontend.compositor_mut())
    {
      diagnostics::log(&format!("input device {} is gone", device.0));
    }
    self.active = true;
    self.frontend.set_session_active(&mut self.host, true);
    self.leds = None;
    self.next_frame = Instant::now();
    diagnostics::log("seat enabled; resumed");
  }

  fn dispatch_input(&mut self) {
    if !self.active {
      return;
    }
    match self.input.dispatch(self.output_size) {
      Ok(events) => self.handle_events(events, true),
      Err(error) => diagnostics::log(&format!("reading input failed: {error}")),
    }
    if self.devices.is_empty() && !self.warned_no_devices {
      self.warned_no_devices = true;
      diagnostics::log("no input devices yet; waiting for some");
    }
  }

  /// Handles libinput's events. Device input is passed on only if `live`;
  /// the events of a resume carry none that should count.
  fn handle_events(&mut self, events: Vec<NativeEvent>, live: bool) {
    for event in events {
      match event {
        NativeEvent::Added(device) => self.device_added(device),
        NativeEvent::Removed(device) => self.device_removed(&device),
        NativeEvent::Input {
          device,
          time_usec,
          input,
        } if live => self.device_input(&device, time_usec, input),
        NativeEvent::Input { .. } => {}
      }
    }
  }

  fn device_added(&mut self, device: crate::input::DeviceDescription) {
    let key = device.key.clone();
    let settings = device_settings(&self.config, &device);
    let enabled = enabled_mode(&self.config, &device);
    let name = device.name.clone();
    let keyboard = device.capabilities.keyboard;
    match self.devices.add(self.frontend.compositor_mut(), device) {
      Added::Ignored => return,
      Added::New(ids) => diagnostics::log(&format!(
        "input device {} ({name}) added as {}",
        key.0,
        id_list(&ids)
      )),
      Added::Returned(_) => {}
    }
    if !settings.is_empty() {
      self.input.configure(&key, &settings);
    }
    // A device that IPC disabled stays disabled when it comes back.
    let mode = if self.devices.wants_events(self.frontend.compositor(), &key) {
      enabled
    } else {
      SendEvents::Disabled
    };
    self.input.set_send_events(&key, mode);
    if keyboard {
      self.leds = None;
    }
  }

  fn device_removed(&mut self, device: &crate::input::DeviceKey) {
    // libinput releases held keys before it removes a device; if any are
    // left, release them while the keyboard still exists.
    if let Some(keyboard) =
      self.devices.seat_device(device, InputDeviceKind::Keyboard)
    {
      for code in self.devices.take_held_keys(device) {
        self.frontend.handle_input(
          &mut self.host,
          keyboard,
          InputEvent::Key {
            code,
            pressed: false,
          },
          None,
        );
      }
    }
    if let Some((ids, _)) =
      self.devices.remove(self.frontend.compositor_mut(), device)
    {
      self.translator.forget(device);
      diagnostics::log(&format!(
        "input device {} removed ({})",
        device.0,
        id_list(&ids)
      ));
    }
  }

  fn device_input(
    &mut self,
    device: &crate::input::DeviceKey,
    time_usec: u64,
    input: DeviceInput,
  ) {
    let Some(id) = self.devices.seat_device(device, input_kind(&input)) else {
      return;
    };
    if let DeviceInput::Key { code, pressed } = input
      && !self.devices.track_key(device, code, pressed)
    {
      return;
    }
    let compositor = self.frontend.compositor();
    let (width, height) = self.output_size;
    let map = |x, y| compositor.map_output_point(x, y, width, height);
    let output = OutputGeometry {
      bounds: compositor.output(),
      map: &map,
      constrained: compositor.pointer_constrained(),
    };
    let events = self.translator.translate(device, input, &output);
    let time = Some(Duration::from_micros(time_usec));
    for event in events {
      self.frontend.handle_input(&mut self.host, id, event, time);
    }
  }

  /// Carries out what the frontend asked: records a pointer warp and
  /// applies send-events modes for devices IPC enabled or disabled.
  fn apply_host_requests(&mut self) {
    if let Some(position) = self.host.warp.take() {
      self.translator.pointer = position;
    }
    while !self.host.device_changes.is_empty() {
      let changes = std::mem::take(&mut self.host.device_changes);
      for (id, enabled) in changes {
        self.device_changed(id, enabled);
      }
    }
  }

  /// Follows IPC enabling or disabling seat device `id`.
  ///
  /// The compositor has released what the device held for clients. A
  /// disabled keyboard's keys also leave the shared xkb state: they are
  /// released through the now disabled device, which updates the state
  /// without dispatching anything. libinput then stops sending the
  /// device's events once none of its seat devices is enabled, as
  /// Wayfire does with `LIBINPUT_CONFIG_SEND_EVENTS_DISABLED`; releases it
  /// sends on the way are dropped as already handled.
  fn device_changed(&mut self, id: InputDeviceId, enabled: bool) {
    let Some(device) = self.devices.owner(id).cloned() else {
      return;
    };
    let keyboard = self
      .frontend
      .compositor()
      .input_device(id)
      .is_some_and(|info| info.kind == InputDeviceKind::Keyboard);
    if !enabled && keyboard {
      for code in self.devices.take_held_keys(&device.key) {
        self.frontend.handle_input(
          &mut self.host,
          id,
          InputEvent::Key {
            code,
            pressed: false,
          },
          None,
        );
      }
    }
    let mode = if self
      .devices
      .wants_events(self.frontend.compositor(), &device.key)
    {
      enabled_mode(&self.config, &device)
    } else {
      SendEvents::Disabled
    };
    self.input.set_send_events(&device.key, mode);
  }

  /// Sends the keyboard LEDs when they change.
  fn update_leds(&mut self) {
    if !self.active || self.devices.keyboards().next().is_none() {
      return;
    }
    let leds = self.frontend.keyboard_leds();
    if self.leds != Some(leds) {
      self.input.set_leds(leds);
      self.leds = Some(leds);
    }
  }

  /// Draws a frame offscreen if one was requested and the refresh
  /// interval has passed since the last, while the seat is enabled.
  fn render_if_due(&mut self, now: Instant) {
    if !self.active || !self.host.redraw || now < self.next_frame {
      return;
    }
    self.host.redraw = false;
    self.next_frame = now + self.frame_interval;
    match self.renderer.as_mut() {
      Some(renderer) => {
        if let Err(error) =
          self.frontend.render(&mut self.host, renderer.as_mut())
        {
          diagnostics::log(&format!("render failed: {error}"));
        }
      }
      None => self.frontend.compositor_mut().frame(),
    }
  }
}

/// Seat device ids for a log line, such as `seat devices 4, 5`.
fn id_list(ids: &[InputDeviceId]) -> String {
  let ids: Vec<String> = ids.iter().map(|id| id.get().to_string()).collect();
  format!("seat devices {}", ids.join(", "))
}

#[cfg(test)]
mod tests;
