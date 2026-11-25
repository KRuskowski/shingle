//! DMA-BUF support shared by client import and zero-copy scanout: the
//! device opened with the Vulkan external-memory extensions, the formats
//! and modifiers it can import, the import itself, and the queue-family
//! ownership transfers around every frame that uses an imported image.
//!
//! The design is section 1 of docs/design/dmabuf.md. wgpu-hal's
//! `texture_from_dmabuf_fd` does the import; this module adds what wgpu
//! does not: `VK_EXT_queue_family_foreign` on the device, the modifier
//! query (as wlroots' render/vulkan/pixel_format.c does it), and the
//! acquire and release barriers wlroots records in render/vulkan/pass.c.

use std::ffi::CStr;
use std::os::fd::BorrowedFd;

use ash::vk;
use thiserror::Error;
use wgpu::hal::api::Vulkan;

use crate::{GpuContext, GpuTexture, TextureFilter};

/// Builds a DRM fourcc code from its four characters, as `fourcc_code` in
/// drm_fourcc.h does.
pub const fn fourcc(code: &[u8; 4]) -> u32 {
  (code[0] as u32)
    | ((code[1] as u32) << 8)
    | ((code[2] as u32) << 16)
    | ((code[3] as u32) << 24)
}

/// `DRM_FORMAT_ARGB8888`: 32-bit little-endian ARGB, bytes B, G, R, A.
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
/// `DRM_FORMAT_XRGB8888`: as ARGB8888 with the alpha byte ignored.
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b"XR24");
/// `DRM_FORMAT_ABGR8888`: 32-bit little-endian ABGR, bytes R, G, B, A.
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b"AB24");
/// `DRM_FORMAT_XBGR8888`: as ABGR8888 with the alpha byte ignored.
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b"XB24");

/// `DRM_FORMAT_MOD_LINEAR`: rows of pixels, no tiling.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: "no explicit modifier". Vulkan can import only
/// explicit modifiers, so it is never listed.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// One DRM format Shingle can import, with the wgpu formats it is imported
/// as. DRM names are little-endian, Vulkan and wgpu names are in byte
/// order, so ARGB8888 is `Bgra8*`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrmFormat {
  /// The DRM fourcc.
  pub fourcc: u32,
  /// Its name in drm_fourcc.h without the `DRM_FORMAT_` prefix.
  pub name: &'static str,
  /// The sRGB format, preferred so that sampling decodes and filters in
  /// linear space.
  pub srgb: wgpu::TextureFormat,
  /// The UNORM format, used when a modifier supports only that; the shader
  /// then decodes sRGB.
  pub unorm: wgpu::TextureFormat,
  /// Whether the alpha byte is ignored, so the shader forces alpha to 1.
  pub opaque: bool,
}

/// The formats Shingle imports: the 8-bit RGB formats `wl_shm` offers.
pub const DRM_FORMATS: [DrmFormat; 4] = [
  DrmFormat {
    fourcc: DRM_FORMAT_ARGB8888,
    name: "ARGB8888",
    srgb: wgpu::TextureFormat::Bgra8UnormSrgb,
    unorm: wgpu::TextureFormat::Bgra8Unorm,
    opaque: false,
  },
  DrmFormat {
    fourcc: DRM_FORMAT_XRGB8888,
    name: "XRGB8888",
    srgb: wgpu::TextureFormat::Bgra8UnormSrgb,
    unorm: wgpu::TextureFormat::Bgra8Unorm,
    opaque: true,
  },
  DrmFormat {
    fourcc: DRM_FORMAT_ABGR8888,
    name: "ABGR8888",
    srgb: wgpu::TextureFormat::Rgba8UnormSrgb,
    unorm: wgpu::TextureFormat::Rgba8Unorm,
    opaque: false,
  },
  DrmFormat {
    fourcc: DRM_FORMAT_XBGR8888,
    name: "XBGR8888",
    srgb: wgpu::TextureFormat::Rgba8UnormSrgb,
    unorm: wgpu::TextureFormat::Rgba8Unorm,
    opaque: true,
  },
];

/// The table entry for `fourcc`, if Shingle imports that format.
pub fn drm_format(fourcc: u32) -> Option<&'static DrmFormat> {
  DRM_FORMATS.iter().find(|format| format.fourcc == fourcc)
}

/// The Vulkan format wgpu-hal creates a texture of `format` with.
fn vk_format(format: wgpu::TextureFormat) -> vk::Format {
  match format {
    wgpu::TextureFormat::Bgra8UnormSrgb => vk::Format::B8G8R8A8_SRGB,
    wgpu::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
    wgpu::TextureFormat::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
    wgpu::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
    _ => vk::Format::UNDEFINED,
  }
}

/// A readable name for a modifier: `LINEAR`, `INVALID`, or the vendor from
/// its top byte and the value in hex.
pub fn modifier_name(modifier: u64) -> String {
  match modifier {
    DRM_FORMAT_MOD_LINEAR => return "LINEAR".into(),
    DRM_FORMAT_MOD_INVALID => return "INVALID".into(),
    _ => {}
  }
  let vendor = match modifier >> 56 {
    0x01 => "INTEL",
    0x02 => "AMD",
    0x03 => "NVIDIA",
    0x04 => "SAMSUNG",
    0x05 => "QCOM",
    0x06 => "VIVANTE",
    0x07 => "BROADCOM",
    0x08 => "ARM",
    0x09 => "ALLWINNER",
    0x0a => "AMLOGIC",
    _ => "VENDOR",
  };
  format!("{vendor} {modifier:#018x}")
}

/// What a modifier of one format allows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModifierCaps {
  /// The DRM modifier.
  pub modifier: u64,
  /// The largest importable width in pixels.
  pub max_width: u32,
  /// The largest importable height in pixels.
  pub max_height: u32,
  /// Whether the format's sRGB variant supports the modifier. Without it
  /// the texture is imported as UNORM and the shader decodes sRGB.
  pub srgb: bool,
}

/// The modifiers one DRM format can be imported with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatCaps {
  /// The DRM fourcc.
  pub fourcc: u32,
  /// Modifiers that can be imported and sampled with linear filtering:
  /// client buffers.
  pub sample: Vec<ModifierCaps>,
  /// Modifiers whose sRGB variant can also be rendered to and blended:
  /// NB-8's scanout buffers.
  pub render: Vec<ModifierCaps>,
}

