//! The frontend state machine: input dispatch, overviews, drags,
//! animations, the scene build and session handling.

mod drag;
mod session;
mod switcher;

use std::{
  collections::{HashMap, HashSet},
  error::Error,
  process::Child,
  time::{Duration, Instant},
};

use shingle::{
  BufferSnapshot, Compositor, Config, Direction, Event, InputDeviceId,
  InputDeviceKind, SurfaceId, SurfaceRole, SurfaceSnapshot,
};
use shingle_config::{
  CommandBinding, ConfigWatcher, Keybindings, Modifiers as ConfigModifiers,
  PointerBindings, ShingleConfig, ViewProperties,
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuTexture, RenderRect, RenderTile, TextureTransform, animation_progress,
  preview_tiles,
};
use shingle_session::{
  SessionStore, launch_autostart, launch_command, launch_saved,
};

use crate::{
  bindings::{grid_slot, modifier_matches, modifiers_held},
  expo, extra_gestures,
  host::{Axis, Gesture, Host, InputEvent, Touch, TouchPhase},
  icon_theme,
  keyboard::{Key, KeyRepeat, KeyState, Keyboard},
  render_target::RenderTarget,
  scale::{
    ScaleState, fit_scale_tile, interpolate_scale_rect, rect_contains,
    scale_cells, scale_columns, scale_filter_matches, scale_geometry_at,
  },
  scale_title, workspace_transition,
};

use drag::{DragState, RotationDrag};
use switcher::{SwitcherKind, SwitcherState};

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;
/// `BTN_RIGHT`.
const BTN_RIGHT: u32 = 0x111;
/// `BTN_MIDDLE`.
const BTN_MIDDLE: u32 = 0x112;

struct GeometryAnimation {
  started: Instant,
  old_geometry: RenderRect,
  old_texture: GpuTexture,
}

struct ClosingAnimation {
  window: TileWindowId,
  started: Instant,
  geometry: RenderRect,
  buffer: BufferSnapshot,
}

struct ScaleTitle {
  title: String,
  target_width: u32,
  texture: GpuTexture,
}

/// Shingle's interactive frontend: everything between a host's input and
/// display and the compositor.
///
/// It owns the [`Compositor`] and drives Scale, Expo, the switchers,
/// workspace transitions, extra gestures, drags, window rotation, zoom,
/// inversion, alpha, animations, the scene build, binding dispatch, config
/// reload, session saving and the lock client. A host feeds it input with
/// [`Frontend::handle_input`], calls [`Frontend::process`] on every turn of
/// its event loop and [`Frontend::render`] when it can draw, and wakes up
/// by [`Frontend::next_deadline`].
pub struct Frontend {
  compositor: Compositor,
  store: SessionStore,
  textures: HashMap<SurfaceId, GpuTexture>,
  animations: HashMap<SurfaceId, GeometryAnimation>,
  open_animations: HashMap<SurfaceId, Instant>,
  closing_animations: HashMap<SurfaceId, ClosingAnimation>,
  minimize_animations: HashMap<SurfaceId, ClosingAnimation>,
  animation_enabled: HashSet<SurfaceId>,
  surface_windows: HashMap<SurfaceId, TileWindowId>,
  animate: shingle_config::AnimateConfig,
  previous_surfaces: HashMap<SurfaceId, (RenderRect, BufferSnapshot)>,
  animation_duration: Duration,
  dirty: HashSet<SurfaceId>,
  lock_client: Option<Child>,
  session_children: Vec<Child>,
  keyboard: Keyboard,
  key_repeat: KeyRepeat,
  /// The keyboard whose key repeats.
  repeat_device: Option<InputDeviceId>,
  switcher: Option<SwitcherState>,
  switcher_inactive_alpha: f32,
  scale: Option<ScaleState>,
  scale_exit: Option<ScaleState>,
  scale_config: shingle_config::ScaleConfig,
  scale_title_filter: shingle_config::ScaleTitleFilterConfig,
  scale_titles: HashMap<TileWindowId, ScaleTitle>,
  scale_icons: HashMap<TileWindowId, GpuTexture>,
  scale_filter: String,
  scale_filter_texture: Option<ScaleTitle>,
  expo: Option<expo::ExpoState>,
  expo_exit: Option<expo::ExpoState>,
  expo_touch: Option<u64>,
  expo_config: shingle_config::ExpoConfig,
  workspace_transition: Option<workspace_transition::WorkspaceTransition>,
  vswitch: shingle_config::VswitchConfig,
  last_workspace: usize,
  left_handed_mode: bool,
  middle_emulation: bool,
  pointer_x: f64,
  pointer_y: f64,
  keybindings: Keybindings,
  commands: Vec<CommandBinding>,
  pointer_bindings: PointerBindings,
  drag: Option<DragState>,
  gestures: extra_gestures::GestureState,
  rotations: HashMap<TileWindowId, f32>,
  rotation_drag: Option<RotationDrag>,
  wrot_reset_radius: f64,
  minimum_extent: u32,
  resize: shingle_config::ResizeConfig,
  discard_command_output: bool,
  zoom: shingle_config::ZoomConfig,
  alpha: shingle_config::AlphaConfig,
  inverted: bool,
  invert_preserve_hue: bool,
  zoom_factor: f32,
  zoom_target: f32,
  zoom_animation: Option<(Instant, f32)>,
  zoom_origin: (f32, f32),
  background_color: [f64; 4],
  preview_base_color: [f64; 4],
  preview_border_color: [f64; 4],
  preview_border_width: u32,
  drop_preview: Option<RenderRect>,
  drop_slot: Option<&'static str>,
  grid_snap: bool,
  snap_threshold: u32,
  quarter_snap_threshold: u32,
  enable_snap_off: bool,
  snap_off_threshold: u32,
  join_views: bool,
  workspace_switch_after: Option<Duration>,
  workspace_switch_started: Option<(i32, i32, Instant)>,
  config_path: Option<std::path::PathBuf>,
  config_watcher: ConfigWatcher,
  auto_reload_config: bool,
  config_reload_delay: Duration,
  last_config_poll: Instant,
  quit: bool,
  /// Whether the host should be asked for a frame at the end of the call.
  redraw_requested: bool,
}

impl Frontend {
  /// Starts the frontend: restores the saved session or creates a new
  /// compositor, launches restored and autostart clients, prints
  /// `WAYLAND_DISPLAY` and starts `spawn`, if given, as a client.
  ///
  /// The host registers its seat devices afterwards with
  /// [`Frontend::compositor_mut`].
  pub fn new(
    config: ShingleConfig,
    spawn: Vec<std::ffi::OsString>,
    config_path: Option<std::path::PathBuf>,
  ) -> Result<Self, Box<dyn Error>> {
    let store = if let Some(path) = &config.session.path {
      SessionStore::new(path)
    } else {
      SessionStore::default_location()?
    };
    let discard_output = config.behavior.discard_command_output;
    let restored = if config.session.restore {
      store.load()?
    } else {
      None
    };
    let (compositor, mut session_children, restored_snapshot) =
      if let Some((core, snapshot)) = restored {
        let compositor =
          Compositor::with_core(Config::from(config.clone()), core)?;
        let report = launch_saved(
          &snapshot,
          Some(compositor.socket_name()),
          discard_output,
        );
        for error in &report.failed {
          eprintln!("shingle: session restore launch failed: {error}");
        }
        let children = report
          .launched
          .into_iter()
          .map(|launched| launched.child)
          .collect();
        (compositor, children, Some(snapshot))
      } else {
        (
          Compositor::new(Config::from(config.clone()))?,
          Vec::new(),
          None,
        )
      };
    let autostart = launch_autostart(
      &config.autostart,
      restored_snapshot.as_ref(),
      compositor.socket_name(),
      discard_output,
    );
    for error in &autostart.failed {
      eprintln!("shingle: {error}");
    }
    session_children.extend(autostart.children);
    let mut frontend = Self::with_compositor(
      config,
      compositor,
      store,
      session_children,
      config_path,
    )?;
    println!(
      "WAYLAND_DISPLAY={}",
      frontend.compositor.socket_name().to_string_lossy()
    );
    if let Some((program, args)) = spawn.split_first() {
      let child = std::process::Command::new(program)
        .args(args)
        .env("WAYLAND_DISPLAY", frontend.compositor.socket_name())
        .spawn()?;
      frontend.session_children.push(child);
    }
    Ok(frontend)
  }

  /// Builds the frontend around an existing compositor, which gets the
  /// configured keymap and repeat settings, and saves sessions to `store`.
  /// `session_children` are clients already started for it. This is what
  /// [`Frontend::new`] uses after starting the session, and what tests use
  /// with a compositor on a private socket.
  pub fn with_compositor(
    config: ShingleConfig,
    mut compositor: Compositor,
    store: SessionStore,
    session_children: Vec<Child>,
    config_path: Option<std::path::PathBuf>,
  ) -> Result<Self, Box<dyn Error>> {
    let keymap = config.input.load_keymap()?;
    compositor.set_keymap(&keymap)?;
    compositor.set_repeat(config.input.repeat_rate, config.input.repeat_delay);
    let mut keyboard = Keyboard::new(&keymap)?;
    keyboard.sync_locked(
      compositor.locked_modifiers(),
      compositor.keyboard_layout_index(),
    );
    let compositor_workspace =
      compositor.current_workspace_index().unwrap_or(0);
    Ok(Self {
      compositor,
      store,
      textures: HashMap::new(),
      animations: HashMap::new(),
      open_animations: HashMap::new(),
      closing_animations: HashMap::new(),
      minimize_animations: HashMap::new(),
      animation_enabled: HashSet::new(),
      surface_windows: HashMap::new(),
      animate: config.animate.clone(),
      previous_surfaces: HashMap::new(),
      animation_duration: Duration::from_millis(
        config.tiling.animation_duration,
      ),
      dirty: HashSet::new(),
      lock_client: None,
      session_children,
      keyboard,
      key_repeat: KeyRepeat::new(
        config.input.repeat_rate,
        config.input.repeat_delay,
      ),
      repeat_device: None,
      switcher: None,
      switcher_inactive_alpha: config.fast_switcher.inactive_alpha,
      scale: None,
      scale_exit: None,
      scale_config: config.scale.clone(),
      scale_title_filter: config.scale_title_filter.clone(),
      scale_titles: HashMap::new(),
      scale_icons: HashMap::new(),
      scale_filter: String::new(),
      scale_filter_texture: None,
      expo: None,
      expo_exit: None,
      expo_touch: None,
      expo_config: config.expo.clone(),
      workspace_transition: None,
      vswitch: config.vswitch.clone(),
      last_workspace: compositor_workspace,
      left_handed_mode: config.input.left_handed_mode,
      middle_emulation: config.input.middle_emulation,
      pointer_x: 0.0,
      pointer_y: 0.0,
      keybindings: config.keybindings.clone(),
      commands: config.commands.clone(),
      pointer_bindings: config.pointer.clone(),
      drag: None,
      gestures: extra_gestures::GestureState::new(
        config.extra_gestures.clone(),
      ),
      rotations: HashMap::new(),
      rotation_drag: None,
      wrot_reset_radius: config.effects.wrot_reset_radius,
      minimum_extent: config.tiling.minimum_extent,
      resize: config.resize.clone(),
      discard_command_output: config.behavior.discard_command_output,
      zoom: config.zoom.clone(),
      alpha: config.alpha.clone(),
      inverted: false,
      invert_preserve_hue: config.effects.invert_preserve_hue,
      zoom_factor: 1.0,
      zoom_target: 1.0,
      zoom_animation: None,
      zoom_origin: (0.0, 0.0),
      background_color: config.background_color,
      preview_base_color: config.tiling.preview_base_color,
      preview_border_color: config.tiling.preview_border_color,
      preview_border_width: config.tiling.preview_border_width,
      drop_preview: None,
      drop_slot: None,
      grid_snap: config.grid.enable_snap,
      snap_threshold: config.grid.snap_threshold,
      quarter_snap_threshold: config.grid.quarter_snap_threshold,
      enable_snap_off: config.grid.enable_snap_off,
      snap_off_threshold: config.grid.snap_off_threshold,
      join_views: config.grid.join_views,
      workspace_switch_after: (config.grid.workspace_switch_after >= 0).then(
        || Duration::from_millis(config.grid.workspace_switch_after as u64),
      ),
      workspace_switch_started: None,
      config_watcher: ConfigWatcher::new(
        config_path
          .clone()
          .unwrap_or_else(shingle_config::default_path),
      ),
      config_path,
      auto_reload_config: config.behavior.auto_reload,
      config_reload_delay: Duration::from_millis(
        config.behavior.config_reload_delay,
      ),
      last_config_poll: Instant::now(),
      quit: false,
      redraw_requested: false,
    })
  }

