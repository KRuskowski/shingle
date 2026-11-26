//! Wayfire's `[core] plugins`: which of the plugins Shingle implements are
//! active (docs/design/wayfire-gaps.md, Decisions 4).
//!
//! With [`ShingleConfig::plugins`] set, every plugin of [`PLUGINS`] that it
//! does not name is off, as when Wayfire does not load it.
//! [`ShingleConfig::turn_off_unlisted_plugins`] turns off what the model
//! holds for such a plugin: its bindings, effects, animations, rules and
//! autostart entries. The rest is checked where it is used, through
//! [`ShingleConfig::plugin_enabled`]: tiling (`simple-tile`), touch
//! gestures (`extra-gestures`), Scale's title filter
//! (`scale-title-filter`) and the protocols of Wayfire's protocol plugins.
//! IPC is not a plugin here: `--ipc` governs it, so `ipc` and `ipc-rules`
//! are always on.

use crate::{Binding, ShingleConfig};

/// The Wayfire plugins Shingle implements, which `[core] plugins` turns on
/// and off. Other names in the list are ignored.
pub const PLUGINS: &[&str] = &[
  "alpha",
  "animate",
  "autostart",
  "command",
  "expo",
  "ext-toplevel",
  "ext-workspace",
  "extra-gestures",
  "fast-switcher",
  "foreign-toplevel",
  "grid",
  "idle",
  "input-method-v1",
  "invert",
  "move",
  "oswitch",
  "place",
  "preserve-output",
  "resize",
  "scale",
  "scale-title-filter",
  "security-context-v1",
  "session-lock",
  "shortcuts-inhibit",
  "simple-tile",
  "switcher",
  "vswitch",
  "window-rules",
  "wm-actions",
  "wrot",
  "wsets",
  "xdg-activation",
  "xkb-bindings",
  "zoom",
];

/// The plugin names of a `[core] plugins` value. Wayfire also accepts a
/// plugin's file, such as `/usr/lib/wayfire/libscale.so`, which names the
/// plugin `scale`.
pub(crate) fn parse_list(value: &str) -> Vec<String> {
  value
    .split_whitespace()
    .map(|entry| {
      let file = entry.rsplit('/').next().unwrap_or(entry);
      match file.strip_suffix(".so") {
        Some(stem) => stem.strip_prefix("lib").unwrap_or(stem).to_owned(),
        None => file.to_owned(),
      }
    })
    .collect()
}

impl ShingleConfig {
  /// Whether Wayfire plugin `name` is active: always when
  /// [`ShingleConfig::plugins`] is unset or `name` is not one of
  /// [`PLUGINS`], and otherwise when the list names it.
  pub fn plugin_enabled(&self, name: &str) -> bool {
    match &self.plugins {
      None => true,
      Some(list) => {
        !PLUGINS.contains(&name) || list.iter().any(|entry| entry == name)
      }
    }
  }

