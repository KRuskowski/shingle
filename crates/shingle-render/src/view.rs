//! Views drawn into a texture of their own, which a later pass draws onto
//! an output with an effect, as Wayfire's view transformers render a
//! view's surfaces and decoration into an auxiliary buffer and then draw
//! that buffer transformed (wayfire:src/api/wayfire/view-transform.hpp,
//! `transformer_render_instance_t::get_texture`).

use crate::{
  ForeignAccess, GpuContext, GpuTexture, OffscreenTarget, RenderError,
  RenderState, RenderTile,
  draw::{Grading, composite},
  gpu::TextureFilter,
};

impl GpuContext {
  /// A `width` by `height` texture to draw a view into with [`draw_view`]
  /// and to sample with bilinear filtering, as Wayfire samples a
  /// transformed view (wayfire:src/view/view-3d.cpp, `render`). Its
  /// format is [`OffscreenTarget::FORMAT`].
  pub fn create_view_texture(
    &self,
    width: u32,
    height: u32,
  ) -> Result<GpuTexture, RenderError> {
    if width == 0 || height == 0 {
      return Err(RenderError::EmptyTexture);
    }
    let texture = self.device().create_texture(&wgpu::TextureDescriptor {
      label: Some("shingle view texture"),
      size: wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: OffscreenTarget::FORMAT,
      usage: wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::COPY_SRC,
      view_formats: &[],
    });
    let bind_group = self.bind_texture(&texture, TextureFilter::Linear);
    Ok(GpuTexture {
      texture,
      bind_group,
      width,
      height,
      flags: 0,
      foreign: None,
    })
  }
}

/// Whether `texture` can be drawn into by [`draw_view`]: one that
/// [`GpuContext::create_view_texture`] made.
pub fn is_view_texture(texture: &GpuTexture) -> bool {
  texture
    .texture
    .usage()
    .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
}

/// Clears `texture` to transparent black and draws `tiles` into it, in
/// logical pixels from its top left corner at `scale` texels per pixel,
/// without the output's gamma ramp and inversion, which apply when the
/// texture is drawn onto an output. Colours stay premultiplied, so the
/// texture is a premultiplied image of the tiles. Imported textures among
/// the tiles are acquired and released around the pass as in
/// [`crate::draw`].
pub fn draw_view(
  gpu: &GpuContext,
  texture: &GpuTexture,
  scale: f64,
  tiles: &[RenderTile<'_>],
) -> Result<wgpu::SubmissionIndex, RenderError> {
  if !is_view_texture(texture) {
    return Err(RenderError::NotViewTexture);
  }
  let mut state = RenderState::default();
  state.set_scale_factor(scale);
  let view = texture.texture.create_view(&Default::default());
  Ok(composite(
    gpu,
    &state,
    (&view, (texture.width, texture.height)),
    tiles,
    [0.0; 4],
    ForeignAccess::new(),
    Grading::Neutral,
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{GpuOptions, RenderRect};

  fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::new(
      GpuContext::create_instance(),
      GpuOptions::default(),
    )) {
      Ok(gpu) => Some(gpu),
      Err(RenderError::RequestAdapter(error)) => {
        eprintln!("skipping, no GPU adapter: {error}");
        None
      }
      Err(error) => panic!("GPU context failed: {error}"),
    }
  }

  fn rect(x: i32, y: i32, width: u32, height: u32) -> RenderRect {
    RenderRect {
      x,
      y,
      width,
      height,
    }
  }

  #[test]
  fn a_view_texture_fades_as_one_and_is_graded_once() {
    let Some(mut gpu) = gpu() else {
      return;
    };
    // A red surface with an opaque blue one over its right half.
    let red = RenderTile::solid(rect(0, 0, 4, 2), [1.0, 0.0, 0.0, 1.0]);
    let blue = RenderTile::solid(rect(2, 0, 2, 2), [0.0, 0.0, 1.0, 1.0]);
    // A ramp that turns every byte around, which applied twice would
    // give the bytes back.
    let ramp = |byte: fn(u16) -> u16| {
      (0..768)
        .map(|index| byte(index % 256) * 257)
        .collect::<Vec<_>>()
    };
    gpu
      .set_gamma_ramp(&ramp(|byte| 255 - byte))
      .expect("a ramp");
    // At scale 2 the 4x2 view is 8x4 texels.
    let view = gpu.create_view_texture(8, 4).expect("a view texture");
    draw_view(&gpu, &view, 2.0, &[red, blue]).expect("drawn");
    let target = OffscreenTarget::new(&gpu, 4, 2).expect("a target");
    let draw = |gpu: &GpuContext, alpha: f32| {
      let mut tile = RenderTile::textured(rect(0, 0, 4, 2), &view);
      tile.color = [alpha; 4];
      target.render(gpu, &RenderState::default(), &[tile], [0.0; 4]);
      target.read_rgba(gpu).expect("readback")
    };
    let close = |actual: &[u8], expected: [u8; 3]| {
      actual[..3]
        .iter()
        .zip(expected)
        .all(|(&actual, expected)| actual.abs_diff(expected) <= 2)
    };
    let graded = draw(&gpu, 1.0);
    assert!(close(&graded[..4], [0, 255, 255]), "{graded:?}");
    assert!(close(&graded[12..16], [255, 255, 0]), "{graded:?}");
    // Half the view over transparent black: half red where it is red
    // and, where the blue covers it, no red showing through, as a fade
    // of the view as a whole has it. Linear 0.5 is byte 188.
    gpu
      .set_gamma_ramp(&ramp(|byte| byte))
      .expect("the identity ramp");
    let faded = draw(&gpu, 0.5);
    assert!(close(&faded[..4], [188, 0, 0]), "{faded:?}");
    assert!(close(&faded[12..16], [0, 0, 188]), "{faded:?}");
    assert!(matches!(
      draw_view(&gpu, &gpu.white_texture, 1.0, &[]),
      Err(RenderError::NotViewTexture)
    ));
  }
}
