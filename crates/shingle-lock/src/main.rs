//! `shingle-lock`: Shingle's lock screen, an ext-session-lock-v1 client
//! that takes swaylock's and swaylock-effects' options and unlocks with
//! PAM through `shingle-auth` (docs/design/lock-screen.md).
//!
//! With `screenshot` it captures every output before it asks for the
//! lock, blurs the captures once on the GPU and shows them, slowly
//! drifting, under a vignette and the clock; typing fades in an outlined
//! password input. Frames follow the compositor's frame callbacks, so
//! nothing is drawn while an output is off.

mod capture;
mod clock;
mod daemon;
mod font;
mod gpu;
mod options;
mod shm;
mod ui;
mod username;

use std::{
  collections::HashMap,
  io,
  panic::{AssertUnwindSafe, catch_unwind},
  process::ExitCode,
  thread,
  time::{Duration, Instant, SystemTime},
};

use ab_glyph::FontArc;
use calloop::{
  EventLoop,
  channel::{self, Event as ChannelEvent, Sender},
  signals::{Signal, Signals},
};
use calloop_wayland_source::WaylandSource;
use shingle_auth::{Authenticator, Password, ZeroOnFree};
use shm::{BufferData, ShmBuffer};
use thiserror::Error;
use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, WEnum,
  globals::{
    BindError, GlobalError, GlobalList, GlobalListContents, registry_queue_init,
  },
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer,
    wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
  },
};
use wayland_protocols::ext::session_lock::v1::client::{
  ext_session_lock_manager_v1::ExtSessionLockManagerV1,
  ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
  ext_session_lock_v1::{self, ExtSessionLockV1},
};
use xkbcommon::xkb;
use zeroize::{Zeroize, Zeroizing};

use crate::{
  gpu::{Drift, Frame, Gpu, Image, Target},
  options::{Action, Settings},
  ui::{Content, Highlight, Indicator, Layout},
};

// Zeroes every freed heap block, including the password copy that nonstick
// drops after handing it to PAM and old buffers of a growing password.
#[global_allocator]
static ALLOCATOR: ZeroOnFree<std::alloc::System> =
  ZeroOnFree::new(std::alloc::System);

const MAX_PASSWORD_BYTES: usize = 4096;
/// How long the screenshots may take before the lock goes ahead without
/// the missing ones.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);
/// The frame interval asked for while the input fades.
const FADE_FRAME: Duration = Duration::from_millis(16);

