//! The offscreen backend: the frontend drawing with the GPU into an
//! offscreen target, without a display or input devices.
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
//! start without a GPU leaves no clients behind. Frames are drawn when
//! the frontend asks for one, at most once per refresh interval of the
//! configured output, at its physical size. A frame counts as presented
//! when the GPU has finished it: clients get their frame callbacks and
//! presentation feedback with that time and a frame counter. Screencopy
//! shows the drawn frames. Input comes only from STIPC.

use std::{
  error::Error,
  ffi::OsString,
  path::{Path, PathBuf},
  time::{Duration, Instant},
};

use shingle::{PresentInfo, PresentationFlags};
use shingle_config::ShingleConfig;
use shingle_frontend::{Frontend, host::Host, render_target::RenderTarget};
use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, OffscreenTarget, RenderError,
  RenderState, RenderTile, TextureTransform,
};

use crate::headless::{frame_interval, register_stipc_devices};

/// How long a frame may take on the GPU before it is given up.
const GPU_WAIT: Duration = Duration::from_secs(1);

/// The longest wait between turns while no frame is wanted, for config
/// reload, drag and gesture timers and child reaping
/// ([`Frontend::next_deadline`]).
const TICK: Duration = Duration::from_millis(16);

/// Runs Shingle offscreen with `config` until SIGINT, SIGTERM or a quit
/// request, then saves the session. The arguments are those of
/// `shingle_nested::run`.
pub fn run(
  config: ShingleConfig,
  config_path: Option<PathBuf>,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
) -> Result<(), Box<dyn Error>> {
  let gpu = open_gpu()?;
  eprintln!(
    "shingle: rendering offscreen on {}",
    gpu.adapter().get_info().name
  );
  eprintln!("shingle: {}", gpu.dmabuf_summary());
  let mut frontend = Frontend::new(config, spawn, config_path, ipc_path)?;
  register_stipc_devices(frontend.compositor_mut());
  frontend.attach_gpu(&gpu);
  frontend.start_clients()?;
  let mut output = OffscreenOutput::new(gpu);
  let mut host = OffscreenHost {
    redraw: true,
    exit: false,
  };
  let interval =
    frame_interval(frontend.compositor().output_info().refresh_mhz);
  let mut pacer = Pacer::new(interval, Instant::now());
  let mut errors = ErrorLog::default();
  let mut sequence = 0;
  loop {
    frontend.process(&mut host);
    if host.exit {
      break;
    }
    let now = Instant::now();
    if host.redraw && pacer.ready(now) {
      host.redraw = false;
      output.resize(frontend.compositor().output_physical_size());
      match frontend.render(&mut host, &mut output) {
        Ok(()) => errors.clear(),
        Err(error) => {
          errors.log(&error.to_string());
        }
      }
      if output.take_presented() {
        sequence += 1;
        let compositor = frontend.compositor_mut();
        // The offscreen target is the first output's until MO-5.
        let primary = compositor.primary_output();
        compositor.frame_presented(
          &primary,
          PresentInfo {
            time: shingle::monotonic_time(),
            refresh_ns: refresh_ns(interval),
            seq: sequence,
            flags: PresentationFlags::empty(),
          },
        );
      }
    }
    let timeout =
      pacer.wait(Instant::now(), host.redraw, frontend.next_deadline());
    frontend.compositor_mut().poll(Some(timeout))?;
  }
  frontend.shutdown();
  Ok(())
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

/// What the frontend asks of the offscreen backend: frames and an exit.
/// There is no pointer to warp, no VT and no device to configure.
#[derive(Debug, Default)]
struct OffscreenHost {
  /// Whether a frame was asked for and not drawn yet.
  redraw: bool,
  /// Whether the frontend asked to exit.
  exit: bool,
}

impl Host for OffscreenHost {
  fn request_redraw(&mut self) {
    self.redraw = true;
  }

  fn warp_pointer(&mut self, _x: f64, _y: f64) {}

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

/// The frontend's render target: an offscreen texture of the output's
/// physical size, whose frames nobody reads unless screencopy asks.
struct OffscreenOutput {
  gpu: GpuContext,
  state: RenderState,
  target: Option<OffscreenTarget>,
  /// Whether a frame finished on the GPU since the last
  /// [`OffscreenOutput::take_presented`].
  presented: bool,
}

impl OffscreenOutput {
  fn new(gpu: GpuContext) -> Self {
    Self {
      gpu,
      state: RenderState::default(),
      target: None,
      presented: false,
    }
  }

  /// Makes the target `size` pixels, reallocating it only when the size
  /// changed. Zero sizes are ignored.
  fn resize(&mut self, (width, height): (u32, u32)) {
    if width == 0 || height == 0 || self.size() == Some((width, height)) {
      return;
    }
    match OffscreenTarget::new(&self.gpu, width, height) {
      Ok(target) => self.target = Some(target),
      Err(error) => {
        eprintln!("shingle: no {width}x{height} offscreen target: {error}");
      }
    }
  }

  /// The target's size, if it has one.
  fn size(&self) -> Option<(u32, u32)> {
    self.target.as_ref().map(OffscreenTarget::size)
  }

  /// Whether a frame was presented since the last call.
  fn take_presented(&mut self) -> bool {
    std::mem::take(&mut self.presented)
  }
}

impl RenderTarget for OffscreenOutput {
  fn set_scale_factor(&mut self, scale: f64) {
    self.state.set_scale_factor(scale);
  }

  fn set_output_transform(&mut self, transform: TextureTransform) {
    self.state.set_output_transform(transform);
  }

  fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    self.state.set_nearest_filter(nearest)
  }

  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    self.state.set_zoom(factor, origin_x, origin_y);
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
    self
      .gpu
      .create_texture(width, height, rgba, self.state.texture_filter())
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
    let Some(target) = &self.target else {
      return Err(RenderError::EmptyTexture);
    };
    let submission = target.render(&self.gpu, &self.state, tiles, clear);
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
    self.presented = true;
    Ok(false)
  }

  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    shingle_render::capture_frame(&self.gpu, &self.state, size, tiles, clear)
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
  fn the_host_records_redraws_and_exits_and_cannot_switch_vt() {
    let mut host = OffscreenHost::default();
    host.request_redraw();
    host.warp_pointer(1.0, 2.0);
    assert!(!host.switch_vt(2));
    host.exit();
    assert!(host.redraw && host.exit);
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
    let mut output = OffscreenOutput::new(gpu);
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
    assert!(!output.take_presented());
    output.resize((8, 4));
    assert_eq!(output.size(), Some((8, 4)));
    output.resize((0, 4));
    assert_eq!(output.size(), Some((8, 4)), "a zero size is ignored");
    output.set_scale_factor(2.0);
    assert_eq!(
      output.render(&tiles, [0.0, 0.0, 1.0, 1.0]).ok(),
      Some(false)
    );
    assert!(output.take_presented());
    assert!(!output.take_presented(), "taken once");
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
