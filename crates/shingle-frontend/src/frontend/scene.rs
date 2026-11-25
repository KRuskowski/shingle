//! The scene build: turns compositor surfaces and the frontend's
//! visual state into render tiles and draws them.

use std::{
  collections::{HashMap, HashSet},
  error::Error,
  time::{Duration, Instant},
};

use shingle::{BufferSnapshot, SurfaceId, SurfaceRole};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuTexture, RenderRect, RenderTile, TextureTransform, animation_progress,
  preview_tiles,
};

use super::{
  Frontend,
  animation::{ClosingAnimation, GeometryAnimation, apply_opening_animation},
  scale::ScaleTitle,
};
use crate::{
  host::Host,
  icon_theme,
  render_target::RenderTarget,
  scale::{
    ScaleState, interpolate_scale_rect, rect_contains, scale_geometry_at,
  },
  scale_title, workspace_transition,
};

impl Frontend {
  pub(super) fn append_closing_surfaces<'a>(
    animations: &'a HashMap<SurfaceId, ClosingAnimation>,
    textures: &'a HashMap<SurfaceId, GpuTexture>,
    now: Instant,
    config: &shingle_config::AnimateConfig,
    effect: &str,
    tiles: &mut Vec<RenderTile<'a>>,
  ) {
    let mut surfaces = animations.keys().copied().collect::<Vec<_>>();
    surfaces.sort_unstable();
    for surface in surfaces {
      let animation = &animations[&surface];
      let Some(texture) = textures.get(&surface) else {
        continue;
      };
      let mut tile = RenderTile::textured(animation.geometry, texture);
      apply_buffer(&mut tile, &animation.buffer);
      let progress = animation_progress(
        animation.started,
        now,
        Duration::from_millis(config.duration_ms),
      );
      apply_opening_animation(&mut tile, effect, 1.0 - progress);
      tiles.push(tile);
    }
  }

  pub(super) fn append_drop_preview<'a>(
    &self,
    tiles: &mut Vec<RenderTile<'a>>,
  ) {
    let Some(rect) = self.drop_preview else {
      return;
    };
    tiles.extend(preview_tiles(
      rect,
      self.preview_base_color.map(|component| component as f32),
      self.preview_border_color.map(|component| component as f32),
      self.preview_border_width,
    ));
  }

  /// Appends client-supplied icons above active Scale thumbnails.
  pub(super) fn append_scale_icons<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut HashMap<TileWindowId, GpuTexture>,
    state: &ScaleState,
    now: Instant,
    duration: Duration,
    icons: &HashMap<TileWindowId, shingle::ToplevelIcon>,
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    let geometry = scale_geometry_at(state, now, duration);
    for window in &state.windows {
      if cache.contains_key(window) {
        continue;
      }
      let Some(icon) = icons.get(window) else {
        continue;
      };
      let supplied = icon.buffers.iter().min_by_key(|buffer| {
        let scale = buffer.scale.max(1) as u32;
        (buffer.width / scale).abs_diff(32)
      });
      let (width, rgba) = if let Some(icon) = supplied {
        (icon.width, icon.rgba.to_vec())
      } else {
        let Some(name) = icon.name.as_deref() else {
          continue;
        };
        let Some(icon) = icon_theme::load(name, 64) else {
          continue;
        };
        (icon.width, icon.rgba)
      };
      let texture = renderer.create_texture(width, width, &rgba)?;
      cache.insert(*window, texture);
    }
    for window in &state.windows {
      let (Some(texture), Some(geometry)) =
        (cache.get(window), geometry.get(window))
      else {
        continue;
      };
      tiles.push(RenderTile::textured(
        RenderRect {
          x: geometry.x.saturating_add(8),
          y: geometry.y.saturating_add(8),
          width: 32,
          height: 32,
        },
        texture,
      ));
    }
    Ok(())
  }

  /// Appends cached title textures above active Scale thumbnails, placed
  /// where the thumbnails are at frame time `now`.
  #[allow(clippy::too_many_arguments)]
  pub(super) fn append_scale_titles<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut HashMap<TileWindowId, ScaleTitle>,
    state: &ScaleState,
    now: Instant,
    config: &shingle_config::ScaleConfig,
    titles: &HashMap<TileWindowId, String>,
    pointer: (f64, f64),
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    if config.title_overlay == "never" {
      return Ok(());
    }
    let geometry =
      scale_geometry_at(state, now, Duration::from_millis(config.duration_ms));
    for window in &state.windows {
      let (Some(title), Some(target), Some(current)) = (
        titles.get(window),
        state.geometry.get(window),
        geometry.get(window),
      ) else {
        continue;
      };
      if config.title_overlay == "mouse"
        && !rect_contains(*current, pointer.0, pointer.1)
      {
        continue;
      }
      let needs_texture = cache.get(window).is_none_or(|cached| {
        cached.title != *title || cached.target_width != target.width
      });
      if !needs_texture {
        continue;
      }
      let Some(raster) = scale_title::rasterize_title(
        title,
        config.title_font_size,
        target.width,
        config.title_bg_color.map(|value| value as f32),
        config.title_text_color.map(|value| value as f32),
      ) else {
        continue;
      };
      let texture =
        renderer.create_texture(raster.width, raster.height, &raster.rgba)?;
      cache.insert(
        *window,
        ScaleTitle {
          title: title.clone(),
          target_width: target.width,
          texture,
        },
      );
    }
    for window in &state.windows {
      let (Some(title), Some(current)) =
        (cache.get(window), geometry.get(window))
      else {
        continue;
      };
      if config.title_overlay == "mouse"
        && !rect_contains(*current, pointer.0, pointer.1)
      {
        continue;
      }
      let x = current.x.saturating_add(
        i32::try_from(current.width.saturating_sub(title.texture.width()) / 2)
          .unwrap_or(i32::MAX),
      );
      let y_offset = match config.title_position.as_str() {
        "top" => 0,
        "bottom" => current.height.saturating_sub(title.texture.height()),
        _ => current.height.saturating_sub(title.texture.height()) / 2,
      };
      let y = current
        .y
        .saturating_add(i32::try_from(y_offset).unwrap_or(i32::MAX));
      tiles.push(RenderTile::textured(
        RenderRect {
          x,
          y,
          width: title.texture.width(),
          height: title.texture.height(),
        },
        &title.texture,
      ));
    }
    Ok(())
  }

  /// Displays the current filter centered above the overview.
  pub(super) fn append_scale_filter_overlay<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut Option<ScaleTitle>,
    filter: &str,
    config: &shingle_config::ScaleTitleFilterConfig,
    output: shingle_core::Rect,
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    if !config.overlay || filter.is_empty() {
      return Ok(());
    }
    let needs_texture = cache.as_ref().is_none_or(|cached| {
      cached.title != filter || cached.target_width != output.width
    });
    if needs_texture {
      let Some(raster) = scale_title::rasterize_title(
        filter,
        config.font_size,
        output.width,
        config.bg_color.map(|value| value as f32),
        config.text_color.map(|value| value as f32),
      ) else {
        return Ok(());
      };
      let texture =
        renderer.create_texture(raster.width, raster.height, &raster.rgba)?;
      *cache = Some(ScaleTitle {
        title: filter.to_owned(),
        target_width: output.width,
        texture,
      });
    }
    let overlay = cache.as_ref().unwrap();
    let x = output.x.saturating_add(
      i32::try_from(output.width.saturating_sub(overlay.texture.width()) / 2)
        .unwrap_or(i32::MAX),
    );
    let y = output.y.saturating_add(
      i32::try_from(output.height.saturating_sub(overlay.texture.height()) / 2)
        .unwrap_or(i32::MAX),
    );
    tiles.push(RenderTile::textured(
      RenderRect {
        x,
        y,
        width: overlay.texture.width(),
        height: overlay.texture.height(),
      },
      &overlay.texture,
    ));
    Ok(())
  }

  /// Builds the scene and draws it into `target`, then sends frame
  /// callbacks if a frame was presented. Asks the host for another frame
  /// while animations run.
  pub fn render(
    &mut self,
    host: &mut dyn Host,
    target: &mut dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    let result = self.render_frame(target);
    self.flush(host);
    result
  }

  /// Draws one frame. Every animation is drawn and then continued or
  /// ended at one time, `now`, taken before drawing. An animation that
  /// was drawn short of its end therefore always gets another frame, even
  /// if drawing took longer than what was left of it, and ends only after
  /// a frame has shown its final state, as Wayfire's `duration_t` does.
  pub(super) fn render_frame(
    &mut self,
    target: &mut dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    self.update_expo_layout();
    let now = Instant::now();
    if let Some((started, start)) = self.zoom_animation {
      let progress = animation_progress(
        started,
        now,
        Duration::from_millis(self.zoom.smoothing_ms),
      );
      self.zoom_factor =
        shingle_render::interpolate_zoom(start, self.zoom_target, progress);
      if progress >= 1.0 {
        self.zoom_factor = self.zoom_target;
        self.zoom_animation = None;
      }
    }
    let mut tiles = {
      let renderer = &mut *target;
      renderer.set_scale_factor(self.compositor.output_scale());
      renderer.set_output_transform(render_output_transform(
        self.compositor.output_transform(),
      ));
      if renderer.set_nearest_filter(self.zoom.interpolation == "nearest") {
        self.textures.clear();
        self.previous_surfaces.clear();
        self.animations.clear();
      }
      renderer.set_zoom(
        self.zoom_factor,
        self.zoom_origin.0,
        self.zoom_origin.1,
      );
      renderer.set_gamma_ramp(self.compositor.gamma_ramp())?;
      renderer.set_inversion(self.inverted, self.invert_preserve_hue);
      let mut snapshots = if self.expo.is_some() || self.expo_exit.is_some() {
        self.compositor.scale_surfaces(true, false)
      } else {
        self.compositor.visible_surfaces()
      };
      if let Some(state) = self.scale.as_ref().or(self.scale_exit.as_ref()) {
        let visible = snapshots
          .iter()
          .map(|snapshot| snapshot.id)
          .collect::<HashSet<_>>();
        snapshots.extend(
          self
            .compositor
            .scale_surfaces(
              state.all_workspaces,
              self.scale_config.include_minimized,
            )
            .into_iter()
            .filter(|snapshot| !visible.contains(&snapshot.id)),
        );
      }
      if self.workspace_transition.is_some() {
        let visible = snapshots
          .iter()
          .map(|snapshot| snapshot.id)
          .collect::<HashSet<_>>();
        snapshots.extend(
          self
            .compositor
            .scale_surfaces(true, false)
            .into_iter()
            .filter(|snapshot| !visible.contains(&snapshot.id)),
        );
      }
      for snapshot in &snapshots {
        let Some(buffer) = &snapshot.buffer else {
          continue;
        };
        let recreate = self.textures.get(&snapshot.id).is_none_or(|texture| {
          texture.width() != buffer.width || texture.height() != buffer.height
        });
        if recreate {
          let texture = renderer.create_texture(
            buffer.width,
            buffer.height,
            &buffer.rgba,
          )?;
          self.textures.insert(snapshot.id, texture);
        } else if self.dirty.contains(&snapshot.id) {
          renderer
            .update_texture(&self.textures[&snapshot.id], &buffer.rgba)?;
        }
      }
      for snapshot in &snapshots {
        if snapshot.buffer.is_none() {
          continue;
        }
        let geometry = RenderRect {
          x: snapshot.geometry.x,
          y: snapshot.geometry.y,
          width: snapshot.geometry.width,
          height: snapshot.geometry.height,
        };
        let Some((old_geometry, old_buffer)) =
          self.previous_surfaces.get(&snapshot.id)
        else {
          continue;
        };
        if *old_geometry == geometry
          || self.animations.contains_key(&snapshot.id)
          || self.animation_duration.is_zero()
        {
          continue;
        }
        let old_texture = renderer.create_texture(
          old_buffer.width,
          old_buffer.height,
          &old_buffer.rgba,
        )?;
        self.animations.insert(
          snapshot.id,
          GeometryAnimation {
            started: now,
            old_geometry: *old_geometry,
            old_texture,
          },
        );
      }
      self.animation_enabled = snapshots
        .iter()
        .filter(|snapshot| self.animation_matches(snapshot))
        .map(|snapshot| snapshot.id)
        .collect();
      self.surface_windows = snapshots
        .iter()
        .filter(|snapshot| {
          snapshot.role == SurfaceRole::Toplevel && snapshot.buffer.is_some()
        })
        .filter_map(|snapshot| {
          snapshot.window.map(|window| (snapshot.id, window))
        })
        .collect();
      let visible = snapshots
        .iter()
        .filter(|snapshot| snapshot.buffer.is_some())
        .map(|snapshot| snapshot.id)
        .collect::<HashSet<_>>();
      self.textures.retain(|id, _| {
        visible.contains(id)
          || self.closing_animations.contains_key(id)
          || self.minimize_animations.contains_key(id)
      });
      self.animations.retain(|id, _| visible.contains(id));
      let tiles = snapshots
        .iter()
        .filter_map(|snapshot| {
          let texture = self.textures.get(&snapshot.id)?;
          let geometry = snapshot.geometry;
          let mut tile = RenderTile::textured(
            RenderRect {
              x: geometry.x,
              y: geometry.y,
              width: geometry.width,
              height: geometry.height,
            },
            texture,
          );
          tile.color = [snapshot.opacity; 4];
          if let Some(buffer) = &snapshot.buffer {
            apply_buffer(&mut tile, buffer);
          }
          let mut tiles = Vec::with_capacity(2);
          if let Some(animation) = self.animations.get(&snapshot.id)
            && let Some((_, old_buffer)) =
              self.previous_surfaces.get(&snapshot.id)
          {
            let progress = animation_progress(
              animation.started,
              now,
              self.animation_duration,
            );
            if progress < 1.0 {
              let mut old_tile = RenderTile::textured(
                animation.old_geometry,
                &animation.old_texture,
              );
              old_tile.color = [snapshot.opacity; 4];
              apply_buffer(&mut old_tile, old_buffer);
              tiles.push(old_tile);
            }
            tile.color = [snapshot.opacity * progress; 4];
          }
          if let Some(started) = self.open_animations.get(&snapshot.id) {
            let progress = animation_progress(
              *started,
              now,
              Duration::from_millis(self.animate.duration_ms),
            );
            apply_opening_animation(
              &mut tile,
              &self.animate.open_animation,
              progress,
            );
          }
          tiles.push(tile);
          if snapshot.role == SurfaceRole::Toplevel
            && !self.minimize_animations.contains_key(&snapshot.id)
            && let Some(window) = snapshot.window
            && let Some(state) =
              self.scale.as_ref().or(self.scale_exit.as_ref())
            && let Some(geometry) = state.geometry.get(&window)
          {
            let selected = state.windows.get(state.selected) == Some(&window);
            let minimized = self
              .compositor
              .core()
              .window_info(window)
              .is_ok_and(|info| info.state.minimized);
            let inactive_alpha = if minimized {
              self.scale_config.minimized_alpha
            } else {
              self.scale_config.inactive_alpha
            };
            let progress = animation_progress(
              state.animation_started,
              now,
              Duration::from_millis(self.scale_config.duration_ms),
            );
            let opacity = if self.scale.is_none() {
              let origin =
                state.opacity_origin.get(&window).copied().unwrap_or(1.0);
              let target = if minimized { 0.0 } else { 1.0 };
              origin + (target - origin) * progress
            } else if selected {
              1.0
            } else {
              1.0 - (1.0 - inactive_alpha) * progress
            };
            for tile in &mut tiles {
              let origin = state
                .animation_origin
                .get(&window)
                .copied()
                .unwrap_or(tile.geometry);
              tile.geometry =
                interpolate_scale_rect(origin, *geometry, progress);
              tile.color[3] *= opacity;
            }
          }
          if let Some(window) = snapshot.window
            && let Some(state) = self.expo.as_ref().or(self.expo_exit.as_ref())
            && let Some(geometry) = state.geometry.get(&window)
            && let Ok(info) = self.compositor.core().window_info(window)
          {
            let (width, _) = self.compositor.workspace_grid_dimensions();
            let workspace = info.location.workspace.y as usize * width as usize
              + info.location.workspace.x as usize;
            let duration = Duration::from_millis(self.expo_config.duration_ms);
            let progress =
              animation_progress(state.animation_started, now, duration);
            let origin = state
              .geometry_origin
              .get(&window)
              .copied()
              .unwrap_or(*geometry);
            let brightness = state.brightness(
              workspace,
              now,
              Duration::from_millis(self.expo_config.transition_length_ms),
              self.expo_config.inactive_brightness,
            );
            let dragged = state
              .dragged_window()
              .filter(|(dragged, _, _)| *dragged == window);
            for tile in &mut tiles {
              tile.geometry =
                crate::expo::interpolate_rect(origin, *geometry, progress);
              if let Some((_, x, y)) = dragged {
                tile.geometry.x =
                  (x - f64::from(tile.geometry.width) / 2.0).round() as i32;
                tile.geometry.y =
                  (y - f64::from(tile.geometry.height) / 2.0).round() as i32;
              }
              for component in &mut tile.color[..3] {
                *component *= brightness;
              }
            }
          }
          if let Some(window) = snapshot.window
            && let Some(transition) = &self.workspace_transition
            && let Ok(info) = self.compositor.core().window_info(window)
          {
            let (grid_width, _) = self.compositor.workspace_grid_dimensions();
            let workspace = info.location.workspace.y as usize
              * grid_width as usize
              + info.location.workspace.x as usize;
            let layout = workspace_transition::TransitionLayout {
              output: self.compositor.output(),
              grid_width,
              gap: self.vswitch.gap,
            };
            let geometry = transition.geometry(
              snapshot.geometry,
              workspace,
              layout,
              now,
              Duration::from_millis(self.vswitch.duration_ms),
            );
            for tile in &mut tiles {
              tile.geometry = geometry;
            }
          }
          if let Some(angle) = snapshot
            .window
            .and_then(|window| self.rotations.get(&window))
          {
            for tile in &mut tiles {
              tile.rotation = *angle;
            }
          }
          Some(tiles)
        })
        .flatten()
        .collect::<Vec<_>>();
      self.previous_surfaces = snapshots
        .iter()
        .filter_map(|snapshot| {
          let geometry = snapshot.geometry;
          Some((
            snapshot.id,
            (
              RenderRect {
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
              },
              snapshot.buffer.clone()?,
            ),
          ))
        })
        .collect();
      tiles
    };
    if let Some(state) = self.expo.as_ref().or(self.expo_exit.as_ref()) {
      let duration = Duration::from_millis(self.expo_config.duration_ms);
      let transition =
        Duration::from_millis(self.expo_config.transition_length_ms);
      let mut cells = Vec::new();
      for (index, cell) in state.cells_at(now, duration).iter().enumerate() {
        let brightness = state.brightness(
          index,
          now,
          transition,
          self.expo_config.inactive_brightness,
        );
        let border = [0.8 * brightness; 4];
        cells.extend(preview_tiles(
          *cell,
          self
            .expo_config
            .background
            .map(|component| component as f32),
          border,
          2,
        ));
      }
      cells.append(&mut tiles);
      tiles = cells;
    }
    self.append_drop_preview(&mut tiles);
    if let Some(state) = &self.scale {
      let snapshots = self.compositor.scale_surfaces(true, true);
      let titles = snapshots
        .iter()
        .filter_map(|snapshot| {
          Some((snapshot.window?, snapshot.title.clone()?))
        })
        .collect::<HashMap<_, _>>();
      let icons = snapshots
        .into_iter()
        .filter_map(|snapshot| Some((snapshot.window?, snapshot.icon?)))
        .collect::<HashMap<_, _>>();
      Self::append_scale_icons(
        &*target,
        &mut self.scale_icons,
        state,
        now,
        Duration::from_millis(self.scale_config.duration_ms),
        &icons,
        &mut tiles,
      )?;
      Self::append_scale_titles(
        &*target,
        &mut self.scale_titles,
        state,
        now,
        &self.scale_config,
        &titles,
        (self.pointer_x, self.pointer_y),
        &mut tiles,
      )?;
    }
    if self.scale.is_some() {
      Self::append_scale_filter_overlay(
        &*target,
        &mut self.scale_filter_texture,
        &self.scale_filter,
        &self.scale_title_filter,
        self.compositor.output(),
        &mut tiles,
      )?;
    }
    Self::append_closing_surfaces(
      &self.closing_animations,
      &self.textures,
      now,
      &self.animate,
      &self.animate.close_animation,
      &mut tiles,
    );
    Self::append_closing_surfaces(
      &self.minimize_animations,
      &self.textures,
      now,
      &self.animate,
      &self.animate.minimize_animation,
      &mut tiles,
    );
    let renderer = target;
    renderer.set_tearing(self.compositor.allows_tearing());
    let background = if self.expo.is_some() || self.expo_exit.is_some() {
      self.expo_config.background
    } else if self.workspace_transition.is_some() {
      self.vswitch.background
    } else {
      self.background_color
    };
    if renderer.render(&tiles, background)? {
      self.compositor.frame();
    }
    // Decide with the time this frame was drawn at, not the time drawing
    // finished: a slow frame must not skip an animation's final state.
    self.animations.retain(|_, animation| {
      animation_progress(animation.started, now, self.animation_duration) < 1.0
    });
    self.open_animations.retain(|_, started| {
      animation_progress(
        *started,
        now,
        Duration::from_millis(self.animate.duration_ms),
      ) < 1.0
    });
    self.expire_closing_animations(now);
    self.expire_minimize_animations(now);
    self.expire_scale_exit(now);
    if self
      .workspace_transition
      .as_ref()
      .is_some_and(|transition| {
        animation_progress(
          transition.started,
          now,
          Duration::from_millis(self.vswitch.duration_ms),
        ) >= 1.0
      })
    {
      self.workspace_transition = None;
    }
    let expo_duration = Duration::from_millis(self.expo_config.duration_ms);
    if self.expo_exit.as_ref().is_some_and(|state| {
      animation_progress(state.animation_started, now, expo_duration) >= 1.0
    }) {
      self.expo_exit = None;
    }
    let expo_transition =
      Duration::from_millis(self.expo_config.transition_length_ms);
    if !self.animations.is_empty()
      || !self.open_animations.is_empty()
      || !self.closing_animations.is_empty()
      || !self.minimize_animations.is_empty()
      || self.scale.as_ref().is_some_and(|state| {
        animation_progress(
          state.animation_started,
          now,
          Duration::from_millis(self.scale_config.duration_ms),
        ) < 1.0
      })
      || self.scale_exit.is_some()
      || self.expo.as_ref().is_some_and(|state| {
        animation_progress(state.animation_started, now, expo_duration) < 1.0
          || animation_progress(state.selection_started, now, expo_transition)
            < 1.0
      })
      || self.expo_exit.is_some()
      || self.workspace_transition.is_some()
      || self.zoom_animation.is_some()
    {
      self.request_redraw();
    }
    self.dirty.clear();
    Ok(())
  }
}