#[derive(Debug, Error)]
enum LockError {
  #[error("failed to connect to WAYLAND_DISPLAY: {0}")]
  Connect(#[from] wayland_client::ConnectError),
  #[error("failed to initialize the Wayland registry: {0}")]
  Globals(#[from] GlobalError),
  #[error("event loop failed: {0}")]
  EventLoop(#[from] calloop::Error),
  #[error("local system operation failed: {0}")]
  Io(#[from] io::Error),
  #[error("{0}")]
  Message(String),
}

enum AuthOutcome {
  Authenticated,
  Denied,
  WorkerFailed(String),
}

struct Seat {
  proxy: wl_seat::WlSeat,
  keyboard: Option<Keyboard>,
  pointer: Option<wl_pointer::WlPointer>,
  touch: Option<wl_touch::WlTouch>,
}

struct Keyboard {
  proxy: wl_keyboard::WlKeyboard,
  xkb: Option<xkb::State>,
}

/// What an output's input image shows, to draw it again only on a
/// change.
#[derive(Clone, Debug, PartialEq)]
struct InputLook {
  state: shingle_config::InputState,
  content: Content,
  highlight: Option<Highlight>,
}

struct LockOutput {
  global: u32,
  output: wl_output::WlOutput,
  surface: wl_surface::WlSurface,
  lock_surface: ExtSessionLockSurfaceV1,
  size: Option<(u32, u32)>,
  target: Option<Target>,
  layout: Option<Layout>,
  buffers: Vec<ShmBuffer>,
  next_buffer_id: u64,
  /// A full frame was committed since the last configure.
  committed: bool,
  /// A frame callback is outstanding: no frame until it is done, so an
  /// output the compositor does not draw, such as one that is off, gets
  /// none.
  frame_pending: bool,
  /// What it shows changed since its last frame.
  dirty: bool,
  last_frame: Option<Instant>,
  clock: Option<(String, String)>,
  /// The clock image's size.
  clock_size: (u32, u32),
  input: Option<InputLook>,
  frames: u64,
}

impl LockOutput {
  fn destroy(self) {
    self.lock_surface.destroy();
    self.surface.destroy();
    if self.output.version() >= 3 {
      self.output.release();
    }
  }
}

struct State {
  connection: Connection,
  qh: QueueHandle<State>,
  globals: GlobalList,
  _manager: ExtSessionLockManagerV1,
  lock: ExtSessionLockV1,
  compositor: wl_compositor::WlCompositor,
  shm: wl_shm::WlShm,
  outputs: HashMap<u32, LockOutput>,
  seats: HashMap<u32, Seat>,
  xkb_context: xkb::Context,
  settings: Settings,
  gpu: Gpu,
  images: HashMap<u32, Image>,
  font: FontArc,
  indicator: Indicator,
  started: Instant,
  /// When the process started, for the timings it reports.
  launched: Instant,
  locked_at: Option<Instant>,
  notifier: daemon::Notifier,
  trace: bool,
  authenticator: Authenticator,
  username: String,
  auth_sender: Sender<AuthOutcome>,
  auth_running: bool,
  password: Zeroizing<String>,
  locked: bool,
  finished: bool,
  unlock_requested: bool,
  ready_reported: bool,
  failure: Option<String>,
}

impl State {
  fn fail(&mut self, message: impl Into<String>) {
    self.password.zeroize();
    if self.failure.is_none() {
      self.failure = Some(message.into());
    }
  }

  fn active(&self) -> bool {
    self.failure.is_none() && !self.finished && !self.unlock_requested
  }

  fn add_output(&mut self, name: u32, version: u32) {
    if !self.active() || self.outputs.contains_key(&name) {
      return;
    }
    let qh = self.qh.clone();
    let output = self.globals.registry().bind(name, version.min(4), &qh, ());
    let surface = self.compositor.create_surface(&qh, ());
    let lock_surface = self.lock.get_lock_surface(&surface, &output, &qh, name);
    self.outputs.insert(
      name,
      LockOutput {
        global: name,
        output,
        surface,
        lock_surface,
        size: None,
        target: None,
        layout: None,
        buffers: Vec::new(),
        next_buffer_id: 0,
        committed: false,
        frame_pending: false,
        dirty: true,
        last_frame: None,
        clock: None,
        clock_size: (0, 0),
        input: None,
        frames: 0,
      },
    );
  }

  fn remove_output(&mut self, name: u32) {
    if let Some(output) = self.outputs.remove(&name) {
      output.destroy();
      self.images.remove(&name);
      self.ready_reported = self.ready_reported && !self.outputs.is_empty();
    }
  }

  fn add_seat(&mut self, name: u32, version: u32) {
    if !self.active() || self.seats.contains_key(&name) {
      return;
    }
    let qh = self.qh.clone();
    let proxy = self
      .globals
      .registry()
      .bind(name, version.min(7), &qh, name);
    self.seats.insert(
      name,
      Seat {
        proxy,
        keyboard: None,
        pointer: None,
        touch: None,
      },
    );
  }

  fn remove_seat(&mut self, name: u32) {
    if let Some(mut seat) = self.seats.remove(&name) {
      release_devices(&mut seat, wl_seat::Capability::empty());
      if seat.proxy.version() >= 5 {
        seat.proxy.release();
      }
    }
  }

  fn set_capabilities(&mut self, name: u32, capabilities: wl_seat::Capability) {
    let qh = self.qh.clone();
    let active = self.active();
    let Some(seat) = self.seats.get_mut(&name) else {
      self.fail("capabilities arrived for an unknown seat");
      return;
    };
    release_devices(seat, capabilities);
    if !active {
      return;
    }
    if capabilities.contains(wl_seat::Capability::Keyboard)
      && seat.keyboard.is_none()
    {
      seat.keyboard = Some(Keyboard {
        proxy: seat.proxy.get_keyboard(&qh, name),
        xkb: None,
      });
    }
    if capabilities.contains(wl_seat::Capability::Pointer)
      && seat.pointer.is_none()
    {
      seat.pointer = Some(seat.proxy.get_pointer(&qh, name));
    }
    if capabilities.contains(wl_seat::Capability::Touch) && seat.touch.is_none()
    {
      seat.touch = Some(seat.proxy.get_touch(&qh, name));
    }
  }

  fn keyboard_mut(&mut self, seat: u32) -> Option<&mut Keyboard> {
    self.seats.get_mut(&seat)?.keyboard.as_mut()
  }

  /// Whether the screenshot of output `global` drifts.
  fn drifts(&self, global: u32) -> bool {
    self.settings.drift.enabled
      && self.settings.drift.amplitude > 0.0
      && self.images.contains_key(&global)
  }

  /// Whether output `name` wants a frame at `now`.
  fn wants_frame(&self, name: u32, now: Instant) -> bool {
    let Some(output) = self.outputs.get(&name) else {
      return false;
    };
    if output.size.is_none() || output.frame_pending {
      return false;
    }
    if output.dirty || self.indicator.animating(now) {
      return true;
    }
    let interval = Duration::from_secs_f64(1.0 / self.settings.drift.fps);
    self.drifts(name)
      && output
        .last_frame
        .is_none_or(|last| now.saturating_duration_since(last) >= interval)
  }

  /// When something wants a frame next without input.
  fn next_wake(&self, now: Instant) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    let mut consider = |at: Instant| {
      next = Some(next.map_or(at, |next| next.min(at)));
    };
    if self.indicator.animating(now) {
      consider(now + FADE_FRAME);
    }
    if let Some(at) = self.indicator.next_change(now) {
      consider(at);
    }
    let interval = Duration::from_secs_f64(1.0 / self.settings.drift.fps);
    for (name, output) in &self.outputs {
      if self.drifts(*name) && !output.frame_pending {
        consider(output.last_frame.map_or(now, |last| last + interval));
      }
    }
    if self.settings.options.clock {
      // The clock is checked at every turn of the wall clock's second.
      consider(now + until_next_second(SystemTime::now()));
    }
    if let Some(end) = self.grace_end()
      && end > now
    {
      consider(end);
    }
    next
  }

  fn grace_end(&self) -> Option<Instant> {
    let grace = self.settings.options.grace;
    self
      .locked_at
      .filter(|_| !grace.is_zero())
      .map(|locked| locked + grace)
  }

  fn in_grace(&self) -> bool {
    self.grace_end().is_some_and(|end| Instant::now() < end)
  }

  fn mark_dirty(&mut self) {
    for output in self.outputs.values_mut() {
      output.dirty = true;
    }
  }

  /// Draws every output that wants a frame now.
  fn tick(&mut self) {
    if !self.active() {
      return;
    }
    let now = Instant::now();
    if self.settings.options.clock {
      let (time, date) = self.clock_text();
      for output in self.outputs.values_mut() {
        if output
          .clock
          .as_ref()
          .is_none_or(|clock| *clock != (time.clone(), date.clone()))
        {
          output.dirty = true;
        }
      }
    }
    let names = self.outputs.keys().copied().collect::<Vec<_>>();
    for name in names {
      if !self.active() {
        break;
      }
      if self.wants_frame(name, now)
        && let Err(error) = self.render_output(name, now)
      {
        self.fail(error);
        break;
      }
    }
    self.maybe_report_ready();
  }

  fn clock_text(&self) -> (String, String) {
    let now = SystemTime::now();
    let options = &self.settings.options;
    (
      clock::format(&options.timestr, now),
      clock::format(&options.datestr, now),
    )
  }

  fn render_output(&mut self, name: u32, now: Instant) -> Result<(), String> {
    let clock = self.settings.options.clock.then(|| self.clock_text());
    let look = InputLook {
      state: self.indicator.state(),
      content: self
        .indicator
        .content(self.settings.options.show_failed_attempts),
      highlight: self.indicator.highlight(now),
    };
    let opacity = if self.settings.options.show_indicator {
      self.indicator.opacity(now)
    } else {
      0.0
    };
    let drifts = self.drifts(name);
    let qh = self.qh.clone();
    let Some(output) = self.outputs.get_mut(&name) else {
      return Ok(());
    };
    let Some((width, height)) = output.size else {
      return Ok(());
    };
    let options = &self.settings.options;
    if output
      .target
      .as_ref()
      .is_none_or(|target| target.size() != (width, height))
    {
      output.target = Some(Target::new(
        &self.gpu,
        (width, height),
        self.images.get(&name),
      ));
      output.layout = Some(Layout::new((width, height), options));
      output.clock = None;
      output.input = None;
    }
    if output.buffers.is_empty() {
      for _ in 0..2 {
        let id = output.next_buffer_id;
        output.next_buffer_id = output.next_buffer_id.wrapping_add(1);
        output.buffers.push(
          ShmBuffer::new(&self.shm, &qh, name, id, width, height).map_err(
            |error| format!("shared-memory allocation failed: {error}"),
          )?,
        );
      }
    }
    let Some(index) = output.buffers.iter().position(|buffer| !buffer.busy)
    else {
      return Ok(());
    };
    let layout = output.layout.expect("made with the target");
    let target = output.target.as_mut().expect("made above");
    if output.clock != clock {
      let raster = clock.as_ref().and_then(|(time, date)| {
        let color =
          options.state_colors(shingle_config::InputState::Input).text;
        ui::rasterize_clock(time, date, &layout, color, &self.font)
      });
      target.set_clock(&self.gpu, raster.as_ref());
      output.clock_size = raster
        .as_ref()
        .map_or((0, 0), |raster| (raster.width, raster.height));
      output.clock = clock;
    }
    if opacity > 0.0 && output.input.as_ref() != Some(&look) {
      let raster = ui::rasterize_input(
        &layout,
        options,
        options.state_colors(look.state),
        &look.content,
        look.highlight,
        &self.font,
      );
      target.set_input(&self.gpu, Some(&raster));
      output.input = Some(look);
    }
    // Centred; the image is as wide as its widest line.
    let clock_at = output.clock.as_ref().map(|_| {
      (
        (layout.size.0 as i32 - output.clock_size.0 as i32) / 2,
        layout.clock_top,
      )
    });
    let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
    let frame = Frame {
      drift: if drifts {
        Drift::at(elapsed, &self.settings.drift, (width, height))
      } else {
        Drift::STILL
      },
      vignette: options.vignette,
      color: options.color,
      clock: clock_at.map(|at| (at, 1.0)),
      input: (opacity > 0.0)
        .then_some(((layout.input.0, layout.input.1), opacity)),
    };
    let buffer = &mut output.buffers[index];
    let stride = buffer.stride();
    let bytes = buffer
      .bytes_mut()
      .map_err(|error| format!("the frame's buffer: {error}"))?;
    target
      .draw(&self.gpu, &frame, bytes, stride)
      .map_err(|error| {
        format!("GPU rendering failed on output {name}: {error}")
      })?;
    let (w, h) = (
      i32::try_from(width).map_err(|_| "the output is too wide".to_owned())?,
      i32::try_from(height).map_err(|_| "the output is too tall".to_owned())?,
    );
    if !output.committed {
      let region = self.compositor.create_region(&qh, ());
      region.add(0, 0, w, h);
      output.surface.set_opaque_region(Some(&region));
      region.destroy();
    }
    output.buffers[index].busy = true;
    output
      .surface
      .attach(Some(&output.buffers[index].buffer), 0, 0);
    output.surface.damage_buffer(0, 0, w, h);
    output.surface.frame(&qh, name);
    output.surface.commit();
    output.frame_pending = true;
    output.committed = true;
    output.dirty = false;
    output.last_frame = Some(now);
    output.frames += 1;
    if self.trace {
      eprintln!(
        "shingle-lock: frame {} of output {} at {} ms",
        output.frames,
        output.global,
        elapsed_ms(self.started)
      );
    }
    Ok(())
  }

  fn maybe_report_ready(&mut self) {
    if self.ready_reported
      || !self.locked
      || self.outputs.is_empty()
      || !self.outputs.values().all(|output| output.committed)
    {
      return;
    }
    println!(
      "shingle-lock: ready; opaque lock surfaces committed on {} output(s) \
       {} ms after start",
      self.outputs.len(),
      elapsed_ms(self.launched)
    );
    self.ready_reported = true;
    self.notifier.locked();
  }

  /// Unlocks without a password, as any input during the grace period
  /// does.
  fn grace_unlock(&mut self, what: &str) {
    if !self.active() || !self.locked || self.auth_running {
      return;
    }
    eprintln!("shingle-lock: {what} during the grace period unlocks");
    self.password.zeroize();
    self.unlock_requested = true;
    self.lock.unlock_and_destroy();
  }

  fn process_key(&mut self, keysym: xkb::Keysym, text: String) {
    if !self.active() || !self.locked || self.auth_running {
      return;
    }
    if self.in_grace() {
      self.grace_unlock("a key");
      return;
    }
    let now = Instant::now();
    match keysym.raw() {
      xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => {
        self.start_authentication();
        return;
      }
      xkb::keysyms::KEY_BackSpace => {
        if self.password.pop().is_some() {
          self.indicator.deleted(self.password.chars().count(), now);
        } else {
          self.indicator.cleared(now);
        }
      }
      xkb::keysyms::KEY_Escape | xkb::keysyms::KEY_Delete => {
        self.password.zeroize();
        self.indicator.cleared(now);
      }
      _ => {
        let previous_len = self.password.len();
        for character in
          text.chars().filter(|character| !character.is_control())
        {
          let mut encoded = [0_u8; 4];
          let encoded = character.encode_utf8(&mut encoded);
          if self.password.len() + encoded.len() > MAX_PASSWORD_BYTES {
            break;
          }
          self.password.push(character);
        }
        if self.password.len() != previous_len {
          self.indicator.typed(self.password.chars().count(), now);
        } else {
          self.indicator.poked(now);
        }
      }
    }
    self.mark_dirty();
  }

  fn start_authentication(&mut self) {
    if !self.active() || !self.locked || self.auth_running {
      return;
    }
    let now = Instant::now();
    if self.password.is_empty() && self.settings.options.ignore_empty_password {
      self.indicator.poked(now);
      self.mark_dirty();
      return;
    }
    self.auth_running = true;
    self.indicator.verifying(now);
    self.mark_dirty();
    let mut password = std::mem::take(&mut self.password);
    let password = Password::new(std::mem::take(&mut *password));
    let authenticator = self.authenticator.clone();
    let username = self.username.clone();
    let sender = self.auth_sender.clone();
    let worker =
      thread::Builder::new()
        .name("shingle-pam".to_owned())
        .spawn(move || {
          let outcome = match catch_unwind(AssertUnwindSafe(|| {
            authenticator.authenticate(&username, password)
          })) {
            Ok(Ok(())) => AuthOutcome::Authenticated,
            Ok(Err(_)) => AuthOutcome::Denied,
            Err(_) => AuthOutcome::WorkerFailed(
              "authentication worker panicked; session remains locked"
                .to_owned(),
            ),
          };
          let _ = sender.send(outcome);
        });
    if let Err(error) = worker {
      self.fail(format!(
        "failed to start authentication worker; session remains locked: {error}"
      ));
      return;
    }
    eprintln!("shingle-lock: authentication started");
  }

  fn handle_auth(&mut self, outcome: AuthOutcome) {
    if !self.auth_running || !self.active() {
      return;
    }
    self.auth_running = false;
    match outcome {
      AuthOutcome::Authenticated => {
        if !self.locked || self.finished {
          self.fail("authentication completed without an active session lock");
          return;
        }
        self.password.zeroize();
        self.unlock_requested = true;
        self.lock.unlock_and_destroy();
        eprintln!(
          "shingle-lock: PAM authentication and account validation succeeded"
        );
      }
      AuthOutcome::Denied => {
        self.password.zeroize();
        self.indicator.denied(Instant::now());
        let attempts = self.indicator.attempts();
        let plural = if attempts == 1 { "" } else { "s" };
        eprintln!(
          "shingle-lock: authentication failed ({attempts} failed \
           attempt{plural}); session remains locked"
        );
        self.mark_dirty();
      }
      AuthOutcome::WorkerFailed(error) => self.fail(error),
    }
  }
}

/// Releases the devices of `seat` its capabilities no longer include.
fn release_devices(seat: &mut Seat, capabilities: wl_seat::Capability) {
  if !capabilities.contains(wl_seat::Capability::Keyboard)
    && let Some(keyboard) = seat.keyboard.take()
    && keyboard.proxy.version() >= 3
  {
    keyboard.proxy.release();
  }
  if !capabilities.contains(wl_seat::Capability::Pointer)
    && let Some(pointer) = seat.pointer.take()
    && pointer.version() >= 3
  {
    pointer.release();
  }
  if !capabilities.contains(wl_seat::Capability::Touch)
    && let Some(touch) = seat.touch.take()
    && touch.version() >= 3
  {
    touch.release();
  }
}

/// How long from `now` until the wall clock's next whole second.
fn until_next_second(now: SystemTime) -> Duration {
  let into = now
    .duration_since(SystemTime::UNIX_EPOCH)
    .map_or(0, |since| since.subsec_nanos());
  Duration::from_nanos(u64::from(1_000_000_000 - into))
}

fn elapsed_ms(since: Instant) -> u128 {
  since.elapsed().as_millis()
}

/// How long each step before the lock request took, for the report on
/// standard error that the GPU harness reads.
struct Timing {
  launched: Instant,
  last: Instant,
  steps: Vec<(&'static str, Duration)>,
}

impl Timing {
  fn new(launched: Instant) -> Self {
    let now = Instant::now();
    Self {
      launched,
      last: now,
      steps: vec![("start", now - launched)],
    }
  }

  /// Ends step `name` now.
  fn mark(&mut self, name: &'static str) {
    let now = Instant::now();
    self.steps.push((name, now - self.last));
    self.last = now;
  }

  /// `name N ms, ...` and the total since the process started.
  fn report(&self) -> String {
    let steps = self
      .steps
      .iter()
      .map(|(name, took)| format!("{name} {} ms", took.as_millis()))
      .collect::<Vec<_>>()
      .join(", ");
    format!(
      "{steps}; lock requested {} ms after start",
      self.launched.elapsed().as_millis()
    )
  }
}

fn required_global<T>(
  globals: &GlobalList,
  qh: &QueueHandle<State>,
  versions: std::ops::RangeInclusive<u32>,
  interface: &str,
) -> Result<T, LockError>
where
  T: Proxy + 'static,
  State: Dispatch<T, ()>,
{
  globals.bind(qh, versions, ()).map_err(|source| {
    let reason = match source {
      BindError::NotPresent => "is not advertised",
      BindError::UnsupportedVersion => "has an unsupported version",
    };
    LockError::Message(format!("required Wayland global {interface} {reason}"))
  })
}

fn run(
  settings: Settings,
  notifier: daemon::Notifier,
  launched: Instant,
) -> Result<(), LockError> {
  let username = username::current()?;
  let connection = Connection::connect_to_env()?;
  let (globals, event_queue) = registry_queue_init::<State>(&connection)?;
  let qh = event_queue.handle();
  let compositor = required_global(&globals, &qh, 4..=4, "wl_compositor")?;
  let shm = required_global(&globals, &qh, 1..=1, "wl_shm")?;
  let manager: ExtSessionLockManagerV1 =
    required_global(&globals, &qh, 1..=1, "ext_session_lock_manager_v1")?;
  let initial_globals = globals.contents().clone_list();
  if !initial_globals
    .iter()
    .any(|global| global.interface == "wl_output")
  {
    return Err(LockError::Message(
      "no wl_output globals are available; lock request aborted".to_owned(),
    ));
  }
  // Everything that can fail or take long happens before the lock is
  // asked for: once it is, the compositor shows no client any more.
  let mut timing = Timing::new(launched);
  let gpu = Gpu::new()
    .map_err(|error| LockError::Message(format!("no GPU: {error}")))?;
  timing.mark("gpu");
  let font = font::load(&settings.options.font);
  timing.mark("font");
  let mut images = HashMap::new();
  if settings.options.screenshot {
    let shots =
      capture::capture_outputs(&connection, &globals, CAPTURE_TIMEOUT);
    timing.mark("capture");
    for (name, shot) in &shots {
      images.insert(*name, gpu.image(shot, settings.options.blur));
    }
    timing.mark("blur");
    eprintln!("shingle-lock: captured {} output(s)", shots.len());
  }
  let lock = manager.lock(&qh, ());
  eprintln!("shingle-lock: timing: {}", timing.report());

  let mut event_loop = EventLoop::<State>::try_new()?;
  let signals = Signals::new(&[
    Signal::SIGINT,
    Signal::SIGTERM,
    Signal::SIGQUIT,
    Signal::SIGHUP,
  ])?;
  let (auth_sender, auth_receiver) = channel::channel();
  let started = Instant::now();
  let indicator = Indicator::new(
    settings.input_timeout,
    settings.options.indicator_always,
    started,
  );
  let mut state = State {
    connection: connection.clone(),
    qh: qh.clone(),
    globals,
    _manager: manager,
    lock,
    compositor,
    shm,
    outputs: HashMap::new(),
    seats: HashMap::new(),
    xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
    settings,
    gpu,
    images,
    font,
    indicator,
    started,
    launched,
    locked_at: None,
    notifier,
    trace: std::env::var_os("SHINGLE_LOCK_TRACE").is_some(),
    authenticator: Authenticator::from_env(),
    username,
    auth_sender,
    auth_running: false,
    password: Zeroizing::new(String::new()),
    locked: false,
    finished: false,
    unlock_requested: false,
    ready_reported: false,
    failure: None,
  };
  for global in &initial_globals {
    match global.interface.as_str() {
      "wl_output" => state.add_output(global.name, global.version),
      "wl_seat" => state.add_seat(global.name, global.version),
      _ => {}
    }
  }

  event_loop
    .handle()
    .insert_source(signals, |_, _, state| {
      state.fail("termination signal received; session remains locked");
    })
    .map_err(|error| {
      LockError::Message(format!("failed to register signals: {error}"))
    })?;
  event_loop
    .handle()
    .insert_source(auth_receiver, |event, _, state| match event {
      ChannelEvent::Msg(outcome) => state.handle_auth(outcome),
      ChannelEvent::Closed => {
        state.fail("authentication result channel closed unexpectedly")
      }
    })
    .map_err(|error| {
      LockError::Message(format!(
        "failed to register authentication channel: {error}"
      ))
    })?;
  WaylandSource::new(connection.clone(), event_queue)
    .insert(event_loop.handle())
    .map_err(|error| {
      LockError::Message(format!("failed to register Wayland source: {error}"))
    })?;

  eprintln!("shingle-lock: session lock requested");
  while state.failure.is_none() && !state.unlock_requested {
    let now = Instant::now();
    let timeout = state
      .next_wake(now)
      .map(|at| at.saturating_duration_since(now));
    event_loop.dispatch(timeout, &mut state)?;
    state.tick();
    let _ = state.connection.flush();
  }
  if let Some(failure) = state.failure {
    return Err(LockError::Message(failure));
  }
  state.connection.roundtrip().map_err(|error| {
    LockError::Message(format!("failed to confirm unlock request: {error}"))
  })?;
  println!("shingle-lock: session unlocked");
  Ok(())
}

fn main() -> ExitCode {
  let launched = Instant::now();
  let arguments = std::env::args().skip(1).collect::<Vec<_>>();
  let (settings, warnings) =
    match options::resolve(&arguments, &options::Sources::system()) {
      Ok(Action::Run(settings, warnings)) => (*settings, warnings),
      Ok(Action::Help) => {
        println!(
          "shingle-lock {}\n\n{}",
          env!("CARGO_PKG_VERSION"),
          options::HELP
        );
        return ExitCode::SUCCESS;
      }
      Ok(Action::Version) => {
        println!("shingle-lock {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
      }
      Err(error) => {
        eprintln!("shingle-lock: {error}");
        return ExitCode::FAILURE;
      }
    };
  for warning in warnings {
    eprintln!("shingle-lock: {warning}");
  }
  let notifier = if settings.options.daemonize {
    match daemon::daemonize() {
      Ok(Ok(notifier)) => notifier,
      Ok(Err(status)) => return ExitCode::from(status),
      Err(error) => {
        eprintln!("shingle-lock: cannot daemonize: {error}");
        return ExitCode::FAILURE;
      }
    }
  } else {
    daemon::Notifier::none()
  };
  match run(settings, notifier, launched) {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("shingle-lock: {error}");
      ExitCode::FAILURE
    }
  }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _data: &GlobalListContents,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if !state.active() {
      return;
    }
    match event {
      wl_registry::Event::Global {
        name,
        interface,
        version,
      } => match interface.as_str() {
        "wl_output" => state.add_output(name, version),
        "wl_seat" => state.add_seat(name, version),
        _ => {}
      },
      wl_registry::Event::GlobalRemove { name } => {
        if state.outputs.contains_key(&name) {
          state.remove_output(name);
        } else if state.seats.contains_key(&name) {
          state.remove_seat(name);
        }
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
  fn event(
    state: &mut Self,
    proxy: &ExtSessionLockV1,
    event: <ExtSessionLockV1 as Proxy>::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if !state.active() {
      return;
    }
    match event {
      ext_session_lock_v1::Event::Locked => {
        state.locked = true;
        state.locked_at = Some(Instant::now());
        eprintln!("shingle-lock: compositor reported the session locked");
        state.maybe_report_ready();
      }
      ext_session_lock_v1::Event::Finished => {
        state.finished = true;
        if state.locked {
          state.fail(concat!(
            "compositor finished the session lock; ",
            "no unlock was requested",
          ));
        } else {
          proxy.destroy();
          state.fail(concat!(
            "compositor denied or finished the lock ",
            "before it was acquired",
          ));
        }
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtSessionLockSurfaceV1, u32> for State {
  fn event(
    state: &mut Self,
    proxy: &ExtSessionLockSurfaceV1,
    event: <ExtSessionLockSurfaceV1 as Proxy>::Event,
    output_name: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if !state.active() {
      return;
    }
    if let ext_session_lock_surface_v1::Event::Configure {
      serial,
      width,
      height,
    } = event
    {
      proxy.ack_configure(serial);
      let Some(output) = state.outputs.get_mut(output_name) else {
        return;
      };
      if width == 0 || height == 0 {
        output.size = None;
        output.target = None;
        output.buffers.clear();
        output.committed = false;
        return;
      }
      if output.size != Some((width, height)) {
        output.size = Some((width, height));
        output.buffers.clear();
      }
      // A configure is answered at once, whatever frame is pending.
      output.committed = false;
      output.dirty = true;
      output.frame_pending = false;
      if let Err(error) = state.render_output(*output_name, Instant::now()) {
        state.fail(error);
        return;
      }
      state.maybe_report_ready();
    }
  }
}

impl Dispatch<wl_callback::WlCallback, u32> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_callback::WlCallback,
    event: wl_callback::Event,
    output_name: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { .. } = event
      && let Some(output) = state.outputs.get_mut(output_name)
    {
      output.frame_pending = false;
    }
  }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_seat::WlSeat,
    event: wl_seat::Event,
    seat_name: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if !state.active() {
      return;
    }
    if let wl_seat::Event::Capabilities { capabilities } = event {
      match capabilities {
        WEnum::Value(capabilities) => {
          state.set_capabilities(*seat_name, capabilities)
        }
        WEnum::Unknown(_) => {
          state.fail("compositor sent unknown seat capabilities")
        }
      }
    }
  }
}

impl Dispatch<wl_keyboard::WlKeyboard, u32> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_keyboard::WlKeyboard,
    event: wl_keyboard::Event,
    seat_name: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if !state.active() {
      return;
    }
    match event {
      wl_keyboard::Event::Keymap { format, fd, size } => {
        if !matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1))
          || size == 0
        {
          state.fail(
            "compositor provided an unsupported or empty keyboard keymap",
          );
          return;
        }
        // SAFETY: The compositor transfers ownership of a valid keymap fd and
        // its exact size; xkbcommon maps it read-only and takes ownership of
        // the descriptor.
        let keymap = match unsafe {
          xkb::Keymap::new_from_fd(
            &state.xkb_context,
            fd,
            size as usize,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
          )
        } {
          Ok(Some(keymap)) => keymap,
          Ok(None) => {
            state.fail("failed to compile the compositor keyboard keymap");
            return;
          }
          Err(error) => {
            state.fail(format!("failed to read the keyboard keymap: {error}"));
            return;
          }
        };
        if let Some(keyboard) = state.keyboard_mut(*seat_name) {
          keyboard.xkb = Some(xkb::State::new(&keymap));
        }
      }
      wl_keyboard::Event::Modifiers {
        mods_depressed,
        mods_latched,
        mods_locked,
        group,
        ..
      } => {
        if let Some(keyboard) = state.keyboard_mut(*seat_name)
          && let Some(xkb) = &mut keyboard.xkb
        {
          xkb.update_mask(
            mods_depressed,
            mods_latched,
            mods_locked,
            0,
            0,
            group,
          );
        }
      }
      wl_keyboard::Event::Key {
        key,
        state: key_state,
        ..
      } => {
        if !matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed)) {
          return;
        }
        let Some(keyboard) = state.keyboard_mut(*seat_name) else {
          state.fail("key event arrived for an unknown keyboard");
          return;
        };
        let Some(xkb) = &keyboard.xkb else {
          state.fail("key event arrived before a valid keyboard keymap");
          return;
        };
        let keycode = xkb::Keycode::new(key.saturating_add(8));
        let keysym = xkb.key_get_one_sym(keycode);
        let text = xkb.key_get_utf8(keycode);
        state.process_key(keysym, text);
      }
      _ => {}
    }
  }
}

impl Dispatch<wl_pointer::WlPointer, u32> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_pointer::WlPointer,
    event: wl_pointer::Event,
    _seat: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    let moved = matches!(
      event,
      wl_pointer::Event::Motion { .. } | wl_pointer::Event::Button { .. }
    );
    if moved && state.in_grace() && !state.settings.options.grace_no_mouse {
      state.grace_unlock("the pointer");
    }
  }
}

