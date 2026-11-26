//! Zero-copy scanout: frames drawn straight into GBM buffers that the
//! primary plane scans out.
//!
//! As docs/design/dmabuf.md, section 7, and docs/design/native-backend.md,
//! section 2, decide: the modifiers are the primary plane's `IN_FORMATS`
//! for XRGB8888 that the renderer can render to and GBM allocates with a
//! single plane ([`negotiate`]); GBM allocates [`BUFFERS`] buffers with
//! `SCANOUT | RENDERING` and one of them; each becomes a framebuffer with
//! its modifier and is imported into the renderer's device, which clears
//! it to black. Every step that fails gives the reason copy scanout is
//! kept for.

use std::{any::Any, collections::VecDeque, io};

use shingle_render::dmabuf::modifier_name;

use super::{
  DrawTarget, Drawn, Scanout, ScanoutDmabuf, ScanoutImporter,
  allocator::BufferAllocator,
};
use crate::kms::{
  FORMAT_XRGB8888, FramebufferId, KmsDevice, MODIFIER_INVALID,
  PlanarFramebuffer, in_formats::InFormats,
};

/// How many buffers zero-copy scanout keeps per output.
pub const BUFFERS: usize = 3;

/// The modifiers zero-copy scanout may allocate with: those in `plane`
/// (the primary plane's `IN_FORMATS` for the format), in that order, that
/// are also in `render` (what the renderer can render to) and have one
/// plane by `plane_count`. `DRM_FORMAT_MOD_INVALID` is never one: Vulkan
/// imports explicit modifiers only.
pub fn negotiate(
  plane: &[u64],
  render: &[u64],
  plane_count: &dyn Fn(u64) -> Option<u32>,
) -> Vec<u64> {
  let mut modifiers = Vec::new();
  for modifier in plane {
    if *modifier != MODIFIER_INVALID
      && render.contains(modifier)
      && plane_count(*modifier) == Some(1)
      && !modifiers.contains(modifier)
    {
      modifiers.push(*modifier);
    }
  }
  modifiers
}

/// A list of modifiers for the log, such as `LINEAR, NVIDIA 0x0300...`.
fn names(modifiers: &[u64]) -> String {
  if modifiers.is_empty() {
    return "none".into();
  }
  let names: Vec<String> = modifiers
    .iter()
    .map(|modifier| modifier_name(*modifier))
    .collect();
  names.join(", ")
}

/// Why [`negotiate`] found nothing, naming each side's modifiers.
fn no_common_modifier(
  plane: &[u64],
  render: &[u64],
  plane_count: &dyn Fn(u64) -> Option<u32>,
) -> String {
  let shared: Vec<u64> = plane
    .iter()
    .copied()
    .filter(|modifier| render.contains(modifier))
    .collect();
  if shared.is_empty() {
    return format!(
      "no XRGB8888 modifier is both scanned out by the primary plane \
       ({}) and rendered to by the renderer ({})",
      names(plane),
      names(render)
    );
  }
  let planes: Vec<String> = shared
    .iter()
    .map(|modifier| match plane_count(*modifier) {
      Some(count) => format!("{} has {count} planes", modifier_name(*modifier)),
      None => format!("{} is unknown to GBM", modifier_name(*modifier)),
    })
    .collect();
  format!(
    "no XRGB8888 modifier that the primary plane and the renderer share \
     has one plane: {}",
    planes.join(", ")
  )
}

/// One GBM buffer with its framebuffer.
struct GbmBuffer {
  dmabuf: ScanoutDmabuf,
  framebuffer: FramebufferId,
  /// The allocator's buffer object, freed with this.
  _keep: Box<dyn Any>,
}

/// [`BUFFERS`] GBM buffers flipped between. The one drawn next is the one
/// that left the screen longest ago.
pub struct GbmScanout {
  buffers: Vec<GbmBuffer>,
  /// The index of the buffer on screen.
  front: usize,
  /// The buffers not on screen, the one that left it first in front.
  order: VecDeque<usize>,
  /// The buffer the last prepare took, until it is presented.
  prepared: Option<usize>,
}

