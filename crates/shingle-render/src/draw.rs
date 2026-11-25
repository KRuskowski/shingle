//! The composite pass shared by every render target.

use std::mem::size_of;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::{
  GpuContext, RenderRect, RenderState, RenderTile, TextureTransform,
  state::Zoom,
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct Vertex {
  pub(crate) position: [f32; 2],
  uv: [f32; 2],
  color: [f32; 4],
}

impl Vertex {
  pub(crate) const LAYOUT: wgpu::VertexBufferLayout<'static> =
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

/// Clears `view` to `clear` and composites `tiles` over it, in order.
///
/// `size` is the target's size in physical pixels, and the pipeline is
/// chosen by the format of the view's texture. Returns the submission, so
/// callers that hand the texture to another device can wait for it.
pub fn draw(
  gpu: &GpuContext,
  state: &RenderState,
  view: &wgpu::TextureView,
  size: (u32, u32),
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
) -> wgpu::SubmissionIndex {
  let device = gpu.device();
  let clear =
    invert_clear(clear, gpu.effects.inverted, gpu.effects.preserve_hue);
  let mut encoder =
    device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
      label: Some("shingle frame encoder"),
    });
  let vertices = tiles
    .iter()
    .flat_map(|tile| {
      quad(
        tile,
        size.0,
        size.1,
        state.scale_factor(),
        state.zoom_state(),
        state.output_transform(),
      )
    })
    .collect::<Vec<_>>();
  let vertex_buffer = (!vertices.is_empty()).then(|| {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
      label: Some("shingle quad vertices"),
      contents: bytemuck::cast_slice(&vertices),
      usage: wgpu::BufferUsages::VERTEX,
    })
  });
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
    if let Some(vertex_buffer) = &vertex_buffer {
      pass.set_pipeline(&gpu.pipeline(view.texture().format()));
      pass.set_vertex_buffer(0, vertex_buffer.slice(..));
      for (index, tile) in tiles.iter().enumerate() {
        let texture = tile.texture.unwrap_or(&gpu.white_texture);
        pass.set_bind_group(0, &texture.bind_group, &[]);
        pass.set_bind_group(1, &gpu.gamma_lut.bind_group, &[]);
        pass.set_bind_group(2, &gpu.effects.bind_group, &[]);
        let first = (index * 6) as u32;
        pass.draw(first..first + 6, 0..1);
      }
    }
  }
  gpu.queue().submit([encoder.finish()])
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

fn zoom_vertex(
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
    TextureTransform::Rotate270 | TextureTransform::AntiTranspose => [y, -x],
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
  Vertex {
    position: [x, y],
    uv: [u, v],
    color: tile.color,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
