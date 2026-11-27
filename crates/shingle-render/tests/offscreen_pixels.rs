//! Pixel regression tests for the offscreen render path.
//!
//! Each test renders a fixed scene and compares the RGBA readback with a
//! reference image in `tests/golden`. The references were rendered by the
//! renderer before it was split into `GpuContext`, `RenderState`, `draw` and
//! targets, so they also prove the split changed no pixels. Set
//! `SHINGLE_RENDER_BLESS=1` to write new reference images instead of
//! comparing. Tests are skipped when the machine has no wgpu adapter.
//!
//! Every test draws with one GPU context, which they take in turn: with a
//! device of its own per test, NVIDIA's driver crashed now and then
//! (SIGSEGV) while the tests ran in parallel, as the DB-4 cache tests
//! did (crates/shingle-frontend/src/dmabuf/tests.rs).

use std::{
  path::PathBuf,
  sync::{Mutex, MutexGuard, OnceLock, PoisonError},
};

use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, OffscreenRenderer, OffscreenTarget,
  RenderError, RenderRect, RenderState, RenderTile, ShmImage, TextureFilter,
  TextureTransform, dmabuf, draw,
};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 64;
const CLEAR: [f64; 4] = [0.05, 0.1, 0.2, 1.0];

/// Largest per-channel difference that still counts as a match.
const CHANNEL_TOLERANCE: u8 = 2;

/// Pixels allowed to exceed the tolerance, for rasteriser edge differences
/// between GPUs.
const MAX_OUTLIERS: usize = (WIDTH * HEIGHT / 100) as usize;

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

/// The identity gamma ramp: 256 values each for red, green and blue.
fn identity_ramp() -> Vec<u16> {
  (0..3)
    .flat_map(|_| (0..256u16).map(|value| value * 257))
    .collect()
}

/// The shared GPU context, for this test alone until the guard drops,
/// with the identity gamma ramp and no inversion; `None` without an
/// adapter. The context lives as long as the test process.
fn gpu() -> Option<MutexGuard<'static, GpuContext>> {
  static GPU: OnceLock<Option<Mutex<GpuContext>>> = OnceLock::new();
  let gpu = GPU.get_or_init(|| {
    let instance = GpuContext::create_instance();
    match pollster::block_on(GpuContext::new(instance, GpuOptions::default())) {
      Ok(gpu) => Some(Mutex::new(gpu)),
      Err(RenderError::RequestAdapter(error)) => {
        eprintln!("skipping pixel test, no GPU adapter: {error}");
        None
      }
      Err(error) => panic!("GPU context failed: {error}"),
    }
  });
  let mut gpu = gpu.as_ref()?.lock().unwrap_or_else(PoisonError::into_inner);
  gpu.set_inversion(false, false);
  gpu.set_gamma_ramp(&identity_ramp()).expect("identity ramp");
  Some(gpu)
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

/// Draws `tiles` over `clear` into a new `width` by `height` target and
/// reads it back.
fn draw_frame(
  gpu: &GpuContext,
  state: &RenderState,
  (width, height): (u32, u32),
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
) -> Vec<u8> {
  let target = OffscreenTarget::new(gpu, width, height).expect("target");
  target.render(gpu, state, tiles, clear);
  target.read_rgba(gpu).expect("readback")
}

/// The fixed scene, with the checker texture uploaded with `state`'s
/// filter, as `OffscreenRenderer` draws it.
fn scene(gpu: &GpuContext, state: &RenderState) -> Vec<u8> {
  let texture = gpu
    .create_texture(4, 4, &checker_rgba(), state.texture_filter())
    .expect("texture upload");
  draw_frame(gpu, state, (WIDTH, HEIGHT), &scene_tiles(&texture), CLEAR)
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
  let Some(gpu) = gpu() else {
    return;
  };
  let rgba = scene(&gpu, &RenderState::default());
  check_golden("plain", &rgba);
}

