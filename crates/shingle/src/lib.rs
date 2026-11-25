mod activation;
mod alpha_modifier;
mod buffer;
mod clock;
mod commit_timing;
mod cursor_shape;
mod data_control;
mod data_device;
mod decoration;
mod dialog;
mod ext_toplevel;
mod ext_workspace;
mod fifo;
mod foreign;
mod foreign_toplevel;
mod fractional_scale;
mod gamma_control;
mod idle_notify;
mod input;
mod input_device;
mod input_method;
mod input_method_v2;
mod input_panel;
mod ipc;
mod launch;
mod layer_shell;
mod output_management;
mod output_power;
mod output_transform;
mod placement;
mod pointer_gestures;
mod pointer_protocols;
mod pointer_warp;
mod presentation;
mod primary_selection;
mod region;
mod screencopy;
mod security_context;
mod shell;
mod shortcuts;
mod single_pixel_buffer;
mod state;
mod subsurface;
mod tearing;
#[cfg(test)]
mod test_client;
mod text_input;
mod toplevel_icon;
mod viewport;
mod virtual_keyboard;
mod virtual_pointer;
mod window_rules;
mod xdg_output;

use std::{
  collections::{HashMap, HashSet},
  ffi::{OsStr, OsString},
  os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
  sync::{Arc, Mutex},
  time::Duration,
};

use calloop::{
  EventLoop, Interest, Mode, PostAction,
  generic::{FdWrapper, Generic},
  signals::{Signal, Signals},
};
pub use input::Modifiers;
pub use input_device::InputDeviceKind;
pub use shingle_core::{Direction, StackingMode, WindowId};
use shingle_core::{Gaps, Rect, Shingle, WorkspaceId};
use shingle_idle::{Config as IdleConfig, IdleCoordinator};
pub use shingle_idle::{DpmsState, LockState};
use wayland_server::{
  Display, ListeningSocket,
  backend::{ClientData, ClientId, DisconnectReason},
};

pub use state::SurfaceId;
use state::{ClientDataImpl, CompositorState};

pub const OUTPUT_NAME: &str = "HEADLESS-1";
pub const OUTPUT_WIDTH: u32 = 1280;
pub const OUTPUT_HEIGHT: u32 = 720;

/// Flags of a presented frame, as `wp_presentation_feedback` sends them.
pub use wayland_protocols::wp::presentation_time::server::{
  wp_presentation_feedback::Kind as PresentationFlags,
};

/// A frame that reached the screen, as [`Compositor::frame_presented`]
/// reports it to `wp_presentation` clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentInfo {
  /// `CLOCK_MONOTONIC` time at which the frame started to be shown, such as
  /// a DRM page-flip timestamp.
  pub time: Duration,
  /// Nanoseconds until the next refresh, or zero when unknown or variable.
  pub refresh_ns: u32,
  /// The display's frame counter, such as the DRM vblank sequence, or zero
  /// when there is none.
  pub seq: u64,
  /// How the frame was presented: `Vsync`, `HwClock`, `HwCompletion`,
  /// `ZeroCopy`.
  pub flags: PresentationFlags,
}

/// What clients are told about the physical output: its identity, size and
/// refresh rate. A backend describes its output with
/// [`Compositor::set_output_info`]; nested and headless compositors keep
/// the default, a 60 Hz output named [`OUTPUT_NAME`].
///
/// The output's position and logical size still come from
/// [`Compositor::set_output`], and its scale and transform from their own
/// setters, so the mode clients see is the output rectangle in physical
/// pixels at `refresh_mhz`.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
  /// Connector name, such as `DP-1`. Reported as the `wl_output`,
  /// xdg-output, output-management and IPC output name.
  pub name: String,
  /// Manufacturer, from the EDID on real hardware.
  pub make: String,
  /// Model name, from the EDID on real hardware.
  pub model: String,
  /// Serial number, if the EDID has one.
  pub serial: Option<String>,
  /// Physical width and height in millimetres, or zero when unknown.
  pub physical_size_mm: (u32, u32),
  /// Subpixel layout of the panel.
  pub subpixel: wayland_server::protocol::wl_output::Subpixel,
  /// Refresh rate of the current mode in millihertz, or zero when unknown.
  pub refresh_mhz: u32,
}

impl Default for OutputInfo {
  fn default() -> Self {
    Self {
      name: OUTPUT_NAME.into(),
      make: "Shingle".into(),
      model: "Headless output".into(),
      serial: None,
      physical_size_mm: (310, 175),
      subpixel: wayland_server::protocol::wl_output::Subpixel::Unknown,
      refresh_mhz: 60_000,
    }
  }
}

