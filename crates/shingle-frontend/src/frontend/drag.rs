//! Pointer drags: moving and resizing windows, grid snapping and
//! workspace switching while dragging, and window rotation.

use std::time::Instant;

use shingle::Direction;
use shingle_core::WindowId as TileWindowId;
use shingle_render::{PreviewDirection, RenderRect, split_preview};

use super::Frontend;

pub(super) enum DragState {
  Move {
    source: TileWindowId,
    last_x: f64,
    last_y: f64,
    snap_off_origin: Option<(f64, f64)>,
  },
  Resize {
    window: TileWindowId,
    horizontal: Option<Direction>,
    vertical: Option<Direction>,
    last_x: f64,
    last_y: f64,
    aspect: Option<(f64, f64, shingle_core::Rect)>,
  },
}

#[derive(Clone, Copy)]
pub(super) struct RotationDrag {
  pub(super) window: TileWindowId,
  pub(super) last_x: f64,
  pub(super) last_y: f64,
}

impl Frontend {
  /// Ends a move or resize drag where the window currently is, without the
  /// drop, swap or snap a button release would perform.
  pub(super) fn abandon_drag(&mut self) {
    if self.drag.take().is_none() {
      return;
    }
    self.drop_slot = None;
    self.drop_preview = None;
    self.workspace_switch_started = None;
    self.save();
  }

  pub(super) fn window_geometry(
    &self,
    window: TileWindowId,
  ) -> Option<shingle_core::Rect> {
    self
      .compositor
      .core()
      .window_layout(window)
      .map(|layout| layout.geometry)
      .ok()
      .or_else(|| {
        self
          .compositor
          .core()
          .window_info(window)
          .ok()
          .and_then(|info| info.state.floating)
      })
  }

  /// Ends a move drag of `source` that dropped on no other window: a
  /// window dropped on another output than its own joins that output's
  /// set, as Wayfire's move plugin moves a view to the output a drag ends
  /// on. A floating window stays where the pointer put it; a tiled one is
  /// appended to the target tree.
  pub(super) fn drop_on_output(
    &mut self,
    source: TileWindowId,
    floating: bool,
  ) {
    let Some(output) =
      self.compositor.output_at(self.pointer_x, self.pointer_y)
    else {
      return;
    };
    let own = self
      .compositor
      .core()
      .window_info(source)
      .ok()
      .and_then(|info| self.compositor.core().wset(info.wset).ok())
      .and_then(|wset| wset.output);
    if own.as_ref().map(shingle_core::OutputId::as_str) == Some(output.name()) {
      return;
    }
    let geometry = if floating {
      shingle::MoveGeometry::Keep
    } else {
      shingle::MoveGeometry::Scale
    };
    if let Err(error) = self
      .compositor
      .move_window_to_output(source, &output, geometry)
    {
      eprintln!("shingle: dropping on {} failed: {error}", output.name());
    }
  }

  /// Makes the output under the pointer active while a move drag crosses
  /// into it, as Wayfire's move plugin focuses the output a drag enters.
  fn follow_drag_output(&mut self) {
    if let Some(output) =
      self.compositor.output_at(self.pointer_x, self.pointer_y)
      && output != self.compositor.active_output()
    {
      self.compositor.focus_output(&output);
    }
  }

  pub(super) fn drop_direction(&self, target: TileWindowId) -> Direction {
    let Some(geometry) = self.window_geometry(target) else {
      return Direction::Right;
    };
    let center_x = geometry.x as f64 + geometry.width as f64 / 2.0;
    let center_y = geometry.y as f64 + geometry.height as f64 / 2.0;
    let dx = (self.pointer_x - center_x) / geometry.width.max(1) as f64;
    let dy = (self.pointer_y - center_y) / geometry.height.max(1) as f64;
    if dx.abs() > dy.abs() {
      if dx < 0.0 {
        Direction::Left
      } else {
        Direction::Right
      }
    } else if dy < 0.0 {
      Direction::Up
    } else {
      Direction::Down
    }
  }

  pub(super) fn rotate_pointer(&mut self, x: f64, y: f64) {
    let Some(mut drag) = self.rotation_drag else {
      return;
    };
    let Some(geometry) = self.window_geometry(drag.window) else {
      self.rotation_drag = None;
      return;
    };
    let center_x = f64::from(geometry.x) + f64::from(geometry.width) / 2.0;
    let center_y = f64::from(geometry.y) + f64::from(geometry.height) / 2.0;
    let old = (drag.last_x - center_x, drag.last_y - center_y);
    let new = (x - center_x, y - center_y);
    let old_length = old.0.hypot(old.1);
    let new_length = new.0.hypot(new.1);
    if new_length <= self.wrot_reset_radius {
      self.rotations.remove(&drag.window);
    } else if old_length > 0.0 && new_length > 0.0 {
      let cross = old.0 * new.1 - new.0 * old.1;
      let delta = (cross / (old_length * new_length)).clamp(-1.0, 1.0).asin();
      let angle =
        self.rotations.get(&drag.window).copied().unwrap_or(0.0) - delta as f32;
      self.rotations.insert(drag.window, angle);
    }
    drag.last_x = x;
    drag.last_y = y;
    self.rotation_drag = Some(drag);
    self.request_redraw();
  }

