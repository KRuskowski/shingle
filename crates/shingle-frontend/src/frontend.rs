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
  cell::RefCell,
  collections::{HashMap, HashSet},
  error::Error,
  process::Child,
  rc::Rc,
  time::{Duration, Instant},
};

use shingle::{
  BufferSnapshot, CaptureId, Compositor, Config, Direction, Event,
  InputDeviceId, InputDeviceKind, SurfaceId,
};
use shingle_config::{
  CommandBinding, ConfigWatcher, Keybindings, PointerBindings, ShingleConfig,
};
use shingle_core::WindowId as TileWindowId;
use shingle_render::{
  GpuContext, GpuTexture, RenderRect, TextureFilter, TextureUploader,
};
use shingle_session::{
  SessionStore, SessionWriter, launch_autostart, launch_program, launch_saved,
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
  /// Saves sessions off this thread; `None` once [`Frontend::shutdown`]
  /// has saved the session for the last time.
  session: Option<SessionWriter>,
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
  /// The last configuration a reload applied, until the host takes it.
  reloaded_config: Option<ShingleConfig>,
  config_reload_delay: Duration,
  last_config_poll: Instant,
  quit: bool,
  /// Whether the host should be asked for a frame at the end of the call.
  redraw_requested: bool,
  /// Screencopy captures the next frame answers, with their outputs and
  /// regions.
  captures: Vec<(CaptureId, shingle::OutputHandle, shingle_core::Rect)>,
  /// Imported DMA-BUF textures and the locks of frames in flight, once
  /// linux-dmabuf is offered.
  dmabuf: Option<Rc<RefCell<crate::dmabuf::DmabufTextures>>>,
  /// Uploads committed shm buffers, once the host attached its GPU
  /// ([`Frontend::attach_gpu`]).
  uploader: Option<TextureUploader>,
  /// Surfaces whose texture holds an uploaded shm buffer that was given
  /// back. These textures are kept while the surface is mapped, drawn or
  /// not, since the buffer cannot be read again.
  uploaded: HashSet<SurfaceId>,
  /// Every shm upload, with the bytes it copied, for tests.
  #[cfg(test)]
  uploads: Vec<(SurfaceId, usize)>,
  /// The clients [`Frontend::start_clients`] starts.
  startup: Option<Startup>,
}

/// What [`Frontend::new`] leaves for [`Frontend::start_clients`].
struct Startup {
  restored: Option<shingle_core::SessionSnapshot>,
  autostart: Vec<String>,
  spawn: Vec<std::ffi::OsString>,
}

/// The major number of a Linux `dev_t`, as `major(3)`.
fn dev_major(device: u64) -> u64 {
  ((device >> 32) & 0xffff_f000) | ((device >> 8) & 0xfff)
}

/// The minor number of a Linux `dev_t`, as `minor(3)`.
fn dev_minor(device: u64) -> u64 {
  ((device >> 12) & 0xffff_ff00) | (device & 0xff)
}

