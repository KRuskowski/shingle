//! The GPU context shared by every render target.

use std::sync::Mutex;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::{GpuTexture, RenderError, draw::Vertex};

/// A predicate on adapter info that [`GpuOptions::adapter_filter`] uses.
pub type AdapterFilter<'a> = &'a dyn Fn(&wgpu::AdapterInfo) -> bool;

/// How a [`GpuContext`] picks its adapter and device.
#[derive(Clone, Copy, Default)]
pub struct GpuOptions<'a> {
  /// A surface the adapter must be able to present to.
  pub compatible_surface: Option<&'a wgpu::Surface<'a>>,
  /// Device features to request in addition to the defaults.
  pub features: wgpu::Features,
  /// Restricts the adapter, for example to the PCI device of a DRM card.
  /// Without a filter wgpu's high-performance adapter is used.
  pub adapter_filter: Option<AdapterFilter<'a>>,
}

/// The sampler filter a texture is created with.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextureFilter {
  /// Bilinear filtering, used normally.
  #[default]
  Linear,
  /// Nearest-neighbour filtering, used for pixelated zoom.
  Nearest,
}

pub(crate) struct GammaLut {
  texture: wgpu::Texture,
  pub(crate) bind_group: wgpu::BindGroup,
  ramp: Vec<u16>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct EffectUniform {
  inverted: u32,
  preserve_hue: u32,
}

pub(crate) struct Effects {
  buffer: wgpu::Buffer,
  pub(crate) bind_group: wgpu::BindGroup,
  pub(crate) inverted: bool,
  pub(crate) preserve_hue: bool,
}

/// Instance, adapter, device and queue, plus the pipeline, layouts,
/// samplers, gamma LUT and colour effects that every target draws with.
pub struct GpuContext {
  instance: wgpu::Instance,
  adapter: wgpu::Adapter,
  device: wgpu::Device,
  queue: wgpu::Queue,
  texture_layout: wgpu::BindGroupLayout,
  pipeline_layout: wgpu::PipelineLayout,
  shader: wgpu::ShaderModule,
  // Render pipelines by target format, built when a format is first drawn.
  pipelines: Mutex<Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>>,
  linear_sampler: wgpu::Sampler,
  nearest_sampler: wgpu::Sampler,
  pub(crate) white_texture: GpuTexture,
  pub(crate) gamma_lut: GammaLut,
  pub(crate) effects: Effects,
}

impl GpuContext {
  /// Creates the wgpu instance that surfaces and the context share.
  pub fn create_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
  }

