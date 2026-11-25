//! The [`Frontend`] struct, its public API and input dispatch. Each child
//! module adds the methods for one area: `bindings` (key and pointer
//! bindings, key repeat), `scale` and `expo` (the overviews), `switcher`,
//! `drag`, `effects` (zoom, inversion, alpha), `animation`, `scene` (the
//! scene build and drawing) and `session` (saving, config reload, the lock
//! client).

mod animation;
mod bindings;
mod drag;
mod effects;
mod expo;
mod scale;
mod scene;
mod session;
mod switcher;
#[cfg(test)]
mod tests;

use std::{
  collections::{HashMap, HashSet},
  error::Error,
  process::Child,
  time::{Duration, Instant},
};

use shingle::{
  BufferSnapshot, Compositor, Config, Direction, Event, InputDeviceId,
  InputDeviceKind, SurfaceId,
};
use shingle_config::{
  CommandBinding, ConfigWatcher, Keybindings, PointerBindings, ShingleConfig,
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{GpuTexture, RenderRect};
use shingle_session::{
  SessionStore, launch_autostart, launch_program, launch_saved,
};

use crate::{
  extra_gestures,
  host::{Gesture, Host, InputEvent, Touch, TouchPhase},
  keyboard::{KeyRepeat, Keyboard},
  scale::ScaleState,
  workspace_transition,
};

use animation::{ClosingAnimation, GeometryAnimation};
use drag::{DragState, RotationDrag};
use scale::ScaleTitle;
use switcher::SwitcherState;

/// `BTN_LEFT`.
const BTN_LEFT: u32 = 0x110;
/// `BTN_RIGHT`.
const BTN_RIGHT: u32 = 0x111;
/// `BTN_MIDDLE`.
const BTN_MIDDLE: u32 = 0x112;

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
  expo: Option<crate::expo::ExpoState>,
  expo_exit: Option<crate::expo::ExpoState>,
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
  /// compositor, serves Wayfire IPC at `ipc` if given, launches restored
  /// and autostart clients, prints `WAYLAND_DISPLAY` and starts `spawn`, if
  /// given, as a client. Every client is started in
  /// [`Compositor::client_environment`], so IPC is enabled first for them
  /// to find it as `WAYFIRE_SOCKET`.
  ///
  /// The host registers its seat devices afterwards with
  /// [`Frontend::compositor_mut`].
  pub fn new(
    config: ShingleConfig,
    spawn: Vec<std::ffi::OsString>,
    config_path: Option<std::path::PathBuf>,
    ipc: Option<&std::path::Path>,
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
        let mut compositor =
          Compositor::with_core(Config::from(config.clone()), core)?;
        if let Some(path) = ipc {
          compositor.enable_ipc(path)?;
        }
        let report = launch_saved(
          &snapshot,
          &compositor.client_environment(),
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
        let mut compositor = Compositor::new(Config::from(config.clone()))?;
        if let Some(path) = ipc {
          compositor.enable_ipc(path)?;
        }
        (compositor, Vec::new(), None)
      };
    let autostart = launch_autostart(
      &config.autostart,
      restored_snapshot.as_ref(),
      &compositor.client_environment(),
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
      let environment = frontend.compositor.client_environment();
      let child = launch_program(program, args, &environment)?;
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
    // STIPC keys take the path of real keys, so configured bindings fire.
    compositor.set_stipc_key_events(true);
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
        Event::StipcKey {
          device,
          code,
          pressed,
        } => self.key_event(host, device, code, pressed),
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
