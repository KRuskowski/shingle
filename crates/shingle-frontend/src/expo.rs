//! Expo, Wayfire's workspace overview: selection, zoom and drags.

use std::{collections::HashMap, time::Instant};

use shingle::Direction;
use shingle_core::{Rect, WindowId};
use shingle_render::RenderRect;

use crate::animate::Timing;

/// Renderer-only state for Wayfire's workspace overview.
///
/// Selection is deliberately independent of the compositor's current
/// workspace. The persistent tiling model changes only when the frontend
/// commits this state. Origin rectangles describe the current zoom transition;
/// the other rectangles are its targets.
pub struct ExpoState {
  /// Index of the selected workspace.
  pub selected: usize,
  /// Index of the workspace selected before the last selection change.
  pub previous_selected: usize,
  /// Target rectangle of each workspace cell, row-major.
  pub cells: Vec<RenderRect>,
  /// Rectangle of each workspace cell when the zoom started.
  pub cell_origin: Vec<RenderRect>,
  /// Windows shown in the overview, in drawing order.
  pub windows: Vec<WindowId>,
  /// Target thumbnail rectangle of each window.
  pub geometry: HashMap<WindowId, RenderRect>,
  /// Thumbnail rectangle of each window when the zoom started.
  pub geometry_origin: HashMap<WindowId, RenderRect>,
  /// The workspaces each window overlaps, its own first, with the shift
  /// that moves it onto each
  /// ([`crate::straddle::overlapped_workspaces`]); the thumbnail is drawn
  /// and taken in each of their cells.
  pub overlaps: HashMap<WindowId, crate::straddle::Overlaps>,
  /// The output Expo shows.
  output: Rect,
  /// When the current zoom animation started.
  pub animation_started: Instant,
  /// When the current brightness crossfade started.
  pub selection_started: Instant,
  pointer: Option<ExpoPointer>,
}

struct ExpoPointer {
  window: Option<WindowId>,
  start_x: f64,
  start_y: f64,
  x: f64,
  y: f64,
  dragging: bool,
}

impl ExpoState {
  /// Creates the overview with `selected` active and no pointer press.
  pub fn new(
    selected: usize,
    output: Rect,
    grid_width: u32,
    grid_height: u32,
    offset: u32,
  ) -> Self {
    let cells = expo_cells(output, grid_width, grid_height, offset);
    let selected = selected.min(cells.len().saturating_sub(1));
    let cell_origin =
      expo_wall_cells(output, grid_width, grid_height, selected);
    let now = Instant::now();
    Self {
      selected,
      previous_selected: selected,
      cells,
      cell_origin,
      windows: Vec::new(),
      geometry: HashMap::new(),
      geometry_origin: HashMap::new(),
      overlaps: HashMap::new(),
      output,
      animation_started: now,
      selection_started: now,
      pointer: None,
    }
  }

  /// Selects any visible workspace cell, including one with no windows.
  /// During zooming, hit testing uses the currently displayed cell geometry.
  pub fn select_at(
    &mut self,
    x: f64,
    y: f64,
    now: Instant,
    timing: Timing,
  ) -> bool {
    let cells = self.cells_at(now, timing);
    let Some(selected) = cells.iter().position(|cell| {
      x >= f64::from(cell.x)
        && y >= f64::from(cell.y)
        && x < f64::from(cell.x) + f64::from(cell.width)
        && y < f64::from(cell.y) + f64::from(cell.height)
    }) else {
      return false;
    };
    self.set_selected(selected, now);
    true
  }

  /// Moves within the configured grid without wrapping at its edges.
  pub fn move_selection(
    &mut self,
    direction: Direction,
    grid_width: u32,
    grid_height: u32,
    now: Instant,
  ) {
    let columns = grid_width as usize;
    let count = columns * grid_height as usize;
    if count == 0 {
      return;
    }
    let row = self.selected / columns;
    let column = self.selected % columns;
    let selected = match direction {
      Direction::Left if column > 0 => self.selected.checked_sub(1),
      Direction::Right if column + 1 < columns => self.selected.checked_add(1),
      Direction::Up if row > 0 => self.selected.checked_sub(columns),
      Direction::Down if row + 1 < grid_height as usize => {
        self.selected.checked_add(columns)
      }
      _ => None,
    };
    if let Some(selected) = selected.filter(|selected| *selected < count) {
      self.set_selected(selected, now);
    }
  }