impl OutputInfo {
  /// Returns the human-readable description sent to clients, built like
  /// wlroots' output description: make, model, serial and name.
  pub fn description(&self) -> String {
    match &self.serial {
      Some(serial) => {
        format!("{} {} {} ({})", self.make, self.model, serial, self.name)
      }
      None => format!("{} {} ({})", self.make, self.model, self.name),
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkspaceWraparound {
  #[default]
  Disabled,
  Grid,
  OnEnd,
}

#[derive(Clone, Debug)]
pub struct Config {
  pub output: Rect,
  pub background_color: [f64; 4],
  pub workspace_width: u32,
  pub workspace_height: u32,
  pub workspace_wraparound: WorkspaceWraparound,
  pub output_scale: f64,
  pub output_transform: wayland_server::protocol::wl_output::Transform,
  pub mouse_scroll_speed: f64,
  pub mouse_natural_scroll: bool,
  pub cursor_theme: String,
  pub cursor_size: u32,
  pub enable_text_input_v3: bool,
  pub initial_locked_modifiers: u32,
  pub idle: IdleConfig,
  pub gaps: Gaps,
  pub minimum_extent: u32,
  pub resize: shingle_config::ResizeConfig,
  pub keep_fullscreen_on_adjacent: bool,
  pub tile_by_default: shingle_config::ViewMatcher,
  pub placement_mode: String,
  pub focus: shingle_config::FocusConfig,
  pub activation: shingle_config::ActivationConfig,
  pub shortcuts: shingle_config::ShortcutConfig,
  pub all_dialogs_modal: bool,
  pub keep_last_toplevel_activated: bool,
  pub max_buffer_size: u32,
  pub preferred_decoration_mode: String,
  pub force_preferred_decoration_mode: bool,
  pub focus_main_surface_instead_of_popup: bool,
  pub disable_primary_selection: bool,
  pub security_privileged_protocols: HashSet<String>,
  pub window_rules: Vec<shingle_config::WindowRule>,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      output: Rect::new(0, 0, OUTPUT_WIDTH, OUTPUT_HEIGHT),
      background_color: [0.1, 0.1, 0.1, 1.0],
      workspace_width: 10,
      workspace_height: 1,
      workspace_wraparound: WorkspaceWraparound::Disabled,
      output_scale: 1.0,
      output_transform: wayland_server::protocol::wl_output::Transform::Normal,
      mouse_scroll_speed: 1.0,
      mouse_natural_scroll: false,
      cursor_theme: "default".into(),
      cursor_size: 24,
      enable_text_input_v3: true,
      initial_locked_modifiers: 0,
      idle: IdleConfig::default(),
      gaps: Gaps::default(),
      minimum_extent: 50,
      resize: shingle_config::ResizeConfig::default(),
      keep_fullscreen_on_adjacent: true,
      tile_by_default: shingle_config::ViewMatcher::default(),
      placement_mode: "center".into(),
      focus: shingle_config::FocusConfig::default(),
      activation: shingle_config::ActivationConfig::default(),
      shortcuts: shingle_config::ShortcutConfig::default(),
      all_dialogs_modal: true,
      keep_last_toplevel_activated: true,
      max_buffer_size: 16384,
      preferred_decoration_mode: "client".into(),
      force_preferred_decoration_mode: false,
      focus_main_surface_instead_of_popup: false,
      disable_primary_selection: false,
      security_privileged_protocols: security_context::parse_privileged(
        &shingle_config::SecurityContextConfig::default().privileged_protocols,
      ),
      window_rules: Vec::new(),
    }
  }
}

impl From<shingle_config::ShingleConfig> for Config {
  fn from(config: shingle_config::ShingleConfig) -> Self {
    Self {
      output: config.output.rect(),
      background_color: config.background_color,
      workspace_width: config.workspaces.width,
      workspace_height: config.workspaces.height,
      workspace_wraparound: match config.workspaces.wraparound.as_str() {
        "true" => WorkspaceWraparound::Grid,
        "on_end" => WorkspaceWraparound::OnEnd,
        _ => WorkspaceWraparound::Disabled,
      },
      output_scale: config.output.scale,
      output_transform: parse_output_transform(&config.output.transform),
      mouse_scroll_speed: config.input.mouse_scroll_speed,
      mouse_natural_scroll: config.input.mouse_natural_scroll,
      cursor_theme: config.input.cursor_theme,
      cursor_size: config.input.cursor_size,
      enable_text_input_v3: config.input.enable_text_input_v3,
      initial_locked_modifiers: (u32::from(config.input.capslock_default) << 1)
        | (u32::from(config.input.numlock_default) << 4),
      idle: config.idle.as_idle_config(),
      gaps: config.tiling.gaps,
      minimum_extent: config.tiling.minimum_extent,
      resize: config.resize,
      keep_fullscreen_on_adjacent: config.tiling.keep_fullscreen_on_adjacent,
      tile_by_default: config.tiling.tile_by_default,
      placement_mode: config.placement.mode,
      focus: config.focus,
      activation: config.activation,
      shortcuts: config.shortcuts,
      all_dialogs_modal: config.behavior.all_dialogs_modal,
      keep_last_toplevel_activated: config
        .behavior
        .keep_last_toplevel_activated,
      max_buffer_size: config.behavior.max_buffer_size,
      preferred_decoration_mode: config.behavior.preferred_decoration_mode,
      force_preferred_decoration_mode: config
        .behavior
        .force_preferred_decoration_mode,
      focus_main_surface_instead_of_popup: config
        .behavior
        .focus_main_surface_instead_of_popup,
      disable_primary_selection: config.behavior.disable_primary_selection,
      security_privileged_protocols: security_context::parse_privileged(
        &config.security_context.privileged_protocols,
      ),
      window_rules: config.window_rules,
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBindingMode {
  Normal,
  Repeat,
  Release,
}

#[derive(Clone, Debug)]
pub struct RuntimeBinding {
  pub id: u64,
  pub binding: shingle_config::Binding,
  pub command: Option<String>,
  pub call_method: Option<String>,
  pub call_data: Option<serde_json::Value>,
  pub mode: RuntimeBindingMode,
  pub exec_always: bool,
  pub(crate) client_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SurfaceRole {
  Unassigned,
  Toplevel,
  Popup,
  Subsurface,
  LayerShell,
  Cursor,
  SessionLock,
}

#[derive(Clone, Debug)]
pub struct BufferSnapshot {
  pub width: u32,
  pub height: u32,
  pub source: Option<[f64; 4]>,
  pub scale: i32,
  pub transform: wayland_server::protocol::wl_output::Transform,
  pub rgba: Arc<[u8]>,
  pub damage: Vec<Rect>,
  pub offset_x: i32,
  pub offset_y: i32,
}

impl BufferSnapshot {
  pub fn stride(&self) -> u32 {
    self.width * 4
  }
}

#[derive(Clone, Debug)]
pub struct ToplevelIconBuffer {
  pub width: u32,
  pub scale: i32,
  pub rgba: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub struct ToplevelIcon {
  pub name: Option<String>,
  pub buffers: Vec<ToplevelIconBuffer>,
}

#[derive(Clone, Debug)]
pub struct SurfaceSnapshot {
  pub id: SurfaceId,
  pub window: Option<WindowId>,
  pub parent: Option<SurfaceId>,
  pub role: SurfaceRole,
  pub app_id: Option<String>,
  pub title: Option<String>,
  pub icon: Option<ToplevelIcon>,
  pub geometry: Rect,
  pub opacity: f32,
  pub buffer: Option<BufferSnapshot>,
}

#[derive(Clone, Debug)]
pub enum Event {
  WindowCreated {
    window: WindowId,
    surface: SurfaceId,
  },
  WindowDestroyed {
    window: WindowId,
    surface: SurfaceId,
  },
  WindowMoved {
    window: WindowId,
    from: u32,
    workspace: u32,
  },
  GeometryChanged {
    window: WindowId,
    old_geometry: Rect,
  },
  AppIdChanged {
    window: WindowId,
    surface: SurfaceId,
    app_id: String,
  },
  TitleChanged {
    window: WindowId,
    surface: SurfaceId,
    title: String,
  },
  IconChanged {
    window: WindowId,
  },
  TilingChanged {
    window: WindowId,
    surface: SurfaceId,
    tiled: bool,
    old_edges: u32,
    new_edges: u32,
  },
  OpacityChanged {
    window: WindowId,
    opacity: f32,
  },
  SnappingChanged {
    window: WindowId,
    snapped: bool,
  },
  MinimizedChanged {
    window: WindowId,
    minimized: bool,
  },
  FullscreenChanged {
    window: WindowId,
    fullscreen: bool,
  },
  MaximizedChanged {
    window: WindowId,
    maximized: bool,
  },
  StickyChanged {
    window: WindowId,
    sticky: bool,
  },
  StackingChanged {
    window: WindowId,
    previous: StackingMode,
    current: StackingMode,
  },
  StackingOrderChanged {
    window: WindowId,
  },
  FocusChanged {
    window: Option<WindowId>,
  },
  ShowDesktopChanged(bool),
  BeginMove {
    window: WindowId,
  },
  BeginResize {
    window: WindowId,
    edges: u32,
  },
  SurfaceCommitted(Box<SurfaceSnapshot>),
  SurfaceDestroyed {
    surface: SurfaceId,
  },
  OutputChanged {
    output: Rect,
  },
  ToggleScale(bool),
  ToggleExpo,
  InvertToggle,
  PointerWarp {
    x: f64,
    y: f64,
  },
  WorkspaceChanged {
    workspace: u32,
  },
  DpmsChanged(DpmsState),
  Idle,
  Resume,
  StartLockClient,
  LockStateChanged(LockState),
  /// A seat input device was enabled or disabled over IPC. Held input has
  /// already been released when a device is disabled.
  InputDeviceChanged {
    device: InputDeviceKind,
    enabled: bool,
  },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error("failed to create Wayland display: {0}")]
  Display(String),
  #[error("failed to bind Wayland socket: {0}")]
  Socket(#[from] wayland_server::BindError),
  #[error("failed to create event loop: {0}")]
  EventLoop(String),
  #[error("Wayland dispatch failed: {0}")]
  Dispatch(#[from] std::io::Error),
  #[error("core state error: {0}")]
  Core(#[from] shingle_core::ShingleError),
  #[error("workspace index {0} is outside the configured workspace grid")]
  InvalidWorkspace(usize),
  #[error("workspace grid dimensions must be nonzero")]
  InvalidWorkspaceGrid,
  #[error("output scale must be finite and positive")]
  InvalidOutputScale,
  #[error("keymap error: {0}")]
  Keymap(String),
  #[error("IPC error: {0}")]
  Ipc(String),
}

pub struct Compositor {
  display: Display<CompositorState>,
  state: CompositorState,
  event_loop: EventLoop<'static, CompositorState>,
  socket: ListeningSocket,
  socket_name: OsString,
  security_listeners: HashMap<u64, security_context::SecurityListener>,
  ipc: Option<ipc::IpcServer>,
}

impl Compositor {
  pub fn new(config: Config) -> Result<Self, Error> {
    Self::with_core(config, Shingle::new())
  }

  pub fn with_core(config: Config, core: Shingle) -> Result<Self, Error> {
    let socket = ListeningSocket::bind_auto("wayland", 0..32)?;
    let socket_name = socket
      .socket_name()
      .expect("bind_auto always returns a socket name")
      .to_owned();
    Self::with_socket(config, core, socket, socket_name)
  }

  /// Builds a compositor whose Wayland socket is bound at the absolute
  /// `path` instead of in `XDG_RUNTIME_DIR`. `socket_name` is then that
  /// path, which libwayland also accepts as `WAYLAND_DISPLAY`. Tests use it
  /// to keep their sockets out of the session's runtime directory.
  pub fn with_socket_path(
    config: Config,
    path: &std::path::Path,
  ) -> Result<Self, Error> {
    let socket = ListeningSocket::bind_absolute(path.to_path_buf())?;
    Self::with_socket(config, Shingle::new(), socket, path.into())
  }

  /// Builds a compositor around an already bound Wayland socket, whose
  /// `WAYLAND_DISPLAY` name is `socket_name`.
  fn with_socket(
    config: Config,
    core: Shingle,
    socket: ListeningSocket,
    socket_name: OsString,
  ) -> Result<Self, Error> {
    let display =
      Display::new().map_err(|error| Error::Display(error.to_string()))?;
    let disconnected = Arc::new(Mutex::new(Vec::new()));
    let state =
      CompositorState::new(config, core, disconnected.clone(), &socket_name)?;
    state.create_globals(&display.handle());

    let event_loop = EventLoop::try_new()
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    let handle = event_loop.handle();
    // SAFETY: both raw descriptors remain owned by Compositor and outlive
    // their registered sources; the wrappers never close them.
    let display_fd = unsafe { FdWrapper::new(display.as_fd().as_raw_fd()) };
    handle
      .insert_source(
        Generic::new(display_fd, Interest::READ, Mode::Level),
        |_, _, state: &mut CompositorState| {
          state.dispatch_pending = true;
          Ok(PostAction::Continue)
        },
      )
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    // SAFETY: the listening socket outlives this non-owning descriptor.
    let socket_fd = unsafe { FdWrapper::new(socket.as_fd().as_raw_fd()) };
    handle
      .insert_source(
        Generic::new(socket_fd, Interest::READ, Mode::Level),
        |_, _, state| {
          state.accept_pending = true;
          Ok(PostAction::Continue)
        },
      )
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    // The housekeeping tick (idle timeouts, IPC) is a timerfd rather than a
    // calloop timer. calloop keeps timers in its poll timeout, which would
    // not make the epoll descriptor readable for a backend polling it
    // through `AsFd`.
    handle
      .insert_source(
        Generic::new(tick_timer()?, Interest::READ, Mode::Level),
        |_, timer, state| {
          let mut expirations = [0; 8];
          let _ = rustix::io::read(timer.as_fd(), &mut expirations);
          state.idle_pending = true;
          Ok(PostAction::Continue)
        },
      )
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    handle
      .insert_source(signals, |_, _, state| {
        state.shutdown = true;
      })
      .map_err(|error| Error::EventLoop(error.to_string()))?;

    Ok(Self {
      display,
      state,
      event_loop,
      socket,
      socket_name,
      security_listeners: HashMap::new(),
      ipc: None,
    })
  }

  pub fn headless() -> Result<Self, Error> {
    Self::new(Config::default())
  }

  pub fn socket_name(&self) -> &OsStr {
    &self.socket_name
  }

  pub fn runtime_bindings(&self) -> Vec<RuntimeBinding> {
    self.state.runtime_bindings.values().cloned().collect()
  }

  pub fn trigger_runtime_binding(&mut self, id: u64) {
    ipc::trigger_runtime_binding(&mut self.state, id);
  }

  /// Returns whether the seat input device of this kind is enabled. The
  /// nested frontend checks this before acting on host input itself, for
  /// example to match bindings.
  pub fn input_device_enabled(&self, kind: InputDeviceKind) -> bool {
    input_device::enabled(&self.state, kind)
  }

  /// Enables Wayfire-style length-prefixed JSON IPC on a local socket.
  pub fn enable_ipc(&mut self, path: &std::path::Path) -> Result<(), Error> {
    let server = ipc::IpcServer::new(path)
      .map_err(|error| Error::Ipc(error.to_string()))?;
    self.ipc = Some(server);
    Ok(())
  }

  /// Reports a frontend plugin's actual activation state to IPC watchers.
  pub fn notify_plugin_activation(&mut self, plugin: &str, activated: bool) {
    let event = ipc::plugin_activation_event(&self.state, plugin, activated);
    self.state.ipc_events.push(event);
  }

  pub fn core(&self) -> &Shingle {
    &self.state.core
  }

  pub fn idle(&self) -> &IdleCoordinator {
    &self.state.idle
  }

  pub fn output(&self) -> Rect {
    self.state.output
  }

  pub fn poll(&mut self, timeout: Option<Duration>) -> Result<usize, Error> {
    self.state.reap_ipc_children();
    self
      .event_loop
      .dispatch(timeout, &mut self.state)
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    if self.state.accept_pending {
      self.state.accept_pending = false;
      self.accept_clients()?;
    }
    let dispatched = if self.state.dispatch_pending {
      self.state.dispatch_pending = false;
      self.display.dispatch_clients(&mut self.state)?
    } else {
      0
    };
    self.register_security_listeners()?;
    self.accept_security_clients()?;
    self.state.cleanup_disconnected();
    if let Some(ipc) = &mut self.ipc {
      ipc.poll(&mut self.state);
    }
    idle_notify::sync_inhibitors(&mut self.state);
    self.state.poll_idle();
    idle_notify::sync(&mut self.state);
    shortcuts::sync(&mut self.state);
    text_input::sync(&mut self.state);
    input_method::sync(&mut self.state, &self.display.handle());
    input_method_v2::sync(&mut self.state);
    input_panel::sync(&mut self.state);
    foreign_toplevel::sync(&mut self.state, &self.display.handle());
    ext_workspace::sync(&mut self.state, &self.display.handle());
    data_device::sync_selection(&mut self.state, &self.display.handle());
    data_device::sync_drag(&mut self.state, &self.display.handle());
    data_control::sync(&mut self.state, &self.display.handle());
    primary_selection::sync_selection(&mut self.state, &self.display.handle());
    self.display.flush_clients()?;
    Ok(dispatched)
  }

  pub fn dispatch_pending(&mut self) -> Result<usize, Error> {
    self.poll(Some(Duration::ZERO))
  }

  /// Reports a frame drawn now, for frontends that learn nothing more
  /// precise about presentation, such as nested. It is
  /// [`Compositor::frame_presented`] with the current `CLOCK_MONOTONIC`
  /// time, the output's refresh rate from [`OutputInfo`], no sequence
  /// number and no flags.
  pub fn frame(&mut self) {
    let refresh_ns =
      presentation::refresh_ns(self.state.output_info.refresh_mhz);
    self.frame_presented(PresentInfo {
      time: clock::monotonic_now(),
      refresh_ns,
      seq: 0,
      flags: PresentationFlags::empty(),
    });
  }

  /// Reports that a frame reached the screen, for example on a DRM page
  /// flip. Surfaces that were visible in it get their frame callbacks,
  /// with `info.time` in milliseconds, and their `wp_presentation`
  /// feedback, with every field of `info`. Hidden surfaces keep their
  /// callbacks for a later frame, and their feedback is discarded. FIFO
  /// and commit-timing barriers waiting for a frame are released.
  ///
  /// Does nothing while the session is inactive
  /// ([`Compositor::set_session_active`]).
  pub fn frame_presented(&mut self, info: PresentInfo) {
    if !input_device::session_active(&self.state) {
      return;
    }
    commit_timing::frame(&mut self.state);
    fifo::frame(&mut self.state);
    let time = clock::millis(info.time);
    let visible = self
      .state
      .visible_surfaces()
      .into_iter()
      .map(|surface| surface.id)
      .collect::<Vec<_>>();
    let pending = std::mem::take(&mut self.state.frame_callbacks);
    let mut hidden = Vec::new();
    for callback in pending {
      if visible.contains(&callback.surface) {
        callback.resource.done(time);
      } else {
        hidden.push(callback);
      }
    }
    self.state.frame_callbacks = hidden;
    presentation::frame(&mut self.state, &info);
  }

  /// Whether a constrained surface update needs a future frame.
  pub fn needs_frame(&self) -> bool {
    commit_timing::has_pending(&self.state) || fifo::has_pending(&self.state)
  }

  pub fn run(&mut self) -> Result<(), Error> {
    while !self.state.shutdown {
      self.poll(None)?;
    }
    Ok(())
  }

  pub fn drain_events(&mut self) -> Vec<Event> {
    let events = self.state.drain_events();
    ipc::record_events(&mut self.state, &events);
    events
  }

  pub fn visible_surfaces(&self) -> Vec<SurfaceSnapshot> {
    self.state.visible_surfaces()
  }

  /// Whether any visible surface has requested asynchronous presentation.
  pub fn allows_tearing(&self) -> bool {
    self.state.visible_surfaces().iter().any(|snapshot| {
      self
        .state
        .surface_ids
        .get(&snapshot.id)
        .and_then(|object| self.state.surfaces.get(object))
        .is_some_and(|surface| surface.tearing)
    })
  }

  /// Returns buffered toplevels eligible for Scale.
  /// The most recently focused windows come first.
  ///
  /// The current output is always used. `all_workspaces` includes non-sticky
  /// windows from other workspaces, and `include_minimized` retains windows
  /// that are absent from the normal visible-surface snapshot.
  pub fn scale_surfaces(
    &self,
    all_workspaces: bool,
    include_minimized: bool,
  ) -> Vec<SurfaceSnapshot> {
    self.state.scale_surfaces(all_workspaces, include_minimized)
  }

  pub fn surface(&self, id: SurfaceId) -> Option<SurfaceSnapshot> {
    self.state.surface(id)
  }

  pub fn is_locked(&self) -> bool {
    self.state.idle.lock_state() != LockState::Unlocked
  }

  pub fn request_lock(&mut self) {
    let commands = self.state.idle.request_lock(self.state.now());
    self.state.process_idle_commands(commands);
  }

  pub fn toggle_idle(&mut self) {
    let (_, commands) = self.state.idle.toggle(self.state.now());
    self.state.process_idle_commands(commands);
  }

  pub fn close_focused(&mut self) -> Option<WindowId> {
    let current = self
      .state
      .core
      .current_workspace(&self.state.output_id)
      .ok()?;
    let window = self
      .state
      .core
      .tree(&self.state.output_id, current)
      .ok()?
      .focused()?;
    self.close_window(window).map(|_| window)
  }

  pub fn close_window(&mut self, window: WindowId) -> Option<()> {
    let resource =
      self
        .state
        .surfaces
        .values()
        .find_map(|surface| match &surface.role {
          shell::Role::Xdg {
            kind: Some(shell::XdgKind::Toplevel(toplevel)),
            ..
          } if toplevel.window() == Some(window) => {
            Some(toplevel.resource.clone())
          }
          _ => None,
        })?;
    resource.close();
    Some(())
  }

  /// Asks the compositor to shut down. The frontend sees this through
  /// [`Compositor::shutdown_requested`].
  pub fn request_quit(&mut self) {
    self.state.shutdown = true;
  }

  /// Returns whether the compositor was asked to shut down, by SIGINT or
  /// SIGTERM or by [`Compositor::request_quit`].
  ///
  /// The compositor blocks SIGINT and SIGTERM on the thread that created it
  /// and reads them from a signalfd while the event loop is dispatched, so
  /// they never terminate the process on their own. A frontend checks this
  /// after each [`Compositor::poll`] or [`Compositor::dispatch_pending`],
  /// then saves its session and exits.
  pub fn shutdown_requested(&self) -> bool {
    self.state.shutdown
  }

  pub fn apply_config(&mut self, config: Config) -> Result<(), Error> {
    if config.workspace_width == 0 || config.workspace_height == 0 {
      return Err(Error::InvalidWorkspaceGrid);
    }
    self.state.background_color = config.background_color;
    self.state.workspace_wraparound = config.workspace_wraparound;
    self.state.tile_by_default = config.tile_by_default;
    self.state.placement_mode = config.placement_mode;
    self.state.focus = config.focus;
    self.state.resize = config.resize;
    self.state.activation.set_config(config.activation);
    self.state.shortcut_inhibitors.set_config(config.shortcuts);
    self.state.all_dialogs_modal = config.all_dialogs_modal;
    self.state.keep_last_toplevel_activated =
      config.keep_last_toplevel_activated;
    self.state.max_buffer_size = config.max_buffer_size;
    for pool in self.state.pools.values() {
      pool.set_max_buffer_size(config.max_buffer_size);
    }
    self.state.preferred_decoration_mode =
      crate::decoration::preferred_mode(&config.preferred_decoration_mode);
    self.state.force_preferred_decoration_mode =
      config.force_preferred_decoration_mode;
    self.state.focus_main_surface_instead_of_popup =
      config.focus_main_surface_instead_of_popup;
    crate::decoration::apply_preferences(&mut self.state);
    self.state.cursor_theme = config.cursor_theme;
    self.state.cursor_size = config.cursor_size;
    input::set_scroll(
      &mut self.state,
      config.mouse_scroll_speed,
      config.mouse_natural_scroll,
    );
    self.state.window_rules = config.window_rules;
    self
      .state
      .set_workspace_grid(config.workspace_width, config.workspace_height)?;
    self.state.gaps = config.gaps;
    self.state.minimum_extent = config.minimum_extent;
    self.state.keep_fullscreen_on_adjacent = config.keep_fullscreen_on_adjacent;
    let windows = self
      .state
      .surfaces
      .values()
      .filter_map(|surface| match &surface.role {
        shell::Role::Xdg {
          kind: Some(shell::XdgKind::Toplevel(toplevel)),
          ..
        } => toplevel.window(),
        _ => None,
      })
      .collect::<Vec<_>>();
    for window in windows {
      self.state.core.set_gaps(window, config.gaps)?;
    }
    let commands = self.state.idle.set_config(config.idle);
    self.state.process_idle_commands(commands);
    self.state.set_output_transform(config.output_transform);
    self.state.set_output_scale(config.output_scale)?;
    let mut output = config.output;
    if output_transform::is_quarter_turn(config.output_transform) {
      std::mem::swap(&mut output.width, &mut output.height);
    }
    self.state.set_output(output)?;
    shell::reconfigure_all(&mut self.state);
    Ok(())
  }

  pub fn unlock(&mut self) {
    self.state.unlock_session();
  }

  pub fn set_dpms(&mut self, state: DpmsState) {
    let commands = self.state.idle.set_dpms(state, self.state.now());
    self.state.process_idle_commands(commands);
  }

  pub fn activity(&mut self) {
    let commands = self.state.idle.activity(self.state.now());
    self.state.process_idle_commands(commands);
  }

  pub fn pointer_motion(&mut self, x: f64, y: f64) {
    input_device::pointer_motion(&mut self.state, x, y);
  }

  pub fn pointer_relative_motion(
    &mut self,
    dx: f64,
    dy: f64,
    dx_unaccel: f64,
    dy_unaccel: f64,
  ) {
    input_device::pointer_relative_motion(
      &mut self.state,
      dx,
      dy,
      dx_unaccel,
      dy_unaccel,
    );
  }

  pub fn pointer_constrained(&self) -> bool {
    pointer_protocols::constrained(&self.state)
  }

  pub fn pointer_swipe_begin(&mut self, fingers: u32) {
    pointer_gestures::swipe_begin(&mut self.state, fingers);
  }

  pub fn pointer_swipe_update(&mut self, dx: f64, dy: f64) {
    pointer_gestures::swipe_update(&mut self.state, dx, dy);
  }

  pub fn pointer_swipe_end(&mut self, cancelled: bool) {
    pointer_gestures::swipe_end(&mut self.state, cancelled);
  }

  pub fn pointer_pinch_begin(&mut self, fingers: u32) {
    pointer_gestures::pinch_begin(&mut self.state, fingers);
  }

  pub fn pointer_pinch_update(
    &mut self,
    dx: f64,
    dy: f64,
    scale: f64,
    rotation: f64,
  ) {
    pointer_gestures::pinch_update(&mut self.state, dx, dy, scale, rotation);
  }

  pub fn pointer_pinch_end(&mut self, cancelled: bool) {
    pointer_gestures::pinch_end(&mut self.state, cancelled);
  }

  pub fn pointer_hold_begin(&mut self, fingers: u32) {
    pointer_gestures::hold_begin(&mut self.state, fingers);
  }

  pub fn pointer_hold_end(&mut self, cancelled: bool) {
    pointer_gestures::hold_end(&mut self.state, cancelled);
  }

  pub fn pointer_button(&mut self, button: u32, pressed: bool) {
    input_device::pointer_button(&mut self.state, button, pressed);
  }

  pub fn pointer_axis(&mut self, horizontal: f64, vertical: f64) {
    input_device::pointer_axis_detailed(
      &mut self.state,
      horizontal,
      vertical,
      0,
      0,
    );
  }

  pub fn pointer_axis_detailed(
    &mut self,
    horizontal: f64,
    vertical: f64,
    horizontal_steps: i32,
    vertical_steps: i32,
  ) {
    input_device::pointer_axis_detailed(
      &mut self.state,
      horizontal,
      vertical,
      horizontal_steps,
      vertical_steps,
    );
  }

  pub fn touch_down(&mut self, id: i32, x: f64, y: f64) {
    input_device::touch_down(&mut self.state, id, x, y);
  }

  pub fn touch_motion(&mut self, id: i32, x: f64, y: f64) {
    input_device::touch_motion(&mut self.state, id, x, y);
  }

  pub fn touch_up(&mut self, id: i32) {
    input_device::touch_up(&mut self.state, id);
  }

  pub fn touch_cancel(&mut self, id: i32) {
    input_device::touch_cancel(&mut self.state, id);
  }

  pub fn keyboard_key(&mut self, key: u32, pressed: bool) {
    input_device::keyboard_key(&mut self.state, key, pressed);
  }

  pub fn keyboard_modifiers(&mut self, modifiers: Modifiers) {
    input_device::keyboard_modifiers(&mut self.state, modifiers);
  }

  /// Tells the compositor whether its seat session is active, as libseat
  /// reports it. A backend calls this with `false` when the session is
  /// paused, for example on a VT switch, and with `true` once it resumes
  /// and can draw again.
  ///
  /// Deactivating releases every key, button and touch point the seat
  /// devices hold, with the same events as disabling them over IPC, so no
  /// client is left with stuck input. While inactive, seat input (including
  /// STIPC input) is dropped and frames send no frame callbacks or
  /// presentation feedback. Clients keep running and their commits are
  /// still processed. Devices disabled over IPC stay disabled either way.
  pub fn set_session_active(&mut self, active: bool) {
    input_device::set_session_active(&mut self.state, active);
  }

  /// Sets when the seat input passed in next happened, as a
  /// `CLOCK_MONOTONIC` time such as libinput's `time_usec`.
  ///
  /// Until it is changed or cleared with `None`, the keyboard, pointer,
  /// gesture and touch entry points send this time to clients, in
  /// milliseconds for `wl_keyboard`, `wl_pointer`, `wl_touch` and pointer
  /// gestures and in microseconds for relative pointer motion. A backend
  /// sets it before feeding one device event and clears it afterwards.
  /// Without it, events carry the `CLOCK_MONOTONIC` time at which the entry
  /// point was called.
  pub fn set_input_time(&mut self, time: Option<Duration>) {
    input::set_event_time(&mut self.state, time);
  }

  pub fn locked_modifiers(&self) -> u32 {
    self.state.input.modifiers.locked
  }

  pub fn shortcuts_inhibited(&self) -> bool {
    shortcuts::active(&self.state)
  }

  pub fn break_shortcuts_inhibit(&mut self) {
    shortcuts::break_grab(&mut self.state);
  }

  pub fn set_keymap(&mut self, keymap: &str) -> Result<(), Error> {
    input::set_keymap(&mut self.state, keymap).map_err(Error::Keymap)
  }

  pub fn set_repeat(&mut self, rate: u32, delay: u32) {
    input::set_repeat(&mut self.state, rate, delay);
  }

  pub fn focus_at(&mut self, x: f64, y: f64) -> Option<WindowId> {
    input::focus_at(&mut self.state, x, y)
  }

  pub fn window_at(&self, x: f64, y: f64) -> Option<WindowId> {
    input::window_at(&self.state, x, y)
  }

  pub fn begin_move_window(&mut self, window: WindowId) {
    self.state.events.push(Event::BeginMove { window });
  }

  pub fn focus_window(&mut self, window: WindowId) -> Option<WindowId> {
    input::focus_window(&mut self.state, window)
  }

  pub fn focused_window(&self) -> Option<WindowId> {
    input::focused_window(&self.state)
  }

  pub fn window_opacity(&self, window: WindowId) -> Option<f32> {
    shell::window_alpha(&self.state, window)
  }

  pub fn set_window_opacity(&mut self, window: WindowId, opacity: f32) -> bool {
    if !opacity.is_finite() || self.window_opacity(window).is_none() {
      return false;
    }
    shell::set_alpha(&mut self.state, window, opacity);
    true
  }

  pub fn adjust_opacity_at(
    &mut self,
    x: f64,
    y: f64,
    delta: f64,
    min_value: f32,
  ) -> Option<f32> {
    if !delta.is_finite() || !min_value.is_finite() {
      return None;
    }
    let window = input::window_at(&self.state, x, y)?;
    let opacity = self.window_opacity(window)?;
    let minimum = min_value.clamp(0.0, 1.0);
    let adjusted = (f64::from(opacity) - delta * 0.003)
      .clamp(f64::from(minimum), 1.0) as f32;
    shell::set_alpha(&mut self.state, window, adjusted);
    Some(adjusted)
  }

  pub fn clamp_opacity(&mut self, min_value: f32) {
    if min_value.is_finite() {
      shell::clamp_alpha(&mut self.state, min_value.clamp(0.0, 1.0));
    }
  }

  pub fn raise_window(&mut self, window: WindowId) -> Result<(), Error> {
    self.state.core.raise_window(window)?;
    self
      .state
      .events
      .push(Event::StackingOrderChanged { window });
    Ok(())
  }

  pub fn lower_window(&mut self, window: WindowId) -> Result<(), Error> {
    self.state.core.lower_window(window)?;
    self
      .state
      .events
      .push(Event::StackingOrderChanged { window });
    Ok(())
  }

  pub fn focus_directional(
    &mut self,
    direction: Direction,
  ) -> Result<Option<WindowId>, Error> {
    input::focus_directional(&mut self.state, direction)
  }

  pub fn switchable_windows(&self) -> Vec<WindowId> {
    if self.state.show_desktop {
      return Vec::new();
    }
    let Ok(workspace) =
      self.state.core.current_workspace(&self.state.output_id)
    else {
      return Vec::new();
    };
    let mut windows = self
      .state
      .core
      .tree(&self.state.output_id, workspace)
      .map(|tree| tree.windows())
      .unwrap_or_default();
    let mut sticky = self
      .state
      .surfaces
      .values()
      .filter_map(|surface| match &surface.role {
        shell::Role::Xdg {
          kind: Some(shell::XdgKind::Toplevel(toplevel)),
          ..
        } => toplevel.window().filter(|window| {
          self.state.core.window_info(*window).is_ok_and(|info| {
            info.state.sticky
              && info.location.output == self.state.output_id
              && info.location.workspace != workspace
          })
        }),
        _ => None,
      })
      .collect::<Vec<_>>();
    sticky.sort_unstable();
    windows.extend(sticky);
    windows.sort_by_key(|window| {
      std::cmp::Reverse(
        self.state.focus_recency.get(window).copied().unwrap_or(0),
      )
    });
    windows
  }

  pub fn cycle_focus(&mut self, reverse: bool) -> Option<WindowId> {
    let windows = self.switchable_windows();
    if windows.is_empty() {
      return None;
    }
    let current = input::focused_window(&self.state);
    let next = match current.and_then(|window| {
      windows.iter().position(|candidate| *candidate == window)
    }) {
      Some(index) if reverse => (index + windows.len() - 1) % windows.len(),
      Some(index) => (index + 1) % windows.len(),
      None if reverse => windows.len() - 1,
      None => 0,
    };
    let window = windows[next];
    if shell::is_minimized(&self.state, window) {
      shell::set_minimized(&mut self.state, window, false).ok()?;
    } else {
      input::focus_window(&mut self.state, window)?;
    }
    Some(window)
  }

  pub fn switch_workspace_relative(
    &mut self,
    reverse: bool,
  ) -> Result<usize, Error> {
    self.switch_workspace_direction(if reverse {
      Direction::Left
    } else {
      Direction::Right
    })
  }

  pub fn switch_workspace_vertical(
    &mut self,
    reverse: bool,
  ) -> Result<usize, Error> {
    self.switch_workspace_direction(if reverse {
      Direction::Up
    } else {
      Direction::Down
    })
  }

  pub fn move_focused_relative(
    &mut self,
    reverse: bool,
  ) -> Result<Option<WindowId>, Error> {
    self.move_focused_direction(if reverse {
      Direction::Left
    } else {
      Direction::Right
    })
  }

  pub fn move_focused_vertical(
    &mut self,
    reverse: bool,
  ) -> Result<Option<WindowId>, Error> {
    self.move_focused_direction(if reverse {
      Direction::Up
    } else {
      Direction::Down
    })
  }

  pub fn send_focused_relative(
    &mut self,
    reverse: bool,
  ) -> Result<Option<WindowId>, Error> {
    self.send_focused_direction(if reverse {
      Direction::Left
    } else {
      Direction::Right
    })
  }

  pub fn send_focused_vertical(
    &mut self,
    reverse: bool,
  ) -> Result<Option<WindowId>, Error> {
    self.send_focused_direction(if reverse {
      Direction::Up
    } else {
      Direction::Down
    })
  }

  pub fn send_focused_to_last_workspace(
    &mut self,
  ) -> Result<Option<WindowId>, Error> {
    let Some(workspace) = self.state.last_workspace else {
      return Ok(None);
    };
    self.move_focused_to_workspace(self.workspace_index(workspace))
  }

  fn send_focused_direction(
    &mut self,
    direction: Direction,
  ) -> Result<Option<WindowId>, Error> {
    let index = self.relative_workspace_index(direction)?;
    self.move_focused_to_workspace(index)
  }

  fn switch_workspace_direction(
    &mut self,
    direction: Direction,
  ) -> Result<usize, Error> {
    let index = self.relative_workspace_index(direction)?;
    self.switch_workspace(index)?;
    Ok(index)
  }

  fn move_focused_direction(
    &mut self,
    direction: Direction,
  ) -> Result<Option<WindowId>, Error> {
    let index = self.relative_workspace_index(direction)?;
    let window = self.move_focused_to_workspace(index)?;
    self.switch_workspace(index)?;
    Ok(window)
  }

  fn relative_workspace_index(
    &self,
    direction: Direction,
  ) -> Result<usize, Error> {
    self.state.relative_workspace_index(direction)
  }

  pub fn switch_to_last_workspace(&mut self) -> Result<Option<usize>, Error> {
    let Some(workspace) = self.state.last_workspace else {
      return Ok(None);
    };
    let index = self.workspace_index(workspace);
    self.switch_workspace(index)?;
    Ok(Some(index))
  }

  pub fn move_focused_to_last_workspace(
    &mut self,
  ) -> Result<Option<WindowId>, Error> {
    let Some(workspace) = self.state.last_workspace else {
      return Ok(None);
    };
    let index = self.workspace_index(workspace);
    let window = self.move_focused_to_workspace(index)?;
    self.switch_workspace(index)?;
    Ok(window)
  }

  pub fn switch_workspace(&mut self, index: usize) -> Result<(), Error> {
    self.state.switch_workspace(index)
  }

  pub fn current_workspace_index(&self) -> Result<usize, Error> {
    let workspace = self.state.core.current_workspace(&self.state.output_id)?;
    Ok(
      workspace.y as usize * self.state.workspace_width as usize
        + workspace.x as usize,
    )
  }

  pub fn workspace_grid_dimensions(&self) -> (u32, u32) {
    (self.state.workspace_width, self.state.workspace_height)
  }

  pub fn move_focused_to_workspace(
    &mut self,
    index: usize,
  ) -> Result<Option<WindowId>, Error> {
    self.state.move_focused_to_workspace(index)
  }

  pub fn move_window_to_workspace(
    &mut self,
    window: WindowId,
    index: usize,
  ) -> Result<(), Error> {
    self.state.move_window_to_workspace(window, index)
  }

  pub fn drop_window(
    &mut self,
    source: WindowId,
    target: WindowId,
    direction: Direction,
  ) -> Result<(), Error> {
    input::drop_window(&mut self.state, source, target, direction)
  }

  pub fn resize_window(
    &mut self,
    window: WindowId,
    direction: Direction,
    delta: i32,
    minimum_extent: u32,
  ) -> Result<bool, Error> {
    input::resize_window(
      &mut self.state,
      window,
      direction,
      delta,
      minimum_extent,
    )
  }

  pub fn resize_floating_from(
    &mut self,
    window: WindowId,
    horizontal: Option<Direction>,
    vertical: Option<Direction>,
    original: Rect,
    delta_x: i32,
    delta_y: i32,
  ) -> Result<bool, Error> {
    input::resize_floating_from(
      &mut self.state,
      window,
      horizontal,
      vertical,
      original,
      delta_x,
      delta_y,
    )
  }

  pub fn move_floating(
    &mut self,
    window: WindowId,
    dx: i32,
    dy: i32,
  ) -> Result<bool, Error> {
    let Some(mut geometry) =
      self.state.core.window_info(window)?.state.floating
    else {
      return Ok(false);
    };
    if dx == 0 && dy == 0 {
      return Ok(false);
    }
    geometry.x = (i64::from(geometry.x) + i64::from(dx)).clamp(
      i64::from(i32::MIN),
      i64::from(i32::MAX) - i64::from(geometry.width),
    ) as i32;
    geometry.y = (i64::from(geometry.y) + i64::from(dy)).clamp(
      i64::from(i32::MIN),
      i64::from(i32::MAX) - i64::from(geometry.height),
    ) as i32;
    self.state.core.set_floating(window, Some(geometry))?;
    shell::sync_floating_geometry(&mut self.state, window, geometry);
    shell::reconfigure_all(&mut self.state);
    Ok(true)
  }

  pub fn move_floating_group(
    &mut self,
    window: WindowId,
    dx: i32,
    dy: i32,
    join_views: bool,
  ) -> Result<bool, Error> {
    let mut windows = vec![window];
    if join_views {
      let mut pending = vec![window];
      while let Some(parent) = pending.pop() {
        for child in self.state.surfaces.values().filter_map(|surface| {
          match &surface.role {
            shell::Role::Xdg {
              kind: Some(shell::XdgKind::Toplevel(toplevel)),
              ..
            } if toplevel.parent == Some(parent) => toplevel.window(),
            _ => None,
          }
        }) {
          if !windows.contains(&child) {
            windows.push(child);
            pending.push(child);
          }
        }
      }
    }
    let mut changed = false;
    for window in windows {
      changed |= self.move_floating(window, dx, dy)?;
    }
    Ok(changed)
  }

  pub fn toggle_maximized(&mut self, window: WindowId) -> Result<bool, Error> {
    input::toggle_maximized(&mut self.state, window)
  }

  pub fn toggle_tiling(&mut self, window: WindowId) -> Result<bool, Error> {
    shell::toggle_tiling(&mut self.state, window)
  }

  pub fn workarea(&self) -> Rect {
    self
      .state
      .core
      .current_workspace(&self.state.output_id)
      .ok()
      .and_then(|workspace| {
        self.state.core.tree(&self.state.output_id, workspace).ok()
      })
      .map_or(self.state.output, |tree| tree.workarea())
  }

  pub fn grid_slot_at(
    &self,
    x: f64,
    y: f64,
    snap_threshold: u32,
    quarter_snap_threshold: u32,
  ) -> Option<&'static str> {
    match shell::grid_slot(
      self.workarea(),
      x,
      y,
      snap_threshold,
      quarter_snap_threshold,
    ) {
      Some("top") => Some("center"),
      slot => slot,
    }
  }

  pub fn workspace_drag_direction_at(
    &self,
    x: f64,
    y: f64,
    snap_threshold: u32,
    quarter_snap_threshold: u32,
  ) -> Option<(i32, i32)> {
    match shell::grid_slot(
      self.workarea(),
      x,
      y,
      snap_threshold,
      quarter_snap_threshold,
    ) {
      Some("top_left") => Some((-1, -1)),
      Some("top") => Some((0, -1)),
      Some("top_right") => Some((1, -1)),
      Some("left") => Some((-1, 0)),
      Some("right") => Some((1, 0)),
      Some("bottom_left") => Some((-1, 1)),
      Some("bottom") => Some((0, 1)),
      Some("bottom_right") => Some((1, 1)),
      _ => None,
    }
  }

  pub fn snap_geometry(&self, slot: &str) -> Option<Rect> {
    shell::snap_geometry(self.workarea(), slot)
  }

  pub fn snap_window(
    &mut self,
    window: WindowId,
    slot: &str,
  ) -> Result<bool, Error> {
    shell::snap_window(&mut self.state, window, slot)
  }

  pub fn restore_snapped(&mut self, window: WindowId) -> Result<bool, Error> {
    if self.state.core.window_info(window)?.state.snapped.is_none() {
      return Ok(false);
    }
    self.toggle_tiling(window)?;
    Ok(true)
  }

  pub fn set_minimized(
    &mut self,
    window: WindowId,
    minimized: bool,
  ) -> Result<(), Error> {
    shell::set_minimized(&mut self.state, window, minimized)
  }

  pub fn minimize_focused(&mut self) -> Option<WindowId> {
    let window = self.focused_window()?;
    self.set_minimized(window, true).ok()?;
    Some(window)
  }

  pub fn set_fullscreen(
    &mut self,
    window: WindowId,
    fullscreen: bool,
  ) -> Result<(), Error> {
    shell::set_fullscreen(&mut self.state, window, fullscreen)
  }

  pub fn toggle_fullscreen(&mut self, window: WindowId) -> Result<bool, Error> {
    let fullscreen = !self.state.core.window_info(window)?.state.fullscreen;
    self.set_fullscreen(window, fullscreen)?;
    Ok(fullscreen)
  }

  pub fn set_sticky(
    &mut self,
    window: WindowId,
    sticky: bool,
  ) -> Result<(), Error> {
    shell::set_sticky(&mut self.state, window, sticky)
  }

  pub fn toggle_sticky(&mut self, window: WindowId) -> Result<bool, Error> {
    let sticky = !self.state.core.window_info(window)?.state.sticky;
    self.set_sticky(window, sticky)?;
    Ok(sticky)
  }

  pub fn set_stacking(
    &mut self,
    window: WindowId,
    mode: StackingMode,
  ) -> Result<(), Error> {
    shell::set_stacking(&mut self.state, window, mode)
  }

  pub fn toggle_always_on_top(
    &mut self,
    window: WindowId,
  ) -> Result<bool, Error> {
    let enabled = self.state.core.window_info(window)?.state.stacking
      != StackingMode::AlwaysOnTop;
    self.set_stacking(
      window,
      if enabled {
        StackingMode::AlwaysOnTop
      } else {
        StackingMode::Normal
      },
    )?;
    Ok(enabled)
  }

  pub fn toggle_always_on_bottom(
    &mut self,
    window: WindowId,
  ) -> Result<bool, Error> {
    let enabled = self.state.core.window_info(window)?.state.stacking
      != StackingMode::AlwaysOnBottom;
    self.set_stacking(
      window,
      if enabled {
        StackingMode::AlwaysOnBottom
      } else {
        StackingMode::Normal
      },
    )?;
    Ok(enabled)
  }

  pub fn set_show_desktop(&mut self, show: bool) {
    if show == self.state.show_desktop {
      return;
    }
    if show {
      shell::enable_show_desktop(&mut self.state);
    } else {
      shell::disable_show_desktop(&mut self.state);
    }
  }

  pub fn toggle_show_desktop(&mut self) -> bool {
    if self.state.show_desktop {
      shell::disable_show_desktop(&mut self.state);
    } else {
      shell::enable_show_desktop(&mut self.state);
    }
    self.state.show_desktop
  }

  pub fn output_scale(&self) -> f64 {
    self.state.output_scale
  }

  pub fn gamma_ramp(&self) -> &[u16] {
    gamma_control::ramp(&self.state)
  }

  pub fn set_output_scale(&mut self, scale: f64) -> Result<(), Error> {
    self.state.set_output_scale(scale)
  }

  pub fn output_transform(
    &self,
  ) -> wayland_server::protocol::wl_output::Transform {
    self.state.output_transform
  }

  pub fn set_output_transform(
    &mut self,
    transform: wayland_server::protocol::wl_output::Transform,
  ) {
    self.state.set_output_transform(transform);
  }

  pub fn logical_output_size(
    &self,
    physical_width: u32,
    physical_height: u32,
  ) -> (u32, u32) {
    output_transform::logical_size(
      physical_width,
      physical_height,
      self.state.output_scale,
      self.state.output_transform,
    )
  }

  pub fn map_output_point(
    &self,
    x: f64,
    y: f64,
    display_width: u32,
    display_height: u32,
  ) -> (f64, f64) {
    let (x, y) = output_transform::map_point(
      x,
      y,
      display_width,
      display_height,
      self.state.output.width,
      self.state.output.height,
      self.state.output_transform,
    );
    (
      x + f64::from(self.state.output.x),
      y + f64::from(self.state.output.y),
    )
  }

  pub fn unmap_output_point(
    &self,
    x: f64,
    y: f64,
    display_width: u32,
    display_height: u32,
  ) -> (f64, f64) {
    output_transform::unmap_point(
      x - f64::from(self.state.output.x),
      y - f64::from(self.state.output.y),
      display_width,
      display_height,
      self.state.output.width,
      self.state.output.height,
      self.state.output_transform,
    )
  }

  pub fn map_output_vector(
    &self,
    x: f64,
    y: f64,
    display_width: u32,
    display_height: u32,
  ) -> (f64, f64) {
    output_transform::map_vector(
      x,
      y,
      display_width,
      display_height,
      self.state.output.width,
      self.state.output.height,
      self.state.output_transform,
    )
  }

  pub fn set_output(&mut self, output: Rect) -> Result<(), Error> {
    self.state.set_output(output)
  }

  /// Makes the compositor draw the cursor theme's default arrow at the
  /// pointer whenever no client cursor applies: over the background, and
  /// over a client that has not set a cursor since the pointer entered it.
  /// A client that hides its cursor still gets none. Nested leaves this off,
  /// because the host draws its own cursor; a native backend turns it on.
  /// The arrow appears among [`Compositor::visible_surfaces`] as a
  /// [`SurfaceRole::Cursor`] surface.
  pub fn set_default_cursor(&mut self, enabled: bool) {
    self.state.default_cursor = enabled;
  }

  /// Returns the description of the output clients currently see.
  pub fn output_info(&self) -> &OutputInfo {
    &self.state.output_info
  }

  /// Describes the physical output to clients: `wl_output` geometry, mode,
  /// name and description, xdg-output, wlr-output-management heads and the
  /// IPC output name. Outputs and heads that clients have already bound get
  /// the new geometry, refresh rate and description at once. Their name
  /// does not change, because Wayland names an output once per binding, so
  /// a backend describes its output before clients connect.
  pub fn set_output_info(&mut self, info: OutputInfo) {
    self.state.set_output_info(info);
  }

  fn workspace_index(&self, workspace: WorkspaceId) -> usize {
    (workspace.y * self.state.workspace_width + workspace.x) as usize
  }

  fn register_security_listeners(&mut self) -> Result<(), Error> {
    let pending = std::mem::take(&mut self.state.security_context.pending);
    for pending in pending {
      let id = pending.id;
      let socket = std::os::unix::net::UnixListener::from(pending.listen_fd);
      socket.set_nonblocking(true)?;
      let close_fd = pending.close_fd;
      self.security_listeners.insert(
        id,
        security_context::SecurityListener {
          socket,
          _close_fd: close_fd,
          context: pending.context,
          listen_source: None,
          close_source: None,
        },
      );
      let listener = &self.security_listeners[&id];
      // SAFETY: the listener and close FD remain owned by this compositor
      // for the lifetime of their registered event sources.
      let listen_wrapper =
        unsafe { FdWrapper::new(listener.socket.as_raw_fd()) };
      let listen_source = self
        .event_loop
        .handle()
        .insert_source(
          Generic::new(listen_wrapper, Interest::READ, Mode::Level),
          move |_, _, state| {
            state.security_context.ready.insert(id);
            Ok(PostAction::Continue)
          },
        )
        .map_err(|error| Error::EventLoop(error.to_string()))?;
      // SAFETY: this wrapper never closes the descriptor stored above.
      let close_wrapper =
        unsafe { FdWrapper::new(listener._close_fd.as_raw_fd()) };
      let close_source = self
        .event_loop
        .handle()
        .insert_source(
          Generic::new(close_wrapper, Interest::READ, Mode::Level),
          move |_, _, state| {
            state.security_context.closed.insert(id);
            state.security_context.ready.insert(id);
            Ok(PostAction::Continue)
          },
        )
        .map_err(|error| Error::EventLoop(error.to_string()))?;
      if let Some(listener) = self.security_listeners.get_mut(&id) {
        listener.listen_source = Some(listen_source);
        listener.close_source = Some(close_source);
      }
    }
    Ok(())
  }

  fn accept_security_clients(&mut self) -> Result<(), Error> {
    let ready = std::mem::take(&mut self.state.security_context.ready);
    let closed = std::mem::take(&mut self.state.security_context.closed);
    for id in closed {
      if let Some(listener) = self.security_listeners.remove(&id) {
        if let Some(source) = listener.listen_source {
          self.event_loop.handle().remove(source);
        }
        if let Some(source) = listener.close_source {
          self.event_loop.handle().remove(source);
        }
      }
    }
    for id in ready {
      let Some(listener) = self.security_listeners.get(&id) else {
        continue;
      };
      let metadata = {
        let Ok(context) = listener.context.lock() else {
          continue;
        };
        if !context.committed {
          continue;
        }
        context.metadata.clone()
      };
      loop {
        let accepted = self.security_listeners[&id].socket.accept();
        let stream = match accepted {
          Ok((stream, _)) => stream,
          Err(error)
            if matches!(error.kind(), std::io::ErrorKind::WouldBlock) =>
          {
            break;
          }
          Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
            continue;
          }
          Err(error) => {
            eprintln!("shingle: sandbox listener {id} accept failed: {error}");
            break;
          }
        };
        let mut launch = launch::capture(&stream);
        if let Some(app_id) = metadata.app_id.clone() {
          launch.app_id = app_id;
        }
        let client = self.display.handle().insert_client(
          stream,
          Arc::new(ClientDataImpl {
            disconnected: self.state.disconnected.clone(),
            sandbox: Some(metadata.clone()),
            privileged_protocols: self
              .state
              .security_context
              .privileged_protocols
              .clone(),
          }),
        )?;
        self.state.launch_templates.insert(client.id(), launch);
        self.state.dispatch_pending = true;
      }
    }
    Ok(())
  }

  fn accept_clients(&mut self) -> Result<(), Error> {
    loop {
      let Some(stream) = self.socket.accept()? else {
        break;
      };
      let launch = launch::capture(&stream);
      let client = self.display.handle().insert_client(
        stream,
        Arc::new(ClientDataImpl {
          disconnected: self.state.disconnected.clone(),
          sandbox: None,
          privileged_protocols: self
            .state
            .security_context
            .privileged_protocols
            .clone(),
        }),
      )?;
      self.state.launch_templates.insert(client.id(), launch);
    }
    self.state.dispatch_pending = true;
    Ok(())
  }
}

/// The event loop's descriptor. It becomes readable whenever
/// [`Compositor::dispatch_pending`] has work: client requests, new
/// connections, SIGINT or SIGTERM, and a 50 ms housekeeping tick that runs
/// idle timeouts and serves IPC clients. A backend with its own event loop
/// polls this descriptor and calls `dispatch_pending` when it is readable,
/// instead of calling [`Compositor::poll`] with a timeout.
impl AsFd for Compositor {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.event_loop.as_fd()
  }
}

/// Creates the timerfd behind the 50 ms housekeeping tick.
fn tick_timer() -> Result<OwnedFd, Error> {
  use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec,
    timerfd_create, timerfd_settime,
  };
  let timer = timerfd_create(
    TimerfdClockId::Monotonic,
    TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
  )
  .map_err(std::io::Error::from)?;
  let period = Timespec {
    tv_sec: 0,
    tv_nsec: 50_000_000,
  };
  timerfd_settime(
    &timer,
    TimerfdTimerFlags::empty(),
    &Itimerspec {
      it_interval: period,
      it_value: period,
    },
  )
  .map_err(std::io::Error::from)?;
  Ok(timer)
}

fn parse_output_transform(
  value: &str,
) -> wayland_server::protocol::wl_output::Transform {
  use wayland_server::protocol::wl_output::Transform;
  match value {
    "90" => Transform::_90,
    "180" => Transform::_180,
    "270" => Transform::_270,
    "flipped" => Transform::Flipped,
    "90_flipped" | "flipped-90" | "flipped90" => Transform::Flipped90,
    "180_flipped" | "flipped-180" | "flipped180" => Transform::Flipped180,
    "270_flipped" | "flipped-270" | "flipped270" => Transform::Flipped270,
    _ => Transform::Normal,
  }
}

impl ClientData for ClientDataImpl {
  fn disconnected(&self, client: ClientId, _reason: DisconnectReason) {
    if let Ok(mut clients) = self.disconnected.lock() {
      clients.push(client);
    }
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
  };

  use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation, wp_presentation_feedback,
  };

  use super::*;

  /// Builds a compositor whose Wayland socket lives in the temporary
  /// directory, so tests need no `XDG_RUNTIME_DIR`. Returns the socket path
  /// for clients to connect to.
  pub(crate) fn test_compositor() -> (Compositor, PathBuf) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
      "shingle-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(&name);
    let socket = ListeningSocket::bind_absolute(path.clone())
      .expect("the temporary directory accepts a test socket");
    let compositor = Compositor::with_socket(
      Config::default(),
      Shingle::new(),
      socket,
      name.into(),
    )
    .expect("the default config builds a compositor");
    (compositor, path)
  }

  /// Dispatches the compositor until `done` holds or a second has passed.
  pub(crate) fn dispatch_until(
    compositor: &mut Compositor,
    mut done: impl FnMut(&Compositor) -> bool,
  ) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
      if done(compositor) {
        return true;
      }
      compositor
        .poll(Some(Duration::from_millis(10)))
        .expect("dispatch succeeds");
    }
    done(compositor)
  }

