use std::path::Path;

use gloomy_core::{
  Align, BoxStyle, ButtonStyle, Direction, Justify, Layout, TextInputStyle,
  Widget, WidgetBounds,
};
use shingle_config::{Binding, ShingleConfig};

static DISABLED_BINDING: std::sync::LazyLock<Binding> =
  std::sync::LazyLock::new(Binding::disabled);

pub const KEY_ACTIONS: &[(&str, &str)] = &[
  ("workspace_next", "Next workspace"),
  ("workspace_previous", "Previous workspace"),
  ("workspace_up", "Workspace above"),
  ("workspace_down", "Workspace below"),
  ("workspace_move_next", "Move window to next workspace"),
  ("workspace_move_up", "Move window to workspace above"),
  ("workspace_move_down", "Move window to workspace below"),
  (
    "workspace_move_previous",
    "Move window to previous workspace",
  ),
  ("workspace_send_next", "Send window to next workspace"),
  (
    "workspace_send_previous",
    "Send window to previous workspace",
  ),
  ("workspace_send_up", "Send window to workspace above"),
  ("workspace_send_down", "Send window to workspace below"),
  ("workspace_last", "Last active workspace"),
  ("workspace_move_last", "Move window to last workspace"),
  ("workspace_send_last", "Send window to last workspace"),
  ("focus_left", "Focus left"),
  ("focus_right", "Focus right"),
  ("focus_up", "Focus up"),
  ("focus_down", "Focus down"),
  ("focus_next", "Focus next window"),
  ("focus_previous", "Focus previous window"),
  ("toggle_fullscreen", "Toggle fullscreen"),
  ("toggle_sticky", "Toggle sticky"),
  ("scale_toggle", "Toggle window overview"),
  ("scale_toggle_all", "Toggle all-workspaces overview"),
  ("expo_toggle", "Toggle workspace overview"),
  ("fast_switcher_next", "Fast-switch to next window"),
  ("fast_switcher_previous", "Fast-switch to previous window"),
  ("invert_toggle", "Invert output colors"),
  ("wrot_reset_one", "Reset rotation of focused window"),
  ("wrot_reset", "Reset rotation of all windows"),
  ("always_on_top", "Toggle always on top"),
  ("always_on_bottom", "Toggle always on bottom"),
  ("minimize", "Minimize focused window"),
  ("bring_to_front", "Bring window to front"),
  ("send_to_back", "Send window to back"),
  ("show_desktop", "Show desktop"),
  ("toggle_maximized", "Toggle maximized"),
  ("toggle_tiling", "Toggle floating / tiling"),
  ("move_left", "Move window left"),
  ("move_right", "Move window right"),
  ("move_up", "Move window up"),
  ("move_down", "Move window down"),
  ("resize_left", "Resize window left"),
  ("resize_right", "Resize window right"),
  ("resize_up", "Resize window up"),
  ("resize_down", "Resize window down"),
  ("idle_toggle", "Enable or disable idle timeouts"),
  ("break_grab", "Temporarily allow compositor shortcuts"),
  ("lock", "Lock session"),
  ("close", "Close focused window"),
  ("reload", "Reload configuration"),
  ("quit", "Quit Shingle"),
  ("toggle_menu", "Toggle shell menu"),
  ("grid_slot_top_left", "Snap top left"),
  ("grid_slot_top", "Snap top"),
  ("grid_slot_top_right", "Snap top right"),
  ("grid_slot_left", "Snap left"),
  ("grid_slot_center", "Snap center / maximize area"),
  ("grid_slot_right", "Snap right"),
  ("grid_slot_bottom_left", "Snap bottom left"),
  ("grid_slot_bottom", "Snap bottom"),
  ("grid_slot_bottom_right", "Snap bottom right"),
  ("grid_restore", "Restore pre-snap geometry"),
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Page {
  #[default]
  General,
  Tiling,
  Keybindings,
  Commands,
  WindowRules,
  Input,
  IdleSession,
  Launcher,
}

impl Page {
  const ALL: [Self; 8] = [
    Self::General,
    Self::Tiling,
    Self::Keybindings,
    Self::Commands,
    Self::WindowRules,
    Self::Input,
    Self::IdleSession,
    Self::Launcher,
  ];

  fn title(self) -> &'static str {
    match self {
      Self::General => "General",
      Self::Tiling => "Tiling",
      Self::Keybindings => "Keybindings",
      Self::Commands => "Commands",
      Self::WindowRules => "Window rules",
      Self::Input => "Keyboard input",
      Self::IdleSession => "Idle & session",
      Self::Launcher => "Launcher",
    }
  }

  pub fn named(name: &str) -> Option<Self> {
    match name {
      "general" => Some(Self::General),
      "tiling" => Some(Self::Tiling),
      "keys" => Some(Self::Keybindings),
      "commands" => Some(Self::Commands),
      "rules" => Some(Self::WindowRules),
      "input" => Some(Self::Input),
      "idle" => Some(Self::IdleSession),
      "launcher" => Some(Self::Launcher),
      _ => None,
    }
  }

  fn action(self) -> String {
    format!(
      "page:{}",
      match self {
        Self::General => "general",
        Self::Tiling => "tiling",
        Self::Keybindings => "keys",
        Self::Commands => "commands",
        Self::WindowRules => "rules",
        Self::Input => "input",
        Self::IdleSession => "idle",
        Self::Launcher => "launcher",
      }
    )
  }
}

