//! `shingle-lock`: Shingle's lock screen, an ext-session-lock-v1 client
//! that takes swaylock's and swaylock-effects' options and unlocks with
//! PAM through `shingle-auth` (docs/design/lock-screen.md).
//!
//! Unless the config turns it off, it captures every output before it
//! asks for the lock, while the GPU opens, blurs the captures once on the
//! GPU and shows them, slowly drifting, under a vignette and the clock;
//! an output added later shows the capture of another. In the minimal
//! style (the default, crates/shingle-lock/src/minimal.rs) typing moves
//! the clock away for a slim white outline with a dot per character; in
//! the swaylock style an outlined input in swaylock's colours fades in.
//! Frames follow the compositor's frame callbacks, at the output's rate
//! while something moves and at the drift's rate otherwise, so nothing is
//! drawn while an output is off.

mod capture;
mod clock;
mod daemon;
mod damage;
mod font;
mod gpu;
mod minimal;
mod motion;
mod options;
mod shm;
mod ui;
mod username;
mod zero_copy;

use std::{
  collections::HashMap,
  io,
  panic::{AssertUnwindSafe, catch_unwind},
  process::ExitCode,
  thread,
  time::{Duration, Instant, SystemTime},
};

use calloop::{
  EventLoop,
  channel::{self, Event as ChannelEvent, Sender},
  signals::{Signal, Signals},
};
use calloop_wayland_source::WaylandSource;
use shingle_auth::{Authenticator, Password, ZeroOnFree};
use shingle_config::LockStyle;
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
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
  zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
  zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use xkbcommon::xkb;
use zeroize::{Zeroize, Zeroizing};

use crate::{
  capture::Screenshot,
  damage::Rect,
  font::Fonts,
  gpu::{Drift, Frame, Gpu, Image, Placement, Target},
  minimal::Scene,
  options::{Action, Settings},
  ui::{Indicator, InputLook, Labels, Layout},
};

// Zeroes every freed heap block, including the password copy that nonstick
// drops after handing it to PAM and old buffers of a growing password.
#[global_allocator]
static ALLOCATOR: ZeroOnFree<std::alloc::System> =
  ZeroOnFree::new(std::alloc::System);

const MAX_PASSWORD_BYTES: usize = 4096;
/// How long the screenshots may take before the lock goes ahead without
/// the missing ones. A drawn output answers within a frame or two; one
/// that has not answered in this time is not being drawn, and its lock
/// surface shows another output's capture.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(1);
/// A wake-up while something moves, in case no frame callback comes;
/// frames themselves follow the callbacks.
const FADE_FRAME: Duration = Duration::from_millis(16);
/// How long after the success fade the unlock waits at most for every
/// output to have shown its faded frame; an output that is off sends no
/// frame callback.
const UNLOCK_WAIT: Duration = Duration::from_millis(250);
/// How bright the minimal style draws the screenshot, so that its white
/// reads over a light desktop too.
const MINIMAL_DIM: f32 = 0.78;

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

/// Ids of DMA-BUFs, apart from those of `wl_shm` buffers.
const DMABUF_ID: u64 = 1 << 63;

/// The buffer a frame is drawn into.
#[derive(Clone, Copy, Debug)]
enum Chosen {
  /// `LockOutput::buffers`' at this index.
  Shm(usize),
  /// `LockOutput::slots`' at this index.
  Dmabuf(usize),
}

/// What the input shows of a keyboard: Caps Lock and its layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KeyboardStatus {
  /// Caps Lock is locked on.
  caps_lock: bool,
  /// How many layouts the keymap has.
  layouts: usize,
  /// The active layout's name, such as `English (US)`.
  layout: Option<String>,
}

impl KeyboardStatus {
  /// The status of `state`, as swaylock reads it: Caps Lock from the
  /// locked `Lock` modifier, the layout the first effective one
  /// (swaylock's `seat.c` and `render.c`).
  fn of(state: &xkb::State) -> Self {
    let keymap = state.get_keymap();
    let layouts = keymap.num_layouts();
    let active = (0..layouts).find(|index| {
      state.layout_index_is_active(*index, xkb::STATE_LAYOUT_EFFECTIVE)
    });
    Self {
      caps_lock: state
        .mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_LOCKED),
      layouts: layouts as usize,
      layout: active.map(|index| keymap.layout_get_name(index).to_owned()),
    }
  }
}

/// A screenshot on the GPU, kept for the whole lock so that an output
/// that comes back or is added gets a blurred background too.
struct Capture {
  /// The `wl_output` global it was taken of.
  global: u32,
  /// That output's name, if the compositor sent one.
  name: Option<String>,
  /// Its size in pixels.
  size: (u32, u32),
  image: Image,
}

/// Which of `captures`, as (global, name, size), an output shows: its
/// own, by `global` or else by `name` (an output turned off and on
/// again comes back under a new global), otherwise the capture whose
/// aspect ratio is closest to `size`'s, the larger on a tie, then the
/// first. `None` without captures.
fn pick_capture(
  captures: &[(u32, Option<&str>, (u32, u32))],
  global: u32,
  name: Option<&str>,
  size: (u32, u32),
) -> Option<usize> {
  if let Some(own) = captures.iter().position(|capture| capture.0 == global) {
    return Some(own);
  }
  if let Some(name) = name
    && let Some(same) =
      captures.iter().position(|capture| capture.1 == Some(name))
  {
    return Some(same);
  }
  let aspect = |(width, height): (u32, u32)| {
    (f64::from(width.max(1)) / f64::from(height.max(1))).ln()
  };
  let wanted = aspect(size);
  let mut best: Option<(usize, f64, u64)> = None;
  for (index, (_, _, size)) in captures.iter().enumerate() {
    let distance = (aspect(*size) - wanted).abs();
    let area = u64::from(size.0) * u64::from(size.1);
    let better = best.is_none_or(|(_, best_distance, best_area)| {
      distance < best_distance - 1e-9
        || (distance <= best_distance + 1e-9 && area > best_area)
    });
    if better {
      best = Some((index, distance, area));
    }
  }
  best.map(|(index, _, _)| index)
}