  /// Returns whether `fd` becomes readable within `timeout`.
  fn readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let timeout = Timespec {
      tv_sec: timeout.as_secs() as i64,
      tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    poll(&mut fds, Some(&timeout)).expect("poll succeeds") > 0
  }

  #[test]
  fn event_loop_fd_is_readable_for_a_connecting_client() {
    let (mut compositor, path) = test_compositor();
    compositor.dispatch_pending().expect("dispatch succeeds");
    let _client = std::os::unix::net::UnixStream::connect(&path)
      .expect("the test socket accepts clients");
    assert!(readable(compositor.as_fd(), Duration::ZERO));
  }

  #[test]
  fn event_loop_fd_is_readable_for_the_housekeeping_tick() {
    let (mut compositor, _) = test_compositor();
    compositor.dispatch_pending().expect("dispatch succeeds");
    assert!(readable(compositor.as_fd(), Duration::from_millis(500)));
    compositor.dispatch_pending().expect("dispatch succeeds");
    // Dispatching consumes the expiration, so the descriptor does not stay
    // readable and spin a backend's loop.
    assert!(!readable(compositor.as_fd(), Duration::ZERO));
  }

  #[test]
  fn set_input_time_stamps_the_following_input() {
    let (mut compositor, _) = test_compositor();
    compositor.set_input_time(Some(Duration::from_micros(7_000_123)));
    assert_eq!(input::timestamp(&compositor.state), 7_000);
    assert_eq!(input::timestamp_usec(&compositor.state), 7_000_123);
    compositor.set_input_time(None);
    assert_ne!(input::timestamp_usec(&compositor.state), 7_000_123);
  }