  /// Turns off what the model holds for each plugin
  /// [`ShingleConfig::plugin_enabled`] says is off. A binding that two
  /// plugins share, such as `toggle_maximized` (`simple-tile` and
  /// `wm-actions`) or `move_button` (`move` and `simple-tile`), stays
  /// while either is on. Running it again changes nothing.
  pub fn turn_off_unlisted_plugins(&mut self) {
    if self.plugins.is_none() {
      return;
    }
    let off = |name: &str| !self.plugin_enabled(name);
    let alpha = off("alpha");
    let animate = off("animate");
    let autostart = off("autostart");
    let command = off("command");
    let expo = off("expo");
    let fast_switcher = off("fast-switcher");
    let grid = off("grid");
    let idle = off("idle");
    let input_method = off("input-method-v1");
    let invert = off("invert");
    let moving = off("move");
    let oswitch = off("oswitch");
    let place = off("place");
    let resize = off("resize");
    let scale = off("scale");
    let shortcuts = off("shortcuts-inhibit");
    let tile = off("simple-tile");
    let switcher = off("switcher");
    let vswitch = off("vswitch");
    let rules = off("window-rules");
    let actions = off("wm-actions");
    let wrot = off("wrot");
    let wsets = off("wsets");
    let xkb = off("xkb-bindings");
    let zoom = off("zoom");

    let keys = &mut self.keybindings;
    let pointer = &mut self.pointer;
    if alpha {
      self.alpha.modifier.clear();
    }
    if animate {
      self.animate.open_animation = "none".into();
      self.animate.close_animation = "none".into();
      self.animate.minimize_animation = "none".into();
    }
    if autostart {
      self.autostart.clear();
    }
    if command {
      self.commands.clear();
    }
    if xkb {
      self
        .commands
        .retain(|command| !command.binding.key.starts_with("xkb:"));
    }
    if expo {
      disable(&mut [&mut keys.expo_toggle]);
      self.expo.workspace_bindings.clear();
    }
    if fast_switcher {
      disable(&mut [
        &mut keys.fast_switcher_next,
        &mut keys.fast_switcher_previous,
      ]);
    }
    if grid {
      for slot in [
        &mut keys.grid_slot_top_left,
        &mut keys.grid_slot_top,
        &mut keys.grid_slot_top_right,
        &mut keys.grid_slot_left,
        &mut keys.grid_slot_center,
        &mut keys.grid_slot_right,
        &mut keys.grid_slot_bottom_left,
        &mut keys.grid_slot_bottom,
        &mut keys.grid_slot_bottom_right,
        &mut keys.grid_restore,
      ] {
        slot.clear();
      }
      // move asks grid to snap on release; without grid nothing snaps.
      self.grid.enable_snap = false;
    }
    if idle {
      self.idle.idle_seconds = None;
      self.idle.dpms_seconds = None;
      disable(&mut [&mut keys.idle_toggle]);
    }
    if input_method {
      self.input.enable_text_input_v3 = false;
    }
    if invert {
      disable(&mut [&mut keys.invert_toggle]);
    }
    if moving && tile {
      disable(&mut [&mut pointer.move_button]);
    }
    if oswitch {
      let o = &mut self.oswitch;
      disable(&mut [
        &mut o.next_output,
        &mut o.next_output_with_win,
        &mut o.prev_output,
        &mut o.prev_output_with_win,
        &mut o.left_output,
        &mut o.right_output,
        &mut o.above_output,
        &mut o.below_output,
      ]);
    }
    if place {
      self.placement.mode = "none".into();
    }
    if resize {
      disable(&mut [&mut pointer.resize_preserve_aspect_button]);
      if tile {
        disable(&mut [&mut pointer.resize_button]);
      }
    }
    if scale {
      disable(&mut [&mut keys.scale_toggle, &mut keys.scale_toggle_all]);
    }
    if shortcuts {
      disable(&mut [&mut keys.break_grab]);
    }
    if tile {
      disable(&mut [
        &mut keys.focus_left,
        &mut keys.focus_right,
        &mut keys.focus_up,
        &mut keys.focus_down,
        &mut keys.toggle_tiling,
        &mut keys.move_left,
        &mut keys.move_right,
        &mut keys.move_up,
        &mut keys.move_down,
        &mut keys.resize_left,
        &mut keys.resize_right,
        &mut keys.resize_up,
        &mut keys.resize_down,
      ]);
    }
    if tile && actions {
      disable(&mut [&mut keys.toggle_maximized]);
    }
    if switcher {
      disable(&mut [&mut keys.focus_next, &mut keys.focus_previous]);
    }
    if vswitch {
      disable(&mut [
        &mut keys.workspace_next,
        &mut keys.workspace_previous,
        &mut keys.workspace_up,
        &mut keys.workspace_down,
        &mut keys.workspace_move_next,
        &mut keys.workspace_move_previous,
        &mut keys.workspace_move_up,
        &mut keys.workspace_move_down,
        &mut keys.workspace_send_next,
        &mut keys.workspace_send_previous,
        &mut keys.workspace_send_up,
        &mut keys.workspace_send_down,
        &mut keys.workspace_last,
        &mut keys.workspace_move_last,
        &mut keys.workspace_send_last,
      ]);
      self.vswitch.workspace_bindings.clear();
      self.vswitch.workspace_bindings_win.clear();
      self.vswitch.bindings_win.clear();
    }
    if rules {
      self.window_rules.clear();
    }
    if actions {
      disable(&mut [
        &mut keys.toggle_fullscreen,
        &mut keys.toggle_sticky,
        &mut keys.always_on_top,
        &mut keys.always_on_bottom,
        &mut keys.minimize,
        &mut keys.bring_to_front,
        &mut keys.send_to_back,
        &mut keys.show_desktop,
      ]);
    }
    if wrot {
      disable(&mut [
        &mut pointer.wrot_activate,
        &mut keys.wrot_reset,
        &mut keys.wrot_reset_one,
      ]);
    }
    if wsets {
      self.wsets.wsets_bindings.clear();
      self.wsets.send_window_bindings.clear();
    }
    if zoom {
      self.zoom.modifier.clear();
    }
  }
}

