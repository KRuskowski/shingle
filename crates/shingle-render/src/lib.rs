use std::{
    mem::size_of,
    sync::Arc,
    time::{Duration, Instant},
};

use bytemuck::{Pod, Zeroable};
use thiserror::Error;
use wgpu::util::DeviceExt;
use winit::{dpi::PhysicalSize, window::Window};

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("failed to create GPU surface")]
    CreateSurface(#[from] wgpu::CreateSurfaceError),
    #[error("no compatible GPU adapter was found")]
    RequestAdapter(#[from] wgpu::RequestAdapterError),
    #[error("failed to create GPU device")]
    RequestDevice(#[from] wgpu::RequestDeviceError),
    #[error("GPU surface was lost")]
    SurfaceLost,
    #[error("GPU surface validation failed")]
    SurfaceValidation,
    #[error("texture dimensions must be nonzero")]
    EmptyTexture,
    #[error("RGBA buffer has {actual} bytes; expected {expected}")]
    InvalidTextureData { expected: usize, actual: usize },
    #[error("gamma ramp must contain 768 16-bit values; got {0}")]
    InvalidGammaRamp(usize),
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewDirection {
    Left,
    Right,
    Up,
    Down,
}

pub fn split_preview(
    rect: RenderRect,
    direction: PreviewDirection,
) -> RenderRect {
    match direction {
        PreviewDirection::Left => RenderRect {
            width: rect.width / 2,
            ..rect
        },
        PreviewDirection::Right => RenderRect {
            x: rect.x + (rect.width / 2) as i32,
            width: rect.width - rect.width / 2,
            ..rect
        },
        PreviewDirection::Up => RenderRect {
            height: rect.height / 2,
            ..rect
        },
        PreviewDirection::Down => RenderRect {
            y: rect.y + (rect.height / 2) as i32,
            height: rect.height - rect.height / 2,
            ..rect
        },
    }
}

pub struct GpuTexture {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl GpuTexture {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

struct GammaLut {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    ramp: Vec<u16>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct EffectUniform {
    inverted: u32,
    preserve_hue: u32,
}

struct Effects {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    inverted: bool,
    preserve_hue: bool,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextureTransform {
    #[default]
    Normal,
    FlipHorizontal,
    Rotate90,
    FlipBoth,
    Rotate270,
    Transpose,
    FlipVertical,
    AntiTranspose,
}

pub struct RenderTile<'a> {
    pub geometry: RenderRect,
    pub color: [f32; 4],
    pub texture: Option<&'a GpuTexture>,
    pub uv: [f32; 4],
    pub transform: TextureTransform,
    /// Counterclockwise rotation around the tile center, in radians.
    pub rotation: f32,
}

impl<'a> RenderTile<'a> {
    pub fn solid(geometry: RenderRect, color: [f32; 4]) -> Self {
        Self {
            geometry,
            color,
            texture: None,
            uv: [0.0, 0.0, 1.0, 1.0],
            transform: TextureTransform::Normal,
            rotation: 0.0,
        }
    }

    pub fn textured(geometry: RenderRect, texture: &'a GpuTexture) -> Self {
        Self {
            geometry,
            color: [1.0; 4],
            texture: Some(texture),
            uv: [0.0, 0.0, 1.0, 1.0],
            transform: TextureTransform::Normal,
            rotation: 0.0,
        }
    }
}

pub fn animation_progress(
    started: Instant,
    now: Instant,
    duration: Duration,
) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    (now.saturating_duration_since(started).as_secs_f64()
        / duration.as_secs_f64())
    .min(1.0) as f32
}

pub fn interpolate_zoom(start: f32, target: f32, progress: f32) -> f32 {
    start + (target - start) * progress.clamp(0.0, 1.0)
}

pub fn preview_tiles(
    rect: RenderRect,
    base_color: [f32; 4],
    border_color: [f32; 4],
    border_width: u32,
) -> Vec<RenderTile<'static>> {
    let mut tiles = vec![RenderTile::solid(rect, base_color)];
    let border = border_width.min(rect.width).min(rect.height);
    if border == 0 {
        return tiles;
    }
    let bottom =
        i32::try_from(rect.height.saturating_sub(border)).unwrap_or(i32::MAX);
    let right =
        i32::try_from(rect.width.saturating_sub(border)).unwrap_or(i32::MAX);
    let border_offset = i32::try_from(border).unwrap_or(i32::MAX);
    let inner_height = rect.height.saturating_sub(border.saturating_mul(2));
    for geometry in [
        RenderRect {
            height: border,
            ..rect
        },
        RenderRect {
            y: rect.y.saturating_add(bottom),
            height: border,
            ..rect
        },
        RenderRect {
            y: rect.y.saturating_add(border_offset),
            width: border,
            height: inner_height,
            ..rect
        },
        RenderRect {
            x: rect.x.saturating_add(right),
            y: rect.y.saturating_add(border_offset),
            width: border,
            height: inner_height,
        },
    ] {
        if geometry.width > 0 && geometry.height > 0 {
            tiles.push(RenderTile::solid(geometry, border_color));
        }
    }
    tiles
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    const LAYOUT: wgpu::VertexBufferLayout<'static> =
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2,
                1 => Float32x2,
                2 => Float32x4,
            ],
        };
}

#[derive(Clone, Copy)]
struct Zoom {
    factor: f32,
    origin_x: f32,
    origin_y: f32,
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            factor: 1.0,
            origin_x: 0.0,
            origin_y: 0.0,
        }
    }
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    tearing_present_mode: Option<wgpu::PresentMode>,
    pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    white_texture: GpuTexture,
    scale_factor: f32,
    output_transform: TextureTransform,
    zoom: Zoom,
    nearest_filter: bool,
    gamma_lut: GammaLut,
    effects: Effects,
}

