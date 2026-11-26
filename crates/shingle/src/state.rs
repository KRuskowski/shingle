use std::{
  collections::{BTreeSet, HashMap, HashSet},
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use shingle_core::{
  Gaps, LaunchIdentity, OutputId, Rect, Shingle, WindowId, WorkspaceId,
};
use shingle_idle::{Command, DpmsState, IdleCoordinator, LockState};
use shingle_session::{ClientEnvironment, DesktopVariables};
use wayland_protocols::{
  ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
  wp::fractional_scale::v1::server::wp_fractional_scale_v1::WpFractionalScaleV1,
  xdg::{
    decoration::zv1::server::zxdg_toplevel_decoration_v1,
    xdg_output::zv1::server::zxdg_output_v1::ZxdgOutputV1,
  },
};
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::Layer;
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
  protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::{self, WlCallback},
    wl_compositor::{self, WlCompositor},
    wl_output::{self, WlOutput},
    wl_region::WlRegion,
    wl_shm::{self, WlShm},
    wl_shm_pool::{self, WlShmPool},
    wl_surface::{self, WlSurface},
  },
};

use crate::{
  BufferSnapshot, Config, Event, OUTPUT_NAME, SurfaceSnapshot,
  buffer::{Attachment, ShmPool},
  buffer_lock::{BufferLock, ReleaseQueue},
  input::InputState,
  shell::{self, Role},
};

/// Identifies a `wl_surface` for the frontend. IDs count up from 1 in
/// creation order and are not reused while the compositor runs (the
/// counter would have to wrap first), so a frontend can key textures by
/// them until `Event::SurfaceDestroyed`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceId(pub u64);

impl SurfaceId {
  /// The raw ID.
  pub const fn get(self) -> u64 {
    self.0
  }
}

pub(crate) struct ClientDataImpl {
  pub(crate) disconnected: Arc<Mutex<Vec<ClientId>>>,
  pub(crate) sandbox: Option<crate::security_context::SandboxMetadata>,
  pub(crate) privileged_protocols: Arc<HashSet<String>>,
  /// The connection is Xwayland's own, the one Shingle started it with.
  pub(crate) xwayland: bool,
}

pub(crate) struct FrameCallback {
  pub(crate) surface: SurfaceId,
  pub(crate) resource: WlCallback,
}

/// Tracks the IPC clients which must approve a view's first map.
pub(crate) struct PendingMap {
  pub(crate) waiting: HashSet<u64>,
  pub(crate) notified: bool,
}

/// The buffer part of a pending commit.
pub(crate) enum PendingBuffer {
  Unchanged,
  /// A buffer to show, locked from `wl_surface.attach` until the commit
  /// applies, so that replacing or dropping the state releases it.
  Attach(Attachment, BufferLock),
  Detach,
}

pub(crate) struct PendingSurface {
  pub(crate) buffer: PendingBuffer,
  pub(crate) damage: Vec<Rect>,
  pub(crate) buffer_damage: Vec<Rect>,
  pub(crate) callbacks: Vec<WlCallback>,
  pub(crate) scale: i32,
  pub(crate) transform: wl_output::Transform,
  pub(crate) offset_x: i32,
  pub(crate) offset_y: i32,
  pub(crate) alpha: Option<u32>,
  pub(crate) tearing: Option<bool>,
  pub(crate) input_region: Option<Option<WlRegion>>,
  /// `wp_fifo_v1.set_barrier` for this update.
  pub(crate) fifo_set_barrier: bool,
  /// `wp_fifo_v1.wait_barrier` for this update.
  pub(crate) fifo_wait_barrier: bool,
  /// `wp_commit_timer_v1.set_timestamp` for this update.
  pub(crate) timestamp: Option<std::time::Duration>,
  /// The `linux-drm-syncobj-v1` acquire point the buffer of this update
  /// waits for; its release point is with the buffer's lock.
  pub(crate) acquire: Option<crate::linux_drm_syncobj::SyncPoint>,
}

impl Default for PendingSurface {
  fn default() -> Self {
    Self {
      buffer: PendingBuffer::Unchanged,
      damage: Vec::new(),
      buffer_damage: Vec::new(),
      callbacks: Vec::new(),
      scale: 1,
      transform: wl_output::Transform::Normal,
      offset_x: 0,
      offset_y: 0,
      alpha: None,
      tearing: None,
      input_region: None,
      fifo_set_barrier: false,
      fifo_wait_barrier: false,
      timestamp: None,
      acquire: None,
    }
  }
}

pub(crate) struct Surface {
  pub(crate) id: SurfaceId,
  pub(crate) client: ClientId,
  pub(crate) resource: WlSurface,
  pub(crate) pending: PendingSurface,
  pub(crate) cached_commit: Option<PendingSurface>,
  pub(crate) buffer: Option<BufferSnapshot>,
  pub(crate) role: Role,
  pub(crate) committed_once: bool,
  pub(crate) alpha: u32,
  pub(crate) tearing: bool,
  pub(crate) input_region: Option<Vec<Rect>>,
}

pub(crate) struct CompositorState {
  pub(crate) core: Shingle,
  pub(crate) idle: IdleCoordinator,
  pub(crate) idle_notifications: crate::idle_notify::IdleNotifications,
  pub(crate) gaps: Gaps,
  pub(crate) minimum_extent: u32,
  pub(crate) gamma_control: crate::gamma_control::GammaControl,
  pub(crate) keep_fullscreen_on_adjacent: bool,
  pub(crate) resize: shingle_config::ResizeConfig,
  pub(crate) tile_by_default: shingle_config::ViewMatcher,
  pub(crate) placement_mode: String,
  pub(crate) focus: shingle_config::FocusConfig,
  pub(crate) cascade_position: (i32, i32),
  pub(crate) window_rules: Vec<shingle_config::WindowRule>,
  pub(crate) applying_window_rules: bool,
  pub(crate) background_color: [f64; 4],
  pub(crate) workspace_width: u32,
  pub(crate) workspace_height: u32,
  pub(crate) workspace_wraparound: crate::WorkspaceWraparound,
  pub(crate) cursor_theme: String,
  pub(crate) cursor_size: u32,
  pub(crate) default_cursor: bool,
  /// The outputs in the order they were added; never empty.
  pub(crate) outputs: crate::output::Outputs,
  /// The focused output: workspace bindings, IPC without an output and new
  /// windows use it.
  pub(crate) active_output: OutputId,
  /// What happens to the workspace set of an output that goes.
  pub(crate) unplug: shingle_config::UnplugPolicy,
  /// Whether windows are drawn, and entered, on every output they
  /// overlap rather than only on their own (`remove_output_limits`).
  pub(crate) remove_output_limits: bool,
  /// The `oswitch` and `wsets` bindings, device outputs and focus timeout.
  pub(crate) focus_settings: crate::output::FocusSettings,
  /// The active output that went, to focus again if it returns soon.
  pub(crate) lost_focus: Option<crate::output::LostFocus>,
  /// Whether a restored session's sets were settled
  /// ([`crate::Compositor::restore_outputs`]).
  pub(crate) outputs_restored: bool,
  /// The display, once the globals exist, for globals created later.
  pub(crate) display: Option<DisplayHandle>,
  /// Where the configuration puts outputs, by name.
  pub(crate) placements: crate::output::Placements,
  /// Mode changes waiting for the backend.
  pub(crate) output_requests: Vec<crate::OutputRequest>,
  pub(crate) next_output_request: u64,
  pub(crate) output_power: crate::output_power::OutputPower,
  pub(crate) output_management: crate::output_management::OutputManagement,
  pub(crate) started: Instant,
  pub(crate) surfaces: HashMap<ObjectId, Surface>,
  pub(crate) regions: HashMap<ObjectId, Vec<Rect>>,
  pub(crate) subsurface_order: HashMap<ObjectId, Vec<ObjectId>>,
  pub(crate) viewports: HashMap<ObjectId, crate::viewport::Viewport>,
  pub(crate) fractional_scales: HashMap<ObjectId, WpFractionalScaleV1>,
  /// The outputs each surface was told it is on.
  pub(crate) surface_outputs: crate::surface_outputs::SurfaceOutputs,
  pub(crate) surface_ids: HashMap<SurfaceId, ObjectId>,
  pub(crate) pools: HashMap<ObjectId, Arc<ShmPool>>,
  pub(crate) buffers: HashMap<ObjectId, Attachment>,
  /// Buffers whose last lock dropped, released at the end of each turn.
  pub(crate) buffer_releases: ReleaseQueue,
  pub(crate) commit_queue: crate::commit_queue::CommitQueues,
  /// `zwp_linux_dmabuf_v1`, once a frontend enabled it.
  pub(crate) dmabuf: Option<crate::linux_dmabuf::DmabufState>,
  /// `wp_linux_drm_syncobj_manager_v1`, once a frontend enabled it.
  pub(crate) syncobj: Option<crate::linux_drm_syncobj::SyncobjState>,
  pub(crate) xdg_outputs: HashMap<ObjectId, ZxdgOutputV1>,
  pub(crate) events: Vec<Event>,
  pub(crate) ipc_events: Vec<serde_json::Value>,
  pub(crate) pre_map_clients: HashSet<u64>,
  pub(crate) pending_maps: HashMap<WindowId, PendingMap>,
  pub(crate) delayed_maps: HashSet<WindowId>,
  pub(crate) frame_callbacks: Vec<FrameCallback>,
  /// Whether shm commits keep their buffers for the frontend to upload
  /// (`Compositor::set_shm_upload`).
  pub(crate) shm_upload: bool,
  pub(crate) disconnected: Arc<Mutex<Vec<ClientId>>>,
  pub(crate) foreign_toplevels: crate::foreign_toplevel::ForeignToplevels,
  /// wayfire-shell's outputs, hotspots and surfaces, and the menu key.
  pub(crate) wayfire_shell: crate::wayfire_shell::WayfireShell,
  pub(crate) foreign: crate::foreign::ForeignHandles,
  pub(crate) ext_workspaces: crate::ext_workspace::ExtWorkspaces,
  pub(crate) ext_toplevels: crate::ext_toplevel::ExtToplevels,
  pub(crate) launch_templates: HashMap<ClientId, LaunchIdentity>,
  pub(crate) socket_name: std::ffi::OsString,
  /// Where Wayfire IPC is served, once [`crate::Compositor::enable_ipc`]
  /// ran; children find it as `WAYFIRE_SOCKET`.
  pub(crate) ipc_socket: Option<std::path::PathBuf>,
  /// The Wayfire file the configuration was read from, which children
  /// find as `WAYFIRE_CONFIG_FILE`; see
  /// [`crate::Compositor::set_wayfire_config_file`].
  pub(crate) wayfire_config_file: Option<std::path::PathBuf>,
  pub(crate) ipc_children: Vec<std::process::Child>,
  /// The X display and server, for IPC and the client environment.
  pub(crate) xwayland: crate::xwayland::Status,
  pub(crate) next_surface: u64,
  pub(crate) next_serial: u32,
  pub(crate) active_lock: Option<ExtSessionLockV1>,
  pub(crate) activation: crate::activation::Activation,
  pub(crate) data_devices: crate::data_device::DataDevices,
  pub(crate) data_control: crate::data_control::DataControl,
  pub(crate) primary_selection: crate::primary_selection::PrimarySelection,
  pub(crate) screencopy: crate::screencopy::Screencopy,
  pub(crate) security_context: crate::security_context::SecurityContexts,
  pub(crate) alpha_modifiers: crate::alpha_modifier::AlphaModifiers,
  pub(crate) tearing_controls: crate::tearing::TearingControls,
  pub(crate) fifo: crate::fifo::FifoState,
  pub(crate) commit_timing: crate::commit_timing::CommitTiming,
  pub(crate) shortcut_inhibitors: crate::shortcuts::ShortcutInhibitors,
  pub(crate) pointer_protocols: crate::pointer_protocols::PointerProtocols,
  pub(crate) pointer_gestures: crate::pointer_gestures::PointerGestures,
  pub(crate) presentation: crate::presentation::Presentation,
  pub(crate) virtual_pointers: crate::virtual_pointer::VirtualPointers,
  pub(crate) virtual_keyboards: crate::virtual_keyboard::VirtualKeyboards,
  pub(crate) input: InputState,
  pub(crate) text_inputs: crate::text_input::TextInputs,
  pub(crate) input_methods: crate::input_method::InputMethods,
  pub(crate) input_methods_v2: crate::input_method_v2::InputMethods,
  pub(crate) icons: crate::toplevel_icon::Icons,
  pub(crate) enable_text_input_v3: bool,
  pub(crate) enable_text_input_v1: bool,
  /// Whether views tile (`Config::tiling`).
  pub(crate) tiling: bool,
  /// Which protocol plugins' globals `create_globals` creates.
  pub(crate) protocol_plugins: crate::ProtocolPlugins,
  /// What a Wayfire file set, for `wayfire/get-config-option`.
  pub(crate) wayfire_values: shingle_config::WayfireValues,
  pub(crate) focus_recency: HashMap<shingle_core::WindowId, u64>,
  pub(crate) view_properties:
    HashMap<shingle_core::WindowId, HashMap<String, crate::ipc::ViewProperty>>,
  pub(crate) runtime_bindings:
    std::collections::BTreeMap<u64, crate::RuntimeBinding>,
  /// Whether `stipc/feed_key` hands keys to the frontend as
  /// [`Event::StipcKey`] instead of handling them itself.
  pub(crate) stipc_key_events: bool,
  /// Whether `stipc/move_cursor` and `stipc/feed_button` hand pointer
  /// input to the frontend as [`Event::StipcPointerMotion`] and
  /// [`Event::StipcButton`].
  pub(crate) stipc_pointer_events: bool,
  /// Whether the backend shows each output in a host window that STIPC
  /// opens and closes ([`crate::Compositor::set_host_outputs`]).
  pub(crate) host_outputs: bool,
  pub(crate) focus_sequence: u64,
  pub(crate) dispatch_pending: bool,
  pub(crate) accept_pending: bool,
  pub(crate) idle_pending: bool,
  pub(crate) shutdown: bool,
  pub(crate) show_desktop: bool,
  pub(crate) show_desktop_windows: BTreeSet<WindowId>,
  pub(crate) show_desktop_focus: Option<shingle_core::WindowId>,
  pub(crate) all_dialogs_modal: bool,
  pub(crate) keep_last_toplevel_activated: bool,
  pub(crate) max_buffer_size: u32,
  pub(crate) disable_primary_selection: bool,
  pub(crate) decorations: HashMap<ObjectId, crate::decoration::Decoration>,
  pub(crate) dialogs: crate::dialog::Dialogs,
  pub(crate) preferred_decoration_mode: zxdg_toplevel_decoration_v1::Mode,
  pub(crate) force_preferred_decoration_mode: bool,
  /// Wayfire's `decoration` plugin is loaded.
  pub(crate) decorations_enabled: bool,
  /// `[decoration]`, for which windows get frames.
  pub(crate) decoration: shingle_config::DecorationConfig,
  /// The frame sizes `decoration` gives.
  pub(crate) frame_theme: crate::frame::FrameTheme,
  /// The frame under the pointer.
  pub(crate) frame_pointer: crate::decorator::FramePointer,
  pub(crate) focus_main_surface_instead_of_popup: bool,
  pub(crate) last_workspace: Option<shingle_core::WorkspaceId>,
  last_lock_state: LockState,
  last_dpms_state: DpmsState,
}

