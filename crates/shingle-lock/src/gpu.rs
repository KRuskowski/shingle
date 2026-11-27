//! The lock screen on the GPU: one device for every output, the
//! screenshot of each blurred once, and per frame the slowly moving
//! background with its vignette and the clock and input overlays.
//!
//! Everything up to the last pass is linear light in 16-bit float
//! (`Rgba16Float`): the capture is read through an sRGB view, blurred,
//! dimmed, vignetted, lit and blended with the overlays there, so that
//! no gradient is ever rounded to 8 bits on the way. The present pass
//! encodes the scene as sRGB into the output's format (`Bgra8Unorm` for
//! `XRGB8888`, `Rgb10a2Unorm` for `XBGR2101010`) with about half a step
//! of interleaved-gradient-noise dither, shifted each frame while the
//! background moves, which turns what would be bands into fine grain.
//! The output is either a texture read back into a `wl_shm` buffer
//! ([`Target::draw`]) or a DMA-BUF the compositor shows as it is
//! ([`Target::draw_to`]).

use std::f32::consts::TAU;

use bytemuck::{Pod, Zeroable};
use shingle_config::DriftConfig;
use shingle_render::{GpuContext, GpuOptions};

use crate::{
  capture::Screenshot,
  damage::{History, Rect},
  ui::Raster,
};

/// The 8-bit output format, whose bytes are `XRGB8888`'s B, G, R, X.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// The 10-bit output format, whose words are `XBGR2101010`'s.
pub const FORMAT_10: wgpu::TextureFormat = wgpu::TextureFormat::Rgb10a2Unorm;
/// An sRGB view of an `XRGB8888` DMA-BUF, which the import may give.
const FORMAT_SRGB: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
/// The scene's format: linear light in half floats.
const SCENE: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The blur's radius in texels at most; larger ones blur a smaller copy.
const MAX_BLUR_TAPS: u32 = 16;
/// How much the two drifting lights brighten and darken, as a fraction,
/// at intensity 1.
const LIGHT: f32 = 0.14;
/// How far the flow bends the image at most, in texture coordinates, at
/// intensity 1: about 100 pixels on a 1920 pixel wide output.
const FLOW: f32 = 0.052;
/// The angular speeds of the flow's and the lights' waves, in radians a
/// second, at intensity 1: none a multiple of another, so that the
/// pattern never repeats visibly. The first six turn the flow (its
/// waves come round in 5 to 11 s, each bent by another), the next two
/// move the lights (in 37 to 48 s).
const SPEEDS: [f64; 8] = [0.87, 1.11, 0.69, 0.99, 1.23, 0.57, 0.17, 0.13];
/// The breathing zoom's angular speed.
const BREATHING: f64 = 0.11;

/// Where the background is at one moment of its movement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drift {
  /// The offset of the image, in texture coordinates.
  pub offset: [f32; 2],
  /// The image's zoom, above 1 so its edges never show.
  pub zoom: f32,
  /// The phases of the flow's and the lights' waves ([`SPEEDS`]), each
  /// computed from the time in double precision and wrapped to a turn,
  /// so that the motion is continuous however long the lock lasts.
  pub phases: [f32; 8],
  /// The drifting lights' strength.
  pub light: f32,
  /// The flow's strength, in texture coordinates.
  pub flow: f32,
}

impl Drift {
  /// No movement.
  pub const STILL: Self = Self {
    offset: [0.0, 0.0],
    zoom: 1.0,
    phases: [0.0; 8],
    light: 0.0,
    flow: 0.0,
  };

