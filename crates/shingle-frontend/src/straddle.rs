//! Windows straddling workspaces on the walls the scene draws.
//!
//! Wayfire renders each workspace of a wall, vswitch's and vswipe's slide
//! and Expo's grid, as the part of its workspace set's plane over that
//! workspace, clipped to the workspace (wayfire:plugins/common/
//! workspace-wall.cpp), so a floating view hanging over the edge of its
//! workspace shows on the neighbour too, with the wall's gap between the
//! two parts. Shingle keeps a window on its own workspace; on a wall the
//! scene draws one copy of its tiles for each workspace it overlaps, moved
//! to that workspace and cut to its cell.

use shingle_core::{Rect, WorkspaceId};
use shingle_render::{RenderRect, RenderTile};

/// One copy of a window's tiles on a wall: moved by `dx`, `dy` from where
/// the window's own workspace puts them and cut to `clip`, the cell of the
/// workspace it shows the window on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WallCopy {
  /// Horizontal move, in logical pixels.
  pub dx: i32,
  /// Vertical move, in logical pixels.
  pub dy: i32,
  /// The workspace's cell on the wall.
  pub clip: RenderRect,
}

/// The workspaces a window overlaps, as row-major indices with the shift
/// that moves it onto each, its own first
/// ([`overlapped_workspaces`]).
pub type Overlaps = Vec<(usize, (i32, i32))>;

/// `rect` moved back by `shift`, the [`shingle::SurfaceSnapshot::shift`]
/// it was drawn with: where it is on its own workspace.
pub fn unshifted(rect: Rect, shift: (i32, i32)) -> Rect {
  Rect::new(
    rect.x.saturating_sub(shift.0),
    rect.y.saturating_sub(shift.1),
    rect.width,
    rect.height,
  )
}

/// The workspaces of a `grid` (columns, rows) that a window of workspace
/// `own` with geometry `rect`, laid over `output`, overlaps, as row-major
/// indices with the shift that moves the window there: the output's size
/// times the difference in columns and rows. Its own workspace comes first
/// with no shift, then the others in index order; only those its rectangle
/// spans are looked at.
pub fn overlapped_workspaces(
  rect: Rect,
  own: WorkspaceId,
  output: Rect,
  grid: (u32, u32),
) -> Overlaps {
  let index = |x: i64, y: i64| (y * i64::from(grid.0) + x) as usize;
  let home = (i64::from(own.x), i64::from(own.y));
  let mut found = vec![(index(home.0, home.1), (0, 0))];
  if output.width == 0
    || output.height == 0
    || rect.width == 0
    || rect.height == 0
  {
    return found;
  }
  // The columns and rows, relative to `own`, its first and last pixel
  // fall on.
  let span = |start: i32, extent: u32, origin: i32, size: u32| {
    let start = i64::from(start) - i64::from(origin);
    let end = start + i64::from(extent) - 1;
    let size = i64::from(size);
    (start.div_euclid(size), end.div_euclid(size))
  };
  let columns = span(rect.x, rect.width, output.x, output.width);
  let rows = span(rect.y, rect.height, output.y, output.height);
  let clamp = |from: i64, to: i64, own: u32, count: u32| {
    let own = i64::from(own);
    let last = i64::from(count.max(1)) - 1;
    ((own + from).max(0), (own + to).min(last))
  };
  let (first_x, last_x) = clamp(columns.0, columns.1, own.x, grid.0);
  let (first_y, last_y) = clamp(rows.0, rows.1, own.y, grid.1);
  let distance = |own: u32, other: i64, size: u32| {
    let value = (i64::from(own) - other) * i64::from(size);
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
  };
  for y in first_y..=last_y {
    for x in first_x..=last_x {
      if (x, y) == home {
        continue;
      }
      found.push((
        index(x, y),
        (
          distance(own.x, x, output.width),
          distance(own.y, y, output.height),
        ),
      ));
    }
  }
  found
}