/// What the device can import: the render node to advertise to clients
/// and, per DRM format, the single-plane modifiers Vulkan can import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmabufCaps {
  /// The render node's major and minor number, from
  /// `VK_EXT_physical_device_drm`. `None` (lavapipe) means import works
  /// but there is no device to advertise to clients.
  pub render_node: Option<(u32, u32)>,
  /// The formats of [`DRM_FORMATS`] with at least one sampling modifier.
  pub formats: Vec<FormatCaps>,
  /// Whether `VK_KHR_external_semaphore_fd` is enabled, for later GPU-side
  /// waits and NB-10's `IN_FENCE_FD`.
  pub external_semaphore_fd: bool,
}

impl DmabufCaps {
  /// Queries what `adapter` can import, without opening a device. Fails,
  /// naming what is missing, if the adapter cannot import DMA-BUFs at
  /// all.
  pub fn for_adapter(
    adapter: &wgpu::Adapter,
  ) -> Result<Self, DmabufUnavailable> {
    // SAFETY: the guard is only used for queries within this function.
    let hal = unsafe { adapter.as_hal::<Vulkan>() }
      .ok_or(DmabufUnavailable::NotVulkan)?;
    if !adapter
      .features()
      .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
      return Err(DmabufUnavailable::MissingFeature);
    }
    let capabilities = hal.physical_device_capabilities();
    if !capabilities.supports_extension(ash::ext::queue_family_foreign::NAME) {
      return Err(DmabufUnavailable::MissingExtension(extension_name(
        ash::ext::queue_family_foreign::NAME,
      )));
    }
    let instance = hal.shared_instance();
    if instance.instance_api_version() < vk::API_VERSION_1_1
      || capabilities.properties().api_version < vk::API_VERSION_1_1
    {
      return Err(DmabufUnavailable::OldVulkan);
    }
    let query = VulkanQuery {
      instance: instance.raw_instance(),
      physical_device: hal.raw_physical_device(),
    };
    let render_node = capabilities
      .supports_extension(ash::ext::physical_device_drm::NAME)
      .then(|| query.render_node())
      .flatten();
    let formats = collect_formats(&query);
    if formats.is_empty() {
      return Err(DmabufUnavailable::NoModifiers);
    }
    Ok(Self {
      render_node,
      formats,
      external_semaphore_fd: capabilities
        .supports_extension(ash::khr::external_semaphore_fd::NAME),
    })
  }

  /// The render node as a `dev_t`, as `zwp_linux_dmabuf_feedback_v1`
  /// sends it.
  pub fn render_node_dev(&self) -> Option<u64> {
    self.render_node.map(|(major, minor)| makedev(major, minor))
  }

  /// The capabilities of one format, if it can be imported.
  pub fn format(&self, fourcc: u32) -> Option<&FormatCaps> {
    self.formats.iter().find(|format| format.fourcc == fourcc)
  }

  /// The sampling capabilities of a (format, modifier) pair.
  pub fn sampling(&self, fourcc: u32, modifier: u64) -> Option<&ModifierCaps> {
    self.format(fourcc).and_then(|format| {
      format.sample.iter().find(|caps| caps.modifier == modifier)
    })
  }

  /// The rendering capabilities of a (format, modifier) pair.
  pub fn rendering(&self, fourcc: u32, modifier: u64) -> Option<&ModifierCaps> {
    self.format(fourcc).and_then(|format| {
      format.render.iter().find(|caps| caps.modifier == modifier)
    })
  }

  /// Every (format, modifier) pair that can be sampled, in table order:
  /// what a client may allocate.
  pub fn sample_pairs(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
    self.formats.iter().flat_map(|format| {
      format
        .sample
        .iter()
        .map(move |caps| (format.fourcc, caps.modifier))
    })
  }
}

/// Linux's `makedev`.
pub fn makedev(major: u32, minor: u32) -> u64 {
  let (major, minor) = (u64::from(major), u64::from(minor));
  ((major & 0xffff_f000) << 32)
    | ((major & 0xfff) << 8)
    | ((minor & 0xffff_ff00) << 12)
    | (minor & 0xff)
}

fn extension_name(name: &CStr) -> String {
  name.to_string_lossy().into_owned()
}

/// Why a [`GpuContext`] cannot import DMA-BUFs.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DmabufUnavailable {
  /// [`crate::GpuOptions::dmabuf`] was not set.
  #[error("DMA-BUF import was not requested")]
  NotRequested,
  /// The adapter does not run on wgpu's Vulkan backend.
  #[error("the adapter is not a Vulkan adapter")]
  NotVulkan,
  /// The adapter lacks `VK_KHR_external_memory_fd`,
  /// `VK_EXT_external_memory_dma_buf` or `VK_EXT_image_drm_format_modifier`,
  /// which wgpu's `VULKAN_EXTERNAL_MEMORY_DMA_BUF` needs.
  #[error(
    "the adapter lacks VK_KHR_external_memory_fd, \
     VK_EXT_external_memory_dma_buf or VK_EXT_image_drm_format_modifier"
  )]
  MissingFeature,
  /// The adapter lacks the named device extension.
  #[error("the adapter lacks {0}")]
  MissingExtension(String),
  /// The instance or device is older than Vulkan 1.1, which the format
  /// queries need.
  #[error("the Vulkan instance or device is older than 1.1")]
  OldVulkan,
  /// No format of [`DRM_FORMATS`] has a single-plane modifier that can be
  /// imported and sampled.
  #[error("no 8-bit RGB format has an importable single-plane modifier")]
  NoModifiers,
  /// Creating the device with the extra extensions failed.
  #[error("opening the device for DMA-BUF import failed: {0}")]
  OpenFailed(String),
}

/// How an imported DMA-BUF is used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmabufUse {
  /// Sampled in the composite pass, with the given filter: a client
  /// buffer.
  Sample(TextureFilter),
  /// Rendered to: one of NB-8's scanout buffers.
  Render,
}

/// One single-plane DMA-BUF to import.
#[derive(Clone, Copy, Debug)]
pub struct DmabufDescriptor<'a> {
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// The DRM fourcc.
  pub fourcc: u32,
  /// The DRM modifier.
  pub modifier: u64,
  /// The plane's DMA-BUF. It is duplicated; the caller keeps its own fd.
  pub fd: BorrowedFd<'a>,
  /// Byte offset of the plane in the buffer.
  pub offset: u32,
  /// Bytes per row.
  pub stride: u32,
}

