//! The display: one KMS output driven with atomic commits.
//!
//! [`Display`] owns the card ([`KmsDevice`]), the lit output with its mode
//! and [`Scanout`], and the [`FrameScheduler`]. It implements the display
//! parts of docs/design/native-backend.md:
//!
//! - **Startup** ([`Display::prepare`]): atomic support, the probe, the
//!   choice of connector, CRTC and plane, and a `TEST_ONLY` modeset for
//!   each candidate mode until one passes, all before the Wayland socket
//!   exists. [`Display::start`] then makes the real modeset.
//! - **Frames**: [`Display::begin_frame`] when the scheduler is idle,
//!   [`Display::present`] to copy the frame into a buffer and flip to it
//!   with `NONBLOCK | PAGE_FLIP_EVENT`, and [`Display::dispatch`] to turn
//!   page-flip events into [`PresentInfo`] for `wp_presentation`.
//! - **Failures**: a flip that does not complete within a second is
//!   dropped and followed by a blocking modeset; three in a row are fatal
//!   ([`Display::check_timeout`]). A commit refused with `EACCES` or
//!   `EPERM` means the seat took DRM master away and counts as a pause.
//! - **Pause and resume** ([`Display::pause`], [`Display::resume`]):
//!   nothing is committed while the seat is disabled; on resume the card
//!   is probed again and a full modeset follows.
//! - **DPMS** ([`Display::set_dpms`]): `ACTIVE=0` or a modeset with
//!   `ACTIVE=1`, never while a flip is pending.

use std::{fmt, io, path::PathBuf, time::Instant};

use shingle::{OutputInfo, PresentInfo, PresentationFlags};
use wayland_server::protocol::wl_output;

use crate::{
  diagnostics,
  kms::{
    BlobId, CommitFlags, ConnectorInfo, KmsDevice, Mode, Subpixel,
    atomic::{self, ModesetState},
    modes::{self, ModeChoice},
    output::{self, Card, Output, OutputError},
  },
  pacing::{FrameScheduler, State},
  scanout::{Frame, Scanout, copy::CopyScanout},
};

/// `EPERM`.
const EPERM: i32 = 1;
/// `EACCES`.
const EACCES: i32 = 13;
/// `EBUSY`.
const EBUSY: i32 = 16;

/// Whether a commit failed because Shingle is not DRM master.
fn not_master(error: &io::Error) -> bool {
  matches!(error.raw_os_error(), Some(EACCES | EPERM))
}

/// Why the native backend cannot start; reported before the Wayland
/// socket exists, with exit status 1.
#[derive(Debug)]
pub enum StartError {
  /// No card could be opened. Holds each card tried with its error; an
  /// empty list means none was found.
  NoDrmDevice(Vec<(PathBuf, String)>),
  /// The driver refused atomic modesetting.
  NoAtomic(String),
  /// Another DRM master holds the card.
  NoMaster(String),
  /// Reading the card's connectors, CRTCs or planes failed.
  Probe(String),
  /// The chosen connector cannot be driven.
  Output(OutputError),
  /// Every mode was rejected. Holds the connector and each mode tried
  /// with the kernel's answer.
  NoMode(String, Vec<(String, String)>),
  /// No Vulkan adapter could be opened.
  NoAdapter(String),
}

impl fmt::Display for StartError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoDrmDevice(tried) if tried.is_empty() => {
        formatter.write_str("no DRM device: no card found under /sys/class/drm")
      }
      Self::NoDrmDevice(tried) => {
        let tried: Vec<String> = tried
          .iter()
          .map(|(path, error)| format!("{} ({error})", path.display()))
          .collect();
        write!(formatter, "no DRM device: tried {}", tried.join(", "))
      }
      Self::NoAtomic(error) => {
        write!(
          formatter,
          "atomic KMS required, but the driver says {error}"
        )
      }
      Self::NoMaster(error) => write!(
        formatter,
        "cannot become DRM master ({error}); another program holds the \
         display"
      ),
      Self::Probe(error) => {
        write!(formatter, "probing the card failed: {error}")
      }
      Self::Output(error) => error.fmt(formatter),
      Self::NoMode(connector, tried) => {
        let tried: Vec<String> = tried
          .iter()
          .map(|(mode, error)| format!("{mode} ({error})"))
          .collect();
        write!(
          formatter,
          "{connector}: every mode was rejected: {}",
          tried.join(", ")
        )
      }
      Self::NoAdapter(error) => {
        write!(formatter, "no Vulkan adapter: {error}")
      }
    }
  }
}

