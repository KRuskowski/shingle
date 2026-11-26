//! The offscreen backend: the frontend drawing with the GPU into an
//! offscreen target per output, without a display or input devices.
//!
//! It is what wlroots' headless backend is to Wayfire, a compositor that
//! renders although nothing shows its frames: the outer compositor of the
//! GPU harness (tools/gpu_test/run.py), where nested Shingle and GPU
//! clients run without touching the host's display
//! (docs/design/dmabuf.md, section 10, layer 2). `--headless` stays the
//! CPU-only backend.
//!
//! The device is wgpu's high-performance adapter, opened for DMA-BUF
//! import where it allows it, before the Wayland socket exists, so a
//! start without a GPU leaves no clients behind. `--outputs N` starts it
//! with `HEADLESS-1` to `HEADLESS-N`, each sized, placed and paced by its
//! `[output:HEADLESS-N]` entry (docs/design/multi-output.md, section 7).
//! Each output is drawn when the frontend asks for a frame of it, at most
//! once per refresh interval of that output, at its physical size, into a
//! target of its own on the one device. A frame counts as presented when
//! the GPU has finished it: clients on that output get their frame
//! callbacks and presentation feedback with that time and the output's
//! own frame counter. Screencopy shows the drawn frames. Input comes only
//! from STIPC.

use std::{
  collections::{BTreeMap, HashSet},
  error::Error,
  ffi::OsString,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use shingle::{
  Compositor, OutputHandle, OutputKind, PresentInfo, PresentationFlags,
};
use shingle_config::ShingleConfig;
use shingle_frontend::{Frontend, host::Host, render_target::RenderTarget};
use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, OffscreenTarget, RenderError,
  RenderState, RenderTile, TextureTransform,
};

use crate::headless::{add_outputs, frame_interval, register_stipc_devices};

/// How long a frame may take on the GPU before it is given up.
const GPU_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between turns while no frame is wanted, for config
/// reload, drag and gesture timers and child reaping
/// ([`Frontend::next_deadline`]).
const TICK: Duration = Duration::from_millis(16);

/// Runs Shingle offscreen with `config` and `outputs` outputs until
/// SIGINT, SIGTERM or a quit request, then saves the session. The other
/// arguments are those of `shingle_nested::run`.
pub fn run(
  config: ShingleConfig,
  config_path: Option<PathBuf>,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
  outputs: u32,
) -> Result<(), Box<dyn Error>> {
  let gpu = open_gpu()?;
  eprintln!(
    "shingle: rendering offscreen on {}",
    gpu.adapter().get_info().name
  );
  eprintln!("shingle: {}", gpu.dmabuf_summary());
  let mut frontend = Frontend::new(config, spawn, config_path, ipc_path)?;
  add_outputs(frontend.compositor_mut(), outputs)?;
  register_stipc_devices(frontend.compositor_mut());
  frontend.attach_gpu(&gpu);
  frontend.start_clients()?;
  let mut display = Outputs::new(gpu);
  let mut host = OffscreenHost::default();
  display.sync(frontend.compositor(), Instant::now());
  // Every output draws its first frame at once.
  host.redraw.extend(display.handles());
  let mut errors = ErrorLog::default();
  loop {
    frontend.process(&mut host);
    if host.exit {
      break;
    }
    let now = Instant::now();
    display.sync(frontend.compositor(), now);
    for output in display.ready(now, &mut host.redraw) {
      draw(&mut frontend, &mut host, &mut display, &output, &mut errors);
    }
    let timeout =
      display.wait(Instant::now(), &host.redraw, frontend.next_deadline());
    frontend.compositor_mut().poll(Some(timeout))?;
  }
  frontend.shutdown();
  Ok(())
}

/// Draws one frame of `output` and, once the GPU has finished it, reports
/// it presented on that output with the output's own frame counter.
fn draw(
  frontend: &mut Frontend,
  host: &mut OffscreenHost,
  display: &mut Outputs,
  output: &OutputHandle,
  errors: &mut ErrorLog,
) {
  let size = frontend.compositor().physical_size_of(output);
  let Some(mut target) = display.target(output) else {
    return;
  };
  if let Some(size) = size {
    target.resize(size);
  }
  match frontend.render(host, output, &mut target) {
    Ok(()) => errors.clear(),
    Err(error) => {
      errors.log(&format!("{}: {error}", output.name()));
    }
  }
  let Some(presented) = target.take_presented() else {
    return;
  };
  frontend.compositor_mut().frame_presented(
    output,
    PresentInfo {
      time: shingle::monotonic_time(),
      refresh_ns: refresh_ns(presented.interval),
      seq: presented.sequence,
      flags: PresentationFlags::empty(),
    },
  );
}

