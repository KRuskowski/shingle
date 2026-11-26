//! Shingle's Wayland compositor: protocol handling, input routing, output
//! state and Wayfire-compatible IPC around the window model in
//! [`shingle_core`].
//!
//! It draws nothing and owns no display or input hardware. A host (the
//! nested winit frontend today, a native DRM/libinput backend later) owns
//! a [`Compositor`] and drives it:
//!
//! 1. Build it with [`Compositor::new`], or [`Compositor::with_core`] to
//!    restore a saved session. It starts with one output, `HEADLESS-1`;
//!    describe it with [`Compositor::set_output_info`], add more with
//!    [`Compositor::add_output`], and once every output is there settle a
//!    restored session with [`Compositor::restore_outputs`]. Optionally
//!    start IPC with
//!    [`Compositor::enable_ipc`]. Start every client in
//!    [`Compositor::client_environment`], which exports
//!    [`Compositor::socket_name`] as `WAYLAND_DISPLAY`.
//! 2. Register every seat device with [`Compositor::add_input_device`] and
//!    pass the returned [`InputDeviceId`] with each keyboard, pointer,
//!    gesture and touch event. Coordinates are in the output layout's
//!    logical pixels; a host converts from its own pixels with
//!    [`Compositor::map_output_point`]. [`Compositor::set_input_time`]
//!    supplies the device's `CLOCK_MONOTONIC` timestamp.
//! 3. Run the event loop: either call [`Compositor::poll`] with a timeout,
//!    or poll the descriptor from [`AsFd`] in the host's own loop and call
//!    [`Compositor::dispatch_pending`] whenever it is readable.
//! 4. After dispatching, take [`Compositor::drain_events`] and draw
//!    [`Compositor::visible_surfaces`] bottom to top. Report each frame
//!    that reached the screen with [`Compositor::frame_presented`], or
//!    [`Compositor::frame`] when the host knows nothing more precise, so
//!    clients get their frame callbacks and presentation feedback.
//! 5. Call [`Compositor::set_session_active`] when the seat session is
//!    paused or resumed, and stop once
//!    [`Compositor::shutdown_requested`] is true.
//!
//! Window state lives in [`shingle_core::Shingle`], which the compositor
//! keeps in step with the XDG roles; [`Compositor::core`] exposes it for
//! session saving. New toplevels can be held back from mapping by IPC
//! clients watching Wayfire's `view-pre-map` event until each of them calls
//! `window-rules/unblock-map` (docs/ipc.md, "Map blocking"); until then
//! they produce no [`Event::WindowCreated`], are not drawn and take no
//! tile. The behaviour follows Wayfire's `plugins/ipc-rules/ipc-events.hpp`.

#![warn(missing_docs)]

mod activation;
mod alpha_modifier;
mod buffer;
mod buffer_lock;
mod clock;
mod commit_queue;
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
mod linux_dmabuf;
mod linux_drm_syncobj;
mod output;
mod output_focus_api;
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
mod surface_outputs;
mod tearing;
#[cfg(test)]
mod test_client;
mod text_input;
mod toplevel_icon;
pub mod trace;
mod viewport;
mod virtual_keyboard;
mod virtual_pointer;
#[cfg(test)]
mod wayland_pins;
mod window_rules;
mod xdg_output;
mod xwayland;

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
pub use input::{Axis, AxisSource, Modifiers};
pub use input_device::{InputDeviceId, InputDeviceInfo, InputDeviceKind};
pub use output::{
  MoveGeometry, OutputAction, OutputHandle, OutputInfo, OutputKind,
  OutputModeInfo, OutputPlacement, OutputRequest, OutputRequestId,
  OutputSwitch, OutputView, RequestedMode,
};
pub use shingle_core::{Direction, StackingMode, WindowId};
use shingle_core::{Gaps, OutputId, Rect, Shingle, WorkspaceId};
use shingle_idle::{Config as IdleConfig, IdleCoordinator};
pub use shingle_idle::{DpmsState, LockState};
pub use shingle_session::ClientEnvironment;
use wayland_server::{
  Display, ListeningSocket,
  backend::{ClientData, ClientId, DisconnectReason},
};

pub use buffer::ShmContent;
pub use buffer_lock::BufferLock;
pub use linux_dmabuf::{
  DmabufBuffer, DmabufFeedback, DmabufFormat, DmabufImporter, DmabufPlane,
  fourcc_name,
};
pub use screencopy::CaptureId;
pub use state::SurfaceId;
use state::{ClientDataImpl, CompositorState};
pub use xwayland::XwaylandSettings;

/// The output name used until a backend calls
/// [`Compositor::set_output_info`], which renames the core's output after
/// the backend's output, so sessions are saved per connector name.
pub const OUTPUT_NAME: &str = "HEADLESS-1";
/// Default logical output width in pixels, used by [`Config::default`].
pub const OUTPUT_WIDTH: u32 = 1280;
/// Default logical output height in pixels, used by [`Config::default`].
pub const OUTPUT_HEIGHT: u32 = 720;

/// The current `CLOCK_MONOTONIC` time, the clock of [`PresentInfo::time`]
/// and of input event times, for backends that report frames themselves.
pub fn monotonic_time() -> Duration {
  clock::monotonic_now()
}

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

/// What a directional workspace switch does at the edge of the grid
/// (`workspaces.wraparound` in the YAML config).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkspaceWraparound {
  /// Stay on the current workspace, like Wayfire's vswitch with
  /// `vswitch/wraparound = false`.
  #[default]
  Disabled,
  /// Wrap within the same row or column, like Wayfire's vswitch with
  /// `vswitch/wraparound = true`
  /// (`plugins/vswitch/wayfire/plugins/vswitch.hpp`, `handle_dir`).
  Grid,
  /// Shingle's `on_end`: treat the grid as one row-major list, so moving
  /// right from the end of a row goes to the start of the next row, and
  /// past the last workspace to the first. Vertical moves do not wrap.
  OnEnd,
}

/// Compositor settings, normally converted from
/// [`shingle_config::ShingleConfig`] with `From`.
///
/// [`Compositor::apply_config`] re-applies most fields at runtime. The
/// exceptions are only read at startup: `enable_text_input_v3`,
/// `initial_locked_modifiers`, `disable_primary_selection`,
/// `security_privileged_protocols` and everything in `xwayland` except the
/// startup script.
#[derive(Clone, Debug)]
pub struct Config {
  /// Logical size before the output transform, so a quarter-turn
  /// transform swaps width and height. Physical pixels are this size times
  /// `output_scale`. Its position is ignored: `output_position` places the
  /// output.
  pub output: Rect,
  /// Where outputs without a named entry go in the layout: `auto`, after
  /// the fixed outputs from left to right, or a fixed point.
  pub output_position: shingle_config::OutputPosition,
  /// What happens to the windows of an output that goes
  /// (`output_layout.unplug`).
  pub output_unplug: shingle_config::UnplugPolicy,
  /// Milliseconds within which an output that comes back is focused again
  /// if it was the active output when it went
  /// (`output_layout.last_output_focus_timeout`).
  pub last_output_focus_timeout: u64,
  /// Wayfire's `oswitch` bindings, which focus another output and can take
  /// the focused window along.
  pub oswitch: shingle_config::OswitchConfig,
  /// Wayfire's `wsets` bindings, which show a workspace set on the active
  /// output or send the focused window to one.
  pub wsets: shingle_config::WsetsConfig,
  /// The output each named absolute input device maps to
  /// (`input_devices.NAME.output`).
  pub input_device_outputs: std::collections::BTreeMap<String, String>,
  /// Draws windows on every output they overlap instead of only on the
  /// output that shows their workspace set (`behavior.remove_output_limits`,
  /// Wayfire's `workarounds/remove_output_limits`).
  pub remove_output_limits: bool,
  /// Background clear colour as RGBA, each component in `0.0..=1.0`.
  pub background_color: [f64; 4],
  /// Columns of the workspace grid; must be nonzero.
  pub workspace_width: u32,
  /// Rows of the workspace grid; must be nonzero.
  pub workspace_height: u32,
  /// Edge behaviour of directional workspace switches.
  pub workspace_wraparound: WorkspaceWraparound,
  /// Output scale; must be finite and positive. `wl_output.scale` gets it
  /// rounded up, fractional-scale clients the exact value.
  pub output_scale: f64,
  /// Output transform, as `wl_output` reports it.
  pub output_transform: wayland_server::protocol::wl_output::Transform,
  /// Refresh rate in millihertz clients are told while no backend
  /// describes the output, as in nested and headless; zero keeps 60 Hz.
  /// From an explicit `output.mode` such as `1920x1080@75`.
  pub output_refresh_mhz: u32,
  /// Placements of named outputs, keyed by output name, which replace
  /// `output`, `output_scale` and `output_transform` while the output has
  /// that name. Converted from [`shingle_config::ShingleConfig::outputs`].
  pub output_placements: std::collections::BTreeMap<String, OutputPlacement>,
  /// Multiplier for pointer axis values; negative or non-finite values are
  /// ignored and keep the previous speed.
  pub mouse_scroll_speed: f64,
  /// Inverts pointer axis direction.
  pub mouse_natural_scroll: bool,
  /// XCursor theme name for compositor-drawn cursors.
  pub cursor_theme: String,
  /// Nominal XCursor size for compositor-drawn cursors.
  pub cursor_size: u32,
  /// Advertises text-input-v3, input-method v1/v2 and input panels. Read
  /// only when the globals are created.
  pub enable_text_input_v3: bool,
  /// xkb locked-modifier mask applied at startup: bit 1 (Lock) for Caps
  /// Lock and bit 4 (Mod2) for Num Lock.
  pub initial_locked_modifiers: u32,
  /// Idle, lock and DPMS timeouts for the [`IdleCoordinator`].
  pub idle: IdleConfig,
  /// Inner and outer tiling gaps.
  pub gaps: Gaps,
  /// Smallest width or height in logical pixels a tile or interactively
  /// resized floating window may get.
  pub minimum_extent: u32,
  /// Interactive resize limits (Wayfire's `resize` plugin options).
  pub resize: shingle_config::ResizeConfig,
  /// Wayfire's `simple-tile/keep_fullscreen_on_adjacent`: directional
  /// focus from a fullscreen view makes the newly focused view fullscreen.
  pub keep_fullscreen_on_adjacent: bool,
  /// Wayfire's `simple-tile/tile_by_default`: which new views tile.
  pub tile_by_default: shingle_config::ViewMatcher,
  /// Wayfire's `place/mode` for new floating views: `center`, `cascade`,
  /// `random` or `maximize`.
  pub placement_mode: String,
  /// Which pointer buttons focus the view under the pointer, and whether
  /// that click is also passed to the client.
  pub focus: shingle_config::FocusConfig,
  /// xdg-activation focus-stealing rules.
  pub activation: shingle_config::ActivationConfig,
  /// Keyboard-shortcuts-inhibit options (`shortcuts-inhibit/*`).
  pub shortcuts: shingle_config::ShortcutConfig,
  /// Wayfire's `workarounds/all_dialogs_modal`: focusing a parent focuses
  /// its topmost child dialog instead (`src/core/seat/seat.cpp`,
  /// `focus_view`).
  pub all_dialogs_modal: bool,
  /// Wayfire's `workarounds/keep_last_toplevel_activated`: a layer surface
  /// taking keyboard focus leaves the last toplevel activated.
  pub keep_last_toplevel_activated: bool,
  /// Largest shared-memory buffer width or height accepted, in pixels
  /// (`workarounds/max_buffer_size`); larger buffers are rejected when
  /// the client creates them.
  pub max_buffer_size: u32,
  /// `server` asks clients for server-side decorations; anything else
  /// means client-side.
  pub preferred_decoration_mode: String,
  /// Ignores clients' xdg-decoration mode requests and always sends the
  /// preferred mode.
  pub force_preferred_decoration_mode: bool,
  /// Wayfire's `workarounds/focus_main_surface_instead_of_popup`: keyboard
  /// focus goes to a popup's main surface, for Qt clipboard bugs.
  pub focus_main_surface_instead_of_popup: bool,
  /// Does not advertise primary selection. Read only at startup.
  pub disable_primary_selection: bool,
  /// Global interface names hidden from clients that connect through a
  /// security-context-v1 listener (Wayfire's
  /// `security-context-v1/privileged_protocols`). Read only at startup.
  pub security_privileged_protocols: HashSet<String>,
  /// Window rules evaluated on view events.
  pub window_rules: Vec<shingle_config::WindowRule>,
  /// The X display and Xwayland. [`Config::default`] disables them;
  /// `From<ShingleConfig>` takes `xwayland` from the configuration, whose
  /// default is `lazy`.
  pub xwayland: XwaylandSettings,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      output: Rect::new(0, 0, OUTPUT_WIDTH, OUTPUT_HEIGHT),
      background_color: [0.1, 0.1, 0.1, 1.0],
      workspace_width: 10,
      workspace_height: 1,
      workspace_wraparound: WorkspaceWraparound::Disabled,
      output_position: shingle_config::OutputPosition::Auto,
      output_unplug: shingle_config::UnplugPolicy::Return,
      last_output_focus_timeout: 10_000,
      oswitch: shingle_config::OswitchConfig::default(),
      wsets: shingle_config::WsetsConfig::default(),
      input_device_outputs: std::collections::BTreeMap::new(),
      remove_output_limits: false,
      output_scale: 1.0,
      output_transform: wayland_server::protocol::wl_output::Transform::Normal,
      output_refresh_mhz: 0,
      output_placements: std::collections::BTreeMap::new(),
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
      xwayland: XwaylandSettings::default(),
    }
  }
}

impl From<shingle_config::ShingleConfig> for Config {
  fn from(config: shingle_config::ShingleConfig) -> Self {
    let output_placements = config
      .outputs
      .iter()
      .filter_map(|output| {
        let name = output.name.clone()?;
        Some((name, OutputPlacement::from_config(output)))
      })
      .collect();
    let (width, height) = output::configured_size(&config.output);
    Self {
      output: Rect::new(config.output.x, config.output.y, width, height),
      output_refresh_mhz: output::explicit_refresh(&config.output),
      output_position: config.output.position,
      output_unplug: config.output_layout.unplug,
      last_output_focus_timeout: config.output_layout.last_output_focus_timeout,
      oswitch: config.oswitch.clone(),
      wsets: config.wsets.clone(),
      input_device_outputs: config
        .input_devices
        .iter()
        .filter_map(|(name, device)| {
          Some((name.clone(), device.output.clone()?))
        })
        .collect(),
      remove_output_limits: config.behavior.remove_output_limits,
      output_placements,
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
      xwayland: XwaylandSettings {
        mode: config.xwayland.mode,
        startup_script: config.xwayland.startup_script,
        discard_script_output: config.behavior.discard_command_output,
        ..XwaylandSettings::default()
      },
    }
  }
}

/// When a binding registered with `command/register-binding` fires, from
/// its optional `mode` field (Wayfire's `plugins/single_plugins/command.cpp`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBindingMode {
  /// On press only; key repeat does not fire it again.
  Normal,
  /// On press and on every key repeat.
  Repeat,
  /// On release.
  Release,
}

