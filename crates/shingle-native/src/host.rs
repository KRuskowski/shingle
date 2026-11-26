//! The native host: the event loop that runs [`Frontend`] on a seat.
//!
//! [`Backend`] owns the frontend, the [`Session`], the [`InputSource`] and
//! the [`DeviceRegistry`], and the [`Display`] with its [`FrameRenderer`].
//! Each [`Backend::step`] dispatches the seat, feeds libinput's events to
//! the frontend with their timestamps, reports completed page flips to the
//! compositor, runs the frontend's turn, carries out what the frontend
//! asked of its [`NativeHost`], follows DPMS and draws a frame if one is
//! wanted and the display is ready for it. [`Backend::run`] repeats that,
//! sleeping in `poll(2)` on the seat, libinput, DRM and compositor
//! descriptors in between, until the frontend asks to exit, and then saves
//! the session and closes every device.
//!
//! Frames are paced by page flips: a frame is drawn only when no flip is
//! pending, and frame callbacks and `wp_presentation` feedback are sent
//! with the flip's timestamp when it completes
//! ([`shingle::Compositor::frame_presented`]).
//!
//! A seat pause ([`SeatEvent::Disable`], for example a VT switch) follows
//! docs/design/native-backend.md, "Pause": stop committing and drop the
//! pending flip, suspend libinput, pause the frontend (which releases held
//! input) and acknowledge. A resume reopens the devices, which keep their
//! seat device ids, probes the card again and makes a fresh modeset,
//! resumes the frontend, resends the keyboard LEDs and draws again.
//!
//! Outputs follow DRM hotplug ([`DeviceMonitor`]): the display probes the
//! card again, and when another output is lit, or none, the compositor is
//! told ([`shingle::Compositor::set_output_info`], which renames the core
//! output and replaces the `wl_output` global) and the frontend, renderer
//! and input mapping get the new size. With nothing connected the
//! compositor keeps the last output and its windows, and nothing is drawn
//! until a display is plugged in again. wlr-output-management mode
//! changes ([`shingle::Compositor::take_output_requests`]) pass a
//! `TEST_ONLY` modeset first, and a config reload applies the output
//! modes it configures.
//!
//! Rendering failures follow [`GpuHealth`]: a lost or hung GPU is replaced
//! with a new renderer from the [`RendererFactory`], and a third loss
//! within a minute, like a third page flip in a row that never completes,
//! ends the loop with [`Fatal`] after the session was saved.

use std::{
  cell::RefCell,
  error::Error,
  io,
  os::fd::{AsFd, BorrowedFd},
  rc::Rc,
  time::{Duration, Instant},
};

use shingle::{
  DpmsState, HeadRequest, HeadState, InputDeviceId, InputDeviceKind,
  OutputHandle, OutputRequest,
};
use shingle_config::InputConfig;
use shingle_frontend::{
  Frontend,
  host::{Host, InputEvent},
  keyboard::Leds,
};