  fn dell_output() -> OutputInfo {
    OutputInfo {
      name: "DP-1".into(),
      make: "Dell Inc.".into(),
      model: "DELL U2720Q".into(),
      serial: Some("ABC123".into()),
      physical_size_mm: (597, 336),
      subpixel: wayland_server::protocol::wl_output::Subpixel::HorizontalRgb,
      refresh_mhz: 59_951,
    }
  }

  #[test]
  fn output_description_follows_wlroots() {
    assert_eq!(
      dell_output().description(),
      "Dell Inc. DELL U2720Q ABC123 (DP-1)"
    );
    assert_eq!(
      OutputInfo::default().description(),
      "Shingle Headless output (HEADLESS-1)"
    );
  }

  #[test]
  fn wl_output_reports_headless_1_without_output_info() {
    use wayland_client::protocol::wl_output::{Event, WlOutput};
    let (mut compositor, path) = test_compositor();
    let mut client = test_client::TestClient::connect(&mut compositor, &path);
    let _output = client.bind::<WlOutput>(4);
    client.roundtrip(&mut compositor);
    let events = &client.events.output;
    assert!(events.iter().any(
      |event| matches!(event, Event::Name { name } if name == "HEADLESS-1")
    ));
    assert!(events.iter().any(|event| matches!(
      event,
      Event::Geometry {
        physical_width: 310,
        physical_height: 175,
        ..
      }
    )));
    assert!(events.iter().any(|event| matches!(
      event,
      Event::Mode {
        width: 1280,
        height: 720,
        refresh: 60_000,
        ..
      }
    )));
  }

