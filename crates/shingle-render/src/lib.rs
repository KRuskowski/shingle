//! wgpu rendering for Shingle: a shared GPU context, per-output render
//! state, one composite pass and the targets it draws into.
//!
//! A frame is a list of [`RenderTile`]s in logical pixels, drawn back to
//! front by [`draw`] after clearing the target. Each tile is mapped to clip
//! space through the output's [`RenderState`] in a fixed order: project
//! its corners through its [`Projection`], if it has one, divide by the
//! scale factor, rotate the tile around its centre, apply the zoom crop,
//! then the output transform. Colours are premultiplied, and the gamma ramp
//! and inversion effect held by the [`GpuContext`] apply to every tile.
//!
//! The crate has no windowing dependency: a host gives it a
//! `wgpu::Surface` through [`WindowTarget`], or renders into an
//! [`OffscreenTarget`] for tests and screencopy.
//!
//! With [`GpuOptions::dmabuf`] the device is opened for DMA-BUF import
//! ([`dmabuf`]): [`GpuContext::import_dmabuf`] turns a client's or a
//! scanout buffer into a texture, and [`draw`] moves imported images in
//! and out of the foreign queue family around each frame.
//!
//! Client `wl_shm` buffers are uploaded from their pools in their own
//! byte order, damaged rectangles only ([`upload`]).

#![warn(missing_docs)]

pub mod dmabuf;
mod draw;
mod gpu;
mod renderer;
mod state;
mod target;
pub mod upload;

use std::time::{Duration, Instant};

use thiserror::Error;

pub use dmabuf::{
  DmabufCaps, DmabufDescriptor, DmabufUnavailable, DmabufUse, ForeignAccess,
  ImportError, ImportedTexture, TextureImporter,
};
pub use draw::{draw, draw_with_access};
pub use gpu::{AdapterFilter, GpuContext, GpuOptions, TextureFilter};
pub use renderer::{OffscreenRenderer, Renderer};
pub use state::RenderState;
pub use target::{
  CAPTURE_TIMEOUT, OffscreenTarget, WindowTarget, capture_frame,
};
pub use upload::{ShmImage, TextureUploader, UploadError, UploadRect};

use state::Zoom;

/// Errors from setting up the GPU, uploading data or presenting a frame.
#[derive(Debug, Error)]
pub enum RenderError {
  /// wgpu could not create a surface for the host's window.
  #[error("failed to create GPU surface")]
  CreateSurface(#[from] wgpu::CreateSurfaceError),
  /// wgpu found no adapter, or none that can present to the given surface,
  /// when no [`AdapterFilter`] was set.
  #[error("no compatible GPU adapter was found")]
  RequestAdapter(#[from] wgpu::RequestAdapterError),
  /// The adapter refused to create a device with the features and limits
  /// the renderer asks for.
  #[error("failed to create GPU device")]
  RequestDevice(#[from] wgpu::RequestDeviceError),
  /// The surface's swapchain is gone, for example after the window was
  /// destroyed. Unlike an outdated surface, which [`WindowTarget::render`]
  /// reconfigures itself, the target must be recreated.
  #[error("GPU surface was lost")]
  SurfaceLost,
  /// wgpu reported a validation error while acquiring a frame. This is a
  /// bug in how the surface was configured, not a transient condition.
  #[error("GPU surface validation failed")]
  SurfaceValidation,
  /// A texture or offscreen target was asked for with a zero width or
  /// height, which wgpu does not allow.
  #[error("texture dimensions must be nonzero")]
  EmptyTexture,
  /// The pixel data passed in is not exactly `width * height * 4` bytes of
  /// tightly packed RGBA8.
  #[error("RGBA buffer has {actual} bytes; expected {expected}")]
  InvalidTextureData {
    /// `width * height * 4`.
    expected: usize,
    /// The length that was passed.
    actual: usize,
  },
  /// A gamma ramp did not have 256 entries for each of red, green and blue,
  /// as wlr-gamma-control-v1 sends it. Holds the length that was passed.
  #[error("gamma ramp must contain 768 16-bit values; got {0}")]
  InvalidGammaRamp(usize),
  /// Mapping or copying an [`OffscreenTarget`] back to the CPU failed. Holds
  /// wgpu's description of the failure.
  #[error("GPU readback failed: {0}")]
  Readback(String),
  /// The GPU did not finish the frame within the time
  /// [`OffscreenTarget::read_rgba_within`] allowed.
  #[error("the GPU did not finish within {0:?}")]
  Timeout(Duration),
  /// No enumerated adapter matched the [`AdapterFilter`] given in
  /// [`GpuOptions`] and, if a surface was given, could present to it.
  #[error("no GPU adapter matched the adapter filter")]
  NoMatchingAdapter,
  /// The surface reports no supported texture format for this adapter.
  #[error("the GPU surface offers no texture format")]
  NoSurfaceFormat,
}

/// A rectangle in logical output pixels: origin at the top left of the
/// output, y pointing down. [`draw`] divides by the output's scale factor,
/// so at scale 2 a 640 by 360 rectangle covers a 1280 by 720 target. The
/// origin may be negative or beyond the output; the part outside is
/// clipped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderRect {
  /// Left edge.
  pub x: i32,
  /// Top edge.
  pub y: i32,
  /// Width; zero draws nothing.
  pub width: u32,
  /// Height; zero draws nothing.
  pub height: u32,
}

/// The half of a tile a dragged window would take if dropped there, for
/// [`split_preview`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewDirection {
  /// The left half.
  Left,
  /// The right half.
  Right,
  /// The top half.
  Up,
  /// The bottom half.
  Down,
}

/// The half of `rect` facing `direction`, where a tile-drag drop would place
/// the dragged window. With an odd size the left or top half is the smaller
/// one, so the two halves always add up to `rect` without overlapping.
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

/// A client or UI texture uploaded with [`GpuContext::create_texture`], or
/// a DMA-BUF imported with [`GpuContext::import_dmabuf`]. Clones share the
/// texture and its bind group.
#[derive(Clone)]
pub struct GpuTexture {
  texture: wgpu::Texture,
  bind_group: wgpu::BindGroup,
  width: u32,
  height: u32,
  // Shader flags for every tile that samples it (dmabuf::tile_flags).
  flags: u32,
  // The Vulkan image of an imported texture, owned by the foreign queue
  // family between frames.
  foreign: Option<dmabuf::ForeignImage>,
}

impl GpuTexture {
  /// Width in texels, as uploaded.
  pub fn width(&self) -> u32 {
    self.width
  }

