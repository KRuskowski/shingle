use std::collections::HashMap;

use crate::{
  Binding, ConfigError, Modifier, ShingleConfig, wayfire::normalize_key,
};

/// Reads the options of an `output NAME ...` line into `output`: `mode`
/// (or `res`/`resolution`, with an optional `--custom`), `pos`/`position`,
/// `scale`, `transform`, `adaptive_sync`, `disable` and `enable`.
fn output_line(
  output: &mut crate::OutputConfig,
  tokens: &[String],
  line_number: usize,
) -> Result<(), ConfigError> {
  let invalid =
    |what: &str| ConfigError::Invalid(format!("line {line_number}: {what}"));
  let mut index = 0;
  while index < tokens.len() {
    let value = |at: usize, what: &str| {
      tokens
        .get(at)
        .map(String::as_str)
        .ok_or_else(|| invalid(what))
    };
    match tokens[index].as_str() {
      "mode" | "res" | "resolution" => {
        let mut mode = value(index + 1, "missing output mode")?;
        if mode == "--custom" {
          index += 1;
          mode = value(index + 1, "missing output mode")?;
        }
        output.mode = crate::OutputMode::from_sway(mode)
          .ok_or_else(|| invalid("invalid output mode"))?;
        index += 2;
      }
      "pos" | "position" => {
        output.set_position(crate::OutputPosition::Fixed {
          x: number(
            value(index + 1, "incomplete output position")?,
            line_number,
          )?,
          y: number(
            value(index + 2, "incomplete output position")?,
            line_number,
          )?,
        });
        index += 3;
      }
      "scale" => {
        output.scale =
          number(value(index + 1, "missing output scale")?, line_number)?;
        index += 2;
      }
      "transform" => {
        output.transform =
          value(index + 1, "missing output transform")?.to_lowercase();
        index += 2;
      }
      "adaptive_sync" => {
        output.vrr = matches!(
          value(index + 1, "missing adaptive_sync value")?,
          "on" | "enable" | "enabled" | "yes" | "true"
        );
        index += 2;
      }
      "disable" => {
        output.mode = crate::OutputMode::Off;
        index += 1;
      }
      "enable" => {
        if output.mode == crate::OutputMode::Off {
          output.mode = crate::OutputMode::Auto;
        }
        index += 1;
      }
      _ => index += 1,
    }
  }
  Ok(())
}

