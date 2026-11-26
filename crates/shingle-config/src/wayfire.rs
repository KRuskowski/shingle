//! The Wayfire `wayfire.ini` importer ([`crate::ConfigFormat::Wayfire`]).

use std::collections::BTreeMap;

use crate::{
  Binding, ConfigError, ConfigWarning, ShingleConfig, XwaylandMode,
  import::{self, Import},
  ini::{self, Entry},
};

/// The sections the importer reads, or whose plugin Shingle implements
/// without options. Any other section's options are reported once per
/// section. `output:NAME` and `input-device:NAME` are matched by prefix.
const SECTIONS: &[&str] = &[
  "alpha",
  "animate",
  "autostart",
  "command",
  "core",
  "expo",
  "ext-toplevel",
  "ext-workspace",
  "extra-gestures",
  "fast-switcher",
  "foreign-toplevel",
  "grid",
  "idle",
  "input",
  "input-device",
  "input-method-v1",
  "invert",
  "ipc",
  "ipc-rules",
  "move",
  "oswitch",
  "output",
  "place",
  "preserve-output",
  "resize",
  "scale",
  "scale-title-filter",
  "security-context-v1",
  "session",
  "session-lock",
  "shortcuts-inhibit",
  "simple-tile",
  "switcher",
  "vswitch",
  "window-rules",
  "wm-actions",
  "workarounds",
  "wrot",
  "wsets",
  "xdg-activation",
  "zoom",
];

/// Whether the importer reads the section `name` (lower case).
fn supported(name: &str) -> bool {
  SECTIONS.contains(&name)
    || name.starts_with("output:")
    || name.starts_with("input-device:")
}

/// A `[command]` binding, waiting for its `command_NAME`.
struct CommandEntry {
  value: String,
  line: usize,
  context: String,
  repeatable: bool,
  on_release: bool,
  always: bool,
}

/// The `[command]` prefixes for bindings: the name, and whether the
/// binding repeats, runs on release or always runs.
const COMMAND_PREFIXES: &[(&str, bool, bool, bool)] = &[
  ("repeatable_binding_", true, false, false),
  ("release_binding_", false, true, false),
  ("always_binding_", false, false, true),
  ("binding_", false, false, false),
];

/// An unsupported section: the line of its first header with options, and
/// how many options it holds.
struct Unsupported {
  name: String,
  line: usize,
  options: usize,
}

pub(crate) fn parse(
  text: &str,
) -> Result<(ShingleConfig, Vec<ConfigWarning>), ConfigError> {
  let mut config = ShingleConfig::default();
  // Wayfire starts Xwayland eagerly unless told otherwise
  // (metadata/core.xml), unlike Shingle's own default.
  config.xwayland.mode = XwaylandMode::Eager;
  let mut importer = Import::new(config);
  let mut command_bindings = BTreeMap::<String, CommandEntry>::new();
  let mut commands = BTreeMap::new();
  let mut unsupported: Vec<Unsupported> = Vec::new();
  // The section name as written: connector names keep their case.
  let mut section_name: Option<String> = None;
  let mut section_line = 0;
  for line in ini::lines(text) {
    let (name, value) = match ini::entry(&line.text) {
      Entry::Section(name) => {
        section_name = Some(name.trim().to_owned());
        section_line = line.number;
        continue;
      }
      Entry::Malformed => {
        importer.warn(
          Some(line.number),
          "not a [section] or an option = value line; ignored".into(),
        );
        continue;
      }
      Entry::Option { name, value } => (name, value),
    };
    let Some(section_name) = section_name.as_deref() else {
      importer.warn(
        Some(line.number),
        format!("option {name:?} comes before any [section]; ignored"),
      );
      continue;
    };
    let section = section_name.to_lowercase();
    if !supported(&section) {
      match unsupported.iter_mut().find(|entry| entry.name == section) {
        Some(entry) => entry.options += 1,
        None => unsupported.push(Unsupported {
          name: section,
          line: section_line,
          options: 1,
        }),
      }
      continue;
    }
    let key = name.to_lowercase();
    let context = format!("[{section_name}] {name}");
    if section == "command" {
      if let Some(command) = key.strip_prefix("command_") {
        commands.insert(command.to_owned(), value.to_owned());
        continue;
      }
      if let Some((suffix, &(_, repeatable, on_release, always))) =
        COMMAND_PREFIXES.iter().find_map(|entry| {
          key.strip_prefix(entry.0).map(|suffix| (suffix, entry))
        })
      {
        command_bindings.insert(
          suffix.to_owned(),
          CommandEntry {
            value: value.to_owned(),
            line: line.number,
            context,
            repeatable,
            on_release,
            always,
          },
        );
        continue;
      }
    }
    importer.option(line.number, &context, |config, notes| {
      apply(config, &section, section_name, &key, value, notes)
    });
  }
  for (name, command) in commands {
    let Some(entry) = command_bindings.remove(&name) else {
      continue;
    };
    importer.option(entry.line, &entry.context, |config, notes| {
      config.commands.push(crate::CommandBinding {
        binding: import::binding(&entry.value, notes)?,
        command,
        repeatable: entry.repeatable,
        on_release: entry.on_release,
        always: entry.always,
      });
      Ok(())
    });
  }
  for section in unsupported {
    let options = match section.options {
      1 => "its option is".to_owned(),
      count => format!("its {count} options are"),
    };
    importer.warn(
      Some(section.line),
      format!(
        "[{}] belongs to a plugin Shingle does not have; {options} ignored",
        section.name
      ),
    );
  }
  importer.finish()
}

