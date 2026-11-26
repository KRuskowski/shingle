//! Drawing server-side frames: the textures of their titles and buttons,
//! and the tiles that put a frame below its window's surface, as
//! Wayfire's `simple_decoration_node_t::render` draws it.

use std::{collections::HashMap, error::Error};

use shingle::{
  SurfaceId,
  frame::{FrameAreaKind, FrameButton, FrameSnapshot},
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{GpuTexture, RenderRect, RenderTile};

use crate::{
  frame_raster::{self, ButtonLook, FramePiece, TitleStyle},
  render_target::RenderTarget,
};

/// A frame's title texture and what it was drawn for.
pub(super) struct FrameTitle {
  title: String,
  size: (u32, u32),
  style: TitleStyle,
  texture: Option<GpuTexture>,
}

/// The textures frames are drawn with.
#[derive(Default)]
pub(super) struct FrameTextures {
  titles: HashMap<TileWindowId, FrameTitle>,
  buttons: HashMap<(FrameButton, ButtonLook, u32), GpuTexture>,
}

impl FrameTextures {
  /// Forgets every texture, after a reload or the loss of the GPU.
  pub(super) fn clear(&mut self) {
    self.titles.clear();
    self.buttons.clear();
  }

  /// Forgets the title of a window that is gone.
  pub(super) fn window_destroyed(&mut self, window: TileWindowId) {
    self.titles.remove(&window);
  }

  /// Makes the textures `frames` need at output scale `scale`: each title
  /// at its rectangle's size in pixels, each button as Wayfire draws it,
  /// at the title bar's height in pixels, to be scaled down to the button.
  pub(super) fn prepare(
    &mut self,
    renderer: &dyn RenderTarget,
    frames: &HashMap<SurfaceId, FrameSnapshot>,
    config: &shingle_config::DecorationConfig,
    scale: f64,
  ) -> Result<(), Box<dyn Error>> {
    let style = TitleStyle::new(config);
    let pixels = |logical: u32| (f64::from(logical) * scale).round() as u32;
    let button_size = pixels(config.title_height).max(1);
    for frame in frames.values() {
      if let Some(title) = frame.layout.title() {
        let size = (pixels(title.width), pixels(title.height));
        let current = self.titles.get(&frame.window).is_some_and(|cached| {
          cached.title == frame.title
            && cached.size == size
            && cached.style == style
        });
        if !current {
          let texture =
            frame_raster::rasterize_title(&frame.title, size.0, size.1, &style)
              .map(|raster| {
                renderer.create_texture(
                  raster.width,
                  raster.height,
                  &raster.rgba,
                )
              })
              .transpose()?;
          self.titles.insert(
            frame.window,
            FrameTitle {
              title: frame.title.clone(),
              size,
              style: style.clone(),
              texture,
            },
          );
        }
      }
      for (index, area) in frame.layout.areas.iter().enumerate() {
        let FrameAreaKind::Button(button) = area.kind else {
          continue;
        };
        let look = frame_raster::button_look(frame, index);
        if self.buttons.contains_key(&(button, look, button_size)) {
          continue;
        }
        let rgba = frame_raster::rasterize_button(button, look, button_size);
        let texture =
          renderer.create_texture(button_size, button_size, &rgba)?;
        self.buttons.insert((button, look, button_size), texture);
      }
    }
    Ok(())
  }

  /// The tiles of `frame`, bottom to top, from
  /// [`frame_raster::frame_pieces`]. They are placed and faded as
  /// `content`, the tile of the window's surface, which the scene may have
  /// moved and scaled from `surface`, its rectangle in the layout;
  /// `content`'s colour, the window's opacity and fades, multiplies
  /// theirs.
  pub(super) fn tiles<'a>(
    &'a self,
    frame: &FrameSnapshot,
    surface: RenderRect,
    content: &RenderTile<'_>,
    config: &shingle_config::DecorationConfig,
    scale: f64,
  ) -> Vec<RenderTile<'a>> {
    let place = |rect: shingle_core::Rect| {
      let rect = RenderRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
      };
      map_rect(rect, surface, content.geometry)
    };
    let button_size =
      ((f64::from(config.title_height) * scale).round() as u32).max(1);
    frame_raster::frame_pieces(frame, config)
      .into_iter()
      .filter_map(|piece| {
        let mut tile = match piece {
          FramePiece::Fill { rect, color } => {
            let mut tile = RenderTile::solid(place(rect), color);
            for (component, factor) in tile.color.iter_mut().zip(content.color)
            {
              *component *= factor;
            }
            return Some(tile);
          }
          FramePiece::Title { rect } => RenderTile::textured(
            place(rect),
            self.titles.get(&frame.window)?.texture.as_ref()?,
          ),
          FramePiece::Button { rect, button, look } => RenderTile::textured(
            place(rect),
            self.buttons.get(&(button, look, button_size))?,
          ),
        };
        tile.color = content.color;
        Some(tile)
      })
      .collect()
  }
}

/// `rect` carried along when `from` becomes `to`: the same position and
/// size relative to it, scaled by its change of size.
pub(super) fn map_rect(
  rect: RenderRect,
  from: RenderRect,
  to: RenderRect,
) -> RenderRect {
  if from == to || from.width == 0 || from.height == 0 {
    return rect;
  }
  let scale_x = f64::from(to.width) / f64::from(from.width);
  let scale_y = f64::from(to.height) / f64::from(from.height);
  let left = f64::from(to.x) + f64::from(rect.x - from.x) * scale_x;
  let top = f64::from(to.y) + f64::from(rect.y - from.y) * scale_y;
  let right = left + f64::from(rect.width) * scale_x;
  let bottom = top + f64::from(rect.height) * scale_y;
  RenderRect {
    x: left.round() as i32,
    y: top.round() as i32,
    width: (right.round() - left.round()).max(0.0) as u32,
    height: (bottom.round() - top.round()).max(0.0) as u32,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rect(x: i32, y: i32, width: u32, height: u32) -> RenderRect {
    RenderRect {
      x,
      y,
      width,
      height,
    }
  }

  #[test]
  fn a_frame_rectangle_follows_its_window_when_it_moves_and_scales() {
    let from = rect(100, 100, 200, 100);
    let frame = rect(96, 66, 208, 138);
    assert_eq!(map_rect(frame, from, from), frame);
    assert_eq!(
      map_rect(frame, from, rect(10, 20, 200, 100)),
      rect(6, -14, 208, 138)
    );
    // Half the size, around a new origin.
    assert_eq!(
      map_rect(frame, from, rect(0, 0, 100, 50)),
      rect(-2, -17, 104, 69)
    );
  }
}
