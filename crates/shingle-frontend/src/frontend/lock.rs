//! The scene while the session lock hides every client
//! ([`shingle::Compositor::lock_hides_clients`]): from the moment a lock
//! client asks for the lock until it unlocks, also after it crashed, an
//! output shows only the lock's surface for it, once that has committed a
//! full opaque frame, over black. Nothing else the frontend draws is
//! there: no window, layer, frame, cursor, overview, switcher, slide,
//! label or closing animation (docs/design/lock-screen.md). Wayfire
//! instead keeps drawing the desktop until every lock surface is there
//! or a second has passed (wayfire:plugins/protocols/session-lock.cpp,
//! `lock_timer`), and then inhibits the outputs, which draw black under
//! the lock layer.

use std::{error::Error, time::Instant};

use shingle::{LockState, SurfaceRole};
use shingle_render::{RenderRect, RenderTile};

use super::{
  Frontend,
  outputs::OutputScene,
  scene::{apply_buffer, render_output_transform},
};
use crate::{
  recorder::{FrameInfo, FrameRecorder},
  render_target::RenderTarget,
};

/// What a locked output is cleared to.
pub(super) const LOCKED_BACKGROUND: [f64; 4] = [0.0, 0.0, 0.0, 1.0];

impl Frontend {
  /// Draws output `view`'s lock scene at `now` and, with `present`,
  /// presents it into `target`; then answers its captures from it.
  pub(super) fn draw_lock_scene(
    &mut self,
    view: &shingle::OutputView,
    scene: &mut OutputScene,
    target: &mut dyn RenderTarget,
    _now: Instant,
    present: bool,
  ) -> Result<(), Box<dyn Error>> {
    let output = &view.handle;
    let rect = view.rect;
    target.set_scale_factor(view.scale);
    target.set_output_transform(render_output_transform(view.transform));
    target.set_zoom(
      scene.zoom_factor,
      scene.zoom_origin.0,
      scene.zoom_origin.1,
    );
    target.set_gamma_ramp(self.compositor.gamma_ramp_of(output))?;
    target.set_inversion(self.inverted, self.invert_preserve_hue);
    let snapshots = self.compositor.visible_surfaces_on(output);
    for snapshot in &snapshots {
      self.prepare_texture(snapshot, &*target)?;
      // Pixels from the CPU path are uploaded again after a commit.
      if let Some(buffer) = &snapshot.buffer
        && buffer.dmabuf().is_none()
        && buffer.shm().is_none()
        && scene.dirty.contains(&snapshot.id)
        && let Some(rgba) = buffer.rgba()
        && let Some(texture) = self.textures.get(&snapshot.id)
        && (texture.width(), texture.height()) == (buffer.width, buffer.height)
        && !texture.is_imported()
      {
        target.update_texture(texture, &rgba)?;
      }
    }
    let lock_surface = snapshots
      .iter()
      .any(|snapshot| snapshot.role == SurfaceRole::SessionLock);
    // Nothing drawn before the lock is animated from after it.
    scene.previous_surfaces.clear();
    scene.animations.clear();
    scene.animation_enabled.clear();
    scene.surface_windows.clear();
    scene.drawn = snapshots
      .iter()
      .filter(|snapshot| snapshot.buffer.is_some())
      .map(|snapshot| snapshot.id)
      .collect();
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
        if let Some(buffer) = &snapshot.buffer {
          apply_buffer(&mut tile, buffer);
        }
        Some(tile)
      })
      .collect::<Vec<_>>();
    // The lock surface's frame, should the lock end with an unlock.
    if let Some((texture, rect)) = snapshots
      .iter()
      .filter(|snapshot| {
        snapshot.role == SurfaceRole::SessionLock && snapshot.buffer.is_some()
      })
      .find_map(|snapshot| {
        let geometry = snapshot.geometry;
        Some((
          self.textures.get(&snapshot.id)?.clone(),
          RenderRect {
            x: geometry.x - rect.x,
            y: geometry.y - rect.y,
            width: geometry.width,
            height: geometry.height,
          },
        ))
      })
    {
      scene.last_lock_frame = Some((texture, rect));
    }
    scene.unlocks_seen = self.compositor.unlock_count();
    scene.unlock_fade = None;
    let locks = snapshots
      .iter()
      .filter_map(|snapshot| snapshot.buffer.as_ref()?.dmabuf())
      .map(|content| content.lock.clone())
      .collect::<Vec<_>>();
    if present {
      target.set_tearing(false);
      let presented = target.render(&tiles, LOCKED_BACKGROUND)?;
      if let Some(cache) = &self.dmabuf {
        cache.borrow_mut().frame_submitted(locks);
      }
      if presented {
        self.compositor.frame_now(output);
      }
      record_frame(
        &mut self.recorder,
        self.compositor.lock_state(),
        view,
        target,
        (&tiles, LOCKED_BACKGROUND),
        lock_surface,
      );
    } else {
      drop(locks);
    }
    if self.damaged.remove(output) {
      for request in self.compositor.output_damaged(output) {
        match request.source {
          shingle::CaptureSource::Output { .. } => self.captures.push(request),
          shingle::CaptureSource::Window { .. } => {
            self.window_captures.push(request)
          }
        }
      }
    }
    super::scene::answer_captures(
      &mut self.compositor,
      &mut self.captures,
      output,
      ((rect.width, rect.height), view.transform, view.scale),
      target,
      (&tiles, &[]),
      LOCKED_BACKGROUND,
    );
    Ok(())
  }
}

/// Draws the frame of `view` just presented, `tiles` over `background`,
/// again and writes it with `recorder`, if there is one, noting `lock`
/// and whether a lock surface was drawn. Failures are logged once per
/// frame and do not stop drawing.
pub(super) fn record_frame(
  recorder: &mut Option<FrameRecorder>,
  lock: LockState,
  view: &shingle::OutputView,
  target: &mut dyn RenderTarget,
  (tiles, background): (&[RenderTile<'_>], [f64; 4]),
  lock_surface: bool,
) {
  let Some(recorder) = recorder else {
    return;
  };
  let size = (view.rect.width, view.rect.height);
  let info = FrameInfo {
    output: view.handle.name(),
    lock: format!("{lock:?}").to_lowercase(),
    lock_surface,
    time: shingle::monotonic_time(),
  };
  let result = target
    .capture(tiles, background, size)
    .map_err(|error| error.to_string())
    .and_then(|rgba| {
      recorder
        .record(&info, size, &rgba)
        .map_err(|error| error.to_string())
    });
  if let Err(error) = result {
    eprintln!(
      "shingle: recording a frame of {} failed: {error}",
      info.output
    );
  }
}
