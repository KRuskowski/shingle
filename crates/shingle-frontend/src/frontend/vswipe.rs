//! Touchpad workspace swipes (Wayfire's `vswipe`, docs/design/wayfire-gaps.md,
//! WG-5): which swipes the frontend takes from clients, starting and
//! ending them, and the wall the scene draws while one runs.
//! [`crate::vswipe`] does the arithmetic.
//!
//! A swipe with `vswipe.fingers` fingers that begins with the pointer on
//! an output is followed from its first update, while clients still get
//! it. Once its direction is decided it starts: clients get the swipe's
//! end, cancelled, and nothing more of it, the output becomes active and
//! the wall follows the fingers. When they lift, the workspace the swipe
//! picked becomes current and the wall slides there. Other swipes, and
//! swipes that never start, go to clients whole.

use std::time::Instant;

use shingle::{Gesture, InputDeviceId, OutputHandle};

use super::Frontend;
use crate::vswipe::{Swipe, Update};

/// The swipe vswipe follows.
pub(super) struct SwipeState {
  /// The output it began on, whose wall it drags.
  pub(super) output: OutputHandle,
  /// The pointer whose swipe it is.
  device: InputDeviceId,
  /// The swipe itself.
  pub(super) swipe: Swipe,
  /// Whether it started: clients lost it and the wall is drawn.
  pub(super) active: bool,
}

impl Frontend {
  /// Handles one step of a touchpad swipe of pointer `device`: vswipe's
  /// when it takes it, the clients' otherwise.
  pub(super) fn swipe_input(
    &mut self,
    device: InputDeviceId,
    gesture: Gesture,
  ) {
    match gesture {
      Gesture::Begin { fingers } => {
        self.begin_vswipe(device, fingers);
        self.compositor.pointer_swipe_begin(device, fingers);
      }
      Gesture::Update { dx, dy, .. } => {
        if !self.update_vswipe(device, dx, dy) {
          self.compositor.pointer_swipe_update(device, dx, dy);
        }
      }
      Gesture::End { cancelled } => {
        if !self.end_vswipe(device) {
          self.compositor.pointer_swipe_end(device, cancelled);
        }
      }
    }
  }

  /// Follows a swipe that began with `fingers` fingers if it is vswipe's,
  /// as Wayfire's `on_swipe_begin`: the plugin is loaded, an axis is
  /// enabled, no swipe of vswipe is running, the fingers are the
  /// configured ones and the pointer is on an output.
  fn begin_vswipe(&mut self, device: InputDeviceId, fingers: u32) {
    let config = &self.vswipe_config;
    if !self.vswipe_enabled
      || !(config.enable_horizontal || config.enable_vertical)
      || self.vswipe.as_ref().is_some_and(|state| state.active)
      || fingers != config.fingers
    {
      return;
    }
    let Some(output) =
      self.compositor.output_at(self.pointer_x, self.pointer_y)
    else {
      return;
    };
    let Ok(workspace) = self
      .compositor
      .core()
      .current_workspace(&shingle_core::OutputId::new(output.name()))
    else {
      return;
    };
    let (columns, rows) = self.compositor.workspace_grid_dimensions();
    let grid = (columns as i32, rows as i32);
    self.vswipe = Some(SwipeState {
      output,
      device,
      swipe: Swipe::begin((workspace.x as i32, workspace.y as i32), grid),
      active: false,
    });
  }