#[test]
fn scaled_transformed_zoomed_scene_matches_reference() {
  let Some(gpu) = gpu() else {
    return;
  };
  let mut state = RenderState::default();
  state.set_scale_factor(1.25);
  state.set_output_transform(TextureTransform::Rotate90);
  state.set_zoom(1.3, 60.0, 40.0);
  let rgba = scene(&gpu, &state);
  check_golden("scaled_transformed_zoomed", &rgba);
}

#[test]
fn gamma_and_hue_inversion_match_reference() {
  let Some(mut gpu) = gpu() else {
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
  gpu.set_gamma_ramp(&ramp).expect("gamma ramp");
  gpu.set_inversion(true, true);
  let rgba = scene(&gpu, &RenderState::default());
  check_golden("gamma_hue_inverted", &rgba);
}

#[test]
fn nearest_filter_and_inversion_match_reference() {
  let Some(mut gpu) = gpu() else {
    return;
  };
  let mut state = RenderState::default();
  assert!(state.set_nearest_filter(true));
  assert!(!state.set_nearest_filter(true));
  gpu.set_inversion(true, false);
  let rgba = scene(&gpu, &state);
  check_golden("nearest_inverted", &rgba);
}

/// A decorated window as the frontend draws it: the checker as its
/// client, inside a frame with Wayfire's translucent active colour, a
/// title and three buttons, the close button hovered, on a small theme
/// that fits the scene.
fn frame_scene(gpu: &GpuContext, state: &RenderState) -> Vec<u8> {
  use shingle::frame::{FrameLayout, FrameSnapshot, FrameTheme};
  use shingle_frontend::frame_raster::{self, FramePiece, TitleStyle};
  let config = shingle_config::DecorationConfig {
    title_height: 14,
    border_size: 2,
    button_padding: 2,
    ..shingle_config::DecorationConfig::default()
  };
  let theme = FrameTheme::new(&config);
  let outer = shingle_core::Rect::new(2, 2, 92, 60);
  let frame = FrameSnapshot {
    window: shingle::WindowId(1),
    rect: outer,
    margins: theme.margins(),
    activated: true,
    title: "Shingle".into(),
    layout: FrameLayout::new(&theme, outer.width, outer.height, false, true),
    hovered: Some(0),
    pressed: None,
  };
  let filter = state.texture_filter();
  let checker = gpu
    .create_texture(4, 4, &checker_rgba(), filter)
    .expect("texture upload");
  let pieces = frame_raster::frame_pieces(&frame, &config);
  let textures = pieces
    .iter()
    .map(|piece| match piece {
      FramePiece::Fill { .. } => None,
      FramePiece::Title { rect } => frame_raster::rasterize_title(
        &frame.title,
        rect.width,
        rect.height,
        &TitleStyle::new(&config),
      )
      .map(|raster| {
        gpu
          .create_texture(raster.width, raster.height, &raster.rgba, filter)
          .expect("title upload")
      }),
      FramePiece::Button { button, look, .. } => {
        let size = config.title_height;
        let rgba = frame_raster::rasterize_button(*button, *look, size);
        Some(
          gpu
            .create_texture(size, size, &rgba, filter)
            .expect("button upload"),
        )
      }
    })
    .collect::<Vec<_>>();
  let at = |rect: shingle_core::Rect| RenderRect {
    x: rect.x,
    y: rect.y,
    width: rect.width,
    height: rect.height,
  };
  let mut tiles = pieces
    .iter()
    .zip(&textures)
    .filter_map(|(piece, texture)| match piece {
      FramePiece::Fill { rect, color } => {
        Some(RenderTile::solid(at(*rect), *color))
      }
      FramePiece::Title { rect } | FramePiece::Button { rect, .. } => {
        Some(RenderTile::textured(at(*rect), texture.as_ref()?))
      }
    })
    .collect::<Vec<_>>();
  tiles.push(RenderTile::textured(
    at(frame.margins.shrink(outer)),
    &checker,
  ));
  draw_frame(gpu, state, (WIDTH, HEIGHT), &tiles, CLEAR)
}

#[test]
fn a_decorated_window_matches_reference() {
  let Some(gpu) = gpu() else {
    return;
  };
  let rgba = frame_scene(&gpu, &RenderState::default());
  check_golden("frame", &rgba);
}

#[test]
fn empty_scene_is_clear_color() {
  let Some(gpu) = gpu() else {
    return;
  };
  let rgba =
    draw_frame(&gpu, &RenderState::default(), (WIDTH, HEIGHT), &[], CLEAR);
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

/// `rgba` (tightly packed RGBA rows of a `width`-pixel image) as `fourcc`
/// bytes in rows `stride` bytes apart, with `alpha` in every pixel of an
/// X format and 0xee in the padding.
fn shm_bytes(
  rgba: &[u8],
  width: usize,
  stride: usize,
  fourcc: u32,
  alpha: u8,
) -> Vec<u8> {
  let rows = rgba.len() / (width * 4);
  let mut bytes = vec![0xee; stride * (rows - 1) + width * 4];
  for (index, pixel) in rgba.chunks_exact(4).enumerate() {
    let at = index / width * stride + index % width * 4;
    let [r, g, b, a] = [pixel[0], pixel[1], pixel[2], pixel[3]];
    bytes[at..at + 4].copy_from_slice(&match fourcc {
      dmabuf::DRM_FORMAT_ARGB8888 => [b, g, r, a],
      dmabuf::DRM_FORMAT_XRGB8888 => [b, g, r, alpha],
      dmabuf::DRM_FORMAT_ABGR8888 => [r, g, b, a],
      _ => [r, g, b, alpha],
    });
  }
  bytes
}

/// Draws `texture` 1:1 into a 4x4 target with nearest filtering.
fn texels(gpu: &GpuContext, texture: &GpuTexture) -> Vec<u8> {
  let mut state = RenderState::default();
  state.set_nearest_filter(true);
  let tile = RenderTile::textured(rect(0, 0, 4, 4), texture);
  draw_frame(gpu, &state, (4, 4), &[tile], [0.0; 4])
}

#[test]
fn shm_uploads_match_rgba_uploads_in_every_format_and_stride() {
  let Some(gpu) = gpu() else {
    return;
  };
  let checker = checker_rgba();
  let reference = gpu
    .create_texture(4, 4, &checker, TextureFilter::Nearest)
    .expect("texture upload");
  let expected = texels(&gpu, &reference);
  let uploader = gpu.texture_uploader();
  for fourcc in dmabuf::DRM_FORMATS.map(|format| format.fourcc) {
    // A padded stride that is not a multiple of 16, and alpha 0 in the
    // X formats, which the opaque flag must ignore.
    let bytes = shm_bytes(&checker, 4, 20, fourcc, 0);
    let image = ShmImage {
      width: 4,
      height: 4,
      stride: 20,
      fourcc,
      bytes: &bytes,
    };
    let texture = uploader
      .create(&image, TextureFilter::Nearest)
      .expect("shm texture");
    assert!(uploader.fits(&texture, &image));
    assert_eq!(uploader.write(&texture, &image, &[[0, 0, 4, 4]]), Ok(64));
    let name = dmabuf::drm_format(fourcc).unwrap().name;
    assert_eq!(texels(&gpu, &texture), expected, "{name}");
    // A filter change keeps the texels.
    let linear = uploader.rebind(&texture, TextureFilter::Linear);
    assert_eq!(texels(&gpu, &linear), expected, "{name} rebound");
  }
}

/// shm uploads go through staging buffers in system memory, not the
/// device-local, host-visible memory `Queue::write_texture` prefers
/// (docs/status.md#shm-upload-speed), and many uploads reuse a few
/// buffers instead of making one each.
#[test]
fn shm_uploads_stage_in_system_memory_and_reuse_their_buffers() {
  let Some(gpu) = gpu() else {
    return;
  };
  let uploader = gpu.texture_uploader();
  let Some(before) = uploader.staging_stats() else {
    panic!("a Vulkan device has staging buffers");
  };
  let fourcc = dmabuf::DRM_FORMAT_XRGB8888;
  let checker = checker_rgba();
  // A stride that is no multiple of the copy alignment.
  let bytes = shm_bytes(&checker, 4, 20, fourcc, 0);
  let image = ShmImage {
    width: 4,
    height: 4,
    stride: 20,
    fourcc,
    bytes: &bytes,
  };
  let texture = uploader
    .create(&image, TextureFilter::Nearest)
    .expect("shm texture");
  let reference = gpu
    .create_texture(4, 4, &checker, TextureFilter::Nearest)
    .expect("texture upload");
  let expected = texels(&gpu, &reference);
  for round in 0..40 {
    // Two rectangles a commit, then the whole buffer now and then.
    let rects: &[[u32; 4]] = if round % 10 == 0 {
      &[[0, 0, 4, 4]]
    } else {
      &[[0, 0, 2, 1], [1, 2, 3, 2]]
    };
    uploader.write(&texture, &image, rects).expect("write");
    if round % 4 == 0 {
      assert_eq!(texels(&gpu, &texture), expected, "round {round}");
    }
  }
  assert_eq!(texels(&gpu, &texture), expected);
  let after = uploader.staging_stats().expect("staging");
  assert!(
    after.created - before.created <= 4,
    "40 uploads made {} staging buffers",
    after.created - before.created
  );
  assert!(after.kept <= 8, "{} buffers kept", after.kept);
  if after.system_memory {
    assert!(
      !after.device_local_heap,
      "staged in video memory: {after:?}"
    );
  }
}

#[test]
fn a_damaged_rectangle_replaces_only_its_texels() {
  let Some(gpu) = gpu() else {
    return;
  };
  let uploader = gpu.texture_uploader();
  let fourcc = dmabuf::DRM_FORMAT_XRGB8888;
  let checker = checker_rgba();
  let first = shm_bytes(&checker, 4, 16, fourcc, 0);
  let image = |bytes| ShmImage {
    width: 4,
    height: 4,
    stride: 16,
    fourcc,
    bytes,
  };
  let texture = uploader
    .create(&image(&first), TextureFilter::Nearest)
    .expect("shm texture");
  uploader
    .write(&texture, &image(&first), &[[0, 0, 4, 4]])
    .expect("full write");
  // The client draws white everywhere but damages only the middle.
  let white = shm_bytes(&[255; 64], 4, 16, fourcc, 255);
  assert_eq!(
    uploader.write(&texture, &image(&white), &[[1, 1, 2, 2]]),
    Ok(16)
  );
  let mut expected = checker.clone();
  for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
    let at = (y * 4 + x) * 4;
    expected[at..at + 4].copy_from_slice(&[255; 4]);
  }
  let reference = gpu
    .create_texture(4, 4, &expected, TextureFilter::Nearest)
    .expect("texture upload");
  assert_eq!(texels(&gpu, &texture), texels(&gpu, &reference));
  // The GPU copy a geometry animation keeps has the same texels.
  let copy = uploader
    .duplicate(&texture, TextureFilter::Nearest)
    .expect("a copy");
  assert_eq!(texels(&gpu, &copy), texels(&gpu, &reference));
  // A texture of another size or byte order does not fit.
  let mut other = image(&white);
  other.fourcc = dmabuf::DRM_FORMAT_ABGR8888;
  assert!(!uploader.fits(&texture, &other));
  assert!(uploader.write(&texture, &other, &[[0, 0, 1, 1]]).is_err());
}

/// The RGBA of pixel (`x`, `y`) of a `WIDTH` wide frame.
fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
  let start = ((y * WIDTH + x) * 4) as usize;
  rgba[start..start + 4].try_into().expect("four channels")
}