impl CompositorState {
  pub(crate) fn new(
    config: Config,
    mut core: Shingle,
    disconnected: Arc<Mutex<Vec<ClientId>>>,
    socket_name: &std::ffi::OsStr,
  ) -> Result<Self, crate::Error> {
    if config.workspace_width == 0 || config.workspace_height == 0 {
      return Err(crate::Error::InvalidWorkspaceGrid);
    }
    let placements = crate::output::Placements::new(&config);
    let focus_settings = crate::output::FocusSettings::new(&config);
    let initial = placements.get(OUTPUT_NAME);
    let mut rect = Rect::new(0, 0, initial.width, initial.height);
    if !initial.scale.is_finite() || initial.scale <= 0.0 {
      return Err(crate::Error::InvalidOutputScale);
    }
    if crate::output_transform::is_quarter_turn(initial.transform) {
      std::mem::swap(&mut rect.width, &mut rect.height);
    }
    rect.validate()?;
    let output_id = OutputId::new(OUTPUT_NAME);
    // A restored session's sets wait, parked, for their outputs, which
    // take them as they are added; `restore_outputs` settles the rest.
    crate::output::park_restored(&mut core);
    core.add_output(output_id.clone(), rect)?;
    let mut outputs = crate::output::Outputs::new();
    outputs.push(
      crate::OutputInfo {
        refresh_mhz: match initial.refresh_mhz {
          0 => crate::OutputInfo::default().refresh_mhz,
          refresh => refresh,
        },
        ..Default::default()
      },
      rect,
      initial.scale,
      initial.transform,
      initial.position,
      crate::OutputKind::Backend,
    );
    let started = Instant::now();
    let idle = IdleCoordinator::new(config.idle, Duration::ZERO);
    let mut state = Self {
      core,
      idle,
      idle_notifications: Default::default(),
      gaps: config.gaps,
      minimum_extent: config.minimum_extent,
      gamma_control: Default::default(),
      keep_fullscreen_on_adjacent: config.keep_fullscreen_on_adjacent,
      resize: config.resize,
      tile_by_default: config.tile_by_default,
      placement_mode: config.placement_mode,
      focus: config.focus,
      cascade_position: (0, 0),
      window_rules: config.window_rules,
      applying_window_rules: false,
      background_color: config.background_color,
      workspace_width: config.workspace_width,
      workspace_height: config.workspace_height,
      workspace_wraparound: config.workspace_wraparound,
      cursor_theme: config.cursor_theme,
      cursor_size: config.cursor_size,
      default_cursor: false,
      outputs,
      active_output: output_id,
      unplug: config.output_unplug,
      remove_output_limits: config.remove_output_limits,
      focus_settings,
      lost_focus: None,
      outputs_restored: false,
      display: None,
      placements,
      output_requests: Vec::new(),
      next_output_request: 0,
      output_power: Default::default(),
      output_management: Default::default(),
      started,
      surfaces: HashMap::new(),
      regions: HashMap::new(),
      subsurface_order: HashMap::new(),
      viewports: HashMap::new(),
      fractional_scales: HashMap::new(),
      surface_outputs: Default::default(),
      surface_ids: HashMap::new(),
      pools: HashMap::new(),
      buffers: HashMap::new(),
      buffer_releases: ReleaseQueue::default(),
      dmabuf: None,
      syncobj: None,
      commit_queue: Default::default(),
      xdg_outputs: HashMap::new(),
      events: Vec::new(),
      ipc_events: Vec::new(),
      pre_map_clients: HashSet::new(),
      pending_maps: HashMap::new(),
      delayed_maps: HashSet::new(),
      frame_callbacks: Vec::new(),
      shm_upload: false,
      disconnected,
      foreign_toplevels: Default::default(),
      wayfire_shell: crate::wayfire_shell::WayfireShell::new(
        config.toggle_menu.clone(),
        config.modifier_binding_timeout,
      ),
      foreign: Default::default(),
      ext_workspaces: Default::default(),
      ext_toplevels: Default::default(),
      launch_templates: HashMap::new(),
      socket_name: socket_name.to_owned(),
      ipc_socket: None,
      wayfire_config_file: None,
      ipc_children: Vec::new(),
      xwayland: crate::xwayland::Status::default(),
      next_surface: 1,
      next_serial: 1,
      active_lock: None,
      activation: crate::activation::Activation::new(config.activation),
      data_devices: Default::default(),
      data_control: Default::default(),
      primary_selection: Default::default(),
      screencopy: Default::default(),
      security_context: crate::security_context::SecurityContexts::new(
        config.security_privileged_protocols,
      ),
      alpha_modifiers: Default::default(),
      tearing_controls: Default::default(),
      fifo: Default::default(),
      commit_timing: Default::default(),
      shortcut_inhibitors: crate::shortcuts::ShortcutInhibitors::new(
        config.shortcuts,
      ),
      pointer_protocols: Default::default(),
      pointer_gestures: Default::default(),
      presentation: Default::default(),
      virtual_pointers: Default::default(),
      virtual_keyboards: Default::default(),
      input: InputState::new(config.initial_locked_modifiers)
        .map_err(crate::Error::Keymap)?,
      text_inputs: Default::default(),
      input_methods: Default::default(),
      input_methods_v2: Default::default(),
      icons: Default::default(),
      enable_text_input_v3: config.enable_text_input_v3,
      enable_text_input_v1: config.enable_text_input_v1,
      tiling: config.tiling,
      protocol_plugins: config.protocol_plugins,
      wayfire_values: config.wayfire_values,
      focus_recency: HashMap::new(),
      view_properties: HashMap::new(),
      runtime_bindings: Default::default(),
      stipc_key_events: false,
      stipc_pointer_events: false,
      host_outputs: false,
      focus_sequence: 0,
      dispatch_pending: true,
      accept_pending: false,
      idle_pending: false,
      shutdown: false,
      show_desktop: false,
      show_desktop_windows: Default::default(),
      show_desktop_focus: None,
      all_dialogs_modal: config.all_dialogs_modal,
      keep_last_toplevel_activated: config.keep_last_toplevel_activated,
      max_buffer_size: config.max_buffer_size,
      disable_primary_selection: config.disable_primary_selection,
      decorations: HashMap::new(),
      dialogs: Default::default(),
      preferred_decoration_mode: crate::decoration::preferred_mode(
        &config.preferred_decoration_mode,
      ),
      force_preferred_decoration_mode: config.force_preferred_decoration_mode,
      decorations_enabled: config.decorations,
      decoration: config.decoration.clone(),
      frame_theme: crate::frame::FrameTheme::new(&config.decoration),
      frame_pointer: Default::default(),
      focus_main_surface_instead_of_popup: config
        .focus_main_surface_instead_of_popup,
      last_workspace: None,
      last_lock_state: LockState::Unlocked,
      last_dpms_state: DpmsState::On,
    };
    crate::output::relayout(&mut state);
    let first = state.primary();
    let (id, rect, identity) = (first.id.clone(), first.rect, first.identity());
    state.core.set_output_workarea(&id, rect)?;
    crate::output::claim_parked(&mut state, &id, identity);
    state.ipc_events.clear();
    Ok(state)
  }

