//! The GPU side of the native backend: which card to open, which Vulkan
//! adapter renders for it, and how rendering failures are survived.
//!
//! Card selection follows docs/design/native-backend.md, section 2: the
//! card with `boot_vga=1`, else the first, unless `SHINGLE_DRM_DEVICE`
//! names one. [`enumerate_cards`] reads the cards from sysfs (a directory
//! the caller passes, so tests use a temporary tree). The renderer is the
//! adapter on the card's PCI slot ([`adapter_matches`]); the copy path
//! accepts any adapter when none matches.
//!
//! [`FrameRenderer`] is what the host draws frames with. The real one,
//! [`GpuRenderer`], draws the frontend's scene offscreen and reads it back
//! for copy scanout; tests use a fake. [`GpuHealth`] decides what a
//! failure means: a missed 100 ms wait is skipped, three in a row or a
//! lost device recreate the renderer, and a third loss within 60 s ends
//! Shingle.

use std::{
  collections::VecDeque,
  ffi::OsStr,
  fmt, fs, io,
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use shingle_frontend::{Frontend, host::Host, render_target::RenderTarget};
use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, OffscreenTarget, RenderError,
  RenderState, RenderTile, TextureTransform,
};

use crate::scanout::Frame;

/// A DRM card as sysfs describes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
  /// Its device node, such as `/dev/dri/card0`.
  pub devnode: PathBuf,
  /// Whether the firmware used it for the boot console.
  pub boot_vga: bool,
  /// Its PCI slot, such as `0000:01:00.0`, if it is a PCI device.
  pub pci_slot: Option<String>,
}

/// The number of a `cardN` name, which excludes connector entries such as
/// `card0-DP-1` and render nodes.
fn card_number(name: &str) -> Option<u32> {
  let digits = name.strip_prefix("card")?;
  if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  digits.parse().ok()
}

/// The PCI slot from a sysfs `uevent` file's `PCI_SLOT_NAME=` line.
fn pci_slot(uevent: &str) -> Option<String> {
  uevent
    .lines()
    .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
    .map(|slot| slot.trim().to_owned())
}

/// Lists the cards under `root` (the real one is `/sys/class/drm`), by
/// number. Their nodes are under `/dev/dri`.
pub fn enumerate_cards(root: &Path) -> io::Result<Vec<Card>> {
  let mut cards = Vec::new();
  for entry in fs::read_dir(root)? {
    let entry = entry?;
    let name = entry.file_name();
    let Some(number) = name.to_str().and_then(card_number) else {
      continue;
    };
    let device = entry.path().join("device");
    let boot_vga = fs::read_to_string(device.join("boot_vga"))
      .is_ok_and(|value| value.trim() == "1");
    let pci_slot = fs::read_to_string(device.join("uevent"))
      .ok()
      .and_then(|uevent| pci_slot(&uevent));
    cards.push((
      number,
      Card {
        devnode: Path::new("/dev/dri").join(&name),
        boot_vga,
        pci_slot,
      },
    ));
  }
  cards.sort_by_key(|(number, _)| *number);
  Ok(cards.into_iter().map(|(_, card)| card).collect())
}

/// The cards to try opening, in order: only `forced` if
/// `SHINGLE_DRM_DEVICE` is set (with the PCI slot sysfs knows for it),
/// else the boot VGA card first and the rest by number.
pub fn card_order(cards: &[Card], forced: Option<&OsStr>) -> Vec<Card> {
  if let Some(path) = forced.filter(|path| !path.is_empty()) {
    let path = Path::new(path);
    return vec![
      cards
        .iter()
        .find(|card| card.devnode == path)
        .cloned()
        .unwrap_or(Card {
          devnode: path.to_owned(),
          boot_vga: false,
          pci_slot: None,
        }),
    ];
  }
  let mut order: Vec<Card> =
    cards.iter().filter(|card| card.boot_vga).cloned().collect();
  order.extend(cards.iter().filter(|card| !card.boot_vga).cloned());
  order
}

/// Whether an adapter's `device_pci_bus_id` is the card's PCI slot.
pub fn adapter_matches(bus_id: &str, slot: &str) -> bool {
  !bus_id.is_empty() && bus_id.eq_ignore_ascii_case(slot)
}

/// How long the renderer waits for the GPU to finish a frame.
pub const GPU_WAIT: Duration = Duration::from_millis(100);
/// How many waits in a row may run out before the GPU counts as hung.
pub const MAX_GPU_WAITS: u32 = 3;
/// How many GPU losses within [`GPU_LOSS_WINDOW`] end Shingle.
pub const MAX_GPU_LOSSES: usize = 3;
/// The window for [`MAX_GPU_LOSSES`].
pub const GPU_LOSS_WINDOW: Duration = Duration::from_secs(60);

