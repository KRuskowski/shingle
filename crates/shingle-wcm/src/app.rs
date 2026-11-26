use std::{cell::RefCell, path::PathBuf, rc::Rc, str::FromStr};

use anyhow::Result;
use gloomy_app::GloomyApp;
use gloomy_core::{
  InteractionState, Vec2, Widget, compute_layout,
  key_chord::{CaptureResult, KeyCapture},
  render_ui,
  ui::{find_widget_mut, handle_interactions, handle_keyboard_event, hit_test},
};
use shingle_config::{Binding, ShingleConfig};
use winit::{
  event::{ElementState, KeyEvent, MouseScrollDelta},
  keyboard::ModifiersState,
};

use crate::ui::{self, Page, UiModel};

pub fn run(
  config: ShingleConfig,
  source: PathBuf,
  destination: PathBuf,
) -> Result<()> {
  let editor = Rc::new(RefCell::new(Editor::new(config, source, destination)));
  let cursor_editor = editor.clone();
  let mouse_editor = editor.clone();
  let keyboard_editor = editor.clone();
  let modifiers_editor = editor.clone();
  let scroll_editor = editor.clone();
  let draw_editor = editor;

  GloomyApp::new()
    .with_title("Shingle Configuration Manager")
    .with_size(1100, 850)
    .on_cursor_move(move |window, x, y| {
      cursor_editor
        .borrow_mut()
        .interaction
        .update_mouse(Vec2::new(x, y));
      window.window.request_redraw();
    })
    .on_mouse_input(move |window, state, _button| {
      mouse_editor.borrow_mut().mouse_input(state);
      window.window.request_redraw();
    })
    .on_keyboard_input(move |window, event| {
      keyboard_editor.borrow_mut().keyboard_input(event);
      window.window.request_redraw();
    })
    .on_modifiers_changed(move |_window, modifiers| {
      modifiers_editor.borrow_mut().modifiers = modifiers.state();
    })
    .on_scroll(move |window, delta, _phase| {
      scroll_editor.borrow_mut().scroll(delta);
      window.window.request_redraw();
    })
    .on_draw(move |window, context| {
      let mut editor = draw_editor.borrow_mut();
      let size = window.renderer.size();
      if let Widget::Container { bounds, .. } = &mut editor.ui {
        bounds.width = size.x;
        bounds.height = size.y;
      }
      compute_layout(&mut editor.ui, 0.0, 0.0, size.x, size.y);
      let interaction = editor.interaction.clone();
      if handle_interactions(&mut editor.ui, &interaction, Vec2::ZERO) {
        editor.dirty = true;
        if let Err(error) = editor.sync_page() {
          editor.status = error;
        }
        editor.interaction.set_clicked(None);
        editor.rebuild();
        compute_layout(&mut editor.ui, 0.0, 0.0, size.x, size.y);
      }
      if let Some(hit) = hit_test(
        &editor.ui,
        editor.interaction.mouse_pos,
        Some(&editor.interaction),
      ) {
        editor.interaction.hovered_action = Some(hit.action);
      } else {
        editor.interaction.hovered_action = None;
      }
      render_ui(
        &editor.ui,
        &mut window.renderer,
        context.device,
        context.queue,
        Some(&editor.interaction),
        None,
      );
      editor.interaction.set_clicked(None);
    })
    .run()
}

struct Editor {
  config: ShingleConfig,
  source: PathBuf,
  destination: PathBuf,
  page: Page,
  ui: Widget,
  interaction: InteractionState,
  modifiers: ModifiersState,
  capture: KeyCapture,
  capture_action: Option<String>,
  status: String,
  dirty: bool,
}

impl Editor {
  fn new(config: ShingleConfig, source: PathBuf, destination: PathBuf) -> Self {
    let mut editor = Self {
      config,
      source,
      destination,
      page: Page::General,
      ui: Widget::container(),
      interaction: InteractionState::default(),
      modifiers: ModifiersState::empty(),
      capture: KeyCapture::default(),
      capture_action: None,
      status: "Ready".to_owned(),
      dirty: false,
    };
    editor.rebuild();
    editor
  }

  fn rebuild(&mut self) {
    self.ui = ui::build(&UiModel {
      config: &self.config,
      source: &self.source,
      destination: &self.destination,
      page: self.page,
      capture_action: self.capture_action.as_deref(),
      status: &self.status,
      dirty: self.dirty,
    });
  }