/// The copies of a window on a slide's wall, which `place` lays out: it
/// puts a rectangle of the layout, as on workspace `index`, where the
/// wall shows it now. `content` is the window's surface rectangle on its
/// own workspace, the first of `overlapped`
/// ([`overlapped_workspaces`]); each copy is that surface placed as on the
/// workspace it overlaps, cut to where the wall shows `output` for that
/// workspace.
pub fn wall_copies(
  content: Rect,
  output: Rect,
  overlapped: &[(usize, (i32, i32))],
  place: impl Fn(Rect, usize) -> RenderRect,
) -> Vec<WallCopy> {
  let Some(&(own, _)) = overlapped.first() else {
    return Vec::new();
  };
  let base = place(content, own);
  overlapped
    .iter()
    .map(|&(index, shift)| {
      let moved = place(
        Rect::new(
          content.x.saturating_add(shift.0),
          content.y.saturating_add(shift.1),
          content.width,
          content.height,
        ),
        index,
      );
      WallCopy {
        dx: moved.x.saturating_sub(base.x),
        dy: moved.y.saturating_sub(base.y),
        clip: place(output, index),
      }
    })
    .collect()
}

/// The copies of a window on Expo's grid of `cells` (row-major, all of one
/// size), whose workspaces are scaled into their cells as a whole: the
/// window's tiles are placed by its own workspace's cell, the first of
/// `overlapped` ([`overlapped_workspaces`]), and each copy moves them by
/// the other cell's distance less the scaled shift, a cell's size per
/// column and row.
pub fn expo_copies(
  cells: &[RenderRect],
  output: Rect,
  overlapped: &[(usize, (i32, i32))],
) -> Vec<WallCopy> {
  let Some(own) = overlapped.first().and_then(|(own, _)| cells.get(*own))
  else {
    return Vec::new();
  };
  let scale = |shift: i32, cell: u32, size: u32| {
    if size == 0 {
      return 0;
    }
    (f64::from(shift) * f64::from(cell) / f64::from(size)).round() as i32
  };
  overlapped
    .iter()
    .filter_map(|&(index, shift)| {
      let cell = cells.get(index)?;
      Some(WallCopy {
        dx: cell.x.saturating_sub(own.x).saturating_add(scale(
          shift.0,
          own.width,
          output.width,
        )),
        dy: cell.y.saturating_sub(own.y).saturating_add(scale(
          shift.1,
          own.height,
          output.height,
        )),
        clip: *cell,
      })
    })
    .collect()
}