fn presentation_mode(
    immediate: Option<wgpu::PresentMode>,
    tearing: bool,
) -> wgpu::PresentMode {
    if tearing {
        immediate.unwrap_or(wgpu::PresentMode::Fifo)
    } else {
        wgpu::PresentMode::Fifo
    }
}

impl Renderer {
    pub async fn new(window: Arc<Window>) -> Result<Self, RenderError> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_without_display_handle(),
        );
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .expect("a compatible surface must expose at least one format");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let texture_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("shingle texture layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float {
                                filterable: true,
                            },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(
                            wgpu::SamplerBindingType::Filtering,
                        ),
                        count: None,
                    },
                ],
            });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shingle linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
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
        let pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shingle tile pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
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
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(
                            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
                        ),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let white_texture = create_texture(
            &device,
            &queue,
            &texture_layout,
            &sampler,
            1,
            1,
            &[255, 255, 255, 255],
        )?;

        Ok(Self {
            surface,
            device,
            queue,
            config,
            tearing_present_mode: capabilities
                .present_modes
                .contains(&wgpu::PresentMode::Immediate)
                .then_some(wgpu::PresentMode::Immediate),
            pipeline,
            texture_layout,
            sampler,
            white_texture,
            scale_factor: 1.0,
            output_transform: TextureTransform::Normal,
            zoom: Zoom::default(),
            nearest_filter: false,
            gamma_lut,
            effects,
        })
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        PhysicalSize::new(self.config.width, self.config.height)
    }

    pub fn set_scale_factor(&mut self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.scale_factor = scale as f32;
        }
    }

    pub fn set_output_transform(&mut self, transform: TextureTransform) {
        self.output_transform = transform;
    }

    pub fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
        if factor.is_finite() && origin_x.is_finite() && origin_y.is_finite() {
            self.zoom = Zoom {
                factor: factor.clamp(1.0, 50.0),
                origin_x,
                origin_y,
            };
        }
    }

    pub fn set_nearest_filter(&mut self, nearest: bool) -> bool {
        if self.nearest_filter == nearest {
            return false;
        }
        self.nearest_filter = nearest;
        let filter = if nearest {
            wgpu::FilterMode::Nearest
        } else {
            wgpu::FilterMode::Linear
        };
        self.sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shingle zoom sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: filter,
            min_filter: filter,
            ..Default::default()
        });
        true
    }

    pub fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
        self.gamma_lut.set(&self.queue, ramp)
    }

    pub fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
        self.effects.set(&self.queue, inverted, preserve_hue);
    }

    /// Uses immediate presentation when requested and supported by the host.
    pub fn set_tearing(&mut self, tearing: bool) {
        let mode = presentation_mode(self.tearing_present_mode, tearing);
        if self.config.present_mode != mode {
            self.config.present_mode = mode;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<GpuTexture, RenderError> {
        create_texture(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            width,
            height,
            rgba,
        )
    }

    pub fn update_texture(
        &self,
        texture: &GpuTexture,
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        let expected = expected_len(texture.width, texture.height)?;
        if rgba.len() != expected {
            return Err(RenderError::InvalidTextureData {
                expected,
                actual: rgba.len(),
            });
        }
        write_texture(
            &self.queue,
            &texture._texture,
            texture.width,
            texture.height,
            rgba,
        );
        Ok(())
    }

    pub fn render(
        &mut self,
        tiles: &[RenderTile<'_>],
        clear: [f64; 4],
    ) -> Result<bool, RenderError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                return Err(RenderError::SurfaceLost);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceValidation);
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let clear = invert_clear(
            clear,
            self.effects.inverted,
            self.effects.preserve_hue,
        );
        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor {
                label: Some("shingle frame encoder"),
            },
        );
        let vertices = tiles
            .iter()
            .flat_map(|tile| {
                quad(
                    tile,
                    self.config.width,
                    self.config.height,
                    self.scale_factor,
                    self.zoom,
                    self.output_transform,
                )
            })
            .collect::<Vec<_>>();
        let vertex_buffer = (!vertices.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("shingle quad vertices"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });
        {
            let mut pass =
                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("shingle composite pass"),
                    color_attachments: &[Some(
                        wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: clear[0],
                                    g: clear[1],
                                    b: clear[2],
                                    a: clear[3],
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        },
                    )],
                    ..Default::default()
                });
            if let Some(vertex_buffer) = &vertex_buffer {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                for (index, tile) in tiles.iter().enumerate() {
                    let texture = tile.texture.unwrap_or(&self.white_texture);
                    pass.set_bind_group(0, &texture.bind_group, &[]);
                    pass.set_bind_group(1, &self.gamma_lut.bind_group, &[]);
                    pass.set_bind_group(2, &self.effects.bind_group, &[]);
                    let first = (index * 6) as u32;
                    pass.draw(first..first + 6, 0..1);
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        Ok(true)
    }
}

