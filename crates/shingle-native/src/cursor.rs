//! The hardware cursor's buffers: two ARGB8888 dumb buffers of the size
//! the driver wants for its cursor planes, one on screen and one written
//! when the cursor image changes (docs/design/native-backend.md, NB-10).
//!
//! The frontend hands the image over as premultiplied RGBA
//! ([`shingle_frontend::CursorImage`]), which is what both `wl_shm`'s
//! ARGB8888 and a cursor plane's default `pixel blend mode` mean, so the
//! copy only reorders the bytes. The image goes to the buffer's top-left
//! corner and the rest is transparent; the plane always shows the whole
//! buffer, since many drivers take no other size.

use std::{io, sync::Arc};

use shingle_frontend::CursorImage;

use crate::kms::{DumbFramebuffer, FramebufferId, KmsDevice};

/// Copies a `width` by `height` premultiplied RGBA image into the top-left
/// corner of ARGB8888 rows `stride` bytes apart, clearing everything else
/// to transparent. ARGB8888 is `B, G, R, A` in memory.
pub fn copy_rgba_to_argb8888(
  rgba: &[u8],
  width: u32,
  height: u32,
  destination: &mut [u8],
  stride: u32,
) -> io::Result<()> {
  let row_bytes = width as usize * 4;
  let stride = stride as usize;
  let rows = destination.len() / stride.max(1);
  if rgba.len() != row_bytes * height as usize
    || stride < row_bytes
    || rows < height as usize
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "the cursor does not fit its buffer",
    ));
  }
  destination.fill(0);
  for (source, target) in rgba
    .chunks_exact(row_bytes)
    .zip(destination.chunks_mut(stride))
  {
    for (pixel, out) in source.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
      out[0] = pixel[2];
      out[1] = pixel[1];
      out[2] = pixel[0];
      out[3] = pixel[3];
    }
  }
  Ok(())
}

/// What a buffer holds: the image's pixels and size.
#[derive(Clone, Debug)]
struct Held {
  rgba: Arc<[u8]>,
  size: (u32, u32),
}

impl Held {
  fn is(&self, image: &CursorImage) -> bool {
    Arc::ptr_eq(&self.rgba, &image.rgba)
      && self.size == (image.width, image.height)
  }
}

/// Two cursor buffers, one shown and one written.
#[derive(Debug)]
pub struct CursorBuffers {
  buffers: [DumbFramebuffer; 2],
  /// What each buffer holds.
  held: [Option<Held>; 2],
  /// The buffer on screen.
  front: usize,
  /// The buffer the last [`CursorBuffers::prepare`] chose, until its
  /// commit is accepted or given up.
  prepared: Option<usize>,
}

impl CursorBuffers {
  /// Allocates two transparent `size` buffers.
  pub fn new(
    device: &mut dyn KmsDevice,
    (width, height): (u32, u32),
  ) -> io::Result<Self> {
    let first = device.create_argb_framebuffer(width, height)?;
    let second = match device.create_argb_framebuffer(width, height) {
      Ok(second) => second,
      Err(error) => {
        let _ = device.destroy_dumb_framebuffer(&first);
        return Err(error);
      }
    };
    Ok(Self {
      buffers: [first, second],
      held: [None, None],
      front: 0,
      prepared: None,
    })
  }

  /// Whether the buffer on screen holds `image`.
  pub fn shows(&self, image: &CursorImage) -> bool {
    self.held[self.front]
      .as_ref()
      .is_some_and(|held| held.is(image))
  }

  /// The buffers' size, which the cursor plane shows.
  pub fn size(&self) -> (u32, u32) {
    (self.buffers[0].width, self.buffers[0].height)
  }