/// Why a frame could not be drawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderFailure {
  /// The device was lost: the device-lost callback fired or wgpu said so.
  Lost(String),
  /// The GPU did not finish within [`GPU_WAIT`].
  Timeout,
  /// Anything else, such as a texture that could not be uploaded.
  Failed(String),
}

impl fmt::Display for RenderFailure {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Lost(reason) => {
        write!(formatter, "the GPU device was lost ({reason})")
      }
      Self::Timeout => write!(
        formatter,
        "the GPU did not finish a frame within {} ms",
        GPU_WAIT.as_millis()
      ),
      Self::Failed(reason) => write!(formatter, "rendering failed: {reason}"),
    }
  }
}

/// What to do after a rendering failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
  /// Skip the frame and carry on.
  Skip,
  /// Drop the GPU context and create a new one.
  Recreate,
  /// Save the session and exit with status 2.
  GiveUp,
}

/// Counts rendering failures to tell a slow frame from a hung or lost
/// GPU (docs/design/native-backend.md, section 6).
#[derive(Clone, Debug, Default)]
pub struct GpuHealth {
  timeouts: u32,
  losses: VecDeque<Instant>,
}

impl GpuHealth {
  /// A frame was drawn.
  pub fn success(&mut self) {
    self.timeouts = 0;
  }

  /// A frame failed at `now` with `failure`.
  pub fn failure(&mut self, failure: &RenderFailure, now: Instant) -> Verdict {
    match failure {
      RenderFailure::Failed(_) => Verdict::Skip,
      RenderFailure::Timeout => {
        self.timeouts += 1;
        if self.timeouts < MAX_GPU_WAITS {
          Verdict::Skip
        } else {
          self.lost(now)
        }
      }
      RenderFailure::Lost(_) => self.lost(now),
    }
  }

  fn lost(&mut self, now: Instant) -> Verdict {
    self.timeouts = 0;
    while self
      .losses
      .front()
      .is_some_and(|loss| now.duration_since(*loss) >= GPU_LOSS_WINDOW)
    {
      self.losses.pop_front();
    }
    self.losses.push_back(now);
    if self.losses.len() >= MAX_GPU_LOSSES {
      Verdict::GiveUp
    } else {
      Verdict::Recreate
    }
  }
}

/// Draws the frontend's frames for scanout.
pub trait FrameRenderer {
  /// Draws the frontend's scene and returns it as a [`Frame`].
  fn draw(
    &mut self,
    frontend: &mut Frontend,
    host: &mut dyn Host,
  ) -> Result<Frame, RenderFailure>;

  /// Changes the frame size, for a new mode.
  fn resize(&mut self, size: (u32, u32)) -> Result<(), RenderFailure>;

  /// The GPU context, for shm uploads and linux-dmabuf
  /// ([`Frontend::attach_gpu`]); `None` for renderers without one.
  fn gpu(&self) -> Option<&GpuContext> {
    None
  }
}

/// Creates a renderer for frames of a size; called again to recreate it
/// after the GPU was lost.
pub type RendererFactory =
  Box<dyn FnMut((u32, u32)) -> Result<Box<dyn FrameRenderer>, String>>;

/// The real renderer: the frontend's scene drawn into an RGBA offscreen
/// target and read back within [`GPU_WAIT`], for copy scanout.
pub struct GpuRenderer {
  gpu: GpuContext,
  state: RenderState,
  target: OffscreenTarget,
  /// Set by wgpu's device-lost callback.
  lost: Arc<AtomicBool>,
  /// What the last call of `render` produced.
  last: Option<Result<Frame, RenderFailure>>,
}

impl GpuRenderer {
  /// Opens the Vulkan adapter on PCI slot `pci` (any adapter when there
  /// is none or no slot is known) and an offscreen target of `size`.
  pub fn new(size: (u32, u32), pci: Option<&str>) -> Result<Self, String> {
    let gpu = open_gpu(pci).map_err(|error| error.to_string())?;
    let lost = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&lost);
    gpu.device().set_device_lost_callback(move |_, _| {
      flag.store(true, Ordering::SeqCst);
    });
    let target = OffscreenTarget::new(&gpu, size.0, size.1)
      .map_err(|error| error.to_string())?;
    Ok(Self {
      gpu,
      state: RenderState::default(),
      target,
      lost,
      last: None,
    })
  }

  /// The adapter's name, for the log.
  pub fn adapter_name(&self) -> String {
    self.gpu.adapter().get_info().name
  }

  /// [`GpuContext::dmabuf_summary`], for the log.
  pub fn dmabuf_summary(&self) -> String {
    self.gpu.dmabuf_summary()
  }

  fn lost(&self) -> bool {
    self.lost.load(Ordering::SeqCst)
  }
}