  #[test]
  fn output_info_reaches_wl_output_xdg_output_and_output_management() {
    use wayland_client::{
      WEnum,
      protocol::wl_output::{Event, Subpixel, WlOutput},
    };
    use wayland_protocols::xdg::xdg_output::zv1::client::{
      zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1,
    };
    use wayland_protocols_wlr::output_management::v1::client::{
      zwlr_output_head_v1, zwlr_output_manager_v1::ZwlrOutputManagerV1,
      zwlr_output_mode_v1,
    };
    let (mut compositor, path) = test_compositor();
    compositor.set_output_info(dell_output());
    assert_eq!(compositor.output_info(), &dell_output());
    let mut client = test_client::TestClient::connect(&mut compositor, &path);
    let output = client.bind::<WlOutput>(4);
    let manager = client.bind::<ZxdgOutputManagerV1>(3);
    manager.get_xdg_output(&output, &client.handle, ());
    let _heads = client.bind::<ZwlrOutputManagerV1>(4);
    client.roundtrip(&mut compositor);
    let events = &client.events;
    assert!(events.output.iter().any(|event| matches!(
      event,
      Event::Geometry {
        physical_width: 597,
        physical_height: 336,
        subpixel: WEnum::Value(Subpixel::HorizontalRgb),
        make,
        model,
        ..
      } if make == "Dell Inc." && model == "DELL U2720Q"
    )));
    assert!(events.output.iter().any(|event| matches!(
      event,
      Event::Mode {
        refresh: 59_951,
        ..
      }
    )));
    assert!(
      events
        .output
        .iter()
        .any(|event| matches!(event, Event::Name { name } if name == "DP-1"))
    );
    assert!(events.output.iter().any(|event| matches!(
      event,
      Event::Description { description }
        if description == "Dell Inc. DELL U2720Q ABC123 (DP-1)"
    )));
    assert!(events.xdg_output.iter().any(|event| matches!(
      event,
      zxdg_output_v1::Event::Name { name } if name == "DP-1"
    )));
    let head_has = |expected: &dyn Fn(&zwlr_output_head_v1::Event) -> bool| {
      events.heads.iter().any(expected)
    };
    assert!(head_has(&|event| matches!(
      event,
      zwlr_output_head_v1::Event::Name { name } if name == "DP-1"
    )));
    assert!(head_has(&|event| matches!(
      event,
      zwlr_output_head_v1::Event::Make { make } if make == "Dell Inc."
    )));
    assert!(head_has(&|event| matches!(
      event,
      zwlr_output_head_v1::Event::SerialNumber { serial_number }
        if serial_number == "ABC123"
    )));
    assert!(head_has(&|event| matches!(
      event,
      zwlr_output_head_v1::Event::PhysicalSize {
        width: 597,
        height: 336
      }
    )));
    assert!(events.modes.iter().any(|event| matches!(
      event,
      zwlr_output_mode_v1::Event::Refresh { refresh: 59_951 }
    )));
  }