  pub(crate) fn switch_workspace(
    &mut self,
    index: usize,
  ) -> Result<(), crate::Error> {
    let output = self.active_output.clone();
    self.switch_workspace_on(&output, index)
  }

  /// Shows workspace `index` (row by row from 0) on output `output`.
  pub(crate) fn switch_workspace_on(
    &mut self,
    output: &OutputId,
    index: usize,
  ) -> Result<(), crate::Error> {
    if index >= self.workspace_width as usize * self.workspace_height as usize {
      return Err(crate::Error::InvalidWorkspace(index));
    }
    let workspace = WorkspaceId::new(
      index as u32 % self.workspace_width,
      index as u32 / self.workspace_width,
    );
    self.core.ensure_workspace(output, workspace)?;
    let previous = self.core.current_workspace(output)?;
    if previous == workspace {
      // A request for the current workspace does nothing, as vswitch's
      // on_set_workspace_request (plugins/vswitch/vswitch.cpp) carries it
      // out without a change.
      return Ok(());
    }
    let active = *output == self.active_output;
    if active {
      shell::disable_show_desktop(self);
      self.last_workspace = Some(previous);
    }
    self.core.set_current_workspace(output, workspace)?;
    if active {
      self.input.keyboard_focus = None;
    }
    crate::input::sync_focus(self);
    shell::reconfigure_all(self);
    self.events.push(crate::Event::WorkspaceChanged {
      workspace: index as u32 + 1,
      previous: previous.y * self.workspace_width + previous.x + 1,
      output: crate::OutputHandle::from(output),
    });
    Ok(())
  }

  /// Resizes the grid, relocating out-of-range windows to the nearest valid
  /// cell.
  pub(crate) fn set_workspace_grid(
    &mut self,
    width: u32,
    height: u32,
  ) -> Result<(), crate::Error> {
    if width == 0 || height == 0 {
      return Err(crate::Error::InvalidWorkspaceGrid);
    }
    if width == self.workspace_width && height == self.workspace_height {
      return Ok(());
    }
    let current = self.core.current_workspace(&self.active_output)?;
    let target =
      WorkspaceId::new(current.x.min(width - 1), current.y.min(height - 1));
    self.workspace_width = width;
    self.workspace_height = height;
    self.last_workspace = self
      .last_workspace
      .filter(|workspace| workspace.x < width && workspace.y < height);
    let windows = self.core.window_ids().collect::<Vec<_>>();
    for window in windows {
      let location = self.core.window_location(window)?.clone();
      if location.output != self.active_output {
        continue;
      }
      if location.workspace.x >= width || location.workspace.y >= height {
        let workspace = WorkspaceId::new(
          location.workspace.x.min(width - 1),
          location.workspace.y.min(height - 1),
        );
        let index = (workspace.y * width + workspace.x) as usize;
        self.move_window_to_workspace(window, index)?;
      }
    }
    let current_index = (target.y * width + target.x) as usize;
    if current != target {
      self.switch_workspace(current_index)?;
    } else {
      crate::input::sync_focus(self);
      shell::reconfigure_all(self);
    }
    Ok(())
  }

  pub(crate) fn relative_workspace_index(
    &self,
    direction: shingle_core::Direction,
  ) -> Result<usize, crate::Error> {
    let current = self.core.current_workspace(&self.active_output)?;
    let (dx, dy) = match direction {
      shingle_core::Direction::Left => (-1, 0),
      shingle_core::Direction::Right => (1, 0),
      shingle_core::Direction::Up => (0, -1),
      shingle_core::Direction::Down => (0, 1),
    };
    let width = self.workspace_width;
    let height = self.workspace_height;
    let x = i64::from(current.x) + dx;
    let y = i64::from(current.y) + i64::from(dy);
    if x >= 0 && y >= 0 && x < i64::from(width) && y < i64::from(height) {
      return Ok((y as u32 * width + x as u32) as usize);
    }
    let target = match self.workspace_wraparound {
      crate::WorkspaceWraparound::Disabled => current,
      crate::WorkspaceWraparound::Grid => WorkspaceId::new(
        x.rem_euclid(i64::from(width)) as u32,
        y.rem_euclid(i64::from(height)) as u32,
      ),
      crate::WorkspaceWraparound::OnEnd if dx != 0 => {
        let index =
          current.y as i64 * i64::from(width) + i64::from(current.x) + dx;
        if index < 0 {
          WorkspaceId::new(width - 1, height - 1)
        } else if index >= i64::from(width) * i64::from(height) {
          WorkspaceId::ORIGIN
        } else {
          WorkspaceId::new(
            if dx > 0 { 0 } else { width - 1 },
            current.y.saturating_add_signed(dy),
          )
        }
      }
      crate::WorkspaceWraparound::OnEnd => current,
    };
    Ok((target.y * width + target.x) as usize)
  }

  pub(crate) fn move_focused_to_workspace(
    &mut self,
    index: usize,
  ) -> Result<Option<WindowId>, crate::Error> {
    let current = self.core.current_workspace(&self.active_output)?;
    let window = self.core.tree(&self.active_output, current)?.focused();
    if let Some(window) = window {
      self.move_window_to_workspace(window, index)?;
    }
    Ok(window)
  }

  /// Moves a window while preserving its persistent tiling state.
  pub(crate) fn move_window_to_workspace(
    &mut self,
    window: WindowId,
    index: usize,
  ) -> Result<(), crate::Error> {
    if index >= self.workspace_width as usize * self.workspace_height as usize {
      return Err(crate::Error::InvalidWorkspace(index));
    }
    let workspace = WorkspaceId::new(
      index as u32 % self.workspace_width,
      index as u32 / self.workspace_width,
    );
    // The window stays in its own set, on whichever output shows it.
    let location = self.core.window_location(window)?;
    let output = location.output;
    self.core.ensure_workspace(&output, workspace)?;
    let previous = location.workspace;
    if previous == workspace {
      return Ok(());
    }
    let from = previous.y * self.workspace_width + previous.x + 1;
    let detached = self.core.detach_window(window)?;
    self.core.reattach_window(detached, &output, workspace)?;
    self.input.keyboard_focus = None;
    crate::input::sync_focus(self);
    shell::reconfigure_all(self);
    self.events.push(crate::Event::WindowMoved {
      window,
      from,
      workspace: index as u32 + 1,
    });
    Ok(())
  }

  pub(crate) fn create_globals(&mut self, handle: &DisplayHandle) {
    self.display = Some(handle.clone());
    let plugins = self.protocol_plugins;
    if plugins.xdg_activation {
      crate::activation::create_global(handle);
    }
    crate::foreign::create_globals(handle);
    if plugins.foreign_toplevel {
      crate::foreign_toplevel::create_global(handle);
    }
    if plugins.ext_toplevel {
      crate::ext_toplevel::create_global(handle);
    }
    if plugins.ext_workspace {
      crate::ext_workspace::create_global(handle);
    }
    crate::xdg_output::create_global(handle);
    handle.create_global::<CompositorState, WlCompositor, ()>(6, ());
    handle.create_global::<CompositorState, WlShm, ()>(2, ());
    let ids = self
      .outputs
      .iter()
      .map(|output| output.id.clone())
      .collect::<Vec<_>>();
    for id in &ids {
      crate::output::create_global(self, id);
    }
    crate::output_power::create_global(handle);
    crate::output_management::create_global(handle);
    if plugins.security_context {
      crate::security_context::create_global(handle);
    }
    crate::alpha_modifier::create_global(handle);
    crate::tearing::create_global(handle);
    crate::fifo::create_global(handle);
    crate::commit_timing::create_global(handle);
    crate::gamma_control::create_global(handle);
    crate::input::create_globals(handle);
    // As in Wayfire's input-method-v1 plugin, each text-input version
    // has its own option, and the input methods serve either.
    if self.enable_text_input_v3 {
      crate::text_input::create_global(handle);
    }
    if self.enable_text_input_v1 {
      crate::text_input::v1::create_global(handle);
    }
    if self.enable_text_input_v3 || self.enable_text_input_v1 {
      crate::input_method::create_global(handle);
      crate::input_method_v2::create_global(handle);
      crate::input_panel::create_global(handle);
    }
    crate::data_device::create_global(handle);
    crate::data_control::create_global(handle);
    crate::cursor_shape::create_global(handle);
    crate::decoration::create_global(handle);
    crate::idle_notify::create_globals(handle);
    crate::pointer_protocols::create_globals(handle);
    crate::pointer_warp::create_global(handle);
    crate::pointer_gestures::create_global(handle);
    crate::presentation::create_global(handle);
    if !self.disable_primary_selection {
      crate::primary_selection::create_global(handle);
    }
    crate::screencopy::create_global(handle);
    if plugins.shortcuts_inhibit {
      crate::shortcuts::create_global(handle);
    }
    if plugins.wayfire_shell {
      crate::wayfire_shell::create_global(handle);
    }
    crate::single_pixel_buffer::create_global(handle);
    crate::toplevel_icon::create_global(handle);
    crate::subsurface::create_global(handle);
    crate::viewport::create_global(handle);
    crate::virtual_pointer::create_global(handle);
    crate::virtual_keyboard::create_global(handle);
    crate::fractional_scale::create_global(handle);
    shell::create_globals(handle, plugins.session_lock);
  }