pub(super) fn apply_buffer(tile: &mut RenderTile<'_>, buffer: &BufferSnapshot) {
  tile.transform = match buffer.transform {
    wayland_server::protocol::wl_output::Transform::Normal => {
      TextureTransform::Normal
    }
    wayland_server::protocol::wl_output::Transform::Flipped => {
      TextureTransform::FlipHorizontal
    }
    wayland_server::protocol::wl_output::Transform::_90 => {
      TextureTransform::Rotate90
    }
    wayland_server::protocol::wl_output::Transform::_180 => {
      TextureTransform::FlipBoth
    }
    wayland_server::protocol::wl_output::Transform::_270 => {
      TextureTransform::Rotate270
    }
    wayland_server::protocol::wl_output::Transform::Flipped90 => {
      TextureTransform::Transpose
    }
    wayland_server::protocol::wl_output::Transform::Flipped180 => {
      TextureTransform::FlipVertical
    }
    wayland_server::protocol::wl_output::Transform::Flipped270 => {
      TextureTransform::AntiTranspose
    }
    _ => TextureTransform::Normal,
  };
  if let (Some(texture), Some(source)) = (tile.texture, buffer.source) {
    tile.uv = [
      ((source[0] + 0.5) / f64::from(texture.width())) as f32,
      ((source[1] + 0.5) / f64::from(texture.height())) as f32,
      ((source[0] + source[2] - 0.5) / f64::from(texture.width())) as f32,
      ((source[1] + source[3] - 0.5) / f64::from(texture.height())) as f32,
    ];
  }
}

pub(super) fn render_output_transform(
  transform: wayland_server::protocol::wl_output::Transform,
) -> TextureTransform {
  use wayland_server::protocol::wl_output::Transform;
  match transform {
    Transform::Normal => TextureTransform::Normal,
    Transform::Flipped => TextureTransform::FlipHorizontal,
    Transform::_90 => TextureTransform::Rotate90,
    Transform::_180 => TextureTransform::FlipBoth,
    Transform::_270 => TextureTransform::Rotate270,
    Transform::Flipped90 => TextureTransform::Transpose,
    Transform::Flipped180 => TextureTransform::FlipVertical,
    Transform::Flipped270 => TextureTransform::AntiTranspose,
    _ => TextureTransform::Normal,
  }
}