  fn process_events(&mut self, host: &mut dyn Host) -> bool {
    let events = self.compositor.drain_events();
    let changed = !events.is_empty();
    let mut save = false;
    for event in events {
      match event {
        Event::WindowDestroyed { window, .. } => {
          self.switcher_window_destroyed(window);
          self.scale_window_destroyed(window);
          self.scale_titles.remove(&window);
          self.scale_icons.remove(&window);
          self.rotations.remove(&window);
          if self
            .rotation_drag
            .as_ref()
            .is_some_and(|drag| drag.window == window)
          {
            self.rotation_drag = None;
          }
          save = true;
        }
        Event::MinimizedChanged { window, minimized } => {
          self.minimize_window_changed(window, minimized);
          if minimized && !self.scale_config.include_minimized {
            self.scale_window_destroyed(window);
          }
          save = true;
        }
        Event::TitleChanged { window, .. } => {
          self.scale_titles.remove(&window);
          self.apply_scale_filter();
          save = true;
        }
        Event::IconChanged { window } => {
          self.scale_icons.remove(&window);
        }
        Event::AppIdChanged { .. } => {
          self.apply_scale_filter();
          save = true;
        }
        Event::WindowCreated { .. }
        | Event::WindowMoved { .. }
        | Event::TilingChanged { .. }
        | Event::SnappingChanged { .. }
        | Event::FullscreenChanged { .. }
        | Event::MaximizedChanged { .. }
        | Event::StickyChanged { .. }
        | Event::StackingChanged { .. }
        | Event::StackingOrderChanged { .. }
        | Event::ShowDesktopChanged(_) => save = true,
        Event::OutputChanged { .. } => {
          self.workspace_transition = None;
          save = true;
        }
        Event::ToggleScale(all_workspaces) => {
          self.toggle_scale(all_workspaces);
        }
        Event::ToggleExpo => self.toggle_expo(),
        Event::InvertToggle => self.toggle_inversion(),
        Event::WorkspaceChanged { workspace } => {
          let target = workspace.saturating_sub(1) as usize;
          let overview_active = self.scale.is_some()
            || self.scale_exit.is_some()
            || self.expo.is_some()
            || self.expo_exit.is_some();
          self.workspace_transition = if self.last_workspace != target
            && !overview_active
            && self.vswitch.duration_ms > 0
          {
            Some(workspace_transition::WorkspaceTransition::new(
              self.last_workspace,
              target,
            ))
          } else {
            None
          };
          self.last_workspace = target;
          save = true;
        }
        Event::BeginMove { window } => {
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
        }
        Event::BeginResize { window, edges } => {
          self.drag = Some(DragState::Resize {
            window,
            horizontal: (edges & 4 != 0)
              .then_some(Direction::Left)
              .or_else(|| (edges & 8 != 0).then_some(Direction::Right)),
            vertical: (edges & 1 != 0)
              .then_some(Direction::Up)
              .or_else(|| (edges & 2 != 0).then_some(Direction::Down)),
            last_x: self.pointer_x,
            last_y: self.pointer_y,
            aspect: None,
          });
        }
        Event::SurfaceCommitted(snapshot) => {
          self.scale_surface_committed(&snapshot);
          if snapshot.buffer.is_some()
            && self.animate.open_animation != "none"
            && self.animation_matches(&snapshot)
            && !self.textures.contains_key(&snapshot.id)
            && !self.previous_surfaces.contains_key(&snapshot.id)
          {
            self
              .open_animations
              .entry(snapshot.id)
              .or_insert_with(Instant::now);
          }
          self.dirty.insert(snapshot.id);
        }
        Event::SurfaceDestroyed { surface } => {
          let previous = self.previous_surfaces.get(&surface).cloned();
          if self.animate.close_animation != "none"
            && self.animation_enabled.contains(&surface)
            && self.surface_windows.contains_key(&surface)
            && self.textures.contains_key(&surface)
            && let Some((geometry, buffer)) = previous
          {
            self.closing_animations.insert(
              surface,
              ClosingAnimation {
                window: self.surface_windows[&surface],
                started: Instant::now(),
                geometry,
                buffer,
              },
            );
          } else {
            self.textures.remove(&surface);
          }
          self.animations.remove(&surface);
          self.open_animations.remove(&surface);
          self.minimize_animations.remove(&surface);
          self.animation_enabled.remove(&surface);
          self.previous_surfaces.remove(&surface);
          self.surface_windows.remove(&surface);
          self.dirty.remove(&surface);
        }
        Event::PointerWarp { x, y } => {
          self.pointer_x = x;
          self.pointer_y = y;
          let output = self.compositor.output();
          let point = shingle_render::zoom_output_point(
            x - f64::from(output.x),
            y - f64::from(output.y),
            output.width,
            output.height,
            self.zoom_factor,
            self.zoom_origin.0,
            self.zoom_origin.1,
          );
          host.warp_pointer(point.0, point.1);
        }
        Event::StartLockClient => self.start_lock_client(),
        Event::InputDeviceChanged { id, enabled } => {
          if let Some(kind) =
            self.compositor.input_device(id).map(|info| info.kind)
          {
            self.input_device_changed(id, kind, enabled);
            host.input_device_changed(id, enabled);
          }
        }
        Event::DpmsChanged(_)
        | Event::OpacityChanged { .. }
        | Event::Idle
        | Event::Resume
        | Event::FocusChanged { .. }
        | Event::GeometryChanged { .. }
        | Event::LockStateChanged(_) => {}
      }
    }
    if save {
      self.save();
    }
    changed
  }

  /// Drops frontend state held by a device that IPC just disabled, so no
  /// switcher, drag or gesture stays active with nothing left to end it. The
  /// compositor has already released the device's keys, buttons and touches.
  fn input_device_changed(
    &mut self,
    id: InputDeviceId,
    device: InputDeviceKind,
    enabled: bool,
  ) {
    match (device, enabled) {
      (InputDeviceKind::Keyboard, false) => {
        // The switcher's modifiers can no longer be released, so finish it
        // as if they had been.
        if self.switcher.is_some() {
          self.finish_switcher();
        }
      }
      (InputDeviceKind::Keyboard, true) => {
        // Keys kept being tracked while disabled; resync the modifiers.
        self.update_modifiers(id);
      }
      (InputDeviceKind::Pointer, false) => {
        self.rotation_drag = None;
        if let Some(state) = &mut self.expo {
          state.cancel_pointer();
        }
        self.abandon_drag();
      }
      (InputDeviceKind::Touch, false) => {
        self.gestures.cancel();
        if self.expo_touch.take().is_some()
          && let Some(state) = &mut self.expo
        {
          state.cancel_pointer();
        }
        // A drag may have been started by a touch gesture.
        self.abandon_drag();
      }
      (InputDeviceKind::Pointer | InputDeviceKind::Touch, true) => {}
    }
  }

  /// Sends the shared keyboard's modifier masks to the compositor.
  fn update_modifiers(&mut self, device: InputDeviceId) {
    self
      .compositor
      .keyboard_modifiers(device, self.keyboard.modifiers());
  }

  /// Ends the switcher once its modifiers are released and sends the new
  /// modifier masks, after a key or focus change altered them.
  fn keyboard_modifiers_changed(&mut self, device: InputDeviceId) {
    if self.switcher.as_ref().is_some_and(|state| {
      !modifiers_held(state.modifiers, self.config_modifiers())
    }) {
      self.finish_switcher();
    }
    self.update_modifiers(device);
  }

  fn config_modifiers(&self) -> ConfigModifiers {
    self.keyboard.binding_modifiers()
  }

  fn map_zoom_input(&self, x: f64, y: f64) -> (f64, f64) {
    let output = self.compositor.output();
    shingle_render::zoom_input_point(
      x,
      y,
      output.width,
      output.height,
      self.zoom_factor,
      self.zoom_origin.0,
      self.zoom_origin.1,
    )
  }