pub(crate) fn parse(text: &str) -> Result<ShingleConfig, ConfigError> {
  let mut config = ShingleConfig::default();
  let mut variables = HashMap::new();
  for (index, original) in text.lines().enumerate() {
    let line_number = index + 1;
    let tokens = tokenize(original)?;
    if tokens.is_empty() {
      continue;
    }
    let tokens = expand(tokens, &variables);
    match tokens[0].as_str() {
      "set" => {
        if tokens.len() < 3 || !tokens[1].starts_with('$') {
          continue;
        }
        let name = tokens[1].clone();
        let value = tokens[2..].join(" ");
        match name.as_str() {
          "$idle_toggle" => {
            config.keybindings.idle_toggle = if value.trim().is_empty()
              || value.trim().eq_ignore_ascii_case("none")
            {
              Binding::disabled()
            } else {
              binding(&value)?
            };
          }
          "$disable_on_fullscreen" => {
            config.idle.disable_on_fullscreen = boolean(&value, line_number)?
          }
          "$disable_initially" => {
            config.idle.disable_initially = boolean(&value, line_number)?
          }
          "$idle_timeout" => {
            config.idle.idle_seconds = optional_seconds(&value, line_number)?
          }
          "$dpms_timeout" => {
            config.idle.dpms_seconds = optional_seconds(&value, line_number)?
          }
          "$lock_timeout" => {
            config.idle.lock_seconds = optional_seconds(&value, line_number)?
          }
          "$session_restore" => {
            config.session.restore = boolean(&value, line_number)?
          }
          "$session_path" => config.session.path = Some(value.into()),
          "$invert_preserve_hue" => {
            config.effects.invert_preserve_hue = boolean(&value, line_number)?
          }
          "$animation_enabled_for" => {
            config.animate.enabled_for = value.try_into()?
          }
          "$open_animation" => {
            config.animate.open_animation = value.to_lowercase()
          }
          "$close_animation" => {
            config.animate.close_animation = value.to_lowercase()
          }
          "$minimize_animation" => {
            config.animate.minimize_animation = value.to_lowercase()
          }
          "$open_animation_duration" => {
            config.animate.duration_ms = number(&value, line_number)?
          }
          "$fast_switcher_inactive_alpha" => {
            config.fast_switcher.inactive_alpha = number(&value, line_number)?
          }
          "$expo_background" => {
            config.expo.background = crate::parse_color(&value)?
          }
          "$expo_duration_ms" => {
            config.expo.duration_ms = number(&value, line_number)?
          }
          "$expo_transition_length_ms" => {
            config.expo.transition_length_ms = number(&value, line_number)?
          }
          "$expo_offset" => config.expo.offset = number(&value, line_number)?,
          "$expo_keyboard_interaction" => {
            config.expo.keyboard_interaction = boolean(&value, line_number)?
          }
          "$expo_inactive_brightness" => {
            config.expo.inactive_brightness = number(&value, line_number)?
          }
          "$scale_duration_ms" => {
            config.scale.duration_ms = number(&value, line_number)?
          }
          "$scale_title_overlay" => {
            config.scale.title_overlay = value.to_lowercase()
          }
          "$scale_title_font_size" => {
            config.scale.title_font_size = number(&value, line_number)?
          }
          "$scale_title_position" => {
            config.scale.title_position = value.to_lowercase()
          }
          "$scale_title_bg_color" => {
            config.scale.title_bg_color = crate::parse_color(&value)?
          }
          "$scale_title_text_color" => {
            config.scale.title_text_color = crate::parse_color(&value)?
          }
          "$scale_filter_case_sensitive" => {
            config.scale_title_filter.case_sensitive =
              boolean(&value, line_number)?
          }
          "$scale_filter_share" => {
            config.scale_title_filter.share_filter =
              boolean(&value, line_number)?
          }
          "$scale_filter_overlay" => {
            config.scale_title_filter.overlay = boolean(&value, line_number)?
          }
          "$scale_filter_font_size" => {
            config.scale_title_filter.font_size = number(&value, line_number)?
          }
          "$scale_filter_bg_color" => {
            config.scale_title_filter.bg_color = crate::parse_color(&value)?
          }
          "$scale_filter_text_color" => {
            config.scale_title_filter.text_color = crate::parse_color(&value)?
          }
          "$scale_spacing" => {
            config.scale.spacing = number(&value, line_number)?
          }
          "$scale_outer_margin" => {
            config.scale.outer_margin = number(&value, line_number)?
          }
          "$scale_inactive_alpha" => {
            config.scale.inactive_alpha = number(&value, line_number)?
          }
          "$scale_minimized_alpha" => {
            config.scale.minimized_alpha = number(&value, line_number)?
          }
          "$scale_include_minimized" => {
            config.scale.include_minimized = boolean(&value, line_number)?
          }
          "$scale_close_on_new_view" => {
            config.scale.close_on_new_view = boolean(&value, line_number)?
          }
          "$scale_allow_zoom" => {
            config.scale.allow_zoom = boolean(&value, line_number)?
          }
          "$scale_middle_click_close" => {
            config.scale.middle_click_close = boolean(&value, line_number)?
          }
          "$alpha_modifier" => config.alpha.modifier = value.to_lowercase(),
          "$alpha_min_value" => {
            config.alpha.min_value = number(&value, line_number)?
          }
          "$zoom_modifier" => config.zoom.modifier = value.to_lowercase(),
          "$zoom_speed" => config.zoom.speed = number(&value, line_number)?,
          "$zoom_smoothing_ms" => {
            config.zoom.smoothing_ms = number(&value, line_number)?
          }
          "$zoom_interpolation" => {
            config.zoom.interpolation = value.to_lowercase()
          }
          "$placement_mode" => config.placement.mode = value.to_lowercase(),
          "$activation_check_surface" => {
            config.activation.check_surface = boolean(&value, line_number)?
          }
          "$activation_only_last_request" => {
            config.activation.only_last_request = boolean(&value, line_number)?
          }
          "$activation_timeout" => {
            config.activation.timeout_seconds = number(&value, line_number)?
          }
          "$shortcuts_inhibit_by_default" => {
            config.shortcuts.inhibit_by_default = value.try_into()?
          }
          "$shortcuts_ignore_views" => {
            config.shortcuts.ignore_views = value.try_into()?
          }
          "$focus_button_with_modifiers" => {
            config.focus.button_with_modifiers = boolean(&value, line_number)?
          }
          "$focus_buttons" => {
            config.focus.buttons = value
              .split('|')
              .filter(|value| !value.trim().is_empty())
              .map(|value| binding(value.trim()))
              .collect::<Result<Vec<_>, _>>()?
          }
          "$focus_buttons_passthrough" => {
            config.focus.buttons_passthrough = boolean(&value, line_number)?
          }
          "$enable_snap" => {
            config.grid.enable_snap = boolean(&value, line_number)?
          }
          "$snap_threshold" => {
            config.grid.snap_threshold = number(&value, line_number)?
          }
          "$quarter_snap_threshold" => {
            config.grid.quarter_snap_threshold = number(&value, line_number)?
          }
          "$enable_snap_off" => {
            config.grid.enable_snap_off = boolean(&value, line_number)?
          }
          "$snap_off_threshold" => {
            config.grid.snap_off_threshold = number(&value, line_number)?
          }
          "$workspace_switch_after" => {
            config.grid.workspace_switch_after = number(&value, line_number)?
          }
          "$join_views" => {
            config.grid.join_views = boolean(&value, line_number)?
          }
          "$gesture_move_fingers" => {
            config.extra_gestures.move_fingers = number(&value, line_number)?
          }
          "$gesture_move_delay" => {
            config.extra_gestures.move_delay = number(&value, line_number)?
          }
          "$gesture_close_fingers" => {
            config.extra_gestures.close_fingers = number(&value, line_number)?
          }
          "$wrot_reset_radius" => {
            config.effects.wrot_reset_radius = number(&value, line_number)?
          }
          "$all_dialogs_modal" => {
            config.behavior.all_dialogs_modal = boolean(&value, line_number)?
          }
          "$keep_last_toplevel_activated" => {
            config.behavior.keep_last_toplevel_activated =
              boolean(&value, line_number)?
          }
          "$discard_command_output" => {
            config.behavior.discard_command_output =
              boolean(&value, line_number)?
          }
          "$max_buffer_size" => {
            config.behavior.max_buffer_size = number(&value, line_number)?
          }
          "$preferred_decoration_mode" => {
            config.behavior.preferred_decoration_mode = value.to_lowercase()
          }
          "$force_preferred_decoration_mode" => {
            config.behavior.force_preferred_decoration_mode =
              boolean(&value, line_number)?
          }
          "$security_privileged_protocols" => {
            config.security_context.privileged_protocols = value
          }
          "$disable_primary_selection" => {
            config.behavior.disable_primary_selection =
              boolean(&value, line_number)?
          }
          "$focus_main_surface_instead_of_popup" => {
            config.behavior.focus_main_surface_instead_of_popup =
              boolean(&value, line_number)?
          }
          "$auto_reload_config" => {
            config.behavior.auto_reload = boolean(&value, line_number)?
          }
          "$config_reload_delay" => {
            config.behavior.config_reload_delay = number(&value, line_number)?
          }
          "$minimum_extent" => {
            config.tiling.minimum_extent = number(&value, line_number)?
          }
          "$resize_min_width" => {
            config.resize.min_width = number(&value, line_number)?
          }
          "$resize_min_height" => {
            config.resize.min_height = number(&value, line_number)?
          }
          "$resize_corner_threshold" => {
            config.resize.corner_threshold = number(&value, line_number)?
          }
          "$keep_fullscreen_on_adjacent" => {
            config.tiling.keep_fullscreen_on_adjacent =
              boolean(&value, line_number)?
          }
          "$tile_by_default" => {
            config.tiling.tile_by_default = value.try_into()?
          }
          "$background_color" => {
            config.background_color = crate::parse_color(&value)?
          }
          "$vswitch_duration_ms" => {
            config.vswitch.duration_ms = number(&value, line_number)?
          }
          "$vswitch_gap" => config.vswitch.gap = number(&value, line_number)?,
          "$vswitch_background" => {
            config.vswitch.background = crate::parse_color(&value)?
          }
          "$workspace_wraparound" => config.workspaces.wraparound = value,
          "$animation_duration" => {
            config.tiling.animation_duration = number(&value, line_number)?
          }
          "$preview_base_color" => {
            config.tiling.preview_base_color = crate::parse_color(&value)?
          }
          "$preview_border_color" => {
            config.tiling.preview_border_color = crate::parse_color(&value)?
          }
          "$preview_border_width" => {
            config.tiling.preview_border_width = number(&value, line_number)?
          }
          "$xkb_rules" => config.input.rules = value,
          "$xkb_file" => config.input.xkb_file = value,
          "$kb_numlock_default_state" => {
            config.input.numlock_default = boolean(&value, line_number)?
          }
          "$kb_capslock_default_state" => {
            config.input.capslock_default = boolean(&value, line_number)?
          }
          "$xkb_model" => config.input.model = value,
          "$xkb_layout" => config.input.layout = value,
          "$xkb_variant" => config.input.variant = value,
          "$xkb_options" => config.input.options = value,
          "$mouse_scroll_speed" => {
            config.input.mouse_scroll_speed = number(&value, line_number)?
          }
          "$mouse_natural_scroll" => {
            config.input.mouse_natural_scroll = boolean(&value, line_number)?
          }
          "$left_handed_mode" => {
            config.input.left_handed_mode = boolean(&value, line_number)?
          }
          "$middle_emulation" => {
            config.input.middle_emulation = boolean(&value, line_number)?
          }
          "$cursor_theme" => config.input.cursor_theme = value,
          "$enable_text_input_v3" => {
            config.input.enable_text_input_v3 = boolean(&value, line_number)?
          }
          "$cursor_size" => {
            config.input.cursor_size = number(&value, line_number)?
          }
          _ => {
            variables.insert(name, value);
          }
        }
      }
      "gaps" => {
        if tokens.len() != 3 {
          continue;
        }
        let gap = number(&tokens[2], line_number)?;
        match tokens[1].as_str() {
          "inner" => config.tiling.gaps.inner = gap,
          "outer" => {
            config.tiling.gaps.top = gap;
            config.tiling.gaps.right = gap;
            config.tiling.gaps.bottom = gap;
            config.tiling.gaps.left = gap;
          }
          "horizontal" => {
            config.tiling.gaps.left = gap;
            config.tiling.gaps.right = gap;
          }
          "vertical" => {
            config.tiling.gaps.top = gap;
            config.tiling.gaps.bottom = gap;
          }
          "top" => config.tiling.gaps.top = gap,
          "right" => config.tiling.gaps.right = gap,
          "bottom" => config.tiling.gaps.bottom = gap,
          "left" => config.tiling.gaps.left = gap,
          _ => {}
        }
      }
      "exec" => {
        let command = tokens[1..].join(" ");
        let command =
          command.strip_prefix("--no-startup-id ").unwrap_or(&command);
        if !command.is_empty() {
          config.autostart.push(command.to_owned());
        }
      }
      "bindsym" => {
        if tokens.len() < 3 {
          continue;
        }
        let binding = binding(&tokens[1])?;
        let command = tokens[2..].join(" ");
        if let Some(workspace) = command.strip_prefix("expo select ") {
          let workspace = workspace.parse().map_err(|_| {
            ConfigError::Invalid(format!(
              "line {line_number}: invalid Expo workspace"
            ))
          })?;
          config
            .expo
            .workspace_bindings
            .push(crate::ExpoWorkspaceBinding { workspace, binding });
          continue;
        }
        match command.as_str() {
          "snap top_left" => {
            config.keybindings.grid_slot_top_left = vec![binding]
          }
          "snap top" => config.keybindings.grid_slot_top = vec![binding],
          "snap top_right" => {
            config.keybindings.grid_slot_top_right = vec![binding]
          }
          "snap left" => config.keybindings.grid_slot_left = vec![binding],
          "snap center" => config.keybindings.grid_slot_center = vec![binding],
          "snap right" => config.keybindings.grid_slot_right = vec![binding],
          "snap bottom_left" => {
            config.keybindings.grid_slot_bottom_left = vec![binding]
          }
          "snap bottom" => config.keybindings.grid_slot_bottom = vec![binding],
          "snap bottom_right" => {
            config.keybindings.grid_slot_bottom_right = vec![binding]
          }
          "snap restore" | "restore" => {
            config.keybindings.grid_restore = vec![binding]
          }
          "wrot activate" => config.pointer.wrot_activate = binding,
          "wrot reset" => config.keybindings.wrot_reset = binding,
          "wrot reset-one" | "wrot reset one" => {
            config.keybindings.wrot_reset_one = binding
          }
          "focus left" => config.keybindings.focus_left = binding,
          "focus right" => config.keybindings.focus_right = binding,
          "focus up" => config.keybindings.focus_up = binding,
          "focus down" => config.keybindings.focus_down = binding,
          "expo toggle" => config.keybindings.expo_toggle = binding,
          "scale toggle" => config.keybindings.scale_toggle = binding,
          "scale toggle_all" | "scale toggle all" => {
            config.keybindings.scale_toggle_all = binding
          }
          "fast-switcher next" => {
            config.keybindings.fast_switcher_next = binding
          }
          "fast-switcher prev" | "fast-switcher previous" => {
            config.keybindings.fast_switcher_previous = binding
          }
          "focus next" => config.keybindings.focus_next = binding,
          "focus prev" | "focus previous" => {
            config.keybindings.focus_previous = binding;
          }
          "fullscreen toggle" => config.keybindings.toggle_fullscreen = binding,
          "sticky toggle" => config.keybindings.toggle_sticky = binding,
          "always_on_top" | "always_on_top toggle" => {
            config.keybindings.always_on_top = binding
          }
          "always_on_bottom" | "always_on_bottom toggle" => {
            config.keybindings.always_on_bottom = binding
          }
          "floating toggle" => config.keybindings.toggle_tiling = binding,
          "invert toggle" => config.keybindings.invert_toggle = binding,
          "resize preserve aspect" => {
            config.pointer.resize_preserve_aspect_button = binding
          }
          "break grab" => config.keybindings.break_grab = binding,
          "kill" => {
            config.keybindings.close = binding;
            config.keybindings.close_alternatives.clear();
          }
          "reload" => config.keybindings.reload = binding,
          "exit" => config.keybindings.quit = binding,
          "move left" => config.keybindings.move_left = binding,
          "move right" => config.keybindings.move_right = binding,
          "move up" => config.keybindings.move_up = binding,
          "move down" => config.keybindings.move_down = binding,
          "workspace next" => config.keybindings.workspace_next = binding,
          "workspace up" => config.keybindings.workspace_up = binding,
          "workspace down" => config.keybindings.workspace_down = binding,
          "workspace prev" | "workspace previous" => {
            config.keybindings.workspace_previous = binding;
          }
          "move container to workspace next" => {
            config.keybindings.workspace_send_next = binding;
          }
          "move container to workspace up" => {
            config.keybindings.workspace_send_up = binding;
          }
          "move container to workspace down" => {
            config.keybindings.workspace_send_down = binding;
          }
          "move container to workspace prev"
          | "move container to workspace previous" => {
            config.keybindings.workspace_send_previous = binding;
          }
          "workspace back_and_forth" => {
            config.keybindings.workspace_last = binding
          }
          "move container to workspace back_and_forth" => {
            config.keybindings.workspace_send_last = binding;
          }
          "resize shrink width" | "resize left" => {
            config.keybindings.resize_left = binding
          }
          "resize grow width" | "resize right" => {
            config.keybindings.resize_right = binding
          }
          "resize shrink height" | "resize up" => {
            config.keybindings.resize_up = binding
          }
          "resize grow height" | "resize down" => {
            config.keybindings.resize_down = binding
          }
          _ => {
            if let Some(workspace) = command.strip_prefix("workspace ")
              && let Some(index) = workspace_index(workspace)
            {
              config.keybindings.workspace_switch[index] = binding;
            } else if let Some(workspace) =
              command.strip_prefix("move container to workspace ")
              && let Some(index) = workspace_index(workspace)
            {
              config.keybindings.workspace_move[index] = binding;
            } else if command == "exec shingle-lock"
              || command == "exec --no-startup-id shingle-lock"
            {
              config.keybindings.lock = binding;
            } else if let Some(command) = command
              .strip_prefix("exec --no-startup-id ")
              .or_else(|| command.strip_prefix("exec "))
            {
              config.commands.push(crate::CommandBinding {
                binding,
                command: command.to_owned(),
                repeatable: false,
                on_release: false,
                always: false,
              });
            }
          }
        }
      }
      "output" => {
        let output = match tokens.get(1).map(String::as_str) {
          None => {
            return Err(ConfigError::Invalid(format!(
              "line {line_number}: missing output name"
            )));
          }
          Some("*") => &mut config.output,
          Some(name) => config.named_output(name),
        };
        output_line(output, &tokens[2..], line_number)?;
      }
      _ => {}
    }
  }
  config.validate()?;
  Ok(config)
}

