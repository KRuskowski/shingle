//! The frontend's side of `zwp_linux_dmabuf_v1` (docs/design/dmabuf.md,
//! section 2): the importer the compositor calls on `create`, the texture
//! cache the scene draws from, and the buffer locks of frames the GPU has
//! not finished yet.
//!
//! Textures are cached by buffer serial, not by surface, because clients
//! cycle through two to four buffers: each is imported once, when the
//! client creates it, and every later commit of it only picks the cached
//! texture. An entry goes when the compositor no longer knows the buffer,
//! that is, when its `wl_buffer` is destroyed and no surface shows it.
//! The fds stay with the buffer, so a texture the cache lost, after a
//! filter change or a GPU loss, is imported again on the next draw.

use std::{
  cell::RefCell,
  collections::HashMap,
  ffi::OsStr,
  os::fd::AsFd,
  rc::Rc,
  sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
  },
};

use shingle::{
  BufferLock, DmabufBuffer, DmabufFeedback, DmabufFormat, DmabufImporter,
  fourcc_name,
};
use shingle_render::{
  DmabufCaps, DmabufDescriptor, DmabufUnavailable, DmabufUse, GpuTexture,
  TextureFilter, TextureImporter,
};

/// How a frontend imports and keeps DMA-BUF textures. The production
/// implementation is [`TextureImporter`]; tests use a fake.
pub(crate) trait Import {
  /// Imports `buffer` for sampling with `filter`.
  fn import(
    &self,
    buffer: &DmabufBuffer,
    filter: TextureFilter,
  ) -> Result<GpuTexture, String>;

  /// Calls `done` once the GPU has finished everything submitted so far.
  fn on_submitted_work_done(&self, done: Box<dyn FnOnce() + Send>);

  /// Runs completed-work callbacks without waiting.
  fn poll(&self);
}

impl Import for TextureImporter {
  fn import(
    &self,
    buffer: &DmabufBuffer,
    filter: TextureFilter,
  ) -> Result<GpuTexture, String> {
    let [plane] = buffer.planes.as_slice() else {
      return Err(format!("{} planes, not one", buffer.planes.len()));
    };
    let descriptor = DmabufDescriptor {
      width: buffer.width,
      height: buffer.height,
      fourcc: buffer.format,
      modifier: buffer.modifier,
      fd: plane.fd.as_fd(),
      offset: plane.offset,
      stride: plane.stride,
    };
    let imported =
      TextureImporter::import(self, &descriptor, DmabufUse::Sample(filter))
        .map_err(|error| error.to_string())?;
    imported
      .gpu_texture()
      .cloned()
      .ok_or_else(|| "the import has no sampled texture".to_string())
  }

  fn on_submitted_work_done(&self, done: Box<dyn FnOnce() + Send>) {
    self.queue().on_submitted_work_done(done);
  }

  fn poll(&self) {
    let _ = self.device().poll(wgpu::PollType::Poll);
  }
}

/// One cached import.
struct Entry {
  /// Gone once the compositor dropped the buffer.
  buffer: Weak<DmabufBuffer>,
  texture: GpuTexture,
}

/// The locks of one submitted frame, until the GPU has finished it.
struct InFlight {
  done: Arc<AtomicBool>,
  _locks: Vec<BufferLock>,
}

/// The cache of imported client buffers, shared by the compositor's
/// importer and the scene.
pub(crate) struct DmabufTextures {
  importer: Box<dyn Import>,
  filter: TextureFilter,
  entries: HashMap<u64, Entry>,
  /// Buffers whose import failed, logged once each.
  failed: HashMap<u64, Weak<DmabufBuffer>>,
  in_flight: Vec<InFlight>,
}

impl DmabufTextures {
  pub(crate) fn new(importer: Box<dyn Import>) -> Self {
    Self {
      importer,
      filter: TextureFilter::Linear,
      entries: HashMap::new(),
      failed: HashMap::new(),
      in_flight: Vec::new(),
    }
  }

  /// Imports `buffer` and caches it, replacing any entry of its serial.
  pub(crate) fn import(
    &mut self,
    buffer: &Arc<DmabufBuffer>,
  ) -> Result<GpuTexture, String> {
    let texture = self.importer.import(buffer, self.filter)?;
    shingle::trace::buffer(format_args!(
      "dmabuf import dmabuf={} {}x{} {} modifier={:#018x}",
      buffer.serial,
      buffer.width,
      buffer.height,
      fourcc_name(buffer.format),
      buffer.modifier
    ));
    self.entries.insert(
      buffer.serial,
      Entry {
        buffer: Arc::downgrade(buffer),
        texture: texture.clone(),
      },
    );
    Ok(texture)
  }