  fn pointer_button_action(&mut self, button: &str, pressed: bool) -> bool {
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

  fn handle_gesture_action(&mut self, action: extra_gestures::GestureAction) {
    match action {
      extra_gestures::GestureAction::BeginMove { x, y } => {
        if let Some(window) = self.compositor.window_at(x, y) {
          self.pointer_x = x;
          self.pointer_y = y;
          self.compositor.begin_move_window(window);
        }
      }
      extra_gestures::GestureAction::Close { x, y } => {
        if let Some(window) = self.compositor.window_at(x, y) {
          self.compositor.close_window(window);
          self.save();
        }
      }
    }
  }

  fn process_extra_gestures(&mut self) {
    if let Some(action) = self.gestures.poll(Instant::now()) {
      self.handle_gesture_action(action);
    }
  }

  fn run_matching_command(
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

  fn run_matching_symbol_command(
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

  fn run_matching_command_with(
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
            self.compositor.socket_name(),
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
        self.compositor.socket_name(),
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

  fn toggle_inversion(&mut self) {
    self.inverted = !self.inverted;
    self
      .compositor
      .notify_plugin_activation("invert", self.inverted);
    self.request_redraw();
  }

  /// Opens Expo or commits the currently selected workspace.
  fn toggle_expo(&mut self) {
    if self.expo.is_some() {
      self.finish_expo();
      return;
    }
    self.expo_exit = None;
    self.workspace_transition = None;
    self.cancel_scale();
    self.scale_exit = None;
    if self.switcher.is_some() {
      self.finish_switcher();
    }
    self.drag = None;
    self.drop_preview = None;
    self.drop_slot = None;
    let selected = self.compositor.current_workspace_index().unwrap_or(0);
    let (width, height) = self.compositor.workspace_grid_dimensions();
    self.expo = Some(expo::ExpoState::new(
      selected,
      self.compositor.output(),
      width,
      height,
      self.expo_config.offset,
    ));
    self.update_expo_layout();
    self.compositor.notify_plugin_activation("expo", true);
    self.request_redraw();
  }

  /// Immediately removes both interactive and exiting Expo render states.
  fn clear_expo(&mut self) {
    if self.expo.is_some() {
      self.compositor.notify_plugin_activation("expo", false);
    }
    self.expo = None;
    self.expo_exit = None;
    self.expo_touch = None;
  }

  /// Zooms back to the original workspace without changing the saved session.
  fn cancel_expo(&mut self) {
    if let Some(mut state) = self.expo.take() {
      let current = self.compositor.current_workspace_index().unwrap_or(0);
      state.selected = current;
      state.previous_selected = current;
      self.compositor.notify_plugin_activation("expo", false);
      self.begin_expo_exit(state);
    }
    self.request_redraw();
  }

  /// Starts the renderer-only return from grid cells to a workspace wall.
  fn begin_expo_exit(&mut self, mut state: expo::ExpoState) {
    state.cancel_pointer();
    self.expo_touch = None;
    let now = Instant::now();
    let duration = Duration::from_millis(self.expo_config.duration_ms);
    state.cell_origin = state.cells_at(now, duration);
    state.geometry_origin = state.geometry_at(now, duration);
    let (width, height) = self.compositor.workspace_grid_dimensions();
    state.cells = expo::expo_wall_cells(
      self.compositor.output(),
      width,
      height,
      state.selected,
    );
    state.geometry = self.original_expo_geometry(state.selected);
    state.animation_started = now;
    state.previous_selected = state.selected;
    state.selection_started = now;
    if duration.is_zero() {
      self.expo_exit = None;
    } else {
      self.expo_exit = Some(state);
    }
  }

  /// Commits the selected workspace and, after a real drag, its window move.
  fn finish_expo(&mut self) {
    let Some(mut state) = self.expo.take() else {
      return;
    };
    self.compositor.notify_plugin_activation("expo", false);
    let dragged = state.pointer_up();
    let mut changed = false;
    if let Some(window) = dragged {
      let source =
        self.compositor.core().window_info(window).ok().map(|info| {
          let (width, _) = self.compositor.workspace_grid_dimensions();
          info.location.workspace.y as usize * width as usize
            + info.location.workspace.x as usize
        });
      if source.is_some_and(|source| source != state.selected) {
        match self
          .compositor
          .move_window_to_workspace(window, state.selected)
        {
          Ok(()) => changed = true,
          Err(error) => {
            eprintln!("shingle: Expo window move failed: {error}")
          }
        }
      }
    }
    if !self
      .compositor
      .current_workspace_index()
      .is_ok_and(|workspace| workspace == state.selected)
    {
      match self.compositor.switch_workspace(state.selected) {
        Ok(()) => changed = true,
        Err(error) => {
          state.selected =
            self.compositor.current_workspace_index().unwrap_or(0);
          eprintln!("shingle: Expo workspace switch failed: {error}");
        }
      }
    }
    if changed {
      self.save();
    }
    self.begin_expo_exit(state);
    self.request_redraw();
  }

  /// Returns normal window rectangles on the wall around `selected`.
  /// Remote workspaces are offscreen at the end of an exit transition.
  fn original_expo_geometry(
    &self,
    selected: usize,
  ) -> HashMap<TileWindowId, RenderRect> {
    let output = self.compositor.output();
    let (grid_width, grid_height) = self.compositor.workspace_grid_dimensions();
    let cells =
      expo::expo_wall_cells(output, grid_width, grid_height, selected);
    self
      .compositor
      .scale_surfaces(true, false)
      .into_iter()
      .filter_map(|snapshot| {
        let window = snapshot.window?;
        let info = self.compositor.core().window_info(window).ok()?;
        let workspace = info.location.workspace.y as usize
          * grid_width as usize
          + info.location.workspace.x as usize;
        let cell = cells.get(workspace)?;
        let original =
          self.window_geometry(window).unwrap_or(snapshot.geometry);
        let x = i64::from(original.x) + i64::from(cell.x) - i64::from(output.x);
        let y = i64::from(original.y) + i64::from(cell.y) - i64::from(output.y);
        Some((
          window,
          RenderRect {
            x: i32::try_from(x).unwrap_or(if x < 0 {
              i32::MIN
            } else {
              i32::MAX
            }),
            y: i32::try_from(y).unwrap_or(if y < 0 {
              i32::MIN
            } else {
              i32::MAX
            }),
            width: original.width,
            height: original.height,
          },
        ))
      })
      .collect()
  }

  /// Rebuilds live targets while preserving the current transition position.
  /// Unchanged targets do not restart the animation on every frame.
  fn update_expo_layout(&mut self) {
    if self.expo.is_none() {
      return;
    }
    let now = Instant::now();
    let duration = Duration::from_millis(self.expo_config.duration_ms);
    let previous = self.expo.as_ref().map(|state| {
      (
        state.cells_at(now, duration),
        state.geometry_at(now, duration),
      )
    });
    let (grid_width, grid_height) = self.compositor.workspace_grid_dimensions();
    let cells = expo::expo_cells(
      self.compositor.output(),
      grid_width,
      grid_height,
      self.expo_config.offset,
    );
    let current = self.compositor.current_workspace_index().unwrap_or(0);
    let original = self.original_expo_geometry(current);
    let geometry = original
      .iter()
      .filter_map(|(window, original)| {
        let info = self.compositor.core().window_info(*window).ok()?;
        let workspace = info.location.workspace.y as usize
          * grid_width as usize
          + info.location.workspace.x as usize;
        let cell = *cells.get(workspace)?;
        Some((*window, fit_scale_tile(cell, *original, false)))
      })
      .collect::<HashMap<_, _>>();
    let windows = self
      .compositor
      .scale_surfaces(true, false)
      .into_iter()
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>();
    if self.expo.as_ref().is_some_and(|state| {
      state.cells == cells
        && state.windows == windows
        && state.geometry == geometry
    }) {
      return;
    }
    if let Some(state) = &mut self.expo {
      state.cell_origin = previous
        .as_ref()
        .map(|previous| previous.0.clone())
        .unwrap_or_else(|| state.cell_origin.clone());
      state.geometry_origin = geometry
        .keys()
        .filter_map(|window| {
          previous
            .as_ref()
            .and_then(|previous| previous.1.get(window))
            .or_else(|| original.get(window))
            .map(|geometry| (*window, *geometry))
        })
        .collect();
      state.cells = cells;
      state.windows = windows;
      state.geometry = geometry;
      state.animation_started = now;
    }
  }

  fn move_expo_selection(&mut self, direction: Direction) {
    let (width, height) = self.compositor.workspace_grid_dimensions();
    if let Some(state) = &mut self.expo {
      state.move_selection(direction, width, height, Instant::now());
    }
    self.request_redraw();
  }

  /// Updates the hovered cell and any pending or active thumbnail drag.
  fn move_expo_pointer(&mut self, x: f64, y: f64) {
    let duration = Duration::from_millis(self.expo_config.duration_ms);
    let changed = if let Some(state) = &mut self.expo {
      let previous = state.selected;
      state.pointer_move(x, y, Instant::now(), duration);
      state.selected != previous || state.pointer_pressed()
    } else {
      false
    };
    if changed {
      self.request_redraw();
    }
  }

  /// Left release commits either a cell click or a completed window drag.
  fn expo_pointer_input(&mut self, button: u32, pressed: bool) -> bool {
    if self.expo.is_none() {
      return false;
    }
    if button == BTN_LEFT {
      if pressed {
        let duration = Duration::from_millis(self.expo_config.duration_ms);
        if let Some(state) = &mut self.expo {
          state.pointer_down(
            self.pointer_x,
            self.pointer_y,
            Instant::now(),
            duration,
          );
        }
      } else {
        self.finish_expo();
      }
    }
    true
  }

  /// Captures all keyboard input while Expo is active.
  fn expo_key(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
    pressed: bool,
    locked: bool,
  ) -> bool {
    if self.expo.is_none() {
      return false;
    }
    if locked {
      self.clear_expo();
      return false;
    }
    if !pressed {
      return true;
    }
    let direct = self
      .expo_config
      .workspace_bindings
      .iter()
      .find(|entry| entry.binding.matches(modifiers, key))
      .map(|entry| entry.workspace - 1);
    if let Some(selected) = direct {
      if let Some(state) = &mut self.expo {
        state.cancel_pointer();
        state.set_selected(selected, Instant::now());
      }
      self.finish_expo();
      return true;
    }
    if self
      .expo
      .as_ref()
      .is_some_and(|state| state.pointer_pressed())
    {
      return true;
    }
    if key == "escape" {
      self.cancel_expo();
    } else if key == "enter" {
      self.finish_expo();
    } else if self.keybindings.expo_toggle.matches(modifiers, key) {
      self.toggle_expo();
    } else if self.expo_config.keyboard_interaction {
      let direction = match key {
        "left" | "arrowleft" | "kp4" | "h" => Some(Direction::Left),
        "right" | "arrowright" | "kp6" | "l" => Some(Direction::Right),
        "up" | "arrowup" | "kp8" | "k" => Some(Direction::Up),
        "down" | "arrowdown" | "kp2" | "j" => Some(Direction::Down),
        _ => None,
      };
      if let Some(direction) = direction {
        self.move_expo_selection(direction);
      }
    }
    true
  }

  /// Opens Scale, changes its workspace scope, or commits an active session.
  ///
  /// Changing scope preserves both the selected window and the focus that
  /// should be restored by a later cancellation.
  fn toggle_scale(&mut self, all_workspaces: bool) {
    self.clear_expo();
    self.workspace_transition = None;
    self.scale_exit = None;
    if let Some(state) = &self.scale {
      let no_remote_windows = self
        .compositor
        .scale_surfaces(true, self.scale_config.include_minimized)
        .len()
        == self
          .compositor
          .scale_surfaces(false, self.scale_config.include_minimized)
          .len();
      if state.all_workspaces == all_workspaces || no_remote_windows {
        self.finish_scale();
        return;
      }
    }
    let was_active = self.scale.is_some();
    let initial_workspace = self
      .scale
      .as_ref()
      .map(|state| state.initial_workspace)
      .unwrap_or_else(|| {
        self.compositor.current_workspace_index().unwrap_or(0)
      });
    let initial_focus = self
      .scale
      .as_ref()
      .and_then(|state| state.initial_focus)
      .or_else(|| self.compositor.focused_window());
    let selected_window = self
      .scale
      .as_ref()
      .and_then(|state| state.windows.get(state.selected).copied());
    if self.switcher.is_some() {
      self.finish_switcher();
    }
    self.compositor.set_show_desktop(false);
    self.drag = None;
    self.drop_preview = None;
    self.drop_slot = None;
    let windows = self
      .compositor
      .scale_surfaces(all_workspaces, self.scale_config.include_minimized)
      .into_iter()
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>();
    if windows.is_empty() {
      self.cancel_scale();
      return;
    }
    let selected = selected_window
      .or_else(|| self.compositor.focused_window())
      .and_then(|focused| windows.iter().position(|window| *window == focused))
      .unwrap_or_else(|| {
        windows
          .iter()
          .position(|window| {
            self
              .compositor
              .core()
              .window_info(*window)
              .is_ok_and(|info| !info.state.minimized)
          })
          .unwrap_or(0)
      });
    self.scale = Some(ScaleState {
      windows,
      selected,
      cells: Vec::new(),
      geometry: HashMap::new(),
      animation_origin: HashMap::new(),
      opacity_origin: HashMap::new(),
      animation_started: Instant::now(),
      all_workspaces,
      initial_focus,
      initial_workspace,
    });
    self.update_scale_layout();
    if !was_active {
      self.compositor.notify_plugin_activation("scale", true);
    }
    self.request_redraw();
  }

  /// Returns normal window rectangles, including retained minimized buffers.
  fn original_scale_geometry(
    &self,
    windows: &[TileWindowId],
  ) -> HashMap<TileWindowId, RenderRect> {
    let snapshots = self
      .compositor
      .scale_surfaces(true, true)
      .into_iter()
      .filter_map(|snapshot| snapshot.window.map(|window| (window, snapshot)))
      .collect::<HashMap<_, _>>();
    windows
      .iter()
      .filter_map(|window| {
        let geometry = self.window_geometry(*window).or_else(|| {
          let snapshot = snapshots.get(window)?;
          let buffer = snapshot.buffer.as_ref()?;
          Some(shingle_core::Rect::new(
            snapshot.geometry.x,
            snapshot.geometry.y,
            buffer.width,
            buffer.height,
          ))
        })?;
        Some((
          *window,
          RenderRect {
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
          },
        ))
      })
      .collect()
  }

  /// Recomputes thumbnail targets without reconfiguring client windows.
  ///
  /// Minimized tiled leaves have no layout entry. Their last buffer size is
  /// therefore used as the aspect-ratio fallback until they are restored.
  fn update_scale_layout(&mut self) {
    let now = Instant::now();
    let previous = self.scale.as_ref().map(|state| {
      scale_geometry_at(
        state,
        now,
        Duration::from_millis(self.scale_config.duration_ms),
      )
    });
    let Some(state) = &self.scale else {
      return;
    };
    let windows = state.windows.clone();
    let original = self.original_scale_geometry(&windows);
    let cells = scale_cells(
      self.compositor.output(),
      windows.len(),
      self.scale_config.spacing,
      self.scale_config.outer_margin,
    );
    let geometry = windows
      .iter()
      .zip(&cells)
      .filter_map(|(window, cell)| {
        original.get(window).map(|original| {
          let original = RenderRect {
            x: original.x,
            y: original.y,
            width: original.width,
            height: original.height,
          };
          (
            *window,
            fit_scale_tile(*cell, original, self.scale_config.allow_zoom),
          )
        })
      })
      .collect::<HashMap<_, _>>();
    let animation_origin = windows
      .iter()
      .filter_map(|window| {
        previous
          .as_ref()
          .and_then(|previous| previous.get(window).copied())
          .or_else(|| {
            original.get(window).map(|original| RenderRect {
              x: original.x,
              y: original.y,
              width: original.width,
              height: original.height,
            })
          })
          .map(|geometry| (*window, geometry))
      })
      .collect();
    if let Some(state) = &mut self.scale {
      state.cells = cells;
      state.geometry = geometry;
      state.animation_origin = animation_origin;
      state.animation_started = now;
    }
  }

  /// Starts a noninteractive return from overview geometry to normal windows.
  fn begin_scale_exit(&mut self, mut state: ScaleState) {
    let now = Instant::now();
    let duration = Duration::from_millis(self.scale_config.duration_ms);
    state.animation_origin = scale_geometry_at(&state, now, duration);
    let progress = animation_progress(state.animation_started, now, duration);
    state.opacity_origin = state
      .windows
      .iter()
      .map(|window| {
        let selected = state.windows.get(state.selected) == Some(window);
        let minimized = self
          .compositor
          .core()
          .window_info(*window)
          .is_ok_and(|info| info.state.minimized);
        let inactive = if minimized {
          self.scale_config.minimized_alpha
        } else {
          self.scale_config.inactive_alpha
        };
        let opacity = if selected {
          1.0
        } else {
          1.0 - (1.0 - inactive) * progress
        };
        (*window, opacity)
      })
      .collect();
    state.geometry = self.original_scale_geometry(&state.windows);
    state.animation_started = now;
    self.scale_exit = Some(state);
    self.scale_titles.clear();
    self.scale_icons.clear();
    self.scale_filter.clear();
    self.scale_filter_texture = None;
    self.request_redraw();
  }

  /// Commits the selected window and saves the resulting normal session.
  ///
  /// Remote workspaces are activated before focus. A minimized selection is
  /// restored first so that the tiling tree can accept focus again.
  fn finish_scale(&mut self) {
    let Some(mut state) = self.scale.take() else {
      return;
    };
    self.compositor.notify_plugin_activation("scale", false);
    if let Some(window) = state.windows.get(state.selected).copied() {
      if let Ok(info) = self.compositor.core().window_info(window) {
        if info.state.minimized
          && let Err(error) = self.compositor.set_minimized(window, false)
        {
          eprintln!("shingle: scale restore failed: {error}");
        }
        let (width, _) = self.compositor.workspace_grid_dimensions();
        let workspace = info.location.workspace.y as usize * width as usize
          + info.location.workspace.x as usize;
        if !self
          .compositor
          .current_workspace_index()
          .is_ok_and(|current| current == workspace)
        {
          match self.compositor.switch_workspace(workspace) {
            Ok(()) => state.all_workspaces = false,
            Err(error) => {
              eprintln!("shingle: scale workspace switch failed: {error}")
            }
          }
        }
      }
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: scale focus failed: {error}");
      }
    }
    self.begin_scale_exit(state);
    self.save();
    self.request_redraw();
  }

  /// Discards thumbnail state and restores the pre-Scale focus when possible.
  fn cancel_scale(&mut self) {
    let Some(state) = self.scale.take() else {
      return;
    };
    self.compositor.notify_plugin_activation("scale", false);
    if !self
      .compositor
      .current_workspace_index()
      .is_ok_and(|workspace| workspace == state.initial_workspace)
      && let Err(error) =
        self.compositor.switch_workspace(state.initial_workspace)
    {
      eprintln!("shingle: scale workspace restore failed: {error}");
    }
    if let Some(window) = state.initial_focus
      && self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| !info.state.minimized)
    {
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: scale cancellation focus failed: {error}");
      }
    }
    self.begin_scale_exit(state);
    self.save();
    self.request_redraw();
  }

  fn select_scale_relative(&mut self, reverse: bool) {
    let Some(state) = &mut self.scale else {
      return;
    };
    if state.windows.is_empty() {
      return;
    }
    state.selected = if reverse {
      (state.selected + state.windows.len() - 1) % state.windows.len()
    } else {
      (state.selected + 1) % state.windows.len()
    };
    self.request_redraw();
  }

  fn move_scale_selection(&mut self, direction: Direction) {
    let Some(state) = &mut self.scale else {
      return;
    };
    if state.windows.is_empty() {
      return;
    }
    let columns = scale_columns(state.cells.len());
    let row = state.selected / columns;
    let column = state.selected % columns;
    let selected = match direction {
      Direction::Left if column > 0 => Some(state.selected - 1),
      Direction::Right
        if column + 1 < columns && state.selected + 1 < state.windows.len() =>
      {
        Some(state.selected + 1)
      }
      Direction::Up if row > 0 => Some(state.selected - columns),
      Direction::Down if state.selected + columns < state.windows.len() => {
        Some(state.selected + columns)
      }
      _ => None,
    };
    if let Some(selected) = selected {
      state.selected = selected;
      self.request_redraw();
    }
  }

  fn select_scale_at(&mut self, x: f64, y: f64) -> bool {
    let Some(state) = &mut self.scale else {
      return false;
    };
    let Some(selected) = state.windows.iter().position(|window| {
      state
        .geometry
        .get(window)
        .is_some_and(|rect| rect_contains(*rect, x, y))
    }) else {
      return false;
    };
    let changed = state.selected != selected;
    state.selected = selected;
    if changed {
      self.request_redraw();
    }
    true
  }

  fn scale_pointer_input(&mut self, button: u32, pressed: bool) -> bool {
    if self.scale.is_none() {
      return false;
    }
    if !pressed || !self.select_scale_at(self.pointer_x, self.pointer_y) {
      return true;
    }
    match button {
      BTN_LEFT => self.finish_scale(),
      BTN_MIDDLE if self.scale_config.middle_click_close => {
        let window = self
          .scale
          .as_ref()
          .and_then(|state| state.windows.get(state.selected).copied());
        if let Some(window) = window {
          self.compositor.close_window(window);
        }
      }
      _ => {}
    }
    true
  }

  /// Rebuilds the visible overview set after text or window metadata changes.
  fn apply_scale_filter(&mut self) {
    let Some(state) = &self.scale else {
      return;
    };
    let selected = state.windows.get(state.selected).copied();
    let windows = self
      .compositor
      .scale_surfaces(state.all_workspaces, self.scale_config.include_minimized)
      .into_iter()
      .filter(|snapshot| {
        scale_filter_matches(
          snapshot,
          &self.scale_filter,
          self.scale_title_filter.case_sensitive,
        )
      })
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>();
    let selected = selected
      .and_then(|window| {
        windows.iter().position(|candidate| *candidate == window)
      })
      .unwrap_or_else(|| {
        windows
          .iter()
          .position(|window| {
            self
              .compositor
              .core()
              .window_info(*window)
              .is_ok_and(|info| !info.state.minimized)
          })
          .unwrap_or(0)
      });
    if let Some(state) = &mut self.scale {
      state.windows = windows;
      state.selected = selected;
    }
    self.update_scale_layout();
    self.request_redraw();
  }

  /// Adds a newly eligible mapped window or honors `close_on_new_view`.
  ///
  /// Repeated commits from existing thumbnails and windows excluded by the
  /// current workspace/minimized filters do not change the overview.
  fn scale_surface_committed(&mut self, snapshot: &SurfaceSnapshot) {
    if snapshot.role != SurfaceRole::Toplevel || snapshot.buffer.is_none() {
      return;
    }
    let Some(window) = snapshot.window else {
      return;
    };
    let Some(state) = &self.scale else {
      return;
    };
    if state.windows.contains(&window) {
      return;
    }
    let all_workspaces = state.all_workspaces;
    if !self
      .compositor
      .scale_surfaces(all_workspaces, self.scale_config.include_minimized)
      .iter()
      .any(|snapshot| snapshot.window == Some(window))
    {
      return;
    }
    if self.scale_config.close_on_new_view {
      self.finish_scale();
      return;
    }
    if let Some(state) = &mut self.scale {
      state.windows.push(window);
    }
    self.apply_scale_filter();
    self.request_redraw();
  }

  fn scale_window_destroyed(&mut self, window: TileWindowId) {
    let Some(state) = &mut self.scale else {
      return;
    };
    let Some(removed) = state
      .windows
      .iter()
      .position(|candidate| *candidate == window)
    else {
      return;
    };
    state.windows.remove(removed);
    if removed < state.selected {
      state.selected -= 1;
    }
    if state.windows.is_empty() {
      self.cancel_scale();
      return;
    }
    state.selected = state.selected.min(state.windows.len() - 1);
    self.update_scale_layout();
    self.request_redraw();
  }

  /// Applies text input and UTF-8-safe backspace to the active title filter.
  fn scale_filter_input(
    &mut self,
    key: &str,
    text: Option<&str>,
    pressed: bool,
  ) {
    if self.scale.is_none() || !pressed {
      return;
    }
    let changed = if key == "backspace" {
      self.scale_filter.pop().is_some()
    } else if let Some(text) = text
      && !text.chars().any(char::is_control)
    {
      let previous = self.scale_filter.len();
      self.scale_filter.push_str(text);
      self.scale_filter.len() != previous
    } else {
      false
    };
    if changed {
      self.apply_scale_filter();
    }
  }

  /// Handles workspace navigation without moving windows during Scale.
  fn scale_workspace_input(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
  ) -> bool {
    if self.scale.is_none() {
      return false;
    }
    let vertical = self.keybindings.workspace_up.matches(modifiers, key)
      || self.keybindings.workspace_down.matches(modifiers, key);
    let reverse = self.keybindings.workspace_previous.matches(modifiers, key)
      || self.keybindings.workspace_up.matches(modifiers, key);
    let relative = vertical
      || reverse
      || self.keybindings.workspace_next.matches(modifiers, key);
    let last = self.keybindings.workspace_last.matches(modifiers, key);
    let direct = self
      .keybindings
      .workspace_switch
      .iter()
      .position(|binding| binding.matches(modifiers, key));
    if !relative && !last && direct.is_none() {
      return false;
    }
    let result = if let Some(workspace) = direct {
      self.compositor.switch_workspace(workspace)
    } else if last {
      self.compositor.switch_to_last_workspace().map(|_| ())
    } else if vertical {
      self
        .compositor
        .switch_workspace_vertical(reverse)
        .map(|_| ())
    } else {
      self
        .compositor
        .switch_workspace_relative(reverse)
        .map(|_| ())
    };
    if let Err(error) = result {
      eprintln!("shingle: Scale workspace navigation failed: {error}");
    } else {
      self.apply_scale_filter();
      self.save();
    }
    true
  }

  fn scale_key(
    &mut self,
    modifiers: ConfigModifiers,
    key: &str,
    pressed: bool,
    locked: bool,
  ) -> bool {
    if self.scale.is_none() {
      return false;
    }
    if locked {
      self.cancel_scale();
      return false;
    }
    if pressed {
      if key == "escape" {
        self.cancel_scale();
      } else if key == "enter" {
        self.finish_scale();
      } else if self.keybindings.scale_toggle.matches(modifiers, key) {
        self.toggle_scale(false);
      } else if self.keybindings.scale_toggle_all.matches(modifiers, key) {
        self.toggle_scale(true);
      } else if self.scale_workspace_input(modifiers, key) {
      } else if key == "tab" {
        self.select_scale_relative(modifiers.shift);
      } else {
        let direction = match key {
          "left" | "arrowleft" => Some(Direction::Left),
          "right" | "arrowright" => Some(Direction::Right),
          "up" | "arrowup" => Some(Direction::Up),
          "down" | "arrowdown" => Some(Direction::Down),
          _ => None,
        };
        if let Some(direction) = direction {
          self.move_scale_selection(direction);
        }
      }
    }
    true
  }

  /// Feeds one key from seat keyboard `device` to the shared keyboard
  /// and, if the device is enabled, dispatches it. Keys are tracked while
  /// the device is disabled, so modifiers are right again the moment it is
  /// re-enabled.
  fn key_event(
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
  fn reset_keyboard(&mut self, device: InputDeviceId) {
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
  fn process_key_repeat(&mut self, host: &mut dyn Host) {
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
  fn keyboard_input(
    &mut self,
    _host: &mut dyn Host,
    device: InputDeviceId,
    event: &Key,
  ) {
    let pressed = event.state != KeyState::Released;
    let repeat = event.state == KeyState::Repeated;
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

  fn animation_matches(&self, snapshot: &SurfaceSnapshot) -> bool {
    let view_type = match snapshot.role {
      SurfaceRole::Toplevel => "toplevel",
      SurfaceRole::Popup => "popup",
      SurfaceRole::LayerShell => "overlay",
      SurfaceRole::Cursor => "cursor",
      _ => "other",
    };
    let mut view = ViewProperties {
      app_id: snapshot.app_id.as_deref().unwrap_or(""),
      title: snapshot.title.as_deref().unwrap_or(""),
      view_type,
      activated: self.compositor.focused_window() == snapshot.window,
      focusable: snapshot.role == SurfaceRole::Toplevel,
      mapped: snapshot.buffer.is_some(),
      ..ViewProperties::default()
    };
    if let Some(window) = snapshot.window
      && let Ok(info) = self.compositor.core().window_info(window)
    {
      view.fullscreen = info.state.fullscreen;
      view.minimized = info.state.minimized;
      view.maximized = info.state.maximized;
      view.floating = info.state.floating.is_some() && !info.state.maximized;
      view.tiled_left = !view.floating;
      view.tiled_right = !view.floating;
      view.tiled_top = !view.floating;
      view.tiled_bottom = !view.floating;
    }
    self.animate.enabled_for.matches_view(&view)
  }

  fn minimize_window_changed(&mut self, window: TileWindowId, minimized: bool) {
    if !minimized {
      self
        .minimize_animations
        .retain(|_, animation| animation.window != window);
      return;
    }
    if self.scale_config.include_minimized
      && self
        .scale
        .as_ref()
        .is_some_and(|state| state.windows.contains(&window))
    {
      self
        .minimize_animations
        .retain(|_, animation| animation.window != window);
      return;
    }
    if self.animate.minimize_animation == "none" {
      return;
    }
    let Some(surface) =
      self
        .surface_windows
        .iter()
        .find_map(|(surface, candidate)| {
          (*candidate == window).then_some(*surface)
        })
    else {
      return;
    };
    if !self.animation_enabled.contains(&surface)
      || self.closing_animations.contains_key(&surface)
      || !self.textures.contains_key(&surface)
    {
      return;
    }
    let Some((geometry, buffer)) =
      self.previous_surfaces.get(&surface).cloned()
    else {
      return;
    };
    self.minimize_animations.insert(
      surface,
      ClosingAnimation {
        window,
        started: Instant::now(),
        geometry,
        buffer,
      },
    );
  }

  fn expire_closing_animations(&mut self, now: Instant) {
    let completed = self
      .closing_animations
      .iter()
      .filter(|(_, animation)| {
        animation_progress(
          animation.started,
          now,
          Duration::from_millis(self.animate.duration_ms),
        ) >= 1.0
      })
      .map(|(surface, _)| *surface)
      .collect::<Vec<_>>();
    for surface in completed {
      self.closing_animations.remove(&surface);
      self.textures.remove(&surface);
    }
  }

  fn expire_minimize_animations(&mut self, now: Instant) {
    self.minimize_animations.retain(|_, animation| {
      animation_progress(
        animation.started,
        now,
        Duration::from_millis(self.animate.duration_ms),
      ) < 1.0
    });
  }

  /// Releases the retained exit snapshots after the final transition frame.
  fn expire_scale_exit(&mut self, now: Instant) {
    if self.scale_exit.as_ref().is_some_and(|state| {
      animation_progress(
        state.animation_started,
        now,
        Duration::from_millis(self.scale_config.duration_ms),
      ) >= 1.0
    }) {
      self.scale_exit = None;
    }
  }

  fn append_closing_surfaces<'a>(
    animations: &'a HashMap<SurfaceId, ClosingAnimation>,
    textures: &'a HashMap<SurfaceId, GpuTexture>,
    config: &shingle_config::AnimateConfig,
    effect: &str,
    tiles: &mut Vec<RenderTile<'a>>,
  ) {
    let mut surfaces = animations.keys().copied().collect::<Vec<_>>();
    surfaces.sort_unstable();
    let now = Instant::now();
    for surface in surfaces {
      let animation = &animations[&surface];
      let Some(texture) = textures.get(&surface) else {
        continue;
      };
      let mut tile = RenderTile::textured(animation.geometry, texture);
      apply_buffer(&mut tile, &animation.buffer);
      let progress = animation_progress(
        animation.started,
        now,
        Duration::from_millis(config.duration_ms),
      );
      apply_opening_animation(&mut tile, effect, 1.0 - progress);
      tiles.push(tile);
    }
  }

  fn append_drop_preview<'a>(&self, tiles: &mut Vec<RenderTile<'a>>) {
    let Some(rect) = self.drop_preview else {
      return;
    };
    tiles.extend(preview_tiles(
      rect,
      self.preview_base_color.map(|component| component as f32),
      self.preview_border_color.map(|component| component as f32),
      self.preview_border_width,
    ));
  }

  /// Appends client-supplied icons above active Scale thumbnails.
  fn append_scale_icons<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut HashMap<TileWindowId, GpuTexture>,
    state: &ScaleState,
    duration: Duration,
    icons: &HashMap<TileWindowId, shingle::ToplevelIcon>,
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    let geometry = scale_geometry_at(state, Instant::now(), duration);
    for window in &state.windows {
      if cache.contains_key(window) {
        continue;
      }
      let Some(icon) = icons.get(window) else {
        continue;
      };
      let supplied = icon.buffers.iter().min_by_key(|buffer| {
        let scale = buffer.scale.max(1) as u32;
        (buffer.width / scale).abs_diff(32)
      });
      let (width, rgba) = if let Some(icon) = supplied {
        (icon.width, icon.rgba.to_vec())
      } else {
        let Some(name) = icon.name.as_deref() else {
          continue;
        };
        let Some(icon) = icon_theme::load(name, 64) else {
          continue;
        };
        (icon.width, icon.rgba)
      };
      let texture = renderer.create_texture(width, width, &rgba)?;
      cache.insert(*window, texture);
    }
    for window in &state.windows {
      let (Some(texture), Some(geometry)) =
        (cache.get(window), geometry.get(window))
      else {
        continue;
      };
      tiles.push(RenderTile::textured(
        RenderRect {
          x: geometry.x.saturating_add(8),
          y: geometry.y.saturating_add(8),
          width: 32,
          height: 32,
        },
        texture,
      ));
    }
    Ok(())
  }

  /// Appends cached title textures above active Scale thumbnails.
  fn append_scale_titles<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut HashMap<TileWindowId, ScaleTitle>,
    state: &ScaleState,
    config: &shingle_config::ScaleConfig,
    titles: &HashMap<TileWindowId, String>,
    pointer: (f64, f64),
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    if config.title_overlay == "never" {
      return Ok(());
    }
    let geometry = scale_geometry_at(
      state,
      Instant::now(),
      Duration::from_millis(config.duration_ms),
    );
    for window in &state.windows {
      let (Some(title), Some(target), Some(current)) = (
        titles.get(window),
        state.geometry.get(window),
        geometry.get(window),
      ) else {
        continue;
      };
      if config.title_overlay == "mouse"
        && !rect_contains(*current, pointer.0, pointer.1)
      {
        continue;
      }
      let needs_texture = cache.get(window).is_none_or(|cached| {
        cached.title != *title || cached.target_width != target.width
      });
      if !needs_texture {
        continue;
      }
      let Some(raster) = scale_title::rasterize_title(
        title,
        config.title_font_size,
        target.width,
        config.title_bg_color.map(|value| value as f32),
        config.title_text_color.map(|value| value as f32),
      ) else {
        continue;
      };
      let texture =
        renderer.create_texture(raster.width, raster.height, &raster.rgba)?;
      cache.insert(
        *window,
        ScaleTitle {
          title: title.clone(),
          target_width: target.width,
          texture,
        },
      );
    }
    for window in &state.windows {
      let (Some(title), Some(current)) =
        (cache.get(window), geometry.get(window))
      else {
        continue;
      };
      if config.title_overlay == "mouse"
        && !rect_contains(*current, pointer.0, pointer.1)
      {
        continue;
      }
      let x = current.x.saturating_add(
        i32::try_from(current.width.saturating_sub(title.texture.width()) / 2)
          .unwrap_or(i32::MAX),
      );
      let y_offset = match config.title_position.as_str() {
        "top" => 0,
        "bottom" => current.height.saturating_sub(title.texture.height()),
        _ => current.height.saturating_sub(title.texture.height()) / 2,
      };
      let y = current
        .y
        .saturating_add(i32::try_from(y_offset).unwrap_or(i32::MAX));
      tiles.push(RenderTile::textured(
        RenderRect {
          x,
          y,
          width: title.texture.width(),
          height: title.texture.height(),
        },
        &title.texture,
      ));
    }
    Ok(())
  }

  /// Displays the current filter centered above the overview.
  fn append_scale_filter_overlay<'a>(
    renderer: &dyn RenderTarget,
    cache: &'a mut Option<ScaleTitle>,
    filter: &str,
    config: &shingle_config::ScaleTitleFilterConfig,
    output: shingle_core::Rect,
    tiles: &mut Vec<RenderTile<'a>>,
  ) -> Result<(), Box<dyn Error>> {
    if !config.overlay || filter.is_empty() {
      return Ok(());
    }
    let needs_texture = cache.as_ref().is_none_or(|cached| {
      cached.title != filter || cached.target_width != output.width
    });
    if needs_texture {
      let Some(raster) = scale_title::rasterize_title(
        filter,
        config.font_size,
        output.width,
        config.bg_color.map(|value| value as f32),
        config.text_color.map(|value| value as f32),
      ) else {
        return Ok(());
      };
      let texture =
        renderer.create_texture(raster.width, raster.height, &raster.rgba)?;
      *cache = Some(ScaleTitle {
        title: filter.to_owned(),
        target_width: output.width,
        texture,
      });
    }
    let overlay = cache.as_ref().unwrap();
    let x = output.x.saturating_add(
      i32::try_from(output.width.saturating_sub(overlay.texture.width()) / 2)
        .unwrap_or(i32::MAX),
    );
    let y = output.y.saturating_add(
      i32::try_from(output.height.saturating_sub(overlay.texture.height()) / 2)
        .unwrap_or(i32::MAX),
    );
    tiles.push(RenderTile::textured(
      RenderRect {
        x,
        y,
        width: overlay.texture.width(),
        height: overlay.texture.height(),
      },
      &overlay.texture,
    ));
    Ok(())
  }

