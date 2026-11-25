//! wgpu rendering for Shingle: a shared GPU context, per-output render
//! state, one composite pass and the targets it draws into.

mod draw;
mod gpu;
mod renderer;
mod state;
mod target;

use std::time::{Duration, Instant};

use thiserror::Error;

pub use draw::draw;
pub use gpu::{AdapterFilter, GpuContext, GpuOptions, TextureFilter};
pub use renderer::{OffscreenRenderer, Renderer};
pub use state::RenderState;
pub use target::{OffscreenTarget, WindowTarget};

use state::Zoom;

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
  #[error("no GPU adapter matched the adapter filter")]
  NoMatchingAdapter,
  #[error("the GPU surface offers no texture format")]
  NoSurfaceFormat,
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

/// A client or UI texture uploaded with [`GpuContext::create_texture`].
pub struct GpuTexture {
  texture: wgpu::Texture,
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
    draw::zoom_crop(output_width as f32, output_height as f32, zoom);
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
    draw::zoom_crop(output_width as f32, output_height as f32, zoom);
  (
    (x - f64::from(crop_x)) * f64::from(factor),
    (y - f64::from(crop_y)) * f64::from(factor),
  )
}

#[cfg(test)]
mod tests {
  #[test]
  fn zoom_output_point_reverses_zoom_input_point() {
    let point =
      super::zoom_input_point(913.0, 277.0, 1280, 720, 2.5, 400.0, 200.0);
    let output =
      super::zoom_output_point(point.0, point.1, 1280, 720, 2.5, 400.0, 200.0);
    assert!((output.0 - 913.0).abs() < 0.0001);
    assert!((output.1 - 277.0).abs() < 0.0001);
  }
}