/// Applies one option of a supported section. `section` and `key` are lower
/// case; `section_name` is the section as written.
fn apply(
  config: &mut ShingleConfig,
  section: &str,
  section_name: &str,
  key: &str,
  value: &str,
  notes: &mut Vec<String>,
) -> Result<(), ConfigError> {
  match (section, key) {
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
      config.tiling.preview_border_width = number(value)?;
    }
    ("simple-tile", "animation_duration") => {
      config.tiling.animation_duration = duration_ms(value)?;
    }
    ("simple-tile", "inner_gap_size") => {
      config.tiling.gaps.inner = number(value)?;
    }
    ("simple-tile", "keep_fullscreen_on_adjacent") => {
      config.tiling.keep_fullscreen_on_adjacent = boolean(value)?;
    }
    ("simple-tile", "outer_horiz_gap_size") => {
      let gap = number(value)?;
      config.tiling.gaps.left = gap;
      config.tiling.gaps.right = gap;
    }
    ("simple-tile", "outer_vert_gap_size") => {
      let gap = number(value)?;
      config.tiling.gaps.top = gap;
      config.tiling.gaps.bottom = gap;
    }
    ("simple-tile", "key_focus_left") => {
      config.keybindings.focus_left = import::binding(value, notes)?;
    }
    ("simple-tile", "key_focus_right") => {
      config.keybindings.focus_right = import::binding(value, notes)?;
    }
    ("simple-tile", "key_focus_above") => {
      config.keybindings.focus_up = import::binding(value, notes)?;
    }
    ("simple-tile", "key_focus_below") => {
      config.keybindings.focus_down = import::binding(value, notes)?;
    }
    ("simple-tile", "key_toggle_maximized") => {
      config.keybindings.toggle_maximized = import::binding(value, notes)?;
    }
    ("switcher", "next_view") => {
      config.keybindings.focus_next = import::binding(value, notes)?
    }
    ("switcher", "prev_view") => {
      config.keybindings.focus_previous = import::binding(value, notes)?;
    }
    ("wm-actions", "toggle_fullscreen") => {
      config.keybindings.toggle_fullscreen = import::binding(value, notes)?;
    }
    ("wm-actions", "toggle_sticky") => {
      config.keybindings.toggle_sticky = import::binding(value, notes)?;
    }
    ("wm-actions", "toggle_always_on_top") => {
      config.keybindings.always_on_top = import::binding(value, notes)?;
    }
    ("wm-actions", "toggle_always_on_bottom") => {
      config.keybindings.always_on_bottom = import::binding(value, notes)?;
    }
    ("wm-actions", "toggle_maximize") => {
      config.keybindings.toggle_maximized = import::binding(value, notes)?;
    }
    ("wm-actions", "minimize") => {
      config.keybindings.minimize = import::binding(value, notes)?
    }
    ("wm-actions", "bring_to_front") => {
      config.keybindings.bring_to_front = import::binding(value, notes)?;
    }
    ("wm-actions", "send_to_back") => {
      config.keybindings.send_to_back = import::binding(value, notes)?;
    }
    ("wm-actions", "toggle_showdesktop") => {
      config.keybindings.show_desktop = import::binding(value, notes)?;
    }
    ("simple-tile", "key_toggle") => {
      config.keybindings.toggle_tiling = import::binding(value, notes)?;
    }
    ("simple-tile", "key_move_left") => {
      config.keybindings.move_left = import::binding(value, notes)?;
    }
    ("simple-tile", "key_move_right") => {
      config.keybindings.move_right = import::binding(value, notes)?;
    }
    ("simple-tile", "key_move_above") => {
      config.keybindings.move_up = import::binding(value, notes)?;
    }
    ("simple-tile", "key_move_below") => {
      config.keybindings.move_down = import::binding(value, notes)?;
    }
    ("simple-tile", "key_resize_left") => {
      config.keybindings.resize_left = import::binding(value, notes)?;
    }
    ("simple-tile", "key_resize_right") => {
      config.keybindings.resize_right = import::binding(value, notes)?;
    }
    ("simple-tile", "key_resize_above") => {
      config.keybindings.resize_up = import::binding(value, notes)?;
    }
    ("simple-tile", "key_resize_below") => {
      config.keybindings.resize_down = import::binding(value, notes)?;
    }
    ("simple-tile", "button_move") => {
      config.pointer.move_button = import::binding(value, notes)?;
    }
    ("simple-tile", "button_resize") | ("resize", "activate") => {
      config.pointer.resize_button = import::binding(value, notes)?;
    }
    ("move", "activate") => {
      config.pointer.move_button = import::binding(value, notes)?
    }
    ("resize", "activate_preserve_aspect") => {
      config.pointer.resize_preserve_aspect_button =
        import::binding(value, notes)?;
    }
    ("resize", "min_width") => config.resize.min_width = number(value)?,
    ("resize", "min_height") => config.resize.min_height = number(value)?,
    ("resize", "corner_threshold") => {
      config.resize.corner_threshold = number(value)?;
    }
    ("grid", "slot_tl") => {
      config.keybindings.grid_slot_top_left = import::bindings(value, notes)?
    }
    ("grid", "slot_t") => {
      config.keybindings.grid_slot_top = import::bindings(value, notes)?
    }
    ("grid", "slot_tr") => {
      config.keybindings.grid_slot_top_right = import::bindings(value, notes)?
    }
    ("grid", "slot_l") => {
      config.keybindings.grid_slot_left = import::bindings(value, notes)?
    }
    ("grid", "slot_c") => {
      config.keybindings.grid_slot_center = import::bindings(value, notes)?
    }
    ("grid", "slot_r") => {
      config.keybindings.grid_slot_right = import::bindings(value, notes)?
    }
    ("grid", "slot_bl") => {
      config.keybindings.grid_slot_bottom_left = import::bindings(value, notes)?
    }
    ("grid", "slot_b") => {
      config.keybindings.grid_slot_bottom = import::bindings(value, notes)?
    }
    ("grid", "slot_br") => {
      config.keybindings.grid_slot_bottom_right =
        import::bindings(value, notes)?
    }
    ("grid", "restore") => {
      config.keybindings.grid_restore = import::bindings(value, notes)?
    }
    ("extra-gestures", "move_fingers") => {
      config.extra_gestures.move_fingers = number(value)?;
    }
    ("extra-gestures", "move_delay") => {
      config.extra_gestures.move_delay = number(value)?;
    }
    ("extra-gestures", "close_fingers") => {
      config.extra_gestures.close_fingers = number(value)?;
    }
    ("input", "xkb_rules") => config.input.rules = value.into(),
    ("input", "xkb_file") => config.input.xkb_file = value.into(),
    ("input", "xkb_model") => config.input.model = value.into(),
    ("input", "xkb_layout") => config.input.layout = value.into(),
    ("input", "xkb_variant") => config.input.variant = value.into(),
    ("input", "xkb_options") => config.input.options = value.into(),
    ("input", "kb_repeat_rate") => {
      config.input.repeat_rate = number(value)?;
    }
    ("input", "kb_repeat_delay") => {
      config.input.repeat_delay = number(value)?;
    }
    ("input", "kb_numlock_default_state") => {
      config.input.numlock_default = boolean(value)?;
    }
    ("input", "kb_capslock_default_state") => {
      config.input.capslock_default = boolean(value)?;
    }
    ("input", "mouse_scroll_speed") => {
      config.input.mouse_scroll_speed = number(value)?;
    }
    ("input", "mouse_natural_scroll") => {
      config.input.mouse_natural_scroll = boolean(value)?;
    }
    ("input", "left_handed_mode") => {
      config.input.left_handed_mode = boolean(value)?;
    }
    ("input", "middle_emulation") => {
      config.input.middle_emulation = boolean(value)?;
    }
    ("input", "mouse_accel_profile") => {
      config.input.mouse_accel_profile = value.into();
    }
    ("input", "mouse_cursor_speed") => {
      config.input.mouse_cursor_speed = number(value)?;
    }
    ("input", "touchpad_accel_profile") => {
      config.input.touchpad_accel_profile = value.into();
    }
    ("input", "touchpad_cursor_speed") => {
      config.input.touchpad_cursor_speed = number(value)?;
    }
    ("input", "tap_to_click") => {
      config.input.tap_to_click = boolean(value)?;
    }
    ("input", "click_method") => config.input.click_method = value.into(),
    ("input", "scroll_method") => config.input.scroll_method = value.into(),
    ("input", "natural_scroll") => {
      config.input.natural_scroll = boolean(value)?;
    }
    ("input", "disable_touchpad_while_typing") => {
      config.input.disable_touchpad_while_typing = boolean(value)?;
    }
    ("input", "disable_touchpad_while_mouse") => {
      config.input.disable_touchpad_while_mouse = boolean(value)?;
    }
    ("input", "tap_and_drag") => {
      config.input.tap_and_drag = boolean(value)?;
    }
    ("input", "drag_lock") => {
      config.input.drag_lock = boolean(value)?;
    }
    ("input", "cursor_theme") => config.input.cursor_theme = value.into(),
    ("input", "cursor_size") => config.input.cursor_size = number(value)?,
    ("simple-tile", "key_lock") | ("idle", "key_lock") => {
      config.keybindings.lock = import::binding(value, notes)?;
    }
    ("idle", "toggle") => {
      config.keybindings.idle_toggle = import::binding(value, notes)?
    }
    ("shortcuts-inhibit", "break_grab") => {
      config.keybindings.break_grab = import::binding(value, notes)?;
    }
    ("shortcuts-inhibit", "inhibit_by_default") => {
      config.shortcuts.inhibit_by_default = value.to_owned().try_into()?;
    }
    ("shortcuts-inhibit", "ignore_views") => {
      config.shortcuts.ignore_views = value.to_owned().try_into()?;
    }
    ("idle", "disable_on_fullscreen") => {
      config.idle.disable_on_fullscreen = boolean(value)?;
    }
    ("idle", "disable_initially") => {
      config.idle.disable_initially = boolean(value)?;
    }
    ("idle", "screensaver_timeout") => {
      config.idle.idle_seconds = optional_seconds(value)?;
    }
    ("idle", "dpms_timeout") => {
      config.idle.dpms_seconds = optional_seconds(value)?;
    }
    ("idle", "lock_timeout") => {
      config.idle.lock_seconds = optional_seconds(value)?;
    }
    ("expo", key) if key.starts_with("select_workspace_") => {
      let workspace = key[17..].parse().map_err(|_| {
        ConfigError::Invalid(format!("invalid Expo workspace {key:?}"))
      })?;
      config
        .expo
        .workspace_bindings
        .push(crate::ExpoWorkspaceBinding {
          workspace,
          binding: import::binding(value, notes)?,
        });
    }
    ("expo", "toggle") => {
      config.keybindings.expo_toggle = import::binding(value, notes)?
    }
    ("expo", "background") => {
      config.expo.background = crate::parse_color(value)?
    }
    ("expo", "duration") => config.expo.duration_ms = duration_ms(value)?,
    ("expo", "transition_length") => {
      config.expo.transition_length_ms = number(value)?
    }
    ("expo", "offset") => config.expo.offset = number(value)?,
    ("expo", "keyboard_interaction") => {
      config.expo.keyboard_interaction = boolean(value)?
    }
    ("expo", "inactive_brightness") => {
      config.expo.inactive_brightness = number(value)?;
    }
    ("input-method-v1", "enable_text_input_v3") => {
      config.input.enable_text_input_v3 = boolean(value)?
    }
    ("scale", "toggle") => {
      config.keybindings.scale_toggle = import::binding(value, notes)?
    }
    ("scale", "toggle_all") => {
      config.keybindings.scale_toggle_all = import::binding(value, notes)?
    }
    ("scale", "duration") => config.scale.duration_ms = duration_ms(value)?,
    ("scale", "title_overlay") => {
      config.scale.title_overlay = value.to_lowercase()
    }
    ("scale", "title_font_size") => {
      config.scale.title_font_size = number(value)?
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
      config.scale_title_filter.case_sensitive = boolean(value)?
    }
    ("scale-title-filter", "share_filter") => {
      config.scale_title_filter.share_filter = boolean(value)?
    }
    ("scale-title-filter", "overlay") => {
      config.scale_title_filter.overlay = boolean(value)?
    }
    ("scale-title-filter", "font_size") => {
      config.scale_title_filter.font_size = number(value)?
    }
    ("scale-title-filter", "bg_color") => {
      config.scale_title_filter.bg_color = crate::parse_color(value)?
    }
    ("scale-title-filter", "text_color") => {
      config.scale_title_filter.text_color = crate::parse_color(value)?
    }
    ("scale", "spacing") => config.scale.spacing = number(value)?,
    ("scale", "outer_margin") => config.scale.outer_margin = number(value)?,
    ("scale", "inactive_alpha") => {
      config.scale.inactive_alpha = number(value)?;
    }
    ("scale", "minimized_alpha") => {
      config.scale.minimized_alpha = number(value)?;
    }
    ("scale", "include_minimized") => {
      config.scale.include_minimized = boolean(value)?
    }
    ("scale", "close_on_new_view") => {
      config.scale.close_on_new_view = boolean(value)?
    }
    ("scale", "allow_zoom") => config.scale.allow_zoom = boolean(value)?,
    ("scale", "middle_click_close") => {
      config.scale.middle_click_close = boolean(value)?
    }
    ("animate", "enabled_for") => {
      config.animate.enabled_for = value.to_owned().try_into()?;
    }
    ("animate", "open_animation") => {
      config.animate.open_animation = import::animation(value, notes);
    }
    ("animate", "close_animation") => {
      config.animate.close_animation = import::animation(value, notes);
    }
    ("animate", "minimize_animation") => {
      config.animate.minimize_animation = import::animation(value, notes);
    }
    ("animate", "duration") => {
      config.animate.duration_ms = duration_ms(value)?;
    }
    ("fast-switcher", "activate") => {
      config.keybindings.fast_switcher_next = import::binding(value, notes)?;
    }
    ("fast-switcher", "activate_backward") => {
      config.keybindings.fast_switcher_previous =
        import::binding(value, notes)?;
    }
    ("fast-switcher", "inactive_alpha") => {
      config.fast_switcher.inactive_alpha = number(value)?;
    }
    ("alpha", "modifier") => config.alpha.modifier = value.to_lowercase(),
    ("alpha", "min_value") => config.alpha.min_value = number(value)?,
    ("invert", "toggle") => {
      config.keybindings.invert_toggle = import::binding(value, notes)?
    }
    ("invert", "preserve_hue") => {
      config.effects.invert_preserve_hue = boolean(value)?;
    }
    ("wrot", "activate") => {
      config.pointer.wrot_activate = import::binding(value, notes)?;
    }
    ("wrot", "reset") => {
      config.keybindings.wrot_reset = import::binding(value, notes)?;
    }
    ("wrot", "reset-one") => {
      config.keybindings.wrot_reset_one = import::binding(value, notes)?;
    }
    ("wrot", "reset_radius") => {
      config.effects.wrot_reset_radius = number(value)?;
    }
    ("zoom", "modifier") => config.zoom.modifier = value.to_lowercase(),
    ("zoom", "speed") => config.zoom.speed = number(value)?,
    ("zoom", "smoothing_duration") => {
      config.zoom.smoothing_ms = duration_ms(value)?;
    }
    ("zoom", "interpolation_method") => {
      config.zoom.interpolation = match number::<u32>(value)? {
        0 => "linear".into(),
        1 => "nearest".into(),
        _ => {
          return Err(ConfigError::Invalid(
            "unknown zoom interpolation".into(),
          ));
        }
      };
    }
    ("place", "mode") => config.placement.mode = value.to_lowercase(),
    ("xdg-activation", "check_surface") => {
      config.activation.check_surface = boolean(value)?;
    }
    ("xdg-activation", "only_last_request") => {
      config.activation.only_last_request = boolean(value)?;
    }
    ("xdg-activation", "timeout") => {
      config.activation.timeout_seconds = number(value)?;
    }
    ("move", "enable_snap") => {
      config.grid.enable_snap = boolean(value)?;
    }
    ("move", "snap_threshold") => {
      config.grid.snap_threshold = number(value)?;
    }
    ("move", "quarter_snap_threshold") => {
      config.grid.quarter_snap_threshold = number(value)?;
    }
    ("move", "enable_snap_off") => {
      config.grid.enable_snap_off = boolean(value)?;
    }
    ("move", "snap_off_threshold") => {
      config.grid.snap_off_threshold = number(value)?;
    }
    ("move", "workspace_switch_after") => {
      config.grid.workspace_switch_after = number(value)?;
    }
    ("move", "join_views") => {
      config.grid.join_views = boolean(value)?;
    }
    ("workarounds", "all_dialogs_modal") => {
      config.behavior.all_dialogs_modal = boolean(value)?;
    }
    ("workarounds", "keep_last_toplevel_activated") => {
      config.behavior.keep_last_toplevel_activated = boolean(value)?;
    }
    ("workarounds", "discard_command_output") => {
      config.behavior.discard_command_output = boolean(value)?;
    }
    ("workarounds", "max_buffer_size") => {
      config.behavior.max_buffer_size = number(value)?
    }
    ("workarounds", "force_preferred_decoration_mode") => {
      config.behavior.force_preferred_decoration_mode = boolean(value)?
    }
    ("security-context-v1", "privileged_protocols") => {
      config.security_context.privileged_protocols = value.to_owned()
    }
    ("workarounds", "disable_primary_selection") => {
      config.behavior.disable_primary_selection = boolean(value)?
    }
    ("workarounds", "focus_main_surface_instead_of_popup") => {
      config.behavior.focus_main_surface_instead_of_popup = boolean(value)?
    }
    ("core", "preferred_decoration_mode") => {
      config.behavior.preferred_decoration_mode = value.to_lowercase()
    }
    ("core", "xwayland") => {
      config.xwayland.mode = value.parse().map_err(|_| {
        ConfigError::Invalid(format!(
          "must be true, false or lazy, not {value:?}"
        ))
      })?;
    }
    ("core", "xwayland_startup_script") => {
      config.xwayland.startup_script = value.to_owned()
    }
    ("workarounds", "auto_reload_config") => {
      config.behavior.auto_reload = boolean(value)?;
    }
    ("workarounds", "config_reload_delay") => {
      config.behavior.config_reload_delay = number(value)?;
    }
    ("core", "background_color") => {
      config.background_color = crate::parse_color(value)?;
    }
    ("core", "focus_button_with_modifiers") => {
      config.focus.button_with_modifiers = boolean(value)?;
    }
    ("core", "focus_buttons") => {
      config.focus.buttons = import::bindings(value, notes)?
    }
    ("core", "focus_buttons_passthrough") => {
      config.focus.buttons_passthrough = boolean(value)?;
    }
    ("core", "close_top_view") => {
      let mut alternatives = import::bindings(value, notes)?;
      config.keybindings.close = if alternatives.is_empty() {
        Binding::disabled()
      } else {
        alternatives.remove(0)
      };
      config.keybindings.close_alternatives = alternatives;
    }
    ("core", "exit") => {
      config.keybindings.quit = import::binding(value, notes)?
    }
    ("vswitch", "duration") => config.vswitch.duration_ms = duration_ms(value)?,
    ("vswitch", "gap") => config.vswitch.gap = number(value)?,
    ("vswitch", "background") => {
      config.vswitch.background = crate::parse_color(value)?
    }
    ("vswitch", "wraparound") => {
      config.workspaces.wraparound = value.to_lowercase();
    }
    ("vswitch", "binding_right") => {
      config.keybindings.workspace_next = import::binding(value, notes)?
    }
    ("vswitch", "binding_left") => {
      config.keybindings.workspace_previous = import::binding(value, notes)?;
    }
    ("vswitch", "binding_up") => {
      config.keybindings.workspace_up = import::binding(value, notes)?
    }
    ("vswitch", "binding_down") => {
      config.keybindings.workspace_down = import::binding(value, notes)?
    }
    ("vswitch", "with_win_right") => {
      config.keybindings.workspace_move_next = import::binding(value, notes)?;
    }
    ("vswitch", "with_win_left") => {
      config.keybindings.workspace_move_previous =
        import::binding(value, notes)?;
    }
    ("vswitch", "with_win_up") => {
      config.keybindings.workspace_move_up = import::binding(value, notes)?;
    }
    ("vswitch", "with_win_down") => {
      config.keybindings.workspace_move_down = import::binding(value, notes)?;
    }
    ("vswitch", "send_win_right") => {
      config.keybindings.workspace_send_next = import::binding(value, notes)?;
    }
    ("vswitch", "send_win_left") => {
      config.keybindings.workspace_send_previous =
        import::binding(value, notes)?;
    }
    ("vswitch", "send_win_up") => {
      config.keybindings.workspace_send_up = import::binding(value, notes)?;
    }
    ("vswitch", "send_win_down") => {
      config.keybindings.workspace_send_down = import::binding(value, notes)?;
    }
    ("vswitch", "send_win_last") => {
      config.keybindings.workspace_send_last = import::binding(value, notes)?;
    }
    ("core", "vwidth") => config.workspaces.width = number(value)?,
    ("core", "vheight") => config.workspaces.height = number(value)?,
    ("vswitch", "binding_last") => {
      config.keybindings.workspace_last = import::binding(value, notes)?
    }
    ("vswitch", "with_win_last") => {
      config.keybindings.workspace_move_last = import::binding(value, notes)?;
    }
    ("autostart", "autostart_wf_shell") => {}
    ("autostart", _) if !value.is_empty() => {
      config.autostart.push(value.to_owned());
    }
    ("session", "restore") => {
      config.session.restore = boolean(value)?;
    }
    ("session", "path") => {
      config.session.path = Some(value.into());
    }
    ("window-rules", _) => {
      config.window_rules.push(value.to_owned().try_into()?);
    }
    ("core", "plugins") => {
      // Without preserve-output Wayfire moves an unplugged output's
      // windows for good; Shingle's default brings them back, so only
      // the plugin's own behaviour is imported.
      if value
        .split_whitespace()
        .any(|plugin| plugin == "preserve-output")
      {
        config.output_layout.unplug = crate::UnplugPolicy::Park;
      }
    }
    ("preserve-output", "last_output_focus_timeout") => {
      config.output_layout.last_output_focus_timeout = number(value)?;
    }
    ("oswitch", "next_output") => {
      config.oswitch.next_output = import::binding(value, notes)?;
    }
    ("oswitch", "next_output_with_win") => {
      config.oswitch.next_output_with_win = import::binding(value, notes)?;
    }
    ("oswitch", "prev_output") => {
      config.oswitch.prev_output = import::binding(value, notes)?;
    }
    ("oswitch", "prev_output_with_win") => {
      config.oswitch.prev_output_with_win = import::binding(value, notes)?;
    }
    ("oswitch", "left_output") => {
      config.oswitch.left_output = import::binding(value, notes)?;
    }
    ("oswitch", "right_output") => {
      config.oswitch.right_output = import::binding(value, notes)?;
    }
    ("oswitch", "above_output") => {
      config.oswitch.above_output = import::binding(value, notes)?;
    }
    ("oswitch", "below_output") => {
      config.oswitch.below_output = import::binding(value, notes)?;
    }
    ("wsets", "label_duration") => {
      config.wsets.label_duration = duration_ms(value)?;
    }
    ("wsets", key) if key.starts_with("send_to_wset_") => {
      config.wsets.send_window_bindings.push(crate::WsetBinding {
        wset: number(&key["send_to_wset_".len()..])?,
        binding: import::binding(value, notes)?,
      });
    }
    ("wsets", key) if key.starts_with("wset_") => {
      config.wsets.wsets_bindings.push(crate::WsetBinding {
        wset: number(&key["wset_".len()..])?,
        binding: import::binding(value, notes)?,
      });
    }
    ("workarounds", "remove_output_limits") => {
      config.behavior.remove_output_limits = boolean(value)?;
    }
    (_, "output") if section.starts_with("input-device:") => {
      let name = section_name["input-device:".len()..].trim();
      if !name.is_empty() && !value.is_empty() {
        config
          .input_devices
          .entry(name.to_owned())
          .or_default()
          .output = Some(value.to_owned());
      }
    }
    _ if section == "output" || section.starts_with("output:") => {
      let output = match section_name[6..].strip_prefix(':').map(str::trim) {
        Some(name) if !name.is_empty() => config.named_output(name),
        _ => &mut config.output,
      };
      output_option(output, key, value)?;
    }
    _ => {}
  }
  Ok(())
}