  /// Builds the scene and draws it into `target`, then sends frame
  /// callbacks if a frame was presented. Asks the host for another frame
  /// while animations run.
  pub fn render(
    &mut self,
    host: &mut dyn Host,
    target: &mut dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    let result = self.render_frame(target);
    self.flush(host);
    result
  }

  fn render_frame(
    &mut self,
    target: &mut dyn RenderTarget,
  ) -> Result<(), Box<dyn Error>> {
    self.update_expo_layout();
    if let Some((started, start)) = self.zoom_animation {
      let progress = animation_progress(
        started,
        Instant::now(),
        Duration::from_millis(self.zoom.smoothing_ms),
      );
      self.zoom_factor =
        shingle_render::interpolate_zoom(start, self.zoom_target, progress);
      if progress >= 1.0 {
        self.zoom_factor = self.zoom_target;
        self.zoom_animation = None;
      }
    }
    let mut tiles = {
      let renderer = &mut *target;
      renderer.set_scale_factor(self.compositor.output_scale());
      renderer.set_output_transform(render_output_transform(
        self.compositor.output_transform(),
      ));
      if renderer.set_nearest_filter(self.zoom.interpolation == "nearest") {
        self.textures.clear();
        self.previous_surfaces.clear();
        self.animations.clear();
      }
      renderer.set_zoom(
        self.zoom_factor,
        self.zoom_origin.0,
        self.zoom_origin.1,
      );
      renderer.set_gamma_ramp(self.compositor.gamma_ramp())?;
      renderer.set_inversion(self.inverted, self.invert_preserve_hue);
      let mut snapshots = if self.expo.is_some() || self.expo_exit.is_some() {
        self.compositor.scale_surfaces(true, false)
      } else {
        self.compositor.visible_surfaces()
      };
      if let Some(state) = self.scale.as_ref().or(self.scale_exit.as_ref()) {
        let visible = snapshots
          .iter()
          .map(|snapshot| snapshot.id)
          .collect::<HashSet<_>>();
        snapshots.extend(
          self
            .compositor
            .scale_surfaces(
              state.all_workspaces,
              self.scale_config.include_minimized,
            )
            .into_iter()
            .filter(|snapshot| !visible.contains(&snapshot.id)),
        );
      }
      if self.workspace_transition.is_some() {
        let visible = snapshots
          .iter()
          .map(|snapshot| snapshot.id)
          .collect::<HashSet<_>>();
        snapshots.extend(
          self
            .compositor
            .scale_surfaces(true, false)
            .into_iter()
            .filter(|snapshot| !visible.contains(&snapshot.id)),
        );
      }
      for snapshot in &snapshots {
        let Some(buffer) = &snapshot.buffer else {
          continue;
        };
        let recreate = self.textures.get(&snapshot.id).is_none_or(|texture| {
          texture.width() != buffer.width || texture.height() != buffer.height
        });
        if recreate {
          let texture = renderer.create_texture(
            buffer.width,
            buffer.height,
            &buffer.rgba,
          )?;
          self.textures.insert(snapshot.id, texture);
        } else if self.dirty.contains(&snapshot.id) {
          renderer
            .update_texture(&self.textures[&snapshot.id], &buffer.rgba)?;
        }
      }
      let now = Instant::now();
      for snapshot in &snapshots {
        if snapshot.buffer.is_none() {
          continue;
        }
        let geometry = RenderRect {
          x: snapshot.geometry.x,
          y: snapshot.geometry.y,
          width: snapshot.geometry.width,
          height: snapshot.geometry.height,
        };
        let Some((old_geometry, old_buffer)) =
          self.previous_surfaces.get(&snapshot.id)
        else {
          continue;
        };
        if *old_geometry == geometry
          || self.animations.contains_key(&snapshot.id)
          || self.animation_duration.is_zero()
        {
          continue;
        }
        let old_texture = renderer.create_texture(
          old_buffer.width,
          old_buffer.height,
          &old_buffer.rgba,
        )?;
        self.animations.insert(
          snapshot.id,
          GeometryAnimation {
            started: now,
            old_geometry: *old_geometry,
            old_texture,
          },
        );
      }
      self.animation_enabled = snapshots
        .iter()
        .filter(|snapshot| self.animation_matches(snapshot))
        .map(|snapshot| snapshot.id)
        .collect();
      self.surface_windows = snapshots
        .iter()
        .filter(|snapshot| {
          snapshot.role == SurfaceRole::Toplevel && snapshot.buffer.is_some()
        })
        .filter_map(|snapshot| {
          snapshot.window.map(|window| (snapshot.id, window))
        })
        .collect();
      let visible = snapshots
        .iter()
        .filter(|snapshot| snapshot.buffer.is_some())
        .map(|snapshot| snapshot.id)
        .collect::<HashSet<_>>();
      self.textures.retain(|id, _| {
        visible.contains(id)
          || self.closing_animations.contains_key(id)
          || self.minimize_animations.contains_key(id)
      });
      self.animations.retain(|id, _| visible.contains(id));
      let tiles = snapshots
        .iter()
        .filter_map(|snapshot| {
          let texture = self.textures.get(&snapshot.id)?;
          let geometry = snapshot.geometry;
          let mut tile = RenderTile::textured(
            RenderRect {
              x: geometry.x,
              y: geometry.y,
              width: geometry.width,
              height: geometry.height,
            },
            texture,
          );
          tile.color = [snapshot.opacity; 4];
          if let Some(buffer) = &snapshot.buffer {
            apply_buffer(&mut tile, buffer);
          }
          let mut tiles = Vec::with_capacity(2);
          if let Some(animation) = self.animations.get(&snapshot.id)
            && let Some((_, old_buffer)) =
              self.previous_surfaces.get(&snapshot.id)
          {
            let progress = animation_progress(
              animation.started,
              now,
              self.animation_duration,
            );
            if progress < 1.0 {
              let mut old_tile = RenderTile::textured(
                animation.old_geometry,
                &animation.old_texture,
              );
              old_tile.color = [snapshot.opacity; 4];
              apply_buffer(&mut old_tile, old_buffer);
              tiles.push(old_tile);
            }
            tile.color = [snapshot.opacity * progress; 4];
          }
          if let Some(started) = self.open_animations.get(&snapshot.id) {
            let progress = animation_progress(
              *started,
              now,
              Duration::from_millis(self.animate.duration_ms),
            );
            apply_opening_animation(
              &mut tile,
              &self.animate.open_animation,
              progress,
            );
          }
          tiles.push(tile);
          if snapshot.role == SurfaceRole::Toplevel
            && !self.minimize_animations.contains_key(&snapshot.id)
            && let Some(window) = snapshot.window
            && let Some(state) =
              self.scale.as_ref().or(self.scale_exit.as_ref())
            && let Some(geometry) = state.geometry.get(&window)
          {
            let selected = state.windows.get(state.selected) == Some(&window);
            let minimized = self
              .compositor
              .core()
              .window_info(window)
              .is_ok_and(|info| info.state.minimized);
            let inactive_alpha = if minimized {
              self.scale_config.minimized_alpha
            } else {
              self.scale_config.inactive_alpha
            };
            let progress = animation_progress(
              state.animation_started,
              now,
              Duration::from_millis(self.scale_config.duration_ms),
            );
            let opacity = if self.scale.is_none() {
              let origin =
                state.opacity_origin.get(&window).copied().unwrap_or(1.0);
              let target = if minimized { 0.0 } else { 1.0 };
              origin + (target - origin) * progress
            } else if selected {
              1.0
            } else {
              1.0 - (1.0 - inactive_alpha) * progress
            };
            for tile in &mut tiles {
              let origin = state
                .animation_origin
                .get(&window)
                .copied()
                .unwrap_or(tile.geometry);
              tile.geometry =
                interpolate_scale_rect(origin, *geometry, progress);
              tile.color[3] *= opacity;
            }
          }
          if let Some(window) = snapshot.window
            && let Some(state) = self.expo.as_ref().or(self.expo_exit.as_ref())
            && let Some(geometry) = state.geometry.get(&window)
            && let Ok(info) = self.compositor.core().window_info(window)
          {
            let (width, _) = self.compositor.workspace_grid_dimensions();
            let workspace = info.location.workspace.y as usize * width as usize
              + info.location.workspace.x as usize;
            let duration = Duration::from_millis(self.expo_config.duration_ms);
            let progress =
              animation_progress(state.animation_started, now, duration);
            let origin = state
              .geometry_origin
              .get(&window)
              .copied()
              .unwrap_or(*geometry);
            let brightness = state.brightness(
              workspace,
              now,
              Duration::from_millis(self.expo_config.transition_length_ms),
              self.expo_config.inactive_brightness,
            );
            let dragged = state
              .dragged_window()
              .filter(|(dragged, _, _)| *dragged == window);
            for tile in &mut tiles {
              tile.geometry =
                expo::interpolate_rect(origin, *geometry, progress);
              if let Some((_, x, y)) = dragged {
                tile.geometry.x =
                  (x - f64::from(tile.geometry.width) / 2.0).round() as i32;
                tile.geometry.y =
                  (y - f64::from(tile.geometry.height) / 2.0).round() as i32;
              }
              for component in &mut tile.color[..3] {
                *component *= brightness;
              }
            }
          }
          if let Some(window) = snapshot.window
            && let Some(transition) = &self.workspace_transition
            && let Ok(info) = self.compositor.core().window_info(window)
          {
            let (grid_width, _) = self.compositor.workspace_grid_dimensions();
            let workspace = info.location.workspace.y as usize
              * grid_width as usize
              + info.location.workspace.x as usize;
            let layout = workspace_transition::TransitionLayout {
              output: self.compositor.output(),
              grid_width,
              gap: self.vswitch.gap,
            };
            let geometry = transition.geometry(
              snapshot.geometry,
              workspace,
              layout,
              now,
              Duration::from_millis(self.vswitch.duration_ms),
            );
            for tile in &mut tiles {
              tile.geometry = geometry;
            }
          }
          if let Some(angle) = snapshot
            .window
            .and_then(|window| self.rotations.get(&window))
          {
            for tile in &mut tiles {
              tile.rotation = *angle;
            }
          }
          Some(tiles)
        })
        .flatten()
        .collect::<Vec<_>>();
      self.previous_surfaces = snapshots
        .iter()
        .filter_map(|snapshot| {
          let geometry = snapshot.geometry;
          Some((
            snapshot.id,
            (
              RenderRect {
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
              },
              snapshot.buffer.clone()?,
            ),
          ))
        })
        .collect();
      tiles
    };
    if let Some(state) = self.expo.as_ref().or(self.expo_exit.as_ref()) {
      let now = Instant::now();
      let duration = Duration::from_millis(self.expo_config.duration_ms);
      let transition =
        Duration::from_millis(self.expo_config.transition_length_ms);
      let mut cells = Vec::new();
      for (index, cell) in state.cells_at(now, duration).iter().enumerate() {
        let brightness = state.brightness(
          index,
          now,
          transition,
          self.expo_config.inactive_brightness,
        );
        let border = [0.8 * brightness; 4];
        cells.extend(preview_tiles(
          *cell,
          self
            .expo_config
            .background
            .map(|component| component as f32),
          border,
          2,
        ));
      }
      cells.append(&mut tiles);
      tiles = cells;
    }
    self.append_drop_preview(&mut tiles);
    if let Some(state) = &self.scale {
      let snapshots = self.compositor.scale_surfaces(true, true);
      let titles = snapshots
        .iter()
        .filter_map(|snapshot| {
          Some((snapshot.window?, snapshot.title.clone()?))
        })
        .collect::<HashMap<_, _>>();
      let icons = snapshots
        .into_iter()
        .filter_map(|snapshot| Some((snapshot.window?, snapshot.icon?)))
        .collect::<HashMap<_, _>>();
      Self::append_scale_icons(
        &*target,
        &mut self.scale_icons,
        state,
        Duration::from_millis(self.scale_config.duration_ms),
        &icons,
        &mut tiles,
      )?;
      Self::append_scale_titles(
        &*target,
        &mut self.scale_titles,
        state,
        &self.scale_config,
        &titles,
        (self.pointer_x, self.pointer_y),
        &mut tiles,
      )?;
    }
    if self.scale.is_some() {
      Self::append_scale_filter_overlay(
        &*target,
        &mut self.scale_filter_texture,
        &self.scale_filter,
        &self.scale_title_filter,
        self.compositor.output(),
        &mut tiles,
      )?;
    }
    Self::append_closing_surfaces(
      &self.closing_animations,
      &self.textures,
      &self.animate,
      &self.animate.close_animation,
      &mut tiles,
    );
    Self::append_closing_surfaces(
      &self.minimize_animations,
      &self.textures,
      &self.animate,
      &self.animate.minimize_animation,
      &mut tiles,
    );
    let renderer = target;
    renderer.set_tearing(self.compositor.allows_tearing());
    let background = if self.expo.is_some() || self.expo_exit.is_some() {
      self.expo_config.background
    } else if self.workspace_transition.is_some() {
      self.vswitch.background
    } else {
      self.background_color
    };
    if renderer.render(&tiles, background)? {
      self.compositor.frame();
    }
    let now = Instant::now();
    self.animations.retain(|_, animation| {
      animation_progress(animation.started, now, self.animation_duration) < 1.0
    });
    self.open_animations.retain(|_, started| {
      animation_progress(
        *started,
        now,
        Duration::from_millis(self.animate.duration_ms),
      ) < 1.0
    });
    self.expire_closing_animations(now);
    self.expire_minimize_animations(now);
    self.expire_scale_exit(now);
    if self
      .workspace_transition
      .as_ref()
      .is_some_and(|transition| {
        animation_progress(
          transition.started,
          now,
          Duration::from_millis(self.vswitch.duration_ms),
        ) >= 1.0
      })
    {
      self.workspace_transition = None;
    }
    let expo_duration = Duration::from_millis(self.expo_config.duration_ms);
    if self.expo_exit.as_ref().is_some_and(|state| {
      animation_progress(state.animation_started, now, expo_duration) >= 1.0
    }) {
      self.expo_exit = None;
    }
    let expo_transition =
      Duration::from_millis(self.expo_config.transition_length_ms);
    if !self.animations.is_empty()
      || !self.open_animations.is_empty()
      || !self.closing_animations.is_empty()
      || !self.minimize_animations.is_empty()
      || self.scale.as_ref().is_some_and(|state| {
        animation_progress(
          state.animation_started,
          now,
          Duration::from_millis(self.scale_config.duration_ms),
        ) < 1.0
      })
      || self.scale_exit.is_some()
      || self.expo.as_ref().is_some_and(|state| {
        animation_progress(state.animation_started, now, expo_duration) < 1.0
          || animation_progress(state.selection_started, now, expo_transition)
            < 1.0
      })
      || self.expo_exit.is_some()
      || self.workspace_transition.is_some()
      || self.zoom_animation.is_some()
    {
      self.request_redraw();
    }
    self.dirty.clear();
    Ok(())
  }
}