/// Opens wgpu's high-performance adapter for DMA-BUF import.
fn open_gpu() -> Result<GpuContext, RenderError> {
  pollster::block_on(GpuContext::new(
    GpuContext::create_instance(),
    GpuOptions {
      dmabuf: true,
      ..GpuOptions::default()
    },
  ))
}

/// `interval` in nanoseconds, as `wp_presentation_feedback` reports the
/// refresh.
fn refresh_ns(interval: Duration) -> u32 {
  u32::try_from(interval.as_nanos()).unwrap_or(0)
}

/// What the frontend asks of the offscreen backend: frames of its outputs
/// and an exit. There is no pointer to warp, no VT and no device to
/// configure.
#[derive(Debug, Default)]
struct OffscreenHost {
  /// The outputs a frame was asked for and not drawn yet.
  redraw: HashSet<OutputHandle>,
  /// Whether the frontend asked to exit.
  exit: bool,
}

impl Host for OffscreenHost {
  fn request_redraw(&mut self, output: &OutputHandle) {
    self.redraw.insert(output.clone());
  }

  fn warp_pointer(&mut self, _output: &OutputHandle, _x: f64, _y: f64) {}

  fn switch_vt(&mut self, _vt: u32) -> bool {
    false
  }

  fn input_device_changed(
    &mut self,
    _device: shingle::InputDeviceId,
    _enabled: bool,
  ) {
  }

  fn exit(&mut self) {
    self.exit = true;
  }
}

/// Spaces frames at least one refresh interval apart, on a grid that
/// starts at the first frame, as a display's vblanks would.
#[derive(Clone, Copy, Debug)]
struct Pacer {
  interval: Duration,
  /// The earliest time the next frame may be drawn.
  next: Instant,
}

impl Pacer {
  /// A pacer whose first frame may be drawn at `now`.
  fn new(interval: Duration, now: Instant) -> Self {
    Self {
      interval: interval.max(Duration::from_millis(1)),
      next: now,
    }
  }

  /// Whether a frame may be drawn at `now`. If so, the next one may be
  /// drawn at the first grid point after `now`.
  fn ready(&mut self, now: Instant) -> bool {
    if now < self.next {
      return false;
    }
    let behind = now.duration_since(self.next).as_nanos();
    let steps = behind / self.interval.as_nanos() + 1;
    let steps = u32::try_from(steps).unwrap_or(u32::MAX);
    self.next += self.interval * steps;
    true
  }

  /// How long to wait at `now`: until the next frame when one is wanted,
  /// otherwise at most [`TICK`], and never past `deadline`.
  fn wait(
    &self,
    now: Instant,
    redraw: bool,
    deadline: Option<Instant>,
  ) -> Duration {
    let mut wake = now + TICK;
    if redraw {
      wake = wake.min(self.next);
    }
    if let Some(deadline) = deadline {
      wake = wake.min(deadline);
    }
    wake.saturating_duration_since(now)
  }
}

/// Logs render errors once each, until a frame succeeds again.
#[derive(Debug, Default)]
struct ErrorLog {
  last: Option<String>,
}

impl ErrorLog {
  /// Logs `error` unless it is the one logged last. Returns whether it
  /// was logged.
  fn log(&mut self, error: &str) -> bool {
    if self.last.as_deref() == Some(error) {
      return false;
    }
    eprintln!("shingle: offscreen frame failed: {error}");
    self.last = Some(error.to_owned());
    true
  }

  /// Forgets the last error after a frame that worked.
  fn clear(&mut self) {
    self.last = None;
  }
}

/// One output's frames: an offscreen texture of the output's physical
/// size, whose frames nobody reads unless screencopy asks, its render
/// state, pacer and frame counter.
struct OutputTarget {
  state: RenderState,
  target: Option<OffscreenTarget>,
  /// The output's refresh interval.
  interval: Duration,
  pacer: Pacer,
  /// Frames presented so far.
  sequence: u64,
  /// Whether a frame finished on the GPU since the last
  /// [`OffscreenOutput::take_presented`].
  presented: bool,
}