fn invert_clear(
    mut color: [f64; 4],
    inverted: bool,
    preserve_hue: bool,
) -> [f64; 4] {
    if !inverted {
        return color;
    }
    if preserve_hue {
        let hue = color[3]
            - color[..3].iter().copied().fold(f64::INFINITY, f64::min)
            - color[..3].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        for component in &mut color[..3] {
            *component += hue;
        }
    } else {
        for component in &mut color[..3] {
            *component = 1.0 - *component;
        }
    }
    color
}

fn expected_len(width: u32, height: u32) -> Result<usize, RenderError> {
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

fn create_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<GpuTexture, RenderError> {
    let expected = expected_len(width, height)?;
    if rgba.len() != expected {
        return Err(RenderError::InvalidTextureData {
            expected,
            actual: rgba.len(),
        });
    }
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
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST,
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
        _texture: texture,
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
                sample_type: wgpu::TextureSampleType::Float {
                    filterable: true,
                },
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
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let rgba = (0..256)
        .flat_map(|value| [value as u8, value as u8, value as u8, 255])
        .collect::<Vec<_>>();
    write_texture(queue, &texture, 256, 1, &rgba);
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
        ramp: (0..256)
            .chain(0..256)
            .chain(0..256)
            .map(|value| value * 257)
            .collect::<Vec<u16>>(),
    }
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
        let mut rgba = Vec::with_capacity(1024);
        for index in 0..256 {
            for value in [ramp[index], ramp[256 + index], ramp[512 + index]] {
                rgba.push(((u32::from(value) + 128) / 257) as u8);
            }
            rgba.push(255);
        }
        write_texture(queue, &self.texture, 256, 1, &rgba);
        self.ramp.copy_from_slice(ramp);
        Ok(())
    }
}

fn quad(
    tile: &RenderTile<'_>,
    output_width: u32,
    output_height: u32,
    scale_factor: f32,
    zoom: Zoom,
    output_transform: TextureTransform,
) -> [Vertex; 6] {
    let rect = tile.geometry;
    let output_width = output_width as f32 / scale_factor;
    let output_height = output_height as f32 / scale_factor;
    let x0 = rect.x as f32 / output_width * 2.0 - 1.0;
    let x1 = (rect.x + rect.width as i32) as f32 / output_width * 2.0 - 1.0;
    let y0 = 1.0 - rect.y as f32 / output_height * 2.0;
    let y1 = 1.0 - (rect.y + rect.height as i32) as f32 / output_height * 2.0;
    let top_left = transformed_vertex(tile, x0, y0, 0.0, 0.0);
    let top_right = transformed_vertex(tile, x1, y0, 1.0, 0.0);
    let bottom_left = transformed_vertex(tile, x0, y1, 0.0, 1.0);
    let bottom_right = transformed_vertex(tile, x1, y1, 1.0, 1.0);
    let mut vertices = [
        top_left,
        bottom_left,
        top_right,
        top_right,
        bottom_left,
        bottom_right,
    ];
    rotate_vertices(
        &mut vertices,
        rect,
        tile.rotation,
        output_width,
        output_height,
    );
    for vertex in &mut vertices {
        zoom_vertex(vertex, output_width, output_height, zoom);
        output_vertex(vertex, output_transform);
    }
    vertices
}

