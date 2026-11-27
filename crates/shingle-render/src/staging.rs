//! Staging buffers in system memory for shm uploads.
//!
//! `Queue::write_texture` copies into a staging buffer that wgpu-hal
//! allocates for every call with gpu-allocator's `CpuToGpu` location,
//! which prefers memory that is device-local as well as host-visible: on
//! a discrete GPU without resizable BAR, the 256 MiB PCIe BAR every
//! process on the GPU shares. gpu-allocator takes that memory in blocks
//! of 64 MiB and more. On this host's NVIDIA card (driver 590.48.01), in
//! nested Shingle, with the outer Shingle and the desktop holding much of
//! the BAR, the CPU wrote into such a block at about 13 MB/s: 270 ms for
//! a 1280x720 frame, whatever the GPU's clocks and PCIe link speed,
//! against 1.6 ms into system memory (docs/status.md#shm-upload-speed).
//!
//! A [`StagingPool`] instead allocates its buffers from a host-visible,
//! host-coherent memory type of a heap that is not device-local
//! ([`staging_memory_type`]), keeps them mapped and reuses them once the
//! GPU has finished the copies out of them, so that an upload is a
//! `memcpy` into system memory and a `copy_buffer_to_texture` on the GPU.

use std::{
  ptr::NonNull,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};

use ash::vk;
use wgpu::hal::api::Vulkan;

/// The environment variable that turns the pool off when it is `0`, so
/// that uploads go through `Queue::write_texture` as before, or makes it
/// stage in device-local memory when it is `device-local`, for
/// comparisons (tools/shm_bench).
pub const STAGING_VARIABLE: &str = "SHINGLE_SHM_STAGING";

/// Which memory a [`StagingPool`] puts its buffers in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StagingMemory {
  /// System memory where the GPU has a heap of it
  /// ([`staging_memory_type`]).
  #[default]
  System,
  /// Device-local, host-visible memory where there is some, as
  /// `Queue::write_texture` prefers: only for comparisons.
  DeviceLocal,
}

/// Whether uploads use a [`StagingPool`], and with which memory, from
/// [`STAGING_VARIABLE`] as `variable`, which reads the environment,
/// gives it: None for `0`, [`StagingMemory::DeviceLocal`] for
/// `device-local` and [`StagingMemory::System`] otherwise.
pub fn configured(
  variable: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<StagingMemory> {
  match variable(STAGING_VARIABLE) {
    Some(value) if value == "0" => None,
    Some(value) if value == "device-local" => Some(StagingMemory::DeviceLocal),
    _ => Some(StagingMemory::System),
  }
}

/// Staging buffers are at least this large, so that small damage does not
/// make a buffer each.
pub const MIN_BUFFER_SIZE: u64 = 1 << 20;

/// Free buffers kept for reuse, beyond which a finished buffer is
/// destroyed.
pub const MAX_FREE_BUFFERS: usize = 4;

/// Rows in a staging buffer start at a multiple of this
/// (`wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`).
const ROW_ALIGNMENT: u64 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;

/// One memory type as `vkGetPhysicalDeviceMemoryProperties` reports it:
/// its property flags and whether its heap is device-local.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryType {
  /// `VkMemoryType::propertyFlags`.
  pub flags: vk::MemoryPropertyFlags,
  /// Whether the type's heap has `VK_MEMORY_HEAP_DEVICE_LOCAL_BIT`.
  pub device_local_heap: bool,
}

/// The index of the memory type staging buffers use, among `types` and
/// those allowed by `allowed` (`VkMemoryRequirements::memoryTypeBits`):
/// for [`StagingMemory::System`], the first host-visible, host-coherent
/// type of a heap that is not device-local, or, where every heap is
/// device-local as on integrated GPUs, the first host-visible,
/// host-coherent type; for [`StagingMemory::DeviceLocal`], the first
/// such type that is device-local, or the first such type. None if no
/// type is host-visible and host-coherent.
pub fn staging_memory_type(
  types: &[MemoryType],
  allowed: u32,
  memory: StagingMemory,
) -> Option<u32> {
  let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE
    | vk::MemoryPropertyFlags::HOST_COHERENT;
  let usable = |index: usize, memory: &MemoryType| {
    index < 32 && allowed & (1 << index) != 0 && memory.flags.contains(wanted)
  };
  let in_video_memory = |memory: &MemoryType| {
    memory.device_local_heap
      || memory.flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
  };
  let preferred = types.iter().enumerate().find(|(index, candidate)| {
    usable(*index, candidate)
      && in_video_memory(candidate) == (memory == StagingMemory::DeviceLocal)
  });
  preferred
    .or_else(|| {
      types
        .iter()
        .enumerate()
        .find(|(index, memory)| usable(*index, memory))
    })
    .map(|(index, _)| index as u32)
}