  /// The texture of `buffer`: cached, or imported again from its fds.
  /// `None` when that fails; the failure is logged once per buffer and
  /// the surface draws nothing until its next buffer.
  pub(crate) fn texture(
    &mut self,
    buffer: &Arc<DmabufBuffer>,
  ) -> Option<GpuTexture> {
    if let Some(entry) = self.entries.get(&buffer.serial) {
      return Some(entry.texture.clone());
    }
    if self.failed.contains_key(&buffer.serial) {
      return None;
    }
    match self.import(buffer) {
      Ok(texture) => Some(texture),
      Err(error) => {
        eprintln!(
          "shingle: importing DMA-BUF {} again failed: {error}",
          buffer.serial
        );
        self.failed.insert(buffer.serial, Arc::downgrade(buffer));
        None
      }
    }
  }

  /// Uses `filter` for later imports. Cached textures sample with the old
  /// one, so they are dropped and imported again when it changes.
  pub(crate) fn set_filter(&mut self, filter: TextureFilter) {
    if self.filter != filter {
      self.filter = filter;
      self.entries.clear();
    }
  }

  /// Drops the textures of buffers the compositor no longer has.
  pub(crate) fn prune(&mut self) {
    self
      .entries
      .retain(|_, entry| entry.buffer.strong_count() > 0);
    self.failed.retain(|_, buffer| buffer.strong_count() > 0);
  }

  /// Keeps `locks`, the buffers a frame just submitted samples, until
  /// the GPU has finished it.
  pub(crate) fn frame_submitted(&mut self, locks: Vec<BufferLock>) {
    if locks.is_empty() {
      return;
    }
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    self.importer.on_submitted_work_done(Box::new(move || {
      flag.store(true, Ordering::Release)
    }));
    self.in_flight.push(InFlight {
      done,
      _locks: locks,
    });
  }

  /// Drops the locks of frames the GPU has finished, which releases
  /// their buffers unless something else holds them.
  pub(crate) fn retire(&mut self) {
    if self.in_flight.is_empty() {
      return;
    }
    self.importer.poll();
    self
      .in_flight
      .retain(|frame| !frame.done.load(Ordering::Acquire));
  }

  /// How many frames still hold locks.
  #[cfg(test)]
  pub(crate) fn frames_in_flight(&self) -> usize {
    self.in_flight.len()
  }

  /// How many textures are cached.
  #[cfg(test)]
  pub(crate) fn cached(&self) -> usize {
    self.entries.len()
  }

  /// Forgets every texture and in-flight lock, after a GPU loss: a lost
  /// device runs nothing more.
  pub(crate) fn clear(&mut self) {
    self.entries.clear();
    self.failed.clear();
    self.in_flight.clear();
  }
}

/// The compositor's importer: imports into the shared cache.
pub(crate) struct CacheImporter(pub(crate) Rc<RefCell<DmabufTextures>>);

impl DmabufImporter for CacheImporter {
  fn import(&mut self, buffer: &Arc<DmabufBuffer>) -> Result<(), String> {
    self.0.borrow_mut().import(buffer).map(|_| ())
  }
}

/// Decides whether to offer linux-dmabuf: not with `SHINGLE_DMABUF=0`
/// (`variable`), nor when the device cannot import or has no render node
/// to advertise. Returns the feedback, or the log line saying why not.
pub(crate) fn feedback(
  variable: Option<&OsStr>,
  caps: Result<&DmabufCaps, &DmabufUnavailable>,
) -> Result<DmabufFeedback, String> {
  if variable.is_some_and(|value| value == "0") {
    return Err("linux-dmabuf is off: SHINGLE_DMABUF=0 forces wl_shm".into());
  }
  let caps = caps.map_err(|reason| format!("no linux-dmabuf: {reason}"))?;
  let Some(main_device) = caps.render_node_dev() else {
    return Err(
      "no linux-dmabuf: the adapter has no render node to advertise".into(),
    );
  };
  let formats = caps
    .formats
    .iter()
    .flat_map(|format| {
      format.sample.iter().map(|modifier| DmabufFormat {
        format: format.fourcc,
        modifier: modifier.modifier,
        max_width: modifier.max_width,
        max_height: modifier.max_height,
      })
    })
    .collect::<Vec<_>>();
  if formats.is_empty() {
    return Err("no linux-dmabuf: no format has a usable modifier".into());
  }
  Ok(DmabufFeedback {
    main_device,
    formats,
  })
}

/// Whether to try `linux-drm-syncobj-v1`: not with `SHINGLE_SYNCOBJ=0`
/// (`variable`). Returns the log line saying why not.
pub(crate) fn syncobj_allowed(variable: Option<&OsStr>) -> Result<(), String> {
  if variable.is_some_and(|value| value == "0") {
    return Err(
      "linux-drm-syncobj is off: SHINGLE_SYNCOBJ=0 forces implicit sync".into(),
    );
  }
  Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