/// Why [`GpuContext::import_dmabuf`] failed.
#[derive(Debug, Error)]
pub enum ImportError {
  /// The context cannot import DMA-BUFs at all.
  #[error("DMA-BUF import is unavailable: {0}")]
  Unavailable(DmabufUnavailable),
  /// The fourcc is not in [`DRM_FORMATS`] or has no usable modifier.
  #[error("DRM format {0:#010x} cannot be imported")]
  UnknownFormat(u32),
  /// The modifier is not listed for this format and use.
  #[error("modifier {} of DRM format {fourcc:#010x} cannot be imported",
    modifier_name(*modifier))]
  UnsupportedModifier {
    /// The DRM fourcc.
    fourcc: u32,
    /// The rejected modifier.
    modifier: u64,
  },
  /// The width or height is zero.
  #[error("DMA-BUF dimensions must be nonzero")]
  EmptySize,
  /// The buffer is larger than the modifier allows.
  #[error("{width}x{height} exceeds the modifier's {max_width}x{max_height}")]
  TooLarge {
    /// Requested width.
    width: u32,
    /// Requested height.
    height: u32,
    /// The modifier's largest width.
    max_width: u32,
    /// The modifier's largest height.
    max_height: u32,
  },
  /// The fd could not be duplicated.
  #[error("failed to duplicate the DMA-BUF fd: {0}")]
  Dup(std::io::Error),
  /// Vulkan refused the import.
  #[error("Vulkan import failed: {0}")]
  Device(String),
}

/// Shader flags of a tile, carried per vertex.
pub(crate) mod tile_flags {
  /// Ignore the texture's alpha: X formats.
  pub(crate) const OPAQUE: u32 = 1;
  /// The texture holds sRGB values in a UNORM format; decode them.
  pub(crate) const DECODE_SRGB: u32 = 2;
}

/// What an import will create, decided from the capabilities alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImportPlan {
  pub(crate) format: wgpu::TextureFormat,
  pub(crate) flags: u32,
}

/// Checks a descriptor against `caps` and picks the texture format and
/// shader flags.
pub(crate) fn plan_import(
  caps: &DmabufCaps,
  descriptor: &DmabufDescriptor<'_>,
  kind: ForeignKind,
) -> Result<ImportPlan, ImportError> {
  let format = drm_format(descriptor.fourcc)
    .filter(|_| caps.format(descriptor.fourcc).is_some())
    .ok_or(ImportError::UnknownFormat(descriptor.fourcc))?;
  let modifier = match kind {
    ForeignKind::Sampled => {
      caps.sampling(descriptor.fourcc, descriptor.modifier)
    }
    ForeignKind::Render => {
      caps.rendering(descriptor.fourcc, descriptor.modifier)
    }
  }
  .ok_or(ImportError::UnsupportedModifier {
    fourcc: descriptor.fourcc,
    modifier: descriptor.modifier,
  })?;
  if descriptor.width == 0 || descriptor.height == 0 {
    return Err(ImportError::EmptySize);
  }
  if descriptor.width > modifier.max_width
    || descriptor.height > modifier.max_height
  {
    return Err(ImportError::TooLarge {
      width: descriptor.width,
      height: descriptor.height,
      max_width: modifier.max_width,
      max_height: modifier.max_height,
    });
  }
  let mut flags = 0;
  if format.opaque && kind == ForeignKind::Sampled {
    flags |= tile_flags::OPAQUE;
  }
  let texture_format = if modifier.srgb {
    format.srgb
  } else {
    flags |= tile_flags::DECODE_SRGB;
    format.unorm
  };
  Ok(ImportPlan {
    format: texture_format,
    flags,
  })
}

/// A DMA-BUF imported as a wgpu texture by [`GpuContext::import_dmabuf`].
/// It owns a duplicate of the buffer's fd through Vulkan; dropping it
/// frees the Vulkan image and memory once the GPU is done with them.
pub struct ImportedTexture {
  texture: wgpu::Texture,
  sampled: Option<GpuTexture>,
  foreign: ForeignImage,
}

impl ImportedTexture {
  /// The wgpu texture, for example to render into with [`DmabufUse::Render`].
  pub fn texture(&self) -> &wgpu::Texture {
    &self.texture
  }

  /// The drawable texture for [`crate::RenderTile`]s, for
  /// [`DmabufUse::Sample`] imports. [`crate::draw`] records the ownership
  /// transfers for it.
  pub fn gpu_texture(&self) -> Option<&GpuTexture> {
    self.sampled.as_ref()
  }

  /// The Vulkan image, for a [`ForeignAccess`] around frames that render
  /// into it.
  pub fn foreign(&self) -> ForeignImage {
    self.foreign
  }
}

/// How a foreign image is accessed in a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignKind {
  /// Sampled by the fragment shader.
  Sampled,
  /// Written as the colour attachment.
  Render,
}

/// An imported image that belongs to `VK_QUEUE_FAMILY_FOREIGN_EXT` between
/// frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForeignImage {
  image: vk::Image,
  kind: ForeignKind,
}

impl ForeignImage {
  /// How the image is used in frames.
  pub fn kind(&self) -> ForeignKind {
    self.kind
  }
}

/// The imported images one submission uses, and the queue-family
/// ownership transfers around it: acquired from the foreign queue family
/// before the frame and released to it after, as wlroots does
/// (render/vulkan/pass.c), so that the client's driver and KMS see an
/// image they own and drivers that keep compression state in the image
/// resolve it.
///
/// wgpu records no such barriers and has no API for them, so they go into
/// their own command buffers, submitted together with the frame's. Between
/// them the wgpu state of a sampled image stays `RESOURCE`, the layout the
/// acquire moves it to, so wgpu emits no conflicting transition.
#[derive(Clone, Debug, Default)]
pub struct ForeignAccess {
  images: Vec<ForeignImage>,
}

/// Source and destination stage masks of one barrier batch.
type Stages = (vk::PipelineStageFlags, vk::PipelineStageFlags);

impl ForeignAccess {
  /// An empty set.
  pub fn new() -> Self {
    Self::default()
  }

  /// Adds an image; adding it again changes nothing.
  pub fn add(&mut self, image: ForeignImage) {
    if !self.images.contains(&image) {
      self.images.push(image);
    }
  }

  /// Adds `texture` if it is an imported one.
  pub fn add_texture(&mut self, texture: &GpuTexture) {
    if let Some(image) = texture.foreign {
      self.add(image);
    }
  }

  /// Whether no imported image is used.
  pub fn is_empty(&self) -> bool {
    self.images.is_empty()
  }

  /// The images, in the order they were added.
  pub fn images(&self) -> &[ForeignImage] {
    &self.images
  }

