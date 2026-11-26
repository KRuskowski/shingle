//! Expo: the workspace grid overview. The layout lives in
//! [`crate::expo`]; this is its state machine and input.

use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use shingle::Direction;
use shingle_config::Modifiers as ConfigModifiers;
use shingle_core::WindowId as TileWindowId;
use shingle_render::RenderRect;

use super::{BTN_LEFT, Frontend};
use crate::{expo, scale::fit_scale_tile};

impl Frontend {
  /// Opens Expo or commits the currently selected workspace.
  pub(super) fn toggle_expo(&mut self) {
    if self.expo.is_some() {
      self.finish_expo();
      return;
    }
    self.expo_exit = None;
    let active = self.compositor.active_output();
    if let Some(scene) = self.scenes.get_mut(&active) {
      scene.workspace_transition = None;
    }
    self.cancel_scale();
    self.scale_exit = None;
    self.overview_output = Some(active);
    if self.switcher.is_some() || self.switcher_view.is_some() {
      self.cancel_switcher();
    }
    self.drag = None;
    self.drop_preview = None;
    self.drop_slot = None;
    let selected = self.compositor.current_workspace_index().unwrap_or(0);
    let (width, height) = self.compositor.workspace_grid_dimensions();
    self.expo = Some(expo::ExpoState::new(
      selected,
      self.output_rect(&self.overview_output()),
      width,
      height,
      self.expo_config.offset,
    ));
    self.update_expo_layout();
    self.compositor.notify_plugin_activation("expo", true);
    self.request_redraw();
  }

  /// Immediately removes both interactive and exiting Expo render states.
  pub(super) fn clear_expo(&mut self) {
    if self.expo.is_some() {
      self.compositor.notify_plugin_activation("expo", false);
    }
    self.expo = None;
    self.expo_exit = None;
    self.expo_touch = None;
  }

  /// Zooms back to the original workspace without changing the saved session.
  pub(super) fn cancel_expo(&mut self) {
    if let Some(mut state) = self.expo.take() {
      let current = self.compositor.current_workspace_index().unwrap_or(0);
      state.selected = current;
      state.previous_selected = current;
      self.compositor.notify_plugin_activation("expo", false);
      self.begin_expo_exit(state);
    }
    self.request_redraw();
  }

  /// Starts the renderer-only return from grid cells to a workspace wall.
  pub(super) fn begin_expo_exit(&mut self, mut state: expo::ExpoState) {
    state.cancel_pointer();
    self.expo_touch = None;
    let now = Instant::now();
    let duration = Duration::from_millis(self.expo_config.duration_ms);
    state.cell_origin = state.cells_at(now, duration);
    state.geometry_origin = state.geometry_at(now, duration);
    let (width, height) = self.compositor.workspace_grid_dimensions();
    state.cells = expo::expo_wall_cells(
      self.output_rect(&self.overview_output()),
      width,
      height,
      state.selected,
    );
    state.geometry = self.original_expo_geometry(state.selected);
    state.animation_started = now;
    state.previous_selected = state.selected;
    state.selection_started = now;
    if duration.is_zero() {
      self.expo_exit = None;
    } else {
      self.expo_exit = Some(state);
    }
  }

  /// Commits the selected workspace and, after a real drag, its window move.
  pub(super) fn finish_expo(&mut self) {
    let Some(mut state) = self.expo.take() else {
      return;
    };
    self.compositor.notify_plugin_activation("expo", false);
    let dragged = state.pointer_up();
    let mut changed = false;
    if let Some(window) = dragged {
      let source =
        self.compositor.core().window_info(window).ok().map(|info| {
          let (width, _) = self.compositor.workspace_grid_dimensions();
          info.location.workspace.y as usize * width as usize
            + info.location.workspace.x as usize
        });
      if source.is_some_and(|source| source != state.selected) {
        match self
          .compositor
          .move_window_to_workspace(window, state.selected)
        {
          Ok(()) => changed = true,
          Err(error) => {
            eprintln!("shingle: Expo window move failed: {error}")
          }
        }
      }
    }
    if !self
      .compositor
      .current_workspace_index()
      .is_ok_and(|workspace| workspace == state.selected)
    {
      match self.compositor.switch_workspace(state.selected) {
        Ok(()) => changed = true,
        Err(error) => {
          state.selected =
            self.compositor.current_workspace_index().unwrap_or(0);
          eprintln!("shingle: Expo workspace switch failed: {error}");
        }
      }
    }
    if changed {
      self.save();
    }
    self.begin_expo_exit(state);
    self.request_redraw();
  }

  /// Returns normal window rectangles on the wall around `selected`.
  /// Remote workspaces are offscreen at the end of an exit transition.
  pub(super) fn original_expo_geometry(
    &self,
    selected: usize,
  ) -> HashMap<TileWindowId, RenderRect> {
    let output = self.output_rect(&self.overview_output());
    let (grid_width, grid_height) = self.compositor.workspace_grid_dimensions();
    let cells =
      expo::expo_wall_cells(output, grid_width, grid_height, selected);
    self
      .compositor
      .scale_surfaces(true, false)
      .into_iter()
      .filter_map(|snapshot| {
        let window = snapshot.window?;
        let info = self.compositor.core().window_info(window).ok()?;
        let workspace = info.location.workspace.y as usize
          * grid_width as usize
          + info.location.workspace.x as usize;
        let cell = cells.get(workspace)?;
        let original =
          self.window_geometry(window).unwrap_or(snapshot.geometry);
        let x = i64::from(original.x) + i64::from(cell.x) - i64::from(output.x);
        let y = i64::from(original.y) + i64::from(cell.y) - i64::from(output.y);
        Some((
          window,
          RenderRect {
            x: i32::try_from(x).unwrap_or(if x < 0 {
              i32::MIN
            } else {
              i32::MAX
            }),
            y: i32::try_from(y).unwrap_or(if y < 0 {
              i32::MIN
            } else {
              i32::MAX
            }),
            width: original.width,
            height: original.height,
          },
        ))
      })
      .collect()
  }

