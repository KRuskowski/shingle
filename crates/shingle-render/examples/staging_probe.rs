//! Times CPU writes into each host-visible Vulkan memory type of the
//! first adapter, as a JSON line per type: index, heap, flags, whether
//! the heap is device-local, the heap's size, MiB/s for one 4 MiB copy
//! and for 5088-byte rows (a 1272-pixel-wide shm buffer), and whether
//! [`staging_memory_type`] picks the type for shm staging. A 4 MiB
//! allocation fits even a crowded PCIe BAR, so this shows how fast each
//! type is written while its heap has room; tools/shm_bench shows what
//! happens to `Queue::write_texture` when it has not
//! (docs/status.md#shm-upload-speed).
//!
//! It opens a GPU device, so it runs only inside tools/gpu_test/run.py's
//! namespace, which sets `SHINGLE_GPU_TEST_NAMESPACE=1`:
//!
//! ```text
//! tools/gpu_test/run.py -- target/debug/examples/staging_probe
//! ```

use std::time::Instant;

use ash::vk;
use shingle_render::{
  GpuContext, GpuOptions,
  staging::{MemoryType, StagingMemory, staging_memory_type},
};
use wgpu::hal::api::Vulkan;

/// The bytes copied per measurement.
const SIZE: usize = 4 << 20;

/// The row of the row-by-row measurement: 1272 XRGB8888 pixels.
const ROW: usize = 1272 * 4;

/// Copies per measurement; the fastest counts.
const REPEATS: usize = 8;

fn main() {
  if std::env::var_os("SHINGLE_GPU_TEST_NAMESPACE").is_none_or(|v| v != "1") {
    eprintln!("staging_probe: run it inside tools/gpu_test/run.py");
    std::process::exit(2);
  }
  if let Err(error) = run() {
    eprintln!("staging_probe: {error}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), String> {
  let gpu = pollster::block_on(GpuContext::new(
    GpuContext::create_instance(),
    GpuOptions::default(),
  ))
  .map_err(|error| error.to_string())?;
  let info = gpu.adapter().get_info();
  println!(
    "{{\"adapter\": {:?}, \"driver\": {:?}}}",
    info.name, info.driver_info
  );
  // SAFETY: only queries, and buffers and memory this function frees.
  let adapter = unsafe { gpu.adapter().as_hal::<Vulkan>() }
    .ok_or("not a Vulkan adapter")?;
  let instance = adapter.shared_instance().raw_instance();
  // SAFETY: a valid physical device of this instance.
  let properties = unsafe {
    instance
      .get_physical_device_memory_properties(adapter.raw_physical_device())
  };
  let device = unsafe { gpu.device().as_hal::<Vulkan>() }
    .ok_or("not a Vulkan device")?
    .raw_device()
    .clone();
  let types = properties
    .memory_types_as_slice()
    .iter()
    .map(|memory| MemoryType {
      flags: memory.property_flags,
      device_local_heap: properties.memory_heaps[memory.heap_index as usize]
        .flags
        .contains(vk::MemoryHeapFlags::DEVICE_LOCAL),
    })
    .collect::<Vec<_>>();
  let chosen = staging_memory_type(&types, u32::MAX, StagingMemory::System);
  let source = vec![0x5a_u8; SIZE];
  for (index, memory) in properties.memory_types_as_slice().iter().enumerate() {
    if !memory
      .property_flags
      .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
    {
      continue;
    }
    let heap = properties.memory_heaps[memory.heap_index as usize];
    let (whole, rows) = match measure(&device, index as u32, &source) {
      Ok(speeds) => speeds,
      Err(error) => {
        println!("{{\"type\": {index}, \"error\": {:?}}}", error.to_string());
        continue;
      }
    };
    println!(
      "{{\"type\": {index}, \"heap\": {}, \"flags\": \"{:?}\", \
       \"device_local_heap\": {}, \"heap_mib\": {}, \
       \"copy_mib_per_s\": {whole:.0}, \"rows_mib_per_s\": {rows:.0}, \
       \"staging\": {}}}",
      memory.heap_index,
      memory.property_flags,
      types[index].device_local_heap,
      heap.size >> 20,
      chosen == Some(index as u32),
    );
  }
  Ok(())
}

/// MiB/s of the fastest of [`REPEATS`] copies of `source` into memory of
/// type `memory_type`, whole and in [`ROW`]-byte rows.
fn measure(
  device: &ash::Device,
  memory_type: u32,
  source: &[u8],
) -> Result<(f64, f64), vk::Result> {
  let allocate = vk::MemoryAllocateInfo::default()
    .allocation_size(SIZE as u64)
    .memory_type_index(memory_type);
  // SAFETY: a valid allocation of a host-visible type, mapped whole and
  // freed before returning.
  let memory = unsafe { device.allocate_memory(&allocate, None) }?;
  let mapped = unsafe {
    device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
  };
  let result = mapped.map(|pointer| {
    let target = pointer.cast::<u8>();
    let whole = fastest(|| {
      // SAFETY: the mapping holds SIZE bytes.
      unsafe { std::ptr::copy_nonoverlapping(source.as_ptr(), target, SIZE) }
    });
    let rows = fastest(|| {
      for start in (0..SIZE - ROW).step_by(ROW) {
        // SAFETY: every row ends inside the SIZE bytes.
        unsafe {
          std::ptr::copy_nonoverlapping(
            source.as_ptr().add(start),
            target.add(start),
            ROW,
          )
        }
      }
    });
    (whole, rows)
  });
  // SAFETY: nothing uses the mapping any more.
  unsafe { device.free_memory(memory, None) };
  result
}

/// MiB/s of the fastest of [`REPEATS`] runs of `copy`, which writes
/// [`SIZE`] bytes.
fn fastest(mut copy: impl FnMut()) -> f64 {
  let seconds = (0..REPEATS)
    .map(|_| {
      let started = Instant::now();
      copy();
      started.elapsed().as_secs_f64()
    })
    .fold(f64::INFINITY, f64::min);
  SIZE as f64 / f64::from(1 << 20) / seconds
}