fn workspace_index(value: &str) -> Option<usize> {
  value.parse::<usize>().ok().and_then(|workspace| {
    (1..=10).contains(&workspace).then_some(workspace - 1)
  })
}

fn binding(value: &str) -> Result<Binding, ConfigError> {
  let mut parts = value.split('+');
  let key = parts
    .next_back()
    .filter(|key| !key.is_empty())
    .ok_or_else(|| ConfigError::Binding(value.to_owned()))?;
  let modifiers = parts
    .map(|part| match part.to_lowercase().as_str() {
      "mod4" | "super" => Ok(Modifier::Super),
      "mod1" | "alt" => Ok(Modifier::Alt),
      "ctrl" | "control" => Ok(Modifier::Control),
      "shift" => Ok(Modifier::Shift),
      _ => Err(ConfigError::Binding(value.to_owned())),
    })
    .collect::<Result<Vec<_>, _>>()?;
  Ok(Binding::new(normalize_key(key), modifiers))
}

fn expand(
  tokens: Vec<String>,
  variables: &HashMap<String, String>,
) -> Vec<String> {
  let mut variables = variables.iter().collect::<Vec<_>>();
  variables.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
  tokens
    .into_iter()
    .map(|mut token| {
      for (name, value) in &variables {
        token = token.replace(name.as_str(), value);
      }
      token
    })
    .collect()
}