/// Where each rectangle's rows go in a staging buffer: the byte offset of
/// its first row and the distance between rows, a multiple of
/// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`. Rectangles follow one another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StagedRect {
  /// The offset of the rectangle's first row.
  pub offset: u64,
  /// Bytes from one row to the next.
  pub row_pitch: u32,
}

/// The layout of `rects` (`[x, y, width, height]` in texels of 4 bytes) in
/// a staging buffer, and the bytes it needs. Empty rectangles take no
/// room.
pub fn layout(rects: &[[u32; 4]]) -> (Vec<StagedRect>, u64) {
  let mut offset = 0;
  let mut staged = Vec::with_capacity(rects.len());
  for &[_, _, width, height] in rects {
    let row_pitch =
      (u64::from(width) * 4).div_ceil(ROW_ALIGNMENT) * ROW_ALIGNMENT;
    staged.push(StagedRect {
      offset,
      row_pitch: row_pitch as u32,
    });
    if width > 0 && height > 0 {
      offset += row_pitch * u64::from(height);
    }
  }
  (staged, offset)
}

/// The size of a new staging buffer for `needed` bytes: the next power
/// of two, at least [`MIN_BUFFER_SIZE`].
pub fn buffer_size(needed: u64) -> u64 {
  needed.max(MIN_BUFFER_SIZE).next_power_of_two()
}

/// A staging buffer: a wgpu buffer on memory this pool allocated and
/// keeps mapped.
struct Buffer {
  buffer: wgpu::Buffer,
  mapped: Mapped,
  size: u64,
}

/// The host address of a buffer's memory, mapped for its whole life.
struct Mapped(NonNull<u8>);

// SAFETY: the pointer is only written through while the pool's mutex is
// held, and only into a buffer the GPU is not reading.
unsafe impl Send for Mapped {}

/// A buffer whose copies were submitted, with the flag
/// `on_submitted_work_done` sets once the GPU has finished them.
struct InFlight {
  buffer: Buffer,
  done: Arc<AtomicBool>,
}

/// Why a staging buffer could not be made.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum StagingError {
  /// The device is not a Vulkan device.
  #[error("the device is not Vulkan")]
  NotVulkan,
  /// No memory type can hold a mapped staging buffer.
  #[error("no host-visible memory type")]
  NoMemoryType,
  /// A Vulkan call failed.
  #[error("{0}")]
  Vulkan(vk::Result),
}

/// What a [`StagingPool`] has done, for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StagingStats {
  /// Buffers made so far.
  pub created: usize,
  /// Buffers waiting for the GPU or free for reuse.
  pub kept: usize,
  /// The memory type of the last buffer made.
  pub memory_type: Option<u32>,
  /// Whether that type's heap is device-local.
  pub device_local_heap: bool,
  /// Whether the device has a host-visible, host-coherent type whose
  /// heap is not device-local.
  pub system_memory: bool,
}

/// The staging buffers of one device, shared by the clones of a
/// [`crate::TextureUploader`].
pub struct StagingPool {
  device: wgpu::Device,
  raw: ash::Device,
  types: Vec<MemoryType>,
  memory: StagingMemory,
  free: Vec<Buffer>,
  in_flight: Vec<InFlight>,
  /// Buffers made so far.
  created: usize,
  /// The memory type of the last buffer made.
  memory_type: Option<u32>,
}