  #[test]
  fn output_info_updates_bound_outputs_but_keeps_their_name() {
    use wayland_client::protocol::wl_output::{Event, WlOutput};
    let (mut compositor, path) = test_compositor();
    let mut client = test_client::TestClient::connect(&mut compositor, &path);
    let _output = client.bind::<WlOutput>(4);
    client.roundtrip(&mut compositor);
    client.events.output.clear();
    compositor.set_output_info(dell_output());
    client.roundtrip(&mut compositor);
    let events = &client.events.output;
    assert!(events.iter().any(|event| matches!(
      event,
      Event::Mode {
        refresh: 59_951,
        ..
      }
    )));
    assert!(events.iter().any(|event| matches!(
      event,
      Event::Description { description } if description.ends_with("(DP-1)")
    )));
    assert!(matches!(events.last(), Some(Event::Done)));
    assert!(
      !events
        .iter()
        .any(|event| matches!(event, Event::Name { .. }))
    );
  }

  /// A client with a visible cursor surface that has asked for a frame
  /// callback and presentation feedback.
  fn client_waiting_for_a_frame(
    compositor: &mut Compositor,
    path: &std::path::Path,
  ) -> test_client::TestClient {
    let mut client = test_client::TestClient::connect(compositor, path);
    let surface = client.cursor_surface(compositor);
    let presentation = client.bind::<WpPresentation>(2);
    request_frame(&mut client, compositor, &surface, &presentation);
    client
  }

