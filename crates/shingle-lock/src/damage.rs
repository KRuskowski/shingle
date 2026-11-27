//! What changed from one frame to the next, so that a frame that only
//! moves the clock or the input reads back and damages only their part
//! of the buffer: each output alternates two `wl_shm` buffers, and the
//! one drawn into holds a frame from two frames ago, so it needs what
//! changed in both since.

use std::collections::VecDeque;

/// A rectangle of pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
  /// Its left edge.
  pub x: u32,
  /// Its top edge.
  pub y: u32,
  /// Its width.
  pub width: u32,
  /// Its height.
  pub height: u32,
}

impl Rect {
  /// All of a target of `size`.
  pub fn full(size: (u32, u32)) -> Self {
    Self {
      x: 0,
      y: 0,
      width: size.0,
      height: size.1,
    }
  }

  /// Whether it covers no pixel.
  pub fn is_empty(&self) -> bool {
    self.width == 0 || self.height == 0
  }

  /// The pixels of a box from (x0, y0) to (x1, y1), rounded outwards and
  /// cut to a target of `size`.
  pub fn around(
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    size: (u32, u32),
  ) -> Self {
    let clamp = |value: f32, limit: u32| value.clamp(0.0, limit as f32) as u32;
    let left = clamp(x0.floor(), size.0);
    let top = clamp(y0.floor(), size.1);
    let right = clamp(x1.ceil(), size.0);
    let bottom = clamp(y1.ceil(), size.1);
    Self {
      x: left,
      y: top,
      width: right.saturating_sub(left),
      height: bottom.saturating_sub(top),
    }
  }

  /// The smallest rectangle around both; an empty one adds nothing.
  pub fn union(self, other: Self) -> Self {
    if self.is_empty() {
      return other;
    }
    if other.is_empty() {
      return self;
    }
    let x = self.x.min(other.x);
    let y = self.y.min(other.y);
    let right = (self.x + self.width).max(other.x + other.width);
    let bottom = (self.y + self.height).max(other.y + other.height);
    Self {
      x,
      y,
      width: right - x,
      height: bottom - y,
    }
  }
}

/// The damage of the last few frames of one target, by frame number.
#[derive(Clone, Debug, Default)]
pub struct History {
  frames: VecDeque<(u64, Rect)>,
  next: u64,
}

/// How many frames' damage is kept: enough for two buffers and a
/// spare.
const KEPT: usize = 4;

impl History {
  /// Records the next frame, which changed `damage` from the one before;
  /// returns its number.
  pub fn push(&mut self, damage: Rect) -> u64 {
    let serial = self.next;
    self.next += 1;
    self.frames.push_back((serial, damage));
    while self.frames.len() > KEPT {
      self.frames.pop_front();
    }
    serial
  }

  /// What a buffer holding frame `holds` must have redrawn to show the
  /// newest frame: the union of the damage since, or `None` when that is
  /// not known any more (or the buffer holds nothing), meaning all of it.
  pub fn since(&self, holds: Option<u64>) -> Option<Rect> {
    let holds = holds?;
    let newest = self.frames.back()?.0;
    if holds > newest {
      return None;
    }
    let mut rect = Rect::default();
    for wanted in holds + 1..=newest {
      let (_, damage) = self.frames.iter().find(|(s, _)| *s == wanted)?;
      rect = rect.union(*damage);
    }
    Some(rect)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
      x,
      y,
      width,
      height,
    }
  }

  #[test]
  fn rectangles_round_out_clip_and_join() {
    let size = (100, 50);
    assert_eq!(
      Rect::around((10.5, 4.2), (20.1, 9.0), size),
      rect(10, 4, 11, 5)
    );
    assert_eq!(
      Rect::around((-5.0, -5.0), (5.0, 60.0), size),
      rect(0, 0, 5, 50)
    );
    assert!(Rect::around((200.0, 0.0), (300.0, 10.0), size).is_empty());
    let joined = rect(0, 0, 10, 10).union(rect(20, 5, 5, 20));
    assert_eq!(joined, rect(0, 0, 25, 25));
    assert_eq!(Rect::default().union(rect(1, 2, 3, 4)), rect(1, 2, 3, 4));
    assert_eq!(rect(1, 2, 3, 4).union(Rect::default()), rect(1, 2, 3, 4));
    assert_eq!(Rect::full(size), rect(0, 0, 100, 50));
  }

  #[test]
  fn a_buffer_two_frames_old_needs_both_frames_damage() {
    let mut history = History::default();
    assert_eq!(history.since(None), None);
    let first = history.push(rect(0, 0, 100, 50));
    let second = history.push(rect(10, 10, 5, 5));
    let third = history.push(rect(40, 10, 5, 5));
    assert_eq!((first, second, third), (0, 1, 2));
    assert_eq!(history.since(Some(second)), Some(rect(40, 10, 5, 5)));
    assert_eq!(history.since(Some(first)), Some(rect(10, 10, 35, 5)));
    assert_eq!(history.since(Some(third)), Some(Rect::default()));
    assert_eq!(history.since(None), None);
    // Too old to know: all of it.
    for _ in 0..KEPT {
      history.push(rect(0, 0, 1, 1));
    }
    assert_eq!(history.since(Some(first)), None);
    assert_eq!(history.since(Some(99)), None);
  }
}