impl StagingPool {
  /// A pool for `device`, opened on `adapter`, with its buffers in
  /// `memory`, or None if it is not a Vulkan device.
  pub fn new(
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    memory: StagingMemory,
  ) -> Option<Self> {
    // SAFETY: the handles are only used to query memory properties and
    // to create, bind, map and free buffers and memory of this device.
    let hal_adapter = unsafe { adapter.as_hal::<Vulkan>() }?;
    let instance = hal_adapter.shared_instance().raw_instance();
    let physical = hal_adapter.raw_physical_device();
    // SAFETY: a valid physical device of this instance.
    let properties =
      unsafe { instance.get_physical_device_memory_properties(physical) };
    let types = properties.memory_types_as_slice().iter().map(|memory| {
      let heap = properties.memory_heaps[memory.heap_index as usize];
      MemoryType {
        flags: memory.property_flags,
        device_local_heap: heap
          .flags
          .contains(vk::MemoryHeapFlags::DEVICE_LOCAL),
      }
    });
    let types = types.collect();
    // SAFETY: as above.
    let raw = unsafe { device.as_hal::<Vulkan>() }?.raw_device().clone();
    Some(Self {
      device: device.clone(),
      raw,
      types,
      memory,
      free: Vec::new(),
      in_flight: Vec::new(),
      created: 0,
      memory_type: None,
    })
  }

  /// What the pool has done so far.
  pub fn stats(&self) -> StagingStats {
    let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE
      | vk::MemoryPropertyFlags::HOST_COHERENT;
    StagingStats {
      created: self.created,
      kept: self.free.len() + self.in_flight.len(),
      memory_type: self.memory_type,
      device_local_heap: self
        .memory_type
        .and_then(|index| self.types.get(index as usize))
        .is_some_and(|memory| memory.device_local_heap),
      system_memory: self.types.iter().any(|memory| {
        memory.flags.contains(wanted) && !memory.device_local_heap
      }),
    }
  }

  /// Moves buffers the GPU has finished with to the free list, keeping at
  /// most [`MAX_FREE_BUFFERS`].
  fn retire(&mut self) {
    // Runs the callbacks of finished submissions.
    let _ = self.device.poll(wgpu::PollType::Poll);
    let mut index = 0;
    while index < self.in_flight.len() {
      if self.in_flight[index].done.load(Ordering::Acquire) {
        let finished = self.in_flight.swap_remove(index);
        self.free.push(finished.buffer);
      } else {
        index += 1;
      }
    }
    if self.free.len() > MAX_FREE_BUFFERS {
      self
        .free
        .sort_by_key(|buffer| std::cmp::Reverse(buffer.size));
      self.free.truncate(MAX_FREE_BUFFERS);
    }
  }

  /// A free buffer of at least `needed` bytes, the smallest that fits, or
  /// a new one.
  fn take(&mut self, needed: u64) -> Result<Buffer, StagingError> {
    self.retire();
    let best = self
      .free
      .iter()
      .enumerate()
      .filter(|(_, buffer)| buffer.size >= needed)
      .min_by_key(|(_, buffer)| buffer.size)
      .map(|(index, _)| index);
    match best {
      Some(index) => Ok(self.free.swap_remove(index)),
      None => self.create(buffer_size(needed)),
    }
  }

