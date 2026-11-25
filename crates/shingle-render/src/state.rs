//! Per-output view state: scale, output transform, zoom and filtering.

use crate::{TextureTransform, gpu::TextureFilter};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Zoom {
  pub(crate) factor: f32,
  pub(crate) origin_x: f32,
  pub(crate) origin_y: f32,
}

impl Default for Zoom {
  fn default() -> Self {
    Self {
      factor: 1.0,
      origin_x: 0.0,
      origin_y: 0.0,
    }
  }
}

/// How tiles are mapped onto one output: its scale factor, transform, zoom
/// and the texture filter new textures should use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderState {
  scale_factor: f32,
  output_transform: TextureTransform,
  zoom: Zoom,
  nearest_filter: bool,
}

impl Default for RenderState {
  fn default() -> Self {
    Self {
      scale_factor: 1.0,
      output_transform: TextureTransform::Normal,
      zoom: Zoom::default(),
      nearest_filter: false,
    }
  }
}

impl RenderState {
  /// Logical-to-physical pixel scale. Non-positive or non-finite values are
  /// ignored.
  pub fn set_scale_factor(&mut self, scale: f64) {
    if scale.is_finite() && scale > 0.0 {
      self.scale_factor = scale as f32;
    }
  }

  /// The current scale factor.
  pub fn scale_factor(&self) -> f32 {
    self.scale_factor
  }

  /// Sets the `wl_output` transform applied to the whole frame.
  pub fn set_output_transform(&mut self, transform: TextureTransform) {
    self.output_transform = transform;
  }

  /// The current output transform.
  pub fn output_transform(&self) -> TextureTransform {
    self.output_transform
  }

  /// Zooms by `factor`, clamped to 1..=50, around the logical point
  /// (`origin_x`, `origin_y`). Non-finite values are ignored.
  pub fn set_zoom(&mut self, factor: f32, origin_x: f32, origin_y: f32) {
    if factor.is_finite() && origin_x.is_finite() && origin_y.is_finite() {
      self.zoom = Zoom {
        factor: factor.clamp(1.0, 50.0),
        origin_x,
        origin_y,
      };
    }
  }

  /// The current zoom factor and origin.
  pub fn zoom(&self) -> (f32, f32, f32) {
    (self.zoom.factor, self.zoom.origin_x, self.zoom.origin_y)
  }

  pub(crate) fn zoom_state(&self) -> Zoom {
    self.zoom
  }

  /// Selects nearest-neighbour filtering for textures created from now on.
  /// Returns true when the setting changed, so callers know to recreate
  /// their textures.
  pub fn set_nearest_filter(&mut self, nearest: bool) -> bool {
    if self.nearest_filter == nearest {
      return false;
    }
    self.nearest_filter = nearest;
    true
  }

  /// The filter new textures should be created with.
  pub fn texture_filter(&self) -> TextureFilter {
    if self.nearest_filter {
      TextureFilter::Nearest
    } else {
      TextureFilter::Linear
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn defaults_are_identity() {
    let state = RenderState::default();
    assert_eq!(state.scale_factor(), 1.0);
    assert_eq!(state.output_transform(), TextureTransform::Normal);
    assert_eq!(state.zoom(), (1.0, 0.0, 0.0));
    assert_eq!(state.texture_filter(), TextureFilter::Linear);
  }

  #[test]
  fn invalid_scale_and_zoom_are_ignored() {
    let mut state = RenderState::default();
    state.set_scale_factor(2.0);
    state.set_scale_factor(0.0);
    state.set_scale_factor(f64::NAN);
    assert_eq!(state.scale_factor(), 2.0);
    state.set_zoom(3.0, 10.0, 20.0);
    state.set_zoom(f32::INFINITY, 0.0, 0.0);
    state.set_zoom(2.0, f32::NAN, 0.0);
    assert_eq!(state.zoom(), (3.0, 10.0, 20.0));
  }

  #[test]
  fn zoom_factor_is_clamped() {
    let mut state = RenderState::default();
    state.set_zoom(0.5, 1.0, 2.0);
    assert_eq!(state.zoom(), (1.0, 1.0, 2.0));
    state.set_zoom(80.0, 1.0, 2.0);
    assert_eq!(state.zoom(), (50.0, 1.0, 2.0));
  }

  #[test]
  fn nearest_filter_reports_changes() {
    let mut state = RenderState::default();
    assert!(!state.set_nearest_filter(false));
    assert!(state.set_nearest_filter(true));
    assert_eq!(state.texture_filter(), TextureFilter::Nearest);
    assert!(!state.set_nearest_filter(true));
    assert!(state.set_nearest_filter(false));
    assert_eq!(state.texture_filter(), TextureFilter::Linear);
  }
}
