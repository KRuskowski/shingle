//! What the frontend keeps per output (docs/design/multi-output.md,
//! section 7): zoom, the workspace slide, the "Workspace set N" label, the
//! surfaces drawn last and their geometry animations, and which outputs
//! need a frame. Outputs created over IPC are paced here and drawn only
//! for screencopy.

use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use shingle::{
  BufferSnapshot, CaptureRequest, CaptureSource, OutputHandle, OutputKind,
  SurfaceId,
};
use shingle_core::{Rect, WindowId as TileWindowId};
use shingle_render::{GpuTexture, RenderRect};

use super::{Frontend, animation::GeometryAnimation};
use crate::workspace_transition::WorkspaceTransition;

/// How often outputs created over IPC get frames: 60 Hz, as decision 8 of
/// the design chose.
pub(super) const VIRTUAL_FRAME: Duration = Duration::from_nanos(16_666_667);

/// The font size of the workspace set label, as Wayfire's wsets plugin
/// draws it.
const LABEL_FONT_SIZE: u32 = 32;

/// Where the label's top-left corner is, from the output's.
const LABEL_OFFSET: i32 = 10;

/// The label's background and text colours, Wayfire's.
const LABEL_BACKGROUND: [f32; 4] = [0.1, 0.1, 0.1, 0.9];
const LABEL_TEXT: [f32; 4] = [0.9, 0.9, 0.9, 1.0];

/// Wayfire's "Workspace set N" label on one output.
pub(super) struct WsetLabel {
  /// What it says.
  pub(super) text: String,
  /// When it was shown; it goes after `wsets.label_duration`.
  pub(super) started: Instant,
  /// Its texture, once drawn.
  pub(super) texture: Option<GpuTexture>,
}

/// The frontend's state for one output.
pub(super) struct OutputScene {
  /// The zoom factor drawn now.
  pub(super) zoom_factor: f32,
  /// The zoom factor the wheel asked for.
  pub(super) zoom_target: f32,
  /// When the zoom started moving towards the target, and from where.
  pub(super) zoom_animation: Option<(Instant, f32)>,
  /// The point the zoom keeps in place, relative to the output.
  pub(super) zoom_origin: (f32, f32),
  /// The workspace slide running on this output.
  pub(super) workspace_transition: Option<WorkspaceTransition>,
  /// The workspace set label, while it is shown.
  pub(super) label: Option<WsetLabel>,
  /// The geometry and buffer of each surface drawn in the last frame.
  pub(super) previous_surfaces:
    HashMap<SurfaceId, (RenderRect, BufferSnapshot)>,
  /// Geometry animations of surfaces drawn here.
  pub(super) animations: HashMap<SurfaceId, GeometryAnimation>,
  /// The surfaces with a buffer drawn in the last frame.
  pub(super) drawn: HashSet<SurfaceId>,
  /// The surfaces of the last frame that `animate.enabled_for` matches.
  pub(super) animation_enabled: HashSet<SurfaceId>,
  /// The toplevels of the last frame and their windows.
  pub(super) surface_windows: HashMap<SurfaceId, TileWindowId>,
  /// Surfaces committed since this output's last frame.
  pub(super) dirty: HashSet<SurfaceId>,
}

impl OutputScene {
  /// A scene with nothing drawn yet, zoom off and no slide.
  pub(super) fn new() -> Self {
    Self {
      zoom_factor: 1.0,
      zoom_target: 1.0,
      zoom_animation: None,
      zoom_origin: (0.0, 0.0),
      workspace_transition: None,
      label: None,
      previous_surfaces: HashMap::new(),
      animations: HashMap::new(),
      drawn: HashSet::new(),
      animation_enabled: HashSet::new(),
      surface_windows: HashMap::new(),
      dirty: HashSet::new(),
    }
  }

  /// Forgets the surfaces drawn and their geometry animations, so that the
  /// next frame starts none.
  pub(super) fn forget_surfaces(&mut self) {
    self.previous_surfaces.clear();
    self.animations.clear();
  }

  /// Forgets everything known about `surface`, which is gone.
  pub(super) fn surface_destroyed(&mut self, surface: SurfaceId) {
    self.previous_surfaces.remove(&surface);
    self.animations.remove(&surface);
    self.drawn.remove(&surface);
    self.animation_enabled.remove(&surface);
    self.surface_windows.remove(&surface);
    self.dirty.remove(&surface);
  }

  /// When the label goes, if one is shown.
  fn label_deadline(&self, duration: Duration) -> Option<Instant> {
    self.label.as_ref().map(|label| label.started + duration)
  }
}

/// The text of the label for workspace set `wset`.
pub(super) fn label_text(wset: u32) -> String {
  format!("Workspace set {wset}")
}

