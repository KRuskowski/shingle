use std::collections::BTreeMap;

use crate::{Binding, ConfigError, Modifier, ShingleConfig, XwaylandMode};

pub(crate) fn parse(text: &str) -> Result<ShingleConfig, ConfigError> {
  let mut config = ShingleConfig::default();
  // Wayfire starts Xwayland eagerly unless told otherwise
  // (metadata/core.xml), unlike Shingle's own default.
  config.xwayland.mode = XwaylandMode::Eager;
  let mut command_bindings = BTreeMap::new();
  let mut command_options = BTreeMap::new();
  let mut commands = BTreeMap::new();
  let mut section = String::new();
  // The section name as written: connector names keep their case.
  let mut section_name = String::new();
  for (index, original) in text.lines().enumerate() {
    let line_number = index + 1;
    let line = strip_comment(original).trim();
    if line.is_empty() {
      continue;
    }
    if line.starts_with('[') && line.ends_with(']') {
      section_name = line[1..line.len() - 1].trim().to_owned();
      section = section_name.to_lowercase();
      continue;
    }
    let Some((key, value)) = line.split_once('=') else {
      continue;
    };
    let key = key.trim().to_lowercase();
    let value = value.trim();
    match (section.as_str(), key.as_str()) {
      ("simple-tile", "tile_by_default") => {
        config.tiling.tile_by_default = value.to_owned().try_into()?;
      }
      ("simple-tile", "preview_base_color") => {
        config.tiling.preview_base_color = crate::parse_color(value)?;
      }
      ("simple-tile", "preview_base_border") => {
        config.tiling.preview_border_color = crate::parse_color(value)?;
      }
      ("simple-tile", "preview_border_width") => {
        config.tiling.preview_border_width = number(value, line_number)?;
      }
      ("simple-tile", "animation_duration") => {
        config.tiling.animation_duration = duration_ms(value, line_number)?;
      }
      ("simple-tile", "inner_gap_size") => {
        config.tiling.gaps.inner = number(value, line_number)?;
      }
      ("simple-tile", "keep_fullscreen_on_adjacent") => {
        config.tiling.keep_fullscreen_on_adjacent =
          boolean(value, line_number)?;
      }
      ("simple-tile", "outer_horiz_gap_size") => {
        let gap = number(value, line_number)?;
        config.tiling.gaps.left = gap;
        config.tiling.gaps.right = gap;
      }
      ("simple-tile", "outer_vert_gap_size") => {
        let gap = number(value, line_number)?;
        config.tiling.gaps.top = gap;
        config.tiling.gaps.bottom = gap;
      }
      ("simple-tile", "key_focus_left") => {
        config.keybindings.focus_left = binding(value)?;
      }
      ("simple-tile", "key_focus_right") => {
        config.keybindings.focus_right = binding(value)?;
      }
      ("simple-tile", "key_focus_above") => {
        config.keybindings.focus_up = binding(value)?;
      }
      ("simple-tile", "key_focus_below") => {
        config.keybindings.focus_down = binding(value)?;
      }
      ("simple-tile", "key_toggle_maximized") => {
        config.keybindings.toggle_maximized = binding(value)?;
      }
      ("switcher", "next_view") => {
        config.keybindings.focus_next = binding(value)?
      }
      ("switcher", "prev_view") => {
        config.keybindings.focus_previous = binding(value)?;
      }
      ("wm-actions", "toggle_fullscreen") => {
        config.keybindings.toggle_fullscreen = binding(value)?;
      }
      ("wm-actions", "toggle_sticky") => {
        config.keybindings.toggle_sticky = binding(value)?;
      }
      ("wm-actions", "toggle_always_on_top") => {
        config.keybindings.always_on_top = binding(value)?;
      }
      ("wm-actions", "toggle_always_on_bottom") => {
        config.keybindings.always_on_bottom = binding(value)?;
      }
      ("wm-actions", "toggle_maximize") => {
        config.keybindings.toggle_maximized = binding(value)?;
      }
      ("wm-actions", "minimize") => {
        config.keybindings.minimize = binding(value)?
      }
      ("wm-actions", "bring_to_front") => {
        config.keybindings.bring_to_front = binding(value)?;
      }
      ("wm-actions", "send_to_back") => {
        config.keybindings.send_to_back = binding(value)?;
      }
      ("wm-actions", "toggle_showdesktop") => {
        config.keybindings.show_desktop = binding(value)?;
      }
      ("simple-tile", "key_toggle") => {
        config.keybindings.toggle_tiling = binding(value)?;
      }
      ("simple-tile", "key_move_left") => {
        config.keybindings.move_left = binding(value)?;
      }
      ("simple-tile", "key_move_right") => {
        config.keybindings.move_right = binding(value)?;
      }
      ("simple-tile", "key_move_above") => {
        config.keybindings.move_up = binding(value)?;
      }
      ("simple-tile", "key_move_below") => {
        config.keybindings.move_down = binding(value)?;
      }
      ("simple-tile", "key_resize_left") => {
        config.keybindings.resize_left = binding(value)?;
      }
      ("simple-tile", "key_resize_right") => {
        config.keybindings.resize_right = binding(value)?;
      }
      ("simple-tile", "key_resize_above") => {
        config.keybindings.resize_up = binding(value)?;
      }
      ("simple-tile", "key_resize_below") => {
        config.keybindings.resize_down = binding(value)?;
      }
      ("simple-tile", "button_move") => {
        config.pointer.move_button = binding(value)?;
      }
      ("simple-tile", "button_resize") | ("resize", "activate") => {
        config.pointer.resize_button = binding(value)?;
      }
      ("move", "activate") => config.pointer.move_button = binding(value)?,
      ("resize", "activate_preserve_aspect") => {
        config.pointer.resize_preserve_aspect_button = binding(value)?;
      }
      ("resize", "min_width") => {
        config.resize.min_width = number(value, line_number)?
      }
      ("resize", "min_height") => {
        config.resize.min_height = number(value, line_number)?
      }
      ("resize", "corner_threshold") => {
        config.resize.corner_threshold = number(value, line_number)?;
      }
      ("grid", "slot_tl") => {
        config.keybindings.grid_slot_top_left = bindings(value)?
      }
      ("grid", "slot_t") => config.keybindings.grid_slot_top = bindings(value)?,
      ("grid", "slot_tr") => {
        config.keybindings.grid_slot_top_right = bindings(value)?
      }
      ("grid", "slot_l") => {
        config.keybindings.grid_slot_left = bindings(value)?
      }
      ("grid", "slot_c") => {
        config.keybindings.grid_slot_center = bindings(value)?
      }
      ("grid", "slot_r") => {
        config.keybindings.grid_slot_right = bindings(value)?
      }
      ("grid", "slot_bl") => {
        config.keybindings.grid_slot_bottom_left = bindings(value)?
      }
      ("grid", "slot_b") => {
        config.keybindings.grid_slot_bottom = bindings(value)?
      }
      ("grid", "slot_br") => {
        config.keybindings.grid_slot_bottom_right = bindings(value)?
      }
      ("grid", "restore") => config.keybindings.grid_restore = bindings(value)?,
      ("extra-gestures", "move_fingers") => {
        config.extra_gestures.move_fingers = number(value, line_number)?;
      }
      ("extra-gestures", "move_delay") => {
        config.extra_gestures.move_delay = number(value, line_number)?;
      }
      ("extra-gestures", "close_fingers") => {
        config.extra_gestures.close_fingers = number(value, line_number)?;
      }
      ("input", "xkb_rules") => config.input.rules = value.into(),
      ("input", "xkb_file") => config.input.xkb_file = value.into(),
      ("input", "xkb_model") => config.input.model = value.into(),
      ("input", "xkb_layout") => config.input.layout = value.into(),
      ("input", "xkb_variant") => config.input.variant = value.into(),
      ("input", "xkb_options") => config.input.options = value.into(),
      ("input", "kb_repeat_rate") => {
        config.input.repeat_rate = number(value, line_number)?;
      }
      ("input", "kb_repeat_delay") => {
        config.input.repeat_delay = number(value, line_number)?;
      }
      ("input", "kb_numlock_default_state") => {
        config.input.numlock_default = boolean(value, line_number)?;
      }
      ("input", "kb_capslock_default_state") => {
        config.input.capslock_default = boolean(value, line_number)?;
      }
      ("input", "mouse_scroll_speed") => {
        config.input.mouse_scroll_speed = number(value, line_number)?;
      }
      ("input", "mouse_natural_scroll") => {
        config.input.mouse_natural_scroll = boolean(value, line_number)?;
      }
      ("input", "left_handed_mode") => {
        config.input.left_handed_mode = boolean(value, line_number)?;
      }
      ("input", "middle_emulation") => {
        config.input.middle_emulation = boolean(value, line_number)?;
      }
      ("input", "mouse_accel_profile") => {
        config.input.mouse_accel_profile = value.into();
      }
      ("input", "mouse_cursor_speed") => {
        config.input.mouse_cursor_speed = number(value, line_number)?;
      }
      ("input", "touchpad_accel_profile") => {
        config.input.touchpad_accel_profile = value.into();
      }
      ("input", "touchpad_cursor_speed") => {
        config.input.touchpad_cursor_speed = number(value, line_number)?;
      }
      ("input", "tap_to_click") => {
        config.input.tap_to_click = boolean(value, line_number)?;
      }
      ("input", "click_method") => config.input.click_method = value.into(),
      ("input", "scroll_method") => config.input.scroll_method = value.into(),
      ("input", "natural_scroll") => {
        config.input.natural_scroll = boolean(value, line_number)?;
      }
      ("input", "disable_touchpad_while_typing") => {
        config.input.disable_touchpad_while_typing =
          boolean(value, line_number)?;
      }
      ("input", "disable_touchpad_while_mouse") => {
        config.input.disable_touchpad_while_mouse =
          boolean(value, line_number)?;
      }
      ("input", "tap_and_drag") => {
        config.input.tap_and_drag = boolean(value, line_number)?;
      }
      ("input", "drag_lock") => {
        config.input.drag_lock = boolean(value, line_number)?;
      }
      ("input", "cursor_theme") => config.input.cursor_theme = value.into(),
      ("input", "cursor_size") => {
        config.input.cursor_size = number(value, line_number)?
      }
      ("simple-tile", "key_lock") | ("idle", "key_lock") => {
        config.keybindings.lock = binding(value)?;
      }
      ("idle", "toggle") => config.keybindings.idle_toggle = binding(value)?,
      ("shortcuts-inhibit", "break_grab") => {
        config.keybindings.break_grab = binding(value)?;
      }
      ("shortcuts-inhibit", "inhibit_by_default") => {
        config.shortcuts.inhibit_by_default = value.to_owned().try_into()?;
      }
      ("shortcuts-inhibit", "ignore_views") => {
        config.shortcuts.ignore_views = value.to_owned().try_into()?;
      }
      ("idle", "disable_on_fullscreen") => {
        config.idle.disable_on_fullscreen = boolean(value, line_number)?;
      }
      ("idle", "disable_initially") => {
        config.idle.disable_initially = boolean(value, line_number)?;
      }
      ("idle", "screensaver_timeout") => {
        config.idle.idle_seconds = optional_seconds(value, line_number)?;
      }
      ("idle", "dpms_timeout") => {
        config.idle.dpms_seconds = optional_seconds(value, line_number)?;
      }
      ("idle", "lock_timeout") => {
        config.idle.lock_seconds = optional_seconds(value, line_number)?;
      }
      ("expo", key) if key.starts_with("select_workspace_") => {
        let workspace = key[17..].parse().map_err(|_| {
          ConfigError::Invalid(format!(
            "line {line_number}: invalid Expo workspace {key:?}"
          ))
        })?;
        config
          .expo
          .workspace_bindings
          .push(crate::ExpoWorkspaceBinding {
            workspace,
            binding: binding(value)?,
          });
      }
      ("expo", "toggle") => config.keybindings.expo_toggle = binding(value)?,
      ("expo", "background") => {
        config.expo.background = crate::parse_color(value)?
      }
      ("expo", "duration") => {
        config.expo.duration_ms = duration_ms(value, line_number)?
      }
      ("expo", "transition_length") => {
        config.expo.transition_length_ms = number(value, line_number)?
      }
      ("expo", "offset") => config.expo.offset = number(value, line_number)?,
      ("expo", "keyboard_interaction") => {
        config.expo.keyboard_interaction = boolean(value, line_number)?
      }
      ("expo", "inactive_brightness") => {
        config.expo.inactive_brightness = number(value, line_number)?;
      }
      ("input-method-v1", "enable_text_input_v3") => {
        config.input.enable_text_input_v3 = boolean(value, line_number)?
      }
      ("scale", "toggle") => config.keybindings.scale_toggle = binding(value)?,
      ("scale", "toggle_all") => {
        config.keybindings.scale_toggle_all = binding(value)?
      }
      ("scale", "duration") => {
        config.scale.duration_ms = duration_ms(value, line_number)?
      }
      ("scale", "title_overlay") => {
        config.scale.title_overlay = value.to_lowercase()
      }
      ("scale", "title_font_size") => {
        config.scale.title_font_size = number(value, line_number)?
      }
      ("scale", "title_position") => {
        config.scale.title_position = value.to_lowercase()
      }
      ("scale", "bg_color") => {
        config.scale.title_bg_color = crate::parse_color(value)?
      }
      ("scale", "text_color") => {
        config.scale.title_text_color = crate::parse_color(value)?
      }
      ("scale-title-filter", "case_sensitive") => {
        config.scale_title_filter.case_sensitive = boolean(value, line_number)?
      }
      ("scale-title-filter", "share_filter") => {
        config.scale_title_filter.share_filter = boolean(value, line_number)?
      }
      ("scale-title-filter", "overlay") => {
        config.scale_title_filter.overlay = boolean(value, line_number)?
      }
      ("scale-title-filter", "font_size") => {
        config.scale_title_filter.font_size = number(value, line_number)?
      }
      ("scale-title-filter", "bg_color") => {
        config.scale_title_filter.bg_color = crate::parse_color(value)?
      }
      ("scale-title-filter", "text_color") => {
        config.scale_title_filter.text_color = crate::parse_color(value)?
      }
      ("scale", "spacing") => {
        config.scale.spacing = number(value, line_number)?
      }
      ("scale", "outer_margin") => {
        config.scale.outer_margin = number(value, line_number)?
      }
      ("scale", "inactive_alpha") => {
        config.scale.inactive_alpha = number(value, line_number)?;
      }
      ("scale", "minimized_alpha") => {
        config.scale.minimized_alpha = number(value, line_number)?;
      }
      ("scale", "include_minimized") => {
        config.scale.include_minimized = boolean(value, line_number)?
      }
      ("scale", "close_on_new_view") => {
        config.scale.close_on_new_view = boolean(value, line_number)?
      }
      ("scale", "allow_zoom") => {
        config.scale.allow_zoom = boolean(value, line_number)?
      }
      ("scale", "middle_click_close") => {
        config.scale.middle_click_close = boolean(value, line_number)?
      }
      ("animate", "enabled_for") => {
        config.animate.enabled_for = value.to_owned().try_into()?;
      }
      ("animate", "open_animation") => {
        config.animate.open_animation = value.to_lowercase();
      }
      ("animate", "close_animation") => {
        config.animate.close_animation = value.to_lowercase();
      }
      ("animate", "minimize_animation") => {
        config.animate.minimize_animation = value.to_lowercase();
      }
      ("animate", "duration") => {
        config.animate.duration_ms = duration_ms(value, line_number)?;
      }
      ("fast-switcher", "activate") => {
        config.keybindings.fast_switcher_next = binding(value)?;
      }
      ("fast-switcher", "activate_backward") => {
        config.keybindings.fast_switcher_previous = binding(value)?;
      }
      ("fast-switcher", "inactive_alpha") => {
        config.fast_switcher.inactive_alpha = number(value, line_number)?;
      }
      ("alpha", "modifier") => config.alpha.modifier = value.to_lowercase(),
      ("alpha", "min_value") => {
        config.alpha.min_value = number(value, line_number)?
      }
      ("invert", "toggle") => {
        config.keybindings.invert_toggle = binding(value)?
      }
      ("invert", "preserve_hue") => {
        config.effects.invert_preserve_hue = boolean(value, line_number)?;
      }
      ("wrot", "activate") => {
        config.pointer.wrot_activate = binding(value)?;
      }
      ("wrot", "reset") => {
        config.keybindings.wrot_reset = binding(value)?;
      }
      ("wrot", "reset-one") => {
        config.keybindings.wrot_reset_one = binding(value)?;
      }
      ("wrot", "reset_radius") => {
        config.effects.wrot_reset_radius = number(value, line_number)?;
      }
      ("zoom", "modifier") => config.zoom.modifier = value.to_lowercase(),
      ("zoom", "speed") => config.zoom.speed = number(value, line_number)?,
      ("zoom", "smoothing_duration") => {
        config.zoom.smoothing_ms = duration_ms(value, line_number)?;
      }
      ("zoom", "interpolation_method") => {
        config.zoom.interpolation = match number::<u32>(value, line_number)? {
          0 => "linear".into(),
          1 => "nearest".into(),
          _ => {
            return Err(ConfigError::Invalid(format!(
              "line {line_number}: unknown zoom interpolation"
            )));
          }
        };
      }
      ("place", "mode") => config.placement.mode = value.to_lowercase(),
      ("xdg-activation", "check_surface") => {
        config.activation.check_surface = boolean(value, line_number)?;
      }
      ("xdg-activation", "only_last_request") => {
        config.activation.only_last_request = boolean(value, line_number)?;
      }
      ("xdg-activation", "timeout") => {
        config.activation.timeout_seconds = number(value, line_number)?;
      }
      ("move", "enable_snap") => {
        config.grid.enable_snap = boolean(value, line_number)?;
      }
      ("move", "snap_threshold") => {
        config.grid.snap_threshold = number(value, line_number)?;
      }
      ("move", "quarter_snap_threshold") => {
        config.grid.quarter_snap_threshold = number(value, line_number)?;
      }
      ("move", "enable_snap_off") => {
        config.grid.enable_snap_off = boolean(value, line_number)?;
      }
      ("move", "snap_off_threshold") => {
        config.grid.snap_off_threshold = number(value, line_number)?;
      }
      ("move", "workspace_switch_after") => {
        config.grid.workspace_switch_after = signed_number(value, line_number)?;
      }
      ("move", "join_views") => {
        config.grid.join_views = boolean(value, line_number)?;
      }
      ("workarounds", "all_dialogs_modal") => {
        config.behavior.all_dialogs_modal = boolean(value, line_number)?;
      }
      ("workarounds", "keep_last_toplevel_activated") => {
        config.behavior.keep_last_toplevel_activated =
          boolean(value, line_number)?;
      }
      ("workarounds", "discard_command_output") => {
        config.behavior.discard_command_output = boolean(value, line_number)?;
      }
      ("workarounds", "max_buffer_size") => {
        config.behavior.max_buffer_size = number(value, line_number)?
      }
      ("workarounds", "force_preferred_decoration_mode") => {
        config.behavior.force_preferred_decoration_mode =
          boolean(value, line_number)?
      }
      ("security-context-v1", "privileged_protocols") => {
        config.security_context.privileged_protocols = value.to_owned()
      }
      ("workarounds", "disable_primary_selection") => {
        config.behavior.disable_primary_selection = boolean(value, line_number)?
      }
      ("workarounds", "focus_main_surface_instead_of_popup") => {
        config.behavior.focus_main_surface_instead_of_popup =
          boolean(value, line_number)?
      }
      ("core", "preferred_decoration_mode") => {
        config.behavior.preferred_decoration_mode = value.to_lowercase()
      }
      ("core", "xwayland") => {
        config.xwayland.mode = value.parse().map_err(|_| {
          ConfigError::Invalid(format!(
            "line {line_number}: [core] xwayland must be true, false or \
             lazy, not {value:?}"
          ))
        })?;
      }
      ("core", "xwayland_startup_script") => {
        config.xwayland.startup_script = value.to_owned()
      }
      ("workarounds", "auto_reload_config") => {
        config.behavior.auto_reload = boolean(value, line_number)?;
      }
      ("workarounds", "config_reload_delay") => {
        config.behavior.config_reload_delay = number(value, line_number)?;
      }
      ("core", "background_color") => {
        config.background_color = crate::parse_color(value)?;
      }
      ("core", "focus_button_with_modifiers") => {
        config.focus.button_with_modifiers = boolean(value, line_number)?;
      }
      ("core", "focus_buttons") => config.focus.buttons = bindings(value)?,
      ("core", "focus_buttons_passthrough") => {
        config.focus.buttons_passthrough = boolean(value, line_number)?;
      }
      ("core", "close_top_view") => {
        let mut alternatives = bindings(value)?;
        config.keybindings.close = if alternatives.is_empty() {
          Binding::disabled()
        } else {
          alternatives.remove(0)
        };
        config.keybindings.close_alternatives = alternatives;
      }
      ("core", "exit") => config.keybindings.quit = binding(value)?,
      ("vswitch", "duration") => {
        config.vswitch.duration_ms = duration_ms(value, line_number)?
      }
      ("vswitch", "gap") => config.vswitch.gap = number(value, line_number)?,
      ("vswitch", "background") => {
        config.vswitch.background = crate::parse_color(value)?
      }
      ("vswitch", "wraparound") => {
        config.workspaces.wraparound = value.to_lowercase();
      }
      ("vswitch", "binding_right") => {
        config.keybindings.workspace_next = binding(value)?
      }
      ("vswitch", "binding_left") => {
        config.keybindings.workspace_previous = binding(value)?;
      }
      ("vswitch", "binding_up") => {
        config.keybindings.workspace_up = binding(value)?
      }
      ("vswitch", "binding_down") => {
        config.keybindings.workspace_down = binding(value)?
      }
      ("vswitch", "with_win_right") => {
        config.keybindings.workspace_move_next = binding(value)?;
      }
      ("vswitch", "with_win_left") => {
        config.keybindings.workspace_move_previous = binding(value)?;
      }
      ("vswitch", "with_win_up") => {
        config.keybindings.workspace_move_up = binding(value)?;
      }
      ("vswitch", "with_win_down") => {
        config.keybindings.workspace_move_down = binding(value)?;
      }
      ("vswitch", "send_win_right") => {
        config.keybindings.workspace_send_next = binding(value)?;
      }
      ("vswitch", "send_win_left") => {
        config.keybindings.workspace_send_previous = binding(value)?;
      }
      ("vswitch", "send_win_up") => {
        config.keybindings.workspace_send_up = binding(value)?;
      }
      ("vswitch", "send_win_down") => {
        config.keybindings.workspace_send_down = binding(value)?;
      }
      ("vswitch", "send_win_last") => {
        config.keybindings.workspace_send_last = binding(value)?;
      }
      ("core", "vwidth") => {
        config.workspaces.width = number(value, line_number)?
      }
      ("core", "vheight") => {
        config.workspaces.height = number(value, line_number)?
      }
      ("vswitch", "binding_last") => {
        config.keybindings.workspace_last = binding(value)?
      }
      ("vswitch", "with_win_last") => {
        config.keybindings.workspace_move_last = binding(value)?;
      }
      ("autostart", "autostart_wf_shell") => {}
      ("autostart", _) if !value.is_empty() => {
        config.autostart.push(value.to_owned());
      }
      ("session", "restore") => {
        config.session.restore = boolean(value, line_number)?;
      }
      ("session", "path") => {
        config.session.path = Some(value.into());
      }
      ("window-rules", _) => {
        config.window_rules.push(value.to_owned().try_into()?);
      }
      ("command", key) if key.starts_with("repeatable_binding_") => {
        let name = key[19..].to_owned();
        command_bindings.insert(name.clone(), value.to_owned());
        command_options.insert(name, (true, false, false));
      }
      ("command", key) if key.starts_with("release_binding_") => {
        let name = key[16..].to_owned();
        command_bindings.insert(name.clone(), value.to_owned());
        command_options.insert(name, (false, true, false));
      }
      ("command", key) if key.starts_with("always_binding_") => {
        let name = key[15..].to_owned();
        command_bindings.insert(name.clone(), value.to_owned());
        command_options.insert(name, (false, false, true));
      }
      ("command", key) if key.starts_with("binding_") => {
        let name = key[8..].to_owned();
        command_bindings.insert(name.clone(), value.to_owned());
        command_options.insert(name, (false, false, false));
      }
      ("command", key) if key.starts_with("command_") => {
        commands.insert(key[8..].to_owned(), value.to_owned());
      }
      _ if section == "output" || section.starts_with("output:") => {
        let output = match section_name[6..].strip_prefix(':').map(str::trim) {
          Some(name) if !name.is_empty() => config.named_output(name),
          _ => &mut config.output,
        };
        output_option(output, &key, value, line_number)?;
      }
      _ => {}
    }
  }
  for (name, command) in commands {
    if let Some(binding_value) = command_bindings.get(&name) {
      let (repeatable, on_release, always) =
        command_options.get(&name).copied().unwrap_or_default();
      config.commands.push(crate::CommandBinding {
        binding: binding(binding_value)?,
        command,
        repeatable,
        on_release,
        always,
      });
    }
  }
  config.validate()?;
  Ok(config)
}

