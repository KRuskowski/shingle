//! The display: the KMS outputs, one CRTC each, driven with atomic
//! commits.
//!
//! [`Display`] owns the card ([`KmsDevice`]) and the lit outputs, each with
//! its connector, CRTC, primary plane, mode and blob, [`Scanout`],
//! [`FrameScheduler`] and presentation sequence. It implements the display
//! parts of docs/design/native-backend.md and, for several outputs,
//! docs/design/multi-output.md (section 7, "Decision: native"):
//!
//! - **Which outputs** ([`Display::prepare`]): every connected connector
//!   that is not configured `off`, in priority order ([`outputs::rank`]),
//!   as many as the card has CRTCs and primary planes for
//!   ([`Card::allocate`], wlroots' matching). The others stay dark and are
//!   listed ([`Display::unlit`]) for output management.
//! - **Modesets**: all lit outputs in one atomic request, `TEST_ONLY`
//!   first and then a blocking `ALLOW_MODESET` commit, with every CRTC,
//!   plane and connector they do not use turned off in the same request.
//!   Each new output takes the first of its candidate modes that passes a
//!   `TEST_ONLY` modeset together with the outputs lit before it, so a
//!   limit they share, such as bandwidth, drops the lowest-priority one.
//!   Startup tests before the Wayland socket exists; [`Display::start`]
//!   then makes the real modeset.
//! - **Frames** per output: [`Display::begin_frame_on`] when its scheduler
//!   is idle, [`Display::present_on`] to copy the frame into a buffer and
//!   flip its CRTC to it with `NONBLOCK | PAGE_FLIP_EVENT`, and
//!   [`Display::dispatch_flips`] to route page-flip events by CRTC and turn
//!   them into [`PresentInfo`] for `wp_presentation`.
//! - **Failures**: a flip that does not complete within a second is
//!   dropped and followed by a blocking modeset of that output; three in a
//!   row are fatal ([`Display::check_timeout`]). A commit refused with
//!   `EACCES` or `EPERM` means the seat took DRM master away and counts as
//!   a pause.
//! - **Pause and resume** ([`Display::pause`], [`Display::resume`]):
//!   nothing is committed while the seat is disabled; on resume the card
//!   is probed again and a full modeset follows.
//! - **DPMS** per output ([`Display::set_output_dpms`],
//!   [`Display::set_dpms`] for all): `ACTIVE=0`, or a modeset of that
//!   output with `ACTIVE=1`, never while its flip is pending.
//! - **Hotplug** ([`Display::hotplug`], [`Display::set_policy`]): the card
//!   is probed again and diffed with the last probe; lit outputs stay
//!   while connected and offering their mode, unplugged ones are dropped,
//!   newly connected ones lit as CRTCs allow.
//! - **Configurations** ([`Display::test_configuration`],
//!   [`Display::apply_configuration`]): wlr-output-management mode changes
//!   and outputs turned on or off, across heads, pass one `TEST_ONLY`
//!   modeset of the whole new configuration before it is made.
//!
//! The calls without an output name ([`Display::size`],
//! [`Display::present`], [`Display::test_mode`] and the like) mean the
//! first lit output.

use std::{collections::BTreeMap, fmt, io, path::PathBuf, time::Instant};

use shingle::{
  HeadState, OutputInfo, PresentInfo, PresentationFlags, RequestedMode,
};
use shingle_config::OutputMode;
use wayland_server::protocol::wl_output;