  /// Starts a brightness crossfade between the old and new active cells.
  pub fn set_selected(&mut self, selected: usize, now: Instant) {
    if selected == self.selected || selected >= self.cells.len() {
      return;
    }
    self.previous_selected = self.selected;
    self.selected = selected;
    self.selection_started = now;
  }

  /// Returns the RGB multiplier for one workspace at the current fade time.
  pub fn brightness(
    &self,
    workspace: usize,
    now: Instant,
    transition: Timing,
    inactive: f32,
  ) -> f32 {
    let progress = transition.progress(self.selection_started, now);
    let from = if workspace == self.previous_selected {
      1.0
    } else {
      inactive
    };
    let to = if workspace == self.selected {
      1.0
    } else {
      inactive
    };
    from + (to - from) * progress
  }

  /// Captures workspace-cell rectangles at a point in the zoom animation.
  pub fn cells_at(&self, now: Instant, timing: Timing) -> Vec<RenderRect> {
    let progress = timing.progress(self.animation_started, now);
    self
      .cells
      .iter()
      .enumerate()
      .map(|(index, target)| {
        interpolate_rect(
          self.cell_origin.get(index).copied().unwrap_or(*target),
          *target,
          progress,
        )
      })
      .collect()
  }

  /// Captures thumbnail rectangles at a point in the zoom animation.
  pub fn geometry_at(
    &self,
    now: Instant,
    timing: Timing,
  ) -> HashMap<WindowId, RenderRect> {
    let progress = timing.progress(self.animation_started, now);
    self
      .geometry
      .iter()
      .map(|(window, target)| {
        let origin =
          self.geometry_origin.get(window).copied().unwrap_or(*target);
        (*window, interpolate_rect(origin, *target, progress))
      })
      .collect()
  }

  /// Starts a possible drag when a press lands on a visible thumbnail.
  pub fn pointer_down(&mut self, x: f64, y: f64, now: Instant, timing: Timing) {
    if !self.select_at(x, y, now, timing) {
      self.pointer = None;
      return;
    }
    let window = self.thumbnail_at(x, y, now, timing);
    self.pointer = Some(ExpoPointer {
      window,
      start_x: x,
      start_y: y,
      x,
      y,
      dragging: false,
    });
  }

  /// The window whose thumbnail is drawn at (`x`, `y`): in each cell of a
  /// workspace it overlaps, the part of it that cell shows.
  fn thumbnail_at(
    &self,
    x: f64,
    y: f64,
    now: Instant,
    timing: Timing,
  ) -> Option<WindowId> {
    let geometry = self.geometry_at(now, timing);
    let cells = self.cells_at(now, timing);
    self.windows.iter().copied().find(|window| {
      let Some(rect) = geometry.get(window) else {
        return false;
      };
      let Some(overlaps) = self.overlaps.get(window) else {
        return rect_contains(*rect, x, y);
      };
      crate::straddle::expo_copies(&cells, self.output, overlaps)
        .iter()
        .any(|copy| {
          let moved = RenderRect {
            x: rect.x.saturating_add(copy.dx),
            y: rect.y.saturating_add(copy.dy),
            ..*rect
          };
          rect_contains(moved, x, y) && rect_contains(copy.clip, x, y)
        })
    })
  }

  /// Moves the selection and promotes a press after five logical pixels.
  pub fn pointer_move(&mut self, x: f64, y: f64, now: Instant, timing: Timing) {
    self.select_at(x, y, now, timing);
    if let Some(pointer) = &mut self.pointer {
      pointer.x = x;
      pointer.y = y;
      if !pointer.dragging && pointer.window.is_some() {
        let dx = x - pointer.start_x;
        let dy = y - pointer.start_y;
        pointer.dragging = dx * dx + dy * dy >= 25.0;
      }
    }
  }