impl std::error::Error for StartError {}

/// An error that ends a running native backend with exit status 2, after
/// the session was saved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fatal(pub String);

impl fmt::Display for Fatal {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.0)
  }
}

impl std::error::Error for Fatal {}

/// Extends the CRTC's 32-bit vblank counter to the 64 bits of
/// `wp_presentation`'s sequence.
#[derive(Clone, Copy, Debug, Default)]
struct Sequence {
  high: u64,
  last: Option<u32>,
}

impl Sequence {
  fn extend(&mut self, value: u32) -> u64 {
    if self.last.is_some_and(|last| value < last) {
      self.high += 1 << 32;
    }
    self.last = Some(value);
    self.high | u64::from(value)
  }
}

/// The output being driven.
struct Lit {
  output: Output,
  mode: Mode,
  blob: BlobId,
  scanout: Box<dyn Scanout>,
}

/// Why no mode could be lit.
enum LightError {
  NoMaster(String),
  NoMode(String, Vec<(String, String)>),
}

/// Finds the first candidate mode of `output` that passes a `TEST_ONLY`
/// modeset with fresh copy buffers.
fn light(
  device: &mut dyn KmsDevice,
  output: Output,
  choice: ModeChoice,
) -> Result<Lit, LightError> {
  let candidates = modes::candidates(
    &output.connector.modes,
    choice,
    output.current_mode.as_ref(),
  );
  let mut tried = Vec::new();
  for mode in candidates {
    let name = format!("{}@{}", mode.name, mode.refresh_mhz() as f64 / 1000.0);
    let size = mode.size();
    let mut scanout = match CopyScanout::new(device, size) {
      Ok(scanout) => scanout,
      Err(error) => {
        tried.push((name, format!("no buffers: {error}")));
        continue;
      }
    };
    let blob = match device.create_mode_blob(&mode) {
      Ok(blob) => blob,
      Err(error) => {
        scanout.destroy(device);
        tried.push((name, error.to_string()));
        continue;
      }
    };
    let state = ModesetState {
      mode: blob,
      framebuffer: scanout.current(),
      size,
      active: true,
      vrr: false,
    };
    let request = atomic::modeset(&output.props, &state, &output.teardown);
    match device.commit(
      &request,
      CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
    ) {
      Ok(()) => {
        return Ok(Lit {
          output,
          mode,
          blob,
          scanout: Box::new(scanout),
        });
      }
      Err(error) => {
        scanout.destroy(device);
        let _ = device.destroy_blob(blob);
        if not_master(&error) || error.raw_os_error() == Some(EBUSY) {
          return Err(LightError::NoMaster(error.to_string()));
        }
        tried.push((name, error.to_string()));
      }
    }
  }
  Err(LightError::NoMode(output.connector.name, tried))
}

fn wl_subpixel(subpixel: Subpixel) -> wl_output::Subpixel {
  match subpixel {
    Subpixel::Unknown => wl_output::Subpixel::Unknown,
    Subpixel::HorizontalRgb => wl_output::Subpixel::HorizontalRgb,
    Subpixel::HorizontalBgr => wl_output::Subpixel::HorizontalBgr,
    Subpixel::VerticalRgb => wl_output::Subpixel::VerticalRgb,
    Subpixel::VerticalBgr => wl_output::Subpixel::VerticalBgr,
    Subpixel::None => wl_output::Subpixel::None,
  }
}

/// What changed in a resume.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resumed {
  /// The output's new size, if a different mode or output is lit now.
  pub resized: Option<(u32, u32)>,
  /// Whether a different connector is lit, or none.
  pub output_changed: bool,
}

/// One KMS output and its frame cycle.
pub struct Display {
  device: Box<dyn KmsDevice>,
  lit: Option<Lit>,
  scheduler: FrameScheduler,
  choice: ModeChoice,
  /// Whether the seat is enabled.
  active: bool,
  /// Whether the display is on, and whether it should be.
  dpms_on: bool,
  wanted_dpms: bool,
  monotonic: bool,
  sequence: Sequence,
}

