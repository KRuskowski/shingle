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
//! - **Outputs** ([`Display::hotplug`], [`Display::set_policy`]): a DRM
//!   hotplug probes the card again and diffs the connectors with the last
//!   probe; the lit connector stays while it is connected and offers its
//!   mode, otherwise the next one is chosen ([`outputs::choose`]), or none,
//!   and nothing is drawn until a display is plugged in again. The mode of
//!   each connector comes from the configuration ([`OutputPolicy`]).
//! - **Mode requests** ([`Display::test_mode`], [`Display::set_mode`]):
//!   wlr-output-management mode changes pass a `TEST_ONLY` modeset before
//!   they are made.

use std::{fmt, io, path::PathBuf, time::Instant};

use shingle::{OutputInfo, PresentInfo, PresentationFlags, RequestedMode};
use wayland_server::protocol::wl_output;

use crate::{
  diagnostics,
  kms::{
    BlobId, CommitFlags, ConnectorInfo, KmsDevice, Mode, Subpixel,
    atomic::{self, ModesetState},
    edid::Edid,
    modes::{self, ModeChoice},
    output::{self, Card, Output, OutputError},
  },
  outputs::{self, ConnectorChange, ConnectorState, OutputPolicy},
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

/// A mode set up on `output` with fresh copy buffers and a mode blob,
/// after a passed `TEST_ONLY` modeset.
fn test_mode(
  device: &mut dyn KmsDevice,
  output: &Output,
  mode: &Mode,
) -> Result<(BlobId, CopyScanout), (io::Error, bool)> {
  let size = mode.size();
  let mut scanout = CopyScanout::new(device, size).map_err(|error| {
    (io::Error::other(format!("no buffers: {error}")), false)
  })?;
  let blob = match device.create_mode_blob(mode) {
    Ok(blob) => blob,
    Err(error) => {
      scanout.destroy(device);
      return Err((error, false));
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
    Ok(()) => Ok((blob, scanout)),
    Err(error) => {
      scanout.destroy(device);
      let _ = device.destroy_blob(blob);
      let master = not_master(&error) || error.raw_os_error() == Some(EBUSY);
      Err((error, master))
    }
  }
}

/// The name of `mode` in log lines, such as `1920x1080@60`.
fn mode_name(mode: &Mode) -> String {
  format!("{}@{}", mode.name, mode.refresh_mhz() as f64 / 1000.0)
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
    match test_mode(device, &output, &mode) {
      Ok((blob, scanout)) => {
        return Ok(Lit {
          output,
          mode,
          blob,
          scanout: Box::new(scanout),
        });
      }
      Err((error, true)) => {
        return Err(LightError::NoMaster(error.to_string()));
      }
      Err((error, false)) => tried.push((mode_name(&mode), error.to_string())),
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
  /// The configured mode of each connector.
  policy: OutputPolicy,
  /// The connectors as the last probe saw them, for diffing.
  connectors: Vec<ConnectorState>,
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
  /// atomic modesetting, probes the card, chooses the output as `policy`
  /// says ([`outputs::choose`]) and finds a mode that passes a `TEST_ONLY`
  /// modeset. No connected output is not an error: the display then
  /// shows nothing until one appears.
  pub fn prepare(
    mut device: Box<dyn KmsDevice>,
    policy: OutputPolicy,
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
      connectors: outputs::snapshot(&card),
      policy,
      active: true,
      dpms_on: true,
      wanted_dpms: true,
      monotonic,
      sequence: Sequence::default(),
    };
    let Some(connector) =
      outputs::choose(&card, &display.policy, None).cloned()
    else {
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
    let choice = self.policy.choice(&connector.name).unwrap_or_default();
    light(self.device.as_mut(), output, choice).map_err(|error| match error {
      LightError::NoMaster(error) => StartError::NoMaster(error),
      LightError::NoMode(name, tried) => StartError::NoMode(name, tried),
    })
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

  /// What clients are told about the lit output: the connector's name,
  /// the EDID's make, model and serial ([`Edid::unknown`] without one),
  /// physical size, subpixel layout, the connector's modes and the lit
  /// one.
  pub fn output_info(&self) -> Option<OutputInfo> {
    let lit = self.lit.as_ref()?;
    let connector = &lit.output.connector;
    let edid = connector.edid.clone().unwrap_or_else(Edid::unknown);
    Some(OutputInfo {
      name: connector.name.clone(),
      make: edid.make,
      model: edid.model,
      serial: edid.serial,
      physical_size_mm: connector.physical_size_mm,
      subpixel: wl_subpixel(connector.subpixel),
      refresh_mhz: lit.mode.refresh_mhz(),
      modes: outputs::mode_list(&connector.modes),
      current_mode: connector.modes.iter().position(|mode| *mode == lit.mode),
    })
  }

  /// The name of the lit connector, if one is lit.
  pub fn output_name(&self) -> Option<&str> {
    self
      .lit
      .as_ref()
      .map(|lit| lit.output.connector.name.as_str())
  }

  /// The lit mode, if an output is lit.
  pub fn mode(&self) -> Option<&Mode> {
    self.lit.as_ref().map(|lit| &lit.mode)
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
    self.log_changes(&card);
    let wanted = before.as_ref().map(|(name, _)| name.as_str());
    let connector = outputs::choose(&card, &self.policy, wanted).cloned();
    let kept = self.refresh_lit(&card, connector.as_ref());
    if !kept {
      self.relight(&card, connector.as_ref());
    }
    self.changes_since(before)
  }

  /// What changed since the lit output was `before` (its name and size).
  fn changes_since(&self, before: Option<(String, (u32, u32))>) -> Resumed {
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

  /// The lit output's name and size.
  fn lit_state(&self) -> Option<(String, (u32, u32))> {
    self
      .lit
      .as_ref()
      .map(|lit| (lit.output.connector.name.clone(), lit.mode.size()))
  }

  /// Logs how the connectors changed since the last probe and remembers
  /// this one. Returns the changes.
  fn log_changes(&mut self, card: &Card) -> Vec<ConnectorChange> {
    let now = outputs::snapshot(card);
    let changes = outputs::diff(&self.connectors, &now);
    for change in &changes {
      diagnostics::log(&match change {
        ConnectorChange::Connected(name) => format!("{name} connected"),
        ConnectorChange::Disconnected(name) => format!("{name} disconnected"),
        ConnectorChange::ModesChanged(name) => {
          format!("{name} offers other modes")
        }
      });
    }
    self.connectors = now;
    changes
  }

  /// Follows a DRM hotplug: probes the card again and diffs it with the
  /// last probe. The lit output stays, without a modeset, while it is
  /// connected and still offers its mode; otherwise the output is chosen
  /// again ([`outputs::choose`]) and lit with a modeset, or, with nothing
  /// connected, none is lit and nothing is drawn. While the seat is
  /// disabled this does nothing, since the resume probes anyway.
  pub fn hotplug(&mut self) -> Resumed {
    if !self.active {
      return Resumed::default();
    }
    let before = self.lit_state();
    let card = match output::probe(self.device.as_mut()) {
      Ok(card) => card,
      Err(error) => {
        diagnostics::log(&format!("probing the card failed: {error}"));
        return Resumed::default();
      }
    };
    let changes = self.log_changes(&card);
    let wanted = before.as_ref().map(|(name, _)| name.as_str());
    let connector = outputs::choose(&card, &self.policy, wanted).cloned();
    let keep = match (&self.lit, &connector) {
      (Some(lit), Some(connector)) => {
        connector.id == lit.output.connector.id
          && connector.modes.contains(&lit.mode)
      }
      (None, None) => true,
      _ => false,
    };
    if keep {
      if let (Some(lit), Some(connector)) = (self.lit.as_mut(), connector) {
        // Modes, EDID and properties may have changed with a new monitor.
        let device = self.device.as_mut();
        match card.output(&connector, &mut |blob| device.blob(blob).ok()) {
          Ok(output) => lit.output = output,
          Err(error) => {
            diagnostics::log(&format!("{}: {error}", connector.name))
          }
        }
      }
      return Resumed {
        output_changed: changes
          .iter()
          .any(|change| Some(change_name(change)) == wanted),
        ..Resumed::default()
      };
    }
    self.relight(&card, connector.as_ref());
    self.changes_since(before)
  }

  /// Takes a reloaded configuration. If it gives the lit output another
  /// mode, turns it off, or names another connected output to prefer, the
  /// output is lit again as it says.
  pub fn set_policy(&mut self, policy: OutputPolicy) -> Resumed {
    if policy == self.policy {
      return Resumed::default();
    }
    self.policy = policy;
    if !self.active {
      return Resumed::default();
    }
    let before = self.lit_state();
    let card = match output::probe(self.device.as_mut()) {
      Ok(card) => card,
      Err(error) => {
        diagnostics::log(&format!("probing the card failed: {error}"));
        return Resumed::default();
      }
    };
    self.log_changes(&card);
    let wanted = before.as_ref().map(|(name, _)| name.as_str());
    let connector = outputs::choose(&card, &self.policy, wanted).cloned();
    let same = match (&self.lit, &connector) {
      (Some(lit), Some(connector)) => {
        connector.id == lit.output.connector.id
          && self.policy.preferred_mode(connector).as_ref() == Some(&lit.mode)
      }
      (None, None) => true,
      _ => false,
    };
    if !same {
      self.relight(&card, connector.as_ref());
    }
    self.changes_since(before)
  }

  /// Whether `request` names a mode of the lit output that passes a
  /// `TEST_ONLY` modeset.
  pub fn test_mode(&mut self, request: RequestedMode) -> Result<(), String> {
    let (blob, mut scanout) = self.tested(request)?;
    scanout.destroy(self.device.as_mut());
    let _ = self.device.destroy_blob(blob);
    Ok(())
  }

  /// Sets the mode `request` names on the lit output: a `TEST_ONLY`
  /// modeset first, then the modeset. The old mode stays if either fails.
  /// The caller waits until no flip is pending.
  pub fn set_mode(
    &mut self,
    request: RequestedMode,
  ) -> Result<Resumed, String> {
    let before = self.lit_state();
    let (blob, scanout) = self.tested(request)?;
    let Some(lit) = self.lit.as_mut() else {
      return Err("no output is lit".into());
    };
    let mode = outputs::requested_mode(&lit.output.connector.modes, request)
      .ok_or("the output does not offer that mode")?;
    let old_mode = std::mem::replace(&mut lit.mode, mode);
    let old_blob = std::mem::replace(&mut lit.blob, blob);
    let old_scanout = std::mem::replace(
      &mut lit.scanout,
      Box::new(scanout) as Box<dyn Scanout>,
    );
    match self.modeset() {
      Ok(()) => {
        let mut old_scanout = old_scanout;
        old_scanout.destroy(self.device.as_mut());
        let _ = self.device.destroy_blob(old_blob);
        if let Some(lit) = &self.lit {
          self.log_lit(lit);
        }
        Ok(self.changes_since(before))
      }
      Err(error) => {
        let Some(lit) = self.lit.as_mut() else {
          return Err(error.to_string());
        };
        lit.mode = old_mode;
        let blob = std::mem::replace(&mut lit.blob, old_blob);
        let mut scanout = std::mem::replace(&mut lit.scanout, old_scanout);
        scanout.destroy(self.device.as_mut());
        let _ = self.device.destroy_blob(blob);
        if let Err(error) = self.modeset() {
          diagnostics::log(&format!("restoring the old mode failed: {error}"));
        }
        Err(error.to_string())
      }
    }
  }

  /// The blob and buffers of `request`'s mode after a passed `TEST_ONLY`
  /// modeset.
  fn tested(
    &mut self,
    request: RequestedMode,
  ) -> Result<(BlobId, CopyScanout), String> {
    if !self.active {
      return Err("the seat is not active".into());
    }
    let Some(lit) = &self.lit else {
      return Err("no output is lit".into());
    };
    let mode = outputs::requested_mode(&lit.output.connector.modes, request)
      .ok_or("the output does not offer that mode")?;
    test_mode(self.device.as_mut(), &lit.output, &mode)
      .map_err(|(error, _)| format!("{}: {error}", mode_name(&mode)))
  }

  /// Whether a page flip is pending, during which no modeset may be made.
  pub fn flip_pending(&self) -> bool {
    self.scheduler.flip_pending()
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

/// The connector a change is about.
fn change_name(change: &ConnectorChange) -> &str {
  match change {
    ConnectorChange::Connected(name)
    | ConnectorChange::Disconnected(name)
    | ConnectorChange::ModesChanged(name) => name,
  }
}

/// The current `CLOCK_MONOTONIC` time.
fn monotonic_now() -> std::time::Duration {
  let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
  std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

#[cfg(test)]
mod tests;