/// Reads one option of an `[output:NAME]` section into `output`. Unknown
/// options are ignored, like every option Shingle does not import.
fn output_option(
  output: &mut crate::OutputConfig,
  key: &str,
  value: &str,
  line_number: usize,
) -> Result<(), ConfigError> {
  match key {
    // Shingle lights one output and does not mirror, so an output set to
    // mirror another stays dark rather than competing with its source.
    "mode" if value.starts_with("mirror") => {
      output.mode = crate::OutputMode::Off;
    }
    "mode" => {
      output.mode = value.parse().map_err(|_| {
        ConfigError::Invalid(format!(
          "line {line_number}: invalid output mode {value:?}"
        ))
      })?;
    }
    "scale" => output.scale = number(value, line_number)?,
    "transform" => output.transform = value.to_lowercase(),
    "vrr" => output.vrr = boolean(value, line_number)?,
    "position" => {
      let position = value
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
      if position.len() != 2 {
        return Err(ConfigError::Invalid(format!(
          "line {line_number}: invalid output position"
        )));
      }
      output.x = signed_number(position[0], line_number)?;
      output.y = signed_number(position[1], line_number)?;
    }
    _ => {}
  }
  Ok(())
}

fn strip_comment(line: &str) -> &str {
  for (index, _) in line.match_indices('#') {
    if index == 0 {
      return "";
    }
    let prefix = &line[..index];
    if !prefix.ends_with(char::is_whitespace) {
      continue;
    }
    let suffix = &line[index + 1..];
    let token = suffix.split_whitespace().next().unwrap_or("");
    let hexadecimal_color = prefix.contains('=')
      && matches!(token.len(), 6 | 8)
      && token.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !hexadecimal_color {
      return prefix;
    }
  }
  line
}