/// The rectangle the label's `size` texture is drawn at on an output at
/// `output`, in layout coordinates.
pub(super) fn label_rect(output: Rect, size: (u32, u32)) -> RenderRect {
  RenderRect {
    x: output.x.saturating_add(LABEL_OFFSET),
    y: output.y.saturating_add(LABEL_OFFSET),
    width: size.0,
    height: size.1,
  }
}

/// Rasterizes `text` for the label, at most `width` pixels wide.
pub(super) fn rasterize_label(
  text: &str,
  width: u32,
) -> Option<crate::scale_title::RasterTitle> {
  crate::scale_title::rasterize_title(
    text,
    LABEL_FONT_SIZE,
    width.saturating_sub(2 * LABEL_OFFSET as u32),
    LABEL_BACKGROUND,
    LABEL_TEXT,
  )
}

/// Whether `first` and `second` share a pixel.
pub(super) fn overlaps(first: Rect, second: Rect) -> bool {
  let right = |rect: Rect| i64::from(rect.x) + i64::from(rect.width);
  let bottom = |rect: Rect| i64::from(rect.y) + i64::from(rect.height);
  let empty = |rect: Rect| rect.width == 0 || rect.height == 0;
  !empty(first)
    && !empty(second)
    && i64::from(first.x) < right(second)
    && i64::from(second.x) < right(first)
    && i64::from(first.y) < bottom(second)
    && i64::from(second.y) < bottom(first)
}

/// `tile` moved by (`dx`, `dy`), saturating at the `i32` range.
pub(super) fn translate(rect: RenderRect, dx: i32, dy: i32) -> RenderRect {
  RenderRect {
    x: rect.x.saturating_add(dx),
    y: rect.y.saturating_add(dy),
    ..rect
  }
}

/// When the next frame of the outputs created over IPC is due, given the
/// last deadline `next` and the time `now`: one interval later, or one
/// interval after `now` if the deadline fell behind by more than that.
pub(super) fn next_virtual_frame(next: Instant, now: Instant) -> Instant {
  let after = next + VIRTUAL_FRAME;
  if after <= now {
    now + VIRTUAL_FRAME
  } else {
    after
  }
}

impl Frontend {
  /// The scene of `output`, made on first use.
  pub(super) fn scene_mut(
    &mut self,
    output: &OutputHandle,
  ) -> &mut OutputScene {
    self
      .scenes
      .entry(output.clone())
      .or_insert_with(OutputScene::new)
  }

  /// The output under layout point (`x`, `y`), else the active output.
  pub(super) fn output_under(&self, x: f64, y: f64) -> OutputHandle {
    self
      .compositor
      .output_at(x, y)
      .unwrap_or_else(|| self.compositor.active_output())
  }

  /// `output`'s rectangle in the layout; empty for an output that is gone.
  pub(super) fn output_rect(&self, output: &OutputHandle) -> Rect {
    self
      .compositor
      .output_view(output)
      .map_or(Rect::new(0, 0, 0, 0), |view| view.rect)
  }

  /// The output Scale and Expo run on: the one they started on, else the
  /// active one.
  pub(super) fn overview_output(&self) -> OutputHandle {
    self
      .overview_output
      .clone()
      .unwrap_or_else(|| self.compositor.active_output())
  }

  /// The zoom factor and output-relative origin drawn on `output`.
  pub(super) fn zoom_of(&self, output: &OutputHandle) -> (f32, (f32, f32)) {
    self.scenes.get(output).map_or((1.0, (0.0, 0.0)), |scene| {
      (scene.zoom_factor, scene.zoom_origin)
    })
  }

  /// Asks for a frame of every output the host draws.
  pub(super) fn request_redraw(&mut self) {
    self.redraw_all = true;
    self.damaged.extend(
      self
        .compositor
        .outputs()
        .into_iter()
        .map(|view| view.handle),
    );
  }

  /// Asks for a frame of `output`, which changed. A capture of an output
  /// created over IPC is drawn with any output's frame, so it asks for all
  /// of them.
  pub(super) fn request_redraw_on(&mut self, output: &OutputHandle) {
    self.damaged.insert(output.clone());
    self.redraw_for_capture(output);
  }

  /// Asks for a frame of `output` for a capture alone: unlike
  /// [`Frontend::request_redraw_on`], it does not damage the output, so
  /// that captures waiting for damage keep waiting.
  pub(super) fn redraw_for_capture(&mut self, output: &OutputHandle) {
    let virtual_output = self
      .compositor
      .output_view(output)
      .is_some_and(|view| view.kind == OutputKind::Virtual);
    if virtual_output {
      self.redraw_all = true;
    } else {
      self.redraw_outputs.insert(output.clone());
    }
  }

