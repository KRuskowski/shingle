//! Pixels of server-side frames: the title text and the buttons, drawn as
//! Wayfire's decoration theme draws them with Cairo and Pango
//! (wayfire:plugins/decor/deco-theme.cpp, `render_text` and
//! `get_button_surface`), on the CPU, for upload as textures. The title
//! uses the bundled Inter whatever `[decoration] font` names.

use ab_glyph::{Font, PxScale, ScaleFont, point};
use shingle::frame::{FrameAreaKind, FrameButton, FrameSnapshot};
use shingle_core::Rect;

use crate::scale_title::{RasterTitle, font};

/// How a button looks, from Wayfire's hover progress: 0 normal, 1 hovered
/// and -0.7 pressed (deco-button.cpp).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ButtonLook {
  /// Neither hovered nor pressed.
  Normal,
  /// The pointer is on it.
  Hovered,
  /// It is held down.
  Pressed,
}

impl ButtonLook {
  fn progress(self) -> f32 {
    match self {
      ButtonLook::Normal => 0.0,
      ButtonLook::Hovered => 1.0,
      ButtonLook::Pressed => -0.7,
    }
  }
}

/// How the title text sits in its rectangle, from `[decoration]
/// title_halign`, `title_valign`, `font_scale` and `font_color`.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleStyle {
  /// `left`, `center` or `right`.
  pub halign: String,
  /// `top`, `center` or `bottom`.
  pub valign: String,
  /// The font's em size as a fraction of the height.
  pub font_scale: f64,
  /// Straight-alpha RGBA text colour.
  pub color: [f64; 4],
}

impl TitleStyle {
  /// The style of `config`.
  pub fn new(config: &shingle_config::DecorationConfig) -> Self {
    Self {
      halign: config.title_halign.clone(),
      valign: config.title_valign.clone(),
      font_scale: config.font_scale,
      color: config.font_color,
    }
  }
}

/// One piece of a frame, in layout coordinates, as the frontend draws it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FramePiece {
  /// A band of the frame's background in a premultiplied linear colour.
  Fill {
    /// Where.
    rect: Rect,
    /// The colour.
    color: [f32; 4],
  },
  /// The title's texture.
  Title {
    /// Where.
    rect: Rect,
  },
  /// A button's texture.
  Button {
    /// Where.
    rect: Rect,
    /// Which button.
    button: FrameButton,
    /// How it looks.
    look: ButtonLook,
  },
}

/// The pieces of `frame`, bottom to top, as Wayfire's
/// `simple_decoration_node_t::render` draws them: the background in
/// `active_color` or `inactive_color` on the frame around the client, in
/// four bands, then the title and the buttons.
pub fn frame_pieces(
  frame: &FrameSnapshot,
  config: &shingle_config::DecorationConfig,
) -> Vec<FramePiece> {
  let rect = frame.rect;
  let margins = frame.margins;
  let color = if frame.activated {
    config.active_color
  } else {
    config.inactive_color
  };
  // Wayfire draws the colour's bytes as they are; Shingle's solid colours
  // are linear and encoded to sRGB on output, so the colour is decoded.
  let alpha = color[3] as f32;
  let color = [
    srgb_to_linear(color[0]) * alpha,
    srgb_to_linear(color[1]) * alpha,
    srgb_to_linear(color[2]) * alpha,
    alpha,
  ];
  let inner = rect
    .height
    .saturating_sub(margins.top.saturating_add(margins.bottom));
  let at = |x: i32, y: i32, width: u32, height: u32| {
    Rect::new(
      rect.x.saturating_add(x),
      rect.y.saturating_add(y),
      width,
      height,
    )
  };
  let bottom = rect.height.saturating_sub(margins.bottom) as i32;
  let right = rect.width.saturating_sub(margins.right) as i32;
  let top = margins.top as i32;
  let mut pieces = [
    at(0, 0, rect.width, margins.top),
    at(0, bottom, rect.width, margins.bottom),
    at(0, top, margins.left, inner),
    at(right, top, margins.right, inner),
  ]
  .into_iter()
  .filter(|band| band.width > 0 && band.height > 0)
  .map(|band| FramePiece::Fill { rect: band, color })
  .collect::<Vec<_>>();
  if let Some(title) = frame.layout.title() {
    pieces.push(FramePiece::Title {
      rect: at(title.x, title.y, title.width, title.height),
    });
  }
  for (index, area) in frame.layout.areas.iter().enumerate() {
    if let FrameAreaKind::Button(button) = area.kind {
      let place = area.rect;
      pieces.push(FramePiece::Button {
        rect: at(place.x, place.y, place.width, place.height),
        button,
        look: button_look(frame, index),
      });
    }
  }
  pieces
}