  /// Height in texels, as uploaded.
  pub fn height(&self) -> u32 {
    self.height
  }

  /// Whether this is an imported DMA-BUF, which cannot be written to.
  pub fn is_imported(&self) -> bool {
    self.foreign.is_some()
  }
}

/// One of the eight `wl_output.transform` values, in protocol order, named
/// by their geometric effect.
///
/// On a [`RenderTile`] it is the client's `wl_surface.set_buffer_transform`
/// and is undone when sampling, so the content appears upright. On a
/// [`RenderState`] it is the output's transform and is applied to the whole
/// frame in clip space, after zoom. The frontend converts from
/// `wl_output::Transform` in `apply_buffer` and `render_output_transform`
/// (crates/shingle-frontend/src/frontend/scene.rs).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextureTransform {
  /// `normal`: no transform.
  #[default]
  Normal,
  /// `flipped`: mirrored around the vertical axis.
  FlipHorizontal,
  /// `90`: rotated 90 degrees counterclockwise.
  Rotate90,
  /// `180`: rotated 180 degrees, the same as flipping both axes.
  FlipBoth,
  /// `270`: rotated 270 degrees counterclockwise.
  Rotate270,
  /// `flipped-90`: mirrored, then rotated 90 degrees; swaps the axes.
  Transpose,
  /// `flipped-180`: mirrored around the horizontal axis.
  FlipVertical,
  /// `flipped-270`: mirrored, then rotated 270 degrees; swaps the axes
  /// across the other diagonal.
  AntiTranspose,
}

/// One textured or solid quad of a frame.
///
/// The fragment colour is the texture sample (opaque white without a
/// texture) multiplied by `color`, then passed through the gamma ramp and the
/// inversion effect, and blended as premultiplied alpha.
pub struct RenderTile<'a> {
  /// Where the quad is drawn, before rotation, zoom and output transform.
  pub geometry: RenderRect,
  /// Premultiplied RGBA multiplier, each component in 0..=1. Window opacity
  /// is `[alpha; 4]`; a solid colour must already be multiplied by its
  /// alpha.
  pub color: [f32; 4],
  /// The texture to sample, or `None` for a solid `color` quad.
  pub texture: Option<&'a GpuTexture>,
  /// The source rectangle as normalized texture coordinates `[left, top,
  /// right, bottom]`, before `transform` is undone. `[0, 0, 1, 1]` is the
  /// whole texture; `wp_viewport` source crops set a sub-rectangle.
  /// Sampling clamps to the edge.
  pub uv: [f32; 4],
  /// The buffer transform the client attached the texture with.
  pub transform: TextureTransform,
  /// Counterclockwise rotation around the tile center, in radians.
  pub rotation: f32,
  /// A perspective transform of the tile's corners, applied before
  /// everything else; `None` draws the tile flat.
  pub projection: Option<Projection>,
}

