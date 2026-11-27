//! The scene build: turns compositor surfaces and the frontend's
//! visual state into render tiles and draws them.

use std::{
  collections::{BTreeSet, HashMap, HashSet},
  error::Error,
  time::{Duration, Instant},
};

use shingle::{
  BufferSnapshot, CaptureRequest, CaptureSource, OutputHandle, OutputKind,
  SurfaceId, SurfaceRole,
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuTexture, RenderRect, RenderTile, TextureTransform, animation_progress,
  preview_tiles, srgb_clear, srgb_fill,
};

use super::{
  Frontend,
  animation::{
    ClosingAnimation, GeometryAnimation, StartupFrame, apply_animation,
    has_effect, rect_of,
  },
  drag::DragState,
  frames::map_rect,
  outputs::{
    OutputScene, Startup, WsetLabel, label_rect, overlaps, rasterize_label,
    translate,
  },
  scale::ScaleTitle,
};
use crate::{
  animate::Timing,
  host::Host,
  icon_theme,
  render_target::RenderTarget,
  scale::{
    ScaleState, interpolate_scale_rect, rect_contains, scale_geometry_at,
  },
  scale_title, workspace_transition,
};

impl Frontend {
  /// Appends the closing or minimizing `animations` drawn on `output`.
  pub(super) fn append_closing_surfaces<'a>(
    animations: &'a HashMap<SurfaceId, ClosingAnimation>,
    textures: &'a HashMap<SurfaceId, GpuTexture>,
    output: &OutputHandle,
    now: Instant,
    tiles: &mut Vec<RenderTile<'a>>,
  ) {
    let mut surfaces = animations.keys().copied().collect::<Vec<_>>();
    surfaces.sort_unstable();
    for surface in surfaces {
      let closing = &animations[&surface];
      if closing.output != *output {
        continue;
      }
      let Some(texture) = textures.get(&surface) else {
        continue;
      };
      let mut tile = RenderTile::textured(closing.geometry, texture);
      apply_buffer(&mut tile, &closing.buffer);
      let above = apply_animation(
        &mut tile,
        &closing.animation,
        now,
        rect_of(closing.geometry),
      );
      tiles.push(tile);
      tiles.extend(above);
    }
  }

  pub(super) fn append_drop_preview<'a>(
    &self,
    tiles: &mut Vec<RenderTile<'a>>,
  ) {
    let Some(rect) = self.drop_preview else {
      return;
    };
    // Move's snap preview has `[move]`'s colours, simple-tile's drop
    // preview `[simple-tile]`'s, as each Wayfire plugin passes its own
    // prefix to `preview_indication_t`
    // (wayfire:plugins/single_plugins/move.cpp:494-495,
    // wayfire:plugins/tile/tile-dragging.hpp:122-123).
    let (base, border, width) = if self.drop_preview_snaps {
      (
        self.snap_preview_base_color,
        self.snap_preview_border_color,
        self.snap_preview_border_width,
      )
    } else {
      (
        self.preview_base_color,
        self.preview_border_color,
        self.preview_border_width,
      )
    };
    tiles.extend(preview_tiles(
      rect,
      srgb_fill(base),
      srgb_fill(border),
      width,
    ));
  }

  /// Appends client-supplied icons above active Scale thumbnails.
  pub(super) fn append_scale_icons<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut HashMap<TileWindowId, GpuTexture>,
    state: &ScaleState,
    now: Instant,
    duration: Timing,
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
    let geometry = scale_geometry_at(
      state,
      now,
      Timing::new(config.duration_ms, config.easing),
    );
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

  /// Builds the scene of `output` and draws it into `target`, the host's
  /// target for that output, then sends that output's frame callbacks if
  /// a frame was presented. Screencopy captures of `output`, and of
  /// outputs created over IPC, which no host draws, are answered from
  /// scenes drawn with the same target. Asks the host for another frame
  /// of each output whose animations run.
  pub fn render(
    &mut self,
    host: &mut dyn Host,
    output: &OutputHandle,
    target: &mut dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    self.turns.begin("render");
    let result = self.render_frame(output, target);
    self.turns.mark("frame");
    self.flush(host);
    self.turns.mark("flush");
    self.turns.end();
    result
  }

  /// Draws one frame of `output`. Every animation is drawn and then
  /// continued or ended at one time, `now`, taken before drawing. An
  /// animation that was drawn short of its end therefore always gets
  /// another frame, even if drawing took longer than what was left of it,
  /// and ends only after a frame has shown its final state, as Wayfire's
  /// `duration_t` does.
  pub(super) fn render_frame(
    &mut self,
    output: &OutputHandle,
    target: &mut dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    self.update_expo_layout();
    self.retire_frames();
    self.turns.mark("retire");
    let now = Instant::now();
    let Some(view) = self.compositor.output_view(output) else {
      return Ok(());
    };
    self.draw_output(&view, target, now, true)?;
    // Outputs created over IPC are drawn only for their captures.
    let virtual_outputs = self
      .captures
      .iter()
      .filter_map(|request| match &request.source {
        CaptureSource::Output { output, .. } => Some(output.clone()),
        CaptureSource::Window { .. } => None,
      })
      .collect::<BTreeSet<_>>()
      .into_iter()
      .filter_map(|captured| self.compositor.output_view(&captured))
      .filter(|view| view.kind == OutputKind::Virtual)
      .collect::<Vec<_>>();
    for view in &virtual_outputs {
      self.draw_output(view, target, now, false)?;
    }
    self.answer_window_captures(target);
    self.finish_frame(now);
    Ok(())
  }

  /// Builds the scene of the output `view` describes at frame time `now`
  /// and, with `present`, draws and presents it into `target`; then
  /// answers its captures. Its own animations, zoom and workspace slide
  /// are continued or ended.
  fn draw_output(
    &mut self,
    view: &shingle::OutputView,
    target: &mut dyn RenderTarget,
    now: Instant,
    present: bool,
  ) -> Result<(), Box<dyn Error>> {
    let output = &view.handle;
    self.scene_mut(output);
    let mut scene = self.scenes.remove(output).expect("made above");
    let result = self.draw_scene(view, &mut scene, target, now, present);
    self.end_scene_animations(output, &mut scene, now);
    self.scenes.insert(output.clone(), scene);
    result
  }

  /// [`Frontend::draw_output`] with the output's scene taken out of the
  /// map, so that the tiles can borrow it.
  fn draw_scene(
    &mut self,
    view: &shingle::OutputView,
    scene: &mut OutputScene,
    target: &mut dyn RenderTarget,
    now: Instant,
    present: bool,
  ) -> Result<(), Box<dyn Error>> {
    if self.compositor.lock_hides_clients() {
      return self.draw_lock_scene(view, scene, target, now, present);
    }
    // A lock released properly since the last frame fades its last
    // frame out over the desktop; one that crashed never got here, its
    // outputs staying black, and leaves nothing to fade.
    let unlocks = self.compositor.unlock_count();
    if let Some((texture, rect)) = scene.last_lock_frame.take()
      && unlocks > scene.unlocks_seen
      && !self.unlock_fade.is_zero()
    {
      scene.unlock_fade = Some(super::outputs::UnlockFade {
        texture,
        rect,
        started: now,
      });
    }
    scene.unlocks_seen = unlocks;
    let unlock_fade = scene.unlock_fade.as_ref().map(|fade| {
      let alpha = super::outputs::unlock_fade_alpha(
        now.saturating_duration_since(fade.started),
        self.unlock_fade,
      );
      (fade.texture.clone(), fade.rect, alpha)
    });
    let output = &view.handle;
    let rect = view.rect;
    let (scale_timing, expo_timing, vswitch_timing) = (
      self.scale_timing(),
      self.expo_timing(),
      self.vswitch_timing(),
    );
    if let Some((started, start)) = scene.zoom_animation {
      let timing =
        Timing::new(self.zoom.smoothing_ms, self.zoom.smoothing_easing);
      let progress = timing.linear_progress(started, now);
      scene.zoom_factor = shingle_render::interpolate_zoom(
        start,
        scene.zoom_target,
        timing.progress(started, now),
      );
      if progress >= 1.0 {
        scene.zoom_factor = scene.zoom_target;
        scene.zoom_animation = None;
      }
    }
    let overview_here = self.overview_output() == *output;
    let vswipe_here = self.vswipe_on(output);
    let expo_here =
      overview_here && (self.expo.is_some() || self.expo_exit.is_some());
    let scale_here =
      overview_here && (self.scale.is_some() || self.scale_exit.is_some());
    // The regular switcher draws over the whole of its output.
    let switcher_here = self
      .switcher_view
      .as_ref()
      .is_some_and(|shown| shown.output == *output);
    let cursor_ids: Vec<SurfaceId>;
    let background_shown: bool;
    let (mut tiles, mut locks) = {
      let renderer = &mut *target;
      renderer.set_scale_factor(view.scale);
      renderer.set_output_transform(render_output_transform(view.transform));
      let nearest = self.zoom.interpolation == "nearest";
      if renderer.set_nearest_filter(nearest) {
        // Uploaded shm buffers were given back, so their textures are
        // rebound with the new sampler rather than uploaded again.
        if let Some(uploader) = &self.uploader {
          let filter = self.texture_filter();
          for texture in self.textures.values_mut() {
            *texture = uploader.rebind(texture, filter);
          }
        } else {
          self.textures.clear();
        }
        scene.forget_surfaces();
        for other in self.scenes.values_mut() {
          other.forget_surfaces();
        }
      }
      if let Some(cache) = &self.dmabuf {
        cache.borrow_mut().set_filter(if nearest {
          shingle_render::TextureFilter::Nearest
        } else {
          shingle_render::TextureFilter::Linear
        });
      }
      renderer.set_zoom(
        scene.zoom_factor,
        scene.zoom_origin.0,
        scene.zoom_origin.1,
      );
      renderer.set_gamma_ramp(self.compositor.gamma_ramp_of(output))?;
      renderer.set_inversion(self.inverted, self.invert_preserve_hue);
      let mut snapshots = if expo_here {
        self.compositor.scale_surfaces_on(output, true, false)
      } else {
        self.compositor.visible_surfaces_on(output)
      };
      // A cursor plane shows the cursor instead.
      if self.hardware_cursors.contains(output) {
        snapshots.retain(|snapshot| snapshot.role != SurfaceRole::Cursor);
      }
      let extend = |snapshots: &mut Vec<shingle::SurfaceSnapshot>,
                    extra: Vec<shingle::SurfaceSnapshot>| {
        let visible = snapshots
          .iter()
          .map(|snapshot| snapshot.id)
          .collect::<HashSet<_>>();
        snapshots.extend(
          extra
            .into_iter()
            .filter(|snapshot| !visible.contains(&snapshot.id)),
        );
      };
      if scale_here
        && let Some(state) = self.scale.as_ref().or(self.scale_exit.as_ref())
      {
        extend(
          &mut snapshots,
          self.compositor.scale_surfaces_on(
            output,
            state.all_workspaces,
            self.scale_config.include_minimized,
          ),
        );
      }
      let wall = self.vswipe_viewport(output, now);
      if scene.workspace_transition.is_some() || wall.is_some() {
        extend(
          &mut snapshots,
          self.compositor.scale_surfaces_on(output, true, false),
        );
      }
      if switcher_here && let Some(shown) = &self.switcher_view {
        let mut windows = shown
          .view
          .thumbnails(now, &[])
          .into_iter()
          .map(|(window, _)| window)
          .collect::<Vec<_>>();
        windows.dedup();
        for window in windows {
          extend(&mut snapshots, self.compositor.window_surfaces(window));
        }
      }
      // A window being dragged is drawn on every output it crosses.
      if let Some(DragState::Move { source, .. }) = &self.drag {
        let dragged = self.compositor.window_surfaces(*source);
        if dragged
          .iter()
          .any(|snapshot| overlaps(snapshot.geometry, rect))
        {
          extend(&mut snapshots, dragged);
        }
      }
      // Fire moves its particles once per frame of its output.
      if self.has_fire() {
        let drawn = snapshots
          .iter()
          .map(|snapshot| (snapshot.id, snapshot.geometry))
          .collect::<Vec<_>>();
        self.step_animations(output, &drawn, now);
      }
      for snapshot in &snapshots {
        let Some(buffer) = &snapshot.buffer else {
          continue;
        };
        if let Some(content) = buffer.dmabuf() {
          // Imported when the client created it: a commit only picks the
          // cached texture.
          let texture = self
            .dmabuf
            .as_ref()
            .and_then(|cache| cache.borrow_mut().texture(&content.buffer));
          if let Some(texture) = texture {
            if scene.dirty.contains(&snapshot.id) {
              shingle::trace::buffer(format_args!(
                "dmabuf cached dmabuf={} surface={}",
                content.buffer.serial,
                snapshot.id.get()
              ));
            }
            self.textures.insert(snapshot.id, texture);
          } else {
            self.textures.remove(&snapshot.id);
          }
          continue;
        }
        if let Some(content) = buffer.shm() {
          // Uploaded after its commit, unless that failed or the commit
          // was missed: then all of it, now.
          if content.is_locked() {
            self.upload_shm(snapshot.id, buffer, content, false);
          }
          continue;
        }
        let Some(rgba) = buffer.rgba() else {
          continue;
        };
        let recreate = self.textures.get(&snapshot.id).is_none_or(|texture| {
          texture.is_imported()
            || texture.width() != buffer.width
            || texture.height() != buffer.height
        });
        if recreate {
          let texture =
            renderer.create_texture(buffer.width, buffer.height, &rgba)?;
          self.textures.insert(snapshot.id, texture);
        } else if scene.dirty.contains(&snapshot.id) {
          renderer.update_texture(&self.textures[&snapshot.id], &rgba)?;
        } else {
          continue;
        }
        shingle::trace::buffer(format_args!(
          "shm upload {} bytes surface={}",
          rgba.len(),
          snapshot.id.get()
        ));
      }
      // Frames of the windows drawn here, below their surfaces.
      let mut frames = self.compositor.frames();
      frames.retain(|surface, _| {
        snapshots.iter().any(|snapshot| snapshot.id == *surface)
      });
      self.frame_textures.prepare(
        &*renderer,
        &frames,
        &self.decoration_config,
        view.scale,
      )?;
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
          scene.previous_surfaces.get(&snapshot.id)
        else {
          continue;
        };
        if *old_geometry == geometry
          || scene.animations.contains_key(&snapshot.id)
          || self.animation_duration.is_zero()
        {
          continue;
        }
        let old_texture = if old_buffer.shm().is_some() {
          // The buffer went back after its upload: the old frame is a
          // GPU copy of the texture, which still shows it unless the
          // client committed again since.
          let copy =
            self.uploader.as_ref().zip(self.textures.get(&snapshot.id));
          let Some(copy) = copy.and_then(|(uploader, texture)| {
            uploader.duplicate(texture, self.texture_filter())
          }) else {
            continue;
          };
          copy
        } else {
          let Some(old_rgba) = old_buffer.rgba() else {
            continue;
          };
          renderer.create_texture(
            old_buffer.width,
            old_buffer.height,
            &old_rgba,
          )?
        };
        scene.animations.insert(
          snapshot.id,
          GeometryAnimation {
            started: now,
            old_geometry: *old_geometry,
            old_texture,
          },
        );
      }
      scene.animation_enabled = snapshots
        .iter()
        .map(|snapshot| (snapshot.id, self.view_matches(snapshot)))
        .filter(|(_, matches)| !matches.is_empty())
        .collect();
      scene.surface_windows = snapshots
        .iter()
        .filter(|snapshot| {
          snapshot.role == SurfaceRole::Toplevel && snapshot.buffer.is_some()
        })
        .filter_map(|snapshot| {
          snapshot.window.map(|window| (snapshot.id, window))
        })
        .collect();
      scene.drawn = snapshots
        .iter()
        .filter(|snapshot| snapshot.buffer.is_some())
        .map(|snapshot| snapshot.id)
        .collect::<HashSet<_>>();
      // Textures go once no output draws their surface.
      let drawn = &scene.drawn;
      let others = &self.scenes;
      self.textures.retain(|id, _| {
        drawn.contains(id)
          || others.values().any(|other| other.drawn.contains(id))
          || self.uploaded.contains(id)
          || self.closing_animations.contains_key(id)
          || self.minimize_animations.contains_key(id)
      });
      scene.animations.retain(|id, _| drawn.contains(id));
      let switcher = self.switcher_view.as_ref().filter(|_| switcher_here);
      let tiles = if let Some(shown) = switcher {
        super::switcher::SwitcherScene {
          compositor: &self.compositor,
          textures: &self.textures,
          frame_textures: &self.frame_textures,
          decoration: &self.decoration_config,
          frames: &frames,
          scale: view.scale,
          cursor: !self.hardware_cursors.contains(output),
        }
        .tiles(shown, now)
      } else {
        snapshots
          .iter()
          .filter_map(|snapshot| {
            let texture = self.textures.get(&snapshot.id)?;
            let geometry = snapshot.geometry;
            let content_rect = RenderRect {
              x: geometry.x,
              y: geometry.y,
              width: geometry.width,
              height: geometry.height,
            };
            // The window's geometry, which includes its frame if it has one.
            let window_rect =
              frames.get(&snapshot.id).map_or(content_rect, |frame| {
                RenderRect {
                  x: frame.rect.x,
                  y: frame.rect.y,
                  width: frame.rect.width,
                  height: frame.rect.height,
                }
              });
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
            if let Some(animation) = scene.animations.get(&snapshot.id)
              && let Some((_, old_buffer)) =
                scene.previous_surfaces.get(&snapshot.id)
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
              let progress = crate::animate::ease(
                self.animation_easing,
                f64::from(progress),
              ) as f32;
              tile.color = [snapshot.opacity * progress; 4];
            }
            // Fire's particles, drawn above the surface and its frame.
            let mut above = None;
            if let Some(animation) = self.open_animations.get(&snapshot.id) {
              above =
                apply_animation(&mut tile, animation, now, snapshot.geometry);
            }
            tiles.push(tile);
            if scale_here
              && snapshot.role == SurfaceRole::Toplevel
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
              let progress =
                scale_timing.progress(state.animation_started, now);
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
              // Scale lays out the window's geometry, its frame included,
              // and the surface goes where the frame puts it.
              let origin = state
                .animation_origin
                .get(&window)
                .copied()
                .unwrap_or(window_rect);
              let target = map_rect(
                content_rect,
                window_rect,
                interpolate_scale_rect(origin, *geometry, progress),
              );
              for tile in &mut tiles {
                tile.geometry = target;
                tile.color[3] *= opacity;
              }
            }
            if expo_here
              && let Some(window) = snapshot.window
              && let Some(state) =
                self.expo.as_ref().or(self.expo_exit.as_ref())
              && let Some(geometry) = state.geometry.get(&window)
              && let Ok(info) = self.compositor.core().window_info(window)
            {
              let (width, _) = self.compositor.workspace_grid_dimensions();
              let workspace = info.location.workspace.y as usize
                * width as usize
                + info.location.workspace.x as usize;
              let progress = expo_timing.progress(state.animation_started, now);
              let origin = state
                .geometry_origin
                .get(&window)
                .copied()
                .unwrap_or(*geometry);
              let brightness = state.brightness(
                workspace,
                now,
                Timing::circle(Duration::from_millis(
                  self.expo_config.transition_length_ms,
                )),
                self.expo_config.inactive_brightness,
              );
              let dragged = state
                .dragged_window()
                .filter(|(dragged, _, _)| *dragged == window);
              // Expo lays out the window's geometry, its frame included,
              // and the surface goes where the frame puts it.
              let mut target =
                crate::expo::interpolate_rect(origin, *geometry, progress);
              if let Some((_, x, y)) = dragged {
                target.x = (x - f64::from(target.width) / 2.0).round() as i32;
                target.y = (y - f64::from(target.height) / 2.0).round() as i32;
              }
              let target = map_rect(content_rect, window_rect, target);
              for tile in &mut tiles {
                tile.geometry = target;
                for component in &mut tile.color[..3] {
                  *component *= brightness;
                }
              }
            }
            if let Some(window) = snapshot.window
              && let Some(transition) = &scene.workspace_transition
              && let Ok(info) = self.compositor.core().window_info(window)
              && info.location.output.as_str() == output.name()
            {
              let (grid_width, _) = self.compositor.workspace_grid_dimensions();
              let workspace = info.location.workspace.y as usize
                * grid_width as usize
                + info.location.workspace.x as usize;
              let layout = workspace_transition::TransitionLayout {
                output: rect,
                grid_width,
                gap: f64::from(self.vswitch.gap),
              };
              let geometry = transition.geometry(
                snapshot.geometry,
                workspace,
                layout,
                now,
                vswitch_timing,
              );
              for tile in &mut tiles {
                tile.geometry = geometry;
              }
            }
            if let Some(window) = snapshot.window
              && let Some(viewport) = wall
              && let Ok(info) = self.compositor.core().window_info(window)
              && info.location.output.as_str() == output.name()
            {
              let (grid_width, _) = self.compositor.workspace_grid_dimensions();
              let workspace = info.location.workspace.y as usize
                * grid_width as usize
                + info.location.workspace.x as usize;
              let layout = workspace_transition::TransitionLayout {
                output: rect,
                grid_width,
                gap: self.vswipe_config.gap,
              };
              let geometry = workspace_transition::wall_geometry(
                snapshot.geometry,
                workspace,
                layout,
                viewport,
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
            // A frame follows its surface wherever the scene put it; a
            // rotated window is drawn without one, since its pieces would
            // turn about their own centres.
            if let Some(frame) = frames.get(&snapshot.id)
              && let Some(content) = tiles.last()
              && content.rotation == 0.0
              && !has_effect(content)
            {
              let mut framed = self.frame_textures.tiles(
                frame,
                content_rect,
                content,
                &self.decoration_config,
                view.scale,
              );
              framed.append(&mut tiles);
              tiles = framed;
            }
            tiles.extend(above);
            Some(tiles)
          })
          .flatten()
          .collect::<Vec<_>>()
      };
      scene.previous_surfaces = snapshots
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
      let locks = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.buffer.as_ref()?.dmabuf())
        .map(|content| content.lock.clone())
        .collect::<Vec<_>>();
      background_shown = snapshots.iter().any(|snapshot| {
        snapshot.layer == Some(shingle::ShellLayer::Background)
          && snapshot.buffer.is_some()
      });
      cursor_ids = snapshots
        .iter()
        .filter(|snapshot| snapshot.role == SurfaceRole::Cursor)
        .map(|snapshot| snapshot.id)
        .collect::<Vec<_>>();
      (tiles, locks)
    };
    if expo_here
      && let Some(state) = self.expo.as_ref().or(self.expo_exit.as_ref())
    {
      let duration = expo_timing;
      let transition = Timing::circle(Duration::from_millis(
        self.expo_config.transition_length_ms,
      ));
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
          srgb_fill(self.expo_config.background),
          border,
          2,
        ));
      }
      cells.append(&mut tiles);
      tiles = cells;
    }
    if !switcher_here {
      self.append_drop_preview(&mut tiles);
    }
    if overview_here && let Some(state) = &self.scale {
      let snapshots = self.compositor.scale_surfaces_on(output, true, true);
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
        scale_timing,
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
    if overview_here && self.scale.is_some() {
      Self::append_scale_filter_overlay(
        &*target,
        &mut self.scale_filter_texture,
        &self.scale_filter,
        &self.scale_title_filter,
        rect,
        &mut tiles,
      )?;
    }
    // Wayfire's switcher draws over closing views and the label.
    if !switcher_here {
      Self::append_closing_surfaces(
        &self.closing_animations,
        &self.textures,
        output,
        now,
        &mut tiles,
      );
      Self::append_closing_surfaces(
        &self.minimize_animations,
        &self.textures,
        output,
        now,
        &mut tiles,
      );
      Self::append_label(
        &*target,
        &mut scene.label,
        rect,
        now,
        self.wsets_label_duration,
        &mut tiles,
      )?;
    }
    let startup_black = Self::startup_frame(
      &self.animate,
      &mut scene.startup,
      background_shown,
      now,
    );
    match startup_black {
      Some(StartupFrame::Black) => tiles.clear(),
      Some(StartupFrame::Fade(alpha)) => tiles.push(RenderTile::solid(
        RenderRect {
          x: rect.x,
          y: rect.y,
          width: rect.width,
          height: rect.height,
        },
        [0.0, 0.0, 0.0, alpha],
      )),
      None => {}
    }
    // Tiles are in layout coordinates; the target is the output.
    for tile in &mut tiles {
      tile.geometry = translate(tile.geometry, -rect.x, -rect.y);
      if let Some(projection) = &mut tile.projection {
        projection.origin[0] -= rect.x as f32;
        projection.origin[1] -= rect.y as f32;
      }
    }
    let renderer = target;
    // Configured colours name the bytes Wayfire writes; the target takes
    // linear ones.
    let background = srgb_clear(if switcher_here {
      [0.0, 0.0, 0.0, 1.0]
    } else if expo_here {
      self.expo_config.background
    } else if vswipe_here {
      self.vswipe_config.background
    } else if scene.workspace_transition.is_some() {
      self.vswitch.background
    } else {
      self.background_color
    });
    // A shell client inhibits the output: it is cleared to black, as
    // Wayfire 0.9 drew it (wayfire-shell's `inhibit_output`).
    let background = if self.compositor.output_inhibited(output)
      || startup_black == Some(StartupFrame::Black)
    {
      tiles.clear();
      [0.0, 0.0, 0.0, 1.0]
    } else {
      background
    };
    if let Some((texture, rect, alpha)) = &unlock_fade
      && *alpha > 0.0
    {
      let mut tile = RenderTile::textured(*rect, texture);
      tile.color = [*alpha; 4];
      tiles.push(tile);
    }
    if present {
      renderer.set_tearing(self.compositor.allows_tearing_on(output));
      self.turns.mark("tiles");
      let presented = renderer.render(&tiles, background)?;
      self.turns.mark("draw");
      // The buffers this frame samples stay locked until the GPU is done.
      if let Some(cache) = &self.dmabuf {
        locks.extend(
          self
            .closing_animations
            .values()
            .chain(self.minimize_animations.values())
            .filter_map(|animation| animation.buffer.dmabuf())
            .map(|content| content.lock.clone()),
        );
        cache.borrow_mut().frame_submitted(locks);
      }
      if presented {
        self.compositor.frame_now(output);
      }
      super::lock::record_frame(
        &mut self.recorder,
        self.compositor.lock_state(),
        view,
        renderer,
        (&tiles, background),
        false,
      );
    } else {
      // A capture reads its frame back before it returns.
      drop(locks);
    }
    // The tiles borrow textures from `self`, so only these fields.
    let cursors = scene
      .drawn
      .iter()
      .filter(|id| cursor_ids.contains(id))
      .filter_map(|id| self.textures.get(id))
      .map(|texture| texture as *const GpuTexture)
      .collect::<Vec<_>>();
    if self.damaged.remove(output) {
      for request in self.compositor.output_damaged(output) {
        match request.source {
          CaptureSource::Output { .. } => self.captures.push(request),
          CaptureSource::Window { .. } => self.window_captures.push(request),
        }
      }
    }
    answer_captures(
      &mut self.compositor,
      &mut self.captures,
      output,
      ((rect.width, rect.height), view.transform, view.scale),
      renderer,
      (&tiles, &cursors),
      background,
    );
    Ok(())
  }

  /// Answers the window captures with each window's surfaces drawn alone
  /// over transparent black, at the toplevel's logical size, without zoom
  /// and without the cursor or anything else of the outputs; into a
  /// DMA-BUF at the scale of the window's output.
  fn answer_window_captures(&mut self, target: &mut dyn RenderTarget) {
    for request in std::mem::take(&mut self.window_captures) {
      let CaptureSource::Window { window, rect } = request.source else {
        continue;
      };
      if let Some(buffer) = &request.dmabuf {
        let scale = self
          .compositor
          .output_view(&self.compositor.output_of_window(window))
          .map_or(1.0, |view| view.scale);
        let result = self
          .draw_window_into(window, rect, target, (buffer, scale))
          .map_err(|error| error.to_string());
        self.compositor.finish_capture_into(request.id, result);
        continue;
      }
      let result = self
        .draw_window(window, rect, target)
        .map_err(|error| error.to_string());
      self.compositor.finish_capture(request.id, result);
    }
  }

  /// [`Frontend::draw_window`] into the DMA-BUF `buffer`, at `scale`.
  fn draw_window_into(
    &mut self,
    window: TileWindowId,
    rect: shingle_core::Rect,
    target: &mut dyn RenderTarget,
    (buffer, scale): (&shingle::DmabufBuffer, f64),
  ) -> Result<(), Box<dyn Error>> {
    let tiles = self.window_tiles(window, rect, &*target)?;
    target.set_zoom(1.0, 0.0, 0.0);
    target.set_scale_factor(scale);
    target.set_output_transform(TextureTransform::Normal);
    Ok(draw_into(target, &tiles, [0.0; 4], buffer, (0, 0))?)
  }

  /// RGBA rows of window `window`'s surfaces, drawn with the toplevel
  /// surface's rectangle `rect`, in layout pixels, at the origin.
  fn draw_window(
    &mut self,
    window: TileWindowId,
    rect: shingle_core::Rect,
    target: &mut dyn RenderTarget,
  ) -> Result<Vec<u8>, Box<dyn Error>> {
    let tiles = self.window_tiles(window, rect, &*target)?;
    target.set_zoom(1.0, 0.0, 0.0);
    Ok(target.capture(&tiles, [0.0; 4], (rect.width, rect.height))?)
  }

  /// The tiles of window `window`'s surfaces, with its toplevel surface's
  /// rectangle `rect` at the origin.
  fn window_tiles(
    &mut self,
    window: TileWindowId,
    rect: shingle_core::Rect,
    target: &dyn RenderTarget,
  ) -> Result<Vec<RenderTile<'_>>, Box<dyn Error>> {
    let snapshots = self.compositor.window_surfaces(window);
    if snapshots.is_empty() {
      return Err("the window has no surfaces".into());
    }
    for snapshot in &snapshots {
      self.prepare_texture(snapshot, target)?;
    }
    let tiles = snapshots
      .iter()
      .filter_map(|snapshot| {
        let texture = self.textures.get(&snapshot.id)?;
        let geometry = snapshot.geometry;
        let mut tile = RenderTile::textured(
          RenderRect {
            x: geometry.x - rect.x,
            y: geometry.y - rect.y,
            width: geometry.width,
            height: geometry.height,
          },
          texture,
        );
        tile.color = [snapshot.opacity; 4];
        if let Some(buffer) = &snapshot.buffer {
          apply_buffer(&mut tile, buffer);
        }
        Some(tile)
      })
      .collect::<Vec<_>>();
    Ok(tiles)
  }

  /// Makes sure `snapshot`'s buffer has a texture, as drawing an output
  /// does, for a window that may be on no output's screen.
  pub(super) fn prepare_texture(
    &mut self,
    snapshot: &shingle::SurfaceSnapshot,
    renderer: &dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    let Some(buffer) = &snapshot.buffer else {
      return Ok(());
    };
    if let Some(content) = buffer.dmabuf() {
      let texture = self
        .dmabuf
        .as_ref()
        .and_then(|cache| cache.borrow_mut().texture(&content.buffer));
      if let Some(texture) = texture {
        self.textures.insert(snapshot.id, texture);
      }
      return Ok(());
    }
    if let Some(content) = buffer.shm() {
      if content.is_locked() {
        self.upload_shm(snapshot.id, buffer, content, false);
      }
      return Ok(());
    }
    let missing = self.textures.get(&snapshot.id).is_none_or(|texture| {
      texture.is_imported()
        || texture.width() != buffer.width
        || texture.height() != buffer.height
    });
    if missing && let Some(rgba) = buffer.rgba() {
      let texture =
        renderer.create_texture(buffer.width, buffer.height, &rgba)?;
      self.textures.insert(snapshot.id, texture);
    }
    Ok(())
  }

  /// Continues or ends the animations of `scene`, the scene of `output`
  /// just drawn at `now`, and asks for another frame of it while any run.
  fn end_scene_animations(
    &mut self,
    output: &OutputHandle,
    scene: &mut OutputScene,
    now: Instant,
  ) {
    // Decide with the time this frame was drawn at, not the time drawing
    // finished: a slow frame must not skip an animation's final state.
    scene.animations.retain(|_, animation| {
      animation_progress(animation.started, now, self.animation_duration) < 1.0
    });
    if scene.unlock_fade.as_ref().is_some_and(|fade| {
      now.saturating_duration_since(fade.started) >= self.unlock_fade
    }) {
      scene.unlock_fade = None;
    }
    let vswitch = self.vswitch_timing();
    if scene
      .workspace_transition
      .as_ref()
      .is_some_and(|transition| {
        vswitch.linear_progress(transition.started, now) >= 1.0
      })
    {
      scene.workspace_transition = None;
    }
    if scene.label.as_ref().is_some_and(|label| {
      now.saturating_duration_since(label.started) >= self.wsets_label_duration
    }) {
      scene.label = None;
    }
    let opening = scene.drawn.iter().any(|surface| {
      self
        .open_animations
        .get(surface)
        .is_some_and(|animation| animation.running(now))
    });
    // vswipe draws every frame while its wall is up, as Wayfire's
    // `post_frame` does; the last one ends it.
    self.finish_vswipe(now);
    self.end_switcher_frame(output, now);
    if let Startup::Fading(started) = scene.startup
      && self.startup_timing().linear_progress(started, now) >= 1.0
    {
      scene.startup = Startup::Done;
    }
    if !scene.animations.is_empty()
      || matches!(scene.startup, Startup::Fading(_))
      || scene.workspace_transition.is_some()
      || self.vswipe_on(output)
      || scene.zoom_animation.is_some()
      || scene.unlock_fade.is_some()
      || opening
    {
      self.request_redraw_on(output);
    }
    scene.dirty.clear();
  }

  /// Continues or ends the animations that are not one output's: opening,
  /// closing and minimizing windows, Scale and Expo, asking for frames of
  /// the outputs they are drawn on while they run and once more after
  /// they end, so that the final state is drawn.
  fn finish_frame(&mut self, now: Instant) {
    let ended = self
      .open_animations
      .iter()
      .filter(|(_, animation)| !animation.running(now))
      .map(|(surface, _)| *surface)
      .collect::<Vec<_>>();
    for surface in ended {
      self.open_animations.remove(&surface);
      let outputs = self
        .scenes
        .iter()
        .filter(|(_, scene)| scene.drawn.contains(&surface))
        .map(|(output, _)| output.clone())
        .collect::<Vec<_>>();
      for output in outputs {
        self.request_redraw_on(&output);
      }
    }
    let closing = self
      .closing_animations
      .values()
      .chain(self.minimize_animations.values())
      .map(|animation| animation.output.clone())
      .collect::<HashSet<_>>();
    for output in closing {
      self.request_redraw_on(&output);
    }
    self.expire_closing_animations(now);
    self.expire_minimize_animations(now);
    let overview = self.overview_output();
    let drawn = self.scenes.contains_key(&overview);
    let scale = self.scale_timing();
    let expo = self.expo_timing();
    let expo_transition = Timing::circle(Duration::from_millis(
      self.expo_config.transition_length_ms,
    ));
    let overview_running = self.scale.as_ref().is_some_and(|state| {
      scale.linear_progress(state.animation_started, now) < 1.0
    }) || self.scale_exit.is_some()
      || self.expo.as_ref().is_some_and(|state| {
        expo.linear_progress(state.animation_started, now) < 1.0
          || expo_transition.linear_progress(state.selection_started, now) < 1.0
      })
      || self.expo_exit.is_some();
    if drawn {
      self.expire_scale_exit(now);
      if self.expo_exit.as_ref().is_some_and(|state| {
        expo.linear_progress(state.animation_started, now) >= 1.0
      }) {
        self.expo_exit = None;
      }
    }
    if overview_running {
      self.request_redraw_on(&overview);
    }
    if self.scale.is_none()
      && self.scale_exit.is_none()
      && self.expo.is_none()
      && self.expo_exit.is_none()
    {
      self.overview_output = None;
    }
  }

  /// Appends the "Workspace set N" label of an output at `rect` while it
  /// is shown at `now`, rasterizing it on first use.
  fn append_label<'a>(
    renderer: &dyn RenderTarget,
    label: &'a mut Option<WsetLabel>,
    rect: shingle_core::Rect,
    now: Instant,
    duration: Duration,
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    let Some(shown) = label.as_mut() else {
      return Ok(());
    };
    if now.saturating_duration_since(shown.started) >= duration {
      return Ok(());
    }
    if shown.texture.is_none()
      && let Some(raster) = rasterize_label(&shown.text, rect.width)
    {
      shown.texture = Some(renderer.create_texture(
        raster.width,
        raster.height,
        &raster.rgba,
      )?);
    }
    if let Some(texture) = &shown.texture {
      tiles.push(RenderTile::textured(
        label_rect(rect, (texture.width(), texture.height())),
        texture,
      ));
    }
    Ok(())
  }
}

