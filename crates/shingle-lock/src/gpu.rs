//! The lock screen on the GPU: one device for every output, the
//! screenshot of each blurred once, and per frame the drifting
//! background with its vignette and the clock and input overlays, read
//! back into the `wl_shm` buffer the lock surface shows.
//!
//! Targets are `Bgra8Unorm`, so the bytes read back are already
//! `XRGB8888`'s B, G, R, X, and colours are the raw bytes that
//! swaylock-effects works on.

use std::f32::consts::TAU;

use bytemuck::{Pod, Zeroable};
use shingle_config::DriftConfig;
use shingle_render::{GpuContext, GpuOptions};

use crate::{capture::Screenshot, ui::Raster};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// The blur's radius in texels at most; larger ones blur a smaller copy.
const MAX_BLUR_TAPS: u32 = 16;
/// How bright the shimmer gets, as a fraction, while the drift moves.
const SHIMMER: f32 = 0.025;

/// Where the background is at one moment of its drift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drift {
  /// The offset of the image, in texture coordinates.
  pub offset: [f32; 2],
  /// The image's zoom, above 1 so its edges never show.
  pub zoom: f32,
  /// The shimmer's phase in radians.
  pub phase: f32,
  /// The shimmer's strength.
  pub shimmer: f32,
}

impl Drift {
  /// No movement.
  pub const STILL: Self = Self {
    offset: [0.0, 0.0],
    zoom: 1.0,
    phase: 0.0,
    shimmer: 0.0,
  };

  /// The drift `seconds` after locking on an output of `size` logical
  /// pixels: a slow Lissajous path of `config.amplitude` pixels, one
  /// cycle per `config.period`, a breathing zoom of half a percent and
  /// a faint moving shimmer.
  pub fn at(seconds: f64, config: &DriftConfig, size: (u32, u32)) -> Self {
    if !config.enabled || config.amplitude <= 0.0 {
      return Self::STILL;
    }
    let (width, height) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let amplitude = config.amplitude as f32;
    let cycle = (seconds / config.period).fract() as f32 * TAU;
    let breathing = 0.0025 * (1.0 + (cycle * 0.5).sin());
    let zoom = 1.0 + 2.0 * amplitude / width.min(height) + breathing;
    Self {
      offset: [
        amplitude * cycle.sin() / width / zoom,
        amplitude * (cycle * 2.0 + 1.1).sin() / height / zoom,
      ],
      zoom,
      phase: cycle,
      shimmer: SHIMMER,
    }
  }
}