impl Display {
  /// Sets up the display without committing anything for real: enables
  /// atomic modesetting, probes the card, chooses the output (`wanted`
  /// by name if connected) and finds a mode that passes a `TEST_ONLY`
  /// modeset. No connected output is not an error: the display then
  /// shows nothing until one appears.
  pub fn prepare(
    mut device: Box<dyn KmsDevice>,
    choice: ModeChoice,
    wanted: Option<&str>,
  ) -> Result<Self, StartError> {
    device
      .enable_atomic()
      .map_err(|error| StartError::NoAtomic(error.to_string()))?;
    let monotonic = device.monotonic_timestamps();
    if !monotonic {
      diagnostics::log(
        "page-flip times are not CLOCK_MONOTONIC; presentation times come \
         from the compositor's clock",
      );
    }
    let card = output::probe(device.as_mut())
      .map_err(|error| StartError::Probe(error.to_string()))?;
    let mut display = Self {
      device,
      lit: None,
      scheduler: FrameScheduler::default(),
      choice,
      active: true,
      dpms_on: true,
      wanted_dpms: true,
      monotonic,
      sequence: Sequence::default(),
    };
    let Some(connector) = card.choose_connector(wanted).cloned() else {
      diagnostics::log("no connected output; nothing is shown until one is");
      return Ok(display);
    };
    let lit = display.light(&card, &connector)?;
    display.log_lit(&lit);
    display.lit = Some(lit);
    Ok(display)
  }

  fn light(
    &mut self,
    card: &Card,
    connector: &ConnectorInfo,
  ) -> Result<Lit, StartError> {
    let device = self.device.as_mut();
    let output = card
      .output(connector, &mut |blob| device.blob(blob).ok())
      .map_err(StartError::Output)?;
    light(self.device.as_mut(), output, self.choice).map_err(
      |error| match error {
        LightError::NoMaster(error) => StartError::NoMaster(error),
        LightError::NoMode(name, tried) => StartError::NoMode(name, tried),
      },
    )
  }

  fn log_lit(&self, lit: &Lit) {
    let (width, height) = lit.mode.size();
    diagnostics::log(&format!(
      "output {}: {width}x{height} at {:.3} Hz, copy scanout",
      lit.output.connector.name,
      f64::from(lit.mode.refresh_mhz()) / 1000.0
    ));
  }

  /// Makes the modeset that `prepare` tested, showing a black frame.
  pub fn start(&mut self) -> io::Result<()> {
    if self.lit.is_some() {
      self.modeset()?;
    }
    Ok(())
  }

  /// A full modeset of the lit output: `TEST_ONLY` first, then a blocking
  /// `ALLOW_MODESET` commit showing the current buffer, with the display
  /// on or off as DPMS wants. Drawing resumes if it is on.
  fn modeset(&mut self) -> io::Result<()> {
    let Some(lit) = &self.lit else {
      return Ok(());
    };
    let state = ModesetState {
      mode: lit.blob,
      framebuffer: lit.scanout.current(),
      size: lit.mode.size(),
      active: self.wanted_dpms,
      vrr: false,
    };
    let request =
      atomic::modeset(&lit.output.props, &state, &lit.output.teardown);
    let result = self
      .device
      .commit(
        &request,
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      )
      .and_then(|()| self.device.commit(&request, CommitFlags::ALLOW_MODESET));
    match result {
      Ok(()) => {
        self.dpms_on = self.wanted_dpms;
        if self.dpms_on {
          self.scheduler.resume();
        } else {
          self.scheduler.pause();
        }
        Ok(())
      }
      Err(error) => {
        self.scheduler.pause();
        Err(error)
      }
    }
  }

  /// The output's size in pixels, if one is lit.
  pub fn size(&self) -> Option<(u32, u32)> {
    self.lit.as_ref().map(|lit| lit.mode.size())
  }

  /// What clients are told about the lit output.
  pub fn output_info(&self) -> Option<OutputInfo> {
    let lit = self.lit.as_ref()?;
    let connector = &lit.output.connector;
    Some(OutputInfo {
      name: connector.name.clone(),
      // EDID parsing is NB-9's; wlroots says "Unknown" without one too.
      make: "Unknown".into(),
      model: "Unknown".into(),
      serial: None,
      physical_size_mm: connector.physical_size_mm,
      subpixel: wl_subpixel(connector.subpixel),
      refresh_mhz: lit.mode.refresh_mhz(),
    })
  }

  /// The scheduler's state.
  pub fn state(&self) -> State {
    self.scheduler.state()
  }

  /// Whether the display is on.
  pub fn dpms_on(&self) -> bool {
    self.dpms_on
  }

  /// Whether a frame may be drawn now: an output is lit, the seat is
  /// enabled, the display is on and no flip is pending.
  pub fn ready(&self) -> bool {
    self.active && self.lit.is_some() && self.scheduler.can_render()
  }