/// Answers the output `captures` of `output` requested since its last
/// frame with the frame just built from `tiles`, which are relative to the
/// output: drawn once more offscreen and cut to each capture's region. An
/// untransformed output at a `scale` other than 1 is drawn in its device
/// pixels, which the compositor cuts the buffer from; others at the
/// output's logical `size`, which the compositor scales into the client's
/// buffer. Captures without the cursor are drawn without the tiles of the
/// `cursors` textures. Captures of other outputs wait for their own
/// frames.
pub(super) fn answer_captures(
  compositor: &mut shingle::Compositor,
  captures: &mut Vec<CaptureRequest>,
  output: &OutputHandle,
  (size, transform, scale): (
    (u32, u32),
    wayland_server::protocol::wl_output::Transform,
    f64,
  ),
  target: &mut dyn RenderTarget,
  (tiles, cursors): (&[RenderTile<'_>], &[*const GpuTexture]),
  background: [f64; 4],
) {
  let (mine, others) = std::mem::take(captures)
    .into_iter()
    .partition::<Vec<_>, _>(|request| {
      matches!(&request.source,
        CaptureSource::Output { output: captured, .. } if captured == output)
    });
  *captures = others;
  if mine.is_empty() {
    return;
  }
  let mut with = None;
  let mut without = None;
  let device = compositor.physical_size_of(output).filter(|_| {
    scale != 1.0
      && transform == wayland_server::protocol::wl_output::Transform::Normal
  });
  let mut device_with = None;
  let mut device_without = None;
  for request in mine {
    let CaptureSource::Output { region, .. } = request.source else {
      continue;
    };
    if let Some(buffer) = &request.dmabuf {
      let tiles = if request.cursor {
        tiles.to_vec()
      } else {
        without_cursor(tiles, cursors)
      };
      let whole = region == shingle_core::Rect::new(0, 0, size.0, size.1);
      let result = if whole
        || transform == wayland_server::protocol::wl_output::Transform::Normal
      {
        draw_into(target, &tiles, background, buffer, (region.x, region.y))
      } else {
        Err("a region of a turned output is copied into shm only".into())
      };
      compositor.finish_capture_into(request.id, result);
      continue;
    }
    if let Some(device) = device {
      let cached = if request.cursor || cursors.is_empty() {
        &mut device_with
      } else {
        &mut device_without
      };
      let frame = cached.get_or_insert_with(|| {
        let tiles = if request.cursor {
          tiles.to_vec()
        } else {
          without_cursor(tiles, cursors)
        };
        target
          .capture_scaled(&tiles, background, device, scale)
          .map(|drawn| drawn.map_err(|error| error.to_string()))
      });
      if let Some(frame) = frame {
        let result = frame.clone().map(|rgba| (rgba, device));
        compositor.finish_capture_device(request.id, result);
        continue;
      }
    }
    let frame = if request.cursor || cursors.is_empty() {
      with.get_or_insert_with(|| {
        target
          .capture(tiles, background, size)
          .map_err(|error| error.to_string())
      })
    } else {
      without.get_or_insert_with(|| {
        let tiles = without_cursor(tiles, cursors);
        target
          .capture(&tiles, background, size)
          .map_err(|error| error.to_string())
      })
    };
    let result = frame.as_ref().map_err(Clone::clone).map(|rgba| {
      crop_rgba(
        rgba,
        size,
        region.x,
        region.y,
        (region.width, region.height),
      )
    });
    compositor.finish_capture(request.id, result);
  }
}

/// Draws `tiles`, shifted by minus `origin` in logical pixels, over
/// `background` into the capture client's DMA-BUF `buffer` with
/// `target`'s scale and output transform.
fn draw_into(
  target: &mut dyn RenderTarget,
  tiles: &[RenderTile<'_>],
  background: [f64; 4],
  buffer: &shingle::DmabufBuffer,
  origin: (i32, i32),
) -> Result<(), String> {
  let descriptor = crate::dmabuf::descriptor(buffer)?;
  let mut tiles = tiles.to_vec();
  if origin != (0, 0) {
    for tile in &mut tiles {
      tile.geometry =
        super::outputs::translate(tile.geometry, -origin.0, -origin.1);
      if let Some(projection) = &mut tile.projection {
        projection.origin[0] -= origin.0 as f32;
        projection.origin[1] -= origin.1 as f32;
      }
    }
  }
  target
    .capture_into(&tiles, background, &descriptor)
    .map_err(|error| error.to_string())
}

/// `tiles` without those that draw one of the `cursors` textures.
fn without_cursor<'a>(
  tiles: &[RenderTile<'a>],
  cursors: &[*const GpuTexture],
) -> Vec<RenderTile<'a>> {
  tiles
    .iter()
    .filter(|tile| {
      tile.texture.is_none_or(|texture| {
        !cursors.contains(&(texture as *const GpuTexture))
      })
    })
    .cloned()
    .collect()
}

/// The `width` by `height` rectangle at (`x`, `y`) of `rgba`, a frame of
/// `size` pixels, as tightly packed RGBA8 rows. Pixels outside the frame
/// are transparent black.
pub(super) fn crop_rgba(
  rgba: &[u8],
  size: (u32, u32),
  x: i32,
  y: i32,
  (width, height): (u32, u32),
) -> Vec<u8> {
  let mut out = vec![0; width as usize * height as usize * 4];
  let frame_width = i64::from(size.0);
  let frame_height = i64::from(size.1);
  for row in 0..i64::from(height) {
    let source_y = i64::from(y) + row;
    if !(0..frame_height).contains(&source_y) {
      continue;
    }
    let first = i64::from(x).max(0);
    let last = (i64::from(x) + i64::from(width)).min(frame_width);
    if first >= last {
      continue;
    }
    let source = ((source_y * frame_width + first) * 4) as usize;
    let length = ((last - first) * 4) as usize;
    let destination =
      ((row * i64::from(width) + first - i64::from(x)) * 4) as usize;
    out[destination..destination + length]
      .copy_from_slice(&rgba[source..source + length]);
  }
  out
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

#[cfg(test)]
mod tests {
  use super::crop_rgba;

  /// A 3x2 frame whose pixel (x, y) is [x, y, 0, 255].
  fn frame() -> Vec<u8> {
    (0..2u8)
      .flat_map(|y| (0..3u8).flat_map(move |x| [x, y, 0, 255]))
      .collect()
  }

  #[test]
  fn a_crop_inside_the_frame_copies_its_rows() {
    assert_eq!(
      crop_rgba(&frame(), (3, 2), 1, 0, (2, 2)),
      [1, 0, 0, 255, 2, 0, 0, 255, 1, 1, 0, 255, 2, 1, 0, 255]
    );
    assert_eq!(crop_rgba(&frame(), (3, 2), 0, 0, (3, 2)), frame());
  }

  #[test]
  fn pixels_outside_the_frame_are_transparent() {
    assert_eq!(
      crop_rgba(&frame(), (3, 2), -1, 1, (2, 2)),
      [0, 0, 0, 0, 0, 1, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(crop_rgba(&frame(), (3, 2), 5, 5, (1, 1)), [0; 4]);
  }
}