  pub(crate) fn now(&self) -> Duration {
    self.started.elapsed()
  }

  /// The environment of every process Shingle starts. `DISPLAY` is
  /// Shingle's own X display while one is bound, and removed otherwise;
  /// the cursor variables follow the live cursor settings, so a reload or
  /// `wayfire/set-config-options` reaches every later child.
  pub(crate) fn client_environment(&self) -> ClientEnvironment {
    self.client_environment_inheriting(|name| std::env::var_os(name))
  }

  /// [`CompositorState::client_environment`] for a Shingle whose own
  /// environment `inherited` looks up.
  pub(crate) fn client_environment_inheriting(
    &self,
    inherited: impl Fn(&str) -> Option<std::ffi::OsString>,
  ) -> ClientEnvironment {
    let display = self.xwayland.display.as_deref().map(std::ffi::OsStr::new);
    let desktop = DesktopVariables {
      cursor_theme: &self.cursor_theme,
      cursor_size: self.cursor_size,
      wayfire_config_file: self.wayfire_config_file.as_deref(),
    };
    ClientEnvironment::new(
      &self.socket_name,
      display,
      self.ipc_socket.as_deref(),
    )
    .with_desktop(&desktop, inherited)
  }

  pub(crate) fn serial(&mut self) -> u32 {
    let serial = self.next_serial;
    self.next_serial = self.next_serial.wrapping_add(1);
    if self.next_serial == 0 {
      self.next_serial = 1;
    }
    serial
  }

  pub(crate) fn drain_events(&mut self) -> Vec<Event> {
    std::mem::take(&mut self.events)
  }

  pub(crate) fn surface(&self, id: SurfaceId) -> Option<SurfaceSnapshot> {
    let object = self.surface_ids.get(&id)?;
    self.snapshot(object)
  }

  /// Whether surface `object` is drawn on any output.
  pub(crate) fn visible_anywhere(&self, object: &ObjectId) -> bool {
    self.outputs.iter().any(|output| {
      self
        .visible_surfaces_on(&output.id)
        .iter()
        .any(|snapshot| self.surface_ids.get(&snapshot.id) == Some(object))
    })
  }

  pub(crate) fn visible_surfaces(&self) -> Vec<SurfaceSnapshot> {
    self.visible_surfaces_on(&self.primary().id)
  }

  /// The surfaces to draw on output `id`, bottom to top: its layer
  /// surfaces, the windows of its workspace set, the unmanaged X windows
  /// whose top-left corner is on it, and the cursor while the pointer is
  /// on it. While the session is locked, only its lock surface. A dark
  /// output shows nothing. Input-method panels are on the first output.
  pub(crate) fn visible_surfaces_on(
    &self,
    id: &OutputId,
  ) -> Vec<SurfaceSnapshot> {
    let Some(output) = self.outputs.get(id) else {
      return Vec::new();
    };
    let primary = self.primary().id == *id;
    if self.idle.dpms_state() == DpmsState::Off
      || output.dark
      || !output.powered
    {
      return Vec::new();
    }
    if self.idle.lock_state() != LockState::Unlocked {
      let mut surfaces = self
        .surfaces
        .values()
        .filter(|surface| {
          surface.buffer.is_some()
            && matches!(
              &surface.role,
              Role::SessionLock { output, .. } if output == id
            )
        })
        .collect::<Vec<_>>();
      surfaces.sort_by_key(|surface| surface.id);
      return surfaces
        .into_iter()
        .filter_map(|surface| self.snapshot(&surface.resource.id()))
        .collect();
    }
    let mut snapshots = Vec::new();
    self.append_layers(
      &mut snapshots,
      id,
      primary,
      &[Layer::Background, Layer::Bottom],
    );
    let Ok(workspace) = self.core.current_workspace(id) else {
      return snapshots;
    };
    let Ok(tree) = self.core.tree(id, workspace) else {
      return snapshots;
    };
    let fullscreen = self.promoted_fullscreen(id);
    let mut sticky = self
      .surfaces
      .values()
      .filter(|surface| {
        !self.show_desktop
          && surface.buffer.is_some()
          && surface
            .role
            .toplevel()
            .and_then(|toplevel| toplevel.window())
            .is_some_and(|window| {
              self.core.window_info(window).is_ok_and(|info| {
                info.state.sticky
                  && !info.state.minimized
                  && info.location.output == *id
                  && info.location.workspace != workspace
              })
            })
      })
      .collect::<Vec<_>>();
    sticky.sort_by_key(|surface| {
      surface
        .role
        .toplevel()
        .and_then(|toplevel| toplevel.window())
    });
    for mode in [
      shingle_core::StackingMode::AlwaysOnBottom,
      shingle_core::StackingMode::Normal,
      shingle_core::StackingMode::AlwaysOnTop,
    ] {
      for window in tree.stack_order() {
        if self.show_desktop
          || tree
            .leaf_state(*window)
            .is_ok_and(|state| state.minimized || state.stacking != mode)
        {
          continue;
        }
        let object = self.surfaces.iter().find_map(|(object, surface)| {
          let matches = surface.role.toplevel().is_some_and(|toplevel| {
            toplevel.window() == Some(*window) && surface.buffer.is_some()
          });
          matches.then_some(object.clone())
        });
        if let Some(object) = object
          && fullscreen.as_ref() != Some(&object)
        {
          self.append_surface_and_popups(&object, &mut snapshots);
        }
      }
      for surface in &sticky {
        let matches_mode = match surface.role.toplevel() {
          Some(toplevel) => toplevel.window().is_some_and(|window| {
            self
              .core
              .window_info(window)
              .is_ok_and(|info| info.state.stacking == mode)
          }),
          _ => false,
        };
        if matches_mode && fullscreen.as_ref() != Some(&surface.resource.id()) {
          self
            .append_surface_and_popups(&surface.resource.id(), &mut snapshots);
        }
      }
    }
    if self.remove_output_limits {
      for object in self.foreign_windows_on(id) {
        if fullscreen.as_ref() != Some(&object) {
          self.append_surface_and_popups(&object, &mut snapshots);
        }
      }
    }
    // Unmanaged X windows (menus, tooltips) are above Top and below
    // Overlay, where Wayfire's UNMANAGED layer is, and above a fullscreen
    // window, whose menus they may be.
    if let Some(object) = fullscreen {
      self.append_surface_and_popups(&object, &mut snapshots);
    } else {
      self.append_layers(&mut snapshots, id, primary, &[Layer::Top]);
    }
    for object in crate::xwayland::unmanaged::drawn(self) {
      if let Some(snapshot) = self.snapshot(&object)
        && crate::surface_outputs::unmanaged_output(self, snapshot.geometry)
          == *id
      {
        snapshots.push(snapshot);
      }
    }
    self.append_layers(&mut snapshots, id, primary, &[Layer::Overlay]);
    // X drag icons are above everything but the cursor, where Wayfire
    // puts them (wayfire_dnd_xwayland_view).
    for object in crate::xwayland::unmanaged::drag_icons(self) {
      if let Some(snapshot) = self.snapshot(&object)
        && crate::surface_outputs::unmanaged_output(self, snapshot.geometry)
          == *id
      {
        snapshots.push(snapshot);
      }
    }
    if crate::surface_outputs::pointer_output(self) != *id {
      return snapshots;
    }
    snapshots.extend(self.cursor_snapshot());
    snapshots
  }

  /// The cursor [`CompositorState::visible_surfaces_on`] draws last on
  /// output `id`: none while the pointer is on another output, the output
  /// is dark or off, or the session is locked.
  pub(crate) fn cursor_on(&self, id: &OutputId) -> Option<SurfaceSnapshot> {
    let output = self.outputs.get(id)?;
    if self.idle.dpms_state() == DpmsState::Off
      || output.dark
      || !output.powered
      || self.idle.lock_state() != LockState::Unlocked
      || crate::surface_outputs::pointer_output(self) != *id
    {
      return None;
    }
    self.cursor_snapshot()
  }

  /// The cursor at the pointer: the default arrow where the compositor
  /// draws one, else the client's cursor surface, else the shape the
  /// client chose.
  fn cursor_snapshot(&self) -> Option<SurfaceSnapshot> {
    if let Some(snapshot) = crate::cursor_shape::default_snapshot(self) {
      return Some(snapshot);
    }
    if let Some(object) = &self.input.cursor_surface
      && self
        .surfaces
        .get(object)
        .is_some_and(|surface| surface.buffer.is_some())
      && let Some(snapshot) = self.snapshot(object)
    {
      return Some(snapshot);
    }
    crate::cursor_shape::snapshot(self)
  }

  /// The surface of the fullscreen window output `id` draws above its top
  /// layer, if any: the window focused there, the keyboard's if it is on
  /// this output and otherwise the focused tile of its current workspace,
  /// when it is fullscreen, not minimized and has a buffer. Wayfire calls
  /// this promoting the view (src/output/promotion-manager.hpp).
  pub(crate) fn promoted_fullscreen(&self, id: &OutputId) -> Option<ObjectId> {
    if self.show_desktop {
      return None;
    }
    let workspace = self.core.current_workspace(id).ok()?;
    let tree = self.core.tree(id, workspace).ok()?;
    let focused = crate::input::focused_window(self)
      .filter(|window| {
        self
          .core
          .window_info(*window)
          .is_ok_and(|info| info.location.output == *id)
      })
      .or_else(|| tree.focused())?;
    let fullscreen = self
      .core
      .window_info(focused)
      .is_ok_and(|info| info.state.fullscreen && !info.state.minimized);
    if !fullscreen {
      return None;
    }
    self.surfaces.iter().find_map(|(object, surface)| {
      surface
        .role
        .toplevel()
        .is_some_and(|toplevel| {
          toplevel.window() == Some(focused) && surface.buffer.is_some()
        })
        .then(|| object.clone())
    })
  }