  /// Picks an adapter on `instance`, opens its device and builds the shared
  /// render resources.
  pub async fn new(
    instance: wgpu::Instance,
    options: GpuOptions<'_>,
  ) -> Result<Self, RenderError> {
    let adapter = select_adapter(&instance, &options).await?;
    let (device, queue) = adapter
      .request_device(&wgpu::DeviceDescriptor {
        required_features: options.features,
        ..Default::default()
      })
      .await?;
    let texture_layout = texture_bind_group_layout(&device);
    let linear_sampler =
      create_sampler(&device, "shingle linear sampler", TextureFilter::Linear);
    let nearest_sampler = create_sampler(
      &device,
      "shingle nearest sampler",
      TextureFilter::Nearest,
    );
    let gamma_layout = gamma_bind_group_layout(&device);
    let gamma_lut = create_gamma_lut(&device, &queue, &gamma_layout);
    let effect_layout = effect_bind_group_layout(&device);
    let effects = create_effects(&device, &effect_layout);
    let pipeline_layout =
      device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shingle pipeline layout"),
        bind_group_layouts: &[
          Some(&texture_layout),
          Some(&gamma_layout),
          Some(&effect_layout),
        ],
        immediate_size: 0,
      });
    let shader =
      device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
    let white_texture = upload_texture(
      &device,
      &queue,
      &texture_layout,
      &linear_sampler,
      1,
      1,
      &[255, 255, 255, 255],
    )?;
    Ok(Self {
      instance,
      adapter,
      device,
      queue,
      texture_layout,
      pipeline_layout,
      shader,
      pipelines: Mutex::new(Vec::new()),
      linear_sampler,
      nearest_sampler,
      white_texture,
      gamma_lut,
      effects,
    })
  }

  /// The wgpu instance the context was created on.
  pub fn instance(&self) -> &wgpu::Instance {
    &self.instance
  }

  /// The selected adapter.
  pub fn adapter(&self) -> &wgpu::Adapter {
    &self.adapter
  }

  /// The logical device.
  pub fn device(&self) -> &wgpu::Device {
    &self.device
  }

  /// The device's queue.
  pub fn queue(&self) -> &wgpu::Queue {
    &self.queue
  }

  /// Uploads an RGBA8 sRGB texture of `width` by `height` pixels.
  pub fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
    filter: TextureFilter,
  ) -> Result<GpuTexture, RenderError> {
    let sampler = match filter {
      TextureFilter::Linear => &self.linear_sampler,
      TextureFilter::Nearest => &self.nearest_sampler,
    };
    upload_texture(
      &self.device,
      &self.queue,
      &self.texture_layout,
      sampler,
      width,
      height,
      rgba,
    )
  }

  /// Replaces the contents of `texture`, which must keep its size.
  pub fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    check_len(texture.width, texture.height, rgba)?;
    write_texture(
      &self.queue,
      &texture.texture,
      texture.width,
      texture.height,
      rgba,
    );
    Ok(())
  }

  /// Sets the wlr-gamma-control ramp: 256 red, then green, then blue values.
  pub fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    self.gamma_lut.set(&self.queue, ramp)
  }

  /// Enables colour inversion, optionally preserving hue.
  pub fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    self.effects.set(&self.queue, inverted, preserve_hue);
  }

  /// Returns the tile pipeline for targets of `format`, building it once.
  pub(crate) fn pipeline(
    &self,
    format: wgpu::TextureFormat,
  ) -> wgpu::RenderPipeline {
    let mut pipelines = self
      .pipelines
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, pipeline)) =
      pipelines.iter().find(|(existing, _)| *existing == format)
    {
      return pipeline.clone();
    }
    let pipeline = self.create_pipeline(format);
    pipelines.push((format, pipeline.clone()));
    pipeline
  }

  fn create_pipeline(
    &self,
    format: wgpu::TextureFormat,
  ) -> wgpu::RenderPipeline {
    self
      .device
      .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("shingle tile pipeline"),
        layout: Some(&self.pipeline_layout),
        vertex: wgpu::VertexState {
          module: &self.shader,
          entry_point: Some("vs_main"),
          compilation_options: Default::default(),
          buffers: &[Some(Vertex::LAYOUT)],
        },
        primitive: wgpu::PrimitiveState {
          topology: wgpu::PrimitiveTopology::TriangleList,
          cull_mode: None,
          ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
          module: &self.shader,
          entry_point: Some("fs_main"),
          compilation_options: Default::default(),
          targets: &[Some(wgpu::ColorTargetState {
            format,
            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
          })],
        }),
        multiview_mask: None,
        cache: None,
      })
  }

  #[cfg(test)]
  pub(crate) fn pipeline_count(&self) -> usize {
    self
      .pipelines
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
      .len()
  }
}

async fn select_adapter(
  instance: &wgpu::Instance,
  options: &GpuOptions<'_>,
) -> Result<wgpu::Adapter, RenderError> {
  let Some(filter) = options.adapter_filter else {
    return Ok(
      instance
        .request_adapter(&wgpu::RequestAdapterOptions {
          power_preference: wgpu::PowerPreference::HighPerformance,
          compatible_surface: options.compatible_surface,
          force_fallback_adapter: false,
          ..Default::default()
        })
        .await?,
    );
  };
  let adapters = instance.enumerate_adapters(wgpu::Backends::all()).await;
  let infos = adapters
    .iter()
    .map(wgpu::Adapter::get_info)
    .collect::<Vec<_>>();
  let index = matching_adapter(&infos, filter, |index| {
    options
      .compatible_surface
      .is_none_or(|surface| adapters[index].is_surface_supported(surface))
  })
  .ok_or(RenderError::NoMatchingAdapter)?;
  Ok(
    adapters
      .into_iter()
      .nth(index)
      .expect("index from enumeration"),
  )
}

/// Returns the index of the first adapter that passes `filter` and
/// `supported`.
fn matching_adapter(
  infos: &[wgpu::AdapterInfo],
  filter: AdapterFilter<'_>,
  supported: impl Fn(usize) -> bool,
) -> Option<usize> {
  infos
    .iter()
    .enumerate()
    .position(|(index, info)| filter(info) && supported(index))
}

fn texture_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
  device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
    label: Some("shingle texture layout"),
    entries: &[
      wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
          sample_type: wgpu::TextureSampleType::Float { filterable: true },
          view_dimension: wgpu::TextureViewDimension::D2,
          multisampled: false,
        },
        count: None,
      },
      wgpu::BindGroupLayoutEntry {
        binding: 1,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
      },
    ],
  })
}

