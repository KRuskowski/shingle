mod expo;
mod extra_gestures;
mod icon_theme;
mod scale_title;
mod workspace_transition;

use std::{
  collections::{HashMap, HashSet},
  error::Error,
  process::Child,
  sync::Arc,
  time::{Duration, Instant},
};

use shingle::{
  BufferSnapshot, Compositor, Config, Direction, Event, InputDeviceId,
  InputDeviceInfo, InputDeviceKind, Modifiers, SurfaceId, SurfaceRole,
  SurfaceSnapshot,
};
use shingle_config::{
  CommandBinding, ConfigWatcher, Keybindings, Modifiers as ConfigModifiers,
  PointerBindings, ShingleConfig, ViewProperties,
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuTexture, PreviewDirection, RenderRect, RenderTile, Renderer,
  TextureTransform, animation_progress, preview_tiles, split_preview,
};
use shingle_session::{
  SessionStore, launch_autostart, launch_command, launch_saved,
};
use winit::{
  application::ApplicationHandler,
  dpi::PhysicalSize,
  event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta,
    TouchPhase, WindowEvent,
  },
  event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
  keyboard::ModifiersState,
  platform::scancode::PhysicalKeyExtScancode,
  window::{Window, WindowAttributes, WindowId},
};

enum DragState {
  Move {
    source: TileWindowId,
    last_x: f64,
    last_y: f64,
    snap_off_origin: Option<(f64, f64)>,
  },
  Resize {
    window: TileWindowId,
    horizontal: Option<Direction>,
    vertical: Option<Direction>,
    last_x: f64,
    last_y: f64,
    aspect: Option<(f64, f64, shingle_core::Rect)>,
  },
}

#[derive(Clone, Copy)]
struct RotationDrag {
  window: TileWindowId,
  last_x: f64,
  last_y: f64,
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwitcherKind {
  Fast,
  Regular,
}

struct SwitcherState {
  kind: SwitcherKind,
  windows: Vec<TileWindowId>,
  selected: usize,
  modifiers: ConfigModifiers,
  original_opacity: HashMap<TileWindowId, f32>,
}

/// Transient overview state.
/// Original layouts and compositor opacity stay intact.
///
/// `initial_focus` is restored on cancellation. `geometry` contains only the
/// renderer's thumbnail rectangles, never replacement tiling geometry.
struct ScaleState {
  windows: Vec<TileWindowId>,
  selected: usize,
  cells: Vec<RenderRect>,
  geometry: HashMap<TileWindowId, RenderRect>,
  animation_origin: HashMap<TileWindowId, RenderRect>,
  opacity_origin: HashMap<TileWindowId, f32>,
  animation_started: Instant,
  all_workspaces: bool,
  initial_focus: Option<TileWindowId>,
  initial_workspace: usize,
}

struct ScaleTitle {
  title: String,
  target_width: u32,
  texture: GpuTexture,
}

#[derive(Default)]
struct MouseButtons {
  left: bool,
  right: bool,
  emulated_middle: bool,
}

impl MouseButtons {
  fn translate<'a>(
    &mut self,
    button: u32,
    name: &'a str,
    pressed: bool,
    middle_emulation: bool,
    left_handed: bool,
  ) -> Option<(u32, &'a str)> {
    let physical = match name {
      "btn_left" => &mut self.left,
      "btn_right" => &mut self.right,
      _ => return Some((button, name)),
    };
    if pressed {
      if *physical {
        return None;
      }
      *physical = true;
      if middle_emulation && self.left && self.right && !self.emulated_middle {
        self.emulated_middle = true;
        return Some((0x112, "btn_middle"));
      }
    } else {
      if !*physical {
        return None;
      }
      *physical = false;
      if self.emulated_middle {
        self.emulated_middle = false;
        return Some((0x112, "btn_middle"));
      }
    }
    let name = if left_handed {
      match name {
        "btn_left" => "btn_right",
        "btn_right" => "btn_left",
        _ => name,
      }
    } else {
      name
    };
    Some((button, name))
  }
}

/// The seat devices nested feeds host input from: one each for the host
/// window's keyboard, pointer and touch input, which STIPC input also goes
/// through.
#[derive(Clone, Copy, Debug)]
struct SeatDevices {
  keyboard: InputDeviceId,
  pointer: InputDeviceId,
  touch: InputDeviceId,
}

impl SeatDevices {
  /// Registers the three devices in the order and with the names that
  /// `input/list-devices` has always reported for nested, so they get IDs
  /// 1 to 3 on a new compositor.
  fn register(compositor: &mut Compositor) -> Self {
    let mut add = |name: &str, kind| {
      compositor.add_input_device(InputDeviceInfo {
        name: name.into(),
        vendor: None,
        product: None,
        kind,
      })
    };
    Self {
      keyboard: add("Shingle nested keyboard", InputDeviceKind::Keyboard),
      pointer: add("Shingle nested pointer", InputDeviceKind::Pointer),
      touch: add("Shingle nested touch", InputDeviceKind::Touch),
    }
  }

  /// Returns the device for host input of `kind`.
  fn get(self, kind: InputDeviceKind) -> InputDeviceId {
    match kind {
      InputDeviceKind::Keyboard => self.keyboard,
      InputDeviceKind::Pointer => self.pointer,
      InputDeviceKind::Touch => self.touch,
    }
  }

  /// Returns the kind of one of these devices.
  fn kind(self, id: InputDeviceId) -> Option<InputDeviceKind> {
    [
      InputDeviceKind::Keyboard,
      InputDeviceKind::Pointer,
      InputDeviceKind::Touch,
    ]
    .into_iter()
    .find(|kind| self.get(*kind) == id)
  }
}

struct App {
  compositor: Compositor,
  devices: SeatDevices,
  store: SessionStore,
  window: Option<Arc<Window>>,
  renderer: Option<Renderer>,
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
  modifiers: ModifiersState,
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
  mouse_buttons: MouseButtons,
  left_handed_mode: bool,
  middle_emulation: bool,
  pointer_x: f64,
  pointer_y: f64,
  pending_warp: Option<(f64, f64)>,
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
}

impl App {
  fn new(
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
    let keymap = config.input.load_keymap()?;
    let (mut compositor, mut session_children, restored_snapshot) =
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
    let devices = SeatDevices::register(&mut compositor);
    compositor.set_keymap(&keymap)?;
    compositor.set_repeat(config.input.repeat_rate, config.input.repeat_delay);
    println!(
      "WAYLAND_DISPLAY={}",
      compositor.socket_name().to_string_lossy()
    );
    if let Some((program, args)) = spawn.split_first() {
      let child = std::process::Command::new(program)
        .args(args)
        .env("WAYLAND_DISPLAY", compositor.socket_name())
        .spawn()?;
      session_children.push(child);
    }
    let initial_workspace = compositor.current_workspace_index().unwrap_or(0);
    Ok(Self {
      compositor,
      devices,
      store,
      window: None,
      renderer: None,
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
      modifiers: ModifiersState::empty(),
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
      last_workspace: initial_workspace,
      mouse_buttons: MouseButtons::default(),
      left_handed_mode: config.input.left_handed_mode,
      middle_emulation: config.input.middle_emulation,
      pointer_x: 0.0,
      pointer_y: 0.0,
      pending_warp: None,
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
    })
  }