  fn mouse_input(&mut self, state: ElementState) {
    let hit = hit_test(
      &self.ui,
      self.interaction.mouse_pos,
      Some(&self.interaction),
    )
    .map(|hit| hit.action);
    if state == ElementState::Pressed {
      self.interaction.set_pressed(true);
      self.interaction.set_active(hit.clone());
      self.interaction.set_clicked(hit.clone());
      self.interaction.focused_id = hit.filter(|action| {
        matches!(
          find_widget_mut(&mut self.ui, action),
          Some(Widget::TextInput { .. })
        )
      });
      return;
    }
    let active = self.interaction.active_action.take();
    self.interaction.set_pressed(false);
    self.interaction.set_clicked(None);
    if active.is_some() && active == hit {
      self.handle_action(active.as_deref().unwrap_or_default());
    }
  }

  fn keyboard_input(&mut self, event: KeyEvent) {
    if self.capture.is_waiting() {
      let (result, chord) = self.capture.capture(
        &event.logical_key,
        self.modifiers,
        event.state == ElementState::Pressed,
      );
      match result {
        CaptureResult::Cancelled => {
          self.capture_action = None;
          self.status = "Shortcut capture cancelled".to_owned();
          self.rebuild();
        }
        CaptureResult::Captured => {
          if let Some(chord) = chord {
            self.capture_binding(chord.to_string());
          }
        }
        CaptureResult::Ignored => {}
      }
      return;
    }
    if handle_keyboard_event(&mut self.ui, &mut self.interaction, &event) {
      self.dirty = true;
      if let Err(error) = self.sync_page() {
        self.status = error;
      }
    }
  }

  fn capture_binding(&mut self, text: String) {
    match Binding::try_from(text) {
      Ok(binding) => {
        let action = self.capture_action.take().unwrap_or_default();
        set_binding(&mut self.config, &action, binding.clone());
        self.status = format!("{} = {}", action, String::from(binding));
        self.dirty = true;
      }
      Err(error) => {
        self.status = format!("{error}; include at least one modifier");
        self.capture.begin();
      }
    }
    self.rebuild();
  }

  fn handle_action(&mut self, action: &str) {
    match action {
      "save" => self.save(),
      "reload" => self.reload(),
      "defaults" => {
        self.config = ShingleConfig::default();
        self.capture.cancel();
        self.capture_action = None;
        self.dirty = true;
        self.status = "Defaults loaded; Save YAML to apply them".to_owned();
        self.rebuild();
      }
      _ => {
        if let Some(page) = action.strip_prefix("page:") {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          self.page = match page {
            "general" => Page::General,
            "tiling" => Page::Tiling,
            "keys" => Page::Keybindings,
            "commands" => Page::Commands,
            "rules" => Page::WindowRules,
            "input" => Page::Input,
            "idle" => Page::IdleSession,
            _ => return,
          };
          self.capture.cancel();
          self.capture_action = None;
          self.rebuild();
        } else if let Some(action) = action.strip_prefix("capture:") {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          self.capture.begin();
          self.capture_action = Some(action.to_owned());
          self.status = "Waiting for a shortcut".to_owned();
          self.rebuild();
        } else if action == "command_add" {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          self.config.commands.push(shingle_config::CommandBinding {
            binding: Binding::new(
              "n",
              vec![
                shingle_config::Modifier::Super,
                shingle_config::Modifier::Control,
              ],
            ),
            command: String::new(),
            repeatable: false,
            on_release: false,
            always: false,
          });
          self.dirty = true;
          self.rebuild();
        } else if let Some(index) = action.strip_prefix("command_remove:") {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          if let Ok(index) = index.parse::<usize>()
            && index < self.config.commands.len()
          {
            self.config.commands.remove(index);
            self.dirty = true;
          }
          self.rebuild();
        } else if action == "autostart_add" {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          self.config.autostart.push(String::new());
          self.dirty = true;
          self.rebuild();
        } else if let Some(index) = action.strip_prefix("autostart_remove:") {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          if let Ok(index) = index.parse::<usize>()
            && index < self.config.autostart.len()
          {
            self.config.autostart.remove(index);
            self.dirty = true;
          }
          self.rebuild();
        } else if action == "rule_add" {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          self.config.window_rules.push(
            "on created if app_id is \"application\" then maximize"
              .to_owned()
              .try_into()
              .expect("the built-in rule template is valid"),
          );
          self.dirty = true;
          self.rebuild();
        } else if let Some(index) = action.strip_prefix("rule_remove:") {
          if let Err(error) = self.sync_page() {
            self.status = error;
            self.rebuild();
            return;
          }
          if let Ok(index) = index.parse::<usize>()
            && index < self.config.window_rules.len()
          {
            self.config.window_rules.remove(index);
            self.dirty = true;
          }
          self.rebuild();
        }
      }
    }
  }