/// How the button in area `index` of `frame` looks: pressed while held,
/// as Wayfire keeps it when the pointer leaves it, else hovered.
pub fn button_look(frame: &FrameSnapshot, index: usize) -> ButtonLook {
  if frame.pressed == Some(index) {
    ButtonLook::Pressed
  } else if frame.hovered == Some(index) {
    ButtonLook::Hovered
  } else {
    ButtonLook::Normal
  }
}

/// The linear value of sRGB component `value`.
fn srgb_to_linear(value: f64) -> f32 {
  let linear = if value <= 0.04045 {
    value / 12.92
  } else {
    ((value + 0.055) / 1.055).powf(2.4)
  };
  linear as f32
}

/// Lays `title` out in a `width` by `height` pixel rectangle as Pango
/// does for Wayfire: an em size of `height` times the font scale, the
/// text ellipsized at its end to the layout width, and aligned. Returns
/// the glyphs with their pen positions and the line's left edge and
/// baseline, or `None` when nothing fits.
fn layout(
  title: &str,
  width: u32,
  height: u32,
  style: &TitleStyle,
) -> Option<(Vec<(char, f32)>, f32)> {
  let font = font();
  let em = height as f32 * style.font_scale as f32;
  let units = font.units_per_em()?;
  let scaled =
    font.as_scaled(PxScale::from(em * font.height_unscaled() / units));
  let advance = |text: &[char]| {
    let mut x = 0.0;
    let mut previous = None;
    let mut pens = Vec::with_capacity(text.len());
    for &character in text {
      let id = scaled.glyph_id(character);
      if let Some(previous) = previous {
        x += scaled.kern(previous, id);
      }
      pens.push((character, x));
      x += scaled.h_advance(id);
      previous = Some(id);
    }
    (pens, x)
  };
  // Wayfire leaves two pixels on the right of a right-aligned title.
  let layout_width = if style.halign == "right" {
    width.saturating_sub(2)
  } else {
    width
  } as f32;
  let characters = title.chars().collect::<Vec<_>>();
  let (mut pens, mut text_width) = advance(&characters);
  if text_width > layout_width {
    let mut kept = characters.len();
    loop {
      if kept == 0 {
        return None;
      }
      kept -= 1;
      let mut shortened = characters[..kept].to_vec();
      shortened.push('…');
      (pens, text_width) = advance(&shortened);
      if text_width <= layout_width {
        break;
      }
    }
  }
  let x = match style.halign.as_str() {
    "center" => (layout_width - text_width) / 2.0,
    "right" => layout_width - text_width,
    _ => 0.0,
  };
  let line_height = scaled.ascent() - scaled.descent();
  let y = match style.valign.as_str() {
    "center" => (height as f32 - line_height) / 2.0,
    "bottom" => height as f32 - line_height,
    _ => 0.0,
  };
  let pens = pens
    .into_iter()
    .map(|(character, pen)| (character, x + pen))
    .collect();
  Some((pens, y + scaled.ascent()))
}