  /// Submits `frame` between the acquire and release barriers, or alone
  /// if no imported image is used or the context has barriers turned off.
  pub fn submit(
    &self,
    gpu: &GpuContext,
    frame: wgpu::CommandBuffer,
  ) -> wgpu::SubmissionIndex {
    if self.images.is_empty() || !gpu.foreign_barriers() {
      return gpu.queue().submit([frame]);
    }
    // SAFETY: the guard is dropped at once; the ash device is a table of
    // function pointers and a handle that outlives the submission.
    let raw = unsafe { gpu.device().as_hal::<Vulkan>() }
      .map(|device| (device.raw_device().clone(), device.queue_family_index()));
    let Some((device, family)) = raw else {
      return gpu.queue().submit([frame]);
    };
    let acquire = record_barriers(
      gpu,
      &device,
      "shingle foreign acquire",
      self.acquire_stages(),
      &self.acquire_barriers(family),
    );
    let release = record_barriers(
      gpu,
      &device,
      "shingle foreign release",
      self.release_stages(),
      &self.release_barriers(family),
    );
    gpu.queue().submit([acquire, frame, release])
  }

  /// Barriers that take each image from the foreign queue family into
  /// `family`, in the layout the frame uses it in.
  pub(crate) fn acquire_barriers(
    &self,
    family: u32,
  ) -> Vec<vk::ImageMemoryBarrier<'static>> {
    self
      .images
      .iter()
      .map(|image| {
        let (layout, access) = in_frame(image.kind);
        barrier(image.image)
          .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
          .dst_queue_family_index(family)
          .old_layout(vk::ImageLayout::GENERAL)
          .new_layout(layout)
          .src_access_mask(vk::AccessFlags::empty())
          .dst_access_mask(access)
      })
      .collect()
  }

  /// Barriers that hand each image back to the foreign queue family in the
  /// `GENERAL` layout.
  pub(crate) fn release_barriers(
    &self,
    family: u32,
  ) -> Vec<vk::ImageMemoryBarrier<'static>> {
    self
      .images
      .iter()
      .map(|image| {
        let (layout, access) = in_frame(image.kind);
        let written = match image.kind {
          ForeignKind::Sampled => vk::AccessFlags::empty(),
          ForeignKind::Render => access,
        };
        barrier(image.image)
          .src_queue_family_index(family)
          .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
          .old_layout(layout)
          .new_layout(vk::ImageLayout::GENERAL)
          .src_access_mask(written)
          .dst_access_mask(vk::AccessFlags::empty())
      })
      .collect()
  }

  /// Stages of the acquire batch: nothing to wait for on this queue, and
  /// the stages that use the images must wait for it.
  pub(crate) fn acquire_stages(&self) -> Stages {
    (vk::PipelineStageFlags::TOP_OF_PIPE, self.using_stages())
  }

  /// Stages of the release batch: after every use, before nothing.
  pub(crate) fn release_stages(&self) -> Stages {
    (self.using_stages(), vk::PipelineStageFlags::BOTTOM_OF_PIPE)
  }

  fn using_stages(&self) -> vk::PipelineStageFlags {
    self
      .images
      .iter()
      .fold(vk::PipelineStageFlags::empty(), |stages, image| {
        stages
          | match image.kind {
            ForeignKind::Sampled => vk::PipelineStageFlags::FRAGMENT_SHADER,
            ForeignKind::Render => {
              vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
            }
          }
      })
  }
}

/// The layout and access of an image during the frame.
fn in_frame(kind: ForeignKind) -> (vk::ImageLayout, vk::AccessFlags) {
  match kind {
    ForeignKind::Sampled => (
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
      vk::AccessFlags::SHADER_READ,
    ),
    ForeignKind::Render => (
      vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
      vk::AccessFlags::COLOR_ATTACHMENT_READ
        | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
    ),
  }
}

fn barrier(image: vk::Image) -> vk::ImageMemoryBarrier<'static> {
  vk::ImageMemoryBarrier::default()
    .image(image)
    .subresource_range(vk::ImageSubresourceRange {
      aspect_mask: vk::ImageAspectFlags::COLOR,
      base_mip_level: 0,
      level_count: 1,
      base_array_layer: 0,
      layer_count: 1,
    })
}

/// A command buffer that holds only one `vkCmdPipelineBarrier`.
fn record_barriers(
  gpu: &GpuContext,
  device: &ash::Device,
  label: &str,
  (source, destination): Stages,
  barriers: &[vk::ImageMemoryBarrier<'_>],
) -> wgpu::CommandBuffer {
  let mut encoder =
    gpu
      .device()
      .create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some(label),
      });
  // SAFETY: the barrier is recorded into the encoder's open command
  // buffer, which wgpu ends in `finish`; nothing else touches the encoder
  // meanwhile, and the images outlive the submission because the frame's
  // bind groups hold their textures.
  unsafe {
    encoder.as_hal_mut::<Vulkan, _, _>(|encoder| {
      if let Some(encoder) = encoder {
        device.cmd_pipeline_barrier(
          encoder.raw_handle(),
          source,
          destination,
          vk::DependencyFlags::empty(),
          &[],
          &[],
          barriers,
        );
      }
    });
  }
  encoder.finish()
}

/// Opens `adapter`'s device with what [`DmabufCaps`] needs enabled:
/// wgpu's `VULKAN_EXTERNAL_MEMORY_DMA_BUF` plus `VK_EXT_queue_family_foreign`
/// and, if present, `VK_KHR_external_semaphore_fd`.
pub(crate) fn open_device(
  adapter: &wgpu::Adapter,
  features: wgpu::Features,
) -> Result<(wgpu::Device, wgpu::Queue, DmabufCaps), DmabufUnavailable> {
  let caps = DmabufCaps::for_adapter(adapter)?;
  let features = features | wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
  let limits = wgpu::Limits::default();
  let semaphore_fd = caps.external_semaphore_fd;
  let open = {
    // SAFETY: the extensions added are supported (checked by
    // `for_adapter`), and nothing is removed.
    let hal = unsafe { adapter.as_hal::<Vulkan>() }
      .ok_or(DmabufUnavailable::NotVulkan)?;
    let callback: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> =
      Box::new(move |args| {
        let mut wanted = vec![ash::ext::queue_family_foreign::NAME];
        if semaphore_fd {
          wanted.push(ash::khr::external_semaphore_fd::NAME);
        }
        for extension in wanted {
          if !args.extensions.contains(&extension) {
            args.extensions.push(extension);
          }
        }
      });
    unsafe {
      hal.open_with_callback(
        features,
        &limits,
        &wgpu::MemoryHints::default(),
        Some(callback),
      )
    }
    .map_err(|error| DmabufUnavailable::OpenFailed(error.to_string()))?
  };
  // SAFETY: `open` was created from this adapter with these features.
  let (device, queue) = unsafe {
    adapter.create_device_from_hal(
      open,
      &wgpu::DeviceDescriptor {
        label: Some("shingle dmabuf device"),
        required_features: features,
        required_limits: limits,
        ..Default::default()
      },
    )
  }
  .map_err(|error| DmabufUnavailable::OpenFailed(error.to_string()))?;
  Ok((device, queue, caps))
}