impl Dispatch<wl_touch::WlTouch, u32> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_touch::WlTouch,
    event: wl_touch::Event,
    _seat: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if matches!(event, wl_touch::Event::Down { .. })
      && state.in_grace()
      && !state.settings.options.grace_no_touch
    {
      state.grace_unlock("a touch");
    }
  }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    data: &BufferData,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if !state.active() || !matches!(event, wl_buffer::Event::Release) {
      return;
    }
    if let Some(output) = state.outputs.get_mut(&data.output)
      && let Some(buffer) = output
        .buffers
        .iter_mut()
        .find(|buffer| buffer.id == data.id)
    {
      buffer.busy = false;
    }
  }
}

macro_rules! noop_dispatch {
    ($($interface:ty),+ $(,)?) => {
        $(
            impl Dispatch<$interface, ()> for State {
                fn event(
                    _state: &mut Self,
                    _proxy: &$interface,
                    _event: <$interface as Proxy>::Event,
                    _data: &(),
                    _conn: &Connection,
                    _qh: &QueueHandle<Self>,
                ) {
                }
            }
        )+
    };
}

noop_dispatch!(
  wl_compositor::WlCompositor,
  wl_output::WlOutput,
  wl_region::WlRegion,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_surface::WlSurface,
  ExtSessionLockManagerV1,
);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_clock_wakes_at_the_next_whole_second() {
    let at = |millis| SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
    assert_eq!(until_next_second(at(10_250)), Duration::from_millis(750));
    assert_eq!(until_next_second(at(10_000)), Duration::from_secs(1));
  }
}