use crate::{
  diagnostics,
  kms::{
    AtomicRequest, BlobId, CommitFlags, Connection, ConnectorInfo, CrtcId,
    KmsDevice, Mode, PlaneId, Subpixel,
    atomic::{self, ModesetState, OutputProps, Teardown},
    edid::Edid,
    modes::{self, ModeChoice},
    output::{self, Card, Output, OutputError, Wanted},
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

/// One output being driven, with its own frame cycle.
struct Lit {
  output: Output,
  mode: Mode,
  blob: BlobId,
  scanout: Box<dyn Scanout>,
  scheduler: FrameScheduler,
  sequence: Sequence,
  /// Whether the CRTC is active.
  dpms_on: bool,
  /// Whether it should be.
  wanted_dpms: bool,
}

impl Lit {
  fn name(&self) -> &str {
    &self.output.connector.name
  }

  fn crtc(&self) -> CrtcId {
    self.output.props.crtc.id
  }

  fn plane(&self) -> PlaneId {
    self.output.props.plane.id
  }

  /// What a full modeset sets it to.
  fn state(&self) -> ModesetState {
    ModesetState {
      mode: self.blob,
      framebuffer: self.scanout.current(),
      size: self.mode.size(),
      active: self.wanted_dpms,
      vrr: false,
    }
  }

  /// Frees its buffers and mode blob.
  fn destroy(mut self, device: &mut dyn KmsDevice) {
    self.scanout.destroy(device);
    let _ = device.destroy_blob(self.blob);
  }
}

/// Why no mode could be lit.
enum LightError {
  NoMaster(String),
  NoMode(String, Vec<(String, String)>),
  Output(OutputError),
}

impl LightError {
  fn into_start_error(self) -> StartError {
    match self {
      Self::NoMaster(error) => StartError::NoMaster(error),
      Self::NoMode(name, tried) => StartError::NoMode(name, tried),
      Self::Output(error) => StartError::Output(error),
    }
  }
}

/// Fresh copy buffers and a mode blob for `mode`, not yet tested.
fn fresh(
  device: &mut dyn KmsDevice,
  mode: &Mode,
) -> io::Result<(BlobId, CopyScanout)> {
  let mut scanout = CopyScanout::new(device, mode.size())
    .map_err(|error| io::Error::other(format!("no buffers: {error}")))?;
  match device.create_mode_blob(mode) {
    Ok(blob) => Ok((blob, scanout)),
    Err(error) => {
      scanout.destroy(device);
      Err(error)
    }
  }
}

/// Frees what [`fresh`] made.
fn discard(device: &mut dyn KmsDevice, blob: BlobId, mut scanout: CopyScanout) {
  scanout.destroy(device);
  let _ = device.destroy_blob(blob);
}

/// The full state of a fresh mode on screen, for its modeset.
fn fresh_state(
  mode: &Mode,
  blob: BlobId,
  scanout: &CopyScanout,
) -> ModesetState {
  ModesetState {
    mode: blob,
    framebuffer: scanout.current(),
    size: mode.size(),
    active: true,
    vrr: false,
  }
}

/// The name of `mode` in log lines, such as `1920x1080@60`.
fn mode_name(mode: &Mode) -> String {
  format!("{}@{}", mode.name, mode.refresh_mhz() as f64 / 1000.0)
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

/// What clients are told about `connector`: its name, the EDID's make,
/// model and serial ([`Edid::unknown`] without one), physical size,
/// subpixel layout and modes, with `mode` as the current one.
fn describe(connector: &ConnectorInfo, mode: Option<&Mode>) -> OutputInfo {
  let edid = connector.edid.clone().unwrap_or_else(Edid::unknown);
  let refresh = mode
    .or(connector.modes.first())
    .map_or(0, Mode::refresh_mhz);
  OutputInfo {
    name: connector.name.clone(),
    make: edid.make,
    model: edid.model,
    serial: edid.serial,
    physical_size_mm: connector.physical_size_mm,
    subpixel: wl_subpixel(connector.subpixel),
    refresh_mhz: refresh,
    modes: outputs::mode_list(&connector.modes),
    current_mode: mode.and_then(|mode| {
      connector.modes.iter().position(|listed| listed == mode)
    }),
  }
}

/// What changed in a resume, a hotplug, a reloaded configuration or a
/// mode change.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resumed {
  /// The first lit output's new size, if a different mode or output is
  /// first now.
  pub resized: Option<(u32, u32)>,
  /// Whether a different connector is the first lit one, or none.
  pub output_changed: bool,
  /// Whether any output was lit, turned off or given another mode.
  pub outputs_changed: bool,
}

/// The lit outputs' names and modes, in order, to tell what changed.
type Snapshot = Vec<(String, Mode)>;

/// What output management asks of the outputs it names.
struct Plan {
  /// Lit outputs turned off, by index.
  off: Vec<usize>,
  /// Lit outputs given another mode, by index, with its buffers and blob.
  changed: Vec<(usize, Mode, BlobId, CopyScanout)>,
  /// Outputs lit that were off, with their buffers and blob.
  added: Vec<(Output, Mode, BlobId, CopyScanout)>,
}

impl Plan {
  fn discard(self, device: &mut dyn KmsDevice) {
    for (_, _, blob, scanout) in self.changed {
      discard(device, blob, scanout);
    }
    for (_, _, blob, scanout) in self.added {
      discard(device, blob, scanout);
    }
  }

  /// The modes it sets, for error messages.
  fn mode_names(&self) -> String {
    let names: Vec<String> = self
      .changed
      .iter()
      .map(|(_, mode, ..)| mode_name(mode))
      .chain(self.added.iter().map(|(_, mode, ..)| mode_name(mode)))
      .collect();
    if names.is_empty() {
      "the configuration".into()
    } else {
      names.join(", ")
    }
  }
}

/// The KMS outputs and their frame cycles.
pub struct Display {
  device: Box<dyn KmsDevice>,
  /// The lit outputs, in the order they were lit.
  lit: Vec<Lit>,
  /// The configured mode of each connector.
  policy: OutputPolicy,
  /// Modes output management set, as `off` or an explicit mode, which
  /// config reloads leave alone until Shingle restarts, as Wayfire's
  /// `is_externally_managed` does.
  managed: BTreeMap<String, OutputMode>,
  /// At most this many outputs are lit (`--outputs N`).
  max_outputs: Option<usize>,
  /// The card as the last probe saw it.
  card: Card,
  /// The connectors as the last probe saw them, for diffing.
  connectors: Vec<ConnectorState>,
  /// Outputs lit before and dropped since the last full modeset, whose
  /// objects that modeset turns off even if the last probe saw them off.
  retired: Vec<OutputProps>,
  /// Whether the seat is enabled.
  active: bool,
  /// Whether outputs lit from now on start on.
  wanted_dpms: bool,
  monotonic: bool,
}

impl Display {
  /// Sets up the display without committing anything for real: enables
  /// atomic modesetting, probes the card, ranks the connected connectors
  /// as `policy` says ([`outputs::rank`]), gives as many as the card's
  /// CRTCs allow a CRTC and plane ([`Card::allocate`]) and finds a mode
  /// for each that passes a `TEST_ONLY` modeset together with the outputs
  /// before it. No connected output is not an error: the display then
  /// shows nothing until one appears. If none can be lit, the first one's
  /// error is.
  pub fn prepare(
    device: Box<dyn KmsDevice>,
    policy: OutputPolicy,
  ) -> Result<Self, StartError> {
    Self::prepare_limited(device, policy, None)
  }

  /// [`Display::prepare`], lighting at most `max_outputs` outputs.
  pub fn prepare_limited(
    mut device: Box<dyn KmsDevice>,
    policy: OutputPolicy,
    max_outputs: Option<usize>,
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
      lit: Vec::new(),
      connectors: outputs::snapshot(&card),
      card,
      policy,
      managed: BTreeMap::new(),
      max_outputs,
      retired: Vec::new(),
      active: true,
      wanted_dpms: true,
      monotonic,
    };
    let ranked = display.ranked();
    if ranked.is_empty() {
      diagnostics::log("no connected output; nothing is shown until one is");
      return Ok(display);
    }
    let failures = display
      .light_new(&ranked)
      .map_err(LightError::into_start_error)?;
    if display.lit.is_empty() {
      let first = &ranked[0].name;
      let error = failures
        .into_iter()
        .find(|(name, _)| name == first)
        .map(|(_, error)| error.into_start_error())
        .unwrap_or_else(|| {
          StartError::Output(OutputError::NoCrtc(first.clone()))
        });
      return Err(error);
    }
    display.log_failures(failures);
    Ok(display)
  }

  /// How to choose the mode of output `name`, or `None` if it is off:
  /// output management's choice if it made one, else the configuration's.
  fn choice(&self, name: &str) -> Option<ModeChoice> {
    let mode = self
      .managed
      .get(name)
      .copied()
      .unwrap_or_else(|| self.policy.mode(name));
    outputs::mode_choice(mode)
  }

  /// The connected connectors that are not off and not lit, in the order
  /// they should be lit.
  fn ranked(&self) -> Vec<ConnectorInfo> {
    let lit: Vec<&str> = self.lit.iter().map(Lit::name).collect();
    let off = |name: &str| self.choice(name).is_none();
    outputs::rank(&self.card, &self.policy, &lit, &off)
      .into_iter()
      .filter(|info| !lit.contains(&info.name.as_str()))
      .cloned()
      .collect()
  }

  /// Lights as many of `candidates` as there are CRTCs and planes for, in
  /// order, each with the first mode that passes a `TEST_ONLY` modeset
  /// together with every lit output. Returns why the others could not be
  /// lit; a commit refused for lack of DRM master ends the search.
  fn light_new(
    &mut self,
    candidates: &[ConnectorInfo],
  ) -> Result<Vec<(String, LightError)>, LightError> {
    let mut failures = Vec::new();
    let mut pending: Vec<&ConnectorInfo> = candidates.iter().collect();
    while !pending.is_empty() {
      let full = self.max_outputs.is_some_and(|max| self.lit.len() >= max);
      if full {
        for connector in pending {
          diagnostics::log(&format!(
            "{} stays dark: --outputs allows {} lit outputs",
            connector.name,
            self.max_outputs.unwrap_or_default()
          ));
        }
        break;
      }
      let lit: Vec<ConnectorInfo> = self
        .lit
        .iter()
        .map(|lit| lit.output.connector.clone())
        .collect();
      let mut wants: Vec<Wanted<'_>> = lit
        .iter()
        .zip(&self.lit)
        .map(|(connector, lit)| Wanted {
          connector,
          lit: Some((lit.crtc(), lit.plane())),
        })
        .collect();
      wants.extend(pending.iter().map(|connector| Wanted {
        connector,
        lit: None,
      }));
      let allocated = self.card.allocate(&wants)[lit.len()..].to_vec();
      // Light the first that has a CRTC, then allocate again: CRTCs of
      // connectors that fail are free for the next ones.
      let Some(index) = allocated.iter().position(Option::is_some) else {
        for connector in pending {
          // A connector some CRTC could drive waits for one to be free.
          let error = match self.card.output(connector, &mut |_| None) {
            Ok(_) => OutputError::Busy(connector.name.clone()),
            Err(error) => error,
          };
          failures.push((connector.name.clone(), LightError::Output(error)));
        }
        break;
      };
      let connector = pending.remove(index);
      let (crtc, plane) = allocated[index].expect("a CRTC was allocated");
      match self.admit(connector, crtc, plane) {
        Ok(()) => {
          if let Some(lit) = self.lit.last() {
            self.log_lit(lit);
          }
        }
        Err(LightError::NoMaster(error)) => {
          return Err(LightError::NoMaster(error));
        }
        Err(error) => failures.push((connector.name.clone(), error)),
      }
    }
    Ok(failures)
  }

  /// Logs why connectors stay dark.
  fn log_failures(&self, failures: Vec<(String, LightError)>) {
    for (name, error) in failures {
      let error = error.into_start_error();
      diagnostics::log(&format!("{error}; {name} stays dark"));
    }
  }

  /// Lights `connector` on `crtc` and `plane` with the first of its
  /// candidate modes that passes a `TEST_ONLY` modeset of every lit
  /// output and this one, with fresh buffers and a mode blob. It joins the
  /// lit outputs, paused until the next full modeset.
  fn admit(
    &mut self,
    connector: &ConnectorInfo,
    crtc: CrtcId,
    plane: PlaneId,
  ) -> Result<(), LightError> {
    let device = self.device.as_mut();
    let output = self
      .card
      .output_on(connector, crtc, plane, &mut |blob| device.blob(blob).ok())
      .map_err(LightError::Output)?;
    let choice = self.choice(&connector.name).unwrap_or_default();
    let candidates = modes::candidates(
      &output.connector.modes,
      choice,
      output.current_mode.as_ref(),
    );
    let mut tried = Vec::new();
    for mode in candidates {
      let (blob, scanout) = match fresh(self.device.as_mut(), &mode) {
        Ok(fresh) => fresh,
        Err(error) => {
          tried.push((mode_name(&mode), error.to_string()));
          continue;
        }
      };
      let state = fresh_state(&mode, blob, &scanout);
      let request = self.request(&[(&output.props, state)], &[]);
      match self.device.commit(
        &request,
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      ) {
        Ok(()) => {
          self.lit.push(Lit {
            output,
            mode,
            blob,
            scanout: Box::new(scanout),
            scheduler: FrameScheduler::default(),
            sequence: Sequence::default(),
            dpms_on: true,
            wanted_dpms: self.wanted_dpms,
          });
          return Ok(());
        }
        Err(error) => {
          discard(self.device.as_mut(), blob, scanout);
          if not_master(&error) || error.raw_os_error() == Some(EBUSY) {
            return Err(LightError::NoMaster(error.to_string()));
          }
          tried.push((mode_name(&mode), error.to_string()));
        }
      }
    }
    Err(LightError::NoMode(output.connector.name, tried))
  }

  /// One full modeset of the lit outputs (but those at `except`) and
  /// `extra`, with everything else turned off: what the last probe saw
  /// lit, and the outputs retired since the last full modeset.
  fn request(
    &self,
    extra: &[(&OutputProps, ModesetState)],
    except: &[usize],
  ) -> AtomicRequest {
    let mut states: Vec<(&OutputProps, ModesetState)> = self
      .lit
      .iter()
      .enumerate()
      .filter(|(index, _)| !except.contains(index))
      .map(|(_, lit)| (&lit.output.props, lit.state()))
      .collect();
    states.extend(extra.iter().map(|(props, state)| (*props, *state)));
    let used: Vec<&OutputProps> =
      states.iter().map(|(props, _)| *props).collect();
    let mut teardown =
      self.card.teardown_except(&used).unwrap_or_else(|error| {
        diagnostics::log(&format!("not turning off other outputs: {error}"));
        Teardown::default()
      });
    let retired = self.retired.iter().chain(
      except
        .iter()
        .filter_map(|index| self.lit.get(*index).map(|lit| &lit.output.props)),
    );
    for props in retired {
      let free = |used: &[&OutputProps]| {
        (
          used
            .iter()
            .all(|ours| ours.connector.id != props.connector.id),
          used.iter().all(|ours| ours.crtc.id != props.crtc.id),
          used.iter().all(|ours| ours.plane.id != props.plane.id),
        )
      };
      let (connector, crtc, plane) = free(&used);
      if connector
        && teardown
          .connectors
          .iter()
          .all(|c| c.id != props.connector.id)
      {
        teardown.connectors.push(props.connector);
      }
      if crtc && teardown.crtcs.iter().all(|c| c.id != props.crtc.id) {
        teardown.crtcs.push(props.crtc);
      }
      if plane && teardown.planes.iter().all(|p| p.id != props.plane.id) {
        teardown.planes.push(props.plane);
      }
    }
    atomic::modeset_all(&states, &teardown)
  }

  fn log_lit(&self, lit: &Lit) {
    let (width, height) = lit.mode.size();
    diagnostics::log(&format!(
      "output {}: {width}x{height} at {:.3} Hz, copy scanout",
      lit.name(),
      f64::from(lit.mode.refresh_mhz()) / 1000.0
    ));
  }

  /// Makes the modeset that `prepare` tested, showing a black frame on
  /// every lit output.
  pub fn start(&mut self) -> io::Result<()> {
    if !self.lit.is_empty() {
      self.modeset()?;
    }
    Ok(())
  }

  /// A full modeset of every lit output in one request: `TEST_ONLY` first,
  /// then a blocking `ALLOW_MODESET` commit showing each output's current
  /// buffer, on or off as its DPMS wants, with every other CRTC, plane and
  /// connector turned off. Drawing resumes on those that are on. With
  /// nothing lit and nothing to turn off, nothing is committed.
  fn modeset(&mut self) -> io::Result<()> {
    let request = self.request(&[], &[]);
    if request.is_empty() {
      return Ok(());
    }
    let result = self
      .device
      .commit(
        &request,
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      )
      .and_then(|()| self.device.commit(&request, CommitFlags::ALLOW_MODESET));
    match result {
      Ok(()) => {
        self.retired.clear();
        for lit in &mut self.lit {
          lit.dpms_on = lit.wanted_dpms;
          if lit.dpms_on {
            lit.scheduler.resume();
          } else {
            lit.scheduler.pause();
          }
        }
        Ok(())
      }
      Err(error) => {
        for lit in &mut self.lit {
          lit.scheduler.pause();
        }
        Err(error)
      }
    }
  }

  /// A full modeset of lit output `index` alone, as DPMS on and the
  /// recovery from a lost flip make it, leaving the other outputs and
  /// their pending flips alone.
  fn modeset_one(&mut self, index: usize) -> io::Result<()> {
    let Some(lit) = self.lit.get(index) else {
      return Ok(());
    };
    let request =
      atomic::modeset(&lit.output.props, &lit.state(), &Teardown::default());
    let result = self
      .device
      .commit(
        &request,
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      )
      .and_then(|()| self.device.commit(&request, CommitFlags::ALLOW_MODESET));
    let lit = &mut self.lit[index];
    match result {
      Ok(()) => {
        lit.dpms_on = lit.wanted_dpms;
        if lit.dpms_on {
          lit.scheduler.resume();
        } else {
          lit.scheduler.pause();
        }
        Ok(())
      }
      Err(error) => {
        lit.scheduler.pause();
        Err(error)
      }
    }
  }

  /// The names of the lit outputs, in the order they were lit.
  pub fn outputs(&self) -> Vec<String> {
    self.lit.iter().map(|lit| lit.name().to_owned()).collect()
  }

  fn find(&self, name: &str) -> Option<&Lit> {
    self.lit.iter().find(|lit| lit.name() == name)
  }

  fn find_mut(&mut self, name: &str) -> Option<&mut Lit> {
    self.lit.iter_mut().find(|lit| lit.name() == name)
  }

  /// The first lit output's size in pixels, if one is lit.
  pub fn size(&self) -> Option<(u32, u32)> {
    self.lit.first().map(|lit| lit.mode.size())
  }

  /// Output `name`'s size in pixels, if it is lit.
  pub fn size_of(&self, name: &str) -> Option<(u32, u32)> {
    self.find(name).map(|lit| lit.mode.size())
  }

  /// What clients are told about the first lit output
  /// ([`Display::output_info_of`]).
  pub fn output_info(&self) -> Option<OutputInfo> {
    let lit = self.lit.first()?;
    Some(describe(&lit.output.connector, Some(&lit.mode)))
  }

  /// What clients are told about lit output `name`: the connector's name,
  /// the EDID's make, model and serial ([`Edid::unknown`] without one),
  /// physical size, subpixel layout, the connector's modes and the lit
  /// one.
  pub fn output_info_of(&self, name: &str) -> Option<OutputInfo> {
    let lit = self.find(name)?;
    Some(describe(&lit.output.connector, Some(&lit.mode)))
  }

  /// The connected connectors that are not lit, because they are
  /// configured or turned off, no CRTC is free for them or no mode
  /// passed, with what clients are told about them and the size of the
  /// mode they would get: output management lists them as disabled
  /// heads.
  pub fn unlit(&self) -> Vec<(OutputInfo, (u32, u32))> {
    self
      .card
      .connectors
      .iter()
      .map(|(info, _)| info)
      .filter(|info| info.connection == Connection::Connected)
      .filter(|info| self.find(&info.name).is_none())
      .filter_map(|info| {
        let choice = self.choice(&info.name).unwrap_or_default();
        let mode = outputs::first_mode(info, choice)?;
        Some((describe(info, None), mode.size()))
      })
      .collect()
  }

  /// The name of the first lit connector, if one is lit.
  pub fn output_name(&self) -> Option<&str> {
    self.lit.first().map(Lit::name)
  }

  /// The first lit output's mode, if one is lit.
  pub fn mode(&self) -> Option<&Mode> {
    self.lit.first().map(|lit| &lit.mode)
  }

  /// The first lit output's frame cycle, or `Paused` with none lit.
  pub fn state(&self) -> State {
    self
      .lit
      .first()
      .map_or(State::Paused, |lit| lit.scheduler.state())
  }

  /// Whether every lit output is on; with none lit, whether outputs lit
  /// later start on.
  pub fn dpms_on(&self) -> bool {
    if self.lit.is_empty() {
      return self.wanted_dpms;
    }
    self.lit.iter().all(|lit| lit.dpms_on)
  }

  /// Whether output `name` is lit and on.
  pub fn output_on(&self, name: &str) -> bool {
    self.find(name).is_some_and(|lit| lit.dpms_on)
  }

  /// Whether a frame may be drawn now on the first lit output
  /// ([`Display::ready_on`]).
  pub fn ready(&self) -> bool {
    self.active
      && self
        .lit
        .first()
        .is_some_and(|lit| lit.scheduler.can_render())
  }

  /// Whether a frame may be drawn now on output `name`: it is lit, the
  /// seat is enabled, it is on and no flip of it is pending.
  pub fn ready_on(&self, name: &str) -> bool {
    self.active
      && self
        .find(name)
        .is_some_and(|lit| lit.scheduler.can_render())
  }

  /// Starts a frame on the first lit output if it is ready.
  pub fn begin_frame(&mut self) -> bool {
    match self.output_name().map(str::to_owned) {
      Some(name) => self.begin_frame_on(&name),
      None => false,
    }
  }

  /// Starts a frame on output `name` if [`Display::ready_on`].
  pub fn begin_frame_on(&mut self, name: &str) -> bool {
    self.ready_on(name)
      && self
        .find_mut(name)
        .is_some_and(|lit| lit.scheduler.begin_frame())
  }

  /// Gives up the frame started on the first lit output.
  pub fn abandon_frame(&mut self) {
    if let Some(lit) = self.lit.first_mut() {
      lit.scheduler.abandon();
    }
  }

  /// Gives up the frame started on output `name`.
  pub fn abandon_frame_on(&mut self, name: &str) {
    if let Some(lit) = self.find_mut(name) {
      lit.scheduler.abandon();
    }
  }

  /// [`Display::present_on`] the first lit output.
  pub fn present(&mut self, frame: &Frame, now: Instant) {
    if let Some(name) = self.output_name().map(str::to_owned) {
      self.present_on(&name, frame, now);
    }
  }

  /// Puts `frame` on screen on output `name` at its next vblank: copies it
  /// into the buffer off screen and commits a flip of its CRTC to it. A
  /// refused commit drops the frame; one refused for lack of DRM master
  /// pauses the output until the seat resumes it.
  pub fn present_on(&mut self, name: &str, frame: &Frame, now: Instant) {
    let Some(index) = self.lit.iter().position(|lit| lit.name() == name) else {
      return;
    };
    let lit = &mut self.lit[index];
    let framebuffer = match lit.scanout.prepare(self.device.as_mut(), frame) {
      Ok(framebuffer) => framebuffer,
      Err(error) => {
        diagnostics::log(&format!("copying the frame failed: {error}"));
        lit.scheduler.abandon();
        return;
      }
    };
    let request = atomic::flip(&lit.output.props, framebuffer);
    match self.device.commit(
      &request,
      CommitFlags::NONBLOCK | CommitFlags::PAGE_FLIP_EVENT,
    ) {
      Ok(()) => lit.scheduler.committed(now),
      Err(error) if not_master(&error) => {
        diagnostics::log(&format!(
          "the page flip was refused ({error}); waiting for the seat"
        ));
        lit.scheduler.pause();
      }
      Err(error) => {
        diagnostics::log(&format!("the page flip of {name} failed: {error}"));
        lit.scheduler.abandon();
      }
    }
  }

  /// [`Display::dispatch_flips`] without the outputs' names, for one
  /// output.
  pub fn dispatch(&mut self) -> Vec<PresentInfo> {
    self
      .dispatch_flips()
      .into_iter()
      .map(|(_, info)| info)
      .collect()
  }

  /// Reads page-flip events, routes each to the output of its CRTC and
  /// returns what each completed flip presented, with the output's name.
  /// Stale flips, such as one from before a pause or of an output that is
  /// gone, return nothing. DPMS changes waiting for a flip are made
  /// afterwards.
  pub fn dispatch_flips(&mut self) -> Vec<(String, PresentInfo)> {
    let flips = match self.device.read_events() {
      Ok(flips) => flips,
      Err(error) => {
        diagnostics::log(&format!("reading DRM events failed: {error}"));
        return Vec::new();
      }
    };
    let mut presented = Vec::new();
    for flip in flips {
      let Some(lit) = self.lit.iter_mut().find(|lit| lit.crtc() == flip.crtc)
      else {
        continue;
      };
      if !lit.scheduler.page_flip() {
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
      let info = PresentInfo {
        time,
        refresh_ns: lit.mode.refresh_ns(),
        seq: lit.sequence.extend(flip.sequence),
        flags,
      };
      presented.push((lit.name().to_owned(), info));
    }
    self.apply_dpms();
    presented
  }

  /// When the first pending flip of any output times out.
  pub fn deadline(&self) -> Option<Instant> {
    self
      .lit
      .iter()
      .filter_map(|lit| lit.scheduler.deadline())
      .min()
  }

  /// Checks every output's pending flip at `now`. A flip pending for a
  /// second is logged and dropped, and a blocking modeset of that output
  /// shows its current buffer again. The third timeout in a row of one
  /// output is [`Fatal`].
  pub fn check_timeout(&mut self, now: Instant) -> Result<(), Fatal> {
    for index in 0..self.lit.len() {
      let Some(timeout) = self.lit[index].scheduler.check_timeout(now) else {
        continue;
      };
      diagnostics::log(&format!(
        "{}: the page flip did not complete within 1 s ({} in a row)",
        self.lit[index].name(),
        timeout.consecutive
      ));
      if timeout.fatal() {
        return Err(Fatal(format!(
          "{} page flips in a row did not complete",
          timeout.consecutive
        )));
      }
      if let Err(error) = self.modeset_one(index) {
        diagnostics::log(&format!("the recovery modeset failed: {error}"));
        // Try again at the next frame.
        let active = self.active;
        let lit = &mut self.lit[index];
        if lit.dpms_on && active {
          lit.scheduler.resume();
        }
      }
    }
    self.apply_dpms();
    Ok(())
  }

  /// Stops committing for a disabled seat. Pending flips are forgotten;
  /// their events may never come.
  pub fn pause(&mut self) {
    self.active = false;
    for lit in &mut self.lit {
      lit.scheduler.pause();
    }
  }

  /// The lit outputs' names and modes.
  fn snapshot(&self) -> Snapshot {
    self
      .lit
      .iter()
      .map(|lit| (lit.name().to_owned(), lit.mode.clone()))
      .collect()
  }

  /// What changed since the lit outputs were `before`.
  fn changes_since(&self, before: &Snapshot) -> Resumed {
    let after = self.snapshot();
    let first = |snapshot: &Snapshot| {
      snapshot
        .first()
        .map(|(name, mode)| (name.clone(), mode.size()))
    };
    let (then, now) = (first(before), first(&after));
    Resumed {
      resized: now
        .as_ref()
        .filter(|(_, size)| Some(*size) != then.as_ref().map(|(_, s)| *s))
        .map(|(_, size)| *size),
      output_changed: now.map(|(name, _)| name) != then.map(|(name, _)| name),
      outputs_changed: after != *before,
    }
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

  /// Probes the card again and remembers it, logging what changed.
  fn reprobe(&mut self) -> io::Result<Vec<ConnectorChange>> {
    let card = output::probe(self.device.as_mut())?;
    let changes = self.log_changes(&card);
    self.card = card;
    Ok(changes)
  }

  /// Keeps the lit outputs whose connector is still connected, still
  /// offers their mode, is not off and passes `keep`, re-reading their
  /// properties, on their CRTCs. The others are dropped, to be turned off
  /// by the next full modeset. Returns whether any was dropped.
  fn keep_lit(
    &mut self,
    keep: &dyn Fn(&Self, &Lit, &ConnectorInfo) -> bool,
  ) -> bool {
    let mut kept = Vec::new();
    let mut dropped = false;
    for mut lit in std::mem::take(&mut self.lit) {
      let connector = self
        .card
        .connector(lit.output.connector.id)
        .filter(|info| info.connection == Connection::Connected)
        .filter(|info| info.modes.contains(&lit.mode))
        .filter(|info| self.choice(&info.name).is_some())
        .filter(|info| keep(self, &lit, info))
        .cloned();
      let refreshed = connector.and_then(|connector| {
        let device = self.device.as_mut();
        self
          .card
          .output_on(&connector, lit.crtc(), lit.plane(), &mut |blob| {
            device.blob(blob).ok()
          })
          .map_err(|error| {
            diagnostics::log(&format!("{}: {error}", connector.name));
          })
          .ok()
      });
      match refreshed {
        Some(output) => {
          lit.output = output;
          kept.push(lit);
        }
        None => {
          dropped = true;
          self.retired.push(lit.output.props.clone());
          lit.destroy(self.device.as_mut());
        }
      }
    }
    self.lit = kept;
    dropped
  }

  /// Lights the connected connectors that are not lit yet, as far as
  /// CRTCs allow. Returns whether any was lit.
  fn light_ranked(&mut self) -> bool {
    let before = self.lit.len();
    let ranked = self.ranked();
    match self.light_new(&ranked) {
      Ok(failures) => self.log_failures(failures),
      Err(error) => diagnostics::log(&format!(
        "{}; nothing more is lit",
        error.into_start_error()
      )),
    }
    self.lit.len() > before
  }

  /// Drops every lit output and lights the connected ones again from the
  /// start, as after a resume whose modeset failed.
  fn relight_all(&mut self) {
    for lit in std::mem::take(&mut self.lit) {
      self.retired.push(lit.output.props.clone());
      lit.destroy(self.device.as_mut());
    }
    self.light_ranked();
    if let Err(error) = self.modeset() {
      diagnostics::log(&format!("the modeset failed: {error}"));
    }
    if self.lit.is_empty() {
      diagnostics::log("no connected output; nothing is shown until one is");
    }
  }

  /// Resumes for an enabled seat: probes the card again, since outputs
  /// may have changed while away, and makes a full modeset of every
  /// output, because the other session may have changed modes. Lit
  /// outputs stay if they are still connected and offer their mode;
  /// connectors plugged in meanwhile are lit as CRTCs allow. If the
  /// modeset fails, every output is chosen again from the start.
  pub fn resume(&mut self) -> Resumed {
    self.active = true;
    let before = self.snapshot();
    if let Err(error) = self.reprobe() {
      diagnostics::log(&format!("probing the card failed: {error}"));
      if let Err(error) = self.modeset() {
        diagnostics::log(&format!("the modeset failed: {error}"));
      }
      return Resumed::default();
    }
    self.keep_lit(&|_, _, _| true);
    self.light_ranked();
    if let Err(error) = self.modeset() {
      diagnostics::log(&format!("the modeset after resume failed: {error}"));
      self.relight_all();
    }
    if self.lit.is_empty() && !before.is_empty() {
      diagnostics::log("no connected output; nothing is shown until one is");
    }
    self.changes_since(&before)
  }

  /// Follows a DRM hotplug: probes the card again and diffs it with the
  /// last probe. Lit outputs stay, without a modeset, while they are
  /// connected and offer their mode; unplugged ones are dropped and newly
  /// connected ones lit as CRTCs allow ([`outputs::rank`],
  /// [`Card::allocate`]), with one full modeset of every output if
  /// anything changed. With nothing connected, none is lit and nothing is
  /// drawn. While the seat is disabled this does nothing, since the resume
  /// probes anyway.
  pub fn hotplug(&mut self) -> Resumed {
    if !self.active {
      return Resumed::default();
    }
    let before = self.snapshot();
    let changes = match self.reprobe() {
      Ok(changes) => changes,
      Err(error) => {
        diagnostics::log(&format!("probing the card failed: {error}"));
        return Resumed::default();
      }
    };
    let dropped = self.keep_lit(&|_, _, _| true);
    let lit = self.light_ranked();
    if dropped || lit {
      if let Err(error) = self.modeset() {
        diagnostics::log(&format!("the modeset failed: {error}"));
      }
      if self.lit.is_empty() {
        diagnostics::log("no connected output; nothing is shown until one is");
      }
    }
    let mut change = self.changes_since(&before);
    // A kept output whose monitor was swapped has another description.
    if let Some(first) = self.output_name()
      && changes.iter().any(|change| change_name(change) == first)
    {
      change.output_changed = true;
    }
    change
  }

  /// Takes a reloaded configuration. Lit outputs that it gives another
  /// mode or turns off, and connectors it now names, are lit again or
  /// turned off as it says; outputs that output management configured
  /// keep what it set.
  pub fn set_policy(&mut self, policy: OutputPolicy) -> Resumed {
    if policy == self.policy {
      return Resumed::default();
    }
    self.policy = policy;
    if !self.active {
      return Resumed::default();
    }
    let before = self.snapshot();
    if let Err(error) = self.reprobe() {
      diagnostics::log(&format!("probing the card failed: {error}"));
      return Resumed::default();
    }
    let dropped = self.keep_lit(&|display, lit, connector| {
      display.managed.contains_key(lit.name())
        || display
          .choice(&connector.name)
          .and_then(|choice| outputs::first_mode(connector, choice))
          .as_ref()
          == Some(&lit.mode)
    });
    let lit = self.light_ranked();
    if (dropped || lit)
      && let Err(error) = self.modeset()
    {
      diagnostics::log(&format!("the modeset failed: {error}"));
    }
    self.changes_since(&before)
  }

  /// The name of the first lit output, for the single-output calls, or
  /// why there is none.
  fn first_for_request(&self) -> Result<String, String> {
    if !self.active {
      return Err("the seat is not active".into());
    }
    self
      .output_name()
      .map(str::to_owned)
      .ok_or_else(|| "no output is lit".into())
  }

  /// Whether `request` names a mode of the first lit output that passes a
  /// `TEST_ONLY` modeset ([`Display::test_configuration`]).
  pub fn test_mode(&mut self, request: RequestedMode) -> Result<(), String> {
    let name = self.first_for_request()?;
    self.test_configuration(&[(name, HeadState::On(Some(request)))])
  }

  /// Sets the mode `request` names on the first lit output
  /// ([`Display::apply_configuration`]).
  pub fn set_mode(
    &mut self,
    request: RequestedMode,
  ) -> Result<Resumed, String> {
    let name = self.first_for_request()?;
    self.apply_configuration(&[(name, HeadState::On(Some(request)))])
  }

  /// Whether the outputs `heads` names can be as it says, with the other
  /// lit outputs unchanged: one `TEST_ONLY` modeset of the whole new
  /// configuration, with fresh buffers and blobs for new modes and for
  /// outputs turned on, which get a free CRTC.
  pub fn test_configuration(
    &mut self,
    heads: &[(String, HeadState)],
  ) -> Result<(), String> {
    let plan = self.plan(heads)?;
    let result = self.test_plan(&plan);
    plan.discard(self.device.as_mut());
    result
  }

  /// Makes the outputs `heads` names as it says, with the other lit
  /// outputs unchanged: a `TEST_ONLY` modeset of the whole configuration,
  /// then the full modeset ([`Display::test_configuration`]). If either
  /// fails, the old configuration stays. Config reloads leave the outputs
  /// it configured alone until Shingle restarts. The caller waits until no
  /// flip is pending.
  pub fn apply_configuration(
    &mut self,
    heads: &[(String, HeadState)],
  ) -> Result<Resumed, String> {
    let plan = self.plan(heads)?;
    if let Err(error) = self.test_plan(&plan) {
      plan.discard(self.device.as_mut());
      return Err(error);
    }
    let before = self.snapshot();
    let names = plan.mode_names();
    let Plan {
      off,
      changed,
      added,
    } = plan;
    // Swap the new configuration in, keeping the old one for a failure.
    let mut old_modes = Vec::new();
    for (index, mode, blob, scanout) in changed {
      let lit = &mut self.lit[index];
      let old = (
        index,
        std::mem::replace(&mut lit.mode, mode),
        std::mem::replace(&mut lit.blob, blob),
        std::mem::replace(&mut lit.scanout, Box::new(scanout)),
      );
      old_modes.push(old);
    }
    let mut removed = Vec::new();
    let mut off = off;
    off.sort_unstable();
    for index in off.into_iter().rev() {
      let lit = self.lit.remove(index);
      self.retired.push(lit.output.props.clone());
      removed.push((index, lit));
    }
    let added_count = added.len();
    for (output, mode, blob, scanout) in added {
      self.lit.push(Lit {
        output,
        mode,
        blob,
        scanout: Box::new(scanout),
        scheduler: FrameScheduler::default(),
        sequence: Sequence::default(),
        dpms_on: true,
        wanted_dpms: self.wanted_dpms,
      });
    }
    match self.modeset() {
      Ok(()) => {
        for (_, _, blob, mut scanout) in old_modes {
          scanout.destroy(self.device.as_mut());
          let _ = self.device.destroy_blob(blob);
        }
        for (_, lit) in removed {
          self.managed.insert(lit.name().to_owned(), OutputMode::Off);
          lit.destroy(self.device.as_mut());
        }
        for lit in &self.lit {
          let changed = before
            .iter()
            .find(|(name, _)| name == lit.name())
            .is_none_or(|(_, mode)| *mode != lit.mode);
          if changed {
            self.log_lit(lit);
          }
        }
        let managed: Vec<(String, OutputMode)> = self
          .lit
          .iter()
          .filter(|lit| heads.iter().any(|(name, _)| name == lit.name()))
          .map(|lit| {
            let (width, height) = lit.mode.size();
            let mode = OutputMode::Explicit {
              width,
              height,
              refresh_mhz: lit.mode.refresh_mhz(),
            };
            (lit.name().to_owned(), mode)
          })
          .collect();
        self.managed.extend(managed);
        Ok(self.changes_since(&before))
      }
      Err(error) => {
        for lit in self.lit.drain(self.lit.len() - added_count..) {
          lit.destroy(self.device.as_mut());
        }
        for (index, lit) in removed.into_iter().rev() {
          self.retired.retain(|props| *props != lit.output.props);
          let index = index.min(self.lit.len());
          self.lit.insert(index, lit);
        }
        for (index, mode, blob, scanout) in old_modes {
          let lit = &mut self.lit[index];
          lit.mode = mode;
          let new_blob = std::mem::replace(&mut lit.blob, blob);
          let mut new_scanout = std::mem::replace(&mut lit.scanout, scanout);
          new_scanout.destroy(self.device.as_mut());
          let _ = self.device.destroy_blob(new_blob);
        }
        if let Err(error) = self.modeset() {
          diagnostics::log(&format!(
            "restoring the old configuration failed: {error}"
          ));
        }
        Err(format!("{names}: {error}"))
      }
    }
  }

  /// Resolves what `heads` asks into modes, CRTCs and fresh buffers.
  fn plan(&mut self, heads: &[(String, HeadState)]) -> Result<Plan, String> {
    if !self.active {
      return Err("the seat is not active".into());
    }
    let mut off = Vec::new();
    let mut changes = Vec::new();
    let mut new = Vec::new();
    for (name, state) in heads {
      let index = self.lit.iter().position(|lit| lit.name() == name);
      match (index, state) {
        (Some(index), HeadState::Off) => off.push(index),
        (Some(_), HeadState::On(None)) | (None, HeadState::Off) => {}
        (Some(index), HeadState::On(Some(request))) => {
          let lit = &self.lit[index];
          let mode =
            outputs::requested_mode(&lit.output.connector.modes, *request)
              .ok_or("the output does not offer that mode")?;
          if mode != lit.mode {
            changes.push((index, mode));
          }
        }
        (None, HeadState::On(request)) => {
          let connector = self
            .card
            .connectors
            .iter()
            .map(|(info, _)| info)
            .find(|info| {
              &info.name == name && info.connection == Connection::Connected
            })
            .ok_or_else(|| format!("{name} is not connected"))?
            .clone();
          let mode = match request {
            Some(request) => {
              outputs::requested_mode(&connector.modes, *request)
                .ok_or("the output does not offer that mode")?
            }
            None => {
              let choice = self.choice(name).unwrap_or_default();
              outputs::first_mode(&connector, choice)
                .ok_or_else(|| format!("{name} offers no mode"))?
            }
          };
          new.push((connector, mode));
        }
      }
    }
    // Outputs turned on get CRTCs among those the others leave free.
    let staying: Vec<(ConnectorInfo, (CrtcId, PlaneId))> = self
      .lit
      .iter()
      .enumerate()
      .filter(|(index, _)| !off.contains(index))
      .map(|(_, lit)| (lit.output.connector.clone(), (lit.crtc(), lit.plane())))
      .collect();
    let mut wants: Vec<Wanted<'_>> = staying
      .iter()
      .map(|(connector, pair)| Wanted {
        connector,
        lit: Some(*pair),
      })
      .collect();
    wants.extend(new.iter().map(|(connector, _)| Wanted {
      connector,
      lit: None,
    }));
    let allocated = self.card.allocate(&wants)[staying.len()..].to_vec();
    let mut pairs = Vec::new();
    for ((connector, _), pair) in new.iter().zip(allocated) {
      let pair = pair
        .ok_or_else(|| format!("no CRTC is free for {}", connector.name))?;
      pairs.push(pair);
    }
    let mut plan = Plan {
      off,
      changed: Vec::new(),
      added: Vec::new(),
    };
    for (index, mode) in changes {
      match fresh(self.device.as_mut(), &mode) {
        Ok((blob, scanout)) => plan.changed.push((index, mode, blob, scanout)),
        Err(error) => {
          plan.discard(self.device.as_mut());
          return Err(format!("{}: {error}", mode_name(&mode)));
        }
      }
    }
    for ((connector, mode), (crtc, plane)) in new.into_iter().zip(pairs) {
      let device = self.device.as_mut();
      let output =
        match self.card.output_on(&connector, crtc, plane, &mut |blob| {
          device.blob(blob).ok()
        }) {
          Ok(output) => output,
          Err(error) => {
            plan.discard(self.device.as_mut());
            return Err(error.to_string());
          }
        };
      match fresh(self.device.as_mut(), &mode) {
        Ok((blob, scanout)) => plan.added.push((output, mode, blob, scanout)),
        Err(error) => {
          plan.discard(self.device.as_mut());
          return Err(format!("{}: {error}", mode_name(&mode)));
        }
      }
    }
    Ok(plan)
  }

  /// One `TEST_ONLY` modeset of what `plan` makes of the lit outputs.
  fn test_plan(&mut self, plan: &Plan) -> Result<(), String> {
    let changed: Vec<usize> =
      plan.changed.iter().map(|(index, ..)| *index).collect();
    let mut except = plan.off.clone();
    except.extend(&changed);
    let mut extra: Vec<(&OutputProps, ModesetState)> = plan
      .changed
      .iter()
      .map(|(index, mode, blob, scanout)| {
        let lit = &self.lit[*index];
        let state = ModesetState {
          active: lit.wanted_dpms,
          ..fresh_state(mode, *blob, scanout)
        };
        (&lit.output.props, state)
      })
      .collect();
    extra.extend(plan.added.iter().map(|(output, mode, blob, scanout)| {
      (&output.props, fresh_state(mode, *blob, scanout))
    }));
    // Outputs whose mode changes are in `extra` with the new mode, so the
    // teardown of those left out spares them.
    let request = self.request(&extra, &except);
    self
      .device
      .commit(
        &request,
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      )
      .map_err(|error| format!("{}: {error}", plan.mode_names()))
  }

  /// Whether a page flip is pending on any output, during which no full
  /// modeset may be made.
  pub fn flip_pending(&self) -> bool {
    self.lit.iter().any(|lit| lit.scheduler.flip_pending())
  }

  /// Turns every output on or off, as the idle timeout asks, and outputs
  /// lit from now on start that way. While an output's flip is pending,
  /// or the seat is disabled, its change waits; a resume applies it with
  /// its modeset.
  pub fn set_dpms(&mut self, on: bool) {
    self.wanted_dpms = on;
    for lit in &mut self.lit {
      lit.wanted_dpms = on;
    }
    self.apply_dpms();
  }

  /// Turns output `name` on or off, as wlr-output-power-management asks,
  /// with the same waiting as [`Display::set_dpms`].
  pub fn set_output_dpms(&mut self, name: &str, on: bool) {
    if let Some(lit) = self.find_mut(name) {
      lit.wanted_dpms = on;
    }
    self.apply_dpms();
  }

  /// Makes each output's DPMS change that can be made now: off is a
  /// commit of only its CRTC's `ACTIVE`, on a full modeset of it alone.
  fn apply_dpms(&mut self) {
    if !self.active {
      return;
    }
    for index in 0..self.lit.len() {
      let lit = &self.lit[index];
      if lit.wanted_dpms == lit.dpms_on || lit.scheduler.flip_pending() {
        continue;
      }
      let name = lit.name().to_owned();
      if lit.wanted_dpms {
        if let Err(error) = self.modeset_one(index) {
          diagnostics::log(&format!("turning {name} on failed: {error}"));
        }
        continue;
      }
      let request = atomic::set_active(&lit.output.props, false);
      let result = self
        .device
        .commit(
          &request,
          CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
        )
        .and_then(|()| {
          self.device.commit(&request, CommitFlags::ALLOW_MODESET)
        });
      match result {
        Ok(()) => {
          let lit = &mut self.lit[index];
          lit.dpms_on = false;
          lit.scheduler.pause();
        }
        Err(error) => {
          diagnostics::log(&format!("turning {name} off failed: {error}"));
        }
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