  /// The framebuffer that shows `image`: the one on screen if it holds it
  /// already, else the other one, written now. The caller commits it and
  /// then calls [`CursorBuffers::committed`]; until then the buffer on
  /// screen is never written.
  pub fn prepare(
    &mut self,
    device: &mut dyn KmsDevice,
    image: &CursorImage,
  ) -> io::Result<FramebufferId> {
    if self.shows(image) {
      self.prepared = Some(self.front);
      return Ok(self.buffers[self.front].framebuffer);
    }
    let back = 1 - self.front;
    let buffer = self.buffers[back];
    let mut result = Ok(());
    // Forget what the buffer held before writing over it, in case the
    // write fails halfway.
    self.held[back] = None;
    device.write_dumb_framebuffer(&buffer, &mut |pixels| {
      result = copy_rgba_to_argb8888(
        &image.rgba,
        image.width,
        image.height,
        pixels,
        buffer.stride,
      );
    })?;
    result?;
    self.held[back] = Some(Held {
      rgba: Arc::clone(&image.rgba),
      size: (image.width, image.height),
    });
    self.prepared = Some(back);
    Ok(buffer.framebuffer)
  }

  /// The commit with the prepared buffer was accepted: it is on screen,
  /// or will be at the next vblank, which no write precedes.
  pub fn committed(&mut self) {
    if let Some(prepared) = self.prepared.take() {
      self.front = prepared;
    }
  }

  /// The commit with the prepared buffer was refused.
  pub fn abandon(&mut self) {
    self.prepared = None;
  }

  /// Frees both buffers.
  pub fn destroy(&mut self, device: &mut dyn KmsDevice) {
    for buffer in &self.buffers {
      let _ = device.destroy_dumb_framebuffer(buffer);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::fake::FakeKms;

  fn image(rgba: &[u8], width: u32, height: u32) -> CursorImage {
    CursorImage {
      width,
      height,
      rgba: Arc::from(rgba),
      position: (0, 0),
    }
  }

  #[test]
  fn pixels_are_reordered_and_the_rest_cleared() {
    let rgba = [1, 2, 3, 4, 5, 6, 7, 8];
    let mut buffer = vec![0xaa; 16 * 3];
    copy_rgba_to_argb8888(&rgba, 2, 1, &mut buffer, 16).unwrap();
    assert_eq!(&buffer[..8], &[3, 2, 1, 4, 7, 6, 5, 8]);
    assert!(buffer[8..].iter().all(|byte| *byte == 0), "transparent");
    assert!(copy_rgba_to_argb8888(&rgba, 2, 1, &mut [0; 4], 4).is_err());
    assert!(copy_rgba_to_argb8888(&rgba, 2, 2, &mut buffer, 16).is_err());
  }

  #[test]
  fn a_new_image_goes_into_the_buffer_off_screen() {
    let mut device = FakeKms::single_output();
    let mut buffers = CursorBuffers::new(&mut device, (4, 4)).unwrap();
    assert_eq!(buffers.size(), (4, 4));
    assert_eq!(device.argb.len(), 2, "two ARGB8888 framebuffers");
    let arrow = image(&[9; 16], 2, 2);
    let first = buffers.prepare(&mut device, &arrow).unwrap();
    assert_ne!(first, buffers.buffers[0].framebuffer, "not the front one");
    buffers.committed();
    let (buffer, pixels) = &device.buffers[&first.0];
    let stride = buffer.stride as usize;
    assert_eq!(pixels[..8], [9; 8]);
    assert_eq!(pixels[stride..stride + 8], [9; 8], "the second row");
    assert!(pixels[8..stride].iter().all(|byte| *byte == 0));
    assert!(pixels[stride + 8..].iter().all(|byte| *byte == 0));
    // The same image again needs no write and keeps the buffer.
    assert_eq!(buffers.prepare(&mut device, &arrow).unwrap(), first);
    buffers.committed();
    // Another image goes into the other buffer; a refused commit keeps
    // the one on screen.
    let hand = image(&[7; 16], 2, 2);
    let second = buffers.prepare(&mut device, &hand).unwrap();
    assert_ne!(second, first);
    buffers.abandon();
    assert_eq!(buffers.prepare(&mut device, &arrow).unwrap(), first);
    buffers.committed();
    buffers.destroy(&mut device);
    assert!(device.argb.is_empty() && device.buffers.is_empty());
  }
}
