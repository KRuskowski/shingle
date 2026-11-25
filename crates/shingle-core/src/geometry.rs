use serde::{Deserialize, Serialize};

use crate::ShingleError;

/// An axis-aligned rectangle in layout coordinates. The right and bottom edges,
/// `x + width` and `y + height`, are exclusive.
#[derive(
  Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct Rect {
  /// Left edge.
  pub x: i32,
  /// Top edge.
  pub y: i32,
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
}

impl Rect {
  /// Creates a rectangle.
  pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
    Self {
      x,
      y,
      width,
      height,
    }
  }

  /// Returns the rectangle if it is nonempty and its right and bottom edges fit
  /// in `i32`.
  ///
  /// # Errors
  ///
  /// [`ShingleError::InvalidWorkarea`] otherwise.
  pub fn validate(self) -> Result<Self, ShingleError> {
    if self.width == 0 || self.height == 0 {
      return Err(ShingleError::InvalidWorkarea);
    }
    if self.x as i64 + self.width as i64 > i32::MAX as i64
      || self.y as i64 + self.height as i64 > i32::MAX as i64
    {
      return Err(ShingleError::InvalidWorkarea);
    }
    Ok(self)
  }

  pub(crate) fn axis_origin(self, vertical: bool) -> i64 {
    if vertical {
      self.x as i64
    } else {
      self.y as i64
    }
  }

  pub(crate) fn axis_extent(self, vertical: bool) -> u32 {
    if vertical { self.width } else { self.height }
  }

  pub(crate) fn with_axis(
    self,
    vertical: bool,
    origin: i64,
    extent: u32,
  ) -> Self {
    if vertical {
      Self {
        x: origin as i32,
        width: extent,
        ..self
      }
    } else {
      Self {
        y: origin as i32,
        height: extent,
        ..self
      }
    }
  }

  pub(crate) fn inset(self, gaps: Gaps) -> Self {
    let x = self.x as i64 + gaps.left as i64;
    let y = self.y as i64 + gaps.top as i64;
    let right_edge = self.x as i64 + self.width as i64 - gaps.right as i64;
    let bottom_edge = self.y as i64 + self.height as i64 - gaps.bottom as i64;
    Self {
      x: x.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
      y: y.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
      width: (right_edge - x).clamp(0, u32::MAX as i64) as u32,
      height: (bottom_edge - y).clamp(0, u32::MAX as i64) as u32,
    }
  }
}

/// Gaps around a tile, in pixels. The outer sizes apply on edges that touch the
/// workarea; `inner` replaces them on edges shared with another tile.
#[derive(
  Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct Gaps {
  /// Outer gap above the tile.
  pub top: u32,
  /// Outer gap right of the tile.
  pub right: u32,
  /// Outer gap below the tile.
  pub bottom: u32,
  /// Outer gap left of the tile.
  pub left: u32,
  /// Gap on edges shared with another tile.
  pub inner: u32,
}

impl Gaps {
  /// Creates gaps with the same outer size on all four sides.
  pub const fn new(outer: u32, inner: u32) -> Self {
    Self {
      top: outer,
      right: outer,
      bottom: outer,
      left: outer,
      inner,
    }
  }

  pub(crate) fn effective(self, edges: InternalEdges) -> Self {
    Self {
      top: if edges.top { self.inner } else { self.top },
      right: if edges.right { self.inner } else { self.right },
      bottom: if edges.bottom {
        self.inner
      } else {
        self.bottom
      },
      left: if edges.left { self.inner } else { self.left },
      inner: self.inner,
    }
  }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InternalEdges {
  pub top: bool,
  pub right: bool,
  pub bottom: bool,
  pub left: bool,
}

/// The axis along which a split arranges its children, named after its dividers
/// as in Wayfire.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
  /// Children are stacked top to bottom, separated by horizontal dividers.
  Horizontal,
  /// Children are placed left to right, separated by vertical dividers.
  Vertical,
}

impl SplitDirection {
  pub(crate) fn is_vertical(self) -> bool {
    matches!(self, Self::Vertical)
  }

  pub(crate) fn for_direction(direction: Direction) -> Self {
    match direction {
      Direction::Up | Direction::Down => Self::Horizontal,
      Direction::Left | Direction::Right => Self::Vertical,
    }
  }
}

