//! Kernel mode setting: what the native backend needs from a DRM card.
//!
//! [`KmsDevice`] is the seam between Shingle and the kernel. It speaks in
//! the plain types of this module (object ids, [`Mode`], [`Property`],
//! [`AtomicRequest`]) so that everything above it, from the property cache
//! ([`props`]) and the `IN_FORMATS` parser ([`in_formats`]) to mode
//! selection ([`modes`]), the atomic request builder ([`atomic`]) and the
//! choice of connector, CRTC and plane ([`output`]), is pure and unit
//! tested with a fake device. [`drm::DrmDevice`] implements it over the
//! `drm` crate; it is compiled and checked everywhere but only ever run on
//! a machine whose seat Shingle may take (docs/native-testing.md).

pub mod atomic;
pub mod drm;
pub mod edid;
pub mod in_formats;
pub mod modes;
pub mod output;
pub mod props;

use std::{io, os::fd::BorrowedFd, time::Duration};

pub use atomic::AtomicRequest;

use crate::scanout::allocator::BufferAllocator;

macro_rules! handle {
  ($(#[$doc:meta] $name:ident),* $(,)?) => {
    $(
      #[$doc]
      #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
      pub struct $name(pub u32);
    )*
  };
}

handle!(
  /// A DRM connector, such as `DP-1`.
  ConnectorId,
  /// A DRM CRTC, the scanout engine that drives one connector.
  CrtcId,
  /// A DRM plane; Shingle uses each CRTC's primary plane.
  PlaneId,
  /// A DRM property of a connector, CRTC or plane.
  PropertyId,
  /// A DRM property blob, such as a mode for `MODE_ID`.
  BlobId,
  /// A KMS framebuffer, for a plane's `FB_ID`.
  FramebufferId,
  /// A dumb buffer's GEM handle.
  BufferId,
);

/// A KMS object that has properties.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Object {
  /// A connector.
  Connector(ConnectorId),
  /// A CRTC.
  Crtc(CrtcId),
  /// A plane.
  Plane(PlaneId),
}

impl Object {
  /// The object's id, which DRM keeps unique across object types.
  pub fn id(self) -> u32 {
    match self {
      Self::Connector(ConnectorId(id))
      | Self::Crtc(CrtcId(id))
      | Self::Plane(PlaneId(id)) => id,
    }
  }
}

/// `DRM_MODE_TYPE_PREFERRED`: the mode the display prefers.
pub const MODE_TYPE_PREFERRED: u32 = 1 << 3;
/// `DRM_MODE_FLAG_INTERLACE`.
pub const MODE_FLAG_INTERLACE: u32 = 1 << 4;
/// `DRM_MODE_FLAG_DBLSCAN`.
pub const MODE_FLAG_DBLSCAN: u32 = 1 << 5;

/// A display mode, field for field `struct drm_mode_modeinfo`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mode {
  /// Pixel clock in kHz.
  pub clock: u32,
  /// Visible width.
  pub hdisplay: u16,
  /// Horizontal sync start.
  pub hsync_start: u16,
  /// Horizontal sync end.
  pub hsync_end: u16,
  /// Total width including blanking.
  pub htotal: u16,
  /// Horizontal skew.
  pub hskew: u16,
  /// Visible height.
  pub vdisplay: u16,
  /// Vertical sync start.
  pub vsync_start: u16,
  /// Vertical sync end.
  pub vsync_end: u16,
  /// Total height including blanking.
  pub vtotal: u16,
  /// Vertical scan multiplier.
  pub vscan: u16,
  /// Refresh rate in Hz as the kernel rounds it.
  pub vrefresh: u32,
  /// `DRM_MODE_FLAG_*`.
  pub flags: u32,
  /// `DRM_MODE_TYPE_*`.
  pub kind: u32,
  /// The mode's name, such as `1920x1080`.
  pub name: String,
}

impl Mode {
  /// Width and height in pixels.
  pub fn size(&self) -> (u32, u32) {
    (u32::from(self.hdisplay), u32::from(self.vdisplay))
  }

  /// Whether the display marks this mode as preferred.
  pub fn preferred(&self) -> bool {
    self.kind & MODE_TYPE_PREFERRED != 0
  }