fn tokenize(line: &str) -> Result<Vec<String>, ConfigError> {
  let mut tokens = Vec::new();
  let mut current = String::new();
  let mut quote = None;
  let mut escaped = false;
  let mut characters = line.chars().peekable();
  while let Some(character) = characters.next() {
    if escaped {
      current.push(character);
      escaped = false;
      continue;
    }
    if character == '\\' {
      escaped = true;
      continue;
    }
    if let Some(delimiter) = quote {
      if character == delimiter {
        quote = None;
      } else {
        current.push(character);
      }
      continue;
    }
    match character {
      '\'' | '"' => quote = Some(character),
      '#' if current.is_empty() => {
        let hex_assignment = tokens.first().is_some_and(|token| token == "set");
        if !hex_assignment
          || characters.peek().is_none_or(|next| next.is_whitespace())
        {
          break;
        }
        current.push(character);
      }
      '#' => current.push(character),
      character if character.is_whitespace() => {
        if !current.is_empty() {
          tokens.push(std::mem::take(&mut current));
        }
      }
      _ => current.push(character),
    }
  }
  if quote.is_some() || escaped {
    return Err(ConfigError::Invalid("unterminated i3 string".into()));
  }
  if !current.is_empty() {
    tokens.push(current);
  }
  Ok(tokens)
}

fn optional_seconds(
  value: &str,
  line: usize,
) -> Result<Option<u64>, ConfigError> {
  let seconds: i64 = number(value, line)?;
  Ok((seconds >= 0).then_some(seconds as u64))
}

fn number<T: std::str::FromStr>(
  value: &str,
  line: usize,
) -> Result<T, ConfigError> {
  value.parse().map_err(|_| {
    ConfigError::Invalid(format!("line {line}: invalid number {value:?}"))
  })
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