impl GpuContext {
  /// Imports a single-plane DMA-BUF as a texture.
  ///
  /// The (format, modifier) pair and the size are checked against
  /// [`GpuContext::dmabuf`] first. The fd is duplicated; Vulkan owns the
  /// duplicate. A sampled import starts in wgpu's `RESOURCE` state, which
  /// it never leaves because Shingle only samples it; a render import
  /// starts uninitialised.
  pub fn import_dmabuf(
    &self,
    descriptor: &DmabufDescriptor<'_>,
    usage: DmabufUse,
  ) -> Result<ImportedTexture, ImportError> {
    let caps = self
      .dmabuf()
      .map_err(|reason| ImportError::Unavailable(reason.clone()))?;
    let kind = match usage {
      DmabufUse::Sample(_) => ForeignKind::Sampled,
      DmabufUse::Render => ForeignKind::Render,
    };
    let plan = plan_import(caps, descriptor, kind)?;
    let fd = descriptor
      .fd
      .try_clone_to_owned()
      .map_err(ImportError::Dup)?;
    let (hal_usage, usage_flags, initial_state) = match kind {
      ForeignKind::Sampled => (
        wgpu::TextureUses::RESOURCE,
        wgpu::TextureUsages::TEXTURE_BINDING,
        wgpu::TextureUses::RESOURCE,
      ),
      ForeignKind::Render => (
        wgpu::TextureUses::COLOR_TARGET,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        wgpu::TextureUses::UNINITIALIZED,
      ),
    };
    let size = wgpu::Extent3d {
      width: descriptor.width,
      height: descriptor.height,
      depth_or_array_layers: 1,
    };
    let hal_descriptor = wgpu::hal::TextureDescriptor {
      label: Some("shingle dmabuf"),
      size,
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: plan.format,
      usage: hal_usage,
      memory_flags: wgpu::hal::MemoryFlags::empty(),
      view_formats: Vec::new(),
    };
    let hal_texture = {
      // SAFETY: the device was opened with VULKAN_EXTERNAL_MEMORY_DMA_BUF
      // (`dmabuf()` is Ok only then), and the modifier, offset and stride
      // describe the buffer as the caller declared it.
      let device = unsafe { self.device().as_hal::<Vulkan>() }
        .ok_or(ImportError::Unavailable(DmabufUnavailable::NotVulkan))?;
      unsafe {
        device.texture_from_dmabuf_fd(
          fd,
          &hal_descriptor,
          descriptor.modifier,
          u64::from(descriptor.stride),
          u64::from(descriptor.offset),
        )
      }
      .map_err(|error| ImportError::Device(error.to_string()))?
    };
    // SAFETY: the handle stays valid as long as the wgpu texture below.
    let image = unsafe { hal_texture.raw_handle() };
    // SAFETY: created from this device with this descriptor; the initial
    // state is the layout the first acquire barrier moves it to (sampled)
    // or one whose contents may be discarded (render).
    let texture = unsafe {
      self.device().create_texture_from_hal::<Vulkan>(
        hal_texture,
        &wgpu::TextureDescriptor {
          label: Some("shingle dmabuf"),
          size,
          mip_level_count: 1,
          sample_count: 1,
          dimension: wgpu::TextureDimension::D2,
          format: plan.format,
          usage: usage_flags,
          view_formats: &[],
        },
        initial_state,
      )
    };
    let foreign = ForeignImage { image, kind };
    let sampled = match usage {
      DmabufUse::Sample(filter) => Some(GpuTexture {
        bind_group: self.bind_texture(&texture, filter),
        texture: texture.clone(),
        width: descriptor.width,
        height: descriptor.height,
        flags: plan.flags,
        foreign: Some(foreign),
      }),
      DmabufUse::Render => None,
    };
    Ok(ImportedTexture {
      texture,
      sampled,
      foreign,
    })
  }
}

/// Vulkan instance-level queries on one physical device.
trait ModifierQuery {
  /// `vkGetPhysicalDeviceFormatProperties2` with a modifier list.
  fn modifiers(
    &self,
    format: vk::Format,
  ) -> Vec<vk::DrmFormatModifierPropertiesEXT>;

  /// `vkGetPhysicalDeviceImageFormatProperties2` for a DMA-BUF import of
  /// `modifier` with `usage`: the maximum extent if the driver accepts it
  /// and reports it importable.
  fn import_extent(
    &self,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
  ) -> Option<(u32, u32)>;
}

struct VulkanQuery<'a> {
  instance: &'a ash::Instance,
  physical_device: vk::PhysicalDevice,
}

impl VulkanQuery<'_> {
  fn render_node(&self) -> Option<(u32, u32)> {
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default();
    properties = properties.push_next(&mut drm);
    // SAFETY: the extension is supported (checked by the caller) and the
    // chain is well-formed.
    unsafe {
      self
        .instance
        .get_physical_device_properties2(self.physical_device, &mut properties);
    }
    (drm.has_render == vk::TRUE).then(|| {
      (
        u32::try_from(drm.render_major).unwrap_or(0),
        u32::try_from(drm.render_minor).unwrap_or(0),
      )
    })
  }
}

impl ModifierQuery for VulkanQuery<'_> {
  fn modifiers(
    &self,
    format: vk::Format,
  ) -> Vec<vk::DrmFormatModifierPropertiesEXT> {
    let count = {
      let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
      let mut properties =
        vk::FormatProperties2::default().push_next(&mut list);
      // SAFETY: a well-formed chain; the driver only writes the count.
      unsafe {
        self.instance.get_physical_device_format_properties2(
          self.physical_device,
          format,
          &mut properties,
        );
      }
      list.drm_format_modifier_count as usize
    };
    if count == 0 {
      return Vec::new();
    }
    let mut modifiers =
      vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
    let written = {
      let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut modifiers);
      let mut properties =
        vk::FormatProperties2::default().push_next(&mut list);
      // SAFETY: the list points at `count` writable entries.
      unsafe {
        self.instance.get_physical_device_format_properties2(
          self.physical_device,
          format,
          &mut properties,
        );
      }
      list.drm_format_modifier_count as usize
    };
    modifiers.truncate(written);
    modifiers
  }

  fn import_extent(
    &self,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
  ) -> Option<(u32, u32)> {
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
      .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier_info =
      vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
      .format(format)
      .ty(vk::ImageType::TYPE_2D)
      .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
      .usage(usage)
      .push_next(&mut external)
      .push_next(&mut modifier_info);
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    let mut properties =
      vk::ImageFormatProperties2::default().push_next(&mut external_properties);
    // SAFETY: well-formed input and output chains.
    let result = unsafe {
      self.instance.get_physical_device_image_format_properties2(
        self.physical_device,
        &info,
        &mut properties,
      )
    };
    let extent = properties.image_format_properties.max_extent;
    result.ok()?;
    let importable = external_properties
      .external_memory_properties
      .external_memory_features
      .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE);
    importable.then_some((extent.width, extent.height))
  }
}

