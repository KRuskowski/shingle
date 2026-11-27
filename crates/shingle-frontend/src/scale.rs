//! Scale's layout and filter: pure geometry and matching, no GPU state.

use std::{collections::HashMap, time::Instant};

use shingle::SurfaceSnapshot;
use shingle_core::{Rect, WindowId as TileWindowId};
use shingle_render::RenderRect;

use crate::animate::Timing;

/// Transient overview state.
/// Original layouts and compositor opacity stay intact.
///
/// `initial_focus` is restored on cancellation. `geometry` contains only the
/// renderer's thumbnail rectangles, never replacement tiling geometry.
pub struct ScaleState {
  /// Windows shown, in selection order.
  pub windows: Vec<TileWindowId>,
  /// Index into `windows` of the selected window.
  pub selected: usize,
  /// Layout cell of each window, in `windows` order.
  pub cells: Vec<RenderRect>,
  /// Target thumbnail rectangle of each window.
  pub geometry: HashMap<TileWindowId, RenderRect>,
  /// Thumbnail rectangle of each window when the animation started.
  pub animation_origin: HashMap<TileWindowId, RenderRect>,
  /// Compositor opacity of each window before Scale started.
  pub opacity_origin: HashMap<TileWindowId, f32>,
  /// When the current layout animation started.
  pub animation_started: Instant,
  /// Whether windows from every workspace are shown.
  pub all_workspaces: bool,
  /// Window focused before Scale started.
  pub initial_focus: Option<TileWindowId>,
  /// Workspace that was current before Scale started.
  pub initial_workspace: usize,
}

/// Normalizes metadata and queries consistently for Scale's title filter.
pub fn normalize_filter_text(text: &str, case_sensitive: bool) -> String {
  let text = text
    .chars()
    .map(|character| {
      if character.is_whitespace() {
        ' '
      } else {
        character
      }
    })
    .collect::<String>();
  if case_sensitive {
    text
  } else {
    text.to_lowercase()
  }
}

/// Returns whether a title or app ID contains the active filter.
pub fn scale_filter_matches(
  snapshot: &SurfaceSnapshot,
  filter: &str,
  case_sensitive: bool,
) -> bool {
  if filter.is_empty() {
    return true;
  }
  let filter = normalize_filter_text(filter, case_sensitive);
  [snapshot.title.as_deref(), snapshot.app_id.as_deref()]
    .into_iter()
    .flatten()
    .any(|value| normalize_filter_text(value, case_sensitive).contains(&filter))
}

/// Returns the smallest square-like column count that can hold `count` items.
pub fn scale_columns(count: usize) -> usize {
  if count == 0 {
    0
  } else {
    (count as f64).sqrt().ceil() as usize
  }
}

/// Lays out row-major cells with output offsets, margins, and inter-cell gaps.
pub fn scale_cells(
  output: Rect,
  count: usize,
  spacing: u32,
  outer_margin: u32,
) -> Vec<RenderRect> {
  let columns = scale_columns(count);
  if columns == 0 {
    return Vec::new();
  }
  let rows = count.div_ceil(columns);
  let horizontal_spacing = spacing.saturating_mul((columns - 1) as u32);
  let vertical_spacing = spacing.saturating_mul((rows - 1) as u32);
  let available_width = output
    .width
    .saturating_sub(outer_margin.saturating_mul(2))
    .saturating_sub(horizontal_spacing);
  let available_height = output
    .height
    .saturating_sub(outer_margin.saturating_mul(2))
    .saturating_sub(vertical_spacing);
  let width = available_width / columns as u32;
  let height = available_height / rows as u32;
  (0..count)
    .map(|index| {
      let column = index % columns;
      let row = index / columns;
      let dx = outer_margin.saturating_add(
        (column as u32).saturating_mul(width.saturating_add(spacing)),
      );
      let dy = outer_margin.saturating_add(
        (row as u32).saturating_mul(height.saturating_add(spacing)),
      );
      RenderRect {
        x: output
          .x
          .saturating_add(i32::try_from(dx).unwrap_or(i32::MAX)),
        y: output
          .y
          .saturating_add(i32::try_from(dy).unwrap_or(i32::MAX)),
        width,
        height,
      }
    })
    .collect()
}

/// Interpolates renderer-only rectangles for a Scale transition.
pub fn interpolate_scale_rect(
  origin: RenderRect,
  target: RenderRect,
  progress: f32,
) -> RenderRect {
  let progress = f64::from(progress.clamp(0.0, 1.0));
  let interpolate = |start: f64, end: f64| start + (end - start) * progress;
  RenderRect {
    x: interpolate(f64::from(origin.x), f64::from(target.x)).round() as i32,
    y: interpolate(f64::from(origin.y), f64::from(target.y)).round() as i32,
    width: interpolate(f64::from(origin.width), f64::from(target.width)).round()
      as u32,
    height: interpolate(f64::from(origin.height), f64::from(target.height))
      .round() as u32,
  }
}

/// Captures the currently displayed thumbnail rectangles at a point in time.
pub fn scale_geometry_at(
  state: &ScaleState,
  now: Instant,
  timing: Timing,
) -> HashMap<TileWindowId, RenderRect> {
  let progress = timing.progress(state.animation_started, now);
  state
    .windows
    .iter()
    .filter_map(|window| {
      let target = *state.geometry.get(window)?;
      let origin = state
        .animation_origin
        .get(window)
        .copied()
        .unwrap_or(target);
      Some((*window, interpolate_scale_rect(origin, target, progress)))
    })
    .collect()
}

