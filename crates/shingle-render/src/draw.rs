//! The composite pass shared by every render target.

use std::mem::size_of;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::{
  ForeignAccess, GpuContext, RenderRect, RenderState, RenderTile,
  TextureTransform,
  state::Zoom,
  tile_effects::{self, ParticleInstance, SqueezeVertex, TileEffect},
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct Vertex {
  pub(crate) position: [f32; 2],
  uv: [f32; 2],
  color: [f32; 4],
  // The sampled texture's dmabuf::tile_flags.
  pub(crate) flags: u32,
  // The clip-space w the position was divided by: 1 for flat tiles, and
  // a projected tile's w' (`Projection`), so that the GPU interpolates
  // its texture coordinates with perspective.
  pub(crate) w: f32,
}

impl Vertex {
  /// A vertex at clip-space `position` with nothing else set.
  pub(crate) fn at(position: [f32; 2]) -> Self {
    Self {
      position,
      w: 1.0,
      ..Self::zeroed()
    }
  }

  pub(crate) const LAYOUT: wgpu::VertexBufferLayout<'static> =
    wgpu::VertexBufferLayout {
      array_stride: size_of::<Self>() as wgpu::BufferAddress,
      step_mode: wgpu::VertexStepMode::Vertex,
      attributes: &wgpu::vertex_attr_array![
          0 => Float32x2,
          1 => Float32x2,
          2 => Float32x4,
          3 => Uint32,
          4 => Float32,
      ],
    };
}

/// Clears `view` to `clear` and composites `tiles` over it, in order.
///
/// `size` is the target's size in physical pixels, and the pipeline is
/// chosen by the format of the view's texture. Imported DMA-BUF textures
/// among the tiles are acquired from their foreign queue family before the
/// frame and released after it ([`ForeignAccess`]). Returns the
/// submission, so callers that hand the texture to another device can wait
/// for it.
pub fn draw(
  gpu: &GpuContext,
  state: &RenderState,
  view: &wgpu::TextureView,
  size: (u32, u32),
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
) -> wgpu::SubmissionIndex {
  draw_with_access(gpu, state, view, size, tiles, clear, ForeignAccess::new())
}

/// [`draw`] with more imported images in `foreign`, such as an imported
/// DMA-BUF the view renders into (NB-8's scanout buffers), which `draw`
/// cannot see. The tiles' imported textures are added to it.
pub fn draw_with_access(
  gpu: &GpuContext,
  state: &RenderState,
  view: &wgpu::TextureView,
  size: (u32, u32),
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
  foreign: ForeignAccess,
) -> wgpu::SubmissionIndex {
  composite(
    gpu,
    state,
    (view, size),
    tiles,
    clear,
    foreign,
    Grading::Output,
  )
}

/// Whether a pass ends in the output's colour grading, the gamma ramp and
/// the inversion effect, or leaves colours as they are, for a texture
/// that a later pass draws onto an output ([`crate::draw_view`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Grading {
  /// The gamma ramp and inversion of the [`GpuContext`].
  Output,
  /// The identity ramp and no inversion.
  Neutral,
}

