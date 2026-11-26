//! Scanout strategies: how a drawn frame becomes a framebuffer the
//! primary plane shows.
//!
//! [`Scanout`] is the seam the design names (docs/design/native-backend.md,
//! section 2). There are two strategies behind it:
//!
//! - **Copy** ([`copy::CopyScanout`], NB-7): frames are drawn offscreen,
//!   read back from the GPU and copied into one of two dumb buffers. It
//!   works with any KMS driver and any Vulkan adapter.
//! - **Zero-copy** ([`gbm::GbmScanout`], NB-8): three GBM buffers with a
//!   modifier both the primary plane and Vulkan support, imported into the
//!   renderer's device and drawn into directly
//!   (docs/design/dmabuf.md, section 7).
//!
//! A scanout tells the renderer where the next frame goes
//! ([`Scanout::target`]): read back ([`DrawTarget::Readback`]), or into a
//! DMA-BUF ([`DrawTarget::Dmabuf`]). The renderer answers with what it
//! drew ([`Drawn`]), and [`Scanout::prepare`] turns that into the
//! framebuffer of the next flip.
//!
//! Every output starts on copy scanout and is moved to zero-copy once its
//! display is lit and a renderer can draw into GBM buffers
//! ([`crate::display::Display::upgrade_scanout`]); any step that fails
//! keeps copy scanout and names why ([`ScanoutStatus::Copy`]).
//! `SHINGLE_SCANOUT=copy` forces copy scanout ([`ScanoutChoice`]).

pub mod allocator;
pub mod copy;
pub mod gbm;

use std::{
  ffi::OsStr,
  io,
  os::fd::{AsFd, OwnedFd},
};

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

/// A single-plane DMA-BUF a frame can be drawn into: one of zero-copy
/// scanout's GBM buffers.
#[derive(Debug)]
pub struct ScanoutDmabuf {
  /// Unique among every scanout buffer the display made, so that the
  /// renderer can keep its import of the buffer.
  pub serial: u64,
  /// The buffer's DMA-BUF.
  pub fd: OwnedFd,
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// The DRM fourcc, XRGB8888.
  pub fourcc: u32,
  /// The DRM modifier GBM chose.
  pub modifier: u64,
  /// Bytes per row of the plane.
  pub stride: u32,
  /// Byte offset of the plane.
  pub offset: u32,
}

impl ScanoutDmabuf {
  /// The descriptor `shingle_render::GpuContext::import_dmabuf` takes.
  pub fn descriptor(&self) -> shingle_render::dmabuf::DmabufDescriptor<'_> {
    shingle_render::dmabuf::DmabufDescriptor {
      width: self.width,
      height: self.height,
      fourcc: self.fourcc,
      modifier: self.modifier,
      fd: self.fd.as_fd(),
      offset: self.offset,
      stride: self.stride,
    }
  }
}

/// Where the renderer draws the next frame of an output.
#[derive(Clone, Copy, Debug)]
pub enum DrawTarget<'a> {
  /// Offscreen, read back as a [`Frame`] for copy scanout.
  Readback,
  /// Into this buffer, which is then scanned out as it is.
  Dmabuf(&'a ScanoutDmabuf),
}

/// What the renderer drew for [`Scanout::prepare`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Drawn {
  /// The frame read back, for [`DrawTarget::Readback`].
  Pixels(Frame),
  /// The frame is in the [`DrawTarget::Dmabuf`] buffer, finished by the
  /// GPU.
  InPlace,
}

/// Buffers the primary plane scans out of.
pub trait Scanout {
  /// The buffers' size in pixels, which is the mode's.
  fn size(&self) -> (u32, u32);

  /// The framebuffer on screen, or the one a modeset shows.
  fn current(&self) -> FramebufferId;

  /// Where the next frame is drawn.
  fn target(&self) -> DrawTarget<'_>;