fn rotate_vertices(
    vertices: &mut [Vertex; 6],
    rect: RenderRect,
    angle: f32,
    output_width: f32,
    output_height: f32,
) {
    if !angle.is_finite() || angle == 0.0 {
        return;
    }
    let center_x =
        (rect.x + rect.width as i32 / 2) as f32 / output_width * 2.0 - 1.0;
    let center_y =
        1.0 - (rect.y + rect.height as i32 / 2) as f32 / output_height * 2.0;
    let (sin, cos) = angle.sin_cos();
    for vertex in vertices {
        let x = (vertex.position[0] - center_x) * output_width / 2.0;
        let y = (vertex.position[1] - center_y) * output_height / 2.0;
        vertex.position = [
            center_x + (x * cos - y * sin) / output_width * 2.0,
            center_y + (x * sin + y * cos) / output_height * 2.0,
        ];
    }
}

fn zoom_crop(output_width: f32, output_height: f32, zoom: Zoom) -> (f32, f32) {
    if zoom.factor <= 1.0 {
        return (0.0, 0.0);
    }
    let visible_width = output_width / zoom.factor;
    let visible_height = output_height / zoom.factor;
    let scale = (zoom.factor - 1.0) / zoom.factor;
    (
        (zoom.origin_x * scale)
            .clamp(0.0, (output_width - visible_width).max(0.0)),
        (zoom.origin_y * scale)
            .clamp(0.0, (output_height - visible_height).max(0.0)),
    )
}

fn zoom_vertex(
    vertex: &mut Vertex,
    output_width: f32,
    output_height: f32,
    zoom: Zoom,
) {
    let (crop_x, crop_y) = zoom_crop(output_width, output_height, zoom);
    vertex.position[0] =
        (vertex.position[0] + 1.0 - 2.0 * crop_x / output_width) * zoom.factor
            - 1.0;
    vertex.position[1] =
        (vertex.position[1] - 1.0 + 2.0 * crop_y / output_height) * zoom.factor
            + 1.0;
}

pub fn zoom_input_point(
    x: f64,
    y: f64,
    output_width: u32,
    output_height: u32,
    factor: f32,
    origin_x: f32,
    origin_y: f32,
) -> (f64, f64) {
    if !factor.is_finite() || factor <= 1.0 {
        return (x, y);
    }
    let zoom = Zoom {
        factor,
        origin_x,
        origin_y,
    };
    let (crop_x, crop_y) =
        zoom_crop(output_width as f32, output_height as f32, zoom);
    (
        f64::from(crop_x) + x / f64::from(factor),
        f64::from(crop_y) + y / f64::from(factor),
    )
}

/// Maps a scene-space point back through the current zoom transformation.
pub fn zoom_output_point(
    x: f64,
    y: f64,
    output_width: u32,
    output_height: u32,
    factor: f32,
    origin_x: f32,
    origin_y: f32,
) -> (f64, f64) {
    if !factor.is_finite() || factor <= 1.0 {
        return (x, y);
    }
    let zoom = Zoom {
        factor,
        origin_x,
        origin_y,
    };
    let (crop_x, crop_y) =
        zoom_crop(output_width as f32, output_height as f32, zoom);
    (
        (x - f64::from(crop_x)) * f64::from(factor),
        (y - f64::from(crop_y)) * f64::from(factor),
    )
}

fn output_vertex(vertex: &mut Vertex, transform: TextureTransform) {
    if matches!(
        transform,
        TextureTransform::FlipHorizontal
            | TextureTransform::Transpose
            | TextureTransform::FlipVertical
            | TextureTransform::AntiTranspose
    ) {
        vertex.position[0] = -vertex.position[0];
    }
    let [x, y] = vertex.position;
    vertex.position = match transform {
        TextureTransform::Rotate90 | TextureTransform::Transpose => [-y, x],
        TextureTransform::FlipBoth | TextureTransform::FlipVertical => [-x, -y],
        TextureTransform::Rotate270 | TextureTransform::AntiTranspose => {
            [y, -x]
        }
        _ => [x, y],
    };
}

