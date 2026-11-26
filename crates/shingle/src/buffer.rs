use std::{
  ffi::c_void,
  os::fd::OwnedFd,
  sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  },
};

use rustix::{
  fs::fstat,
  mm::{MapFlags, ProtFlags, mmap, munmap},
};
use shingle_core::Rect;
use wayland_server::{
  Resource,
  protocol::{
    wl_buffer::WlBuffer,
    wl_output,
    wl_shm::Format,
    wl_shm_pool::{self, WlShmPool},
  },
};

use crate::{
  BufferSnapshot, SnapshotContent,
  buffer_lock::{BufferLock, BufferLocks, ReleaseQueue},
};

pub(crate) struct ShmPool {
  fd: OwnedFd,
  size: AtomicUsize,
  max_buffer_size: AtomicUsize,
}

impl ShmPool {
  pub(crate) fn new(
    fd: OwnedFd,
    size: i32,
    max_buffer_size: u32,
  ) -> Result<Self, String> {
    if size <= 0 {
      return Err("shared memory pool size must be positive".into());
    }
    let size = size as usize;
    let actual = fstat(&fd).map_err(|error| error.to_string())?.st_size;
    if actual < 0 || (actual as u64) < size as u64 {
      return Err(
        "shared memory pool is smaller than its declared size".into(),
      );
    }
    Ok(Self {
      fd,
      size: AtomicUsize::new(size),
      max_buffer_size: AtomicUsize::new(max_buffer_size as usize),
    })
  }

  pub(crate) fn resize(&self, size: i32) -> Result<(), String> {
    if size < 0 {
      return Err("shared memory pool size cannot be negative".into());
    }
    let size = size as usize;
    let current = self.size.load(Ordering::Acquire);
    if size < current {
      return Err("shared memory pools cannot shrink".into());
    }
    let actual = fstat(&self.fd).map_err(|error| error.to_string())?.st_size;
    if actual < 0 || (actual as u64) < size as u64 {
      return Err("shared memory pool is smaller than its resized size".into());
    }
    self.size.store(size, Ordering::Release);
    Ok(())
  }

  fn size(&self) -> usize {
    self.size.load(Ordering::Acquire)
  }

  fn max_buffer_size(&self) -> u32 {
    self.max_buffer_size.load(Ordering::Acquire) as u32
  }

  pub(crate) fn set_max_buffer_size(&self, maximum: u32) {
    self
      .max_buffer_size
      .store(maximum as usize, Ordering::Release);
  }
}

/// Where an attached buffer's pixels live (docs/design/dmabuf.md,
/// section 4). DB-4 adds DMA-BUFs.
#[derive(Clone)]
pub(crate) enum BufferContent {
  /// A `wl_shm` buffer in its client's pool.
  Shm(ShmView),
  /// A `wp_single_pixel_buffer_v1` colour, already RGBA.
  SinglePixel([u8; 4]),
}

/// Where a `wl_shm` buffer sits in its pool, and how.
#[derive(Clone)]
pub(crate) struct ShmView {
  pool: Arc<ShmPool>,
  offset: usize,
  stride: usize,
  format: Format,
}

/// A client buffer as the compositor knows it: one per `wl_buffer`,
/// cloned wherever it is attached.
#[derive(Clone)]
pub(crate) struct Attachment {
  pub(crate) resource: WlBuffer,
  pub(crate) content: BufferContent,
  pub(crate) width: u32,
  pub(crate) height: u32,
  /// The buffer's lock count, shared by every clone.
  locks: Arc<BufferLocks>,
}