  /// Returns a window only when the released press became a drag.
  pub fn pointer_up(&mut self) -> Option<WindowId> {
    self.pointer.take().and_then(|pointer| {
      if pointer.dragging {
        pointer.window
      } else {
        None
      }
    })
  }

  /// Forgets a press or drag without selecting or dropping anything.
  pub fn cancel_pointer(&mut self) {
    self.pointer = None;
  }

  /// Returns whether a pointer press is in progress.
  pub fn pointer_pressed(&self) -> bool {
    self.pointer.is_some()
  }

  /// Returns the dragged window and the pointer position used for rendering.
  pub fn dragged_window(&self) -> Option<(WindowId, f64, f64)> {
    self.pointer.as_ref().and_then(|pointer| {
      if pointer.dragging {
        pointer.window.map(|window| (window, pointer.x, pointer.y))
      } else {
        None
      }
    })
  }
}

fn rect_contains(rect: RenderRect, x: f64, y: f64) -> bool {
  x >= f64::from(rect.x)
    && y >= f64::from(rect.y)
    && x < f64::from(rect.x) + f64::from(rect.width)
    && y < f64::from(rect.y) + f64::from(rect.height)
}

/// Returns full-sized workspaces arranged around the zoom destination.
/// Offscreen placement makes inactive windows leave the output during exit.
pub fn expo_wall_cells(
  output: Rect,
  grid_width: u32,
  grid_height: u32,
  selected: usize,
) -> Vec<RenderRect> {
  let selected_x = selected % grid_width.max(1) as usize;
  let selected_y = selected / grid_width.max(1) as usize;
  (0..grid_width as usize * grid_height as usize)
    .map(|index| {
      let column = index % grid_width as usize;
      let row = index / grid_width as usize;
      let dx = (column as i64 - selected_x as i64) * i64::from(output.width);
      let dy = (row as i64 - selected_y as i64) * i64::from(output.height);
      RenderRect {
        x: i32::try_from(i64::from(output.x) + dx).unwrap_or(if dx < 0 {
          i32::MIN
        } else {
          i32::MAX
        }),
        y: i32::try_from(i64::from(output.y) + dy).unwrap_or(if dy < 0 {
          i32::MIN
        } else {
          i32::MAX
        }),
        width: output.width,
        height: output.height,
      }
    })
    .collect()
}

/// Where Expo draws `rect`, a window's rectangle on the full-size wall
/// whose cell for its workspace is `wall`, in that workspace's grid
/// `cell`: the workspace is scaled into its cell as a whole, as Wayfire's
/// expo scales its workspace wall, so the window keeps its place and size
/// relative to its workspace.
pub fn workspace_scaled(
  rect: RenderRect,
  wall: RenderRect,
  cell: RenderRect,
) -> RenderRect {
  if wall.width == 0 || wall.height == 0 {
    return rect;
  }
  let scale_x = f64::from(cell.width) / f64::from(wall.width);
  let scale_y = f64::from(cell.height) / f64::from(wall.height);
  let left = f64::from(cell.x) + f64::from(rect.x - wall.x) * scale_x;
  let top = f64::from(cell.y) + f64::from(rect.y - wall.y) * scale_y;
  let right = left + f64::from(rect.width) * scale_x;
  let bottom = top + f64::from(rect.height) * scale_y;
  RenderRect {
    x: left.round() as i32,
    y: top.round() as i32,
    width: (right.round() - left.round()).max(0.0) as u32,
    height: (bottom.round() - top.round()).max(0.0) as u32,
  }
}

