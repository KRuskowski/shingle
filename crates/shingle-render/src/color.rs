//! Configured colours, which name the bytes Wayfire writes.
//!
//! Wayfire draws through wlroots' GLES2 renderer into an 8-bit framebuffer
//! without sRGB encoding (wayfire:src/core/opengl.cpp, `clear` and
//! `render_rectangle`; wayfire:src/view/compositor-view.cpp premultiplies
//! a rectangle's colour), so a configured `0.2 0.4 0.6 1.0` or `#336699FF`
//! reaches the screen as the bytes 0x33, 0x66, 0x99. Shingle's targets are
//! sRGB and blend in linear light: a tile's colour and the clear colour are
//! linear and encoded on output. So every configured colour is decoded from
//! sRGB first, and the encoding gives its bytes back, as wlroots' Vulkan
//! renderer decodes a rectangle's colour (render/vulkan/pass.c,
//! `color_to_linear_premult`). Translucent colours still blend in linear
//! light, so what shows through them differs slightly from Wayfire, which
//! blends the bytes.
//!
//! Colours that CPU code writes into textures as bytes, such as Scale's
//! titles and a frame's title and buttons, need no decoding: the textures
//! are sRGB and decode their bytes when sampled.

/// The linear value of the sRGB-encoded component `value`, in 0..=1. Values
/// outside that range are clamped first.
pub fn srgb_to_linear(value: f64) -> f64 {
  let value = value.clamp(0.0, 1.0);
  if value <= 0.04045 {
    value / 12.92
  } else {
    ((value + 0.055) / 1.055).powf(2.4)
  }
}

/// A configured straight-alpha colour as the premultiplied linear
/// [`RenderTile`](crate::RenderTile) colour that draws its bytes, as
/// Wayfire's colour rectangles do.
pub fn srgb_fill(color: [f64; 4]) -> [f32; 4] {
  let alpha = color[3].clamp(0.0, 1.0);
  [
    (srgb_to_linear(color[0]) * alpha) as f32,
    (srgb_to_linear(color[1]) * alpha) as f32,
    (srgb_to_linear(color[2]) * alpha) as f32,
    alpha as f32,
  ]
}

/// A configured colour as the linear clear colour of [`draw`](crate::draw)
/// that clears a target to its bytes, as Wayfire's `glClearColor` does.
/// Alpha is kept as it is, unpremultiplied, as `glClearColor` writes it.
pub fn srgb_clear(color: [f64; 4]) -> [f64; 4] {
  [
    srgb_to_linear(color[0]),
    srgb_to_linear(color[1]),
    srgb_to_linear(color[2]),
    color[3].clamp(0.0, 1.0),
  ]
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The sRGB encoding a target applies, as the GPU does it.
  fn encode(linear: f64) -> u8 {
    let value = if linear <= 0.0031308 {
      linear * 12.92
    } else {
      1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (value * 255.0).round() as u8
  }

  #[test]
  fn srgb_components_decode_to_linear() {
    assert_eq!(srgb_to_linear(0.0), 0.0);
    assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-12);
    assert!((srgb_to_linear(0.5) - 0.214).abs() < 1e-3);
    assert!((srgb_to_linear(0.04) - 0.04 / 12.92).abs() < 1e-12);
    assert_eq!(srgb_to_linear(-1.0), 0.0);
    assert_eq!(srgb_to_linear(2.0), 1.0);
  }

  #[test]
  fn every_byte_survives_decoding_and_encoding() {
    for byte in 0..=255_u8 {
      let configured = f64::from(byte) / 255.0;
      let [red, ..] = srgb_clear([configured, 0.0, 0.0, 1.0]);
      assert_eq!(encode(red), byte, "clear {byte}");
      let [red, ..] = srgb_fill([configured, 0.0, 0.0, 1.0]);
      assert_eq!(encode(f64::from(red)), byte, "fill {byte}");
    }
  }

  #[test]
  fn fills_are_premultiplied_and_clears_are_not() {
    let fill = srgb_fill([1.0, 0.5, 0.0, 0.5]);
    assert_eq!(fill[0], 0.5);
    assert!((f64::from(fill[1]) - srgb_to_linear(0.5) * 0.5).abs() < 1e-6);
    assert_eq!(fill[2], 0.0);
    assert_eq!(fill[3], 0.5);
    let clear = srgb_clear([1.0, 0.5, 0.0, 0.5]);
    assert_eq!(clear, [1.0, srgb_to_linear(0.5), 0.0, 0.5]);
  }
}
