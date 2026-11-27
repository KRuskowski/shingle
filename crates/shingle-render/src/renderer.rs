//! Single-target convenience wrappers that own a [`GpuContext`], a
//! [`RenderState`] and one target.

use crate::{
  GpuContext, GpuOptions, GpuTexture, OffscreenTarget, RenderError,
  RenderState, RenderTile, TextureTransform, WindowTarget, capture_frame,
};

/// Renders to one window surface, for the nested frontend.
pub struct Renderer {
  gpu: GpuContext,
  state: RenderState,
  target: WindowTarget,
}

impl Renderer {
  /// Creates a GPU context able to present to `window` and a window target
  /// of `size` physical pixels. The device is opened for DMA-BUF import
  /// where the adapter allows it ([`GpuContext::dmabuf`]).
  pub async fn new(
    window: impl Into<wgpu::SurfaceTarget<'static>>,
    size: (u32, u32),
  ) -> Result<Self, RenderError> {
    let instance = GpuContext::create_instance();
    let surface = WindowTarget::create_surface(&instance, window)?;
    let gpu = GpuContext::new(
      instance,
      GpuOptions {
        compatible_surface: Some(&surface),
        dmabuf: true,
        ..Default::default()
      },
    )
    .await?;
    let target = WindowTarget::new(&gpu, surface, size)?;
    Ok(Self {
      gpu,
      state: RenderState::default(),
      target,
    })
  }

  /// The shared GPU context.
  pub fn gpu(&self) -> &GpuContext {
    &self.gpu
  }

  /// The surface size in physical pixels.
  pub fn size(&self) -> (u32, u32) {
    self.target.size()
  }

  /// See [`RenderState::set_scale_factor`].
  pub fn set_scale_factor(&mut self, scale: f64) {
    self.state.set_scale_factor(scale);
  }

  /// See [`RenderState::set_output_transform`].
  pub fn set_output_transform(&mut self, transform: TextureTransform) {
    self.state.set_output_transform(transform);
  }

  /// See [`RenderState::set_zoom`].
  pub fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    self.state.set_zoom(factor, origin_x, origin_y);
  }

  /// See [`RenderState::set_nearest_filter`].
  pub fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    self.state.set_nearest_filter(nearest)
  }

  /// See [`GpuContext::set_gamma_ramp`].
  pub fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    self.gpu.set_gamma_ramp(ramp)
  }

  /// See [`GpuContext::set_inversion`].
  pub fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    self.gpu.set_inversion(inverted, preserve_hue);
  }

  /// See [`WindowTarget::set_tearing`].
  pub fn set_tearing(&mut self, tearing: bool) {
    self.target.set_tearing(&self.gpu, tearing);
  }

  /// See [`WindowTarget::resize`].
  pub fn resize(&mut self, size: (u32, u32)) {
    self.target.resize(&self.gpu, size);
  }

  /// Uploads a texture with the current filter.
  pub fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError> {
    self
      .gpu
      .create_texture(width, height, rgba, self.state.texture_filter())
  }

  /// See [`GpuContext::update_texture`].
  pub fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    self.gpu.update_texture(texture, rgba)
  }

  /// See [`WindowTarget::render`].
  pub fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    self.target.render(&self.gpu, &self.state, tiles, clear)
  }

  /// Draws the frame again offscreen for screencopy; see
  /// [`capture_frame`].
  pub fn capture(
    &self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    capture_frame(&self.gpu, &self.state, size, tiles, clear)
  }

  /// Draws the frame again at `scale` into a `size` target of device
  /// pixels; see [`crate::capture_frame_scaled`].
  pub fn capture_scaled(
    &self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
    scale: f64,
  ) -> Result<Vec<u8>, RenderError> {
    crate::capture_frame_scaled(
      &self.gpu,
      &self.state,
      size,
      scale,
      tiles,
      clear,
    )
  }

  /// Draws the frame into a capture client's DMA-BUF; see
  /// [`crate::capture_into_dmabuf`].
  pub fn capture_into(
    &self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    buffer: &crate::DmabufDescriptor<'_>,
  ) -> Result<(), RenderError> {
    crate::capture_into_dmabuf(&self.gpu, &self.state, buffer, tiles, clear)
  }
}

/// Renders to one offscreen texture and reads every frame back, for
/// `shingle-lock`.
pub struct OffscreenRenderer {
  gpu: GpuContext,
  state: RenderState,
  target: OffscreenTarget,
}

impl OffscreenRenderer {
  /// Creates a GPU context and a `width` by `height` offscreen target.
  pub async fn new(width: u32, height: u32) -> Result<Self, RenderError> {
    if width == 0 || height == 0 {
      return Err(RenderError::EmptyTexture);
    }
    let gpu =
      GpuContext::new(GpuContext::create_instance(), GpuOptions::default())
        .await?;
    let target = OffscreenTarget::new(&gpu, width, height)?;
    Ok(Self {
      gpu,
      state: RenderState::default(),
      target,
    })
  }

  /// The shared GPU context.
  pub fn gpu(&self) -> &GpuContext {
    &self.gpu
  }

  /// See [`RenderState::set_scale_factor`].
  pub fn set_scale_factor(&mut self, scale: f64) {
    self.state.set_scale_factor(scale);
  }

  /// See [`RenderState::set_output_transform`].
  pub fn set_output_transform(&mut self, transform: TextureTransform) {
    self.state.set_output_transform(transform);
  }

  /// See [`RenderState::set_zoom`].
  pub fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    self.state.set_zoom(factor, origin_x, origin_y);
  }

  /// See [`RenderState::set_nearest_filter`].
  pub fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    self.state.set_nearest_filter(nearest)
  }

  /// See [`GpuContext::set_gamma_ramp`].
  pub fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    self.gpu.set_gamma_ramp(ramp)
  }

  /// See [`GpuContext::set_inversion`].
  pub fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    self.gpu.set_inversion(inverted, preserve_hue);
  }

  /// Uploads a texture with the current filter.
  pub fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError> {
    self
      .gpu
      .create_texture(width, height, rgba, self.state.texture_filter())
  }

  /// Draws one frame and returns it as tightly packed RGBA8 rows.
  pub fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<Vec<u8>, RenderError> {
    self.target.render(&self.gpu, &self.state, tiles, clear);
    self.target.read_rgba(&self.gpu)
  }

  /// Draws the frame again for screencopy; see [`capture_frame`].
  pub fn capture(
    &self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    capture_frame(&self.gpu, &self.state, size, tiles, clear)
  }

  /// Draws the frame again at `scale` into a `size` target of device
  /// pixels; see [`crate::capture_frame_scaled`].
  pub fn capture_scaled(
    &self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
    scale: f64,
  ) -> Result<Vec<u8>, RenderError> {
    crate::capture_frame_scaled(
      &self.gpu,
      &self.state,
      size,
      scale,
      tiles,
      clear,
    )
  }

  /// Draws the frame into a capture client's DMA-BUF; see
  /// [`crate::capture_into_dmabuf`].
  pub fn capture_into(
    &self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    buffer: &crate::DmabufDescriptor<'_>,
  ) -> Result<(), RenderError> {
    crate::capture_into_dmabuf(&self.gpu, &self.state, buffer, tiles, clear)
  }
}