  fn process_events(&mut self) -> bool {
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
          if let Some(window) = &self.window {
            let size = window.inner_size();
            let point = self.compositor.unmap_output_point(
              point.0,
              point.1,
              size.width,
              size.height,
            );
            self.pending_warp = Some(point);
            if let Err(error) = window.set_cursor_position(
              winit::dpi::PhysicalPosition::new(point.0, point.1),
            ) {
              eprintln!("shingle: pointer warp failed: {error}");
            }
          }
        }
        Event::StartLockClient => self.start_lock_client(),
        Event::InputDeviceChanged { id, enabled } => {
          if let Some(kind) = self.devices.kind(id) {
            self.input_device_changed(kind, enabled);
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
  fn input_device_changed(&mut self, device: InputDeviceKind, enabled: bool) {
    match (device, enabled) {
      (InputDeviceKind::Keyboard, false) => {
        // The switcher's modifiers can no longer be released, so finish it
        // as if they had been.
        if self.switcher.is_some() {
          self.finish_switcher();
        }
      }
      (InputDeviceKind::Keyboard, true) => {
        // Host modifiers kept being tracked while disabled; resync them.
        self.update_modifiers();
      }
      (InputDeviceKind::Pointer, false) => {
        self.mouse_buttons = MouseButtons::default();
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

  /// Ends a move or resize drag where the window currently is, without the
  /// drop, swap or snap a button release would perform.
  fn abandon_drag(&mut self) {
    if self.drag.take().is_none() {
      return;
    }
    self.drop_slot = None;
    self.drop_preview = None;
    self.workspace_switch_started = None;
    self.save();
  }

  fn start_lock_client(&mut self) {
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
      .unwrap_or_else(|| "shingle-lock".into());
    match std::process::Command::new(program)
      .env("WAYLAND_DISPLAY", self.compositor.socket_name())
      .spawn()
    {
      Ok(child) => self.lock_client = Some(child),
      Err(error) => {
        eprintln!("shingle: failed to start lock client: {error}")
      }
    }
  }

  fn save(&self) {
    if let Err(error) = self.store.save(self.compositor.core()) {
      eprintln!("shingle: failed to save session: {error}");
    }
  }

  fn reload_config(&mut self) {
    self.config_watcher.mark_loaded();
    let result = (|| {
      let config = ShingleConfig::load(self.config_path.as_deref())?;
      let keymap = config.input.load_keymap()?;
      self.compositor.apply_config(config.clone().into())?;
      self.compositor.set_keymap(&keymap)?;
      self
        .compositor
        .set_repeat(config.input.repeat_rate, config.input.repeat_delay);
      Ok::<ShingleConfig, Box<dyn Error>>(config)
    })();
    match result {
      Ok(config) => {
        self.finish_switcher();
        self.cancel_scale();
        self.clear_expo();
        self.workspace_transition = None;
        self.last_workspace =
          self.compositor.current_workspace_index().unwrap_or(0);
        self.scale_exit = None;
        self.scale_titles.clear();
        self.scale_icons.clear();
        self.scale_filter.clear();
        self.scale_filter_texture = None;
        self.switcher_inactive_alpha = config.fast_switcher.inactive_alpha;
        self.scale_config = config.scale.clone();
        self.scale_title_filter = config.scale_title_filter.clone();
        self.expo_config = config.expo.clone();
        self.vswitch = config.vswitch.clone();
        self.keybindings = config.keybindings;
        self.commands = config.commands;
        self.pointer_bindings = config.pointer;
        self.minimum_extent = config.tiling.minimum_extent;
        self.resize = config.resize.clone();
        self.discard_command_output = config.behavior.discard_command_output;
        self.zoom = config.zoom.clone();
        self.alpha = config.alpha.clone();
        self.left_handed_mode = config.input.left_handed_mode;
        self.middle_emulation = config.input.middle_emulation;
        self.compositor.clamp_opacity(self.alpha.min_value);
        self.animate = config.animate.clone();
        if self.animate.close_animation == "none" {
          for surface in self.closing_animations.keys() {
            self.textures.remove(surface);
          }
          self.closing_animations.clear();
        }
        if self.animate.minimize_animation == "none" {
          self.minimize_animations.clear();
        }
        if self.animate.open_animation == "none" {
          self.open_animations.clear();
        }
        self.invert_preserve_hue = config.effects.invert_preserve_hue;
        self.background_color = config.background_color;
        self.animation_duration =
          Duration::from_millis(config.tiling.animation_duration);
        self.preview_base_color = config.tiling.preview_base_color;
        self.preview_border_color = config.tiling.preview_border_color;
        self.preview_border_width = config.tiling.preview_border_width;
        self.grid_snap = config.grid.enable_snap;
        self.snap_threshold = config.grid.snap_threshold;
        self.quarter_snap_threshold = config.grid.quarter_snap_threshold;
        self.enable_snap_off = config.grid.enable_snap_off;
        self.snap_off_threshold = config.grid.snap_off_threshold;
        self.join_views = config.grid.join_views;
        self.gestures.set_config(config.extra_gestures.clone());
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
        if let Some(window) = &self.window {
          window.request_redraw();
        }
      }
      Err(error) => eprintln!("shingle: config reload failed: {error}"),
    }
  }

  fn reload_if_changed(&mut self) -> bool {
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

  fn update_modifiers(&mut self) {
    let mut depressed = 0;
    if self.modifiers.shift_key() {
      depressed |= 1;
    }
    if self.modifiers.control_key() {
      depressed |= 4;
    }
    if self.modifiers.alt_key() {
      depressed |= 8;
    }
    if self.modifiers.super_key() {
      depressed |= 64;
    }
    self.compositor.keyboard_modifiers(
      self.devices.keyboard,
      Modifiers {
        depressed,
        latched: 0,
        locked: self.compositor.locked_modifiers(),
        group: 0,
      },
    );
  }

  fn config_modifiers(&self) -> ConfigModifiers {
    ConfigModifiers {
      super_key: self.modifiers.super_key(),
      control: self.modifiers.control_key(),
      alt: self.modifiers.alt_key(),
      shift: self.modifiers.shift_key(),
    }
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

  fn window_geometry(
    &self,
    window: TileWindowId,
  ) -> Option<shingle_core::Rect> {
    self
      .compositor
      .core()
      .window_layout(window)
      .map(|layout| layout.geometry)
      .ok()
      .or_else(|| {
        self
          .compositor
          .core()
          .window_info(window)
          .ok()
          .and_then(|info| info.state.floating)
      })
  }

  fn drop_direction(&self, target: TileWindowId) -> Direction {
    let Some(geometry) = self.window_geometry(target) else {
      return Direction::Right;
    };
    let center_x = geometry.x as f64 + geometry.width as f64 / 2.0;
    let center_y = geometry.y as f64 + geometry.height as f64 / 2.0;
    let dx = (self.pointer_x - center_x) / geometry.width.max(1) as f64;
    let dy = (self.pointer_y - center_y) / geometry.height.max(1) as f64;
    if dx.abs() > dy.abs() {
      if dx < 0.0 {
        Direction::Left
      } else {
        Direction::Right
      }
    } else if dy < 0.0 {
      Direction::Up
    } else {
      Direction::Down
    }
  }

  fn rotate_pointer(&mut self, x: f64, y: f64) {
    let Some(mut drag) = self.rotation_drag else {
      return;
    };
    let Some(geometry) = self.window_geometry(drag.window) else {
      self.rotation_drag = None;
      return;
    };
    let center_x = f64::from(geometry.x) + f64::from(geometry.width) / 2.0;
    let center_y = f64::from(geometry.y) + f64::from(geometry.height) / 2.0;
    let old = (drag.last_x - center_x, drag.last_y - center_y);
    let new = (x - center_x, y - center_y);
    let old_length = old.0.hypot(old.1);
    let new_length = new.0.hypot(new.1);
    if new_length <= self.wrot_reset_radius {
      self.rotations.remove(&drag.window);
    } else if old_length > 0.0 && new_length > 0.0 {
      let cross = old.0 * new.1 - new.0 * old.1;
      let delta = (cross / (old_length * new_length)).clamp(-1.0, 1.0).asin();
      let angle =
        self.rotations.get(&drag.window).copied().unwrap_or(0.0) - delta as f32;
      self.rotations.insert(drag.window, angle);
    }
    drag.last_x = x;
    drag.last_y = y;
    self.rotation_drag = Some(drag);
    if let Some(window) = &self.window {
      window.request_redraw();
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

  fn update_workspace_switch_timer(&mut self) {
    let direction = if self.workspace_switch_after.is_some()
      && matches!(self.drag, Some(DragState::Move { .. }))
    {
      self.compositor.workspace_drag_direction_at(
        self.pointer_x,
        self.pointer_y,
        self.snap_threshold,
        self.quarter_snap_threshold,
      )
    } else {
      None
    };
    match direction {
      Some((dx, dy)) => {
        if self
          .workspace_switch_started
          .is_none_or(|(old_dx, old_dy, _)| (old_dx, old_dy) != (dx, dy))
        {
          self.workspace_switch_started = Some((dx, dy, Instant::now()));
        }
      }
      None => self.workspace_switch_started = None,
    }
  }

  fn process_workspace_switch_timeout(&mut self) {
    let Some((dx, dy, started)) = self.workspace_switch_started else {
      return;
    };
    let Some(delay) = self.workspace_switch_after else {
      return;
    };
    if started.elapsed() < delay {
      return;
    }
    self.workspace_switch_started = None;
    let Some(source) = (match self.drag {
      Some(DragState::Move { source, .. }) => Some(source),
      _ => None,
    }) else {
      return;
    };
    let result = (|| {
      let current = self.compositor.current_workspace_index()?;
      let (width, height) = self.compositor.workspace_grid_dimensions();
      let x = current as i64 % i64::from(width) + i64::from(dx);
      let y = current as i64 / i64::from(width) + i64::from(dy);
      if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
        return Ok(());
      }
      let target = (y * i64::from(width) + x) as usize;
      self.compositor.move_window_to_workspace(source, target)?;
      self.compositor.switch_workspace(target)
    })();
    if let Err(error) = result {
      eprintln!("shingle: drag workspace switch failed: {error}");
    }
    self.save();
    self.update_drop_preview(source);
  }

  fn update_drop_preview(&mut self, source: TileWindowId) {
    self.update_workspace_switch_timer();
    self.drop_preview = None;
    self.drop_slot = None;
    if self.grid_snap
      && let Some(slot) = self.compositor.grid_slot_at(
        self.pointer_x,
        self.pointer_y,
        self.snap_threshold,
        self.quarter_snap_threshold,
      )
    {
      self.drop_slot = Some(slot);
      if let Some(geometry) = self.compositor.snap_geometry(slot) {
        self.drop_preview = Some(RenderRect {
          x: geometry.x,
          y: geometry.y,
          width: geometry.width,
          height: geometry.height,
        });
      }
      return;
    }
    if self
      .compositor
      .core()
      .window_info(source)
      .is_ok_and(|info| info.state.floating.is_some())
    {
      return;
    }
    let Some(target) = self.compositor.focus_at(self.pointer_x, self.pointer_y)
    else {
      return;
    };
    if target == source {
      return;
    }
    let direction = match self.drop_direction(target) {
      Direction::Left => PreviewDirection::Left,
      Direction::Right => PreviewDirection::Right,
      Direction::Up => PreviewDirection::Up,
      Direction::Down => PreviewDirection::Down,
    };
    if let Some(geometry) = self.window_geometry(target) {
      self.drop_preview = Some(split_preview(
        RenderRect {
          x: geometry.x,
          y: geometry.y,
          width: geometry.width,
          height: geometry.height,
        },
        direction,
      ));
    }
  }

  fn move_drag(&mut self, x: f64, y: f64) {
    let (source, changed) = {
      let Some(DragState::Move {
        source,
        last_x,
        last_y,
        snap_off_origin,
      }) = &mut self.drag
      else {
        return;
      };
      let mut released = false;
      let (dx, dy) = if let Some((origin_x, origin_y)) = *snap_off_origin {
        if (x - origin_x).hypot(y - origin_y)
          >= f64::from(self.snap_off_threshold)
        {
          *snap_off_origin = None;
          released = true;
          ((x - origin_x).round() as i32, (y - origin_y).round() as i32)
        } else {
          (0, 0)
        }
      } else {
        ((x - *last_x).round() as i32, (y - *last_y).round() as i32)
      };
      if released && let Err(error) = self.compositor.restore_snapped(*source) {
        eprintln!("shingle: snap-off failed: {error}");
      }
      let moved = if snap_off_origin.is_some() {
        false
      } else {
        match self.compositor.move_floating_group(
          *source,
          dx,
          dy,
          self.join_views,
        ) {
          Ok(changed) => changed,
          Err(error) => {
            eprintln!("shingle: pointer move failed: {error}");
            false
          }
        }
      };
      *last_x = x;
      *last_y = y;
      (*source, released || moved)
    };
    self.update_drop_preview(source);
    if changed {
      self.save();
    }
  }

  fn resize_drag(&mut self, x: f64, y: f64) {
    let Some(DragState::Resize {
      window,
      horizontal,
      vertical,
      last_x,
      last_y,
      aspect,
    }) = &mut self.drag
    else {
      return;
    };
    if let Some((start_x, start_y, original)) = *aspect {
      let dx = (x - start_x).round() as i32;
      let dy = (y - start_y).round() as i32;
      if let Err(error) = self.compositor.resize_floating_from(
        *window,
        *horizontal,
        *vertical,
        original,
        dx,
        dy,
      ) {
        eprintln!("shingle: aspect-preserving resize failed: {error}");
      }
    } else {
      let dx = (x - *last_x).round() as i32;
      let dy = (y - *last_y).round() as i32;
      for (direction, delta) in [(*horizontal, dx), (*vertical, dy)] {
        if let Some(direction) = direction
          && delta != 0
          && let Err(error) = self.compositor.resize_window(
            *window,
            direction,
            delta,
            self.minimum_extent,
          )
        {
          eprintln!("shingle: pointer resize failed: {error}");
        }
      }
    }
    *last_x = x;
    *last_y = y;
    self.save();
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
    key: &winit::keyboard::Key,
    pressed: bool,
    repeat: bool,
  ) -> bool {
    let Some(symbol) = xkb_symbol(key) else {
      return false;
    };
    self.run_matching_command_with(pressed, repeat, |binding| {
      binding.matches_xkb(modifiers, &symbol)
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

  fn start_switcher(
    &mut self,
    reverse: bool,
    binding: shingle_config::Binding,
    kind: SwitcherKind,
  ) {
    if self.switcher.is_none() {
      self.compositor.set_show_desktop(false);
      let mut windows = self.compositor.switchable_windows();
      windows.retain(|window| {
        self
          .compositor
          .core()
          .window_info(*window)
          .is_ok_and(|info| !info.state.minimized)
      });
      if windows.is_empty() {
        return;
      }
      let selected = self
        .compositor
        .focused_window()
        .and_then(|focused| {
          windows.iter().position(|window| *window == focused)
        })
        .unwrap_or(0);
      let original_opacity = windows
        .iter()
        .filter_map(|window| {
          self
            .compositor
            .window_opacity(*window)
            .map(|opacity| (*window, opacity))
        })
        .collect();
      self.switcher = Some(SwitcherState {
        kind,
        windows,
        selected,
        modifiers: binding_modifiers(&binding),
        original_opacity,
      });
    }
    self.advance_switcher(reverse);
    if self
      .switcher
      .as_ref()
      .is_some_and(|state| modifiers_empty(state.modifiers))
    {
      self.finish_switcher();
    }
  }

  fn advance_switcher(&mut self, reverse: bool) {
    let Some(state) = &mut self.switcher else {
      return;
    };
    if state.windows.is_empty() {
      self.switcher = None;
      return;
    }
    state.selected = if reverse {
      (state.selected + state.windows.len() - 1) % state.windows.len()
    } else {
      (state.selected + 1) % state.windows.len()
    };
    self.apply_switcher();
  }

  fn apply_switcher(&mut self) {
    let Some(state) = &self.switcher else {
      return;
    };
    let selected = state.windows.get(state.selected).copied();
    let updates = state
      .original_opacity
      .iter()
      .map(|(window, opacity)| (*window, *opacity))
      .collect::<Vec<_>>();
    for (window, original) in updates {
      self.compositor.set_window_opacity(
        window,
        if Some(window) == selected || state.kind == SwitcherKind::Regular {
          original
        } else {
          original * self.switcher_inactive_alpha
        },
      );
    }
    if let Some(selected) = selected
      && let Err(error) = self.compositor.raise_window(selected)
    {
      eprintln!("shingle: fast-switcher raise failed: {error}");
    }
    if let Some(window) = &self.window {
      window.request_redraw();
    }
  }

  fn finish_switcher(&mut self) {
    let Some(state) = self.switcher.take() else {
      return;
    };
    for (window, opacity) in state.original_opacity {
      self.compositor.set_window_opacity(window, opacity);
    }
    if let Some(window) = state.windows.get(state.selected).copied() {
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: fast-switcher focus failed: {error}");
      }
    }
    self.save();
    if let Some(window) = &self.window {
      window.request_redraw();
    }
  }

  fn switcher_window_destroyed(&mut self, window: TileWindowId) {
    let Some(state) = &mut self.switcher else {
      return;
    };
    state.original_opacity.remove(&window);
    if let Some(removed) = state
      .windows
      .iter()
      .position(|candidate| *candidate == window)
    {
      state.windows.remove(removed);
      if removed < state.selected {
        state.selected -= 1;
      }
    }
    if state.windows.is_empty() {
      self.finish_switcher();
      return;
    }
    state.selected = state.selected.min(state.windows.len() - 1);
    self.apply_switcher();
  }

  fn toggle_inversion(&mut self) {
    self.inverted = !self.inverted;
    self
      .compositor
      .notify_plugin_activation("invert", self.inverted);
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if changed && let Some(window) = &self.window {
      window.request_redraw();
    }
  }

  /// Left release commits either a cell click or a completed window drag.
  fn expo_pointer_input(&mut self, button: MouseButton, pressed: bool) -> bool {
    if self.expo.is_none() {
      return false;
    }
    if button == MouseButton::Left {
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
      if let Some(window) = &self.window {
        window.request_redraw();
      }
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
    if changed && let Some(window) = &self.window {
      window.request_redraw();
    }
    true
  }

  fn scale_pointer_input(
    &mut self,
    button: MouseButton,
    pressed: bool,
  ) -> bool {
    if self.scale.is_none() {
      return false;
    }
    if !pressed || !self.select_scale_at(self.pointer_x, self.pointer_y) {
      return true;
    }
    match button {
      MouseButton::Left => self.finish_scale(),
      MouseButton::Middle if self.scale_config.middle_click_close => {
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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
    if let Some(window) = &self.window {
      window.request_redraw();
    }
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

  fn keyboard_input(&mut self, event: winit::event::KeyEvent) {
    let pressed = event.state == ElementState::Pressed;
    let modifiers = self.config_modifiers();
    let key = binding_key_name(event.physical_key);
    let locked = self.compositor.is_locked();
    if self.expo_key(modifiers, &key, pressed, locked) {
      return;
    }
    if self.scale_key(modifiers, &key, pressed, locked) {
      self.scale_filter_input(&key, event.text.as_deref(), pressed);
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
          let reverse = previous.matches(modifiers, &key);
          if reverse || next.matches(modifiers, &key) {
            self.advance_switcher(reverse);
          }
        }
        return;
      }
    }
    if (locked || !pressed) && !self.compositor.shortcuts_inhibited() {
      if self.run_matching_symbol_command(
        modifiers,
        &event.logical_key,
        pressed,
        event.repeat,
      ) {
        return;
      }
      if self.run_matching_command(modifiers, &key, pressed, event.repeat) {
        return;
      }
    }
    if locked {
      if let Some(scancode) = event.physical_key.to_scancode() {
        self
          .compositor
          .keyboard_key(self.devices.keyboard, scancode, pressed);
      }
      return;
    }
    if pressed && self.keybindings.break_grab.matches(modifiers, &key) {
      self.compositor.break_shortcuts_inhibit();
      return;
    }
    if pressed && !self.compositor.shortcuts_inhibited() {
      if self.keybindings.wrot_reset.matches(modifiers, &key) {
        self.rotations.clear();
        self.rotation_drag = None;
        if let Some(window) = &self.window {
          window.request_redraw();
        }
        return;
      }
      if self.keybindings.wrot_reset_one.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          self.rotations.remove(&window);
        }
        self.rotation_drag = None;
        if let Some(window) = &self.window {
          window.request_redraw();
        }
        return;
      }
      if self.keybindings.invert_toggle.matches(modifiers, &key) {
        self.toggle_inversion();
        return;
      }
      let direction = if self.keybindings.focus_left.matches(modifiers, &key) {
        Some(Direction::Left)
      } else if self.keybindings.focus_right.matches(modifiers, &key) {
        Some(Direction::Right)
      } else if self.keybindings.focus_up.matches(modifiers, &key) {
        Some(Direction::Up)
      } else if self.keybindings.focus_down.matches(modifiers, &key) {
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
      if self.keybindings.expo_toggle.matches(modifiers, &key) {
        self.toggle_expo();
        return;
      }
      if self.keybindings.scale_toggle_all.matches(modifiers, &key) {
        self.toggle_scale(true);
        return;
      }
      if self.keybindings.scale_toggle.matches(modifiers, &key) {
        self.toggle_scale(false);
        return;
      }
      let fast_reverse = self
        .keybindings
        .fast_switcher_previous
        .matches(modifiers, &key);
      if fast_reverse
        || self.keybindings.fast_switcher_next.matches(modifiers, &key)
      {
        let binding = if fast_reverse {
          self.keybindings.fast_switcher_previous.clone()
        } else {
          self.keybindings.fast_switcher_next.clone()
        };
        self.start_switcher(fast_reverse, binding, SwitcherKind::Fast);
        return;
      }
      let reverse = self.keybindings.focus_previous.matches(modifiers, &key);
      if reverse || self.keybindings.focus_next.matches(modifiers, &key) {
        let binding = if reverse {
          self.keybindings.focus_previous.clone()
        } else {
          self.keybindings.focus_next.clone()
        };
        self.start_switcher(reverse, binding, SwitcherKind::Regular);
        return;
      }
      if self.keybindings.toggle_fullscreen.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_fullscreen(window) {
            eprintln!("shingle: fullscreen toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.toggle_sticky.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_sticky(window) {
            eprintln!("shingle: sticky toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.always_on_top.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_always_on_top(window) {
            eprintln!("shingle: always-on-top toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.always_on_bottom.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_always_on_bottom(window) {
            eprintln!("shingle: always-on-bottom toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.minimize.matches(modifiers, &key) {
        self.compositor.minimize_focused();
        self.save();
        return;
      }
      if self.keybindings.bring_to_front.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window()
          && let Err(error) = self.compositor.raise_window(window)
        {
          eprintln!("shingle: raise failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.send_to_back.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window()
          && let Err(error) = self.compositor.lower_window(window)
        {
          eprintln!("shingle: lower failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.show_desktop.matches(modifiers, &key) {
        self.compositor.toggle_show_desktop();
        self.save();
        return;
      }
      if self.keybindings.toggle_maximized.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_maximized(window) {
            eprintln!("shingle: maximize failed: {error}");
          }
          self.save();
        }
        return;
      }
      if self.keybindings.toggle_tiling.matches(modifiers, &key) {
        if let Some(window) = self.compositor.focused_window() {
          if let Err(error) = self.compositor.toggle_tiling(window) {
            eprintln!("shingle: tiling toggle failed: {error}");
          }
          self.save();
        }
        return;
      }
      let grid_slot = grid_slot(&self.keybindings, modifiers, &key);
      let grid_restore = self
        .keybindings
        .grid_restore
        .iter()
        .any(|binding| binding.matches(modifiers, &key));
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
      let move_direction =
        if self.keybindings.move_left.matches(modifiers, &key) {
          Some(Direction::Left)
        } else if self.keybindings.move_right.matches(modifiers, &key) {
          Some(Direction::Right)
        } else if self.keybindings.move_up.matches(modifiers, &key) {
          Some(Direction::Up)
        } else if self.keybindings.move_down.matches(modifiers, &key) {
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
        if self.keybindings.resize_left.matches(modifiers, &key) {
          Some(Direction::Left)
        } else if self.keybindings.resize_right.matches(modifiers, &key) {
          Some(Direction::Right)
        } else if self.keybindings.resize_up.matches(modifiers, &key) {
          Some(Direction::Up)
        } else if self.keybindings.resize_down.matches(modifiers, &key) {
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
      if self.keybindings.workspace_last.matches(modifiers, &key) {
        if let Err(error) = self.compositor.switch_to_last_workspace() {
          eprintln!("shingle: last workspace switch failed: {error}");
        }
        self.save();
        return;
      }
      if self
        .keybindings
        .workspace_move_last
        .matches(modifiers, &key)
      {
        if let Err(error) = self.compositor.move_focused_to_last_workspace() {
          eprintln!("shingle: move to last workspace failed: {error}");
        }
        self.save();
        return;
      }
      if self
        .keybindings
        .workspace_send_last
        .matches(modifiers, &key)
      {
        if let Err(error) = self.compositor.send_focused_to_last_workspace() {
          eprintln!("shingle: send to last workspace failed: {error}");
        }
        self.save();
        return;
      }
      if self
        .keybindings
        .workspace_send_next
        .matches(modifiers, &key)
        || self
          .keybindings
          .workspace_send_previous
          .matches(modifiers, &key)
        || self.keybindings.workspace_send_up.matches(modifiers, &key)
        || self
          .keybindings
          .workspace_send_down
          .matches(modifiers, &key)
      {
        let vertical =
          self.keybindings.workspace_send_up.matches(modifiers, &key)
            || self
              .keybindings
              .workspace_send_down
              .matches(modifiers, &key);
        let reverse = self
          .keybindings
          .workspace_send_previous
          .matches(modifiers, &key)
          || self.keybindings.workspace_send_up.matches(modifiers, &key);
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
      if self.keybindings.workspace_next.matches(modifiers, &key)
        || self.keybindings.workspace_previous.matches(modifiers, &key)
        || self.keybindings.workspace_up.matches(modifiers, &key)
        || self.keybindings.workspace_down.matches(modifiers, &key)
      {
        let vertical = self.keybindings.workspace_up.matches(modifiers, &key)
          || self.keybindings.workspace_down.matches(modifiers, &key);
        let reverse =
          self.keybindings.workspace_previous.matches(modifiers, &key)
            || self.keybindings.workspace_up.matches(modifiers, &key);
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
      if self
        .keybindings
        .workspace_move_next
        .matches(modifiers, &key)
        || self
          .keybindings
          .workspace_move_previous
          .matches(modifiers, &key)
        || self.keybindings.workspace_move_up.matches(modifiers, &key)
        || self
          .keybindings
          .workspace_move_down
          .matches(modifiers, &key)
      {
        let vertical =
          self.keybindings.workspace_move_up.matches(modifiers, &key)
            || self
              .keybindings
              .workspace_move_down
              .matches(modifiers, &key);
        let reverse = self
          .keybindings
          .workspace_move_previous
          .matches(modifiers, &key)
          || self.keybindings.workspace_move_up.matches(modifiers, &key);
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
        .position(|binding| binding.matches(modifiers, &key))
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
        .position(|binding| binding.matches(modifiers, &key))
      {
        if let Err(error) = self.compositor.move_focused_to_workspace(index) {
          eprintln!("shingle: workspace move failed: {error}");
        }
        self.save();
        return;
      }
      if self.keybindings.close.matches(modifiers, &key)
        || self
          .keybindings
          .close_alternatives
          .iter()
          .any(|binding| binding.matches(modifiers, &key))
      {
        self.compositor.close_focused();
        return;
      }
      if self.keybindings.reload.matches(modifiers, &key) {
        self.reload_config();
        return;
      }
      if self.keybindings.quit.matches(modifiers, &key) {
        self.quit = true;
        self.save();
        return;
      }
      if self.run_matching_symbol_command(
        modifiers,
        &event.logical_key,
        pressed,
        event.repeat,
      ) {
        return;
      }
      if self.run_matching_command(modifiers, &key, pressed, event.repeat) {
        return;
      }
      if self.keybindings.lock.matches(modifiers, &key) {
        self.compositor.request_lock();
        return;
      }
      if self.keybindings.idle_toggle.matches(modifiers, &key) {
        self.compositor.toggle_idle();
        return;
      }
    }
    if let Some(scancode) = event.physical_key.to_scancode() {
      self
        .compositor
        .keyboard_key(self.devices.keyboard, scancode, pressed);
    }
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
    renderer: &Renderer,
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
    renderer: &Renderer,
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
    renderer: &Renderer,
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

  fn render(&mut self) -> Result<(), Box<dyn Error>> {
    if self.renderer.is_none() {
      return Ok(());
    }
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
      let renderer = self.renderer.as_mut().expect("renderer presence checked");
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
        self.renderer.as_ref().expect("renderer presence checked"),
        &mut self.scale_icons,
        state,
        Duration::from_millis(self.scale_config.duration_ms),
        &icons,
        &mut tiles,
      )?;
      Self::append_scale_titles(
        self.renderer.as_ref().expect("renderer presence checked"),
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
        self.renderer.as_ref().expect("renderer presence checked"),
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
    let renderer = self.renderer.as_mut().expect("renderer presence checked");
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
    if (!self.animations.is_empty()
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
      || self.zoom_animation.is_some())
      && let Some(window) = &self.window
    {
      window.request_redraw();
    }
    self.dirty.clear();
    Ok(())
  }
}

/// Normalizes metadata and queries consistently for Scale's title filter.
fn normalize_filter_text(text: &str, case_sensitive: bool) -> String {
  let text = text
    .chars()
    .map(|character| {
      if character.is_whitespace() {
        ' '
      } else {
        character
      }
    })
    .collect::<String>();
  if case_sensitive {
    text
  } else {
    text.to_lowercase()
  }
}

/// Returns whether a title or app ID contains the active filter.
fn scale_filter_matches(
  snapshot: &SurfaceSnapshot,
  filter: &str,
  case_sensitive: bool,
) -> bool {
  if filter.is_empty() {
    return true;
  }
  let filter = normalize_filter_text(filter, case_sensitive);
  [snapshot.title.as_deref(), snapshot.app_id.as_deref()]
    .into_iter()
    .flatten()
    .any(|value| normalize_filter_text(value, case_sensitive).contains(&filter))
}

/// Returns the smallest square-like column count that can hold `count` items.
fn scale_columns(count: usize) -> usize {
  if count == 0 {
    0
  } else {
    (count as f64).sqrt().ceil() as usize
  }
}

/// Lays out row-major cells with output offsets, margins, and inter-cell gaps.
fn scale_cells(
  output: shingle_core::Rect,
  count: usize,
  spacing: u32,
  outer_margin: u32,
) -> Vec<RenderRect> {
  let columns = scale_columns(count);
  if columns == 0 {
    return Vec::new();
  }
  let rows = count.div_ceil(columns);
  let horizontal_spacing = spacing.saturating_mul((columns - 1) as u32);
  let vertical_spacing = spacing.saturating_mul((rows - 1) as u32);
  let available_width = output
    .width
    .saturating_sub(outer_margin.saturating_mul(2))
    .saturating_sub(horizontal_spacing);
  let available_height = output
    .height
    .saturating_sub(outer_margin.saturating_mul(2))
    .saturating_sub(vertical_spacing);
  let width = available_width / columns as u32;
  let height = available_height / rows as u32;
  (0..count)
    .map(|index| {
      let column = index % columns;
      let row = index / columns;
      let dx = outer_margin.saturating_add(
        (column as u32).saturating_mul(width.saturating_add(spacing)),
      );
      let dy = outer_margin.saturating_add(
        (row as u32).saturating_mul(height.saturating_add(spacing)),
      );
      RenderRect {
        x: output
          .x
          .saturating_add(i32::try_from(dx).unwrap_or(i32::MAX)),
        y: output
          .y
          .saturating_add(i32::try_from(dy).unwrap_or(i32::MAX)),
        width,
        height,
      }
    })
    .collect()
}

/// Interpolates renderer-only rectangles for a Scale transition.
fn interpolate_scale_rect(
  origin: RenderRect,
  target: RenderRect,
  progress: f32,
) -> RenderRect {
  let progress = f64::from(progress.clamp(0.0, 1.0));
  let interpolate = |start: f64, end: f64| start + (end - start) * progress;
  RenderRect {
    x: interpolate(f64::from(origin.x), f64::from(target.x)).round() as i32,
    y: interpolate(f64::from(origin.y), f64::from(target.y)).round() as i32,
    width: interpolate(f64::from(origin.width), f64::from(target.width)).round()
      as u32,
    height: interpolate(f64::from(origin.height), f64::from(target.height))
      .round() as u32,
  }
}

/// Captures the currently displayed thumbnail rectangles at a point in time.
fn scale_geometry_at(
  state: &ScaleState,
  now: Instant,
  duration: Duration,
) -> HashMap<TileWindowId, RenderRect> {
  let progress = animation_progress(state.animation_started, now, duration);
  state
    .windows
    .iter()
    .filter_map(|window| {
      let target = *state.geometry.get(window)?;
      let origin = state
        .animation_origin
        .get(window)
        .copied()
        .unwrap_or(target);
      Some((*window, interpolate_scale_rect(origin, target, progress)))
    })
    .collect()
}

/// Centers a thumbnail in its cell while preserving the original aspect ratio.
///
/// Upscaling is capped at the original size unless `allow_zoom` is enabled.
fn fit_scale_tile(
  cell: RenderRect,
  original: RenderRect,
  allow_zoom: bool,
) -> RenderRect {
  if original.width == 0
    || original.height == 0
    || cell.width == 0
    || cell.height == 0
  {
    return RenderRect {
      x: cell
        .x
        .saturating_add(i32::try_from(cell.width / 2).unwrap_or(i32::MAX)),
      y: cell
        .y
        .saturating_add(i32::try_from(cell.height / 2).unwrap_or(i32::MAX)),
      width: 0,
      height: 0,
    };
  }
  let mut factor = (f64::from(cell.width) / f64::from(original.width))
    .min(f64::from(cell.height) / f64::from(original.height));
  if !allow_zoom {
    factor = factor.min(1.0);
  }
  let width = ((f64::from(original.width) * factor).round() as u32)
    .max(1)
    .min(cell.width);
  let height = ((f64::from(original.height) * factor).round() as u32)
    .max(1)
    .min(cell.height);
  RenderRect {
    x: cell.x.saturating_add(
      i32::try_from((cell.width - width) / 2).unwrap_or(i32::MAX),
    ),
    y: cell.y.saturating_add(
      i32::try_from((cell.height - height) / 2).unwrap_or(i32::MAX),
    ),
    width,
    height,
  }
}

fn rect_contains(rect: RenderRect, x: f64, y: f64) -> bool {
  x >= f64::from(rect.x)
    && y >= f64::from(rect.y)
    && x < f64::from(rect.x) + f64::from(rect.width)
    && y < f64::from(rect.y) + f64::from(rect.height)
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

impl ApplicationHandler for App {
  fn resumed(&mut self, event_loop: &ActiveEventLoop) {
    if self.window.is_some() {
      return;
    }
    let attributes = WindowAttributes::default()
      .with_title("Shingle")
      .with_inner_size(PhysicalSize::new(1280, 720));
    let window = match event_loop.create_window(attributes) {
      Ok(window) => Arc::new(window),
      Err(error) => {
        eprintln!("shingle: failed to create nested window: {error}");
        event_loop.exit();
        return;
      }
    };
    let size = window.inner_size();
    let renderer = match pollster::block_on(Renderer::new(
      window.clone(),
      (size.width, size.height),
    )) {
      Ok(renderer) => renderer,
      Err(error) => {
        eprintln!("shingle: failed to initialize wgpu: {error}");
        event_loop.exit();
        return;
      }
    };
    let (width, height) =
      self.compositor.logical_output_size(size.width, size.height);
    if let Err(error) = self
      .compositor
      .set_output(shingle_core::Rect::new(0, 0, width, height))
    {
      eprintln!("shingle: failed to set output size: {error}");
      event_loop.exit();
      return;
    }
    self.renderer = Some(renderer);
    self.window = Some(window);
  }

  fn window_event(
    &mut self,
    event_loop: &ActiveEventLoop,
    window_id: WindowId,
    event: WindowEvent,
  ) {
    if self.window.as_ref().map(|window| window.id()) != Some(window_id) {
      return;
    }
    if let WindowEvent::ModifiersChanged(modifiers) = &event {
      // Track host modifiers even while the keyboard is disabled, so they
      // are correct again the moment it is re-enabled.
      self.modifiers = modifiers.state();
    }
    if window_event_device(&event).is_some_and(|kind| {
      !self.compositor.input_device_enabled(self.devices.get(kind))
    }) {
      return;
    }
    match event {
      WindowEvent::CloseRequested => {
        self.save();
        event_loop.exit();
      }
      WindowEvent::Resized(size) => {
        if let Some(renderer) = self.renderer.as_mut() {
          renderer.resize((size.width, size.height));
        }
        let (width, height) =
          self.compositor.logical_output_size(size.width, size.height);
        if let Err(error) = self
          .compositor
          .set_output(shingle_core::Rect::new(0, 0, width, height))
        {
          eprintln!("shingle: failed to resize output: {error}");
          event_loop.exit();
        }
        self.expo_exit = None;
        self.workspace_transition = None;
        self.update_scale_layout();
        self.update_expo_layout();
      }
      WindowEvent::RedrawRequested => {
        if let Err(error) = self.render() {
          eprintln!("shingle: render failed: {error}");
          event_loop.exit();
        }
      }
      WindowEvent::KeyboardInput { event, .. } => self.keyboard_input(event),
      WindowEvent::ModifiersChanged(_) => {
        if self.switcher.as_ref().is_some_and(|state| {
          !modifiers_held(state.modifiers, self.config_modifiers())
        }) {
          self.finish_switcher();
        }
        self.update_modifiers();
      }
      WindowEvent::CursorMoved { position, .. } => {
        if self.pending_warp.is_some_and(|pending| {
          (position.x - pending.0).abs() <= 1.0
            && (position.y - pending.1).abs() <= 1.0
        }) {
          self.pending_warp = None;
          return;
        }
        let overview_active = self.scale.is_some() || self.expo.is_some();
        if !overview_active && self.compositor.pointer_constrained() {
          return;
        }
        let position = if let Some(window) = &self.window {
          let host_scale = window.scale_factor();
          let size = window.inner_size();
          let (x, y) = self.compositor.map_output_point(
            position.x * host_scale,
            position.y * host_scale,
            size.width,
            size.height,
          );
          winit::dpi::PhysicalPosition::new(x, y)
        } else {
          position
        };
        let (x, y) = self.map_zoom_input(position.x, position.y);
        let position = winit::dpi::PhysicalPosition::new(x, y);
        self.pointer_x = position.x;
        self.pointer_y = position.y;
        if self.expo.is_some() {
          self.move_expo_pointer(position.x, position.y);
          return;
        }
        if self.scale.is_some() {
          self.select_scale_at(position.x, position.y);
          return;
        }
        if self.zoom_factor > 1.0 {
          self.zoom_origin = (position.x as f32, position.y as f32);
        }
        if self.rotation_drag.is_some() {
          self.rotate_pointer(position.x, position.y);
          return;
        }
        match self.drag {
          Some(DragState::Move { .. }) => {
            self.move_drag(position.x, position.y)
          }
          Some(DragState::Resize { .. }) => {
            self.resize_drag(position.x, position.y)
          }
          None => {}
        }
        if self.drag.is_none() {
          self.compositor.pointer_motion(
            self.devices.pointer,
            position.x,
            position.y,
          );
        }
      }
      WindowEvent::MouseInput { state, button, .. } => {
        let pressed = state == ElementState::Pressed;
        if self.expo_pointer_input(button, pressed) {
          return;
        }
        if self.scale_pointer_input(button, pressed) {
          return;
        }
        let (button, name) = match button {
          MouseButton::Left => (0x110, "btn_left"),
          MouseButton::Right => (0x111, "btn_right"),
          MouseButton::Middle => (0x112, "btn_middle"),
          MouseButton::Back => (0x113, "btn_back"),
          MouseButton::Forward => (0x114, "btn_forward"),
          MouseButton::Other(code) => (code as u32, "btn_other"),
        };
        let Some((button, name)) = self.mouse_buttons.translate(
          button,
          name,
          pressed,
          self.middle_emulation,
          self.left_handed_mode,
        ) else {
          return;
        };
        if !self.pointer_button_action(name, pressed) {
          self
            .compositor
            .pointer_button(self.devices.pointer, button, pressed);
          self.save();
        }
      }
      WindowEvent::Touch(mut touch) => {
        if let Some(window) = &self.window {
          let host_scale = window.scale_factor();
          let size = window.inner_size();
          (touch.location.x, touch.location.y) =
            self.compositor.map_output_point(
              touch.location.x * host_scale,
              touch.location.y * host_scale,
              size.width,
              size.height,
            );
        }
        let (x, y) = self.map_zoom_input(touch.location.x, touch.location.y);
        (touch.location.x, touch.location.y) = (x, y);
        if self.expo.is_some() {
          match touch.phase {
            TouchPhase::Started if self.expo_touch.is_none() => {
              self.expo_touch = Some(touch.id);
              self.pointer_x = touch.location.x;
              self.pointer_y = touch.location.y;
              self.expo_pointer_input(MouseButton::Left, true);
            }
            TouchPhase::Moved if self.expo_touch == Some(touch.id) => {
              self.move_expo_pointer(touch.location.x, touch.location.y);
            }
            TouchPhase::Ended if self.expo_touch == Some(touch.id) => {
              self.expo_pointer_input(MouseButton::Left, false);
            }
            TouchPhase::Cancelled if self.expo_touch == Some(touch.id) => {
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
          if touch.phase == TouchPhase::Started
            && self.select_scale_at(touch.location.x, touch.location.y)
          {
            self.finish_scale();
          }
          return;
        }
        let now = Instant::now();
        let action = match touch.phase {
          TouchPhase::Started => {
            self.compositor.touch_down(
              self.devices.touch,
              touch.id as i32,
              touch.location.x,
              touch.location.y,
            );
            self.gestures.touch_down(
              touch.id,
              touch.location.x,
              touch.location.y,
              now,
            );
            None
          }
          TouchPhase::Moved => {
            let action = self.gestures.touch_motion(
              touch.id,
              touch.location.x,
              touch.location.y,
              now,
            );
            if self.drag.is_some() {
              self.pointer_x = touch.location.x;
              self.pointer_y = touch.location.y;
              self.move_drag(touch.location.x, touch.location.y);
            } else {
              self.compositor.touch_motion(
                self.devices.touch,
                touch.id as i32,
                touch.location.x,
                touch.location.y,
              );
            }
            action
          }
          TouchPhase::Ended => {
            let action = self.gestures.touch_up(touch.id, now);
            if self.drag.is_some() {
              self.pointer_button_action("btn_left", false);
            }
            self
              .compositor
              .touch_up(self.devices.touch, touch.id as i32);
            action
          }
          TouchPhase::Cancelled => {
            self.gestures.cancel();
            if self.drag.is_some() {
              self.pointer_button_action("btn_left", false);
            }
            self
              .compositor
              .touch_cancel(self.devices.touch, touch.id as i32);
            None
          }
        };
        if let Some(action) = action {
          self.handle_gesture_action(action);
        }
      }
      WindowEvent::MouseWheel { delta, .. } => {
        if self.expo.is_some() {
          return;
        }
        if self.scale.is_some() {
          let vertical = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y),
            MouseScrollDelta::PixelDelta(position) => position.y,
          };
          if vertical != 0.0 {
            self.select_scale_relative(vertical > 0.0);
          }
          return;
        }
        let (horizontal, vertical, horizontal_steps, vertical_steps) =
          match delta {
            MouseScrollDelta::LineDelta(x, y) => {
              (x as f64 * 10.0, y as f64 * 10.0, x as i32, y as i32)
            }
            MouseScrollDelta::PixelDelta(position) => {
              (position.x, position.y, 0, 0)
            }
          };
        let shortcuts_allowed = !self.compositor.is_locked()
          && !self.compositor.shortcuts_inhibited();
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
          if let Some(window) = &self.window {
            window.request_redraw();
          }
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
          if let Some(window) = &self.window {
            window.request_redraw();
          }
          return;
        }
        self.compositor.pointer_axis_detailed(
          self.devices.pointer,
          horizontal,
          vertical,
          horizontal_steps,
          vertical_steps,
        );
      }
      _ => {}
    }
  }

  fn device_event(
    &mut self,
    _event_loop: &ActiveEventLoop,
    _device_id: DeviceId,
    event: DeviceEvent,
  ) {
    if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
      && self.compositor.input_device_enabled(self.devices.pointer)
      && self.compositor.pointer_constrained()
    {
      let (dx, dy) = if let Some(window) = &self.window {
        let host_scale = window.scale_factor();
        let size = window.inner_size();
        self.compositor.map_output_vector(
          dx * host_scale,
          dy * host_scale,
          size.width,
          size.height,
        )
      } else {
        (dx, dy)
      };
      let pointer = self.devices.pointer;
      self
        .compositor
        .pointer_relative_motion(pointer, dx, dy, dx, dy);
    }
  }

  fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    if self.quit {
      event_loop.exit();
      return;
    }
    if let Err(error) = self.compositor.dispatch_pending() {
      eprintln!("shingle: dispatch failed: {error}");
      event_loop.exit();
      return;
    }
    // SIGINT and SIGTERM reach the compositor's event loop, not winit's.
    // `exiting` saves the session.
    if self.compositor.shutdown_requested() {
      event_loop.exit();
      return;
    }
    self.reload_if_changed();
    self.process_workspace_switch_timeout();
    self.process_extra_gestures();
    self
      .session_children
      .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
    let changed = self.process_events();
    if (changed || self.compositor.needs_frame())
      && let Some(window) = &self.window
    {
      window.request_redraw();
    }
    event_loop.set_control_flow(ControlFlow::WaitUntil(
      Instant::now() + Duration::from_millis(16),
    ));
  }

  fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
    self.save();
  }
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

fn grid_slot(
  bindings: &Keybindings,
  modifiers: ConfigModifiers,
  key: &str,
) -> Option<&'static str> {
  for (slot, bindings) in [
    ("top_left", &bindings.grid_slot_top_left),
    ("top", &bindings.grid_slot_top),
    ("top_right", &bindings.grid_slot_top_right),
    ("left", &bindings.grid_slot_left),
    ("center", &bindings.grid_slot_center),
    ("right", &bindings.grid_slot_right),
    ("bottom_left", &bindings.grid_slot_bottom_left),
    ("bottom", &bindings.grid_slot_bottom),
    ("bottom_right", &bindings.grid_slot_bottom_right),
  ] {
    if bindings
      .iter()
      .any(|binding| binding.matches(modifiers, key))
    {
      return Some(slot);
    }
  }
  None
}

fn binding_modifiers(binding: &shingle_config::Binding) -> ConfigModifiers {
  ConfigModifiers {
    super_key: binding.modifiers.contains(&shingle_config::Modifier::Super),
    control: binding
      .modifiers
      .contains(&shingle_config::Modifier::Control),
    alt: binding.modifiers.contains(&shingle_config::Modifier::Alt),
    shift: binding.modifiers.contains(&shingle_config::Modifier::Shift),
  }
}

fn modifiers_empty(modifiers: ConfigModifiers) -> bool {
  modifiers == ConfigModifiers::default()
}

fn modifiers_held(expected: ConfigModifiers, actual: ConfigModifiers) -> bool {
  (!expected.super_key || actual.super_key)
    && (!expected.control || actual.control)
    && (!expected.alt || actual.alt)
    && (!expected.shift || actual.shift)
}

fn modifier_matches(text: &str, actual: ConfigModifiers) -> bool {
  let normalized = text.replace("><", "> <").replace(['<', '>'], "");
  let mut expected = ConfigModifiers::default();
  let mut found = false;
  for modifier in normalized.split_whitespace() {
    for modifier in modifier.split(['+', '|']) {
      match modifier {
        "super" | "mod4" | "logo" => expected.super_key = true,
        "ctrl" | "control" => expected.control = true,
        "alt" | "mod1" => expected.alt = true,
        "shift" => expected.shift = true,
        _ => return false,
      }
      found = true;
    }
  }
  found && actual == expected
}

/// Converts winit logical keys to XKB keysym names for symbol bindings.
fn xkb_symbol(key: &winit::keyboard::Key) -> Option<String> {
  use winit::keyboard::{Key, NamedKey};
  match key {
    Key::Character(text) if text == " " => Some("space".into()),
    Key::Character(text) if !text.is_empty() => Some(text.to_string()),
    Key::Named(key) => {
      let name = match key {
        NamedKey::Enter => "Return",
        NamedKey::Escape => "Escape",
        NamedKey::Backspace => "BackSpace",
        NamedKey::Tab => "Tab",
        NamedKey::Space => "space",
        NamedKey::Delete => "Delete",
        NamedKey::Insert => "Insert",
        NamedKey::Home => "Home",
        NamedKey::End => "End",
        NamedKey::PageUp => "Prior",
        NamedKey::PageDown => "Next",
        NamedKey::ArrowLeft => "Left",
        NamedKey::ArrowRight => "Right",
        NamedKey::ArrowUp => "Up",
        NamedKey::ArrowDown => "Down",
        NamedKey::CapsLock => "Caps_Lock",
        _ => return None,
      };
      Some(name.into())
    }
    _ => None,
  }
}

/// Converts winit physical keys to the names used by Wayfire and our config.
/// Returns the seat device a host window event comes from, if it is input.
fn window_event_device(event: &WindowEvent) -> Option<InputDeviceKind> {
  match event {
    WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) => {
      Some(InputDeviceKind::Keyboard)
    }
    WindowEvent::CursorMoved { .. }
    | WindowEvent::MouseInput { .. }
    | WindowEvent::MouseWheel { .. }
    | WindowEvent::PinchGesture { .. }
    | WindowEvent::PanGesture { .. }
    | WindowEvent::DoubleTapGesture { .. }
    | WindowEvent::RotationGesture { .. } => Some(InputDeviceKind::Pointer),
    WindowEvent::Touch(_) => Some(InputDeviceKind::Touch),
    _ => None,
  }
}

fn binding_key_name(key: winit::keyboard::PhysicalKey) -> String {
  use winit::keyboard::KeyCode;
  let winit::keyboard::PhysicalKey::Code(code) = key else {
    return String::new();
  };
  let name = match code {
    KeyCode::Backquote => "grave".to_owned(),
    KeyCode::ArrowLeft => "left".to_owned(),
    KeyCode::ArrowRight => "right".to_owned(),
    KeyCode::ArrowUp => "up".to_owned(),
    KeyCode::ArrowDown => "down".to_owned(),
    KeyCode::Numpad0 => "kp0".to_owned(),
    KeyCode::Numpad1 => "kp1".to_owned(),
    KeyCode::Numpad2 => "kp2".to_owned(),
    KeyCode::Numpad3 => "kp3".to_owned(),
    KeyCode::Numpad4 => "kp4".to_owned(),
    KeyCode::Numpad5 => "kp5".to_owned(),
    KeyCode::Numpad6 => "kp6".to_owned(),
    KeyCode::Numpad7 => "kp7".to_owned(),
    KeyCode::Numpad8 => "kp8".to_owned(),
    KeyCode::Numpad9 => "kp9".to_owned(),
    KeyCode::NumpadAdd => "kpplus".to_owned(),
    KeyCode::NumpadSubtract => "kpminus".to_owned(),
    KeyCode::NumpadMultiply => "kpasterisk".to_owned(),
    KeyCode::NumpadDivide => "kpslash".to_owned(),
    KeyCode::NumpadDecimal => "kpdot".to_owned(),
    KeyCode::NumpadComma => "kpcomma".to_owned(),
    KeyCode::NumpadEnter => "kpenter".to_owned(),
    KeyCode::NumpadEqual => "kpequal".to_owned(),
    _ => format!("{code:?}").to_lowercase(),
  };
  name
    .strip_prefix("key")
    .filter(|suffix| suffix.chars().count() == 1)
    .unwrap_or(&name)
    .to_owned()
}

fn main() -> Result<(), Box<dyn Error>> {
  let mut args = std::env::args_os().skip(1);
  let mut config_path = None;
  let mut ipc_path = None;
  let mut spawn = Vec::new();
  while let Some(argument) = args.next() {
    match argument.to_str() {
      Some("--config") => {
        let Some(path) = args.next() else {
          return Err("--config needs a path".into());
        };
        config_path = Some(std::path::PathBuf::from(path));
      }
      Some("--ipc") => {
        let Some(path) = args.next() else {
          return Err("--ipc needs a path".into());
        };
        ipc_path = Some(std::path::PathBuf::from(path));
      }
      Some("--spawn") => {
        spawn.extend(args);
        if spawn.is_empty() {
          return Err("--spawn needs a program".into());
        }
        break;
      }
      _ => {
        eprintln!(
          "usage: shingle-nested [--config PATH] [--ipc PATH] \
           [--spawn PROGRAM [ARGS...]]"
        );
        return Err("invalid arguments".into());
      }
    }
  }
  let config = ShingleConfig::load(config_path.as_deref())?;
  let event_loop = EventLoop::new()?;
  let mut app = App::new(config, spawn, config_path)?;
  if let Some(path) = ipc_path {
    app.compositor.enable_ipc(&path)?;
    unsafe {
      std::env::set_var("WAYFIRE_SOCKET", &path);
    }
  }
  event_loop.run_app(&mut app)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
  };

  use super::*;

  /// The `input/list-devices` reply of `shingle-nested` before the device
  /// registry, recorded byte for byte from a real run of `master` at
  /// 7189b50.
  const NESTED_LIST_DEVICES: &str = concat!(
    r#"[{"enabled":true,"id":1,"name":"Shingle nested keyboard","#,
    r#""product":"unknown","type":"keyboard","vendor":"unknown"},"#,
    r#"{"enabled":true,"id":2,"name":"Shingle nested pointer","#,
    r#""product":"unknown","type":"pointer","vendor":"unknown"},"#,
    r#"{"enabled":true,"id":3,"name":"Shingle nested touch","#,
    r#""product":"unknown","type":"touch","vendor":"unknown"}]"#,
  );

  /// A compositor with its Wayland and IPC sockets in the temporary
  /// directory, set up the way `App::new` sets up the nested one.
  struct IpcFixture {
    compositor: Compositor,
    devices: SeatDevices,
    ipc: PathBuf,
  }

  impl IpcFixture {
    fn new() -> Self {
      static NEXT: AtomicU32 = AtomicU32::new(0);
      let name = format!(
        "shingle-nested-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
      );
      let base = std::env::temp_dir().join(name);
      let mut compositor =
        Compositor::with_socket_path(Config::default(), &base)
          .expect("the temporary directory accepts a Wayland socket");
      let devices = SeatDevices::register(&mut compositor);
      let ipc = base.with_extension("ipc");
      compositor
        .enable_ipc(&ipc)
        .expect("the temporary directory accepts an IPC socket");
      Self {
        compositor,
        devices,
        ipc,
      }
    }

    /// Sends one IPC request and returns the raw reply.
    fn request(&mut self, request: &str) -> String {
      let mut stream =
        UnixStream::connect(&self.ipc).expect("the IPC socket accepts");
      let length = u32::try_from(request.len()).expect("a short request");
      stream
        .write_all(&length.to_ne_bytes())
        .and_then(|()| stream.write_all(request.as_bytes()))
        .expect("the request is sent");
      stream
        .set_nonblocking(true)
        .expect("the stream is nonblocking");
      let mut reply = Vec::new();
      let deadline = Instant::now() + Duration::from_secs(2);
      while Instant::now() < deadline {
        self
          .compositor
          .poll(Some(Duration::from_millis(10)))
          .expect("dispatch succeeds");
        let mut buffer = [0; 4096];
        match stream.read(&mut buffer) {
          Ok(read) => reply.extend_from_slice(&buffer[..read]),
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
          Err(error) => panic!("reading the reply failed: {error}"),
        }
        if reply.len() >= 4 {
          let length = u32::from_ne_bytes(reply[..4].try_into().unwrap());
          if reply.len() >= 4 + length as usize {
            return String::from_utf8(reply[4..].to_vec())
              .expect("the reply is UTF-8");
          }
        }
      }
      panic!("no IPC reply to {request}");
    }
  }

  #[test]
  fn nested_list_devices_reply_is_unchanged() {
    let mut fixture = IpcFixture::new();
    let reply = fixture.request(r#"{"method":"input/list-devices","data":{}}"#);
    assert_eq!(reply, NESTED_LIST_DEVICES);
  }

  #[test]
  fn seat_devices_get_ids_one_to_three_and_map_back_to_their_kind() {
    let fixture = IpcFixture::new();
    let devices = fixture.devices;
    let kinds = [
      InputDeviceKind::Keyboard,
      InputDeviceKind::Pointer,
      InputDeviceKind::Touch,
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
      let id = devices.get(kind);
      assert_eq!(id.get() as usize, index + 1);
      assert_eq!(devices.kind(id), Some(kind));
      let info = fixture.compositor.input_device(id).expect("registered");
      assert_eq!(info.kind, kind);
      assert!(fixture.compositor.input_device_enabled(id));
    }
  }

  #[test]
  fn configure_device_events_carry_the_nested_device_id() {
    let mut fixture = IpcFixture::new();
    fixture.request(
      r#"{"method":"input/configure-device","data":{"id":3,"enabled":false}}"#,
    );
    let changes: Vec<_> = fixture
      .compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::InputDeviceChanged { id, enabled } => Some((id, enabled)),
        _ => None,
      })
      .collect();
    assert_eq!(changes, [(fixture.devices.touch, false)]);
    assert_eq!(
      fixture.devices.kind(fixture.devices.touch),
      Some(InputDeviceKind::Touch)
    );
  }

  #[test]
  fn nested_configure_device_replies_are_unchanged() {
    let mut fixture = IpcFixture::new();
    let cases = [
      (r#"{"id":2,"enabled":false}"#, r#"{"result":"ok"}"#),
      (
        r#"{"id":4,"enabled":false}"#,
        r#"{"error":"Unknown input device!"}"#,
      ),
      (
        r#"{"id":0,"enabled":true}"#,
        r#"{"error":"Unknown input device!"}"#,
      ),
      (r#"{"enabled":true}"#, r#"{"error":"Missing \"id\""}"#),
    ];
    for (data, expected) in cases {
      let request =
        format!(r#"{{"method":"input/configure-device","data":{data}}}"#);
      assert_eq!(fixture.request(&request), expected, "{data}");
    }
    let reply = fixture.request(r#"{"method":"input/list-devices","data":{}}"#);
    assert_eq!(
      reply,
      NESTED_LIST_DEVICES.replacen(
        r#""enabled":true,"id":2"#,
        r#""enabled":false,"id":2"#,
        1
      )
    );
  }

  #[test]
  fn host_input_events_map_to_their_seat_device() {
    let device_id = DeviceId::dummy();
    let cases = [
      (
        WindowEvent::ModifiersChanged(Default::default()),
        Some(InputDeviceKind::Keyboard),
      ),
      (
        WindowEvent::CursorMoved {
          device_id,
          position: winit::dpi::PhysicalPosition::new(1.0, 2.0),
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::MouseInput {
          device_id,
          state: ElementState::Pressed,
          button: MouseButton::Left,
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::MouseWheel {
          device_id,
          delta: MouseScrollDelta::LineDelta(0.0, 1.0),
          phase: TouchPhase::Moved,
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::PinchGesture {
          device_id,
          delta: 0.1,
          phase: TouchPhase::Started,
        },
        Some(InputDeviceKind::Pointer),
      ),
      (
        WindowEvent::Touch(winit::event::Touch {
          device_id,
          phase: TouchPhase::Started,
          location: winit::dpi::PhysicalPosition::new(1.0, 2.0),
          force: None,
          id: 0,
        }),
        Some(InputDeviceKind::Touch),
      ),
      (WindowEvent::CloseRequested, None),
      (WindowEvent::Resized(PhysicalSize::new(10, 10)), None),
      (WindowEvent::Focused(true), None),
    ];
    for (event, device) in cases {
      assert_eq!(window_event_device(&event), device, "{event:?}");
    }
  }

  #[test]
  fn left_handed_swaps_real_buttons() {
    let mut buttons = MouseButtons::default();
    assert_eq!(
      buttons.translate(0x110, "btn_left", true, false, true),
      Some((0x110, "btn_right"))
    );
    assert_eq!(
      buttons.translate(0x110, "btn_left", false, false, true),
      Some((0x110, "btn_right"))
    );
    assert_eq!(
      buttons.translate(0x113, "btn_back", true, false, true),
      Some((0x113, "btn_back"))
    );
  }

  #[test]
  fn middle_emulation_balances_presses_and_releases() {
    let mut buttons = MouseButtons::default();
    assert_eq!(
      buttons.translate(0x110, "btn_left", true, true, false),
      Some((0x110, "btn_left"))
    );
    assert_eq!(
      buttons.translate(0x111, "btn_right", true, true, false),
      Some((0x112, "btn_middle"))
    );
    assert_eq!(
      buttons.translate(0x110, "btn_left", false, true, false),
      Some((0x112, "btn_middle"))
    );
    assert_eq!(
      buttons.translate(0x111, "btn_right", false, true, false),
      Some((0x111, "btn_right"))
    );
    assert!(!buttons.left && !buttons.right && !buttons.emulated_middle);
  }

  #[test]
  fn disabled_emulation_preserves_independent_buttons() {
    let mut buttons = MouseButtons::default();
    for (button, name) in [(0x110, "btn_left"), (0x111, "btn_right")] {
      assert_eq!(
        buttons.translate(button, name, true, false, false),
        Some((button, name))
      );
    }
    assert!(!buttons.emulated_middle);
  }

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

  #[test]
  fn scale_cells_respect_output_offsets_margins_and_spacing() {
    assert_eq!(
      scale_cells(shingle_core::Rect::new(100, 50, 800, 600), 1, 50, 20),
      vec![RenderRect {
        x: 120,
        y: 70,
        width: 760,
        height: 560,
      }]
    );
    assert_eq!(
      scale_cells(shingle_core::Rect::new(0, 0, 800, 600), 4, 20, 10),
      vec![
        RenderRect {
          x: 10,
          y: 10,
          width: 380,
          height: 280,
        },
        RenderRect {
          x: 410,
          y: 10,
          width: 380,
          height: 280,
        },
        RenderRect {
          x: 10,
          y: 310,
          width: 380,
          height: 280,
        },
        RenderRect {
          x: 410,
          y: 310,
          width: 380,
          height: 280,
        },
      ]
    );
  }

  #[test]
  fn scale_cells_handle_odd_counts() {
    let cells = scale_cells(shingle_core::Rect::new(0, 0, 900, 600), 5, 0, 0);
    assert_eq!(scale_columns(5), 3);
    assert_eq!(cells.len(), 5);
    assert!(
      cells
        .iter()
        .all(|cell| cell.width == 300 && cell.height == 300)
    );
    assert_eq!(cells[4].x, 300);
    assert_eq!(cells[4].y, 300);
    assert!(
      scale_cells(shingle_core::Rect::new(0, 0, 900, 600), 0, 0, 0).is_empty()
    );
  }

  #[test]
  fn scale_transition_interpolates_and_clamps_geometry() {
    let origin = RenderRect {
      x: 0,
      y: 0,
      width: 100,
      height: 100,
    };
    let target = RenderRect {
      x: 100,
      y: 50,
      width: 200,
      height: 50,
    };
    assert_eq!(interpolate_scale_rect(origin, target, 0.0), origin);
    assert_eq!(
      interpolate_scale_rect(origin, target, 0.5),
      RenderRect {
        x: 50,
        y: 25,
        width: 150,
        height: 75,
      }
    );
    assert_eq!(interpolate_scale_rect(origin, target, 1.0), target);
    assert_eq!(interpolate_scale_rect(origin, target, 2.0), target);
  }

  #[test]
  fn scale_tiles_preserve_aspect_and_control_upscaling() {
    let cell = RenderRect {
      x: 10,
      y: 20,
      width: 100,
      height: 100,
    };
    let original = RenderRect {
      x: 0,
      y: 0,
      width: 200,
      height: 100,
    };
    assert_eq!(
      fit_scale_tile(cell, original, false),
      RenderRect {
        x: 10,
        y: 45,
        width: 100,
        height: 50,
      }
    );

    let small = RenderRect {
      width: 40,
      height: 20,
      ..original
    };
    assert_eq!(
      fit_scale_tile(cell, small, false),
      RenderRect {
        x: 40,
        y: 60,
        width: 40,
        height: 20,
      }
    );
    assert_eq!(
      fit_scale_tile(cell, small, true),
      RenderRect {
        x: 10,
        y: 45,
        width: 100,
        height: 50,
      }
    );
  }

  #[test]
  fn physical_keys_match_wayfire_shortcut_names() {
    use winit::keyboard::{KeyCode, PhysicalKey};
    for (code, expected) in [
      (KeyCode::ArrowLeft, "left"),
      (KeyCode::ArrowRight, "right"),
      (KeyCode::ArrowUp, "up"),
      (KeyCode::ArrowDown, "down"),
      (KeyCode::Numpad0, "kp0"),
      (KeyCode::Numpad7, "kp7"),
      (KeyCode::NumpadEnter, "kpenter"),
      (KeyCode::NumpadAdd, "kpplus"),
      (KeyCode::NumpadDivide, "kpslash"),
      (KeyCode::Backquote, "grave"),
    ] {
      assert_eq!(binding_key_name(PhysicalKey::Code(code)), expected);
    }

    let keys = Keybindings::default();
    assert!(keys.workspace_next.matches(
      ConfigModifiers {
        super_key: true,
        alt: true,
        ..ConfigModifiers::default()
      },
      &binding_key_name(PhysicalKey::Code(KeyCode::ArrowRight)),
    ));
    assert!(keys.grid_slot_top_left[0].matches(
      ConfigModifiers {
        super_key: true,
        ..ConfigModifiers::default()
      },
      &binding_key_name(PhysicalKey::Code(KeyCode::Numpad7)),
    ));
  }

  #[test]
  fn xkb_symbol_bindings_match_logical_keys() {
    use winit::keyboard::{Key, NamedKey};
    assert_eq!(
      xkb_symbol(&Key::Character("A".into())).as_deref(),
      Some("A")
    );
    assert_eq!(
      xkb_symbol(&Key::Named(NamedKey::Enter)).as_deref(),
      Some("Return")
    );
    assert_eq!(
      xkb_symbol(&Key::Named(NamedKey::PageUp)).as_deref(),
      Some("Prior")
    );
    let binding = shingle_config::Binding::new(
      "xkb:A",
      vec![shingle_config::Modifier::Super],
    );
    assert!(binding.matches_xkb(
      ConfigModifiers {
        super_key: true,
        ..ConfigModifiers::default()
      },
      "A",
    ));
    assert!(!binding.matches_xkb(ConfigModifiers::default(), "A"));
    assert!(!binding.matches_xkb(
      ConfigModifiers {
        super_key: true,
        ..ConfigModifiers::default()
      },
      "a",
    ));
    assert!(!binding.matches(ConfigModifiers::default(), "a"));
  }

  #[test]
  fn scale_filter_matches_title_and_app_id() {
    let snapshot = SurfaceSnapshot {
      id: SurfaceId(1),
      window: None,
      parent: None,
      role: SurfaceRole::Toplevel,
      app_id: Some("org.example.Editor".into()),
      title: Some("Alpha\tEditor".into()),
      icon: None,
      geometry: shingle_core::Rect::new(0, 0, 100, 100),
      opacity: 1.0,
      buffer: None,
    };
    assert!(scale_filter_matches(&snapshot, "alpha editor", false));
    assert!(scale_filter_matches(&snapshot, "ORG.EXAMPLE", false));
    assert!(!scale_filter_matches(&snapshot, "alpha editor", true));
    assert!(!scale_filter_matches(&snapshot, "missing", false));
    assert_eq!(normalize_filter_text("Café\tEditor", false), "café editor");
  }
}