  /// The refresh rate in millihertz, computed from the timings as
  /// wlroots' `calculate_refresh_rate` does, or zero for an empty mode.
  pub fn refresh_mhz(&self) -> u32 {
    let htotal = u64::from(self.htotal);
    let vtotal = u64::from(self.vtotal);
    if htotal == 0 || vtotal == 0 {
      return 0;
    }
    let mut refresh =
      (u64::from(self.clock) * 1_000_000 / htotal + vtotal / 2) / vtotal;
    if self.flags & MODE_FLAG_INTERLACE != 0 {
      refresh *= 2;
    }
    if self.flags & MODE_FLAG_DBLSCAN != 0 {
      refresh /= 2;
    }
    if self.vscan > 1 {
      refresh /= u64::from(self.vscan);
    }
    u32::try_from(refresh).unwrap_or(u32::MAX)
  }

  /// The time between two refreshes in nanoseconds, or zero when
  /// unknown.
  pub fn refresh_ns(&self) -> u32 {
    match self.refresh_mhz() {
      0 => 0,
      mhz => {
        u32::try_from(1_000_000_000_000 / u64::from(mhz)).unwrap_or(u32::MAX)
      }
    }
  }
}

/// Whether a display is attached to a connector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Connection {
  /// A display is attached.
  Connected,
  /// Nothing is attached.
  Disconnected,
  /// The driver cannot tell.
  Unknown,
}

/// A panel's subpixel layout, as the connector reports it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Subpixel {
  /// Not known.
  #[default]
  Unknown,
  /// Horizontal red, green, blue.
  HorizontalRgb,
  /// Horizontal blue, green, red.
  HorizontalBgr,
  /// Vertical red, green, blue.
  VerticalRgb,
  /// Vertical blue, green, red.
  VerticalBgr,
  /// No subpixels.
  None,
}

/// What a connector reports.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorInfo {
  /// The connector.
  pub id: ConnectorId,
  /// Its name from type and type id, such as `eDP-1` or `HDMI-A-2`.
  pub name: String,
  /// Whether it is an internal panel (eDP, LVDS or DSI).
  pub internal: bool,
  /// Whether a display is attached.
  pub connection: Connection,
  /// The modes the display offers, in the driver's order.
  pub modes: Vec<Mode>,
  /// Physical size in millimetres, or zero when unknown.
  pub physical_size_mm: (u32, u32),
  /// Subpixel layout.
  pub subpixel: Subpixel,
  /// The CRTC its current encoder drives, if any.
  pub current_crtc: Option<CrtcId>,
  /// The CRTCs any of its encoders can drive.
  pub possible_crtcs: Vec<CrtcId>,
  /// What its `EDID` property says about the display. [`KmsDevice`]
  /// leaves it `None`; [`output::probe`] reads it for connected
  /// connectors.
  pub edid: Option<edid::Edid>,
}

/// What a CRTC reports.
#[derive(Clone, Debug, PartialEq)]
pub struct CrtcInfo {
  /// The CRTC.
  pub id: CrtcId,
  /// Its current mode, such as the one the console set, if any.
  pub mode: Option<Mode>,
}

/// What a plane reports.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaneInfo {
  /// The plane.
  pub id: PlaneId,
  /// The CRTCs it can be attached to.
  pub possible_crtcs: Vec<CrtcId>,
  /// The CRTC it is attached to now, if any.
  pub current_crtc: Option<CrtcId>,
  /// The fourcc formats it can scan out.
  pub formats: Vec<u32>,
}

/// The objects of a card.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resources {
  /// Every connector.
  pub connectors: Vec<ConnectorId>,
  /// Every CRTC, in the kernel's index order.
  pub crtcs: Vec<CrtcId>,
  /// Every plane.
  pub planes: Vec<PlaneId>,
}

/// One property of an object with its current value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Property {
  /// The property.
  pub id: PropertyId,
  /// Its name, such as `CRTC_ID`.
  pub name: String,
  /// Its current value.
  pub value: u64,
  /// The smallest and largest value of a range property, such as
  /// `max bpc`; `None` for other kinds.
  pub range: Option<(u64, u64)>,
}