impl Frontend {
  /// The compositor, for queries.
  pub fn compositor(&self) -> &Compositor {
    &self.compositor
  }

  /// The compositor, for the host's setup: registering seat devices,
  /// enabling IPC and mapping host coordinates.
  pub fn compositor_mut(&mut self) -> &mut Compositor {
    &mut self.compositor
  }

  /// Whether the configuration swaps the left and right buttons. libinput
  /// applies this itself; nested emulates it.
  pub fn left_handed_mode(&self) -> bool {
    self.left_handed_mode
  }

  /// Whether the configuration emulates a middle click from left and
  /// right pressed together. libinput applies this itself; nested
  /// emulates it.
  pub fn middle_emulation(&self) -> bool {
    self.middle_emulation
  }

  /// Handles one input event from seat device `device`, which happened at
  /// `time` (`CLOCK_MONOTONIC`) if the host knows it. Clients get the
  /// time the compositor received the event otherwise.
  ///
  /// Input from a disabled device is dropped, except that keys still
  /// update the keyboard state.
  pub fn handle_input(
    &mut self,
    host: &mut dyn Host,
    device: InputDeviceId,
    event: InputEvent,
    time: Option<Duration>,
  ) {
    if time.is_some() {
      self.compositor.set_input_time(time);
    }
    self.dispatch_input(host, device, event);
    if time.is_some() {
      self.compositor.set_input_time(None);
    }
    self.flush(host);
  }