impl OutputTarget {
  /// A target for an output refreshed every `interval`, whose first frame
  /// may be drawn at `now`.
  fn new(interval: Duration, now: Instant) -> Self {
    Self {
      state: RenderState::default(),
      target: None,
      interval,
      pacer: Pacer::new(interval, now),
      sequence: 0,
      presented: false,
    }
  }
}

/// A presented frame of one output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Presented {
  /// Its number, counted from 1 per output.
  sequence: u64,
  /// The output's refresh interval.
  interval: Duration,
}

/// The GPU and the targets of the outputs the backend draws: every output
/// it added that is lit, by name. Outputs created over IPC are drawn by
/// the frontend for screencopy only.
struct Outputs {
  gpu: GpuContext,
  outputs: BTreeMap<OutputHandle, OutputTarget>,
}

impl Outputs {
  fn new(gpu: GpuContext) -> Self {
    Self {
      gpu,
      outputs: BTreeMap::new(),
    }
  }

  /// Follows the compositor's outputs at `now`: a target for each lit
  /// output the backend added, paced at its refresh rate, and none for
  /// outputs that went, are dark or are turned off.
  fn sync(&mut self, compositor: &Compositor, now: Instant) {
    let lit = compositor
      .outputs()
      .into_iter()
      .filter(|view| {
        view.kind == OutputKind::Backend
          && !view.dark
          && compositor.output_powered(&view.handle)
      })
      .map(|view| (view.handle, frame_interval(view.info.refresh_mhz)))
      .collect::<BTreeMap<_, _>>();
    self.outputs.retain(|output, _| lit.contains_key(output));
    for (output, interval) in lit {
      let target = self
        .outputs
        .entry(output)
        .or_insert_with(|| OutputTarget::new(interval, now));
      if target.interval != interval {
        target.interval = interval;
        target.pacer = Pacer::new(interval, now);
      }
    }
  }

  /// The outputs drawn.
  fn handles(&self) -> Vec<OutputHandle> {
    self.outputs.keys().cloned().collect()
  }

  /// The outputs among `wanted` whose next frame may be drawn at `now`,
  /// taken out of `wanted`. Wanted outputs that are not drawn are
  /// forgotten.
  fn ready(
    &mut self,
    now: Instant,
    wanted: &mut HashSet<OutputHandle>,
  ) -> Vec<OutputHandle> {
    ready(&mut self.outputs, now, wanted)
  }

  /// How long to wait at `now`: until the next frame of an output in
  /// `wanted`, otherwise at most [`TICK`], and never past `deadline`.
  fn wait(
    &self,
    now: Instant,
    wanted: &HashSet<OutputHandle>,
    deadline: Option<Instant>,
  ) -> Duration {
    self
      .outputs
      .iter()
      .map(|(output, target)| {
        target.pacer.wait(now, wanted.contains(output), deadline)
      })
      .min()
      .unwrap_or_else(|| Pacer::new(TICK, now).wait(now, false, deadline))
  }

  /// The render target of `output`, if it is drawn.
  fn target(&mut self, output: &OutputHandle) -> Option<OffscreenOutput<'_>> {
    let target = self.outputs.get_mut(output)?;
    Some(OffscreenOutput {
      gpu: &mut self.gpu,
      output: target,
    })
  }
}

/// The outputs among `wanted` whose next frame may be drawn at `now`,
/// for [`Outputs::ready`]; each output's pacer moves to its next frame.
fn ready(
  outputs: &mut BTreeMap<OutputHandle, OutputTarget>,
  now: Instant,
  wanted: &mut HashSet<OutputHandle>,
) -> Vec<OutputHandle> {
  wanted.retain(|output| outputs.contains_key(output));
  let mut ready = Vec::new();
  for (output, target) in outputs {
    if wanted.contains(output) && target.pacer.ready(now) {
      wanted.remove(output);
      ready.push(output.clone());
    }
  }
  ready
}

/// The frontend's render target for one output: that output's target on
/// the shared GPU.
struct OffscreenOutput<'a> {
  gpu: &'a mut GpuContext,
  output: &'a mut OutputTarget,
}