fn disable(bindings: &mut [&mut Binding]) {
  for binding in bindings {
    **binding = Binding::disabled();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn plugin_files_name_their_plugin() {
    assert_eq!(
      parse_list("scale /usr/lib/wayfire/libgrid.so libzoom.so  expo\n"),
      ["scale", "grid", "zoom", "expo"]
    );
  }

  #[test]
  fn without_a_list_every_plugin_is_on() {
    let mut config = ShingleConfig::default();
    assert!(PLUGINS.iter().all(|name| config.plugin_enabled(name)));
    let before = config.to_yaml().unwrap();
    config.turn_off_unlisted_plugins();
    assert_eq!(config.to_yaml().unwrap(), before);
  }

  #[test]
  fn ipc_and_unknown_plugins_are_always_on() {
    let config = ShingleConfig {
      plugins: Some(Vec::new()),
      ..ShingleConfig::default()
    };
    assert!(config.plugin_enabled("ipc"));
    assert!(config.plugin_enabled("ipc-rules"));
    assert!(config.plugin_enabled("core"));
    assert!(!config.plugin_enabled("scale"));
  }

  #[test]
  fn an_empty_list_turns_everything_off() {
    let mut config = ShingleConfig {
      plugins: Some(Vec::new()),
      autostart: vec!["foot".into()],
      ..ShingleConfig::default()
    };
    config.turn_off_unlisted_plugins();
    let keys = &config.keybindings;
    for binding in [
      &keys.scale_toggle,
      &keys.expo_toggle,
      &keys.focus_next,
      &keys.fast_switcher_next,
      &keys.invert_toggle,
      &keys.toggle_maximized,
      &keys.focus_left,
      &keys.workspace_next,
      &keys.toggle_fullscreen,
      &keys.wrot_reset,
      &config.pointer.move_button,
      &config.pointer.resize_button,
      &config.oswitch.next_output,
    ] {
      assert_eq!(*binding, Binding::disabled());
    }
    assert!(keys.grid_slot_left.is_empty());
    assert_eq!(config.animate.open_animation, "none");
    assert!(config.zoom.modifier.is_empty());
    assert!(config.alpha.modifier.is_empty());
    assert!(config.autostart.is_empty());
    assert_eq!(config.placement.mode, "none");
    assert_eq!(config.idle.idle_seconds, None);
    assert!(!config.input.enable_text_input_v3);
    assert!(!config.grid.enable_snap);
    // Core bindings are not a plugin's.
    assert_ne!(keys.close, Binding::disabled());
    assert_ne!(keys.quit, Binding::disabled());
    config.validate().unwrap();
  }

  #[test]
  fn shared_bindings_stay_while_one_plugin_is_on() {
    let defaults = ShingleConfig::default();
    let mut config = ShingleConfig {
      plugins: Some(vec!["wm-actions".into(), "move".into()]),
      ..ShingleConfig::default()
    };
    config.turn_off_unlisted_plugins();
    assert_eq!(
      config.keybindings.toggle_maximized,
      defaults.keybindings.toggle_maximized
    );
    assert_eq!(config.pointer.move_button, defaults.pointer.move_button);
    assert_eq!(config.pointer.resize_button, Binding::disabled());
    assert_eq!(config.keybindings.focus_left, Binding::disabled());
  }

  #[test]
  fn xkb_commands_need_xkb_bindings() {
    let command = |key: &str| crate::CommandBinding {
      binding: Binding::new(key, Vec::new()),
      command: key.into(),
      repeatable: false,
      on_release: false,
      always: false,
    };
    let mut config = ShingleConfig {
      plugins: Some(vec!["command".into()]),
      commands: vec![command("xkb:XF86AudioMute"), command("f1")],
      ..ShingleConfig::default()
    };
    config.turn_off_unlisted_plugins();
    assert_eq!(config.commands.len(), 1);
    assert_eq!(config.commands[0].command, "f1");
  }
}