use crate::{
  OUTPUT_SIZE, diagnostics,
  display::{Display, Fatal},
  gpu::{FrameRenderer, GpuHealth, RenderFailure, RendererFactory, Verdict},
  hotplug::{DeviceMonitor, DrmEvent},
  input::{
    DeviceInput, InputSource, NativeEvent, OutputGeometry, SendEvents,
    Translator, device_settings, enabled_mode,
  },
  outputs::OutputPolicy,
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
  /// The frontend warped the pointer here, relative to this output.
  warp: Option<(OutputHandle, f64, f64)>,
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
  /// Native lights one output, the compositor's first, until MO-7; the
  /// frontend asks only for outputs a backend added, which is that one.
  fn request_redraw(&mut self, _output: &OutputHandle) {
    self.redraw = true;
  }

  fn warp_pointer(&mut self, output: &OutputHandle, x: f64, y: f64) {
    self.warp = Some((output.clone(), x, y));
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

/// What the backend draws with: a renderer, and how to make a new one
/// after the GPU was lost.
pub struct Rendering {
  /// The renderer for the output's current size.
  pub renderer: Box<dyn FrameRenderer>,
  /// Makes a new renderer of a size.
  pub factory: RendererFactory,
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
  display: Display,
  /// The renderer, or none after creating one failed.
  renderer: Option<Box<dyn FrameRenderer>>,
  factory: RendererFactory,
  health: GpuHealth,
  /// The DPMS state the display was last asked for.
  dpms: DpmsState,
  /// Why the loop ended, if it had to.
  fatal: Option<Fatal>,
  /// Whether every presented page flip is logged.
  trace_flips: bool,
  /// The LEDs last sent to the keyboards, or `None` to send them again.
  leds: Option<Leds>,
  /// Whether the missing-devices warning was given.
  warned_no_devices: bool,
  /// Where DRM hotplug events come from, if anywhere.
  monitor: Option<Box<dyn DeviceMonitor>>,
  /// Mode changes waiting for no flip to be pending.
  output_requests: Vec<OutputRequest>,
  /// A hotplug waiting for no flip to be pending.
  hotplug_pending: bool,
  /// Reloaded output modes waiting for no flip to be pending.
  policy_pending: Option<OutputPolicy>,
}

impl<S: Session, I: InputSource> Backend<S, I> {
  /// Sets up the frontend for the native backend on an enabled seat and
  /// makes the display's first modeset: the compositor describes the lit
  /// output (before any client could bind it) and draws its own cursor,
  /// libinput applies natural scrolling, the output has the mode's size
  /// ([`OUTPUT_SIZE`] while none is lit) and the pointer starts in its
  /// middle.
  pub fn new(
    mut frontend: Frontend,
    session: Rc<RefCell<S>>,
    input: I,
    config: InputConfig,
    mut display: Display,
    rendering: Rendering,
  ) -> Result<Self, Box<dyn Error>> {
    let output_size = display.size().unwrap_or(OUTPUT_SIZE);
    let compositor = frontend.compositor_mut();
    if let Some(info) = display.output_info() {
      compositor.set_output_info(info);
    }
    compositor.set_default_cursor(true);
    compositor.set_device_natural_scroll(true);
    frontend.set_output_size(output_size.0, output_size.1)?;
    if let Some(gpu) = rendering.renderer.gpu() {
      frontend.attach_gpu(gpu);
    }
    display.start()?;
    let output = frontend.compositor().output();
    let mut translator = Translator::default();
    translator.pointer = (
      f64::from(output.x) + f64::from(output.width) / 2.0,
      f64::from(output.y) + f64::from(output.height) / 2.0,
    );
    let dpms = frontend.compositor().dpms_state();
    Ok(Self {
      frontend,
      host: NativeHost::new(session),
      input,
      devices: DeviceRegistry::default(),
      translator,
      config,
      output_size,
      active: true,
      display,
      renderer: Some(rendering.renderer),
      factory: rendering.factory,
      health: GpuHealth::default(),
      dpms,
      fatal: None,
      trace_flips: false,
      leds: None,
      warned_no_devices: false,
      monitor: None,
      output_requests: Vec::new(),
      hotplug_pending: false,
      policy_pending: None,
    })
  }

  /// Follows DRM hotplug events from `monitor`.
  pub fn set_device_monitor(&mut self, monitor: Box<dyn DeviceMonitor>) {
    self.monitor = Some(monitor);
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

  /// The display, for queries.
  pub fn display(&self) -> &Display {
    &self.display
  }

  /// Logs every presented page flip with its sequence and time, as
  /// `SHINGLE_TRACE_FLIPS=1` asks, for comparing with what clients get
  /// (tools/native_test/present_check.py).
  pub fn set_trace_flips(&mut self, trace: bool) {
    self.trace_flips = trace;
  }

  /// Runs one turn of the loop without waiting. Returns false once the
  /// frontend asked to exit.
  pub fn step(&mut self) -> bool {
    self.step_at(Instant::now())
  }

  /// [`Backend::step`] at `now`, the time the page-flip timeout, frame
  /// pacing and GPU health are judged by. Tests pass a fixed time, so
  /// that a slow machine cannot make a pending flip time out between two
  /// of their steps.
  pub(crate) fn step_at(&mut self, now: Instant) -> bool {
    self.dispatch_session();
    self.dispatch_input();
    self.dispatch_display(now);
    self.dispatch_monitor();
    self.frontend.process(&mut self.host);
    self.apply_host_requests();
    self.apply_reloaded_config();
    self.apply_output_changes();
    self.handle_output_requests();
    self.update_dpms();
    self.update_leds();
    self.render_if_wanted(now);
    !self.host.exit
  }

  /// Runs until the frontend asks to exit (the quit binding, IPC, SIGINT
  /// or SIGTERM), then saves the session and closes every device. A
  /// [`Fatal`] failure also ends it, after the session was saved, and is
  /// returned.
  pub fn run(mut self) -> Result<(), Box<dyn Error>> {
    while self.step() {
      self.wait(self.timeout(Instant::now()))?;
    }
    let fatal = self.fatal.take();
    self.finish();
    match fatal {
      Some(fatal) => Err(Box::new(fatal)),
      None => Ok(()),
    }
  }

  /// Why the loop ended, if a failure ended it.
  pub fn fatal(&self) -> Option<&Fatal> {
    self.fatal.as_ref()
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
  /// the page-flip timeout, at once if a frame is wanted and the display
  /// is ready, or [`TICK`]. Completed flips wake the loop through the DRM
  /// descriptor.
  fn timeout(&self, now: Instant) -> Duration {
    let mut wake = now + TICK;
    if let Some(deadline) = self.frontend.next_deadline() {
      wake = wake.min(deadline);
    }
    if let Some(deadline) = self.display.deadline() {
      wake = wake.min(deadline);
    }
    if self.host.redraw && self.display.ready() {
      wake = now;
    }
    wake.saturating_duration_since(now)
  }

  /// Sleeps until the seat, libinput, the card or the compositor has
  /// work, or for `timeout`.
  fn wait(&self, timeout: Duration) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let session = self.host.session.borrow();
    let mut fds: Vec<BorrowedFd<'_>> = vec![self.frontend.compositor().as_fd()];
    fds.extend(session.poll_fd());
    if self.active {
      fds.extend(self.input.poll_fd());
      fds.extend(self.display.poll_fd());
    }
    // Read even while paused, so that events do not pile up.
    fds.extend(self.monitor.as_ref().and_then(|monitor| monitor.poll_fd()));
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
      self.display.pause();
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
    let resumed = self.display.resume();
    if resumed.output_changed || resumed.resized.is_some() {
      self.describe_output();
    }
    self.active = true;
    self.frontend.set_session_active(&mut self.host, true);
    self.leds = None;
    self.host.redraw = true;
    diagnostics::log("seat enabled; resumed");
  }

  /// Tells the compositor about the lit output, if one is lit, and gives
  /// the frontend, renderer and input mapping its size. With none lit, the
  /// compositor keeps the last output, so windows stay where they are.
  fn describe_output(&mut self) {
    if let Some(info) = self.display.output_info() {
      self.frontend.compositor_mut().set_output_info(info);
    }
    if let Some(size) = self.display.size()
      && size != self.output_size
    {
      self.resize(size);
    }
    self.host.redraw = true;
  }

  /// Reads DRM hotplug events. While the seat is disabled they are
  /// dropped, since the resume probes anyway; otherwise the next
  /// [`Backend::apply_output_changes`] probes the card again.
  fn dispatch_monitor(&mut self) {
    let Some(monitor) = self.monitor.as_mut() else {
      return;
    };
    let events = monitor.dispatch();
    if events.contains(&DrmEvent::Removed) {
      diagnostics::log("the DRM card was removed; nothing can be shown");
    }
    if self.active
      && events
        .iter()
        .any(|event| matches!(event, DrmEvent::Hotplug(_)))
    {
      self.hotplug_pending = true;
    }
  }

  /// Keeps the output modes of a reloaded configuration for
  /// [`Backend::apply_output_changes`].
  fn apply_reloaded_config(&mut self) {
    if let Some(config) = self.frontend.take_reloaded_config() {
      self.policy_pending = Some(OutputPolicy::from_config(&config));
    }
  }

  /// Follows a hotplug and reloaded output modes once no page flip is
  /// pending, since lighting an output takes a modeset: the display
  /// probes the card again, and the compositor learns about a different
  /// output. A flip that never completes times out within a second.
  fn apply_output_changes(&mut self) {
    if !self.active || self.display.flip_pending() {
      return;
    }
    let mut changed = false;
    if let Some(policy) = self.policy_pending.take() {
      let change = self.display.set_policy(policy);
      changed |= change.output_changed || change.resized.is_some();
    }
    if std::mem::take(&mut self.hotplug_pending) {
      let change = self.display.hotplug();
      changed |= change.output_changed || change.resized.is_some();
    }
    if changed {
      self.describe_output();
    }
  }

  /// Tests or makes the mode changes wlr-output-management clients asked
  /// for, each with a `TEST_ONLY` modeset first, and answers them. A
  /// change waits while a page flip is pending, because no modeset may be
  /// committed then; tests do not.
  fn handle_output_requests(&mut self) {
    let requests = self.frontend.compositor_mut().take_output_requests();
    self.output_requests.extend(requests);
    while let Some(request) = self.output_requests.first().cloned() {
      if !request.test_only && self.display.flip_pending() {
        return;
      }
      self.output_requests.remove(0);
      let mode = match request.heads.as_slice() {
        [
          HeadRequest {
            state: HeadState::On(Some(mode)),
            ..
          },
        ] => Ok(*mode),
        _ => Err("only one output is lit".to_owned()),
      };
      let result = mode.and_then(|mode| {
        if request.test_only {
          self.display.test_mode(mode)
        } else {
          self.display.set_mode(mode).map(|_| {
            self.describe_output();
          })
        }
      });
      if let Err(error) = &result {
        diagnostics::log(&format!("output mode {:?}: {error}", request.heads));
      }
      self
        .frontend
        .compositor_mut()
        .finish_output_request(request.id, result.is_ok());
    }
  }

  /// Follows a new mode: the frontend's output, the renderer and the
  /// input mapping get the new size.
  fn resize(&mut self, size: (u32, u32)) {
    self.output_size = size;
    if let Err(error) = self.frontend.output_resized(size.0, size.1) {
      diagnostics::log(&format!("resizing the output failed: {error}"));
    }
    if let Some(renderer) = self.renderer.as_mut()
      && let Err(error) = renderer.resize(size)
    {
      diagnostics::log(&format!("resizing the renderer failed: {error}"));
      self.renderer = None;
    }
  }

  /// Reports completed page flips to the compositor, which sends frame
  /// callbacks and presentation feedback with their times, and recovers
  /// from a flip that never completes.
  fn dispatch_display(&mut self, now: Instant) {
    for info in self.display.dispatch() {
      if self.trace_flips {
        diagnostics::log(&flip_line(&info));
      }
      let compositor = self.frontend.compositor_mut();
      // Native lights one output, the compositor's first, until MO-7.
      let output = compositor.primary_output();
      compositor.frame_presented(&output, info);
    }
    if let Err(fatal) = self.display.check_timeout(now) {
      self.fail(fatal);
    }
  }

  /// Ends the loop because of `fatal`.
  fn fail(&mut self, fatal: Fatal) {
    diagnostics::log(&format!("{fatal}; saving the session and exiting"));
    self.fatal = Some(fatal);
    self.host.exit = true;
  }

  /// Turns the display off or on when the compositor's DPMS state
  /// changes (wlr-output-power-management, idle timeouts, activity).
  fn update_dpms(&mut self) {
    let state = self.frontend.compositor().dpms_state();
    if state == self.dpms {
      return;
    }
    self.dpms = state;
    let on = state == DpmsState::On;
    diagnostics::log(if on { "display on" } else { "display off" });
    self.display.set_dpms(on);
    if on {
      self.host.redraw = true;
    }
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
    if let Some((output, x, y)) = self.host.warp.take() {
      // The translator works in layout coordinates.
      let origin = self
        .frontend
        .compositor()
        .output_view(&output)
        .map_or((0, 0), |view| (view.rect.x, view.rect.y));
      self.translator.pointer =
        (x + f64::from(origin.0), y + f64::from(origin.1));
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

  /// Draws and flips a frame if one was requested and the display is
  /// ready for it: the seat is enabled, an output is lit, the display is
  /// on and no flip is pending.
  fn render_if_wanted(&mut self, now: Instant) {
    if !self.active || !self.host.redraw || !self.display.begin_frame() {
      return;
    }
    self.host.redraw = false;
    let result = match self.renderer.as_mut() {
      Some(renderer) => renderer.draw(&mut self.frontend, &mut self.host),
      None => Err(RenderFailure::Lost("no renderer".into())),
    };
    match result {
      Ok(frame) => {
        self.health.success();
        self.display.present(&frame, now);
      }
      Err(failure) => {
        self.display.abandon_frame();
        self.render_failed(&failure, now);
      }
    }
  }

  /// Decides what a failed frame means and acts on it.
  fn render_failed(&mut self, failure: &RenderFailure, now: Instant) {
    diagnostics::log(&failure.to_string());
    match self.health.failure(failure, now) {
      Verdict::Skip => {
        // A slow frame is drawn again; another error waits for the next
        // change.
        if *failure == RenderFailure::Timeout {
          self.host.redraw = true;
        }
      }
      Verdict::Recreate => self.recreate_renderer(),
      Verdict::GiveUp => self.fail(Fatal(format!(
        "the GPU failed {} times within {} s",
        crate::gpu::MAX_GPU_LOSSES,
        crate::gpu::GPU_LOSS_WINDOW.as_secs()
      ))),
    }
  }

  /// Drops the GPU context and every texture made with it, and creates a
  /// new renderer. The frontend uploads again what the compositor's
  /// surface snapshots still hold ([`Frontend::drop_gpu_resources`]).
  fn recreate_renderer(&mut self) {
    diagnostics::log("recreating the GPU context");
    self.renderer = None;
    self.frontend.drop_gpu_resources();
    match (self.factory)(self.output_size) {
      Ok(renderer) => {
        // The same feedback if the render node is the same; the importer,
        // cache and shm uploader are the new device's.
        if let Some(gpu) = renderer.gpu() {
          self.frontend.attach_gpu(gpu);
        }
        self.renderer = Some(renderer);
      }
      Err(error) => {
        diagnostics::log(&format!("creating a GPU context failed: {error}"))
      }
    }
    self.host.redraw = true;
  }
}

/// The log line for a presented flip: `page flip seq=S at SEC.NSEC s`.
fn flip_line(info: &shingle::PresentInfo) -> String {
  format!(
    "page flip seq={} at {}.{:09} s",
    info.seq,
    info.time.as_secs(),
    info.time.subsec_nanos()
  )
}

/// Seat device ids for a log line, such as `seat devices 4, 5`.
fn id_list(ids: &[InputDeviceId]) -> String {
  let ids: Vec<String> = ids.iter().map(|id| id.get().to_string()).collect();
  format!("seat devices {}", ids.join(", "))
}

#[cfg(test)]
pub(crate) mod tests;