  /// The surfaces of `window`'s toplevel, its subsurfaces and popups,
  /// bottom to top; empty without a mapped buffer.
  pub(crate) fn window_surfaces(
    &self,
    window: shingle_core::WindowId,
  ) -> Vec<SurfaceSnapshot> {
    let object = self.surfaces.iter().find_map(|(object, surface)| {
      surface
        .role
        .toplevel()
        .is_some_and(|toplevel| {
          toplevel.window() == Some(window) && surface.buffer.is_some()
        })
        .then(|| object.clone())
    });
    let mut snapshots = Vec::new();
    if let Some(object) = object {
      self.append_surface_and_popups(&object, &mut snapshots);
    }
    snapshots
  }

  /// The toplevels that other outputs show and that overlap output `id`,
  /// bottom to top and in the order outputs were added, which
  /// `remove_output_limits` draws on `id` too.
  pub(crate) fn foreign_windows_on(&self, id: &OutputId) -> Vec<ObjectId> {
    let Some(rect) = self.outputs.get(id).map(|output| output.rect) else {
      return Vec::new();
    };
    let mut objects = Vec::new();
    for other in self.outputs.iter().filter(|other| other.id != *id) {
      let Ok(workspace) = self.core.current_workspace(&other.id) else {
        continue;
      };
      let Ok(tree) = self.core.tree(&other.id, workspace) else {
        continue;
      };
      for window in tree.stack_order() {
        let object = self.surfaces.iter().find_map(|(object, surface)| {
          surface
            .role
            .toplevel()
            .is_some_and(|toplevel| toplevel.window() == Some(*window))
            .then(|| object.clone())
        });
        let Some(object) = object else {
          continue;
        };
        let shown = crate::surface_outputs::toplevel_output(self, &object)
          .is_some_and(|shown| shown == other.id);
        let overlaps =
          shell::surface_geometry(self, &object).is_some_and(|geometry| {
            crate::surface_outputs::intersection(geometry, rect).is_some()
          });
        if shown && overlaps {
          objects.push(object);
        }
      }
    }
    objects
  }

  /// Appends the mapped layer surfaces of `layers` on output `id`, and
  /// with `primary` the visible input-method panels among the overlays.
  /// The layer-shell surfaces of output `id` with their popups, bottom
  /// to top: of the background and bottom layers, or with `above` of the
  /// top and overlay layers and the input panel. Nothing on an output
  /// `visible_surfaces_on` draws nothing on, or while the session is
  /// locked.
  pub(crate) fn layer_surfaces_on(
    &self,
    id: &OutputId,
    above: bool,
  ) -> Vec<SurfaceSnapshot> {
    let Some(output) = self.outputs.get(id) else {
      return Vec::new();
    };
    if self.idle.dpms_state() == DpmsState::Off
      || output.dark
      || !output.powered
      || self.idle.lock_state() != LockState::Unlocked
    {
      return Vec::new();
    }
    let primary = self.primary().id == *id;
    let mut snapshots = Vec::new();
    let layers: &[&[Layer]] = if above {
      &[&[Layer::Top], &[Layer::Overlay]]
    } else {
      &[&[Layer::Background, Layer::Bottom]]
    };
    for layers in layers {
      self.append_layers(&mut snapshots, id, primary, layers);
    }
    snapshots
  }

  /// The windows of the current workspace of output `id`'s workspace set,
  /// bottom to top.
  pub(crate) fn stacking_order_on(
    &self,
    id: &OutputId,
  ) -> Vec<shingle_core::WindowId> {
    self
      .core
      .current_workspace(id)
      .and_then(|workspace| self.core.tree(id, workspace))
      .map(|tree| tree.stack_order().to_vec())
      .unwrap_or_default()
  }

  fn append_layers(
    &self,
    snapshots: &mut Vec<SurfaceSnapshot>,
    id: &OutputId,
    primary: bool,
    layers: &[Layer],
  ) {
    let mut surfaces = self
      .surfaces
      .values()
      .filter(|surface| {
        surface.buffer.is_some()
          && match &surface.role {
            Role::Layer(layer) => {
              layer.output == *id && layers.contains(&layer.current.layer)
            }
            Role::InputPanel(panel) => {
              primary && panel.visible && layers.contains(&Layer::Overlay)
            }
            _ => false,
          }
      })
      .collect::<Vec<_>>();
    surfaces.sort_by_key(|surface| surface.id);
    for surface in surfaces {
      self.append_surface_and_popups(&surface.resource.id(), snapshots);
    }
  }

  fn append_surface_and_popups(
    &self,
    object: &ObjectId,
    snapshots: &mut Vec<SurfaceSnapshot>,
  ) {
    let unmapped = self.surfaces.get(object).is_some_and(|surface| {
      surface.role.toplevel().is_some_and(|toplevel| {
        toplevel.window().is_some_and(|window| {
          self.core.window_info(window).is_ok_and(|info| {
            info.presence != shingle_core::WindowPresence::Mapped
          })
        })
      })
    });
    if unmapped {
      return;
    }
    if let Some(snapshot) = self.snapshot(object) {
      snapshots.push(snapshot);
    }
    if let Some(children) = self.subsurface_order.get(object) {
      for child in children {
        if self
          .surfaces
          .get(child)
          .is_some_and(|surface| surface.buffer.is_some())
        {
          self.append_surface_and_popups(child, snapshots);
        }
      }
    }
    let mut children = self
      .surfaces
      .iter()
      .filter(|(_, surface)| {
        surface.buffer.is_some()
          && surface
            .role
            .popup()
            .is_some_and(|popup| &popup.parent == object)
      })
      .map(|(object, surface)| (surface.id, object.clone()))
      .collect::<Vec<_>>();
    children.sort_by_key(|(id, _)| *id);
    for (_, child) in children {
      self.append_surface_and_popups(&child, snapshots);
    }
  }

  pub(crate) fn scale_surfaces(
    &self,
    all_workspaces: bool,
    include_minimized: bool,
  ) -> Vec<SurfaceSnapshot> {
    self.scale_surfaces_on(
      &self.active_output,
      all_workspaces,
      include_minimized,
    )
  }

  /// [`CompositorState::scale_surfaces`] for the windows of output `id`'s
  /// workspace set.
  pub(crate) fn scale_surfaces_on(
    &self,
    id: &OutputId,
    all_workspaces: bool,
    include_minimized: bool,
  ) -> Vec<SurfaceSnapshot> {
    let Ok(current_workspace) = self.core.current_workspace(id) else {
      return Vec::new();
    };
    let mut snapshots = self
      .surfaces
      .iter()
      .filter_map(|(object, surface)| {
        surface.buffer.as_ref()?;
        let toplevel = surface.role.toplevel()?;
        let window = toplevel.window()?;
        let info = self.core.window_info(window).ok()?;
        if info.presence != shingle_core::WindowPresence::Mapped
          || info.location.output != *id
          || (info.state.minimized && !include_minimized)
          || (!all_workspaces
            && !info.state.sticky
            && info.location.workspace != current_workspace)
        {
          return None;
        }
        self.snapshot(object)
      })
      .collect::<Vec<_>>();
    snapshots.sort_by_key(|snapshot| {
      (
        std::cmp::Reverse(
          snapshot
            .window
            .and_then(|window| self.focus_recency.get(&window).copied())
            .unwrap_or(0),
        ),
        snapshot.id,
      )
    });
    snapshots
  }

  pub(crate) fn snapshot(&self, object: &ObjectId) -> Option<SurfaceSnapshot> {
    let surface = self.surfaces.get(object)?;
    let (role, window, parent, app_id, title) =
      shell::snapshot_metadata(self, surface);
    let geometry = shell::surface_geometry(self, object)?;
    Some(SurfaceSnapshot {
      id: surface.id,
      window,
      parent,
      role,
      app_id,
      title,
      icon: crate::toplevel_icon::for_surface(self, object),
      geometry,
      opacity: shell::opacity(self, object),
      buffer: surface.buffer.clone(),
    })
  }

  /// The first output added that is still there, which hosts draw until
  /// the frontend draws every output (MO-5).
  pub(crate) fn primary(&self) -> &crate::output::OutputState {
    self.outputs.first()
  }

  /// The active output.
  pub(crate) fn active(&self) -> &crate::output::OutputState {
    self
      .outputs
      .get(&self.active_output)
      .unwrap_or_else(|| self.outputs.first())
  }

  /// The output that shows `window`'s workspace set, else the active one.
  pub(crate) fn output_of_window(
    &self,
    window: WindowId,
  ) -> &crate::output::OutputState {
    self
      .core
      .window_info(window)
      .ok()
      .and_then(|info| self.outputs.get(&info.location.output))
      .unwrap_or_else(|| self.active())
  }

  /// Changes the transform of output `id`. Switching between upright and
  /// quarter-turn transforms swaps its logical width and height.
  pub(crate) fn set_transform_of(
    &mut self,
    id: &OutputId,
    transform: wl_output::Transform,
  ) {
    let Some(output) = self.outputs.get_mut(id) else {
      return;
    };
    let old = output.transform;
    if old == transform {
      return;
    }
    output.transform = transform;
    if crate::output_transform::is_quarter_turn(old)
      != crate::output_transform::is_quarter_turn(transform)
    {
      std::mem::swap(&mut output.rect.width, &mut output.rect.height);
    }
    let moved = crate::output::relayout(self);
    let output = self.outputs.get(id).expect("the output exists");
    output.send_all();
    let rect = output.rect;
    crate::layer_shell::update_workarea(self);
    shell::reconfigure_lock_surfaces(self);
    shell::reconfigure_all(self);
    crate::xdg_output::update(self);
    crate::output_management::sync(self);
    self.events.push(Event::OutputChanged {
      output: crate::OutputHandle::from(id),
      rect,
    });
    for other in moved.iter().filter(|other| *other != id) {
      crate::output::moved(self, other);
    }
  }

  /// Describes output `id` to clients again.
  pub(crate) fn set_info_of(&mut self, id: &OutputId, info: crate::OutputInfo) {
    let Some(output) = self.outputs.get_mut(id) else {
      return;
    };
    output.info = info;
    output.send_all();
    crate::xdg_output::update(self);
    crate::output_management::sync(self);
  }