/// A key binding an IPC client registered with `command/register-binding`.
///
/// Exactly one of three actions runs when it fires: `command` is spawned
/// by the frontend, `call_method` is dispatched as an IPC request with
/// `call_data`, and with neither the registering client gets a
/// `command-binding` event (see [`Compositor::trigger_runtime_binding`]).
/// Bindings of the third kind are removed when their client disconnects.
#[derive(Clone, Debug)]
pub struct RuntimeBinding {
  /// The `binding-id` returned to the client; counts up from 1 per IPC
  /// server.
  pub id: u64,
  /// The parsed key combination.
  pub binding: shingle_config::Binding,
  /// Shell command to spawn in [`Compositor::client_environment`].
  pub command: Option<String>,
  /// IPC method to call, such as `wm-actions/toggle_fullscreen`.
  pub call_method: Option<String>,
  /// The `data` object for `call_method`.
  pub call_data: Option<serde_json::Value>,
  /// Press, repeat or release.
  pub mode: RuntimeBindingMode,
  /// Wayfire's `exec-always`: also fires while the session is locked.
  pub exec_always: bool,
  pub(crate) client_id: Option<u64>,
}

/// The Wayland role of a surface, as [`SurfaceSnapshot`] reports it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SurfaceRole {
  /// No role yet, or a role Shingle does not draw by itself (such as a
  /// drag icon).
  Unassigned,
  /// An `xdg_toplevel`; its snapshot carries the [`WindowId`].
  Toplevel,
  /// An `xdg_popup`, placed relative to its parent surface.
  Popup,
  /// A `wl_subsurface`, placed relative to its parent surface.
  Subsurface,
  /// A wlr-layer-shell surface or an input-method panel.
  LayerShell,
  /// A client cursor, a cursor-shape image or the default arrow from
  /// [`Compositor::set_default_cursor`].
  Cursor,
  /// An `ext_session_lock_surface_v1`, covering the whole output.
  SessionLock,
  /// An unmanaged X window, such as an override-redirect menu or
  /// tooltip, drawn above the Top layer at the position the X client
  /// chose. It has no [`WindowId`]; its snapshot carries the X class as
  /// `app_id`.
  Unmanaged,
}

/// What a [`BufferSnapshot`] holds of its buffer (docs/design/dmabuf.md,
/// sections 4 and 5).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SnapshotContent {
  /// Tightly packed 8-bit RGBA rows, top to bottom, copied when the commit
  /// applied, so the buffer is released to its client at once. ARGB/XRGB
  /// formats are swizzled, X formats get opaque alpha, and colour stays
  /// premultiplied as `wl_shm` defines it. Single-pixel buffers are copied
  /// like this, and shared-memory buffers too unless
  /// [`Compositor::set_shm_upload`] is on.
  Pixels(Arc<[u8]>),
  /// A shared-memory buffer, still in the client's pool, that the
  /// frontend uploads to the GPU and then releases, with
  /// [`Compositor::set_shm_upload`] on.
  Shm(ShmContent),
  /// A client DMA-BUF, which only the GPU reads: the frontend imports it
  /// (or finds it in its cache by serial) and samples it.
  Dmabuf(DmabufContent),
}

/// A committed DMA-BUF and the lock that keeps it from its client while
/// it is shown. Every clone of the snapshot is a further lock: a frontend
/// keeps one for every frame that samples the buffer until the GPU has
/// finished that frame, and must not keep snapshots longer than that.
#[derive(Clone, Debug)]
pub struct DmabufContent {
  /// The buffer, with its planes' fds.
  pub buffer: Arc<DmabufBuffer>,
  /// The buffer's lock.
  pub lock: BufferLock,
}

/// A surface's committed buffer, as a frontend draws it.
#[derive(Clone)]
pub struct BufferSnapshot {
  /// Buffer width in buffer pixels.
  pub width: u32,
  /// Buffer height in buffer pixels.
  pub height: u32,
  /// wp_viewporter source rectangle `[x, y, width, height]` in buffer
  /// pixels (already multiplied by the buffer scale), or `None` for the
  /// whole buffer.
  pub source: Option<[f64; 4]>,
  /// `wl_surface.set_buffer_scale`; the buffer size is a multiple of it.
  pub scale: i32,
  /// `wl_surface.set_buffer_transform`.
  pub transform: wayland_server::protocol::wl_output::Transform,
  /// The buffer's pixels, or where to find them.
  pub content: SnapshotContent,
  /// Rectangles in buffer pixels that changed in this commit, clipped to
  /// the buffer. `wl_surface.damage` is converted from surface coordinates
  /// through the viewport, buffer scale and buffer transform. A new buffer
  /// committed without damage is fully damaged.
  pub damage: Vec<Rect>,
  /// `wl_surface.offset` (or `attach` x) of this commit, in surface-local
  /// coordinates; zero when the commit carried none.
  pub offset_x: i32,
  /// `wl_surface.offset` (or `attach` y) of this commit.
  pub offset_y: i32,
}

impl std::fmt::Debug for BufferSnapshot {
  /// Pixel content prints as `rgba`, the field `content` replaced, so that
  /// recorded transcripts such as crates/shingle/testdata/wayland_pins.txt
  /// still compare.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut debug = formatter.debug_struct("BufferSnapshot");
    debug
      .field("width", &self.width)
      .field("height", &self.height)
      .field("source", &self.source)
      .field("scale", &self.scale)
      .field("transform", &self.transform);
    match &self.content {
      SnapshotContent::Pixels(rgba) => debug.field("rgba", rgba),
      SnapshotContent::Shm(content) => debug.field("shm", content),
      SnapshotContent::Dmabuf(content) => {
        debug.field("dmabuf", &content.buffer)
      }
    };
    debug
      .field("damage", &self.damage)
      .field("offset_x", &self.offset_x)
      .field("offset_y", &self.offset_y)
      .finish()
  }
}

impl BufferSnapshot {
  /// Bytes per row of [`BufferSnapshot::rgba`].
  pub fn stride(&self) -> u32 {
    self.width * 4
  }

  /// The buffer as tightly packed RGBA rows, as described for
  /// [`SnapshotContent::Pixels`], or `None` for content the CPU cannot
  /// read: a DMA-BUF, or a shared-memory buffer the frontend has already
  /// uploaded and released. A shared-memory buffer that is still locked
  /// is converted now, on every call.
  pub fn rgba(&self) -> Option<Arc<[u8]>> {
    match &self.content {
      SnapshotContent::Pixels(rgba) => Some(Arc::clone(rgba)),
      SnapshotContent::Shm(content) => content.rgba().map(Arc::from),
      SnapshotContent::Dmabuf(_) => None,
    }
  }

  /// The DMA-BUF this snapshot shows, if it is one.
  pub fn dmabuf(&self) -> Option<&DmabufContent> {
    match &self.content {
      SnapshotContent::Dmabuf(content) => Some(content),
      _ => None,
    }
  }

  /// The shared-memory buffer this snapshot shows for upload, if it is
  /// one.
  pub fn shm(&self) -> Option<&ShmContent> {
    match &self.content {
      SnapshotContent::Shm(content) => Some(content),
      _ => None,
    }
  }
}

/// One square image from `xdg_toplevel_icon_v1.add_buffer`.
#[derive(Clone, Debug)]
pub struct ToplevelIconBuffer {
  /// Width and height in buffer pixels; icon buffers must be square.
  pub width: u32,
  /// The scale the image is meant for.
  pub scale: i32,
  /// Tightly packed RGBA rows, converted as in [`SnapshotContent::Pixels`].
  pub rgba: Arc<[u8]>,
}

/// A toplevel's icon from xdg-toplevel-icon-v1.
#[derive(Clone, Debug)]
pub struct ToplevelIcon {
  /// An icon-theme name, which a frontend prefers when it can resolve it.
  pub name: Option<String>,
  /// Pixel icons at one or more sizes and scales.
  pub buffers: Vec<ToplevelIconBuffer>,
}

/// What a frontend needs to draw one surface.
#[derive(Clone, Debug)]
pub struct SurfaceSnapshot {
  /// Stable for the surface's lifetime. Cursor-shape images use synthetic
  /// IDs near `u64::MAX`.
  pub id: SurfaceId,
  /// The window, for toplevels only.
  pub window: Option<WindowId>,
  /// The parent toplevel of a toplevel, or the parent surface of a popup
  /// or subsurface.
  pub parent: Option<SurfaceId>,
  /// The surface's role.
  pub role: SurfaceRole,
  /// The toplevel's app ID; `None` for other roles.
  pub app_id: Option<String>,
  /// The toplevel's title; `None` for other roles.
  pub title: Option<String>,
  /// The toplevel's xdg-toplevel-icon, if it set one.
  pub icon: Option<ToplevelIcon>,
  /// Where the surface is drawn, in output-layout logical pixels. For a
  /// toplevel this is its tile, floating, maximized (workarea) or
  /// fullscreen (output) rectangle; for popups and subsurfaces it follows
  /// the parent.
  pub geometry: Rect,
  /// Combined alpha in `0.0..=1.0`: the window's opacity (`wf/alpha`) times
  /// the surface's wp-alpha-modifier value. Popups and subsurfaces inherit
  /// their toplevel's opacity.
  pub opacity: f32,
  /// The committed buffer, or `None` when nothing is attached.
  pub buffer: Option<BufferSnapshot>,
}

/// Something the frontend may need to react to, collected while the
/// compositor dispatches and returned by [`Compositor::drain_events`] in
/// the order it happened.
///
/// The window events also feed the Wayfire IPC event stream; the IPC name
/// is given where there is one. Changes made through [`Compositor`]
/// methods are reported here too, not only client-initiated ones.
#[derive(Clone, Debug)]
pub enum Event {
  /// A toplevel mapped (IPC `view-mapped`): it has a buffer and, when map
  /// blocking applies, every `view-pre-map` watcher has unblocked it. Only
  /// now does it enter its tiling tree; it is focused if it is on the
  /// current workspace, and show-desktop ends.
  WindowCreated {
    /// The new window.
    window: WindowId,
    /// Its toplevel surface.
    surface: SurfaceId,
  },
  /// A mapped toplevel's role or surface was destroyed, or it committed a
  /// null buffer (IPC `view-unmapped`). Toplevels that never mapped,
  /// including views still blocked, produce no event. An unmapped toplevel
  /// leaves its tile; when it maps again it is a new window with a new id.
  WindowDestroyed {
    /// The window that is gone.
    window: WindowId,
    /// Its former toplevel surface.
    surface: SurfaceId,
  },
  /// A window was moved to another workspace (IPC
  /// `view-workspace-changed`). Workspaces are 1-based row-major indices.
  WindowMoved {
    /// The moved window.
    window: WindowId,
    /// The 1-based workspace it left.
    from: u32,
    /// The 1-based workspace it is now on.
    workspace: u32,
  },
  /// A toplevel was configured to a new rectangle (IPC
  /// `view-geometry-changed`). Not emitted for the first configure.
  GeometryChanged {
    /// The reconfigured window.
    window: WindowId,
    /// The previously configured rectangle in logical pixels.
    old_geometry: Rect,
  },
  /// `xdg_toplevel.set_app_id` (IPC `view-app-id-changed`).
  AppIdChanged {
    /// The window.
    window: WindowId,
    /// Its toplevel surface.
    surface: SurfaceId,
    /// The new app ID.
    app_id: String,
  },
  /// `xdg_toplevel.set_title` (IPC `view-title-changed`).
  TitleChanged {
    /// The window.
    window: WindowId,
    /// Its toplevel surface.
    surface: SurfaceId,
    /// The new title.
    title: String,
  },
  /// The window's xdg-toplevel-icon was set or cleared; the next
  /// [`SurfaceSnapshot::icon`] has the result.
  IconChanged {
    /// The window.
    window: WindowId,
  },
  /// A window was tiled or floated, or its Wayfire tiled-edges mask
  /// changed (IPC `view-tiled`). Edge masks use Wayfire's bits: 1 left,
  /// 2 right, 4 top, 8 bottom; 15 is a full tile, 0 floating.
  TilingChanged {
    /// The window.
    window: WindowId,
    /// Its toplevel surface.
    surface: SurfaceId,
    /// Whether it is now part of the tiled layout (tiled or snapped).
    tiled: bool,
    /// The previous tiled-edges mask.
    old_edges: u32,
    /// The new tiled-edges mask.
    new_edges: u32,
  },
  /// The window's `wf/alpha` opacity changed.
  OpacityChanged {
    /// The window.
    window: WindowId,
    /// The new opacity in `0.0..=1.0`.
    opacity: f32,
  },
  /// A floating window was snapped into a grid slot.
  SnappingChanged {
    /// The window.
    window: WindowId,
    /// Whether it is now snapped.
    snapped: bool,
  },
  /// IPC `view-minimized`. Emitted on every request, even when the state
  /// did not change.
  MinimizedChanged {
    /// The window.
    window: WindowId,
    /// The requested state.
    minimized: bool,
  },
  /// IPC `view-fullscreen`. Emitted on every request, even when the state
  /// did not change, and when snapping ends fullscreen.
  FullscreenChanged {
    /// The window.
    window: WindowId,
    /// The requested state.
    fullscreen: bool,
  },
  /// Emitted on every maximize request, and when snapping ends it.
  MaximizedChanged {
    /// The window.
    window: WindowId,
    /// The requested state.
    maximized: bool,
  },
  /// IPC `view-sticky`. Emitted on every request, even when the state did
  /// not change.
  StickyChanged {
    /// The window.
    window: WindowId,
    /// The requested state.
    sticky: bool,
  },
  /// The window's stacking layer changed (IPC `view-always-on-top` or
  /// `view-always-on-bottom`, depending on the layer involved). Only
  /// emitted for a real change.
  StackingChanged {
    /// The window.
    window: WindowId,
    /// The layer it left.
    previous: StackingMode,
    /// The layer it is now in.
    current: StackingMode,
  },
  /// The window was raised or lowered within its layer.
  StackingOrderChanged {
    /// The window.
    window: WindowId,
  },
  /// Keyboard focus moved (IPC `view-focused`). `None` when focus went to
  /// a non-window surface such as a layer surface, or nowhere.
  FocusChanged {
    /// The newly focused window.
    window: Option<WindowId>,
  },
  /// The window model changed in a way no other event reports, such as
  /// the split weights or nesting `simple-tile/set-layout` applied or the
  /// rectangle IPC gave a floating window. The session should be saved.
  LayoutChanged,
  /// Show-desktop was turned on (`true`) or off. Turning it on minimizes
  /// the current workspace's windows; mapping or unminimizing a window,
  /// or switching workspace, turns it off, as in Wayfire's
  /// `plugins/wm-actions/wm-actions.cpp`.
  ShowDesktopChanged(bool),
  /// A client asked for an interactive move (`xdg_toplevel.move`), or
  /// [`Compositor::begin_move_window`] was called. The frontend runs the
  /// drag.
  BeginMove {
    /// The window to move.
    window: WindowId,
  },
  /// A client asked for an interactive resize (`xdg_toplevel.resize`).
  BeginResize {
    /// The window to resize.
    window: WindowId,
    /// The raw `xdg_toplevel.resize_edge` value.
    edges: u32,
  },
  /// A `wl_surface.commit` was applied, with the surface's new state.
  /// Emitted for every surface, including ones not currently visible.
  SurfaceCommitted(Box<SurfaceSnapshot>),
  /// A `wl_surface` was destroyed; drop anything cached for it.
  SurfaceDestroyed {
    /// The destroyed surface.
    surface: SurfaceId,
  },
  /// An output's rectangle, scale or transform changed (IPC
  /// `output-layout-changed`).
  OutputChanged {
    /// The output.
    output: OutputHandle,
    /// Its rectangle in logical pixels after the transform.
    rect: Rect,
  },
  /// An output was added ([`Compositor::add_output`], or IPC
  /// `wayfire/create-headless-output`).
  OutputAdded {
    /// The new output.
    output: OutputHandle,
  },
  /// An output was removed; its windows went where
  /// `output_layout.unplug` says.
  OutputRemoved {
    /// The output, which no longer exists.
    output: OutputHandle,
  },
  /// IPC `scale/toggle` (`false`, current workspace) or `scale/toggle_all`
  /// (`true`, all workspaces). The frontend runs Scale.
  ToggleScale(bool),
  /// IPC `expo/toggle`.
  ToggleExpo,
  /// IPC `invert/toggle`.
  InvertToggle,
  /// A client warped the pointer with wp-pointer-warp-v1, or `oswitch`
  /// moved it into the output it focused. The position is in output-layout
  /// logical pixels; a host with its own cursor moves it there.
  PointerWarp {
    /// New pointer x.
    x: f64,
    /// New pointer y.
    y: f64,
    /// The output under the new position, whose host window, for a host
    /// with one per output, shows the cursor.
    output: Option<OutputHandle>,
  },
  /// Another output became the active one (IPC `output-gain-focus`): by a
  /// click or touch on it, a window on it taking focus, `oswitch`, IPC
  /// naming it, or the active output going away.
  OutputFocused {
    /// The output that is active now.
    output: OutputHandle,
  },
  /// The current workspace changed (IPC `wset-workspace-changed`).
  WorkspaceChanged {
    /// The 1-based row-major index of the new workspace.
    workspace: u32,
    /// The 1-based row-major index of the workspace before.
    previous: u32,
    /// The output whose workspace set changed workspace.
    output: OutputHandle,
  },
  /// The output should be powered on or off: an idle timeout, activity,
  /// [`Compositor::set_dpms`] or wlr-output-power-management. A single
  /// change is currently reported twice in a row.
  DpmsChanged(DpmsState),
  /// The idle timeout passed without input.
  Idle,
  /// Input arrived after [`Event::Idle`].
  Resume,
  /// The lock timeout passed or a lock was requested; the frontend starts
  /// the lock client, which then locks with ext-session-lock-v1.
  StartLockClient,
  /// The session lock state changed. While not unlocked, only lock
  /// surfaces are drawn and only they can receive input.
  LockStateChanged(LockState),
  /// A seat input device was enabled or disabled over IPC. Held input has
  /// already been released when a device is disabled. A native backend
  /// mirrors this in libinput's send-events mode.
  InputDeviceChanged {
    /// The device.
    id: InputDeviceId,
    /// Its new state.
    enabled: bool,
  },
  /// `stipc/feed_key` pressed or released evdev key `code` on seat
  /// keyboard `device`, the keyboard STIPC goes through. Only emitted
  /// after [`Compositor::set_stipc_key_events`]; the frontend feeds it
  /// through its own key handling, so bindings fire as for a real key.
  StipcKey {
    /// The keyboard STIPC feeds.
    device: InputDeviceId,
    /// The evdev key code.
    code: u32,
    /// Press or release.
    pressed: bool,
  },
  /// A wlr-screencopy client asked for `region` of `output`, and frame
  /// capture is on ([`Compositor::set_frame_capture`]). The frontend
  /// draws that output's frame as it shows it and answers with
  /// [`Compositor::finish_capture`].
  CaptureRequested {
    /// The capture to answer.
    id: CaptureId,
    /// The captured output.
    output: OutputHandle,
    /// The captured rectangle in the output's logical pixels, with
    /// (0, 0) at its top-left corner; the frontend answers with this
    /// many pixels, which the compositor scales into the client's buffer
    /// of physical pixels.
    region: Rect,
  },
}