/// The blur's scale and radius for swaylock-effects' `effect-blur`
/// `radius` in pixels of an image of `size`: a radius above
/// [`MAX_BLUR_TAPS`] blurs a copy a power of two smaller, by as much.
fn blur_plan(radius: u32, size: (u32, u32)) -> (u32, (u32, u32), f32) {
  let mut divisor = 1;
  while radius / divisor > MAX_BLUR_TAPS && divisor < 16 {
    divisor *= 2;
  }
  let scaled = (
    size.0.div_ceil(divisor).max(1),
    size.1.div_ceil(divisor).max(1),
  );
  (divisor, scaled, radius as f32 / divisor as f32)
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlurUniform {
  direction: [f32; 2],
  radius: f32,
  pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BackgroundUniform {
  transform: [f32; 4],
  vignette: [f32; 4],
  color: [f32; 4],
  shimmer: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct OverlayUniform {
  rect: [f32; 4],
  alpha: [f32; 4],
}

/// The device and the pipelines every output draws with.
pub struct Gpu {
  context: GpuContext,
  layout: wgpu::BindGroupLayout,
  blur: wgpu::RenderPipeline,
  background: wgpu::RenderPipeline,
  overlay: wgpu::RenderPipeline,
  sampler: wgpu::Sampler,
  /// A black pixel, for outputs without a screenshot.
  empty: (wgpu::Texture, wgpu::TextureView),
}

/// An output's screenshot on the GPU, blurred if asked.
pub struct Image {
  _texture: wgpu::Texture,
  view: wgpu::TextureView,
  /// The screenshot's size in pixels, whose aspect ratio
  /// [`Target::new`] keeps.
  size: (u32, u32),
}

/// The texture-coordinate scale that makes an image of `image` pixels
/// cover a target of `target` pixels with its aspect ratio kept: the
/// image is scaled until both sides fit and the overflow on one axis is
/// cut off evenly, so that a screenshot shown on an output of another
/// shape is neither stretched nor letterboxed.
pub fn cover_scale(image: (u32, u32), target: (u32, u32)) -> [f32; 2] {
  let aspect =
    |(width, height): (u32, u32)| width.max(1) as f32 / height.max(1) as f32;
  let (image, target) = (aspect(image), aspect(target));
  if target > image {
    [1.0, image / target]
  } else {
    [target / image, 1.0]
  }
}

impl Gpu {
  /// Opens the default adapter and builds the pipelines.
  ///
  /// # Errors
  ///
  /// When no adapter or device can be had.
  pub fn new() -> Result<Self, String> {
    let context = pollster::block_on(GpuContext::new(
      GpuContext::create_instance(),
      GpuOptions::default(),
    ))
    .map_err(|error| error.to_string())?;
    let device = context.device();
    let shader = device.create_shader_module(wgpu::include_wgsl!("lock.wgsl"));
    let layout =
      device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shingle-lock layout"),
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
          wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
              ty: wgpu::BufferBindingType::Uniform,
              has_dynamic_offset: false,
              min_binding_size: None,
            },
            count: None,
          },
        ],
      });
    let pipeline_layout =
      device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shingle-lock pipeline layout"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
      });
    let pipeline = |vertex: &str, fragment: &str, blend| {
      device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(fragment),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
          module: &shader,
          entry_point: Some(vertex),
          compilation_options: Default::default(),
          buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
          module: &shader,
          entry_point: Some(fragment),
          compilation_options: Default::default(),
          targets: &[Some(wgpu::ColorTargetState {
            format: FORMAT,
            blend,
            write_mask: wgpu::ColorWrites::ALL,
          })],
        }),
        multiview_mask: None,
        cache: None,
      })
    };
    let blur = pipeline("vs_full", "fs_blur", None);
    let background = pipeline("vs_full", "fs_background", None);
    let overlay = pipeline(
      "vs_overlay",
      "fs_overlay",
      Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
    );
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
      label: Some("shingle-lock sampler"),
      address_mode_u: wgpu::AddressMode::ClampToEdge,
      address_mode_v: wgpu::AddressMode::ClampToEdge,
      mag_filter: wgpu::FilterMode::Linear,
      min_filter: wgpu::FilterMode::Linear,
      ..Default::default()
    });
    let gpu = Self {
      empty: with_view(texture(
        device,
        (1, 1),
        FORMAT,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
      )),
      context,
      layout,
      blur,
      background,
      overlay,
      sampler,
    };
    Ok(gpu)
  }

  fn device(&self) -> &wgpu::Device {
    self.context.device()
  }

  fn queue(&self) -> &wgpu::Queue {
    self.context.queue()
  }

  fn uniform<T: Pod>(&self, value: &T) -> wgpu::Buffer {
    let buffer = self.device().create_buffer(&wgpu::BufferDescriptor {
      label: Some("shingle-lock uniform"),
      size: std::mem::size_of::<T>() as u64,
      usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
      mapped_at_creation: false,
    });
    self
      .queue()
      .write_buffer(&buffer, 0, bytemuck::bytes_of(value));
    buffer
  }

  fn bind(
    &self,
    view: &wgpu::TextureView,
    uniform: &wgpu::Buffer,
  ) -> wgpu::BindGroup {
    self.device().create_bind_group(&wgpu::BindGroupDescriptor {
      label: Some("shingle-lock bind group"),
      layout: &self.layout,
      entries: &[
        wgpu::BindGroupEntry {
          binding: 0,
          resource: wgpu::BindingResource::TextureView(view),
        },
        wgpu::BindGroupEntry {
          binding: 1,
          resource: wgpu::BindingResource::Sampler(&self.sampler),
        },
        wgpu::BindGroupEntry {
          binding: 2,
          resource: uniform.as_entire_binding(),
        },
      ],
    })
  }

  /// Draws `pipeline` over `target` with `bind`.
  fn pass(
    &self,
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind: &wgpu::BindGroup,
    vertices: u32,
    clear: bool,
  ) {
    let load = if clear {
      wgpu::LoadOp::Clear(wgpu::Color::BLACK)
    } else {
      wgpu::LoadOp::Load
    };
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
      label: Some("shingle-lock pass"),
      color_attachments: &[Some(wgpu::RenderPassColorAttachment {
        view: target,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
          load,
          store: wgpu::StoreOp::Store,
        },
      })],
      ..Default::default()
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind, &[]);
    pass.draw(0..vertices, 0..1);
  }

  /// Uploads `shot` and blurs it as `blur` (`effect-blur`'s radius and
  /// passes) asks.
  pub fn image(&self, shot: &Screenshot, blur: Option<(u32, u32)>) -> Image {
    let size = (shot.width.max(1), shot.height.max(1));
    let usage = wgpu::TextureUsages::TEXTURE_BINDING
      | wgpu::TextureUsages::COPY_DST
      | wgpu::TextureUsages::RENDER_ATTACHMENT;
    let source = texture(self.device(), size, FORMAT, usage);
    write(self.queue(), &source, size, &shot.bgra);
    let (source_texture, source) = with_view(source);
    let Some((radius, times)) =
      blur.filter(|(radius, times)| *radius > 0 && *times > 0)
    else {
      return Image {
        _texture: source_texture,
        view: source,
        size,
      };
    };
    let (divisor, scaled, radius) = blur_plan(radius, size);
    let (ping_texture, ping) =
      with_view(texture(self.device(), scaled, FORMAT, usage));
    let (_pong_texture, pong) =
      with_view(texture(self.device(), scaled, FORMAT, usage));
    let mut encoder = self
      .device()
      .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    // Shrinking samples the source once per texel; the blur smooths what
    // the linear filter leaves.
    let copy = self.uniform(&BlurUniform {
      direction: [0.0, 0.0],
      radius: 0.0,
      pad: 0.0,
    });
    self.pass(
      &mut encoder,
      &ping,
      &self.blur,
      &self.bind(&source, &copy),
      3,
      true,
    );
    let mut keep = vec![copy];
    for _ in 0..times {
      for (direction, from, to) in
        [([1.0, 0.0], &ping, &pong), ([0.0, 1.0], &pong, &ping)]
      {
        let uniform = self.uniform(&BlurUniform {
          direction,
          radius,
          pad: 0.0,
        });
        self.pass(
          &mut encoder,
          to,
          &self.blur,
          &self.bind(from, &uniform),
          3,
          true,
        );
        keep.push(uniform);
      }
    }
    self.queue().submit([encoder.finish()]);
    eprintln!(
      "shingle-lock: blurred a {}x{} screenshot at 1/{divisor} size",
      size.0, size.1
    );
    Image {
      _texture: ping_texture,
      view: ping,
      size,
    }
  }
}

