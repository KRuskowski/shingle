//! DMA-BUF import on a real device: buffers are imported with
//! `GpuContext::import_dmabuf`, drawn into an `OffscreenTarget` twice (so
//! the second frame acquires an image the first released) and read back.
//!
//! The buffers come from three places:
//!
//! - a sealed memfd turned into a DMA-BUF by `/dev/udmabuf`, written by
//!   the CPU;
//! - a linear image exported by a second Vulkan device (the "client"),
//!   written by the CPU through a host-visible mapping;
//! - an image of the client device in a render modifier, non-linear where
//!   the driver has one, exported, imported back into the client with
//!   `DmabufUse::Render` and drawn into by the client's GPU, as a GPU
//!   client renders its buffers.
//!
//! Each test skips, saying why, without a Vulkan adapter that can import
//! DMA-BUFs, without `/dev/udmabuf`, or when the driver offers no modifier
//! the test needs. On the development host they run on NVIDIA in
//! tools/gpu_test/run.py's namespace, and on lavapipe with
//! `VK_ICD_FILENAMES`; docs/status.md records both. NVIDIA's driver does
//! not show the contents of udmabuf memory (it reads other data), so the
//! udmabuf tests skip on NVIDIA and the exported buffers stand in for
//! them. The ignored tests are experiments run by hand with `--ignored`:
//! whether a driver needs the foreign queue-family barriers, and whether
//! it reads udmabuf memory.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use ash::vk;
use rustix::fs::{MemfdFlags, SealFlags};
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, opcode};
use shingle_render::dmabuf::{
  DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888,
};
use shingle_render::{
  DmabufDescriptor, DmabufUse, ForeignAccess, GpuContext, GpuOptions,
  OffscreenTarget, RenderError, RenderRect, RenderState, RenderTile,
  TextureFilter, draw_with_access,
};
use wgpu::hal::api::Vulkan;

const SIZE: u32 = 64;
// 256 bytes, the pitch alignment NVIDIA and most drivers accept for linear
// buffers.
const STRIDE: u32 = SIZE * 4;
// Each pass through the shader indexes the gamma LUT with an 8-bit linear
// value; the rendered test goes through it twice.
const TOLERANCE: u8 = 4;
const NVIDIA: u32 = 0x10de;

/// One colour per quadrant, RGB, bright enough that the 8-bit gamma LUT
/// keeps them within the tolerance.
const QUADRANTS: [[u8; 3]; 4] = [
  [220, 100, 120],
  [100, 230, 140],
  [120, 150, 250],
  [255, 255, 110],
];

fn quadrant(x: u32, y: u32) -> [u8; 3] {
  let index = usize::from(x >= SIZE / 2) + 2 * usize::from(y >= SIZE / 2);
  QUADRANTS[index]
}

/// `struct udmabuf_create` from linux/udmabuf.h.
#[repr(C)]
struct UdmabufCreate {
  memfd: u32,
  flags: u32,
  offset: u64,
  size: u64,
}

const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// `UDMABUF_CREATE`, whose return value is the new DMA-BUF fd.
struct CreateUdmabuf(UdmabufCreate);

// SAFETY: the opcode is `_IOW('u', 0x42, struct udmabuf_create)`, the
// pointer is to that struct, and the kernel only reads it.
unsafe impl Ioctl for CreateUdmabuf {
  type Output = IoctlOutput;
  const IS_MUTATING: bool = false;

  fn opcode(&self) -> Opcode {
    opcode::write::<UdmabufCreate>(b'u', 0x42)
  }

  fn as_ptr(&mut self) -> *mut std::ffi::c_void {
    (&mut self.0 as *mut UdmabufCreate).cast()
  }

  unsafe fn output_from_ptr(
    out: IoctlOutput,
    _: *mut std::ffi::c_void,
  ) -> rustix::io::Result<IoctlOutput> {
    Ok(out)
  }
}

