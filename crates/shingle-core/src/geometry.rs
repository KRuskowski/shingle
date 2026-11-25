use serde::{Deserialize, Serialize};

use crate::ShingleError;

#[derive(
  Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct Rect {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
}

impl Rect {
  pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
    Self {
      x,
      y,
      width,
      height,
    }
  }

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

  fn center(self) -> (i64, i64) {
    (
      self.x as i64 * 2 + self.width as i64,
      self.y as i64 * 2 + self.height as i64,
    )
  }
}

#[derive(
  Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct Gaps {
  pub top: u32,
  pub right: u32,
  pub bottom: u32,
  pub left: u32,
  pub inner: u32,
}

impl Gaps {
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
  Horizontal,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
  Up,
  Down,
  Left,
  Right,
}

impl Direction {
  pub(crate) fn is_positive(self) -> bool {
    matches!(self, Self::Down | Self::Right)
  }

  pub(crate) fn axis(self) -> SplitDirection {
    SplitDirection::for_direction(self)
  }

  pub fn score(self, from: Rect, candidate: Rect) -> Option<(i64, i64)> {
    let (fx, fy) = from.center();
    let (cx, cy) = candidate.center();
    let (primary, cross) = match self {
      Self::Up => (fy - cy, (fx - cx).abs()),
      Self::Down => (cy - fy, (fx - cx).abs()),
      Self::Left => (fx - cx, (fy - cy).abs()),
      Self::Right => (cx - fx, (fy - cy).abs()),
    };
    (primary > 0).then_some((primary, cross))
  }
}