/// Tiling features a modifier needs to be sampled, as wlroots'
/// `dma_tex_features`.
const SAMPLE_FEATURES: vk::FormatFeatureFlags =
  vk::FormatFeatureFlags::from_raw(
    vk::FormatFeatureFlags::SAMPLED_IMAGE.as_raw()
      | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR.as_raw(),
  );

/// Tiling features a modifier needs to be rendered to and blended.
const RENDER_FEATURES: vk::FormatFeatureFlags =
  vk::FormatFeatureFlags::from_raw(
    vk::FormatFeatureFlags::COLOR_ATTACHMENT.as_raw()
      | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND.as_raw(),
  );

/// The single-plane modifiers of `format` with `features` that `query`
/// reports importable for `usage`, with their extents.
fn usable_modifiers(
  query: &impl ModifierQuery,
  format: vk::Format,
  features: vk::FormatFeatureFlags,
  usage: vk::ImageUsageFlags,
) -> Vec<(u64, (u32, u32))> {
  query
    .modifiers(format)
    .into_iter()
    .filter(|properties| {
      properties.drm_format_modifier != DRM_FORMAT_MOD_INVALID
        && properties.drm_format_modifier_plane_count == 1
        && properties
          .drm_format_modifier_tiling_features
          .contains(features)
    })
    .filter_map(|properties| {
      let modifier = properties.drm_format_modifier;
      query
        .import_extent(format, modifier, usage)
        .map(|extent| (modifier, extent))
    })
    .collect()
}

/// The capabilities of every table format that has a sampling modifier.
fn collect_formats(query: &impl ModifierQuery) -> Vec<FormatCaps> {
  DRM_FORMATS
    .iter()
    .filter_map(|format| {
      let srgb = usable_modifiers(
        query,
        vk_format(format.srgb),
        SAMPLE_FEATURES,
        vk::ImageUsageFlags::SAMPLED,
      );
      let unorm = usable_modifiers(
        query,
        vk_format(format.unorm),
        SAMPLE_FEATURES,
        vk::ImageUsageFlags::SAMPLED,
      );
      let mut sample = srgb
        .iter()
        .map(|&(modifier, extent)| caps(modifier, extent, true))
        .collect::<Vec<_>>();
      for &(modifier, extent) in &unorm {
        if !sample.iter().any(|caps| caps.modifier == modifier) {
          sample.push(caps(modifier, extent, false));
        }
      }
      if sample.is_empty() {
        return None;
      }
      let render = usable_modifiers(
        query,
        vk_format(format.srgb),
        RENDER_FEATURES,
        vk::ImageUsageFlags::COLOR_ATTACHMENT,
      )
      .into_iter()
      .map(|(modifier, extent)| caps(modifier, extent, true))
      .collect();
      Some(FormatCaps {
        fourcc: format.fourcc,
        sample,
        render,
      })
    })
    .collect()
}

