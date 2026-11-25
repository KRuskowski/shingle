//! Pixel regression tests for the offscreen render path.
//!
//! Each test renders a fixed scene and compares the RGBA readback with a
//! reference image in `tests/golden`. The references were rendered by the
//! renderer before it was split into `GpuContext`, `RenderState`, `draw` and
//! targets, so they also prove the split changed no pixels. Set
//! `SHINGLE_RENDER_BLESS=1` to write new reference images instead of
//! comparing. Tests are skipped when the machine has no wgpu adapter.

use std::path::PathBuf;

use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, OffscreenRenderer, OffscreenTarget,
  RenderError, RenderRect, RenderState, RenderTile, TextureFilter,
  TextureTransform, draw,
};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 64;
const CLEAR: [f64; 4] = [0.05, 0.1, 0.2, 1.0];

/// Largest per-channel difference that still counts as a match.
const CHANNEL_TOLERANCE: u8 = 2;

/// Pixels allowed to exceed the tolerance, for rasteriser edge differences
/// between GPUs.
const MAX_OUTLIERS: usize = (WIDTH * HEIGHT / 100) as usize;

fn renderer() -> Option<OffscreenRenderer> {
  match pollster::block_on(OffscreenRenderer::new(WIDTH, HEIGHT)) {
    Ok(renderer) => Some(renderer),
    Err(RenderError::RequestAdapter(error)) => {
      eprintln!("skipping pixel test, no GPU adapter: {error}");
      None
    }
    Err(error) => panic!("offscreen renderer failed: {error}"),
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

/// A 4x4 texture with a distinct colour in every texel.
fn checker_rgba() -> Vec<u8> {
  (0..16u8)
    .flat_map(|index| {
      let x = index % 4;
      let y = index / 4;
      [x * 80, y * 80, 255 - x * 40 - y * 20, 255]
    })
    .collect()
}

fn gpu() -> Option<GpuContext> {
  let instance = GpuContext::create_instance();
  match pollster::block_on(GpuContext::new(instance, GpuOptions::default())) {
    Ok(gpu) => Some(gpu),
    Err(RenderError::RequestAdapter(error)) => {
      eprintln!("skipping pixel test, no GPU adapter: {error}");
      None
    }
    Err(error) => panic!("GPU context failed: {error}"),
  }
}

/// Overlapping solid, translucent, textured, transformed and rotated tiles.
fn scene_tiles(texture: &GpuTexture) -> Vec<RenderTile<'_>> {
  let mut transformed = RenderTile::textured(rect(52, 6, 36, 24), texture);
  transformed.transform = TextureTransform::Rotate90;
  transformed.uv = [0.25, 0.0, 1.0, 0.75];
  let mut rotated =
    RenderTile::solid(rect(10, 36, 30, 14), [0.9, 0.8, 0.1, 1.0]);
  rotated.rotation = 0.5;
  vec![
    RenderTile::solid(rect(4, 4, 40, 26), [0.8, 0.1, 0.1, 1.0]),
    RenderTile::solid(rect(20, 14, 40, 30), [0.0, 0.4, 0.0, 0.5]),
    RenderTile::textured(rect(60, 34, 32, 26), texture),
    transformed,
    rotated,
  ]
}

fn scene(renderer: &mut OffscreenRenderer) -> Vec<u8> {
  let texture = renderer
    .create_texture(4, 4, &checker_rgba())
    .expect("texture upload");
  renderer
    .render(&scene_tiles(&texture), CLEAR)
    .expect("offscreen render")
}

fn golden_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/golden")
    .join(format!("{name}.rgba"))
}

/// Compares `rgba` with the reference image `name`, or writes it when
/// blessing.
fn check_golden(name: &str, rgba: &[u8]) {
  assert_eq!(rgba.len(), (WIDTH * HEIGHT * 4) as usize);
  let path = golden_path(name);
  if std::env::var_os("SHINGLE_RENDER_BLESS").is_some() {
    std::fs::create_dir_all(path.parent().expect("golden dir"))
      .expect("create golden dir");
    std::fs::write(&path, rgba).expect("write golden image");
    return;
  }
  let expected = std::fs::read(&path)
    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
  assert_eq!(expected.len(), rgba.len(), "{name}: size mismatch");
  let mut outliers = 0;
  let mut max_diff = 0;
  for (actual, expected) in rgba.chunks_exact(4).zip(expected.chunks_exact(4)) {
    let diff = actual
      .iter()
      .zip(expected)
      .map(|(a, e)| a.abs_diff(*e))
      .max()
      .unwrap_or(0);
    max_diff = max_diff.max(diff);
    if diff > CHANNEL_TOLERANCE {
      outliers += 1;
    }
  }
  eprintln!("{name}: max channel diff {max_diff}, {outliers} outliers");
  assert!(
    outliers <= MAX_OUTLIERS,
    "{name}: {outliers} pixels differ by more than {CHANNEL_TOLERANCE}"
  );
}