pub struct UiModel<'a> {
  pub config: &'a ShingleConfig,
  pub source: &'a Path,
  pub destination: &'a Path,
  pub page: Page,
  pub capture_action: Option<&'a str>,
  pub status: &'a str,
  pub dirty: bool,
}

pub fn build(model: &UiModel<'_>) -> Widget {
  let config = model.config;
  let navigation = Page::ALL.into_iter().map(|page| {
    let selected = page == model.page;
    let mut button = Widget::button(page.title(), page.action(), 122.0, 42.0);
    if selected && let Widget::Button { style, .. } = &mut button {
      style.idle = BoxStyle::fill(ACCENT).with_radius(5.0);
    }
    button
  });

  let content = match model.page {
    Page::General => vec![
      heading("Output"),
      text_row("Width", "output_width", config.output.width.to_string()),
      text_row("Height", "output_height", config.output.height.to_string()),
      text_row("X position", "output_x", config.output.x.to_string()),
      text_row("Y position", "output_y", config.output.y.to_string()),
      text_row("Scale", "output_scale", config.output.scale.to_string()),
      text_row(
        "Transform",
        "output_transform",
        config.output.transform.clone(),
      ),
      text_row(
        "Background color (R G B A)",
        "background_color",
        config
          .background_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      heading("Configuration"),
      checkbox_row(
        "auto_reload",
        "Reload automatically after this file changes",
        config.behavior.auto_reload,
      ),
      text_row(
        "Reload delay (milliseconds)",
        "config_reload_delay",
        config.behavior.config_reload_delay.to_string(),
      ),
      checkbox_row(
        "all_dialogs_modal",
        "Focus the topmost dialog instead of its parent",
        config.behavior.all_dialogs_modal,
      ),
      checkbox_row(
        "discard_command_output",
        "Discard stdout and stderr from launched applications",
        config.behavior.discard_command_output,
      ),
      checkbox_row(
        "keep_last_toplevel_activated",
        "Keep the last toplevel activated when a layer takes focus",
        config.behavior.keep_last_toplevel_activated,
      ),
      text_row(
        "Maximum shared-memory buffer dimension",
        "max_buffer_size",
        config.behavior.max_buffer_size.to_string(),
      ),
      text_row(
        "Preferred decoration mode (client or server)",
        "preferred_decoration_mode",
        config.behavior.preferred_decoration_mode.clone(),
      ),
      checkbox_row(
        "force_preferred_decoration_mode",
        "Force the preferred decoration mode on clients",
        config.behavior.force_preferred_decoration_mode,
      ),
      checkbox_row(
        "focus_main_surface_instead_of_popup",
        "Focus the main surface instead of popup menus",
        config.behavior.focus_main_surface_instead_of_popup,
      ),
      checkbox_row(
        "disable_primary_selection",
        "Disable primary selection (requires restart)",
        config.behavior.disable_primary_selection,
      ),
      text_row(
        "XWayland (true, lazy or false; requires restart)",
        "xwayland_mode",
        config.xwayland.mode.to_string(),
      ),
      text_row(
        "XWayland startup script",
        "xwayland_startup_script",
        config.xwayland.startup_script.clone(),
      ),
      text_row(
        "Privileged protocols for sandbox filtering",
        "security_privileged_protocols",
        config.security_context.privileged_protocols.clone(),
      ),
      text_row(
        "Floating placement (center, cascade, maximize, random, pointer)",
        "placement_mode",
        config.placement.mode.clone(),
      ),
      heading("Click to focus"),
      text_row(
        "Mouse buttons (comma-separated)",
        "focus_buttons",
        config
          .focus
          .buttons
          .iter()
          .cloned()
          .map(String::from)
          .collect::<Vec<_>>()
          .join(", "),
      ),
      checkbox_row(
        "focus_button_with_modifiers",
        "Allow click to focus while keyboard modifiers are held",
        config.focus.button_with_modifiers,
      ),
      checkbox_row(
        "focus_buttons_passthrough",
        "Deliver focus-changing clicks to the client",
        config.focus.buttons_passthrough,
      ),
      heading("Window activation"),
      checkbox_row(
        "activation_check_surface",
        "Require a source surface for activation tokens",
        config.activation.check_surface,
      ),
      checkbox_row(
        "activation_only_last_request",
        "Accept only the most recent activation token",
        config.activation.only_last_request,
      ),
      text_row(
        "Activation token timeout (seconds)",
        "activation_timeout",
        config.activation.timeout_seconds.to_string(),
      ),
      heading("Screen zoom"),
      text_row(
        "Zoom modifier (super, ctrl, alt, shift)",
        "zoom_modifier",
        config.zoom.modifier.clone(),
      ),
      text_row("Zoom speed", "zoom_speed", config.zoom.speed.to_string()),
      text_row(
        "Zoom smoothing (ms; 0 disables)",
        "zoom_smoothing_ms",
        config.zoom.smoothing_ms.to_string(),
      ),
      text_row(
        "Zoom interpolation (linear or nearest)",
        "zoom_interpolation",
        config.zoom.interpolation.clone(),
      ),
      heading("Window opacity"),
      text_row(
        "Opacity modifier (super, ctrl, alt, shift)",
        "alpha_modifier",
        config.alpha.modifier.clone(),
      ),
      text_row(
        "Minimum opacity (0.0 to 1.0)",
        "alpha_min_value",
        config.alpha.min_value.to_string(),
      ),
      text_row(
        "Fast-switcher inactive opacity (0.0 to 1.0)",
        "fast_switcher_inactive_alpha",
        config.fast_switcher.inactive_alpha.to_string(),
      ),
      heading("Window overview"),
      text_row(
        "Thumbnail transition duration (ms)",
        "scale_duration_ms",
        config.scale.duration_ms.to_string(),
      ),
      checkbox_row(
        "scale_include_minimized",
        "Include minimized windows",
        config.scale.include_minimized,
      ),
      checkbox_row(
        "scale_close_on_new_view",
        "Close the overview when a new window is mapped",
        config.scale.close_on_new_view,
      ),
      text_row(
        "Unselected minimized opacity (0.0 to 1.0)",
        "scale_minimized_alpha",
        config.scale.minimized_alpha.to_string(),
      ),
      text_row(
        "Title display (never, mouse, all)",
        "scale_title_overlay",
        config.scale.title_overlay.clone(),
      ),
      text_row(
        "Title font size",
        "scale_title_font_size",
        config.scale.title_font_size.to_string(),
      ),
      text_row(
        "Title placement (top, center, bottom)",
        "scale_title_position",
        config.scale.title_position.clone(),
      ),
      text_row(
        "Title background color",
        "scale_title_bg_color",
        config
          .scale
          .title_bg_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      text_row(
        "Title text color",
        "scale_title_text_color",
        config
          .scale
          .title_text_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      text_row(
        "Filter overlay font size",
        "scale_filter_font_size",
        config.scale_title_filter.font_size.to_string(),
      ),
      checkbox_row(
        "scale_filter_case_sensitive",
        "Match title filter case-sensitively",
        config.scale_title_filter.case_sensitive,
      ),
      checkbox_row(
        "scale_filter_overlay",
        "Show the active title filter",
        config.scale_title_filter.overlay,
      ),
      text_row(
        "Filter background color",
        "scale_filter_bg_color",
        config
          .scale_title_filter
          .bg_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      text_row(
        "Filter text color",
        "scale_filter_text_color",
        config
          .scale_title_filter
          .text_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      checkbox_row(
        "scale_middle_click_close",
        "Middle-click a thumbnail to close its window",
        config.scale.middle_click_close,
      ),
      heading("Workspace overview"),
      text_row(
        "Background color",
        "expo_background",
        config
          .expo
          .background
          .map(|component| component.to_string())
          .join(" "),
      ),
      text_row(
        "Zoom duration (ms; 0 disables)",
        "expo_duration_ms",
        config.expo.duration_ms.to_string(),
      ),
      text_row(
        "Brightness transition duration (ms)",
        "expo_transition_length_ms",
        config.expo.transition_length_ms.to_string(),
      ),
      text_row(
        "Cell gap (logical pixels)",
        "expo_offset",
        config.expo.offset.to_string(),
      ),
      checkbox_row(
        "expo_keyboard_interaction",
        "Enable arrow-key workspace selection",
        config.expo.keyboard_interaction,
      ),
      text_row(
        "Inactive workspace brightness (0.0 to 1.0)",
        "expo_inactive_brightness",
        config.expo.inactive_brightness.to_string(),
      ),
      text_row(
        "Direct selection (workspace=binding, comma-separated)",
        "expo_workspace_bindings",
        config
          .expo
          .workspace_bindings
          .iter()
          .map(|entry| {
            format!(
              "{}={}",
              entry.workspace,
              String::from(entry.binding.clone())
            )
          })
          .collect::<Vec<_>>()
          .join(", "),
      ),
      heading("Open-window animation"),
      text_row(
        "Animate windows matching",
        "animation_enabled_for",
        config.animate.enabled_for.as_str().to_owned(),
      ),
      text_row(
        "Animation type (none, fade, zoom)",
        "open_animation",
        config.animate.open_animation.clone(),
      ),
      text_row(
        "Close animation (none, fade, zoom)",
        "close_animation",
        config.animate.close_animation.clone(),
      ),
      text_row(
        "Minimize animation (none, fade, zoom)",
        "minimize_animation",
        config.animate.minimize_animation.clone(),
      ),
      text_row(
        "Animation duration (ms)",
        "open_animation_duration",
        config.animate.duration_ms.to_string(),
      ),
      checkbox_row(
        "invert_preserve_hue",
        "Preserve hue when inverting output colors",
        config.effects.invert_preserve_hue,
      ),
      text_row(
        "2D rotation center reset radius",
        "wrot_reset_radius",
        config.effects.wrot_reset_radius.to_string(),
      ),
      heading("Workspace grid"),
      text_row(
        "Columns",
        "workspace_width",
        config.workspaces.width.to_string(),
      ),
      text_row(
        "Rows",
        "workspace_height",
        config.workspaces.height.to_string(),
      ),
      text_row(
        "Wraparound (false, true, on_end)",
        "workspace_wraparound",
        config.workspaces.wraparound.clone(),
      ),
      text_row(
        "Workspace transition duration (ms; 0 disables)",
        "vswitch_duration_ms",
        config.vswitch.duration_ms.to_string(),
      ),
      text_row(
        "Workspace transition gap",
        "vswitch_gap",
        config.vswitch.gap.to_string(),
      ),
      text_row(
        "Workspace transition background",
        "vswitch_background",
        config
          .vswitch
          .background
          .map(|component| component.to_string())
          .join(" "),
      ),
    ],
    Page::Tiling => vec![
      heading("Tiling layout"),
      text_row(
        "Tile new windows matching",
        "tile_by_default",
        config.tiling.tile_by_default.as_str().into(),
      ),
      checkbox_row(
        "keep_fullscreen_on_adjacent",
        "Keep fullscreen when focusing an adjacent tile",
        config.tiling.keep_fullscreen_on_adjacent,
      ),
      text_row(
        "Minimum tile extent",
        "minimum_extent",
        config.tiling.minimum_extent.to_string(),
      ),
      text_row(
        "Minimum floating resize width",
        "resize_min_width",
        config.resize.min_width.to_string(),
      ),
      text_row(
        "Minimum floating resize height",
        "resize_min_height",
        config.resize.min_height.to_string(),
      ),
      text_row(
        "Corner resize zone (0.0–0.5)",
        "resize_corner_threshold",
        config.resize.corner_threshold.to_string(),
      ),
      checkbox_row(
        "enable_snap",
        "Snap dragged windows to output edges",
        config.grid.enable_snap,
      ),
      text_row(
        "Edge snap threshold",
        "snap_threshold",
        config.grid.snap_threshold.to_string(),
      ),
      text_row(
        "Corner snap threshold",
        "quarter_snap_threshold",
        config.grid.quarter_snap_threshold.to_string(),
      ),
      checkbox_row(
        "enable_snap_off",
        "Restore snapped windows when dragged away",
        config.grid.enable_snap_off,
      ),
      text_row(
        "Snap-off threshold",
        "snap_off_threshold",
        config.grid.snap_off_threshold.to_string(),
      ),
      text_row(
        "Workspace switch delay (ms; -1 disables)",
        "workspace_switch_after",
        config.grid.workspace_switch_after.to_string(),
      ),
      checkbox_row(
        "join_views",
        "Move descendant dialogs with their parent",
        config.grid.join_views,
      ),
      text_row(
        "Touch-hold move finger count",
        "gesture_move_fingers",
        config.extra_gestures.move_fingers.to_string(),
      ),
      text_row(
        "Touch-hold move delay (ms)",
        "gesture_move_delay",
        config.extra_gestures.move_delay.to_string(),
      ),
      text_row(
        "Tap-to-close finger count (20 disables)",
        "gesture_close_fingers",
        config.extra_gestures.close_fingers.to_string(),
      ),
      text_row(
        "Outer gap — top",
        "gap_top",
        config.tiling.gaps.top.to_string(),
      ),
      text_row(
        "Outer gap — right",
        "gap_right",
        config.tiling.gaps.right.to_string(),
      ),
      text_row(
        "Outer gap — bottom",
        "gap_bottom",
        config.tiling.gaps.bottom.to_string(),
      ),
      text_row(
        "Outer gap — left",
        "gap_left",
        config.tiling.gaps.left.to_string(),
      ),
      text_row(
        "Inner gap",
        "gap_inner",
        config.tiling.gaps.inner.to_string(),
      ),
      text_row(
        "Geometry animation duration (ms)",
        "animation_duration",
        config.tiling.animation_duration.to_string(),
      ),
      text_row(
        "Preview fill (R G B A)",
        "preview_base_color",
        config
          .tiling
          .preview_base_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      text_row(
        "Preview border (R G B A)",
        "preview_border_color",
        config
          .tiling
          .preview_border_color
          .map(|component| component.to_string())
          .join(" "),
      ),
      text_row(
        "Preview border width",
        "preview_border_width",
        config.tiling.preview_border_width.to_string(),
      ),
    ],
    Page::Keybindings => {
      let mut rows = vec![
        heading("Keyboard shortcuts"),
        label(
          concat!(
            "Select a shortcut, then press the new key combination. ",
            "Escape cancels.",
          ),
          15.0,
          850.0,
          30.0,
        ),
      ];
      rows.extend(KEY_ACTIONS.iter().map(|(name, title)| {
        let waiting = model.capture_action == Some(*name);
        binding_row(title, name, binding(config, name), waiting)
      }));
      for (index, value) in
        config.keybindings.close_alternatives.iter().enumerate()
      {
        let action = format!("close_alternative:{index}");
        let waiting = model.capture_action == Some(action.as_str());
        rows.push(binding_row(
          &format!("Close focused window (alternative {})", index + 1),
          &action,
          value,
          waiting,
        ));
      }
      rows.push(heading("Workspace shortcuts"));
      for (action, title, bindings) in [
        (
          "workspace_switch",
          "Switch to workspace",
          &config.keybindings.workspace_switch,
        ),
        (
          "workspace_move",
          "Send window to workspace (stay)",
          &config.keybindings.workspace_move,
        ),
      ] {
        for (index, value) in bindings.iter().enumerate() {
          let action = format!("{action}:{index}");
          let waiting = model.capture_action == Some(action.as_str());
          rows.push(binding_row(
            &format!("{title} {}", index + 1),
            &action,
            value,
            waiting,
          ));
        }
      }
      rows.push(heading("Pointer bindings"));
      rows.push(text_row(
        "Move / reorder window",
        "pointer_move",
        String::from(config.pointer.move_button.clone()),
      ));
      rows.push(text_row(
        "Resize window",
        "pointer_resize",
        String::from(config.pointer.resize_button.clone()),
      ));
      rows.push(text_row(
        "Resize preserving aspect ratio",
        "pointer_resize_preserve_aspect",
        String::from(config.pointer.resize_preserve_aspect_button.clone()),
      ));
      rows.push(text_row(
        "Rotate window in 2D",
        "pointer_wrot_activate",
        String::from(config.pointer.wrot_activate.clone()),
      ));
      rows.push(heading("Shortcut inhibition"));
      rows.push(text_row(
        "Inhibit by default (view matcher)",
        "shortcuts_inhibit_by_default",
        config.shortcuts.inhibit_by_default.as_str().into(),
      ));
      rows.push(text_row(
        "Ignore matching views (view matcher)",
        "shortcuts_ignore_views",
        config.shortcuts.ignore_views.as_str().into(),
      ));
      rows
    }
    Page::Commands => {
      let mut rows = vec![
        heading("Command bindings"),
        label(
          concat!(
            "Commands run through /bin/sh with the nested Shingle ",
            "Wayland socket.",
          ),
          15.0,
          850.0,
          30.0,
        ),
      ];
      for (index, command) in config.commands.iter().enumerate() {
        rows.push(row(vec![
          label("Binding", 15.0, 65.0, 38.0),
          Widget::text_input(
            &format!("command_binding_{index}"),
            String::from(command.binding.clone()),
            180.0,
            38.0,
          ),
          label("Command", 15.0, 80.0, 38.0),
          Widget::text_input(
            &format!("command_value_{index}"),
            command.command.clone(),
            300.0,
            38.0,
          ),
          Widget::button(
            "Remove",
            format!("command_remove:{index}"),
            80.0,
            38.0,
          ),
        ]));
        rows.push(row(vec![
          label("Options", 15.0, 65.0, 32.0),
          Widget::checkbox(
            &format!("command_repeatable_{index}"),
            command.repeatable,
          ),
          label("Repeat", 15.0, 90.0, 32.0),
          Widget::checkbox(
            &format!("command_on_release_{index}"),
            command.on_release,
          ),
          label("On release", 15.0, 120.0, 32.0),
          Widget::checkbox(&format!("command_always_{index}"), command.always),
          label("Available while locked", 15.0, 210.0, 32.0),
        ]));
      }
      rows.push(Widget::button("Add command", "command_add", 160.0, 40.0));
      rows.push(heading("Autostart"));
      rows.push(label(
        concat!(
          "Run once at startup; commands already restored from the ",
          "session are skipped.",
        ),
        15.0,
        850.0,
        30.0,
      ));
      for (index, command) in config.autostart.iter().enumerate() {
        rows.push(row(vec![
          Widget::text_input(
            &format!("autostart_value_{index}"),
            command.clone(),
            740.0,
            38.0,
          ),
          Widget::button(
            "Remove",
            format!("autostart_remove:{index}"),
            100.0,
            38.0,
          ),
        ]));
      }
      rows.push(Widget::button(
        "Add autostart command",
        "autostart_add",
        210.0,
        40.0,
      ));
      rows
    }
    Page::WindowRules => {
      let mut rows = vec![
        heading("Window rules"),
        label(
          "Triggers: created, maximized, unmaximized, minimized, fullscreened",
          15.0,
          850.0,
          30.0,
        ),
        label(
          concat!(
            "Example: on maximized if app_id is \"application\" ",
            "then set alpha 0.8",
          ),
          15.0,
          850.0,
          30.0,
        ),
      ];
      for (index, rule) in config.window_rules.iter().enumerate() {
        rows.push(row(vec![
          Widget::text_input(
            &format!("rule_value_{index}"),
            String::from(rule.clone()),
            740.0,
            38.0,
          ),
          Widget::button("Remove", format!("rule_remove:{index}"), 100.0, 38.0),
        ]));
      }
      rows.push(Widget::button("Add rule", "rule_add", 160.0, 40.0));
      rows
    }
    Page::Input => vec![
      heading("XKB keyboard layout"),
      text_row("Rules", "input_rules", config.input.rules.clone()),
      text_row(
        "Complete keymap file (overrides generated keymap)",
        "input_xkb_file",
        config.input.xkb_file.clone(),
      ),
      text_row("Model", "input_model", config.input.model.clone()),
      text_row("Layout", "input_layout", config.input.layout.clone()),
      text_row("Variant", "input_variant", config.input.variant.clone()),
      text_row(
        "Options (comma-separated)",
        "input_options",
        config.input.options.clone(),
      ),
      text_row(
        "Repeat rate (keys/sec)",
        "input_repeat_rate",
        config.input.repeat_rate.to_string(),
      ),
      text_row(
        "Repeat delay (ms)",
        "input_repeat_delay",
        config.input.repeat_delay.to_string(),
      ),
      label(
        "Example options: compose:menu,caps:escape",
        15.0,
        700.0,
        30.0,
      ),
      checkbox_row(
        "input_numlock_default",
        "Enable NumLock at startup",
        config.input.numlock_default,
      ),
      checkbox_row(
        "input_capslock_default",
        "Enable CapsLock at startup",
        config.input.capslock_default,
      ),
      heading("Mouse scrolling"),
      text_row(
        "Scroll speed (non-negative)",
        "input_mouse_scroll_speed",
        config.input.mouse_scroll_speed.to_string(),
      ),
      checkbox_row(
        "input_mouse_natural_scroll",
        "Natural (inverted) scrolling",
        config.input.mouse_natural_scroll,
      ),
      heading("Mouse buttons"),
      checkbox_row(
        "input_left_handed_mode",
        "Left-handed mode (swap left and right)",
        config.input.left_handed_mode,
      ),
      checkbox_row(
        "input_middle_emulation",
        "Middle-click when left and right are held together",
        config.input.middle_emulation,
      ),
      heading("Cursor"),
      text_row(
        "Cursor theme (default follows XCURSOR_THEME)",
        "input_cursor_theme",
        config.input.cursor_theme.clone(),
      ),
      text_row(
        "Cursor size (default follows XCURSOR_SIZE)",
        "input_cursor_size",
        config.input.cursor_size.to_string(),
      ),
      checkbox_row(
        "enable_text_input_v3",
        "Enable text-input-v3 for editable surfaces",
        config.input.enable_text_input_v3,
      ),
      checkbox_row(
        "enable_text_input_v1",
        "Enable text-input-v1 (Chromium and Electron)",
        config.input.enable_text_input_v1,
      ),
    ],
    Page::IdleSession => vec![
      heading("Idle, display power, and locking"),
      text_row(
        "Idle timeout (seconds)",
        "idle_seconds",
        optional_number(config.idle.idle_seconds),
      ),
      text_row(
        "DPMS timeout (seconds)",
        "dpms_seconds",
        optional_number(config.idle.dpms_seconds),
      ),
      text_row(
        "Lock timeout (seconds)",
        "lock_seconds",
        optional_number(config.idle.lock_seconds),
      ),
      checkbox_row(
        "disable_on_fullscreen",
        "Disable idle timeouts while fullscreen",
        config.idle.disable_on_fullscreen,
      ),
      checkbox_row(
        "disable_initially",
        "Disable idle timeouts at startup",
        config.idle.disable_initially,
      ),
      label("Leave a timeout empty to disable it.", 15.0, 700.0, 30.0),
      heading("Desktop session"),
      checkbox_row(
        "restore_session",
        "Restore tiles, order, geometry, and applications",
        config.session.restore,
      ),
      text_row(
        "Session file (empty for default)",
        "session_path",
        config
          .session
          .path
          .as_ref()
          .map(|path| path.display().to_string())
          .unwrap_or_default(),
      ),
    ],
    Page::Launcher => launcher_page(config),
  };

  let mut content_widget =
    container(Some("content"), Direction::Column, 10.0, 16.0, content);
  if let Widget::Container {
    scrollable,
    flex,
    style,
    ..
  } = &mut content_widget
  {
    *scrollable = true;
    *flex = 1.0;
    *style = BoxStyle::fill(SURFACE).with_radius(6.0);
  }

  let status = if model.dirty {
    format!("Unsaved changes — {}", model.status)
  } else {
    model.status.to_owned()
  };
  container(
    Some("root"),
    Direction::Column,
    14.0,
    24.0,
    vec![
      label("Shingle Configuration Manager", 30.0, 900.0, 42.0),
      label(
        format!(
          "Source: {}  →  YAML: {}",
          model.source.display(),
          model.destination.display()
        ),
        14.0,
        1000.0,
        28.0,
      ),
      container(
        Some("navigation"),
        Direction::Row,
        10.0,
        0.0,
        navigation.collect(),
      ),
      content_widget,
      label(status, 15.0, 950.0, 30.0),
      container(
        Some("footer"),
        Direction::Row,
        12.0,
        0.0,
        vec![
          Widget::button("Save YAML", "save", 160.0, 42.0),
          Widget::button("Reload", "reload", 140.0, 42.0),
          Widget::button("Reset to defaults", "defaults", 190.0, 42.0),
        ],
      ),
    ],
  )
}

/// The colours of the `launcher` section: field ID suffix, title and
/// value.
pub fn launcher_colors(
  colors: &shingle_config::LauncherColors,
) -> [(&'static str, &'static str, [f64; 4]); 7] {
  [
    ("backdrop", "Backdrop over the output", colors.backdrop),
    ("panel", "Panel", colors.panel),
    ("field", "Search field and hovered rows", colors.field),
    ("text", "Text", colors.text),
    ("dim_text", "Hints and counts", colors.dim_text),
    ("accent", "Accent (caret, favourites)", colors.accent),
    ("selection", "Selected row and category", colors.selection),
  ]
}

/// The Launcher page: shingle-launcher's `launcher` section.
fn launcher_page(config: &ShingleConfig) -> Vec<Widget> {
  let launcher = &config.launcher;
  let mut page = vec![
    heading("Application launcher (shingle-launcher)"),
    text_row(
      "Panel width (logical pixels)",
      "launcher_width",
      launcher.width.to_string(),
    ),
    text_row(
      "Panel height (logical pixels)",
      "launcher_height",
      launcher.height.to_string(),
    ),
    text_row(
      "Font size (icons and rows scale with it)",
      "launcher_font_size",
      launcher.font_size.to_string(),
    ),
    text_row(
      "Icon theme (empty follows KDE or GTK)",
      "launcher_icon_theme",
      launcher.icon_theme.clone(),
    ),
    text_row(
      "Renderer (auto, software or hardware)",
      "launcher_renderer",
      launcher.renderer.as_str().to_owned(),
    ),
    text_row(
      "Terminal (empty picks $TERMINAL, foot, ...)",
      "launcher_terminal",
      launcher.terminal.clone(),
    ),
    heading("Launcher colours (R G B A, 0 to 1)"),
  ];
  page.extend(launcher_colors(&launcher.colors).into_iter().map(
    |(id, title, value)| {
      text_row(
        title,
        &format!("launcher_color_{id}"),
        value.map(|component| component.to_string()).join(" "),
      )
    },
  ));
  page.push(label(
    "The launcher reads this when it starts; no reload is needed.",
    15.0,
    700.0,
    30.0,
  ));
  page
}

fn binding<'a>(config: &'a ShingleConfig, name: &str) -> &'a Binding {
  let keys = &config.keybindings;
  if let Some(index) = name.strip_prefix("workspace_switch:") {
    return &keys.workspace_switch[index.parse::<usize>().unwrap()];
  }
  if let Some(index) = name.strip_prefix("workspace_move:") {
    return &keys.workspace_move[index.parse::<usize>().unwrap()];
  }
  match name {
    "workspace_next" => &keys.workspace_next,
    "workspace_previous" => &keys.workspace_previous,
    "workspace_up" => &keys.workspace_up,
    "workspace_down" => &keys.workspace_down,
    "workspace_move_next" => &keys.workspace_move_next,
    "workspace_move_up" => &keys.workspace_move_up,
    "workspace_move_down" => &keys.workspace_move_down,
    "workspace_move_previous" => &keys.workspace_move_previous,
    "workspace_send_next" => &keys.workspace_send_next,
    "workspace_send_previous" => &keys.workspace_send_previous,
    "workspace_send_up" => &keys.workspace_send_up,
    "workspace_send_down" => &keys.workspace_send_down,
    "workspace_last" => &keys.workspace_last,
    "workspace_move_last" => &keys.workspace_move_last,
    "workspace_send_last" => &keys.workspace_send_last,
    "focus_left" => &keys.focus_left,
    "focus_right" => &keys.focus_right,
    "focus_up" => &keys.focus_up,
    "focus_down" => &keys.focus_down,
    "focus_next" => &keys.focus_next,
    "focus_previous" => &keys.focus_previous,
    "toggle_fullscreen" => &keys.toggle_fullscreen,
    "toggle_sticky" => &keys.toggle_sticky,
    "scale_toggle" => &keys.scale_toggle,
    "scale_toggle_all" => &keys.scale_toggle_all,
    "fast_switcher_next" => &keys.fast_switcher_next,
    "fast_switcher_previous" => &keys.fast_switcher_previous,
    "invert_toggle" => &keys.invert_toggle,
    "wrot_reset_one" => &keys.wrot_reset_one,
    "wrot_reset" => &keys.wrot_reset,
    "always_on_top" => &keys.always_on_top,
    "always_on_bottom" => &keys.always_on_bottom,
    "minimize" => &keys.minimize,
    "bring_to_front" => &keys.bring_to_front,
    "send_to_back" => &keys.send_to_back,
    "show_desktop" => &keys.show_desktop,
    "toggle_maximized" => &keys.toggle_maximized,
    "toggle_tiling" => &keys.toggle_tiling,
    "move_left" => &keys.move_left,
    "move_right" => &keys.move_right,
    "move_up" => &keys.move_up,
    "move_down" => &keys.move_down,
    "resize_left" => &keys.resize_left,
    "resize_right" => &keys.resize_right,
    "resize_up" => &keys.resize_up,
    "resize_down" => &keys.resize_down,
    "grid_slot_top_left" => {
      keys.grid_slot_top_left.first().unwrap_or(&DISABLED_BINDING)
    }
    "grid_slot_top" => keys.grid_slot_top.first().unwrap_or(&DISABLED_BINDING),
    "grid_slot_top_right" => keys
      .grid_slot_top_right
      .first()
      .unwrap_or(&DISABLED_BINDING),
    "grid_slot_left" => {
      keys.grid_slot_left.first().unwrap_or(&DISABLED_BINDING)
    }
    "grid_slot_center" => {
      keys.grid_slot_center.first().unwrap_or(&DISABLED_BINDING)
    }
    "grid_slot_right" => {
      keys.grid_slot_right.first().unwrap_or(&DISABLED_BINDING)
    }
    "grid_slot_bottom_left" => keys
      .grid_slot_bottom_left
      .first()
      .unwrap_or(&DISABLED_BINDING),
    "grid_slot_bottom" => {
      keys.grid_slot_bottom.first().unwrap_or(&DISABLED_BINDING)
    }
    "grid_slot_bottom_right" => keys
      .grid_slot_bottom_right
      .first()
      .unwrap_or(&DISABLED_BINDING),
    "grid_restore" => keys.grid_restore.first().unwrap_or(&DISABLED_BINDING),
    "idle_toggle" => &keys.idle_toggle,
    "break_grab" => &keys.break_grab,
    "lock" => &keys.lock,
    "close" => &keys.close,
    "reload" => &keys.reload,
    "quit" => &keys.quit,
    "toggle_menu" => &keys.toggle_menu,
    _ => unreachable!("unknown keybinding"),
  }
}

fn binding_row(
  title: &str,
  action: &str,
  binding: &Binding,
  waiting: bool,
) -> Widget {
  let text = if waiting {
    "Press a key… (Esc cancels)".to_owned()
  } else {
    String::from(binding.clone())
  };
  let mut button =
    Widget::button(text, format!("capture:{action}"), 320.0, 38.0);
  if waiting && let Widget::Button { style, .. } = &mut button {
    style.idle = BoxStyle::fill(ACCENT).with_radius(5.0);
  }
  row(vec![label(title, 17.0, 380.0, 38.0), button])
}

fn text_row(title: &str, id: &str, value: String) -> Widget {
  row(vec![
    label(title, 17.0, 380.0, 38.0),
    Widget::text_input(id, value, 360.0, 38.0),
  ])
}

fn checkbox_row(id: &str, title: &str, checked: bool) -> Widget {
  row(vec![
    Widget::checkbox(id, checked),
    label(title, 17.0, 650.0, 32.0),
  ])
}

fn row(children: Vec<Widget>) -> Widget {
  let mut widget = container(None, Direction::Row, 16.0, 0.0, children);
  if let Widget::Container { layout, .. } = &mut widget {
    layout.align_items = Align::Center;
  }
  widget
}

fn heading(text: &str) -> Widget {
  label(text, 23.0, 800.0, 42.0)
}

fn label(
  text: impl Into<String>,
  size: f32,
  width: f32,
  height: f32,
) -> Widget {
  let mut widget = Widget::label(text);
  if let Widget::Label {
    size: label_size,
    color,
    width: label_width,
    height: label_height,
    ..
  } = &mut widget
  {
    *label_size = size;
    *color = TEXT;
    *label_width = width;
    *label_height = height;
  }
  widget
}

fn container(
  id: Option<&str>,
  direction: Direction,
  spacing: f32,
  padding: f32,
  children: Vec<Widget>,
) -> Widget {
  let mut widget = Widget::container();
  if let Widget::Container {
    id: widget_id,
    style,
    padding: widget_padding,
    layout,
    children: widget_children,
    ..
  } = &mut widget
  {
    *widget_id = id.map(str::to_owned);
    *style = BoxStyle::fill(BACKGROUND);
    *widget_padding = padding;
    *layout = Layout {
      direction,
      spacing,
      align_items: Align::Start,
      justify_content: Justify::Start,
      template_columns: Vec::new(),
    };
    *widget_children = children;
  }
  widget
}

fn optional_number(value: Option<u64>) -> String {
  value.map(|value| value.to_string()).unwrap_or_default()
}

const BACKGROUND: (f32, f32, f32, f32) = (0.075, 0.085, 0.105, 1.0);
const SURFACE: (f32, f32, f32, f32) = (0.105, 0.12, 0.15, 1.0);
const ACCENT: (f32, f32, f32, f32) = (0.12, 0.42, 0.62, 1.0);
const TEXT: (f32, f32, f32, f32) = (0.9, 0.92, 0.95, 1.0);

trait WidgetExt {
  fn button(
    text: impl Into<String>,
    action: impl Into<String>,
    width: f32,
    height: f32,
  ) -> Self;
  fn text_input(id: &str, value: String, width: f32, height: f32) -> Self;
  fn checkbox(id: &str, checked: bool) -> Self;
}

impl WidgetExt for Widget {
  fn button(
    text: impl Into<String>,
    action: impl Into<String>,
    width: f32,
    height: f32,
  ) -> Self {
    Self::Button {
      text: text.into(),
      action: action.into(),
      bounds: WidgetBounds {
        width,
        height,
        ..WidgetBounds::default()
      },
      style: ButtonStyle::default(),
      width: Some(width),
      height: Some(height),
      disabled: false,
      layout: Layout::default(),
      flex: 0.0,
      grid_col: None,
      grid_row: None,
      col_span: 1,
      row_span: 1,
      font: None,
    }
  }

  fn text_input(id: &str, value: String, width: f32, height: f32) -> Self {
    Self::TextInput {
      value,
      placeholder: String::new(),
      id: id.to_owned(),
      font_size: 16.0,
      text_align: gloomy_core::widget::TextAlign::Left,
      bounds: WidgetBounds {
        width,
        height,
        ..WidgetBounds::default()
      },
      validation: None,
      style: TextInputStyle::default(),
      width,
      height,
      flex: 0.0,
      grid_col: None,
      grid_row: None,
      col_span: 1,
      row_span: 1,
    }
  }

  fn checkbox(id: &str, checked: bool) -> Self {
    Self::Checkbox {
      id: id.to_owned(),
      checked,
      size: 24.0,
      style: Default::default(),
      bounds: WidgetBounds::default(),
      flex: 0.0,
      grid_col: None,
      grid_row: None,
      col_span: 1,
      row_span: 1,
    }
  }
}

#[cfg(test)]
mod tests {
  use gloomy_core::ui::find_widget_mut;

  use super::*;

  /// The value of the text input `id` on `page` for `config`.
  fn text_value(config: &ShingleConfig, page: Page, id: &str) -> String {
    let mut ui = build(&UiModel {
      config,
      source: Path::new("in.yaml"),
      destination: Path::new("out.yaml"),
      page,
      capture_action: None,
      status: "",
      dirty: false,
    });
    match find_widget_mut(&mut ui, id) {
      Some(Widget::TextInput { value, .. }) => value.clone(),
      _ => panic!("no text input {id}"),
    }
  }

  #[test]
  fn general_page_shows_the_xwayland_options() {
    let mut config = ShingleConfig::default();
    assert_eq!(text_value(&config, Page::General, "xwayland_mode"), "lazy");
    config.xwayland.mode = shingle_config::XwaylandMode::Disabled;
    config.xwayland.startup_script = "xrdb -merge x".into();
    assert_eq!(text_value(&config, Page::General, "xwayland_mode"), "false");
    assert_eq!(
      text_value(&config, Page::General, "xwayland_startup_script"),
      "xrdb -merge x"
    );
  }

  #[test]
  fn the_launcher_page_shows_the_launcher_section() {
    let mut config = ShingleConfig::default();
    config.launcher.width = 900;
    config.launcher.icon_theme = "breeze".into();
    config.launcher.renderer = shingle_config::LauncherRenderer::Hardware;
    config.launcher.colors.accent = [1.0, 0.5, 0.0, 1.0];
    let value = |id: &str| text_value(&config, Page::Launcher, id);
    assert_eq!(value("launcher_width"), "900");
    assert_eq!(value("launcher_font_size"), "16");
    assert_eq!(value("launcher_icon_theme"), "breeze");
    assert_eq!(value("launcher_renderer"), "hardware");
    assert_eq!(value("launcher_terminal"), "");
    assert_eq!(value("launcher_color_accent"), "1 0.5 0 1");
    assert_eq!(value("launcher_color_backdrop"), "0 0 0 0.45");
    assert_eq!(Page::named("launcher"), Some(Page::Launcher));
  }
}