  /// A new mapped buffer of `size` bytes in the staging memory type.
  fn create(&mut self, size: u64) -> Result<Buffer, StagingError> {
    let info = vk::BufferCreateInfo::default()
      .size(size)
      .usage(vk::BufferUsageFlags::TRANSFER_SRC)
      .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: a valid create info on this device.
    let buffer = unsafe { self.raw.create_buffer(&info, None) }
      .map_err(StagingError::Vulkan)?;
    // SAFETY: `buffer` was just created on this device; every path below
    // destroys it, and the memory, or hands both to wgpu.
    let requirements =
      unsafe { self.raw.get_buffer_memory_requirements(buffer) };
    let destroy = |memory: Option<vk::DeviceMemory>| unsafe {
      if let Some(memory) = memory {
        self.raw.free_memory(memory, None);
      }
      self.raw.destroy_buffer(buffer, None);
    };
    let Some(memory_type) = staging_memory_type(
      &self.types,
      requirements.memory_type_bits,
      self.memory,
    ) else {
      destroy(None);
      return Err(StagingError::NoMemoryType);
    };
    let allocate = vk::MemoryAllocateInfo::default()
      .allocation_size(requirements.size)
      .memory_type_index(memory_type);
    // SAFETY: a valid allocate info for a type the buffer allows.
    let memory = match unsafe { self.raw.allocate_memory(&allocate, None) } {
      Ok(memory) => memory,
      Err(error) => {
        destroy(None);
        return Err(StagingError::Vulkan(error));
      }
    };
    // SAFETY: fresh memory of a type the buffer allows, bound once at 0,
    // and mapped whole; the type is host-visible.
    let mapped = unsafe {
      self
        .raw
        .bind_buffer_memory(buffer, memory, 0)
        .and_then(|()| {
          self.raw.map_memory(
            memory,
            0,
            vk::WHOLE_SIZE,
            vk::MemoryMapFlags::empty(),
          )
        })
    };
    let mapped = match mapped.map(|pointer| NonNull::new(pointer.cast())) {
      Ok(Some(pointer)) => pointer,
      Ok(None) => {
        destroy(Some(memory));
        return Err(StagingError::Vulkan(vk::Result::ERROR_MEMORY_MAP_FAILED));
      }
      Err(error) => {
        destroy(Some(memory));
        return Err(StagingError::Vulkan(error));
      }
    };
    // SAFETY: wgpu-hal destroys the buffer and frees its memory, which
    // unmaps it, when the wgpu buffer is dropped; wgpu never maps it.
    let hal = unsafe {
      wgpu::hal::vulkan::Buffer::from_raw_managed(
        buffer,
        memory,
        0,
        requirements.size,
      )
    };
    // SAFETY: the buffer was created on this device with TRANSFER_SRC,
    // and its memory is bound.
    let buffer = unsafe {
      self.device.create_buffer_from_hal::<Vulkan>(
        hal,
        &wgpu::BufferDescriptor {
          label: Some("shingle shm staging"),
          size,
          usage: wgpu::BufferUsages::COPY_SRC,
          mapped_at_creation: false,
        },
      )
    };
    self.created += 1;
    self.memory_type = Some(memory_type);
    Ok(Buffer {
      buffer,
      mapped: Mapped(mapped),
      size,
    })
  }

  /// Writes `rects` of the rows in `bytes` (`stride` bytes apart, 4 bytes
  /// a texel) into `texture` through a staging buffer, and submits the
  /// copies on `queue`. The rectangles must lie inside the image that
  /// `bytes` holds, as [`crate::upload::upload_bytes`] checks; the copy
  /// out of `bytes` is done when this returns.
  pub(crate) fn write(
    &mut self,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    bytes: &[u8],
    stride: u32,
    rects: &[[u32; 4]],
  ) -> Result<(), StagingError> {
    let (staged, needed) = layout(rects);
    if needed == 0 {
      return Ok(());
    }
    let buffer = self.take(needed)?;
    for (&[x, y, width, height], place) in rects.iter().zip(&staged) {
      let row = width as usize * 4;
      for line in 0..height as usize {
        let from = (y as usize + line) * stride as usize + x as usize * 4;
        let to = place.offset as usize + line * place.row_pitch as usize;
        let source = &bytes[from..from + row];
        // SAFETY: `layout` keeps every row inside `needed` bytes, which
        // the buffer holds, and the GPU is not reading this buffer.
        unsafe {
          std::ptr::copy_nonoverlapping(
            source.as_ptr(),
            buffer.mapped.0.as_ptr().add(to),
            row,
          );
        }
      }
    }
    let mut encoder =
      self
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
          label: Some("shingle shm upload"),
        });
    for (&[x, y, width, height], place) in rects.iter().zip(&staged) {
      if width == 0 || height == 0 {
        continue;
      }
      encoder.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
          buffer: &buffer.buffer,
          layout: wgpu::TexelCopyBufferLayout {
            offset: place.offset,
            bytes_per_row: Some(place.row_pitch),
            rows_per_image: None,
          },
        },
        wgpu::TexelCopyTextureInfo {
          texture,
          mip_level: 0,
          origin: wgpu::Origin3d { x, y, z: 0 },
          aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
          width,
          height,
          depth_or_array_layers: 1,
        },
      );
    }
    queue.submit([encoder.finish()]);
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    queue.on_submitted_work_done(move || flag.store(true, Ordering::Release));
    self.in_flight.push(InFlight { buffer, done });
    Ok(())
  }
}