/// [`draw_with_access`] into `view` of `size` physical pixels, graded as
/// `grading` says.
pub(crate) fn composite(
  gpu: &GpuContext,
  state: &RenderState,
  (view, size): (&wgpu::TextureView, (u32, u32)),
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
  mut foreign: ForeignAccess,
  grading: Grading,
) -> wgpu::SubmissionIndex {
  let device = gpu.device();
  let (gamma, effects) = match grading {
    Grading::Output => (&gpu.gamma_lut.bind_group, &gpu.effects),
    Grading::Neutral => (&gpu.neutral_gamma.bind_group, &gpu.neutral_effects),
  };
  let clear = invert_clear(clear, effects.inverted, effects.preserve_hue);
  let mut encoder =
    device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
      label: Some("shingle frame encoder"),
    });
  let plan = Plan::new(tiles, size, state);
  let buffer = |label, contents: &[u8]| {
    (!contents.is_empty()).then(|| {
      device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage: wgpu::BufferUsages::VERTEX,
      })
    })
  };
  let vertex_buffer = buffer(
    "shingle quad vertices",
    bytemuck::cast_slice(&plan.vertices),
  );
  let squeeze_buffer = buffer(
    "shingle squeeze vertices",
    bytemuck::cast_slice(&plan.squeezed),
  );
  let particle_buffer =
    buffer("shingle particles", bytemuck::cast_slice(&plan.particles));
  let format = view.texture().format();
  let effect_pipelines =
    plan.has_effects().then(|| gpu.effect_pipelines(format));
  {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
      label: Some("shingle composite pass"),
      color_attachments: &[Some(wgpu::RenderPassColorAttachment {
        view,
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
      })],
      ..Default::default()
    });
    pass.set_bind_group(1, gamma, &[]);
    pass.set_bind_group(2, &effects.bind_group, &[]);
    // Which pipeline and vertex buffer are bound: the tile pipeline,
    // squeezing, or one of the two particle passes.
    let mut bound = None;
    for (tile, step) in tiles.iter().zip(&plan.steps) {
      let texture = tile.texture.unwrap_or(&gpu.white_texture);
      match *step {
        Step::Quad(first) => {
          let Some(vertex_buffer) = &vertex_buffer else {
            continue;
          };
          if bound != Some(0) {
            pass.set_pipeline(&gpu.pipeline(format));
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            bound = Some(0);
          }
          pass.set_bind_group(0, &texture.bind_group, &[]);
          pass.draw(first..first + 6, 0..1);
        }
        Step::Squeeze(first) => {
          let (Some(buffer), Some(pipelines)) =
            (&squeeze_buffer, &effect_pipelines)
          else {
            continue;
          };
          if bound != Some(1) {
            pass.set_pipeline(&pipelines.squeeze);
            pass.set_vertex_buffer(0, buffer.slice(..));
            bound = Some(1);
          }
          pass.set_bind_group(0, &texture.bind_group, &[]);
          pass.draw(first..first + 6, 0..1);
        }
        Step::Particles(first, count) => {
          let (Some(buffer), Some(pipelines)) =
            (&particle_buffer, &effect_pipelines)
          else {
            continue;
          };
          if count == 0 {
            continue;
          }
          pass.set_bind_group(0, &gpu.white_texture.bind_group, &[]);
          pass.set_vertex_buffer(0, buffer.slice(..));
          for pipeline in [&pipelines.darken, &pipelines.glow] {
            pass.set_pipeline(pipeline);
            pass.draw(0..6, first..first + count);
          }
          bound = None;
        }
      }
    }
  }
  for texture in tiles.iter().filter_map(|tile| tile.texture) {
    foreign.add_texture(texture);
  }
  foreign.submit(gpu, encoder.finish())
}

/// How each tile of a frame is drawn: the first vertex or instance of its
/// draw in the buffer of its kind.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
  Quad(u32),
  Squeeze(u32),
  Particles(u32, u32),
}

/// The vertices and instances of a frame's tiles, by kind.
struct Plan {
  steps: Vec<Step>,
  vertices: Vec<Vertex>,
  squeezed: Vec<SqueezeVertex>,
  particles: Vec<ParticleInstance>,
}