/// A DMA-BUF holding `bytes`, or why none could be made.
fn udmabuf(bytes: &[u8]) -> Result<OwnedFd, String> {
  let device = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open("/dev/udmabuf")
    .map_err(|error| format!("/dev/udmabuf: {error}"))?;
  let memfd = rustix::fs::memfd_create(
    "shingle-dmabuf-test",
    MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
  )
  .map_err(|error| format!("memfd_create: {error}"))?;
  let file = std::fs::File::from(memfd);
  std::io::Write::write_all(&mut &file, bytes)
    .map_err(|error| format!("write: {error}"))?;
  // udmabuf needs a page-aligned size and a memfd that cannot shrink.
  rustix::fs::fcntl_add_seals(&file, SealFlags::SHRINK)
    .map_err(|error| format!("seal: {error}"))?;
  let create = CreateUdmabuf(UdmabufCreate {
    memfd: u32::try_from(std::os::fd::AsRawFd::as_raw_fd(&file)).expect("fd"),
    flags: UDMABUF_FLAGS_CLOEXEC,
    offset: 0,
    size: bytes.len() as u64,
  });
  // SAFETY: see `CreateUdmabuf`.
  let fd = unsafe { rustix::ioctl::ioctl(&device, create) }
    .map_err(|error| format!("UDMABUF_CREATE: {error}"))?;
  // SAFETY: the ioctl returned a new fd that nothing else owns.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The test pattern in `fourcc`'s byte order, alpha byte `alpha`.
fn pattern(fourcc: u32, alpha: u8) -> Vec<u8> {
  let mut bytes = Vec::with_capacity((STRIDE * SIZE) as usize);
  for y in 0..SIZE {
    for x in 0..SIZE {
      let [r, g, b] = quadrant(x, y);
      if fourcc == DRM_FORMAT_XRGB8888 {
        bytes.extend([b, g, r, alpha]);
      } else {
        bytes.extend([r, g, b, alpha]);
      }
    }
  }
  bytes
}

fn gpu() -> Option<GpuContext> {
  let options = GpuOptions {
    dmabuf: true,
    ..GpuOptions::default()
  };
  let gpu = match pollster::block_on(GpuContext::new(
    GpuContext::create_instance(),
    options,
  )) {
    Ok(gpu) => gpu,
    Err(RenderError::RequestAdapter(error)) => {
      eprintln!("skipping, no GPU adapter: {error}");
      return None;
    }
    Err(error) => panic!("GPU context failed: {error}"),
  };
  eprintln!(
    "adapter {}: {}",
    gpu.adapter().get_info().name,
    gpu.dmabuf_summary()
  );
  if let Err(reason) = gpu.dmabuf() {
    eprintln!("skipping, {reason}");
    return None;
  }
  Some(gpu)
}

/// Where a test buffer lives in its DMA-BUF.
struct Buffer {
  fd: OwnedFd,
  fourcc: u32,
  modifier: u64,
  offset: u32,
  stride: u32,
}

/// Imports `buffer`, draws it over a white clear twice and checks every
/// pixel against the pattern, with alpha 255.
fn check_pixels(gpu: &GpuContext, buffer: &Buffer) {
  let imported = gpu
    .import_dmabuf(
      &DmabufDescriptor {
        width: SIZE,
        height: SIZE,
        fourcc: buffer.fourcc,
        modifier: buffer.modifier,
        fd: buffer.fd.as_fd(),
        offset: buffer.offset,
        stride: buffer.stride,
      },
      DmabufUse::Sample(TextureFilter::Nearest),
    )
    .expect("import");
  let texture = imported.gpu_texture().expect("sampled texture");
  let target = OffscreenTarget::new(gpu, SIZE, SIZE).expect("target");
  let tiles = [RenderTile::textured(full(), texture)];
  let state = RenderState::default();
  for frame in 0..2 {
    // The white clear shows through wherever alpha is not 1.
    target.render(gpu, &state, &tiles, [1.0; 4]);
    let rgba = target.read_rgba(gpu).expect("readback");
    let mut wrong = Vec::new();
    for y in 0..SIZE {
      for x in 0..SIZE {
        let start = ((y * SIZE + x) * 4) as usize;
        let actual = &rgba[start..start + 4];
        let [r, g, b] = quadrant(x, y);
        let close = actual
          .iter()
          .zip([r, g, b, 255])
          .all(|(&actual, expected)| actual.abs_diff(expected) <= TOLERANCE);
        if !close && wrong.len() < 4 {
          wrong.push(format!("({x}, {y}) = {actual:?}, want {r} {g} {b}"));
        }
      }
    }
    assert!(wrong.is_empty(), "frame {frame}: {}", wrong.join("; "));
  }
}

fn full() -> RenderRect {
  RenderRect {
    x: 0,
    y: 0,
    width: SIZE,
    height: SIZE,
  }
}

/// The udmabuf buffer of the pattern, or why the test skips.
fn udmabuf_buffer(
  gpu: &GpuContext,
  fourcc: u32,
  alpha: u8,
) -> Result<Buffer, String> {
  if gpu.adapter().get_info().vendor == NVIDIA {
    return Err("NVIDIA does not read udmabuf memory".into());
  }
  let caps = gpu.dmabuf().expect("caps");
  if caps.sampling(fourcc, DRM_FORMAT_MOD_LINEAR).is_none() {
    return Err(format!("no linear modifier for {fourcc:#010x}"));
  }
  Ok(Buffer {
    fd: udmabuf(&pattern(fourcc, alpha))?,
    fourcc,
    modifier: DRM_FORMAT_MOD_LINEAR,
    offset: 0,
    stride: STRIDE,
  })
}

/// A B8G8R8A8 image of a client device, created with one explicit
/// modifier in exportable memory and destroyed on drop. The DMA-BUF fd
/// keeps the memory alive for the importer.
struct ExportedImage {
  device: ash::Device,
  image: vk::Image,
  memory: vk::DeviceMemory,
  offset: u32,
  stride: u32,
}

impl Drop for ExportedImage {
  fn drop(&mut self) {
    // SAFETY: the client device is idle (every test waits for it), and
    // the handles were created on it.
    unsafe {
      self.device.destroy_image(self.image, None);
      self.device.free_memory(self.memory, None);
    }
  }
}

/// Creates an exportable `modifier` image on `client`; with `bytes`, in
/// host-visible memory written with them row by row. Returns the image
/// and its DMA-BUF.
fn export_image(
  client: &GpuContext,
  modifier: u64,
  usage: vk::ImageUsageFlags,
  bytes: Option<&[u8]>,
) -> Result<(ExportedImage, OwnedFd), String> {
  // SAFETY: the guard lives for this function; every handle made here is
  // owned by the returned image, which destroys it.
  let device = unsafe { client.device().as_hal::<Vulkan>() }
    .ok_or("not a Vulkan device")?;
  let instance = device.shared_instance().raw_instance();
  let raw = device.raw_device().clone();
  let modifiers = [modifier];
  let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
    .drm_format_modifiers(&modifiers);
  let mut external = vk::ExternalMemoryImageCreateInfo::default()
    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
  let info = vk::ImageCreateInfo::default()
    .image_type(vk::ImageType::TYPE_2D)
    .format(vk::Format::B8G8R8A8_SRGB)
    .extent(vk::Extent3D {
      width: SIZE,
      height: SIZE,
      depth: 1,
    })
    .mip_levels(1)
    .array_layers(1)
    .samples(vk::SampleCountFlags::TYPE_1)
    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
    .usage(usage)
    .sharing_mode(vk::SharingMode::EXCLUSIVE)
    .initial_layout(vk::ImageLayout::UNDEFINED)
    .push_next(&mut external)
    .push_next(&mut list);
  // SAFETY: a valid create info for a device with the modifier extension.
  let image = unsafe { raw.create_image(&info, None) }
    .map_err(|error| format!("vkCreateImage: {error}"))?;
  let mut exported = ExportedImage {
    device: raw.clone(),
    image,
    memory: vk::DeviceMemory::null(),
    offset: 0,
    stride: 0,
  };
  // SAFETY: queries on the device and the image just created.
  let requirements = unsafe { raw.get_image_memory_requirements(image) };
  let types = unsafe {
    instance.get_physical_device_memory_properties(device.raw_physical_device())
  };
  let wanted = if bytes.is_some() {
    vk::MemoryPropertyFlags::HOST_VISIBLE
      | vk::MemoryPropertyFlags::HOST_COHERENT
  } else {
    vk::MemoryPropertyFlags::DEVICE_LOCAL
  };
  let index = types
    .memory_types_as_slice()
    .iter()
    .enumerate()
    .position(|(index, memory_type)| {
      requirements.memory_type_bits & (1 << index) != 0
        && memory_type.property_flags.contains(wanted)
    })
    .ok_or_else(|| format!("no {wanted:?} memory for the image"))?;
  let mut export = vk::ExportMemoryAllocateInfo::default()
    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
  let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
  let allocate = vk::MemoryAllocateInfo::default()
    .allocation_size(requirements.size)
    .memory_type_index(index as u32)
    .push_next(&mut export)
    .push_next(&mut dedicated);
  // SAFETY: a valid allocation and bind for the image.
  exported.memory = unsafe { raw.allocate_memory(&allocate, None) }
    .map_err(|error| format!("vkAllocateMemory: {error}"))?;
  unsafe { raw.bind_image_memory(image, exported.memory, 0) }
    .map_err(|error| format!("vkBindImageMemory: {error}"))?;
  let layout = unsafe {
    raw.get_image_subresource_layout(
      image,
      vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        mip_level: 0,
        array_layer: 0,
      },
    )
  };
  exported.offset = u32::try_from(layout.offset).map_err(|_| "offset")?;
  exported.stride = u32::try_from(layout.row_pitch).map_err(|_| "pitch")?;
  if let Some(bytes) = bytes {
    // SAFETY: host-visible memory of at least the image's size, written
    // only within the rows the layout describes, and unmapped after.
    unsafe {
      let mapped = raw
        .map_memory(
          exported.memory,
          0,
          vk::WHOLE_SIZE,
          vk::MemoryMapFlags::empty(),
        )
        .map_err(|error| format!("vkMapMemory: {error}"))?
        .cast::<u8>();
      for (row, source) in bytes.chunks_exact(STRIDE as usize).enumerate() {
        let start = layout.offset as usize + row * layout.row_pitch as usize;
        std::ptr::copy_nonoverlapping(
          source.as_ptr(),
          mapped.add(start),
          source.len(),
        );
      }
      raw.unmap_memory(exported.memory);
    }
  }
  let fd_device = ash::khr::external_memory_fd::Device::new(instance, &raw);
  // SAFETY: the memory was allocated exportable as a DMA-BUF.
  let fd = unsafe {
    fd_device.get_memory_fd(
      &vk::MemoryGetFdInfoKHR::default()
        .memory(exported.memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
    )
  }
  .map_err(|error| format!("vkGetMemoryFdKHR: {error}"))?;
  // SAFETY: a new fd that nothing else owns.
  Ok((exported, unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// A linear XRGB8888 pattern written by the CPU into an image exported by
/// `client`.
fn exported_linear(
  client: &GpuContext,
) -> Result<(ExportedImage, Buffer), String> {
  let (image, fd) = export_image(
    client,
    DRM_FORMAT_MOD_LINEAR,
    vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
    Some(&pattern(DRM_FORMAT_XRGB8888, 0)),
  )?;
  let buffer = Buffer {
    fd,
    fourcc: DRM_FORMAT_XRGB8888,
    modifier: DRM_FORMAT_MOD_LINEAR,
    offset: image.offset,
    stride: image.stride,
  };
  Ok((image, buffer))
}

/// sRGB-encoded `value` as a linear colour component.
fn linear(value: u8) -> f32 {
  let value = f32::from(value) / 255.0;
  if value <= 0.04045 {
    value / 12.92
  } else {
    ((value + 0.055) / 1.055).powf(2.4)
  }
}

/// The pattern drawn by `client`'s GPU into an exported XRGB8888 image in
/// a modifier `client` can render to and `server` can sample, non-linear
/// where there is one.
fn rendered(
  client: &GpuContext,
  server: &GpuContext,
) -> Result<(ExportedImage, Buffer), String> {
  let caps = client.dmabuf().map_err(|reason| reason.to_string())?;
  let server_caps = server.dmabuf().map_err(|reason| reason.to_string())?;
  let usable = caps
    .format(DRM_FORMAT_XRGB8888)
    .map(|format| {
      format
        .render
        .iter()
        .map(|caps| caps.modifier)
        .filter(|&modifier| {
          server_caps
            .sampling(DRM_FORMAT_XRGB8888, modifier)
            .is_some()
        })
        .collect::<Vec<_>>()
    })
    .unwrap_or_default();
  // A tiled modifier if there is one (NVIDIA); lavapipe has only linear.
  let modifier = usable
    .iter()
    .copied()
    .find(|&modifier| modifier != DRM_FORMAT_MOD_LINEAR)
    .or_else(|| usable.first().copied())
    .ok_or("no XRGB8888 render modifier")?;
  eprintln!(
    "rendering into {}",
    shingle_render::dmabuf::modifier_name(modifier)
  );
  let (image, fd) = export_image(
    client,
    modifier,
    vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT,
    None,
  )?;
  let descriptor = DmabufDescriptor {
    width: SIZE,
    height: SIZE,
    fourcc: DRM_FORMAT_XRGB8888,
    modifier,
    fd: fd.as_fd(),
    offset: image.offset,
    stride: image.stride,
  };
  let target = client
    .import_dmabuf(&descriptor, DmabufUse::Render)
    .map_err(|error| format!("render import: {error}"))?;
  let half = SIZE / 2;
  let tiles = QUADRANTS
    .iter()
    .enumerate()
    .map(|(index, &[r, g, b])| {
      let index = index as u32;
      RenderTile::solid(
        RenderRect {
          x: (index % 2 * half) as i32,
          y: (index / 2 * half) as i32,
          width: half,
          height: half,
        },
        [linear(r), linear(g), linear(b), 1.0],
      )
    })
    .collect::<Vec<_>>();
  let mut access = ForeignAccess::new();
  access.add(target.foreign());
  let view = target.texture().create_view(&Default::default());
  draw_with_access(
    client,
    &RenderState::default(),
    &view,
    (SIZE, SIZE),
    &tiles,
    [0.0, 0.0, 0.0, 1.0],
    access,
  );
  client
    .device()
    .poll(wgpu::PollType::wait_indefinitely())
    .map_err(|error| error.to_string())?;
  let buffer = Buffer {
    fd,
    fourcc: DRM_FORMAT_XRGB8888,
    modifier,
    offset: image.offset,
    stride: image.stride,
  };
  Ok((image, buffer))
}

fn skip<T>(result: Result<T, String>) -> Option<T> {
  result
    .map_err(|reason| eprintln!("skipping, {reason}"))
    .ok()
}

#[test]
fn udmabuf_xrgb_import_draws_its_pixels_opaque() {
  // The ignored alpha byte is 0, so an import that did not force alpha
  // to 1 would show the white clear.
  let Some(gpu) = gpu() else { return };
  if let Some(buffer) = skip(udmabuf_buffer(&gpu, DRM_FORMAT_XRGB8888, 0)) {
    check_pixels(&gpu, &buffer);
  }
}

#[test]
fn udmabuf_abgr_import_keeps_byte_order() {
  let Some(gpu) = gpu() else { return };
  if let Some(buffer) = skip(udmabuf_buffer(&gpu, DRM_FORMAT_ABGR8888, 255)) {
    check_pixels(&gpu, &buffer);
  }
}

#[test]
fn exported_linear_import_draws_its_pixels() {
  let (Some(gpu), Some(client)) = (gpu(), gpu()) else {
    return;
  };
  if let Some((_image, buffer)) = skip(exported_linear(&client)) {
    check_pixels(&gpu, &buffer);
  }
}

#[test]
fn gpu_rendered_import_draws_its_pixels() {
  let (Some(gpu), Some(client)) = (gpu(), gpu()) else {
    return;
  };
  if let Some((_image, buffer)) = skip(rendered(&client, &gpu)) {
    check_pixels(&gpu, &buffer);
  }
}

#[test]
#[ignore = "an experiment: whether the driver needs the barriers"]
fn exported_linear_import_without_barriers() {
  let (Some(mut gpu), Some(client)) = (gpu(), gpu()) else {
    return;
  };
  gpu.set_foreign_barriers(false);
  if let Some((_image, buffer)) = skip(exported_linear(&client)) {
    check_pixels(&gpu, &buffer);
  }
}

#[test]
#[ignore = "an experiment: whether the driver needs the barriers"]
fn gpu_rendered_import_without_barriers() {
  let (Some(mut gpu), Some(mut client)) = (gpu(), gpu()) else {
    return;
  };
  gpu.set_foreign_barriers(false);
  client.set_foreign_barriers(false);
  if let Some((_image, buffer)) = skip(rendered(&client, &gpu)) {
    check_pixels(&gpu, &buffer);
  }
}

#[test]
#[ignore = "an experiment: whether the driver reads udmabuf memory"]
fn udmabuf_import_on_any_driver() {
  let Some(gpu) = gpu() else { return };
  let buffer = Buffer {
    fd: udmabuf(&pattern(DRM_FORMAT_XRGB8888, 0)).expect("udmabuf"),
    fourcc: DRM_FORMAT_XRGB8888,
    modifier: DRM_FORMAT_MOD_LINEAR,
    offset: 0,
    stride: STRIDE,
  };
  check_pixels(&gpu, &buffer);
}