  fn save(&mut self) {
    match self.sync_page().and_then(|()| {
      self
        .config
        .save_yaml(&self.destination)
        .map_err(|error| error.to_string())
    }) {
      Ok(()) => {
        self.dirty = false;
        self.status = format!("Saved {}", self.destination.display());
      }
      Err(error) => self.status = error,
    }
    self.rebuild();
  }

  fn reload(&mut self) {
    match ShingleConfig::load_with_warnings(Some(&self.source)) {
      Ok((config, warnings)) => {
        self.config = config;
        self.capture.cancel();
        self.capture_action = None;
        self.dirty = false;
        self.status = format!("Loaded {}", self.source.display());
        if !warnings.is_empty() {
          self.status += &format!(
            " with {} warnings; run shingle-wcm --check to list them",
            warnings.len()
          );
        }
      }
      Err(error) => self.status = error.to_string(),
    }
    self.rebuild();
  }

  fn scroll(&mut self, delta: MouseScrollDelta) {
    let y = match delta {
      MouseScrollDelta::LineDelta(_, y) => y * 40.0,
      MouseScrollDelta::PixelDelta(position) => position.y as f32,
    };
    self.interaction.handle_scroll("content", Vec2::new(0.0, y));
    let max_scroll = match find_widget_mut(&mut self.ui, "content") {
      Some(Widget::Container {
        bounds,
        padding,
        children,
        ..
      }) => {
        children
          .iter()
          .map(|child| {
            let bounds = child.bounds();
            bounds.y + bounds.height
          })
          .fold(0.0_f32, f32::max)
          + *padding
          - bounds.height
      }
      _ => 0.0,
    }
    .max(0.0);
    if let Some(offset) = self.interaction.scroll_offsets.get_mut("content") {
      *offset = Vec2::new(0.0, offset.y.clamp(0.0, max_scroll));
    }
  }