  /// Starts a frame if [`Display::ready`].
  pub fn begin_frame(&mut self) -> bool {
    self.ready() && self.scheduler.begin_frame()
  }

  /// Gives up the frame started with [`Display::begin_frame`].
  pub fn abandon_frame(&mut self) {
    self.scheduler.abandon();
  }

  /// Puts `frame` on screen at the next vblank: copies it into the buffer
  /// off screen and commits a flip to it. A refused commit drops the
  /// frame; one refused for lack of DRM master pauses the display until
  /// the seat resumes it.
  pub fn present(&mut self, frame: &Frame, now: Instant) {
    let Some(lit) = &mut self.lit else {
      self.scheduler.abandon();
      return;
    };
    let framebuffer = match lit.scanout.prepare(self.device.as_mut(), frame) {
      Ok(framebuffer) => framebuffer,
      Err(error) => {
        diagnostics::log(&format!("copying the frame failed: {error}"));
        self.scheduler.abandon();
        return;
      }
    };
    let request = atomic::flip(&lit.output.props, framebuffer);
    match self.device.commit(
      &request,
      CommitFlags::NONBLOCK | CommitFlags::PAGE_FLIP_EVENT,
    ) {
      Ok(()) => self.scheduler.committed(now),
      Err(error) if not_master(&error) => {
        diagnostics::log(&format!(
          "the page flip was refused ({error}); waiting for the seat"
        ));
        self.scheduler.pause();
      }
      Err(error) => {
        diagnostics::log(&format!("the page flip failed: {error}"));
        self.scheduler.abandon();
      }
    }
  }

  /// Reads page-flip events and returns what each completed flip
  /// presented. Stale flips, such as one from before a pause, return
  /// nothing. A DPMS change waiting for the flip is made afterwards.
  pub fn dispatch(&mut self) -> Vec<PresentInfo> {
    let flips = match self.device.read_events() {
      Ok(flips) => flips,
      Err(error) => {
        diagnostics::log(&format!("reading DRM events failed: {error}"));
        return Vec::new();
      }
    };
    let mut presented = Vec::new();
    for flip in flips {
      let Some(lit) = &mut self.lit else {
        continue;
      };
      if flip.crtc != lit.output.props.crtc.id || !self.scheduler.page_flip() {
        continue;
      }
      lit.scanout.presented();
      let mut flags =
        PresentationFlags::Vsync | PresentationFlags::HwCompletion;
      let time = if self.monotonic {
        flags |= PresentationFlags::HwClock;
        flip.time
      } else {
        monotonic_now()
      };
      if lit.scanout.zero_copy() {
        flags |= PresentationFlags::ZeroCopy;
      }
      presented.push(PresentInfo {
        time,
        refresh_ns: lit.mode.refresh_ns(),
        seq: self.sequence.extend(flip.sequence),
        flags,
      });
    }
    self.apply_dpms();
    presented
  }

  /// When the pending flip times out, if one is pending.
  pub fn deadline(&self) -> Option<Instant> {
    self.scheduler.deadline()
  }

  /// Checks the pending flip at `now`. A flip pending for a second is
  /// logged and dropped, and a blocking modeset shows the current buffer
  /// again. The third timeout in a row is [`Fatal`].
  pub fn check_timeout(&mut self, now: Instant) -> Result<(), Fatal> {
    let Some(timeout) = self.scheduler.check_timeout(now) else {
      return Ok(());
    };
    diagnostics::log(&format!(
      "the page flip did not complete within 1 s ({} in a row)",
      timeout.consecutive
    ));
    if timeout.fatal() {
      return Err(Fatal(format!(
        "{} page flips in a row did not complete",
        timeout.consecutive
      )));
    }
    if let Err(error) = self.modeset() {
      diagnostics::log(&format!("the recovery modeset failed: {error}"));
      // Try again at the next frame.
      if self.dpms_on && self.active {
        self.scheduler.resume();
      }
    }
    self.apply_dpms();
    Ok(())
  }

  /// Stops committing for a disabled seat. A pending flip is forgotten;
  /// its event may never come.
  pub fn pause(&mut self) {
    self.active = false;
    self.scheduler.pause();
  }