impl OffscreenOutput<'_> {
  /// Makes the target `size` pixels, reallocating it only when the size
  /// changed. Zero sizes are ignored.
  fn resize(&mut self, (width, height): (u32, u32)) {
    if width == 0 || height == 0 || self.size() == Some((width, height)) {
      return;
    }
    match OffscreenTarget::new(self.gpu, width, height) {
      Ok(target) => self.output.target = Some(target),
      Err(error) => {
        eprintln!("shingle: no {width}x{height} offscreen target: {error}");
      }
    }
  }

  /// The target's size, if it has one.
  fn size(&self) -> Option<(u32, u32)> {
    self.output.target.as_ref().map(OffscreenTarget::size)
  }

  /// The frame presented since the last call, if one was.
  fn take_presented(&mut self) -> Option<Presented> {
    if !std::mem::take(&mut self.output.presented) {
      return None;
    }
    self.output.sequence += 1;
    Some(Presented {
      sequence: self.output.sequence,
      interval: self.output.interval,
    })
  }
}

impl RenderTarget for OffscreenOutput<'_> {
  fn set_scale_factor(&mut self, scale: f64) {
    self.output.state.set_scale_factor(scale);
  }

  fn set_output_transform(&mut self, transform: TextureTransform) {
    self.output.state.set_output_transform(transform);
  }

  fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    self.output.state.set_nearest_filter(nearest)
  }

  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    self.output.state.set_zoom(factor, origin_x, origin_y);
  }

  fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    self.gpu.set_gamma_ramp(ramp)
  }

  fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    self.gpu.set_inversion(inverted, preserve_hue);
  }

  // Nothing is shown, so nothing can tear.
  fn set_tearing(&mut self, _tearing: bool) {}

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
      self.output.state.texture_filter(),
    )
  }

  fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    self.gpu.update_texture(texture, rgba)
  }

  /// Draws the frame and waits up to [`GPU_WAIT`] for the GPU to finish
  /// it. Returns false, because the backend reports the presentation
  /// itself, with its frame counter.
  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    let Some(target) = &self.output.target else {
      return Err(RenderError::EmptyTexture);
    };
    let submission = target.render(self.gpu, &self.output.state, tiles, clear);
    self
      .gpu
      .device()
      .poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: Some(GPU_WAIT),
      })
      .map_err(|error| match error {
        wgpu::PollError::Timeout => RenderError::Timeout(GPU_WAIT),
        error => RenderError::Readback(error.to_string()),
      })?;
    self.output.presented = true;
    Ok(false)
  }

  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    shingle_render::capture_frame(
      self.gpu,
      &self.output.state,
      size,
      tiles,
      clear,
    )
  }
}

#[cfg(test)]
mod tests {
  use shingle_render::RenderRect;

  use super::*;

  const FRAME: Duration = Duration::from_millis(16);

  #[test]
  fn frames_are_spaced_by_the_refresh_interval() {
    let start = Instant::now();
    let mut pacer = Pacer::new(FRAME, start);
    assert!(pacer.ready(start), "the first frame is drawn at once");
    assert!(!pacer.ready(start + Duration::from_millis(5)));
    assert!(pacer.ready(start + FRAME));
    assert!(!pacer.ready(start + FRAME + Duration::from_millis(15)));
    assert!(pacer.ready(start + FRAME * 2));
  }

  #[test]
  fn after_an_idle_spell_the_next_frame_is_drawn_at_once_on_the_grid() {
    let start = Instant::now();
    let mut pacer = Pacer::new(FRAME, start);
    assert!(pacer.ready(start));
    let late = start + FRAME * 100 + Duration::from_millis(3);
    assert!(pacer.ready(late), "a commit after a pause draws at once");
    assert!(!pacer.ready(late), "but only one frame");
    assert_eq!(pacer.next, start + FRAME * 101);
  }

  #[test]
  fn waits_end_at_the_next_frame_tick_or_deadline() {
    let start = Instant::now();
    let mut pacer = Pacer::new(FRAME, start);
    assert!(pacer.ready(start));
    let now = start + Duration::from_millis(10);
    assert_eq!(pacer.wait(now, true, None), Duration::from_millis(6));
    assert_eq!(pacer.wait(now, false, None), TICK);
    let deadline = now + Duration::from_millis(2);
    assert_eq!(
      pacer.wait(now, false, Some(deadline)),
      Duration::from_millis(2)
    );
    assert_eq!(pacer.wait(now, true, Some(start)), Duration::ZERO);
  }

  #[test]
  fn a_zero_interval_cannot_spin() {
    let start = Instant::now();
    let mut pacer = Pacer::new(Duration::ZERO, start);
    assert!(pacer.ready(start));
    assert!(!pacer.ready(start));
  }

  #[test]
  fn the_refresh_is_reported_in_nanoseconds() {
    assert_eq!(refresh_ns(frame_interval(60_000)), 16_666_666);
    assert_eq!(refresh_ns(Duration::from_secs(10)), 0, "too long for u32");
  }

