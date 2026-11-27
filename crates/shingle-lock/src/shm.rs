use std::{
  fs::File,
  io,
  os::fd::AsFd,
  ptr::{self, NonNull},
  slice,
};

use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use wayland_client::{
  Dispatch, QueueHandle,
  protocol::{wl_buffer, wl_shm, wl_shm_pool},
};

/// Which output's buffer a `wl_buffer` is, and which of its buffers.
#[derive(Clone, Copy, Debug)]
pub struct BufferData {
  pub output: u32,
  pub id: u64,
}

/// An XRGB8888 `wl_shm` buffer mapped into this process.
pub struct ShmBuffer {
  pub id: u64,
  pub buffer: wl_buffer::WlBuffer,
  pub busy: bool,
  pool: wl_shm_pool::WlShmPool,
  _file: File,
  mapping: NonNull<std::ffi::c_void>,
  len: usize,
  stride: usize,
}

impl ShmBuffer {
  /// A `width` by `height` buffer of output `output`.
  pub fn new<State>(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    output: u32,
    id: u64,
    width: u32,
    height: u32,
  ) -> io::Result<Self>
  where
    State: Dispatch<wl_buffer::WlBuffer, BufferData>
      + Dispatch<wl_shm_pool::WlShmPool, ()>
      + 'static,
  {
    let stride = width.checked_mul(4).ok_or_else(|| {
      io::Error::new(io::ErrorKind::InvalidInput, "buffer stride overflow")
    })?;
    let len =
      (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| {
          io::Error::new(io::ErrorKind::InvalidInput, "buffer size overflow")
        })?;
    let protocol_len = i32::try_from(len).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "buffer is too large")
    })?;
    let protocol_stride = i32::try_from(stride).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "buffer stride is too large")
    })?;
    let protocol_width = i32::try_from(width).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "buffer width is too large")
    })?;
    let protocol_height = i32::try_from(height).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "buffer height is too large")
    })?;
    let file = tempfile::tempfile()?;
    file.set_len(len as u64)?;
    // SAFETY: The file has the requested length, the mapping is shared and
    // uniquely owned by this buffer, and the returned non-null mapping is
    // unmapped exactly once in Drop.
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
    let mapping = NonNull::new(mapping).ok_or_else(|| {
      io::Error::new(io::ErrorKind::OutOfMemory, "mmap returned a null address")
    })?;
    let pool = shm.create_pool(file.as_fd(), protocol_len, qh, ());
    let buffer = pool.create_buffer(
      0,
      protocol_width,
      protocol_height,
      protocol_stride,
      wl_shm::Format::Xrgb8888,
      qh,
      BufferData { output, id },
    );
    Ok(Self {
      id,
      buffer,
      busy: false,
      pool,
      _file: file,
      mapping,
      len,
      stride: stride as usize,
    })
  }

  /// The buffer's bytes, rows `stride` apart, for a frame to be drawn
  /// into while the compositor does not hold it.
  pub fn bytes_mut(&mut self) -> io::Result<&mut [u8]> {
    if self.busy {
      return Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "the compositor still holds the buffer",
      ));
    }
    // SAFETY: The mapping is valid for len bytes and only this client writes
    // it while the compositor has released the buffer; the borrow of self
    // keeps the mapping alive.
    Ok(unsafe {
      slice::from_raw_parts_mut(self.mapping.as_ptr().cast::<u8>(), self.len)
    })
  }

  /// Bytes from one row to the next.
  pub fn stride(&self) -> usize {
    self.stride
  }
}

impl Drop for ShmBuffer {
  fn drop(&mut self) {
    self.buffer.destroy();
    self.pool.destroy();
    // SAFETY: This is the unique mapping returned by mmap, no mapped slice
    // remains live, and destroying the Wayland objects does not invalidate the
    // compositor's independent mapping.
    if let Err(error) = unsafe { munmap(self.mapping.as_ptr(), self.len) } {
      eprintln!("shingle-lock: failed to unmap shared memory: {error}");
    }
  }
}