/// A [`StagingPool`] shared by clones of an uploader.
pub type SharedStaging = Arc<Mutex<StagingPool>>;

#[cfg(test)]
mod tests {
  use super::*;

  const HOST: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::from_raw(
    vk::MemoryPropertyFlags::HOST_VISIBLE.as_raw()
      | vk::MemoryPropertyFlags::HOST_COHERENT.as_raw(),
  );

  fn memory(
    flags: vk::MemoryPropertyFlags,
    device_local_heap: bool,
  ) -> MemoryType {
    MemoryType {
      flags,
      device_local_heap,
    }
  }

  /// This host's RTX 3060 (driver 590.48.01), as vulkaninfo lists it.
  fn nvidia() -> Vec<MemoryType> {
    let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    vec![
      memory(vk::MemoryPropertyFlags::empty(), false),
      memory(local, true),
      memory(local, true),
      memory(HOST, false),
      memory(HOST | vk::MemoryPropertyFlags::HOST_CACHED, false),
      memory(local | HOST, true),
    ]
  }

  #[test]
  fn a_discrete_gpu_stages_in_system_memory_not_its_bar() {
    let system =
      |allowed| staging_memory_type(&nvidia(), allowed, StagingMemory::System);
    assert_eq!(system(u32::MAX), Some(3));
    // Only what the buffer allows.
    assert_eq!(system(1 << 4 | 1 << 5), Some(4));
    // The BAR only if nothing else is allowed.
    assert_eq!(system(1 << 5), Some(5));
    assert_eq!(system(1 << 1 | 1 << 2), None);
    // For comparisons, the BAR on request.
    let local = |allowed| {
      staging_memory_type(&nvidia(), allowed, StagingMemory::DeviceLocal)
    };
    assert_eq!(local(u32::MAX), Some(5));
    assert_eq!(local(1 << 3), Some(3));
  }

  #[test]
  fn an_integrated_gpu_stages_in_its_device_local_memory() {
    let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let types = vec![memory(local, true), memory(local | HOST, true)];
    for memory in [StagingMemory::System, StagingMemory::DeviceLocal] {
      assert_eq!(staging_memory_type(&types, u32::MAX, memory), Some(1));
    }
  }

  #[test]
  fn rows_are_aligned_and_rectangles_follow_one_another() {
    let (staged, size) = layout(&[[0, 0, 1272, 682], [5, 5, 10, 3]]);
    // 1272 texels are 5088 bytes, padded to 5120.
    assert_eq!(
      staged,
      [
        StagedRect {
          offset: 0,
          row_pitch: 5120
        },
        StagedRect {
          offset: 5120 * 682,
          row_pitch: 256
        }
      ]
    );
    assert_eq!(size, 5120 * 682 + 256 * 3);
    let (staged, size) = layout(&[[0, 0, 0, 4], [0, 0, 64, 0]]);
    assert_eq!(staged.len(), 2);
    assert_eq!(size, 0);
  }

  #[test]
  fn the_pool_is_on_unless_turned_off() {
    let set = |value: &'static str| {
      move |name: &str| (name == STAGING_VARIABLE).then(|| value.into())
    };
    assert_eq!(configured(|_| None), Some(StagingMemory::System));
    assert_eq!(configured(set("1")), Some(StagingMemory::System));
    assert_eq!(
      configured(set("device-local")),
      Some(StagingMemory::DeviceLocal)
    );
    assert_eq!(configured(set("0")), None);
    assert_eq!(
      configured(|name| (name == "OTHER").then(|| "0".into())),
      Some(StagingMemory::System)
    );
  }

  #[test]
  fn buffers_grow_in_powers_of_two_from_a_mebibyte() {
    assert_eq!(buffer_size(1), MIN_BUFFER_SIZE);
    assert_eq!(buffer_size(MIN_BUFFER_SIZE), MIN_BUFFER_SIZE);
    assert_eq!(buffer_size(MIN_BUFFER_SIZE + 1), 2 * MIN_BUFFER_SIZE);
    assert_eq!(buffer_size(5120 * 682), 4 << 20);
  }
}