  /// Asks for a frame of every output that `rect`, in layout coordinates,
  /// overlaps, as damage of a surface there.
  pub(super) fn damage(&mut self, rect: Rect) {
    let outputs = self
      .compositor
      .outputs()
      .into_iter()
      .filter(|view| view.kind != OutputKind::Virtual)
      .filter(|view| overlaps(view.rect, rect))
      .map(|view| view.handle)
      .collect::<Vec<_>>();
    self.damaged.extend(outputs.iter().cloned());
    self.redraw_outputs.extend(outputs);
  }

  /// Takes a capture the compositor asked for: an output's is answered by
  /// that output's next frame, a window's by the next frame of the output
  /// it is on.
  pub(super) fn add_capture(&mut self, request: CaptureRequest) {
    match &request.source {
      CaptureSource::Output { output, .. } => {
        let output = output.clone();
        self.redraw_for_capture(&output);
        self.captures.push(request);
      }
      CaptureSource::Window { window, .. } => {
        let output = self.compositor.output_of_window(*window);
        self.redraw_for_capture(&output);
        self.window_captures.push(request);
      }
    }
  }

  /// Passes pending redraw requests on to the host, one per output it may
  /// draw: every output but those created over IPC.
  pub(super) fn flush(&mut self, host: &mut dyn crate::host::Host) {
    self.track_cursor();
    let all = std::mem::take(&mut self.redraw_all);
    let some = std::mem::take(&mut self.redraw_outputs);
    if !all && some.is_empty() {
      return;
    }
    for view in self.compositor.outputs() {
      if view.kind != OutputKind::Virtual
        && !view.dark
        && (all || some.contains(&view.handle))
      {
        host.request_redraw(&view.handle);
      }
    }
  }

  /// Drops the scenes of outputs that are gone, and ends Scale and Expo
  /// if their output went.
  pub(super) fn prune_scenes(&mut self) {
    let present = self
      .compositor
      .outputs()
      .into_iter()
      .map(|view| view.handle)
      .collect::<HashSet<_>>();
    self.scenes.retain(|output, _| present.contains(output));
    if self
      .switcher_view
      .as_ref()
      .is_some_and(|shown| !present.contains(&shown.output))
    {
      self.cancel_switcher();
    }
    self
      .closing_animations
      .retain(|_, animation| present.contains(&animation.output));
    self
      .minimize_animations
      .retain(|_, animation| present.contains(&animation.output));
    if self
      .overview_output
      .as_ref()
      .is_some_and(|output| !present.contains(output))
    {
      self.end_overviews();
    }
  }

  /// Ends Scale and Expo at once, without their exit animations, as when
  /// the output they run on goes or another output becomes active.
  pub(super) fn end_overviews(&mut self) {
    self.cancel_scale();
    self.scale_exit = None;
    self.clear_expo();
    self.overview_output = None;
  }

  /// The output `surface` was drawn on last, with its geometry and buffer
  /// there.
  pub(super) fn previous_of(
    &self,
    surface: SurfaceId,
  ) -> Option<(OutputHandle, (RenderRect, BufferSnapshot))> {
    let mut scenes = self.scenes.iter().collect::<Vec<_>>();
    scenes.sort_by(|first, second| first.0.cmp(second.0));
    scenes.into_iter().find_map(|(output, scene)| {
      let previous = scene.previous_surfaces.get(&surface)?;
      Some((output.clone(), previous.clone()))
    })
  }

  /// The window of toplevel `surface`, as the last frame that drew it knew.
  pub(super) fn surface_window(
    &self,
    surface: SurfaceId,
  ) -> Option<TileWindowId> {
    self
      .scenes
      .values()
      .find_map(|scene| scene.surface_windows.get(&surface).copied())
  }

  /// The toplevel surface of `window`, as the last frame that drew it
  /// knew.
  pub(super) fn window_surface(
    &self,
    window: TileWindowId,
  ) -> Option<SurfaceId> {
    self.scenes.values().find_map(|scene| {
      scene
        .surface_windows
        .iter()
        .find_map(|(surface, candidate)| {
          (*candidate == window).then_some(*surface)
        })
    })
  }

  /// Whether `animate.enabled_for` matched `surface` when it was drawn.
  pub(super) fn animation_enabled_for(&self, surface: SurfaceId) -> bool {
    self
      .scenes
      .values()
      .any(|scene| scene.animation_enabled.contains(&surface))
  }

  /// Marks `surface` committed for every output's next frame.
  pub(super) fn mark_dirty(&mut self, surface: SurfaceId) {
    for scene in self.scenes.values_mut() {
      scene.dirty.insert(surface);
    }
  }

