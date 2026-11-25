//! The root window's cursor, which X clients that never set their own
//! show (bring-up step 6 in docs/design/xwayland.md, section 2).

/// A cursor image in the X server's format: premultiplied ARGB32 in
/// native byte order, which is what Render's `CreateCursor` takes from a
/// depth-32 picture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorImage {
  /// Width in pixels.
  pub width: u16,
  /// Height in pixels.
  pub height: u16,
  /// The hotspot's column.
  pub hotspot_x: u16,
  /// The hotspot's row.
  pub hotspot_y: u16,
  /// `width * height` native-endian ARGB32 pixels.
  pub argb: Vec<u8>,
}

impl CursorImage {
  /// Converts an RGBA image, such as one an xcursor theme provides, whose
  /// colours are already premultiplied as xcursor files store them.
  /// Returns `None` when the size does not match the data, or a
  /// dimension is 0 or does not fit X's 16 bits. The hotspot is clamped
  /// into the image.
  pub fn from_rgba(
    width: u32,
    height: u32,
    hotspot: (i32, i32),
    rgba: &[u8],
  ) -> Option<Self> {
    let width16 = u16::try_from(width).ok().filter(|width| *width > 0)?;
    let height16 = u16::try_from(height).ok().filter(|height| *height > 0)?;
    if rgba.len() != width as usize * height as usize * 4 {
      return None;
    }
    let argb = rgba
      .chunks_exact(4)
      .flat_map(|pixel| {
        let [red, green, blue, alpha] =
          [pixel[0], pixel[1], pixel[2], pixel[3]];
        u32::from_be_bytes([alpha, red, green, blue]).to_ne_bytes()
      })
      .collect();
    let clamp =
      |value: i32, size: u16| value.clamp(0, i32::from(size) - 1) as u16;
    Some(Self {
      width: width16,
      height: height16,
      hotspot_x: clamp(hotspot.0, width16),
      hotspot_y: clamp(hotspot.1, height16),
      argb,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rgba_becomes_native_argb() {
    let image =
      CursorImage::from_rgba(2, 1, (1, 0), &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    assert_eq!((image.width, image.height), (2, 1));
    assert_eq!((image.hotspot_x, image.hotspot_y), (1, 0));
    let pixels = image
      .argb
      .chunks_exact(4)
      .map(|pixel| u32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]))
      .collect::<Vec<_>>();
    assert_eq!(pixels, [0x0401_0203, 0x0805_0607]);
  }

  #[test]
  fn bad_sizes_are_refused_and_hotspots_clamped() {
    assert!(CursorImage::from_rgba(0, 1, (0, 0), &[]).is_none());
    assert!(CursorImage::from_rgba(2, 2, (0, 0), &[0; 4]).is_none());
    assert!(CursorImage::from_rgba(70_000, 1, (0, 0), &[]).is_none());
    let image = CursorImage::from_rgba(1, 1, (-3, 9), &[0; 4]).unwrap();
    assert_eq!((image.hotspot_x, image.hotspot_y), (0, 0));
  }
}
