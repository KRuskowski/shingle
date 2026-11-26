//! Scanout strategies: how a drawn frame becomes a framebuffer the
//! primary plane shows.
//!
//! [`Scanout`] is the seam the design names (docs/design/native-backend.md,
//! section 2). NB-7 has the copy path ([`copy::CopyScanout`]): frames are
//! read back from the GPU and copied into one of two dumb buffers. NB-8
//! adds zero-copy GBM buffers behind the same trait.

pub mod allocator;
pub mod copy;

use std::io;

use crate::kms::{FramebufferId, KmsDevice};

/// A frame read back from the GPU: tightly packed RGBA8 rows in sRGB.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// `width * height * 4` bytes.
  pub rgba: Vec<u8>,
}

/// Buffers the primary plane scans out of.
pub trait Scanout {
  /// The buffers' size in pixels, which is the mode's.
  fn size(&self) -> (u32, u32);

  /// The framebuffer on screen, or the one a modeset shows.
  fn current(&self) -> FramebufferId;

  /// Puts `frame` into a buffer that is not on screen and returns its
  /// framebuffer for the next flip.
  fn prepare(
    &mut self,
    device: &mut dyn KmsDevice,
    frame: &Frame,
  ) -> io::Result<FramebufferId>;

  /// Records that the buffer from the last [`Scanout::prepare`] is on
  /// screen now.
  fn presented(&mut self);

  /// Whether frames reach the plane without a copy, for
  /// `wp_presentation`'s `ZERO_COPY` flag.
  fn zero_copy(&self) -> bool;

  /// Frees the buffers.
  fn destroy(&mut self, device: &mut dyn KmsDevice);
}