impl Frontend {
  /// Creates the frontend: restores the saved session's window model or
  /// creates a new compositor, and serves Wayfire IPC at `ipc` if given.
  /// No client is started yet: the host first enables linux-dmabuf once
  /// its renderer exists ([`Frontend::attach_gpu`]), since most
  /// clients look for the global only once, and then calls
  /// [`Frontend::start_clients`], which launches the restored session's
  /// clients, autostart and `spawn`.
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
    let restored = if config.session.restore {
      store.load()?
    } else {
      None
    };
    let (mut compositor, restored) = match restored {
      Some((core, snapshot)) => (
        Compositor::with_core(Config::from(config.clone()), core)?,
        Some(snapshot),
      ),
      None => (Compositor::new(Config::from(config.clone()))?, None),
    };
    if let Some(path) = ipc {
      compositor.enable_ipc(path)?;
    }
    let startup = Startup {
      restored,
      autostart: config.autostart.clone(),
      spawn,
    };
    let mut frontend = Self::with_compositor(
      config,
      compositor,
      store,
      Vec::new(),
      config_path,
    )?;
    frontend.startup = Some(startup);
    Ok(frontend)
  }

  /// Settles the restored session's workspace sets on the outputs the
  /// backend added ([`Compositor::restore_outputs`]) and starts the clients
  /// [`Frontend::new`] left for later: the restored session's, autostart
  /// and the `spawn` program, all in
  /// [`Compositor::client_environment`], and prints `WAYLAND_DISPLAY` and
  /// `DISPLAY`. Only the first call does anything.
  ///
  /// # Errors
  ///
  /// When the `spawn` program cannot be started. Failed session and
  /// autostart launches are logged instead.
  pub fn start_clients(&mut self) -> Result<(), Box<dyn Error>> {
    let Some(startup) = self.startup.take() else {
      return Ok(());
    };
    // The backend has added its outputs by now, so saved workspace sets
    // that none of them took are settled before clients connect.
    self.compositor.restore_outputs();
    let environment = self.compositor.client_environment();
    if let Some(snapshot) = &startup.restored {
      let report =
        launch_saved(snapshot, &environment, self.discard_command_output);
      for error in &report.failed {
        eprintln!("shingle: session restore launch failed: {error}");
      }
      self
        .session_children
        .extend(report.launched.into_iter().map(|launched| launched.child));
    }
    let autostart = launch_autostart(
      &startup.autostart,
      startup.restored.as_ref(),
      &environment,
      self.discard_command_output,
    );
    for error in &autostart.failed {
      eprintln!("shingle: {error}");
    }
    self.session_children.extend(autostart.children);
    println!(
      "WAYLAND_DISPLAY={}",
      self.compositor.socket_name().to_string_lossy()
    );
    if let Some(display) = self.compositor.x_display() {
      println!("DISPLAY={display}");
    }
    if let Some((program, args)) = startup.spawn.split_first() {
      let child = launch_program(program, args, &environment)?;
      self.session_children.push(child);
    }
    Ok(())
  }

  /// Gives the frontend the host's GPU, once its renderer exists and
  /// before [`Frontend::start_clients`], and again with the new context
  /// after a GPU loss. Committed `wl_shm` buffers are then uploaded from
  /// the clients' pools right after the dispatch that committed them and
  /// given back at once ([`shingle::Compositor::set_shm_upload`],
  /// docs/design/dmabuf.md, section 5), and linux-dmabuf is offered as
  /// [`Frontend::enable_dmabuf`] decides, whose answer this returns.
  pub fn attach_gpu(&mut self, gpu: &GpuContext) -> bool {
    self.uploader = Some(gpu.texture_uploader());
    self.compositor.set_shm_upload(true);
    self.enable_dmabuf(gpu)
  }

  /// Offers linux-dmabuf to clients if `gpu` can import DMA-BUFs, has a
  /// render node to advertise and `SHINGLE_DMABUF` is not `0`, and logs
  /// one line saying which (docs/design/dmabuf.md, section 9). With it,
  /// it offers linux-drm-syncobj too, unless `SHINGLE_SYNCOBJ` is `0` or
  /// the render node lacks explicit sync, and logs a second line. Returns
  /// whether linux-dmabuf is offered. Hosts call it once their renderer
  /// exists and before [`Frontend::start_clients`], and again with the
  /// new context after a GPU loss.
  pub fn enable_dmabuf(&mut self, gpu: &GpuContext) -> bool {
    let decision = crate::dmabuf::feedback(
      std::env::var_os("SHINGLE_DMABUF").as_deref(),
      gpu.dmabuf(),
    );
    let (feedback, importer) = match (decision, gpu.texture_importer()) {
      (Ok(feedback), Some(importer)) => (feedback, importer),
      (Err(reason), _) => {
        eprintln!("shingle: {reason}");
        return false;
      }
      (Ok(_), None) => {
        eprintln!("shingle: no linux-dmabuf: the device cannot import");
        return false;
      }
    };
    let pairs = feedback.formats.len();
    let device = feedback.main_device;
    match self.set_dmabuf_importer(feedback, Box::new(importer)) {
      Ok(()) => {
        eprintln!(
          "shingle: linux-dmabuf version 5 on render node {}:{}, {pairs} \
           format and modifier pairs",
          dev_major(device),
          dev_minor(device)
        );
        self.enable_syncobj();
        true
      }
      Err(error) => {
        eprintln!("shingle: no linux-dmabuf: {error}");
        false
      }
    }
  }

  /// Offers linux-drm-syncobj on linux-dmabuf's render node unless
  /// `SHINGLE_SYNCOBJ=0`, and logs whether it did.
  fn enable_syncobj(&mut self) {
    let allowed = crate::dmabuf::syncobj_allowed(
      std::env::var_os("SHINGLE_SYNCOBJ").as_deref(),
    );
    match allowed.map(|()| self.compositor.enable_syncobj()) {
      Err(reason) => eprintln!("shingle: {reason}"),
      Ok(Ok(line)) => eprintln!("shingle: {line}"),
      Ok(Err(error)) => {
        eprintln!("shingle: no linux-drm-syncobj, implicit sync only: {error}");
      }
    }
  }

  /// Offers linux-dmabuf with `feedback`, importing through `importer`
  /// into a texture cache the scene draws from.
  pub(crate) fn set_dmabuf_importer(
    &mut self,
    feedback: shingle::DmabufFeedback,
    importer: Box<dyn crate::dmabuf::Import>,
  ) -> Result<(), shingle::Error> {
    let cache =
      Rc::new(RefCell::new(crate::dmabuf::DmabufTextures::new(importer)));
    self.compositor.enable_dmabuf(
      feedback,
      Box::new(crate::dmabuf::CacheImporter(Rc::clone(&cache))),
    )?;
    self.dmabuf = Some(cache);
    self.request_redraw();
    Ok(())
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
    // And STIPC pointer input that of a real pointer, so drags see it.
    compositor.set_stipc_pointer_events(true);
    // Screencopy shows the frames drawn here, not a CPU composite.
    compositor.set_frame_capture(true);
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
      session: Some(SessionWriter::for_store(store)?),
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
      reloaded_config: None,
      config_reload_delay: Duration::from_millis(
        config.behavior.config_reload_delay,
      ),
      last_config_poll: Instant::now(),
      quit: false,
      redraw_requested: false,
      captures: Vec::new(),
      dmabuf: None,
      uploader: None,
      uploaded: HashSet::new(),
      #[cfg(test)]
      uploads: Vec::new(),
      startup: None,
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
        | Event::ShowDesktopChanged(_)
        | Event::OutputFocused { .. }
        | Event::LayoutChanged => save = true,
        Event::OutputChanged { .. }
        | Event::OutputAdded { .. }
        | Event::OutputRemoved { .. } => {
          self.workspace_transition = None;
          save = true;
        }
        Event::ToggleScale(all_workspaces) => {
          self.toggle_scale(all_workspaces);
        }
        Event::ToggleExpo => self.toggle_expo(),
        Event::InvertToggle => self.toggle_inversion(),
        // Only the first output is drawn until MO-5, so only its
        // workspace changes are animated.
        Event::WorkspaceChanged { output, .. }
          if output != self.compositor.primary_output() =>
        {
          save = true;
        }
        Event::WorkspaceChanged { workspace, .. } => {
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
          self.upload_committed(&snapshot);
        }
        Event::SurfaceDestroyed { surface } => {
          self.uploaded.remove(&surface);
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
        Event::PointerWarp { x, y, .. } => {
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
        Event::StipcPointerMotion { device, x, y } => self.dispatch_input(
          host,
          device,
          InputEvent::PointerMotionAbsolute { x, y, output: None },
        ),
        Event::StipcButton {
          device,
          code,
          pressed,
        } => self.dispatch_input(
          host,
          device,
          InputEvent::PointerButton { code, pressed },
        ),
        Event::DpmsChanged(_)
        | Event::OpacityChanged { .. }
        | Event::Idle
        | Event::Resume
        | Event::FocusChanged { .. }
        | Event::GeometryChanged { .. }
        | Event::LockStateChanged(_) => {}
        Event::CaptureRequested { id, output, region } => {
          self.captures.push((id, output, region));
        }
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
    // Buffers of finished frames are released by this turn's dispatch.
    self.retire_frames();
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
  /// size after the output scale, keeping its position. The host calls
  /// this once it knows its display size.
  pub fn set_output_size(
    &mut self,
    width: u32,
    height: u32,
  ) -> Result<(), shingle::Error> {
    let (width, height) = self.compositor.logical_output_size(width, height);
    let output = self.compositor.output();
    self
      .compositor
      .set_output(shingle_core::Rect::new(output.x, output.y, width, height))
  }

  /// Takes the configuration the last config reload applied, if one was
  /// reloaded since the last call. The native host follows the output
  /// modes in it.
  pub fn take_reloaded_config(&mut self) -> Option<ShingleConfig> {
    self.reloaded_config.take()
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

  /// Pauses or resumes the seat session, as the native host does when
  /// libseat disables the seat for a VT switch and enables it again.
  ///
  /// Pausing forgets every held key and stops key repeat, ends the
  /// switcher as if its modifiers were released, abandons drags, window
  /// rotation, Expo's pointer and touch gestures, and then calls
  /// [`Compositor::set_session_active`], which releases what clients hold
  /// and drops seat input until the session resumes. Nothing that was held
  /// before the pause continues after it, because the keys and buttons may
  /// have been released on the other VT. Resuming reactivates the
  /// compositor and asks for a frame.
  pub fn set_session_active(&mut self, host: &mut dyn Host, active: bool) {
    if active {
      self.compositor.set_session_active(true);
      self.request_redraw();
    } else {
      self.key_repeat.cancel();
      self.keyboard.release_all();
      self.finish_switcher();
      self.rotation_drag = None;
      if let Some(state) = &mut self.expo {
        state.cancel_pointer();
      }
      self.expo_touch = None;
      self.gestures.cancel();
      self.abandon_drag();
      self.compositor.set_session_active(false);
    }
    self.flush(host);
  }

  /// Forgets every GPU texture: surface textures, geometry animations'
  /// old frames, Scale titles, icons and the filter text. The native host
  /// calls it before drawing with a new GPU context after the old device
  /// was lost, and then [`Frontend::attach_gpu`] with that context; the
  /// next frame uploads again what the compositor's surface snapshots
  /// still hold: copied pixels, and shm buffers not uploaded yet. It
  /// imports DMA-BUFs again from their kept fds. An shm buffer that was
  /// uploaded has gone back to its client, so its surface draws nothing
  /// until it commits again. The locks of frames in flight are dropped,
  /// since a lost device runs nothing more. Closing animations whose
  /// texture is gone are not drawn.
  pub fn drop_gpu_resources(&mut self) {
    if let Some(cache) = &self.dmabuf {
      cache.borrow_mut().clear();
    }
    self.textures.clear();
    self.uploaded.clear();
    self.previous_surfaces.clear();
    self.animations.clear();
    self.scale_titles.clear();
    self.scale_icons.clear();
    self.scale_filter_texture = None;
    self.request_redraw();
  }

  /// The keyboard LEDs the shared keyboard state lights, after adopting
  /// locks and the layout that IPC may have changed. The native host
  /// passes them to every libinput keyboard.
  pub fn keyboard_leds(&mut self) -> crate::keyboard::Leds {
    self.keyboard.sync_locked(
      self.compositor.locked_modifiers(),
      self.compositor.keyboard_layout_index(),
    );
    self.keyboard.leds()
  }

  /// Saves the session for the last time. Hosts call this when their
  /// event loop ends; later calls, and saves after it, do nothing.
  ///
  /// The snapshot is taken first, then Xwayland is stopped and the X
  /// display's lock and sockets are removed, and only then is the
  /// snapshot written: the write syncs to disk, which can take many
  /// seconds on a busy one, and a SIGKILL during it must not leave the
  /// display behind. Stopping Xwayland takes the X windows out of the
  /// model, so a later save would lose them.
  ///
  /// The final write waits for a background save still in progress, drops
  /// one still waiting, and is on disk when this returns
  /// ([`SessionWriter::finish`]).
  pub fn shutdown(&mut self) {
    let Some(session) = self.session.take() else {
      return;
    };
    let snapshot = self.compositor.core().snapshot();
    self.compositor.stop_xwayland();
    if let Err(error) = session.finish(snapshot) {
      eprintln!("shingle: failed to save session: {error}");
    }
  }

  fn request_redraw(&mut self) {
    self.redraw_requested = true;
  }

  /// The filter new textures are sampled with.
  fn texture_filter(&self) -> TextureFilter {
    if self.zoom.interpolation == "nearest" {
      TextureFilter::Nearest
    } else {
      TextureFilter::Linear
    }
  }

  /// Uploads the shm buffer a commit showed, if it has one still to be
  /// uploaded, and forgets that a surface's texture is an upload when it
  /// committed something else or nothing.
  fn upload_committed(&mut self, snapshot: &shingle::SurfaceSnapshot) {
    let shm = snapshot
      .buffer
      .as_ref()
      .and_then(|buffer| Some((buffer, buffer.shm()?)));
    match shm {
      Some((buffer, content)) if content.is_locked() => {
        self.upload_shm(snapshot.id, buffer, content, true);
      }
      // The same buffer again, already uploaded: nothing to write.
      Some(_) => {}
      None => {
        self.uploaded.remove(&snapshot.id);
      }
    }
  }

  /// Uploads `content`, which `buffer` of `surface` shows, and gives the
  /// buffer back to its client. With `damage_known` only the damage is
  /// written into a texture that fits; otherwise all of it. Returns
  /// whether it was uploaded; on failure the buffer stays locked, so that
  /// the next frame can try again.
  pub(super) fn upload_shm(
    &mut self,
    surface: SurfaceId,
    buffer: &shingle::BufferSnapshot,
    content: &shingle::ShmContent,
    damage_known: bool,
  ) -> bool {
    let Some(uploader) = &self.uploader else {
      return false;
    };
    let result = crate::shm::upload(
      uploader,
      self.textures.get(&surface),
      buffer,
      content,
      self.texture_filter(),
      damage_known,
    );
    match result {
      Ok(uploaded) => {
        shingle::trace::buffer(format_args!(
          "shm upload {} bytes surface={}",
          uploaded.bytes,
          surface.get()
        ));
        #[cfg(test)]
        self.uploads.push((surface, uploaded.bytes));
        self.textures.insert(surface, uploaded.texture);
        self.uploaded.insert(surface);
        content.release();
        true
      }
      Err(error) => {
        eprintln!(
          "shingle: shm upload of surface {} failed: {error}",
          surface.get()
        );
        false
      }
    }
  }

  /// Drops the buffer locks of frames the GPU has finished and the
  /// textures of buffers that are gone.
  fn retire_frames(&mut self) {
    if let Some(cache) = &self.dmabuf {
      let mut cache = cache.borrow_mut();
      cache.retire();
      cache.prune();
    }
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
      InputEvent::PointerMotionAbsolute { x, y, output } => {
        let (x, y) = self.layout_point(x, y, output.as_ref());
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

  /// `(x, y)` in layout coordinates: as given, or relative to `output`'s
  /// top-left corner when the event names its output.
  fn layout_point(
    &self,
    x: f64,
    y: f64,
    output: Option<&shingle::OutputHandle>,
  ) -> (f64, f64) {
    match output.and_then(|output| self.compositor.output_view(output)) {
      Some(view) => (x + f64::from(view.rect.x), y + f64::from(view.rect.y)),
      None => (x, y),
    }
  }

  fn touch(&mut self, device: InputDeviceId, touch: Touch) {
    let (x, y) = self.layout_point(touch.x, touch.y, touch.output.as_ref());
    let (x, y) = self.map_zoom_input(x, y);
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