struct LockOutput {
  global: u32,
  output: wl_output::WlOutput,
  /// Its `wl_output` name, once the compositor sent it.
  name: Option<String>,
  /// The index in [`State::captures`] it shows, chosen with its target.
  capture: Option<usize>,
  /// Its surface and lock surface, made once its `wl_output`
  /// described itself ([`State::give_lock_surface`]); `None` while it
  /// waits, or while another global of the same output has them.
  surfaces: Option<(wl_surface::WlSurface, ExtSessionLockSurfaceV1)>,
  size: Option<(u32, u32)>,
  target: Option<Target>,
  layout: Option<Layout>,
  /// The minimal style's layout, made with the target.
  minimal_layout: Option<minimal::Layout>,
  buffers: Vec<ShmBuffer>,
  /// Its DMA-BUFs, when the compositor shows them ([`zero_copy`]);
  /// then `buffers` stays empty.
  slots: Vec<zero_copy::Slot>,
  next_buffer_id: u64,
  /// A full frame was committed since the last configure.
  committed: bool,
  /// A frame callback is outstanding: no frame until it is done, so an
  /// output the compositor does not draw, such as one that is off, gets
  /// none.
  frame_pending: bool,
  /// What it shows changed since its last frame.
  dirty: bool,
  /// The drift step of its last frame ([`State::drift_step`]).
  drawn_step: Option<u64>,
  clock: Option<(String, String)>,
  /// The clock image's size.
  clock_size: (u32, u32),
  input: Option<InputLook>,
  /// The minimal input's image: what it shows, its size and where the
  /// outline sits in it.
  minimal_input: Option<minimal::ShownInput>,
  /// A frame drawn after the success fade ended.
  faded: bool,
  frames: u64,
}

impl LockOutput {
  fn destroy(self) {
    if let Some((surface, lock_surface)) = self.surfaces {
      lock_surface.destroy();
      surface.destroy();
    }
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
  /// The DMA-BUF allocator and the compositor's `zwp_linux_dmabuf_v1`,
  /// when frames go to the compositor without a copy.
  zero_copy: Option<(zero_copy::Allocator, ZwpLinuxDmabufV1)>,
  /// The `wl_shm` formats the compositor takes.
  shm_formats: Vec<wl_shm::Format>,
  /// Every screenshot, in the order of the outputs' globals.
  captures: Vec<Capture>,
  /// The status of the keyboard that last reported one.
  keyboard: KeyboardStatus,
  fonts: Fonts,
  indicator: Indicator,
  /// The minimal style's animations.
  scene: Scene,
  /// When PAM accepted the password and the minimal style began to fade
  /// out; the unlock follows the fade.
  leaving: Option<Instant>,
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
    let output: wl_output::WlOutput =
      self
        .globals
        .registry()
        .bind(name, version.min(4), &qh, name);
    // wl_output before version 2 sends no done event to wait for.
    let describes_itself = output.version() >= 2;
    self.outputs.insert(
      name,
      LockOutput {
        global: name,
        output,
        name: None,
        capture: None,
        surfaces: None,
        size: None,
        target: None,
        layout: None,
        minimal_layout: None,
        buffers: Vec::new(),
        slots: Vec::new(),
        next_buffer_id: 0,
        committed: false,
        frame_pending: false,
        dirty: true,
        drawn_step: None,
        clock: None,
        clock_size: (0, 0),
        input: None,
        minimal_input: None,
        faded: false,
        frames: 0,
      },
    );
    if !describes_itself {
      self.give_lock_surface(name);
    }
  }

  /// Gives output `global` its lock surface, unless another global of
  /// the same output, by its `wl_output` name, has one: a compositor
  /// that replaces an output's global announces the new one while the
  /// client may still hold the old, and a second lock surface for one
  /// output is a protocol error. The waiting one gets its surface when
  /// the other goes ([`State::remove_output`]).
  fn give_lock_surface(&mut self, global: u32) {
    if !self.active() {
      return;
    }
    let others = self
      .outputs
      .values()
      .map(|output| {
        (
          output.global,
          output.name.as_deref(),
          output.surfaces.is_some(),
        )
      })
      .collect::<Vec<_>>();
    let Some(output) = self.outputs.get(&global) else {
      return;
    };
    if output.surfaces.is_some() {
      return;
    }
    if let Some(holder) =
      lock_surface_holder(&others, global, output.name.as_deref())
    {
      eprintln!(
        "shingle-lock: output {global} ({}) is the output of global {holder}, \
         which has its lock surface; waiting for that one to go",
        output.name.as_deref().unwrap_or("unnamed")
      );
      return;
    }
    let qh = self.qh.clone();
    let surface = self.compositor.create_surface(&qh, ());
    let lock_surface =
      self
        .lock
        .get_lock_surface(&surface, &output.output, &qh, global);
    if let Some(output) = self.outputs.get_mut(&global) {
      output.surfaces = Some((surface, lock_surface));
    }
  }