  /// Lets host glue change the keyboard state without dispatching keys,
  /// as nested's focus handling presses stand-in keys for modifiers the
  /// host reports. If `update` returns true, the modifiers changed: they
  /// are sent for `device` if it is enabled, and the switcher ends if its
  /// modifiers were released.
  pub fn update_keyboard(
    &mut self,
    host: &mut dyn Host,
    device: InputDeviceId,
    update: impl FnOnce(&mut Keyboard) -> bool,
  ) {
    if update(&mut self.keyboard)
      && self.compositor.input_device_enabled(device)
    {
      self.keyboard_modifiers_changed(device);
    }
    self.flush(host);
  }

  /// Runs one turn of the frontend: dispatches the compositor's clients
  /// and IPC, reloads a changed config, runs key repeat and the drag and
  /// gesture timers, reaps finished children and handles compositor
  /// events. Calls [`Host::exit`] after the quit binding, on a signal or
  /// when dispatch fails.
  pub fn process(&mut self, host: &mut dyn Host) {
    if self.quit {
      host.exit();
      return;
    }
    if let Err(error) = self.compositor.dispatch_pending() {
      eprintln!("shingle: dispatch failed: {error}");
      host.exit();
      return;
    }
    // SIGINT and SIGTERM reach the compositor's event loop. The host
    // saves the session through `shutdown` on its way out.
    if self.compositor.shutdown_requested() {
      host.exit();
      return;
    }
    self.reload_if_changed();
    self.process_key_repeat(host);
    self.process_workspace_switch_timeout();
    self.process_extra_gestures();
    self
      .session_children
      .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
    let changed = self.process_events(host);
    if changed || self.compositor.needs_frame() {
      self.request_redraw();
    }
    self.flush(host);
  }