impl GbmScanout {
  /// Sets up zero-copy scanout of `size` pixels for output `output`,
  /// whose primary plane lists `in_formats`: negotiates the modifiers
  /// with `importer` and `allocator`, allocates [`BUFFERS`] buffers, makes
  /// framebuffers of them and has `importer` import them and clear them
  /// to black. Buffer serials are taken from `serial`. On failure nothing
  /// is left allocated, and the error is the reason for the log.
  pub fn create(
    device: &mut dyn KmsDevice,
    allocator: &mut dyn BufferAllocator,
    importer: &mut dyn ScanoutImporter,
    output: &str,
    in_formats: Option<&InFormats>,
    (width, height): (u32, u32),
    serial: &mut u64,
  ) -> Result<Self, String> {
    let in_formats =
      in_formats.ok_or("the primary plane has no IN_FORMATS property")?;
    let plane = in_formats.modifiers(FORMAT_XRGB8888);
    let render = importer.render_modifiers(FORMAT_XRGB8888, (width, height))?;
    let count =
      |modifier: u64| allocator.plane_count(FORMAT_XRGB8888, modifier);
    let modifiers = negotiate(&plane, &render, &count);
    if modifiers.is_empty() {
      return Err(no_common_modifier(&plane, &render, &count));
    }
    let mut scanout = Self {
      buffers: Vec::new(),
      front: 0,
      order: VecDeque::new(),
      prepared: None,
    };
    for _ in 0..BUFFERS {
      *serial += 1;
      let size = (width, height);
      match Self::buffer(device, allocator, &modifiers, size, *serial) {
        Ok(buffer) => scanout.buffers.push(buffer),
        Err(error) => {
          scanout.destroy(device);
          return Err(error);
        }
      }
    }
    let dmabufs: Vec<&ScanoutDmabuf> = scanout
      .buffers
      .iter()
      .map(|buffer| &buffer.dmabuf)
      .collect();
    if let Err(error) = importer.prepare_buffers(output, &dmabufs) {
      scanout.destroy(device);
      return Err(format!(
        "the renderer cannot draw into the GBM buffers: {error}"
      ));
    }
    scanout.order = (1..BUFFERS).collect();
    Ok(scanout)
  }

  /// One buffer with serial `serial` and its framebuffer.
  fn buffer(
    device: &mut dyn KmsDevice,
    allocator: &mut dyn BufferAllocator,
    modifiers: &[u64],
    (width, height): (u32, u32),
    serial: u64,
  ) -> Result<GbmBuffer, String> {
    let buffer = allocator
      .allocate(width, height, FORMAT_XRGB8888, modifiers)
      .map_err(|error| {
        format!("GBM cannot allocate a {width}x{height} buffer: {error}")
      })?;
    if buffer.planes != 1 {
      return Err(format!(
        "GBM allocated a buffer with {} planes, not one",
        buffer.planes
      ));
    }
    if !modifiers.contains(&buffer.modifier) {
      return Err(format!(
        "GBM chose modifier {}, which was not offered",
        modifier_name(buffer.modifier)
      ));
    }
    let framebuffer = device
      .add_framebuffer(&PlanarFramebuffer {
        buffer: buffer.handle,
        width,
        height,
        fourcc: FORMAT_XRGB8888,
        modifier: buffer.modifier,
        stride: buffer.stride,
        offset: buffer.offset,
      })
      .map_err(|error| {
        format!(
          "the card refuses a framebuffer with modifier {}: {error}",
          modifier_name(buffer.modifier)
        )
      })?;
    Ok(GbmBuffer {
      dmabuf: ScanoutDmabuf {
        serial,
        fd: buffer.fd,
        width,
        height,
        fourcc: FORMAT_XRGB8888,
        modifier: buffer.modifier,
        stride: buffer.stride,
        offset: buffer.offset,
      },
      framebuffer,
      _keep: buffer.keep,
    })
  }

  /// The modifier GBM chose for the buffers.
  pub fn modifier(&self) -> u64 {
    self
      .buffers
      .first()
      .map_or(MODIFIER_INVALID, |buffer| buffer.dmabuf.modifier)
  }

  /// The serials of the buffers, in allocation order.
  pub fn serials(&self) -> Vec<u64> {
    self
      .buffers
      .iter()
      .map(|buffer| buffer.dmabuf.serial)
      .collect()
  }

  /// The buffer drawn into next.
  fn back(&self) -> usize {
    self.order.front().copied().unwrap_or(self.front)
  }
}

impl Scanout for GbmScanout {
  fn size(&self) -> (u32, u32) {
    self
      .buffers
      .first()
      .map_or((0, 0), |buffer| (buffer.dmabuf.width, buffer.dmabuf.height))
  }

  fn current(&self) -> FramebufferId {
    self.buffers[self.front].framebuffer
  }