impl Plan {
  fn new(
    tiles: &[RenderTile<'_>],
    size: (u32, u32),
    state: &RenderState,
  ) -> Self {
    let mut plan = Self {
      steps: Vec::with_capacity(tiles.len()),
      vertices: Vec::with_capacity(tiles.len() * 6),
      squeezed: Vec::new(),
      particles: Vec::new(),
    };
    let (scale, zoom, transform) = (
      state.scale_factor(),
      state.zoom_state(),
      state.output_transform(),
    );
    for tile in tiles {
      let step = match tile.effect {
        TileEffect::None => {
          let first = plan.vertices.len() as u32;
          plan
            .vertices
            .extend(quad(tile, size.0, size.1, scale, zoom, transform));
          Step::Quad(first)
        }
        TileEffect::Squeeze(squeeze) => {
          let first = plan.squeezed.len() as u32;
          plan.squeezed.extend(tile_effects::squeeze_vertices(
            tile, &squeeze, size, scale, zoom, transform,
          ));
          Step::Squeeze(first)
        }
        TileEffect::Particles(particles) => {
          let first = plan.particles.len() as u32;
          plan.particles.extend(tile_effects::particle_instances(
            tile, particles, size, scale, zoom, transform,
          ));
          Step::Particles(first, plan.particles.len() as u32 - first)
        }
      };
      plan.steps.push(step);
    }
    plan
  }

  /// Whether a tile needs the effect pipelines.
  fn has_effects(&self) -> bool {
    !self.squeezed.is_empty() || !self.particles.is_empty()
  }
}

/// Applies the inversion effect to the clear colour, which the shader never
/// sees.
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

pub(crate) fn quad(
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
  // Edges on whole output pixels, as wlroots' scene rounds a node's box
  // at the output's scale, so that at a fractional scale a buffer of the
  // drawn size is still sampled texel for texel instead of half a pixel
  // off. At whole scales every edge is a whole pixel already; a zoomed
  // output scales every edge again, so there it would gain nothing.
  let unzoomed = zoom.factor == 1.0;
  let snap = |logical: i32| {
    let logical = logical as f32;
    if unzoomed {
      (logical * scale_factor).round() / scale_factor
    } else {
      logical
    }
  };
  let x0 = snap(rect.x) / output_width * 2.0 - 1.0;
  let x1 = snap(rect.x + rect.width as i32) / output_width * 2.0 - 1.0;
  let y0 = 1.0 - snap(rect.y) / output_height * 2.0;
  let y1 = 1.0 - snap(rect.y + rect.height as i32) / output_height * 2.0;
  let mut top_left = transformed_vertex(tile, x0, y0, 0.0, 0.0);
  let mut top_right = transformed_vertex(tile, x1, y0, 1.0, 0.0);
  let mut bottom_left = transformed_vertex(tile, x0, y1, 0.0, 1.0);
  let mut bottom_right = transformed_vertex(tile, x1, y1, 1.0, 1.0);
  if let Some(projection) = &tile.projection {
    let left = rect.x as f32;
    let top = rect.y as f32;
    let right = left + rect.width as f32;
    let bottom = top + rect.height as f32;
    let corners = [
      (&mut top_left, left, top),
      (&mut top_right, right, top),
      (&mut bottom_left, left, bottom),
      (&mut bottom_right, right, bottom),
    ];
    for (vertex, x, y) in corners {
      let Some(([x, y], w)) = projection.apply(x, y) else {
        // A corner behind the eye: the tile is not drawn.
        return [Vertex::zeroed(); 6];
      };
      vertex.position =
        [x / output_width * 2.0 - 1.0, 1.0 - y / output_height * 2.0];
      vertex.w = w;
    }
  }
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

pub(crate) fn zoom_crop(
  output_width: f32,
  output_height: f32,
  zoom: Zoom,
) -> (f32, f32) {
  if zoom.factor <= 1.0 {
    return (0.0, 0.0);
  }
  let visible_width = output_width / zoom.factor;
  let visible_height = output_height / zoom.factor;
  let scale = (zoom.factor - 1.0) / zoom.factor;
  (
    (zoom.origin_x * scale).clamp(0.0, (output_width - visible_width).max(0.0)),
    (zoom.origin_y * scale)
      .clamp(0.0, (output_height - visible_height).max(0.0)),
  )
}

pub(crate) fn zoom_vertex(
  vertex: &mut Vertex,
  output_width: f32,
  output_height: f32,
  zoom: Zoom,
) {
  let (crop_x, crop_y) = zoom_crop(output_width, output_height, zoom);
  vertex.position[0] = (vertex.position[0] + 1.0 - 2.0 * crop_x / output_width)
    * zoom.factor
    - 1.0;
  vertex.position[1] =
    (vertex.position[1] - 1.0 + 2.0 * crop_y / output_height) * zoom.factor
      + 1.0;
}

pub(crate) fn output_vertex(vertex: &mut Vertex, transform: TextureTransform) {
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
    TextureTransform::Rotate270 | TextureTransform::AntiTranspose => [y, -x],
    _ => [x, y],
  };
}

/// The texture coordinates `tile` samples at (`u`, `v`) of its geometry,
/// 0 to 1 from its top left corner: through its `uv` rectangle, then with
/// its buffer transform undone.
pub(crate) fn texture_coordinates(
  tile: &RenderTile<'_>,
  u: f32,
  v: f32,
) -> (f32, f32) {
  let u = tile.uv[0] + u * (tile.uv[2] - tile.uv[0]);
  let v = tile.uv[1] + v * (tile.uv[3] - tile.uv[1]);
  match tile.transform {
    TextureTransform::Normal => (u, v),
    TextureTransform::FlipHorizontal => (1.0 - u, v),
    TextureTransform::Rotate90 => (v, 1.0 - u),
    TextureTransform::FlipBoth => (1.0 - u, 1.0 - v),
    TextureTransform::Rotate270 => (1.0 - v, u),
    TextureTransform::Transpose => (v, u),
    TextureTransform::FlipVertical => (u, 1.0 - v),
    TextureTransform::AntiTranspose => (1.0 - v, 1.0 - u),
  }
}

fn transformed_vertex(
  tile: &RenderTile<'_>,
  x: f32,
  y: f32,
  u: f32,
  v: f32,
) -> Vertex {
  let (u, v) = texture_coordinates(tile, u, v);
  Vertex {
    position: [x, y],
    uv: [u, v],
    color: tile.color,
    flags: tile.texture.map_or(0, |texture| texture.flags),
    w: 1.0,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Projection;

  #[test]
  fn tile_rotation_preserves_center_and_swaps_extents() {
    let mut tile = RenderTile::solid(
      RenderRect {
        x: 10,
        y: 20,
        width: 100,
        height: 50,
      },
      [1.0; 4],
    );
    tile.rotation = std::f32::consts::FRAC_PI_2;
    let vertices = quad(
      &tile,
      1280,
      720,
      1.0,
      Zoom::default(),
      TextureTransform::Normal,
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

  #[test]
  fn full_output_tile_covers_clip_space() {
    let tile = RenderTile::solid(
      RenderRect {
        x: 0,
        y: 0,
        width: 640,
        height: 360,
      },
      [1.0; 4],
    );
    // At scale 2 the logical tile fills the 1280x720 physical output.
    let vertices = quad(
      &tile,
      1280,
      720,
      2.0,
      Zoom::default(),
      TextureTransform::Normal,
    );
    assert_eq!(vertices[0].position, [-1.0, 1.0]);
    assert_eq!(vertices[5].position, [1.0, -1.0]);
  }

  #[test]
  fn fractional_scales_put_tile_edges_on_whole_pixels() {
    let tile = RenderTile::solid(
      RenderRect {
        x: 201,
        y: 141,
        width: 80,
        height: 61,
      },
      [1.0; 4],
    );
    // 1920x1080 output pixels at scale 1.5; logical 201 is pixel 301.5.
    let vertices = quad(
      &tile,
      1920,
      1080,
      1.5,
      Zoom::default(),
      TextureTransform::Normal,
    );
    let pixel_x = |clip: f32| (clip + 1.0) / 2.0 * 1920.0;
    let pixel_y = |clip: f32| (1.0 - clip) / 2.0 * 1080.0;
    let close = |value: f32, expected: f32| (value - expected).abs() < 0.01;
    assert!(close(pixel_x(vertices[0].position[0]), 302.0));
    assert!(close(pixel_y(vertices[0].position[1]), 212.0));
    assert!(close(pixel_x(vertices[5].position[0]), 422.0));
    assert!(close(pixel_y(vertices[5].position[1]), 303.0));
  }

  /// `matrix` as a column-major [`Projection`] around (`x`, `y`).
  fn projection(x: f32, y: f32, matrix: [[f32; 4]; 4]) -> Projection {
    Projection {
      origin: [x, y],
      matrix,
    }
  }

  const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
  ];

  #[test]
  fn an_identity_projection_draws_the_tile_flat() {
    let rect = RenderRect {
      x: 100,
      y: 200,
      width: 300,
      height: 100,
    };
    let flat = RenderTile::solid(rect, [1.0; 4]);
    let mut projected = RenderTile::solid(rect, [1.0; 4]);
    projected.projection = Some(projection(250.0, 250.0, IDENTITY));
    let flat = quad(&flat, 1000, 800, 1.0, Zoom::default(), Default::default());
    let projected = quad(
      &projected,
      1000,
      800,
      1.0,
      Zoom::default(),
      Default::default(),
    );
    for (flat, projected) in flat.iter().zip(&projected) {
      for axis in 0..2 {
        assert!((flat.position[axis] - projected.position[axis]).abs() < 1e-6);
      }
      assert_eq!(projected.w, 1.0);
    }
  }

  #[test]
  fn a_projection_divides_by_w_and_keeps_it() {
    // Scales by 2 around the origin, then w = 2 halves it again, and a
    // translation of (10, 20) pixels with y up, scaled by w as well.
    let mut matrix = IDENTITY;
    matrix[0][0] = 2.0;
    matrix[1][1] = 2.0;
    matrix[3][0] = 20.0;
    matrix[3][1] = 40.0;
    matrix[3][3] = 2.0;
    let projection = projection(500.0, 400.0, matrix);
    let (point, w) = projection.apply(600.0, 300.0).expect("in front");
    assert_eq!(w, 2.0);
    assert!((point[0] - 610.0).abs() < 1e-4, "{point:?}");
    assert!((point[1] - 280.0).abs() < 1e-4, "{point:?}");
    let mut tile = RenderTile::solid(
      RenderRect {
        x: 500,
        y: 400,
        width: 100,
        height: 100,
      },
      [1.0; 4],
    );
    tile.projection = Some(projection);
    let vertices =
      quad(&tile, 1000, 800, 1.0, Zoom::default(), Default::default());
    // The top left corner, at the origin, only moves by the translation.
    assert!((vertices[0].position[0] - (510.0 / 500.0 - 1.0)).abs() < 1e-6);
    assert!((vertices[0].position[1] - (1.0 - 380.0 / 400.0)).abs() < 1e-6);
    assert!(vertices.iter().all(|vertex| vertex.w == 2.0));
  }

  #[test]
  fn a_tile_with_a_corner_behind_the_eye_is_not_drawn() {
    // w = 1 - x / 50 is negative right of x = 50.
    let mut matrix = IDENTITY;
    matrix[0][3] = -0.02;
    let mut tile = RenderTile::solid(
      RenderRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
      },
      [1.0; 4],
    );
    tile.projection = Some(projection(0.0, 0.0, matrix));
    let vertices =
      quad(&tile, 1000, 800, 1.0, Zoom::default(), Default::default());
    assert!(vertices.iter().all(|vertex| vertex.position == [0.0, 0.0]));
  }

  #[test]
  fn clear_inversion_matches_shader() {
    let color = [0.2, 0.5, 0.7, 1.0];
    assert_eq!(invert_clear(color, false, true), color);
    let inverted = invert_clear(color, true, false);
    assert!((inverted[0] - 0.8).abs() < 1e-9);
    assert!((inverted[2] - 0.3).abs() < 1e-9);
    assert_eq!(inverted[3], 1.0);
    // Hue preservation shifts every channel by alpha - min - max.
    let preserved = invert_clear(color, true, true);
    assert!((preserved[0] - 0.3).abs() < 1e-9);
    assert!((preserved[1] - 0.6).abs() < 1e-9);
    assert!((preserved[2] - 0.8).abs() < 1e-9);
  }
}