  fn sync_page(&mut self) -> std::result::Result<(), String> {
    match self.page {
      Page::General => {
        self.config.output.width = self.number("output_width")?;
        self.config.output.height = self.number("output_height")?;
        self.config.output.x = self.number("output_x")?;
        self.config.output.y = self.number("output_y")?;
        self.config.output.scale = self.number("output_scale")?;
        self.config.output.transform = self.text("output_transform")?;
        self.config.background_color =
          shingle_config::parse_color(&self.text("background_color")?)
            .map_err(|error| error.to_string())?;
        self.config.behavior.auto_reload = self.checkbox("auto_reload")?;
        self.config.behavior.config_reload_delay =
          self.number("config_reload_delay")?;
        self.config.behavior.all_dialogs_modal =
          self.checkbox("all_dialogs_modal")?;
        self.config.behavior.discard_command_output =
          self.checkbox("discard_command_output")?;
        self.config.behavior.keep_last_toplevel_activated =
          self.checkbox("keep_last_toplevel_activated")?;
        self.config.behavior.max_buffer_size =
          self.number("max_buffer_size")?;
        self.config.behavior.preferred_decoration_mode =
          self.text("preferred_decoration_mode")?;
        self.config.behavior.force_preferred_decoration_mode =
          self.checkbox("force_preferred_decoration_mode")?;
        self.config.behavior.focus_main_surface_instead_of_popup =
          self.checkbox("focus_main_surface_instead_of_popup")?;
        self.config.behavior.disable_primary_selection =
          self.checkbox("disable_primary_selection")?;
        self.config.xwayland.mode = self
          .text("xwayland_mode")?
          .parse()
          .map_err(|error: shingle_config::ConfigError| error.to_string())?;
        self.config.xwayland.startup_script =
          self.text("xwayland_startup_script")?;
        self.config.security_context.privileged_protocols =
          self.text("security_privileged_protocols")?;
        self.config.placement.mode = self.text("placement_mode")?;
        self.config.zoom.modifier = self.text("zoom_modifier")?;
        self.config.zoom.speed = self.number("zoom_speed")?;
        self.config.zoom.smoothing_ms = self.number("zoom_smoothing_ms")?;
        self.config.zoom.interpolation = self.text("zoom_interpolation")?;
        self.config.alpha.modifier = self.text("alpha_modifier")?;
        self.config.alpha.min_value = self.number("alpha_min_value")?;
        self.config.fast_switcher.inactive_alpha =
          self.number("fast_switcher_inactive_alpha")?;
        self.config.scale.duration_ms = self.number("scale_duration_ms")?;
        self.config.scale.include_minimized =
          self.checkbox("scale_include_minimized")?;
        self.config.scale.close_on_new_view =
          self.checkbox("scale_close_on_new_view")?;
        self.config.scale.minimized_alpha =
          self.number("scale_minimized_alpha")?;
        self.config.scale.title_overlay = self.text("scale_title_overlay")?;
        self.config.scale.title_font_size =
          self.number("scale_title_font_size")?;
        self.config.scale.title_position = self.text("scale_title_position")?;
        self.config.scale.title_bg_color =
          shingle_config::parse_color(&self.text("scale_title_bg_color")?)
            .map_err(|error| error.to_string())?;
        self.config.scale.title_text_color =
          shingle_config::parse_color(&self.text("scale_title_text_color")?)
            .map_err(|error| error.to_string())?;
        self.config.scale_title_filter.font_size =
          self.number("scale_filter_font_size")?;
        self.config.scale_title_filter.case_sensitive =
          self.checkbox("scale_filter_case_sensitive")?;
        self.config.scale_title_filter.overlay =
          self.checkbox("scale_filter_overlay")?;
        self.config.scale_title_filter.bg_color =
          shingle_config::parse_color(&self.text("scale_filter_bg_color")?)
            .map_err(|error| error.to_string())?;
        self.config.scale_title_filter.text_color =
          shingle_config::parse_color(&self.text("scale_filter_text_color")?)
            .map_err(|error| error.to_string())?;
        self.config.scale.middle_click_close =
          self.checkbox("scale_middle_click_close")?;
        self.config.expo.background =
          shingle_config::parse_color(&self.text("expo_background")?)
            .map_err(|error| error.to_string())?;
        self.config.expo.duration_ms = self.number("expo_duration_ms")?;
        self.config.expo.transition_length_ms =
          self.number("expo_transition_length_ms")?;
        self.config.expo.offset = self.number("expo_offset")?;
        self.config.expo.keyboard_interaction =
          self.checkbox("expo_keyboard_interaction")?;
        self.config.expo.inactive_brightness =
          self.number("expo_inactive_brightness")?;
        self.config.expo.workspace_bindings = self
          .text("expo_workspace_bindings")?
          .split(',')
          .filter(|entry| !entry.trim().is_empty())
          .map(|entry| {
            let (workspace, binding) =
              entry.trim().split_once('=').ok_or_else(|| {
                "Expo bindings need workspace=binding".to_owned()
              })?;
            Ok(shingle_config::ExpoWorkspaceBinding {
              workspace: workspace
                .trim()
                .parse()
                .map_err(|_| "invalid Expo workspace number".to_owned())?,
              binding: parse_binding(binding.trim())?,
            })
          })
          .collect::<Result<Vec<_>, String>>()?;
        self.config.animate.enabled_for = self
          .text("animation_enabled_for")?
          .try_into()
          .map_err(|error: shingle_config::ConfigError| error.to_string())?;
        self.config.animate.open_animation = self.text("open_animation")?;
        self.config.animate.close_animation = self.text("close_animation")?;
        self.config.animate.minimize_animation =
          self.text("minimize_animation")?;
        self.config.animate.duration_ms =
          self.number("open_animation_duration")?;
        self.config.effects.invert_preserve_hue =
          self.checkbox("invert_preserve_hue")?;
        self.config.effects.wrot_reset_radius =
          self.number("wrot_reset_radius")?;
        self.config.focus.buttons = self
          .text("focus_buttons")?
          .split(',')
          .filter(|value| !value.trim().is_empty())
          .map(|value| parse_binding(value.trim()))
          .collect::<Result<Vec<_>, _>>()?;
        self.config.focus.button_with_modifiers =
          self.checkbox("focus_button_with_modifiers")?;
        self.config.focus.buttons_passthrough =
          self.checkbox("focus_buttons_passthrough")?;
        self.config.activation.check_surface =
          self.checkbox("activation_check_surface")?;
        self.config.activation.only_last_request =
          self.checkbox("activation_only_last_request")?;
        self.config.activation.timeout_seconds =
          self.number("activation_timeout")?;
        self.config.workspaces.width = self.number("workspace_width")?;
        self.config.workspaces.height = self.number("workspace_height")?;
        self.config.workspaces.wraparound =
          self.text("workspace_wraparound")?;
        self.config.vswitch.duration_ms = self.number("vswitch_duration_ms")?;
        self.config.vswitch.gap = self.number("vswitch_gap")?;
        self.config.vswitch.background =
          shingle_config::parse_color(&self.text("vswitch_background")?)
            .map_err(|error| error.to_string())?;
      }
      Page::Tiling => {
        self.config.tiling.tile_by_default = self
          .text("tile_by_default")?
          .try_into()
          .map_err(|error: shingle_config::ConfigError| error.to_string())?;
        self.config.tiling.keep_fullscreen_on_adjacent =
          self.checkbox("keep_fullscreen_on_adjacent")?;
        self.config.tiling.minimum_extent = self.number("minimum_extent")?;
        self.config.grid.enable_snap = self.checkbox("enable_snap")?;
        self.config.grid.snap_threshold = self.number("snap_threshold")?;
        self.config.grid.quarter_snap_threshold =
          self.number("quarter_snap_threshold")?;
        self.config.grid.enable_snap_off = self.checkbox("enable_snap_off")?;
        self.config.grid.snap_off_threshold =
          self.number("snap_off_threshold")?;
        self.config.grid.workspace_switch_after =
          self.number("workspace_switch_after")?;
        self.config.grid.join_views = self.checkbox("join_views")?;
        self.config.extra_gestures.move_fingers =
          self.number("gesture_move_fingers")?;
        self.config.extra_gestures.move_delay =
          self.number("gesture_move_delay")?;
        self.config.extra_gestures.close_fingers =
          self.number("gesture_close_fingers")?;
        self.config.tiling.gaps.top = self.number("gap_top")?;
        self.config.tiling.gaps.right = self.number("gap_right")?;
        self.config.tiling.gaps.bottom = self.number("gap_bottom")?;
        self.config.tiling.gaps.left = self.number("gap_left")?;
        self.config.tiling.gaps.inner = self.number("gap_inner")?;
        self.config.tiling.animation_duration =
          self.number("animation_duration")?;
        self.config.tiling.preview_base_color =
          shingle_config::parse_color(&self.text("preview_base_color")?)
            .map_err(|error| error.to_string())?;
        self.config.tiling.preview_border_color =
          shingle_config::parse_color(&self.text("preview_border_color")?)
            .map_err(|error| error.to_string())?;
        self.config.tiling.preview_border_width =
          self.number("preview_border_width")?;
        self.config.resize.min_width = self.number("resize_min_width")?;
        self.config.resize.min_height = self.number("resize_min_height")?;
        self.config.resize.corner_threshold =
          self.number("resize_corner_threshold")?;
      }
      Page::Keybindings => {
        self.config.pointer.move_button =
          parse_binding(&self.text("pointer_move")?)?;
        self.config.pointer.resize_button =
          parse_binding(&self.text("pointer_resize")?)?;
        self.config.pointer.resize_preserve_aspect_button =
          parse_binding(&self.text("pointer_resize_preserve_aspect")?)?;
        self.config.pointer.wrot_activate =
          parse_binding(&self.text("pointer_wrot_activate")?)?;
        self.config.shortcuts.inhibit_by_default = self
          .text("shortcuts_inhibit_by_default")?
          .try_into()
          .map_err(|error: shingle_config::ConfigError| error.to_string())?;
        self.config.shortcuts.ignore_views = self
          .text("shortcuts_ignore_views")?
          .try_into()
          .map_err(|error: shingle_config::ConfigError| error.to_string())?;
      }
      Page::Commands => {
        for index in 0..self.config.commands.len() {
          self.config.commands[index].binding =
            parse_binding(&self.text(&format!("command_binding_{index}"))?)?;
          self.config.commands[index].command =
            self.text(&format!("command_value_{index}"))?;
          self.config.commands[index].repeatable =
            self.checkbox(&format!("command_repeatable_{index}"))?;
          self.config.commands[index].on_release =
            self.checkbox(&format!("command_on_release_{index}"))?;
          self.config.commands[index].always =
            self.checkbox(&format!("command_always_{index}"))?;
        }
        for index in 0..self.config.autostart.len() {
          self.config.autostart[index] =
            self.text(&format!("autostart_value_{index}"))?;
        }
      }
      Page::WindowRules => {
        let mut rules = Vec::with_capacity(self.config.window_rules.len());
        for index in 0..self.config.window_rules.len() {
          rules.push(
            self
              .text(&format!("rule_value_{index}"))?
              .try_into()
              .map_err(|error: shingle_config::ConfigError| {
                error.to_string()
              })?,
          );
        }
        self.config.window_rules = rules;
      }
      Page::Input => {
        self.config.input.rules = self.text("input_rules")?;
        self.config.input.xkb_file = self.text("input_xkb_file")?;
        self.config.input.model = self.text("input_model")?;
        self.config.input.layout = self.text("input_layout")?;
        self.config.input.variant = self.text("input_variant")?;
        self.config.input.options = self.text("input_options")?;
        self.config.input.repeat_rate = self.number("input_repeat_rate")?;
        self.config.input.repeat_delay = self.number("input_repeat_delay")?;
        self.config.input.numlock_default =
          self.checkbox("input_numlock_default")?;
        self.config.input.capslock_default =
          self.checkbox("input_capslock_default")?;
        self.config.input.mouse_scroll_speed =
          self.number("input_mouse_scroll_speed")?;
        self.config.input.mouse_natural_scroll =
          self.checkbox("input_mouse_natural_scroll")?;
        self.config.input.left_handed_mode =
          self.checkbox("input_left_handed_mode")?;
        self.config.input.middle_emulation =
          self.checkbox("input_middle_emulation")?;
        self.config.input.cursor_theme = self.text("input_cursor_theme")?;
        self.config.input.cursor_size = self.number("input_cursor_size")?;
        self.config.input.enable_text_input_v3 =
          self.checkbox("enable_text_input_v3")?;
      }
      Page::IdleSession => {
        self.config.idle.idle_seconds = self.optional_number("idle_seconds")?;
        self.config.idle.dpms_seconds = self.optional_number("dpms_seconds")?;
        self.config.idle.lock_seconds = self.optional_number("lock_seconds")?;
        self.config.idle.disable_on_fullscreen =
          self.checkbox("disable_on_fullscreen")?;
        self.config.idle.disable_initially =
          self.checkbox("disable_initially")?;
        self.config.session.restore = self.checkbox("restore_session")?;
        let path = self.text("session_path")?;
        self.config.session.path =
          (!path.is_empty()).then_some(PathBuf::from(path));
      }
    }
    self.config.validate().map_err(|error| error.to_string())
  }