/// Reads one option of an `[output:NAME]` section into `output`. Unknown
/// options are ignored, like every option Shingle does not import.
fn output_option(
  output: &mut crate::OutputConfig,
  key: &str,
  value: &str,
) -> Result<(), ConfigError> {
  match key {
    // Shingle lights one output and does not mirror, so an output set to
    // mirror another stays dark rather than competing with its source.
    "mode" if value.starts_with("mirror") => {
      output.mode = crate::OutputMode::Off;
    }
    "mode" => {
      output.mode = value.parse().map_err(|_| {
        ConfigError::Invalid(format!("invalid output mode {value:?}"))
      })?;
    }
    "scale" => output.scale = number(value)?,
    "transform" => output.transform = value.to_lowercase(),
    "vrr" => output.vrr = boolean(value)?,
    "position" => {
      let position = value.parse().map_err(|_| {
        ConfigError::Invalid(format!("invalid output position {value:?}"))
      })?;
      output.set_position(position);
    }
    _ => {}
  }
  Ok(())
}

fn optional_seconds(value: &str) -> Result<Option<u64>, ConfigError> {
  let seconds: i64 = number(value)?;
  Ok((seconds >= 0).then_some(seconds as u64))
}

/// A Wayfire animation duration in milliseconds: `400`, `400ms` or `0.4s`,
/// optionally followed by an easing curve, which Shingle ignores.
fn duration_ms(value: &str) -> Result<u64, ConfigError> {
  let value = value.split_whitespace().next().unwrap_or(value);
  if let Some(seconds) =
    value.strip_suffix('s').filter(|_| !value.ends_with("ms"))
  {
    let seconds: f64 = number(seconds)?;
    if !seconds.is_finite()
      || seconds < 0.0
      || seconds * 1000.0 > u64::MAX as f64
    {
      return Err(ConfigError::Invalid(format!(
        "invalid animation duration {value:?}"
      )));
    }
    return Ok((seconds * 1000.0).round() as u64);
  }
  number(value.strip_suffix("ms").unwrap_or(value))
}

