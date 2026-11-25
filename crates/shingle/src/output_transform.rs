use wayland_server::protocol::wl_output::Transform;

pub(crate) fn is_quarter_turn(transform: Transform) -> bool {
  matches!(
    transform,
    Transform::_90
      | Transform::_270
      | Transform::Flipped90
      | Transform::Flipped270
  )
}

pub(crate) fn physical_size(
  width: u32,
  height: u32,
  scale: f64,
  transform: Transform,
) -> (u32, u32) {
  let scaled = |value| (f64::from(value) * scale).round().max(1.0) as u32;
  let (width, height) = (scaled(width), scaled(height));
  if is_quarter_turn(transform) {
    (height, width)
  } else {
    (width, height)
  }
}

pub(crate) fn logical_size(
  width: u32,
  height: u32,
  scale: f64,
  transform: Transform,
) -> (u32, u32) {
  let (width, height) = (scaled(width, scale), scaled(height, scale));
  if is_quarter_turn(transform) {
    (height, width)
  } else {
    (width, height)
  }
}

pub(crate) fn map_point(
  x: f64,
  y: f64,
  display_width: u32,
  display_height: u32,
  logical_width: u32,
  logical_height: u32,
  transform: Transform,
) -> (f64, f64) {
  let ndc_x = 2.0 * x / f64::from(display_width.max(1)) - 1.0;
  let ndc_y = 1.0 - 2.0 * y / f64::from(display_height.max(1));
  let (x, y) = inverse(ndc_x, ndc_y, transform);
  (
    (x + 1.0) * 0.5 * f64::from(logical_width),
    (1.0 - y) * 0.5 * f64::from(logical_height),
  )
}

pub(crate) fn unmap_point(
  x: f64,
  y: f64,
  display_width: u32,
  display_height: u32,
  logical_width: u32,
  logical_height: u32,
  transform: Transform,
) -> (f64, f64) {
  let ndc_x = 2.0 * x / f64::from(logical_width.max(1)) - 1.0;
  let ndc_y = 1.0 - 2.0 * y / f64::from(logical_height.max(1));
  let (x, y) = forward(ndc_x, ndc_y, transform);
  (
    (x + 1.0) * 0.5 * f64::from(display_width),
    (1.0 - y) * 0.5 * f64::from(display_height),
  )
}

pub(crate) fn map_vector(
  x: f64,
  y: f64,
  display_width: u32,
  display_height: u32,
  logical_width: u32,
  logical_height: u32,
  transform: Transform,
) -> (f64, f64) {
  let ndc_x = 2.0 * x / f64::from(display_width.max(1));
  let ndc_y = -2.0 * y / f64::from(display_height.max(1));
  let (x, y) = inverse(ndc_x, ndc_y, transform);
  (
    x * f64::from(logical_width) * 0.5,
    -y * f64::from(logical_height) * 0.5,
  )
}

fn inverse(x: f64, y: f64, transform: Transform) -> (f64, f64) {
  let (x, y) = match transform {
    Transform::_90 | Transform::Flipped90 => (y, -x),
    Transform::_180 | Transform::Flipped180 => (-x, -y),
    Transform::_270 | Transform::Flipped270 => (-y, x),
    _ => (x, y),
  };
  if matches!(
    transform,
    Transform::Flipped
      | Transform::Flipped90
      | Transform::Flipped180
      | Transform::Flipped270
  ) {
    (-x, y)
  } else {
    (x, y)
  }
}

fn forward(x: f64, y: f64, transform: Transform) -> (f64, f64) {
  let (x, y) = if matches!(
    transform,
    Transform::Flipped
      | Transform::Flipped90
      | Transform::Flipped180
      | Transform::Flipped270
  ) {
    (-x, y)
  } else {
    (x, y)
  };
  match transform {
    Transform::_90 | Transform::Flipped90 => (-y, x),
    Transform::_180 | Transform::Flipped180 => (-x, -y),
    Transform::_270 | Transform::Flipped270 => (y, -x),
    _ => (x, y),
  }
}

fn scaled(value: u32, scale: f64) -> u32 {
  (f64::from(value) / scale).round().max(1.0) as u32
}

#[cfg(test)]
mod tests {
  use wayland_server::protocol::wl_output::Transform;

  #[test]
  fn point_mapping_roundtrips_every_output_transform() {
    let transforms = [
      Transform::Normal,
      Transform::_90,
      Transform::_180,
      Transform::_270,
      Transform::Flipped,
      Transform::Flipped90,
      Transform::Flipped180,
      Transform::Flipped270,
    ];
    for transform in transforms {
      let mapped =
        super::map_point(913.0, 277.0, 1920, 1080, 1280, 720, transform);
      let point = super::unmap_point(
        mapped.0, mapped.1, 1920, 1080, 1280, 720, transform,
      );
      assert!((point.0 - 913.0).abs() < 0.0001);
      assert!((point.1 - 277.0).abs() < 0.0001);
    }
  }
}