/// Centers a thumbnail in its cell while preserving the original aspect ratio.
///
/// Upscaling is capped at the original size unless `allow_zoom` is enabled.
pub fn fit_scale_tile(
  cell: RenderRect,
  original: RenderRect,
  allow_zoom: bool,
) -> RenderRect {
  if original.width == 0
    || original.height == 0
    || cell.width == 0
    || cell.height == 0
  {
    return RenderRect {
      x: cell
        .x
        .saturating_add(i32::try_from(cell.width / 2).unwrap_or(i32::MAX)),
      y: cell
        .y
        .saturating_add(i32::try_from(cell.height / 2).unwrap_or(i32::MAX)),
      width: 0,
      height: 0,
    };
  }
  let mut factor = (f64::from(cell.width) / f64::from(original.width))
    .min(f64::from(cell.height) / f64::from(original.height));
  if !allow_zoom {
    factor = factor.min(1.0);
  }
  let width = ((f64::from(original.width) * factor).round() as u32)
    .max(1)
    .min(cell.width);
  let height = ((f64::from(original.height) * factor).round() as u32)
    .max(1)
    .min(cell.height);
  RenderRect {
    x: cell.x.saturating_add(
      i32::try_from((cell.width - width) / 2).unwrap_or(i32::MAX),
    ),
    y: cell.y.saturating_add(
      i32::try_from((cell.height - height) / 2).unwrap_or(i32::MAX),
    ),
    width,
    height,
  }
}

/// Returns whether `(x, y)` lies inside `rect`, including its top-left edge.
pub fn rect_contains(rect: RenderRect, x: f64, y: f64) -> bool {
  x >= f64::from(rect.x)
    && y >= f64::from(rect.y)
    && x < f64::from(rect.x) + f64::from(rect.width)
    && y < f64::from(rect.y) + f64::from(rect.height)
}

#[cfg(test)]
mod tests {
  use shingle::{SurfaceId, SurfaceRole};

  use super::*;

  #[test]
  fn scale_cells_respect_output_offsets_margins_and_spacing() {
    assert_eq!(
      scale_cells(Rect::new(100, 50, 800, 600), 1, 50, 20),
      vec![RenderRect {
        x: 120,
        y: 70,
        width: 760,
        height: 560,
      }]
    );
    assert_eq!(
      scale_cells(Rect::new(0, 0, 800, 600), 4, 20, 10),
      vec![
        RenderRect {
          x: 10,
          y: 10,
          width: 380,
          height: 280,
        },
        RenderRect {
          x: 410,
          y: 10,
          width: 380,
          height: 280,
        },
        RenderRect {
          x: 10,
          y: 310,
          width: 380,
          height: 280,
        },
        RenderRect {
          x: 410,
          y: 310,
          width: 380,
          height: 280,
        },
      ]
    );
  }

  #[test]
  fn scale_cells_handle_odd_counts() {
    let cells = scale_cells(Rect::new(0, 0, 900, 600), 5, 0, 0);
    assert_eq!(scale_columns(5), 3);
    assert_eq!(cells.len(), 5);
    assert!(
      cells
        .iter()
        .all(|cell| cell.width == 300 && cell.height == 300)
    );
    assert_eq!(cells[4].x, 300);
    assert_eq!(cells[4].y, 300);
    assert!(scale_cells(Rect::new(0, 0, 900, 600), 0, 0, 0).is_empty());
  }

  #[test]
  fn scale_transition_interpolates_and_clamps_geometry() {
    let origin = RenderRect {
      x: 0,
      y: 0,
      width: 100,
      height: 100,
    };
    let target = RenderRect {
      x: 100,
      y: 50,
      width: 200,
      height: 50,
    };
    assert_eq!(interpolate_scale_rect(origin, target, 0.0), origin);
    assert_eq!(
      interpolate_scale_rect(origin, target, 0.5),
      RenderRect {
        x: 50,
        y: 25,
        width: 150,
        height: 75,
      }
    );
    assert_eq!(interpolate_scale_rect(origin, target, 1.0), target);
    assert_eq!(interpolate_scale_rect(origin, target, 2.0), target);
  }

  #[test]
  fn scale_tiles_preserve_aspect_and_control_upscaling() {
    let cell = RenderRect {
      x: 10,
      y: 20,
      width: 100,
      height: 100,
    };
    let original = RenderRect {
      x: 0,
      y: 0,
      width: 200,
      height: 100,
    };
    assert_eq!(
      fit_scale_tile(cell, original, false),
      RenderRect {
        x: 10,
        y: 45,
        width: 100,
        height: 50,
      }
    );

    let small = RenderRect {
      width: 40,
      height: 20,
      ..original
    };
    assert_eq!(
      fit_scale_tile(cell, small, false),
      RenderRect {
        x: 40,
        y: 60,
        width: 40,
        height: 20,
      }
    );
    assert_eq!(
      fit_scale_tile(cell, small, true),
      RenderRect {
        x: 10,
        y: 45,
        width: 100,
        height: 50,
      }
    );
  }

  #[test]
  fn scale_filter_matches_title_and_app_id() {
    let snapshot = SurfaceSnapshot {
      id: SurfaceId(1),
      window: None,
      parent: None,
      role: SurfaceRole::Toplevel,
      app_id: Some("org.example.Editor".into()),
      title: Some("Alpha\tEditor".into()),
      icon: None,
      geometry: Rect::new(0, 0, 100, 100),
      opacity: 1.0,
      buffer: None,
    };
    assert!(scale_filter_matches(&snapshot, "alpha editor", false));
    assert!(scale_filter_matches(&snapshot, "ORG.EXAMPLE", false));
    assert!(!scale_filter_matches(&snapshot, "alpha editor", true));
    assert!(!scale_filter_matches(&snapshot, "missing", false));
    assert_eq!(normalize_filter_text("Café\tEditor", false), "café editor");
  }
}
