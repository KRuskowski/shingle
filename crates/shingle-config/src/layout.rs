//! The output layout as pure functions: where outputs go, how the pointer
//! is kept on them and which output lies in a direction.
//!
//! [`layout`] is wlroots' `output_layout_reconfigure`
//! (types/wlr_output_layout.c), which Wayfire builds on;
//! [`closest_point`] is `wlr_output_layout_closest_point`;
//! [`output_in_direction`] and [`output_relative`] are Wayfire's `oswitch`
//! (plugins/single_plugins/oswitch.cpp); [`layout_warnings`] are the checks
//! of Wayfire's `test_configuration` (src/core/output-layout.cpp).

use shingle_core::Rect;

use crate::OutputPosition;

/// One output as the layout sees it: where it was configured to go and its
/// size in layout coordinates ([`logical_size`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutOutput {
  /// The configured position.
  pub position: OutputPosition,
  /// Logical width.
  pub width: u32,
  /// Logical height.
  pub height: u32,
}

/// An output's size in layout coordinates: its mode divided by its scale,
/// truncated, with width and height swapped for 90° and 270° transforms, as
/// Wayfire's `calculate_geometry_from_state` and wlroots'
/// `wlr_output_effective_resolution` compute it. `transform` takes the
/// spellings of [`crate::OutputConfig::transform`].
pub fn logical_size(
  mode: (u32, u32),
  scale: f64,
  transform: &str,
) -> (u32, u32) {
  let (width, height) = if transform.contains("90") || transform.contains("270")
  {
    (mode.1, mode.0)
  } else {
    mode
  };
  let scale = if scale.is_finite() && scale > 0.0 {
    scale
  } else {
    1.0
  };
  let divide = |extent: u32| (f64::from(extent) / scale) as u32;
  (divide(width), divide(height))
}

/// Places outputs, given in the order they were added, as wlroots does:
/// fixed outputs where configured, and automatic ones left to right in
/// add order, starting at the right edge of the rightmost fixed output and
/// at its `y`, or at 0,0 when every output is automatic. Returns one
/// rectangle per output, in the same order.
pub fn layout(outputs: &[LayoutOutput]) -> Vec<Rect> {
  let mut rightmost: Option<(i64, i32)> = None;
  for output in outputs {
    if let OutputPosition::Fixed { x, y } = output.position {
      let right = i64::from(x) + i64::from(output.width);
      if rightmost.is_none_or(|(edge, _)| right > edge) {
        rightmost = Some((right, y));
      }
    }
  }
  let (mut next_x, next_y) = rightmost.unwrap_or((0, 0));
  outputs
    .iter()
    .map(|output| match output.position {
      OutputPosition::Fixed { x, y } => {
        Rect::new(x, y, output.width, output.height)
      }
      OutputPosition::Auto => {
        let x = clamp_i32(next_x);
        next_x += i64::from(output.width);
        Rect::new(x, next_y, output.width, output.height)
      }
    })
    .collect()
}

/// The point on any output closest to `(x, y)`, as
/// `wlr_output_layout_closest_point` finds it: inside an output it is the
/// point itself; the right and bottom edges are exclusive, so a point
/// beyond them is pulled to 1/256 inside. With no outputs, or only empty
/// ones, the point is returned unchanged.
pub fn closest_point(outputs: &[Rect], x: f64, y: f64) -> (f64, f64) {
  let mut closest = (x, y);
  let mut best = f64::MAX;
  for output in outputs {
    if output.width == 0 || output.height == 0 {
      continue;
    }
    let clamp = |value: f64, start: i32, extent: u32| {
      let start = f64::from(start);
      let end = start + f64::from(extent) - 1.0 / 256.0;
      if value < start {
        start
      } else if value > end {
        end
      } else {
        value
      }
    };
    let point = (
      clamp(x, output.x, output.width),
      clamp(y, output.y, output.height),
    );
    let distance = (x - point.0).powi(2) + (y - point.1).powi(2);
    let distance = if distance.is_finite() {
      distance
    } else {
      f64::MAX
    };
    if distance < best {
      closest = point;
      best = distance;
    }
  }
  closest
}

