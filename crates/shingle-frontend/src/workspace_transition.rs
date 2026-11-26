//! Vswitch's animated slide between workspaces, and the wall vswipe
//! drags.

use std::time::{Duration, Instant};

use shingle_core::Rect;
use shingle_render::{RenderRect, animation_progress};

use crate::expo::interpolate_rect;

/// Output geometry needed to place workspaces on an animated wall.
#[derive(Clone, Copy)]
pub struct TransitionLayout {
  /// The output the wall is drawn on.
  pub output: Rect,
  /// Workspaces per row of the wall.
  pub grid_width: u32,
  /// Gap between neighbouring workspaces, in pixels.
  pub gap: f64,
}

/// A renderer-only Vswitch transition between two workspace indices.
pub struct WorkspaceTransition {
  /// Workspace the slide starts from.
  pub from: usize,
  /// Workspace the slide ends on.
  pub to: usize,
  /// When the slide started.
  pub started: Instant,
}

impl WorkspaceTransition {
  /// Starts a slide from `from` to `to` now.
  pub fn new(from: usize, to: usize) -> Self {
    Self {
      from,
      to,
      started: Instant::now(),
    }
  }

  /// Places one window relative to the animated source and destination.
  /// Other workspaces retain their wall order and move offscreen naturally.
  pub fn geometry(
    &self,
    original: Rect,
    workspace: usize,
    layout: TransitionLayout,
    now: Instant,
    duration: Duration,
  ) -> RenderRect {
    let window = RenderRect {
      x: original.x,
      y: original.y,
      width: original.width,
      height: original.height,
    };
    let (window_x, window_y) = wall_origin(workspace, layout);
    let (from_x, from_y) = wall_origin(self.from, layout);
    let (to_x, to_y) = wall_origin(self.to, layout);
    let origin = offset_rect(window, window_x - from_x, window_y - from_y);
    let target = offset_rect(window, window_x - to_x, window_y - to_y);
    let progress = animation_progress(self.started, now, duration);
    interpolate_rect(origin, target, progress)
  }
}

/// Places one window of `workspace` on the wall when the wall shows
/// `viewport`, a fractional (column, row) at the output's top-left
/// corner, as vswipe draws it while a swipe drags the wall.
pub fn wall_geometry(
  original: Rect,
  workspace: usize,
  layout: TransitionLayout,
  viewport: (f64, f64),
) -> RenderRect {
  let window = RenderRect {
    x: original.x,
    y: original.y,
    width: original.width,
    height: original.height,
  };
  let (window_x, window_y) = wall_origin(workspace, layout);
  let (view_x, view_y) = wall_point(viewport.0, viewport.1, layout);
  offset_rect(window, window_x - view_x, window_y - view_y)
}

fn wall_origin(workspace: usize, layout: TransitionLayout) -> (f64, f64) {
  let column = workspace % layout.grid_width.max(1) as usize;
  let row = workspace / layout.grid_width.max(1) as usize;
  wall_point(column as f64, row as f64, layout)
}

/// The layout point of fractional wall position (`column`, `row`).
fn wall_point(column: f64, row: f64, layout: TransitionLayout) -> (f64, f64) {
  (
    f64::from(layout.output.x)
      + column * (f64::from(layout.output.width) + layout.gap),
    f64::from(layout.output.y)
      + row * (f64::from(layout.output.height) + layout.gap),
  )
}

fn offset_rect(rect: RenderRect, dx: f64, dy: f64) -> RenderRect {
  RenderRect {
    x: (f64::from(rect.x) + dx).round() as i32,
    y: (f64::from(rect.y) + dy).round() as i32,
    ..rect
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn horizontal_switch_preserves_order_size_and_gap() {
    let started = Instant::now();
    let transition = WorkspaceTransition {
      from: 0,
      to: 1,
      started,
    };
    let layout = TransitionLayout {
      output: Rect::new(0, 0, 200, 100),
      grid_width: 2,
      gap: 10.0,
    };
    let original = Rect::new(0, 0, 200, 100);
    assert_eq!(
      transition.geometry(
        original,
        0,
        layout,
        started,
        Duration::from_millis(300),
      ),
      RenderRect {
        x: 0,
        y: 0,
        width: 200,
        height: 100,
      }
    );
    let middle = transition.geometry(
      original,
      0,
      layout,
      started + Duration::from_millis(150),
      Duration::from_millis(300),
    );
    assert_eq!(middle.x, -105);
    let old = transition.geometry(
      original,
      0,
      layout,
      started + Duration::from_millis(300),
      Duration::from_millis(300),
    );
    let new = transition.geometry(
      original,
      1,
      layout,
      started + Duration::from_millis(300),
      Duration::from_millis(300),
    );
    assert_eq!(old.x, -210);
    assert_eq!(new.x, 0);
    assert_eq!(new.width, original.width);
    assert_eq!(new.height, original.height);
  }

  #[test]
  fn the_wall_shows_the_fractional_viewport() {
    let layout = TransitionLayout {
      output: Rect::new(100, 50, 200, 100),
      grid_width: 3,
      gap: 32.0,
    };
    let window = Rect::new(110, 60, 50, 40);
    // At the window's own workspace it is where it is.
    assert_eq!(
      wall_geometry(window, 4, layout, (1.0, 1.0)),
      RenderRect {
        x: 110,
        y: 60,
        width: 50,
        height: 40,
      }
    );
    // Half way to the workspace to the right, it is half a workspace and
    // half a gap to the left.
    let half = wall_geometry(window, 4, layout, (1.5, 1.0));
    assert_eq!((half.x, half.y), (110 - 116, 60));
    // The neighbour below comes up from under the output.
    let below = wall_geometry(window, 7, layout, (1.0, 1.25));
    assert_eq!((below.x, below.y), (110, 60 + 132 - 33));
    // Beyond the grid's edge, the wall moves on.
    let beyond = wall_geometry(window, 0, layout, (-0.5, 0.0));
    assert_eq!(beyond.x, 110 + 116);
  }

  #[test]
  fn nonzero_output_origin_and_vertical_motion_are_preserved() {
    let started = Instant::now();
    let transition = WorkspaceTransition {
      from: 1,
      to: 0,
      started,
    };
    let layout = TransitionLayout {
      output: Rect::new(100, 50, 200, 100),
      grid_width: 1,
      gap: 20.0,
    };
    let geometry = transition.geometry(
      Rect::new(110, 60, 50, 40),
      0,
      layout,
      started,
      Duration::from_millis(180),
    );
    assert_eq!(
      geometry,
      RenderRect {
        x: 110,
        y: -60,
        width: 50,
        height: 40,
      }
    );
  }
}
