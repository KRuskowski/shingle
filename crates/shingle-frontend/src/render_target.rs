//! What the frontend draws into.
//!
//! [`RenderTarget`] is the part of a `shingle-render` renderer the scene
//! build uses: per-output view state, effects, texture uploads and the
//! final draw. The nested host passes its window [`Renderer`]; tests pass
//! an [`OffscreenRenderer`]; the native backend will pass its KMS target.

use shingle_render::{
  DmabufDescriptor, GpuTexture, OffscreenRenderer, RenderError, RenderTile,
  Renderer, TextureTransform,
};

/// A renderer the frontend can build its scene for and draw into.
pub trait RenderTarget {
  /// Sets the logical-to-physical scale of the output.
  fn set_scale_factor(&mut self, scale: f64);

  /// Sets the output transform applied to the whole frame.
  fn set_output_transform(&mut self, transform: TextureTransform);

  /// Chooses nearest or linear filtering for new textures. Returns true
  /// if the filter changed, in which case existing textures must be
  /// recreated.
  fn set_nearest_filter(&mut self, nearest: bool) -> bool;

  /// Sets the zoom factor and the output point it zooms around.
  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32);

  /// Sets the gamma ramp, as `wlr-gamma-control` delivers it.
  fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError>;

  /// Turns colour inversion on or off.
  fn set_inversion(&mut self, inverted: bool, preserve_hue: bool);

  /// Allows or forbids tearing presentation.
  fn set_tearing(&mut self, tearing: bool);

  /// Uploads a `width` by `height` RGBA8 texture with the current filter.
  fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError>;

  /// Replaces the contents of `texture`.
  fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError>;

  /// Draws `tiles` over `clear` and presents the frame. Returns whether a
  /// frame was presented, so the compositor can send frame callbacks.
  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError>;

  /// Draws the frame just rendered again, into a `size` offscreen target
  /// in layout logical pixels, and reads it back as RGBA8 rows for
  /// screencopy ([`shingle_render::capture_frame`]).
  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError>;

  /// Draws the frame just rendered again, into a capture client's
  /// DMA-BUF, with the target's scale and output transform as they are,
  /// and waits for the GPU ([`shingle_render::capture_into_dmabuf`]).
  fn capture_into(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    buffer: &DmabufDescriptor<'_>,
  ) -> Result<(), RenderError>;
}

impl RenderTarget for Renderer {
  fn set_scale_factor(&mut self, scale: f64) {
    Renderer::set_scale_factor(self, scale);
  }

  fn set_output_transform(&mut self, transform: TextureTransform) {
    Renderer::set_output_transform(self, transform);
  }

  fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    Renderer::set_nearest_filter(self, nearest)
  }

  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    Renderer::set_zoom(self, factor, origin_x, origin_y);
  }

  fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    Renderer::set_gamma_ramp(self, ramp)
  }

  fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    Renderer::set_inversion(self, inverted, preserve_hue);
  }

  fn set_tearing(&mut self, tearing: bool) {
    Renderer::set_tearing(self, tearing);
  }

  fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError> {
    Renderer::create_texture(self, width, height, rgba)
  }

  fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    Renderer::update_texture(self, texture, rgba)
  }

  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    Renderer::render(self, tiles, clear)
  }

  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    Renderer::capture(self, tiles, clear, size)
  }

  fn capture_into(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    buffer: &DmabufDescriptor<'_>,
  ) -> Result<(), RenderError> {
    Renderer::capture_into(self, tiles, clear, buffer)
  }
}

/// Offscreen rendering reads every frame back and drops it; tearing does
/// not apply.
impl RenderTarget for OffscreenRenderer {
  fn set_scale_factor(&mut self, scale: f64) {
    OffscreenRenderer::set_scale_factor(self, scale);
  }

  fn set_output_transform(&mut self, transform: TextureTransform) {
    OffscreenRenderer::set_output_transform(self, transform);
  }

  fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    OffscreenRenderer::set_nearest_filter(self, nearest)
  }

  fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    OffscreenRenderer::set_zoom(self, factor, origin_x, origin_y);
  }

  fn set_gamma_ramp(&mut self, ramp: &[u16]) -> Result<(), RenderError> {
    OffscreenRenderer::set_gamma_ramp(self, ramp)
  }

  fn set_inversion(&mut self, inverted: bool, preserve_hue: bool) {
    OffscreenRenderer::set_inversion(self, inverted, preserve_hue);
  }

  fn set_tearing(&mut self, _tearing: bool) {}

  fn create_texture(
    &self,
    width: u32,
    height: u32,
    rgba: &[u8],
  ) -> Result<GpuTexture, RenderError> {
    OffscreenRenderer::create_texture(self, width, height, rgba)
  }

  fn update_texture(
    &self,
    texture: &GpuTexture,
    rgba: &[u8],
  ) -> Result<(), RenderError> {
    self.gpu().update_texture(texture, rgba)
  }

  fn render(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    OffscreenRenderer::render(self, tiles, clear).map(|_| true)
  }

  fn capture(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    size: (u32, u32),
  ) -> Result<Vec<u8>, RenderError> {
    OffscreenRenderer::capture(self, tiles, clear, size)
  }

  fn capture_into(
    &mut self,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
    buffer: &DmabufDescriptor<'_>,
  ) -> Result<(), RenderError> {
    OffscreenRenderer::capture_into(self, tiles, clear, buffer)
  }
}