fn open_gpu(pci: Option<&str>) -> Result<GpuContext, RenderError> {
  if let Some(slot) = pci {
    let filter =
      |info: &wgpu::AdapterInfo| adapter_matches(&info.device_pci_bus_id, slot);
    let options = GpuOptions {
      adapter_filter: Some(&filter),
      dmabuf: true,
      ..GpuOptions::default()
    };
    match pollster::block_on(GpuContext::new(
      GpuContext::create_instance(),
      options,
    )) {
      Err(RenderError::NoMatchingAdapter) => crate::diagnostics::log(&format!(
        "no Vulkan adapter on PCI {slot}; copy scanout renders on any"
      )),
      result => return result,
    }
  }
  pollster::block_on(GpuContext::new(
    GpuContext::create_instance(),
    GpuOptions {
      dmabuf: true,
      ..GpuOptions::default()
    },
  ))
}

impl RenderTarget for GpuRenderer {
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

  // Tearing needs async flips, which are NB-10's.
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

  /// Draws and reads the frame back for [`FrameRenderer::draw`]. Returns
  /// false: the frame is presented when its page flip completes, not
  /// now.
  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    if self.lost() {
      self.last = Some(Err(RenderFailure::Lost("device lost".into())));
      return Ok(false);
    }
    self.target.render(&self.gpu, &self.state, tiles, clear);
    let (width, height) = self.target.size();
    self.last = Some(
      match self.target.read_rgba_within(&self.gpu, Some(GPU_WAIT)) {
        Ok(rgba) => Ok(Frame {
          width,
          height,
          rgba,
        }),
        Err(RenderError::Timeout(_)) => Err(RenderFailure::Timeout),
        Err(_) if self.lost() => Err(RenderFailure::Lost("device lost".into())),
        Err(error) => Err(RenderFailure::Failed(error.to_string())),
      },
    );
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

impl FrameRenderer for GpuRenderer {
  fn gpu(&self) -> Option<&GpuContext> {
    Some(&self.gpu)
  }

  fn draw(
    &mut self,
    frontend: &mut Frontend,
    host: &mut dyn Host,
  ) -> Result<Frame, RenderFailure> {
    self.last = None;
    // Native lights the compositor's first output until MO-7.
    let output = frontend.compositor().primary_output();
    let result = frontend.render(host, &output, self);
    match (self.last.take(), result) {
      (Some(frame), _) => frame,
      (None, _) if self.lost() => {
        Err(RenderFailure::Lost("device lost".into()))
      }
      (None, Err(error)) => Err(RenderFailure::Failed(error.to_string())),
      (None, Ok(())) => Err(RenderFailure::Failed("nothing was drawn".into())),
    }
  }