/// A linear XRGB8888 dumb buffer with its framebuffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumbFramebuffer {
  /// The buffer's GEM handle.
  pub buffer: BufferId,
  /// The framebuffer that scans it out.
  pub framebuffer: FramebufferId,
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// Bytes per row.
  pub stride: u32,
}

/// A single-plane buffer with an explicit modifier, as a framebuffer is
/// made of it with `DRM_MODE_FB_MODIFIERS`: one of zero-copy scanout's
/// GBM buffers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanarFramebuffer {
  /// The buffer's GEM handle.
  pub buffer: BufferId,
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// The DRM fourcc.
  pub fourcc: u32,
  /// The DRM modifier, never `DRM_FORMAT_MOD_INVALID`.
  pub modifier: u64,
  /// Bytes per row.
  pub stride: u32,
  /// Byte offset of the plane.
  pub offset: u32,
}

/// A completed page flip, from a `DRM_EVENT_FLIP_COMPLETE` event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFlip {
  /// The CRTC that flipped.
  pub crtc: CrtcId,
  /// The CRTC's vblank counter at the flip.
  pub sequence: u32,
  /// `CLOCK_MONOTONIC` time at which the new frame started to scan out.
  pub time: Duration,
}

/// Flags of an atomic commit, with the kernel's values.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct CommitFlags(pub u32);

impl CommitFlags {
  /// `DRM_MODE_PAGE_FLIP_EVENT`: send a page-flip event when done.
  pub const PAGE_FLIP_EVENT: Self = Self(0x01);
  /// `DRM_MODE_PAGE_FLIP_ASYNC`: flip at once rather than at the next
  /// vblank, which may tear; only for flips that change nothing but the
  /// primary plane's framebuffer.
  pub const PAGE_FLIP_ASYNC: Self = Self(0x02);
  /// `DRM_MODE_ATOMIC_TEST_ONLY`: only check the request.
  pub const TEST_ONLY: Self = Self(0x0100);
  /// `DRM_MODE_ATOMIC_NONBLOCK`: return before the commit is done.
  pub const NONBLOCK: Self = Self(0x0200);
  /// `DRM_MODE_ATOMIC_ALLOW_MODESET`: the request may change modes.
  pub const ALLOW_MODESET: Self = Self(0x0400);

  /// Whether every flag of `other` is set.
  pub fn contains(self, other: Self) -> bool {
    self.0 & other.0 == other.0
  }
}

impl std::ops::BitOr for CommitFlags {
  type Output = Self;

  fn bitor(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }
}

/// What the native backend needs from a DRM card. Every call may fail
/// with the kernel's error.
pub trait KmsDevice {
  /// Asks for universal planes and atomic modesetting. Failing means the
  /// driver has no atomic support.
  fn enable_atomic(&mut self) -> io::Result<()>;

  /// Whether page-flip timestamps use `CLOCK_MONOTONIC`.
  fn monotonic_timestamps(&mut self) -> bool;

  /// Lists the card's connectors, CRTCs and planes.
  fn resources(&mut self) -> io::Result<Resources>;

  /// Reads a connector, probing it for displays and modes.
  fn connector(&mut self, id: ConnectorId) -> io::Result<ConnectorInfo>;

  /// Reads a CRTC.
  fn crtc(&mut self, id: CrtcId) -> io::Result<CrtcInfo>;

  /// Reads a plane.
  fn plane(&mut self, id: PlaneId) -> io::Result<PlaneInfo>;

  /// Reads an object's properties with their current values.
  fn properties(&mut self, object: Object) -> io::Result<Vec<Property>>;

  /// Reads a property blob, such as `IN_FORMATS`.
  fn blob(&mut self, id: u64) -> io::Result<Vec<u8>>;

  /// Creates a blob holding `mode`, for `MODE_ID`.
  fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<BlobId>;

  /// Destroys a blob made by [`KmsDevice::create_mode_blob`].
  fn destroy_blob(&mut self, blob: BlobId) -> io::Result<()>;

  /// Commits `request` atomically.
  fn commit(
    &mut self,
    request: &AtomicRequest,
    flags: CommitFlags,
  ) -> io::Result<()>;

  /// Allocates a `width` by `height` XRGB8888 dumb buffer and a
  /// framebuffer for it. Its contents start black.
  fn create_dumb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer>;