  /// The background `seconds` after locking on an output of `size`
  /// logical pixels, like a slowly flowing liquid under frosted glass: a
  /// flow of large, slow waves that bends the blurred image by up to
  /// [`FLOW`] times `config.intensity` of its size and makes its colour
  /// masses drift and morph, two broad lights wandering across, a slow
  /// Lissajous path of `config.amplitude` pixels, one cycle per
  /// `config.period`, and a breathing zoom of half a percent. Everything
  /// is in texture coordinates, so it moves alike at any resolution and
  /// scale, and smooth in time, so it can be drawn at any frame rate.
  /// Intensity 0 holds it still.
  pub fn at(seconds: f64, config: &DriftConfig, size: (u32, u32)) -> Self {
    if !config.enabled || config.intensity <= 0.0 {
      return Self::STILL;
    }
    let (width, height) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let amplitude = config.amplitude.max(0.0) as f32;
    let cycle = (seconds / config.period).fract() as f32 * TAU;
    let turn = |speed: f64| (seconds * speed).rem_euclid(TAU as f64) as f32;
    let breathing = 0.0025 * (1.0 + turn(BREATHING).sin());
    // Intensity scales both how far and how fast the flow moves, by its
    // square root each, so that the motion grows with it and the margin
    // the zoom must keep grows more slowly.
    let root = config.intensity.sqrt();
    let flow = FLOW * root as f32;
    // Room for the path, the breathing and the flow, so that no edge of
    // the image ever shows: half the view plus the flow stays inside.
    let zoom = (1.0 + 2.0 * amplitude / width.min(height) + breathing) * 0.5
      / (0.5 - flow);
    let mut phases = [0.0; 8];
    for (phase, speed) in phases.iter_mut().zip(SPEEDS) {
      *phase = turn(speed * root);
    }
    Self {
      offset: [
        amplitude * cycle.sin() / width / zoom,
        amplitude * (cycle * 2.0 + 1.1).sin() / height / zoom,
      ],
      zoom,
      phases,
      light: LIGHT * config.intensity as f32,
      flow,
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

/// The linear value of an sRGB byte.
fn linear(byte: u8) -> f32 {
  let value = f32::from(byte) / 255.0;
  if value <= 0.04045 {
    value / 12.92
  } else {
    ((value + 0.055) / 1.055).powf(2.4)
  }
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
  motion: [f32; 4],
  phases: [[f32; 4]; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct OverlayUniform {
  rect: [f32; 4],
  alpha: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PresentUniform {
  levels: [f32; 4],
}

/// The device and the pipelines every output draws with.
pub struct Gpu {
  context: GpuContext,
  layout: wgpu::BindGroupLayout,
  blur: wgpu::RenderPipeline,
  background: wgpu::RenderPipeline,
  overlay: wgpu::RenderPipeline,
  /// The present pass for [`FORMAT`], [`FORMAT_10`] and an sRGB view
  /// of a DMA-BUF.
  present: [wgpu::RenderPipeline; 3],
  sampler: wgpu::Sampler,
  /// A black pixel, for outputs without a screenshot.
  empty: (wgpu::Texture, wgpu::TextureView),
}

/// An output's screenshot on the GPU, linear, blurred if asked.
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

/// The output formats' levels per channel less one.
fn levels(format: wgpu::TextureFormat) -> f32 {
  if format == FORMAT_10 { 1023.0 } else { 255.0 }
}

impl Gpu {
  /// Opens the default adapter, for DMA-BUF import where it can, and
  /// builds the pipelines.
  ///
  /// # Errors
  ///
  /// When no adapter or device can be had.
  pub fn new() -> Result<Self, String> {
    let context = pollster::block_on(GpuContext::new(
      GpuContext::create_instance(),
      GpuOptions {
        dmabuf: true,
        ..GpuOptions::default()
      },
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
    let pipeline = |vertex: &str, fragment: &str, format, blend| {
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
            format,
            blend,
            write_mask: wgpu::ColorWrites::ALL,
          })],
        }),
        multiview_mask: None,
        cache: None,
      })
    };
    let blur = pipeline("vs_full", "fs_blur", SCENE, None);
    let background = pipeline("vs_full", "fs_background", SCENE, None);
    let overlay = pipeline(
      "vs_overlay",
      "fs_overlay",
      SCENE,
      Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
    );
    let present = [
      pipeline("vs_full", "fs_present", FORMAT, None),
      pipeline("vs_full", "fs_present", FORMAT_10, None),
      pipeline("vs_full", "fs_present", FORMAT_SRGB, None),
    ];
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
        SCENE,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
      )),
      context,
      layout,
      blur,
      background,
      overlay,
      present,
      sampler,
    };
    Ok(gpu)
  }

  /// The GPU context, for DMA-BUF import.
  pub fn context(&self) -> &GpuContext {
    &self.context
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
  /// passes) asks, in linear light and half floats.
  pub fn image(&self, shot: &Screenshot, blur: Option<(u32, u32)>) -> Image {
    let size = (shot.width.max(1), shot.height.max(1));
    // Its bytes are sRGB; read through an sRGB view, it is linear, and
    // filtered in linear light.
    let source = self.device().create_texture(&wgpu::TextureDescriptor {
      label: Some("shingle-lock capture"),
      size: wgpu::Extent3d {
        width: size.0,
        height: size.1,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: FORMAT,
      usage: wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_DST,
      view_formats: &[wgpu::TextureFormat::Bgra8UnormSrgb],
    });
    write(self.queue(), &source, size, &shot.bgra);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor {
      format: Some(wgpu::TextureFormat::Bgra8UnormSrgb),
      ..Default::default()
    });
    let Some((radius, times)) =
      blur.filter(|(radius, times)| *radius > 0 && *times > 0)
    else {
      return Image {
        _texture: source,
        view: source_view,
        size,
      };
    };
    let usage = wgpu::TextureUsages::TEXTURE_BINDING
      | wgpu::TextureUsages::RENDER_ATTACHMENT;
    let (divisor, scaled, radius) = blur_plan(radius, size);
    let (ping_texture, ping) =
      with_view(texture(self.device(), scaled, SCENE, usage));
    let (_pong_texture, pong) =
      with_view(texture(self.device(), scaled, SCENE, usage));
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
      &self.bind(&source_view, &copy),
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

/// A scene texture of `size`: drawn into, then read by the present pass.
fn texture_for_scene(gpu: &Gpu, size: (u32, u32)) -> wgpu::Texture {
  texture(
    gpu.device(),
    size,
    SCENE,
    wgpu::TextureUsages::RENDER_ATTACHMENT
      | wgpu::TextureUsages::TEXTURE_BINDING,
  )
}

/// An overlay's texture and where it goes.
struct Overlay {
  texture: wgpu::Texture,
  size: (u32, u32),
  uniform: wgpu::Buffer,
  bind: wgpu::BindGroup,
}

/// Where an overlay goes in one frame and how it looks there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
  /// Its top-left corner at full size, in pixels; fractions move it
  /// smoothly between pixels.
  pub at: (f32, f32),
  /// Its size, scaled about its centre.
  pub scale: f32,
  /// Its opacity.
  pub alpha: f32,
}

impl Placement {
  /// At `at`, full size, with opacity `alpha`.
  pub fn at((x, y): (i32, i32), alpha: f32) -> Self {
    Self {
      at: (x as f32, y as f32),
      scale: 1.0,
      alpha,
    }
  }

  /// The box it covers for an image of `size`: top-left and
  /// bottom-right corners.
  fn corners(&self, size: (u32, u32)) -> ((f32, f32), (f32, f32)) {
    let (width, height) = (size.0 as f32, size.1 as f32);
    let centre = (self.at.0 + width / 2.0, self.at.1 + height / 2.0);
    let half = (width * self.scale / 2.0, height * self.scale / 2.0);
    (
      (centre.0 - half.0, centre.1 - half.1),
      (centre.0 + half.0, centre.1 + half.1),
    )
  }
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
  /// How bright the screenshot is drawn, 1 as it was.
  pub dim: f32,
  /// The dither's seed: changed each frame while the background
  /// moves, so that its grain moves too, and kept while it stands, so
  /// that frames which change only an overlay match elsewhere.
  pub seed: u32,
  /// The clock, if it shows.
  pub clock: Option<Placement>,
  /// The input, if it shows.
  pub input: Option<Placement>,
}

impl Frame {
  /// Whether `self` and `other` draw the same background.
  fn same_background(&self, other: &Self) -> bool {
    self.drift == other.drift
      && self.vignette == other.vignette
      && self.color == other.color
      && self.dim == other.dim
      && self.seed == other.seed
  }
}

/// One output's target, its background and overlays, and the buffer its
/// frames are read back through.
pub struct Target {
  size: (u32, u32),
  /// The scene, linear light in half floats.
  scene: wgpu::TextureView,
  _scene_texture: wgpu::Texture,
  /// The present pass's uniform and its binding of the scene.
  present_uniform: wgpu::Buffer,
  present: wgpu::BindGroup,
  /// The output read back into `wl_shm` buffers, in `format`.
  view: wgpu::TextureView,
  texture: wgpu::Texture,
  format: wgpu::TextureFormat,
  readback: wgpu::Buffer,
  background_uniform: wgpu::Buffer,
  background: wgpu::BindGroup,
  has_image: bool,
  /// [`cover_scale`] of the image on this target.
  cover: [f32; 2],
  clock: Option<Overlay>,
  input: Option<Overlay>,
  /// The last frame drawn and the overlays' boxes in it.
  last: Option<(Frame, Rect)>,
  /// Set when an overlay's image changed since the last frame.
  overlays_changed: Rect,
  history: History,
}

/// A frame drawn into a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drawn {
  /// Its number, which the buffer now holds.
  pub serial: u64,
  /// What changed from the frame before, for `wl_surface.damage_buffer`.
  pub damage: Rect,
  /// What was read back into the buffer.
  pub copied: Rect,
}