  /// Resumes for an enabled seat: probes the card again, since outputs
  /// may have changed while away, and makes a full modeset, because the
  /// other session may have changed modes. The lit connector stays if it
  /// is still connected and its mode still passes; otherwise the output
  /// is chosen again, or none is lit if nothing is connected.
  pub fn resume(&mut self) -> Resumed {
    self.active = true;
    let before = self
      .lit
      .as_ref()
      .map(|lit| (lit.output.connector.name.clone(), lit.mode.size()));
    let card = match output::probe(self.device.as_mut()) {
      Ok(card) => card,
      Err(error) => {
        diagnostics::log(&format!("probing the card failed: {error}"));
        if let Err(error) = self.modeset() {
          diagnostics::log(&format!("the modeset failed: {error}"));
        }
        return Resumed::default();
      }
    };
    let wanted = before.as_ref().map(|(name, _)| name.as_str());
    let connector = card.choose_connector(wanted).cloned();
    let kept = self.refresh_lit(&card, connector.as_ref());
    if !kept {
      self.relight(&card, connector.as_ref());
    }
    let after = self
      .lit
      .as_ref()
      .map(|lit| (lit.output.connector.name.clone(), lit.mode.size()));
    Resumed {
      resized: after
        .as_ref()
        .filter(|(_, size)| Some(*size) != before.as_ref().map(|(_, s)| *s))
        .map(|(_, size)| *size),
      output_changed: after.map(|(name, _)| name)
        != before.map(|(name, _)| name),
    }
  }

  /// Keeps the lit output if `connector` is it and still offers its
  /// mode, re-reading its properties, and modesets it. Returns whether
  /// that worked.
  fn refresh_lit(
    &mut self,
    card: &Card,
    connector: Option<&ConnectorInfo>,
  ) -> bool {
    let (Some(lit), Some(connector)) = (self.lit.as_mut(), connector) else {
      return false;
    };
    if connector.id != lit.output.connector.id
      || !connector.modes.contains(&lit.mode)
    {
      return false;
    }
    let device = self.device.as_mut();
    match card.output(connector, &mut |blob| device.blob(blob).ok()) {
      Ok(output) => lit.output = output,
      Err(error) => {
        diagnostics::log(&format!("{}: {error}", connector.name));
        return false;
      }
    }
    match self.modeset() {
      Ok(()) => true,
      Err(error) => {
        diagnostics::log(&format!("the modeset after resume failed: {error}"));
        false
      }
    }
  }

  /// Replaces the lit output with `connector` (or none) and modesets it.
  fn relight(&mut self, card: &Card, connector: Option<&ConnectorInfo>) {
    if let Some(mut old) = self.lit.take() {
      old.scanout.destroy(self.device.as_mut());
      let _ = self.device.destroy_blob(old.blob);
    }
    self.scheduler.pause();
    let Some(connector) = connector else {
      diagnostics::log("no connected output; nothing is shown until one is");
      return;
    };
    match self.light(card, connector) {
      Ok(lit) => {
        self.log_lit(&lit);
        self.lit = Some(lit);
        if let Err(error) = self.modeset() {
          diagnostics::log(&format!("the modeset failed: {error}"));
        }
      }
      Err(error) => diagnostics::log(&format!("{error}; nothing is shown")),
    }
  }

  /// Turns the display on or off, as wlr-output-power-management or the
  /// idle timeout asks. While a flip is pending, or the seat is disabled,
  /// the change waits; a resume applies it with its modeset.
  pub fn set_dpms(&mut self, on: bool) {
    self.wanted_dpms = on;
    self.apply_dpms();
  }

  fn apply_dpms(&mut self) {
    if self.wanted_dpms == self.dpms_on
      || !self.active
      || self.scheduler.flip_pending()
    {
      return;
    }
    let Some(lit) = &self.lit else {
      self.dpms_on = self.wanted_dpms;
      return;
    };
    if self.wanted_dpms {
      if let Err(error) = self.modeset() {
        diagnostics::log(&format!("turning the display on failed: {error}"));
      }
      return;
    }
    let request = atomic::set_active(&lit.output.props, false);
    let result = self
      .device
      .commit(
        &request,
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      )
      .and_then(|()| self.device.commit(&request, CommitFlags::ALLOW_MODESET));
    match result {
      Ok(()) => {
        self.dpms_on = false;
        self.scheduler.pause();
      }
      Err(error) => {
        diagnostics::log(&format!("turning the display off failed: {error}"));
      }
    }
  }

  /// The card's descriptor, readable when page flips completed.
  pub fn poll_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
    self.device.poll_fd()
  }
}

/// The current `CLOCK_MONOTONIC` time.
fn monotonic_now() -> std::time::Duration {
  let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
  std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

#[cfg(test)]
mod tests;