fn create_sampler(
  device: &wgpu::Device,
  label: &str,
  filter: TextureFilter,
) -> wgpu::Sampler {
  let filter = match filter {
    TextureFilter::Linear => wgpu::FilterMode::Linear,
    TextureFilter::Nearest => wgpu::FilterMode::Nearest,
  };
  device.create_sampler(&wgpu::SamplerDescriptor {
    label: Some(label),
    address_mode_u: wgpu::AddressMode::ClampToEdge,
    address_mode_v: wgpu::AddressMode::ClampToEdge,
    address_mode_w: wgpu::AddressMode::ClampToEdge,
    mag_filter: filter,
    min_filter: filter,
    ..Default::default()
  })
}

pub(crate) fn expected_len(
  width: u32,
  height: u32,
) -> Result<usize, RenderError> {
  if width == 0 || height == 0 {
    return Err(RenderError::EmptyTexture);
  }
  (width as usize)
    .checked_mul(height as usize)
    .and_then(|pixels| pixels.checked_mul(4))
    .ok_or(RenderError::InvalidTextureData {
      expected: usize::MAX,
      actual: 0,
    })
}

fn check_len(width: u32, height: u32, rgba: &[u8]) -> Result<(), RenderError> {
  let expected = expected_len(width, height)?;
  if rgba.len() != expected {
    return Err(RenderError::InvalidTextureData {
      expected,
      actual: rgba.len(),
    });
  }
  Ok(())
}

fn upload_texture(
  device: &wgpu::Device,
  queue: &wgpu::Queue,
  layout: &wgpu::BindGroupLayout,
  sampler: &wgpu::Sampler,
  width: u32,
  height: u32,
  rgba: &[u8],
) -> Result<GpuTexture, RenderError> {
  check_len(width, height, rgba)?;
  let texture = device.create_texture(&wgpu::TextureDescriptor {
    label: Some("shingle client texture"),
    size: wgpu::Extent3d {
      width,
      height,
      depth_or_array_layers: 1,
    },
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    format: wgpu::TextureFormat::Rgba8UnormSrgb,
    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    view_formats: &[],
  });
  write_texture(queue, &texture, width, height, rgba);
  let view = texture.create_view(&Default::default());
  let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
    label: Some("shingle client bind group"),
    layout,
    entries: &[
      wgpu::BindGroupEntry {
        binding: 0,
        resource: wgpu::BindingResource::TextureView(&view),
      },
      wgpu::BindGroupEntry {
        binding: 1,
        resource: wgpu::BindingResource::Sampler(sampler),
      },
    ],
  });
  Ok(GpuTexture {
    texture,
    bind_group,
    width,
    height,
  })
}

fn write_texture(
  queue: &wgpu::Queue,
  texture: &wgpu::Texture,
  width: u32,
  height: u32,
  rgba: &[u8],
) {
  queue.write_texture(
    wgpu::TexelCopyTextureInfo {
      texture,
      mip_level: 0,
      origin: wgpu::Origin3d::ZERO,
      aspect: wgpu::TextureAspect::All,
    },
    rgba,
    wgpu::TexelCopyBufferLayout {
      offset: 0,
      bytes_per_row: Some(width * 4),
      rows_per_image: Some(height),
    },
    wgpu::Extent3d {
      width,
      height,
      depth_or_array_layers: 1,
    },
  );
}

fn gamma_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
  device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
    label: Some("shingle gamma LUT layout"),
    entries: &[wgpu::BindGroupLayoutEntry {
      binding: 0,
      visibility: wgpu::ShaderStages::FRAGMENT,
      ty: wgpu::BindingType::Texture {
        sample_type: wgpu::TextureSampleType::Float { filterable: true },
        view_dimension: wgpu::TextureViewDimension::D2,
        multisampled: false,
      },
      count: None,
    }],
  })
}

fn effect_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
  device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
    label: Some("shingle effect layout"),
    entries: &[wgpu::BindGroupLayoutEntry {
      binding: 0,
      visibility: wgpu::ShaderStages::FRAGMENT,
      ty: wgpu::BindingType::Buffer {
        ty: wgpu::BufferBindingType::Uniform,
        has_dynamic_offset: false,
        min_binding_size: None,
      },
      count: None,
    }],
  })
}

fn create_effects(
  device: &wgpu::Device,
  layout: &wgpu::BindGroupLayout,
) -> Effects {
  let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
    label: Some("shingle effect uniform"),
    contents: bytemuck::bytes_of(&EffectUniform {
      inverted: 0,
      preserve_hue: 0,
    }),
    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
  });
  let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
    label: Some("shingle effect bind group"),
    layout,
    entries: &[wgpu::BindGroupEntry {
      binding: 0,
      resource: buffer.as_entire_binding(),
    }],
  });
  Effects {
    buffer,
    bind_group,
    inverted: false,
    preserve_hue: false,
  }
}