  /// Shows the label of workspace set `wset` on `output`.
  pub(super) fn show_wset_label(&mut self, output: &OutputHandle, wset: u32) {
    let scene = self.scene_mut(output);
    scene.label = Some(WsetLabel {
      text: label_text(wset),
      started: Instant::now(),
      texture: None,
    });
    self.request_redraw_on(output);
  }

  /// Asks for a frame of each output whose label has run its time, so
  /// that the frame without it is drawn.
  pub(super) fn expire_labels(&mut self, now: Instant) {
    let duration = self.wsets_label_duration;
    let expired = self
      .scenes
      .iter()
      .filter(|(_, scene)| {
        scene.label_deadline(duration).is_some_and(|end| end <= now)
      })
      .map(|(output, _)| output.clone())
      .collect::<Vec<_>>();
    for output in expired {
      self.request_redraw_on(&output);
    }
  }

  /// When the next label goes.
  pub(super) fn label_deadline(&self) -> Option<Instant> {
    self
      .scenes
      .values()
      .filter_map(|scene| scene.label_deadline(self.wsets_label_duration))
      .min()
  }

  /// Gives the outputs created over IPC their frames, at 60 Hz, as no host
  /// presents them. They are drawn only when a screencopy asks.
  pub(super) fn pace_virtual_outputs(&mut self, now: Instant) {
    let outputs = self
      .compositor
      .outputs()
      .into_iter()
      .filter(|view| view.kind == OutputKind::Virtual && !view.dark)
      .map(|view| view.handle)
      .collect::<Vec<_>>();
    if outputs.is_empty() {
      self.virtual_frame = None;
      return;
    }
    let due = *self.virtual_frame.get_or_insert(now + VIRTUAL_FRAME);
    if now < due {
      return;
    }
    for output in &outputs {
      self.compositor.frame_now(output);
    }
    self.virtual_frame = Some(next_virtual_frame(due, now));
  }

  /// Answers at once the captures of outputs that will not be drawn: gone,
  /// dark or turned off.
  pub(super) fn fail_undrawable_captures(&mut self) {
    let captures = std::mem::take(&mut self.captures);
    for capture in captures {
      let CaptureSource::Output { output, .. } = &capture.source else {
        continue;
      };
      let drawable = self
        .compositor
        .output_view(output)
        .is_some_and(|view| !view.dark)
        && self.compositor.output_powered(output);
      if drawable {
        self.captures.push(capture);
      } else {
        let error = format!("{} is not drawn", output.name());
        self.compositor.finish_capture(capture.id, Err(error));
      }
    }
    // A window on an output that is not drawn is drawn with the next
    // frame of any output.
    if !self.window_captures.is_empty()
      && let Some(view) = self.compositor.outputs().into_iter().find(|view| {
        view.kind != OutputKind::Virtual
          && !view.dark
          && self.compositor.output_powered(&view.handle)
      })
    {
      self.redraw_for_capture(&view.handle);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rectangles_overlap_only_when_they_share_a_pixel() {
    let output = Rect::new(0, 0, 100, 50);
    assert!(overlaps(output, Rect::new(99, 49, 10, 10)));
    assert!(!overlaps(output, Rect::new(100, 0, 10, 10)), "touching");
    assert!(!overlaps(output, Rect::new(0, 50, 10, 10)));
    assert!(overlaps(output, Rect::new(-5, -5, 10, 10)));
    assert!(!overlaps(output, Rect::new(10, 10, 0, 5)), "empty");
  }

  #[test]
  fn the_label_sits_at_wayfires_offset_on_its_output() {
    assert_eq!(label_text(3), "Workspace set 3");
    assert_eq!(
      label_rect(Rect::new(1280, 100, 800, 600), (200, 40)),
      RenderRect {
        x: 1290,
        y: 110,
        width: 200,
        height: 40,
      }
    );
  }

  #[test]
  fn virtual_frames_keep_their_grid_unless_far_behind() {
    let start = Instant::now();
    assert_eq!(
      next_virtual_frame(start, start + Duration::from_millis(1)),
      start + VIRTUAL_FRAME
    );
    let late = start + Duration::from_secs(1);
    assert_eq!(next_virtual_frame(start, late), late + VIRTUAL_FRAME);
  }

  #[test]
  fn translation_saturates() {
    let rect = RenderRect {
      x: i32::MAX - 1,
      y: 5,
      width: 3,
      height: 4,
    };
    assert_eq!(translate(rect, 10, -5).x, i32::MAX);
    assert_eq!(translate(rect, -1, -5).y, 0);
  }
}