/// The title text of a frame in a transparent `width` by `height` pixel
/// image, premultiplied, or `None` when there is nothing to draw.
pub fn rasterize_title(
  title: &str,
  width: u32,
  height: u32,
  style: &TitleStyle,
) -> Option<RasterTitle> {
  if title.is_empty() || width == 0 || height == 0 {
    return None;
  }
  let (pens, baseline) = layout(title, width, height, style)?;
  let font = font();
  let em = height as f32 * style.font_scale as f32;
  let scale = PxScale::from(em * font.height_unscaled() / font.units_per_em()?);
  let scaled = font.as_scaled(scale);
  let mut rgba = vec![0; width as usize * height as usize * 4];
  let color = style.color.map(|component| component as f32);
  for (character, x) in pens {
    let mut glyph = scaled.scaled_glyph(character);
    glyph.position = point(x, baseline);
    let Some(outlined) = font.outline_glyph(glyph) else {
      continue;
    };
    let bounds = outlined.px_bounds();
    outlined.draw(|glyph_x, glyph_y, coverage| {
      let x = bounds.min.x as i32 + glyph_x as i32;
      let y = bounds.min.y as i32 + glyph_y as i32;
      if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
      }
      let offset = (y as usize * width as usize + x as usize) * 4;
      over(
        &mut rgba[offset..offset + 4],
        color,
        color[3] * coverage.clamp(0.0, 1.0),
      );
    });
  }
  Some(RasterTitle {
    width,
    height,
    rgba,
  })
}

/// Blends straight-alpha `color` at `alpha` over a premultiplied pixel.
fn over(pixel: &mut [u8], color: [f32; 4], alpha: f32) {
  let alpha = alpha.clamp(0.0, 1.0);
  let remaining = 1.0 - alpha;
  for channel in 0..3 {
    let below = f32::from(pixel[channel]) / 255.0;
    pixel[channel] =
      ((color[channel] * alpha + below * remaining) * 255.0).round() as u8;
  }
  let below = f32::from(pixel[3]) / 255.0;
  pixel[3] = ((alpha + below * remaining) * 255.0).round() as u8;
}

/// A button's image, `size` pixels square and premultiplied, as Wayfire
/// draws it at the title bar's height before scaling it to the button:
/// a disc, grey or in the button's colour when hovered or pressed, a
/// one-pixel ring, and its icon, a cross, a square or a dash, drawn
/// without antialiasing.
pub fn rasterize_button(
  button: FrameButton,
  look: ButtonLook,
  size: u32,
) -> Vec<u8> {
  let progress = look.progress();
  let side = size as f32;
  let border = 1.0;
  let (mut base, mut line) = ([0.60, 0.60, 0.63, 0.36], 0.27);
  if progress.abs() > 1e-3 {
    let rgb = match button {
      FrameButton::Close => [242.0, 80.0, 86.0],
      FrameButton::Maximize => [57.0, 234.0, 73.0],
      FrameButton::Minimize => [250.0, 198.0, 54.0],
    };
    base = [rgb[0] / 255.0, rgb[1] / 255.0, rgb[2] / 255.0, 0.63];
    line *= 2.0;
  }
  base[3] += 0.27 * progress;
  let centre = side / 2.0;
  let ring = side / 2.0 - 0.5 * border;
  let quarter = side / 4.0;
  let mut rgba = vec![0; size as usize * size as usize * 4];
  for y in 0..size {
    for x in 0..size {
      let offset = (y as usize * size as usize + x as usize) * 4;
      let pixel = &mut rgba[offset..offset + 4];
      // Antialiased disc and ring, from 4×4 samples.
      let (mut disc, mut stroke) = (0.0, 0.0);
      for sample_y in 0..4 {
        for sample_x in 0..4 {
          let px = x as f32 + (sample_x as f32 + 0.5) / 4.0;
          let py = y as f32 + (sample_y as f32 + 0.5) / 4.0;
          let distance = (px - centre).hypot(py - centre);
          if distance <= side / 2.0 {
            disc += 1.0 / 16.0;
          }
          if (distance - ring).abs() <= border / 2.0 {
            stroke += 1.0 / 16.0;
          }
        }
      }
      over(pixel, base, base[3] * disc);
      over(pixel, [0.0, 0.0, 0.0, line], line * stroke);
      // The icon covers whole pixels whose centres it contains.
      let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
      let icon = match button {
        FrameButton::Close => {
          let width = 1.5 * border;
          on_segment(
            (px, py),
            (quarter, quarter),
            (3.0 * quarter, 3.0 * quarter),
            width,
          ) || on_segment(
            (px, py),
            (3.0 * quarter, quarter),
            (quarter, 3.0 * quarter),
            width,
          )
        }
        FrameButton::Maximize => {
          let half = 1.5 * border / 2.0;
          let (low, high) = (quarter, 3.0 * quarter);
          let outer = px >= low - half
            && px <= high + half
            && py >= low - half
            && py <= high + half;
          let inner = px > low + half
            && px < high - half
            && py > low + half
            && py < high - half;
          outer && !inner
        }
        FrameButton::Minimize => on_segment(
          (px, py),
          (quarter, centre),
          (3.0 * quarter, centre),
          1.75 * border,
        ),
      };
      if icon {
        over(pixel, [0.0, 0.0, 0.0, line / 2.0], line / 2.0);
      }
    }
  }
  rgba
}

