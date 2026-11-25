//! Key and pointer binding dispatch, key repeat and modifier
//! updates. The binding helpers live in [`crate::bindings`].

use std::time::Instant;

use shingle::{Direction, InputDeviceId};
use shingle_config::Modifiers as ConfigModifiers;
use shingle_session::launch_command;

use super::{
  Frontend,
  drag::{DragState, RotationDrag},
  switcher::SwitcherKind,
};
use crate::{
  bindings::{grid_slot, modifiers_held},
  host::Host,
  keyboard::{Key, KeyState},
};

impl Frontend {
  /// Sends the shared keyboard's modifier masks to the compositor.
  pub(super) fn update_modifiers(&mut self, device: InputDeviceId) {
    self
      .compositor
      .keyboard_modifiers(device, self.keyboard.modifiers());
  }

  /// Ends the switcher once its modifiers are released and sends the new
  /// modifier masks, after a key or focus change altered them.
  pub(super) fn keyboard_modifiers_changed(&mut self, device: InputDeviceId) {
    if self.switcher.as_ref().is_some_and(|state| {
      !modifiers_held(state.modifiers, self.config_modifiers())
    }) {
      self.finish_switcher();
    }
    self.update_modifiers(device);
  }

  pub(super) fn config_modifiers(&self) -> ConfigModifiers {
    self.keyboard.binding_modifiers()
  }