/// A squeezed tile over the whole frame whose window is the left half at
/// `progress`, going towards a small box at the bottom centre.
fn squeezed<'a>(texture: &'a GpuTexture, progress: f32) -> RenderTile<'a> {
  let mut tile = RenderTile::textured(rect(0, 0, WIDTH, HEIGHT), texture);
  tile.effect = shingle_render::TileEffect::Squeeze(shingle_render::Squeeze {
    progress,
    source: [0.0, 0.0, 0.5, 0.75],
    target: [0.4, 0.9, 0.6, 1.0],
    upward: false,
    horizontal: false,
  });
  tile
}

#[test]
fn a_squeeze_starts_as_the_window_and_ends_in_its_target() {
  let Some(gpu) = gpu() else {
    return;
  };
  let state = RenderState::default();
  let texture = gpu
    .create_texture(4, 4, &checker_rgba(), state.texture_filter())
    .expect("texture upload");
  // At 0 the warp is the window, where a plain tile would draw it.
  let start = draw_frame(
    &gpu,
    &state,
    (WIDTH, HEIGHT),
    &[squeezed(&texture, 0.0)],
    CLEAR,
  );
  let plain = draw_frame(
    &gpu,
    &state,
    (WIDTH, HEIGHT),
    &[RenderTile::textured(
      rect(0, 0, WIDTH / 2, HEIGHT * 3 / 4),
      &texture,
    )],
    CLEAR,
  );
  for (x, y) in [(4, 4), (40, 4), (4, 40), (40, 40), (70, 10), (70, 60)] {
    let (actual, expected) = (pixel(&start, x, y), pixel(&plain, x, y));
    let diff = actual
      .iter()
      .zip(expected)
      .map(|(a, e)| a.abs_diff(e))
      .max()
      .unwrap();
    assert!(diff <= 3, "({x}, {y}): {actual:?} is not {expected:?}");
  }
  // At 1 the window has gone into the box: its top corner is clear.
  let end = draw_frame(
    &gpu,
    &state,
    (WIDTH, HEIGHT),
    &[squeezed(&texture, 1.0)],
    CLEAR,
  );
  assert_eq!(pixel(&end, 4, 4), pixel(&plain, 80, 10));
}

