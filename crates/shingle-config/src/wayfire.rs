//! The Wayfire `wayfire.ini` importer ([`crate::ConfigFormat::Wayfire`]).

use std::collections::BTreeMap;

use crate::{
  Binding, ConfigError, ConfigWarning, ShingleConfig,
  import::{self, Import},
  ini::{self, Entry},
};

/// The sections the importer reads, or whose plugin Shingle implements
/// without options. Any other section's options are reported once per
/// section. `output:NAME`, `input:NAME` and `input-device:NAME` are matched
/// by prefix.
const SECTIONS: &[&str] = &[
  "alpha",
  "animate",
  "autostart",
  "command",
  "core",
  "decoration",
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
  "vswipe",
  "vswitch",
  "wayfire-shell",
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
    || name.starts_with("input:")
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

/// Sections that belong to no plugin, whose options always apply:
/// Wayfire's core sections, its objects and Shingle's own `[session]`.
const CORE_SECTIONS: &[&str] = &["core", "input", "workarounds", "session"];

/// Defaults applied after the others, because they set a field that an
/// earlier default of another plugin disables: `[wm-actions]
/// toggle_maximize` is `disabled`, while simple-tile binds `super+m` to the
/// same action.
const LATE_DEFAULTS: &[(&str, &str)] =
  &[("simple-tile", "key_toggle_maximized")];

/// The plugin a lower-case section belongs to, or `None` for a section
/// that always applies.
fn plugin_of(section: &str) -> Option<&str> {
  let base = section.split_once(':').map_or(section, |(base, _)| base);
  (!CORE_SECTIONS.contains(&base)
    && !crate::wayfire_options::OBJECTS.contains(&base))
  .then_some(base)
}

/// Whether the options of a lower-case `section` apply with `plugins`.
fn applies(section: &str, plugins: &[String]) -> bool {
  plugin_of(section).is_none_or(|plugin| {
    !crate::PLUGINS.contains(&plugin)
      || plugins.iter().any(|entry| entry == plugin)
  })
}

/// The section name [`crate::WayfireValues`] keeps: lower case, except
/// for the name of an object instance such as `output:DP-1`.
fn values_section(section_name: &str) -> String {
  match section_name.split_once(':') {
    Some((object, name))
      if crate::wayfire_options::OBJECTS
        .contains(&object.trim().to_lowercase().as_str()) =>
    {
      format!("{}:{}", object.trim().to_lowercase(), name.trim())
    }
    _ => section_name.to_lowercase(),
  }
}

/// The plugins of a file: its last `[core] plugins` line, or Wayfire's
/// default list.
fn listed_plugins(lines: &[ini::Line]) -> Vec<String> {
  let mut section = String::new();
  let mut value = None;
  for line in lines {
    match ini::entry(&line.text) {
      Entry::Section(name) => section = name.trim().to_lowercase(),
      Entry::Option { name, value: text }
        if section == "core" && name.eq_ignore_ascii_case("plugins") =>
      {
        value = Some(text.to_owned());
      }
      _ => {}
    }
  }
  let value = value.unwrap_or_else(|| {
    crate::wayfire_option("core", "plugins")
      .map(|option| option.default.to_owned())
      .unwrap_or_default()
  });
  crate::plugins::parse_list(&value)
}

/// Shingle's defaults with every Shingle binding that no Wayfire option
/// sets turned off (docs/design/wayfire-gaps.md, Decisions 3).
fn without_shingle_bindings() -> ShingleConfig {
  let mut config = ShingleConfig::default();
  let keys = &mut config.keybindings;
  for binding in [
    &mut keys.move_left,
    &mut keys.move_right,
    &mut keys.move_up,
    &mut keys.move_down,
    &mut keys.resize_left,
    &mut keys.resize_right,
    &mut keys.resize_up,
    &mut keys.resize_down,
    &mut keys.reload,
    &mut keys.lock,
  ] {
    *binding = Binding::disabled();
  }
  for binding in keys
    .workspace_switch
    .iter_mut()
    .chain(&mut keys.workspace_move)
  {
    *binding = Binding::disabled();
  }
  config
}

/// Applies the default of one option of the table to `config`, returning
/// the notes it raised. A default that Shingle cannot use leaves `config`
/// as it was.
fn apply_default(
  config: &mut ShingleConfig,
  option: &crate::WayfireOption,
) -> Result<Vec<String>, ConfigError> {
  let before = config.clone();
  let mut notes = Vec::new();
  let result = apply(
    config,
    option.section,
    option.section,
    option.name,
    option.default,
    &mut notes,
  )
  .and_then(|()| config.validate_fields());
  match result {
    Ok(()) => Ok(notes),
    Err(error) => {
      *config = before;
      Err(error)
    }
  }
}

/// Wayfire's defaults for a file whose plugin list is `plugins`: Shingle's
/// own defaults without its extra bindings, then the default of every
/// option of the table whose section applies, in table order. Defaults
/// whose values Shingle lacks (`squeezimize`) fall back silently; the
/// file's own values warn.
fn wayfire_defaults(plugins: &[String]) -> ShingleConfig {
  let mut config = without_shingle_bindings();
  let options = crate::wayfire_options().iter().filter(|option| {
    !option.is_list()
      && !crate::wayfire_options::OBJECTS.contains(&option.section)
      && supported(option.section)
      && applies(option.section, plugins)
  });
  let (late, early): (Vec<_>, Vec<_>) = options
    .partition(|option| LATE_DEFAULTS.contains(&(option.section, option.name)));
  for option in early.into_iter().chain(late) {
    let _ = apply_default(&mut config, option);
  }
  config
}

/// The commands Wayfire's autostart plugin adds with
/// `autostart_wf_shell`: `wf-panel` and `wf-background`, each unless an
/// entry's command already names it.
fn wf_shell_commands(autostart: &[String]) -> Vec<String> {
  ["wf-panel", "wf-background"]
    .into_iter()
    .filter(|program| !autostart.iter().any(|entry| entry.contains(program)))
    .map(str::to_owned)
    .collect()
}

pub(crate) fn parse(
  text: &str,
) -> Result<(ShingleConfig, Vec<ConfigWarning>), ConfigError> {
  let lines = ini::lines(text);
  let plugins = listed_plugins(&lines);
  let mut importer = Import::new(wayfire_defaults(&plugins));
  let mut values = crate::WayfireValues::default();
  let mut wf_shell = true;
  let mut command_bindings = BTreeMap::<String, CommandEntry>::new();
  let mut commands = BTreeMap::new();
  let mut unsupported: Vec<Unsupported> = Vec::new();
  // The section name as written: connector names keep their case.
  let mut section_name: Option<String> = None;
  let mut section_line = 0;
  for line in &lines {
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
    let key = name.to_lowercase();
    let kept_section = values_section(section_name);
    values.set(&kept_section, &key, value);
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
    // A plugin Wayfire does not load reads none of its options.
    if !applies(&section, &plugins) {
      continue;
    }
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
    let applied = if (section.as_str(), key.as_str())
      == ("autostart", "autostart_wf_shell")
    {
      importer.option(line.number, &context, |_, _| {
        wf_shell = boolean(value)?;
        Ok(())
      })
    } else {
      importer.option(line.number, &context, |config, notes| {
        apply(config, &section, section_name, &key, value, notes)
      })
    };
    if !applied {
      values.reject(&kept_section, &key);
    }
  }
  for (name, command) in commands {
    let Some(entry) = command_bindings.remove(&name) else {
      continue;
    };
    let applied =
      importer.option(entry.line, &entry.context, |config, notes| {
        config.commands.push(crate::CommandBinding {
          binding: import::binding_or_modifiers(&entry.value, notes)?,
          command,
          repeatable: entry.repeatable,
          on_release: entry.on_release,
          always: entry.always,
        });
        Ok(())
      });
    if !applied {
      let prefix = COMMAND_PREFIXES
        .iter()
        .find(|prefix| {
          (prefix.1, prefix.2, prefix.3)
            == (entry.repeatable, entry.on_release, entry.always)
        })
        .map_or("binding_", |prefix| prefix.0);
      values.reject("command", &format!("{prefix}{name}"));
    }
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
  let config = importer.config_mut();
  // Without preserve-output Wayfire moves an unplugged output's windows
  // for good; Shingle's default brings them back, so only the plugin's
  // own behaviour is imported.
  if plugins.iter().any(|plugin| plugin == "preserve-output") {
    config.output_layout.unplug = crate::UnplugPolicy::Park;
  }
  if wf_shell {
    let added = wf_shell_commands(&config.autostart);
    config.autostart.extend(added);
  }
  config.plugins = Some(plugins);
  config.wayfire_values = values;
  config.turn_off_unlisted_plugins();
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
    ("switcher", "speed") => config.switcher.duration_ms = duration_ms(value)?,
    ("switcher", "view_thumbnail_scale") => {
      config.switcher.view_thumbnail_scale = number(value)?
    }
    ("switcher", "view_thumbnail_rotation") => {
      config.switcher.view_thumbnail_rotation = number(value)?
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
    ("wayfire-shell", "toggle_menu") => {
      config.keybindings.toggle_menu =
        import::binding_or_modifiers(value, notes)?;
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
    ("input", "modifier_binding_timeout") => {
      // Wayfire never cancels at 0 or below.
      let timeout: i64 = number(value)?;
      config.input.modifier_binding_timeout =
        u32::try_from(timeout.max(0)).unwrap_or(u32::MAX);
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
    ("input-method-v1", "enable_text_input_v1") => {
      config.input.enable_text_input_v1 = boolean(value)?
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
    ("decoration", "font") => config.decoration.font = value.to_owned(),
    ("decoration", "font_color") => {
      config.decoration.font_color = crate::parse_color(value)?
    }
    // wf-config clamps a value to the bounds of Wayfire's metadata
    // (wf-config:include/wayfire/config/option.hpp, `clamp_value`).
    ("decoration", "font_scale") => {
      config.decoration.font_scale = clamped(value, 0.1, 1.0)?
    }
    ("decoration", "title_halign") => {
      config.decoration.title_halign = value.to_owned()
    }
    ("decoration", "title_valign") => {
      config.decoration.title_valign = value.to_owned()
    }
    ("decoration", "title_height") => {
      config.decoration.title_height = clamped::<i64>(value, 0, 100)? as u32
    }
    ("decoration", "border_size") => {
      config.decoration.border_size = clamped::<i64>(value, 0, 100)? as u32
    }
    ("decoration", "button_order") => {
      config.decoration.button_order = value.to_owned()
    }
    ("decoration", "button_scale") => {
      config.decoration.button_scale = clamped(value, 0.1, 1.0)?
    }
    ("decoration", "button_padding") => {
      config.decoration.button_padding = number(value)?
    }
    ("decoration", "active_color") => {
      config.decoration.active_color = crate::parse_color(value)?
    }
    ("decoration", "inactive_color") => {
      config.decoration.inactive_color = crate::parse_color(value)?
    }
    ("decoration", "ignore_views") => {
      config.decoration.ignore_views = value.to_owned().try_into()?
    }
    ("decoration", "forced_views") => {
      config.decoration.forced_views = value.to_owned().try_into()?
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
    ("vswipe", "enable_horizontal") => {
      config.vswipe.enable_horizontal = boolean(value)?
    }
    ("vswipe", "enable_vertical") => {
      config.vswipe.enable_vertical = boolean(value)?
    }
    ("vswipe", "enable_free_movement") => {
      config.vswipe.enable_free_movement = boolean(value)?
    }
    ("vswipe", "enable_smooth_transition") => {
      config.vswipe.enable_smooth_transition = boolean(value)?
    }
    ("vswipe", "duration") => config.vswipe.duration_ms = duration_ms(value)?,
    ("vswipe", "background") => {
      config.vswipe.background = crate::parse_color(value)?
    }
    ("vswipe", "fingers") => config.vswipe.fingers = number(value)?,
    ("vswipe", "gap") => config.vswipe.gap = number(value)?,
    ("vswipe", "threshold") => config.vswipe.threshold = number(value)?,
    ("vswipe", "delta_threshold") => {
      config.vswipe.delta_threshold = number(value)?
    }
    ("vswipe", "speed_factor") => config.vswipe.speed_factor = number(value)?,
    ("vswipe", "speed_cap") => config.vswipe.speed_cap = number(value)?,
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
    ("vswitch", key) if numbered(key, "binding_").is_some() => {
      config
        .vswitch
        .workspace_bindings
        .push(crate::WorkspaceBinding {
          workspace: numbered(key, "binding_").unwrap_or_default(),
          binding: import::binding(value, notes)?,
        });
    }
    ("vswitch", key) if numbered(key, "with_win_").is_some() => {
      config
        .vswitch
        .workspace_bindings_win
        .push(crate::WorkspaceBinding {
          workspace: numbered(key, "with_win_").unwrap_or_default(),
          binding: import::binding(value, notes)?,
        });
    }
    ("vswitch", key) if numbered(key, "send_win_").is_some() => {
      config.vswitch.bindings_win.push(crate::WorkspaceBinding {
        workspace: numbered(key, "send_win_").unwrap_or_default(),
        binding: import::binding(value, notes)?,
      });
    }
    // Read by `parse`, which adds wf-shell's clients at the end.
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
    // Read before the rest of the file (`listed_plugins`).
    ("core", "plugins") => {}
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
    (_, "calibration") if section.starts_with("input-device:") => {
      let name = section_name["input-device:".len()..].trim();
      let calibration = crate::parse_calibration(value)?;
      if !name.is_empty() {
        config
          .input_devices
          .entry(name.to_owned())
          .or_default()
          .calibration = calibration;
      }
    }
    _ if section.starts_with("input:") => {
      let name = section_name["input:".len()..].trim();
      if name.is_empty() {
        return Ok(());
      }
      let mut device =
        config.input_devices.get(name).cloned().unwrap_or_default();
      if device_option(&mut device, key, value)? {
        config.input_devices.insert(name.to_owned(), device);
      } else {
        notes.push(format!(
          "Shingle sets only libinput options per device ({}); {key} \
           follows [input]",
          crate::DEVICE_OPTIONS.join(", ")
        ));
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

/// The workspace number of a numbered vswitch option such as `binding_3`
/// for `prefix` `binding_`. Wayfire reads every name the prefix completes
/// as a number and logs the others, such as `binding_left`, which is an
/// option of its own.
fn numbered(key: &str, prefix: &str) -> Option<usize> {
  key.strip_prefix(prefix)?.parse().ok()
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

/// Reads libinput option `key` of an `[input:NAME]` section into `device`,
/// as the `[input]` arms read it. Returns `Ok(false)` for an option that
/// is not one of [`crate::DEVICE_OPTIONS`].
fn device_option(
  device: &mut crate::InputDeviceConfig,
  key: &str,
  value: &str,
) -> Result<bool, ConfigError> {
  match key {
    "left_handed_mode" => device.left_handed_mode = Some(boolean(value)?),
    "middle_emulation" => device.middle_emulation = Some(boolean(value)?),
    "mouse_accel_profile" => device.mouse_accel_profile = Some(value.into()),
    "mouse_cursor_speed" => device.mouse_cursor_speed = Some(number(value)?),
    "mouse_natural_scroll" => {
      device.mouse_natural_scroll = Some(boolean(value)?)
    }
    "touchpad_accel_profile" => {
      device.touchpad_accel_profile = Some(value.into())
    }
    "touchpad_cursor_speed" => {
      device.touchpad_cursor_speed = Some(number(value)?)
    }
    "tap_to_click" => device.tap_to_click = Some(boolean(value)?),
    "click_method" => device.click_method = Some(value.into()),
    "scroll_method" => device.scroll_method = Some(value.into()),
    "natural_scroll" => device.natural_scroll = Some(boolean(value)?),
    "disable_touchpad_while_typing" => {
      device.disable_touchpad_while_typing = Some(boolean(value)?)
    }
    "disable_touchpad_while_mouse" => {
      device.disable_touchpad_while_mouse = Some(boolean(value)?)
    }
    "tap_and_drag" => device.tap_and_drag = Some(boolean(value)?),
    "drag_lock" => device.drag_lock = Some(boolean(value)?),
    _ => return Ok(false),
  }
  Ok(true)
}

fn number<T: std::str::FromStr>(value: &str) -> Result<T, ConfigError> {
  value
    .parse()
    .map_err(|_| ConfigError::Invalid(format!("invalid number {value:?}")))
}

/// A number clamped to `minimum..=maximum`, as wf-config clamps an option
/// to its metadata's bounds. NaN is refused.
fn clamped<T: std::str::FromStr + PartialOrd>(
  value: &str,
  minimum: T,
  maximum: T,
) -> Result<T, ConfigError> {
  let parsed: T = number(value)?;
  if parsed.partial_cmp(&minimum).is_none() {
    return Err(ConfigError::Invalid(format!("invalid number {value:?}")));
  }
  Ok(if parsed < minimum {
    minimum
  } else if parsed > maximum {
    maximum
  } else {
    parsed
  })
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
  use crate::{ConfigFormat, Modifier, OutputMode, UnplugPolicy, XwaylandMode};

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
      [concat!(
        "line 276: [cube] belongs to a plugin Shingle does not have; ",
        "its option is ignored"
      ),]
    );
    assert_eq!(text(&config.keybindings.toggle_menu), "super");
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
    // autostart_wf_shell = true adds wf-shell's clients, which no entry
    // names.
    assert_eq!(config.autostart.len(), 8);
    assert_eq!(config.autostart[6..], ["wf-panel", "wf-background"]);
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
      (93, "[cube] belongs to a plugin"),
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

    // Activators: keys stay, everything else is never a key. Scale is not
    // in the plugin list, so its toggle is off.
    assert_eq!(config.keybindings.scale_toggle, Binding::disabled());
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
    assert_eq!(
      command("synthetic-menu").binding,
      Binding::modifiers_alone(vec![Modifier::Super])
    );
    assert_eq!(
      config.keybindings.toggle_menu,
      Binding::modifiers_alone(vec![Modifier::Super])
    );
    assert_eq!(config.input.modifier_binding_timeout, 400);
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
    // The fixture, with Scale loaded, imports exactly as it does without
    // the gesture.
    let scale = WCM.replace("expo \\\n", "expo scale \\\n");
    assert_ne!(scale, WCM);
    let without = scale.replace(" | pinch in 4", "");
    assert_ne!(without, scale);
    assert_eq!(
      String::from(import(&scale).0.keybindings.scale_toggle),
      "super+p"
    );
    assert_eq!(
      import(&scale).0.to_yaml().unwrap(),
      import(&without).0.to_yaml().unwrap()
    );
    for text in ["pinch in 4", "swipe up 3", "edge-swipe left 2"] {
      let (config, warnings) = import(&format!(
        "[core]\nplugins = scale\n[scale]\ntoggle = {text}\n"
      ));
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
      "[core]\n",
      "plugins = simple-tile scale\n",
      "[simple-tile]\n",
      "inner_gap_size = wide\n",
      "outer_horiz_gap_size = 3\n",
      "[scale]\n",
      "inactive_alpha = 1.5\n",
      "spacing = 12\n",
    ));
    // The defaults kept are Wayfire's.
    assert_eq!(config.tiling.gaps.inner, 5);
    assert_eq!(config.tiling.gaps.left, 3);
    assert_eq!(config.scale.inactive_alpha, 0.75);
    assert_eq!(config.scale.spacing, 12);
    assert_eq!(
      warnings,
      [
        concat!(
          "line 4: [simple-tile] inner_gap_size: invalid number \"wide\"; ",
          "keeping the default"
        ),
        concat!(
          "line 7: [scale] inactive_alpha: scale.inactive_alpha must be ",
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

  /// Every string a configuration's YAML holds, which includes every
  /// binding in its canonical form.
  fn strings(config: &ShingleConfig) -> Vec<String> {
    fn walk(value: &serde_yaml::Value, found: &mut Vec<String>) {
      match value {
        serde_yaml::Value::String(text) => found.push(text.clone()),
        serde_yaml::Value::Sequence(items) => {
          items.iter().for_each(|item| walk(item, found))
        }
        serde_yaml::Value::Mapping(map) => {
          map.values().for_each(|item| walk(item, found))
        }
        _ => {}
      }
    }
    let value = serde_yaml::to_value(config).unwrap();
    let mut found = Vec::new();
    walk(&value, &mut found);
    found
  }

  #[test]
  fn an_empty_file_has_wayfires_defaults() {
    let (config, warnings) = import("");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!((config.workspaces.width, config.workspaces.height), (3, 3));
    assert_eq!(
      config.keybindings.quit,
      Binding::new("backspace", vec![Modifier::Control, Modifier::Alt])
    );
    let bound = strings(&config);
    for binding in [
      "super+shift+q",
      "super+s",
      "super+d",
      "super+n",
      "super+f",
      "super+b",
      "super+1",
      "super+shift+1",
      "super+shift+h",
      "super+ctrl+h",
      "super+grave",
      "super+shift+r",
      "super+f8",
    ] {
      assert!(
        !bound.iter().any(|text| text == binding),
        "{binding} is bound"
      );
    }
    // Wayfire's other defaults, where they differ from Shingle's.
    assert_eq!(config.xwayland.mode, XwaylandMode::Eager);
    assert_eq!(
      (config.input.repeat_rate, config.input.repeat_delay),
      (40, 400)
    );
    assert_eq!(config.keybindings.workspace_move_down, Binding::disabled());
    assert_eq!(config.idle.idle_seconds, Some(3600));
    // Wayfire's default plugin list leaves out simple-tile, scale and
    // wm-actions.
    let plugins = config.plugins.as_deref().unwrap();
    assert!(plugins.iter().any(|plugin| plugin == "animate"));
    assert!(!config.plugin_enabled("simple-tile"));
    assert!(!config.plugin_enabled("scale"));
    assert_eq!(config.keybindings.scale_toggle, Binding::disabled());
    assert_eq!(config.keybindings.focus_left, Binding::disabled());
    assert_eq!(config.keybindings.toggle_maximized, Binding::disabled());
    // Animate is in it; minimize falls back from squeezimize silently.
    assert_eq!(config.animate.minimize_animation, "zoom");
    assert_eq!(text(&config.keybindings.expo_toggle), "super+e");
    // autostart_wf_shell defaults to true.
    assert_eq!(config.autostart, ["wf-panel", "wf-background"]);
  }

  #[test]
  fn every_default_applies_but_the_ones_shingle_lacks() {
    let mut config = without_shingle_bindings();
    let mut problems = Vec::new();
    for option in crate::wayfire_options() {
      if option.is_list()
        || crate::wayfire_options::OBJECTS.contains(&option.section)
        || !supported(option.section)
      {
        continue;
      }
      match apply_default(&mut config, option) {
        Ok(notes) if notes.is_empty() => {}
        Ok(notes) => problems.push(format!(
          "[{}] {}: {}",
          option.section,
          option.name,
          notes.join("; ")
        )),
        Err(error) => problems
          .push(format!("[{}] {}: {error}", option.section, option.name)),
      }
    }
    assert_eq!(
      problems,
      [concat!(
        "[animate] minimize_animation: Shingle has no \"squeezimize\" ",
        "animation yet; using \"zoom\""
      )]
    );
  }

  #[test]
  fn a_wayfire_file_keeps_shingle_bindings_set_by_name() {
    let (config, _) = import(concat!(
      "[core]\n",
      "plugins = simple-tile idle\n",
      "[simple-tile]\n",
      "key_move_left = <super> <shift> KEY_H\n",
      "[idle]\n",
      "key_lock = <super> KEY_F8\n",
    ));
    assert_eq!(text(&config.keybindings.move_left), "super+shift+h");
    assert_eq!(text(&config.keybindings.lock), "super+f8");
    assert_eq!(config.keybindings.move_right, Binding::disabled());
  }

  #[test]
  fn plugins_the_list_leaves_out_are_off() {
    let all = concat!(
      "[core]\n",
      "plugins = scale zoom grid animate simple-tile\n",
      "[scale]\n",
      "toggle = <super> KEY_P\n",
    );
    let (on, _) = import(all);
    assert_eq!(text(&on.keybindings.scale_toggle), "super+p");
    assert_eq!(on.zoom.modifier, "<super>");
    assert_eq!(on.keybindings.grid_slot_left.len(), 2);
    assert_eq!(on.animate.open_animation, "zoom");
    assert!(on.plugin_enabled("simple-tile"));
    assert_eq!(text(&on.keybindings.focus_left), "super+h");

    let (off, warnings) =
      import(&all.replace("scale zoom grid animate simple-tile", "expo"));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(off.keybindings.scale_toggle, Binding::disabled());
    assert!(off.zoom.modifier.is_empty());
    assert!(off.keybindings.grid_slot_left.is_empty());
    assert_eq!(off.animate.open_animation, "none");
    assert!(!off.plugin_enabled("simple-tile"));
    assert_eq!(off.keybindings.focus_left, Binding::disabled());
    assert_eq!(text(&off.keybindings.expo_toggle), "super+e");
    // What the file says about an unloaded plugin is still kept.
    assert_eq!(
      off.wayfire_values.value("scale", "toggle"),
      Some(crate::OptionValue::Text {
        value: "<super> KEY_P".into(),
        default: "<super> KEY_P",
      })
    );
  }

  #[test]
  fn a_shared_binding_follows_the_plugin_that_is_loaded() {
    // simple-tile and wm-actions both set toggle_maximized.
    let file = concat!(
      "[core]\n",
      "plugins = PLUGINS\n",
      "[simple-tile]\n",
      "key_toggle_maximized = <super> KEY_M\n",
      "[wm-actions]\n",
      "toggle_maximize = <super> KEY_X\n",
    );
    let maximize = |plugins: &str| {
      let (config, _) = import(&file.replace("PLUGINS", plugins));
      text(&config.keybindings.toggle_maximized)
    };
    assert_eq!(maximize("simple-tile"), "super+m");
    assert_eq!(maximize("wm-actions"), "super+x");
    assert_eq!(maximize("expo"), "");
    // Both loaded, without the file's values: simple-tile's default wins
    // over wm-actions' disabled one.
    let (both, _) = import("[core]\nplugins = simple-tile wm-actions\n");
    assert_eq!(text(&both.keybindings.toggle_maximized), "super+m");
  }

  #[test]
  fn numbered_vswitch_bindings_import() {
    let (config, warnings) = import(concat!(
      "[core]\n",
      "plugins = vswitch\n",
      "[vswitch]\n",
      "binding_3 = <super> KEY_3\n",
      "with_win_2 = <super> <shift> KEY_2\n",
      "send_win_9 = <super> <ctrl> KEY_9\n",
      "send_win_10 = <super> <ctrl> KEY_0\n",
      "binding_left = <super> KEY_LEFT\n",
    ));
    let vswitch = &config.vswitch;
    assert_eq!(
      vswitch.workspace_bindings,
      [crate::WorkspaceBinding {
        workspace: 3,
        binding: Binding::new("3", vec![Modifier::Super]),
      }]
    );
    assert_eq!(vswitch.workspace_bindings_win[0].workspace, 2);
    assert_eq!(vswitch.bindings_win.len(), 1);
    assert_eq!(vswitch.bindings_win[0].workspace, 9);
    assert_eq!(text(&config.keybindings.workspace_previous), "super+left");
    // Workspace 10 is outside the 3x3 grid.
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].starts_with("line 7: vswitch workspace 10 is outside"));
    let yaml = config.to_yaml().unwrap();
    let back = ShingleConfig::parse(&yaml, ConfigFormat::Yaml).unwrap();
    assert_eq!(back.vswitch.workspace_bindings, vswitch.workspace_bindings);
  }

  #[test]
  fn autostart_adds_wf_shell_as_wayfire_does() {
    let autostart = |text: &str| import(text).0.autostart;
    // Nothing names wf-shell: both start.
    assert_eq!(
      autostart("[autostart]\nterm = foot\n"),
      ["foot", "wf-panel", "wf-background"]
    );
    // An entry runs the panel: only the background is added.
    assert_eq!(
      autostart("[autostart]\npanel = env X=1 wf-panel --flag\n"),
      ["env X=1 wf-panel --flag", "wf-background"]
    );
    // Turned off: nothing is added.
    assert_eq!(
      autostart("[autostart]\nautostart_wf_shell = false\nterm = foot\n"),
      ["foot"]
    );
    // Without the autostart plugin nothing starts at all.
    assert!(
      autostart("[core]\nplugins = expo\n[autostart]\nterm = foot\n")
        .is_empty()
    );
    let (config, warnings) = import("[autostart]\nautostart_wf_shell = no?\n");
    assert_eq!(config.autostart.len(), 2);
    assert_eq!(warnings.len(), 1);
  }

  #[test]
  fn the_file_is_kept_for_ipc() {
    let (config, _) = import(concat!(
      "[core]\n",
      "vwidth = 4\n",
      "vheight = many\n",
      "background_color = \\#FF0000FF\n",
      "[output:DP-1]\n",
      "mode = 1920x1080@60\n",
      "[command]\n",
      "binding_term = <super> KEY_ENTER\n",
      "command_term = foot\n",
    ));
    let values = &config.wayfire_values;
    let text = |section: &str, option: &str| match values.value(section, option)
    {
      Some(crate::OptionValue::Text { value, .. }) => value,
      other => panic!("{other:?}"),
    };
    assert_eq!(text("core", "vwidth"), "4");
    // A value Shingle rejected reports the default, as in wf-config.
    assert_eq!(text("core", "vheight"), "3");
    assert_eq!(text("core", "background_color"), "#FF0000FF");
    assert_eq!(text("output:DP-1", "mode"), "1920x1080@60");
    assert_eq!(text("simple-tile", "inner_gap_size"), "5");
    assert_eq!(
      values.value("command", "bindings"),
      Some(crate::OptionValue::List(vec![vec![
        "term".into(),
        "foot".into(),
        "<super> KEY_ENTER".into(),
      ]]))
    );
    assert!(values.sections().contains(&"output:DP-1".to_owned()));
  }

  #[test]
  fn wayfire_shells_menu_key_takes_a_modifier_alone() {
    let (config, warnings) = import(concat!(
      "[core]\nplugins = wayfire-shell command\n",
      "[input]\nmodifier_binding_timeout = 250\n",
      "[wayfire-shell]\ntoggle_menu = <alt> <super> | pinch in 4\n",
      "[command]\nbinding_menu = <super>\ncommand_menu = wofi\n",
    ));
    assert_eq!(
      config.keybindings.toggle_menu,
      Binding::modifiers_alone(vec![Modifier::Alt, Modifier::Super])
    );
    assert_eq!(config.input.modifier_binding_timeout, 250);
    assert_eq!(
      config.commands[0].binding,
      Binding::modifiers_alone(vec![Modifier::Super])
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("touch gesture"), "{warnings:?}");
    // A key works too, and Wayfire never cancels at 0 or below.
    let (config, _) = import(concat!(
      "[core]\nplugins = wayfire-shell\n",
      "[input]\nmodifier_binding_timeout = -5\n",
      "[wayfire-shell]\ntoggle_menu = <super> KEY_M\n",
    ));
    assert_eq!(text(&config.keybindings.toggle_menu), "super+m");
    assert_eq!(config.input.modifier_binding_timeout, 0);
  }

  #[test]
  fn wayfire_shells_menu_key_follows_the_plugin_list() {
    let (listed, _) = import("[core]\nplugins = wayfire-shell\n");
    assert_eq!(text(&listed.keybindings.toggle_menu), "super");
    assert_eq!(listed.input.modifier_binding_timeout, 400);
    let (unlisted, _) =
      import("[core]\nplugins = expo\n[wayfire-shell]\ntoggle_menu = <ctrl>\n");
    assert_eq!(unlisted.keybindings.toggle_menu, Binding::disabled());
    assert!(!unlisted.plugin_enabled("wayfire-shell"));
    // Wayfire's default list loads it.
    let (default, _) = import("");
    assert!(default.plugin_enabled("wayfire-shell"));
    assert_eq!(text(&default.keybindings.toggle_menu), "super");
  }
}