impl Target {
  /// A target of `size` logical pixels over `image`, scaled to cover it
  /// with its aspect ratio kept ([`cover_scale`]), or over the plain
  /// background colour without one, whose frames read back in `format`
  /// ([`FORMAT`] or [`FORMAT_10`]).
  pub fn new(
    gpu: &Gpu,
    size: (u32, u32),
    image: Option<&Image>,
    format: wgpu::TextureFormat,
  ) -> Self {
    let size = (size.0.max(1), size.1.max(1));
    let texture = texture(
      gpu.device(),
      size,
      format,
      wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let (scene_texture, scene) = with_view(texture_for_scene(gpu, size));
    let present_uniform = gpu.uniform(&PresentUniform::zeroed());
    let present = gpu.bind(&scene, &present_uniform);
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
      scene,
      _scene_texture: scene_texture,
      present_uniform,
      present,
      view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
      texture,
      format,
      readback,
      background_uniform,
      background,
      has_image: image.is_some(),
      cover: image.map_or([1.0, 1.0], |image| cover_scale(image.size, size)),
      clock: None,
      input: None,
      last: None,
      overlays_changed: Rect::default(),
      history: History::default(),
    }
  }

  /// The target's size.
  pub fn size(&self) -> (u32, u32) {
    self.size
  }

  /// The format its frames are read back in.
  pub fn format(&self) -> wgpu::TextureFormat {
    self.format
  }

