//! The native host: the event loop that runs [`Frontend`] on a seat.
//!
//! [`Backend`] owns the frontend, the [`Session`], the [`InputSource`] and
//! the [`DeviceRegistry`], and the [`Display`] with its [`FrameRenderer`].
//! Each [`Backend::step`] dispatches the seat, feeds libinput's events to
//! the frontend with their timestamps, reports completed page flips to the
//! compositor, runs the frontend's turn, carries out what the frontend
//! asked of its [`NativeHost`], follows DPMS and draws a frame of every
//! output that wants one and whose display is ready for it.
//! [`Backend::run`] repeats that, sleeping in `poll(2)` on the seat,
//! libinput, DRM and compositor descriptors in between, until the frontend
//! asks to exit, and then saves the session and closes every device.
//!
//! Before an output's frame, its scanout is moved to zero-copy if that was
//! not tried for its mode yet, or back to copy scanout if the renderer
//! could not draw into its GBM buffers ([`Display::upgrade_scanout`]).
//!
//! Frames are paced by page flips, per CRTC: a frame of an output is drawn
//! only when no flip of it is pending, and frame callbacks and
//! `wp_presentation` feedback of that output are sent with the flip's
//! timestamp when it completes
//! ([`shingle::Compositor::frame_presented`]). The outputs are drawn one
//! after another on the one GPU.
//!
//! A seat pause ([`SeatEvent::Disable`], for example a VT switch) follows
//! docs/design/native-backend.md, "Pause": stop committing and drop the
//! pending flips, suspend libinput, pause the frontend (which releases held
//! input) and acknowledge. A resume reopens the devices, which keep their
//! seat device ids, probes the card again and makes a fresh modeset,
//! resumes the frontend, resends the keyboard LEDs and draws again.
//!
//! The compositor's outputs follow the lit connectors
//! ([`Backend::sync_outputs`], docs/design/multi-output.md, MO-7): the
//! first monitor lit renames the compositor's default output, other lit
//! monitors are added with [`shingle::Compositor::add_output`], unplugged
//! ones removed under `output_layout.unplug`, and connected monitors that
//! are not lit (no free CRTC, configured or turned off) listed as disabled
//! heads. DRM hotplug ([`DeviceMonitor`]) and reloaded output modes are
//! followed once no flip is pending, and no new frame starts while they
//! wait. With nothing connected the compositor keeps the last output, dark,
//! with its windows, and nothing is drawn until a display is plugged in
//! again. wlr-output-management configurations
//! ([`shingle::Compositor::take_output_requests`]) pass one `TEST_ONLY`
//! modeset of every head first. DPMS and wlr-output-power-management are
//! per output. Absolute input devices are mapped to the output configured
//! for them, else to the whole layout.
//!
//! Rendering failures follow [`GpuHealth`]: a lost or hung GPU is replaced
//! with a new renderer from the [`RendererFactory`], and a third loss
//! within a minute, like a third page flip in a row that never completes,
//! ends the loop with [`Fatal`] after the session was saved.

use std::{
  cell::RefCell,
  collections::{BTreeSet, HashMap},
  error::Error,
  io,
  os::fd::{AsFd, BorrowedFd},
  rc::Rc,
  time::{Duration, Instant},
};

use shingle::{
  Compositor, HeadState, InputDeviceId, InputDeviceKind, OutputHandle,
  OutputKind, OutputRequest,
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
  /// The outputs a frame was requested of.
  redraw: BTreeSet<OutputHandle>,
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
      redraw: BTreeSet::new(),
      warp: None,
      exit: false,
      device_changes: Vec::new(),
    }
  }
}