#[test]
fn particles_darken_then_light_what_is_below() {
  let Some(gpu) = gpu() else {
    return;
  };
  let state = RenderState::default();
  let particles = [shingle_render::Particle {
    center: [8.0, 8.0],
    radius: 8.0,
    color: [1.0, 0.5, 0.0, 1.0],
  }];
  let background = RenderTile::solid(rect(0, 0, WIDTH, HEIGHT), [0.5; 4]);
  let mut fire = RenderTile::solid(rect(20, 20, 0, 0), [1.0; 4]);
  fire.effect = shingle_render::TileEffect::Particles(&particles);
  let rgba = draw_frame(
    &gpu,
    &state,
    (WIDTH, HEIGHT),
    &[background.clone(), fire],
    CLEAR,
  );
  let plain = draw_frame(&gpu, &state, (WIDTH, HEIGHT), &[background], CLEAR);
  // The middle of the particle, at (28, 28), turns orange; beyond its
  // radius nothing changes.
  let middle = pixel(&rgba, 28, 28);
  let below = pixel(&plain, 28, 28);
  assert!(middle[0] > below[0], "{middle:?} over {below:?}");
  assert!(middle[2] < below[2], "{middle:?} over {below:?}");
  assert_eq!(pixel(&rgba, 28, 40), pixel(&plain, 28, 40));
  // 0.75 of the radius wide: 8 pixels to the side is outside.
  assert_eq!(pixel(&rgba, 36, 28), pixel(&plain, 36, 28));
}