  /// Maps a dumb buffer and lets `write` fill it, `stride` bytes per row.
  fn write_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
    write: &mut dyn FnMut(&mut [u8]),
  ) -> io::Result<()>;

  /// Allocates a `width` by `height` ARGB8888 dumb buffer and a
  /// framebuffer for it, for a cursor plane. Its contents start
  /// transparent. [`KmsDevice::write_dumb_framebuffer`] fills it and
  /// [`KmsDevice::destroy_dumb_framebuffer`] frees it.
  fn create_argb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer>;

  /// The size of the buffers cursor planes take (`DRM_CAP_CURSOR_WIDTH`
  /// and `DRM_CAP_CURSOR_HEIGHT`), 64 by 64 where the driver does not say.
  fn cursor_size(&mut self) -> (u32, u32);

  /// Whether the driver takes `DRM_MODE_PAGE_FLIP_ASYNC` in atomic commits
  /// (`DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`).
  fn async_page_flips(&mut self) -> bool;

  /// Removes a framebuffer and frees its dumb buffer.
  fn destroy_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
  ) -> io::Result<()>;

  /// Makes a framebuffer of a single-plane buffer with an explicit
  /// modifier.
  fn add_framebuffer(
    &mut self,
    buffer: &PlanarFramebuffer,
  ) -> io::Result<FramebufferId>;

  /// Removes a framebuffer made by [`KmsDevice::add_framebuffer`]; the
  /// buffer itself belongs to its allocator.
  fn remove_framebuffer(
    &mut self,
    framebuffer: FramebufferId,
  ) -> io::Result<()>;

  /// A GBM allocator on this card, for zero-copy scanout's buffers.
  fn buffer_allocator(&mut self) -> io::Result<Box<dyn BufferAllocator>>;

  /// Reads the page flips that completed, without waiting.
  fn read_events(&mut self) -> io::Result<Vec<PageFlip>>;

  /// The descriptor that becomes readable when events are pending.
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}

/// The `drm_fourcc` code of XRGB8888, the format Shingle scans out.
pub const FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// The `drm_fourcc` code of ARGB8888, the format of cursor planes.
pub const FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");

/// The linear modifier, `DRM_FORMAT_MOD_LINEAR`.
pub const MODIFIER_LINEAR: u64 = 0;

/// `DRM_FORMAT_MOD_INVALID`: no explicit modifier.
pub const MODIFIER_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
pub(crate) mod tests {
  use super::*;

  /// The CEA 1920x1080 60 Hz mode.
  pub(crate) fn mode_1080p() -> Mode {
    Mode {
      clock: 148_500,
      hdisplay: 1920,
      hsync_start: 2008,
      hsync_end: 2052,
      htotal: 2200,
      vdisplay: 1080,
      vsync_start: 1084,
      vsync_end: 1089,
      vtotal: 1125,
      vrefresh: 60,
      ..Mode::default()
    }
  }

  #[test]
  fn refresh_follows_the_timings() {
    let mode = mode_1080p();
    assert_eq!(mode.refresh_mhz(), 60_000);
    assert_eq!(mode.refresh_ns(), 16_666_666);
    let interlaced = Mode {
      flags: MODE_FLAG_INTERLACE,
      ..mode.clone()
    };
    assert_eq!(interlaced.refresh_mhz(), 120_000);
    let doubled = Mode {
      flags: MODE_FLAG_DBLSCAN,
      vscan: 2,
      ..mode
    };
    assert_eq!(doubled.refresh_mhz(), 15_000);
    assert_eq!(Mode::default().refresh_mhz(), 0);
    assert_eq!(Mode::default().refresh_ns(), 0);
  }

  #[test]
  fn flags_combine_with_the_kernel_values() {
    let flags = CommitFlags::NONBLOCK | CommitFlags::PAGE_FLIP_EVENT;
    assert_eq!(flags.0, 0x201);
    assert!(flags.contains(CommitFlags::NONBLOCK));
    assert!(!flags.contains(CommitFlags::ALLOW_MODESET));
    assert_eq!(CommitFlags::PAGE_FLIP_ASYNC.0, 0x02);
    assert_eq!(FORMAT_XRGB8888, 0x3432_5258);
    assert_eq!(FORMAT_ARGB8888, 0x3432_5241);
  }
}