fn optional_seconds(
  value: &str,
  line: usize,
) -> Result<Option<u64>, ConfigError> {
  let seconds: i64 = signed_number(value, line)?;
  Ok((seconds >= 0).then_some(seconds as u64))
}

fn duration_ms(value: &str, line: usize) -> Result<u64, ConfigError> {
  let value = value.split_whitespace().next().unwrap_or(value);
  if let Some(seconds) =
    value.strip_suffix('s').filter(|_| !value.ends_with("ms"))
  {
    let seconds: f64 = number(seconds, line)?;
    if !seconds.is_finite()
      || seconds < 0.0
      || seconds * 1000.0 > u64::MAX as f64
    {
      return Err(ConfigError::Invalid(format!(
        "line {line}: invalid animation duration {value:?}"
      )));
    }
    return Ok((seconds * 1000.0).round() as u64);
  }
  number(value.strip_suffix("ms").unwrap_or(value), line)
}

fn bindings(value: &str) -> Result<Vec<Binding>, ConfigError> {
  if value.trim().is_empty() {
    return Ok(Vec::new());
  }
  value.split('|').map(binding).collect()
}

fn binding(value: &str) -> Result<Binding, ConfigError> {
  if value.trim().is_empty() || value.trim() == "none" {
    return Ok(Binding::disabled());
  }
  let mut modifiers = Vec::new();
  let mut key = None;
  for part in value.replace("><", "> <").split_whitespace() {
    match part.to_lowercase().as_str() {
      "<super>" | "<logo>" => modifiers.push(Modifier::Super),
      "<ctrl>" | "<control>" => modifiers.push(Modifier::Control),
      "<alt>" => modifiers.push(Modifier::Alt),
      "<shift>" => modifiers.push(Modifier::Shift),
      _ => key = Some(normalize_key(part)),
    }
  }
  let key = key
    .filter(|key| !key.is_empty() && key != "xkb:")
    .ok_or_else(|| ConfigError::Binding(value.to_owned()))?;
  Ok(Binding::new(key, modifiers))
}