  /// Changes the scale of output `id`, keeping its logical size.
  pub(crate) fn set_scale_of(
    &mut self,
    id: &OutputId,
    scale: f64,
  ) -> Result<(), crate::Error> {
    if !scale.is_finite() || scale <= 0.0 {
      return Err(crate::Error::InvalidOutputScale);
    }
    let Some(output) = self.outputs.get_mut(id) else {
      return Ok(());
    };
    output.scale = scale;
    output.send_all();
    let rect = output.rect;
    crate::output_management::sync(self);
    self.events.push(Event::OutputChanged {
      output: crate::OutputHandle::from(id),
      rect,
    });
    Ok(())
  }

  /// Gives output `id` the logical size of `rect`. With `fix`, a position
  /// other than the current one fixes the output there; otherwise the
  /// layout keeps placing it as configured.
  pub(crate) fn set_rect_of(
    &mut self,
    id: &OutputId,
    rect: Rect,
    fix: bool,
  ) -> Result<(), crate::Error> {
    rect.validate()?;
    let Some(output) = self.outputs.get_mut(id) else {
      return Ok(());
    };
    if fix && (rect.x, rect.y) != (output.rect.x, output.rect.y) {
      output.position = shingle_config::OutputPosition::Fixed {
        x: rect.x,
        y: rect.y,
      };
    }
    output.rect = rect;
    let moved = crate::output::relayout(self);
    let output = self.outputs.get(id).expect("the output exists");
    output.send_all();
    let rect = output.rect;
    crate::layer_shell::update_workarea(self);
    shell::reconfigure_lock_surfaces(self);
    crate::xdg_output::update(self);
    crate::output_management::sync(self);
    self.events.push(Event::OutputChanged {
      output: crate::OutputHandle::from(id),
      rect,
    });
    for other in moved.iter().filter(|other| *other != id) {
      crate::output::moved(self, other);
    }
    Ok(())
  }

  /// [`CompositorState::set_transform_of`] for the first output.
  pub(crate) fn set_output_transform(
    &mut self,
    transform: wl_output::Transform,
  ) {
    let id = self.primary().id.clone();
    self.set_transform_of(&id, transform);
  }

  /// [`CompositorState::set_info_of`] for the first output.
  #[cfg(test)]
  pub(crate) fn set_output_info(&mut self, info: crate::OutputInfo) {
    let id = self.primary().id.clone();
    self.set_info_of(&id, info);
  }

  /// [`CompositorState::set_scale_of`] for the first output.
  pub(crate) fn set_output_scale(
    &mut self,
    scale: f64,
  ) -> Result<(), crate::Error> {
    let id = self.primary().id.clone();
    self.set_scale_of(&id, scale)
  }

  /// [`CompositorState::set_rect_of`] for the first output, fixing it at a
  /// new position.
  pub(crate) fn set_output(
    &mut self,
    output: Rect,
  ) -> Result<(), crate::Error> {
    let id = self.primary().id.clone();
    self.set_rect_of(&id, output, true)
  }

  pub(crate) fn poll_idle(&mut self) {
    if !self.idle_pending {
      return;
    }
    self.idle_pending = false;
    let commands = self.idle.poll(self.now());
    self.process_idle_commands(commands);
  }

  pub(crate) fn reap_ipc_children(&mut self) {
    self
      .ipc_children
      .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
  }

  pub(crate) fn process_idle_commands(&mut self, commands: Vec<Command>) {
    for command in commands {
      match command {
        Command::StartLockClient => self.events.push(Event::StartLockClient),
        // Every DPMS command changes `dpms_state`, which emit_idle_changes
        // below reports as the one Event::DpmsChanged.
        Command::RequestDpmsOff | Command::RequestDpmsOn => {}
        Command::NotifyIdle => self.events.push(Event::Idle),
        Command::NotifyResume => self.events.push(Event::Resume),
      }
    }
    self.emit_idle_changes();
  }

  pub(crate) fn emit_idle_changes(&mut self) {
    let lock = self.idle.lock_state();
    if self.last_lock_state != lock {
      self.last_lock_state = lock;
      self.events.push(Event::LockStateChanged(lock));
    }
    let dpms = self.idle.dpms_state();
    if self.last_dpms_state != dpms {
      self.last_dpms_state = dpms;
      self.events.push(Event::DpmsChanged(dpms));
      crate::output_power::sync(self);
    }
  }

  pub(crate) fn unlock_session(&mut self) {
    self.active_lock = None;
    let commands = self.idle.unlock(self.now());
    self.process_idle_commands(commands);
    self.emit_idle_changes();
    crate::input::sync_focus(self);
  }

  pub(crate) fn cleanup_disconnected(&mut self) {
    let clients = self
      .disconnected
      .lock()
      .map(|mut clients| std::mem::take(&mut *clients))
      .unwrap_or_default();
    if clients.is_empty() {
      return;
    }
    self
      .launch_templates
      .retain(|client, _| !clients.contains(client));
    crate::data_control::cleanup_clients(self, &clients);
    self.data_devices.cleanup_clients(&clients);
    self.primary_selection.cleanup_clients(&clients);
    for client in &clients {
      crate::presentation::cleanup_client(self, client);
      crate::output_management::cleanup_client(self, client);
      crate::gamma_control::cleanup_client(self, client);
    }
    let objects = self
      .surfaces
      .iter()
      .filter(|(_, surface)| clients.contains(&surface.client))
      .map(|(object, _)| object.clone())
      .collect::<Vec<_>>();
    for object in objects {
      self.destroy_surface(&object);
    }
    if let Some(lock) = self.active_lock.clone()
      && lock
        .client()
        .is_none_or(|client| clients.contains(&client.id()))
    {
      self.idle.lock_client_failed(self.now());
      self.active_lock = None;
      self.emit_idle_changes();
    }
  }

  pub(crate) fn destroy_surface(&mut self, object: &ObjectId) {
    let Some(surface) = self.surfaces.remove(object) else {
      return;
    };
    self.surface_ids.remove(&surface.id);
    crate::commit_queue::surface_destroyed(self, object);
    crate::linux_drm_syncobj::surface_destroyed(self, object);
    crate::fifo::surface_destroyed(self, object);
    crate::commit_timing::surface_destroyed(self, object);
    crate::presentation::surface_destroyed(self, surface.id);
    crate::pointer_protocols::surface_destroyed(self, object);
    self.viewports.remove(object);
    self.fractional_scales.remove(object);
    self.subsurface_order.remove(object);
    for children in self.subsurface_order.values_mut() {
      children.retain(|child| child != object);
    }
    if self.input.cursor_surface.as_ref() == Some(object) {
      self.input.cursor_surface = None;
    }
    shell::destroy_role(self, &surface);
    if matches!(surface.role, Role::Layer(_)) {
      crate::layer_shell::update_workarea(self);
    }
    self.events.push(Event::SurfaceDestroyed {
      surface: surface.id,
    });
    crate::input::sync_focus(self);
  }

  fn create_surface(
    &mut self,
    resource: New<WlSurface>,
    client: &Client,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let id = SurfaceId(self.next_surface);
    self.next_surface = self.next_surface.checked_add(1).unwrap_or(1);
    let resource = data_init.init(resource, SurfaceData);
    let object = resource.id();
    self.surface_ids.insert(id, object.clone());
    self.surfaces.insert(
      object,
      Surface {
        id,
        client: client.id(),
        resource,
        pending: PendingSurface::default(),
        cached_commit: None,
        buffer: None,
        role: Role::None,
        committed_once: false,
        alpha: u32::MAX,
        tearing: false,
        input_region: None,
      },
    );
  }

  /// `wl_surface.commit`, which goes through the surface's commit queue
  /// first.
  fn commit(&mut self, object: &ObjectId) {
    crate::commit_queue::commit(self, object);
  }

  /// Applies a synchronized subsurface's cached commit, when its parent
  /// commits or it becomes desynchronized.
  pub(crate) fn commit_cached(&mut self, object: &ObjectId) {
    let Some(cached) = self
      .surfaces
      .get_mut(object)
      .and_then(|surface| surface.cached_commit.take())
    else {
      return;
    };
    self.commit_state(object, cached, true);
  }

  /// Takes the surface's pending state for a later commit. Buffer scale
  /// and transform stay pending too, since they persist until changed.
  pub(crate) fn take_pending(
    &mut self,
    object: &ObjectId,
  ) -> Option<PendingSurface> {
    let surface = self.surfaces.get_mut(object)?;
    let pending = std::mem::take(&mut surface.pending);
    surface.pending.scale = pending.scale;
    surface.pending.transform = pending.transform;
    Some(pending)
  }

  /// Commits `pending`, state taken earlier with
  /// [`CompositorState::take_pending`], as if the client committed it now.
  /// What the client has set since stays pending for its next commit.
  /// `cached` skips the synchronized-subsurface cache, for cached state.
  pub(crate) fn commit_state(
    &mut self,
    object: &ObjectId,
    pending: PendingSurface,
    cached: bool,
  ) {
    let Some(surface) = self.surfaces.get_mut(object) else {
      return;
    };
    let current = std::mem::replace(&mut surface.pending, pending);
    self.commit_inner(object, cached);
    if let Some(surface) = self.surfaces.get_mut(object) {
      // What the commit left unused, such as a buffer it refused, drops
      // here with its lock.
      surface.pending = current;
    }
  }

