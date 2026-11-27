use std::{
  ffi::c_void,
  os::fd::OwnedFd,
  sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicUsize, Ordering},
  },
};

use rustix::{
  fs::{SealFlags, fcntl_get_seals, fstat},
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
  BufferSnapshot, DmabufContent, SnapshotContent,
  buffer_lock::{BufferLock, BufferLocks, ReleaseQueue},
  linux_dmabuf::DmabufBuffer,
};

/// The `wl_shm` formats captures write: BGRA bytes, with or without
/// alpha, as wlroots' screencopy offers them.
pub(crate) const CAPTURE_SHM_FORMATS: [Format; 2] =
  [Format::Argb8888, Format::Xrgb8888];

/// A `wl_shm_pool`: the client's fd and its declared size.
///
/// A pool whose fd carries `F_SEAL_SHRINK` cannot be truncated under the
/// compositor, so it is mapped once and kept mapped while it lives, and
/// mapped again after it grew. Any other pool is mapped for each access,
/// after an `fstat` check that the client did not truncate it, because a
/// read past the end of a truncated file raises `SIGBUS`
/// (docs/design/dmabuf.md, section 5).
pub(crate) struct ShmPool {
  fd: OwnedFd,
  size: AtomicUsize,
  max_buffer_size: AtomicUsize,
  /// Whether the fd is sealed against shrinking.
  sealed: bool,
  /// The persistent mapping of a sealed pool, once one was needed.
  mapping: Mutex<Option<Arc<Mapping>>>,
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
    let sealed = is_sealed(fcntl_get_seals(&fd).ok());
    Ok(Self {
      fd,
      size: AtomicUsize::new(size),
      max_buffer_size: AtomicUsize::new(max_buffer_size as usize),
      sealed,
      mapping: Mutex::new(None),
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

  /// Whether the pool is kept mapped, because its fd is sealed against
  /// shrinking.
  #[cfg(test)]
  pub(crate) fn is_sealed(&self) -> bool {
    self.sealed
  }

  /// Whether a persistent mapping exists, and of how many bytes.
  #[cfg(test)]
  pub(crate) fn mapped_len(&self) -> Option<usize> {
    self
      .mapping
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .as_ref()
      .map(|mapping| mapping.len)
  }

  /// Calls `read` with the whole pool, read-only. A sealed pool uses its
  /// persistent mapping, made here or made again at the pool's new size;
  /// any other pool is checked with `fstat` and mapped for this call.
  fn read<R>(&self, read: impl FnOnce(&[u8]) -> R) -> Result<R, String> {
    let size = self.size();
    let mapping = if self.sealed {
      let mut kept =
        self.mapping.lock().unwrap_or_else(PoisonError::into_inner);
      match kept.as_ref() {
        Some(mapping) if mapping.len == size => Arc::clone(mapping),
        _ => {
          let mapping = Arc::new(Mapping::new(&self.fd, size)?);
          *kept = Some(Arc::clone(&mapping));
          mapping
        }
      }
    } else {
      let actual = fstat(&self.fd).map_err(|error| error.to_string())?.st_size;
      if actual < 0 || (actual as u64) < size as u64 {
        return Err("shared memory pool was truncated".into());
      }
      Arc::new(Mapping::new(&self.fd, size)?)
    };
    // SAFETY: the mapping is `len` bytes long and lives until the end of
    // this call. A sealed pool's fd cannot shrink, and any other pool was
    // just checked to be at least that long.
    let bytes = unsafe {
      std::slice::from_raw_parts(mapping.pointer.cast::<u8>(), mapping.len)
    };
    Ok(read(bytes))
  }
}

/// Whether `seals`, from `F_GET_SEALS`, keep a pool from shrinking. An fd
/// that cannot be sealed, such as a plain file, has none (`None`).
fn is_sealed(seals: Option<SealFlags>) -> bool {
  seals.is_some_and(|seals| seals.contains(SealFlags::SHRINK))
}

/// Where an attached buffer's pixels live (docs/design/dmabuf.md,
/// section 4).
#[derive(Clone)]
pub(crate) enum BufferContent {
  /// A `wl_shm` buffer in its client's pool.
  Shm(ShmView),
  /// A `wp_single_pixel_buffer_v1` colour, already RGBA.
  SinglePixel([u8; 4]),
  /// A `zwp_linux_dmabuf_v1` buffer, which only the GPU reads.
  Dmabuf(Arc<DmabufBuffer>),
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

  /// A `zwp_linux_dmabuf_v1` buffer.
  pub(crate) fn dmabuf(
    resource: WlBuffer,
    buffer: Arc<DmabufBuffer>,
    releases: &ReleaseQueue,
  ) -> Self {
    Self {
      locks: BufferLocks::new(resource.clone(), releases.clone()),
      resource,
      width: buffer.width,
      height: buffer.height,
      content: BufferContent::Dmabuf(buffer),
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

  /// Makes the snapshot a commit shows. This sends no release: whoever
  /// holds the buffer's lock drops it once the snapshot no longer needs
  /// the buffer.
  ///
  /// Single-pixel buffers are copied. A `wl_shm` buffer is copied too,
  /// unless `upload` is set and there is a `lock`: then the snapshot
  /// keeps a view of the pool and a clone of the lock in a
  /// [`ShmContent`] until the frontend has uploaded it
  /// (docs/design/dmabuf.md, section 5). A DMA-BUF is not copied; its
  /// snapshot keeps a clone of `lock`, so the buffer stays with the
  /// compositor while it is shown, and needs one.
  pub(crate) fn snapshot(
    &self,
    damage: Vec<Rect>,
    offset_x: i32,
    offset_y: i32,
    lock: Option<&BufferLock>,
    upload: bool,
  ) -> Result<BufferSnapshot, String> {
    let content = match (&self.content, lock) {
      (BufferContent::SinglePixel(rgba), _) => {
        SnapshotContent::Pixels(rgba.to_vec().into())
      }
      (BufferContent::Shm(view), Some(lock)) if upload => {
        // A buffer that no longer fits its pool is the client's error at
        // commit, as with the copy.
        view.check(self.width, self.height)?;
        SnapshotContent::Shm(ShmContent::new(
          view.clone(),
          self.width,
          self.height,
          lock.clone(),
        ))
      }
      (BufferContent::Shm(view), _) => {
        SnapshotContent::Pixels(view.rgba(self.width, self.height)?.into())
      }
      (BufferContent::Dmabuf(buffer), lock) => {
        SnapshotContent::Dmabuf(DmabufContent {
          buffer: Arc::clone(buffer),
          lock: lock.ok_or("a DMA-BUF is shown only while locked")?.clone(),
        })
      }
    };
    Ok(BufferSnapshot {
      width: self.width,
      height: self.height,
      source: None,
      scale: 1,
      transform: wl_output::Transform::Normal,
      content,
      damage,
      offset_x,
      offset_y,
    })
  }

  /// Writes tightly packed RGBA rows into a capture's `wl_shm` buffer of
  /// one of [`CAPTURE_SHM_FORMATS`]. Capture buffers are not released:
  /// the protocols leave `wl_buffer.release` unused.
  pub(crate) fn write_capture(&self, rgba: &[u8]) -> Result<(), String> {
    let BufferContent::Shm(view) = &self.content else {
      return Err("buffer has no shared memory pool".into());
    };
    if !CAPTURE_SHM_FORMATS.contains(&view.format) {
      return Err(format!("cannot capture into {:?} buffers", view.format));
    }
    view.write_rgba(self.width, self.height, rgba)
  }

  /// The DMA-BUF of a linux-dmabuf buffer.
  pub(crate) fn dmabuf_buffer(&self) -> Option<&Arc<DmabufBuffer>> {
    match &self.content {
      BufferContent::Dmabuf(buffer) => Some(buffer),
      _ => None,
    }
  }

  /// The `wl_shm` format of a shared-memory buffer.
  pub(crate) fn shm_format(&self) -> Option<Format> {
    match &self.content {
      BufferContent::Shm(view) => Some(view.format),
      _ => None,
    }
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
  /// The DRM fourcc of the buffer's format. `wl_shm` codes are DRM
  /// fourccs, except ARGB8888 and XRGB8888, which are 0 and 1.
  pub(crate) fn fourcc(&self) -> u32 {
    match self.format {
      Format::Argb8888 => u32::from_le_bytes(*b"AR24"),
      Format::Xrgb8888 => u32::from_le_bytes(*b"XR24"),
      format => format as u32,
    }
  }

  /// Bytes from the start of one row to the start of the next.
  pub(crate) fn stride(&self) -> usize {
    self.stride
  }

  /// Whether the format ignores the alpha byte.
  pub(crate) fn is_opaque(&self) -> bool {
    matches!(self.format, Format::Xrgb8888 | Format::Xbgr8888)
  }

  /// The length of the `width` by `height` buffer in its pool, from its
  /// first pixel to the end of its last row's pixels.
  fn extent(&self, width: u32, height: u32) -> Result<usize, String> {
    self
      .stride
      .checked_mul((height as usize).saturating_sub(1))
      .and_then(|prefix| prefix.checked_add(width as usize * 4))
      .ok_or_else(|| "buffer extent overflows address space".to_string())
  }

  /// Checks that the `width` by `height` buffer still fits its pool.
  fn check(&self, width: u32, height: u32) -> Result<(), String> {
    let required = self
      .offset
      .checked_add(self.extent(width, height)?)
      .ok_or_else(|| "buffer extent overflows address space".to_string())?;
    if required > self.pool.size() {
      return Err("buffer no longer fits its shared memory pool".into());
    }
    Ok(())
  }

  /// Calls `read` with the `width` by `height` buffer's bytes in its
  /// pool, from its first pixel to the end of its last row's pixels,
  /// rows [`ShmView::stride`] bytes apart.
  pub(crate) fn read<R>(
    &self,
    width: u32,
    height: u32,
    read: impl FnOnce(&[u8]) -> R,
  ) -> Result<R, String> {
    self.check(width, height)?;
    let extent = self.extent(width, height)?;
    let offset = self.offset;
    self.pool.read(|pool| {
      pool
        .get(offset..offset + extent)
        .map(read)
        .ok_or_else(|| "buffer extends outside its mapping".to_string())
    })?
  }

  /// Converts the `width` by `height` buffer into tightly packed RGBA
  /// rows: ARGB/XRGB are swizzled and X formats get opaque alpha.
  pub(crate) fn rgba(
    &self,
    width: u32,
    height: u32,
  ) -> Result<Vec<u8>, String> {
    let (format, stride) = (self.format, self.stride);
    self.read(width, height, |bytes| {
      to_rgba(bytes, width, height, stride, format)
    })?
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

/// Converts `width` by `height` pixels of `format`, rows `stride` bytes
/// apart in `bytes`, into tightly packed RGBA rows.
pub(crate) fn to_rgba(
  bytes: &[u8],
  width: u32,
  height: u32,
  stride: usize,
  format: Format,
) -> Result<Vec<u8>, String> {
  let rgba_len = (width as usize)
    .checked_mul(height as usize)
    .and_then(|pixels| pixels.checked_mul(4))
    .ok_or_else(|| {
      "RGBA allocation size overflows address space".to_string()
    })?;
  let mut rgba = Vec::with_capacity(rgba_len);
  for row in 0..height as usize {
    let start = row * stride;
    let source = bytes
      .get(start..start + width as usize * 4)
      .ok_or_else(|| "buffer row extends outside its mapping".to_string())?;
    let (swizzle, opaque) = match format {
      Format::Argb8888 => (true, false),
      Format::Xrgb8888 => (true, true),
      Format::Abgr8888 => (false, false),
      Format::Xbgr8888 => (false, true),
      _ => return Err("unsupported shared memory pixel format".into()),
    };
    for pixel in source.as_chunks::<4>().0 {
      let alpha = if opaque { 255 } else { pixel[3] };
      if swizzle {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
      } else {
        rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], alpha]);
      }
    }
  }
  Ok(rgba)
}

/// A committed `wl_shm` buffer that the frontend has yet to upload
/// (docs/design/dmabuf.md, section 5): a view of the client's pool and
/// the buffer's lock, shared by every clone of the snapshot.
///
/// While locked, [`ShmContent::read`] gives the frontend the client's
/// bytes to upload straight from the pool, and [`ShmContent::rgba`]
/// converts them for CPU readers. [`ShmContent::release`] drops the lock
/// for every clone at once, so the client gets its buffer back at the
/// end of the turn; after it the content can no longer be read, because
/// the client may be drawing into it again.
#[derive(Clone)]
pub struct ShmContent(Arc<ShmShared>);

struct ShmShared {
  view: ShmView,
  width: u32,
  height: u32,
  lock: Mutex<Option<BufferLock>>,
}

impl ShmContent {
  pub(crate) fn new(
    view: ShmView,
    width: u32,
    height: u32,
    lock: BufferLock,
  ) -> Self {
    Self(Arc::new(ShmShared {
      view,
      width,
      height,
      lock: Mutex::new(Some(lock)),
    }))
  }

  /// The DRM fourcc of the format: ARGB8888, XRGB8888, ABGR8888 or
  /// XBGR8888, the formats `wl_shm` offers.
  pub fn fourcc(&self) -> u32 {
    self.0.view.fourcc()
  }

  /// Whether the format ignores the alpha byte (XRGB8888, XBGR8888).
  pub fn is_opaque(&self) -> bool {
    self.0.view.is_opaque()
  }

  /// Bytes from the start of one row to the start of the next, as the
  /// client chose it.
  pub fn stride(&self) -> u32 {
    self.0.view.stride() as u32
  }

  /// Whether the buffer is still locked, that is, not uploaded yet.
  pub fn is_locked(&self) -> bool {
    self
      .0
      .lock
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .is_some()
  }

  /// Calls `read` with the buffer's bytes in the client's pool, from its
  /// first pixel to the end of its last row's pixels, rows
  /// [`ShmContent::stride`] bytes apart. Fails once the buffer was
  /// released, or when the pool no longer holds it.
  pub fn read<R>(&self, read: impl FnOnce(&[u8]) -> R) -> Result<R, String> {
    let lock = self.0.lock.lock().unwrap_or_else(PoisonError::into_inner);
    if lock.is_none() {
      return Err("the buffer was released".into());
    }
    self.0.view.read(self.0.width, self.0.height, read)
  }

  /// The buffer as tightly packed RGBA rows, converted as
  /// [`SnapshotContent::Pixels`] describes, or `None` once it was
  /// released or when it cannot be read.
  pub fn rgba(&self) -> Option<Vec<u8>> {
    let (format, stride) = (self.0.view.format, self.0.view.stride);
    let (width, height) = (self.0.width, self.0.height);
    self
      .read(|bytes| to_rgba(bytes, width, height, stride, format))
      .ok()?
      .ok()
  }

  /// Gives the buffer back to its client: the lock drops, for every
  /// clone, and `wl_buffer.release` goes out at the end of the turn.
  pub fn release(&self) {
    let lock = self
      .0
      .lock
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .take();
    drop(lock);
  }
}

impl std::fmt::Debug for ShmContent {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ShmContent")
      .field("fourcc", &format_args!("{:#010x}", self.fourcc()))
      .field("stride", &self.stride())
      .field("locked", &self.is_locked())
      .finish()
  }
}

/// A shared mapping of a pool's fd, unmapped when dropped.
pub(crate) struct Mapping {
  pointer: *mut c_void,
  len: usize,
}

// SAFETY: the mapping is only read through shared slices while it lives,
// or written by the one caller that made it; the pointer is not tied to
// a thread.
unsafe impl Send for Mapping {}
// SAFETY: as above; concurrent reads of shared memory are fine.
unsafe impl Sync for Mapping {}

impl Mapping {
  /// Maps the first `len` bytes of `fd` read-only and shared.
  fn new(fd: &OwnedFd, len: usize) -> Result<Self, String> {
    // SAFETY: a fresh read-only shared mapping of an fd the pool owns;
    // the caller checked that the file is at least `len` bytes long.
    let pointer = unsafe {
      mmap(
        std::ptr::null_mut(),
        len,
        ProtFlags::READ,
        MapFlags::SHARED,
        fd,
        0,
      )
    }
    .map_err(|error| error.to_string())?;
    Ok(Self { pointer, len })
  }
}

impl Drop for Mapping {
  fn drop(&mut self) {
    // SAFETY: this is the exact live mapping returned by mmap, and no
    // slices into it survive this guard.
    let _ = unsafe { munmap(self.pointer, self.len) };
  }
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use super::*;

  /// A memfd of `bytes`, sealed against shrinking if asked.
  fn memfd(bytes: &[u8], seal: bool) -> OwnedFd {
    let fd = rustix::fs::memfd_create(
      "shingle-buffer-test",
      rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).unwrap();
    let fd = OwnedFd::from(file);
    if seal {
      rustix::fs::fcntl_add_seals(&fd, SealFlags::SHRINK).unwrap();
    }
    fd
  }

  fn view(pool: Arc<ShmPool>, offset: usize, stride: usize) -> ShmView {
    ShmView {
      pool,
      offset,
      stride,
      format: Format::Xrgb8888,
    }
  }

  #[test]
  fn only_a_shrink_seal_counts() {
    assert!(!is_sealed(None));
    assert!(!is_sealed(Some(SealFlags::empty())));
    assert!(!is_sealed(Some(SealFlags::GROW | SealFlags::WRITE)));
    assert!(is_sealed(Some(SealFlags::SHRINK)));
    assert!(is_sealed(Some(SealFlags::SHRINK | SealFlags::SEAL)));
  }

  #[test]
  fn a_sealed_pool_stays_mapped_and_is_mapped_again_when_it_grows() {
    let fd = memfd(&[7; 64], true);
    rustix::fs::ftruncate(&fd, 128).unwrap();
    let pool = Arc::new(ShmPool::new(fd, 64, 4096).unwrap());
    assert!(pool.is_sealed());
    assert_eq!(pool.mapped_len(), None);
    let shm = view(Arc::clone(&pool), 16, 16);
    assert_eq!(shm.read(4, 2, |bytes| bytes.len()), Ok(32));
    assert_eq!(pool.mapped_len(), Some(64));
    assert_eq!(shm.read(4, 2, |bytes| bytes[0]), Ok(7));
    pool.resize(128).unwrap();
    assert_eq!(shm.read(4, 2, |bytes| bytes.len()), Ok(32));
    assert_eq!(pool.mapped_len(), Some(128));
  }

  #[test]
  fn an_unsealed_pool_is_checked_and_mapped_per_read() {
    let fd = memfd(&[1; 64], false);
    let duplicate = fd.try_clone().unwrap();
    let pool = Arc::new(ShmPool::new(fd, 64, 4096).unwrap());
    assert!(!pool.is_sealed());
    let shm = view(Arc::clone(&pool), 0, 16);
    assert_eq!(shm.read(4, 4, |bytes| bytes.len()), Ok(64));
    assert_eq!(pool.mapped_len(), None);
    // A client that truncates its file gets an error, not a SIGBUS.
    rustix::fs::ftruncate(&duplicate, 8).unwrap();
    assert!(shm.read(4, 4, |_| ()).is_err());
  }

  #[test]
  fn a_read_covers_the_buffer_from_its_first_pixel_with_its_stride() {
    let bytes = (0..=255).collect::<Vec<u8>>();
    let pool = Arc::new(ShmPool::new(memfd(&bytes, true), 256, 4096).unwrap());
    // Two rows of three pixels, 20 bytes apart, starting at byte 8.
    let shm = view(pool, 8, 20);
    let read = shm.read(3, 2, <[u8]>::to_vec).unwrap();
    assert_eq!(read.len(), 20 + 12);
    assert_eq!(read[0], 8);
    assert_eq!(read[20], 28);
    // A buffer that no longer fits is refused.
    assert!(shm.read(3, 13, |_| ()).is_err());
  }

  #[test]
  fn conversion_swizzles_and_forces_opaque_alpha() {
    let bytes = [1, 2, 3, 4, 9, 9, 5, 6, 7, 8];
    assert_eq!(
      to_rgba(&bytes, 1, 2, 6, Format::Argb8888).unwrap(),
      [3, 2, 1, 4, 7, 6, 5, 8]
    );
    assert_eq!(
      to_rgba(&bytes, 1, 2, 6, Format::Xrgb8888).unwrap(),
      [3, 2, 1, 255, 7, 6, 5, 255]
    );
    assert_eq!(
      to_rgba(&bytes, 1, 2, 6, Format::Abgr8888).unwrap(),
      [1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!(
      to_rgba(&bytes, 1, 2, 6, Format::Xbgr8888).unwrap(),
      [1, 2, 3, 255, 5, 6, 7, 255]
    );
    assert!(to_rgba(&bytes, 1, 2, 6, Format::Rgb565).is_err());
  }

  #[test]
  fn the_fourcc_follows_drm_and_x_formats_are_opaque() {
    let pool = Arc::new(ShmPool::new(memfd(&[0; 4], false), 4, 16).unwrap());
    let shm = |format| ShmView {
      pool: Arc::clone(&pool),
      offset: 0,
      stride: 4,
      format,
    };
    let fourcc = |format| shm(format).fourcc();
    assert!(shm(Format::Xrgb8888).is_opaque());
    assert!(shm(Format::Xbgr8888).is_opaque());
    assert!(!shm(Format::Argb8888).is_opaque());
    assert!(!shm(Format::Abgr8888).is_opaque());
    assert_eq!(fourcc(Format::Argb8888), u32::from_le_bytes(*b"AR24"));
    assert_eq!(fourcc(Format::Xrgb8888), u32::from_le_bytes(*b"XR24"));
    assert_eq!(fourcc(Format::Abgr8888), u32::from_le_bytes(*b"AB24"));
    assert_eq!(fourcc(Format::Xbgr8888), u32::from_le_bytes(*b"XB24"));
  }
}