  /// Returns when [`Frontend::process`] next has timed work: a key
  /// repeat. Hosts also call it at least every 16 ms, for config reload,
  /// drag and gesture timers and child reaping.
  pub fn next_deadline(&self) -> Option<Instant> {
    self.key_repeat.deadline()
  }

  /// Sets the output to a `width` by `height` pixel display, in logical
  /// size after the output scale. The host calls this once it knows its
  /// display size.
  pub fn set_output_size(
    &mut self,
    width: u32,
    height: u32,
  ) -> Result<(), shingle::Error> {
    let (width, height) = self.compositor.logical_output_size(width, height);
    self
      .compositor
      .set_output(shingle_core::Rect::new(0, 0, width, height))
  }

  /// Handles a resize of the host's display to `width` by `height`
  /// pixels: resizes the output and lays out Scale and Expo again, ending
  /// transitions that no longer fit.
  pub fn output_resized(
    &mut self,
    width: u32,
    height: u32,
  ) -> Result<(), shingle::Error> {
    let result = self.set_output_size(width, height);
    self.expo_exit = None;
    self.workspace_transition = None;
    self.update_scale_layout();
    self.update_expo_layout();
    result
  }

  /// Saves the session. Hosts call this when their event loop ends.
  pub fn shutdown(&mut self) {
    self.save();
  }

  fn request_redraw(&mut self) {
    self.redraw_requested = true;
  }

  /// Passes a pending redraw request on to the host.
  fn flush(&mut self, host: &mut dyn Host) {
    if std::mem::take(&mut self.redraw_requested) {
      host.request_redraw();
    }
  }