  fn resize(
    &mut self,
    (width, height): (u32, u32),
  ) -> Result<(), RenderFailure> {
    self.target = OffscreenTarget::new(&self.gpu, width, height)
      .map_err(|error| RenderFailure::Failed(error.to_string()))?;
    Ok(())
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use std::{cell::RefCell, rc::Rc};

  use super::*;

  /// A renderer that draws nothing and returns frames of one colour,
  /// failing as scripted.
  pub(crate) struct FakeRenderer {
    pub(crate) size: (u32, u32),
    pub(crate) failures: Rc<RefCell<VecDeque<RenderFailure>>>,
    pub(crate) frames: Rc<RefCell<u32>>,
  }

  impl FrameRenderer for FakeRenderer {
    fn draw(
      &mut self,
      _frontend: &mut Frontend,
      _host: &mut dyn Host,
    ) -> Result<Frame, RenderFailure> {
      if let Some(failure) = self.failures.borrow_mut().pop_front() {
        return Err(failure);
      }
      *self.frames.borrow_mut() += 1;
      let (width, height) = self.size;
      Ok(Frame {
        width,
        height,
        rgba: [10, 20, 30, 255].repeat((width * height) as usize),
      })
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<(), RenderFailure> {
      self.size = size;
      Ok(())
    }
  }

  fn sysfs(cards: &[(&str, Option<&str>, Option<&str>)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
      "shingle-native-sysfs-{}-{}",
      std::process::id(),
      cards.len()
    ));
    let _ = fs::remove_dir_all(&root);
    for (name, boot_vga, slot) in cards {
      let device = root.join(name).join("device");
      fs::create_dir_all(&device).unwrap();
      if let Some(value) = boot_vga {
        fs::write(device.join("boot_vga"), format!("{value}\n")).unwrap();
      }
      if let Some(slot) = slot {
        let uevent = format!("DRIVER=amdgpu\nPCI_SLOT_NAME={slot}\n");
        fs::write(device.join("uevent"), uevent).unwrap();
      }
    }
    root
  }

  #[test]
  fn cards_come_from_sysfs_by_number() {
    let root = sysfs(&[
      ("card10", Some("0"), None),
      ("card1", Some("1"), Some("0000:01:00.0")),
      ("card1-DP-1", None, None),
      ("renderD128", None, None),
      ("card0", None, Some("0000:00:02.0")),
    ]);
    let cards = enumerate_cards(&root).unwrap();
    let nodes: Vec<_> = cards.iter().map(|card| card.devnode.clone()).collect();
    assert_eq!(
      nodes,
      ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card10"]
        .map(PathBuf::from)
    );
    assert!(cards[1].boot_vga);
    assert!(!cards[0].boot_vga);
    assert_eq!(cards[1].pci_slot.as_deref(), Some("0000:01:00.0"));
    assert_eq!(cards[2].pci_slot, None);
    fs::remove_dir_all(root).unwrap();
  }

  fn card(n: u32, boot_vga: bool) -> Card {
    Card {
      devnode: PathBuf::from(format!("/dev/dri/card{n}")),
      boot_vga,
      pci_slot: Some(format!("0000:0{n}:00.0")),
    }
  }

  #[test]
  fn the_boot_vga_card_is_tried_first() {
    let cards = [card(0, false), card(1, true), card(2, false)];
    let order: Vec<_> = card_order(&cards, None)
      .into_iter()
      .map(|card| card.devnode)
      .collect();
    assert_eq!(
      order,
      ["/dev/dri/card1", "/dev/dri/card0", "/dev/dri/card2"].map(PathBuf::from)
    );
    assert!(card_order(&[], None).is_empty());
  }

  #[test]
  fn shingle_drm_device_overrides_the_choice() {
    let cards = [card(0, true), card(1, false)];
    let forced = card_order(&cards, Some(OsStr::new("/dev/dri/card1")));
    assert_eq!(forced, vec![card(1, false)]);
    let unknown = card_order(&cards, Some(OsStr::new("/dev/dri/card7")));
    assert_eq!(unknown[0].devnode, PathBuf::from("/dev/dri/card7"));
    assert_eq!(unknown[0].pci_slot, None);
    assert_eq!(card_order(&cards, Some(OsStr::new(""))).len(), 2);
  }

  #[test]
  fn adapters_match_by_pci_slot() {
    assert!(adapter_matches("0000:01:00.0", "0000:01:00.0"));
    assert!(adapter_matches("0000:0A:00.0", "0000:0a:00.0"));
    assert!(!adapter_matches("0000:02:00.0", "0000:01:00.0"));
    assert!(!adapter_matches("", "0000:01:00.0"));
  }

  #[test]
  fn slow_frames_are_skipped_until_the_gpu_counts_as_hung() {
    let now = Instant::now();
    let mut health = GpuHealth::default();
    let timeout = RenderFailure::Timeout;
    assert_eq!(health.failure(&timeout, now), Verdict::Skip);
    assert_eq!(health.failure(&timeout, now), Verdict::Skip);
    assert_eq!(health.failure(&timeout, now), Verdict::Recreate);
    assert_eq!(health.failure(&timeout, now), Verdict::Skip);
    health.success();
    assert_eq!(health.failure(&timeout, now), Verdict::Skip);
    assert_eq!(health.failure(&timeout, now), Verdict::Skip);
    assert_eq!(
      health.failure(&RenderFailure::Failed("x".into()), now),
      Verdict::Skip
    );
  }

  #[test]
  fn three_losses_within_a_minute_give_up() {
    let start = Instant::now();
    let mut health = GpuHealth::default();
    let lost = RenderFailure::Lost("test".into());
    assert_eq!(health.failure(&lost, start), Verdict::Recreate);
    assert_eq!(
      health.failure(&lost, start + Duration::from_secs(30)),
      Verdict::Recreate
    );
    // The first loss has left the window.
    assert_eq!(
      health.failure(&lost, start + Duration::from_secs(61)),
      Verdict::Recreate
    );
    assert_eq!(
      health.failure(&lost, start + Duration::from_secs(62)),
      Verdict::GiveUp
    );
  }

  #[test]
  fn failures_read_well_in_the_log() {
    assert_eq!(
      RenderFailure::Timeout.to_string(),
      "the GPU did not finish a frame within 100 ms"
    );
    assert_eq!(
      RenderFailure::Lost("reset".into()).to_string(),
      "the GPU device was lost (reset)"
    );
  }
}