pub(crate) fn normalize_key(key: &str) -> String {
  if key
    .get(..4)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("xkb:"))
  {
    return format!("xkb:{}", &key[4..]);
  }
  let key = key.strip_prefix("KEY_").unwrap_or(key).to_lowercase();
  let key = key
    .strip_prefix("kp_")
    .map(|suffix| format!("kp{suffix}"))
    .unwrap_or(key);
  match key.as_str() {
    "return" => "enter".into(),
    "esc" => "escape".into(),
    _ => key,
  }
}

fn number<T: std::str::FromStr>(
  value: &str,
  line: usize,
) -> Result<T, ConfigError> {
  value.parse().map_err(|_| {
    ConfigError::Invalid(format!("line {line}: invalid number {value:?}"))
  })
}

fn signed_number<T: std::str::FromStr>(
  value: &str,
  line: usize,
) -> Result<T, ConfigError> {
  number(value, line)
}

fn boolean(value: &str, line: usize) -> Result<bool, ConfigError> {
  match value.to_lowercase().as_str() {
    "true" | "yes" | "on" | "1" => Ok(true),
    "false" | "no" | "off" | "0" => Ok(false),
    _ => Err(ConfigError::Invalid(format!(
      "line {line}: invalid boolean {value:?}"
    ))),
  }
}