/// Normalizes a Wayfire or i3 key name to Shingle's: without `KEY_`, lower
/// case, `kp_5` as `kp5`, `return` as `enter` and `esc` as `escape`. An
/// `xkb:` name keeps its case.
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

fn number<T: std::str::FromStr>(value: &str) -> Result<T, ConfigError> {
  value
    .parse()
    .map_err(|_| ConfigError::Invalid(format!("invalid number {value:?}")))
}

fn boolean(value: &str) -> Result<bool, ConfigError> {
  match value.to_lowercase().as_str() {
    "true" | "yes" | "on" | "1" => Ok(true),
    "false" | "no" | "off" | "0" => Ok(false),
    _ => Err(ConfigError::Invalid(format!("invalid boolean {value:?}"))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConfigFormat, Modifier, OutputMode, UnplugPolicy};

  const STOCK: &str = include_str!("../testdata/wayfire-stock.ini");
  const WCM: &str = include_str!("../testdata/wayfire-wcm.ini");

  fn import(text: &str) -> (ShingleConfig, Vec<String>) {
    let (config, warnings) =
      ShingleConfig::parse_with_warnings(text, ConfigFormat::Wayfire)
        .expect("a Wayfire file always imports");
    (config, warnings.iter().map(ToString::to_string).collect())
  }

  fn text(binding: &Binding) -> String {
    String::from(binding.clone())
  }

  #[test]
  fn wayfires_stock_file_warns_only_about_missing_plugins() {
    let (config, warnings) = import(STOCK);
    assert_eq!(
      warnings,
      [
        concat!(
          "line 276: [cube] belongs to a plugin Shingle does not have; ",
          "its option is ignored"
        ),
        concat!(
          "line 318: [wayfire-shell] belongs to a plugin Shingle does not ",
          "have; its option is ignored"
        ),
      ]
    );
    assert_eq!(text(&config.keybindings.close), "super+q");
    assert_eq!(text(&config.keybindings.close_alternatives[0]), "alt+f4");
    assert_eq!((config.workspaces.width, config.workspaces.height), (3, 3));
    assert_eq!(config.commands.len(), 11);
    let screenshot = config
      .commands
      .iter()
      .find(|command| command.command.starts_with("slurp"))
      .unwrap();
    assert_eq!(text(&screenshot.binding), "shift+print");
    assert!(config.commands.iter().any(|command| command.repeatable));
    assert_eq!(config.expo.workspace_bindings.len(), 9);
    assert_eq!(config.autostart.len(), 6);
    assert_eq!(config.keybindings.grid_slot_left.len(), 2);
    // The continued plugins line is read whole, without preserve-output.
    assert_eq!(config.output_layout.unplug, UnplugPolicy::Return);
  }

  #[test]
  fn a_wcm_written_file_imports_with_the_expected_warnings() {
    let (config, warnings) = import(WCM);
    let expected = [
      (
        14,
        "[animate] close_animation: Shingle has no \"fire\" animation",
      ),
      (
        24,
        "[animate] minimize_animation: Shingle has no \"squeezimize\"",
      ),
      (
        40,
        "[blur] belongs to a plugin Shingle does not have; its 18",
      ),
      (
        61,
        "[command] binding_menu: \"<super>\" is a modifier-alone",
      ),
      (93, "[cube] belongs to a plugin"),
      (110, "[decoration] belongs to a plugin"),
      (
        141,
        "[expo] toggle: \"hotspot top-left 10x10 500\" is a hotspot",
      ),
      (154, "[fisheye] belongs to a plugin"),
      (175, "[gtk-shell] belongs to a plugin"),
      (
        305,
        "[place] mode: unknown placement mode; keeping the default",
      ),
      (332, "[scale] toggle: \"pinch in 4\" is a touch gesture"),
      (376, "[vswipe] belongs to a plugin"),
      (412, "[wayfire-shell] belongs to a plugin"),
      (
        417,
        "window rule workspace 5,5 is outside the 3x3 workspace grid",
      ),
      (430, "[wobbly] belongs to a plugin"),
    ];
    assert_eq!(warnings.len(), expected.len(), "{warnings:#?}");
    for (warning, (line, start)) in warnings.iter().zip(expected) {
      assert!(
        warning.starts_with(&format!("line {line}: {start}")),
        "{warning} does not start with line {line}: {start}"
      );
    }

    // Escaped colours, durations with an easing curve, and a trailing
    // comment.
    assert_eq!(
      config.background_color,
      [26.0 / 255.0, 43.0 / 255.0, 60.0 / 255.0, 1.0]
    );
    assert_eq!(config.tiling.gaps.inner, 7);
    assert_eq!(config.expo.duration_ms, 300);
    assert_eq!(config.zoom.smoothing_ms, 300);
    assert_eq!(config.wsets.label_duration, 2000);
    // Wayfire's default matcher, and fallbacks for missing animations.
    assert_eq!(
      config.animate.enabled_for.as_str(),
      concat!(
        "(type equals \"toplevel\" | (type equals \"x-or\" & focusable ",
        "equals true))"
      )
    );
    assert_eq!(config.animate.open_animation, "zoom");
    assert_eq!(config.animate.close_animation, "fade");
    assert_eq!(config.animate.minimize_animation, "zoom");
    assert_eq!(config.placement.mode, "center");
    // The continued plugins line names preserve-output.
    assert_eq!(config.output_layout.unplug, UnplugPolicy::Park);
    assert_eq!(config.xwayland.mode, XwaylandMode::Disabled);

    // Activators: keys stay, everything else is never a key.
    assert_eq!(text(&config.keybindings.scale_toggle), "super+p");
    assert_eq!(text(&config.keybindings.expo_toggle), "super+e");
    assert_eq!(config.keybindings.toggle_fullscreen, Binding::disabled());
    assert_eq!(
      config.pointer.resize_preserve_aspect_button,
      Binding::disabled()
    );
    let command = |name: &str| {
      config
        .commands
        .iter()
        .find(|command| command.command.starts_with(name))
        .unwrap_or_else(|| panic!("no {name} command"))
    };
    assert_eq!(command("synthetic-menu").binding, Binding::disabled());
    assert_eq!(command("synthetic-shot").command, "synthetic-shot --tag #1");
    assert_eq!(
      command("synthetic-echo").command,
      "synthetic-echo trailing\\"
    );
    assert!(command("synthetic-volume").repeatable);
    assert_eq!(
      command("synthetic-terminal").binding,
      Binding::new("enter", vec![Modifier::Super])
    );
    assert_eq!(config.autostart, ["true", "true synthetic-panel"]);
    assert_eq!(config.expo.workspace_bindings.len(), 9);
    assert_eq!(config.window_rules.len(), 1);
    assert_eq!(config.output_config("DP-7").mode, OutputMode::Off);
    assert_eq!(
      config.input_devices["Synthetic Pen"].output.as_deref(),
      Some("HEADLESS-1")
    );
  }

  #[test]
  fn the_silent_pinch_binding_is_gone() {
    // `pinch in 4` used to bind the key `4`, the gesture's last word.
    // The fixture imports exactly as it does without the gesture.
    let without = WCM.replace(" | pinch in 4", "");
    assert_ne!(without, WCM);
    assert_eq!(
      import(WCM).0.to_yaml().unwrap(),
      import(&without).0.to_yaml().unwrap()
    );
    for text in ["pinch in 4", "swipe up 3", "edge-swipe left 2"] {
      let (config, warnings) = import(&format!("[scale]\ntoggle = {text}\n"));
      assert_eq!(config.keybindings.scale_toggle, Binding::disabled());
      assert_eq!(warnings.len(), 1, "{text}");
    }
  }

  #[test]
  fn lines_outside_sections_and_without_values_warn() {
    let (config, warnings) = import(concat!(
      "orphan = 1\n",
      "[core]\n",
      "vwidth = 4 # four\n",
      "just words\n",
    ));
    assert_eq!(config.workspaces.width, 4);
    assert_eq!(
      warnings,
      [
        "line 1: option \"orphan\" comes before any [section]; ignored",
        "line 4: not a [section] or an option = value line; ignored",
      ]
    );
  }

  #[test]
  fn an_unescaped_sharp_is_a_comment_even_after_an_equals_sign() {
    // wf-config cuts the value, so the option is empty and keeps its
    // default; Shingle used to read the colour.
    let (config, warnings) = import("[core]\nbackground_color = #FF0000FF\n");
    assert_eq!(
      config.background_color,
      ShingleConfig::default().background_color
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("invalid color \"\""), "{warnings:?}");
  }

  #[test]
  fn a_bad_value_keeps_the_default_and_the_rest_imports() {
    let (config, warnings) = import(concat!(
      "[simple-tile]\n",
      "inner_gap_size = wide\n",
      "outer_horiz_gap_size = 3\n",
      "[scale]\n",
      "inactive_alpha = 1.5\n",
      "spacing = 12\n",
    ));
    let defaults = ShingleConfig::default();
    assert_eq!(config.tiling.gaps.inner, defaults.tiling.gaps.inner);
    assert_eq!(config.tiling.gaps.left, 3);
    assert_eq!(config.scale.inactive_alpha, defaults.scale.inactive_alpha);
    assert_eq!(config.scale.spacing, 12);
    assert_eq!(
      warnings,
      [
        concat!(
          "line 2: [simple-tile] inner_gap_size: invalid number \"wide\"; ",
          "keeping the default"
        ),
        concat!(
          "line 5: [scale] inactive_alpha: scale.inactive_alpha must be ",
          "between 0 and 1; keeping the default"
        ),
      ]
    );
  }

  #[test]
  fn command_bindings_keep_their_kind_and_line() {
    let (config, warnings) = import(concat!(
      "[command]\n",
      "command_a = run-a\n",
      "always_binding_a = <super> KEY_A\n",
      "release_binding_b = KEY_B | swipe up 3\n",
      "command_b = run-b\n",
      "binding_c = <hyper> KEY_C\n",
      "command_c = run-c\n",
      "command_orphan = nothing\n",
    ));
    assert_eq!(config.commands.len(), 3);
    assert!(config.commands[0].always);
    assert!(config.commands[1].on_release);
    assert_eq!(text(&config.commands[1].binding), "b");
    assert_eq!(config.commands[2].binding, Binding::disabled());
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].starts_with("line 4: [command] release_binding_b: "));
    assert!(warnings[1].starts_with("line 6: [command] binding_c: "));
  }

  #[test]
  fn sections_shingle_reads_do_not_warn() {
    // Every option of [window-rules] is a rule, which "1" is not.
    for section in SECTIONS.iter().filter(|name| **name != "window-rules") {
      let (_, warnings) = import(&format!("[{section}]\nsome_option = 1\n"));
      assert!(warnings.is_empty(), "{section}: {warnings:?}");
    }
    for section in ["output:DP-1", "input-device:Pen", "OUTPUT:DP-2"] {
      let (_, warnings) = import(&format!("[{section}]\nsome_option = 1\n"));
      assert!(warnings.is_empty(), "{section}: {warnings:?}");
    }
    let (_, warnings) =
      import("[blur]\nmethod = kawase\n[cube]\n[blur]\ntoggle = none\n");
    assert_eq!(
      warnings,
      [concat!(
        "line 1: [blur] belongs to a plugin Shingle does not have; its 2 ",
        "options are ignored"
      )]
    );
  }
}