  fn target(&self) -> DrawTarget<'_> {
    DrawTarget::Dmabuf(&self.buffers[self.back()].dmabuf)
  }

  fn prepare(
    &mut self,
    _device: &mut dyn KmsDevice,
    drawn: &Drawn,
  ) -> io::Result<FramebufferId> {
    if *drawn != Drawn::InPlace {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "zero-copy scanout takes frames drawn into its buffers",
      ));
    }
    let back = self.back();
    self.prepared = Some(back);
    Ok(self.buffers[back].framebuffer)
  }

  fn presented(&mut self) {
    let Some(back) = self.prepared.take() else {
      return;
    };
    self.order.retain(|index| *index != back);
    self.order.push_back(self.front);
    self.front = back;
  }

  fn zero_copy(&self) -> bool {
    true
  }

  fn destroy(&mut self, device: &mut dyn KmsDevice) {
    for buffer in self.buffers.drain(..) {
      let _ = device.remove_framebuffer(buffer.framebuffer);
    }
    self.order.clear();
    self.prepared = None;
    self.front = 0;
  }
}

#[cfg(test)]
mod tests {
  use std::{cell::RefCell, rc::Rc};

  use super::*;
  use crate::{
    kms::{
      MODIFIER_LINEAR,
      fake::{FakeAllocator, FakeKms},
      in_formats::{parse, tests::blob},
    },
    scanout::tests::FakeImporter,
  };

  const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6015;
  const X_TILED: u64 = 0x0100_0000_0000_0001;
  const CCS: u64 = 0x0100_0000_0000_0004;

  fn one_plane(_: u64) -> Option<u32> {
    Some(1)
  }

  #[test]
  fn negotiation_keeps_what_plane_renderer_and_gbm_share() {
    assert_eq!(
      negotiate(
        &[X_TILED, MODIFIER_LINEAR, BLOCK_LINEAR],
        &[BLOCK_LINEAR, MODIFIER_LINEAR],
        &one_plane
      ),
      vec![MODIFIER_LINEAR, BLOCK_LINEAR],
      "in the plane's order"
    );
    let planes = |modifier: u64| match modifier {
      CCS => Some(2),
      X_TILED => None,
      _ => Some(1),
    };
    assert_eq!(
      negotiate(
        &[CCS, X_TILED, MODIFIER_LINEAR],
        &[CCS, X_TILED, MODIFIER_LINEAR],
        &planes
      ),
      vec![MODIFIER_LINEAR],
      "compression with a metadata plane and unknown modifiers go"
    );
    assert!(
      negotiate(&[MODIFIER_INVALID], &[MODIFIER_INVALID], &one_plane)
        .is_empty(),
      "the implicit modifier is never offered"
    );
    assert_eq!(
      negotiate(&[X_TILED, X_TILED], &[X_TILED], &one_plane),
      vec![X_TILED]
    );
  }

  #[test]
  fn an_empty_negotiation_names_both_sides() {
    let reason =
      no_common_modifier(&[MODIFIER_LINEAR], &[BLOCK_LINEAR], &one_plane);
    assert!(reason.contains("(LINEAR)"), "{reason}");
    assert!(reason.contains(&modifier_name(BLOCK_LINEAR)), "{reason}");
    assert!(no_common_modifier(&[], &[], &one_plane).contains("(none)"));
    let reason =
      no_common_modifier(&[CCS], &[CCS], &|_| Some(2)).to_lowercase();
    assert!(reason.contains("has 2 planes"), "{reason}");
  }

  struct Setup {
    device: FakeKms,
    allocator: FakeAllocator,
    importer: Rc<RefCell<FakeImporter>>,
    in_formats: InFormats,
    serial: u64,
  }

  impl Setup {
    fn new(plane: &[u64], render: &[u64]) -> Self {
      let device = FakeKms::single_output();
      let allocator = FakeAllocator(Rc::clone(&device.gbm));
      let entries: Vec<(u64, u32, u64)> =
        plane.iter().map(|modifier| (1, 0, *modifier)).collect();
      Self {
        device,
        allocator,
        importer: FakeImporter::rendering(render),
        in_formats: parse(&blob(&[FORMAT_XRGB8888], &entries)).unwrap(),
        serial: 0,
      }
    }

    fn create(&mut self) -> Result<GbmScanout, String> {
      GbmScanout::create(
        &mut self.device,
        &mut self.allocator,
        &mut self.importer,
        "DP-1",
        Some(&self.in_formats),
        (64, 32),
        &mut self.serial,
      )
    }
  }