impl<S: Session> Host for NativeHost<S> {
  /// Asks for a frame of `output`; outputs that are not lit draw none.
  fn request_redraw(&mut self, output: &OutputHandle) {
    self.redraw.insert(output.clone());
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
  /// The renderer, which draws every output.
  pub renderer: Box<dyn FrameRenderer>,
  /// Makes a new renderer.
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
  /// The first lit output's size in pixels, which libinput transforms
  /// absolute and touch positions to before they are mapped onto their
  /// output or the layout.
  output_size: (u32, u32),
  /// Whether the seat is enabled.
  active: bool,
  display: Display,
  /// The renderer, or none after creating one failed.
  renderer: Option<Box<dyn FrameRenderer>>,
  factory: RendererFactory,
  health: GpuHealth,
  /// Whether each lit output was last asked to be on.
  powered: HashMap<String, bool>,
  /// The size each lit output was last given, in physical pixels.
  sizes: HashMap<String, (u32, u32)>,
  /// Whether the compositor's default output was renamed to a monitor.
  described: bool,
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
  /// outputs (before any client could bind them) and draws its own
  /// cursor, libinput applies natural scrolling, each output has its
  /// mode's size (the default output [`OUTPUT_SIZE`] while none is lit)
  /// and the pointer starts in the middle of the first.
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
    compositor.set_default_cursor(true);
    compositor.set_device_natural_scroll(true);
    if display.size().is_none() {
      frontend.set_output_size(output_size.0, output_size.1)?;
    }
    if let Some(gpu) = rendering.renderer.gpu() {
      frontend.attach_gpu(gpu);
    }
    display.start()?;
    let mut backend = Self {
      frontend,
      host: NativeHost::new(session),
      input,
      devices: DeviceRegistry::default(),
      translator: Translator::default(),
      config,
      output_size,
      active: true,
      display,
      renderer: Some(rendering.renderer),
      factory: rendering.factory,
      health: GpuHealth::default(),
      powered: HashMap::new(),
      sizes: HashMap::new(),
      described: false,
      fatal: None,
      trace_flips: false,
      leds: None,
      warned_no_devices: false,
      monitor: None,
      output_requests: Vec::new(),
      hotplug_pending: false,
      policy_pending: None,
    };
    backend.sync_outputs();
    let output = backend.frontend.compositor().output();
    backend.translator.pointer = (
      f64::from(output.x) + f64::from(output.width) / 2.0,
      f64::from(output.y) + f64::from(output.height) / 2.0,
    );
    Ok(backend)
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
    let ready = self
      .host
      .redraw
      .iter()
      .any(|output| self.display.ready_on(output.name()));
    if ready && !self.modeset_waiting() {
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
    self.display.resume();
    self.active = true;
    self.sync_outputs();
    self.frontend.set_session_active(&mut self.host, true);
    self.leds = None;
    self.redraw_all();
    diagnostics::log("seat enabled; resumed");
  }

  /// Asks for a frame of every lit output.
  fn redraw_all(&mut self) {
    for name in self.display.outputs() {
      self.host.redraw.insert(OutputHandle::new(name));
    }
  }

  /// Brings the compositor's outputs in line with the lit connectors: the
  /// first monitor lit renames the default output, the others are added
  /// (or the dark last output lit again), lit outputs get their
  /// description and mode size, outputs no longer lit are removed under
  /// `output_layout.unplug` or, if their monitor is still connected,
  /// turned off and kept as disabled heads, and connected monitors that
  /// are not lit are listed as disabled heads. New outputs are added
  /// before gone ones are removed, so that windows have somewhere to go.
  pub fn sync_outputs(&mut self) {
    let lit = self.display.outputs();
    let unlit = self.display.unlit();
    if !self.described
      && let Some(info) = lit
        .first()
        .and_then(|first| self.display.output_info_of(first))
    {
      let compositor = self.frontend.compositor_mut();
      let primary = compositor.primary_output();
      compositor.describe_output(&primary, info);
      self.described = true;
    }
    for name in &lit {
      let (Some(info), Some(size)) = (
        self.display.output_info_of(name),
        self.display.size_of(name),
      ) else {
        continue;
      };
      let handle = OutputHandle::new(name.clone());
      let compositor = self.frontend.compositor_mut();
      match compositor.output_view(&handle) {
        Some(view) if !view.dark => {
          if view.info != info {
            compositor.describe_output(&handle, info);
          }
        }
        _ => {
          if let Err(error) = compositor.add_output(info, size) {
            diagnostics::log(&format!("adding output {name} failed: {error}"));
            continue;
          }
          self.sizes.insert(name.clone(), size);
        }
      }
      if self.sizes.get(name) != Some(&size) {
        if let Err(error) =
          self.frontend.output_resized_on(&handle, size.0, size.1)
        {
          diagnostics::log(&format!("resizing {name} failed: {error}"));
        }
        self.sizes.insert(name.clone(), size);
      }
      self.host.redraw.insert(handle);
    }
    let connected =
      |name: &str| unlit.iter().any(|(info, _)| info.name == name);
    let is_lit = |name: &str| lit.iter().any(|lit| lit == name);
    let gone: Vec<OutputHandle> = self
      .frontend
      .compositor()
      .outputs()
      .into_iter()
      .filter(|view| view.kind == OutputKind::Backend && !view.dark)
      .map(|view| view.handle)
      .filter(|handle| !is_lit(handle.name()))
      .collect();
    // The default output stays until a monitor is lit.
    if self.described {
      for handle in gone {
        let compositor = self.frontend.compositor_mut();
        let result = if connected(handle.name()) {
          compositor.disable_output(&handle)
        } else {
          compositor.remove_output(&handle)
        };
        if let Err(error) = result {
          let name = handle.name();
          diagnostics::log(&format!("removing {name} failed: {error}"));
        }
        self.forget_output(&handle);
      }
    }
    let compositor = self.frontend.compositor_mut();
    for handle in compositor.disabled_outputs() {
      if !connected(handle.name()) && !is_lit(handle.name()) {
        let _ = compositor.remove_output(&handle);
      }
    }
    for (info, size) in unlit.clone() {
      if let Err(error) = compositor.add_disabled_output(info, size) {
        diagnostics::log(&format!("listing an unlit output failed: {error}"));
      }
    }
    self.host.redraw.retain(|output| is_lit(output.name()));
    if let Some(size) = self.display.size() {
      self.output_size = size;
    }
  }

  /// Forgets what the backend kept for an output that is no longer lit.
  fn forget_output(&mut self, output: &OutputHandle) {
    self.sizes.remove(output.name());
    self.powered.remove(output.name());
    self.host.redraw.remove(output);
    if let Some(renderer) = self.renderer.as_mut() {
      renderer.forget(output);
    }
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
  /// pending on any output, since lighting outputs takes a full modeset:
  /// the display probes the card again, and the compositor's outputs
  /// follow. A flip that never completes times out within a second.
  fn apply_output_changes(&mut self) {
    if !self.active || self.display.flip_pending() {
      return;
    }
    let mut changed = false;
    if let Some(policy) = self.policy_pending.take() {
      self.display.set_policy(policy);
      changed = true;
    }
    if std::mem::take(&mut self.hotplug_pending) {
      self.display.hotplug();
      changed = true;
    }
    if changed {
      self.sync_outputs();
    }
  }

  /// Whether a full modeset waits for the pending flips: no new frame
  /// starts meanwhile, so that it does not wait long.
  fn modeset_waiting(&self) -> bool {
    self.hotplug_pending
      || self.policy_pending.is_some()
      || self
        .output_requests
        .iter()
        .any(|request| !request.test_only)
  }

  /// Tests or carries out the configurations wlr-output-management
  /// clients asked for, each with one `TEST_ONLY` modeset of every head
  /// first, and answers them. A configuration that changes something
  /// waits while a page flip is pending, because no full modeset may be
  /// committed then; tests do not.
  fn handle_output_requests(&mut self) {
    let requests = self.frontend.compositor_mut().take_output_requests();
    self.output_requests.extend(requests);
    while let Some(request) = self.output_requests.first().cloned() {
      if !request.test_only && self.display.flip_pending() {
        return;
      }
      self.output_requests.remove(0);
      let heads: Vec<(String, HeadState)> = request
        .heads
        .iter()
        .map(|head| (head.output.name().to_owned(), head.state))
        .collect();
      let result = if request.test_only {
        self.display.test_configuration(&heads)
      } else {
        let result = self.display.apply_configuration(&heads).map(|_| ());
        self.sync_outputs();
        result
      };
      if let Err(error) = &result {
        diagnostics::log(&format!("output configuration {heads:?}: {error}"));
      }
      self
        .frontend
        .compositor_mut()
        .finish_output_request(request.id, result.is_ok());
    }
  }

  /// Reports completed page flips to the compositor, which sends frame
  /// callbacks and presentation feedback with their times, and recovers
  /// from a flip that never completes.
  fn dispatch_display(&mut self, now: Instant) {
    for (output, info) in self.display.dispatch_flips() {
      if self.trace_flips {
        diagnostics::log(&format!("{output}: {}", flip_line(&info)));
      }
      let output = OutputHandle::new(output);
      self
        .frontend
        .compositor_mut()
        .frame_presented(&output, info);
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

  /// Turns each output off or on when the compositor's power state of it
  /// changes (wlr-output-power-management, idle timeouts, activity).
  fn update_dpms(&mut self) {
    for name in self.display.outputs() {
      let handle = OutputHandle::new(name.clone());
      let on = self.frontend.compositor().output_powered(&handle);
      let before = self.powered.insert(name.clone(), on);
      if before == Some(on) {
        continue;
      }
      if before.is_some() {
        let state = if on { "on" } else { "off" };
        diagnostics::log(&format!("output {name} {state}"));
      }
      self.display.set_output_dpms(&name, on);
      if on {
        self.host.redraw.insert(handle);
      }
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
    let mapped = self
      .devices
      .owner(id)
      .and_then(|owner| compositor.input_device_output(&owner.name));
    let size = self.output_size;
    let map = |x, y| absolute_position(compositor, mapped.as_ref(), size, x, y);
    let clamp = |x, y| compositor.clamp_to_layout(x, y);
    let output = OutputGeometry {
      clamp: &clamp,
      map: &map,
      output: mapped.as_ref().and_then(|output| {
        let rect = compositor.output_view(output)?.rect;
        Some((output.clone(), (f64::from(rect.x), f64::from(rect.y))))
      }),
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

  /// Draws and flips a frame of each output that wants one and is ready
  /// for it: the seat is enabled, the output is lit and on and no flip of
  /// it is pending, and no full modeset waits for the flips to end.
  fn render_if_wanted(&mut self, now: Instant) {
    if !self.active || self.modeset_waiting() {
      return;
    }
    for name in self.display.outputs() {
      let output = OutputHandle::new(name.clone());
      if !self.host.redraw.contains(&output) {
        continue;
      }
      // Zero-copy is tried, or given up, before the output's next frame.
      if let Some(renderer) = self.renderer.as_mut() {
        self.display.upgrade_scanout(&name, renderer.as_mut());
      }
      if !self.display.begin_frame_on(&name) {
        continue;
      }
      self.host.redraw.remove(&output);
      let size = self.display.size_of(&name).unwrap_or(self.output_size);
      let target = self.display.draw_target(&name);
      let result = match self.renderer.as_mut() {
        Some(renderer) => renderer.draw(
          &mut self.frontend,
          &mut self.host,
          &output,
          size,
          target,
        ),
        None => Err(RenderFailure::Lost("no renderer".into())),
      };
      match result {
        Ok(drawn) => {
          self.health.success();
          self.display.present_on(&name, &drawn, now);
        }
        Err(failure) => {
          self.display.abandon_frame_on(&name);
          self.render_failed(&failure, &output, now);
          if self.renderer.is_none() || self.host.exit {
            break;
          }
        }
      }
    }
  }

  /// Decides what a failed frame means and acts on it.
  fn render_failed(
    &mut self,
    failure: &RenderFailure,
    output: &OutputHandle,
    now: Instant,
  ) {
    diagnostics::log(&format!("{}: {failure}", output.name()));
    if let RenderFailure::Scanout(reason) = failure {
      self.display.scanout_failed(output.name(), reason);
      self.host.redraw.insert(output.clone());
    }
    match self.health.failure(failure, now) {
      Verdict::Skip => {
        // A slow frame is drawn again; another error waits for the next
        // change.
        if *failure == RenderFailure::Timeout {
          self.host.redraw.insert(output.clone());
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
    self.redraw_all();
  }
}

/// Where an absolute position `(x, y)` of a device, which libinput
/// transformed to `size` pixels, lies in the layout: on `mapped`, the
/// output configured for the device, undoing its scale and transform; else
/// on the only output, the same way; else on the bounding box of the
/// outputs, as wlroots maps unmapped absolute devices onto the whole
/// layout. IPC outputs, which nobody sees, do not count.
fn absolute_position(
  compositor: &Compositor,
  mapped: Option<&OutputHandle>,
  size: (u32, u32),
  x: f64,
  y: f64,
) -> (f64, f64) {
  let views: Vec<_> = compositor
    .outputs()
    .into_iter()
    .filter(|view| view.kind == OutputKind::Backend && !view.dark)
    .collect();
  let target = mapped
    .cloned()
    .or_else(|| (views.len() == 1).then(|| views[0].handle.clone()))
    .and_then(|output| compositor.output_view(&output));
  if let Some(view) = target {
    let (local_x, local_y) = compositor
      .map_point_on(&view.handle, x, y, size.0, size.1)
      .unwrap_or((x, y));
    return (
      f64::from(view.rect.x) + local_x,
      f64::from(view.rect.y) + local_y,
    );
  }
  if views.is_empty() {
    return compositor.map_output_point(x, y, size.0, size.1);
  }
  let left = views.iter().map(|view| view.rect.x).min().unwrap_or(0);
  let top = views.iter().map(|view| view.rect.y).min().unwrap_or(0);
  let right = views
    .iter()
    .map(|view| i64::from(view.rect.x) + i64::from(view.rect.width))
    .max()
    .unwrap_or(0);
  let bottom = views
    .iter()
    .map(|view| i64::from(view.rect.y) + i64::from(view.rect.height))
    .max()
    .unwrap_or(0);
  let (width, height) = (f64::from(size.0.max(1)), f64::from(size.1.max(1)));
  (
    f64::from(left) + x / width * (right - i64::from(left)) as f64,
    f64::from(top) + y / height * (bottom - i64::from(top)) as f64,
  )
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