  fn text(&mut self, id: &str) -> std::result::Result<String, String> {
    match find_widget_mut(&mut self.ui, id) {
      Some(Widget::TextInput { value, .. }) => Ok(value.trim().to_owned()),
      _ => Err(format!("configuration field {id} is unavailable")),
    }
  }

  fn checkbox(&mut self, id: &str) -> std::result::Result<bool, String> {
    match find_widget_mut(&mut self.ui, id) {
      Some(Widget::Checkbox { checked, .. }) => Ok(*checked),
      _ => Err(format!("configuration field {id} is unavailable")),
    }
  }

  fn number<T>(&mut self, id: &str) -> std::result::Result<T, String>
  where
    T: FromStr,
    T::Err: std::fmt::Display,
  {
    self
      .text(id)?
      .parse()
      .map_err(|error| format!("invalid value for {id}: {error}"))
  }

  fn optional_number<T>(
    &mut self,
    id: &str,
  ) -> std::result::Result<Option<T>, String>
  where
    T: FromStr,
    T::Err: std::fmt::Display,
  {
    let text = self.text(id)?;
    if text.is_empty() {
      Ok(None)
    } else {
      text
        .parse()
        .map(Some)
        .map_err(|error| format!("invalid value for {id}: {error}"))
    }
  }
}

