//! Tiles drawn with shaders of their own: the warp of Wayfire's
//! squeezimize animation and the particles of its fire animation. Their
//! pipelines are built the first time a frame has such a tile, so frames
//! without one cost what they did before.

use std::mem::size_of;

use bytemuck::{Pod, Zeroable};

use crate::{
  RenderTile, TextureTransform,
  draw::{self, Vertex},
  state::Zoom,
};

/// What a [`RenderTile`] draws in place of its plain quad.
#[derive(Clone, Copy, Debug, Default)]
pub enum TileEffect<'a> {
  /// The quad, textured or solid.
  #[default]
  None,
  /// The tile's texture squeezed towards a rectangle ([`Squeeze`]). The
  /// tile's geometry is the area the warp may cover, which holds both the
  /// window and the rectangle; its `uv` and `transform` sample the window
  /// as a plain tile would. Rotation and projection are ignored.
  Squeeze(Squeeze),
  /// Round particles blended over what is below: first darkening it, then
  /// adding their colour, as Wayfire's fire draws them
  /// (wayfire:plugins/animate/fire/particle.cpp:290-338). Centres are in
  /// logical pixels from the tile's top left corner; the tile's size,
  /// texture and colour are not used.
  Particles(&'a [Particle]),
}

/// The uniforms of Wayfire's squeezimize shader
/// (wayfire:plugins/animate/squeezimize.hpp:68-174), in the coordinates
/// it gives them: fractions of the tile's geometry, `[left, top, right,
/// bottom]` with `y` down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Squeeze {
  /// How far the window has gone, 0 (whole) to 1 (inside `target`).
  pub progress: f32,
  /// Where the window is, `src_box`.
  pub source: [f32; 4],
  /// Where it goes, `target_box`.
  pub target: [f32; 4],
  /// `upward`: the target is above the window (or, horizontally, left of
  /// it).
  pub upward: bool,
  /// The target is on the left or right edge: the horizontal shader.
  pub horizontal: bool,
}

/// One particle of [`TileEffect::Particles`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct Particle {
  /// The centre, in logical pixels from the tile's origin.
  pub center: [f32; 2],
  /// The radius in logical pixels; the particle is 0.75 of it wide.
  pub radius: f32,
  /// Linear colour, not premultiplied: `rgb` is added times `a`, and the
  /// darkening takes half of each.
  pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct SqueezeVertex {
  position: [f32; 2],
  // Where the vertex is in the tile, 0 to 1 with y up, as Wayfire's
  // `uv_in`.
  box_uv: [f32; 2],
  source: [f32; 4],
  target: [f32; 4],
  // progress, upward, horizontal, unused.
  params: [f32; 4],
  // The texture coordinates of the window's top left corner, and their
  // change across its width and down its height.
  tex_origin: [f32; 2],
  tex_u: [f32; 2],
  tex_v: [f32; 2],
  color: [f32; 4],
  flags: u32,
}