  #[test]
  fn the_host_records_redraws_per_output_and_exits_and_cannot_switch_vt() {
    let mut host = OffscreenHost::default();
    let second = OutputHandle::new("HEADLESS-2");
    host.request_redraw(&second);
    host.request_redraw(&second);
    host.warp_pointer(&second, 1.0, 2.0);
    assert!(!host.switch_vt(2));
    host.exit();
    assert_eq!(host.redraw, HashSet::from([second]));
    assert!(host.exit);
  }

  #[test]
  fn each_output_is_paced_on_its_own() {
    let start = Instant::now();
    let fast = OutputHandle::new("HEADLESS-1");
    let slow = OutputHandle::new("HEADLESS-2");
    let mut targets = BTreeMap::new();
    targets.insert(fast.clone(), OutputTarget::new(FRAME, start));
    targets.insert(slow.clone(), OutputTarget::new(FRAME * 2, start));
    let mut wanted = HashSet::from([fast.clone(), slow.clone()]);
    assert_eq!(
      ready(&mut targets, start, &mut wanted),
      [fast.clone(), slow]
    );
    wanted.extend(targets.keys().cloned());
    let one = start + FRAME;
    assert_eq!(
      ready(&mut targets, one, &mut wanted),
      std::slice::from_ref(&fast)
    );
    assert_eq!(wanted.len(), 1, "the slow output still wants its frame");
    wanted.insert(OutputHandle::new("HEADLESS-9"));
    assert!(ready(&mut targets, one, &mut wanted).is_empty());
    assert_eq!(wanted.len(), 1, "outputs not drawn are forgotten");
  }

  #[test]
  fn repeated_errors_are_logged_once() {
    let mut log = ErrorLog::default();
    assert!(log.log("GPU timed out"));
    assert!(!log.log("GPU timed out"));
    assert!(log.log("device lost"));
    log.clear();
    assert!(log.log("device lost"));
  }

  /// A GPU context, or `None` without an adapter.
  fn gpu() -> Option<GpuContext> {
    match open_gpu() {
      Ok(gpu) => Some(gpu),
      Err(RenderError::RequestAdapter(error)) => {
        eprintln!("skipping, no GPU adapter: {error}");
        None
      }
      Err(error) => panic!("GPU context failed: {error}"),
    }
  }

  #[test]
  fn frames_are_drawn_at_the_output_size_and_captured_as_drawn() {
    let Some(gpu) = gpu() else {
      return;
    };
    let mut display = Outputs::new(gpu);
    let handle = OutputHandle::new("HEADLESS-1");
    display
      .outputs
      .insert(handle.clone(), OutputTarget::new(FRAME, Instant::now()));
    assert!(display.target(&OutputHandle::new("HEADLESS-2")).is_none());
    let mut output = display.target(&handle).expect("a target");
    let tiles = [RenderTile::solid(
      RenderRect {
        x: 2,
        y: 1,
        width: 2,
        height: 2,
      },
      [1.0, 0.0, 0.0, 1.0],
    )];
    assert!(output.render(&tiles, [0.0; 4]).is_err(), "no size yet");
    assert_eq!(output.take_presented(), None);
    output.resize((8, 4));
    assert_eq!(output.size(), Some((8, 4)));
    output.resize((0, 4));
    assert_eq!(output.size(), Some((8, 4)), "a zero size is ignored");
    output.set_scale_factor(2.0);
    assert_eq!(
      output.render(&tiles, [0.0, 0.0, 1.0, 1.0]).ok(),
      Some(false)
    );
    assert_eq!(
      output.take_presented(),
      Some(Presented {
        sequence: 1,
        interval: FRAME,
      })
    );
    assert_eq!(output.take_presented(), None, "taken once");
    // The capture is in logical pixels, whatever the scale.
    let rgba = output
      .capture(&tiles, [0.0, 0.0, 1.0, 1.0], (4, 3))
      .expect("the capture is read back");
    let pixel = |x: usize, y: usize| &rgba[(y * 4 + x) * 4..][..4];
    assert_eq!(pixel(0, 0), [0, 0, 255, 255]);
    assert_eq!(pixel(2, 1), [255, 0, 0, 255]);
    assert_eq!(pixel(3, 2), [255, 0, 0, 255]);
    assert_eq!(pixel(1, 2), [0, 0, 255, 255]);
  }
}
