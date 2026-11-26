//! Open, close, minimize and Scale exit animations.

use std::time::{Duration, Instant};

use shingle::{BufferSnapshot, SurfaceRole, SurfaceSnapshot};
use shingle_config::ViewProperties;
use shingle_core::WindowId as TileWindowId;
use shingle_render::{GpuTexture, RenderRect, RenderTile, animation_progress};

use super::Frontend;

pub(super) struct GeometryAnimation {
  pub(super) started: Instant,
  pub(super) old_geometry: RenderRect,
  pub(super) old_texture: GpuTexture,
}

pub(super) struct ClosingAnimation {
  pub(super) window: TileWindowId,
  /// The output it is drawn on, where its surface was drawn last.
  pub(super) output: shingle::OutputHandle,
  pub(super) started: Instant,
  pub(super) geometry: RenderRect,
  pub(super) buffer: BufferSnapshot,
}

impl Frontend {
  pub(super) fn animation_matches(&self, snapshot: &SurfaceSnapshot) -> bool {
    // A Wayland client's drag icon, which has no X class, is no view in
    // Wayfire and never animates.
    if snapshot.role == SurfaceRole::DragIcon && snapshot.app_id.is_none() {
      return false;
    }
    let view_type = match snapshot.role {
      SurfaceRole::Toplevel => "toplevel",
      SurfaceRole::Popup => "popup",
      SurfaceRole::LayerShell => "overlay",
      SurfaceRole::Cursor => "cursor",
      // Wayfire's name for override-redirect X views, which its drag
      // icons are too.
      SurfaceRole::Unmanaged | SurfaceRole::DragIcon => "x-or",
      _ => "other",
    };
    let mut view = ViewProperties {
      app_id: snapshot.app_id.as_deref().unwrap_or(""),
      title: snapshot.title.as_deref().unwrap_or(""),
      view_type,
      activated: self.compositor.focused_window() == snapshot.window,
      focusable: snapshot.role == SurfaceRole::Toplevel,
      mapped: snapshot.buffer.is_some(),
      ..ViewProperties::default()
    };
    if let Some(window) = snapshot.window
      && let Ok(info) = self.compositor.core().window_info(window)
    {
      view.fullscreen = info.state.fullscreen;
      view.minimized = info.state.minimized;
      view.maximized = info.state.maximized;
      view.floating = info.state.floating.is_some() && !info.state.maximized;
      view.tiled_left = !view.floating;
      view.tiled_right = !view.floating;
      view.tiled_top = !view.floating;
      view.tiled_bottom = !view.floating;
    }
    self.animate.enabled_for.matches_view(&view)
  }

  pub(super) fn minimize_window_changed(
    &mut self,
    window: TileWindowId,
    minimized: bool,
  ) {
    if !minimized {
      self
        .minimize_animations
        .retain(|_, animation| animation.window != window);
      return;
    }
    if self.scale_config.include_minimized
      && self
        .scale
        .as_ref()
        .is_some_and(|state| state.windows.contains(&window))
    {
      self
        .minimize_animations
        .retain(|_, animation| animation.window != window);
      return;
    }
    if self.animate.minimize_animation == "none" {
      return;
    }
    let Some(surface) = self.window_surface(window) else {
      return;
    };
    if !self.animation_enabled_for(surface)
      || self.closing_animations.contains_key(&surface)
      || !self.textures.contains_key(&surface)
    {
      return;
    }
    let Some((output, (geometry, buffer))) = self.previous_of(surface) else {
      return;
    };
    self.minimize_animations.insert(
      surface,
      ClosingAnimation {
        window,
        output,
        started: Instant::now(),
        geometry,
        buffer,
      },
    );
  }

  pub(super) fn expire_closing_animations(&mut self, now: Instant) {
    let completed = self
      .closing_animations
      .iter()
      .filter(|(_, animation)| {
        animation_progress(
          animation.started,
          now,
          Duration::from_millis(self.animate.duration_ms),
        ) >= 1.0
      })
      .map(|(surface, _)| *surface)
      .collect::<Vec<_>>();
    for surface in completed {
      self.closing_animations.remove(&surface);
      self.textures.remove(&surface);
    }
  }

  pub(super) fn expire_minimize_animations(&mut self, now: Instant) {
    self.minimize_animations.retain(|_, animation| {
      animation_progress(
        animation.started,
        now,
        Duration::from_millis(self.animate.duration_ms),
      ) < 1.0
    });
  }

  /// Releases the retained exit snapshots after the final transition frame.
  pub(super) fn expire_scale_exit(&mut self, now: Instant) {
    if self.scale_exit.as_ref().is_some_and(|state| {
      animation_progress(
        state.animation_started,
        now,
        Duration::from_millis(self.scale_config.duration_ms),
      ) >= 1.0
    }) {
      self.scale_exit = None;
    }
  }
}

pub(super) fn apply_opening_animation(
  tile: &mut RenderTile<'_>,
  animation: &str,
  progress: f32,
) {
  let progress = progress.clamp(0.0, 1.0);
  tile.color[3] *= progress;
  if animation != "zoom" {
    return;
  }
  let original = tile.geometry;
  let scale = 1.0 / 3.0 + 2.0 / 3.0 * f64::from(progress);
  tile.geometry.width = (f64::from(original.width) * scale).round() as u32;
  tile.geometry.height = (f64::from(original.height) * scale).round() as u32;
  tile.geometry.x = original.x.saturating_add(
    i32::try_from((original.width - tile.geometry.width) / 2)
      .unwrap_or(i32::MAX),
  );
  tile.geometry.y = original.y.saturating_add(
    i32::try_from((original.height - tile.geometry.height) / 2)
      .unwrap_or(i32::MAX),
  );
}

#[cfg(test)]
mod tests {
  use shingle_render::{RenderRect, RenderTile};

  use super::*;

  #[test]
  fn opening_animation_fades_and_zooms_from_center() {
    let geometry = RenderRect {
      x: 100,
      y: 50,
      width: 200,
      height: 100,
    };
    let mut fade = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut fade, "fade", 0.25);
    assert_eq!(fade.geometry, geometry);
    assert_eq!(fade.color[3], 0.25);

    let mut zoom = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut zoom, "zoom", 0.5);
    assert_eq!(
      zoom.geometry,
      RenderRect {
        x: 133,
        y: 66,
        width: 133,
        height: 67,
      }
    );
    assert_eq!(zoom.color[3], 0.5);

    let mut start = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut start, "zoom", 0.0);
    assert_eq!(start.geometry.width, 67);
    assert_eq!(start.geometry.height, 33);
    assert_eq!(start.color[3], 0.0);

    let mut complete = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut complete, "zoom", 1.0);
    assert_eq!(complete.geometry, geometry);
    assert_eq!(complete.color[3], 1.0);
  }
}