#[test]
fn plain_scene_matches_reference() {
  let Some(mut renderer) = renderer() else {
    return;
  };
  let rgba = scene(&mut renderer);
  check_golden("plain", &rgba);
}

#[test]
fn scaled_transformed_zoomed_scene_matches_reference() {
  let Some(mut renderer) = renderer() else {
    return;
  };
  renderer.set_scale_factor(1.25);
  renderer.set_output_transform(TextureTransform::Rotate90);
  renderer.set_zoom(1.3, 60.0, 40.0);
  let rgba = scene(&mut renderer);
  check_golden("scaled_transformed_zoomed", &rgba);
}

#[test]
fn gamma_and_hue_inversion_match_reference() {
  let Some(mut renderer) = renderer() else {
    return;
  };
  let ramp = (0..3)
    .flat_map(|channel| {
      (0..256u32).map(move |value| {
        let scaled = value * 257;
        match channel {
          0 => (scaled * 3 / 4) as u16,
          1 => scaled as u16,
          _ => (65535 - scaled) as u16,
        }
      })
    })
    .collect::<Vec<_>>();
  renderer.set_gamma_ramp(&ramp).expect("gamma ramp");
  renderer.set_inversion(true, true);
  let rgba = scene(&mut renderer);
  check_golden("gamma_hue_inverted", &rgba);
}

#[test]
fn nearest_filter_and_inversion_match_reference() {
  let Some(mut renderer) = renderer() else {
    return;
  };
  assert!(renderer.set_nearest_filter(true));
  assert!(!renderer.set_nearest_filter(true));
  renderer.set_inversion(true, false);
  let rgba = scene(&mut renderer);
  check_golden("nearest_inverted", &rgba);
}

#[test]
fn empty_scene_is_clear_color() {
  let Some(mut renderer) = renderer() else {
    return;
  };
  let rgba = renderer.render(&[], CLEAR).expect("offscreen render");
  // The target is sRGB, so the linear clear colour is encoded on readback.
  let first = &rgba[..4];
  assert!(rgba.chunks_exact(4).all(|pixel| pixel == first));
  assert_eq!(first[3], 255);
}

#[test]
fn draw_into_offscreen_target_matches_reference() {
  let Some(gpu) = gpu() else {
    return;
  };
  let state = RenderState::default();
  let target = OffscreenTarget::new(&gpu, WIDTH, HEIGHT).expect("target");
  let texture = gpu
    .create_texture(4, 4, &checker_rgba(), TextureFilter::Linear)
    .expect("texture upload");
  draw(
    &gpu,
    &state,
    target.view(),
    target.size(),
    &scene_tiles(&texture),
    CLEAR,
  );
  let rgba = target.read_rgba(&gpu).expect("readback");
  check_golden("plain", &rgba);
}

#[test]
fn one_context_serves_targets_with_separate_state() {
  let Some(mut gpu) = gpu() else {
    return;
  };
  let texture = gpu
    .create_texture(4, 4, &checker_rgba(), TextureFilter::Linear)
    .expect("texture upload");
  let plain = OffscreenTarget::new(&gpu, WIDTH, HEIGHT).expect("target");
  let zoomed = OffscreenTarget::new(&gpu, WIDTH, HEIGHT).expect("target");
  let small = OffscreenTarget::new(&gpu, 7, 5).expect("target");
  let mut zoom_state = RenderState::default();
  zoom_state.set_scale_factor(1.25);
  zoom_state.set_output_transform(TextureTransform::Rotate90);
  zoom_state.set_zoom(1.3, 60.0, 40.0);
  let tiles = scene_tiles(&texture);
  plain.render(&gpu, &RenderState::default(), &tiles, CLEAR);
  zoomed.render(&gpu, &zoom_state, &tiles, CLEAR);
  small.render(&gpu, &RenderState::default(), &[], [1.0, 1.0, 1.0, 1.0]);
  check_golden("plain", &plain.read_rgba(&gpu).expect("readback"));
  check_golden(
    "scaled_transformed_zoomed",
    &zoomed.read_rgba(&gpu).expect("readback"),
  );
  let white = small.read_rgba(&gpu).expect("readback");
  assert_eq!(white.len(), 7 * 5 * 4);
  assert!(white.iter().all(|&byte| byte == 255));
  // Effects live in the context and apply to every target drawn after.
  gpu.set_inversion(true, false);
  small.render(&gpu, &RenderState::default(), &[], [1.0, 1.0, 1.0, 1.0]);
  let black = small.read_rgba(&gpu).expect("readback");
  assert!(black.chunks_exact(4).all(|pixel| pixel == [0, 0, 0, 255]));
}

#[test]
fn zero_sized_targets_are_rejected() {
  let Some(gpu) = gpu() else {
    return;
  };
  assert!(matches!(
    OffscreenTarget::new(&gpu, 0, 4),
    Err(RenderError::EmptyTexture)
  ));
  assert!(matches!(
    pollster::block_on(OffscreenRenderer::new(4, 0)),
    Err(RenderError::EmptyTexture)
  ));
}