/// Interpolates renderer-only rectangles without changing client geometry.
pub fn interpolate_rect(
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

/// Divides the complete output into the configured row-major workspace grid.
pub fn expo_cells(
  output: Rect,
  grid_width: u32,
  grid_height: u32,
  offset: u32,
) -> Vec<RenderRect> {
  if grid_width == 0 || grid_height == 0 {
    return Vec::new();
  }
  let horizontal = offset.saturating_mul(grid_width.saturating_add(1));
  let vertical = offset.saturating_mul(grid_height.saturating_add(1));
  let width = output.width.saturating_sub(horizontal) / grid_width;
  let height = output.height.saturating_sub(vertical) / grid_height;
  (0..grid_width as usize * grid_height as usize)
    .map(|index| {
      let column = index % grid_width as usize;
      let row = index / grid_width as usize;
      let dx = offset.saturating_add(
        (column as u32).saturating_mul(width.saturating_add(offset)),
      );
      let dy = offset.saturating_add(
        (row as u32).saturating_mul(height.saturating_add(offset)),
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

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  #[test]
  fn cells_cover_the_exact_grid_with_output_origins_and_gaps() {
    let cells = expo_cells(Rect::new(100, 50, 830, 470), 2, 2, 10);
    assert_eq!(
      cells,
      vec![
        RenderRect {
          x: 110,
          y: 60,
          width: 400,
          height: 220,
        },
        RenderRect {
          x: 520,
          y: 60,
          width: 400,
          height: 220,
        },
        RenderRect {
          x: 110,
          y: 290,
          width: 400,
          height: 220,
        },
        RenderRect {
          x: 520,
          y: 290,
          width: 400,
          height: 220,
        },
      ]
    );
  }

  #[test]
  fn zero_offset_and_degenerate_grids_are_deterministic() {
    assert_eq!(
      expo_cells(Rect::new(0, 0, 800, 600), 1, 1, 0),
      vec![RenderRect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
      }]
    );
    assert!(expo_cells(Rect::new(0, 0, 800, 600), 0, 2, 10).is_empty());
  }

  #[test]
  fn navigation_and_hits_include_empty_cells_without_wrapping() {
    let now = Instant::now();
    let mut state = ExpoState::new(0, Rect::new(0, 0, 630, 350), 3, 2, 10);
    state.move_selection(Direction::Right, 3, 2, now);
    state.move_selection(Direction::Down, 3, 2, now);
    assert_eq!(state.selected, 4);
    state.move_selection(Direction::Right, 3, 2, now);
    assert_eq!(state.selected, 5);
    state.move_selection(Direction::Right, 3, 2, now);
    assert_eq!(state.selected, 5);
    assert!(state.select_at(15.0, 15.0, now, Timing::linear(Duration::ZERO)));
    assert_eq!(state.selected, 0);
    assert!(!state.select_at(5.0, 5.0, now, Timing::linear(Duration::ZERO)));
    assert_eq!(state.selected, 0);
  }

  #[test]
  fn zoom_interpolates_wall_cells_and_window_rectangles() {
    let started = Instant::now();
    let mut state = ExpoState::new(0, Rect::new(0, 0, 200, 100), 2, 1, 10);
    state.animation_started = started;
    assert_eq!(
      state.cells_at(started, Timing::linear(Duration::from_millis(300))),
      expo_wall_cells(Rect::new(0, 0, 200, 100), 2, 1, 0)
    );
    let cells = state.cells_at(
      started + Duration::from_millis(150),
      Timing::linear(Duration::from_millis(300)),
    );
    assert_eq!(cells[0].width, 143);
    assert_eq!(cells[1].x, 153);

    let window = WindowId(1);
    state.geometry.insert(
      window,
      RenderRect {
        x: 10,
        y: 10,
        width: 90,
        height: 80,
      },
    );
    state.geometry_origin.insert(
      window,
      RenderRect {
        x: 0,
        y: 0,
        width: 200,
        height: 100,
      },
    );
    let geometry = state.geometry_at(
      started + Duration::from_millis(150),
      Timing::linear(Duration::from_millis(300)),
    );
    assert_eq!(geometry[&window].width, 145);
    assert_eq!(geometry[&window].height, 90);
  }

  #[test]
  fn selection_fades_brightness_without_changing_immediately() {
    let started = Instant::now();
    let mut state = ExpoState::new(0, Rect::new(0, 0, 200, 100), 2, 1, 10);
    state.set_selected(1, started);
    let transition = Timing::linear(Duration::from_millis(200));
    assert_eq!(state.brightness(0, started, transition, 0.7), 1.0);
    assert_eq!(state.brightness(1, started, transition, 0.7), 0.7);
    let middle = started + Duration::from_millis(100);
    assert!(
      (state.brightness(0, middle, transition, 0.7) - 0.85).abs() < 0.001
    );
    assert!(
      (state.brightness(1, middle, transition, 0.7) - 0.85).abs() < 0.001
    );
    let end = started + transition.duration;
    assert_eq!(state.brightness(0, end, transition, 0.7), 0.7);
    assert_eq!(state.brightness(1, end, transition, 0.7), 1.0);
  }

  #[test]
  fn a_workspace_is_scaled_into_its_cell_as_a_whole() {
    let rect = |x, y, width, height| RenderRect {
      x,
      y,
      width,
      height,
    };
    let wall = rect(1000, 0, 1000, 500);
    let cell = rect(510, 10, 500, 250);
    // A quarter-size window at the workspace's centre stays at its
    // centre, at half its size.
    assert_eq!(
      workspace_scaled(rect(1375, 125, 250, 250), wall, cell),
      rect(698, 73, 125, 125)
    );
    // Over the workspace's right edge, it goes past the cell's.
    assert_eq!(
      workspace_scaled(rect(1900, 100, 200, 100), wall, cell),
      rect(960, 60, 100, 50)
    );
  }

  #[test]
  fn a_thumbnail_over_two_cells_is_taken_in_both() {
    let now = Instant::now();
    let timing = Timing::linear(Duration::ZERO);
    // Two half-size cells, 10 apart, of a 230x110 output.
    let mut state = ExpoState::new(0, Rect::new(0, 0, 230, 110), 2, 1, 10);
    let window = WindowId(1);
    state.windows.push(window);
    // From x 90 to 120 of the first cell, which ends at 110.
    state.geometry.insert(
      window,
      RenderRect {
        x: 90,
        y: 30,
        width: 30,
        height: 20,
      },
    );
    state
      .overlaps
      .insert(window, vec![(0, (0, 0)), (1, (-230, 0))]);
    let at = |state: &ExpoState, x| state.thumbnail_at(x, 40.0, now, timing);
    assert_eq!(at(&state, 100.0), Some(window));
    // The gap between the cells shows neither part.
    assert_eq!(at(&state, 115.0), None);
    // The part in the second cell is moved by the gap.
    assert_eq!(at(&state, 125.0), Some(window));
    assert_eq!(at(&state, 135.0), None);
    // Without overlaps, the thumbnail is taken where it is.
    state.overlaps.clear();
    assert_eq!(at(&state, 115.0), Some(window));
  }

  #[test]
  fn pointer_distinguishes_clicks_drags_and_cancellation() {
    let now = Instant::now();
    let mut state = ExpoState::new(0, Rect::new(0, 0, 230, 110), 2, 1, 10);
    let window = WindowId(1);
    state.windows.push(window);
    state.geometry.insert(
      window,
      RenderRect {
        x: 20,
        y: 20,
        width: 40,
        height: 30,
      },
    );
    state.pointer_down(30.0, 30.0, now, Timing::linear(Duration::ZERO));
    state.pointer_move(32.0, 31.0, now, Timing::linear(Duration::ZERO));
    assert_eq!(state.pointer_up(), None);

    state.pointer_down(30.0, 30.0, now, Timing::linear(Duration::ZERO));
    state.pointer_move(150.0, 50.0, now, Timing::linear(Duration::ZERO));
    assert_eq!(state.selected, 1);
    assert_eq!(state.dragged_window(), Some((window, 150.0, 50.0)));
    assert_eq!(state.pointer_up(), Some(window));
    assert!(!state.pointer_pressed());

    state.pointer_down(30.0, 30.0, now, Timing::linear(Duration::ZERO));
    state.cancel_pointer();
    assert_eq!(state.pointer_up(), None);
  }
}