  pub(super) fn update_workspace_switch_timer(&mut self) {
    let direction = if self.workspace_switch_after.is_some()
      && matches!(self.drag, Some(DragState::Move { .. }))
    {
      self.compositor.workspace_drag_direction_at(
        self.pointer_x,
        self.pointer_y,
        self.snap_threshold,
        self.quarter_snap_threshold,
      )
    } else {
      None
    };
    match direction {
      Some((dx, dy)) => {
        if self
          .workspace_switch_started
          .is_none_or(|(old_dx, old_dy, _)| (old_dx, old_dy) != (dx, dy))
        {
          self.workspace_switch_started = Some((dx, dy, Instant::now()));
        }
      }
      None => self.workspace_switch_started = None,
    }
  }

  pub(super) fn process_workspace_switch_timeout(&mut self) {
    let Some((dx, dy, started)) = self.workspace_switch_started else {
      return;
    };
    let Some(delay) = self.workspace_switch_after else {
      return;
    };
    if started.elapsed() < delay {
      return;
    }
    self.workspace_switch_started = None;
    let Some(source) = (match self.drag {
      Some(DragState::Move { source, .. }) => Some(source),
      _ => None,
    }) else {
      return;
    };
    let result = (|| {
      let current = self.compositor.current_workspace_index()?;
      let (width, height) = self.compositor.workspace_grid_dimensions();
      let x = current as i64 % i64::from(width) + i64::from(dx);
      let y = current as i64 / i64::from(width) + i64::from(dy);
      if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
        return Ok(());
      }
      let target = (y * i64::from(width) + x) as usize;
      self.compositor.move_window_to_workspace(source, target)?;
      self.compositor.switch_workspace(target)
    })();
    if let Err(error) = result {
      eprintln!("shingle: drag workspace switch failed: {error}");
    }
    self.save();
    self.update_drop_preview(source);
  }

  pub(super) fn update_drop_preview(&mut self, source: TileWindowId) {
    self.update_workspace_switch_timer();
    self.drop_preview = None;
    self.drop_slot = None;
    if self.grid_snap
      && let Some(slot) = self.compositor.grid_slot_at(
        self.pointer_x,
        self.pointer_y,
        self.snap_threshold,
        self.quarter_snap_threshold,
      )
    {
      self.drop_slot = Some(slot);
      if let Some(geometry) = self.compositor.snap_geometry(slot) {
        self.drop_preview = Some(RenderRect {
          x: geometry.x,
          y: geometry.y,
          width: geometry.width,
          height: geometry.height,
        });
      }
      return;
    }
    if self
      .compositor
      .core()
      .window_info(source)
      .is_ok_and(|info| info.state.floating.is_some())
    {
      return;
    }
    let Some(target) = self.compositor.focus_at(self.pointer_x, self.pointer_y)
    else {
      return;
    };
    if target == source {
      return;
    }
    let direction = match self.drop_direction(target) {
      Direction::Left => PreviewDirection::Left,
      Direction::Right => PreviewDirection::Right,
      Direction::Up => PreviewDirection::Up,
      Direction::Down => PreviewDirection::Down,
    };
    if let Some(geometry) = self.window_geometry(target) {
      self.drop_preview = Some(split_preview(
        RenderRect {
          x: geometry.x,
          y: geometry.y,
          width: geometry.width,
          height: geometry.height,
        },
        direction,
      ));
    }
  }

  pub(super) fn move_drag(&mut self, x: f64, y: f64) {
    let (source, changed) = {
      let Some(DragState::Move {
        source,
        last_x,
        last_y,
        snap_off_origin,
      }) = &mut self.drag
      else {
        return;
      };
      let mut released = false;
      let (dx, dy) = if let Some((origin_x, origin_y)) = *snap_off_origin {
        if (x - origin_x).hypot(y - origin_y)
          >= f64::from(self.snap_off_threshold)
        {
          *snap_off_origin = None;
          released = true;
          ((x - origin_x).round() as i32, (y - origin_y).round() as i32)
        } else {
          (0, 0)
        }
      } else {
        ((x - *last_x).round() as i32, (y - *last_y).round() as i32)
      };
      if released && let Err(error) = self.compositor.restore_snapped(*source) {
        eprintln!("shingle: snap-off failed: {error}");
      }
      let moved = if snap_off_origin.is_some() {
        false
      } else {
        match self.compositor.move_floating_group(
          *source,
          dx,
          dy,
          self.join_views,
        ) {
          Ok(changed) => changed,
          Err(error) => {
            eprintln!("shingle: pointer move failed: {error}");
            false
          }
        }
      };
      *last_x = x;
      *last_y = y;
      (*source, released || moved)
    };
    self.follow_drag_output();
    self.update_drop_preview(source);
    if changed {
      self.save();
    }
  }

  pub(super) fn resize_drag(&mut self, x: f64, y: f64) {
    let Some(DragState::Resize {
      window,
      horizontal,
      vertical,
      last_x,
      last_y,
      aspect,
    }) = &mut self.drag
    else {
      return;
    };
    if let Some((start_x, start_y, original)) = *aspect {
      let dx = (x - start_x).round() as i32;
      let dy = (y - start_y).round() as i32;
      if let Err(error) = self.compositor.resize_floating_from(
        *window,
        *horizontal,
        *vertical,
        original,
        dx,
        dy,
      ) {
        eprintln!("shingle: aspect-preserving resize failed: {error}");
      }
    } else {
      let dx = (x - *last_x).round() as i32;
      let dy = (y - *last_y).round() as i32;
      for (direction, delta) in [(*horizontal, dx), (*vertical, dy)] {
        if let Some(direction) = direction
          && delta != 0
          && let Err(error) = self.compositor.resize_window(
            *window,
            direction,
            delta,
            self.minimum_extent,
          )
        {
          eprintln!("shingle: pointer resize failed: {error}");
        }
      }
    }
    *last_x = x;
    *last_y = y;
    self.save();
  }
}