  fn remove_output(&mut self, name: u32) {
    // Its capture stays, for the output if it comes back.
    if let Some(output) = self.outputs.remove(&name) {
      let alias = output.name.clone();
      output.destroy();
      self.ready_reported = self.ready_reported && !self.outputs.is_empty();
      // Another global of the same output may have waited for this one.
      let waiting = self
        .outputs
        .values()
        .filter(|other| {
          other.surfaces.is_none() && alias.is_some() && other.name == alias
        })
        .map(|other| other.global)
        .collect::<Vec<_>>();
      for global in waiting {
        self.give_lock_surface(global);
      }
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

  /// Whether the screenshot on output `global` drifts.
  fn drifts(&self, global: u32) -> bool {
    self.settings.drift.enabled
      && self.settings.drift.amplitude > 0.0
      && self
        .outputs
        .get(&global)
        .is_some_and(|output| output.capture.is_some())
  }

  /// The capture output `global` of `size` shows ([`pick_capture`]).
  fn capture_for(&self, global: u32, size: (u32, u32)) -> Option<usize> {
    let candidates = self
      .captures
      .iter()
      .map(|capture| (capture.global, capture.name.as_deref(), capture.size))
      .collect::<Vec<_>>();
    let name = self.outputs.get(&global)?.name.as_deref();
    pick_capture(&candidates, global, name, size)
  }

  /// Takes `status` from a keyboard; redraws on a change.
  fn set_keyboard_status(&mut self, status: KeyboardStatus) {
    if status != self.keyboard {
      self.keyboard = status;
      self.mark_dirty();
    }
  }

  /// The input as it looks at `now`.
  fn input_look(&self, now: Instant) -> InputLook {
    let options = &self.settings.options;
    let state = self.indicator.state();
    let layout = self
      .keyboard
      .layout
      .as_deref()
      .filter(|_| options.shows_keyboard_layout(self.keyboard.layouts));
    InputLook {
      state,
      caps_lock: self.keyboard.caps_lock,
      content: self
        .indicator
        .content(options.show_failed_attempts, &options.texts),
      highlight: self.indicator.highlight(now),
      labels: Labels::new(options, state, self.keyboard.caps_lock, layout),
    }
  }

  /// Whether output `name` wants a frame at `now`.
  fn wants_frame(&self, name: u32, now: Instant) -> bool {
    let Some(output) = self.outputs.get(&name) else {
      return false;
    };
    if output.size.is_none() || output.frame_pending {
      return false;
    }
    if output.dirty || self.animating(now) {
      return true;
    }
    if self.leaving.is_some() && self.scene.left(now) && !output.faded {
      return true;
    }
    self.drifts(name)
      && (self.continuous() || output.drawn_step != Some(self.drift_step(now)))
  }

  /// Whether the drift moves at every frame callback (`drift.fps` 0)
  /// rather than in steps.
  fn continuous(&self) -> bool {
    self.settings.drift.fps <= 0.0
  }

  /// Where the drift is at `now`, in seconds since the lock: smooth, or
  /// at the start of its step with `drift.fps` steps.
  fn drift_seconds(&self, now: Instant) -> f64 {
    let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
    if self.continuous() {
      elapsed
    } else {
      self.drift_step(now) as f64 / self.settings.drift.fps
    }
  }

  /// Whether the input or the scene moves at `now`, which wants a frame
  /// at every frame callback.
  fn animating(&self, now: Instant) -> bool {
    match self.settings.style {
      LockStyle::Minimal => self.scene.animating(now),
      LockStyle::Swaylock => self.indicator.animating(now),
    }
  }

  /// The drift's step at `now` with `drift.fps` steps a second: between
  /// two steps the background stays, and a frame that only moves the
  /// clock or the input reads back only their part. 0 when it moves at
  /// every frame.
  fn drift_step(&self, now: Instant) -> u64 {
    if self.continuous() {
      return 0;
    }
    let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
    (elapsed * self.settings.drift.fps).floor() as u64
  }

  /// When drift step `step` begins.
  fn drift_step_at(&self, step: u64) -> Instant {
    self.started
      + Duration::from_secs_f64(step as f64 / self.settings.drift.fps)
  }

  /// When something wants a frame next without input.
  fn next_wake(&self, now: Instant) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    let mut consider = |at: Instant| {
      next = Some(next.map_or(at, |next| next.min(at)));
    };
    if self.animating(now) {
      consider(now + FADE_FRAME);
    }
    let change = match self.settings.style {
      LockStyle::Minimal => self.scene.next_change(now),
      LockStyle::Swaylock => self.indicator.next_change(now),
    };
    if let Some(at) = change {
      consider(at);
    }
    if let Some(leaving) = self.leaving {
      consider((leaving + minimal::LEAVE).max(now + FADE_FRAME));
    }
    let next_step = if self.continuous() {
      // Frame callbacks pace it; this only wakes the loop.
      now + FADE_FRAME
    } else {
      self.drift_step_at(self.drift_step(now) + 1)
    };
    for (name, output) in &self.outputs {
      if self.drifts(*name) && !output.frame_pending {
        consider(next_step);
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
    self.maybe_unlock_after_fade(now);
  }

  /// Unlocks once the success fade is over and every output showed its
  /// faded frame, or [`UNLOCK_WAIT`] after the fade for outputs that do
  /// not draw.
  fn maybe_unlock_after_fade(&mut self, now: Instant) {
    let Some(leaving) = self.leaving else {
      return;
    };
    if !self.active() || !self.scene.left(now) {
      return;
    }
    let shown = self
      .outputs
      .values()
      .all(|output| output.faded && !output.frame_pending);
    if !shown && now < leaving + minimal::LEAVE + UNLOCK_WAIT {
      return;
    }
    eprintln!(
      "shingle-lock: unlocking after the fade, {} ms after the password \
       was accepted",
      now.saturating_duration_since(leaving).as_millis()
    );
    self.unlock_requested = true;
    self.lock.unlock_and_destroy();
  }

  /// The format frames read back into `wl_shm` are drawn in: 10 bits a
  /// channel where the compositor takes `XBGR2101010`, else 8. Frames
  /// drawn as DMA-BUFs take the buffer's format.
  fn texture_format(&self) -> wgpu::TextureFormat {
    if self.zero_copy.is_none()
      && self.shm_formats.contains(&wl_shm::Format::Xbgr2101010)
    {
      gpu::FORMAT_10
    } else {
      gpu::FORMAT
    }
  }

  fn clock_text(&self) -> (String, String) {
    let now = SystemTime::now();
    let options = &self.settings.options;
    let style = self.settings.style;
    (
      clock::format(options.time_format(style), now),
      clock::format(options.date_format(style), now),
    )
  }

  /// The minimal style's labels below the input: Caps Lock, the layout
  /// and the failed attempts, each when it applies.
  fn minimal_labels(&self) -> Vec<String> {
    let options = &self.settings.options;
    let mut labels = Vec::new();
    if self.keyboard.caps_lock
      && options.caps_lock_text
      && !options.texts.caps_lock.is_empty()
    {
      labels.push(options.texts.caps_lock.clone());
    }
    if let Some(layout) = self
      .keyboard
      .layout
      .as_deref()
      .filter(|name| !name.is_empty())
      .filter(|_| options.shows_keyboard_layout(self.keyboard.layouts))
    {
      labels.push(layout.to_owned());
    }
    let attempts = self.indicator.attempts();
    if options.show_failed_attempts && attempts > 0 {
      let plural = if attempts == 1 { "" } else { "s" };
      labels.push(format!("{attempts} failed attempt{plural}"));
    }
    labels
  }

  fn render_output(&mut self, name: u32, now: Instant) -> Result<(), String> {
    let style = self.settings.style;
    let clock = self.settings.options.clock.then(|| self.clock_text());
    let look = self.input_look(now);
    let opacity = if self.settings.options.show_indicator {
      self.indicator.opacity(now)
    } else {
      0.0
    };
    let scene = self.scene.look(now);
    let left = self.leaving.is_some() && self.scene.left(now);
    let labels = self.minimal_labels();
    let step = self.drift_step(now);
    let drift_seconds = self.drift_seconds(now);
    let continuous = self.continuous();
    let qh = self.qh.clone();
    let Some((width, height)) =
      self.outputs.get(&name).and_then(|output| output.size)
    else {
      return Ok(());
    };
    let capture = self.capture_for(name, (width, height));
    let texture_format = self.texture_format();
    let Some(output) = self.outputs.get_mut(&name) else {
      return Ok(());
    };
    let Some((surface, _)) = output.surfaces.clone() else {
      return Ok(());
    };
    let options = &self.settings.options;
    if output
      .target
      .as_ref()
      .is_none_or(|target| target.size() != (width, height))
    {
      output.capture = capture;
      let image = capture.map(|index| &self.captures[index].image);
      output.target = Some(Target::new(
        &self.gpu,
        (width, height),
        image,
        texture_format,
      ));
      if self.trace {
        let shown = capture.map(|index| &self.captures[index]);
        eprintln!(
          "shingle-lock: output {name} ({}) shows {}",
          output.name.as_deref().unwrap_or("unnamed"),
          shown.map_or("the colour".to_owned(), |capture| format!(
            "the capture of output {} ({})",
            capture.global,
            capture.name.as_deref().unwrap_or("unnamed")
          ))
        );
      }
      output.layout = Some(Layout::new((width, height), options));
      output.minimal_layout =
        Some(minimal::Layout::new((width, height), options.font_size));
      output.clock = None;
      output.input = None;
      output.minimal_input = None;
      for buffer in &mut output.buffers {
        buffer.holds = None;
      }
    }
    // DMA-BUFs when the compositor and this GPU share them, else wl_shm.
    if output.buffers.is_empty() && output.slots.is_empty() {
      if let Some((allocator, dmabuf)) = &self.zero_copy {
        let mut slots = Vec::new();
        for _ in 0..zero_copy::BUFFERS {
          let id = DMABUF_ID | output.next_buffer_id;
          output.next_buffer_id = output.next_buffer_id.wrapping_add(1);
          let data = BufferData { output: name, id };
          match allocator.buffer(&self.gpu, dmabuf, &qh, (width, height), data)
          {
            Ok(slot) => slots.push(slot),
            Err(error) => {
              eprintln!(
                "shingle-lock: warning: DMA-BUF frames failed on output \
                 {name}: {error}; drawing through wl_shm"
              );
              slots.clear();
              break;
            }
          }
        }
        output.slots = slots;
      }
      if output.slots.is_empty() {
        let format = match output.target.as_ref().map(Target::format) {
          Some(gpu::FORMAT_10) => wl_shm::Format::Xbgr2101010,
          _ => wl_shm::Format::Xrgb8888,
        };
        for _ in 0..2 {
          let id = output.next_buffer_id;
          output.next_buffer_id = output.next_buffer_id.wrapping_add(1);
          output.buffers.push(
            ShmBuffer::new(&self.shm, &qh, name, id, (width, height), format)
              .map_err(|error| {
              format!("shared-memory allocation failed: {error}")
            })?,
          );
        }
      }
    }
    let chosen = if output.slots.is_empty() {
      output
        .buffers
        .iter()
        .position(|buffer| !buffer.busy)
        .map(Chosen::Shm)
    } else {
      output
        .slots
        .iter()
        .position(|slot| !slot.busy)
        .map(Chosen::Dmabuf)
    };
    let Some(chosen) = chosen else {
      return Ok(());
    };
    let layout = output.layout.expect("made with the target");
    let minimal_layout = output.minimal_layout.expect("made with the target");
    let target = output.target.as_mut().expect("made above");
    if output.clock != clock {
      let raster = clock.as_ref().and_then(|(time, date)| match style {
        LockStyle::Minimal => minimal::rasterize_clock(
          time,
          date,
          &minimal_layout,
          &self.fonts.clock,
          &self.fonts.text,
        ),
        LockStyle::Swaylock => {
          let color =
            options.state_colors(shingle_config::InputState::Input).text;
          ui::rasterize_clock(time, date, &layout, color, &self.fonts.text)
        }
      });
      target.set_clock(&self.gpu, raster.as_ref());
      output.clock_size = raster
        .as_ref()
        .map_or((0, 0), |raster| (raster.width, raster.height));
      output.clock = clock;
    }
    let (clock_placement, input_placement) = match style {
      LockStyle::Swaylock => {
        if opacity > 0.0 && output.input.as_ref() != Some(&look) {
          let raster =
            ui::rasterize_input(&layout, options, &look, &self.fonts.text);
          target.set_input(&self.gpu, Some(&raster));
          if self.trace {
            eprintln!(
              "shingle-lock: input of output {name}: {:?}, caps lock {}, \
               layout label {:?}, caps lock label {:?}",
              look.state,
              if look.caps_lock { "on" } else { "off" },
              look.labels.layout,
              look.labels.caps_lock,
            );
          }
          output.input = Some(look);
        }
        // Centred; the image is as wide as its widest line.
        let clock_at = output.clock.as_ref().map(|_| {
          Placement::at(
            (
              (layout.size.0 as i32 - output.clock_size.0 as i32) / 2,
              layout.clock_top,
            ),
            1.0,
          )
        });
        let input_at = (opacity > 0.0)
          .then(|| Placement::at((layout.input.0, layout.input.1), opacity));
        (clock_at, input_at)
      }
      LockStyle::Minimal => {
        let (clock_at, input_at, redrawn) = minimal::update_overlays(
          &self.gpu,
          target,
          &minimal_layout,
          &scene,
          labels,
          &self.fonts.text,
          &mut output.minimal_input,
          output.clock.as_ref().map(|_| output.clock_size),
        );
        if redrawn
          && self.trace
          && let Some((shown, _, _)) = &output.minimal_input
        {
          eprintln!(
            "shingle-lock: minimal input of output {name}: {} dots, \
             labels {:?}",
            shown.dots.len(),
            shown.labels
          );
        }
        (clock_at, input_at)
      }
    };
    let drifts = self.settings.drift.enabled
      && self.settings.drift.amplitude > 0.0
      && output.capture.is_some();
    let frame = Frame {
      drift: if drifts {
        Drift::at(drift_seconds, &self.settings.drift, (width, height))
      } else {
        Drift::STILL
      },
      vignette: options.vignette,
      color: options.color,
      dim: match style {
        LockStyle::Minimal => MINIMAL_DIM,
        LockStyle::Swaylock => 1.0,
      },
      // The grain moves with the background, and stands with it.
      seed: match (drifts, continuous) {
        (false, _) => 0,
        (true, true) => (output.frames as u32).wrapping_add(1),
        (true, false) => step as u32,
      },
      clock: clock_placement,
      input: input_placement,
    };
    let failed =
      |error: String| format!("GPU rendering failed on output {name}: {error}");
    let (drawn, wl_buffer) = match chosen {
      Chosen::Shm(index) => {
        let buffer = &mut output.buffers[index];
        let stride = buffer.stride();
        let holds = buffer.holds;
        let bytes = buffer
          .bytes_mut()
          .map_err(|error| format!("the frame's buffer: {error}"))?;
        let drawn = target
          .draw(&self.gpu, &frame, bytes, stride, holds)
          .map_err(failed)?;
        buffer.holds = Some(drawn.serial);
        buffer.busy = true;
        (drawn, buffer.buffer.clone())
      }
      Chosen::Dmabuf(index) => {
        let slot = &mut output.slots[index];
        let drawn = target
          .draw_to(&self.gpu, &frame, &slot.texture)
          .map_err(failed)?;
        slot.holds = Some(drawn.serial);
        slot.busy = true;
        (drawn, slot.buffer.clone())
      }
    };
    let (w, h) = (
      i32::try_from(width).map_err(|_| "the output is too wide".to_owned())?,
      i32::try_from(height).map_err(|_| "the output is too tall".to_owned())?,
    );
    let damage = if output.committed {
      drawn.damage
    } else {
      let region = self.compositor.create_region(&qh, ());
      region.add(0, 0, w, h);
      surface.set_opaque_region(Some(&region));
      region.destroy();
      Rect::full((width, height))
    };
    surface.attach(Some(&wl_buffer), 0, 0);
    if !damage.is_empty() {
      surface.damage_buffer(
        damage.x as i32,
        damage.y as i32,
        damage.width as i32,
        damage.height as i32,
      );
    }
    surface.frame(&qh, name);
    surface.commit();
    output.frame_pending = true;
    output.committed = true;
    output.dirty = false;
    output.drawn_step = Some(step);
    output.faded = left;
    output.frames += 1;
    if self.trace {
      eprintln!(
        "shingle-lock: frame {} of output {} at {} ms, read back {}x{}",
        output.frames,
        output.global,
        elapsed_ms(self.started),
        drawn.copied.width,
        drawn.copied.height,
      );
      if style == LockStyle::Minimal {
        eprintln!(
          "shingle-lock: look of output {}: clock {:.2} input {:.2} \
           rise {:.2} shake {:.2} outline {:.2} tint {:.2} dots {}",
          output.global,
          scene.clock_opacity,
          scene.input_opacity,
          scene.input_below,
          scene.shake,
          scene.outline_alpha,
          scene.tint,
          scene.dots.len(),
        );
      }
    }
    Ok(())
  }

  fn maybe_report_ready(&mut self) {
    if self.ready_reported
      || !self.locked
      || !self
        .outputs
        .values()
        .any(|output| output.surfaces.is_some())
      || !self
        .outputs
        .values()
        .filter(|output| output.surfaces.is_some())
        .all(|output| output.committed)
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
    if !self.active()
      || !self.locked
      || self.auth_running
      || self.leaving.is_some()
    {
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
          let chars = self.password.chars().count();
          self.indicator.deleted(chars, now);
          self.scene.deleted(chars, now);
        } else {
          self.indicator.cleared(now);
          self.scene.cleared(now);
        }
      }
      xkb::keysyms::KEY_Escape | xkb::keysyms::KEY_Delete => {
        self.password.zeroize();
        self.indicator.cleared(now);
        self.scene.cleared(now);
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
          let chars = self.password.chars().count();
          self.indicator.typed(chars, now);
          self.scene.typed(chars, now);
        } else {
          self.indicator.poked(now);
          self.scene.poked(now);
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
      self.scene.poked(now);
      self.mark_dirty();
      return;
    }
    self.auth_running = true;
    self.indicator.verifying(now);
    self.scene.verifying(now);
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
        eprintln!(
          "shingle-lock: PAM authentication and account validation succeeded"
        );
        if self.settings.style == LockStyle::Minimal {
          // The input fades out over the blur; the unlock follows.
          let now = Instant::now();
          self.leaving = Some(now);
          self.scene.accepted(now);
          self.mark_dirty();
        } else {
          self.unlock_requested = true;
          self.lock.unlock_and_destroy();
        }
      }
      AuthOutcome::Denied => {
        self.password.zeroize();
        let now = Instant::now();
        self.indicator.denied(now);
        self.scene.denied(now);
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

/// Why the loop must stop after a turn that `dispatched` ended: the
/// compositor's protocol error, which ends the connection, or the
/// loop's own error; `None` to go on.
fn connection_failure(
  connection: &Connection,
  dispatched: Result<(), calloop::Error>,
) -> Option<String> {
  if let Some(error) = connection.protocol_error() {
    return Some(format!(
      "the compositor ended the connection with a protocol error: {error}"
    ));
  }
  dispatched
    .err()
    .map(|error| format!("event loop failed: {error}"))
}

/// Which other global of `outputs`, as (global, `wl_output` name, has a
/// lock surface), already holds the lock surface of the output that
/// global `global` named `name` shows: one of the same name with a lock
/// surface. `None` lets `global` have its own; so does an output without
/// a name, which cannot be told apart.
fn lock_surface_holder(
  outputs: &[(u32, Option<&str>, bool)],
  global: u32,
  name: Option<&str>,
) -> Option<u32> {
  let name = name?;
  outputs
    .iter()
    .find(|(other, other_name, has)| {
      *other != global && *has && *other_name == Some(name)
    })
    .map(|(other, _, _)| *other)
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

/// What to warn about the screenshots when `outputs` outputs got
/// `captured` captures with `screenshot` on or off; `None` when every
/// output was captured.
fn capture_warning(
  screenshot: bool,
  outputs: usize,
  captured: usize,
) -> Option<String> {
  if !screenshot {
    return Some(
      "no screenshot: the config turns screenshot off; the background is \
       the colour"
        .to_owned(),
    );
  }
  if captured == 0 {
    return Some(
      "no screenshot was taken (the reason is above); the background is \
       the colour"
        .to_owned(),
    );
  }
  (captured < outputs).then(|| {
    format!(
      "{} of {outputs} outputs were not captured; they show the capture \
       closest in shape",
      outputs - captured
    )
  })
}

/// The screenshots uploaded and blurred, in the order of their outputs'
/// globals.
fn blur_captures(
  gpu: &Gpu,
  shots: HashMap<u32, Screenshot>,
  blur: Option<(u32, u32)>,
) -> Vec<Capture> {
  let mut shots = shots.into_iter().collect::<Vec<_>>();
  shots.sort_by_key(|(global, _)| *global);
  shots
    .into_iter()
    .map(|(global, shot)| Capture {
      global,
      size: (shot.width, shot.height),
      image: gpu.image(&shot, blur),
      name: shot.name,
    })
    .collect()
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

  /// Notes that `name`, which ran beside the other steps, took `took`.
  fn note(&mut self, name: &'static str, took: Duration) {
    self.steps.push((name, took));
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
  //
  // The captures wait for the compositor to draw a frame of each output;
  // the GPU and the font are opened on another thread meanwhile.
  let mut timing = Timing::new(launched);
  let family = settings.options.font.clone();
  let opening = thread::Builder::new()
    .name("shingle-lock-gpu".to_owned())
    .spawn(move || {
      let start = Instant::now();
      let gpu = Gpu::new();
      let opened = start.elapsed();
      let fonts = font::load(&family);
      (gpu, opened, fonts, start.elapsed() - opened)
    })?;
  let shots = if settings.options.screenshot {
    capture::capture_outputs(&connection, &globals, CAPTURE_TIMEOUT)
  } else {
    HashMap::new()
  };
  timing.mark("capture");
  let (gpu, opened, fonts, loaded) = opening
    .join()
    .map_err(|_| LockError::Message("opening the GPU panicked".to_owned()))?;
  let gpu =
    gpu.map_err(|error| LockError::Message(format!("no GPU: {error}")))?;
  timing.mark("waiting for the GPU");
  timing.note("gpu", opened);
  timing.note("font", loaded);
  let captures = blur_captures(&gpu, shots, settings.options.blur);
  if settings.options.screenshot {
    timing.mark("blur");
    eprintln!("shingle-lock: captured {} output(s)", captures.len());
  }
  let outputs = initial_globals
    .iter()
    .filter(|global| global.interface == "wl_output")
    .count();
  if let Some(warning) =
    capture_warning(settings.options.screenshot, outputs, captures.len())
  {
    eprintln!("shingle-lock: warning: {warning}");
  }
  eprintln!(
    "shingle-lock: {} style, fonts: {}",
    match settings.style {
      LockStyle::Minimal => "minimal",
      LockStyle::Swaylock => "swaylock",
    },
    fonts.description
  );
  // DMA-BUF frames unless SHINGLE_LOCK_SHM asks for wl_shm, which the
  // frame-cost measurements compare.
  let zero_copy = if std::env::var_os("SHINGLE_LOCK_SHM").is_some() {
    Err("SHINGLE_LOCK_SHM is set".to_owned())
  } else {
    zero_copy::query(&connection, &globals, Duration::from_millis(500))
      .and_then(|feedback| zero_copy::Allocator::new(&gpu, &feedback))
      .and_then(|allocator| {
        globals
          .bind::<ZwpLinuxDmabufV1, _, _>(&qh, 4..=4, ())
          .map(|dmabuf| (allocator, dmabuf))
          .map_err(|error| error.to_string())
      })
  };
  let zero_copy = match zero_copy {
    Ok(zero_copy) => {
      eprintln!("shingle-lock: frames go to the compositor as DMA-BUFs");
      Some(zero_copy)
    }
    Err(reason) => {
      eprintln!("shingle-lock: frames are read back into wl_shm: {reason}");
      None
    }
  };
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
  let scene = Scene::new(
    settings.input_timeout,
    settings.options.show_indicator,
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
    zero_copy,
    shm_formats: Vec::new(),
    captures,
    keyboard: KeyboardStatus::default(),
    fonts,
    indicator,
    scene,
    leaving: None,
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
    let dispatched = event_loop.dispatch(timeout, &mut state);
    // A protocol error ends the connection: nothing more will come, and
    // the loop would only spin on it.
    if let Some(failure) = connection_failure(&state.connection, dispatched) {
      state.fail(failure);
      break;
    }
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
        output.slots.clear();
        output.committed = false;
        return;
      }
      if output.size != Some((width, height)) {
        output.size = Some((width, height));
        output.buffers.clear();
        output.slots.clear();
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
        let xkb = xkb::State::new(&keymap);
        let status = KeyboardStatus::of(&xkb);
        if let Some(keyboard) = state.keyboard_mut(*seat_name) {
          keyboard.xkb = Some(xkb);
        } else {
          return;
        }
        state.set_keyboard_status(status);
      }
      wl_keyboard::Event::Modifiers {
        mods_depressed,
        mods_latched,
        mods_locked,
        group,
        ..
      } => {
        let Some(keyboard) = state.keyboard_mut(*seat_name) else {
          return;
        };
        let Some(xkb) = &mut keyboard.xkb else {
          return;
        };
        xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
        let status = KeyboardStatus::of(xkb);
        state.set_keyboard_status(status);
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
    let Some(output) = state.outputs.get_mut(&data.output) else {
      return;
    };
    if let Some(buffer) = output
      .buffers
      .iter_mut()
      .find(|buffer| buffer.id == data.id)
    {
      buffer.busy = false;
    }
    if let Some(slot) = output.slots.iter_mut().find(|slot| {
      slot.buffer.data::<BufferData>().map(|d| d.id) == Some(data.id)
    }) {
      slot.busy = false;
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

impl Dispatch<wl_output::WlOutput, u32> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_output::WlOutput,
    event: wl_output::Event,
    global: &u32,
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    match event {
      wl_output::Event::Name { name } => {
        if let Some(output) = state.outputs.get_mut(global) {
          output.name = Some(name);
        }
      }
      // Its name, if it has one, came before.
      wl_output::Event::Done => state.give_lock_surface(*global),
      _ => {}
    }
  }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
  fn event(
    state: &mut Self,
    _proxy: &wl_shm::WlShm,
    event: wl_shm::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    if let wl_shm::Event::Format {
      format: WEnum::Value(format),
    } = event
    {
      state.shm_formats.push(format);
    }
  }
}

noop_dispatch!(
  wl_compositor::WlCompositor,
  ZwpLinuxDmabufV1,
  ZwpLinuxBufferParamsV1,
  wl_region::WlRegion,
  wl_shm_pool::WlShmPool,
  wl_surface::WlSurface,
  ExtSessionLockManagerV1,
);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_output_shows_its_own_capture_or_the_closest_shape() {
    let captures = [
      (12, Some("HEADLESS-1"), (640, 400)),
      (13, Some("HEADLESS-2"), (640, 400)),
      (14, None, (1000, 1000)),
      (15, None, (2000, 1000)),
    ];
    assert_eq!(pick_capture(&captures, 13, None, (640, 400)), Some(1));
    // Back under a new global: its own by name.
    assert_eq!(
      pick_capture(&captures, 40, Some("HEADLESS-2"), (640, 400)),
      Some(1)
    );
    // New: the closest aspect ratio, the larger, then the first.
    assert_eq!(pick_capture(&captures, 41, None, (300, 290)), Some(2));
    assert_eq!(
      pick_capture(&captures, 41, Some("DP-3"), (3000, 1400)),
      Some(3)
    );
    assert_eq!(pick_capture(&captures, 41, None, (1280, 800)), Some(0));
    assert_eq!(pick_capture(&[], 41, None, (1280, 800)), None);
  }

  #[test]
  fn the_keyboard_status_follows_caps_lock_and_the_group() {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let Some(keymap) = xkb::Keymap::new_from_names(
      &context,
      "evdev",
      "pc105",
      "us,de",
      "",
      None,
      xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
      eprintln!("no xkeyboard-config; skipped");
      return;
    };
    let mut state = xkb::State::new(&keymap);
    let status = KeyboardStatus::of(&state);
    assert_eq!(status.layouts, 2);
    assert!(!status.caps_lock);
    let first = status.layout.clone().unwrap();
    let lock = keymap.mod_get_index(xkb::MOD_NAME_CAPS);
    state.update_mask(0, 0, 1 << lock, 0, 0, 1);
    let status = KeyboardStatus::of(&state);
    assert!(status.caps_lock);
    assert_ne!(status.layout.unwrap(), first);
  }

  /// A binary PPM's width, height and RGB bytes.
  fn read_ppm(bytes: &[u8]) -> Option<(u32, u32, &[u8])> {
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
      while bytes.get(at)?.is_ascii_whitespace() {
        at += 1;
      }
      let start = at;
      while !bytes.get(at)?.is_ascii_whitespace() {
        at += 1;
      }
      fields.push(std::str::from_utf8(&bytes[start..at]).ok()?);
    }
    let (width, height) = (fields[1].parse().ok()?, fields[2].parse().ok()?);
    (fields[0] == "P6").then_some((width, height, &bytes[at + 1..]))
  }

  /// The minimal style in each state at 1920x1080, over the PPM that
  /// `SHINGLE_LOCK_PREVIEW_BACKGROUND` names, blurred as a screenshot
  /// is, written as PPMs into `SHINGLE_LOCK_PREVIEW_DIR`, through the
  /// same layout, images, placements and GPU target as the lock's
  /// frames: `cargo test -p shingle-lock -- --ignored preview
  /// --nocapture`.
  #[test]
  #[ignore = "writes images for a person to look at"]
  fn preview_states() {
    let (Some(background), Some(directory)) = (
      std::env::var_os("SHINGLE_LOCK_PREVIEW_BACKGROUND"),
      std::env::var_os("SHINGLE_LOCK_PREVIEW_DIR"),
    ) else {
      eprintln!("no SHINGLE_LOCK_PREVIEW_BACKGROUND or _DIR; skipped");
      return;
    };
    let Ok(gpu) = Gpu::new() else {
      eprintln!("no GPU; skipped");
      return;
    };
    let bytes = std::fs::read(background).unwrap();
    let (width, height, rgb) = read_ppm(&bytes).unwrap();
    let size = (width, height);
    let shot = Screenshot {
      name: None,
      width,
      height,
      bgra: rgb
        .chunks_exact(3)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect(),
    };
    let options = shingle_config::LockOptions::default();
    let image = gpu.image(&shot, options.blur);
    let fonts = font::load(&options.font);
    eprintln!("fonts: {}", fonts.description);
    let layout = minimal::Layout::new(size, None);
    let start = Instant::now();
    let at = |millis: u64| start + Duration::from_millis(millis);
    let typed = |scene: &mut Scene, count: usize, from: u64| {
      for index in 1..=count {
        scene.typed(index, at(from + 90 * index as u64));
      }
    };
    let mut states: Vec<(&str, Scene, Instant, Vec<String>)> = Vec::new();
    let timeout = Duration::from_secs(5);
    let rest = Scene::new(timeout, true, start);
    states.push(("rest", rest.clone(), at(1000), Vec::new()));
    let mut typing = rest.clone();
    typed(&mut typing, 7, 1000);
    states.push(("typing", typing.clone(), at(2600), Vec::new()));
    let mut crossing = rest.clone();
    crossing.typed(1, at(1000));
    states.push(("transition", crossing, at(1070), Vec::new()));
    let mut verifying = typing.clone();
    verifying.verifying(at(2700));
    states.push(("verifying", verifying.clone(), at(3500), Vec::new()));
    let mut wrong = verifying.clone();
    wrong.denied(at(4000));
    states.push(("wrong", wrong, at(4030), Vec::new()));
    let caps = vec!["Caps Lock".to_owned(), "English (US)".to_owned()];
    states.push(("caps-lock", typing.clone(), at(2600), caps));
    let mut leaving = verifying;
    leaving.accepted(at(4000));
    states.push(("success-fade", leaving, at(4120), Vec::new()));
    let config = shingle_config::DriftConfig::default();
    let clock =
      minimal::rasterize_clock("21:47", "", &layout, &fonts.clock, &fonts.text);
    for (name, scene, now, labels) in states {
      let mut target = Target::new(&gpu, size, Some(&image), gpu::FORMAT);
      target.set_clock(&gpu, clock.as_ref());
      let look = scene.look(now);
      let mut shown = None;
      let (clock_at, input_at, _) = minimal::update_overlays(
        &gpu,
        &mut target,
        &layout,
        &look,
        labels,
        &fonts.text,
        &mut shown,
        clock.as_ref().map(|raster| (raster.width, raster.height)),
      );
      let frame = Frame {
        drift: Drift::at(3.0, &config, size),
        vignette: options.vignette,
        color: options.color,
        dim: MINIMAL_DIM,
        seed: 1,
        clock: clock_at,
        input: input_at,
      };
      let stride = width as usize * 4;
      let mut out = vec![0; stride * height as usize];
      target.draw(&gpu, &frame, &mut out, stride, None).unwrap();
      let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
      for pixel in out.chunks_exact(4) {
        ppm.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
      }
      let path = std::path::Path::new(&directory).join(format!("{name}.ppm"));
      std::fs::write(&path, ppm).unwrap();
      eprintln!("wrote {}", path.display());
    }
  }

  #[test]
  fn ppm_headers_are_read() {
    let ppm = b"P6\n2 1\n255\n\x01\x02\x03\x04\x05\x06";
    assert_eq!(read_ppm(ppm), Some((2, 1, &ppm[11..])));
    assert_eq!(read_ppm(b"P5\n2 1\n255\n\x01\x02"), None);
    assert_eq!(read_ppm(b"P6\n2"), None);
  }

  /// A client of nothing but a registry, for the protocol error test.
  struct Bare;

  impl Dispatch<wl_registry::WlRegistry, ()> for Bare {
    fn event(
      _state: &mut Self,
      _proxy: &wl_registry::WlRegistry,
      _event: wl_registry::Event,
      _data: &(),
      _conn: &Connection,
      _qh: &QueueHandle<Self>,
    ) {
    }
  }

  /// A `wl_display.error` event for `object` with `code` and `message`,
  /// as a compositor writes it.
  fn display_error(object: u32, code: u32, message: &str) -> Vec<u8> {
    let mut text = message.as_bytes().to_vec();
    text.push(0);
    let length = text.len() as u32;
    while !text.len().is_multiple_of(4) {
      text.push(0);
    }
    let size = 8 + 4 + 4 + 4 + text.len() as u32;
    let mut bytes = Vec::new();
    for word in [1, size << 16, object, code, length] {
      bytes.extend_from_slice(&u32::to_ne_bytes(word));
    }
    bytes.extend_from_slice(&text);
    bytes
  }

  #[test]
  fn a_protocol_error_stops_the_loop_at_once() {
    use std::io::{Read, Write};
    let (client, mut server) =
      std::os::unix::net::UnixStream::pair().expect("a socket pair");
    let connection = Connection::from_socket(client).expect("a connection");
    let queue = connection.new_event_queue::<Bare>();
    let _registry = connection.display().get_registry(&queue.handle(), ());
    connection.flush().expect("the request is sent");
    let mut request = [0_u8; 64];
    let read = server.read(&mut request).expect("the request arrives");
    assert!(read >= 12);
    // The registry, object 2, is what the compositor refuses.
    server
      .write_all(&display_error(2, 3, "invented refusal"))
      .expect("the error is sent");
    let mut event_loop = EventLoop::<Bare>::try_new().expect("a loop");
    WaylandSource::new(connection.clone(), queue)
      .insert(event_loop.handle())
      .expect("the source is inserted");
    let mut turns = 0;
    let failure = loop {
      turns += 1;
      assert!(turns <= 5, "the loop went on after the protocol error");
      let dispatched =
        event_loop.dispatch(Some(Duration::from_millis(100)), &mut Bare);
      if let Some(failure) = connection_failure(&connection, dispatched) {
        break failure;
      }
    };
    // The error's code and object; the backend adds the message only
    // when it read it with the event.
    assert!(failure.contains("protocol error"), "{failure}");
    assert!(failure.contains("error 3 on object wl_registry@2"), "{failure}");
    // Nothing wrong, nothing to stop for.
    let (fine, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let fine = Connection::from_socket(fine).unwrap();
    assert_eq!(connection_failure(&fine, Ok(())), None);
  }

  #[test]
  fn one_lock_surface_per_output_whatever_its_globals() {
    let outputs = [
      (12, Some("HEADLESS-1"), true),
      (13, Some("HEADLESS-2"), true),
      (60, Some("HEADLESS-1"), false),
      (61, None, false),
    ];
    // A second global of HEADLESS-1 waits for the first to go.
    assert_eq!(
      lock_surface_holder(&outputs, 60, Some("HEADLESS-1")),
      Some(12)
    );
    // The holder itself, another output and an unnamed one go ahead.
    assert_eq!(lock_surface_holder(&outputs, 12, Some("HEADLESS-1")), None);
    assert_eq!(lock_surface_holder(&outputs, 14, Some("DP-1")), None);
    assert_eq!(lock_surface_holder(&outputs, 61, None), None);
    // Once the first is gone, the second has no holder.
    let gone = [(60, Some("HEADLESS-1"), false)];
    assert_eq!(lock_surface_holder(&gone, 60, Some("HEADLESS-1")), None);
  }

  #[test]
  fn missing_captures_are_warned_about() {
    assert!(capture_warning(false, 2, 0).unwrap().contains("turns"));
    assert!(
      capture_warning(true, 2, 0)
        .unwrap()
        .contains("no screenshot")
    );
    assert!(capture_warning(true, 2, 1).unwrap().starts_with("1 of 2"));
    assert_eq!(capture_warning(true, 2, 2), None);
  }

  #[test]
  fn the_clock_wakes_at_the_next_whole_second() {
    let at = |millis| SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
    assert_eq!(until_next_second(at(10_250)), Duration::from_millis(750));
    assert_eq!(until_next_second(at(10_000)), Duration::from_secs(1));
  }
}