  /// Rebuilds live targets while preserving the current transition position.
  /// Unchanged targets do not restart the animation on every frame.
  pub(super) fn update_expo_layout(&mut self) {
    if self.expo.is_none() {
      return;
    }
    let now = Instant::now();
    let duration = Duration::from_millis(self.expo_config.duration_ms);
    let previous = self.expo.as_ref().map(|state| {
      (
        state.cells_at(now, duration),
        state.geometry_at(now, duration),
      )
    });
    let (grid_width, grid_height) = self.compositor.workspace_grid_dimensions();
    let cells = expo::expo_cells(
      self.output_rect(&self.overview_output()),
      grid_width,
      grid_height,
      self.expo_config.offset,
    );
    let current = self.compositor.current_workspace_index().unwrap_or(0);
    let original = self.original_expo_geometry(current);
    let geometry = original
      .iter()
      .filter_map(|(window, original)| {
        let info = self.compositor.core().window_info(*window).ok()?;
        let workspace = info.location.workspace.y as usize
          * grid_width as usize
          + info.location.workspace.x as usize;
        let cell = *cells.get(workspace)?;
        Some((*window, fit_scale_tile(cell, *original, false)))
      })
      .collect::<HashMap<_, _>>();
    let windows = self
      .compositor
      .scale_surfaces(true, false)
      .into_iter()
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>();
    if self.expo.as_ref().is_some_and(|state| {
      state.cells == cells
        && state.windows == windows
        && state.geometry == geometry
    }) {
      return;
    }
    if let Some(state) = &mut self.expo {
      state.cell_origin = previous
        .as_ref()
        .map(|previous| previous.0.clone())
        .unwrap_or_else(|| state.cell_origin.clone());
      state.geometry_origin = geometry
        .keys()
        .filter_map(|window| {
          previous
            .as_ref()
            .and_then(|previous| previous.1.get(window))
            .or_else(|| original.get(window))
            .map(|geometry| (*window, *geometry))
        })
        .collect();
      state.cells = cells;
      state.windows = windows;
      state.geometry = geometry;
      state.animation_started = now;
    }
  }

  pub(super) fn move_expo_selection(&mut self, direction: Direction) {
    let (width, height) = self.compositor.workspace_grid_dimensions();
    if let Some(state) = &mut self.expo {
      state.move_selection(direction, width, height, Instant::now());
    }
    self.request_redraw();
  }

  /// Updates the hovered cell and any pending or active thumbnail drag.
  pub(super) fn move_expo_pointer(&mut self, x: f64, y: f64) {
    let duration = Duration::from_millis(self.expo_config.duration_ms);
    let changed = if let Some(state) = &mut self.expo {
      let previous = state.selected;
      state.pointer_move(x, y, Instant::now(), duration);
      state.selected != previous || state.pointer_pressed()
    } else {
      false
    };
    if changed {
      self.request_redraw();
    }
  }

  /// Left release commits either a cell click or a completed window drag.
  pub(super) fn expo_pointer_input(
    &mut self,
    button: u32,
    pressed: bool,
  ) -> bool {
    if self.expo.is_none() {
      return false;
    }
    if button == BTN_LEFT {
      if pressed {
        let duration = Duration::from_millis(self.expo_config.duration_ms);
        if let Some(state) = &mut self.expo {
          state.pointer_down(
            self.pointer_x,
            self.pointer_y,
            Instant::now(),
            duration,
          );
        }
      } else {
        self.finish_expo();
      }
    }
    true
  }

  /// Captures all keyboard input while Expo is active.
  pub(super) fn expo_key(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
    pressed: bool,
    locked: bool,
  ) -> bool {
    if self.expo.is_none() {
      return false;
    }
    if locked {
      self.clear_expo();
      return false;
    }
    if !pressed {
      return true;
    }
    let direct = self
      .expo_config
      .workspace_bindings
      .iter()
      .find(|entry| entry.binding.matches(modifiers, key))
      .map(|entry| entry.workspace - 1);
    if let Some(selected) = direct {
      if let Some(state) = &mut self.expo {
        state.cancel_pointer();
        state.set_selected(selected, Instant::now());
      }
      self.finish_expo();
      return true;
    }
    if self
      .expo
      .as_ref()
      .is_some_and(|state| state.pointer_pressed())
    {
      return true;
    }
    if key == "escape" {
      self.cancel_expo();
    } else if key == "enter" {
      self.finish_expo();
    } else if self.keybindings.expo_toggle.matches(modifiers, key) {
      self.toggle_expo();
    } else if self.expo_config.keyboard_interaction {
      let direction = match key {
        "left" | "arrowleft" | "kp4" | "h" => Some(Direction::Left),
        "right" | "arrowright" | "kp6" | "l" => Some(Direction::Right),
        "up" | "arrowup" | "kp8" | "k" => Some(Direction::Up),
        "down" | "arrowdown" | "kp2" | "j" => Some(Direction::Down),
        _ => None,
      };
      if let Some(direction) = direction {
        self.move_expo_selection(direction);
      }
    }
    true
  }
}
