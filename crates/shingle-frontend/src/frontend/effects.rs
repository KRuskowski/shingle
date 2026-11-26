//! Zoom, colour inversion and window alpha. Zoom is per output, as
//! Wayfire's zoom plugin is: the wheel zooms the output under the pointer.

use std::time::Instant;

use shingle::InputDeviceId;

use super::Frontend;
use crate::{bindings::modifier_matches, host::Axis};

impl Frontend {
  /// Maps a layout point on the screen to the scene point under it, undoing
  /// the zoom of the output it is on.
  pub(super) fn map_zoom_input(&self, x: f64, y: f64) -> (f64, f64) {
    let output = self.output_under(x, y);
    let (factor, origin) = self.zoom_of(&output);
    if factor == 1.0 {
      return (x, y);
    }
    let rect = self.output_rect(&output);
    let (local_x, local_y) = shingle_render::zoom_input_point(
      x - f64::from(rect.x),
      y - f64::from(rect.y),
      rect.width,
      rect.height,
      factor,
      origin.0,
      origin.1,
    );
    (local_x + f64::from(rect.x), local_y + f64::from(rect.y))
  }

  /// Keeps the zoom of the output under the pointer centred on it.
  pub(super) fn follow_zoom(&mut self, x: f64, y: f64) {
    let output = self.output_under(x, y);
    let rect = self.output_rect(&output);
    if let Some(scene) = self.scenes.get_mut(&output)
      && scene.zoom_factor > 1.0
    {
      scene.zoom_origin = (
        (x - f64::from(rect.x)) as f32,
        (y - f64::from(rect.y)) as f32,
      );
    }
  }

  pub(super) fn toggle_inversion(&mut self) {
    self.inverted = !self.inverted;
    self
      .compositor
      .notify_plugin_activation("invert", self.inverted);
    self.request_redraw();
  }

  pub(super) fn pointer_axis(&mut self, device: InputDeviceId, axis: Axis) {
    if self.expo.is_some() {
      return;
    }
    let Axis { vertical, .. } = axis;
    if self.scale.is_some() {
      if vertical != 0.0 {
        self.select_scale_relative(vertical > 0.0);
      }
      return;
    }
    let shortcuts_allowed =
      !self.compositor.is_locked() && !self.compositor.shortcuts_inhibited();
    if shortcuts_allowed
      && vertical != 0.0
      && modifier_matches(&self.alpha.modifier, self.config_modifiers())
      && self
        .compositor
        .adjust_opacity_at(
          self.pointer_x,
          self.pointer_y,
          vertical,
          self.alpha.min_value,
        )
        .is_some()
    {
      self.request_redraw();
      return;
    }
    if shortcuts_allowed
      && modifier_matches(&self.zoom.modifier, self.config_modifiers())
    {
      let output = self.output_under(self.pointer_x, self.pointer_y);
      let rect = self.output_rect(&output);
      let origin = (
        (self.pointer_x - f64::from(rect.x)) as f32,
        (self.pointer_y - f64::from(rect.y)) as f32,
      );
      let speed = self.zoom.speed as f32;
      let smoothing = self.zoom.smoothing_ms;
      let scene = self.scene_mut(&output);
      scene.zoom_origin = origin;
      let target = (scene.zoom_target
        - scene.zoom_target * vertical as f32 * speed)
        .clamp(1.0, 50.0);
      if target == scene.zoom_target {
        return;
      }
      scene.zoom_target = target;
      if smoothing == 0 {
        scene.zoom_factor = target;
        scene.zoom_animation = None;
      } else {
        scene.zoom_animation = Some((Instant::now(), scene.zoom_factor));
      }
      self.request_redraw_on(&output);
      return;
    }
    self.compositor.pointer_axis_detailed(device, axis);
  }
}