impl SqueezeVertex {
  pub(crate) const LAYOUT: wgpu::VertexBufferLayout<'static> =
    wgpu::VertexBufferLayout {
      array_stride: size_of::<Self>() as wgpu::BufferAddress,
      step_mode: wgpu::VertexStepMode::Vertex,
      attributes: &wgpu::vertex_attr_array![
          0 => Float32x2,
          1 => Float32x2,
          2 => Float32x4,
          3 => Float32x4,
          4 => Float32x4,
          5 => Float32x2,
          6 => Float32x2,
          7 => Float32x2,
          8 => Float32x4,
          9 => Uint32,
      ],
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct ParticleInstance {
  // The centre in clip space, and the clip-space change of one logical
  // pixel to the right and one down.
  center: [f32; 2],
  axis_x: [f32; 2],
  axis_y: [f32; 2],
  radius: f32,
  color: [f32; 4],
}

impl ParticleInstance {
  pub(crate) const LAYOUT: wgpu::VertexBufferLayout<'static> =
    wgpu::VertexBufferLayout {
      array_stride: size_of::<Self>() as wgpu::BufferAddress,
      step_mode: wgpu::VertexStepMode::Instance,
      attributes: &wgpu::vertex_attr_array![
          0 => Float32x2,
          1 => Float32x2,
          2 => Float32x2,
          3 => Float32,
          4 => Float32x4,
      ],
    };
}

/// The six vertices of a squeezed tile, in the order of [`draw::quad`].
pub(crate) fn squeeze_vertices(
  tile: &RenderTile<'_>,
  squeeze: &Squeeze,
  size: (u32, u32),
  scale_factor: f32,
  zoom: Zoom,
  output_transform: TextureTransform,
) -> [SqueezeVertex; 6] {
  let mut flat = tile.clone();
  flat.rotation = 0.0;
  flat.projection = None;
  let quad =
    draw::quad(&flat, size.0, size.1, scale_factor, zoom, output_transform);
  let origin = draw::texture_coordinates(tile, 0.0, 0.0);
  let right = draw::texture_coordinates(tile, 1.0, 0.0);
  let down = draw::texture_coordinates(tile, 0.0, 1.0);
  // draw::quad's order: top left, bottom left, top right, top right,
  // bottom left, bottom right.
  let box_uv = [
    [0.0, 1.0],
    [0.0, 0.0],
    [1.0, 1.0],
    [1.0, 1.0],
    [0.0, 0.0],
    [1.0, 0.0],
  ];
  let flag = |value: bool| if value { 1.0 } else { 0.0 };
  std::array::from_fn(|index| SqueezeVertex {
    position: quad[index].position,
    box_uv: box_uv[index],
    source: squeeze.source,
    target: squeeze.target,
    params: [
      squeeze.progress,
      flag(squeeze.upward),
      flag(squeeze.horizontal),
      0.0,
    ],
    tex_origin: [origin.0, origin.1],
    tex_u: [right.0 - origin.0, right.1 - origin.1],
    tex_v: [down.0 - origin.0, down.1 - origin.1],
    color: tile.color,
    flags: quad[index].flags,
  })
}

/// The instances of `particles`, drawn relative to `tile`'s origin.
pub(crate) fn particle_instances(
  tile: &RenderTile<'_>,
  particles: &[Particle],
  size: (u32, u32),
  scale_factor: f32,
  zoom: Zoom,
  output_transform: TextureTransform,
) -> Vec<ParticleInstance> {
  let width = size.0 as f32 / scale_factor;
  let height = size.1 as f32 / scale_factor;
  let point = |x: f32, y: f32| {
    let mut vertex =
      Vertex::at([x / width * 2.0 - 1.0, 1.0 - y / height * 2.0]);
    draw::zoom_vertex(&mut vertex, width, height, zoom);
    draw::output_vertex(&mut vertex, output_transform);
    vertex.position
  };
  let x = tile.geometry.x as f32;
  let y = tile.geometry.y as f32;
  let origin = point(x, y);
  let right = point(x + 1.0, y);
  let below = point(x, y + 1.0);
  let axis_x = [right[0] - origin[0], right[1] - origin[1]];
  let axis_y = [below[0] - origin[0], below[1] - origin[1]];
  particles
    .iter()
    .filter(|particle| particle.radius > 0.0)
    .map(|particle| ParticleInstance {
      center: [
        origin[0]
          + axis_x[0] * particle.center[0]
          + axis_y[0] * particle.center[1],
        origin[1]
          + axis_x[1] * particle.center[0]
          + axis_y[1] * particle.center[1],
      ],
      axis_x,
      axis_y,
      radius: particle.radius,
      color: particle.color,
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::RenderRect;

  #[test]
  fn particles_are_placed_from_the_tile_origin_in_clip_space() {
    let tile = RenderTile::solid(
      RenderRect {
        x: 100,
        y: 50,
        width: 0,
        height: 0,
      },
      [1.0; 4],
    );
    let particles = [
      Particle {
        center: [100.0, 50.0],
        radius: 8.0,
        color: [1.0; 4],
      },
      Particle {
        radius: 0.0,
        ..Particle::default()
      },
    ];
    let instances = particle_instances(
      &tile,
      &particles,
      (800, 400),
      2.0,
      Zoom::default(),
      TextureTransform::Normal,
    );
    // Dead particles, of radius 0, are left out.
    assert_eq!(instances.len(), 1);
    // (200, 100) of a 400 by 200 logical output is its centre.
    assert!(instances[0].center[0].abs() < 1e-6);
    assert!(instances[0].center[1].abs() < 1e-6);
    assert!((instances[0].axis_x[0] - 2.0 / 400.0).abs() < 1e-6);
    assert!((instances[0].axis_y[1] + 2.0 / 200.0).abs() < 1e-6);
  }

  #[test]
  fn a_squeezed_tile_samples_the_window_through_its_buffer_transform() {
    let mut tile = RenderTile::solid(
      RenderRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
      },
      [1.0; 4],
    );
    tile.transform = TextureTransform::FlipHorizontal;
    let squeeze = Squeeze {
      progress: 0.5,
      source: [0.0, 0.0, 0.5, 0.5],
      target: [0.5, 0.9, 0.6, 1.0],
      upward: false,
      horizontal: false,
    };
    let vertices = squeeze_vertices(
      &tile,
      &squeeze,
      (100, 100),
      1.0,
      Zoom::default(),
      TextureTransform::Normal,
    );
    // The window's left edge is the texture's right one.
    assert_eq!(vertices[0].tex_origin, [1.0, 0.0]);
    assert_eq!(vertices[0].tex_u, [-1.0, 0.0]);
    assert_eq!(vertices[0].tex_v, [0.0, 1.0]);
    assert_eq!(vertices[0].box_uv, [0.0, 1.0]);
    assert_eq!(vertices[5].box_uv, [1.0, 0.0]);
    assert_eq!(vertices[5].params, [0.5, 0.0, 0.0, 0.0]);
    assert_eq!(vertices[5].position, [1.0, -1.0]);
  }
}
