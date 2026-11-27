//! `wl_shm` buffers covering the output: a premultiplied backdrop written
//! once, and the panel's rows copied in each frame.

use std::{
  fs::File,
  io,
  os::fd::AsFd,
  ptr::{self, NonNull},
  slice,
};

use rustix::{
  fs::{MemfdFlags, memfd_create},
  mm::{MapFlags, ProtFlags, mmap, munmap},
};
use wayland_client::{
  Dispatch, QueueHandle,
  protocol::{wl_buffer, wl_shm, wl_shm_pool},
};

/// One buffer and its mapping.
pub struct ShmBuffer {
  /// The buffer.
  pub buffer: wl_buffer::WlBuffer,
  /// Whether the compositor holds it.
  pub busy: bool,
  /// Whether the backdrop still has to be written.
  pub fresh: bool,
  /// Its size in pixels.
  pub width: u32,
  /// Its height in pixels.
  pub height: u32,
  pool: wl_shm_pool::WlShmPool,
  _file: File,
  mapping: NonNull<std::ffi::c_void>,
  len: usize,
}

impl ShmBuffer {
  /// A `width` × `height` `ARGB8888` buffer whose `wl_buffer` carries
  /// `data`.
  pub fn new<State>(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    width: u32,
    height: u32,
    data: usize,
  ) -> io::Result<Self>
  where
    State: Dispatch<wl_buffer::WlBuffer, usize>
      + Dispatch<wl_shm_pool::WlShmPool, ()>
      + 'static,
  {
    let invalid =
      |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what);
    let stride = width.checked_mul(4).ok_or_else(|| invalid("stride"))?;
    let len = (stride as usize)
      .checked_mul(height as usize)
      .filter(|len| *len > 0)
      .ok_or_else(|| invalid("size"))?;
    let protocol = |value: usize| {
      i32::try_from(value).map_err(|_| invalid("buffer too large"))
    };
    let file = File::from(memfd_create(
      "shingle-launcher",
      MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    file.set_len(len as u64)?;
    // SAFETY: the file has `len` bytes; the shared mapping is owned by
    // this buffer alone and unmapped once, in Drop.
    let mapping = unsafe {
      mmap(
        ptr::null_mut(),
        len,
        ProtFlags::READ | ProtFlags::WRITE,
        MapFlags::SHARED,
        file.as_fd(),
        0,
      )?
    };
    let mapping = NonNull::new(mapping)
      .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "mmap"))?;
    let pool = shm.create_pool(file.as_fd(), protocol(len)?, qh, ());
    let buffer = pool.create_buffer(
      0,
      protocol(width as usize)?,
      protocol(height as usize)?,
      protocol(stride as usize)?,
      wl_shm::Format::Argb8888,
      qh,
      data,
    );
    Ok(Self {
      buffer,
      busy: false,
      fresh: true,
      width,
      height,
      pool,
      _file: file,
      mapping,
      len,
    })
  }

  fn pixels(&mut self) -> &mut [u8] {
    // SAFETY: the mapping is valid for `len` bytes, and only this client
    // writes it, while the compositor does not hold the buffer.
    unsafe { slice::from_raw_parts_mut(self.mapping.as_ptr().cast(), self.len) }
  }

  /// Fills the whole buffer with `pixel` (four `ARGB8888` bytes).
  pub fn fill(&mut self, pixel: [u8; 4]) {
    for chunk in self.pixels().chunks_exact_mut(4) {
      chunk.copy_from_slice(&pixel);
    }
  }

  /// Copies `bytes` to row `y` from column `x`, cut at the right edge.
  pub fn write_row(&mut self, x: u32, y: u32, bytes: &[u8]) {
    if y >= self.height || x >= self.width {
      return;
    }
    let start = (y as usize * self.width as usize + x as usize) * 4;
    let room = (self.width - x) as usize * 4;
    let count = bytes.len().min(room);
    self.pixels()[start..start + count].copy_from_slice(&bytes[..count]);
  }
}

impl Drop for ShmBuffer {
  fn drop(&mut self) {
    self.buffer.destroy();
    self.pool.destroy();
    // SAFETY: the unique mapping from `new`, with no slice of it alive.
    if let Err(error) = unsafe { munmap(self.mapping.as_ptr(), self.len) } {
      eprintln!("shingle-launcher: cannot unmap a buffer: {error}");
    }
  }
}

/// `colour` as premultiplied `ARGB8888` bytes in memory order (B, G, R,
/// A), each component times 255 as the panel's colours are.
pub fn premultiplied(colour: (f32, f32, f32, f32)) -> [u8; 4] {
  let (r, g, b, a) = colour;
  let a = a.clamp(0.0, 1.0);
  let byte = |value: f32| (value.clamp(0.0, 1.0) * a * 255.0).round() as u8;
  [byte(b), byte(g), byte(r), (a * 255.0).round() as u8]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn backdrops_are_premultiplied() {
    assert_eq!(premultiplied((0.0, 0.0, 0.0, 0.5)), [0, 0, 0, 128]);
    assert_eq!(premultiplied((1.0, 0.5, 0.0, 1.0)), [0, 128, 255, 255]);
    assert_eq!(premultiplied((1.0, 1.0, 1.0, 0.0)), [0, 0, 0, 0]);
  }
}