impl Attachment {
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn new(
    resource: WlBuffer,
    pool: Arc<ShmPool>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: Format,
    releases: &ReleaseQueue,
  ) -> Result<Self, String> {
    if offset < 0 || width <= 0 || height <= 0 || stride < 0 {
      return Err("buffer dimensions, stride, and offset are invalid".into());
    }
    let (width, height, stride, offset) = (
      width as u32,
      height as u32,
      stride as usize,
      offset as usize,
    );
    let max_buffer_size = pool.max_buffer_size();
    if width > max_buffer_size || height > max_buffer_size {
      return Err(format!(
        "buffer dimensions exceed the {max_buffer_size}-pixel limit"
      ));
    }
    let minimum_stride = (width as usize)
      .checked_mul(4)
      .ok_or_else(|| "buffer stride overflows address space".to_string())?;
    if stride < minimum_stride {
      return Err("buffer stride is smaller than one row".into());
    }
    let required = if height == 0 {
      0
    } else {
      stride
        .checked_mul(height as usize - 1)
        .and_then(|prefix| prefix.checked_add(minimum_stride))
        .ok_or_else(|| "buffer extent overflows address space".to_string())?
    };
    let end = offset
      .checked_add(required)
      .ok_or_else(|| "buffer offset overflows address space".to_string())?;
    if end > pool.size() {
      return Err(
        "buffer extends past the end of its shared memory pool".into(),
      );
    }
    Ok(Self {
      locks: BufferLocks::new(resource.clone(), releases.clone()),
      resource,
      content: BufferContent::Shm(ShmView {
        pool,
        offset,
        stride,
        format,
      }),
      width,
      height,
    })
  }

  pub(crate) fn single_pixel(
    resource: WlBuffer,
    rgba: [u8; 4],
    releases: &ReleaseQueue,
  ) -> Self {
    Self {
      locks: BufferLocks::new(resource.clone(), releases.clone()),
      resource,
      content: BufferContent::SinglePixel(rgba),
      width: 1,
      height: 1,
    }
  }

  /// Locks the buffer: it is not released while the lock lives.
  pub(crate) fn lock(&self) -> BufferLock {
    self.locks.lock()
  }

  /// How many locks the buffer has.
  #[cfg(test)]
  pub(crate) fn lock_count(&self) -> usize {
    self.locks.count()
  }

  /// Whether the buffer is a `wl_shm` buffer.
  pub(crate) fn is_shm(&self) -> bool {
    matches!(self.content, BufferContent::Shm(_))
  }

  /// Copies the buffer's pixels into a snapshot. This sends no release:
  /// whoever holds the buffer's lock drops it once the copy is complete.
  pub(crate) fn snapshot(
    &self,
    damage: Vec<Rect>,
    offset_x: i32,
    offset_y: i32,
  ) -> Result<BufferSnapshot, String> {
    let rgba = match &self.content {
      BufferContent::SinglePixel(rgba) => rgba.to_vec(),
      BufferContent::Shm(view) => view.rgba(self.width, self.height)?,
    };
    Ok(BufferSnapshot {
      width: self.width,
      height: self.height,
      source: None,
      scale: 1,
      transform: wl_output::Transform::Normal,
      content: SnapshotContent::Pixels(rgba.into()),
      damage,
      offset_x,
      offset_y,
    })
  }

  /// Writes tightly packed RGBA rows into a `wl_shm` buffer, as
  /// screencopy does, and releases it.
  pub(crate) fn write_rgba(&self, rgba: &[u8]) -> Result<(), String> {
    let BufferContent::Shm(view) = &self.content else {
      return Err("buffer has no shared memory pool".into());
    };
    view.write_rgba(self.width, self.height, rgba)?;
    self.resource.release();
    Ok(())
  }

  pub(crate) fn validate_format(
    pool: &WlShmPool,
    format: Format,
  ) -> Result<(), wayland_server::WEnum<Format>> {
    match format {
      Format::Argb8888
      | Format::Xrgb8888
      | Format::Abgr8888
      | Format::Xbgr8888 => Ok(()),
      _ => {
        pool.post_error(
          wl_shm_pool::Error::InvalidFormat,
          "Shingle supports ARGB8888, XRGB8888, ABGR8888, and XBGR8888",
        );
        Err(wayland_server::WEnum::Value(format))
      }
    }
  }
}