/// Whether `point` is on the segment from `start` to `end` stroked
/// `width` wide with butt caps, Cairo's default.
fn on_segment(
  point: (f32, f32),
  start: (f32, f32),
  end: (f32, f32),
  width: f32,
) -> bool {
  let (dx, dy) = (end.0 - start.0, end.1 - start.1);
  let length = dx.hypot(dy);
  if length == 0.0 {
    return false;
  }
  let along = ((point.0 - start.0) * dx + (point.1 - start.1) * dy) / length;
  let across =
    ((point.0 - start.0) * dy - (point.1 - start.1) * dx).abs() / length;
  (0.0..=length).contains(&along) && across <= width / 2.0
}

#[cfg(test)]
mod tests {
  use super::*;

  fn style() -> TitleStyle {
    TitleStyle::new(&shingle_config::DecorationConfig::default())
  }

  fn alpha_at(raster: &RasterTitle, x: u32, y: u32) -> u8 {
    raster.rgba[(y as usize * raster.width as usize + x as usize) * 4 + 3]
  }

  /// The leftmost and rightmost columns with any ink.
  fn ink_columns(raster: &RasterTitle) -> Option<(u32, u32)> {
    let columns = (0..raster.width)
      .filter(|x| (0..raster.height).any(|y| alpha_at(raster, *x, y) > 0))
      .collect::<Vec<_>>();
    Some((*columns.first()?, *columns.last()?))
  }

  /// The topmost and bottom rows with any ink.
  fn ink_rows(raster: &RasterTitle) -> Option<(u32, u32)> {
    let rows = (0..raster.height)
      .filter(|y| (0..raster.width).any(|x| alpha_at(raster, x, *y) > 0))
      .collect::<Vec<_>>();
    Some((*rows.first()?, *rows.last()?))
  }

  #[test]
  fn srgb_components_decode_to_linear() {
    assert_eq!(srgb_to_linear(0.0), 0.0);
    assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
    assert!((srgb_to_linear(0.5) - 0.214).abs() < 1e-3);
  }

  fn snapshot(activated: bool) -> FrameSnapshot {
    let theme = shingle::frame::FrameTheme::default();
    FrameSnapshot {
      window: shingle_core::WindowId(1),
      rect: Rect::new(10, 20, 400, 300),
      margins: theme.margins(),
      activated,
      title: "t".into(),
      layout: shingle::frame::FrameLayout::new(&theme, 400, 300, false, true),
      hovered: Some(1),
      pressed: Some(0),
    }
  }