/// A 3D transform of a tile, as Wayfire's `view_3d_transformer_t` draws a
/// view: each corner of the tile, as the point `(x, y, 0, 1)` in logical
/// pixels relative to `origin` with `y` pointing up, is multiplied by
/// `matrix` to `(x', y', z', w')` and drawn at `origin + (x' / w', -y' /
/// w')`. Texture coordinates are interpolated with perspective, and `z'`
/// is ignored, since tiles are drawn in order without a depth test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projection {
  /// The point the corners are relative to, in logical pixels of the
  /// output layout, `y` pointing down as everywhere else.
  pub origin: [f32; 2],
  /// The transform, column-major as in GLSL and glm: `matrix[column][row]`.
  /// Every corner must end up with `w'` above 0, in front of the eye.
  pub matrix: [[f32; 4]; 4],
}

impl Projection {
  /// Where the point `(x, y)` of the layout, in logical pixels with `y`
  /// pointing down, is drawn, and its `w'`. `None` when `w'` is not above
  /// a small positive bound, behind or too close to the eye.
  pub fn apply(&self, x: f32, y: f32) -> Option<([f32; 2], f32)> {
    let local = [x - self.origin[0], self.origin[1] - y, 0.0, 1.0];
    let row = |index: usize| {
      (0..4)
        .map(|column| self.matrix[column][index] * local[column])
        .sum::<f32>()
    };
    let w = row(3);
    if !w.is_finite() || w < 1e-4 {
      return None;
    }
    Some((
      [self.origin[0] + row(0) / w, self.origin[1] - row(1) / w],
      w,
    ))
  }
}

impl<'a> RenderTile<'a> {
  /// A quad filled with premultiplied `color`, untransformed.
  pub fn solid(geometry: RenderRect, color: [f32; 4]) -> Self {
    Self {
      geometry,
      color,
      texture: None,
      uv: [0.0, 0.0, 1.0, 1.0],
      transform: TextureTransform::Normal,
      rotation: 0.0,
      projection: None,
    }
  }

  /// A quad showing all of `texture` at full opacity, untransformed. The
  /// texture is stretched to `geometry`.
  pub fn textured(geometry: RenderRect, texture: &'a GpuTexture) -> Self {
    Self {
      geometry,
      color: [1.0; 4],
      texture: Some(texture),
      uv: [0.0, 0.0, 1.0, 1.0],
      transform: TextureTransform::Normal,
      rotation: 0.0,
      projection: None,
    }
  }
}

/// Linear progress of an animation from `started` to `started + duration`,
/// in 0..=1. A `now` before `started` gives 0, and a zero `duration` gives 1
/// so the animation ends on its first frame. Easing is left to the caller.
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

/// Linear interpolation from `start` to `target` zoom factor, with
/// `progress` clamped to 0..=1. Not used inside the workspace.
pub fn interpolate_zoom(start: f32, target: f32, progress: f32) -> f32 {
  start + (target - start) * progress.clamp(0.0, 1.0)
}

/// A filled rectangle with a border of `border_width` logical pixels drawn
/// inside it, as solid tiles: the fill first, then the top, bottom, left
/// and right edges. The border is limited to the rectangle's smaller
/// dimension, and a zero border gives only the fill. Not used inside the
/// workspace.
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

/// Maps a point on the zoomed output to the scene point drawn there, so
/// pointer input lands on what is under the cursor. The point, output size
/// and origin are in the same units, logical pixels relative to the output.
///
/// `factor` and the origin are as given to [`RenderState::set_zoom`]: the
/// origin is the scene point that stays in place on screen, unless that
/// would move the visible area past an output edge, in which case the
/// visible area is clamped to the output. Wayfire's zoom plugin
/// (/home/karl/dev/wayfire/plugins/single_plugins/zoom.cpp) computes the
/// same crop but clamps only its top-left corner. A non-finite factor or one
/// of at most 1 returns the point unchanged. Inverse of
/// [`zoom_output_point`].
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

/// Maps a scene point to where it appears on the zoomed output, both in
/// logical pixels, for example to place a client's pointer warp. See
/// [`zoom_input_point`] for the parameters; this is its inverse.
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