impl Effects {
  fn set(&mut self, queue: &wgpu::Queue, inverted: bool, preserve_hue: bool) {
    if self.inverted == inverted && self.preserve_hue == preserve_hue {
      return;
    }
    self.inverted = inverted;
    self.preserve_hue = preserve_hue;
    queue.write_buffer(
      &self.buffer,
      0,
      bytemuck::bytes_of(&EffectUniform {
        inverted: u32::from(inverted),
        preserve_hue: u32::from(preserve_hue),
      }),
    );
  }
}

fn identity_ramp() -> Vec<u16> {
  (0..256)
    .chain(0..256)
    .chain(0..256)
    .map(|value| value * 257)
    .collect()
}

fn create_gamma_lut(
  device: &wgpu::Device,
  queue: &wgpu::Queue,
  layout: &wgpu::BindGroupLayout,
) -> GammaLut {
  let texture = device.create_texture(&wgpu::TextureDescriptor {
    label: Some("shingle gamma LUT"),
    size: wgpu::Extent3d {
      width: 256,
      height: 1,
      depth_or_array_layers: 1,
    },
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    format: wgpu::TextureFormat::Rgba8Unorm,
    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    view_formats: &[],
  });
  let ramp = identity_ramp();
  write_texture(queue, &texture, 256, 1, &gamma_rgba(&ramp));
  let view = texture.create_view(&Default::default());
  let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
    label: Some("shingle gamma LUT bind group"),
    layout,
    entries: &[wgpu::BindGroupEntry {
      binding: 0,
      resource: wgpu::BindingResource::TextureView(&view),
    }],
  });
  GammaLut {
    texture,
    bind_group,
    ramp,
  }
}

/// Packs a 768-entry 16-bit ramp into 256 RGBA8 LUT texels.
fn gamma_rgba(ramp: &[u16]) -> Vec<u8> {
  let mut rgba = Vec::with_capacity(1024);
  for index in 0..256 {
    for value in [ramp[index], ramp[256 + index], ramp[512 + index]] {
      rgba.push(((u32::from(value) + 128) / 257) as u8);
    }
    rgba.push(255);
  }
  rgba
}

impl GammaLut {
  fn set(
    &mut self,
    queue: &wgpu::Queue,
    ramp: &[u16],
  ) -> Result<(), RenderError> {
    if ramp.len() != 768 {
      return Err(RenderError::InvalidGammaRamp(ramp.len()));
    }
    if self.ramp == ramp {
      return Ok(());
    }
    write_texture(queue, &self.texture, 256, 1, &gamma_rgba(ramp));
    self.ramp.copy_from_slice(ramp);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn info(name: &str) -> wgpu::AdapterInfo {
    wgpu::AdapterInfo {
      name: name.into(),
      ..wgpu::AdapterInfo::new(
        wgpu::DeviceType::DiscreteGpu,
        wgpu::Backend::Vulkan,
      )
    }
  }

  #[test]
  fn identity_ramp_packs_to_identity_lut() {
    let rgba = gamma_rgba(&identity_ramp());
    for (index, texel) in rgba.chunks_exact(4).enumerate() {
      let value = index as u8;
      assert_eq!(texel, [value, value, value, 255]);
    }
  }

  #[test]
  fn expected_len_rejects_empty_and_counts_bytes() {
    assert!(matches!(expected_len(0, 4), Err(RenderError::EmptyTexture)));
    assert!(matches!(expected_len(4, 0), Err(RenderError::EmptyTexture)));
    assert_eq!(expected_len(3, 2).unwrap(), 24);
    assert!(matches!(
      check_len(2, 2, &[0; 15]),
      Err(RenderError::InvalidTextureData {
        expected: 16,
        actual: 15
      })
    ));
  }

  #[test]
  fn adapter_filter_needs_both_filter_and_support() {
    let infos = [info("llvmpipe"), info("nvidia"), info("nvidia second")];
    let nvidia = |info: &wgpu::AdapterInfo| info.name.starts_with("nvidia");
    assert_eq!(matching_adapter(&infos, &nvidia, |_| true), Some(1));
    assert_eq!(
      matching_adapter(&infos, &nvidia, |index| index != 1),
      Some(2)
    );
    assert_eq!(matching_adapter(&infos, &nvidia, |_| false), None);
  }
}