/// Why a [`Compositor`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
  /// libwayland could not create the display.
  #[error("failed to create Wayland display: {0}")]
  Display(String),
  /// No `wayland-N` socket could be bound in `XDG_RUNTIME_DIR` (N from 0
  /// to 31), or the absolute path given was in use.
  #[error("failed to bind Wayland socket: {0}")]
  Socket(#[from] wayland_server::BindError),
  /// The calloop event loop or one of its sources (signals, timerfd,
  /// sockets) failed.
  #[error("failed to create event loop: {0}")]
  EventLoop(String),
  /// An I/O error while accepting, dispatching or flushing clients.
  #[error("Wayland dispatch failed: {0}")]
  Dispatch(#[from] std::io::Error),
  /// The window model rejected an operation, for example an unknown
  /// window or an invalid output rectangle.
  #[error("core state error: {0}")]
  Core(#[from] shingle_core::ShingleError),
  /// A 0-based workspace index outside the grid.
  #[error("workspace index {0} is outside the configured workspace grid")]
  InvalidWorkspace(usize),
  /// A workspace grid with zero columns or rows.
  #[error("workspace grid dimensions must be nonzero")]
  InvalidWorkspaceGrid,
  /// An output scale that is zero, negative, NaN or infinite.
  #[error("output scale must be finite and positive")]
  InvalidOutputScale,
  /// An output with this name is already there.
  #[error("an output called {0} exists already")]
  OutputExists(String),
  /// The XKB keymap did not compile, contained a NUL byte or could not be
  /// shared with clients.
  #[error("keymap error: {0}")]
  Keymap(String),
  /// The IPC socket could not be bound, for example because the path
  /// already exists.
  #[error("IPC error: {0}")]
  Ipc(String),
  /// `zwp_linux_dmabuf_v1` could not be offered.
  #[error("DMA-BUF error: {0}")]
  Dmabuf(String),
  /// `wp_linux_drm_syncobj_manager_v1` could not be offered.
  #[error("syncobj error: {0}")]
  Syncobj(String),
}

/// A Wayland compositor with one or more outputs, run by a host frontend;
/// see the crate documentation for the lifecycle.
///
/// SIGINT and SIGTERM are blocked on the thread that creates it and read
/// through a signalfd (see [`Compositor::shutdown_requested`]).
pub struct Compositor {
  display: Display<CompositorState>,
  state: CompositorState,
  event_loop: EventLoop<'static, CompositorState>,
  socket: ListeningSocket,
  socket_name: OsString,
  security_listeners: HashMap<u64, security_context::SecurityListener>,
  ipc: Option<ipc::IpcServer>,
  xwayland: Option<xwayland::Xwayland>,
}

impl Compositor {
  /// Builds a compositor with an empty window model, listening on the first
  /// free `wayland-N` socket (N from 0 to 31) in `XDG_RUNTIME_DIR`.
  ///
  /// It does not set `WAYLAND_DISPLAY` itself; the host starts its clients
  /// in [`Compositor::client_environment`].
  ///
  /// # Errors
  ///
  /// [`Error::Socket`] when no socket name is free, [`Error::Display`] or
  /// [`Error::EventLoop`] when libwayland or calloop cannot be set up, and
  /// [`Error::Keymap`] when the built-in US keymap cannot be prepared.
  pub fn new(config: Config) -> Result<Self, Error> {
    Self::with_core(config, Shingle::new())
  }

  /// Like [`Compositor::new`], but starts from an existing window model,
  /// typically a session restored by `shingle-session`. Its saved windows are
  /// unmapped placeholders; a new toplevel with the same app ID claims one
  /// ([`Shingle::reconcile_mapped`]) and keeps its tile, workspace and
  /// flags. Its workspace sets are parked, and each output takes its own as
  /// it is added; [`Compositor::restore_outputs`] settles the rest.
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
    let xwayland = xwayland::Xwayland::bind(&config.xwayland);
    let disconnected = Arc::new(Mutex::new(Vec::new()));
    let mut state =
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

    let mut compositor = Self {
      display,
      state,
      event_loop,
      socket,
      socket_name,
      security_listeners: HashMap::new(),
      ipc: None,
      xwayland,
    };
    // An eager Xwayland starts here, before the host launches any client.
    if let Some(xwayland) = &mut compositor.xwayland {
      xwayland.start_up(
        &mut compositor.state,
        &compositor.display.handle(),
        &compositor.event_loop.handle(),
      );
    }
    Ok(compositor)
  }

  /// [`Compositor::new`] with [`Config::default`]: a 1280x720 output at scale
  /// 1 and a 10x1 workspace grid.
  pub fn headless() -> Result<Self, Error> {
    Self::new(Config::default())
  }

  /// The `WAYLAND_DISPLAY` value for clients: a name such as `wayland-1`
  /// relative to `XDG_RUNTIME_DIR`, or the absolute path given to
  /// [`Compositor::with_socket_path`].
  pub fn socket_name(&self) -> &OsStr {
    &self.socket_name
  }

  /// Shingle's X display, `:N`, while XWayland is enabled. It is bound
  /// when the compositor is built, so it is known before Xwayland runs.
  pub fn x_display(&self) -> Option<&str> {
    self.state.xwayland.display.as_deref()
  }

  /// Every binding IPC clients have registered with `command/register-binding`,
  /// ordered by ID. The frontend matches its key events against these and
  /// calls [`Compositor::trigger_runtime_binding`] or runs the command.
  pub fn runtime_bindings(&self) -> Vec<RuntimeBinding> {
    self.state.runtime_bindings.values().cloned().collect()
  }

  /// Runs a registered binding's compositor-side action: dispatches its
  /// `call-method` as an IPC request, or sends `command-binding` to the client
  /// that registered it. Bindings with a `command` do nothing here, because the
  /// frontend spawns the command itself. Unknown IDs are ignored.
  pub fn trigger_runtime_binding(&mut self, id: u64) {
    ipc::trigger_runtime_binding(&mut self.state, id);
  }

  /// Registers a seat input device and returns its ID, which the backend
  /// passes with every event from that device. The device starts enabled
  /// and is listed by `input/list-devices` in registration order. IDs count
  /// up from 1 and are not reused.
  pub fn add_input_device(&mut self, info: InputDeviceInfo) -> InputDeviceId {
    input_device::add(&mut self.state, info)
  }

  /// Unregisters a seat input device, for example on a libinput
  /// `DeviceEvent::Removed`. Everything it holds is released first, as when
  /// it is disabled, and later input with its ID is dropped. Returns whether
  /// the device was registered. No `InputDeviceChanged` event is emitted.
  pub fn remove_input_device(&mut self, id: InputDeviceId) -> bool {
    input_device::remove(&mut self.state, id)
  }

  /// Returns how a registered seat input device was described.
  pub fn input_device(&self, id: InputDeviceId) -> Option<&InputDeviceInfo> {
    input_device::info(&self.state, id)
  }

  /// Returns whether a seat input device is registered and enabled. The
  /// nested frontend checks this before acting on host input itself, for
  /// example to match bindings.
  pub fn input_device_enabled(&self, id: InputDeviceId) -> bool {
    input_device::enabled(&self.state, id)
  }

  /// Enables Wayfire-style length-prefixed JSON IPC on a local socket.
  /// From then on [`Compositor::client_environment`] exports `path` as
  /// `WAYFIRE_SOCKET`, so enable IPC before starting any client.
  pub fn enable_ipc(&mut self, path: &std::path::Path) -> Result<(), Error> {
    let server = ipc::IpcServer::new(path)
      .map_err(|error| Error::Ipc(error.to_string()))?;
    self.ipc = Some(server);
    self.state.ipc_socket = Some(path.to_owned());
    Ok(())
  }

  /// The environment for every process the host starts as a client of
  /// this compositor: `WAYLAND_DISPLAY` is [`Compositor::socket_name`],
  /// `WAYFIRE_SOCKET` is the IPC socket once [`Compositor::enable_ipc`]
  /// ran, `DISPLAY` is Shingle's own X display ([`Compositor::x_display`])
  /// or removed when XWayland is disabled, and
  /// nothing of the host session's `WAYLAND_SOCKET`, `DISPLAY` or
  /// `WAYFIRE_SOCKET` gets through. IPC `stipc/run` and IPC binding
  /// commands use it too. See [`ClientEnvironment`].
  pub fn client_environment(&self) -> ClientEnvironment {
    self.state.client_environment()
  }

  /// Reports a frontend plugin's actual activation state to IPC watchers.
  pub fn notify_plugin_activation(&mut self, plugin: &str, activated: bool) {
    let event = ipc::plugin_activation_event(&self.state, plugin, activated);
    self.state.ipc_events.push(event);
  }

  /// The persistent window model: outputs, workspaces, tiling trees and window
  /// state. This is what a session saves.
  pub fn core(&self) -> &Shingle {
    &self.state.core
  }

  /// The idle, lock and DPMS state machine. Its timeouts advance on the 50 ms
  /// housekeeping tick.
  pub fn idle(&self) -> &IdleCoordinator {
    &self.state.idle
  }

  /// The output rectangle in layout logical pixels, after the output transform
  /// (a quarter turn swaps width and height).
  pub fn output(&self) -> Rect {
    self.state.primary().rect
  }

  /// The output's size in physical pixels: the current mode's, or the
  /// logical rectangle scaled and transformed back. A backend without a
  /// display of its own, such as offscreen, draws frames of this size.
  pub fn output_physical_size(&self) -> (u32, u32) {
    self.state.primary().physical_size()
  }

  /// [`Compositor::output_physical_size`] of `output`, if it is there.
  pub fn physical_size_of(&self, output: &OutputHandle) -> Option<(u32, u32)> {
    self
      .state
      .outputs
      .get(&output.id())
      .map(|output| output.physical_size())
  }

  /// Runs one turn of the event loop, waiting up to `timeout` (`None` blocks)
  /// for client requests, connections, signals or the housekeeping tick, then
  /// dispatches clients, serves IPC, advances idle timers and flushes replies.
  /// Returns the number of client requests dispatched.
  ///
  /// Events produced during the turn wait in [`Compositor::drain_events`].
  ///
  /// # Errors
  ///
  /// [`Error::EventLoop`] or [`Error::Dispatch`] when the loop or a client
  /// socket fails. A single misbehaving client is disconnected with a protocol
  /// error rather than failing the call.
  pub fn poll(&mut self, timeout: Option<Duration>) -> Result<usize, Error> {
    output::restore(&mut self.state);
    self.state.reap_ipc_children();
    self
      .event_loop
      .dispatch(timeout, &mut self.state)
      .map_err(|error| Error::EventLoop(error.to_string()))?;
    if self.state.accept_pending {
      self.state.accept_pending = false;
      self.accept_clients()?;
    }
    if let Some(xwayland) = &mut self.xwayland {
      xwayland.poll(
        &mut self.state,
        &self.display.handle(),
        &self.event_loop.handle(),
      );
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
    output::remove_retired(
      &mut self.state,
      &self.display.handle(),
      std::time::Instant::now(),
    );
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
    surface_outputs::sync(&mut self.state);
    foreign_toplevel::sync(&mut self.state, &self.display.handle());
    ext_workspace::sync(&mut self.state, &self.display.handle());
    data_device::sync_selection(&mut self.state, &self.display.handle());
    data_device::sync_drag(&mut self.state, &self.display.handle());
    data_control::sync(&mut self.state, &self.display.handle());
    primary_selection::sync_selection(&mut self.state, &self.display.handle());
    if let Some(xwayland) = &mut self.xwayland {
      xwayland.flush(&mut self.state, &self.event_loop.handle());
    }
    self.state.buffer_releases.flush();
    self.display.flush_clients()?;
    Ok(dispatched)
  }

  /// [`Compositor::poll`] without waiting; call it whenever the [`AsFd`]
  /// descriptor is readable.
  pub fn dispatch_pending(&mut self) -> Result<usize, Error> {
    self.poll(Some(Duration::ZERO))
  }

  /// Reports a frame drawn now on every output that is not dark, for
  /// frontends that learn nothing more precise about presentation, such as
  /// nested. For each output it is [`Compositor::frame_presented`] with the
  /// current `CLOCK_MONOTONIC` time, the output's refresh rate from
  /// [`OutputInfo`], no sequence number and no flags. The frontend reports
  /// each output it draws with [`Compositor::frame_now`] instead, and
  /// paces outputs created over IPC with a timer of its own.
  pub fn frame(&mut self) {
    let outputs = self
      .state
      .outputs
      .iter()
      .filter(|output| !output.dark)
      .map(|output| OutputHandle::from(&output.id))
      .collect::<Vec<_>>();
    for output in outputs {
      self.frame_now(&output);
    }
  }

  /// [`Compositor::frame`] for one output, as a host that paces each
  /// output by its own timer reports it.
  pub fn frame_now(&mut self, output: &OutputHandle) {
    let Some(refresh_mhz) = self
      .state
      .outputs
      .get(&output.id())
      .map(|output| output.info.refresh_mhz)
    else {
      return;
    };
    self.frame_presented(
      output,
      PresentInfo {
        time: clock::monotonic_now(),
        refresh_ns: presentation::refresh_ns(refresh_mhz),
        seq: 0,
        flags: PresentationFlags::empty(),
      },
    );
  }

  /// Reports that a frame of `output` reached the screen, for example on a
  /// DRM page flip. Surfaces that were visible on it get their frame
  /// callbacks, with `info.time` in milliseconds, and their
  /// `wp_presentation` feedback, with every field of `info` and the output
  /// as `sync_output`. Surfaces hidden everywhere keep their callbacks for
  /// a later frame, and their feedback is discarded. Feedback, FIFO
  /// barriers and commit-timing deadlines follow each surface's primary
  /// output, the one it overlaps most: a surface also shown on another
  /// output gets its feedback from its primary output's frame, as
  /// wlroots' scene gives it. A dark output presents nothing.
  ///
  /// Does nothing while the session is inactive
  /// ([`Compositor::set_session_active`]).
  pub fn frame_presented(&mut self, output: &OutputHandle, info: PresentInfo) {
    if !input_device::session_active(&self.state) {
      return;
    }
    let id = output.id();
    if self.state.outputs.get(&id).is_none_or(|output| output.dark) {
      return;
    }
    commit_timing::frame(&mut self.state, &id);
    fifo::frame(&mut self.state, &id);
    self.state.buffer_releases.flush();
    let time = clock::millis(info.time);
    let visible = self
      .state
      .visible_surfaces_on(&id)
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
    presentation::frame(&mut self.state, &id, &info);
  }

  /// Whether a constrained surface update needs a future frame.
  pub fn needs_frame(&self) -> bool {
    commit_timing::has_pending(&self.state) || fifo::has_pending(&self.state)
  }

  /// Polls until [`Compositor::shutdown_requested`], for hosts that draw
  /// nothing, such as the `shingle --headless` binary. Nothing drains the
  /// events, so they accumulate.
  pub fn run(&mut self) -> Result<(), Error> {
    while !self.state.shutdown {
      self.poll(None)?;
    }
    Ok(())
  }

  /// Takes the events produced since the last call, oldest first. The window
  /// events among them are also queued for IPC `events/watch` clients at this
  /// point, so a frontend must drain regularly for IPC events to flow.
  pub fn drain_events(&mut self) -> Vec<Event> {
    let events = self.state.drain_events();
    ipc::record_events(&mut self.state, &events);
    events
  }

  /// The surfaces to draw this frame, bottom to top, each followed by its
  /// subsurfaces and popups.
  ///
  /// The order is: background and bottom layer-shell surfaces; the current
  /// workspace's windows by stacking layer (always-on-bottom, normal,
  /// always-on-top) and stacking order, with sticky windows from other
  /// workspaces at the end of each layer; top and overlay layer surfaces; and
  /// the cursor. A focused fullscreen window is drawn above the top layer,
  /// under only the overlay layer. Minimized windows, toplevels without a
  /// buffer and views still held by map blocking are left out.
  ///
  /// While the session is not unlocked only lock surfaces are returned, and
  /// while DPMS is off nothing is.
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

  /// [`Compositor::scale_surfaces`] for the windows that `output` shows,
  /// as Scale, Expo and workspace slides on that output draw them.
  pub fn scale_surfaces_on(
    &self,
    output: &OutputHandle,
    all_workspaces: bool,
    include_minimized: bool,
  ) -> Vec<SurfaceSnapshot> {
    self.state.scale_surfaces_on(
      &output.id(),
      all_workspaces,
      include_minimized,
    )
  }

  /// A snapshot of any live surface, visible or not; `None` once it is
  /// destroyed.
  pub fn surface(&self, id: SurfaceId) -> Option<SurfaceSnapshot> {
    self.state.surface(id)
  }

  /// Whether the session is locked or a lock is pending or failed: anything
  /// but [`LockState::Unlocked`].
  pub fn is_locked(&self) -> bool {
    self.state.idle.lock_state() != LockState::Unlocked
  }

  /// Starts locking, as an expired lock timeout would: emits
  /// [`Event::StartLockClient`] unless a lock is already pending or held. The
  /// lock is held once the lock client's surface is ready.
  pub fn request_lock(&mut self) {
    let commands = self.state.idle.request_lock(self.state.now());
    self.state.process_idle_commands(commands);
  }

  /// Turns the idle timeouts off or back on, like Wayfire's `idle/toggle`
  /// binding (`plugins/single_plugins/idle.cpp`).
  pub fn toggle_idle(&mut self) {
    let (_, commands) = self.state.idle.toggle(self.state.now());
    self.state.process_idle_commands(commands);
  }

  /// Asks the focused window on the current workspace to close; see
  /// [`Compositor::close_window`]. Returns the window asked.
  pub fn close_focused(&mut self) -> Option<WindowId> {
    let current = self
      .state
      .core
      .current_workspace(&self.state.active_output)
      .ok()?;
    let window = self
      .state
      .core
      .tree(&self.state.active_output, current)
      .ok()?
      .focused()?;
    self.close_window(window).map(|_| window)
  }

  /// Sends `xdg_toplevel.close`. The client decides whether and when to close,
  /// so the window is still there afterwards. Returns `None` if the window has
  /// no toplevel.
  pub fn close_window(&mut self, window: WindowId) -> Option<()> {
    shell::close_window(&mut self.state, window)
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

  /// Stops the X window manager and Xwayland now and removes the X
  /// display's sockets and lock, which dropping the compositor does too.
  /// Afterwards [`Compositor::x_display`] is `None`, and the model no
  /// longer has the X windows, so a host takes its session snapshot
  /// first. Calling it again does nothing.
  ///
  /// Hosts call it on shutdown before writing the session, so that a
  /// write stalled on a busy disk does not hold the display: a SIGKILL
  /// that ends such a stall would otherwise leave the lock and socket
  /// behind.
  pub fn stop_xwayland(&mut self) {
    if let Some(mut xwayland) = self.xwayland.take() {
      xwayland.shut_down(&mut self.state, &self.event_loop.handle());
    }
    self.state.xwayland.display = None;
  }

  /// Re-applies a reloaded configuration to the running compositor:
  /// workspace grid, output geometry, scale and transform, gaps, idle timers,
  /// focus, decoration and input settings and window rules, then reconfigures
  /// every toplevel. Shrinking the grid moves windows from removed workspaces
  /// to the nearest remaining one. Fields documented on [`Config`] as read at
  /// startup keep their old values.
  ///
  /// # Errors
  ///
  /// [`Error::InvalidWorkspaceGrid`], [`Error::InvalidOutputScale`] or
  /// [`Error::Core`] for an invalid output rectangle, before anything
  /// changes.
  pub fn apply_config(&mut self, config: Config) -> Result<(), Error> {
    if config.workspace_width == 0 || config.workspace_height == 0 {
      return Err(Error::InvalidWorkspaceGrid);
    }
    if !config.output_scale.is_finite() || config.output_scale <= 0.0 {
      return Err(Error::InvalidOutputScale);
    }
    let placements = output::Placements::new(&config);
    // Every output is placed again; a backend with a mode list sets the
    // size, the configuration the rest.
    let mut placed = Vec::new();
    // Outputs an output-management client configured keep its placement
    // until Shingle restarts, as Wayfire's is_externally_managed.
    for output in self.state.outputs.iter().filter(|output| !output.managed) {
      let placement = placements.get(&output.info.name);
      if !placement.scale.is_finite() || placement.scale <= 0.0 {
        return Err(Error::InvalidOutputScale);
      }
      let (width, height) = match (output.kind, output.info.current()) {
        (_, Some(mode)) => output_transform::logical_size(
          mode.width,
          mode.height,
          placement.scale,
          placement.transform,
        ),
        // An IPC output keeps the size it was created with.
        (OutputKind::Virtual, None) => output_transform::logical_size(
          output.physical_size().0,
          output.physical_size().1,
          placement.scale,
          placement.transform,
        ),
        (OutputKind::Backend, None)
          if output_transform::is_quarter_turn(placement.transform) =>
        {
          (placement.height, placement.width)
        }
        (OutputKind::Backend, None) => (placement.width, placement.height),
      };
      let (x, y) = match placement.position {
        shingle_config::OutputPosition::Fixed { x, y } => (x, y),
        shingle_config::OutputPosition::Auto => (output.rect.x, output.rect.y),
      };
      let rect = Rect::new(x, y, width, height);
      rect.validate()?;
      placed.push((output.id.clone(), placement, rect));
    }
    self.state.placements = placements;
    self.state.unplug = config.output_unplug;
    self.state.remove_output_limits = config.remove_output_limits;
    self.state.focus_settings = output::FocusSettings::new(&config);
    for (id, placement, _) in &placed {
      if placement.refresh_mhz > 0
        && let Some(output) = self.state.outputs.get_mut(id)
        && output.info.modes.is_empty()
      {
        output.info.refresh_mhz = placement.refresh_mhz;
      }
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
    self.apply_xwayland_config(config.xwayland);
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
      .filter_map(|surface| {
        surface
          .role
          .toplevel()
          .and_then(|toplevel| toplevel.window())
      })
      .collect::<Vec<_>>();
    for window in windows {
      self.state.core.set_gaps(window, config.gaps)?;
    }
    let commands = self.state.idle.set_config(config.idle);
    self.state.process_idle_commands(commands);
    for (id, placement, rect) in placed {
      self.state.set_transform_of(&id, placement.transform);
      self.state.set_scale_of(&id, placement.scale)?;
      if let Some(output) = self.state.outputs.get_mut(&id) {
        output.position = placement.position;
      }
      self.state.set_rect_of(&id, rect, false)?;
    }
    shell::reconfigure_all(&mut self.state);
    Ok(())
  }

  /// Takes a reloaded startup script. A changed mode needs a restart, as
  /// in Wayfire, which reads `core/xwayland` once; it is only logged.
  fn apply_xwayland_config(&mut self, settings: XwaylandSettings) {
    let running = self.xwayland.as_ref().map(xwayland::Xwayland::mode);
    let wanted = match settings.mode {
      shingle_config::XwaylandMode::Eager => {
        Some(shingle_xwayland::StartMode::Eager)
      }
      shingle_config::XwaylandMode::Lazy => {
        Some(shingle_xwayland::StartMode::Lazy)
      }
      shingle_config::XwaylandMode::Disabled => None,
    };
    if running != wanted {
      eprintln!(
        "shingle: xwayland.mode is now {}; restart Shingle to apply it",
        settings.mode
      );
    }
    if let Some(xwayland) = &mut self.xwayland {
      xwayland.set_startup_script(
        settings.startup_script,
        settings.discard_script_output,
      );
    }
  }

  /// Ends a held session lock, as when the lock client authenticated: turns
  /// DPMS back on and reports [`Event::Resume`] if the session went idle.
  /// Does nothing unless the lock is held.
  pub fn unlock(&mut self) {
    self.state.unlock_session();
  }

  /// Turns the output on or off, as a wlr-output-power-management client
  /// can. Emits [`Event::DpmsChanged`] for a change; while DPMS is off,
  /// [`Compositor::visible_surfaces`] is empty.
  pub fn set_dpms(&mut self, state: DpmsState) {
    let commands = self.state.idle.set_dpms(state, self.state.now());
    self.state.process_idle_commands(commands);
  }

  /// Whether the output should be on, after idle timeouts, activity,
  /// [`Compositor::set_dpms`] and wlr-output-power-management. The native
  /// backend follows it with the CRTC's `ACTIVE`.
  pub fn dpms_state(&self) -> DpmsState {
    self.state.idle.dpms_state()
  }

  /// Records user activity for the idle timers: resets them, turns DPMS back
  /// on and ends an idle period. Seat input does this by itself; a frontend
  /// calls it for input the compositor never sees.
  pub fn activity(&mut self) {
    let commands = self.state.idle.activity(self.state.now());
    self.state.process_idle_commands(commands);
  }

  // The seat entry points below take the ID of the device the event comes
  // from. Input from an unknown, disabled or differently kinded device, or
  // while the session is inactive, is dropped.

  /// Moves the pointer to a position in the output layout.
  pub fn pointer_motion(&mut self, device: InputDeviceId, x: f64, y: f64) {
    input_device::pointer_motion(&mut self.state, device, x, y);
  }

  /// Sends relative pointer motion to `zwp_relative_pointer_v1` clients.
  pub fn pointer_relative_motion(
    &mut self,
    device: InputDeviceId,
    dx: f64,
    dy: f64,
    dx_unaccel: f64,
    dy_unaccel: f64,
  ) {
    input_device::pointer_relative_motion(
      &mut self.state,
      device,
      dx,
      dy,
      dx_unaccel,
      dy_unaccel,
    );
  }

  /// Whether a pointer-constraints-v1 lock or confinement is active. While
  /// it is, the nested frontend stops moving the pointer itself and feeds
  /// [`Compositor::pointer_relative_motion`] instead.
  pub fn pointer_constrained(&self) -> bool {
    pointer_protocols::constrained(&self.state)
  }

  /// Begins a touchpad swipe. Only one gesture is active at a time, and
  /// only the device that began it updates or ends it; disabling or
  /// removing that device ends it as cancelled.
  pub fn pointer_swipe_begin(&mut self, device: InputDeviceId, fingers: u32) {
    pointer_gestures::swipe_begin(&mut self.state, device, fingers);
  }

  /// Moves the active swipe by `(dx, dy)` logical pixels. Ignored unless
  /// `device` began it.
  pub fn pointer_swipe_update(
    &mut self,
    device: InputDeviceId,
    dx: f64,
    dy: f64,
  ) {
    pointer_gestures::swipe_update(&mut self.state, device, dx, dy);
  }

  /// Ends the active swipe; `cancelled` tells clients to undo its effect.
  pub fn pointer_swipe_end(&mut self, device: InputDeviceId, cancelled: bool) {
    pointer_gestures::swipe_end(&mut self.state, device, cancelled);
  }

  /// Begins a touchpad pinch with `fingers` fingers; the same one-gesture rule
  /// as [`Compositor::pointer_swipe_begin`] applies.
  pub fn pointer_pinch_begin(&mut self, device: InputDeviceId, fingers: u32) {
    pointer_gestures::pinch_begin(&mut self.state, device, fingers);
  }

  /// Updates the active pinch: `(dx, dy)` is the centre's motion in logical
  /// pixels, `scale` the absolute scale since the start (1.0 unchanged) and
  /// `rotation` the change in degrees, clockwise.
  pub fn pointer_pinch_update(
    &mut self,
    device: InputDeviceId,
    dx: f64,
    dy: f64,
    scale: f64,
    rotation: f64,
  ) {
    pointer_gestures::pinch_update(
      &mut self.state,
      device,
      dx,
      dy,
      scale,
      rotation,
    );
  }

  /// Ends the active pinch; `cancelled` tells clients to undo its effect.
  pub fn pointer_pinch_end(&mut self, device: InputDeviceId, cancelled: bool) {
    pointer_gestures::pinch_end(&mut self.state, device, cancelled);
  }

  /// Begins a touchpad hold gesture (fingers resting without moving); the same
  /// one-gesture rule as [`Compositor::pointer_swipe_begin`] applies.
  pub fn pointer_hold_begin(&mut self, device: InputDeviceId, fingers: u32) {
    pointer_gestures::hold_begin(&mut self.state, device, fingers);
  }

  /// Ends the active hold; `cancelled` means the fingers moved or lifted early.
  pub fn pointer_hold_end(&mut self, device: InputDeviceId, cancelled: bool) {
    pointer_gestures::hold_end(&mut self.state, device, cancelled);
  }

  /// Presses or releases evdev button `button` (`BTN_LEFT` is 0x110). A press
  /// focuses the window under the pointer when the focus configuration says so,
  /// and the button goes to the surface under the pointer. A button that
  /// another device still holds stays down for clients.
  pub fn pointer_button(
    &mut self,
    device: InputDeviceId,
    button: u32,
    pressed: bool,
  ) {
    input_device::pointer_button(&mut self.state, device, button, pressed);
  }

  /// Scrolls by continuous axis values, without discrete wheel steps or an
  /// end, reported as an [`AxisSource::Continuous`] source; see
  /// [`Compositor::pointer_axis_detailed`].
  pub fn pointer_axis(
    &mut self,
    device: InputDeviceId,
    horizontal: f64,
    vertical: f64,
  ) {
    input_device::pointer_axis_detailed(
      &mut self.state,
      device,
      Axis {
        horizontal,
        vertical,
        source: AxisSource::Continuous,
        ..Axis::default()
      },
    );
  }

  /// Scrolls the surface under the pointer. `horizontal` and `vertical` are
  /// `wl_pointer.axis` values (positive is right and down) and the `_steps`
  /// are wheel clicks, sent as `axis_discrete` and `axis_value120`. Values are
  /// multiplied by the configured scroll speed and negated for natural
  /// scrolling. The source is sent as `wl_pointer.axis_source`, and the end
  /// of a finger or continuous scroll on an axis as `wl_pointer.axis_stop`.
  pub fn pointer_axis_detailed(&mut self, device: InputDeviceId, axis: Axis) {
    input_device::pointer_axis_detailed(&mut self.state, device, axis);
  }

  /// Starts touch point `id` at `(x, y)` in layout logical pixels, on the
  /// surface under it, which keeps the point until it ends.
  pub fn touch_down(&mut self, device: InputDeviceId, id: i32, x: f64, y: f64) {
    input_device::touch_down(&mut self.state, device, id, x, y);
  }

  /// Moves touch point `id` to `(x, y)` in layout logical pixels.
  pub fn touch_motion(
    &mut self,
    device: InputDeviceId,
    id: i32,
    x: f64,
    y: f64,
  ) {
    input_device::touch_motion(&mut self.state, device, id, x, y);
  }

  /// Ends touch point `id`. The point stays down for clients while another
  /// touch device holds the same ID.
  pub fn touch_up(&mut self, device: InputDeviceId, id: i32) {
    input_device::touch_up(&mut self.state, device, id);
  }

  /// Cancels touch point `id` with `wl_touch.cancel`, for example when the
  /// compositor takes over the gesture. The protocol cancels the client's whole
  /// touch sequence, so every other point down on that client's surfaces ends
  /// too: lifting it sends nothing, and touching down with its ID again starts
  /// a new point.
  pub fn touch_cancel(&mut self, device: InputDeviceId, id: i32) {
    input_device::touch_cancel(&mut self.state, device, id);
  }

  /// Feeds an evdev key from a seat keyboard. A key that two keyboards hold
  /// is released for clients when the last of them lets go.
  pub fn keyboard_key(
    &mut self,
    device: InputDeviceId,
    key: u32,
    pressed: bool,
  ) {
    input_device::keyboard_key(&mut self.state, device, key, pressed);
  }

  /// Sets the seat's modifier state, as computed by the frontend's xkb
  /// state, on behalf of a seat keyboard.
  pub fn keyboard_modifiers(
    &mut self,
    device: InputDeviceId,
    modifiers: Modifiers,
  ) {
    input_device::keyboard_modifiers(&mut self.state, device, modifiers);
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

  /// The xkb locked-modifier mask, such as Caps Lock (bit 1) and Num Lock
  /// (bit 4) in standard keymaps.
  pub fn locked_modifiers(&self) -> u32 {
    self.state.input.modifiers.locked
  }

  /// Returns the index of the active keyboard layout, as `wl_keyboard`
  /// modifiers and `wayfire/get-keyboard-state` report it. IPC can change
  /// it with `wayfire/set-keyboard-state`.
  pub fn keyboard_layout_index(&self) -> u32 {
    self.state.input.modifiers.group
  }

  /// Whether compositor shortcuts are inhibited for the focused surface: by a
  /// keyboard-shortcuts-inhibit-v1 inhibitor, or implicitly for views matching
  /// `shortcuts-inhibit/inhibit_by_default` and not `ignore_views`. The
  /// frontend then passes keys to the client instead of matching bindings, as
  /// Wayfire's `plugins/protocols/shortcuts-inhibit.cpp` does.
  pub fn shortcuts_inhibited(&self) -> bool {
    shortcuts::active(&self.state)
  }

  /// Wayfire's `shortcuts-inhibit/break_grab`: lifts the inhibition for the
  /// currently focused surface until focus moves elsewhere.
  pub fn break_shortcuts_inhibit(&mut self) {
    shortcuts::break_grab(&mut self.state);
  }

  /// Compiles an XKB keymap in text form, which may use `include` statements,
  /// and sends the compiled result to every keyboard and input-method grab.
  /// The active layout index is reset to 0.
  ///
  /// # Errors
  ///
  /// [`Error::Keymap`] when the keymap does not compile or contains a NUL
  /// byte; the previous keymap stays in use.
  pub fn set_keymap(&mut self, keymap: &str) -> Result<(), Error> {
    input::set_keymap(&mut self.state, keymap).map_err(Error::Keymap)
  }

  /// Makes `stipc/feed_key` emit [`Event::StipcKey`] instead of matching
  /// IPC bindings and passing the key to clients itself. A frontend with
  /// its own keyboard state and bindings enables this, so STIPC keys go
  /// through the same path as keys from a real keyboard, as Wayfire's
  /// headless STIPC keyboard does. Keys are still dropped while the
  /// session is inactive or no keyboard is registered.
  pub fn set_stipc_key_events(&mut self, enabled: bool) {
    self.state.stipc_key_events = enabled;
  }

  /// Offers `zwp_linux_dmabuf_v1` version 5 with `feedback` to clients,
  /// importing their buffers through `importer`, and makes commits of
  /// DMA-BUFs wait for their implicit write fences
  /// (docs/design/dmabuf.md, sections 2 and 3). A frontend calls it once
  /// its renderer exists and before it starts clients, since most
  /// clients look for the global only once. Calling it again, as after a
  /// GPU loss, replaces the feedback and the importer for later requests
  /// and keeps the global.
  ///
  /// # Errors
  ///
  /// [`Error::Dmabuf`] when `feedback` lists no format or the format table
  /// cannot be made.
  pub fn enable_dmabuf(
    &mut self,
    feedback: DmabufFeedback,
    importer: Box<dyn DmabufImporter>,
  ) -> Result<(), Error> {
    linux_dmabuf::enable(
      &mut self.state,
      &self.display.handle(),
      &self.event_loop.handle(),
      feedback,
      importer,
    )
    .map_err(Error::Dmabuf)
  }

  /// Whether `zwp_linux_dmabuf_v1` is offered.
  pub fn dmabuf_enabled(&self) -> bool {
    self.state.dmabuf.is_some()
  }

  /// Offers `wp_linux_drm_syncobj_manager_v1` version 1, explicit sync
  /// for DMA-BUFs (docs/design/dmabuf.md, section 3), on the render node
  /// [`Compositor::enable_dmabuf`] advertises as the main device. The
  /// compositor opens that node itself, never a card node, and offers
  /// the global only if it has timeline syncobjs and
  /// `DRM_IOCTL_SYNCOBJ_EVENTFD`. Returns the line to log. Calling it
  /// again, as after a GPU loss, opens the node again for later imports
  /// and keeps the global.
  ///
  /// # Errors
  ///
  /// [`Error::Syncobj`] when linux-dmabuf is not offered, or the render
  /// node cannot be opened or lacks either feature; clients then use
  /// implicit sync.
  pub fn enable_syncobj(&mut self) -> Result<String, Error> {
    let main_device = self
      .state
      .dmabuf
      .as_ref()
      .map(|dmabuf| dmabuf.main_device())
      .ok_or_else(|| Error::Syncobj("linux-dmabuf is not offered".into()))?;
    let node = linux_drm_syncobj::open_render_node(main_device)
      .map_err(Error::Syncobj)?;
    linux_drm_syncobj::enable(
      &mut self.state,
      &self.display.handle(),
      &self.event_loop.handle(),
      Box::new(node),
    )
    .map_err(Error::Syncobj)?;
    Ok(linux_drm_syncobj::offered(main_device))
  }

  /// Whether `wp_linux_drm_syncobj_manager_v1` is offered.
  pub fn syncobj_enabled(&self) -> bool {
    self.state.syncobj.is_some()
  }

  /// Makes wlr-screencopy `copy` and `copy_with_damage` emit
  /// [`Event::CaptureRequested`] instead of compositing the surfaces'
  /// CPU pixels, for a frontend that renders: what it draws, with
  /// inversion, Scale, Expo and animations, is what clients capture, as
  /// in Wayfire, whose wlroots copies the rendered output. Without it,
  /// as in the headless backend, the compositor composites on the CPU.
  pub fn set_frame_capture(&mut self, enabled: bool) {
    self.state.screencopy.frame_capture = enabled;
  }

  /// Makes `wl_shm` commits keep the client's buffer instead of copying
  /// it, for a frontend that uploads to the GPU (docs/design/dmabuf.md,
  /// section 5). Each committed snapshot then carries a
  /// [`SnapshotContent::Shm`], locked until the frontend has written it
  /// into a texture and called [`ShmContent::release`]; the frontend
  /// must do that for every surface it is told about in
  /// [`Event::SurfaceCommitted`], drawn or not, or clients wait for their
  /// buffers. Without it, as in the headless backend and for buffers
  /// committed before it is turned on, shared-memory buffers are copied
  /// into [`SnapshotContent::Pixels`] and released at commit.
  pub fn set_shm_upload(&mut self, enabled: bool) {
    self.state.shm_upload = enabled;
  }

  /// Answers [`Event::CaptureRequested`] `id` with `result`: the region
  /// in logical pixels as tightly packed RGBA8 rows, top to bottom, which
  /// are scaled and turned into the client's buffer, in the output's
  /// physical pixels, before `ready` is sent; or why it could not be
  /// drawn, which sends `failed`. Pixels of the wrong size also fail. An
  /// unknown `id`, or one whose frame was destroyed, is ignored.
  pub fn finish_capture(
    &mut self,
    id: CaptureId,
    result: Result<Vec<u8>, String>,
  ) {
    screencopy::finish(&mut self.state, id, result);
  }

  /// Sets key repeat for clients via `wl_keyboard.repeat_info`: `rate` in keys
  /// per second (0 disables repeat) and `delay` in milliseconds.
  pub fn set_repeat(&mut self, rate: u32, delay: u32) {
    input::set_repeat(&mut self.state, rate, delay);
  }

  /// Focuses the surface at `(x, y)` in layout logical pixels, as a click
  /// does, honouring input regions. A parent with a modal dialog focuses the
  /// dialog instead. Returns the focused window, or `None` for a non-window
  /// surface or when nothing can be focused.
  pub fn focus_at(&mut self, x: f64, y: f64) -> Option<WindowId> {
    input::focus_at(&mut self.state, x, y)
  }

  /// The window whose surface, popup or subsurface is topmost at `(x, y)` in
  /// layout logical pixels, honouring input regions. Layer surfaces and the
  /// cursor yield `None`.
  pub fn window_at(&self, x: f64, y: f64) -> Option<WindowId> {
    input::window_at(&self.state, x, y)
  }

  /// Starts an interactive move by emitting [`Event::BeginMove`], as if the
  /// client had called `xdg_toplevel.move`.
  pub fn begin_move_window(&mut self, window: WindowId) {
    self.state.events.push(Event::BeginMove { window });
  }

  /// Gives the window keyboard and pointer focus and raises it. With
  /// `all_dialogs_modal` (or a modal dialog) the topmost dialog of the window
  /// gets focus instead, as in Wayfire's `seat_t::focus_view`
  /// (`src/core/seat/seat.cpp`). Returns the window that got focus, or `None`
  /// while locked, in show-desktop, or when the window is minimized or
  /// unknown.
  pub fn focus_window(&mut self, window: WindowId) -> Option<WindowId> {
    input::focus_window(&mut self.state, window)
  }

  /// The window with keyboard focus, or the current workspace's focused tile
  /// when keyboard focus is on no window. `None` during show-desktop.
  pub fn focused_window(&self) -> Option<WindowId> {
    input::focused_window(&self.state)
  }

  /// The window's `wf/alpha` opacity in `0.0..=1.0`, or `None` for an unknown
  /// window.
  pub fn window_opacity(&self, window: WindowId) -> Option<f32> {
    shell::window_alpha(&self.state, window)
  }

  /// Sets the window's opacity, clamped to `0.0..=1.0`. Returns `false` for an
  /// unknown window or a non-finite value.
  pub fn set_window_opacity(&mut self, window: WindowId, opacity: f32) -> bool {
    if !opacity.is_finite() || self.window_opacity(window).is_none() {
      return false;
    }
    shell::set_alpha(&mut self.state, window, opacity);
    true
  }

  /// Wayfire's alpha plugin scroll action
  /// (`plugins/single_plugins/alpha.cpp`, `update_alpha`): lowers the opacity
  /// of the window at `(x, y)` by `delta * 0.003`, keeping it within
  /// `min_value..=1.0`. Returns the new opacity, or `None` when there is no
  /// window there or an argument is not finite.
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

  /// Raises every window whose opacity is below `min_value` to it, for when
  /// `alpha/min_value` is increased.
  pub fn clamp_opacity(&mut self, min_value: f32) {
    if min_value.is_finite() {
      shell::clamp_alpha(&mut self.state, min_value.clamp(0.0, 1.0));
    }
  }

  /// Moves the window to the top of its stacking layer.
  pub fn raise_window(&mut self, window: WindowId) -> Result<(), Error> {
    self.state.core.raise_window(window)?;
    self
      .state
      .events
      .push(Event::StackingOrderChanged { window });
    Ok(())
  }

  /// Moves the window to the bottom of its stacking layer.
  pub fn lower_window(&mut self, window: WindowId) -> Result<(), Error> {
    self.state.core.lower_window(window)?;
    self
      .state
      .events
      .push(Event::StackingOrderChanged { window });
    Ok(())
  }

  /// Focuses the nearest tiled, unminimized window in `direction` from the
  /// focused one, like Wayfire's simple-tile `focus_adjacent`
  /// (`plugins/tile/tile-plugin.cpp`). Maximized state moves to the new
  /// window; fullscreen ends, and with `keep_fullscreen_on_adjacent` the new
  /// window becomes fullscreen. Returns the new window, or `None` if there is
  /// none in that direction.
  pub fn focus_directional(
    &mut self,
    direction: Direction,
  ) -> Result<Option<WindowId>, Error> {
    input::focus_directional(&mut self.state, direction)
  }

  /// The windows a window switcher cycles through: the current workspace's
  /// windows plus sticky windows from other workspaces, most recently focused
  /// first. Empty during show-desktop. Minimized windows are included.
  pub fn switchable_windows(&self) -> Vec<WindowId> {
    if self.state.show_desktop {
      return Vec::new();
    }
    let Ok(workspace) =
      self.state.core.current_workspace(&self.state.active_output)
    else {
      return Vec::new();
    };
    let mut windows = self
      .state
      .core
      .tree(&self.state.active_output, workspace)
      .map(|tree| tree.windows())
      .unwrap_or_default();
    let mut sticky = self
      .state
      .surfaces
      .values()
      .filter_map(|surface| match surface.role.toplevel() {
        Some(toplevel) => toplevel.window().filter(|window| {
          self.state.core.window_info(*window).is_ok_and(|info| {
            info.state.sticky
              && info.location.output == self.state.active_output
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

  /// Focuses the next window in [`Compositor::switchable_windows`] (the
  /// previous one with `reverse`), wrapping around. A minimized window is
  /// restored, which also focuses it.
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

  /// Switches one workspace right (left with `reverse`), subject to
  /// [`WorkspaceWraparound`]. Returns the new 0-based index.
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

  /// Switches one workspace down (up with `reverse`), subject to
  /// [`WorkspaceWraparound`]. Returns the new 0-based index.
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

  /// Moves the focused window one workspace right (left with `reverse`) and
  /// follows it, like vswitch's `with_win_*` bindings. Returns the moved
  /// window, or `None` when nothing is focused.
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

  /// Moves the focused window one workspace down (up with `reverse`) and
  /// follows it.
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

  /// Sends the focused window one workspace right (left with `reverse`)
  /// without following it, like vswitch's `send_win_*` bindings.
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

  /// Sends the focused window one workspace down (up with `reverse`) without
  /// following it.
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

  /// Sends the focused window to the previously active workspace without
  /// following it. `Ok(None)` when there is no previous workspace.
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

  /// Switches back to the previously active workspace, like vswitch's
  /// `binding_last`. Returns its 0-based index, or `None` when there is none.
  pub fn switch_to_last_workspace(&mut self) -> Result<Option<usize>, Error> {
    let Some(workspace) = self.state.last_workspace else {
      return Ok(None);
    };
    let index = self.workspace_index(workspace);
    self.switch_workspace(index)?;
    Ok(Some(index))
  }

  /// Moves the focused window to the previously active workspace and follows
  /// it.
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

  /// Makes workspace `index` (0-based, row-major) current. Keyboard focus
  /// moves to that workspace's focused window, every toplevel is reconfigured
  /// and [`Event::WorkspaceChanged`] is emitted. Leaving a workspace ends
  /// show-desktop and remembers it as the last workspace. Switching to the
  /// current workspace does nothing.
  ///
  /// # Errors
  ///
  /// [`Error::InvalidWorkspace`] when `index` is outside the grid.
  pub fn switch_workspace(&mut self, index: usize) -> Result<(), Error> {
    self.state.switch_workspace(index)
  }

  /// The current workspace as a 0-based row-major index.
  pub fn current_workspace_index(&self) -> Result<usize, Error> {
    let workspace = self
      .state
      .core
      .current_workspace(&self.state.active_output)?;
    Ok(
      workspace.y as usize * self.state.workspace_width as usize
        + workspace.x as usize,
    )
  }

  /// The workspace grid as `(columns, rows)`.
  pub fn workspace_grid_dimensions(&self) -> (u32, u32) {
    (self.state.workspace_width, self.state.workspace_height)
  }

  /// Sends the focused window to workspace `index` (0-based) without
  /// following it. Returns the window, or `None` when nothing is focused.
  pub fn move_focused_to_workspace(
    &mut self,
    index: usize,
  ) -> Result<Option<WindowId>, Error> {
    self.state.move_focused_to_workspace(index)
  }

  /// Moves `window` to workspace `index` (0-based), keeping its floating,
  /// snapped and other persistent state, and emits [`Event::WindowMoved`].
  /// A window already on that workspace stays where it is in the layout,
  /// and nothing is emitted.
  ///
  /// # Errors
  ///
  /// [`Error::InvalidWorkspace`] for an index outside the grid and
  /// [`Error::Core`] for an unknown window.
  pub fn move_window_to_workspace(
    &mut self,
    window: WindowId,
    index: usize,
  ) -> Result<(), Error> {
    self.state.move_window_to_workspace(window, index)
  }

  /// Finishes a tile drag: moves `source` next to `target` in the tiling tree,
  /// on the `direction` side, then focuses `source`.
  pub fn drop_window(
    &mut self,
    source: WindowId,
    target: WindowId,
    direction: Direction,
  ) -> Result<(), Error> {
    input::drop_window(&mut self.state, source, target, direction)
  }

  /// Grows (positive `delta`) or shrinks the window's edge facing `direction`
  /// by `delta` logical pixels. Floating windows change their own rectangle,
  /// limited by `minimum_extent`, the client's min/max size and the resize
  /// plugin's minimums; tiled windows move the split with their neighbour.
  /// Returns whether anything changed.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for an unknown window or a zero `minimum_extent`.
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

  /// Aspect-preserving interactive resize of a floating window: applies the
  /// pointer's total motion `(delta_x, delta_y)` since the drag began to
  /// `original`, the rectangle at its start, on the given edges, and keeps
  /// `original`'s width-to-height ratio within the size limits. The opposite
  /// edges stay put, also when a size limit is reached. Returns `false`
  /// when the window is not floating or the rectangle did not change.
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

  /// Moves a floating window by `(dx, dy)` logical pixels, saturating at the
  /// `i32` range. Returns `false` for a tiled window or a zero move.
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

  /// [`Compositor::move_floating`] for `window` and, with `join_views`, every
  /// transient child toplevel below it (move's `join_views` option).
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
          match surface.role.toplevel() {
            Some(toplevel) if toplevel.parent == Some(parent) => {
              toplevel.window()
            }
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

  /// Toggles maximized state and returns the new state. A maximized
  /// floating window is drawn over the whole workarea.
  pub fn toggle_maximized(&mut self, window: WindowId) -> Result<bool, Error> {
    input::toggle_maximized(&mut self.state, window)
  }

  /// Floats a tiled window (at its saved restore rectangle, or its tile) or
  /// tiles a floating one, then focuses it. Returns whether it is now tiled.
  pub fn toggle_tiling(&mut self, window: WindowId) -> Result<bool, Error> {
    shell::toggle_tiling(&mut self.state, window)
  }

  /// The current workspace's workarea: the output minus layer-shell exclusive
  /// zones, in layout logical pixels.
  pub fn workarea(&self) -> Rect {
    self
      .state
      .core
      .current_workspace(&self.state.active_output)
      .ok()
      .and_then(|workspace| {
        self
          .state
          .core
          .tree(&self.state.active_output, workspace)
          .ok()
      })
      .map_or(self.state.primary().rect, |tree| tree.workarea())
  }

  /// The grid slot a window dragged to `(x, y)` would snap to, as Wayfire's
  /// `calc_slot` (`plugins/grid/wayfire/plugins/grid.hpp`) computes it from
  /// move's `snap_threshold` and `quarter_snap_threshold`. Slot names are
  /// those of [`Compositor::snap_window`]; the top edge yields `center`,
  /// which maximizes, as in Wayfire.
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

  /// The workspace direction `(dx, dy)` for a window dragged to an edge or
  /// corner at `(x, y)`, each in `-1..=1`, for move's
  /// `workspace_switch_after`. `None` away from the edges and on the top
  /// edge, which [`Compositor::grid_slot_at`] makes the maximize slot: as in
  /// Wayfire's `update_workspace_switch_timeout`
  /// (`plugins/single_plugins/move.cpp`), only the top corners switch up,
  /// diagonally.
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
      Some("top_right") => Some((1, -1)),
      Some("left") => Some((-1, 0)),
      Some("right") => Some((1, 0)),
      Some("bottom_left") => Some((-1, 1)),
      Some("bottom") => Some((0, 1)),
      Some("bottom_right") => Some((1, 1)),
      _ => None,
    }
  }

  /// The rectangle grid slot `slot` covers in the current workarea, or `None`
  /// for an unknown slot name.
  pub fn snap_geometry(&self, slot: &str) -> Option<Rect> {
    shell::snap_geometry(self.workarea(), slot)
  }

  /// Snaps a window into grid slot `slot`: `top_left`, `top`, `top_right`,
  /// `left`, `center`, `right`, `bottom_left`, `bottom` or `bottom_right`. It
  /// joins the tiled layout with the matching Wayfire tiled-edges mask, and
  /// maximized or fullscreen state ends. Returns `false` for an unknown slot.
  pub fn snap_window(
    &mut self,
    window: WindowId,
    slot: &str,
  ) -> Result<bool, Error> {
    shell::snap_window(&mut self.state, window, slot)
  }

  /// Floats a snapped window again, at its restore rectangle when it has
  /// one. Returns `false` if it was not snapped.
  pub fn restore_snapped(&mut self, window: WindowId) -> Result<bool, Error> {
    if self.state.core.window_info(window)?.state.snapped.is_none() {
      return Ok(false);
    }
    self.toggle_tiling(window)?;
    Ok(true)
  }

  /// Minimizes or restores a window. Minimizing moves focus to the next
  /// window; restoring focuses the window and ends show-desktop.
  pub fn set_minimized(
    &mut self,
    window: WindowId,
    minimized: bool,
  ) -> Result<(), Error> {
    shell::set_minimized(&mut self.state, window, minimized)
  }

  /// Minimizes the focused window and returns it.
  pub fn minimize_focused(&mut self) -> Option<WindowId> {
    let window = self.focused_window()?;
    self.set_minimized(window, true).ok()?;
    Some(window)
  }

  /// Sets fullscreen state. A fullscreen window covers the whole output, above
  /// the top layer-shell layer, while it is focused.
  pub fn set_fullscreen(
    &mut self,
    window: WindowId,
    fullscreen: bool,
  ) -> Result<(), Error> {
    shell::set_fullscreen(&mut self.state, window, fullscreen)
  }

  /// Toggles fullscreen state and returns the new state.
  pub fn toggle_fullscreen(&mut self, window: WindowId) -> Result<bool, Error> {
    let fullscreen = !self.state.core.window_info(window)?.state.fullscreen;
    self.set_fullscreen(window, fullscreen)?;
    Ok(fullscreen)
  }

  /// Sets sticky state: a sticky window shows on every workspace of its
  /// output.
  pub fn set_sticky(
    &mut self,
    window: WindowId,
    sticky: bool,
  ) -> Result<(), Error> {
    shell::set_sticky(&mut self.state, window, sticky)
  }

  /// Toggles sticky state and returns the new state.
  pub fn toggle_sticky(&mut self, window: WindowId) -> Result<bool, Error> {
    let sticky = !self.state.core.window_info(window)?.state.sticky;
    self.set_sticky(window, sticky)?;
    Ok(sticky)
  }

  /// Moves the window to stacking layer `mode` (always-on-bottom, normal or
  /// always-on-top). Emits [`Event::StackingChanged`] only for a change.
  pub fn set_stacking(
    &mut self,
    window: WindowId,
    mode: StackingMode,
  ) -> Result<(), Error> {
    shell::set_stacking(&mut self.state, window, mode)
  }

  /// Toggles between always-on-top and normal stacking; returns whether the
  /// window is now always on top.
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

  /// Toggles between always-on-bottom and normal stacking; returns whether the
  /// window is now always on bottom.
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

  /// Turns Wayfire's show-desktop on or off; see
  /// [`Compositor::toggle_show_desktop`].
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

  /// Wayfire's `wm-actions/toggle_showdesktop`
  /// (`plugins/wm-actions/wm-actions.cpp`): minimizes every unminimized window
  /// on the current workspace and clears focus, or restores exactly those
  /// windows and the previous focus. Returns the new state.
  pub fn toggle_show_desktop(&mut self) -> bool {
    if self.state.show_desktop {
      shell::disable_show_desktop(&mut self.state);
    } else {
      shell::enable_show_desktop(&mut self.state);
    }
    self.state.show_desktop
  }

  /// The exact output scale, possibly fractional.
  pub fn output_scale(&self) -> f64 {
    self.state.primary().scale
  }

  /// The gamma table to apply to the first output: 256 red, then 256
  /// green, then 256 blue 16-bit entries, from its wlr-gamma-control
  /// client, or the identity ramp without one.
  pub fn gamma_ramp(&self) -> &[u16] {
    gamma_control::ramp(&self.state, &self.state.primary().id)
  }

  /// [`Compositor::gamma_ramp`] of `output`; the identity ramp for an
  /// output that is not there.
  pub fn gamma_ramp_of(&self, output: &OutputHandle) -> &[u16] {
    gamma_control::ramp(&self.state, &output.id())
  }

  /// Whether `output` is lit: idle DPMS is on and no wlr-output-power
  /// client turned it off. An output that is not lit draws nothing.
  pub fn output_powered(&self, output: &OutputHandle) -> bool {
    output_power::is_on(&self.state, &output.id())
  }

  /// Sets the output scale. Clients get `wl_output.scale` rounded up, the
  /// exact value through fractional-scale-v1, and a new mode size.
  ///
  /// # Errors
  ///
  /// [`Error::InvalidOutputScale`] for a zero, negative or non-finite scale.
  pub fn set_output_scale(&mut self, scale: f64) -> Result<(), Error> {
    self.state.set_output_scale(scale)
  }

  /// The output transform, as `wl_output.geometry` reports it.
  pub fn output_transform(
    &self,
  ) -> wayland_server::protocol::wl_output::Transform {
    self.state.primary().transform
  }

  /// Changes the output transform. Switching between upright and quarter-turn
  /// transforms swaps the output's logical width and height; layer surfaces,
  /// lock surfaces and toplevels are reconfigured.
  pub fn set_output_transform(
    &mut self,
    transform: wayland_server::protocol::wl_output::Transform,
  ) {
    self.state.set_output_transform(transform);
  }

  /// Converts a display size in physical pixels to the output's logical size:
  /// divided by the scale, rounded, at least 1, and swapped for quarter-turn
  /// transforms. A host passes its window or mode size and feeds the result
  /// to [`Compositor::set_output`].
  pub fn logical_output_size(
    &self,
    physical_width: u32,
    physical_height: u32,
  ) -> (u32, u32) {
    output_transform::logical_size(
      physical_width,
      physical_height,
      self.state.primary().scale,
      self.state.primary().transform,
    )
  }

  /// Maps a point in display pixels, on a `display_width` x `display_height`
  /// surface as the host draws it, to layout logical pixels, undoing the
  /// output transform and adding the output's position. The inverse of
  /// [`Compositor::unmap_output_point`].
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
      self.state.primary().rect.width,
      self.state.primary().rect.height,
      self.state.primary().transform,
    );
    (
      x + f64::from(self.state.primary().rect.x),
      y + f64::from(self.state.primary().rect.y),
    )
  }

  /// Maps a point in layout logical pixels to display pixels; the inverse of
  /// [`Compositor::map_output_point`]. Used to place the host cursor after a
  /// warp.
  pub fn unmap_output_point(
    &self,
    x: f64,
    y: f64,
    display_width: u32,
    display_height: u32,
  ) -> (f64, f64) {
    output_transform::unmap_point(
      x - f64::from(self.state.primary().rect.x),
      y - f64::from(self.state.primary().rect.y),
      display_width,
      display_height,
      self.state.primary().rect.width,
      self.state.primary().rect.height,
      self.state.primary().transform,
    )
  }

  /// Maps a relative motion in display pixels to logical pixels, like
  /// [`Compositor::map_output_point`] but without the translation.
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
      self.state.primary().rect.width,
      self.state.primary().rect.height,
      self.state.primary().transform,
    )
  }

  /// Sets the output's position and logical size, after the transform.
  /// Clients get the new geometry and mode, the workarea is recomputed,
  /// layer, lock and toplevel surfaces are reconfigured and
  /// [`Event::OutputChanged`] is emitted.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for a zero-sized rectangle or one past the `i32` range.
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

  /// Tells the compositor that the backend's input devices apply natural
  /// scrolling themselves, as the native backend has libinput do per
  /// device (`mouse_natural_scroll` for mice, `natural_scroll` for
  /// touchpads, as in Wayfire). The compositor then no longer inverts axis
  /// values for `mouse_natural_scroll`. Like
  /// [`Compositor::set_default_cursor`] it is a property of the backend,
  /// so config reloads keep it.
  pub fn set_device_natural_scroll(&mut self, enabled: bool) {
    self.state.input.device_natural_scroll = enabled;
  }

  /// Returns the description of the output clients currently see.
  pub fn output_info(&self) -> &OutputInfo {
    &self.state.primary().info
  }

  /// Describes the physical output to clients: `wl_output` geometry, mode,
  /// name and description, xdg-output, wlr-output-management heads and
  /// modes, and the IPC output name. Outputs and heads that clients have
  /// already bound get the new geometry, refresh rate, description and mode
  /// list at once.
  ///
  /// A new name is a new output. Wayland names an output once per binding,
  /// so the `wl_output` global is replaced: clients see the old one removed
  /// and a new one announced, surfaces leave the old output, and its
  /// xdg-outputs, power and gamma controls, output-management heads and
  /// workspace groups are finished. The core output is renamed, so windows
  /// keep their tiling trees and the session is saved under the new name,
  /// and the configured placement for that name (position, scale,
  /// transform) is applied. A backend that knows its output at startup
  /// describes it before clients connect, so they never see the default.
  ///
  /// A backend that lists modes ([`OutputInfo::modes`]) also decides the
  /// output's size: it passes the mode's size to
  /// [`Compositor::set_output`] (through the frontend), config reloads keep
  /// it, and wlr-output-management mode changes become
  /// [`Compositor::take_output_requests`].
  pub fn set_output_info(&mut self, info: OutputInfo) {
    let output = OutputHandle::from(&self.state.primary().id);
    self.describe_output(&output, info);
  }

  /// [`Compositor::set_output_info`] for any output. A new name renames
  /// the output: clients see its global replaced, and if a workspace set
  /// waits, parked, for an output of that name or EDID identity, the
  /// output shows it with its windows. Does nothing for an unknown output.
  pub fn describe_output(&mut self, output: &OutputHandle, info: OutputInfo) {
    let id = output.id();
    let Some(current) = self.state.outputs.get(&id) else {
      return;
    };
    let renamed = info.name != current.info.name;
    let modes_changed = info.modes != current.info.modes;
    if !renamed {
      self.state.set_info_of(&id, info);
      if modes_changed {
        output_management::modes_changed(&mut self.state, &id);
      }
      return;
    }
    let name = info.name.clone();
    if self
      .state
      .outputs
      .get(&OutputId::new(name.clone()))
      .is_some()
    {
      eprintln!("shingle: renaming {} to {name} failed: taken", id.as_str());
      return;
    }
    if let Some(output) = self.state.outputs.get_mut(&id) {
      output.info = info;
    }
    let id = match output::rename(&mut self.state, &id, &name) {
      Ok(id) => id,
      Err(error) => {
        eprintln!("shingle: renaming the output to {name} failed: {error}");
        return;
      }
    };
    let placement = self.state.placements.get(&name);
    let physical = output::physical_size(&self.state, &id);
    if let Err(error) = output::place(&mut self.state, &id, placement, physical)
    {
      eprintln!("shingle: placing output {name} failed: {error}");
    }
  }

  /// Adds an output described by `info` with a size of `physical` pixels,
  /// before the transform. Its configuration entry, by name, places and
  /// scales it, with `auto` positions placed left to right after the fixed
  /// ones, as wlroots does. It shows the parked workspace set of the same
  /// EDID identity or name, whose windows away elsewhere come back, or a
  /// new set with the lowest free index. Clients see a new `wl_output`
  /// global and output-management head, and IPC watchers `output-added`.
  ///
  /// # Errors
  ///
  /// [`Error::OutputExists`] for a name that another output has,
  /// [`Error::InvalidOutputScale`] for a bad configured scale, and
  /// [`Error::Core`] for an empty name or size.
  pub fn add_output(
    &mut self,
    info: OutputInfo,
    physical: (u32, u32),
  ) -> Result<OutputHandle, Error> {
    output::add(&mut self.state, info, physical, OutputKind::Backend)
      .map(|id| OutputHandle::from(&id))
  }

  /// The size in physical pixels, before the transform, that the
  /// configuration gives an output called `name` when the backend has no
  /// mode of its own, as in headless: its explicit mode, else its logical
  /// size times its scale.
  pub fn configured_physical_size(&self, name: &str) -> (u32, u32) {
    let placement = self.state.placements.get(name);
    let scale = |extent: u32| (f64::from(extent) * placement.scale).round();
    (
      scale(placement.width).clamp(1.0, f64::from(u32::MAX)) as u32,
      scale(placement.height).clamp(1.0, f64::from(u32::MAX)) as u32,
    )
  }

  /// Removes an output whose monitor, window or connector is gone. Its
  /// windows are handled by `output_layout.unplug` (Wayfire's `move`,
  /// preserve-output's `park`, or `return`, the default, which moves them
  /// to the active output and back when the output returns). The last
  /// output is never removed: it stays described, marked dark, draws
  /// nothing and gets no frames until another output is added, which then
  /// takes its windows.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for an unknown output.
  pub fn remove_output(&mut self, output: &OutputHandle) -> Result<(), Error> {
    output::remove(&mut self.state, &output.id())
  }

  /// Gives an output a new size in physical pixels, before the transform,
  /// keeping its scale and transform. Outputs placed after it move.
  ///
  /// # Errors
  ///
  /// [`Error::Core`] for an empty size.
  pub fn set_output_size(
    &mut self,
    output: &OutputHandle,
    physical: (u32, u32),
  ) -> Result<(), Error> {
    let id = output.id();
    let Some(current) = self.state.outputs.get(&id) else {
      return Ok(());
    };
    let (width, height) = output_transform::logical_size(
      physical.0,
      physical.1,
      current.scale,
      current.transform,
    );
    let rect = Rect::new(current.rect.x, current.rect.y, width, height);
    self.state.set_rect_of(&id, rect, false)
  }

  /// Every output, in the order they were added.
  pub fn outputs(&self) -> Vec<OutputView> {
    self
      .state
      .outputs
      .iter()
      .map(|output| OutputView::new(&self.state, output))
      .collect()
  }

  /// One output, if it is there.
  pub fn output_view(&self, output: &OutputHandle) -> Option<OutputView> {
    self
      .state
      .outputs
      .get(&output.id())
      .map(|state| OutputView::new(&self.state, state))
  }

  /// The output under a point of the layout, the first added where
  /// outputs overlap.
  pub fn output_at(&self, x: f64, y: f64) -> Option<OutputHandle> {
    self
      .state
      .outputs
      .iter()
      .find(|output| {
        let rect = output.rect;
        x >= f64::from(rect.x)
          && y >= f64::from(rect.y)
          && x < f64::from(rect.x) + f64::from(rect.width)
          && y < f64::from(rect.y) + f64::from(rect.height)
      })
      .map(|output| OutputHandle::from(&output.id))
  }

  /// The focused output, which workspace bindings, new windows and IPC
  /// without an output act on.
  pub fn active_output(&self) -> OutputHandle {
    OutputHandle::from(&self.state.active().id)
  }

  /// The first output added that is still there. [`Compositor::output`],
  /// [`Compositor::visible_surfaces`] and the other calls without an
  /// output mean this one; the nested and native hosts draw only it until
  /// they have one window or CRTC per output (MO-6, MO-7).
  pub fn primary_output(&self) -> OutputHandle {
    OutputHandle::from(&self.state.primary().id)
  }

  /// The surfaces to draw on one output, bottom to top, as
  /// [`Compositor::visible_surfaces`] lists them for the first: its layer
  /// surfaces, the windows of the workspace set it shows, the unmanaged X
  /// windows that belong to it and the cursor while the pointer is on it.
  /// With `behavior.remove_output_limits`, windows of other outputs that
  /// overlap it follow its own windows. A dark output, or one turned off,
  /// shows nothing.
  pub fn visible_surfaces_on(
    &self,
    output: &OutputHandle,
  ) -> Vec<SurfaceSnapshot> {
    self.state.visible_surfaces_on(&output.id())
  }

  /// The surfaces of `window` bottom to top: its toplevel, then its
  /// subsurfaces and popups, as [`Compositor::visible_surfaces_on`] lists
  /// them. Empty for a window without a mapped buffer. The frontend draws
  /// a window it drags on every output the window crosses with them.
  pub fn window_surfaces(&self, window: WindowId) -> Vec<SurfaceSnapshot> {
    self.state.window_surfaces(window)
  }

  /// Whether any surface visible on `output` has asked for asynchronous
  /// presentation ([`Compositor::allows_tearing`] for one output).
  pub fn allows_tearing_on(&self, output: &OutputHandle) -> bool {
    self
      .state
      .visible_surfaces_on(&output.id())
      .iter()
      .any(|snapshot| {
        self
          .state
          .surface_ids
          .get(&snapshot.id)
          .and_then(|object| self.state.surfaces.get(object))
          .is_some_and(|surface| surface.tearing)
      })
  }

  /// Settles a restored session once the backend has added its outputs
  /// ([`Compositor::with_core`] parks every saved workspace set, and each
  /// added output takes its own): saved sets that no output took are
  /// handled by `output_layout.unplug` as if their outputs had just gone,
  /// and the saved active output is focused again if it is there. Only
  /// the first call does anything; the first [`Compositor::poll`] calls it
  /// if the host did not.
  pub fn restore_outputs(&mut self) {
    output::restore(&mut self.state);
  }

  /// Takes the mode changes wlr-output-management clients asked for since
  /// the last call. Only a backend that lists modes in [`OutputInfo`] gets
  /// any. It tests each one, for a native output with a KMS `TEST_ONLY`
  /// commit, and unless [`OutputRequest::test_only`] makes it, describes the
  /// new current mode with [`Compositor::set_output_info`] and resizes the
  /// output, and then calls [`Compositor::finish_output_request`].
  pub fn take_output_requests(&mut self) -> Vec<OutputRequest> {
    std::mem::take(&mut self.state.output_requests)
  }

  /// Answers the client behind `request`: `failed` unless `ok`, else
  /// `succeeded`, after applying the position, scale and transform it asked
  /// for around the current mode, unless it only tested.
  pub fn finish_output_request(&mut self, request: OutputRequestId, ok: bool) {
    output_management::finish_request(&mut self.state, request, ok);
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
            xwayland: false,
          }),
        )?;
        self.state.launch_templates.insert(client.id(), launch);
        self.state.dispatch_pending = true;
      }
    }
    Ok(())
  }

  /// Adds a Wayland client marked as Shingle's Xwayland, for tests of
  /// `xwayland_shell_v1`, and returns the client's end of its socket.
  #[cfg(test)]
  pub(crate) fn connect_test_xwayland(
    &mut self,
  ) -> std::os::unix::net::UnixStream {
    let (client, server) = std::os::unix::net::UnixStream::pair()
      .expect("a socket pair for the test Xwayland");
    self
      .display
      .handle()
      .insert_client(
        server,
        Arc::new(ClientDataImpl {
          disconnected: self.state.disconnected.clone(),
          sandbox: None,
          privileged_protocols: self
            .state
            .security_context
            .privileged_protocols
            .clone(),
          xwayland: true,
        }),
      )
      .expect("the test Xwayland is added");
    self.state.dispatch_pending = true;
    client
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
          xwayland: false,
        }),
      )?;
      self.state.launch_templates.insert(client.id(), launch);
    }
    self.state.dispatch_pending = true;
    Ok(())
  }
}