/// A direction for focus, insertion, dropping and resizing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
  /// Towards smaller y.
  Up,
  /// Towards larger y.
  Down,
  /// Towards smaller x.
  Left,
  /// Towards larger x.
  Right,
}

impl Direction {
  pub(crate) fn is_positive(self) -> bool {
    matches!(self, Self::Down | Self::Right)
  }

  pub(crate) fn axis(self) -> SplitDirection {
    SplitDirection::for_direction(self)
  }

  /// Ranks `candidate` as a target for moving focus from `from` in this
  /// direction.
  ///
  /// Returns `None` unless the candidate lies entirely beyond the edge being
  /// crossed. Otherwise returns `(gap, cross)`: the distance between the facing
  /// edges, and how far the midpoint of the crossed edge lies outside the
  /// candidate's span on the other axis, zero if the candidate contains it.
  /// Lower is nearer; callers compare the pair lexicographically and break ties
  /// by window id. On a tight tiling this picks the tile just past the edge's
  /// midpoint.
  pub fn score(self, from: Rect, candidate: Rect) -> Option<(i64, i64)> {
    let left = |rect: Rect| rect.x as i64;
    let top = |rect: Rect| rect.y as i64;
    let right = |rect: Rect| rect.x as i64 + rect.width as i64;
    let bottom = |rect: Rect| rect.y as i64 + rect.height as i64;
    let horizontal_midpoint = left(from) + from.width as i64 / 2;
    let vertical_midpoint = top(from) + from.height as i64 / 2;
    // Like Wayfire's tile plugin, look past the middle of the edge being
    // crossed: the gap is measured between facing edges, and the cross
    // distance is how far that midpoint lies outside the candidate's span.
    let (gap, midpoint, start, end) = match self {
      Self::Up => (
        top(from) - bottom(candidate),
        horizontal_midpoint,
        left(candidate),
        right(candidate),
      ),
      Self::Down => (
        top(candidate) - bottom(from),
        horizontal_midpoint,
        left(candidate),
        right(candidate),
      ),
      Self::Left => (
        left(from) - right(candidate),
        vertical_midpoint,
        top(candidate),
        bottom(candidate),
      ),
      Self::Right => (
        left(candidate) - right(from),
        vertical_midpoint,
        top(candidate),
        bottom(candidate),
      ),
    };
    if gap < 0 {
      return None;
    }
    let cross = if midpoint < start {
      start - midpoint
    } else if midpoint >= end {
      midpoint - end + 1
    } else {
      0
    };
    Some((gap, cross))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn score_measures_edge_gap_and_midpoint_offset() {
    let from = Rect::new(0, 0, 600, 800);
    let upper = Rect::new(600, 0, 600, 400);
    let lower = Rect::new(600, 400, 600, 400);
    assert_eq!(Direction::Right.score(from, upper), Some((0, 1)));
    assert_eq!(Direction::Right.score(from, lower), Some((0, 0)));
    assert_eq!(
      Direction::Right.score(from, Rect::new(700, 900, 10, 10)),
      Some((100, 500))
    );
    assert_eq!(Direction::Left.score(from, upper), None);
    assert_eq!(Direction::Left.score(upper, from), Some((0, 0)));
    assert_eq!(Direction::Down.score(upper, lower), Some((0, 0)));
    assert_eq!(Direction::Up.score(lower, upper), Some((0, 0)));
    assert_eq!(Direction::Down.score(upper, from), None);
    assert_eq!(Direction::Up.score(upper, lower), None);
  }

  #[test]
  fn rect_validate_rejects_empty_and_overflowing_geometry() {
    assert!(Rect::new(-50, -50, 10, 10).validate().is_ok());
    assert!(Rect::new(0, 0, 0, 10).validate().is_err());
    assert!(Rect::new(0, 0, 10, 0).validate().is_err());
    assert!(Rect::new(0, i32::MAX - 9, 10, 10).validate().is_err());
    assert!(Rect::new(0, i32::MAX - 10, 10, 10).validate().is_ok());
  }

  #[test]
  fn inset_clamps_to_empty() {
    let rect = Rect::new(0, 0, 10, 10);
    assert_eq!(rect.inset(Gaps::new(20, 0)), Rect::new(20, 20, 0, 0));
    assert_eq!(rect.inset(Gaps::new(2, 0)), Rect::new(2, 2, 6, 6));
  }
}
