//! Scanout buffer allocation: GBM on the card, behind a trait.
//!
//! [`BufferAllocator`] is what zero-copy scanout needs from GBM: how many
//! planes a format and modifier have, and buffers for scanout and
//! rendering allocated with a list of modifiers to choose from.
//! [`GbmAllocator`] is the real one over libgbm (the `gbm` crate, without
//! its `drm` feature, which would pull in a second `drm`). It is compiled
//! and checked everywhere but only ever created on a card the seat gave
//! Shingle ([`crate::kms::KmsDevice::buffer_allocator`]); unit tests use
//! the fake card's allocator.

use std::{
  any::Any,
  fmt, io,
  os::fd::{BorrowedFd, OwnedFd},
  rc::Rc,
};

use crate::kms::BufferId;

/// A buffer allocated for scanout and rendering.
pub struct AllocatedBuffer {
  /// The buffer's GEM handle on the card, for its framebuffer.
  pub handle: BufferId,
  /// A DMA-BUF of the buffer, for the renderer's import.
  pub fd: OwnedFd,
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// The DRM fourcc.
  pub fourcc: u32,
  /// The modifier the allocator chose from the list it was given.
  pub modifier: u64,
  /// Bytes per row of the first plane.
  pub stride: u32,
  /// Byte offset of the first plane.
  pub offset: u32,
  /// How many planes the buffer has; zero-copy scanout uses single-plane
  /// buffers only.
  pub planes: u32,
  /// Keeps the allocator's buffer object, and what it needs, alive until
  /// the buffer is dropped.
  pub keep: Box<dyn Any>,
}

impl fmt::Debug for AllocatedBuffer {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("AllocatedBuffer")
      .field("handle", &self.handle)
      .field("width", &self.width)
      .field("height", &self.height)
      .field("modifier", &self.modifier)
      .field("stride", &self.stride)
      .field("offset", &self.offset)
      .field("planes", &self.planes)
      .finish_non_exhaustive()
  }
}

/// What zero-copy scanout needs from GBM.
pub trait BufferAllocator {
  /// How many planes a buffer of `fourcc` with `modifier` has, or `None`
  /// if the allocator does not know the modifier.
  fn plane_count(&self, fourcc: u32, modifier: u64) -> Option<u32>;

  /// Allocates a `width` by `height` buffer of `fourcc` for scanout and
  /// rendering, with one of `modifiers`.
  fn allocate(
    &mut self,
    width: u32,
    height: u32,
    fourcc: u32,
    modifiers: &[u64],
  ) -> io::Result<AllocatedBuffer>;
}

/// The real allocator: a GBM device on a duplicate of the card's
/// descriptor. A duplicate shares the card's open file, so GEM handles of
/// its buffers are valid for the card's framebuffers.
pub struct GbmAllocator {
  device: Rc<gbm::Device<OwnedFd>>,
}

impl GbmAllocator {
  /// Creates a GBM device on a duplicate of `card`.
  pub fn new(card: BorrowedFd<'_>) -> io::Result<Self> {
    let fd = card.try_clone_to_owned()?;
    Ok(Self {
      device: Rc::new(gbm::Device::new(fd)?),
    })
  }
}

fn gbm_format(fourcc: u32) -> io::Result<gbm::Format> {
  gbm::Format::try_from(fourcc).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("DRM format {fourcc:#010x} is unknown to GBM"),
    )
  })
}

impl BufferAllocator for GbmAllocator {
  fn plane_count(&self, fourcc: u32, modifier: u64) -> Option<u32> {
    let format = gbm_format(fourcc).ok()?;
    self
      .device
      .format_modifier_plane_count(format, gbm::Modifier::from(modifier))
  }

  fn allocate(
    &mut self,
    width: u32,
    height: u32,
    fourcc: u32,
    modifiers: &[u64],
  ) -> io::Result<AllocatedBuffer> {
    let format = gbm_format(fourcc)?;
    let bo = self.device.create_buffer_object_with_modifiers2::<()>(
      width,
      height,
      format,
      modifiers
        .iter()
        .map(|modifier| gbm::Modifier::from(*modifier)),
      gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING,
    )?;
    let fd = bo
      .fd()
      .map_err(|_| io::Error::other("GBM gave no DMA-BUF for the buffer"))?;
    // SAFETY: for a buffer on a DRM device, GBM's handle is the GEM handle,
    // the union's u32 member.
    let handle = unsafe { bo.handle().u32_ };
    Ok(AllocatedBuffer {
      handle: BufferId(handle),
      fd,
      width: bo.width(),
      height: bo.height(),
      fourcc,
      modifier: u64::from(bo.modifier()),
      stride: bo.stride_for_plane(0),
      offset: bo.offset(0),
      planes: bo.plane_count(),
      // The buffer object first, then the device, which must outlive it.
      keep: Box::new((bo, Rc::clone(&self.device))),
    })
  }
}