/// The output `step` places after `current` in add order, wrapping, as
/// oswitch's next and previous output do. `current` is returned when there
/// are no outputs.
pub fn output_relative(count: usize, current: usize, step: isize) -> usize {
  if count == 0 {
    return current;
  }
  let count = count as isize;
  let current = current as isize;
  (((current + step) % count + count) % count) as usize
}

/// The output that lies in direction `(dx, dy)` from `current`, as oswitch
/// picks it: among outputs whose centre lies that way, those that overlap
/// `current` by at least 20 % on the other axis, the best ratio of overlap
/// to the distance between centres. Returns `current` when none qualifies.
/// Exactly one of `dx` and `dy` is expected to be nonzero.
pub fn output_in_direction(
  outputs: &[Rect],
  current: usize,
  dx: i32,
  dy: i32,
) -> usize {
  const MIN_OVERLAP: f64 = 20.0;
  let Some(from) = outputs.get(current) else {
    return current;
  };
  let center = |rect: &Rect| {
    (
      i64::from(rect.x) + i64::from(rect.width / 2),
      i64::from(rect.y) + i64::from(rect.height / 2),
    )
  };
  let (from_x, from_y) = center(from);
  let mut best = None;
  let mut best_score = f64::NEG_INFINITY;
  for (index, output) in outputs.iter().enumerate() {
    if index == current {
      continue;
    }
    let (x, y) = center(output);
    let (delta_x, delta_y) = ((x - from_x) as f64, (y - from_y) as f64);
    if (dx != 0 && delta_x * f64::from(dx) <= 0.0)
      || (dy != 0 && delta_y * f64::from(dy) <= 0.0)
    {
      continue;
    }
    let overlap = |start: i32, extent: u32, other: i32, other_extent: u32| {
      let end = i64::from(start) + i64::from(extent);
      let other_end = i64::from(other) + i64::from(other_extent);
      let shared = end.min(other_end) - i64::from(start.max(other));
      shared as f64 / f64::from(extent)
    };
    let ortho = if dx != 0 {
      overlap(from.y, from.height, output.y, output.height)
    } else if dy != 0 {
      overlap(from.x, from.width, output.x, output.width)
    } else {
      1.0
    };
    if ortho * 100.0 < MIN_OVERLAP {
      continue;
    }
    let score = ortho / (delta_x * delta_x + delta_y * delta_y).sqrt();
    if score > best_score {
      best = Some(index);
      best_score = score;
    }
  }
  best.unwrap_or(current)
}

/// Wayfire's warnings about a layout, from the fixed outputs only: overlap
/// ("unexpected behavior might occur") and gaps that split them into
/// groups the pointer cannot cross. Neither is refused.
pub fn layout_warnings(outputs: &[LayoutOutput]) -> Vec<&'static str> {
  let fixed = layout(outputs)
    .into_iter()
    .zip(outputs)
    .filter(|(_, output)| {
      matches!(output.position, OutputPosition::Fixed { .. })
    })
    .map(|(rect, _)| rect)
    .collect::<Vec<_>>();
  let mut warnings = Vec::new();
  if overlapping(&fixed) {
    warnings.push(
      "overlapping outputs in the output configuration, unexpected \
       behavior might occur",
    );
  }
  if disjoint(&fixed) {
    warnings.push(
      "outputs have been configured with gaps between them, pointer will \
       not be movable between them",
    );
  }
  warnings
}

/// Whether two of the rectangles share interior points.
pub fn overlapping(rects: &[Rect]) -> bool {
  rects.iter().enumerate().any(|(index, first)| {
    rects[index + 1..].iter().any(|second| {
      let left = i64::from(first.x).max(i64::from(second.x));
      let right = right_edge(first).min(right_edge(second));
      let top = i64::from(first.y).max(i64::from(second.y));
      let bottom = bottom_edge(first).min(bottom_edge(second));
      left < right && top < bottom
    })
  })
}