fn transformed_vertex(
    tile: &RenderTile<'_>,
    x: f32,
    y: f32,
    u: f32,
    v: f32,
) -> Vertex {
    let u = tile.uv[0] + u * (tile.uv[2] - tile.uv[0]);
    let v = tile.uv[1] + v * (tile.uv[3] - tile.uv[1]);
    let (u, v) = match tile.transform {
        TextureTransform::Normal => (u, v),
        TextureTransform::FlipHorizontal => (1.0 - u, v),
        TextureTransform::Rotate90 => (v, 1.0 - u),
        TextureTransform::FlipBoth => (1.0 - u, 1.0 - v),
        TextureTransform::Rotate270 => (1.0 - v, u),
        TextureTransform::Transpose => (v, u),
        TextureTransform::FlipVertical => (u, 1.0 - v),
        TextureTransform::AntiTranspose => (1.0 - v, 1.0 - u),
    };
    vertex(x, y, u, v, tile.color)
}

fn vertex(x: f32, y: f32, u: f32, v: f32, color: [f32; 4]) -> Vertex {
    Vertex {
        position: [x, y],
        uv: [u, v],
        color,
    }
}

pub struct OffscreenRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    white_texture: GpuTexture,
    scale_factor: f32,
    output_transform: TextureTransform,
    zoom: Zoom,
    nearest_filter: bool,
    gamma_lut: GammaLut,
    effects: Effects,
    target: wgpu::Texture,
    width: u32,
    height: u32,
}

impl OffscreenRenderer {
    pub async fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::EmptyTexture);
        }
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_without_display_handle(),
        );
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await?;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("shingle offscreen texture layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float {
                                filterable: true,
                            },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(
                            wgpu::SamplerBindingType::Filtering,
                        ),
                        count: None,
                    },
                ],
            });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shingle offscreen sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let gamma_layout = gamma_bind_group_layout(&device);
        let gamma_lut = create_gamma_lut(&device, &queue, &gamma_layout);
        let effect_layout = effect_bind_group_layout(&device);
        let effects = create_effects(&device, &effect_layout);
        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("shingle offscreen pipeline layout"),
                bind_group_layouts: &[
                    Some(&texture_layout),
                    Some(&gamma_layout),
                    Some(&effect_layout),
                ],
                immediate_size: 0,
            });
        let shader =
            device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("shingle offscreen pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
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
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(
                            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
                        ),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let white_texture = create_texture(
            &device,
            &queue,
            &texture_layout,
            &sampler,
            1,
            1,
            &[255, 255, 255, 255],
        )?;
        let target = create_render_target(&device, width, height, format);
        Ok(Self {
            device,
            queue,
            pipeline,
            texture_layout,
            sampler,
            white_texture,
            scale_factor: 1.0,
            output_transform: TextureTransform::Normal,
            zoom: Zoom::default(),
            nearest_filter: false,
            gamma_lut,
            effects,
            target,
            width,
            height,
        })
    }

    pub fn set_scale_factor(&mut self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.scale_factor = scale as f32;
        }
    }

    pub fn set_output_transform(&mut self, transform: TextureTransform) {
        self.output_transform = transform;
    }

    pub fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
        if factor.is_finite() && origin_x.is_finite() && origin_y.is_finite() {
            self.zoom = Zoom {
                factor: factor.clamp(1.0, 50.0),
                origin_x,
                origin_y,
            };
        }
    }

    pub fn set_nearest_filter(&mut self, nearest: bool) -> bool {
        if self.nearest_filter == nearest {
            return false;
        }
        self.nearest_filter = nearest;
        let filter = if nearest {
            wgpu::FilterMode::Nearest
        } else {
            wgpu::FilterMode::Linear
        };
        self.sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shingle zoom sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: filter,
            min_filter: filter,
            ..Default::default()
        });
        true
    }

    pub fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
        self.gamma_lut.set(&self.queue, ramp)
    }

    pub fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
        self.effects.set(&self.queue, inverted, preserve_hue);
    }

    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<GpuTexture, RenderError> {
        create_texture(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            width,
            height,
            rgba,
        )
    }

    pub fn render(
        &mut self,
        tiles: &[RenderTile<'_>],
        clear: [f64; 4],
    ) -> Result<Vec<u8>, RenderError> {
        let view = self.target.create_view(&Default::default());
        let clear = invert_clear(
            clear,
            self.effects.inverted,
            self.effects.preserve_hue,
        );
        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor {
                label: Some("shingle offscreen encoder"),
            },
        );
        let vertices = tiles
            .iter()
            .flat_map(|tile| {
                quad(
                    tile,
                    self.width,
                    self.height,
                    self.scale_factor,
                    self.zoom,
                    self.output_transform,
                )
            })
            .collect::<Vec<_>>();
        let vertex_buffer = (!vertices.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("shingle offscreen vertices"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });
        {
            let mut pass =
                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("shingle offscreen pass"),
                    color_attachments: &[Some(
                        wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: clear[0],
                                    g: clear[1],
                                    b: clear[2],
                                    a: clear[3],
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        },
                    )],
                    ..Default::default()
                });
            if let Some(vertex_buffer) = &vertex_buffer {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                for (index, tile) in tiles.iter().enumerate() {
                    let texture = tile.texture.unwrap_or(&self.white_texture);
                    pass.set_bind_group(0, &texture.bind_group, &[]);
                    pass.set_bind_group(1, &self.gamma_lut.bind_group, &[]);
                    pass.set_bind_group(2, &self.effects.bind_group, &[]);
                    let first = (index * 6) as u32;
                    pass.draw(first..first + 6, 0..1);
                }
            }
        }
        let row_bytes = self.width as usize * 4;
        let padded_row_bytes = row_bytes
            .div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shingle offscreen readback"),
            size: (padded_row_bytes * self.height as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes as u32),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::channel();
        readback
            .clone()
            .map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = sender.send(result);
            });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| RenderError::Readback(error.to_string()))?;
        receiver
            .recv()
            .map_err(|error| RenderError::Readback(error.to_string()))?
            .map_err(|error| RenderError::Readback(error.to_string()))?;
        let mapped = readback
            .get_mapped_range(..)
            .map_err(|error| RenderError::Readback(error.to_string()))?;
        let mut rgba = vec![0; row_bytes * self.height as usize];
        for (row, destination) in rgba.chunks_exact_mut(row_bytes).enumerate() {
            let start = row * padded_row_bytes;
            destination.copy_from_slice(&mapped[start..start + row_bytes]);
        }
        drop(mapped);
        readback.unmap();
        Ok(rgba)
    }
}