impl Drop for Compositor {
  /// Stops the X window manager and Xwayland before the Wayland clients
  /// are destroyed, as Wayfire does on shutdown, then removes the X
  /// display's sockets and lock.
  fn drop(&mut self) {
    self.stop_xwayland();
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

pub(crate) fn parse_output_transform(
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
  fn disconnected(&self, client: ClientId, reason: DisconnectReason) {
    // Xwayland exits with its connection; the pidfd reports that.
    if self.xwayland {
      eprintln!("shingle: Xwayland's Wayland connection closed: {reason:?}");
    }
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
    test_compositor_with(Config::default())
  }

  #[test]
  fn a_snapshot_gives_its_pixels_and_prints_them_as_rgba() {
    let snapshot = BufferSnapshot {
      width: 1,
      height: 1,
      source: None,
      scale: 1,
      transform: wayland_server::protocol::wl_output::Transform::Normal,
      content: SnapshotContent::Pixels(vec![1, 2, 3, 4].into()),
      damage: Vec::new(),
      offset_x: 0,
      offset_y: 0,
    };
    assert_eq!(snapshot.rgba().as_deref(), Some(&[1, 2, 3, 4][..]));
    assert_eq!(
      format!("{snapshot:?}"),
      "BufferSnapshot { width: 1, height: 1, source: None, scale: 1, \
       transform: Normal, rgba: [1, 2, 3, 4], damage: [], offset_x: 0, \
       offset_y: 0 }"
    );
  }

  /// [`test_compositor`] with `config`.
  pub(crate) fn test_compositor_with(config: Config) -> (Compositor, PathBuf) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
      "shingle-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(&name);
    let socket = ListeningSocket::bind_absolute(path.clone())
      .expect("the temporary directory accepts a test socket");
    let compositor =
      Compositor::with_socket(config, Shingle::new(), socket, name.into())
        .expect("the config builds a compositor");
    (compositor, path)
  }

  #[test]
  fn the_physical_output_size_follows_the_scale() {
    let (compositor, _path) = test_compositor();
    assert_eq!(compositor.output_physical_size(), (1280, 720));
    let (compositor, _path) = test_compositor_with(Config {
      output: Rect::new(0, 0, 640, 360),
      output_scale: 2.0,
      ..Config::default()
    });
    assert_eq!(compositor.output_physical_size(), (1280, 720));
    assert!(monotonic_time() > Duration::ZERO);
  }

  #[test]
  fn client_environment_names_this_compositor_and_its_ipc() {
    let (mut compositor, path) = test_compositor();
    let environment = compositor.client_environment();
    assert_eq!(
      environment.value("WAYLAND_DISPLAY"),
      Some(compositor.socket_name())
    );
    assert_eq!(environment.value("WAYFIRE_SOCKET"), None);
    assert_eq!(environment.value("DISPLAY"), None);
    assert!(
      environment
        .variables()
        .iter()
        .any(|(name, value)| name == "WAYFIRE_SOCKET" && value.is_none()),
      "a host WAYFIRE_SOCKET is removed"
    );
    let ipc = path.with_extension("ipc");
    compositor.enable_ipc(&ipc).unwrap();
    let environment = compositor.client_environment();
    assert_eq!(environment.value("WAYFIRE_SOCKET"), Some(ipc.as_os_str()));
    assert_eq!(environment.value("DISPLAY"), None);
  }

  #[test]
  fn stipc_run_starts_the_command_in_the_client_environment() {
    let (mut compositor, path) = test_compositor();
    let ipc = path.with_extension("ipc");
    compositor.enable_ipc(&ipc).unwrap();
    let out = path.with_extension("env");
    let command = format!(
      "printf '%s|%s|%s' \"${{DISPLAY-unset}}\" \"$WAYFIRE_SOCKET\" \
       \"$WAYLAND_DISPLAY\" > {}",
      out.display()
    );
    let reply = ipc::respond(
      &mut compositor.state,
      &serde_json::json!({"method": "stipc/run", "data": {"cmd": command}}),
    );
    assert_eq!(reply["result"], "ok", "{reply}");
    let mut child = compositor.state.ipc_children.pop().expect("a child");
    assert!(child.wait().unwrap().success());
    assert_eq!(
      std::fs::read_to_string(&out).unwrap(),
      format!(
        "unset|{}|{}",
        ipc.display(),
        compositor.socket_name().to_string_lossy()
      )
    );
    std::fs::remove_file(out).unwrap();
  }

  #[test]
  fn keyboard_layout_index_follows_set_keyboard_state() {
    let (mut compositor, _) = test_compositor();
    let keymap = shingle_config::InputConfig {
      layout: "us,de".into(),
      ..shingle_config::InputConfig::default()
    }
    .keymap()
    .unwrap();
    compositor.set_keymap(&keymap).expect("the keymap compiles");
    assert_eq!(compositor.keyboard_layout_index(), 0);
    let reply = ipc::respond(
      &mut compositor.state,
      &serde_json::json!({
        "method": "wayfire/set-keyboard-state",
        "data": {"layout-index": 1},
      }),
    );
    assert_eq!(reply["result"], "ok");
    assert_eq!(compositor.keyboard_layout_index(), 1);
    compositor.set_keymap(&keymap).expect("the keymap compiles");
    assert_eq!(compositor.keyboard_layout_index(), 0);
  }

  /// Maps one 64x64 toplevel with `app_id` and returns its window, with the
  /// events so far drained.
  fn mapped_window(
    compositor: &mut Compositor,
    path: &std::path::Path,
    app_id: &str,
  ) -> (crate::test_client::TestClient, WindowId) {
    let mut client = crate::test_client::TestClient::connect(compositor, path);
    let toplevel = client.toplevel(compositor, app_id);
    client.attach_buffer(compositor, &toplevel.surface, 64, 64);
    let window = compositor
      .focused_window()
      .expect("the toplevel is focused");
    compositor.drain_events();
    (client, window)
  }

  #[test]
  fn setting_a_window_state_it_already_has_reports_nothing() {
    let (mut compositor, path) = test_compositor();
    let (_client, window) = mapped_window(&mut compositor, &path, "same");
    let changes = |compositor: &mut Compositor| {
      compositor
        .drain_events()
        .into_iter()
        .filter(|event| {
          matches!(
            event,
            Event::FullscreenChanged { .. }
              | Event::StickyChanged { .. }
              | Event::MinimizedChanged { .. }
              | Event::MaximizedChanged { .. }
          )
        })
        .count()
    };
    compositor.set_fullscreen(window, false).unwrap();
    compositor.set_sticky(window, false).unwrap();
    compositor.set_minimized(window, false).unwrap();
    shell::set_maximized(&mut compositor.state, window, false).unwrap();
    assert_eq!(changes(&mut compositor), 0);
    compositor.set_fullscreen(window, true).unwrap();
    compositor.set_sticky(window, true).unwrap();
    shell::set_maximized(&mut compositor.state, window, true).unwrap();
    compositor.set_minimized(window, true).unwrap();
    assert_eq!(changes(&mut compositor), 4);
    compositor.set_fullscreen(window, true).unwrap();
    compositor.set_sticky(window, true).unwrap();
    shell::set_maximized(&mut compositor.state, window, true).unwrap();
    compositor.set_minimized(window, true).unwrap();
    assert_eq!(changes(&mut compositor), 0);
  }

  #[test]
  fn an_invalid_reloaded_output_changes_nothing() {
    use wayland_server::protocol::wl_output;
    let (mut compositor, _) = test_compositor();
    let grid = compositor.workspace_grid_dimensions();
    let background = compositor.state.background_color;
    use shingle_config::OutputPosition;
    for (scale, output, output_position) in [
      (f64::NAN, Rect::new(0, 0, 800, 600), OutputPosition::Auto),
      (1.0, Rect::new(0, 0, 0, 600), OutputPosition::Auto),
      (
        1.0,
        Rect::new(0, 0, 800, 600),
        OutputPosition::Fixed { x: i32::MAX, y: 0 },
      ),
    ] {
      let config = Config {
        workspace_width: 4,
        workspace_height: 2,
        background_color: [1.0, 0.0, 0.0, 1.0],
        output_transform: wl_output::Transform::_90,
        output_scale: scale,
        output,
        output_position,
        ..Config::default()
      };
      assert!(compositor.apply_config(config).is_err());
      assert_eq!(compositor.workspace_grid_dimensions(), grid);
      assert_eq!(compositor.state.background_color, background);
      assert_eq!(
        compositor.state.primary().transform,
        wl_output::Transform::Normal
      );
      assert_eq!(compositor.output_scale(), 1.0);
    }
  }

  #[test]
  fn staying_on_a_workspace_reports_and_reorders_nothing() {
    let (mut compositor, path) = test_compositor();
    let (mut client, first) = mapped_window(&mut compositor, &path, "first");
    let second_toplevel = client.toplevel(&mut compositor, "second");
    client.attach_buffer(&mut compositor, &second_toplevel.surface, 64, 64);
    let order = |compositor: &Compositor| {
      let workspace = compositor
        .state
        .core
        .current_workspace(&compositor.state.active_output)
        .unwrap();
      compositor
        .state
        .core
        .tree(&compositor.state.active_output, workspace)
        .unwrap()
        .windows()
    };
    let before = order(&compositor);
    compositor.drain_events();
    compositor.switch_workspace(0).unwrap();
    compositor.move_window_to_workspace(first, 0).unwrap();
    let events = compositor.drain_events();
    assert!(
      !events.iter().any(|event| matches!(
        event,
        Event::WorkspaceChanged { .. } | Event::WindowMoved { .. }
      )),
      "{events:?}"
    );
    assert_eq!(order(&compositor), before);
  }

  #[test]
  fn dragging_to_the_top_edge_maximizes_and_switches_no_workspace() {
    let (compositor, _) = test_compositor();
    let area = compositor.workarea();
    let middle = f64::from(area.x) + f64::from(area.width) / 2.0;
    let top = f64::from(area.y) + 2.0;
    let right = f64::from(area.x) + f64::from(area.width) - 2.0;
    let bottom = f64::from(area.y) + f64::from(area.height) - 2.0;
    let at = |x, y| {
      (
        compositor.grid_slot_at(x, y, 10, 50),
        compositor.workspace_drag_direction_at(x, y, 10, 50),
      )
    };
    // Wayfire's calc_slot makes the top edge the centre slot, which
    // move.cpp's update_workspace_switch_timeout gives no direction.
    assert_eq!(at(middle, top), (Some("center"), None));
    assert_eq!(
      at(f64::from(area.x) + 2.0, top),
      (Some("top_left"), Some((-1, -1)))
    );
    assert_eq!(at(right, top), (Some("top_right"), Some((1, -1))));
    assert_eq!(at(middle, bottom), (Some("bottom"), Some((0, 1))));
    assert_eq!(
      at(right, f64::from(area.y) + 300.0),
      (Some("right"), Some((1, 0)))
    );
  }

  /// Commits `damage` in surface coordinates on a toplevel whose 1:1
  /// buffer was attached after `setup`, and returns the damage the
  /// renderer is given, in buffer pixels.
  fn buffer_damage(
    width: i32,
    height: i32,
    setup: impl FnOnce(
      &crate::test_client::TestClient,
      &wayland_client::protocol::wl_surface::WlSurface,
    ),
    damage: Rect,
  ) -> Vec<Rect> {
    let (mut compositor, path) = test_compositor();
    let mut client =
      crate::test_client::TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "damaged");
    setup(&client, &toplevel.surface);
    client.attach_buffer(&mut compositor, &toplevel.surface, width, height);
    compositor.drain_events();
    toplevel.surface.damage(
      damage.x,
      damage.y,
      damage.width as i32,
      damage.height as i32,
    );
    toplevel.surface.commit();
    client.roundtrip(&mut compositor);
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::SurfaceCommitted(snapshot) => snapshot.buffer,
        _ => None,
      })
      .last()
      .expect("the commit reached the renderer")
      .damage
  }

  #[test]
  fn surface_damage_follows_the_buffer_transform_scale_and_viewport() {
    use wayland_client::protocol::wl_output::Transform;
    use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

    // A 100x50 buffer turned a quarter: the surface is 50x100, and its
    // top-left 10x20 is the buffer's bottom-left 20x10.
    let rotated = buffer_damage(
      100,
      50,
      |_, surface| surface.set_buffer_transform(Transform::_90),
      Rect::new(0, 0, 10, 20),
    );
    assert_eq!(rotated, [Rect::new(0, 40, 20, 10)]);

    // Upside down at scale 2: the surface's top-left 5x5 is the buffer's
    // bottom-right 10x10.
    let turned = buffer_damage(
      100,
      60,
      |_, surface| {
        surface.set_buffer_transform(Transform::_180);
        surface.set_buffer_scale(2);
      },
      Rect::new(0, 0, 5, 5),
    );
    assert_eq!(turned, [Rect::new(90, 50, 10, 10)]);

    // The bottom-right quarter of a 100x100 buffer shown at 25x25: the
    // surface's top-left 5x5 is 10x10 buffer pixels from (50, 50).
    let cropped = buffer_damage(
      100,
      100,
      |client, surface| {
        let viewporter = client.bind::<WpViewporter>(1);
        let viewport = viewporter.get_viewport(surface, &client.handle, ());
        viewport.set_source(50.0, 50.0, 50.0, 50.0);
        viewport.set_destination(25, 25);
      },
      Rect::new(0, 0, 5, 5),
    );
    assert_eq!(cropped, [Rect::new(50, 50, 10, 10)]);
  }

  #[test]
  fn each_dpms_change_is_reported_once() {
    let (mut compositor, _) = test_compositor();
    let dpms_events = |compositor: &mut Compositor| {
      compositor
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
          Event::DpmsChanged(state) => Some(state),
          _ => None,
        })
        .collect::<Vec<_>>()
    };
    compositor.drain_events();
    compositor.set_dpms(DpmsState::Off);
    assert_eq!(dpms_events(&mut compositor), [DpmsState::Off]);
    compositor.set_dpms(DpmsState::Off);
    assert_eq!(dpms_events(&mut compositor), []);
    compositor.activity();
    assert_eq!(dpms_events(&mut compositor), [DpmsState::On]);
  }

  #[test]
  fn the_dpms_state_follows_set_dpms_and_activity() {
    let (mut compositor, _) = test_compositor();
    assert_eq!(compositor.dpms_state(), DpmsState::On);
    compositor.set_dpms(DpmsState::Off);
    assert_eq!(compositor.dpms_state(), DpmsState::Off);
    compositor.activity();
    assert_eq!(compositor.dpms_state(), DpmsState::On);
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
      ..OutputInfo::default()
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
  fn output_info_with_the_same_name_updates_bound_outputs() {
    use wayland_client::protocol::wl_output::{Event, WlOutput};
    let (mut compositor, path) = test_compositor();
    let mut client = test_client::TestClient::connect(&mut compositor, &path);
    let _output = client.bind::<WlOutput>(4);
    client.roundtrip(&mut compositor);
    client.events.output.clear();
    compositor.set_output_info(OutputInfo {
      name: OUTPUT_NAME.into(),
      ..dell_output()
    });
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
      Event::Description { description }
        if description.ends_with("ABC123 (HEADLESS-1)")
    )));
    assert!(matches!(events.last(), Some(Event::Done)));
    assert!(client.events.globals_removed.is_empty());
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
    let output = compositor.primary_output();
    compositor.frame_presented(
      &output,
      PresentInfo {
        time: Duration::new(5, 250_000_123),
        refresh_ns: 16_666_667,
        seq: (1 << 32) + 2,
        flags: PresentationFlags::Vsync | PresentationFlags::HwClock,
      },
    );
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
