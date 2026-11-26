//! Scale: the window overview. The layout lives in
//! [`crate::scale`]; this is its state machine, input and title filter.

use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use shingle::{Direction, SurfaceRole, SurfaceSnapshot};
use shingle_config::Modifiers as ConfigModifiers;
use shingle_core::WindowId as TileWindowId;
use shingle_render::{GpuTexture, RenderRect, animation_progress};

use super::{BTN_LEFT, BTN_MIDDLE, Frontend};
use crate::scale::{
  ScaleState, fit_scale_tile, rect_contains, scale_cells, scale_columns,
  scale_filter_matches, scale_geometry_at,
};

pub(super) struct ScaleTitle {
  pub(super) title: String,
  pub(super) target_width: u32,
  pub(super) texture: GpuTexture,
}

impl Frontend {
  /// Opens Scale, changes its workspace scope, or commits an active session.
  ///
  /// Changing scope preserves both the selected window and the focus that
  /// should be restored by a later cancellation.
  pub(super) fn toggle_scale(&mut self, all_workspaces: bool) {
    self.clear_expo();
    let active = self.compositor.active_output();
    if let Some(scene) = self.scenes.get_mut(&active) {
      scene.workspace_transition = None;
    }
    self.scale_exit = None;
    self.overview_output = Some(active);
    if let Some(state) = &self.scale {
      let no_remote_windows = self
        .compositor
        .scale_surfaces(true, self.scale_config.include_minimized)
        .len()
        == self
          .compositor
          .scale_surfaces(false, self.scale_config.include_minimized)
          .len();
      if state.all_workspaces == all_workspaces || no_remote_windows {
        self.finish_scale();
        return;
      }
    }
    let was_active = self.scale.is_some();
    let initial_workspace = self
      .scale
      .as_ref()
      .map(|state| state.initial_workspace)
      .unwrap_or_else(|| {
        self.compositor.current_workspace_index().unwrap_or(0)
      });
    let initial_focus = self
      .scale
      .as_ref()
      .and_then(|state| state.initial_focus)
      .or_else(|| self.compositor.focused_window());
    let selected_window = self
      .scale
      .as_ref()
      .and_then(|state| state.windows.get(state.selected).copied());
    if self.switcher.is_some() {
      self.finish_switcher();
    }
    self.compositor.set_show_desktop(false);
    self.drag = None;
    self.drop_preview = None;
    self.drop_slot = None;
    let windows = self
      .compositor
      .scale_surfaces(all_workspaces, self.scale_config.include_minimized)
      .into_iter()
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>();
    if windows.is_empty() {
      self.cancel_scale();
      return;
    }
    let selected = selected_window
      .or_else(|| self.compositor.focused_window())
      .and_then(|focused| windows.iter().position(|window| *window == focused))
      .unwrap_or_else(|| {
        windows
          .iter()
          .position(|window| {
            self
              .compositor
              .core()
              .window_info(*window)
              .is_ok_and(|info| !info.state.minimized)
          })
          .unwrap_or(0)
      });
    self.scale = Some(ScaleState {
      windows,
      selected,
      cells: Vec::new(),
      geometry: HashMap::new(),
      animation_origin: HashMap::new(),
      opacity_origin: HashMap::new(),
      animation_started: Instant::now(),
      all_workspaces,
      initial_focus,
      initial_workspace,
    });
    self.update_scale_layout();
    if !was_active {
      self.compositor.notify_plugin_activation("scale", true);
    }
    self.request_redraw();
  }

  /// Returns normal window rectangles, including retained minimized buffers.
  pub(super) fn original_scale_geometry(
    &self,
    windows: &[TileWindowId],
  ) -> HashMap<TileWindowId, RenderRect> {
    let snapshots = self
      .compositor
      .scale_surfaces(true, true)
      .into_iter()
      .filter_map(|snapshot| snapshot.window.map(|window| (window, snapshot)))
      .collect::<HashMap<_, _>>();
    windows
      .iter()
      .filter_map(|window| {
        let geometry = self.window_geometry(*window).or_else(|| {
          let snapshot = snapshots.get(window)?;
          let buffer = snapshot.buffer.as_ref()?;
          Some(shingle_core::Rect::new(
            snapshot.geometry.x,
            snapshot.geometry.y,
            buffer.width,
            buffer.height,
          ))
        })?;
        Some((
          *window,
          RenderRect {
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
          },
        ))
      })
      .collect()
  }