  fn request_frame(
    client: &mut test_client::TestClient,
    compositor: &mut Compositor,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
    presentation: &WpPresentation,
  ) {
    surface.frame(&client.handle, test_client::Frame);
    presentation.feedback(surface, &client.handle, ());
    surface.commit();
    client.roundtrip(compositor);
  }

  #[test]
  fn frame_presented_sends_the_page_flip_to_clients() {
    use wp_presentation_feedback::{Event, Kind};
    let (mut compositor, path) = test_compositor();
    let mut client = client_waiting_for_a_frame(&mut compositor, &path);
    assert!(client.events.frame_done.is_empty());
    compositor.frame_presented(PresentInfo {
      time: Duration::new(5, 250_000_123),
      refresh_ns: 16_666_667,
      seq: (1 << 32) + 2,
      flags: PresentationFlags::Vsync | PresentationFlags::HwClock,
    });
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.frame_done, [5_250]);
    let presented = client
      .events
      .feedback
      .iter()
      .find(|event| matches!(event, Event::Presented { .. }));
    let Some(Event::Presented {
      tv_sec_hi,
      tv_sec_lo,
      tv_nsec,
      refresh,
      seq_hi,
      seq_lo,
      flags,
    }) = presented
    else {
      panic!("no presented event in {:?}", client.events.feedback);
    };
    assert_eq!(
      (*tv_sec_hi, *tv_sec_lo, *tv_nsec, *refresh, *seq_hi, *seq_lo),
      (0, 5, 250_000_123, 16_666_667, 1, 2)
    );
    assert_eq!(
      flags,
      &wayland_client::WEnum::Value(Kind::Vsync | Kind::HwClock)
    );
  }

  #[test]
  fn frame_uses_the_output_refresh_rate_and_monotonic_clock() {
    use wp_presentation_feedback::Event;
    let (mut compositor, path) = test_compositor();
    compositor.set_output_info(OutputInfo {
      refresh_mhz: 59_951,
      ..Default::default()
    });
    let mut client = client_waiting_for_a_frame(&mut compositor, &path);
    let before = clock::monotonic_now();
    compositor.frame();
    client.roundtrip(&mut compositor);
    let Some(Event::Presented {
      tv_sec_lo,
      refresh,
      seq_lo,
      ..
    }) = client.events.feedback.first()
    else {
      panic!("no presented event in {:?}", client.events.feedback);
    };
    assert_eq!((*refresh, *seq_lo), (16_680_289, 0));
    assert!(u64::from(*tv_sec_lo) >= before.as_secs());
    assert_eq!(client.events.frame_done.len(), 1);
    assert!(client.events.frame_done[0] >= clock::millis(before));
  }

  #[test]
  fn inactive_session_withholds_frames_until_resumed() {
    use wp_presentation_feedback::Event;
    let (mut compositor, path) = test_compositor();
    let mut client = client_waiting_for_a_frame(&mut compositor, &path);
    compositor.set_session_active(false);
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert!(client.events.frame_done.is_empty());
    assert!(client.events.feedback.is_empty());
    compositor.set_session_active(true);
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.frame_done.len(), 1);
    assert!(
      client
        .events
        .feedback
        .iter()
        .any(|event| matches!(event, Event::Presented { .. }))
    );
  }

  #[test]
  fn client_cursor_replaces_the_default_cursor() {
    let (mut compositor, path) = test_compositor();
    let cursors = |compositor: &Compositor| {
      compositor
        .visible_surfaces()
        .into_iter()
        .filter(|surface| surface.role == SurfaceRole::Cursor)
        .map(|surface| surface.id)
        .collect::<Vec<_>>()
    };
    assert!(cursors(&compositor).is_empty());
    compositor.set_default_cursor(true);
    let default = cursors(&compositor);
    assert_eq!(default.len(), 1);
    let mut client = test_client::TestClient::connect(&mut compositor, &path);
    client.cursor_surface(&mut compositor);
    let current = cursors(&compositor);
    assert_eq!(current.len(), 1);
    assert_ne!(current, default);
  }

  #[test]
  fn with_socket_path_binds_the_given_path() {
    let path = std::env::temp_dir()
      .join(format!("shingle-socket-path-{}", std::process::id()));
    let mut compositor = Compositor::with_socket_path(Config::default(), &path)
      .expect("the temporary directory accepts a socket");
    assert_eq!(compositor.socket_name(), path.as_os_str());
    let client = test_client::TestClient::connect(&mut compositor, &path);
    assert!(
      client
        .events
        .globals
        .iter()
        .any(|(_, interface, _)| interface == "wl_seat")
    );
    drop(client);
    drop(compositor);
    assert!(!path.exists());
  }

  #[test]
  fn shutdown_is_not_requested_initially() {
    let (mut compositor, _) = test_compositor();
    compositor.dispatch_pending().expect("dispatch succeeds");
    assert!(!compositor.shutdown_requested());
  }

  #[test]
  fn request_quit_requests_shutdown() {
    let (mut compositor, _) = test_compositor();
    compositor.request_quit();
    assert!(compositor.shutdown_requested());
  }

  #[test]
  fn sigterm_and_sigint_request_shutdown() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
      let (mut compositor, _) = test_compositor();
      // SAFETY: raise() directs the signal at this thread, which the
      // compositor's signal source has blocked, so the signal stays pending
      // for the signalfd instead of terminating the test process.
      assert_eq!(unsafe { libc::raise(signal) }, 0);
      assert!(
        dispatch_until(&mut compositor, Compositor::shutdown_requested),
        "signal {signal} did not request shutdown"
      );
    }
  }
}