/// `tiles` once for each of `copies`, moved and cut to its cell; parts
/// left with no area are dropped. Tiles that are rotated, projected or
/// drawn by an effect cannot be cut, so with any of those `tiles` come
/// back as they are.
pub fn apply_copies<'a>(
  tiles: Vec<RenderTile<'a>>,
  copies: &[WallCopy],
) -> Vec<RenderTile<'a>> {
  let plain = tiles.iter().all(|tile| {
    tile.rotation == 0.0
      && tile.projection.is_none()
      && matches!(tile.effect, shingle_render::TileEffect::None)
  });
  if copies.is_empty() || !plain {
    return tiles;
  }
  let mut copied = Vec::with_capacity(tiles.len() * copies.len());
  for copy in copies {
    for tile in &tiles {
      let mut tile = tile.clone();
      tile.geometry.x = tile.geometry.x.saturating_add(copy.dx);
      tile.geometry.y = tile.geometry.y.saturating_add(copy.dy);
      tile.crop(copy.clip);
      if tile.geometry.width > 0 && tile.geometry.height > 0 {
        copied.push(tile);
      }
    }
  }
  copied
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rect(x: i32, y: i32, width: u32, height: u32) -> RenderRect {
    RenderRect {
      x,
      y,
      width,
      height,
    }
  }

  #[test]
  fn a_window_inside_its_workspace_overlaps_only_it() {
    let output = Rect::new(0, 0, 1280, 720);
    let found = overlapped_workspaces(
      Rect::new(100, 100, 300, 200),
      WorkspaceId::new(1, 0),
      output,
      (3, 2),
    );
    assert_eq!(found, vec![(1, (0, 0))]);
  }

  #[test]
  fn a_window_over_edges_overlaps_each_neighbour_with_its_shift() {
    let output = Rect::new(100, 50, 1280, 720);
    // Over the right edge of workspace 0 and the bottom edge too.
    let found = overlapped_workspaces(
      Rect::new(1200, 700, 300, 200),
      WorkspaceId::new(0, 0),
      output,
      (3, 2),
    );
    assert_eq!(
      found,
      vec![
        (0, (0, 0)),
        (1, (-1280, 0)),
        (3, (0, -720)),
        (4, (-1280, -720)),
      ]
    );
    // Over the top-left corner of workspace 4.
    let found = overlapped_workspaces(
      Rect::new(0, 0, 300, 200),
      WorkspaceId::new(1, 1),
      output,
      (3, 2),
    );
    assert_eq!(
      found,
      vec![(4, (0, 0)), (0, (1280, 720)), (1, (0, 720)), (3, (1280, 0))]
    );
    // Ending exactly at the edge overlaps nothing more.
    let found = overlapped_workspaces(
      Rect::new(1080, 100, 300, 200),
      WorkspaceId::new(0, 0),
      output,
      (3, 2),
    );
    assert_eq!(found, vec![(0, (0, 0))]);
  }

  #[test]
  fn the_grid_edge_ends_the_neighbours() {
    let output = Rect::new(0, 0, 1280, 720);
    let found = overlapped_workspaces(
      Rect::new(-100, 600, 3000, 300),
      WorkspaceId::new(0, 1),
      output,
      (2, 2),
    );
    assert_eq!(found, vec![(2, (0, 0)), (3, (-1280, 0))]);
    // A degenerate rectangle or output finds its own workspace only.
    let found = overlapped_workspaces(
      Rect::new(1200, 0, 0, 300),
      WorkspaceId::new(0, 0),
      output,
      (2, 2),
    );
    assert_eq!(found, vec![(0, (0, 0))]);
  }

  #[test]
  fn wall_copies_are_apart_by_the_gap_and_cut_to_their_cells() {
    let output = Rect::new(0, 0, 1000, 500);
    let gap = 20;
    // A wall of two columns, at its own workspace's place.
    let place = |rect: Rect, index: usize| RenderRect {
      x: rect.x + index as i32 * (1000 + gap),
      y: rect.y,
      width: rect.width,
      height: rect.height,
    };
    let content = Rect::new(900, 100, 200, 100);
    let overlapped =
      overlapped_workspaces(content, WorkspaceId::new(0, 0), output, (2, 1));
    let copies = wall_copies(content, output, &overlapped, place);
    assert_eq!(
      copies,
      vec![
        WallCopy {
          dx: 0,
          dy: 0,
          clip: rect(0, 0, 1000, 500),
        },
        WallCopy {
          dx: gap,
          dy: 0,
          clip: rect(1020, 0, 1000, 500),
        },
      ]
    );
    let tile = RenderTile::solid(rect(900, 100, 200, 100), [1.0; 4]);
    let tiles = apply_copies(vec![tile], &copies);
    let drawn = tiles.iter().map(|tile| tile.geometry).collect::<Vec<_>>();
    // The left part to the first cell's edge, the right part from the
    // second's, the gap between them.
    assert_eq!(
      drawn,
      vec![rect(900, 100, 100, 100), rect(1020, 100, 100, 100)]
    );
    assert_eq!(tiles[1].uv, [0.5, 0.0, 1.0, 1.0]);
  }

  #[test]
  fn expo_copies_follow_the_cells_scale() {
    let output = Rect::new(0, 0, 1000, 500);
    // Half-size cells 10 pixels apart.
    let cells = [rect(10, 10, 500, 250), rect(520, 10, 500, 250)];
    let overlapped = [(0, (0, 0)), (1, (-1000, 0))];
    let copies = expo_copies(&cells, output, &overlapped);
    assert_eq!(copies[0].dx, 0);
    // The second cell is 510 to the right, less half the shift, 500.
    assert_eq!((copies[1].dx, copies[1].dy), (10, 0));
    assert_eq!(copies[1].clip, cells[1]);
    // A cell the grid lacks gives no copy.
    let missing = [(0, (0, 0)), (5, (0, 0))];
    assert_eq!(expo_copies(&cells, output, &missing).len(), 1);
  }

  #[test]
  fn rotated_tiles_are_not_cut() {
    let mut tile = RenderTile::solid(rect(0, 0, 10, 10), [1.0; 4]);
    tile.rotation = 0.5;
    let copies = [WallCopy {
      dx: 5,
      dy: 0,
      clip: rect(0, 0, 3, 3),
    }];
    let tiles = apply_copies(vec![tile], &copies);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].geometry, rect(0, 0, 10, 10));
  }
}
