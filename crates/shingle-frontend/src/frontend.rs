//! The [`Frontend`] struct, its public API and input dispatch. Each child
//! module adds the methods for one area: `bindings` (key and pointer
//! bindings, key repeat), `scale` and `expo` (the overviews), `switcher`,
//! `drag`, `effects` (zoom, inversion, alpha), `animation`, `outputs`
//! (what is kept per output, and which outputs need a frame), `frames`
//! (server-side window frames), `scene` (the scene build and drawing),
//! `session` (saving, config reload, the lock client), `vswipe`
//! (touchpad workspace swipes) and `whole_view` (views drawn as one for
//! their animations).

mod animation;
mod bindings;
mod cursor;
mod drag;
mod effects;
mod expo;
mod frames;
mod lock;
mod outputs;
mod scale;
mod scene;
mod session;
mod switcher;
#[cfg(test)]
mod tests;
mod vswipe;
mod whole_view;

pub use cursor::{CursorImage, CursorPlane};

use std::{
  cell::RefCell,
  collections::{HashMap, HashSet},
  error::Error,
  process::Child,
  rc::Rc,
  time::{Duration, Instant},
};

use shingle::{
  CaptureRequest, Compositor, Config, Direction, Event, InputDeviceId,
  InputDeviceKind, OutputHandle, SurfaceId, SurfaceRole,
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

use animation::ClosingAnimation;
use drag::{DragState, RotationDrag};
use outputs::OutputScene;
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
/// its event loop and [`Frontend::render`] for each output it draws when
/// [`Host::request_redraw`] asked for one, and wakes up by
/// [`Frontend::next_deadline`]. Each output has its own scene, zoom,
/// workspace slide and redraw requests; Scale and Expo run on the output
/// that was active when they started.
pub struct Frontend {
  compositor: Compositor,
  /// Saves sessions off this thread; `None` once [`Frontend::shutdown`]
  /// has saved the session for the last time.
  session: Option<SessionWriter>,
  textures: HashMap<SurfaceId, GpuTexture>,
  /// Animations of surfaces that are drawn: opening and restoring.
  open_animations: HashMap<SurfaceId, crate::animate::Animation>,
  closing_animations: HashMap<SurfaceId, ClosingAnimation>,
  minimize_animations: HashMap<SurfaceId, ClosingAnimation>,
  animate: shingle_config::AnimateConfig,
  animation_duration: Duration,
  /// The curve of `animation_duration`.
  animation_easing: shingle_config::Easing,
  /// What is drawn, zoomed and animated on each output.
  scenes: HashMap<OutputHandle, OutputScene>,
  /// The outputs whose workspace slide IPC was told about, as Wayfire's
  /// vswitch activates its plugin while it slides.
  vswitch_active: HashSet<OutputHandle>,
  /// The plugin IPC was told a pointer drag activated: `simple-tile` for
  /// a tiled window, else `move` or `resize`, as in Wayfire.
  drag_plugin: Option<&'static str>,
  /// The output Scale and Expo run on, from when they start until their
  /// exit animations end.
  overview_output: Option<OutputHandle>,
  /// When the outputs created over IPC get their next frame.
  virtual_frame: Option<Instant>,
  /// How long the "Workspace set N" label stays (`wsets.label_duration`).
  wsets_label_duration: Duration,
  /// How long a lock's last frame fades into the desktop after a proper
  /// unlock (`lock.unlock_fade`); zero for none.
  unlock_fade: Duration,
  lock_client: Option<Child>,
  /// What locking runs (`lock.command`), unless `SHINGLE_LOCK` says.
  lock_command: String,
  /// Every presented frame, written for the GPU harness
  /// (`SHINGLE_RECORD_FRAMES`).
  recorder: Option<crate::recorder::FrameRecorder>,
  /// Long turns, logged for `SHINGLE_TRACE_TURNS`.
  turns: crate::turn_trace::TurnTrace,
  session_children: Vec<Child>,
  keyboard: Keyboard,
  /// Modifier-alone bindings: `toggle_menu` and commands.
  modifier_bindings: shingle::ModifierBindingTracker,
  /// `input.modifier_binding_timeout` in milliseconds.
  modifier_binding_timeout: u32,
  key_repeat: KeyRepeat,
  /// The keyboard whose key repeats.
  repeat_device: Option<InputDeviceId>,
  switcher: Option<SwitcherState>,
  switcher_inactive_alpha: f32,
  /// The regular switcher's thumbnails, until they are back in place.
  switcher_view: Option<switcher::ShownSwitcher>,
  /// `[switcher]`'s view options.
  switcher_config: shingle_config::SwitcherConfig,
  scale: Option<ScaleState>,
  scale_exit: Option<ScaleState>,
  scale_config: shingle_config::ScaleConfig,
  scale_title_filter: shingle_config::ScaleTitleFilterConfig,
  /// Wayfire's `scale-title-filter` plugin is loaded, so typing in Scale
  /// filters it.
  scale_filter_enabled: bool,
  scale_titles: HashMap<TileWindowId, ScaleTitle>,
  scale_icons: HashMap<TileWindowId, GpuTexture>,
  /// `[decoration]`, which server-side frames are drawn with.
  decoration_config: shingle_config::DecorationConfig,
  /// The title and button textures of server-side frames.
  frame_textures: frames::FrameTextures,
  /// The windows gone whose frame titles stay until a closing animation
  /// drew its view.
  departed_titles: Vec<TileWindowId>,
  /// The textures of views drawn as one while they open.
  view_textures: whole_view::ViewTextures,
  scale_filter: String,
  scale_filter_texture: Option<ScaleTitle>,
  expo: Option<crate::expo::ExpoState>,
  expo_exit: Option<crate::expo::ExpoState>,
  expo_touch: Option<u64>,
  expo_config: shingle_config::ExpoConfig,
  vswitch: shingle_config::VswitchConfig,
  /// `[vswipe]`'s options.
  vswipe_config: shingle_config::VswipeConfig,
  /// Wayfire's `vswipe` plugin is loaded.
  vswipe_enabled: bool,
  /// The touchpad swipe vswipe follows, from its begin until its slide
  /// ends.
  vswipe: Option<vswipe::SwipeState>,
  left_handed_mode: bool,
  middle_emulation: bool,
  pointer_x: f64,
  pointer_y: f64,
  /// The buttons whose press reached the compositor and whose release
  /// has not, by device.
  delivered_buttons: HashSet<(InputDeviceId, u32)>,
  keybindings: Keybindings,
  commands: Vec<CommandBinding>,
  pointer_bindings: PointerBindings,
  drag: Option<DragState>,
  gestures: extra_gestures::GestureState,
  /// Wayfire's `extra-gestures` plugin is loaded.
  gestures_enabled: bool,
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
  background_color: [f64; 4],
  preview_base_color: [f64; 4],
  preview_border_color: [f64; 4],
  preview_border_width: u32,
  /// `[move]`'s colours for the snap preview, which Wayfire's move draws
  /// with its own options (wayfire:plugins/single_plugins/move.cpp:494).
  snap_preview_base_color: [f64; 4],
  snap_preview_border_color: [f64; 4],
  snap_preview_border_width: u32,
  drop_preview: Option<RenderRect>,
  /// Whether `drop_preview` is move's snap preview rather than
  /// simple-tile's drop preview.
  drop_preview_snaps: bool,
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
  /// Whether the host should be asked for a frame of every output it
  /// draws at the end of the call.
  redraw_all: bool,
  /// The outputs the host should be asked for a frame of at the end of the
  /// call.
  redraw_outputs: HashSet<OutputHandle>,
  /// The outputs whose cursor a cursor plane of the host shows, which
  /// their frames leave out ([`Frontend::set_hardware_cursor`]).
  hardware_cursors: HashSet<OutputHandle>,
  /// Where the cursor was when it was last looked at, to redraw where it
  /// moved.
  drawn_cursor: Option<cursor::DrawnCursor>,
  /// Output captures the next frame of their output answers.
  captures: Vec<CaptureRequest>,
  /// Window captures, which the next frame of any output answers.
  window_captures: Vec<CaptureRequest>,
  /// The outputs that changed since their last frame: their next frame,
  /// unless drawn only for a capture, is damage to captures
  /// ([`shingle::Compositor::output_damaged`]).
  damaged: HashSet<OutputHandle>,
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
        let capture =
          gpu.dmabuf().ok().and_then(crate::dmabuf::capture_formats);
        if let Some(capture) = &capture {
          eprintln!(
            "shingle: captures into DMA-BUFs of {} formats",
            capture.formats.len()
          );
        }
        self.compositor.set_capture_dmabuf(capture);
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
    let wayfire_file = config.wayfire_config_file(
      &config_path
        .clone()
        .unwrap_or_else(shingle_config::default_path),
    );
    compositor.set_wayfire_config_file(wayfire_file.as_deref());
    let mut keyboard = Keyboard::new(&keymap)?;
    keyboard.sync_locked(
      compositor.locked_modifiers(),
      compositor.keyboard_layout_index(),
    );
    Ok(Self {
      compositor,
      session: Some(SessionWriter::for_store(store)?),
      textures: HashMap::new(),
      open_animations: HashMap::new(),
      closing_animations: HashMap::new(),
      minimize_animations: HashMap::new(),
      animate: config.animate.clone(),
      animation_duration: Duration::from_millis(
        config.tiling.animation_duration,
      ),
      animation_easing: config.tiling.animation_easing,
      scenes: HashMap::new(),
      vswitch_active: HashSet::new(),
      drag_plugin: None,
      overview_output: None,
      virtual_frame: None,
      wsets_label_duration: Duration::from_millis(config.wsets.label_duration),
      unlock_fade: Duration::from_secs_f64(config.lock.unlock_fade),
      lock_client: None,
      lock_command: config.lock.command.clone(),
      recorder: crate::recorder::FrameRecorder::from_env(),
      turns: crate::turn_trace::TurnTrace::from_env(),
      session_children,
      keyboard,
      key_repeat: KeyRepeat::new(
        config.input.repeat_rate,
        config.input.repeat_delay,
      ),
      repeat_device: None,
      switcher: None,
      switcher_inactive_alpha: config.fast_switcher.inactive_alpha,
      switcher_view: None,
      switcher_config: config.switcher.clone(),
      scale: None,
      scale_exit: None,
      scale_config: config.scale.clone(),
      scale_title_filter: config.scale_title_filter.clone(),
      scale_filter_enabled: config.plugin_enabled("scale-title-filter"),
      scale_titles: HashMap::new(),
      scale_icons: HashMap::new(),
      decoration_config: config.decoration.clone(),
      frame_textures: frames::FrameTextures::default(),
      departed_titles: Vec::new(),
      view_textures: whole_view::ViewTextures::default(),
      scale_filter: String::new(),
      scale_filter_texture: None,
      expo: None,
      expo_exit: None,
      expo_touch: None,
      expo_config: config.expo.clone(),
      vswitch: config.vswitch.clone(),
      vswipe_config: config.vswipe.clone(),
      vswipe_enabled: config.plugin_enabled("vswipe"),
      vswipe: None,
      left_handed_mode: config.input.left_handed_mode,
      middle_emulation: config.input.middle_emulation,
      pointer_x: 0.0,
      pointer_y: 0.0,
      delivered_buttons: HashSet::new(),
      keybindings: config.keybindings.clone(),
      commands: config.commands.clone(),
      modifier_bindings: shingle::ModifierBindingTracker::default(),
      modifier_binding_timeout: config.input.modifier_binding_timeout,
      pointer_bindings: config.pointer.clone(),
      drag: None,
      gestures: extra_gestures::GestureState::new(
        config.extra_gestures.clone(),
      ),
      gestures_enabled: config.plugin_enabled("extra-gestures"),
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
      background_color: config.background_color,
      preview_base_color: config.tiling.preview_base_color,
      preview_border_color: config.tiling.preview_border_color,
      preview_border_width: config.tiling.preview_border_width,
      snap_preview_base_color: config.grid.preview_base_color,
      snap_preview_border_color: config.grid.preview_border_color,
      snap_preview_border_width: config.grid.preview_border_width,
      drop_preview: None,
      drop_preview_snaps: false,
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
      redraw_all: false,
      redraw_outputs: HashSet::new(),
      hardware_cursors: HashSet::new(),
      drawn_cursor: None,
      captures: Vec::new(),
      window_captures: Vec::new(),
      damaged: HashSet::new(),
      dmabuf: None,
      uploader: None,
      uploaded: HashSet::new(),
      #[cfg(test)]
      uploads: Vec::new(),
      startup: None,
    })
  }

  /// Handles the compositor's events. Returns whether any but surface
  /// commits came, which may change what every output shows; a commit
  /// asks only for the outputs its surface overlaps.
  fn process_events(&mut self, host: &mut dyn Host) -> bool {
    let events = self.compositor.drain_events();
    let changed = events
      .iter()
      .any(|event| !matches!(event, Event::SurfaceCommitted(_)));
    let mut save = false;
    // The views that go, as last drawn, before any of their surfaces is
    // forgotten below: a subsurface may go first.
    let mut departing = events
      .iter()
      .filter_map(|event| match event {
        Event::SurfaceDestroyed { surface } => {
          Some((*surface, self.view_pieces(*surface)?))
        }
        _ => None,
      })
      .collect::<HashMap<_, _>>();
    for event in events {
      match event {
        Event::WindowDestroyed { window, .. } => {
          self.switcher_window_destroyed(window);
          self.scale_window_destroyed(window);
          self.scale_titles.remove(&window);
          self.scale_icons.remove(&window);
          // A closing animation may still draw its frame's title.
          self.departed_titles.push(window);
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
        | Event::LayoutChanged => save = true,
        Event::OutputFocused { output } => {
          // Scale and Expo stay on their output; focus elsewhere ends them.
          if self.overview_output.as_ref().is_some_and(|overview| {
            *overview != output && (self.scale.is_some() || self.expo.is_some())
          }) {
            self.end_overviews();
          }
          save = true;
        }
        Event::OutputChanged { output, .. } => {
          if let Some(scene) = self.scenes.get_mut(&output) {
            scene.workspace_transition = None;
          }
          if self.overview_output.as_ref() == Some(&output) {
            self.expo_exit = None;
            self.update_scale_layout();
            self.update_expo_layout();
          }
          save = true;
        }
        Event::OutputAdded { .. } | Event::OutputRemoved { .. } => {
          if self.vswipe.as_ref().is_some_and(|state| {
            self.compositor.output_view(&state.output).is_none()
          }) {
            self.vswipe = None;
          }
          self.prune_scenes();
          save = true;
        }
        Event::OutputInhibitChanged { output, .. } => {
          self.request_redraw_on(&output);
        }
        Event::CreateHostOutput { output } => host.open_output(&output),
        Event::DestroyHostOutput { output } => host.close_output(&output),
        Event::WsetSelected { output, wset } => {
          self.show_wset_label(&output, wset);
        }
        Event::ToggleScale(all_workspaces) => {
          self.toggle_scale(all_workspaces);
        }
        Event::ToggleExpo => self.toggle_expo(),
        Event::InvertToggle => self.toggle_inversion(),
        Event::WorkspaceChanged {
          workspace,
          previous,
          output,
        } => {
          let target = workspace.saturating_sub(1) as usize;
          let from = previous.saturating_sub(1) as usize;
          let overview_active = self.overview_output.as_ref() == Some(&output)
            && (self.scale.is_some()
              || self.scale_exit.is_some()
              || self.expo.is_some()
              || self.expo_exit.is_some());
          // vswipe's own slide shows a swipe's switch.
          let animate = !overview_active
            && !self.vswipe_on(&output)
            && self.vswitch.duration_ms > 0;
          let scene = self.scene_mut(&output);
          scene.workspace_transition = (from != target && animate).then(|| {
            workspace_transition::WorkspaceTransition::new(from, target)
          });
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
            && !self.textures.contains_key(&snapshot.id)
            && self.previous_of(snapshot.id).is_none()
          {
            self.start_open_animation(&snapshot);
          }
          self.mark_dirty(snapshot.id);
          // A cursor on a cursor plane changes without a frame.
          let on_plane = snapshot.role == SurfaceRole::Cursor
            && self
              .hardware_cursors
              .contains(&self.compositor.pointer_output());
          if !on_plane {
            self.damage(snapshot.geometry);
          }
          self.upload_committed(&snapshot);
        }
        Event::SurfaceDestroyed { surface } => {
          self.uploaded.remove(&surface);
          let previous = self.previous_of(surface);
          let window = self.surface_window(surface);
          let closing =
            previous.filter(|_| self.textures.contains_key(&surface));
          let pieces = departing.remove(&surface);
          if let Some((output, (geometry, buffer))) = closing
            && let Some(animation) =
              self.close_animation(surface, window, geometry, pieces.as_ref())
          {
            self.closing_animations.insert(
              surface,
              ClosingAnimation::new(
                (window, output),
                (geometry, buffer),
                animation,
                pieces,
              ),
            );
          } else {
            self.textures.remove(&surface);
          }
          self.open_animations.remove(&surface);
          self.minimize_animations.remove(&surface);
          for scene in self.scenes.values_mut() {
            scene.surface_destroyed(surface);
          }
        }
        Event::PointerWarp { x, y, output } => {
          self.pointer_x = x;
          self.pointer_y = y;
          let output = output.unwrap_or_else(|| self.output_under(x, y));
          let rect = self.output_rect(&output);
          let (factor, origin) = self.zoom_of(&output);
          let point = shingle_render::zoom_output_point(
            x - f64::from(rect.x),
            y - f64::from(rect.y),
            rect.width,
            rect.height,
            factor,
            origin.0,
            origin.1,
          );
          host.warp_pointer(&output, point.0, point.1);
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
        Event::StipcGesture {
          device,
          kind,
          gesture,
        } => {
          let event = match kind {
            shingle::PointerGesture::Swipe => InputEvent::Swipe(gesture),
            shingle::PointerGesture::Pinch => InputEvent::Pinch(gesture),
            shingle::PointerGesture::Hold => InputEvent::Hold(gesture),
          };
          self.dispatch_input(host, device, event);
        }
        // Any event but a commit redraws, which a frame change needs.
        Event::DpmsChanged(_)
        | Event::DecorationChanged { .. }
        | Event::OpacityChanged { .. }
        | Event::Idle
        | Event::Resume
        | Event::FocusChanged { .. }
        | Event::GeometryChanged { .. }
        | Event::LockStateChanged(_) => {}
        Event::CaptureRequested(request) => self.add_capture(request),
      }
    }
    self.fail_undrawable_captures();
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
        self.cancel_vswipe();
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
    self.turns.begin("process");
    self.process_turn(host);
    self.turns.end();
  }

  fn process_turn(&mut self, host: &mut dyn Host) {
    if self.quit {
      host.exit();
      return;
    }
    // Buffers of finished frames are released by this turn's dispatch.
    self.retire_frames();
    self.turns.mark("retire");
    if let Err(error) = self.compositor.dispatch_pending() {
      eprintln!("shingle: dispatch failed: {error}");
      host.exit();
      return;
    }
    self.turns.mark("dispatch");
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
    self.turns.mark("timers");
    let changed = self.process_events(host);
    self.turns.mark("events");
    if changed || self.compositor.needs_frame() {
      self.request_redraw();
    }
    let now = Instant::now();
    self.finish_vswipe(now);
    self.pace_virtual_outputs(now);
    self.expire_labels(now);
    self.expire_startup_inhibits(now);
    self.publish_plugin_activations();
    self.turns.mark("pace");
    self.flush(host);
    self.turns.mark("flush");
    self.publish_counts();
  }

  /// Sends `plugin-activation-state-changed` for each output whose
  /// workspace slide started or ended since the last turn (vswitch), and
  /// for a pointer drag that started or ended.
  fn publish_plugin_activations(&mut self) {
    let active = self
      .scenes
      .iter()
      .filter(|(_, scene)| scene.workspace_transition.is_some())
      .map(|(output, _)| output.clone())
      .collect::<HashSet<_>>();
    let started = active.difference(&self.vswitch_active).count();
    let ended = self.vswitch_active.difference(&active).count();
    for _ in 0..started {
      self.compositor.notify_plugin_activation("vswitch", true);
    }
    for _ in 0..ended {
      self.compositor.notify_plugin_activation("vswitch", false);
    }
    self.vswitch_active = active;
    let dragging = self.drag.as_ref().map(|drag| {
      let (window, plugin) = match drag {
        DragState::Move { source, .. } => (*source, "move"),
        DragState::Resize { window, .. } => (*window, "resize"),
      };
      let tiled = self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| info.state.floating.is_none());
      if tiled { "simple-tile" } else { plugin }
    });
    match (self.drag_plugin, dragging) {
      (None, Some(plugin)) => {
        self.compositor.notify_plugin_activation(plugin, true);
        self.drag_plugin = Some(plugin);
      }
      (Some(plugin), None) => {
        self.compositor.notify_plugin_activation(plugin, false);
        self.drag_plugin = None;
      }
      _ => {}
    }
  }

  /// Hands the compositor the sizes of the frontend's caches, which
  /// `stipc/object_counts` reports for soak tests (tools/soak_test).
  fn publish_counts(&mut self) {
    let (dmabuf_textures, frames_in_flight) =
      self.dmabuf.as_ref().map_or((0, 0), |dmabuf| {
        let dmabuf = dmabuf.borrow();
        (dmabuf.cached(), dmabuf.frames_in_flight())
      });
    self.compositor.set_frontend_counts(&[
      ("textures", self.textures.len()),
      ("uploaded", self.uploaded.len()),
      ("dmabuf_textures", dmabuf_textures),
      ("frames_in_flight", frames_in_flight),
      ("open_animations", self.open_animations.len()),
      ("closing_animations", self.closing_animations.len()),
      ("minimize_animations", self.minimize_animations.len()),
      ("scale_titles", self.scale_titles.len()),
      ("scale_icons", self.scale_icons.len()),
      ("frame_textures", self.frame_textures.len()),
      ("rotations", self.rotations.len()),
      ("scenes", self.scenes.len()),
    ]);
  }

  /// Returns when [`Frontend::process`] next has timed work: a key
  /// repeat, a frame of the outputs created over IPC, the end of a
  /// workspace set label, or an output that stops waiting for its
  /// background to fade in. Hosts also call it at least every 16 ms, for
  /// config reload, drag and gesture timers and child reaping.
  pub fn next_deadline(&self) -> Option<Instant> {
    [
      self.key_repeat.deadline(),
      self.virtual_frame,
      self.label_deadline(),
      self.startup_deadline(),
    ]
    .into_iter()
    .flatten()
    .min()
  }

  /// Sets the output to a `width` by `height` pixel display, in logical
  /// size after the output scale, keeping its position. The host calls
  /// this once it knows its display size. It means the first output;
  /// [`Frontend::output_resized_on`] names one.
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
  /// transitions that no longer fit. It means the first output;
  /// [`Frontend::output_resized_on`] names one.
  pub fn output_resized(
    &mut self,
    width: u32,
    height: u32,
  ) -> Result<(), shingle::Error> {
    let result = self.set_output_size(width, height);
    let primary = self.compositor.primary_output();
    self.after_output_resize(&primary);
    result
  }

  /// Handles a resize of the host window or display that shows `output`
  /// to `width` by `height` physical pixels, as
  /// [`Frontend::output_resized`] does for the first output. Nested calls
  /// it for each of its windows.
  pub fn output_resized_on(
    &mut self,
    output: &OutputHandle,
    width: u32,
    height: u32,
  ) -> Result<(), shingle::Error> {
    let result = self.compositor.set_output_size(output, (width, height));
    self.after_output_resize(output);
    result
  }

  /// Gives `output` the scale of the host window or display that shows
  /// it, keeping its physical size, and lays out what depends on its
  /// logical size again, as [`Frontend::output_resized_on`] does. Nested
  /// calls it with each window's scale factor.
  pub fn output_rescaled_on(
    &mut self,
    output: &OutputHandle,
    scale: f64,
  ) -> Result<(), shingle::Error> {
    let before = self.compositor.output_view(output).map(|view| view.scale);
    let result = self.compositor.set_output_scale_on(output, scale);
    if before != Some(scale) {
      self.after_output_resize(output);
    }
    result
  }

  /// Ends what a resize of `output` invalidates: its workspace slide,
  /// vswipe's swipe on it, and Expo's exit; Scale and Expo are laid out
  /// again.
  fn after_output_resize(&mut self, output: &OutputHandle) {
    self.expo_exit = None;
    if self.vswipe_on(output) {
      self.cancel_vswipe();
    }
    if let Some(scene) = self.scenes.get_mut(output) {
      scene.workspace_transition = None;
    }
    self.update_scale_layout();
    self.update_expo_layout();
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
      self.cancel_vswipe();
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
    for scene in self.scenes.values_mut() {
      scene.forget_surfaces();
    }
    self.scale_titles.clear();
    self.scale_icons.clear();
    self.frame_textures.clear();
    self.view_textures.clear();
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
    let started = Instant::now();
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
        // The time is what tools/shm_bench reads.
        shingle::trace::buffer(format_args!(
          "shm upload {} bytes surface={} us={}",
          uploaded.bytes,
          surface.get(),
          started.elapsed().as_micros()
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
      // Buttons, scrolling and touches end a modifier-alone binding, as
      // in Wayfire.
      InputEvent::PointerButton { code, pressed } => {
        self.modifier_bindings.interrupt();
        self.pointer_button(device, code, pressed)
      }
      InputEvent::PointerAxis(axis) => {
        self.modifier_bindings.interrupt();
        self.pointer_axis(device, axis)
      }
      InputEvent::Touch(touch) => {
        if touch.phase == TouchPhase::Down {
          self.modifier_bindings.interrupt();
        }
        self.touch(device, touch)
      }
      InputEvent::Swipe(gesture) => self.swipe_input(device, gesture),
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
    self.follow_zoom(x, y);
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
    let consumed = self.expo_pointer_input(code, pressed)
      || self.scale_pointer_input(code, pressed)
      || self.pointer_button_action(button_name(code), pressed);
    if !consumed {
      if pressed {
        self.delivered_buttons.insert((device, code));
      } else {
        self.delivered_buttons.remove(&(device, code));
      }
      self.compositor.pointer_button(device, code, pressed);
      self.save();
    } else if !pressed && self.delivered_buttons.remove(&(device, code)) {
      // A drag the press started, such as a move the client asked for,
      // took the release. The client got the press, so it gets the
      // release too, and the seat no longer holds the button.
      self.compositor.pointer_button(device, code, false);
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
        if self.gestures_enabled {
          self.gestures.touch_down(touch.id, x, y, now);
        }
        None
      }
      TouchPhase::Motion => {
        let action = self
          .gestures_enabled
          .then(|| self.gestures.touch_motion(touch.id, x, y, now))
          .flatten();
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
        let action = self
          .gestures_enabled
          .then(|| self.gestures.touch_up(touch.id, now))
          .flatten();
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
