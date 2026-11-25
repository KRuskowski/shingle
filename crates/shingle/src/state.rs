use std::{
  collections::{BTreeSet, HashMap, HashSet},
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use shingle_core::{
  Gaps, LaunchIdentity, OutputId, Rect, Shingle, WindowId, WorkspaceId,
};
use shingle_idle::{Command, DpmsState, IdleCoordinator, LockState};
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
  input::InputState,
  shell::{self, Role},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceId(pub u64);

impl SurfaceId {
  pub const fn get(self) -> u64 {
    self.0
  }
}

pub(crate) struct ClientDataImpl {
  pub(crate) disconnected: Arc<Mutex<Vec<ClientId>>>,
  pub(crate) sandbox: Option<crate::security_context::SandboxMetadata>,
  pub(crate) privileged_protocols: Arc<HashSet<String>>,
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

pub(crate) enum PendingBuffer {
  Unchanged,
  Attach(Attachment),
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
  pub(crate) output: Rect,
  pub(crate) background_color: [f64; 4],
  pub(crate) workspace_width: u32,
  pub(crate) workspace_height: u32,
  pub(crate) workspace_wraparound: crate::WorkspaceWraparound,
  pub(crate) output_scale: f64,
  pub(crate) output_transform: wl_output::Transform,
  pub(crate) cursor_theme: String,
  pub(crate) cursor_size: u32,
  pub(crate) output_id: OutputId,
  pub(crate) output_info: crate::OutputInfo,
  pub(crate) output_power: crate::output_power::OutputPower,
  pub(crate) output_management: crate::output_management::OutputManagement,
  pub(crate) started: Instant,
  pub(crate) surfaces: HashMap<ObjectId, Surface>,
  pub(crate) regions: HashMap<ObjectId, Vec<Rect>>,
  pub(crate) subsurface_order: HashMap<ObjectId, Vec<ObjectId>>,
  pub(crate) viewports: HashMap<ObjectId, crate::viewport::Viewport>,
  pub(crate) fractional_scales: HashMap<ObjectId, WpFractionalScaleV1>,
  pub(crate) surface_ids: HashMap<SurfaceId, ObjectId>,
  pub(crate) pools: HashMap<ObjectId, Arc<ShmPool>>,
  pub(crate) buffers: HashMap<ObjectId, Attachment>,
  pub(crate) outputs: HashMap<ObjectId, WlOutput>,
  pub(crate) xdg_outputs: HashMap<ObjectId, ZxdgOutputV1>,
  pub(crate) events: Vec<Event>,
  pub(crate) ipc_events: Vec<serde_json::Value>,
  pub(crate) pre_map_clients: HashSet<u64>,
  pub(crate) pending_maps: HashMap<WindowId, PendingMap>,
  pub(crate) delayed_maps: HashSet<WindowId>,
  pub(crate) frame_callbacks: Vec<FrameCallback>,
  pub(crate) disconnected: Arc<Mutex<Vec<ClientId>>>,
  pub(crate) foreign_toplevels: crate::foreign_toplevel::ForeignToplevels,
  pub(crate) foreign: crate::foreign::ForeignHandles,
  pub(crate) ext_workspaces: crate::ext_workspace::ExtWorkspaces,
  pub(crate) ext_toplevels: crate::ext_toplevel::ExtToplevels,
  pub(crate) launch_templates: HashMap<ClientId, LaunchIdentity>,
  pub(crate) socket_name: std::ffi::OsString,
  pub(crate) ipc_children: Vec<std::process::Child>,
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
  pub(crate) focus_recency: HashMap<shingle_core::WindowId, u64>,
  pub(crate) view_properties:
    HashMap<shingle_core::WindowId, HashMap<String, crate::ipc::ViewProperty>>,
  pub(crate) runtime_bindings:
    std::collections::BTreeMap<u64, crate::RuntimeBinding>,
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
  pub(crate) focus_main_surface_instead_of_popup: bool,
  pub(crate) last_workspace: Option<shingle_core::WorkspaceId>,
  last_lock_state: LockState,
  last_dpms_state: DpmsState,
}

impl CompositorState {
  pub(crate) fn new(
    mut config: Config,
    mut core: Shingle,
    disconnected: Arc<Mutex<Vec<ClientId>>>,
    socket_name: &std::ffi::OsStr,
  ) -> Result<Self, crate::Error> {
    if config.workspace_width == 0 || config.workspace_height == 0 {
      return Err(crate::Error::InvalidWorkspaceGrid);
    }
    if !config.output_scale.is_finite() || config.output_scale <= 0.0 {
      return Err(crate::Error::InvalidOutputScale);
    }
    if crate::output_transform::is_quarter_turn(config.output_transform) {
      std::mem::swap(&mut config.output.width, &mut config.output.height);
    }
    let output_id = OutputId::new(OUTPUT_NAME);
    if core.outputs().any(|output| output == &output_id) {
      core.set_output_workarea(&output_id, config.output)?;
    } else {
      core.add_output(output_id.clone(), config.output)?;
    }
    let started = Instant::now();
    let idle = IdleCoordinator::new(config.idle, Duration::ZERO);
    Ok(Self {
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
      output: config.output,
      background_color: config.background_color,
      workspace_width: config.workspace_width,
      workspace_height: config.workspace_height,
      workspace_wraparound: config.workspace_wraparound,
      output_scale: config.output_scale,
      output_transform: config.output_transform,
      cursor_theme: config.cursor_theme,
      cursor_size: config.cursor_size,
      output_id,
      output_info: Default::default(),
      output_power: Default::default(),
      output_management: Default::default(),
      started,
      surfaces: HashMap::new(),
      regions: HashMap::new(),
      subsurface_order: HashMap::new(),
      viewports: HashMap::new(),
      fractional_scales: HashMap::new(),
      surface_ids: HashMap::new(),
      pools: HashMap::new(),
      buffers: HashMap::new(),
      outputs: HashMap::new(),
      xdg_outputs: HashMap::new(),
      events: Vec::new(),
      ipc_events: Vec::new(),
      pre_map_clients: HashSet::new(),
      pending_maps: HashMap::new(),
      delayed_maps: HashSet::new(),
      frame_callbacks: Vec::new(),
      disconnected,
      foreign_toplevels: Default::default(),
      foreign: Default::default(),
      ext_workspaces: Default::default(),
      ext_toplevels: Default::default(),
      launch_templates: HashMap::new(),
      socket_name: socket_name.to_owned(),
      ipc_children: Vec::new(),
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
      focus_recency: HashMap::new(),
      view_properties: HashMap::new(),
      runtime_bindings: Default::default(),
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
      focus_main_surface_instead_of_popup: config
        .focus_main_surface_instead_of_popup,
      last_workspace: None,
      last_lock_state: LockState::Unlocked,
      last_dpms_state: DpmsState::On,
    })
  }

  pub(crate) fn switch_workspace(
    &mut self,
    index: usize,
  ) -> Result<(), crate::Error> {
    if index >= self.workspace_width as usize * self.workspace_height as usize {
      return Err(crate::Error::InvalidWorkspace(index));
    }
    let workspace = WorkspaceId::new(
      index as u32 % self.workspace_width,
      index as u32 / self.workspace_width,
    );
    self.core.ensure_workspace(&self.output_id, workspace)?;
    let previous = self.core.current_workspace(&self.output_id)?;
    if previous != workspace {
      shell::disable_show_desktop(self);
      self.last_workspace = Some(previous);
    }
    self
      .core
      .set_current_workspace(&self.output_id, workspace)?;
    self.input.keyboard_focus = None;
    crate::input::sync_focus(self);
    shell::reconfigure_all(self);
    self.events.push(crate::Event::WorkspaceChanged {
      workspace: index as u32 + 1,
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
    let current = self.core.current_workspace(&self.output_id)?;
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
      if location.output != self.output_id {
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
    let current = self.core.current_workspace(&self.output_id)?;
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
    let current = self.core.current_workspace(&self.output_id)?;
    let window = self.core.tree(&self.output_id, current)?.focused();
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
    self.core.ensure_workspace(&self.output_id, workspace)?;
    let previous = self.core.window_location(window)?.workspace;
    let from = previous.y * self.workspace_width + previous.x + 1;
    let detached = self.core.detach_window(window)?;
    self
      .core
      .reattach_window(detached, &self.output_id, workspace)?;
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

  pub(crate) fn create_globals(&self, handle: &DisplayHandle) {
    crate::activation::create_global(handle);
    crate::foreign::create_globals(handle);
    crate::foreign_toplevel::create_global(handle);
    crate::ext_toplevel::create_global(handle);
    crate::ext_workspace::create_global(handle);
    crate::xdg_output::create_global(handle);
    handle.create_global::<CompositorState, WlCompositor, ()>(6, ());
    handle.create_global::<CompositorState, WlShm, ()>(2, ());
    handle.create_global::<CompositorState, WlOutput, ()>(4, ());
    crate::output_power::create_global(handle);
    crate::output_management::create_global(handle);
    crate::security_context::create_global(handle);
    crate::alpha_modifier::create_global(handle);
    crate::tearing::create_global(handle);
    crate::fifo::create_global(handle);
    crate::commit_timing::create_global(handle);
    crate::gamma_control::create_global(handle);
    crate::input::create_globals(handle);
    if self.enable_text_input_v3 {
      crate::text_input::create_global(handle);
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
    crate::shortcuts::create_global(handle);
    crate::single_pixel_buffer::create_global(handle);
    crate::toplevel_icon::create_global(handle);
    crate::subsurface::create_global(handle);
    crate::viewport::create_global(handle);
    crate::virtual_pointer::create_global(handle);
    crate::virtual_keyboard::create_global(handle);
    crate::fractional_scale::create_global(handle);
    shell::create_globals(handle);
  }

  pub(crate) fn now(&self) -> Duration {
    self.started.elapsed()
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

  pub(crate) fn visible_surfaces(&self) -> Vec<SurfaceSnapshot> {
    if self.idle.dpms_state() == DpmsState::Off {
      return Vec::new();
    }
    if self.idle.lock_state() != LockState::Unlocked {
      let mut surfaces = self
        .surfaces
        .values()
        .filter(|surface| {
          surface.buffer.is_some()
            && matches!(surface.role, Role::SessionLock { .. })
        })
        .collect::<Vec<_>>();
      surfaces.sort_by_key(|surface| surface.id);
      return surfaces
        .into_iter()
        .filter_map(|surface| self.snapshot(&surface.resource.id()))
        .collect();
    }
    let mut snapshots = Vec::new();
    self.append_layers(&mut snapshots, &[Layer::Background, Layer::Bottom]);
    let Ok(workspace) = self.core.current_workspace(&self.output_id) else {
      return snapshots;
    };
    let Ok(tree) = self.core.tree(&self.output_id, workspace) else {
      return snapshots;
    };
    let focused = crate::input::focused_window(self).or_else(|| tree.focused());
    let fullscreen = self.surfaces.iter().find_map(|(object, surface)| {
      (!self.show_desktop).then_some(object.clone()).filter(|_| {
        surface.role.toplevel().is_some_and(|toplevel| {
          toplevel.window() == focused
            && toplevel.window().is_some_and(|window| {
              self.core.window_info(window).is_ok_and(|info| {
                info.state.fullscreen && !info.state.minimized
              })
            })
            && surface.buffer.is_some()
        })
      })
    });
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
                  && (info.location.output != self.output_id
                    || info.location.workspace != workspace)
              })
            })
      })
      .collect::<Vec<_>>();
    sticky.sort_by_key(|surface| match &surface.role {
      Role::Xdg {
        kind: Some(shell::XdgKind::Toplevel(toplevel)),
        ..
      } => toplevel.window(),
      _ => None,
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
        let matches_mode = match &surface.role {
          Role::Xdg {
            kind: Some(shell::XdgKind::Toplevel(toplevel)),
            ..
          } => toplevel.window().is_some_and(|window| {
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
    if let Some(object) = fullscreen {
      self.append_surface_and_popups(&object, &mut snapshots);
      self.append_layers(&mut snapshots, &[Layer::Overlay]);
    } else {
      self.append_layers(&mut snapshots, &[Layer::Top, Layer::Overlay]);
    }
    if let Some(object) = &self.input.cursor_surface
      && self
        .surfaces
        .get(object)
        .is_some_and(|surface| surface.buffer.is_some())
      && let Some(snapshot) = self.snapshot(object)
    {
      snapshots.push(snapshot);
    } else if let Some(snapshot) = crate::cursor_shape::snapshot(self) {
      snapshots.push(snapshot);
    }
    snapshots
  }

  fn append_layers(
    &self,
    snapshots: &mut Vec<SurfaceSnapshot>,
    layers: &[Layer],
  ) {
    let mut surfaces = self
      .surfaces
      .values()
      .filter(|surface| {
        surface.buffer.is_some()
          && match &surface.role {
            Role::Layer(layer) => layers.contains(&layer.current.layer),
            Role::InputPanel(panel) => {
              panel.visible && layers.contains(&Layer::Overlay)
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
      matches!(&surface.role, Role::Xdg {
          kind: Some(shell::XdgKind::Toplevel(toplevel)), ..
      } if toplevel.window().is_some_and(|window| {
          self.core.window_info(window).is_ok_and(|info| {
              info.presence != shingle_core::WindowPresence::Mapped
          })
      }))
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
    let Ok(current_workspace) = self.core.current_workspace(&self.output_id)
    else {
      return Vec::new();
    };
    let mut snapshots = self
      .surfaces
      .iter()
      .filter_map(|(object, surface)| {
        surface.buffer.as_ref()?;
        let Role::Xdg {
          kind: Some(shell::XdgKind::Toplevel(toplevel)),
          ..
        } = &surface.role
        else {
          return None;
        };
        let window = toplevel.window()?;
        let info = self.core.window_info(window).ok()?;
        if info.presence != shingle_core::WindowPresence::Mapped
          || info.location.output != self.output_id
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

  pub(crate) fn set_output_transform(
    &mut self,
    transform: wl_output::Transform,
  ) {
    let old = self.output_transform;
    if old == transform {
      return;
    }
    self.output_transform = transform;
    if crate::output_transform::is_quarter_turn(old)
      != crate::output_transform::is_quarter_turn(transform)
    {
      std::mem::swap(&mut self.output.width, &mut self.output.height);
    }
    let output = self.output;
    for resource in self.outputs.values() {
      self.send_output_geometry(resource);
      let (width, height) = crate::output_transform::physical_size(
        output.width,
        output.height,
        self.output_scale,
        transform,
      );
      resource.mode(
        wl_output::Mode::Current | wl_output::Mode::Preferred,
        width.min(i32::MAX as u32) as i32,
        height.min(i32::MAX as u32) as i32,
        self.output_info.refresh_mhz as i32,
      );
      if resource.version() >= 2 {
        resource.done();
      }
    }
    crate::layer_shell::update_workarea(self);
    shell::reconfigure_lock_surfaces(self);
    shell::reconfigure_all(self);
    crate::xdg_output::update(self);
    crate::output_management::sync(self);
    self.events.push(Event::OutputChanged { output });
  }

  /// Sends the output's position, physical size, subpixel layout, make,
  /// model and transform.
  fn send_output_geometry(&self, resource: &WlOutput) {
    let info = &self.output_info;
    let (width_mm, height_mm) = info.physical_size_mm;
    resource.geometry(
      self.output.x,
      self.output.y,
      width_mm.min(i32::MAX as u32) as i32,
      height_mm.min(i32::MAX as u32) as i32,
      info.subpixel,
      info.make.clone(),
      info.model.clone(),
      self.output_transform,
    );
  }

  pub(crate) fn set_output_info(&mut self, info: crate::OutputInfo) {
    self.output_info = info;
    let output = self.output;
    for resource in self.outputs.values() {
      self.send_output_geometry(resource);
      let (width, height) = crate::output_transform::physical_size(
        output.width,
        output.height,
        self.output_scale,
        self.output_transform,
      );
      resource.mode(
        wl_output::Mode::Current | wl_output::Mode::Preferred,
        width.min(i32::MAX as u32) as i32,
        height.min(i32::MAX as u32) as i32,
        self.output_info.refresh_mhz as i32,
      );
      if resource.version() >= 4 {
        resource.description(self.output_info.description());
      }
      if resource.version() >= 2 {
        resource.done();
      }
    }
    crate::xdg_output::update(self);
    crate::output_management::sync(self);
  }

  pub(crate) fn set_output_scale(
    &mut self,
    scale: f64,
  ) -> Result<(), crate::Error> {
    if !scale.is_finite() || scale <= 0.0 {
      return Err(crate::Error::InvalidOutputScale);
    }
    self.output_scale = scale;
    let output = self.output;
    for resource in self.outputs.values() {
      if resource.version() >= 2 {
        resource.scale(scale.ceil().clamp(1.0, i32::MAX as f64) as i32);
      }
      let (width, height) = crate::output_transform::physical_size(
        output.width,
        output.height,
        scale,
        self.output_transform,
      );
      resource.mode(
        wl_output::Mode::Current | wl_output::Mode::Preferred,
        width.min(i32::MAX as u32) as i32,
        height.min(i32::MAX as u32) as i32,
        self.output_info.refresh_mhz as i32,
      );
      if resource.version() >= 2 {
        resource.done();
      }
    }
    crate::fractional_scale::sync(self);
    crate::output_management::sync(self);
    self.events.push(Event::OutputChanged { output });
    Ok(())
  }

  pub(crate) fn set_output(
    &mut self,
    output: Rect,
  ) -> Result<(), crate::Error> {
    output.validate()?;
    self.output = output;
    for resource in self.outputs.values() {
      self.send_output_geometry(resource);
      let scale = self.output_scale;
      let (width, height) = crate::output_transform::physical_size(
        output.width,
        output.height,
        scale,
        self.output_transform,
      );
      resource.mode(
        wl_output::Mode::Current | wl_output::Mode::Preferred,
        width.min(i32::MAX as u32) as i32,
        height.min(i32::MAX as u32) as i32,
        self.output_info.refresh_mhz as i32,
      );
      if resource.version() >= 2 {
        resource.scale(scale.ceil().clamp(1.0, i32::MAX as f64) as i32);
        resource.done();
      }
    }
    crate::layer_shell::update_workarea(self);
    shell::reconfigure_lock_surfaces(self);
    crate::xdg_output::update(self);
    crate::output_management::sync(self);
    self.events.push(Event::OutputChanged { output });
    Ok(())
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
        Command::RequestDpmsOff => {
          self.events.push(Event::DpmsChanged(DpmsState::Off))
        }
        Command::RequestDpmsOn => {
          self.events.push(Event::DpmsChanged(DpmsState::On))
        }
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

  fn commit(&mut self, object: &ObjectId) {
    self.commit_inner(object, false);
  }

  pub(crate) fn commit_cached(&mut self, object: &ObjectId) {
    self.commit_inner(object, true);
  }

  pub(crate) fn commit_fifo(&mut self, object: &ObjectId) {
    self.commit_inner(object, false);
  }

  fn commit_inner(&mut self, object: &ObjectId, apply_cached: bool) {
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
      let Some(surface) = self.surfaces.get_mut(object) else {
        return;
      };
      let mut pending = std::mem::take(&mut surface.pending);
      surface.pending.scale = pending.scale;
      surface.pending.transform = pending.transform;
      if let Some(cached) = &mut surface.cached_commit {
        merge_pending(cached, &mut pending);
      } else {
        surface.cached_commit = Some(pending);
      }
      return;
    }
    if apply_cached {
      let Some(surface) = self.surfaces.get_mut(object) else {
        return;
      };
      let Some(pending) = surface.cached_commit.take() else {
        return;
      };
      surface.pending = pending;
    }
    if crate::fifo::commit(self, object) {
      let Some(surface) = self.surfaces.get_mut(object) else {
        return;
      };
      let pending = std::mem::take(&mut surface.pending);
      surface.pending.scale = pending.scale;
      surface.pending.transform = pending.transform;
      crate::fifo::defer(self, object, pending);
      return;
    }
    if crate::commit_timing::commit(self, object) {
      let Some(surface) = self.surfaces.get_mut(object) else {
        return;
      };
      let pending = std::mem::take(&mut surface.pending);
      surface.pending.scale = pending.scale;
      surface.pending.transform = pending.transform;
      crate::commit_timing::defer(self, object, pending);
      return;
    }
    crate::region::commit_input(self, object);
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
    let pending_buffer =
      std::mem::replace(&mut surface.pending.buffer, PendingBuffer::Unchanged);
    let attachment = match pending_buffer {
      PendingBuffer::Unchanged => None,
      PendingBuffer::Attach(attachment) => Some(Some(attachment)),
      PendingBuffer::Detach => Some(None),
    };
    let surface_damage = std::mem::take(&mut surface.pending.damage);
    let mut buffer_damage = std::mem::take(&mut surface.pending.buffer_damage);
    if let Some(Some(attachment)) = &attachment {
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
      let mut damage =
        clip_damage(surface_damage, attachment.width, attachment.height, scale);
      damage.extend(clip_damage(
        buffer_damage,
        attachment.width,
        attachment.height,
        1,
      ));
      let offset_x = surface.pending.offset_x;
      let offset_y = surface.pending.offset_y;
      match attachment.snapshot(damage, offset_x, offset_y) {
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
      let mut damage =
        clip_damage(surface_damage, buffer.width, buffer.height, scale);
      damage.extend(clip_damage(buffer_damage, buffer.width, buffer.height, 1));
      if let Some(surface) = self.surfaces.get_mut(object)
        && let Some(buffer) = &mut surface.buffer
      {
        buffer.damage = damage;
        buffer.transform = surface.pending.transform;
      }
    }

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
    if let Some(surface) = self.surfaces.get(object) {
      for output in self.outputs.values() {
        if output.client().map(|client| client.id())
          == Some(surface.client.clone())
        {
          surface.resource.enter(output);
        }
      }
    }
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
}

fn clip_damage(
  damage: Vec<Rect>,
  width: u32,
  height: u32,
  scale: i32,
) -> Vec<Rect> {
  let mut clipped = Vec::new();
  for rect in damage {
    let scale = i64::from(scale.max(1));
    let x0 = i64::from(rect.x) * scale;
    let y0 = i64::from(rect.y) * scale;
    let x1 = x0.saturating_add(i64::from(rect.width) * scale);
    let y1 = y0.saturating_add(i64::from(rect.height) * scale);
    let x0 = x0.clamp(0, i64::from(width));
    let y0 = y0.clamp(0, i64::from(height));
    let x1 = x1.clamp(0, i64::from(width));
    let y1 = y1.clamp(0, i64::from(height));
    if x0 < x1 && y0 < y1 {
      clipped.push(Rect::new(
        x0 as i32,
        y0 as i32,
        (x1 - x0) as u32,
        (y1 - y0) as u32,
      ));
    }
  }
  clipped
}

struct SurfaceData;
struct PoolData;
pub(crate) struct BufferData;
struct OutputData;

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
                PendingBuffer::Attach(attachment)
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

impl GlobalDispatch<WlOutput, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wl_output");

  fn bind(
    state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WlOutput>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let resource = data_init.init(resource, OutputData);
    state.send_output_geometry(&resource);
    let scale = state.output_scale;
    let (width, height) = crate::output_transform::physical_size(
      state.output.width,
      state.output.height,
      scale,
      state.output_transform,
    );
    resource.mode(
      wl_output::Mode::Current | wl_output::Mode::Preferred,
      width.min(i32::MAX as u32) as i32,
      height.min(i32::MAX as u32) as i32,
      state.output_info.refresh_mhz as i32,
    );
    if resource.version() >= 2 {
      resource.scale(scale.ceil().clamp(1.0, i32::MAX as f64) as i32);
    }
    if resource.version() >= 4 {
      resource.name(state.output_info.name.clone());
      resource.description(state.output_info.description());
    }
    if resource.version() >= 2 {
      resource.done();
    }
    state.outputs.insert(resource.id(), resource);
  }
}

impl Dispatch<WlOutput, OutputData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WlOutput,
    request: wl_output::Request,
    _data: &OutputData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wl_output::Request::Release = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlOutput,
    _data: &OutputData,
  ) {
    state.outputs.remove(&resource.id());
  }
}