  fn dispatch_input(
    &mut self,
    host: &mut dyn Host,
    device: InputDeviceId,
    event: InputEvent,
  ) {
    match event {
      InputEvent::Key { code, pressed } => {
        self.key_event(host, device, code, pressed)
      }
      InputEvent::Reset => self.reset_keyboard(device),
      _ if !self.compositor.input_device_enabled(device) => {}
      InputEvent::PointerMotion {
        dx,
        dy,
        dx_unaccel,
        dy_unaccel,
      } => {
        if self.compositor.pointer_constrained() {
          self
            .compositor
            .pointer_relative_motion(device, dx, dy, dx_unaccel, dy_unaccel);
        }
      }
      InputEvent::PointerMotionAbsolute { x, y } => {
        self.pointer_motion(device, x, y)
      }
      InputEvent::PointerButton { code, pressed } => {
        self.pointer_button(device, code, pressed)
      }
      InputEvent::PointerAxis(axis) => self.pointer_axis(device, axis),
      InputEvent::Touch(touch) => self.touch(device, touch),
      InputEvent::Swipe(gesture) => match gesture {
        Gesture::Begin { fingers } => {
          self.compositor.pointer_swipe_begin(device, fingers)
        }
        Gesture::Update { dx, dy, .. } => {
          self.compositor.pointer_swipe_update(device, dx, dy)
        }
        Gesture::End { cancelled } => {
          self.compositor.pointer_swipe_end(device, cancelled)
        }
      },
      InputEvent::Pinch(gesture) => match gesture {
        Gesture::Begin { fingers } => {
          self.compositor.pointer_pinch_begin(device, fingers)
        }
        Gesture::Update {
          dx,
          dy,
          scale,
          rotation,
        } => self
          .compositor
          .pointer_pinch_update(device, dx, dy, scale, rotation),
        Gesture::End { cancelled } => {
          self.compositor.pointer_pinch_end(device, cancelled)
        }
      },
      InputEvent::Hold(gesture) => match gesture {
        Gesture::Begin { fingers } => {
          self.compositor.pointer_hold_begin(device, fingers)
        }
        Gesture::Update { .. } => {}
        Gesture::End { cancelled } => {
          self.compositor.pointer_hold_end(device, cancelled)
        }
      },
    }
  }

  /// Moves the pointer to `(x, y)` in output pixels, before the zoom.
  fn pointer_motion(&mut self, device: InputDeviceId, x: f64, y: f64) {
    let overview_active = self.scale.is_some() || self.expo.is_some();
    if !overview_active && self.compositor.pointer_constrained() {
      return;
    }
    let (x, y) = self.map_zoom_input(x, y);
    self.pointer_x = x;
    self.pointer_y = y;
    if self.expo.is_some() {
      self.move_expo_pointer(x, y);
      return;
    }
    if self.scale.is_some() {
      self.select_scale_at(x, y);
      return;
    }
    if self.zoom_factor > 1.0 {
      self.zoom_origin = (x as f32, y as f32);
    }
    if self.rotation_drag.is_some() {
      self.rotate_pointer(x, y);
      return;
    }
    match self.drag {
      Some(DragState::Move { .. }) => self.move_drag(x, y),
      Some(DragState::Resize { .. }) => self.resize_drag(x, y),
      None => {}
    }
    if self.drag.is_none() {
      self.compositor.pointer_motion(device, x, y);
    }
  }

  fn pointer_button(
    &mut self,
    device: InputDeviceId,
    code: u32,
    pressed: bool,
  ) {
    if self.expo_pointer_input(code, pressed) {
      return;
    }
    if self.scale_pointer_input(code, pressed) {
      return;
    }
    if !self.pointer_button_action(button_name(code), pressed) {
      self.compositor.pointer_button(device, code, pressed);
      self.save();
    }
  }

  fn touch(&mut self, device: InputDeviceId, touch: Touch) {
    let (x, y) = self.map_zoom_input(touch.x, touch.y);
    if self.expo.is_some() {
      match touch.phase {
        TouchPhase::Down if self.expo_touch.is_none() => {
          self.expo_touch = Some(touch.id);
          self.pointer_x = x;
          self.pointer_y = y;
          self.expo_pointer_input(BTN_LEFT, true);
        }
        TouchPhase::Motion if self.expo_touch == Some(touch.id) => {
          self.move_expo_pointer(x, y);
        }
        TouchPhase::Up if self.expo_touch == Some(touch.id) => {
          self.expo_pointer_input(BTN_LEFT, false);
        }
        TouchPhase::Cancel if self.expo_touch == Some(touch.id) => {
          self.expo_touch = None;
          if let Some(state) = &mut self.expo {
            state.cancel_pointer();
          }
        }
        _ => {}
      }
      return;
    }
    if self.scale.is_some() {
      if touch.phase == TouchPhase::Down && self.select_scale_at(x, y) {
        self.finish_scale();
      }
      return;
    }
    let now = Instant::now();
    let id = touch.id as i32;
    let action = match touch.phase {
      TouchPhase::Down => {
        self.compositor.touch_down(device, id, x, y);
        self.gestures.touch_down(touch.id, x, y, now);
        None
      }
      TouchPhase::Motion => {
        let action = self.gestures.touch_motion(touch.id, x, y, now);
        if self.drag.is_some() {
          self.pointer_x = x;
          self.pointer_y = y;
          self.move_drag(x, y);
        } else {
          self.compositor.touch_motion(device, id, x, y);
        }
        action
      }
      TouchPhase::Up => {
        let action = self.gestures.touch_up(touch.id, now);
        if self.drag.is_some() {
          self.pointer_button_action("btn_left", false);
        }
        self.compositor.touch_up(device, id);
        action
      }
      TouchPhase::Cancel => {
        self.gestures.cancel();
        if self.drag.is_some() {
          self.pointer_button_action("btn_left", false);
        }
        self.compositor.touch_cancel(device, id);
        None
      }
    };
    if let Some(action) = action {
      self.handle_gesture_action(action);
    }
  }

  fn pointer_axis(&mut self, device: InputDeviceId, axis: Axis) {
    if self.expo.is_some() {
      return;
    }
    let Axis {
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    } = axis;
    if self.scale.is_some() {
      if vertical != 0.0 {
        self.select_scale_relative(vertical > 0.0);
      }
      return;
    }
    let shortcuts_allowed =
      !self.compositor.is_locked() && !self.compositor.shortcuts_inhibited();
    if shortcuts_allowed
      && vertical != 0.0
      && modifier_matches(&self.alpha.modifier, self.config_modifiers())
      && self
        .compositor
        .adjust_opacity_at(
          self.pointer_x,
          self.pointer_y,
          vertical,
          self.alpha.min_value,
        )
        .is_some()
    {
      self.request_redraw();
      return;
    }
    if shortcuts_allowed
      && modifier_matches(&self.zoom.modifier, self.config_modifiers())
    {
      self.zoom_origin = (self.pointer_x as f32, self.pointer_y as f32);
      let target = (self.zoom_target
        - self.zoom_target * vertical as f32 * self.zoom.speed as f32)
        .clamp(1.0, 50.0);
      if target == self.zoom_target {
        return;
      }
      self.zoom_target = target;
      if self.zoom.smoothing_ms == 0 {
        self.zoom_factor = target;
        self.zoom_animation = None;
      } else {
        self.zoom_animation = Some((Instant::now(), self.zoom_factor));
      }
      self.request_redraw();
      return;
    }
    self.compositor.pointer_axis_detailed(
      device,
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    );
  }
}

/// Returns the binding name of pointer button `code`. Codes `0x113` and
/// `0x114` (`BTN_SIDE` and `BTN_EXTRA`) are named `btn_back` and
/// `btn_forward`, after the winit buttons nested has always reported.
fn button_name(code: u32) -> &'static str {
  match code {
    BTN_LEFT => "btn_left",
    BTN_RIGHT => "btn_right",
    BTN_MIDDLE => "btn_middle",
    0x113 => "btn_back",
    0x114 => "btn_forward",
    _ => "btn_other",
  }
}

fn apply_buffer(tile: &mut RenderTile<'_>, buffer: &BufferSnapshot) {
  tile.transform = match buffer.transform {
    wayland_server::protocol::wl_output::Transform::Normal => {
      TextureTransform::Normal
    }
    wayland_server::protocol::wl_output::Transform::Flipped => {
      TextureTransform::FlipHorizontal
    }
    wayland_server::protocol::wl_output::Transform::_90 => {
      TextureTransform::Rotate90
    }
    wayland_server::protocol::wl_output::Transform::_180 => {
      TextureTransform::FlipBoth
    }
    wayland_server::protocol::wl_output::Transform::_270 => {
      TextureTransform::Rotate270
    }
    wayland_server::protocol::wl_output::Transform::Flipped90 => {
      TextureTransform::Transpose
    }
    wayland_server::protocol::wl_output::Transform::Flipped180 => {
      TextureTransform::FlipVertical
    }
    wayland_server::protocol::wl_output::Transform::Flipped270 => {
      TextureTransform::AntiTranspose
    }
    _ => TextureTransform::Normal,
  };
  if let (Some(texture), Some(source)) = (tile.texture, buffer.source) {
    tile.uv = [
      ((source[0] + 0.5) / f64::from(texture.width())) as f32,
      ((source[1] + 0.5) / f64::from(texture.height())) as f32,
      ((source[0] + source[2] - 0.5) / f64::from(texture.width())) as f32,
      ((source[1] + source[3] - 0.5) / f64::from(texture.height())) as f32,
    ];
  }
}

fn apply_opening_animation(
  tile: &mut RenderTile<'_>,
  animation: &str,
  progress: f32,
) {
  let progress = progress.clamp(0.0, 1.0);
  tile.color[3] *= progress;
  if animation != "zoom" {
    return;
  }
  let original = tile.geometry;
  let scale = 1.0 / 3.0 + 2.0 / 3.0 * f64::from(progress);
  tile.geometry.width = (f64::from(original.width) * scale).round() as u32;
  tile.geometry.height = (f64::from(original.height) * scale).round() as u32;
  tile.geometry.x = original.x.saturating_add(
    i32::try_from((original.width - tile.geometry.width) / 2)
      .unwrap_or(i32::MAX),
  );
  tile.geometry.y = original.y.saturating_add(
    i32::try_from((original.height - tile.geometry.height) / 2)
      .unwrap_or(i32::MAX),
  );
}

fn render_output_transform(
  transform: wayland_server::protocol::wl_output::Transform,
) -> TextureTransform {
  use wayland_server::protocol::wl_output::Transform;
  match transform {
    Transform::Normal => TextureTransform::Normal,
    Transform::Flipped => TextureTransform::FlipHorizontal,
    Transform::_90 => TextureTransform::Rotate90,
    Transform::_180 => TextureTransform::FlipBoth,
    Transform::_270 => TextureTransform::Rotate270,
    Transform::Flipped90 => TextureTransform::Transpose,
    Transform::Flipped180 => TextureTransform::FlipVertical,
    Transform::Flipped270 => TextureTransform::AntiTranspose,
    _ => TextureTransform::Normal,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn opening_animation_fades_and_zooms_from_center() {
    let geometry = RenderRect {
      x: 100,
      y: 50,
      width: 200,
      height: 100,
    };
    let mut fade = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut fade, "fade", 0.25);
    assert_eq!(fade.geometry, geometry);
    assert_eq!(fade.color[3], 0.25);

    let mut zoom = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut zoom, "zoom", 0.5);
    assert_eq!(
      zoom.geometry,
      RenderRect {
        x: 133,
        y: 66,
        width: 133,
        height: 67,
      }
    );
    assert_eq!(zoom.color[3], 0.5);

    let mut start = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut start, "zoom", 0.0);
    assert_eq!(start.geometry.width, 67);
    assert_eq!(start.geometry.height, 33);
    assert_eq!(start.color[3], 0.0);

    let mut complete = RenderTile::solid(geometry, [1.0; 4]);
    apply_opening_animation(&mut complete, "zoom", 1.0);
    assert_eq!(complete.geometry, geometry);
    assert_eq!(complete.color[3], 1.0);
  }
}