  /// Puts `raster` into `slot`, reusing its texture when the size is the
  /// same.
  fn set_overlay(gpu: &Gpu, slot: &mut Option<Overlay>, raster: &Raster) {
    let size = (raster.width, raster.height);
    if let Some(overlay) = slot.as_ref().filter(|overlay| overlay.size == size)
    {
      write(gpu.queue(), &overlay.texture, size, &raster.rgba);
      return;
    }
    let texture = texture(
      gpu.device(),
      size,
      wgpu::TextureFormat::Rgba8Unorm,
      wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    write(gpu.queue(), &texture, size, &raster.rgba);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let uniform = gpu.uniform(&OverlayUniform::zeroed());
    let bind = gpu.bind(&view, &uniform);
    *slot = Some(Overlay {
      texture,
      size,
      uniform,
      bind,
    });
  }

  /// The boxes of the overlays `frame` places, together.
  fn overlay_rect(&self, frame: &Frame) -> Rect {
    [(&self.clock, frame.clock), (&self.input, frame.input)]
      .into_iter()
      .filter_map(|(overlay, placement)| {
        let (overlay, placement) = (overlay.as_ref()?, placement?);
        let (from, to) = placement.corners(overlay.size);
        Some(Rect::around(from, to, self.size))
      })
      .fold(Rect::default(), Rect::union)
  }

  /// Replaces the clock's image; it is redrawn where the last frame
  /// placed it.
  pub fn set_clock(&mut self, gpu: &Gpu, raster: Option<&Raster>) {
    self.note_overlay_change();
    match raster {
      Some(raster) => Self::set_overlay(gpu, &mut self.clock, raster),
      None => self.clock = None,
    }
  }

  /// Replaces the input's image.
  pub fn set_input(&mut self, gpu: &Gpu, raster: Option<&Raster>) {
    self.note_overlay_change();
    match raster {
      Some(raster) => Self::set_overlay(gpu, &mut self.input, raster),
      None => self.input = None,
    }
  }

  fn note_overlay_change(&mut self) {
    if let Some((_, rect)) = self.last {
      self.overlays_changed = self.overlays_changed.union(rect);
    }
  }

  /// The clip-space rectangle of `placement` for an image of `size`.
  fn clip_rect(&self, placement: &Placement, size: (u32, u32)) -> [f32; 4] {
    let (width, height) = (self.size.0 as f32, self.size.1 as f32);
    let ((x0, y0), (x1, y1)) = placement.corners(size);
    [
      x0 / width * 2.0 - 1.0,
      1.0 - y0 / height * 2.0,
      x1 / width * 2.0 - 1.0,
      1.0 - y1 / height * 2.0,
    ]
  }

  /// What changed between the last frame and `frame`, whose overlays
  /// cover `overlays`: everything when the background did.
  fn damage(&self, frame: &Frame, overlays: Rect) -> Rect {
    match &self.last {
      Some((last, last_overlays)) if last.same_background(frame) => {
        overlays.union(*last_overlays).union(self.overlays_changed)
      }
      _ => Rect::full(self.size),
    }
  }

  /// Records `frame` into `encoder`: the scene, then the present pass
  /// into `output`, of `format`.
  fn encode(
    &mut self,
    gpu: &Gpu,
    frame: &Frame,
    encoder: &mut wgpu::CommandEncoder,
    output: &wgpu::TextureView,
    format: wgpu::TextureFormat,
  ) {
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
        frame.drift.light,
      ],
      color: [
        linear(frame.color[0]),
        linear(frame.color[1]),
        linear(frame.color[2]),
        frame.dim,
      ],
      motion: [0.0, self.cover[0], self.cover[1], frame.drift.flow],
      phases: [
        [
          frame.drift.phases[0],
          frame.drift.phases[1],
          frame.drift.phases[2],
          frame.drift.phases[3],
        ],
        [
          frame.drift.phases[4],
          frame.drift.phases[5],
          frame.drift.phases[6],
          frame.drift.phases[7],
        ],
      ],
    };
    gpu.queue().write_buffer(
      &self.background_uniform,
      0,
      bytemuck::bytes_of(&uniform),
    );
    gpu.pass(
      encoder,
      &self.scene,
      &gpu.background,
      &self.background,
      3,
      true,
    );
    for (overlay, placement) in
      [(&self.clock, frame.clock), (&self.input, frame.input)]
    {
      let (Some(overlay), Some(placement)) = (overlay, placement) else {
        continue;
      };
      if placement.alpha <= 0.0 {
        continue;
      }
      let uniform = OverlayUniform {
        rect: self.clip_rect(&placement, overlay.size),
        alpha: [placement.alpha.min(1.0), 0.0, 0.0, 0.0],
      };
      gpu.queue().write_buffer(
        &overlay.uniform,
        0,
        bytemuck::bytes_of(&uniform),
      );
      gpu.pass(encoder, &self.scene, &gpu.overlay, &overlay.bind, 6, false);
    }
    let srgb = format == FORMAT_SRGB;
    let present = PresentUniform {
      levels: [
        levels(format),
        (frame.seed % 64) as f32,
        1.0,
        if srgb { 1.0 } else { 0.0 },
      ],
    };
    gpu.queue().write_buffer(
      &self.present_uniform,
      0,
      bytemuck::bytes_of(&present),
    );
    let pipeline = match format {
      FORMAT_10 => &gpu.present[1],
      FORMAT_SRGB => &gpu.present[2],
      _ => &gpu.present[0],
    };
    gpu.pass(encoder, output, pipeline, &self.present, 3, true);
  }

  /// Notes `frame` as drawn and returns its serial and its damage.
  fn record(&mut self, frame: &Frame) -> (u64, Rect) {
    let overlays = self.overlay_rect(frame);
    let damage = self.damage(frame, overlays);
    let serial = self.history.push(damage);
    self.last = Some((*frame, overlays));
    self.overlays_changed = Rect::default();
    (serial, damage)
  }

  /// Draws `frame` straight into `output`, a DMA-BUF of `format` the
  /// compositor shows, and waits for the GPU to finish it, so that the
  /// buffer is complete when it is committed. Returns the serial and the
  /// damage, as [`Target::draw`] does.
  ///
  /// # Errors
  ///
  /// When the GPU fails or does not finish in 2 s.
  pub fn draw_to(
    &mut self,
    gpu: &Gpu,
    frame: &Frame,
    output: &shingle_render::dmabuf::ImportedTexture,
  ) -> Result<Drawn, String> {
    let (serial, damage) = self.record(frame);
    let texture = output.texture();
    let format = texture.format();
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu
      .device()
      .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    self.encode(gpu, frame, &mut encoder, &view, format);
    // The buffer is handed to the compositor's queue family after it.
    let mut access = shingle_render::dmabuf::ForeignAccess::new();
    access.add(output.foreign());
    let index = access.submit(gpu.context(), encoder.finish());
    gpu
      .device()
      .poll(wgpu::PollType::Wait {
        submission_index: Some(index),
        timeout: Some(std::time::Duration::from_secs(2)),
      })
      .map_err(|error| error.to_string())?;
    Ok(Drawn {
      serial,
      damage,
      copied: Rect::default(),
    })
  }

  /// Draws `frame` and reads it back into `out`, rows `stride` bytes
  /// apart, as `XRGB8888` (or `XBGR2101010` for [`FORMAT_10`]). `holds`
  /// is the frame `out` holds from an earlier draw, if any: only what
  /// changed since is read back.
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
    holds: Option<u64>,
  ) -> Result<Drawn, String> {
    let (width, height) = self.size;
    let row = width as usize * 4;
    if stride < row || out.len() < stride * (height as usize - 1) + row {
      return Err("the buffer does not fit the frame".into());
    }
    let (serial, damage) = self.record(frame);
    let copied = self
      .history
      .since(holds)
      .unwrap_or_else(|| Rect::full(self.size));
    let mut encoder = gpu
      .device()
      .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let view = self.view.clone();
    self.encode(gpu, frame, &mut encoder, &view, self.format);
    if copied.is_empty() {
      gpu.queue().submit([encoder.finish()]);
      return Ok(Drawn {
        serial,
        damage,
        copied,
      });
    }
    // The part read back starts at the buffer's beginning, its rows
    // padded to wgpu's alignment.
    let padded = (copied.width * 4).div_ceil(256) * 256;
    encoder.copy_texture_to_buffer(
      wgpu::TexelCopyTextureInfo {
        texture: &self.texture,
        mip_level: 0,
        origin: wgpu::Origin3d {
          x: copied.x,
          y: copied.y,
          z: 0,
        },
        aspect: wgpu::TextureAspect::All,
      },
      wgpu::TexelCopyBufferInfo {
        buffer: &self.readback,
        layout: wgpu::TexelCopyBufferLayout {
          offset: 0,
          bytes_per_row: Some(padded),
          rows_per_image: Some(copied.height),
        },
      },
      wgpu::Extent3d {
        width: copied.width,
        height: copied.height,
        depth_or_array_layers: 1,
      },
    );
    gpu.queue().submit([encoder.finish()]);
    let length = u64::from(padded) * u64::from(copied.height);
    let (sender, receiver) = std::sync::mpsc::channel();
    let slice = self.readback.slice(..length);
    slice.map_async(wgpu::MapMode::Read, move |result| {
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
      let mapped = slice
        .get_mapped_range()
        .map_err(|error| error.to_string())?;
      let (x, part) = (copied.x as usize * 4, copied.width as usize * 4);
      if copied.x == 0 && part == stride && padded as usize == stride {
        // Whole rows, laid out as the buffer's: one copy.
        let start = copied.y as usize * stride;
        out[start..start + mapped.len()].copy_from_slice(&mapped);
      } else {
        for (index, source) in mapped
          .chunks_exact(padded as usize)
          .take(copied.height as usize)
          .enumerate()
        {
          let start = (copied.y as usize + index) * stride + x;
          out[start..start + part].copy_from_slice(&source[..part]);
        }
      }
    }
    self.readback.unmap();
    // Every pass writes an alpha of 1: the passes are opaque and the
    // overlays blend premultiplied over them, so XRGB8888's X byte is
    // 0xff without touching it.
    Ok(Drawn {
      serial,
      damage,
      copied,
    })
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
      // offset and the flow, stays inside the image.
      let half = 0.5 / drift.zoom;
      for (axis, offset) in drift.offset.iter().enumerate() {
        assert!(
          half + offset.abs() + drift.flow <= 0.5 + 1e-4,
          "{axis} at {tenth}"
        );
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
    // The path comes round after a period, in pixels.
    let (x30, y30) = offset(Drift::at(30.0, &config, size));
    assert!((x30 - x0).abs() < 1e-3 && (y30 - y0).abs() < 1e-3);
  }

  #[test]
  fn the_flow_follows_its_intensity_and_never_jumps() {
    let config = DriftConfig::default();
    let size = (3840, 2160);
    let at = |seconds: f64, intensity: f64| {
      Drift::at(
        seconds,
        &DriftConfig {
          intensity,
          ..config.clone()
        },
        size,
      )
    };
    assert_eq!(at(10.0, 0.0), Drift::STILL, "intensity 0 holds it still");
    assert_eq!(at(10.0, 1.0).flow, FLOW);
    assert!((at(10.0, 4.0).flow - 2.0 * FLOW).abs() < 1e-6);
    assert!(at(10.0, 3.0).zoom > at(10.0, 1.0).zoom);
    // From one frame to the next at 60 Hz every phase moves a little,
    // also a day and a year into the lock, where a time in f32 would
    // have lost its milliseconds.
    for start in [0.0, 86_400.0, 31_536_000.5] {
      let (now, next) = (at(start, 1.0), at(start + 1.0 / 60.0, 1.0));
      for (a, b) in now.phases.iter().zip(next.phases) {
        let step = (b - a).rem_euclid(TAU);
        let step = step.min(TAU - step);
        assert!(step > 1e-3 && step < 0.03, "{step} at {start}");
      }
    }
    // The same in texture coordinates whatever the output's size.
    let small = Drift::at(10.0, &config, (1280, 720));
    assert_eq!(small.phases, at(10.0, 1.0).phases);
    assert_eq!(small.flow, at(10.0, 1.0).flow);
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
      dim: 1.0,
      seed: 0,
      clock: None,
      input: None,
    };
    let mut target = Target::new(&gpu, (64, 32), Some(&sharp), FORMAT);
    target.draw(&gpu, &plain, &mut frame, 256, None).unwrap();
    assert_eq!(at(&frame, 30, 16), [0, 0, 255]);
    assert_eq!(at(&frame, 33, 16), [0, 255, 0]);
    assert!(frame.chunks_exact(4).all(|pixel| pixel[3] == 255));
    let mut target = Target::new(&gpu, (64, 32), Some(&blurred), FORMAT);
    target.draw(&gpu, &plain, &mut frame, 256, None).unwrap();
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
      input: Some(Placement::at((8, 8), 1.0)),
      ..plain
    };
    target.draw(&gpu, &overlaid, &mut frame, 256, None).unwrap();
    assert_eq!(at(&frame, 9, 9), [255, 255, 255]);
    assert!(at(&frame, 0, 0)[2] < 10, "the corner is dark");
    assert!(at(&frame, 16, 16)[2] > 150, "the middle is not");
    // A screenshot of another shape covers the target, cut evenly: the
    // middle half of four stripes on a square target.
    let stripes = gpu.image(&stripes(), None);
    let mut square = Target::new(&gpu, (32, 32), Some(&stripes), FORMAT);
    let mut small = vec![0; 256 * 32];
    square.draw(&gpu, &plain, &mut small, 256, None).unwrap();
    assert_eq!(at(&small, 4, 16), [0, 255, 0], "green, not red");
    assert_eq!(at(&small, 28, 16), [255, 0, 0], "blue, not white");
    // Without a screenshot the colour shows.
    let mut target = Target::new(&gpu, (64, 32), None, FORMAT);
    let coloured = Frame {
      color: [0x10, 0x20, 0x30, 0xff],
      ..plain
    };
    target.draw(&gpu, &coloured, &mut frame, 256, None).unwrap();
    // The colour, within the dither's step.
    let [b, g, r] = at(&frame, 5, 5);
    assert!(b.abs_diff(0x30) <= 1 && g.abs_diff(0x20) <= 1, "{b} {g}");
    assert!(r.abs_diff(0x10) <= 1, "{r}");
    // Dimmed, the screenshot is darker; the colour is not.
    let mut dimmed = Target::new(&gpu, (64, 32), Some(&sharp), FORMAT);
    let dark = Frame { dim: 0.5, ..plain };
    dimmed.draw(&gpu, &dark, &mut frame, 256, None).unwrap();
    // Half the light: 188 in sRGB.
    let red = at(&frame, 2, 16)[2];
    assert!((184..=192).contains(&red), "{red}");
  }

  /// A 4x4 white square in a transparent pixel's border, as the
  /// overlays have, so that they move smoothly between pixels.
  fn square() -> Raster {
    let mut rgba = vec![0; 6 * 6 * 4];
    for y in 1..5 {
      for x in 1..5 {
        rgba[(y * 6 + x) * 4..(y * 6 + x) * 4 + 4].fill(255);
      }
    }
    Raster {
      width: 6,
      height: 6,
      rgba,
    }
  }

  #[test]
  fn a_frame_that_only_moves_an_overlay_reads_back_only_that() {
    let Ok(gpu) = Gpu::new() else {
      eprintln!("no GPU; skipped");
      return;
    };
    let image = gpu.image(&halves(), None);
    let mut target = Target::new(&gpu, (64, 32), Some(&image), FORMAT);
    target.set_input(&gpu, Some(&square()));
    let frame = Frame {
      drift: Drift::STILL,
      vignette: None,
      color: [0, 0, 0, 255],
      dim: 1.0,
      seed: 0,
      clock: None,
      input: Some(Placement::at((4, 4), 1.0)),
    };
    // Two buffers, as an output alternates them.
    let mut buffers = [vec![0_u8; 256 * 32], vec![0_u8; 256 * 32]];
    let first = target
      .draw(&gpu, &frame, &mut buffers[0], 256, None)
      .unwrap();
    assert_eq!(first.copied, Rect::full((64, 32)));
    let second = target
      .draw(&gpu, &frame, &mut buffers[1], 256, None)
      .unwrap();
    // Nothing moved: only the input's box.
    assert_eq!(
      second.damage,
      Rect {
        x: 4,
        y: 4,
        width: 6,
        height: 6,
      }
    );
    // The input moves by 10.5 pixels: the old buffer gets both frames'
    // boxes, the new place smoothly between pixels.
    let moved = Frame {
      input: Some(Placement {
        at: (14.5, 4.0),
        scale: 1.0,
        alpha: 1.0,
      }),
      ..frame
    };
    let third = target
      .draw(&gpu, &moved, &mut buffers[0], 256, Some(first.serial))
      .unwrap();
    let expected = Rect {
      x: 4,
      y: 4,
      width: 17,
      height: 6,
    };
    assert_eq!(third.damage, expected);
    assert_eq!(third.copied, expected);
    // The old place is red again, the new one white, half covered at
    // its edges.
    assert_eq!(at(&buffers[0], 6, 6), [0, 0, 255]);
    assert_eq!(at(&buffers[0], 17, 6), [255, 255, 255]);
    // Half covered, blended in linear light: about 188.
    let edge = at(&buffers[0], 15, 6);
    assert!(edge[0] > 170 && edge[0] < 205, "{edge:?}");
    // Outside the box the buffer kept what it had.
    assert_eq!(at(&buffers[0], 40, 20), [0, 255, 0]);
    // A drift step changes the background: all of it.
    let drifted = Frame {
      drift: Drift {
        offset: [0.01, 0.0],
        ..Drift::STILL
      },
      seed: 1,
      ..moved
    };
    let fourth = target
      .draw(&gpu, &drifted, &mut buffers[1], 256, Some(second.serial))
      .unwrap();
    assert_eq!(fourth.copied, Rect::full((64, 32)));
    // A new image for the input redraws its box though nothing moved.
    target.set_input(&gpu, Some(&square()));
    let fifth = target
      .draw(&gpu, &drifted, &mut buffers[0], 256, Some(fourth.serial))
      .unwrap();
    assert_eq!(
      fifth.damage,
      Rect::around((14.5, 4.0), (20.5, 10.0), (64, 32))
    );
    // A scaled overlay covers its scaled box about its centre.
    let scaled = Placement {
      at: (8.0, 8.0),
      scale: 0.5,
      alpha: 1.0,
    };
    assert_eq!(scaled.corners((4, 4)), ((9.0, 9.0), (11.0, 11.0)));
  }

  /// The cost of one frame, the GPU's drawing and the read-back into
  /// memory, at 1080p and 4K: `cargo test --release -p shingle-lock --
  /// --ignored frame_cost --nocapture` (docs/design/lock-screen.md).
  #[test]
  #[ignore = "a measurement, run by hand"]
  fn frame_cost() {
    let Ok(gpu) = Gpu::new() else {
      eprintln!("no GPU; skipped");
      return;
    };
    for size in [(1920_u32, 1080_u32), (3840, 2160)] {
      let shot = Screenshot {
        name: None,
        width: size.0,
        height: size.1,
        bgra: (0..size.0 * size.1 * 4).map(|i| (i % 251) as u8).collect(),
      };
      let image = gpu.image(&shot, Some((24, 3)));
      let mut target = Target::new(&gpu, size, Some(&image), FORMAT);
      target.set_input(
        &gpu,
        Some(&Raster {
          width: 320,
          height: 80,
          rgba: vec![128; 320 * 80 * 4],
        }),
      );
      let stride = size.0 as usize * 4;
      let mut out = vec![0_u8; stride * size.1 as usize];
      let config = DriftConfig::default();
      let mut times = Vec::new();
      for index in 0..120 {
        let frame = Frame {
          drift: Drift::at(index as f64 / 60.0, &config, size),
          vignette: Some((0.5, 0.5)),
          color: [0, 0, 0, 255],
          dim: 0.8,
          seed: index as u32,
          clock: None,
          input: Some(Placement::at((800, 500 + index % 20), 0.5)),
        };
        let start = std::time::Instant::now();
        target.draw(&gpu, &frame, &mut out, stride, None).unwrap();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
      }
      report("full", size, &mut times);
      // The background still between drift steps: only the input's box
      // changes, in the buffer of two frames before.
      let still = Drift::at(0.5, &config, size);
      let mut buffers = [out.clone(), out];
      let mut serials = [None, None];
      for index in 0..120 {
        let frame = Frame {
          drift: still,
          vignette: Some((0.5, 0.5)),
          color: [0, 0, 0, 255],
          dim: 0.8,
          seed: 0,
          clock: None,
          input: Some(Placement {
            at: (800.0 + index as f32 * 0.7, 500.0),
            scale: 1.0,
            alpha: 0.5,
          }),
        };
        let buffer = index % 2;
        let start = std::time::Instant::now();
        let drawn = target
          .draw(&gpu, &frame, &mut buffers[buffer], stride, serials[buffer])
          .unwrap();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        serials[buffer] = Some(drawn.serial);
      }
      report("overlay only", size, &mut times);
    }
  }

  /// Prints the mean, median, 95th percentile and maximum of `times`
  /// after the first ten, and empties it.
  fn report(what: &str, size: (u32, u32), times: &mut Vec<f64>) {
    times.drain(..10);
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    eprintln!(
      "frame cost {}x{} {what}: mean {mean:.2} ms, median {:.2} ms, \
       p95 {:.2} ms, max {:.2} ms",
      size.0,
      size.1,
      times[times.len() / 2],
      times[times.len() * 95 / 100],
      times[times.len() - 1],
    );
    times.clear();
  }

  /// An IEEE half float's value.
  fn half(bits: u16) -> f32 {
    let sign = if bits >> 15 == 1 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let fraction = f32::from(bits & 0x3ff) / 1024.0;
    sign
      * match exponent {
        0 => fraction * 2.0_f32.powi(-14),
        _ => (1.0 + fraction) * 2.0_f32.powi(exponent - 15),
      }
  }

  #[test]
  fn frames_drawn_into_a_dmabuf_hold_the_frame() {
    use shingle_render::dmabuf::{DmabufDescriptor, DmabufUse};
    use std::os::fd::AsFd;
    let Ok(gpu) = Gpu::new() else {
      eprintln!("no GPU; skipped");
      return;
    };
    let allocator = match crate::zero_copy::Allocator::for_gpu(&gpu) {
      Ok(allocator) => allocator,
      Err(reason) => {
        eprintln!("no DMA-BUFs here ({reason}); skipped");
        return;
      }
    };
    let (allocation, mut target, frame) =
      crate::zero_copy::tests::dmabuf_frame(&gpu, &allocator, (64, 32));
    let drawn = target
      .draw_to(&gpu, &frame, &allocation.texture)
      .expect("the frame is drawn");
    assert_eq!(drawn.copied, Rect::default(), "nothing is read back");
    // The same buffer imported again to sample, as the compositor does,
    // and copied into a texture this test can read.
    let sampled = gpu
      .context()
      .import_dmabuf(
        &DmabufDescriptor {
          width: 64,
          height: 32,
          fourcc: crate::zero_copy::XRGB8888,
          modifier: allocation.modifier,
          fd: allocation.fd.as_fd(),
          offset: allocation.offset,
          stride: allocation.stride,
        },
        DmabufUse::Sample(shingle_render::TextureFilter::Linear),
      )
      .expect("the buffer imports to sample");
    let srgb = sampled.texture().format().is_srgb();
    let view = sampled
      .texture()
      .create_view(&wgpu::TextureViewDescriptor::default());
    let (copy, copy_view) = with_view(texture(
      gpu.device(),
      (64, 32),
      SCENE,
      wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    ));
    let uniform = gpu.uniform(&BlurUniform::zeroed());
    let mut encoder = gpu
      .device()
      .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    gpu.pass(
      &mut encoder,
      &copy_view,
      &gpu.blur,
      &gpu.bind(&view, &uniform),
      3,
      true,
    );
    let readback = gpu.device().create_buffer(&wgpu::BufferDescriptor {
      label: None,
      size: 512 * 32,
      usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
      mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
      copy.as_image_copy(),
      wgpu::TexelCopyBufferInfo {
        buffer: &readback,
        layout: wgpu::TexelCopyBufferLayout {
          offset: 0,
          bytes_per_row: Some(512),
          rows_per_image: Some(32),
        },
      },
      wgpu::Extent3d {
        width: 64,
        height: 32,
        depth_or_array_layers: 1,
      },
    );
    let mut access = shingle_render::dmabuf::ForeignAccess::new();
    access.add(sampled.foreign());
    access.submit(gpu.context(), encoder.finish());
    readback.map_async(wgpu::MapMode::Read, .., |_| {});
    gpu
      .device()
      .poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(std::time::Duration::from_secs(2)),
      })
      .unwrap();
    let mapped = readback.get_mapped_range(..).unwrap();
    // R, G, B as sRGB bytes, whichever way the import reads them.
    let at = |x: usize, y: usize| -> [u8; 3] {
      let offset = y * 512 + x * 8;
      let channel = |index: usize| {
        let value = half(u16::from_le_bytes([
          mapped[offset + index * 2],
          mapped[offset + index * 2 + 1],
        ]));
        let encoded = if srgb {
          if value <= 0.0031308 {
            value * 12.92
          } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
          }
        } else {
          value
        };
        (encoded * 255.0).round() as u8
      };
      [channel(0), channel(1), channel(2)]
    };
    let colour = at(2, 2);
    for (got, want) in colour.iter().zip([0x20_u8, 0x40, 0x80]) {
      assert!(got.abs_diff(want) <= 1, "{colour:?}");
    }
    assert_eq!(at(9, 9), [255, 255, 255]);
  }

  #[test]
  fn large_blurs_run_on_a_smaller_copy() {
    assert_eq!(blur_plan(7, (1920, 1080)), (1, (1920, 1080), 7.0));
    assert_eq!(blur_plan(40, (1920, 1080)), (4, (480, 270), 10.0));
    assert_eq!(blur_plan(17, (5, 3)), (2, (3, 2), 8.5));
  }
}