fn create_render_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shingle offscreen target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn tearing_uses_immediate_only_when_requested_and_supported() {
        assert_eq!(
            super::presentation_mode(Some(wgpu::PresentMode::Immediate), true,),
            wgpu::PresentMode::Immediate
        );
        assert_eq!(
            super::presentation_mode(Some(wgpu::PresentMode::Immediate), false,),
            wgpu::PresentMode::Fifo
        );
        assert_eq!(
            super::presentation_mode(None, true),
            wgpu::PresentMode::Fifo
        );
    }

    #[test]
    fn zoom_output_point_reverses_zoom_input_point() {
        let point =
            super::zoom_input_point(913.0, 277.0, 1280, 720, 2.5, 400.0, 200.0);
        let output = super::zoom_output_point(
            point.0, point.1, 1280, 720, 2.5, 400.0, 200.0,
        );
        assert!((output.0 - 913.0).abs() < 0.0001);
        assert!((output.1 - 277.0).abs() < 0.0001);
    }

    #[test]
    fn tile_rotation_preserves_center_and_swaps_extents() {
        let mut tile = super::RenderTile::solid(
            super::RenderRect {
                x: 10,
                y: 20,
                width: 100,
                height: 50,
            },
            [1.0; 4],
        );
        tile.rotation = std::f32::consts::FRAC_PI_2;
        let vertices = super::quad(
            &tile,
            1280,
            720,
            1.0,
            super::Zoom::default(),
            super::TextureTransform::Normal,
        );
        let center = vertices.iter().fold([0.0; 2], |sum, vertex| {
            [sum[0] + vertex.position[0], sum[1] + vertex.position[1]]
        });
        assert!((center[0] / 6.0 - (60.0 / 1280.0 * 2.0 - 1.0)).abs() < 0.0001);
        assert!((center[1] / 6.0 - (1.0 - 45.0 / 720.0 * 2.0)).abs() < 0.0001);
        let width = vertices
            .iter()
            .map(|vertex| vertex.position[0])
            .fold(f32::NEG_INFINITY, f32::max)
            - vertices
                .iter()
                .map(|vertex| vertex.position[0])
                .fold(f32::INFINITY, f32::min);
        assert!(
            (width - 50.0 / 1280.0 * 2.0).abs() < 0.0001,
            "width={width}, positions={:?}",
            vertices.map(|vertex| vertex.position)
        );
    }
}