  /// Applies the surface's pending state, unless a synchronized parent
  /// (without `apply_cached`), FIFO or commit timing defers it.
  pub(crate) fn commit_inner(&mut self, object: &ObjectId, apply_cached: bool) {
    let synchronized = self.surfaces.get(object).is_some_and(|surface| {
      matches!(
        &surface.role,
        Role::Subsurface {
          synchronized: true,
          ..
        }
      )
    });
    if synchronized && !apply_cached {
      let Some(mut pending) = self.take_pending(object) else {
        return;
      };
      let Some(surface) = self.surfaces.get_mut(object) else {
        return;
      };
      if let Some(cached) = &mut surface.cached_commit {
        merge_pending(cached, &mut pending);
      } else {
        surface.cached_commit = Some(pending);
      }
      return;
    }
    // FIFO and commit-timing waits happen in the commit queue, before this.
    let sets_barrier = self.surfaces.get_mut(object).is_some_and(|surface| {
      surface.pending.fifo_wait_barrier = false;
      surface.pending.timestamp = None;
      std::mem::take(&mut surface.pending.fifo_set_barrier)
    });
    crate::fifo::applied(self, object, sets_barrier);
    crate::region::commit_input(self, object);
    let viewport = crate::viewport::pending(self, object).unwrap_or_default();
    let Some(surface) = self.surfaces.get_mut(object) else {
      return;
    };
    if let Err(()) = shell::validate_commit(surface) {
      return;
    }
    if let Some(alpha) = surface.pending.alpha.take() {
      surface.alpha = alpha;
    }
    if let Some(tearing) = surface.pending.tearing.take() {
      surface.tearing = tearing;
    }
    let scale = surface.pending.scale;
    // Cursor images keep their pixels, which a native backend copies into
    // its cursor plane's buffer whenever the cursor is shown.
    let cursor = matches!(surface.role, Role::Cursor);
    let pending_buffer =
      std::mem::replace(&mut surface.pending.buffer, PendingBuffer::Unchanged);
    // The lock travels with the attachment until its pixels are copied.
    let attachment = match pending_buffer {
      PendingBuffer::Unchanged => None,
      PendingBuffer::Attach(attachment, lock) => Some(Some((attachment, lock))),
      PendingBuffer::Detach => Some(None),
    };
    let surface_damage = std::mem::take(&mut surface.pending.damage);
    let mut buffer_damage = std::mem::take(&mut surface.pending.buffer_damage);
    if let Some(Some((attachment, lock))) = &attachment {
      if attachment.width % scale as u32 != 0
        || attachment.height % scale as u32 != 0
      {
        surface.resource.post_error(
          wl_surface::Error::InvalidSize,
          "buffer dimensions must be multiples of the buffer scale",
        );
        return;
      }
      if surface_damage.is_empty() && buffer_damage.is_empty() {
        buffer_damage.push(Rect::new(
          0,
          0,
          attachment.width,
          attachment.height,
        ));
      }
      let mut damage = surface_damage_to_buffer(
        surface_damage,
        &DamageSpace {
          width: attachment.width,
          height: attachment.height,
          scale,
          transform: surface.pending.transform,
          viewport,
        },
      );
      damage.extend(clip_damage(
        buffer_damage,
        attachment.width,
        attachment.height,
      ));
      let offset_x = surface.pending.offset_x;
      let offset_y = surface.pending.offset_y;
      let upload = self.shm_upload && !cursor;
      match attachment.snapshot(damage, offset_x, offset_y, Some(lock), upload)
      {
        Ok(mut snapshot) => {
          snapshot.scale = scale;
          snapshot.transform = surface.pending.transform;
          if let Some(surface) = self.surfaces.get_mut(object) {
            surface.buffer = Some(snapshot);
          }
        }
        Err(message) => {
          surface
            .resource
            .post_error(wl_surface::Error::NoBuffer, message);
          return;
        }
      }
    } else if matches!(attachment, Some(None)) {
      if let Some(surface) = self.surfaces.get_mut(object) {
        surface.buffer = None;
      }
    } else if let Some(surface) = self.surfaces.get(object)
      && let Some(buffer) = &surface.buffer
    {
      let mut damage = surface_damage_to_buffer(
        surface_damage,
        &DamageSpace {
          width: buffer.width,
          height: buffer.height,
          scale,
          transform: surface.pending.transform,
          viewport,
        },
      );
      damage.extend(clip_damage(buffer_damage, buffer.width, buffer.height));
      if let Some(surface) = self.surfaces.get_mut(object)
        && let Some(buffer) = &mut surface.buffer
      {
        buffer.damage = damage;
        buffer.transform = surface.pending.transform;
      }
    }
    // The pixels are copied, so the buffer goes back to its client, unless
    // a DMA-BUF's snapshot or an shm buffer waiting for upload holds it.
    drop(attachment);

    if let Some(viewport) = crate::viewport::commit(self, object, scale)
      && let Some(surface) = self.surfaces.get_mut(object)
      && let Some(buffer) = &mut surface.buffer
    {
      crate::viewport::apply(buffer, viewport, scale);
    }

    let callbacks = if let Some(surface) = self.surfaces.get_mut(object) {
      surface.pending.offset_x = 0;
      surface.pending.offset_y = 0;
      surface.committed_once = true;
      let id = surface.id;
      std::mem::take(&mut surface.pending.callbacks)
        .into_iter()
        .map(|resource| FrameCallback {
          surface: id,
          resource,
        })
        .collect::<Vec<_>>()
    } else {
      Vec::new()
    };
    self.frame_callbacks.extend(callbacks);
    crate::xwayland::windows::committed(self, object);
    shell::committed(self, object);
    crate::toplevel_icon::commit(self, object);
    crate::text_input::surface_commit(self, object);
    crate::pointer_protocols::commit(self, object);
    crate::subsurface::commit_children(self, object);
    if let Some(snapshot) = self.snapshot(object) {
      self
        .events
        .push(Event::SurfaceCommitted(Box::new(snapshot)));
    }
  }
}

pub(crate) fn merge_pending(
  cached: &mut PendingSurface,
  pending: &mut PendingSurface,
) {
  if !matches!(pending.buffer, PendingBuffer::Unchanged) {
    cached.buffer =
      std::mem::replace(&mut pending.buffer, PendingBuffer::Unchanged);
  }
  cached.damage.append(&mut pending.damage);
  cached.buffer_damage.append(&mut pending.buffer_damage);
  cached.callbacks.append(&mut pending.callbacks);
  cached.scale = pending.scale;
  cached.transform = pending.transform;
  cached.offset_x = pending.offset_x;
  cached.offset_y = pending.offset_y;
  if pending.alpha.is_some() {
    cached.alpha = pending.alpha.take();
  }
  if pending.tearing.is_some() {
    cached.tearing = pending.tearing.take();
  }
  if pending.input_region.is_some() {
    cached.input_region = pending.input_region.take();
  }
  cached.fifo_set_barrier |= std::mem::take(&mut pending.fifo_set_barrier);
  cached.fifo_wait_barrier |= std::mem::take(&mut pending.fifo_wait_barrier);
  if pending.timestamp.is_some() {
    cached.timestamp = pending.timestamp.take();
  }
  if pending.acquire.is_some() {
    cached.acquire = pending.acquire.take();
  }
}

/// What surface damage is converted into: a `width` by `height` buffer at
/// `scale`, turned by `transform` and shown through `viewport`.
struct DamageSpace {
  width: u32,
  height: u32,
  scale: i32,
  transform: wl_output::Transform,
  viewport: crate::viewport::ViewportState,
}

/// Converts `wl_surface.damage` rectangles from surface coordinates to
/// buffer pixels, clipped to the buffer, in the order wlroots'
/// `surface_update_damage` (types/wlr_compositor.c) uses: undo the
/// viewport's scaling to its destination, move by the source origin,
/// multiply by the buffer scale, then undo the buffer transform. Partly
/// covered pixels count as damaged.
fn surface_damage_to_buffer(
  damage: Vec<Rect>,
  space: &DamageSpace,
) -> Vec<Rect> {
  let scale = f64::from(space.scale.max(1));
  let quarter_turn = matches!(
    space.transform,
    wl_output::Transform::_90
      | wl_output::Transform::_270
      | wl_output::Transform::Flipped90
      | wl_output::Transform::Flipped270
  );
  // The buffer as the surface sees it, in buffer pixels.
  let (width, height) = if quarter_turn {
    (f64::from(space.height), f64::from(space.width))
  } else {
    (f64::from(space.width), f64::from(space.height))
  };
  let source = space.viewport.source;
  let (source_width, source_height) = source
    .map_or((width / scale, height / scale), |source| {
      (source[2], source[3])
    });
  let (ratio_x, ratio_y) =
    space.viewport.destination.map_or((1.0, 1.0), |(x, y)| {
      (
        source_width / f64::from(x.max(1)),
        source_height / f64::from(y.max(1)),
      )
    });
  let (origin_x, origin_y) =
    source.map_or((0.0, 0.0), |source| (source[0], source[1]));
  let rects = damage.into_iter().map(|rect| {
    let x0 = (f64::from(rect.x) * ratio_x + origin_x) * scale;
    let y0 = (f64::from(rect.y) * ratio_y + origin_y) * scale;
    let x1 = x0 + f64::from(rect.width) * ratio_x * scale;
    let y1 = y0 + f64::from(rect.height) * ratio_y * scale;
    let (a, b) = (
      untransform(space.transform, width, height, x0, y0),
      untransform(space.transform, width, height, x1, y1),
    );
    let (x0, x1) = (a.0.min(b.0).floor(), a.0.max(b.0).ceil());
    let (y0, y1) = (a.1.min(b.1).floor(), a.1.max(b.1).ceil());
    let bound = |value: f64| value.clamp(i64::MIN as f64, i64::MAX as f64);
    [bound(x0), bound(y0), bound(x1), bound(y1)].map(|value| value as i64)
  });
  clip_rects(rects, space.width, space.height)
}

/// Maps a point of the buffer as the surface sees it, `width` by
/// `height`, to the buffer itself, whose content `transform` turned:
/// `_90` is a quarter turn counter-clockwise, and the flipped variants
/// mirror horizontally first.
fn untransform(
  transform: wl_output::Transform,
  width: f64,
  height: f64,
  x: f64,
  y: f64,
) -> (f64, f64) {
  match transform {
    wl_output::Transform::_90 => (y, width - x),
    wl_output::Transform::_180 => (width - x, height - y),
    wl_output::Transform::_270 => (height - y, x),
    wl_output::Transform::Flipped => (width - x, y),
    wl_output::Transform::Flipped90 => (y, x),
    wl_output::Transform::Flipped180 => (x, height - y),
    wl_output::Transform::Flipped270 => (height - y, width - x),
    _ => (x, y),
  }
}

/// Clips `wl_surface.damage_buffer` rectangles to the buffer.
fn clip_damage(damage: Vec<Rect>, width: u32, height: u32) -> Vec<Rect> {
  let rects = damage.into_iter().map(|rect| {
    let x0 = i64::from(rect.x);
    let y0 = i64::from(rect.y);
    [
      x0,
      y0,
      x0.saturating_add(i64::from(rect.width)),
      y0.saturating_add(i64::from(rect.height)),
    ]
  });
  clip_rects(rects, width, height)
}