  pub(super) fn pointer_button_action(
    &mut self,
    button: &str,
    pressed: bool,
  ) -> bool {
    let modifiers = self.config_modifiers();
    if self.run_matching_command_with(pressed, false, |binding| {
      binding.matches(modifiers, button)
    }) {
      return true;
    }
    if !pressed && self.rotation_drag.take().is_some() {
      return true;
    }
    if !pressed {
      let Some(drag) = self.drag.take() else {
        return false;
      };
      let drop_slot = self.drop_slot.take();
      self.workspace_switch_started = None;
      self.drop_preview = None;
      if let DragState::Move { source, .. } = drag {
        if let Some(slot) = drop_slot {
          if let Err(error) = self.compositor.snap_window(source, slot) {
            eprintln!("shingle: grid snap failed: {error}");
          }
        } else {
          let floating = self
            .compositor
            .core()
            .window_info(source)
            .is_ok_and(|info| info.state.floating.is_some());
          if !floating
            && let Some(target) =
              self.compositor.focus_at(self.pointer_x, self.pointer_y)
            && target != source
          {
            let direction = self.drop_direction(target);
            if let Err(error) =
              self.compositor.drop_window(source, target, direction)
            {
              eprintln!("shingle: pointer move failed: {error}");
            }
          }
        }
        self.save();
      }
      return true;
    }

    let modifiers = self.config_modifiers();
    if self
      .pointer_bindings
      .wrot_activate
      .matches(modifiers, button)
      && let Some(window) =
        self.compositor.focus_at(self.pointer_x, self.pointer_y)
    {
      self.rotation_drag = Some(RotationDrag {
        window,
        last_x: self.pointer_x,
        last_y: self.pointer_y,
      });
      return true;
    }
    let moving = self.pointer_bindings.move_button.matches(modifiers, button);
    let preserve_aspect = self
      .pointer_bindings
      .resize_preserve_aspect_button
      .matches(modifiers, button);
    let resizing = self
      .pointer_bindings
      .resize_button
      .matches(modifiers, button)
      || preserve_aspect;
    if !moving && !resizing {
      return false;
    }
    let Some(window) = self.compositor.focus_at(self.pointer_x, self.pointer_y)
    else {
      return false;
    };
    self.workspace_switch_started = None;
    if moving {
      self.drag = Some(DragState::Move {
        source: window,
        last_x: self.pointer_x,
        last_y: self.pointer_y,
        snap_off_origin: (self.enable_snap_off
          && self
            .compositor
            .core()
            .window_info(window)
            .is_ok_and(|info| info.state.snapped.is_some()))
        .then_some((self.pointer_x, self.pointer_y)),
      });
    } else if let Some(geometry) = self.window_geometry(window) {
      let local_x = self.pointer_x - geometry.x as f64;
      let local_y = self.pointer_y - geometry.y as f64;
      let threshold = self.resize.corner_threshold;
      let horizontal = if local_x < geometry.width as f64 * threshold {
        Some(Direction::Left)
      } else if local_x >= geometry.width as f64 * (1.0 - threshold) {
        Some(Direction::Right)
      } else {
        None
      };
      let vertical = if local_y < geometry.height as f64 * threshold {
        Some(Direction::Up)
      } else if local_y >= geometry.height as f64 * (1.0 - threshold) {
        Some(Direction::Down)
      } else {
        None
      };
      let floating = self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| info.state.floating.is_some());
      self.drag = Some(DragState::Resize {
        window,
        horizontal,
        vertical,
        last_x: self.pointer_x,
        last_y: self.pointer_y,
        aspect: (preserve_aspect && floating).then_some((
          self.pointer_x,
          self.pointer_y,
          geometry,
        )),
      });
    }
    true
  }

  pub(super) fn run_matching_command(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
    pressed: bool,
    repeat: bool,
  ) -> bool {
    self.run_matching_command_with(pressed, repeat, |binding| {
      binding.matches(modifiers, key)
    })
  }

  pub(super) fn run_matching_symbol_command(
    &mut self,
    modifiers: ConfigModifiers,
    symbol: Option<&str>,
    pressed: bool,
    repeat: bool,
  ) -> bool {
    let Some(symbol) = symbol else {
      return false;
    };
    self.run_matching_command_with(pressed, repeat, |binding| {
      binding.matches_xkb(modifiers, symbol)
    })
  }

  pub(super) fn run_matching_command_with(
    &mut self,
    pressed: bool,
    repeat: bool,
    mut matches: impl FnMut(&shingle_config::Binding) -> bool,
  ) -> bool {
    let locked = self.compositor.is_locked();
    if let Some(binding) = self
      .compositor
      .runtime_bindings()
      .into_iter()
      .find(|binding| matches(&binding.binding))
    {
      if locked && !binding.exec_always {
        return true;
      }
      let should_run = match binding.mode {
        shingle::RuntimeBindingMode::Normal => pressed && !repeat,
        shingle::RuntimeBindingMode::Repeat => pressed,
        shingle::RuntimeBindingMode::Release => !pressed,
      };
      if should_run {
        if let Some(command) = binding.command {
          match launch_command(
            &command,
            &self.compositor.client_environment(),
            self.discard_command_output,
          ) {
            Ok(child) => self.session_children.push(child),
            Err(error) => {
              eprintln!("shingle: runtime command failed: {error}")
            }
          }
        } else {
          self.compositor.trigger_runtime_binding(binding.id);
        }
      }
      return true;
    }
    let Some(command) = self
      .commands
      .iter()
      .find(|command| matches(&command.binding))
    else {
      return false;
    };
    if locked && !command.always {
      return false;
    }
    if command.should_run(pressed, repeat, locked) {
      match launch_command(
        &command.command,
        &self.compositor.client_environment(),
        self.discard_command_output,
      ) {
        Ok(child) => self.session_children.push(child),
        Err(error) => {
          eprintln!("shingle: command launch failed: {error}")
        }
      }
    }
    true
  }

  /// Feeds one key from seat keyboard `device` to the shared keyboard
  /// and, if the device is enabled, dispatches it. Keys are tracked while
  /// the device is disabled, so modifiers are right again the moment it is
  /// re-enabled.
  pub(super) fn key_event(
    &mut self,
    host: &mut dyn Host,
    device: InputDeviceId,
    code: u32,
    pressed: bool,
  ) {
    let enabled = self.compositor.input_device_enabled(device);
    let state = if pressed {
      self.key_repeat.press(
        code,
        self.keyboard.key_repeats(code),
        Instant::now(),
      );
      self.repeat_device = Some(device);
      KeyState::Pressed
    } else {
      self.key_repeat.release(code);
      KeyState::Released
    };
    // Locks and the layout can change over IPC.
    self.keyboard.sync_locked(
      self.compositor.locked_modifiers(),
      self.compositor.keyboard_layout_index(),
    );
    let key = self.keyboard.key(code, state);
    if enabled {
      self.keyboard_input(host, device, &key);
      if key.modifiers_changed {
        self.keyboard_modifiers_changed(device);
      }
    }
  }

  /// Forgets every held key after the host lost keyboard focus or the
  /// seat, and stops key repeat.
  pub(super) fn reset_keyboard(&mut self, device: InputDeviceId) {
    self.key_repeat.cancel();
    let before = self.keyboard.modifiers();
    self.keyboard.release_all();
    if self.keyboard.modifiers() != before
      && self.compositor.input_device_enabled(device)
    {
      self.keyboard_modifiers_changed(device);
    }
  }

  /// Repeats the held key once if its repeat is due.
  pub(super) fn process_key_repeat(&mut self, host: &mut dyn Host) {
    let Some(code) = self.key_repeat.poll(Instant::now()) else {
      return;
    };
    let Some(device) = self.repeat_device else {
      return;
    };
    self.keyboard.sync_locked(
      self.compositor.locked_modifiers(),
      self.compositor.keyboard_layout_index(),
    );
    let key = self.keyboard.key(code, KeyState::Repeated);
    if self.compositor.input_device_enabled(device) {
      self.keyboard_input(host, device, &key);
    }
  }

  /// Dispatches one key, interpreted by the shared keyboard, to the
  /// overviews, the switcher, bindings and finally the focused client.
  pub(super) fn keyboard_input(
    &mut self,
    host: &mut dyn Host,
    device: InputDeviceId,
    event: &Key,
  ) {
    let pressed = event.state != KeyState::Released;
    let repeat = event.state == KeyState::Repeated;
    // Ctrl+Alt+Fn switches VT before anything else sees the key, even on
    // the lock screen, as in Wayfire. Hosts that cannot switch get the key
    // like any other.
    if event.state == KeyState::Pressed
      && let Some(vt) = event.symbol.as_deref().and_then(switch_vt_target)
      && host.switch_vt(vt)
    {
      return;
    }
    let modifiers = event.modifiers;
    let key = event.name.unwrap_or_default();
    let locked = self.compositor.is_locked();
    if self.expo_key(modifiers, key, pressed, locked) {
      return;
    }
    if self.scale_key(modifiers, key, pressed, locked) {
      self.scale_filter_input(key, event.text.as_deref(), pressed);
      return;
    }
    if self.switcher.is_some() {
      if locked {
        self.finish_switcher();
      } else {
        if pressed {
          let Some(kind) = self.switcher.as_ref().map(|state| state.kind)
          else {
            return;
          };
          let (previous, next) = match kind {
            SwitcherKind::Fast => (
              &self.keybindings.fast_switcher_previous,
              &self.keybindings.fast_switcher_next,
            ),
            SwitcherKind::Regular => (
              &self.keybindings.focus_previous,
              &self.keybindings.focus_next,
            ),
          };
          let reverse = previous.matches(modifiers, key);
          if reverse || next.matches(modifiers, key) {
            self.advance_switcher(reverse);
          }
        }
        return;
      }
    }
    if (locked || !pressed) && !self.compositor.shortcuts_inhibited() {
      if self.run_matching_symbol_command(
        modifiers,
        event.symbol.as_deref(),
        pressed,
        repeat,
      ) {
        return;
      }
      if self.run_matching_command(modifiers, key, pressed, repeat) {
        return;
      }
    }
    if locked {
      self.compositor.keyboard_key(device, event.code, pressed);
      return;
    }
    if pressed && self.keybindings.break_grab.matches(modifiers, key) {
      self.compositor.break_shortcuts_inhibit();
      return;
    }
    if pressed && !self.compositor.shortcuts_inhibited() {
      if self.keybindings.wrot_reset.matches(modifiers, key) {
        self.rotations.clear();
        self.rotation_drag = None;
        self.request_redraw();
        return;
      }
      if self.keybindings.wrot_reset_one.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          self.rotations.remove(&window);
        }
        self.rotation_drag = None;
        self.request_redraw();
        return;
      }
      if self.keybindings.invert_toggle.matches(modifiers, key) {
        self.toggle_inversion();
        return;
      }
      let direction = if self.keybindings.focus_left.matches(modifiers, key) {
        Some(Direction::Left)
      } else if self.keybindings.focus_right.matches(modifiers, key) {
        Some(Direction::Right)
      } else if self.keybindings.focus_up.matches(modifiers, key) {
        Some(Direction::Up)
      } else if self.keybindings.focus_down.matches(modifiers, key) {
        Some(Direction::Down)
      } else {
        None
      };
      if let Some(direction) = direction {
        if let Err(error) = self.compositor.focus_directional(direction) {
          eprintln!("shingle: directional focus failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.expo_toggle.matches(modifiers, key) {
        self.toggle_expo();
        return;
      }
      if self.keybindings.scale_toggle_all.matches(modifiers, key) {
        self.toggle_scale(true);
        return;
      }
      if self.keybindings.scale_toggle.matches(modifiers, key) {
        self.toggle_scale(false);
        return;
      }
      let fast_reverse = self
        .keybindings
        .fast_switcher_previous
        .matches(modifiers, key);
      if fast_reverse
        || self.keybindings.fast_switcher_next.matches(modifiers, key)
      {
        let binding = if fast_reverse {
          self.keybindings.fast_switcher_previous.clone()
        } else {
          self.keybindings.fast_switcher_next.clone()
        };
        self.start_switcher(fast_reverse, binding, SwitcherKind::Fast);
        return;
      }
      let reverse = self.keybindings.focus_previous.matches(modifiers, key);
      if reverse || self.keybindings.focus_next.matches(modifiers, key) {
        let binding = if reverse {
          self.keybindings.focus_previous.clone()
        } else {
          self.keybindings.focus_next.clone()
        };
        self.start_switcher(reverse, binding, SwitcherKind::Regular);
        return;
      }
      if self.keybindings.toggle_fullscreen.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_fullscreen(window) {
            eprintln!("shingle: fullscreen toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.toggle_sticky.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_sticky(window) {
            eprintln!("shingle: sticky toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.always_on_top.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_always_on_top(window) {
            eprintln!("shingle: always-on-top toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.always_on_bottom.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_always_on_bottom(window) {
            eprintln!("shingle: always-on-bottom toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.minimize.matches(modifiers, key) {
        self.compositor.minimize_focused();
        self.save();
        return;
      }
      if self.keybindings.bring_to_front.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window()
          && let Err(error) = self.compositor.raise_window(window)
        {
          eprintln!("shingle: raise failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.send_to_back.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window()
          && let Err(error) = self.compositor.lower_window(window)
        {
          eprintln!("shingle: lower failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.show_desktop.matches(modifiers, key) {
        self.compositor.toggle_show_desktop();
        self.save();
        return;
      }
      if self.keybindings.toggle_maximized.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_maximized(window) {
            eprintln!("shingle: maximize failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.toggle_tiling.matches(modifiers, key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_tiling(window) {
            eprintln!("shingle: tiling toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      let grid_slot = grid_slot(&self.keybindings, modifiers, key);
      let grid_restore = self
        .keybindings
        .grid_restore
        .iter()
        .any(|binding| binding.matches(modifiers, key));
      if grid_slot.is_some() || grid_restore {
        if let Some(window) = self.compositor.focused_window() {
          let result = if let Some(slot) = grid_slot {
            self.compositor.snap_window(window, slot)
          } else {
            self.compositor.restore_snapped(window)
          };
          if let Err(error) = result {
            eprintln!("shingle: grid action failed: {error}");
          }
          self.save();
        }
        return;
      }
      let move_direction = if self.keybindings.move_left.matches(modifiers, key)
      {
        Some(Direction::Left)
      } else if self.keybindings.move_right.matches(modifiers, key) {
        Some(Direction::Right)
      } else if self.keybindings.move_up.matches(modifiers, key) {
        Some(Direction::Up)
      } else if self.keybindings.move_down.matches(modifiers, key) {
        Some(Direction::Down)
      } else {
        None
      };
      if let Some(direction) = move_direction {
        if let Some(source) = self.compositor.focused_window() {
          match self.compositor.focus_directional(direction) {
            Ok(Some(target)) => {
              if let Err(error) =
                self.compositor.drop_window(source, target, direction)
              {
                eprintln!("shingle: move failed: {error}");
              }
              self.save();
            }
            Ok(None) => {}
            Err(error) => {
              eprintln!("shingle: move focus failed: {error}")
            }
          }
        }
        return;
      }
      let resize_direction =
        if self.keybindings.resize_left.matches(modifiers, key) {
          Some(Direction::Left)
        } else if self.keybindings.resize_right.matches(modifiers, key) {
          Some(Direction::Right)
        } else if self.keybindings.resize_up.matches(modifiers, key) {
          Some(Direction::Up)
        } else if self.keybindings.resize_down.matches(modifiers, key) {
          Some(Direction::Down)
        } else {
          None
        };
      if let Some(direction) = resize_direction {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.resize_window(
            window,
            direction,
            10,
            self.minimum_extent,
          ) {
            eprintln!("shingle: resize failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.workspace_last.matches(modifiers, key) {
        if let Err(error) = self.compositor.switch_to_last_workspace() {
          eprintln!("shingle: last workspace switch failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.workspace_move_last.matches(modifiers, key) {
        if let Err(error) = self.compositor.move_focused_to_last_workspace() {
          eprintln!("shingle: move to last workspace failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.workspace_send_last.matches(modifiers, key) {
        if let Err(error) = self.compositor.send_focused_to_last_workspace() {
          eprintln!("shingle: send to last workspace failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.workspace_send_next.matches(modifiers, key)
        || self
          .keybindings
          .workspace_send_previous
          .matches(modifiers, key)
        || self.keybindings.workspace_send_up.matches(modifiers, key)
        || self.keybindings.workspace_send_down.matches(modifiers, key)
      {
        let vertical =
          self.keybindings.workspace_send_up.matches(modifiers, key)
            || self.keybindings.workspace_send_down.matches(modifiers, key);
        let reverse = self
          .keybindings
          .workspace_send_previous
          .matches(modifiers, key)
          || self.keybindings.workspace_send_up.matches(modifiers, key);
        let result = if vertical {
          self.compositor.send_focused_vertical(reverse)
        } else {
          self.compositor.send_focused_relative(reverse)
        };
        if let Err(error) = result {
          eprintln!("shingle: relative window send failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.workspace_next.matches(modifiers, key)
        || self.keybindings.workspace_previous.matches(modifiers, key)
        || self.keybindings.workspace_up.matches(modifiers, key)
        || self.keybindings.workspace_down.matches(modifiers, key)
      {
        let vertical = self.keybindings.workspace_up.matches(modifiers, key)
          || self.keybindings.workspace_down.matches(modifiers, key);
        let reverse =
          self.keybindings.workspace_previous.matches(modifiers, key)
            || self.keybindings.workspace_up.matches(modifiers, key);
        let result = if vertical {
          self.compositor.switch_workspace_vertical(reverse)
        } else {
          self.compositor.switch_workspace_relative(reverse)
        };
        if let Err(error) = result {
          eprintln!("shingle: relative workspace switch failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.workspace_move_next.matches(modifiers, key)
        || self
          .keybindings
          .workspace_move_previous
          .matches(modifiers, key)
        || self.keybindings.workspace_move_up.matches(modifiers, key)
        || self.keybindings.workspace_move_down.matches(modifiers, key)
      {
        let vertical =
          self.keybindings.workspace_move_up.matches(modifiers, key)
            || self.keybindings.workspace_move_down.matches(modifiers, key);
        let reverse = self
          .keybindings
          .workspace_move_previous
          .matches(modifiers, key)
          || self.keybindings.workspace_move_up.matches(modifiers, key);
        let result = if vertical {
          self.compositor.move_focused_vertical(reverse)
        } else {
          self.compositor.move_focused_relative(reverse)
        };
        if let Err(error) = result {
          eprintln!("shingle: relative window move failed: {error}");
        }
        self.save();
        return;
      }
      if let Some(index) = self
        .keybindings
        .workspace_switch
        .iter()
        .position(|binding| binding.matches(modifiers, key))
      {
        if let Err(error) = self.compositor.switch_workspace(index) {
          eprintln!("shingle: workspace switch failed: {error}");
        }
        self.save();
        return;
      }
      if let Some(index) = self
        .keybindings
        .workspace_move
        .iter()
        .position(|binding| binding.matches(modifiers, key))
      {
        if let Err(error) = self.compositor.move_focused_to_workspace(index) {
          eprintln!("shingle: workspace move failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.close.matches(modifiers, key)
        || self
          .keybindings
          .close_alternatives
          .iter()
          .any(|binding| binding.matches(modifiers, key))
      {
        self.compositor.close_focused();
        return;
      }
      if self.keybindings.reload.matches(modifiers, key) {
        self.reload_config();
        return;
      }
      if self.keybindings.quit.matches(modifiers, key) {
        self.quit = true;
        self.save();
        return;
      }
      if self.run_matching_symbol_command(
        modifiers,
        event.symbol.as_deref(),
        pressed,
        repeat,
      ) {
        return;
      }
      if self.run_matching_command(modifiers, key, pressed, repeat) {
        return;
      }
      if self.keybindings.lock.matches(modifiers, key) {
        self.compositor.request_lock();
        return;
      }
      if self.keybindings.idle_toggle.matches(modifiers, key) {
        self.compositor.toggle_idle();
        return;
      }
    }
    self.compositor.keyboard_key(device, event.code, pressed);
  }
}

/// Returns the VT that keysym `symbol` switches to, for the
/// `XF86Switch_VT_1` to `XF86Switch_VT_12` keysyms that Ctrl+Alt+F1 to F12
/// produce.
fn switch_vt_target(symbol: &str) -> Option<u32> {
  symbol
    .strip_prefix("XF86Switch_VT_")?
    .parse()
    .ok()
    .filter(|vt| (1..=12).contains(vt))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_switch_vt_keysyms_name_a_vt() {
    assert_eq!(switch_vt_target("XF86Switch_VT_1"), Some(1));
    assert_eq!(switch_vt_target("XF86Switch_VT_12"), Some(12));
    assert_eq!(switch_vt_target("XF86Switch_VT_0"), None);
    assert_eq!(switch_vt_target("XF86Switch_VT_13"), None);
    assert_eq!(switch_vt_target("XF86Switch_VT_"), None);
    assert_eq!(switch_vt_target("F2"), None);
  }
}
