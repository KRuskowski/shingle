//! Copy scanout: frames rendered offscreen, read back and copied into two
//! linear XRGB8888 dumb buffers, one on screen and one being filled.
//!
//! It works with any KMS driver and any Vulkan adapter, at the cost of one
//! GPU readback and one CPU copy per frame. The renderer draws into an
//! RGBA8 target (`shingle_render::OffscreenTarget`), so the copy swaps red
//! and blue on the way: XRGB8888 is `B, G, R, X` in memory.

use std::io;

use super::{DrawTarget, Drawn, Scanout};
use crate::kms::{DumbFramebuffer, FramebufferId, KmsDevice};

/// Copies tightly packed RGBA8 rows of a `width` by `height` image into
/// XRGB8888 rows `stride` bytes apart, with the unused byte set to 0xff.
pub fn copy_rgba_to_xrgb8888(
  rgba: &[u8],
  width: u32,
  height: u32,
  destination: &mut [u8],
  stride: u32,
) -> io::Result<()> {
  let row_bytes = width as usize * 4;
  let stride = stride as usize;
  let height = height as usize;
  if rgba.len() != row_bytes * height
    || stride < row_bytes
    || destination.len() < stride * height.saturating_sub(1) + row_bytes
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "the frame does not fit the buffer",
    ));
  }
  for (source, target) in rgba
    .chunks_exact(row_bytes)
    .zip(destination.chunks_mut(stride))
  {
    for (pixel, out) in source.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
      out[0] = pixel[2];
      out[1] = pixel[1];
      out[2] = pixel[0];
      out[3] = 0xff;
    }
  }
  Ok(())
}

/// Two dumb buffers, flipped between.
#[derive(Debug)]
pub struct CopyScanout {
  buffers: [DumbFramebuffer; 2],
  /// The index of the buffer on screen.
  front: usize,
  /// The buffer the last prepare filled, until it is presented.
  prepared: Option<usize>,
}

impl CopyScanout {
  /// Allocates two `width` by `height` dumb buffers. They start black.
  pub fn new(
    device: &mut dyn KmsDevice,
    (width, height): (u32, u32),
  ) -> io::Result<Self> {
    let first = device.create_dumb_framebuffer(width, height)?;
    let second = match device.create_dumb_framebuffer(width, height) {
      Ok(second) => second,
      Err(error) => {
        let _ = device.destroy_dumb_framebuffer(&first);
        return Err(error);
      }
    };
    Ok(Self {
      buffers: [first, second],
      front: 0,
      prepared: None,
    })
  }
}

impl Scanout for CopyScanout {
  fn size(&self) -> (u32, u32) {
    (self.buffers[0].width, self.buffers[0].height)
  }

  fn current(&self) -> FramebufferId {
    self.buffers[self.front].framebuffer
  }

  fn target(&self) -> DrawTarget<'_> {
    DrawTarget::Readback
  }

  fn prepare(
    &mut self,
    device: &mut dyn KmsDevice,
    drawn: &Drawn,
  ) -> io::Result<FramebufferId> {
    let Drawn::Pixels(frame) = drawn else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "copy scanout needs the frame's pixels",
      ));
    };
    let back = 1 - self.front;
    let buffer = self.buffers[back];
    if (frame.width, frame.height) != (buffer.width, buffer.height) {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "a {}x{} frame for {}x{} buffers",
          frame.width, frame.height, buffer.width, buffer.height
        ),
      ));
    }
    let mut result = Ok(());
    device.write_dumb_framebuffer(&buffer, &mut |pixels| {
      result = copy_rgba_to_xrgb8888(
        &frame.rgba,
        frame.width,
        frame.height,
        pixels,
        buffer.stride,
      );
    })?;
    result?;
    self.prepared = Some(back);
    Ok(buffer.framebuffer)
  }

  fn presented(&mut self) {
    if let Some(back) = self.prepared.take() {
      self.front = back;
    }
  }

  fn zero_copy(&self) -> bool {
    false
  }

  fn destroy(&mut self, device: &mut dyn KmsDevice) {
    for buffer in &self.buffers {
      let _ = device.destroy_dumb_framebuffer(buffer);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{kms::fake::FakeKms, scanout::Frame};

  fn frame(width: u32, height: u32, rgba: [u8; 4]) -> Drawn {
    Drawn::Pixels(Frame {
      width,
      height,
      rgba: rgba.repeat((width * height) as usize),
    })
  }

  #[test]
  fn rows_are_swizzled_into_padded_rows() {
    let rgba = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    let mut out = vec![0; 2 * 12];
    copy_rgba_to_xrgb8888(&rgba, 2, 2, &mut out, 12).unwrap();
    assert_eq!(
      out,
      vec![
        3, 2, 1, 255, 7, 6, 5, 255, 0, 0, 0, 0, 11, 10, 9, 255, 15, 14, 13,
        255, 0, 0, 0, 0,
      ]
    );
  }

  #[test]
  fn frames_that_do_not_fit_are_refused() {
    let mut out = vec![0; 16];
    assert!(copy_rgba_to_xrgb8888(&[0; 12], 2, 2, &mut out, 8).is_err());
    assert!(copy_rgba_to_xrgb8888(&[0; 16], 2, 2, &mut out, 4).is_err());
    assert!(copy_rgba_to_xrgb8888(&[0; 16], 2, 2, &mut out[..12], 8).is_err());
    // The last row needs no padding after it.
    assert!(copy_rgba_to_xrgb8888(&[0; 16], 2, 2, &mut out[..16], 8).is_ok());
  }

  #[test]
  fn frames_alternate_between_two_buffers() {
    let mut device = FakeKms::single_output();
    let mut scanout = CopyScanout::new(&mut device, (4, 2)).unwrap();
    assert_eq!(device.buffers.len(), 2);
    let first = scanout.current();
    assert!(!scanout.zero_copy());
    assert!(matches!(scanout.target(), DrawTarget::Readback));
    assert_eq!(scanout.size(), (4, 2));
    let back = scanout
      .prepare(&mut device, &frame(4, 2, [9, 8, 7, 6]))
      .unwrap();
    assert_ne!(back, first, "the frame goes into the buffer off screen");
    assert_eq!(scanout.current(), first, "until it is presented");
    let (_, pixels) = &device.buffers[&back.0];
    assert_eq!(&pixels[..4], &[7, 8, 9, 255]);
    scanout.presented();
    assert_eq!(scanout.current(), back);
    let next = scanout.prepare(&mut device, &frame(4, 2, [0; 4])).unwrap();
    assert_eq!(next, first);
    scanout.destroy(&mut device);
    assert!(device.buffers.is_empty());
  }

  #[test]
  fn a_frame_of_another_size_is_refused() {
    let mut device = FakeKms::single_output();
    let mut scanout = CopyScanout::new(&mut device, (4, 2)).unwrap();
    assert!(scanout.prepare(&mut device, &frame(2, 2, [0; 4])).is_err());
    assert!(scanout.prepare(&mut device, &Drawn::InPlace).is_err());
    scanout.presented();
    assert_eq!(scanout.current(), scanout.buffers[0].framebuffer);
  }

  #[test]
  fn allocation_failures_free_what_was_allocated() {
    let mut device = FakeKms::single_output();
    device.no_buffers = true;
    assert!(CopyScanout::new(&mut device, (4, 2)).is_err());
    assert!(device.buffers.is_empty());
  }
}
