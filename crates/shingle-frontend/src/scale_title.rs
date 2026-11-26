//! Rasterised window titles for Scale.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};

// Inter Regular, the same font Gloomy embeds, for Scale title overlays.
const FONT: &[u8] = include_bytes!("../fonts/Inter-Regular.ttf");
const HORIZONTAL_PADDING: u32 = 8;
const VERTICAL_PADDING: u32 = 4;

/// A CPU-rasterized, pre-multiplied RGBA title ready for wgpu upload.
pub struct RasterTitle {
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// `width * height` pre-multiplied RGBA pixels, row-major.
  pub rgba: Vec<u8>,
}

/// The bundled Inter Regular.
pub(crate) fn font() -> &'static FontRef<'static> {
  static FONT_REF: OnceLock<FontRef<'static>> = OnceLock::new();
  FONT_REF.get_or_init(|| FontRef::try_from_slice(FONT).unwrap())
}

fn write_background(pixel: &mut [u8], color: [f32; 4]) {
  let alpha = color[3].clamp(0.0, 1.0);
  for channel in 0..3 {
    pixel[channel] = (color[channel] * alpha * 255.0).round() as u8;
  }
  pixel[3] = (alpha * 255.0).round() as u8;
}

fn blend_text(pixel: &mut [u8], color: [f32; 4], coverage: f32) {
  let alpha = color[3] * coverage.clamp(0.0, 1.0);
  let remaining = 1.0 - alpha;
  let background_alpha = f32::from(pixel[3]) / 255.0;
  for channel in 0..3 {
    let foreground = color[channel] * alpha;
    let background = f32::from(pixel[channel]) / 255.0 * remaining;
    pixel[channel] = ((foreground + background) * 255.0).round() as u8;
  }
  pixel[3] = ((alpha + background_alpha * remaining) * 255.0).round() as u8;
}

/// Rasterizes one clipped title line using the Inter font bundled with Gloomy.
pub fn rasterize_title(
  title: &str,
  font_size: u32,
  max_width: u32,
  background: [f32; 4],
  foreground: [f32; 4],
) -> Option<RasterTitle> {
  if title.is_empty() || max_width == 0 || font_size == 0 {
    return None;
  }
  let font = font();
  let scaled = font.as_scaled(PxScale::from(font_size as f32));
  let available = max_width.saturating_sub(HORIZONTAL_PADDING * 2);
  if available == 0 {
    return None;
  }

  let mut text = String::new();
  let mut text_width = 0.0;
  let mut previous = None;
  for character in title.chars() {
    let id = scaled.glyph_id(character);
    let advance = previous
      .map(|previous| scaled.kern(previous, id))
      .unwrap_or(0.0)
      + scaled.h_advance(id);
    if (text_width + advance).ceil() as u32 > available {
      break;
    }
    text_width += advance;
    text.push(character);
    previous = Some(id);
  }

  let width = (text_width.ceil() as u32 + HORIZONTAL_PADDING * 2)
    .min(max_width)
    .max(1);
  let height = (scaled.height().ceil() as u32 + VERTICAL_PADDING * 2).max(1);
  let mut rgba = vec![0; width as usize * height as usize * 4];
  for pixel in rgba.as_chunks_mut::<4>().0 {
    write_background(pixel, background);
  }

  let baseline = VERTICAL_PADDING as f32 + scaled.ascent();
  let mut x = HORIZONTAL_PADDING as f32;
  let mut previous = None;
  for character in text.chars() {
    let id = scaled.glyph_id(character);
    if let Some(previous) = previous {
      x += scaled.kern(previous, id);
    }
    let mut glyph = scaled.scaled_glyph(character);
    glyph.position = point(x, baseline);
    if let Some(outlined) = font.outline_glyph(glyph) {
      let bounds = outlined.px_bounds();
      outlined.draw(|glyph_x, glyph_y, coverage| {
        let x = bounds.min.x as i32 + glyph_x as i32;
        let y = bounds.min.y as i32 + glyph_y as i32;
        if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
          return;
        }
        let offset = (y as usize * width as usize + x as usize) * 4;
        blend_text(&mut rgba[offset..offset + 4], foreground, coverage);
      });
    }
    x += scaled.h_advance(id);
    previous = Some(id);
  }

  Some(RasterTitle {
    width,
    height,
    rgba,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn titles_are_clipped_and_use_premultiplied_colors() {
    let title = rasterize_title(
      "A title that is too long",
      16,
      80,
      [0.2, 0.4, 0.6, 0.5],
      [0.8, 0.7, 0.6, 1.0],
    )
    .unwrap();
    assert!(title.width <= 80);
    assert!(title.height > 16);
    assert_eq!(
      title.rgba.len(),
      title.width as usize * title.height as usize * 4
    );
    assert_eq!(&title.rgba[..4], &[26, 51, 77, 128]);

    let changed = rasterize_title(
      "A title that is too long",
      16,
      80,
      [0.2, 0.4, 0.6, 0.5],
      [0.1, 0.2, 0.3, 1.0],
    )
    .unwrap();
    assert_ne!(title.rgba, changed.rgba);
  }

  #[test]
  fn empty_or_zero_sized_titles_are_not_allocated() {
    assert!(rasterize_title("", 16, 100, [0.0; 4], [1.0; 4]).is_none());
    assert!(rasterize_title("title", 0, 100, [0.0; 4], [1.0; 4]).is_none());
    assert!(rasterize_title("title", 16, 0, [0.0; 4], [1.0; 4]).is_none());
  }
}