impl ShmView {
  /// Converts the `width` by `height` buffer into tightly packed RGBA
  /// rows: ARGB/XRGB are swizzled and X formats get opaque alpha.
  fn rgba(&self, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let required = self
      .stride
      .checked_mul(height as usize - 1)
      .and_then(|prefix| prefix.checked_add(width as usize * 4))
      .and_then(|extent| self.offset.checked_add(extent))
      .ok_or_else(|| "buffer extent overflows address space".to_string())?;
    let pool_size = self.pool.size();
    if required > pool_size {
      return Err("buffer no longer fits its shared memory pool".into());
    }
    let actual = fstat(&self.pool.fd)
      .map_err(|error| error.to_string())?
      .st_size;
    if actual < 0 || (actual as u64) < pool_size as u64 {
      return Err("shared memory pool was truncated".into());
    }

    // SAFETY: the mapping is read-only, its length was checked against the
    // owned fd, and Mapping unmaps it after the owned RGBA copy is complete.
    let pointer = unsafe {
      mmap(
        std::ptr::null_mut(),
        pool_size,
        ProtFlags::READ,
        MapFlags::SHARED,
        &self.pool.fd,
        0,
      )
    }
    .map_err(|error| error.to_string())?;
    let mapping = Mapping {
      pointer,
      len: pool_size,
    };
    // SAFETY: `required <= pool_size`; each row below additionally checks
    // its own stride and pixel extent before constructing a slice.
    let bytes = unsafe {
      std::slice::from_raw_parts(mapping.pointer.cast::<u8>(), pool_size)
    };
    let rgba_len = (width as usize)
      .checked_mul(height as usize)
      .and_then(|pixels| pixels.checked_mul(4))
      .ok_or_else(|| {
        "RGBA allocation size overflows address space".to_string()
      })?;
    let mut rgba = Vec::with_capacity(rgba_len);
    for row in 0..height as usize {
      let start = self.offset + row * self.stride;
      let end = start + width as usize * 4;
      let source = bytes
        .get(start..end)
        .ok_or_else(|| "buffer row extends outside its mapping".to_string())?;
      match self.format {
        Format::Argb8888 | Format::Xrgb8888 => {
          for pixel in source.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[
              pixel[2],
              pixel[1],
              pixel[0],
              if self.format == Format::Xrgb8888 {
                255
              } else {
                pixel[3]
              },
            ]);
          }
        }
        Format::Abgr8888 | Format::Xbgr8888 => {
          for pixel in source.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[
              pixel[0],
              pixel[1],
              pixel[2],
              if self.format == Format::Xbgr8888 {
                255
              } else {
                pixel[3]
              },
            ]);
          }
        }
        _ => {
          return Err("unsupported shared memory pixel format".into());
        }
      }
    }
    Ok(rgba)
  }

  /// Writes tightly packed RGBA rows into the `width` by `height` buffer,
  /// swizzled to BGRA.
  fn write_rgba(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<(), String> {
    let expected = (width as usize)
      .checked_mul(height as usize)
      .and_then(|pixels| pixels.checked_mul(4))
      .ok_or_else(|| "RGBA input size overflows address space".to_string())?;
    if rgba.len() != expected {
      return Err("RGBA input has the wrong dimensions".into());
    }
    let pool_size = self.pool.size();
    // SAFETY: the mapping is shared and writable, and every row is bounded
    // by the validated attachment extent before being written.
    let pointer = unsafe {
      mmap(
        std::ptr::null_mut(),
        pool_size,
        ProtFlags::READ | ProtFlags::WRITE,
        MapFlags::SHARED,
        &self.pool.fd,
        0,
      )
    }
    .map_err(|error| error.to_string())?;
    let mapping = Mapping {
      pointer,
      len: pool_size,
    };
    // SAFETY: the mapping remains alive for all row writes.
    let bytes = unsafe {
      std::slice::from_raw_parts_mut(mapping.pointer.cast::<u8>(), pool_size)
    };
    let row_bytes = width as usize * 4;
    for row in 0..height as usize {
      let start = self.offset + row * self.stride;
      let destination = bytes
        .get_mut(start..start + row_bytes)
        .ok_or_else(|| "buffer row extends outside its mapping".to_string())?;
      let source = &rgba[row * row_bytes..][..row_bytes];
      for (destination, source) in destination
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(source.as_chunks::<4>().0)
      {
        *destination = [source[2], source[1], source[0], source[3]];
      }
    }
    Ok(())
  }
}

struct Mapping {
  pointer: *mut c_void,
  len: usize,
}

impl Drop for Mapping {
  fn drop(&mut self) {
    // SAFETY: this is the exact live mapping returned by mmap, and no
    // slices into it survive this guard.
    let _ = unsafe { munmap(self.pointer, self.len) };
  }
}