  #[test]
  fn a_frame_is_four_bands_a_title_and_its_buttons() {
    let config = shingle_config::DecorationConfig {
      active_color: [1.0, 0.0, 0.0, 0.5],
      inactive_color: [0.0, 0.0, 1.0, 1.0],
      ..shingle_config::DecorationConfig::default()
    };
    let pieces = frame_pieces(&snapshot(true), &config);
    let red = [0.5, 0.0, 0.0, 0.5];
    assert_eq!(
      pieces[..4],
      [
        FramePiece::Fill {
          rect: Rect::new(10, 20, 400, 34),
          color: red
        },
        FramePiece::Fill {
          rect: Rect::new(10, 316, 400, 4),
          color: red
        },
        FramePiece::Fill {
          rect: Rect::new(10, 54, 4, 262),
          color: red
        },
        FramePiece::Fill {
          rect: Rect::new(406, 54, 4, 262),
          color: red
        },
      ]
    );
    assert_eq!(
      pieces[4],
      FramePiece::Title {
        rect: Rect::new(14, 24, 309, 30)
      }
    );
    assert_eq!(
      pieces[5..],
      [
        FramePiece::Button {
          rect: Rect::new(381, 28, 21, 21),
          button: FrameButton::Close,
          look: ButtonLook::Pressed,
        },
        FramePiece::Button {
          rect: Rect::new(352, 28, 21, 21),
          button: FrameButton::Maximize,
          look: ButtonLook::Hovered,
        },
        FramePiece::Button {
          rect: Rect::new(323, 28, 21, 21),
          button: FrameButton::Minimize,
          look: ButtonLook::Normal,
        },
      ]
    );
    let inactive = frame_pieces(&snapshot(false), &config);
    assert_eq!(
      inactive[0],
      FramePiece::Fill {
        rect: Rect::new(10, 20, 400, 34),
        color: [0.0, 0.0, 1.0, 1.0]
      }
    );
  }

  #[test]
  fn a_title_fills_its_rectangle_and_starts_at_the_left() {
    let raster = rasterize_title("Terminal", 300, 30, &style()).unwrap();
    assert_eq!((raster.width, raster.height), (300, 30));
    let (left, right) = ink_columns(&raster).unwrap();
    assert!(left <= 3, "{left}");
    assert!(right < 150, "{right}");
    // White text, premultiplied: every inked pixel is grey.
    for pixel in raster.rgba.as_chunks::<4>().0 {
      assert!(pixel[0] == pixel[1] && pixel[1] == pixel[2]);
      assert!(pixel[0] <= pixel[3]);
    }
    assert!(rasterize_title("", 300, 30, &style()).is_none());
  }

  #[test]
  fn alignment_moves_the_text() {
    // A small font, so that the vertical alignments differ by pixels.
    let style = || TitleStyle {
      font_scale: 0.4,
      ..super::tests::style()
    };
    let left = rasterize_title("Title", 300, 30, &style()).unwrap();
    let centre = rasterize_title(
      "Title",
      300,
      30,
      &TitleStyle {
        halign: "center".into(),
        valign: "center".into(),
        ..style()
      },
    )
    .unwrap();
    let right = rasterize_title(
      "Title",
      300,
      30,
      &TitleStyle {
        halign: "right".into(),
        valign: "bottom".into(),
        ..style()
      },
    )
    .unwrap();
    let (left_start, left_end) = ink_columns(&left).unwrap();
    let (centre_start, centre_end) = ink_columns(&centre).unwrap();
    let (_, right_end) = ink_columns(&right).unwrap();
    let ink = left_end - left_start;
    assert!(centre_start.abs_diff(150 - ink / 2) <= 3, "{centre_start}");
    assert!(centre_end.abs_diff(150 + ink / 2) <= 3, "{centre_end}");
    // Two pixels are left on the right, as in Wayfire.
    assert!((294..=298).contains(&right_end), "{right_end}");
    // A 24-pixel font in 30 pixels: top, centre and bottom differ.
    let top = ink_rows(&left).unwrap();
    let middle = ink_rows(&centre).unwrap();
    let bottom = ink_rows(&right).unwrap();
    assert!(
      top.0 < middle.0 && middle.0 < bottom.0,
      "{top:?} {middle:?} {bottom:?}"
    );
  }

