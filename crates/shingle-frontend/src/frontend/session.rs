//! Session saving, config reload and the lock client.

use std::{
  error::Error,
  time::{Duration, Instant},
};

use shingle_config::ShingleConfig;

use super::Frontend;

/// The variable that names Shingle's config file for the lock client,
/// which reads its `lock` section (crates/shingle-lock/src/options.rs).
pub const LOCK_CONFIG_VARIABLE: &str = "SHINGLE_CONFIG_FILE";

impl Frontend {
  pub(super) fn start_lock_client(&mut self) {
    if self
      .lock_client
      .as_mut()
      .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    {
      return;
    }
    // TILEFIRE_LOCK is the pre-rename name and is still honoured.
    let program = std::env::var_os("SHINGLE_LOCK")
      .or_else(|| std::env::var_os("TILEFIRE_LOCK"))
      .unwrap_or_else(|| self.lock_command.clone().into());
    let mut environment = self.compositor.client_environment();
    // shingle-lock reads its `lock` section from the file Shingle read.
    if let Some(path) = self.config_path.as_deref() {
      let path = std::path::absolute(path).unwrap_or_else(|_| path.into());
      environment =
        environment.with_variable(LOCK_CONFIG_VARIABLE, path.as_os_str());
    }
    match shingle_session::launch_program(&program, &[], &environment) {
      Ok(child) => self.lock_client = Some(child),
      Err(error) => {
        eprintln!("shingle: failed to start lock client: {error}")
      }
    }
  }

  /// Queues the session for the background writer. Only the model is
  /// cloned here; validation, serialization and the synced write happen
  /// on the writer's thread.
  pub(super) fn save(&self) {
    if let Some(session) = &self.session {
      session.submit(self.compositor.core().snapshot());
    }
  }

  /// Waits until the background writer has written every queued session.
  #[cfg(test)]
  pub(super) fn flush_session(&self) {
    if let Some(session) = &self.session {
      session.flush();
    }
  }

  pub(super) fn reload_config(&mut self) {
    self.config_watcher.mark_loaded();
    let result = (|| {
      let (config, warnings) =
        ShingleConfig::load_with_warnings(self.config_path.as_deref())?;
      let path = self
        .config_path
        .clone()
        .unwrap_or_else(shingle_config::default_path);
      for warning in warnings {
        eprintln!("shingle: {}: {warning}", path.display());
      }
      let keymap = config.input.load_keymap()?;
      self.compositor.apply_config(config.clone().into())?;
      self
        .compositor
        .set_wayfire_config_file(config.wayfire_config_file(&path).as_deref());
      self.compositor.set_keymap(&keymap)?;
      self.keyboard.set_keymap(&keymap)?;
      self
        .compositor
        .set_repeat(config.input.repeat_rate, config.input.repeat_delay);
      self
        .key_repeat
        .configure(config.input.repeat_rate, config.input.repeat_delay);
      Ok::<ShingleConfig, Box<dyn Error>>(config)
    })();
    match result {
      Ok(config) => {
        let gestures_enabled = config.plugin_enabled("extra-gestures");
        let scale_filter_enabled = config.plugin_enabled("scale-title-filter");
        let vswipe_enabled = config.plugin_enabled("vswipe");
        self.reloaded_config = Some(config.clone());
        self.cancel_switcher();
        self.cancel_scale();
        self.clear_expo();
        for scene in self.scenes.values_mut() {
          scene.workspace_transition = None;
        }
        self.wsets_label_duration =
          Duration::from_millis(config.wsets.label_duration);
        self.lock_command = config.lock.command.clone();
        self.unlock_fade = Duration::from_secs_f64(config.lock.unlock_fade);
        self.scale_exit = None;
        self.scale_titles.clear();
        self.scale_icons.clear();
        self.decoration_config = config.decoration.clone();
        self.frame_textures.clear();
        self.scale_filter.clear();
        self.scale_filter_texture = None;
        self.switcher_inactive_alpha = config.fast_switcher.inactive_alpha;
        self.switcher_config = config.switcher.clone();
        self.scale_config = config.scale.clone();
        self.scale_title_filter = config.scale_title_filter.clone();
        self.scale_filter_enabled = scale_filter_enabled;
        self.expo_config = config.expo.clone();
        self.vswitch = config.vswitch.clone();
        self.cancel_vswipe();
        self.vswipe_config = config.vswipe.clone();
        self.vswipe_enabled = vswipe_enabled;
        self.keybindings = config.keybindings;
        self.commands = config.commands;
        self.modifier_binding_timeout = config.input.modifier_binding_timeout;
        self.pointer_bindings = config.pointer;
        self.minimum_extent = config.tiling.minimum_extent;
        self.resize = config.resize.clone();
        self.discard_command_output = config.behavior.discard_command_output;
        self.zoom = config.zoom.clone();
        self.alpha = config.alpha.clone();
        self.left_handed_mode = config.input.left_handed_mode;
        self.middle_emulation = config.input.middle_emulation;
        self.compositor.clamp_opacity(self.alpha.min_value);
        // Running animations finish as they started, as in Wayfire.
        self.animate = config.animate.clone();
        self.invert_preserve_hue = config.effects.invert_preserve_hue;
        self.background_color = config.background_color;
        self.animation_duration =
          Duration::from_millis(config.tiling.animation_duration);
        self.animation_easing = config.tiling.animation_easing;
        self.preview_base_color = config.tiling.preview_base_color;
        self.preview_border_color = config.tiling.preview_border_color;
        self.preview_border_width = config.tiling.preview_border_width;
        self.snap_preview_base_color = config.grid.preview_base_color;
        self.snap_preview_border_color = config.grid.preview_border_color;
        self.snap_preview_border_width = config.grid.preview_border_width;
        self.grid_snap = config.grid.enable_snap;
        self.snap_threshold = config.grid.snap_threshold;
        self.quarter_snap_threshold = config.grid.quarter_snap_threshold;
        self.enable_snap_off = config.grid.enable_snap_off;
        self.snap_off_threshold = config.grid.snap_off_threshold;
        self.join_views = config.grid.join_views;
        self.gestures.set_config(config.extra_gestures.clone());
        self.gestures_enabled = gestures_enabled;
        self.wrot_reset_radius = config.effects.wrot_reset_radius;
        self.workspace_switch_after = (config.grid.workspace_switch_after >= 0)
          .then(|| {
            Duration::from_millis(config.grid.workspace_switch_after as u64)
          });
        if self.workspace_switch_after.is_none() {
          self.workspace_switch_started = None;
        }
        self.auto_reload_config = config.behavior.auto_reload;
        self.config_reload_delay =
          Duration::from_millis(config.behavior.config_reload_delay);
        self.request_redraw();
      }
      Err(error) => eprintln!("shingle: config reload failed: {error}"),
    }
  }

  pub(super) fn reload_if_changed(&mut self) -> bool {
    if !self.auto_reload_config
      || self.last_config_poll.elapsed() < Duration::from_millis(100)
    {
      return false;
    }
    self.last_config_poll = Instant::now();
    if !self
      .config_watcher
      .poll(self.last_config_poll, self.config_reload_delay)
    {
      return false;
    }
    self.reload_config();
    true
  }
}
