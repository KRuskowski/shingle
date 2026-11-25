//! The frontend state machine: input dispatch, overviews, drags,
//! animations, the scene build and session handling.

mod animation;
mod bindings;
mod drag;
mod effects;
mod expo;
mod scale;
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
  InputDeviceKind, SurfaceId, SurfaceRole,
};
use shingle_config::{
  CommandBinding, ConfigWatcher, Keybindings, PointerBindings, ShingleConfig,
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuTexture, RenderRect, RenderTile, TextureTransform, animation_progress,
  preview_tiles,
};
use shingle_session::{SessionStore, launch_autostart, launch_saved};

use crate::{
  extra_gestures,
  host::{Gesture, Host, InputEvent, Touch, TouchPhase},
  icon_theme,
  keyboard::{KeyRepeat, Keyboard},
  render_target::RenderTarget,
  scale::{
    ScaleState, interpolate_scale_rect, rect_contains, scale_geometry_at,
  },
  scale_title, workspace_transition,
};

use animation::{ClosingAnimation, GeometryAnimation, apply_opening_animation};
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
                crate::expo::interpolate_rect(origin, *geometry, progress);
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