  /// Takes the frame the renderer `drawn` into the buffer that
  /// [`Scanout::target`] named (or copies its pixels into a buffer off
  /// screen) and returns that buffer's framebuffer for the next flip.
  fn prepare(
    &mut self,
    device: &mut dyn KmsDevice,
    drawn: &Drawn,
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

/// What the GPU side of zero-copy scanout needs from the renderer: which
/// modifiers it can render to, and that it can import and draw into the
/// buffers GBM allocated.
pub trait ScanoutImporter {
  /// The modifiers of DRM format `fourcc` that the renderer can import
  /// and render into at `size` pixels, or why it cannot render into
  /// DMA-BUFs at all.
  fn render_modifiers(
    &self,
    fourcc: u32,
    size: (u32, u32),
  ) -> Result<Vec<u64>, String> {
    let _ = (fourcc, size);
    Err("the renderer cannot draw into DMA-BUFs".into())
  }

  /// Imports `buffers` for output `output`, replacing what it imported for
  /// it before, and clears each to black, so that a modeset or flip shows
  /// a black frame until the first one is drawn.
  fn prepare_buffers(
    &mut self,
    output: &str,
    buffers: &[&ScanoutDmabuf],
  ) -> Result<(), String> {
    let _ = (output, buffers);
    Err("the renderer cannot draw into DMA-BUFs".into())
  }
}

/// Which scanout the outputs use, from `SHINGLE_SCANOUT`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScanoutChoice {
  /// Zero-copy where it works, else copy.
  #[default]
  Auto,
  /// Copy scanout always (`SHINGLE_SCANOUT=copy`).
  Copy,
}

impl ScanoutChoice {
  /// Reads `SHINGLE_SCANOUT`: `copy` forces copy scanout, `auto`, empty or
  /// unset try zero-copy. Any other value also tries zero-copy and comes
  /// with a warning for the log.
  pub fn from_env(value: Option<&OsStr>) -> (Self, Option<String>) {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
      return (Self::Auto, None);
    };
    match value.to_str() {
      Some("copy") => (Self::Copy, None),
      Some("auto") => (Self::Auto, None),
      _ => (
        Self::Auto,
        Some(format!(
          "SHINGLE_SCANOUT={} is neither copy nor auto; trying zero-copy",
          value.to_string_lossy()
        )),
      ),
    }
  }
}

/// Which scanout an output uses, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanoutStatus {
  /// Copy scanout until the output is drawn and zero-copy is tried.
  Untried,
  /// Zero-copy scanout from GBM buffers with this modifier.
  ZeroCopy(u64),
  /// Copy scanout, for this reason, as logged.
  Copy(String),
}

#[cfg(test)]
pub(crate) mod tests {
  use std::{cell::RefCell, rc::Rc};

  use super::*;

  /// A renderer's GPU side for zero-copy tests: the modifiers it renders
  /// to (`None` for a renderer without a GPU), failures it is told to
  /// give, and what it imported.
  #[derive(Debug, Default)]
  pub(crate) struct FakeImporter {
    pub(crate) modifiers: Option<Vec<u64>>,
    /// Makes `prepare_buffers` fail with this reason.
    pub(crate) fail_prepare: Option<String>,
    /// Each `prepare_buffers` call: the output and the buffers' serials.
    pub(crate) prepared: Vec<(String, Vec<u64>)>,
    /// The size asked for in the last `render_modifiers`.
    pub(crate) asked_size: Option<(u32, u32)>,
  }

  impl FakeImporter {
    /// An importer that renders to `modifiers`.
    pub(crate) fn rendering(modifiers: &[u64]) -> Rc<RefCell<Self>> {
      Rc::new(RefCell::new(Self {
        modifiers: Some(modifiers.to_vec()),
        ..Self::default()
      }))
    }
  }

  impl ScanoutImporter for Rc<RefCell<FakeImporter>> {
    fn render_modifiers(
      &self,
      _fourcc: u32,
      size: (u32, u32),
    ) -> Result<Vec<u64>, String> {
      let mut importer = self.borrow_mut();
      importer.asked_size = Some(size);
      importer
        .modifiers
        .clone()
        .ok_or_else(|| "the fake renderer has no GPU".to_owned())
    }

    fn prepare_buffers(
      &mut self,
      output: &str,
      buffers: &[&ScanoutDmabuf],
    ) -> Result<(), String> {
      let mut importer = self.borrow_mut();
      if let Some(reason) = importer.fail_prepare.clone() {
        return Err(reason);
      }
      let serials = buffers.iter().map(|buffer| buffer.serial).collect();
      importer.prepared.push((output.to_owned(), serials));
      Ok(())
    }
  }

  #[test]
  fn shingle_scanout_copy_forces_copy() {
    assert_eq!(
      ScanoutChoice::from_env(Some(OsStr::new("copy"))),
      (ScanoutChoice::Copy, None)
    );
    for value in [None, Some(""), Some("auto")] {
      assert_eq!(
        ScanoutChoice::from_env(value.map(OsStr::new)),
        (ScanoutChoice::Auto, None)
      );
    }
    let (choice, warning) = ScanoutChoice::from_env(Some(OsStr::new("gbm")));
    assert_eq!(choice, ScanoutChoice::Auto);
    assert_eq!(
      warning.as_deref(),
      Some("SHINGLE_SCANOUT=gbm is neither copy nor auto; trying zero-copy")
    );
  }

  #[test]
  fn importers_without_a_gpu_refuse() {
    struct Plain;
    impl ScanoutImporter for Plain {}
    let mut plain = Plain;
    assert!(plain.render_modifiers(0, (1, 1)).is_err());
    assert!(plain.prepare_buffers("DP-1", &[]).is_err());
  }
}