/// Whether the rectangles fall into more than one group of rectangles that
/// touch or overlap, as Wayfire's `test_disjoint_outputs` decides.
pub fn disjoint(rects: &[Rect]) -> bool {
  if rects.is_empty() {
    return false;
  }
  let touching = |a: &Rect, b: &Rect| {
    !(right_edge(a) < i64::from(b.x)
      || bottom_edge(a) < i64::from(b.y)
      || right_edge(b) < i64::from(a.x)
      || bottom_edge(b) < i64::from(a.y))
  };
  let mut reached = vec![false; rects.len()];
  let mut stack = vec![0];
  reached[0] = true;
  while let Some(index) = stack.pop() {
    for (other, rect) in rects.iter().enumerate() {
      if !reached[other] && touching(&rects[index], rect) {
        reached[other] = true;
        stack.push(other);
      }
    }
  }
  reached.contains(&false)
}

fn right_edge(rect: &Rect) -> i64 {
  i64::from(rect.x) + i64::from(rect.width)
}

fn bottom_edge(rect: &Rect) -> i64 {
  i64::from(rect.y) + i64::from(rect.height)
}

fn clamp_i32(value: i64) -> i32 {
  value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
  use super::*;

  fn auto(width: u32, height: u32) -> LayoutOutput {
    LayoutOutput {
      position: OutputPosition::Auto,
      width,
      height,
    }
  }

  fn fixed(x: i32, y: i32, width: u32, height: u32) -> LayoutOutput {
    LayoutOutput {
      position: OutputPosition::Fixed { x, y },
      width,
      height,
    }
  }

  #[test]
  fn automatic_outputs_go_left_to_right_in_add_order() {
    assert_eq!(
      layout(&[auto(1920, 1080), auto(1280, 1024), auto(800, 600)]),
      [
        Rect::new(0, 0, 1920, 1080),
        Rect::new(1920, 0, 1280, 1024),
        Rect::new(3200, 0, 800, 600),
      ]
    );
    assert!(layout(&[]).is_empty());
  }

  #[test]
  fn automatic_outputs_follow_the_rightmost_fixed_one() {
    // wlroots starts at the right edge of the fixed output that reaches
    // furthest right, at that output's y, whatever the add order.
    let rects = layout(&[
      auto(1000, 800),
      fixed(0, 500, 1920, 1080),
      fixed(-1280, 0, 1280, 1024),
      auto(800, 600),
    ]);
    assert_eq!(
      rects,
      [
        Rect::new(1920, 500, 1000, 800),
        Rect::new(0, 500, 1920, 1080),
        Rect::new(-1280, 0, 1280, 1024),
        Rect::new(2920, 500, 800, 600),
      ]
    );
  }

  #[test]
  fn a_single_automatic_output_sits_at_the_origin() {
    assert_eq!(layout(&[auto(1280, 720)]), [Rect::new(0, 0, 1280, 720)]);
  }

  #[test]
  fn logical_sizes_divide_by_the_scale_and_follow_the_transform() {
    assert_eq!(logical_size((3840, 2160), 2.0, "normal"), (1920, 1080));
    // Truncated, as Wayfire's integer conversion does.
    assert_eq!(logical_size((2560, 1440), 1.5, "normal"), (1706, 960));
    for transform in ["90", "270", "90_flipped", "flipped-270", "flipped90"] {
      assert_eq!(logical_size((1920, 1080), 1.0, transform), (1080, 1920));
    }
    for transform in ["180", "flipped", "180_flipped"] {
      assert_eq!(logical_size((1920, 1080), 1.0, transform), (1920, 1080));
    }
    assert_eq!(logical_size((1920, 1080), 0.0, "normal"), (1920, 1080));
  }

  #[test]
  fn the_pointer_is_kept_on_the_nearest_output() {
    let outputs = [Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1280, 1024)];
    // Inside an output nothing changes, across the shared edge too.
    assert_eq!(closest_point(&outputs, 100.5, 200.0), (100.5, 200.0));
    assert_eq!(closest_point(&outputs, 1920.0, 10.0), (1920.0, 10.0));
    // Below the shorter output, the edge of the taller one is nearer.
    assert_eq!(
      closest_point(&outputs, 1925.0, 1050.0),
      (1920.0 - 1.0 / 256.0, 1050.0)
    );
    assert_eq!(
      closest_point(&outputs, 1900.0, 1100.0),
      (1900.0, 1080.0 - 1.0 / 256.0)
    );
    assert_eq!(closest_point(&outputs, -50.0, -50.0), (0.0, 0.0));
    assert_eq!(
      closest_point(&outputs, 4000.0, 500.0),
      (3200.0 - 1.0 / 256.0, 500.0)
    );
    assert_eq!(closest_point(&[], 5.0, 6.0), (5.0, 6.0));
    assert_eq!(
      closest_point(&[Rect::new(0, 0, 0, 10)], 5.0, 6.0),
      (5.0, 6.0)
    );
  }

  #[test]
  fn next_and_previous_wrap_in_add_order() {
    assert_eq!(output_relative(3, 0, 1), 1);
    assert_eq!(output_relative(3, 2, 1), 0);
    assert_eq!(output_relative(3, 0, -1), 2);
    assert_eq!(output_relative(3, 1, -4), 0);
    assert_eq!(output_relative(0, 0, 1), 0);
  }

  #[test]
  fn directions_need_overlap_and_prefer_close_outputs() {
    // A laptop at the origin, a monitor to its right, one above that
    // monitor and a small one far right, half overlapping vertically.
    let outputs = [
      Rect::new(0, 0, 1920, 1080),
      Rect::new(1920, 0, 2560, 1440),
      Rect::new(1920, -1440, 2560, 1440),
      Rect::new(4480, 900, 1000, 1000),
    ];
    assert_eq!(output_in_direction(&outputs, 0, 1, 0), 1);
    assert_eq!(output_in_direction(&outputs, 1, -1, 0), 0);
    assert_eq!(output_in_direction(&outputs, 1, 0, -1), 2);
    assert_eq!(output_in_direction(&outputs, 2, 0, 1), 1);
    // The laptop shares no rows with the output above the monitor.
    assert_eq!(output_in_direction(&outputs, 2, -1, 0), 2);
    // From the monitor, the small output overlaps 540 of 1440 rows, over
    // 20 %, and is the only one to the right.
    assert_eq!(output_in_direction(&outputs, 1, 1, 0), 3);
    // From the laptop, the small output overlaps 180 of 1080 rows, under
    // 20 %, so only the monitor qualifies.
    assert_eq!(
      output_in_direction(
        &outputs[..1]
          .iter()
          .chain(&outputs[3..])
          .copied()
          .collect::<Vec<_>>(),
        0,
        1,
        0
      ),
      0
    );
    // Nothing lies left of the laptop.
    assert_eq!(output_in_direction(&outputs, 0, -1, 0), 0);
    assert_eq!(output_in_direction(&[], 0, 1, 0), 0);
  }

  #[test]
  fn overlap_below_twenty_percent_does_not_count() {
    let outputs = [
      Rect::new(0, 0, 1000, 1000),
      Rect::new(1000, 850, 1000, 1000),
    ];
    assert_eq!(output_in_direction(&outputs, 0, 1, 0), 0);
    let outputs = [
      Rect::new(0, 0, 1000, 1000),
      Rect::new(1000, 800, 1000, 1000),
    ];
    assert_eq!(output_in_direction(&outputs, 0, 1, 0), 1);
  }

  #[test]
  fn overlap_and_gaps_between_fixed_outputs_are_warned_about() {
    assert!(
      layout_warnings(&[fixed(0, 0, 100, 100), fixed(100, 0, 100, 100)])
        .is_empty()
    );
    let overlap =
      layout_warnings(&[fixed(0, 0, 100, 100), fixed(50, 50, 100, 100)]);
    assert_eq!(overlap.len(), 1);
    assert!(overlap[0].contains("overlapping"));
    let gap =
      layout_warnings(&[fixed(0, 0, 100, 100), fixed(150, 0, 100, 100)]);
    assert_eq!(gap.len(), 1);
    assert!(gap[0].contains("gaps"));
    // Touching at a corner is not a gap; automatic outputs are not checked.
    assert!(
      layout_warnings(&[fixed(0, 0, 100, 100), fixed(100, 100, 10, 10)])
        .is_empty()
    );
    assert!(
      layout_warnings(&[auto(100, 100), fixed(5000, 0, 10, 10)]).is_empty()
    );
    assert!(!disjoint(&[]));
    assert!(!overlapping(&[Rect::new(0, 0, 10, 10)]));
  }
}