  /// Moves vswipe's swipe, starting it once its direction is decided.
  /// Returns whether vswipe took the update from clients.
  fn update_vswipe(&mut self, device: InputDeviceId, dx: f64, dy: f64) -> bool {
    let now = Instant::now();
    let Some(state) = self
      .vswipe
      .as_mut()
      .filter(|state| state.device == device && state.swipe.swiping())
    else {
      return false;
    };
    match state.swipe.update(dx, dy, &self.vswipe_config, now) {
      Update::Pending => false,
      Update::Started => {
        let output = state.output.clone();
        if !self.can_start_vswipe(&output) {
          self.vswipe = None;
          return false;
        }
        // Clients saw the swipe begin; it is no longer theirs.
        self.compositor.pointer_swipe_end(device, true);
        self.compositor.focus_output(&output);
        if let Some(state) = &mut self.vswipe {
          state.active = true;
        }
        self.request_redraw_on(&output);
        true
      }
      Update::Moved => {
        let output = state.output.clone();
        self.request_redraw_on(&output);
        true
      }
    }
  }

  /// Whether vswipe may start on `output`, as Wayfire's
  /// `activate_plugin` allows it: no other activity of the frontend that
  /// Wayfire runs as a plugin with a conflicting grab (Scale, Expo, the
  /// switchers, a drag, window rotation or vswitch's slide), the session
  /// is not locked and no shell client inhibits the output.
  fn can_start_vswipe(&self, output: &OutputHandle) -> bool {
    self.scale.is_none()
      && self.scale_exit.is_none()
      && self.expo.is_none()
      && self.expo_exit.is_none()
      && self.switcher.is_none()
      && self.drag.is_none()
      && self.rotation_drag.is_none()
      && !self.compositor.is_locked()
      && !self.compositor.output_inhibited(output)
      && self
        .scenes
        .get(output)
        .is_none_or(|scene| scene.workspace_transition.is_none())
  }

  /// Lifts the fingers of vswipe's swipe: switches to the workspace it
  /// picked and slides the wall there. Returns whether the end was
  /// vswipe's; a swipe that never started is the clients'.
  fn end_vswipe(&mut self, device: InputDeviceId) -> bool {
    let now = Instant::now();
    let Some(state) = self
      .vswipe
      .as_mut()
      .filter(|state| state.device == device && state.swipe.swiping())
    else {
      return false;
    };
    if !state.active {
      self.vswipe = None;
      return false;
    }
    let target = state.swipe.end(&self.vswipe_config, now);
    let start = state.swipe.workspace();
    let output = state.output.clone();
    if target != start {
      let (columns, _) = self.compositor.workspace_grid_dimensions();
      let index = target.1 as usize * columns as usize + target.0 as usize;
      self.compositor.focus_output(&output);
      if let Err(error) = self.compositor.switch_workspace(index) {
        eprintln!("shingle: vswipe could not switch workspace: {error}");
      }
    }
    self.request_redraw_on(&output);
    true
  }

  /// Ends vswipe's swipe once its slide is over, as Wayfire's
  /// `post_frame` does, and asks for a last frame without the wall.
  pub(super) fn finish_vswipe(&mut self, now: Instant) {
    if let Some(state) = &self.vswipe
      && state.active
      && state.swipe.finished(now)
    {
      let output = state.output.clone();
      self.vswipe = None;
      self.request_redraw_on(&output);
    }
  }

  /// Drops vswipe's swipe without switching the workspace, as Wayfire's
  /// `finalize_and_exit` does when the plugin is cancelled.
  pub(super) fn cancel_vswipe(&mut self) {
    if let Some(state) = self.vswipe.take()
      && state.active
    {
      self.request_redraw_on(&state.output);
    }
  }

  /// The wall position `output` shows at `now` while vswipe drags its
  /// wall: the fractional (column, row) at the output's top-left corner.
  pub(super) fn vswipe_viewport(
    &self,
    output: &OutputHandle,
    now: Instant,
  ) -> Option<(f64, f64)> {
    self
      .vswipe
      .as_ref()
      .filter(|state| state.active && state.output == *output)
      .map(|state| state.swipe.viewport(now))
  }

  /// Whether vswipe's wall is on `output`, until its slide ends.
  pub(super) fn vswipe_on(&self, output: &OutputHandle) -> bool {
    self
      .vswipe
      .as_ref()
      .is_some_and(|state| state.active && state.output == *output)
  }
}