fn parse_binding(text: &str) -> std::result::Result<Binding, String> {
  Binding::try_from(text.to_owned()).map_err(|error| error.to_string())
}

fn set_binding(config: &mut ShingleConfig, action: &str, binding: Binding) {
  let keys = &mut config.keybindings;
  if let Some(index) = action.strip_prefix("close_alternative:") {
    keys.close_alternatives[index.parse::<usize>().unwrap()] = binding;
    return;
  }
  if let Some(index) = action.strip_prefix("workspace_switch:") {
    keys.workspace_switch[index.parse::<usize>().unwrap()] = binding;
    return;
  }
  if let Some(index) = action.strip_prefix("workspace_move:") {
    keys.workspace_move[index.parse::<usize>().unwrap()] = binding;
    return;
  }
  match action {
    "workspace_next" => keys.workspace_next = binding,
    "workspace_previous" => keys.workspace_previous = binding,
    "workspace_up" => keys.workspace_up = binding,
    "workspace_down" => keys.workspace_down = binding,
    "workspace_move_next" => keys.workspace_move_next = binding,
    "workspace_move_up" => keys.workspace_move_up = binding,
    "workspace_move_down" => keys.workspace_move_down = binding,
    "workspace_move_previous" => keys.workspace_move_previous = binding,
    "workspace_send_next" => keys.workspace_send_next = binding,
    "workspace_send_previous" => keys.workspace_send_previous = binding,
    "workspace_send_up" => keys.workspace_send_up = binding,
    "workspace_send_down" => keys.workspace_send_down = binding,
    "workspace_last" => keys.workspace_last = binding,
    "workspace_move_last" => keys.workspace_move_last = binding,
    "workspace_send_last" => keys.workspace_send_last = binding,
    "focus_left" => keys.focus_left = binding,
    "focus_right" => keys.focus_right = binding,
    "focus_up" => keys.focus_up = binding,
    "focus_down" => keys.focus_down = binding,
    "focus_next" => keys.focus_next = binding,
    "focus_previous" => keys.focus_previous = binding,
    "toggle_fullscreen" => keys.toggle_fullscreen = binding,
    "toggle_sticky" => keys.toggle_sticky = binding,
    "scale_toggle" => keys.scale_toggle = binding,
    "scale_toggle_all" => keys.scale_toggle_all = binding,
    "expo_toggle" => keys.expo_toggle = binding,
    "fast_switcher_next" => keys.fast_switcher_next = binding,
    "fast_switcher_previous" => keys.fast_switcher_previous = binding,
    "invert_toggle" => keys.invert_toggle = binding,
    "wrot_reset_one" => keys.wrot_reset_one = binding,
    "wrot_reset" => keys.wrot_reset = binding,
    "always_on_top" => keys.always_on_top = binding,
    "always_on_bottom" => keys.always_on_bottom = binding,
    "minimize" => keys.minimize = binding,
    "bring_to_front" => keys.bring_to_front = binding,
    "send_to_back" => keys.send_to_back = binding,
    "show_desktop" => keys.show_desktop = binding,
    "toggle_maximized" => keys.toggle_maximized = binding,
    "toggle_tiling" => keys.toggle_tiling = binding,
    "move_left" => keys.move_left = binding,
    "move_right" => keys.move_right = binding,
    "move_up" => keys.move_up = binding,
    "move_down" => keys.move_down = binding,
    "resize_left" => keys.resize_left = binding,
    "resize_right" => keys.resize_right = binding,
    "resize_up" => keys.resize_up = binding,
    "resize_down" => keys.resize_down = binding,
    "idle_toggle" => keys.idle_toggle = binding,
    "break_grab" => keys.break_grab = binding,
    "lock" => keys.lock = binding,
    "close" => keys.close = binding,
    "reload" => keys.reload = binding,
    "quit" => keys.quit = binding,
    "grid_slot_top_left" => keys.grid_slot_top_left = vec![binding],
    "grid_slot_top" => keys.grid_slot_top = vec![binding],
    "grid_slot_top_right" => keys.grid_slot_top_right = vec![binding],
    "grid_slot_left" => keys.grid_slot_left = vec![binding],
    "grid_slot_center" => keys.grid_slot_center = vec![binding],
    "grid_slot_right" => keys.grid_slot_right = vec![binding],
    "grid_slot_bottom_left" => keys.grid_slot_bottom_left = vec![binding],
    "grid_slot_bottom" => keys.grid_slot_bottom = vec![binding],
    "grid_slot_bottom_right" => keys.grid_slot_bottom_right = vec![binding],
    "grid_restore" => keys.grid_restore = vec![binding],
    _ => {}
  }
}