  #[test]
  fn a_long_title_is_ellipsized_to_fit() {
    let raster = rasterize_title(&"W".repeat(100), 120, 30, &style()).unwrap();
    let (_, right) = ink_columns(&raster).unwrap();
    assert!(right < 120);
    let (pens, _) = layout(&"W".repeat(100), 120, 30, &style()).unwrap();
    assert_eq!(pens.last().map(|(character, _)| *character), Some('…'));
    assert!(layout("Wide", 1, 30, &style()).is_none());
  }

  #[test]
  fn the_font_scale_sets_the_text_size() {
    let big = rasterize_title("H", 100, 30, &style()).unwrap();
    let small = rasterize_title(
      "H",
      100,
      30,
      &TitleStyle {
        font_scale: 0.4,
        ..style()
      },
    )
    .unwrap();
    let height = |raster: &RasterTitle| {
      let (top, bottom) = ink_rows(raster).unwrap();
      bottom - top + 1
    };
    // Inter's capital H is about 0.73 em: 24 pixels make it 17 or 18.
    assert!((16..=19).contains(&height(&big)), "{}", height(&big));
    assert!(height(&small) * 2 <= height(&big) + 2);
  }

  fn pixel(rgba: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
    let offset = (y as usize * size as usize + x as usize) * 4;
    rgba[offset..offset + 4].try_into().unwrap()
  }

  #[test]
  fn buttons_are_grey_until_hovered_and_carry_their_icon() {
    let size = 30;
    let normal = rasterize_button(FrameButton::Close, ButtonLook::Normal, size);
    // Outside the disc is transparent.
    assert_eq!(pixel(&normal, size, 0, 0), [0, 0, 0, 0]);
    // Inside, the grey base at 36 % shows where no icon is.
    let base = pixel(&normal, size, 15, 4);
    assert!(base[3].abs_diff(92) <= 1, "{base:?}");
    assert!(base[0].abs_diff(55) <= 1, "{base:?}");
    // The cross passes through the centre, darker than the base.
    let centre = pixel(&normal, size, 15, 15);
    assert!(centre[0] < base[0] && centre[3] > base[3], "{centre:?}");
    // Hovered, the close button is red and nearly opaque.
    let hovered =
      rasterize_button(FrameButton::Close, ButtonLook::Hovered, size);
    let red = pixel(&hovered, size, 15, 4);
    assert!(red[3].abs_diff(230) <= 1, "{red:?}");
    assert!(red[0] > red[1] && red[0] > red[2], "{red:?}");
    // Pressed, it is fainter.
    let pressed =
      rasterize_button(FrameButton::Close, ButtonLook::Pressed, size);
    assert!(pixel(&pressed, size, 15, 4)[3] < red[3]);
    let maximize =
      rasterize_button(FrameButton::Maximize, ButtonLook::Hovered, size);
    let green = pixel(&maximize, size, 15, 4);
    assert!(green[1] > green[0] && green[1] > green[2], "{green:?}");
    // The square's edge is drawn and its middle is not.
    assert!(pixel(&maximize, size, 7, 15)[1] < green[1]);
    assert_eq!(pixel(&maximize, size, 15, 15), green);
    // The dash runs across the middle row only.
    let minimize =
      rasterize_button(FrameButton::Minimize, ButtonLook::Normal, size);
    assert!(pixel(&minimize, size, 15, 15)[3] > base[3]);
    assert_eq!(pixel(&minimize, size, 15, 10), base);
  }
}