  /// Recomputes thumbnail targets without reconfiguring client windows.
  ///
  /// Minimized tiled leaves have no layout entry. Their last buffer size is
  /// therefore used as the aspect-ratio fallback until they are restored.
  pub(super) fn update_scale_layout(&mut self) {
    let now = Instant::now();
    let previous = self.scale.as_ref().map(|state| {
      scale_geometry_at(
        state,
        now,
        Duration::from_millis(self.scale_config.duration_ms),
      )
    });
    let Some(state) = &self.scale else {
      return;
    };
    let windows = state.windows.clone();
    let original = self.original_scale_geometry(&windows);
    let cells = scale_cells(
      self.output_rect(&self.overview_output()),
      windows.len(),
      self.scale_config.spacing,
      self.scale_config.outer_margin,
    );
    let geometry = windows
      .iter()
      .zip(&cells)
      .filter_map(|(window, cell)| {
        original.get(window).map(|original| {
          let original = RenderRect {
            x: original.x,
            y: original.y,
            width: original.width,
            height: original.height,
          };
          (
            *window,
            fit_scale_tile(*cell, original, self.scale_config.allow_zoom),
          )
        })
      })
      .collect::<HashMap<_, _>>();
    let animation_origin = windows
      .iter()
      .filter_map(|window| {
        previous
          .as_ref()
          .and_then(|previous| previous.get(window).copied())
          .or_else(|| {
            original.get(window).map(|original| RenderRect {
              x: original.x,
              y: original.y,
              width: original.width,
              height: original.height,
            })
          })
          .map(|geometry| (*window, geometry))
      })
      .collect();
    if let Some(state) = &mut self.scale {
      state.cells = cells;
      state.geometry = geometry;
      state.animation_origin = animation_origin;
      state.animation_started = now;
    }
  }

  /// Starts a noninteractive return from overview geometry to normal windows.
  pub(super) fn begin_scale_exit(&mut self, mut state: ScaleState) {
    let now = Instant::now();
    let duration = Duration::from_millis(self.scale_config.duration_ms);
    state.animation_origin = scale_geometry_at(&state, now, duration);
    let progress = animation_progress(state.animation_started, now, duration);
    state.opacity_origin = state
      .windows
      .iter()
      .map(|window| {
        let selected = state.windows.get(state.selected) == Some(window);
        let minimized = self
          .compositor
          .core()
          .window_info(*window)
          .is_ok_and(|info| info.state.minimized);
        let inactive = if minimized {
          self.scale_config.minimized_alpha
        } else {
          self.scale_config.inactive_alpha
        };
        let opacity = if selected {
          1.0
        } else {
          1.0 - (1.0 - inactive) * progress
        };
        (*window, opacity)
      })
      .collect();
    state.geometry = self.original_scale_geometry(&state.windows);
    state.animation_started = now;
    self.scale_exit = Some(state);
    self.scale_titles.clear();
    self.scale_icons.clear();
    self.scale_filter.clear();
    self.scale_filter_texture = None;
    self.request_redraw();
  }