  #[test]
  fn three_buffers_are_allocated_imported_and_flipped_between() {
    let mut setup =
      Setup::new(&[X_TILED, BLOCK_LINEAR], &[BLOCK_LINEAR, MODIFIER_LINEAR]);
    let mut scanout = setup.create().expect("zero-copy works");
    assert_eq!(scanout.modifier(), BLOCK_LINEAR);
    assert!(scanout.zero_copy());
    assert_eq!(scanout.size(), (64, 32));
    assert_eq!(setup.device.gbm.borrow().live, 3);
    assert_eq!(
      setup.device.gbm.borrow().requests,
      vec![vec![BLOCK_LINEAR]; 3],
      "GBM is offered the negotiated modifiers"
    );
    let framebuffers: Vec<_> =
      setup.device.framebuffers.values().copied().collect();
    assert_eq!(framebuffers.len(), 3);
    assert!(framebuffers.iter().all(|fb| fb.modifier == BLOCK_LINEAR
      && fb.fourcc == FORMAT_XRGB8888
      && (fb.width, fb.height) == (64, 32)
      && fb.stride == 256));
    assert_eq!(
      setup.importer.borrow().prepared,
      vec![("DP-1".to_owned(), vec![1, 2, 3])],
      "the renderer imports and clears every buffer"
    );
    assert_eq!(setup.importer.borrow().asked_size, Some((64, 32)));
    // Buffer 1 is on screen; 2 and 3 are drawn into in turn.
    let first = scanout.current();
    let serial = |scanout: &GbmScanout| match scanout.target() {
      DrawTarget::Dmabuf(buffer) => buffer.serial,
      DrawTarget::Readback => panic!("zero-copy draws into a buffer"),
    };
    assert_eq!(serial(&scanout), 2);
    let back = scanout.prepare(&mut setup.device, &Drawn::InPlace).unwrap();
    assert_ne!(back, first);
    assert_eq!(scanout.current(), first, "until it is presented");
    scanout.presented();
    assert_eq!(scanout.current(), back);
    assert_eq!(serial(&scanout), 3);
    scanout.prepare(&mut setup.device, &Drawn::InPlace).unwrap();
    scanout.presented();
    assert_eq!(serial(&scanout), 1, "the buffer off screen longest");
    scanout.presented();
    assert_eq!(serial(&scanout), 1, "nothing was prepared");
    let pixels = Drawn::Pixels(crate::scanout::Frame {
      width: 64,
      height: 32,
      rgba: vec![0; 64 * 32 * 4],
    });
    assert!(scanout.prepare(&mut setup.device, &pixels).is_err());
    scanout.destroy(&mut setup.device);
    drop(scanout);
    assert!(setup.device.framebuffers.is_empty());
    assert_eq!(setup.device.gbm.borrow().live, 0);
  }

  #[test]
  fn each_failure_frees_everything_and_says_why() {
    let reason = |setup: &mut Setup| -> String {
      let error = setup.create().err().expect("zero-copy fails");
      assert!(setup.device.framebuffers.is_empty(), "{error}");
      assert_eq!(setup.device.gbm.borrow().live, 0, "{error}");
      error
    };
    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[BLOCK_LINEAR]);
    assert!(reason(&mut setup).starts_with("no XRGB8888 modifier"));
    assert!(setup.device.gbm.borrow().requests.is_empty());

    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[MODIFIER_LINEAR]);
    setup.importer.borrow_mut().modifiers = None;
    assert_eq!(reason(&mut setup), "the fake renderer has no GPU");

    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[MODIFIER_LINEAR]);
    setup.device.gbm.borrow_mut().fail = true;
    assert!(
      reason(&mut setup).starts_with("GBM cannot allocate a 64x32 buffer")
    );

    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[MODIFIER_LINEAR]);
    setup.device.gbm.borrow_mut().forced_planes = Some(2);
    assert_eq!(
      reason(&mut setup),
      "GBM allocated a buffer with 2 planes, not one"
    );

    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[MODIFIER_LINEAR]);
    setup.device.reject_framebuffers = true;
    assert!(
      reason(&mut setup)
        .starts_with("the card refuses a framebuffer with modifier LINEAR")
    );

    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[MODIFIER_LINEAR]);
    setup.importer.borrow_mut().fail_prepare = Some("no memory".into());
    assert_eq!(
      reason(&mut setup),
      "the renderer cannot draw into the GBM buffers: no memory"
    );

    let mut setup = Setup::new(&[MODIFIER_LINEAR], &[MODIFIER_LINEAR]);
    let error = GbmScanout::create(
      &mut setup.device,
      &mut setup.allocator,
      &mut setup.importer,
      "DP-1",
      None,
      (64, 32),
      &mut setup.serial,
    )
    .err()
    .expect("no IN_FORMATS");
    assert_eq!(error, "the primary plane has no IN_FORMATS property");
  }
}