/// `texture` with a view of all of it.
fn with_view(texture: wgpu::Texture) -> (wgpu::Texture, wgpu::TextureView) {
  let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
  (texture, view)
}

fn texture(
  device: &wgpu::Device,
  (width, height): (u32, u32),
  format: wgpu::TextureFormat,
  usage: wgpu::TextureUsages,
) -> wgpu::Texture {
  device.create_texture(&wgpu::TextureDescriptor {
    label: Some("shingle-lock texture"),
    size: wgpu::Extent3d {
      width,
      height,
      depth_or_array_layers: 1,
    },
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    format,
    usage,
    view_formats: &[],
  })
}

fn write(
  queue: &wgpu::Queue,
  texture: &wgpu::Texture,
  (width, height): (u32, u32),
  bytes: &[u8],
) {
  queue.write_texture(
    wgpu::TexelCopyTextureInfo {
      texture,
      mip_level: 0,
      origin: wgpu::Origin3d::ZERO,
      aspect: wgpu::TextureAspect::All,
    },
    bytes,
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

/// An overlay's texture and where it goes.
struct Overlay {
  _texture: wgpu::Texture,
  size: (u32, u32),
  uniform: wgpu::Buffer,
  bind: wgpu::BindGroup,
}

/// What one frame shows besides the background's drift.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
  /// The drift now.
  pub drift: Drift,
  /// `effect-vignette`'s base and factor.
  pub vignette: Option<(f64, f64)>,
  /// The background colour, sRGB bytes.
  pub color: [u8; 4],
  /// The clock's top-left corner and opacity, if it shows.
  pub clock: Option<((i32, i32), f32)>,
  /// The input's top-left corner and opacity, if it shows.
  pub input: Option<((i32, i32), f32)>,
}

/// One output's target, its background and overlays, and the buffer its
/// frames are read back through.
pub struct Target {
  size: (u32, u32),
  view: wgpu::TextureView,
  texture: wgpu::Texture,
  readback: wgpu::Buffer,
  padded_row: u32,
  background_uniform: wgpu::Buffer,
  background: wgpu::BindGroup,
  has_image: bool,
  /// [`cover_scale`] of the image on this target.
  cover: [f32; 2],
  clock: Option<Overlay>,
  input: Option<Overlay>,
}

impl Target {
  /// A target of `size` logical pixels over `image`, scaled to cover it
  /// with its aspect ratio kept ([`cover_scale`]), or over the plain
  /// background colour without one.
  pub fn new(gpu: &Gpu, size: (u32, u32), image: Option<&Image>) -> Self {
    let size = (size.0.max(1), size.1.max(1));
    let texture = texture(
      gpu.device(),
      size,
      FORMAT,
      wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let padded_row = (size.0 * 4).div_ceil(256) * 256;
    let readback = gpu.device().create_buffer(&wgpu::BufferDescriptor {
      label: Some("shingle-lock readback"),
      size: u64::from(padded_row) * u64::from(size.1),
      usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
      mapped_at_creation: false,
    });
    let background_uniform = gpu.uniform(&BackgroundUniform::zeroed());
    let view = image.map_or(&gpu.empty.1, |image| &image.view);
    let background = gpu.bind(view, &background_uniform);
    Self {
      size,
      view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
      texture,
      readback,
      padded_row,
      background_uniform,
      background,
      has_image: image.is_some(),
      cover: image.map_or([1.0, 1.0], |image| cover_scale(image.size, size)),
      clock: None,
      input: None,
    }
  }

  /// The target's size.
  pub fn size(&self) -> (u32, u32) {
    self.size
  }

  fn overlay(gpu: &Gpu, raster: &Raster) -> Overlay {
    let size = (raster.width, raster.height);
    let texture = texture(
      gpu.device(),
      size,
      wgpu::TextureFormat::Rgba8Unorm,
      wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    write(gpu.queue(), &texture, size, &raster.rgba);
    let (texture, view) = with_view(texture);
    let uniform = gpu.uniform(&OverlayUniform::zeroed());
    let bind = gpu.bind(&view, &uniform);
    Overlay {
      _texture: texture,
      size,
      uniform,
      bind,
    }
  }

  /// Replaces the clock's image.
  pub fn set_clock(&mut self, gpu: &Gpu, raster: Option<&Raster>) {
    self.clock = raster.map(|raster| Self::overlay(gpu, raster));
  }

  /// Replaces the input's image.
  pub fn set_input(&mut self, gpu: &Gpu, raster: Option<&Raster>) {
    self.input = raster.map(|raster| Self::overlay(gpu, raster));
  }

  /// The clip-space rectangle of `size` pixels at `at`.
  fn clip_rect(&self, (x, y): (i32, i32), size: (u32, u32)) -> [f32; 4] {
    let (width, height) = (self.size.0 as f32, self.size.1 as f32);
    let x0 = x as f32 / width * 2.0 - 1.0;
    let y0 = 1.0 - y as f32 / height * 2.0;
    let x1 = (x as f32 + size.0 as f32) / width * 2.0 - 1.0;
    let y1 = 1.0 - (y as f32 + size.1 as f32) / height * 2.0;
    [x0, y0, x1, y1]
  }

  /// Draws `frame` and copies it into `out`, rows `stride` bytes apart,
  /// as `XRGB8888`.
  ///
  /// # Errors
  ///
  /// When the GPU fails or `out` is too small.
  pub fn draw(
    &mut self,
    gpu: &Gpu,
    frame: &Frame,
    out: &mut [u8],
    stride: usize,
  ) -> Result<(), String> {
    let (width, height) = self.size;
    let row = width as usize * 4;
    if stride < row || out.len() < stride * (height as usize - 1) + row {
      return Err("the buffer does not fit the frame".into());
    }
    let byte = |value: u8| f32::from(value) / 255.0;
    let (base, factor) = frame.vignette.unwrap_or((1.0, 0.0));
    let uniform = BackgroundUniform {
      transform: [
        frame.drift.offset[0],
        frame.drift.offset[1],
        frame.drift.zoom,
        if self.has_image { 1.0 } else { 0.0 },
      ],
      vignette: [
        base as f32,
        factor as f32,
        if frame.vignette.is_some() { 1.0 } else { 0.0 },
        frame.drift.shimmer,
      ],
      color: [
        byte(frame.color[0]),
        byte(frame.color[1]),
        byte(frame.color[2]),
        1.0,
      ],
      shimmer: [frame.drift.phase, self.cover[0], self.cover[1], 0.0],
    };
    gpu.queue().write_buffer(
      &self.background_uniform,
      0,
      bytemuck::bytes_of(&uniform),
    );
    let mut encoder = gpu
      .device()
      .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    gpu.pass(
      &mut encoder,
      &self.view,
      &gpu.background,
      &self.background,
      3,
      true,
    );
    for (overlay, placement) in
      [(&self.clock, frame.clock), (&self.input, frame.input)]
    {
      let (Some(overlay), Some((at, alpha))) = (overlay, placement) else {
        continue;
      };
      if alpha <= 0.0 {
        continue;
      }
      let uniform = OverlayUniform {
        rect: self.clip_rect(at, overlay.size),
        alpha: [alpha.min(1.0), 0.0, 0.0, 0.0],
      };
      gpu.queue().write_buffer(
        &overlay.uniform,
        0,
        bytemuck::bytes_of(&uniform),
      );
      gpu.pass(
        &mut encoder,
        &self.view,
        &gpu.overlay,
        &overlay.bind,
        6,
        false,
      );
    }
    encoder.copy_texture_to_buffer(
      wgpu::TexelCopyTextureInfo {
        texture: &self.texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
      },
      wgpu::TexelCopyBufferInfo {
        buffer: &self.readback,
        layout: wgpu::TexelCopyBufferLayout {
          offset: 0,
          bytes_per_row: Some(self.padded_row),
          rows_per_image: Some(height),
        },
      },
      wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
      },
    );
    gpu.queue().submit([encoder.finish()]);
    let (sender, receiver) = std::sync::mpsc::channel();
    self
      .readback
      .clone()
      .map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = sender.send(result);
      });
    gpu
      .device()
      .poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(std::time::Duration::from_secs(2)),
      })
      .map_err(|error| error.to_string())?;
    receiver
      .recv()
      .map_err(|error| error.to_string())?
      .map_err(|error| error.to_string())?;
    {
      let mapped = self
        .readback
        .get_mapped_range(..)
        .map_err(|error| error.to_string())?;
      for (y, source) in mapped
        .chunks_exact(self.padded_row as usize)
        .take(height as usize)
        .enumerate()
      {
        let destination = &mut out[y * stride..y * stride + row];
        destination.copy_from_slice(&source[..row]);
        // XRGB8888 ignores the X byte; it stays opaque either way.
        for pixel in destination.chunks_exact_mut(4) {
          pixel[3] = 0xff;
        }
      }
    }
    self.readback.unmap();
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_drift_moves_slowly_and_never_shows_an_edge() {
    let config = DriftConfig::default();
    let size = (1920, 1080);
    let start = Drift::at(0.0, &config, size);
    let later = Drift::at(1.0, &config, size);
    let offset = |drift: Drift| {
      (
        drift.offset[0] * size.0 as f32 * drift.zoom,
        drift.offset[1] * size.1 as f32 * drift.zoom,
      )
    };
    let (x0, y0) = offset(start);
    let (x1, y1) = offset(later);
    let moved = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
    assert!(moved > 0.5 && moved < 8.0, "{moved} pixels in a second");
    for tenth in 0..300 {
      let drift = Drift::at(f64::from(tenth) / 10.0, &config, size);
      // The visible part, half of 1/zoom around the centre plus the
      // offset, stays inside the image.
      let half = 0.5 / drift.zoom;
      for (axis, offset) in drift.offset.iter().enumerate() {
        assert!(half + offset.abs() <= 0.5 + 1e-4, "{axis} at {tenth}");
      }
    }
    let still = Drift::at(
      5.0,
      &DriftConfig {
        enabled: false,
        ..config.clone()
      },
      size,
    );
    assert_eq!(still, Drift::STILL);
    assert_eq!(Drift::at(30.0, &config, size).offset, start.offset);
  }

  /// A 64x32 screenshot, red on the left half and green on the right.
  fn halves() -> Screenshot {
    let mut bgra = Vec::new();
    for _ in 0..32 {
      for x in 0..64 {
        bgra.extend_from_slice(if x < 32 {
          &[0, 0, 255, 255]
        } else {
          &[0, 255, 0, 255]
        });
      }
    }
    Screenshot {
      name: None,
      width: 64,
      height: 32,
      bgra,
    }
  }

  /// A 64x32 screenshot of four 16 pixel wide stripes: red, green, blue
  /// and white.
  fn stripes() -> Screenshot {
    let colours: [[u8; 4]; 4] = [
      [0, 0, 255, 255],
      [0, 255, 0, 255],
      [255, 0, 0, 255],
      [255, 255, 255, 255],
    ];
    let mut bgra = Vec::new();
    for _ in 0..32 {
      for x in 0..64 {
        bgra.extend_from_slice(&colours[x / 16]);
      }
    }
    Screenshot {
      name: None,
      width: 64,
      height: 32,
      bgra,
    }
  }

  #[test]
  fn other_shapes_are_covered_not_stretched() {
    assert_eq!(cover_scale((1920, 1080), (1920, 1080)), [1.0, 1.0]);
    assert_eq!(cover_scale((3840, 2160), (1920, 1080)), [1.0, 1.0]);
    // A wider output shows the middle rows, a taller one the middle
    // columns.
    assert_eq!(cover_scale((1600, 1000), (2000, 1000)), [1.0, 0.8]);
    assert_eq!(cover_scale((2000, 1000), (1000, 1000)), [0.5, 1.0]);
  }

  /// The B, G, R bytes at (x, y) of a 64 pixel wide frame.
  fn at(frame: &[u8], x: usize, y: usize) -> [u8; 3] {
    let offset = y * 256 + x * 4;
    [frame[offset], frame[offset + 1], frame[offset + 2]]
  }

  #[test]
  fn a_frame_blurs_the_screenshot_and_draws_the_overlays() {
    let Ok(gpu) = Gpu::new() else {
      eprintln!("no GPU; skipped");
      return;
    };
    let sharp = gpu.image(&halves(), None);
    let blurred = gpu.image(&halves(), Some((6, 2)));
    let mut frame = vec![0; 256 * 32];
    let plain = Frame {
      drift: Drift::STILL,
      vignette: None,
      color: [0, 0, 0, 255],
      clock: None,
      input: None,
    };
    let mut target = Target::new(&gpu, (64, 32), Some(&sharp));
    target.draw(&gpu, &plain, &mut frame, 256).unwrap();
    assert_eq!(at(&frame, 30, 16), [0, 0, 255]);
    assert_eq!(at(&frame, 33, 16), [0, 255, 0]);
    assert!(frame.chunks_exact(4).all(|pixel| pixel[3] == 255));
    let mut target = Target::new(&gpu, (64, 32), Some(&blurred));
    target.draw(&gpu, &plain, &mut frame, 256).unwrap();
    let [_, green, red] = at(&frame, 31, 16);
    assert!(red > 60 && green > 60, "the edge is blurred: {red} {green}");
    assert!(at(&frame, 2, 16)[2] > 200, "far from it stays red");
    // The input overlay at its place and opacity, and the vignette.
    target.set_input(
      &gpu,
      Some(&Raster {
        width: 4,
        height: 4,
        rgba: [255; 64].to_vec(),
      }),
    );
    let overlaid = Frame {
      vignette: Some((0.0, 1.0)),
      input: Some(((8, 8), 1.0)),
      ..plain
    };
    target.draw(&gpu, &overlaid, &mut frame, 256).unwrap();
    assert_eq!(at(&frame, 9, 9), [255, 255, 255]);
    assert!(at(&frame, 0, 0)[2] < 10, "the corner is dark");
    assert!(at(&frame, 16, 16)[2] > 150, "the middle is not");
    // A screenshot of another shape covers the target, cut evenly: the
    // middle half of four stripes on a square target.
    let stripes = gpu.image(&stripes(), None);
    let mut square = Target::new(&gpu, (32, 32), Some(&stripes));
    let mut small = vec![0; 256 * 32];
    square.draw(&gpu, &plain, &mut small, 256).unwrap();
    assert_eq!(at(&small, 4, 16), [0, 255, 0], "green, not red");
    assert_eq!(at(&small, 28, 16), [255, 0, 0], "blue, not white");
    // Without a screenshot the colour shows.
    let mut target = Target::new(&gpu, (64, 32), None);
    let coloured = Frame {
      color: [0x10, 0x20, 0x30, 0xff],
      ..plain
    };
    target.draw(&gpu, &coloured, &mut frame, 256).unwrap();
    assert_eq!(at(&frame, 5, 5), [0x30, 0x20, 0x10]);
  }

  #[test]
  fn large_blurs_run_on_a_smaller_copy() {
    assert_eq!(blur_plan(7, (1920, 1080)), (1, (1920, 1080), 7.0));
    assert_eq!(blur_plan(40, (1920, 1080)), (4, (480, 270), 10.0));
    assert_eq!(blur_plan(17, (5, 3)), (2, (3, 2), 8.5));
  }
}