/// Clips `[x0, y0, x1, y1]` boxes to a `width` by `height` buffer and drops
/// the empty ones.
fn clip_rects(
  rects: impl Iterator<Item = [i64; 4]>,
  width: u32,
  height: u32,
) -> Vec<Rect> {
  rects
    .filter_map(|[x0, y0, x1, y1]| {
      let x0 = x0.clamp(0, i64::from(width));
      let y0 = y0.clamp(0, i64::from(height));
      let x1 = x1.clamp(0, i64::from(width));
      let y1 = y1.clamp(0, i64::from(height));
      (x0 < x1 && y0 < y1).then(|| {
        Rect::new(x0 as i32, y0 as i32, (x1 - x0) as u32, (y1 - y0) as u32)
      })
    })
    .collect()
}

struct SurfaceData;
struct PoolData;
pub(crate) struct BufferData;

impl GlobalDispatch<WlCompositor, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wl_compositor");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WlCompositor>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WlCompositor, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &WlCompositor,
    request: wl_compositor::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_compositor::Request::CreateSurface { id } => {
        state.create_surface(id, client, data_init)
      }
      wl_compositor::Request::CreateRegion { id } => {
        data_init.init(id, crate::region::RegionData);
      }
      _ => {}
    }
  }
}

impl Dispatch<WlSurface, SurfaceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlSurface,
    request: wl_surface::Request,
    _data: &SurfaceData,
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let object = resource.id();
    match request {
      wl_surface::Request::Destroy => {
        if state
          .surfaces
          .get(&object)
          .is_some_and(shell::has_live_role)
        {
          resource.post_error(
            wl_surface::Error::DefunctRoleObject,
            "destroy the surface role before destroying wl_surface",
          );
        }
      }
      wl_surface::Request::Attach { buffer, x, y } => {
        if resource.version() >= 5 && (x != 0 || y != 0) {
          resource.post_error(
            wl_surface::Error::InvalidOffset,
            concat!(
              "wl_surface.attach offsets are invalid since ",
              "version 5; use wl_surface.offset",
            ),
          );
          return;
        }
        let pending = match buffer {
          Some(buffer) => {
            state
              .buffers
              .get(&buffer.id())
              .cloned()
              .map(|mut attachment| {
                attachment.resource = buffer;
                let lock = attachment.lock();
                PendingBuffer::Attach(attachment, lock)
              })
          }
          None => Some(PendingBuffer::Detach),
        };
        if let (Some(pending), Some(surface)) =
          (pending, state.surfaces.get_mut(&object))
        {
          surface.pending.buffer = pending;
          surface.pending.offset_x = x;
          surface.pending.offset_y = y;
        }
      }
      wl_surface::Request::Damage {
        x,
        y,
        width,
        height,
      } => {
        if let Some(surface) = state.surfaces.get_mut(&object)
          && width > 0
          && height > 0
        {
          surface.pending.damage.push(Rect::new(
            x,
            y,
            width as u32,
            height as u32,
          ));
        }
      }
      wl_surface::Request::Frame { callback } => {
        let callback = data_init.init(callback, ());
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.pending.callbacks.push(callback);
        }
      }
      wl_surface::Request::SetOpaqueRegion { .. } => {}
      wl_surface::Request::SetInputRegion { region } => {
        crate::region::set_input(state, &object, region);
      }
      wl_surface::Request::Commit => state.commit(&object),
      wl_surface::Request::SetBufferTransform { transform } => {
        match transform {
          WEnum::Value(transform) => {
            if let Some(surface) = state.surfaces.get_mut(&object) {
              surface.pending.transform = transform;
            }
          }
          WEnum::Unknown(_) => resource.post_error(
            wl_surface::Error::InvalidTransform,
            "unknown buffer transform",
          ),
        }
      }
      wl_surface::Request::SetBufferScale { scale } => {
        if scale <= 0 {
          resource.post_error(
            wl_surface::Error::InvalidScale,
            "buffer scale must be positive",
          );
        } else if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.pending.scale = scale;
        }
      }
      wl_surface::Request::DamageBuffer {
        x,
        y,
        width,
        height,
      } => {
        if let Some(surface) = state.surfaces.get_mut(&object)
          && width > 0
          && height > 0
        {
          surface.pending.buffer_damage.push(Rect::new(
            x,
            y,
            width as u32,
            height as u32,
          ));
        }
      }
      wl_surface::Request::Offset { x, y } => {
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.pending.offset_x = x;
          surface.pending.offset_y = y;
        }
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlSurface,
    _data: &SurfaceData,
  ) {
    state.destroy_surface(&resource.id());
  }
}

impl Dispatch<WlCallback, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WlCallback,
    _request: wl_callback::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }
}

impl GlobalDispatch<WlShm, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wl_shm");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WlShm>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let resource = data_init.init(resource, ());
    for format in [
      wl_shm::Format::Argb8888,
      wl_shm::Format::Xrgb8888,
      wl_shm::Format::Abgr8888,
      wl_shm::Format::Xbgr8888,
    ] {
      resource.format(format);
    }
  }
}

impl Dispatch<WlShm, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlShm,
    request: wl_shm::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_shm::Request::CreatePool { id, fd, size } = request {
      let id = data_init.init(id, PoolData);
      match ShmPool::new(fd, size, state.max_buffer_size) {
        Ok(pool) => {
          state.pools.insert(id.id(), Arc::new(pool));
        }
        Err(message) => resource.post_error(wl_shm::Error::InvalidFd, message),
      }
    }
  }
}

impl Dispatch<WlShmPool, PoolData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlShmPool,
    request: wl_shm_pool::Request,
    _data: &PoolData,
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wl_shm_pool::Request::CreateBuffer {
        id,
        offset,
        width,
        height,
        stride,
        format,
      } => {
        let buffer = data_init.init(id, BufferData);
        let WEnum::Value(format) = format else {
          resource.post_error(
            wl_shm_pool::Error::InvalidFormat,
            "unknown buffer format",
          );
          return;
        };
        if Attachment::validate_format(resource, format).is_err() {
          return;
        }
        let Some(pool) = state.pools.get(&resource.id()).cloned() else {
          return;
        };
        match Attachment::new(
          buffer.clone(),
          pool,
          offset,
          width,
          height,
          stride,
          format,
          &state.buffer_releases,
        ) {
          Ok(attachment) => {
            state.buffers.insert(buffer.id(), attachment);
          }
          Err(message) => {
            resource.post_error(wl_shm_pool::Error::InvalidStride, message)
          }
        }
      }
      wl_shm_pool::Request::Resize { size } => {
        if let Some(pool) = state.pools.get(&resource.id())
          && let Err(message) = pool.resize(size)
        {
          resource.post_error(wl_shm_pool::Error::InvalidStride, message);
        }
      }
      wl_shm_pool::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlShmPool,
    _data: &PoolData,
  ) {
    state.pools.remove(&resource.id());
  }
}

impl Dispatch<WlBuffer, BufferData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WlBuffer,
    request: wl_buffer::Request,
    _data: &BufferData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_buffer::Request::Destroy = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlBuffer,
    _data: &BufferData,
  ) {
    crate::toplevel_icon::buffer_destroyed(state, &resource.id());
    state.buffers.remove(&resource.id());
  }
}

impl GlobalDispatch<WlOutput, crate::output::OutputGlobal> for CompositorState {
  fn can_view(client: Client, _: &crate::output::OutputGlobal) -> bool {
    crate::security_context::can_view(client, "wl_output")
  }

  /// Binds the output. An output bound from a replaced global, which a
  /// client may still bind for a few seconds, is described once but not
  /// kept: it stands for an output that is gone.
  fn bind(
    state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WlOutput>,
    global_data: &crate::output::OutputGlobal,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let resource = data_init.init(resource, *global_data);
    let generation = global_data.generation;
    let Some(output) = state.outputs.by_generation(generation) else {
      // An output that is gone: described as the first output once, and
      // never kept.
      state.primary().send(&resource, true);
      return;
    };
    // Surfaces, toplevel handles and workspace groups on it hear so at
    // the end of the turn.
    output.send(&resource, true);
    if let Some(output) = state.outputs.by_generation_mut(generation) {
      output.bound.insert(resource.id(), resource);
    }
  }
}

impl Dispatch<WlOutput, crate::output::OutputGlobal> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WlOutput,
    request: wl_output::Request,
    _data: &crate::output::OutputGlobal,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_output::Request::Release = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlOutput,
    data: &crate::output::OutputGlobal,
  ) {
    if let Some(output) = state.outputs.by_generation_mut(data.generation) {
      output.bound.remove(&resource.id());
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn surface_damage_lands_on_the_turned_buffer_pixel() {
    use wl_output::Transform;
    // A 4x2 surface; its top-left pixel and where each transform keeps it
    // in the buffer, which is 2x4 for quarter turns.
    for (transform, width, height, pixel) in [
      (Transform::Normal, 4, 2, (0, 0)),
      (Transform::_90, 2, 4, (0, 3)),
      (Transform::_180, 4, 2, (3, 1)),
      (Transform::_270, 2, 4, (1, 0)),
      (Transform::Flipped, 4, 2, (3, 0)),
      (Transform::Flipped90, 2, 4, (0, 0)),
      (Transform::Flipped180, 4, 2, (0, 1)),
      (Transform::Flipped270, 2, 4, (1, 3)),
    ] {
      let space = DamageSpace {
        width,
        height,
        scale: 1,
        transform,
        viewport: Default::default(),
      };
      assert_eq!(
        surface_damage_to_buffer(vec![Rect::new(0, 0, 1, 1)], &space),
        [Rect::new(pixel.0, pixel.1, 1, 1)],
        "{transform:?}"
      );
      assert_eq!(
        surface_damage_to_buffer(vec![Rect::new(-5, -5, 50, 50)], &space),
        [Rect::new(0, 0, width, height)],
        "{transform:?}"
      );
    }
  }

  #[test]
  fn fractional_viewport_damage_covers_every_touched_pixel() {
    let space = DamageSpace {
      width: 10,
      height: 10,
      scale: 1,
      transform: wl_output::Transform::Normal,
      viewport: crate::viewport::ViewportState {
        source: Some([0.5, 0.0, 9.0, 9.0]),
        destination: Some((3, 3)),
      },
    };
    // One destination pixel is 3x3 source pixels from x 0.5.
    assert_eq!(
      surface_damage_to_buffer(vec![Rect::new(0, 0, 1, 1)], &space),
      [Rect::new(0, 0, 4, 3)]
    );
  }
}