fn caps(
  modifier: u64,
  (max_width, max_height): (u32, u32),
  srgb: bool,
) -> ModifierCaps {
  ModifierCaps {
    modifier,
    max_width,
    max_height,
    srgb,
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::os::fd::AsFd;

  use ash::vk::Handle;

  use super::*;

  const NV_BLOCK: u64 = 0x0300_0000_0060_6015;
  const NV_BLOCK_SMALL: u64 = 0x0300_0000_0060_6014;
  const AMD_DCC: u64 = 0x0200_0000_0004_3b01;
  const ALL: vk::FormatFeatureFlags = vk::FormatFeatureFlags::from_raw(
    SAMPLE_FEATURES.as_raw() | RENDER_FEATURES.as_raw(),
  );

  /// A recorded driver: modifier lists per format and which
  /// (format, modifier, usage) imports it accepts.
  #[derive(Default)]
  struct FakeQuery {
    modifiers: HashMap<i32, Vec<vk::DrmFormatModifierPropertiesEXT>>,
    refused: Vec<(vk::Format, u64, vk::ImageUsageFlags)>,
  }

  impl FakeQuery {
    fn with(
      mut self,
      format: vk::Format,
      list: &[(u64, u32, vk::FormatFeatureFlags)],
    ) -> Self {
      self.modifiers.insert(
        format.as_raw(),
        list
          .iter()
          .map(|&(modifier, planes, features)| {
            vk::DrmFormatModifierPropertiesEXT {
              drm_format_modifier: modifier,
              drm_format_modifier_plane_count: planes,
              drm_format_modifier_tiling_features: features,
            }
          })
          .collect(),
      );
      self
    }
  }

  impl ModifierQuery for FakeQuery {
    fn modifiers(
      &self,
      format: vk::Format,
    ) -> Vec<vk::DrmFormatModifierPropertiesEXT> {
      self
        .modifiers
        .get(&format.as_raw())
        .cloned()
        .unwrap_or_default()
    }

    fn import_extent(
      &self,
      format: vk::Format,
      modifier: u64,
      usage: vk::ImageUsageFlags,
    ) -> Option<(u32, u32)> {
      (!self.refused.contains(&(format, modifier, usage)))
        .then_some((16384, 8192))
    }
  }

  #[test]
  fn fourcc_codes_match_drm_fourcc_h() {
    assert_eq!(DRM_FORMAT_ARGB8888, 0x3432_5241);
    assert_eq!(DRM_FORMAT_XRGB8888, 0x3432_5258);
    assert_eq!(DRM_FORMAT_ABGR8888, 0x3432_4241);
    assert_eq!(DRM_FORMAT_XBGR8888, 0x3432_4258);
  }

  #[test]
  fn table_maps_little_endian_drm_to_byte_order() {
    let argb = drm_format(DRM_FORMAT_ARGB8888).unwrap();
    assert_eq!(argb.srgb, wgpu::TextureFormat::Bgra8UnormSrgb);
    assert_eq!(argb.unorm, wgpu::TextureFormat::Bgra8Unorm);
    assert!(!argb.opaque);
    let xrgb = drm_format(DRM_FORMAT_XRGB8888).unwrap();
    assert_eq!(xrgb.srgb, wgpu::TextureFormat::Bgra8UnormSrgb);
    assert!(xrgb.opaque);
    let abgr = drm_format(DRM_FORMAT_ABGR8888).unwrap();
    assert_eq!(abgr.srgb, wgpu::TextureFormat::Rgba8UnormSrgb);
    assert!(!abgr.opaque);
    let xbgr = drm_format(DRM_FORMAT_XBGR8888).unwrap();
    assert_eq!(xbgr.unorm, wgpu::TextureFormat::Rgba8Unorm);
    assert!(xbgr.opaque);
    assert_eq!(drm_format(fourcc(b"NV12")), None);
    for format in DRM_FORMATS {
      assert_ne!(vk_format(format.srgb), vk::Format::UNDEFINED);
      assert_ne!(vk_format(format.unorm), vk::Format::UNDEFINED);
    }
    assert_eq!(
      vk_format(wgpu::TextureFormat::Bgra8UnormSrgb),
      vk::Format::B8G8R8A8_SRGB
    );
  }

  #[test]
  fn makedev_matches_the_render_node() {
    assert_eq!(makedev(226, 128), 0xe280);
    assert_eq!(makedev(0x1234, 0x56789), 0x1000_5672_3489);
  }

  #[test]
  fn modifier_names_show_vendor() {
    assert_eq!(modifier_name(0), "LINEAR");
    assert_eq!(modifier_name(DRM_FORMAT_MOD_INVALID), "INVALID");
    assert_eq!(modifier_name(NV_BLOCK), "NVIDIA 0x0300000000606015");
  }

  #[test]
  fn caps_keep_single_plane_importable_sampled_modifiers() {
    let mut query = FakeQuery::default().with(
      vk::Format::B8G8R8A8_SRGB,
      &[
        (DRM_FORMAT_MOD_LINEAR, 1, ALL),
        (NV_BLOCK, 1, ALL),
        // Compression metadata in a second plane.
        (AMD_DCC, 2, ALL),
        // Not filterable.
        (NV_BLOCK_SMALL, 1, vk::FormatFeatureFlags::SAMPLED_IMAGE),
        (DRM_FORMAT_MOD_INVALID, 1, ALL),
      ],
    );
    query.refused.push((
      vk::Format::B8G8R8A8_SRGB,
      NV_BLOCK,
      vk::ImageUsageFlags::COLOR_ATTACHMENT,
    ));
    let formats = collect_formats(&query);
    let fourccs = formats.iter().map(|f| f.fourcc).collect::<Vec<_>>();
    // Both B8G8R8A8 formats share the Vulkan format; the R8G8B8A8 ones
    // have no modifiers and are left out.
    assert_eq!(fourccs, [DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888]);
    let argb = &formats[0];
    assert_eq!(
      argb.sample,
      [
        caps(DRM_FORMAT_MOD_LINEAR, (16384, 8192), true),
        caps(NV_BLOCK, (16384, 8192), true),
      ]
    );
    // The refused colour-attachment import keeps NV_BLOCK out of render.
    assert_eq!(
      argb.render,
      [caps(DRM_FORMAT_MOD_LINEAR, (16384, 8192), true)]
    );
  }

  #[test]
  fn caps_fall_back_to_unorm_without_srgb_support() {
    let mut query = FakeQuery::default()
      .with(vk::Format::R8G8B8A8_SRGB, &[(NV_BLOCK, 1, ALL)])
      .with(
        vk::Format::R8G8B8A8_UNORM,
        &[(NV_BLOCK, 1, ALL), (DRM_FORMAT_MOD_LINEAR, 1, ALL)],
      );
    // sRGB lists NV_BLOCK, but the import check refuses it.
    query.refused.push((
      vk::Format::R8G8B8A8_SRGB,
      NV_BLOCK,
      vk::ImageUsageFlags::SAMPLED,
    ));
    let formats = collect_formats(&query);
    let abgr = formats
      .iter()
      .find(|format| format.fourcc == DRM_FORMAT_ABGR8888)
      .unwrap();
    assert_eq!(
      abgr.sample,
      [
        caps(NV_BLOCK, (16384, 8192), false),
        caps(DRM_FORMAT_MOD_LINEAR, (16384, 8192), false),
      ]
    );
    assert_eq!(abgr.render, [caps(NV_BLOCK, (16384, 8192), true)]);
  }

  #[test]
  fn caps_without_any_modifier_are_empty() {
    assert!(collect_formats(&FakeQuery::default()).is_empty());
  }

  fn sample_caps() -> DmabufCaps {
    DmabufCaps {
      render_node: Some((226, 128)),
      formats: vec![
        FormatCaps {
          fourcc: DRM_FORMAT_XRGB8888,
          sample: vec![
            caps(DRM_FORMAT_MOD_LINEAR, (4096, 4096), true),
            caps(NV_BLOCK, (4096, 4096), false),
          ],
          render: vec![caps(DRM_FORMAT_MOD_LINEAR, (4096, 4096), true)],
        },
        FormatCaps {
          fourcc: DRM_FORMAT_ABGR8888,
          sample: vec![caps(NV_BLOCK, (4096, 4096), true)],
          render: Vec::new(),
        },
      ],
      external_semaphore_fd: true,
    }
  }

  #[test]
  fn caps_list_sample_pairs_and_render_node() {
    let caps = sample_caps();
    assert_eq!(caps.render_node_dev(), Some(0xe280));
    assert_eq!(
      caps.sample_pairs().collect::<Vec<_>>(),
      [
        (DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR),
        (DRM_FORMAT_XRGB8888, NV_BLOCK),
        (DRM_FORMAT_ABGR8888, NV_BLOCK),
      ]
    );
    assert!(caps.sampling(DRM_FORMAT_ABGR8888, NV_BLOCK).is_some());
    assert!(caps.rendering(DRM_FORMAT_ABGR8888, NV_BLOCK).is_none());
  }

  fn descriptor<'a>(
    fd: BorrowedFd<'a>,
    fourcc: u32,
    modifier: u64,
    width: u32,
    height: u32,
  ) -> DmabufDescriptor<'a> {
    DmabufDescriptor {
      width,
      height,
      fourcc,
      modifier,
      fd,
      offset: 0,
      stride: width * 4,
    }
  }

  #[test]
  fn plan_picks_format_and_flags() {
    let caps = sample_caps();
    let file = std::fs::File::open("/dev/null").unwrap();
    let fd = file.as_fd();
    let plan = plan_import(
      &caps,
      &descriptor(fd, DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR, 64, 64),
      ForeignKind::Sampled,
    )
    .unwrap();
    assert_eq!(plan.format, wgpu::TextureFormat::Bgra8UnormSrgb);
    assert_eq!(plan.flags, tile_flags::OPAQUE);
    let plan = plan_import(
      &caps,
      &descriptor(fd, DRM_FORMAT_XRGB8888, NV_BLOCK, 64, 64),
      ForeignKind::Sampled,
    )
    .unwrap();
    assert_eq!(plan.format, wgpu::TextureFormat::Bgra8Unorm);
    assert_eq!(plan.flags, tile_flags::OPAQUE | tile_flags::DECODE_SRGB);
    let plan = plan_import(
      &caps,
      &descriptor(fd, DRM_FORMAT_ABGR8888, NV_BLOCK, 64, 64),
      ForeignKind::Sampled,
    )
    .unwrap();
    assert_eq!(plan.format, wgpu::TextureFormat::Rgba8UnormSrgb);
    assert_eq!(plan.flags, 0);
    // A render target writes alpha as it is; nothing samples it.
    let plan = plan_import(
      &caps,
      &descriptor(fd, DRM_FORMAT_XRGB8888, DRM_FORMAT_MOD_LINEAR, 64, 64),
      ForeignKind::Render,
    )
    .unwrap();
    assert_eq!(plan.flags, 0);
  }

  #[test]
  fn plan_rejects_unlisted_pairs_and_sizes() {
    let caps = sample_caps();
    let file = std::fs::File::open("/dev/null").unwrap();
    let fd = file.as_fd();
    let check = |fourcc, modifier, width, height, kind| {
      plan_import(
        &caps,
        &descriptor(fd, fourcc, modifier, width, height),
        kind,
      )
    };
    assert!(matches!(
      check(DRM_FORMAT_ARGB8888, 0, 64, 64, ForeignKind::Sampled),
      Err(ImportError::UnknownFormat(DRM_FORMAT_ARGB8888))
    ));
    assert!(matches!(
      check(fourcc(b"NV12"), 0, 64, 64, ForeignKind::Sampled),
      Err(ImportError::UnknownFormat(_))
    ));
    assert!(matches!(
      check(DRM_FORMAT_ABGR8888, 0, 64, 64, ForeignKind::Sampled),
      Err(ImportError::UnsupportedModifier { modifier: 0, .. })
    ));
    assert!(matches!(
      check(DRM_FORMAT_ABGR8888, NV_BLOCK, 64, 64, ForeignKind::Render),
      Err(ImportError::UnsupportedModifier { .. })
    ));
    assert!(matches!(
      check(DRM_FORMAT_XRGB8888, 0, 0, 64, ForeignKind::Sampled),
      Err(ImportError::EmptySize)
    ));
    assert!(matches!(
      check(DRM_FORMAT_XRGB8888, 0, 4097, 64, ForeignKind::Sampled),
      Err(ImportError::TooLarge {
        max_width: 4096,
        ..
      })
    ));
  }

  fn image(raw: u64, kind: ForeignKind) -> ForeignImage {
    ForeignImage {
      image: vk::Image::from_raw(raw),
      kind,
    }
  }

  #[test]
  fn foreign_access_ignores_duplicates() {
    let mut access = ForeignAccess::new();
    assert!(access.is_empty());
    access.add(image(1, ForeignKind::Sampled));
    access.add(image(1, ForeignKind::Sampled));
    access.add(image(2, ForeignKind::Render));
    assert_eq!(access.images().len(), 2);
  }

  #[test]
  fn sampled_images_are_acquired_for_shader_reads_and_released() {
    let mut access = ForeignAccess::new();
    access.add(image(7, ForeignKind::Sampled));
    let acquire = access.acquire_barriers(3);
    assert_eq!(acquire.len(), 1);
    let barrier = acquire[0];
    assert_eq!(barrier.image, vk::Image::from_raw(7));
    assert_eq!(barrier.src_queue_family_index, vk::QUEUE_FAMILY_FOREIGN_EXT);
    assert_eq!(barrier.dst_queue_family_index, 3);
    assert_eq!(barrier.old_layout, vk::ImageLayout::GENERAL);
    assert_eq!(
      barrier.new_layout,
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
    );
    assert_eq!(barrier.dst_access_mask, vk::AccessFlags::SHADER_READ);
    assert_eq!(
      barrier.subresource_range.aspect_mask,
      vk::ImageAspectFlags::COLOR
    );
    assert_eq!(barrier.subresource_range.level_count, 1);
    assert_eq!(barrier.subresource_range.layer_count, 1);
    let release = access.release_barriers(3)[0];
    assert_eq!(release.src_queue_family_index, 3);
    assert_eq!(release.dst_queue_family_index, vk::QUEUE_FAMILY_FOREIGN_EXT);
    assert_eq!(
      release.old_layout,
      vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
    );
    assert_eq!(release.new_layout, vk::ImageLayout::GENERAL);
    assert_eq!(release.src_access_mask, vk::AccessFlags::empty());
    assert_eq!(
      access.acquire_stages(),
      (
        vk::PipelineStageFlags::TOP_OF_PIPE,
        vk::PipelineStageFlags::FRAGMENT_SHADER
      )
    );
    assert_eq!(
      access.release_stages(),
      (
        vk::PipelineStageFlags::FRAGMENT_SHADER,
        vk::PipelineStageFlags::BOTTOM_OF_PIPE
      )
    );
  }

  #[test]
  fn render_images_are_acquired_as_attachments_and_flushed() {
    let mut access = ForeignAccess::new();
    access.add(image(1, ForeignKind::Sampled));
    access.add(image(2, ForeignKind::Render));
    let acquire = access.acquire_barriers(0);
    assert_eq!(
      acquire[1].new_layout,
      vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
    );
    assert!(
      acquire[1]
        .dst_access_mask
        .contains(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
    );
    let release = access.release_barriers(0);
    assert_eq!(
      release[1].old_layout,
      vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
    );
    assert_eq!(release[1].new_layout, vk::ImageLayout::GENERAL);
    // Writes are made available before the foreign queue reads them.
    assert!(
      release[1]
        .src_access_mask
        .contains(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
    );
    let both = vk::PipelineStageFlags::FRAGMENT_SHADER
      | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT;
    assert_eq!(access.acquire_stages().1, both);
    assert_eq!(access.release_stages().0, both);
  }
}
