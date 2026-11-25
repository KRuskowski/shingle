//! Zoom, colour inversion and window alpha.

use std::time::Instant;

use shingle::InputDeviceId;

use super::Frontend;
use crate::{bindings::modifier_matches, host::Axis};

impl Frontend {
  pub(super) fn map_zoom_input(&self, x: f64, y: f64) -> (f64, f64) {
    let output = self.compositor.output();
    shingle_render::zoom_input_point(
      x,
      y,
      output.width,
      output.height,
      self.zoom_factor,
      self.zoom_origin.0,
      self.zoom_origin.1,
    )
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
    let Axis {
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    } = axis;
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
      self.zoom_origin = (self.pointer_x as f32, self.pointer_y as f32);
      let target = (self.zoom_target
        - self.zoom_target * vertical as f32 * self.zoom.speed as f32)
        .clamp(1.0, 50.0);
      if target == self.zoom_target {
        return;
      }
      self.zoom_target = target;
      if self.zoom.smoothing_ms == 0 {
        self.zoom_factor = target;
        self.zoom_animation = None;
      } else {
        self.zoom_animation = Some((Instant::now(), self.zoom_factor));
      }
      self.request_redraw();
      return;
    }
    self.compositor.pointer_axis_detailed(
      device,
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    );
  }
}