  /// Commits the selected window and saves the resulting normal session.
  ///
  /// Remote workspaces are activated before focus. A minimized selection is
  /// restored first so that the tiling tree can accept focus again.
  pub(super) fn finish_scale(&mut self) {
    let Some(mut state) = self.scale.take() else {
      return;
    };
    self.compositor.notify_plugin_activation("scale", false);
    if let Some(window) = state.windows.get(state.selected).copied() {
      if let Ok(info) = self.compositor.core().window_info(window) {
        if info.state.minimized
          && let Err(error) = self.compositor.set_minimized(window, false)
        {
          eprintln!("shingle: scale restore failed: {error}");
        }
        let (width, _) = self.compositor.workspace_grid_dimensions();
        let workspace = info.location.workspace.y as usize * width as usize
          + info.location.workspace.x as usize;
        if !self
          .compositor
          .current_workspace_index()
          .is_ok_and(|current| current == workspace)
        {
          match self.compositor.switch_workspace(workspace) {
            Ok(()) => state.all_workspaces = false,
            Err(error) => {
              eprintln!("shingle: scale workspace switch failed: {error}")
            }
          }
        }
      }
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: scale focus failed: {error}");
      }
    }
    self.begin_scale_exit(state);
    self.save();
    self.request_redraw();
  }

  /// Discards thumbnail state and restores the pre-Scale focus when possible.
  pub(super) fn cancel_scale(&mut self) {
    let Some(state) = self.scale.take() else {
      return;
    };
    self.compositor.notify_plugin_activation("scale", false);
    if !self
      .compositor
      .current_workspace_index()
      .is_ok_and(|workspace| workspace == state.initial_workspace)
      && let Err(error) =
        self.compositor.switch_workspace(state.initial_workspace)
    {
      eprintln!("shingle: scale workspace restore failed: {error}");
    }
    if let Some(window) = state.initial_focus
      && self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| !info.state.minimized)
    {
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: scale cancellation focus failed: {error}");
      }
    }
    self.begin_scale_exit(state);
    self.save();
    self.request_redraw();
  }

  pub(super) fn select_scale_relative(&mut self, reverse: bool) {
    let Some(state) = &mut self.scale else {
      return;
    };
    if state.windows.is_empty() {
      return;
    }
    state.selected = if reverse {
      (state.selected + state.windows.len() - 1) % state.windows.len()
    } else {
      (state.selected + 1) % state.windows.len()
    };
    self.request_redraw();
  }

  pub(super) fn move_scale_selection(&mut self, direction: Direction) {
    let Some(state) = &mut self.scale else {
      return;
    };
    if state.windows.is_empty() {
      return;
    }
    let columns = scale_columns(state.cells.len());
    let row = state.selected / columns;
    let column = state.selected % columns;
    let selected = match direction {
      Direction::Left if column > 0 => Some(state.selected - 1),
      Direction::Right
        if column + 1 < columns && state.selected + 1 < state.windows.len() =>
      {
        Some(state.selected + 1)
      }
      Direction::Up if row > 0 => Some(state.selected - columns),
      Direction::Down if state.selected + columns < state.windows.len() => {
        Some(state.selected + columns)
      }
      _ => None,
    };
    if let Some(selected) = selected {
      state.selected = selected;
      self.request_redraw();
    }
  }

  pub(super) fn select_scale_at(&mut self, x: f64, y: f64) -> bool {
    let Some(state) = &mut self.scale else {
      return false;
    };
    let Some(selected) = state.windows.iter().position(|window| {
      state
        .geometry
        .get(window)
        .is_some_and(|rect| rect_contains(*rect, x, y))
    }) else {
      return false;
    };
    let changed = state.selected != selected;
    state.selected = selected;
    if changed {
      self.request_redraw();
    }
    true
  }

  pub(super) fn scale_pointer_input(
    &mut self,
    button: u32,
    pressed: bool,
  ) -> bool {
    if self.scale.is_none() {
      return false;
    }
    if !pressed || !self.select_scale_at(self.pointer_x, self.pointer_y) {
      return true;
    }
    match button {
      BTN_LEFT => self.finish_scale(),
      BTN_MIDDLE if self.scale_config.middle_click_close => {
        let window = self
          .scale
          .as_ref()
          .and_then(|state| state.windows.get(state.selected).copied());
        if let Some(window) = window {
          self.compositor.close_window(window);
        }
      }
      _ => {}
    }
    true
  }

  /// Rebuilds the visible overview set after text or window metadata changes.
  pub(super) fn apply_scale_filter(&mut self) {
    let Some(state) = &self.scale else {
      return;
    };
    let selected = state.windows.get(state.selected).copied();
    let windows = self
      .compositor
      .scale_surfaces(state.all_workspaces, self.scale_config.include_minimized)
      .into_iter()
      .filter(|snapshot| {
        scale_filter_matches(
          snapshot,
          &self.scale_filter,
          self.scale_title_filter.case_sensitive,
        )
      })
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>();
    let selected = selected
      .and_then(|window| {
        windows.iter().position(|candidate| *candidate == window)
      })
      .unwrap_or_else(|| {
        windows
          .iter()
          .position(|window| {
            self
              .compositor
              .core()
              .window_info(*window)
              .is_ok_and(|info| !info.state.minimized)
          })
          .unwrap_or(0)
      });
    if let Some(state) = &mut self.scale {
      state.windows = windows;
      state.selected = selected;
    }
    self.update_scale_layout();
    self.request_redraw();
  }

  /// Adds a newly eligible mapped window or honors `close_on_new_view`.
  ///
  /// Repeated commits from existing thumbnails and windows excluded by the
  /// current workspace/minimized filters do not change the overview.
  pub(super) fn scale_surface_committed(&mut self, snapshot: &SurfaceSnapshot) {
    if snapshot.role != SurfaceRole::Toplevel || snapshot.buffer.is_none() {
      return;
    }
    let Some(window) = snapshot.window else {
      return;
    };
    let Some(state) = &self.scale else {
      return;
    };
    if state.windows.contains(&window) {
      return;
    }
    let all_workspaces = state.all_workspaces;
    if !self
      .compositor
      .scale_surfaces(all_workspaces, self.scale_config.include_minimized)
      .iter()
      .any(|snapshot| snapshot.window == Some(window))
    {
      return;
    }
    if self.scale_config.close_on_new_view {
      self.finish_scale();
      return;
    }
    if let Some(state) = &mut self.scale {
      state.windows.push(window);
    }
    self.apply_scale_filter();
    self.request_redraw();
  }

  pub(super) fn scale_window_destroyed(&mut self, window: TileWindowId) {
    let Some(state) = &mut self.scale else {
      return;
    };
    let Some(removed) = state
      .windows
      .iter()
      .position(|candidate| *candidate == window)
    else {
      return;
    };
    state.windows.remove(removed);
    if removed < state.selected {
      state.selected -= 1;
    }
    if state.windows.is_empty() {
      self.cancel_scale();
      return;
    }
    state.selected = state.selected.min(state.windows.len() - 1);
    self.update_scale_layout();
    self.request_redraw();
  }

  /// Applies text input and UTF-8-safe backspace to the active title filter.
  pub(super) fn scale_filter_input(
    &mut self,
    key: &str,
    text: Option<&str>,
    pressed: bool,
  ) {
    if self.scale.is_none() || !pressed || !self.scale_filter_enabled {
      return;
    }
    let changed = if key == "backspace" {
      self.scale_filter.pop().is_some()
    } else if let Some(text) = text
      && !text.chars().any(char::is_control)
    {
      let previous = self.scale_filter.len();
      self.scale_filter.push_str(text);
      self.scale_filter.len() != previous
    } else {
      false
    };
    if changed {
      self.apply_scale_filter();
    }
  }

  /// Handles workspace navigation without moving windows during Scale.
  pub(super) fn scale_workspace_input(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
  ) -> bool {
    if self.scale.is_none() {
      return false;
    }
    let vertical = self.keybindings.workspace_up.matches(modifiers, key)
      || self.keybindings.workspace_down.matches(modifiers, key);
    let reverse = self.keybindings.workspace_previous.matches(modifiers, key)
      || self.keybindings.workspace_up.matches(modifiers, key);
    let relative = vertical
      || reverse
      || self.keybindings.workspace_next.matches(modifiers, key);
    let last = self.keybindings.workspace_last.matches(modifiers, key);
    let direct = self
      .keybindings
      .workspace_switch
      .iter()
      .position(|binding| binding.matches(modifiers, key));
    if !relative && !last && direct.is_none() {
      return false;
    }
    let result = if let Some(workspace) = direct {
      self.compositor.switch_workspace(workspace)
    } else if last {
      self.compositor.switch_to_last_workspace().map(|_| ())
    } else if vertical {
      self
        .compositor
        .switch_workspace_vertical(reverse)
        .map(|_| ())
    } else {
      self
        .compositor
        .switch_workspace_relative(reverse)
        .map(|_| ())
    };
    if let Err(error) = result {
      eprintln!("shingle: Scale workspace navigation failed: {error}");
    } else {
      self.apply_scale_filter();
      self.save();
    }
    true
  }

  pub(super) fn scale_key(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
    pressed: bool,
    locked: bool,
  ) -> bool {
    if self.scale.is_none() {
      return false;
    }
    if locked {
      self.cancel_scale();
      return false;
    }
    if pressed {
      if key == "escape" {
        self.cancel_scale();
      } else if key == "enter" {
        self.finish_scale();
      } else if self.keybindings.scale_toggle.matches(modifiers, key) {
        self.toggle_scale(false);
      } else if self.keybindings.scale_toggle_all.matches(modifiers, key) {
        self.toggle_scale(true);
      } else if self.scale_workspace_input(modifiers, key) {
      } else if key == "tab" {
        self.select_scale_relative(modifiers.shift);
      } else {
        let direction = match key {
          "left" | "arrowleft" => Some(Direction::Left),
          "right" | "arrowright" => Some(Direction::Right),
          "up" | "arrowup" => Some(Direction::Up),
          "down" | "arrowdown" => Some(Direction::Down),
          _ => None,
        };
        if let Some(direction) = direction {
          self.move_scale_selection(direction);
        }
      }
    }
    true
  }
}
