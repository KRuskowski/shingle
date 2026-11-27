use std::{
  collections::{BTreeMap, BTreeSet},
  sync::Mutex,
};

use shingle_config::RuleEvent;
use shingle_core::{
  LaunchIdentity, LeafState, OutputId, Rect, WindowId, WindowPresence,
};
use shingle_idle::LockState;
use wayland_protocols::{
  ext::session_lock::v1::server::{
    ext_session_lock_manager_v1::{self, ExtSessionLockManagerV1},
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
  },
  xdg::shell::server::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
  },
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
};

use crate::{
  Event, SurfaceRole,
  state::{CompositorState, PendingBuffer, Surface, SurfaceId},
};

#[derive(Clone)]
pub(crate) enum Role {
  None,
  Dead,
  Cursor,
  /// A Wayland client's drag icon (`wl_data_device.start_drag`), drawn at
  /// the pointer moved by every `wl_surface` offset since the drag began.
  DragIcon {
    offset: (i32, i32),
  },
  Xdg {
    resource: XdgSurface,
    kind: Option<XdgKind>,
    serials: BTreeSet<u32>,
    initial_acked: bool,
  },
  Layer(Box<crate::layer_shell::LayerRole>),
  InputPanel(Box<crate::input_panel::InputPanelRole>),
  Subsurface {
    resource: wayland_server::protocol::wl_subsurface::WlSubsurface,
    parent: ObjectId,
    relative: (i32, i32),
    synchronized: bool,
  },
  SessionLock {
    resource: ExtSessionLockSurfaceV1,
    /// The output the surface covers.
    output: OutputId,
    /// The configures not acknowledged yet, with the size each sent.
    serials: BTreeMap<u32, (u32, u32)>,
    acked: Option<(u32, u32, u32)>,
    /// Whether it has committed a full frame: a buffer of its output's
    /// size that hides everything under it. Until then its output stays
    /// black (docs/design/lock-screen.md).
    shown: bool,
  },
  /// A surface of Xwayland's, named by `xwayland_shell_v1`.
  Xwayland(Box<crate::xwayland::shell::XwaylandRole>),
}

impl Role {
  /// Returns the toplevel role data if this surface is a toplevel, whatever
  /// shell it came from.
  pub(crate) fn toplevel(&self) -> Option<&ToplevelRole> {
    match self {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } => Some(toplevel),
      Role::Xwayland(role) => role.toplevel.as_deref(),
      _ => None,
    }
  }

  /// Returns the toplevel role data mutably if this surface is a toplevel,
  /// whatever shell it came from.
  pub(crate) fn toplevel_mut(&mut self) -> Option<&mut ToplevelRole> {
    match self {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } => Some(toplevel),
      Role::Xwayland(role) => role.toplevel.as_deref_mut(),
      _ => None,
    }
  }

  /// Returns the xdg_popup role data if this is a popup surface.
  pub(crate) fn popup(&self) -> Option<&PopupRole> {
    match self {
      Role::Xdg {
        kind: Some(XdgKind::Popup(popup)),
        ..
      } => Some(popup),
      _ => None,
    }
  }
}

#[derive(Clone)]
pub(crate) enum XdgKind {
  Toplevel(Box<ToplevelRole>),
  Popup(PopupRole),
  PendingPopup {
    resource: XdgPopup,
    positioner: XdgPositioner,
  },
}

/// The protocol object behind a toplevel.
///
/// Everything else about a toplevel lives in [`ToplevelRole`] and is the
/// same for every shell, so tiling, rules, Scale, Expo, IPC and sessions
/// never look at this. Only configure, close and destruction differ per
/// shell, and they go through `configure_toplevel`, `close_window` and
/// `detach_toplevel` (docs/design/xwayland.md, section 4).
#[derive(Clone)]
pub(crate) enum ToplevelHandle {
  /// An `xdg_toplevel`.
  Xdg(XdgToplevel),
  /// A managed X11 window, by its X id.
  X11(shingle_xwayland::Window),
}

impl ToplevelHandle {
  /// The `xdg_toplevel`, for xdg-only protocols such as xdg-dialog,
  /// xdg-foreign and xdg-toplevel-icon.
  pub(crate) fn xdg(&self) -> Option<&XdgToplevel> {
    match self {
      ToplevelHandle::Xdg(toplevel) => Some(toplevel),
      ToplevelHandle::X11(_) => None,
    }
  }
}

/// The window states a toplevel is configured with, whatever its shell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToplevelStates {
  /// In a tiling tree, so tiled on all four edges.
  pub(crate) tiled: bool,
  /// Shown as the active window.
  pub(crate) activated: bool,
  pub(crate) maximized: bool,
  pub(crate) fullscreen: bool,
}

impl ToplevelStates {
  /// The `xdg_toplevel.configure` state array, in native byte order.
  pub(crate) fn xdg_states(self) -> Vec<u8> {
    let mut states = if self.tiled {
      vec![
        xdg_toplevel::State::TiledLeft,
        xdg_toplevel::State::TiledRight,
        xdg_toplevel::State::TiledTop,
        xdg_toplevel::State::TiledBottom,
      ]
    } else {
      Vec::new()
    };
    if self.activated {
      states.push(xdg_toplevel::State::Activated);
    }
    if self.maximized {
      states.push(xdg_toplevel::State::Maximized);
    }
    if self.fullscreen {
      states.push(xdg_toplevel::State::Fullscreen);
    }
    states
      .into_iter()
      .flat_map(|state| (state as u32).to_ne_bytes())
      .collect()
  }
}

/// The size and states of one `xdg_toplevel.configure`.
type XdgConfigure = (i32, i32, ToplevelStates);

/// When an xdg toplevel is configured. Configures are sent once per
/// compositor turn, in [`flush_configures`], with the toplevel's latest
/// size and states, and only when those differ from the last configure
/// sent or the client asked for one, as wlroots schedules them on idle and
/// Wayfire configures only a size that changed
/// (wayfire:src/view/xdg-shell/xdg-toplevel.cpp,
/// `configure_surface_with_state`). A pointer resize that moves many
/// times between two frames thus sends one configure to the window it
/// resizes and none to the windows whose size stays the same.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ConfigureState {
  /// What the next configure would say, set during the turn.
  pending: Option<XdgConfigure>,
  /// What the last configure said.
  sent: Option<XdgConfigure>,
  /// The client asked for a configure, as `set_maximized` and the other
  /// state requests do, so one goes out even if nothing changed.
  requested: bool,
}

#[derive(Clone)]
pub(crate) struct ToplevelRole {
  pub(crate) handle: ToplevelHandle,
  pub(crate) window: WindowId,
  pub(crate) identity: LaunchIdentity,
  pub(crate) app_id: String,
  pub(crate) title: String,
  pub(crate) parent: Option<WindowId>,
  pub(crate) tiled: bool,
  pub(crate) opacity: f32,
  pub(crate) floating_geometry: Rect,
  pub(crate) min_size: (i32, i32),
  pub(crate) max_size: (i32, i32),
  pub(crate) configured_geometry: Option<Rect>,
  /// The xdg configure waiting for the end of the turn, and the last one
  /// sent.
  pub(crate) configure: ConfigureState,
  pub(crate) created_rules_pending: Option<bool>,
  /// Where a tiled window floats when it leaves the tree, in layout
  /// coordinates, once a window rule moved or resized it: Wayfire's rules
  /// change a view's geometry before simple-tile tiles it, and untiling
  /// gives it back (`update_last_windowed_geometry`).
  pub(crate) windowed: Option<Rect>,
  /// A null buffer unmapped the toplevel, so its next commit without a
  /// buffer is a new initial commit, which xdg-shell answers with a
  /// configure.
  pub(crate) unmapped: bool,
  /// The client's PID, for IPC's `pid`: an xdg toplevel's from its
  /// connection's peer credentials, an X window's from XRes or
  /// `_NET_WM_PID`.
  pub(crate) pid: Option<i32>,
  /// The client draws its own decorations: an xdg toplevel unless it
  /// acknowledged server-side mode, an X window whose `_MOTIF_WM_HINTS`
  /// leave out the title or border (Wayfire's `has_client_decoration`).
  pub(crate) client_decorated: bool,
  /// Whether the window has a server-side frame (`decorator.rs`), once
  /// that is decided.
  pub(crate) decorated: Option<bool>,
  /// The active output when the toplevel was created, which its window
  /// opens on unless it has a parent: Wayfire sets a view's output to the
  /// focused one when it creates the view, and `adjust_view_output_on_map`
  /// keeps it (wayfire:src/view/xdg-shell/xdg-toplevel-view.cpp,
  /// `xdg_toplevel_view_t::create`; src/view/view-impl.cpp).
  pub(crate) created_output: OutputId,
}

impl ToplevelRole {
  pub(crate) fn window(&self) -> Option<WindowId> {
    (self.window.0 != 0).then_some(self.window)
  }
}

#[derive(Clone)]
pub(crate) struct PopupRole {
  pub(crate) resource: XdgPopup,
  pub(crate) parent: ObjectId,
  pub(crate) relative: Rect,
}

/// The shell globals; `ext_session_lock_manager_v1` only with
/// `session_lock`, Wayfire's `session-lock` plugin.
pub(crate) fn create_globals(handle: &DisplayHandle, session_lock: bool) {
  handle.create_global::<CompositorState, XdgWmBase, ()>(6, ());
  if session_lock {
    handle.create_global::<CompositorState, ExtSessionLockManagerV1, ()>(1, ());
  }
  crate::dialog::create_global(handle);
  crate::layer_shell::create_global(handle);
  crate::xwayland::shell::create_global(handle);
}

pub(crate) fn has_live_role(surface: &Surface) -> bool {
  match &surface.role {
    Role::None | Role::Dead | Role::Cursor | Role::DragIcon { .. } => false,
    Role::Xwayland(role) => role.is_live(),
    _ => true,
  }
}

pub(crate) fn snapshot_metadata(
  state: &CompositorState,
  surface: &Surface,
) -> (
  SurfaceRole,
  Option<WindowId>,
  Option<SurfaceId>,
  Option<String>,
  Option<String>,
) {
  if let Some(toplevel) = surface.role.toplevel() {
    return (
      SurfaceRole::Toplevel,
      toplevel.window(),
      toplevel.parent.and_then(|parent| {
        state
          .surfaces
          .values()
          .find(|surface| {
            surface
              .role
              .toplevel()
              .is_some_and(|candidate| candidate.window == parent)
          })
          .map(|surface| surface.id)
      }),
      Some(toplevel.app_id.clone()),
      Some(toplevel.title.clone()),
    );
  }
  match &surface.role {
    Role::Xdg {
      kind: Some(XdgKind::Popup(popup)),
      ..
    } => (
      SurfaceRole::Popup,
      None,
      state.surfaces.get(&popup.parent).map(|surface| surface.id),
      None,
      None,
    ),
    Role::Subsurface { parent, .. } => (
      SurfaceRole::Subsurface,
      None,
      state.surfaces.get(parent).map(|surface| surface.id),
      None,
      None,
    ),
    Role::Layer(_) | Role::InputPanel(_) => {
      (SurfaceRole::LayerShell, None, None, None, None)
    }
    Role::Cursor => (SurfaceRole::Cursor, None, None, None, None),
    Role::DragIcon { .. } => (SurfaceRole::DragIcon, None, None, None, None),
    Role::SessionLock { .. } => {
      (SurfaceRole::SessionLock, None, None, None, None)
    }
    Role::Xwayland(role) => {
      let kind = if role.is_drag_icon() {
        SurfaceRole::DragIcon
      } else {
        SurfaceRole::Unmanaged
      };
      match crate::xwayland::unmanaged::names(state, role) {
        Some((class, title)) => (kind, None, None, Some(class), Some(title)),
        None => (SurfaceRole::Unassigned, None, None, None, None),
      }
    }
    _ => (SurfaceRole::Unassigned, None, None, None, None),
  }
}

pub(crate) fn opacity(state: &CompositorState, object: &ObjectId) -> f32 {
  let modifier = state
    .surfaces
    .get(object)
    .map_or(1.0, |surface| surface.alpha as f32 / u32::MAX as f32);
  let mut current = object.clone();
  loop {
    let Some(surface) = state.surfaces.get(&current) else {
      return modifier;
    };
    if let Some(toplevel) = surface.role.toplevel() {
      return modifier * toplevel.opacity;
    }
    match &surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Popup(popup)),
        ..
      } => current = popup.parent.clone(),
      Role::Subsurface { parent, .. } => current = parent.clone(),
      _ => return modifier,
    }
  }
}

/// The geometry of the window on toplevel surface `object`: its tile,
/// floating, maximized (workarea) or fullscreen (output) rectangle, which
/// includes its server-side frame, as a Wayfire view's geometry does.
pub(crate) fn window_geometry(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  let surface = state.surfaces.get(object)?;
  let toplevel = surface.role.toplevel()?;
  if toplevel.window().is_some_and(|window| {
    state
      .core
      .window_info(window)
      .is_ok_and(|info| info.state.fullscreen)
  }) {
    toplevel
      .window()
      .map(|window| state.output_of_window(window).rect)
  } else if !toplevel.tiled
    && toplevel.window().is_some_and(|window| {
      state
        .core
        .window_info(window)
        .is_ok_and(|info| info.state.maximized)
    })
  {
    toplevel
      .window()
      .and_then(|window| state.core.window_location(window).ok())
      .and_then(|location| {
        state.core.tree(&location.output, location.workspace).ok()
      })
      .map_or(state.active().rect, |tree| tree.workarea())
      .into()
  } else if !toplevel.tiled {
    Some(toplevel.floating_geometry)
  } else {
    Some(
      toplevel
        .window()
        .and_then(|window| state.core.window_layout(window).ok())
        .map_or(state.active().rect, |layout| layout.geometry),
    )
  }
}

/// Where surface `object` is drawn, in layout coordinates. A toplevel is
/// drawn inside its window's geometry, less its frame.
pub(crate) fn surface_geometry(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  let surface = state.surfaces.get(object)?;
  if surface.role.toplevel().is_some() {
    return window_geometry(state, object).map(|geometry| {
      crate::decorator::margins(state, object).shrink(geometry)
    });
  }
  match &surface.role {
    Role::Xwayland(role) => crate::xwayland::unmanaged::geometry(state, role),
    Role::Xdg {
      kind: Some(XdgKind::Popup(popup)),
      ..
    } => {
      let parent = surface_geometry(state, &popup.parent)?;
      Some(Rect::new(
        parent.x.saturating_add(popup.relative.x),
        parent.y.saturating_add(popup.relative.y),
        popup.relative.width,
        popup.relative.height,
      ))
    }
    Role::Subsurface {
      parent, relative, ..
    } => {
      let parent = surface_rect(state, parent)?;
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(Rect::new(
        parent.x.saturating_add(relative.0),
        parent.y.saturating_add(relative.1),
        width,
        height,
      ))
    }
    Role::Layer(_) => crate::layer_shell::geometry(state, object),
    Role::InputPanel(_) => crate::input_panel::geometry(state, object),
    Role::Cursor => {
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(Rect::new(
        state.input.pointer_x as i32 - state.input.cursor_hotspot.0,
        state.input.pointer_y as i32 - state.input.cursor_hotspot.1,
        width,
        height,
      ))
    }
    Role::DragIcon { offset } => {
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(Rect::new(
        (state.input.pointer_x as i32).saturating_add(offset.0),
        (state.input.pointer_y as i32).saturating_add(offset.1),
        width,
        height,
      ))
    }
    Role::SessionLock { output, .. } => {
      state.outputs.get(output).map(|output| output.rect)
    }
    _ => {
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(Rect::new(0, 0, width, height))
    }
  }
}

/// Where surface `object` is drawn and takes input, in layout
/// coordinates: [`surface_geometry`], except that an xdg surface with a
/// window geometry (`xdg_surface.set_window_geometry`) has that part of
/// it at its geometry and the rest, such as client-side shadows, around
/// it, as wlroots places an xdg surface by its geometry. A toplevel drawn
/// at another size than its window geometry is scaled as a whole, as
/// Shingle scales a toplevel's buffer into its view; a subsurface is
/// placed relative to its parent's drawn rectangle.
pub(crate) fn surface_rect(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  let surface = state.surfaces.get(object)?;
  match &surface.role {
    Role::Xdg { .. } => {
      let content = surface_geometry(state, object)?;
      let Some(window) = surface.window_geometry else {
        return Some(content);
      };
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(around_window_geometry(content, window, (width, height)))
    }
    Role::Subsurface {
      parent, relative, ..
    } => {
      let parent = surface_rect(state, parent)?;
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(Rect::new(
        parent.x.saturating_add(relative.0),
        parent.y.saturating_add(relative.1),
        width,
        height,
      ))
    }
    _ => surface_geometry(state, object),
  }
}

/// The rectangle of a whole `size` surface whose `window` part (in
/// surface coordinates) is shown at `content`, scaled by `content`'s size
/// over `window`'s.
pub(crate) fn around_window_geometry(
  content: Rect,
  window: Rect,
  size: (u32, u32),
) -> Rect {
  if window.width == 0 || window.height == 0 || size.0 == 0 || size.1 == 0 {
    return content;
  }
  let scale_x = f64::from(content.width) / f64::from(window.width);
  let scale_y = f64::from(content.height) / f64::from(window.height);
  let to_i32 = |value: f64| value.round().clamp(-1e9, 1e9) as i32;
  let to_u32 = |value: f64| value.round().clamp(1.0, 1e9) as u32;
  Rect::new(
    content
      .x
      .saturating_sub(to_i32(f64::from(window.x) * scale_x)),
    content
      .y
      .saturating_sub(to_i32(f64::from(window.y) * scale_y)),
    to_u32(f64::from(size.0) * scale_x),
    to_u32(f64::from(size.1) * scale_y),
  )
}

pub(crate) fn logical_buffer_dimensions(surface: &Surface) -> (u32, u32) {
  surface.buffer.as_ref().map_or((0, 0), |buffer| {
    let scale = buffer.scale.max(1) as u32;
    let (width, height) = (buffer.width / scale, buffer.height / scale);
    if matches!(
      buffer.transform,
      wayland_server::protocol::wl_output::Transform::Flipped90
        | wayland_server::protocol::wl_output::Transform::Flipped270
        | wayland_server::protocol::wl_output::Transform::_90
        | wayland_server::protocol::wl_output::Transform::_270
    ) {
      (height, width)
    } else {
      (width, height)
    }
  })
}

pub(crate) fn validate_commit(surface: &Surface) -> Result<(), ()> {
  crate::layer_shell::validate_commit(surface)?;
  match &surface.role {
    Role::Xdg {
      resource,
      initial_acked,
      ..
    } => {
      if !initial_acked
        && matches!(surface.pending.buffer, PendingBuffer::Attach(..))
      {
        resource.post_error(
          xdg_surface::Error::UnconfiguredBuffer,
          "ack the initial xdg_surface.configure before attaching a buffer",
        );
        return Err(());
      }
    }
    Role::SessionLock {
      resource, acked, ..
    } => {
      if matches!(surface.pending.buffer, PendingBuffer::Detach)
        || (surface.buffer.is_none()
          && matches!(surface.pending.buffer, PendingBuffer::Unchanged))
      {
        resource.post_error(
          ext_session_lock_surface_v1::Error::NullBuffer,
          "session lock surfaces must commit a non-null buffer",
        );
        return Err(());
      }
      let Some((_, width, height)) = *acked else {
        resource.post_error(
          ext_session_lock_surface_v1::Error::CommitBeforeFirstAck,
          "ack the initial lock surface configure before committing",
        );
        return Err(());
      };
      if let PendingBuffer::Attach(attachment, _) = &surface.pending.buffer
        && (attachment.width != width || attachment.height != height)
      {
        resource.post_error(
          ext_session_lock_surface_v1::Error::DimensionsMismatch,
          concat!(
            "lock surface buffer dimensions must match the ",
            "acknowledged configure",
          ),
        );
        return Err(());
      }
    }
    _ => {}
  }
  Ok(())
}

fn finish_map(state: &mut CompositorState, window: WindowId) {
  let Some(surface) = state.surfaces.values().find_map(|surface| {
    surface
      .role
      .toplevel()
      .is_some_and(|toplevel| toplevel.window() == Some(window))
      .then_some(surface.id)
  }) else {
    return;
  };
  if state
    .core
    .window_info(window)
    .is_ok_and(|info| info.presence == WindowPresence::Mapped)
  {
    return;
  }
  // A dialog maps in its parent's set and workspace, as Wayfire's
  // adjust_view_output_on_map puts a view on its parent's output; its
  // parent may have come after it was created.
  if let Some((wset, workspace)) = state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| info.parent)
    .and_then(|parent| state.core.window_info(parent).ok())
    .map(|info| (info.wset, info.location.workspace))
    && state
      .core
      .window_info(window)
      .is_ok_and(|info| info.wset != wset)
  {
    let _ = state.core.move_window_to_wset(window, wset, workspace);
  }
  // A held view joins its tiling tree only now, as Wayfire's tile plugin
  // attaches a view on view-mapped (plugins/tile/tile-plugin.cpp,
  // on_view_mapped).
  let _ = state.core.release_held(window);
  if state.core.mark_window_mapped(window).is_err() {
    return;
  }
  state.pending_maps.remove(&window);
  disable_show_desktop(state);
  state.events.push(Event::WindowCreated { window, surface });
  reconfigure_all(state);
  if state
    .core
    .current_workspace(&state.active_output)
    .is_ok_and(|workspace| {
      state.core.window_info(window).is_ok_and(|info| {
        info.location.output == state.active_output
          && info.location.workspace == workspace
      })
    })
    && crate::xwayland::requests::focus_on_map(state, window)
  {
    crate::input::focus_window(state, window);
  }
}

fn request_map(state: &mut CompositorState, object: &ObjectId) {
  let Some((window, has_buffer, mapped, created)) = state
    .surfaces
    .get(object)
    .and_then(|surface| match surface.role.toplevel() {
      Some(toplevel) => toplevel.window().map(|window| {
        (
          window,
          surface.buffer.is_some(),
          state
            .core
            .window_info(window)
            .is_ok_and(|info| info.presence == WindowPresence::Mapped),
          toplevel.created_rules_pending == Some(true),
        )
      }),
      _ => None,
    })
  else {
    return;
  };
  if !has_buffer || mapped {
    return;
  }
  if !state.delayed_maps.contains(&window) && !state.pre_map_clients.is_empty()
  {
    // A new view gives up its tile until it is released; a claimed
    // placeholder keeps the slot its session saved.
    if created {
      let _ = state.core.set_held(window, true);
      reconfigure_all(state);
    }
    state.delayed_maps.insert(window);
    state.pending_maps.insert(
      window,
      crate::state::PendingMap {
        waiting: state.pre_map_clients.clone(),
        notified: false,
      },
    );
  }
  let (notify, waiting) =
    if let Some(pending) = state.pending_maps.get_mut(&window) {
      let notify = !pending.notified;
      pending.notified = true;
      (notify, !pending.waiting.is_empty())
    } else {
      (false, false)
    };
  if notify {
    state.ipc_events.push(crate::ipc::view_event(
      state,
      "view-pre-map",
      window,
    ));
  }
  if waiting {
    return;
  }
  finish_map(state, window);
}

/// Releases one IPC client's delay and maps the view after the last release.
pub(crate) fn unblock_map(
  state: &mut CompositorState,
  window: WindowId,
  client: u64,
) -> Result<(), &'static str> {
  if !state.delayed_maps.contains(&window) {
    return Err("view is not being delayed for mapping");
  }
  if let Some(pending) = state.pending_maps.get_mut(&window) {
    pending.waiting.remove(&client);
    if !pending.waiting.is_empty() {
      return Ok(());
    }
  }
  finish_map(state, window);
  Ok(())
}

pub(crate) fn committed(state: &mut CompositorState, object: &ObjectId) {
  let has_buffer = state
    .surfaces
    .get(object)
    .is_some_and(|surface| surface.buffer.is_some());
  if has_buffer
    && state
      .surfaces
      .get(object)
      .and_then(|surface| surface.role.toplevel())
      .is_some_and(|toplevel| toplevel.window().is_none())
  {
    materialize_toplevel(state, object);
  }
  if has_buffer {
    request_map(state, object);
  } else if !unmap_toplevel(state, object) {
    reconfigure_remapping(state, object);
  }
  let pending = state.surfaces.get_mut(object).and_then(|surface| {
    surface.buffer.as_ref()?;
    match surface.role.toplevel_mut() {
      Some(toplevel) => {
        toplevel.created_rules_pending.take().and_then(|created| {
          toplevel
            .window()
            .map(|window| (created, window, toplevel.identity.clone()))
        })
      }
      _ => None,
    }
  });
  if let Some((created, window, identity)) = pending {
    if created {
      let properties =
        crate::window_rules::view_properties(state, window, &identity);
      // Without simple-tile nothing tiles, as in Wayfire.
      if !state.tiling || !state.tile_by_default.matches_view(&properties) {
        let _ = toggle_tiling(state, window);
      } else if !crate::xwayland::requests::can_tile(state, object) {
        crate::xwayland::requests::float(state, object, window);
      } else if !can_tile_xdg(state, object) {
        // Out of the tree at its buffer's size; placement places it.
        let size = buffer_rect(state, object);
        let _ = set_floating_geometry(state, window, size);
      } else {
        // Tiled, after it mapped at its own size: untiling gives back that
        // size, placed as it would have floated.
        if let Some(windowed) = crate::placement::windowed_on_map(state, window)
        {
          set_windowed_geometry(state, window, windowed);
        }
      }
      crate::placement::place(state, window);
      crate::window_rules::apply_created(state, window, &identity);
      crate::xwayland::requests::mapped(state, object, window);
    } else if !state.tiling {
      // A restored window claims its saved tile.
      float_if_tiled(state, window);
    }
    reconfigure_all(state);
    if state
      .core
      .current_workspace(&state.active_output)
      .is_ok_and(|workspace| {
        state.core.window_info(window).is_ok_and(|info| {
          info.presence == WindowPresence::Mapped
            && info.location.output == state.active_output
            && info.location.workspace == workspace
        })
      })
      && crate::xwayland::requests::focus_on_map(state, window)
    {
      crate::input::focus_window(state, window);
    }
  }
  crate::layer_shell::committed(state, object);
  if !matches!(
    state.surfaces.get(object).map(|surface| &surface.role),
    Some(Role::SessionLock { .. })
  ) {
    return;
  }
  lock_surface_committed(state, object);
  if state.idle.lock_state() == LockState::Pending {
    finish_lock(state);
  }
}

/// Marks lock surface `object` shown once it commits a full frame, a
/// buffer of its output's size that covers it opaquely; its output draws
/// it from then on, and black before.
fn lock_surface_committed(state: &mut CompositorState, object: &ObjectId) {
  let Some(surface) = state.surfaces.get(object) else {
    return;
  };
  let Role::SessionLock {
    output,
    shown: false,
    ..
  } = &surface.role
  else {
    return;
  };
  let full = state.outputs.get(output).is_some_and(|output| {
    surface.buffer.as_ref().is_some_and(|buffer| {
      buffer.width == output.rect.width
        && buffer.height == output.rect.height
        && covers_opaquely(buffer)
    })
  });
  if full
    && let Some(Role::SessionLock { shown, .. }) = state
      .surfaces
      .get_mut(object)
      .map(|surface| &mut surface.role)
  {
    *shown = true;
  }
}

/// Whether surface `object` is a lock surface of the active lock that
/// has shown a full frame on output `output`: the only surface an
/// output draws while [`LockState::hides_clients`] holds.
pub(crate) fn shown_lock_surface(
  state: &CompositorState,
  surface: &crate::state::Surface,
  output: &OutputId,
) -> bool {
  let active_client = state
    .active_lock
    .as_ref()
    .and_then(|lock| lock.client())
    .map(|client| client.id());
  active_client.is_some_and(|client| surface.client == client)
    && surface.buffer.is_some()
    && matches!(
      &surface.role,
      Role::SessionLock { output: on, shown: true, .. } if on == output
    )
}

/// Sends `locked` once the pending lock's client covers every output that
/// is not dark with a lock surface of its size whose buffer hides what is
/// under it (ext-session-lock-v1; docs/design/multi-output.md, section
/// 8). Outputs added later show the background until their surface comes.
pub(crate) fn finish_lock(state: &mut CompositorState) {
  if state.idle.lock_state() != LockState::Pending {
    return;
  }
  let Some(active) = state.active_lock.clone() else {
    return;
  };
  let Some(active_client) = active.client().map(|client| client.id()) else {
    return;
  };
  let ready =
    state
      .outputs
      .iter()
      .filter(|output| !output.dark)
      .all(|output| {
        state.surfaces.values().any(|surface| {
          surface.client == active_client
            && matches!(
              &surface.role,
              Role::SessionLock { output: on, .. } if *on == output.id
            )
            && surface.buffer.as_ref().is_some_and(|buffer| {
              buffer.width == output.rect.width
                && buffer.height == output.rect.height
                && covers_opaquely(buffer)
            })
        })
      });
  if ready {
    state.idle.lock_surface_ready(state.now());
    active.locked();
    state.emit_idle_changes();
    crate::input::sync_focus(state);
  }
}

/// Makes the lock surfaces on output `id`, which is going, inert: they
/// are no longer drawn or configured, and the session stays locked.
pub(crate) fn lock_output_removed(state: &mut CompositorState, id: &OutputId) {
  for surface in state.surfaces.values_mut() {
    if matches!(&surface.role, Role::SessionLock { output, .. } if output == id)
    {
      surface.role = Role::Dead;
      surface.buffer = None;
    }
  }
}

/// Moves the lock surfaces of output `old` to `new`, its new name.
pub(crate) fn lock_output_renamed(
  state: &mut CompositorState,
  old: &OutputId,
  new: &OutputId,
) {
  for surface in state.surfaces.values_mut() {
    if let Role::SessionLock { output, .. } = &mut surface.role
      && output == old
    {
      *output = new.clone();
    }
  }
}

/// Whether a lock surface's `buffer`, which covers the output, hides
/// everything under it.
fn covers_opaquely(buffer: &crate::BufferSnapshot) -> bool {
  match &buffer.content {
    crate::SnapshotContent::Pixels(rgba) => opaque(rgba),
    // Still locked here, in the commit that showed it: an opaque format
    // is ready, any other is read now.
    crate::SnapshotContent::Shm(content) => {
      content.is_opaque() || content.rgba().is_some_and(|rgba| opaque(&rgba))
    }
    // The GPU content cannot be read on the CPU, so a DMA-BUF covering
    // the output counts as ready, whatever its format.
    crate::SnapshotContent::Dmabuf(_) => true,
  }
}

/// Whether every pixel of tightly packed RGBA rows is opaque.
fn opaque(rgba: &[u8]) -> bool {
  rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255)
}

/// Unmaps a toplevel that committed a null buffer while mapped or waiting
/// to map, as xdg-shell defines it: the window leaves the model as if it had
/// been destroyed, emitting [`Event::WindowDestroyed`] if it was mapped,
/// and the xdg_surface returns to its unconfigured state, so a remap starts
/// with an initial commit and maps as a new window. Returns whether it
/// unmapped. Wayfire likewise
/// detaches an unmapped view from its tiling tree and attaches it again on
/// the next map (plugins/tile/tile-plugin.cpp, `on_view_unmapped` and
/// `on_view_mapped`).
fn unmap_toplevel(state: &mut CompositorState, object: &ObjectId) -> bool {
  let Some((id, role, window)) =
    state.surfaces.get(object).and_then(|surface| {
      let window = surface.role.toplevel()?.window()?;
      Some((surface.id, surface.role.clone(), window))
    })
  else {
    return false;
  };
  let mapped = state
    .core
    .window_info(window)
    .is_ok_and(|info| info.presence == WindowPresence::Mapped);
  if !mapped && !state.pending_maps.contains_key(&window) {
    return false;
  }
  // Mapped again, it opens where it was, as a Wayfire view keeps its
  // output while it is unmapped.
  let output = state
    .core
    .window_location(window)
    .ok()
    .map(|location| location.output);
  if let Some(surface) = state.surfaces.get_mut(object) {
    match &mut surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        serials,
        initial_acked,
        ..
      } => {
        toplevel.window = WindowId(0);
        toplevel.tiled = true;
        toplevel.configured_geometry = None;
        toplevel.configure = ConfigureState::default();
        toplevel.created_rules_pending = None;
        toplevel.unmapped = true;
        if let Some(output) = output {
          toplevel.created_output = output;
        }
        serials.clear();
        *initial_acked = false;
      }
      // Xwayland does not commit null buffers, but if it did, the next
      // buffer maps the window again as a new one.
      Role::Xwayland(role) => {
        if let Some(toplevel) = &mut role.toplevel {
          toplevel.window = WindowId(0);
          toplevel.tiled = true;
          toplevel.configured_geometry = None;
          toplevel.created_rules_pending = None;
        }
      }
      _ => {}
    }
  }
  remove_window(state, id, &role);
  crate::input::sync_focus(state);
  true
}

/// Answers the initial commit of a toplevel that a null buffer unmapped with
/// a configure, as wlroots does for a reset xdg_surface.
fn reconfigure_remapping(state: &mut CompositorState, object: &ObjectId) {
  let remapping = state
    .surfaces
    .get_mut(object)
    .and_then(|surface| surface.role.toplevel_mut())
    .is_some_and(|toplevel| std::mem::take(&mut toplevel.unmapped));
  if remapping {
    reconfigure_all(state);
    request_configure(state, object);
  }
}

pub(crate) fn destroy_role(state: &mut CompositorState, surface: &Surface) {
  crate::foreign::surface_destroyed(state, &Resource::id(&surface.resource));
  crate::xwayland::windows::surface_destroyed(state, surface);
  detach_toplevel(state, surface.id, &surface.role);
}

fn destroy_role_at(state: &mut CompositorState, object: &ObjectId) {
  crate::foreign::surface_destroyed(state, object);
  let Some(surface) = state.surfaces.get(object) else {
    return;
  };
  let id = surface.id;
  let role = surface.role.clone();
  detach_toplevel(state, id, &role);
}

/// Removes a toplevel's window when its role goes away.
pub(crate) fn detach_toplevel(
  state: &mut CompositorState,
  surface: SurfaceId,
  role: &Role,
) {
  if let Some(toplevel) = role.toplevel() {
    if let Some(xdg_toplevel) = toplevel.handle.xdg() {
      let id = Resource::id(xdg_toplevel);
      crate::dialog::toplevel_destroyed(state, &id);
      crate::toplevel_icon::toplevel_destroyed(state, &id);
    }
    remove_window(state, surface, role);
  }
}

/// Removes the window of a toplevel role from the model, emitting
/// [`Event::WindowDestroyed`] if it was mapped.
fn remove_window(state: &mut CompositorState, surface: SurfaceId, role: &Role) {
  let Some(window) = role.toplevel().and_then(|toplevel| toplevel.window())
  else {
    return;
  };
  let mapped = state
    .core
    .window_info(window)
    .is_ok_and(|info| info.presence == WindowPresence::Mapped);
  if state.core.detach_window(window).is_ok() {
    state.pending_maps.remove(&window);
    state.delayed_maps.remove(&window);
    state.focus_recency.remove(&window);
    if let Some(properties) = state.view_properties.remove(&window) {
      state.departing_view_properties.insert(window, properties);
    }
    if mapped {
      state
        .events
        .push(Event::WindowDestroyed { window, surface });
    }
    reconfigure_all(state);
  }
}

/// Updates both the XDG role and persistent core parent relationship.
pub(crate) fn set_toplevel_parent(
  state: &mut CompositorState,
  object: &ObjectId,
  parent: Option<WindowId>,
) {
  let had_parent = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
    .is_some_and(|toplevel| toplevel.parent.is_some());
  if let Some(surface) = state.surfaces.get_mut(object)
    && let Some(toplevel) = surface.role.toplevel_mut()
  {
    toplevel.parent = parent;
  }
  let window = state.surfaces.get(object).and_then(|surface| {
    surface
      .role
      .toplevel()
      .and_then(|toplevel| toplevel.window())
  });
  let Some(window) = window else {
    return;
  };
  let _ = state.core.set_window_parent(window, parent);
  // A tiled xdg toplevel that becomes a dialog leaves its tile and is
  // centred on its parent, as Wayfire takes a view with a parent out of
  // its workspace set and so out of simple-tile.
  let tiled = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
    .is_some_and(|toplevel| {
      toplevel.tiled && matches!(toplevel.handle, ToplevelHandle::Xdg(_))
    });
  let has_buffer = state
    .surfaces
    .get(object)
    .is_some_and(|surface| surface.buffer.is_some());
  if parent.is_some() && !had_parent && tiled && has_buffer {
    let size = buffer_rect(state, object);
    let _ = set_floating_geometry(state, window, size);
    crate::placement::place(state, window);
  } else if parent.is_some() && !has_buffer && tiled {
    // Not drawn yet: out of the tree at once, so that its parent keeps its
    // tile, and its next configure leaves it its own size.
    let _ = set_floating_geometry(state, window, Rect::new(0, 0, 1, 1));
  }
}

/// Whether `object` is a toplevel without a buffer yet whose configures
/// leave the size to the client: one that has not mapped yet, as a new
/// window is held out of its tree until it maps, or one that will float
/// when it maps ([`can_tile_xdg`]). Wayfire's initial configure is the
/// state committed before the view mapped, 0x0 unless a request changed
/// it, and simple-tile only tiles a view on `view-mapped`
/// (wayfire:src/view/xdg-shell/xdg-toplevel.cpp, `handle_surface_commit`;
/// plugins/tile/tile-plugin.cpp, `on_view_mapped`).
fn chooses_own_size(state: &CompositorState, object: &ObjectId) -> bool {
  let Some(surface) = state.surfaces.get(object) else {
    return false;
  };
  if surface.buffer.is_some() {
    return false;
  }
  let unmapped = surface
    .role
    .toplevel()
    .is_some_and(|toplevel| match toplevel.window() {
      None => true,
      Some(window) => state.core.window_info(window).is_ok_and(|info| {
        info.state.held && info.presence != WindowPresence::Mapped
      }),
    });
  unmapped || !can_tile_xdg(state, object)
}

/// Wayfire's `can_tile_view` for xdg toplevels: a dialog (a toplevel with
/// a parent) or a window of fixed size (equal positive minimum and
/// maximum) floats (wayfire:plugins/tile/tile-plugin.cpp). X toplevels
/// have their own rule, `xwayland::requests::can_tile`.
pub(crate) fn can_tile_xdg(state: &CompositorState, object: &ObjectId) -> bool {
  let Some(toplevel) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
  else {
    return true;
  };
  if !matches!(toplevel.handle, ToplevelHandle::Xdg(_)) {
    return true;
  }
  let (min, max) = (toplevel.min_size, toplevel.max_size);
  let fixed = min == max && min.0 > 0 && min.1 > 0;
  toplevel.parent.is_none() && !fixed
}

/// The window shown as active: the focused one, else the one keeping the
/// activated state while an X menu or a layer surface has the keyboard.
pub(crate) fn activated_window(state: &CompositorState) -> Option<WindowId> {
  if let Some(window) = crate::input::focused_window(state) {
    return Some(window);
  }
  // A Wayfire toplevel stays activated while one of its X menus has the
  // keyboard.
  if crate::xwayland::unmanaged::has_keyboard_focus(state) {
    let workspace = state.core.current_workspace(&state.active_output).ok()?;
    return state
      .core
      .tree(&state.active_output, workspace)
      .ok()?
      .focused();
  }
  if !state.keep_last_toplevel_activated {
    return None;
  }
  let object = state.input.keyboard_focus.as_ref()?;
  if !matches!(
    state.surfaces.get(object).map(|surface| &surface.role),
    Some(Role::Layer(_))
  ) {
    return None;
  }
  let workspace = state.core.current_workspace(&state.active_output).ok()?;
  state
    .core
    .tree(&state.active_output, workspace)
    .ok()?
    .focused()
}

pub(crate) fn reconfigure_all(state: &mut CompositorState) {
  let mut targets = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      let toplevel = surface.role.toplevel()?;
      Some((object.clone(), toplevel.window()))
    })
    .collect::<Vec<_>>();
  targets.sort_by_key(|(_, window)| window.unwrap_or(WindowId(u64::MAX)));
  for (object, _) in &targets {
    crate::decorator::refresh(state, object);
  }
  for (object, window) in targets {
    let layout =
      window.and_then(|window| state.core.window_layout(window).ok());
    let focused =
      window.is_some_and(|window| activated_window(state) == Some(window));
    let fullscreen = window.is_some_and(|window| {
      state
        .core
        .window_info(window)
        .is_ok_and(|info| info.state.fullscreen)
    });
    let maximized = window.is_some_and(|window| {
      state
        .core
        .window_info(window)
        .is_ok_and(|info| info.state.maximized)
    });
    let (tiled, floating_geometry) = state
      .surfaces
      .get(&object)
      .and_then(|surface| surface.role.toplevel())
      .map(|toplevel| (toplevel.tiled, toplevel.floating_geometry))
      .unwrap_or((true, state.active().rect));
    let geometry = if fullscreen {
      window.map_or(state.active().rect, |window| {
        state.output_of_window(window).rect
      })
    } else if chooses_own_size(state, &object) {
      // Wayfire's initial configure is 0x0 and only a tiled view is
      // resized at map; a dialog keeps the size it picks.
      Rect::new(0, 0, 0, 0)
    } else if tiled {
      layout.map_or(state.active().rect, |layout| layout.geometry)
    } else if maximized {
      window
        .and_then(|window| state.core.window_location(window).ok())
        .and_then(|location| {
          state.core.tree(&location.output, location.workspace).ok()
        })
        .map_or(state.active().rect, |tree| tree.workarea())
    } else {
      floating_geometry
    };
    let tiled = tiled && !chooses_own_size(state, &object);
    let states = ToplevelStates {
      tiled,
      activated: focused,
      maximized,
      fullscreen,
    };
    configure_toplevel(state, &object, geometry, states);
  }
  sync_fullscreen_idle(state);
}

fn sync_fullscreen_idle(state: &mut CompositorState) {
  let is_visible_fullscreen = |window: WindowId| {
    let Ok(workspace) = state.core.current_workspace(&state.active_output)
    else {
      return false;
    };
    state.core.window_info(window).is_ok_and(|info| {
      info.state.fullscreen
        && !info.state.minimized
        && info.location.output == state.active_output
        && (info.state.sticky || info.location.workspace == workspace)
    })
  };
  let fullscreen = !state.show_desktop
    && state.surfaces.values().any(|surface| {
      surface.buffer.is_some()
        && surface.role.toplevel().is_some_and(|toplevel| {
          toplevel.window().is_some_and(is_visible_fullscreen)
        })
    });
  let commands = state.idle.set_fullscreen(fullscreen, state.now());
  state.process_idle_commands(commands);
}

/// Sends a toplevel its geometry and states in its own shell's terms, and
/// reports a changed geometry.
fn configure_toplevel(
  state: &mut CompositorState,
  object: &ObjectId,
  geometry: Rect,
  states: ToplevelStates,
) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Some(toplevel) = surface.role.toplevel_mut() else {
    return;
  };
  let previous = toplevel.configured_geometry.replace(geometry);
  let window = toplevel.window();
  let handle = toplevel.handle.clone();
  // The client gets the window's geometry less its frame; a size of 0
  // leaves it to the client.
  let content = if geometry.width == 0 || geometry.height == 0 {
    geometry
  } else {
    crate::decorator::margins(state, object).shrink(geometry)
  };
  match handle {
    ToplevelHandle::X11(x_window) => {
      // No acknowledgement: X11 has no configure serials.
      crate::xwayland::windows::configure(
        state, x_window, window, content, states,
      );
    }
    ToplevelHandle::Xdg(_) => {
      // Sent at the end of the turn by flush_configures.
      if let Some(toplevel) = state
        .surfaces
        .get_mut(object)
        .and_then(|surface| surface.role.toplevel_mut())
      {
        toplevel.configure.pending = Some((
          content.width.min(i32::MAX as u32) as i32,
          content.height.min(i32::MAX as u32) as i32,
          states,
        ));
      }
      state.configures_pending = true;
    }
  }
  if let (Some(window), Some(old_geometry)) = (window, previous)
    && old_geometry != geometry
  {
    state.events.push(Event::GeometryChanged {
      window,
      old_geometry,
    });
  }
}

/// Sends the xdg toplevel configures of this turn ([`ConfigureState`]):
/// one per toplevel whose size or states changed since its last
/// configure, whose client asked for one, or whose decoration mode
/// waits for one.
pub(crate) fn flush_configures(state: &mut CompositorState) {
  if !std::mem::take(&mut state.configures_pending) {
    return;
  }
  let mut objects = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      let toplevel = surface.role.toplevel()?;
      toplevel.configure.pending?;
      Some((object.clone(), toplevel.window()))
    })
    .collect::<Vec<_>>();
  // In window order, as reconfigure_all configures them.
  objects.sort_by_key(|(object, window)| {
    (window.unwrap_or(WindowId(u64::MAX)), object.protocol_id())
  });
  for (object, _) in objects {
    let awaiting = crate::decoration::awaiting_configure(state, &object);
    let serial = state.serial();
    let Some(surface) = state.surfaces.get_mut(&object) else {
      continue;
    };
    let Role::Xdg {
      resource,
      serials,
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
    else {
      continue;
    };
    let ToplevelHandle::Xdg(xdg_toplevel) = toplevel.handle.clone() else {
      continue;
    };
    let configure = &mut toplevel.configure;
    let Some(pending) = configure.pending.take() else {
      continue;
    };
    if configure.sent == Some(pending) && !configure.requested && !awaiting {
      continue;
    }
    configure.sent = Some(pending);
    configure.requested = false;
    let (width, height, states) = pending;
    xdg_toplevel.configure(width, height, states.xdg_states());
    // The xdg_surface half of the configure, which the client
    // acknowledges with this serial.
    resource.configure(serial);
    serials.insert(serial);
    crate::decoration::configured(state, &xdg_toplevel, serial);
  }
}

/// Makes the next [`flush_configures`] send toplevel `object` a configure
/// even if nothing about it changed, as xdg-shell's state requests expect.
pub(crate) fn request_configure(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  let Some(toplevel) = state
    .surfaces
    .get_mut(object)
    .and_then(|surface| surface.role.toplevel_mut())
  else {
    return;
  };
  if !matches!(toplevel.handle, ToplevelHandle::Xdg(_)) {
    return;
  }
  toplevel.configure.requested = true;
  if toplevel.configure.pending.is_none() {
    toplevel.configure.pending = toplevel.configure.sent;
  }
  state.configures_pending = true;
}

pub(crate) fn reconfigure_lock_surfaces(state: &mut CompositorState) {
  let objects = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      matches!(surface.role, Role::SessionLock { .. }).then_some(object.clone())
    })
    .collect::<Vec<_>>();
  for object in objects {
    configure_lock_surface(state, &object);
  }
}

fn configure_lock_surface(state: &mut CompositorState, object: &ObjectId) {
  let Some(Role::SessionLock { output, .. }) =
    state.surfaces.get(object).map(|surface| &surface.role)
  else {
    return;
  };
  let Some(rect) = state.outputs.get(output).map(|output| output.rect) else {
    return;
  };
  let serial = state.serial();
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::SessionLock {
    resource, serials, ..
  } = &mut surface.role
  else {
    return;
  };
  resource.configure(serial, rect.width, rect.height);
  serials.insert(serial, (rect.width, rect.height));
}

fn update_app_id(
  state: &mut CompositorState,
  object: &ObjectId,
  app_id: String,
) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Some(toplevel) = surface.role.toplevel_mut() else {
    return;
  };
  toplevel.app_id = app_id.clone();
  toplevel.identity.app_id = app_id.clone();
  let surface = surface.id;
  let window = materialize_toplevel(state, object);
  if let Some(window) = window {
    let _ = state.core.set_window_app_id(window, app_id.clone());
    state.events.push(Event::AppIdChanged {
      window,
      surface,
      app_id,
    });
  }
}

/// Sets a toplevel's title, and its window's once it has one.
pub(crate) fn update_title(
  state: &mut CompositorState,
  object: &ObjectId,
  title: String,
) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Some(toplevel) = surface.role.toplevel_mut() else {
    return;
  };
  toplevel.title = title.clone();
  toplevel.identity.title = Some(title.clone());
  // A title alone must not create the window: clients often send it before
  // their app ID, and a window created from the launch template's app ID
  // never claims the restored placeholder saved under the real one. The
  // title is kept in the identity and applied when the window is created.
  let window = toplevel.window();
  let surface = surface.id;
  if let Some(window) = window {
    let _ = state.core.set_window_title(window, Some(title.clone()));
    state.events.push(Event::TitleChanged {
      window,
      surface,
      title,
    });
  }
}

pub(crate) fn materialize_toplevel(
  state: &mut CompositorState,
  object: &ObjectId,
) -> Option<WindowId> {
  let (existing, identity, parent, created_output) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
    .map(|toplevel| {
      (
        toplevel.window(),
        toplevel.identity.clone(),
        toplevel.parent,
        toplevel.created_output.clone(),
      )
    })?;
  if let Some(window) = existing {
    return Some(window);
  }
  let (window, created) = match state.core.reconcile_mapped(&identity) {
    Some(window) => (window, false),
    None => {
      // A dialog maps in its parent's set and workspace, anything else on
      // the current workspace of the output that was active when its
      // toplevel was created, as Wayfire's adjust_view_output_on_map
      // chooses (src/view/view-impl.cpp); the active one if that output
      // has gone since.
      let parent_place = parent
        .and_then(|parent| state.core.window_info(parent).ok())
        .map(|info| (info.wset, info.location.workspace));
      let output = if state.core.wset_of(&created_output).is_some() {
        created_output
      } else {
        state.active_output.clone()
      };
      let (wset, workspace) = match parent_place {
        Some(place) => place,
        None => (
          state.core.wset_of(&output)?,
          state.core.current_workspace(&output).ok()?,
        ),
      };
      let window = state
        .core
        .attach_window_to_wset(
          wset,
          workspace,
          identity.clone(),
          LeafState {
            gaps: state.gaps,
            maximized: false,
            minimized: false,
            stacking: shingle_core::StackingMode::Normal,
            floating: None,
            fullscreen: false,
            sticky: false,
            snapped: None,
            restore_geometry: None,
            tiled_edges: 15,
            // A new view takes no tile before its first map, as
            // simple-tile attaches a view only on `view-mapped`
            // (wayfire:plugins/tile/tile-plugin.cpp, `on_view_mapped`);
            // until then its configures leave its size to the client
            // (`chooses_own_size`), and `finish_map` releases it.
            held: true,
          },
          parent,
        )
        .ok()?;
      (window, true)
    }
  };
  state.core.reconcile_unmapped(window).ok()?;
  // A claimed placeholder still has the saved title; the client may have
  // sent a newer one before the window existed.
  if !created && identity.title.is_some() {
    let _ = state.core.set_window_title(window, identity.title.clone());
  }
  let restored_state =
    state.core.window_info(window).ok().map(|info| info.state);
  let restored_parent = state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| info.parent);
  if let Some(surface) = state.surfaces.get_mut(object)
    && let Some(toplevel) = surface.role.toplevel_mut()
  {
    toplevel.window = window;
    toplevel.parent = restored_parent;
    toplevel.created_rules_pending = Some(created);
    if let Some(leaf) = restored_state {
      toplevel.tiled = leaf.floating.is_none();
      if let Some(geometry) = leaf.floating {
        toplevel.floating_geometry = geometry;
      }
    }
  }
  // A dialog or a window of fixed size never takes a tile, not even
  // before it is drawn (see `chooses_own_size`).
  if created && !can_tile_xdg(state, object) {
    let _ = set_floating_geometry(state, window, Rect::new(0, 0, 1, 1));
  }
  reconfigure_all(state);
  request_map(state, object);
  Some(window)
}

/// Asks the client of `window`'s toplevel to close it, in its own shell's
/// terms. Returns `None` if the window has no toplevel. The client decides
/// whether and when to close, so the window is still there afterwards.
pub(crate) fn close_window(
  state: &mut CompositorState,
  window: WindowId,
) -> Option<()> {
  let handle = state.surfaces.values().find_map(|surface| {
    surface
      .role
      .toplevel()
      .filter(|toplevel| toplevel.window() == Some(window))
      .map(|toplevel| toplevel.handle.clone())
  })?;
  match handle {
    ToplevelHandle::Xdg(toplevel) => toplevel.close(),
    // WM_DELETE_WINDOW, or KillClient for clients without it.
    ToplevelHandle::X11(x_window) => {
      crate::xwayland::windows::close(state, x_window);
    }
  }
  Some(())
}

/// The minimum and maximum size of `window`'s geometry, 0 for none: its
/// client's, grown by a server-side frame, as Wayfire's toplevel grows
/// them by its margins.
pub(crate) fn size_constraints(
  state: &CompositorState,
  window: WindowId,
) -> Option<((i32, i32), (i32, i32))> {
  let (object, min, max) =
    state.surfaces.iter().find_map(|(object, surface)| {
      match surface.role.toplevel() {
        Some(toplevel) if toplevel.window() == Some(window) => {
          Some((object.clone(), toplevel.min_size, toplevel.max_size))
        }
        _ => None,
      }
    })?;
  let margins = crate::decorator::margins(state, &object);
  let grow = |(width, height): (i32, i32)| {
    let horizontal = (margins.left + margins.right) as i32;
    let vertical = (margins.top + margins.bottom) as i32;
    (
      if width > 0 {
        width.saturating_add(horizontal)
      } else {
        width
      },
      if height > 0 {
        height.saturating_add(vertical)
      } else {
        height
      },
    )
  };
  Some((grow(min), grow(max)))
}

pub(crate) fn leaf_state(
  state: &CompositorState,
  window: WindowId,
) -> Option<LeafState> {
  state.core.window_info(window).ok().map(|info| info.state)
}

pub(crate) fn is_minimized(state: &CompositorState, window: WindowId) -> bool {
  leaf_state(state, window).is_some_and(|state| state.minimized)
}

pub(crate) fn enable_show_desktop(state: &mut CompositorState) {
  if state.show_desktop {
    return;
  }
  state.show_desktop = true;
  state.show_desktop_focus = crate::input::focused_window(state);
  let windows = state
    .core
    .current_workspace(&state.active_output)
    .ok()
    .and_then(|workspace| state.core.tree(&state.active_output, workspace).ok())
    .map(|tree| tree.stack_order().to_vec())
    .unwrap_or_default();
  for window in windows {
    if !is_minimized(state, window) {
      state.show_desktop_windows.insert(window);
      let _ = set_minimized(state, window, true);
    }
  }
  crate::input::sync_focus(state);
  state.events.push(Event::ShowDesktopChanged(true));
}

pub(crate) fn disable_show_desktop(state: &mut CompositorState) {
  if !state.show_desktop {
    return;
  }
  state.show_desktop = false;
  let windows = std::mem::take(&mut state.show_desktop_windows);
  let focus = state.show_desktop_focus.take();
  for window in windows {
    if is_minimized(state, window) {
      let _ = set_minimized(state, window, false);
    }
  }
  if let Some(window) = focus
    && !is_minimized(state, window)
  {
    crate::input::focus_window(state, window);
  }
  crate::input::sync_focus(state);
  state.events.push(Event::ShowDesktopChanged(false));
}

pub(crate) fn set_minimized(
  state: &mut CompositorState,
  window: WindowId,
  minimized: bool,
) -> Result<(), crate::Error> {
  let changed = state.core.window_info(window)?.state.minimized != minimized;
  state.core.set_minimized(window, minimized)?;
  if !minimized && state.show_desktop {
    disable_show_desktop(state);
  }
  reconfigure_all(state);
  if minimized {
    crate::input::sync_focus(state);
  } else {
    crate::input::focus_window(state, window);
  }
  if !changed {
    return Ok(());
  }
  state
    .events
    .push(Event::MinimizedChanged { window, minimized });
  if minimized {
    crate::window_rules::apply_event(state, window, RuleEvent::Minimized);
  }
  Ok(())
}

pub(crate) fn set_maximized(
  state: &mut CompositorState,
  window: WindowId,
  maximized: bool,
) -> Result<(), crate::Error> {
  let changed = state.core.window_info(window)?.state.maximized != maximized;
  state.core.set_maximized(window, maximized)?;
  reconfigure_all(state);
  if !changed {
    return Ok(());
  }
  state
    .events
    .push(Event::MaximizedChanged { window, maximized });
  crate::window_rules::apply_event(
    state,
    window,
    if maximized {
      RuleEvent::Maximized
    } else {
      RuleEvent::Unmaximized
    },
  );
  Ok(())
}

pub(crate) fn set_fullscreen(
  state: &mut CompositorState,
  window: WindowId,
  fullscreen: bool,
) -> Result<(), crate::Error> {
  let changed = state.core.window_info(window)?.state.fullscreen != fullscreen;
  state.core.set_fullscreen(window, fullscreen)?;
  reconfigure_all(state);
  if !changed {
    return Ok(());
  }
  state
    .events
    .push(Event::FullscreenChanged { window, fullscreen });
  if fullscreen {
    crate::window_rules::apply_event(state, window, RuleEvent::Fullscreened);
  }
  Ok(())
}

/// Moves `window` to the output of `wl_output` resource `output` before
/// it is made fullscreen there, as Wayfire's `fullscreen_request` moves a
/// view to the output a client names (src/core/window-manager.cpp).
/// Dialogs move with their parent, and an output that is gone is ignored.
pub(crate) fn fullscreen_on(
  state: &mut CompositorState,
  window: WindowId,
  output: &wayland_server::protocol::wl_output::WlOutput,
) {
  let Some(id) = state
    .outputs
    .of_resource(output)
    .map(|output| output.id.clone())
  else {
    return;
  };
  let top = crate::output::topmost_parent(state, window);
  if let Err(error) = crate::output::move_window_to_output(
    state,
    top,
    &id,
    crate::output::MoveGeometry::Scale,
  ) {
    eprintln!("shingle: fullscreen on {id:?} failed: {error}");
  }
}

pub(crate) fn set_stacking(
  state: &mut CompositorState,
  window: WindowId,
  mode: shingle_core::StackingMode,
) -> Result<(), crate::Error> {
  let previous = state.core.window_info(window)?.state.stacking;
  if previous == mode {
    return Ok(());
  }
  state.core.set_stacking(window, mode)?;
  state.events.push(Event::StackingChanged {
    window,
    previous,
    current: mode,
  });
  Ok(())
}

/// Sets `window`'s sticky state. A floating or snapped window that stops
/// being sticky stays where it is drawn, on its set's current workspace:
/// Wayfire never moves a sticky view on a switch
/// (wayfire:src/output/workspace-impl.cpp:612), so it is in the current
/// workspace's part of the plane, and `set_sticky` leaves its geometry
/// alone (wayfire:src/view/toplevel-view.cpp:258-276) and sends no
/// `view-workspace-changed`. A tiled window keeps its tile on its own
/// workspace, as simple-tile keeps the view in that workspace's tree
/// (wayfire:plugins/tile/tree.cpp:433-437 only draws it on the current
/// one while it is sticky).
pub(crate) fn set_sticky(
  state: &mut CompositorState,
  window: WindowId,
  sticky: bool,
) -> Result<(), crate::Error> {
  let info = state.core.window_info(window)?;
  let changed = info.state.sticky != sticky;
  state.core.set_sticky(window, sticky)?;
  if changed && !sticky {
    unstick_to_current(state, window, &info)?;
  }
  reconfigure_all(state);
  if changed {
    state.events.push(Event::StickyChanged { window, sticky });
  }
  Ok(())
}

/// Moves a floating or snapped `window` that has just stopped being sticky
/// to its set's current workspace, unreported ([`set_sticky`]).
fn unstick_to_current(
  state: &mut CompositorState,
  window: WindowId,
  info: &shingle_core::WindowInfo,
) -> Result<(), crate::Error> {
  if info.state.floating.is_none() && info.state.snapped.is_none() {
    return Ok(());
  }
  let wset = state.core.wset(info.wset)?;
  if wset.output.is_none() {
    return Ok(());
  }
  let current = wset.current_workspace;
  if current == info.location.workspace {
    return Ok(());
  }
  let index =
    current.y as usize * state.workspace_width as usize + current.x as usize;
  state.move_window_to_workspace_as(window, index, false)
}

pub(crate) fn window_alpha(
  state: &CompositorState,
  window: WindowId,
) -> Option<f32> {
  state
    .surfaces
    .values()
    .find_map(|surface| match surface.role.toplevel() {
      Some(toplevel) if toplevel.window() == Some(window) => {
        Some(toplevel.opacity)
      }
      _ => None,
    })
}

pub(crate) fn clamp_alpha(state: &mut CompositorState, minimum: f32) {
  let windows = state
    .surfaces
    .values()
    .filter_map(|surface| match surface.role.toplevel() {
      Some(toplevel) => {
        toplevel.window().filter(|_| toplevel.opacity < minimum)
      }
      _ => None,
    })
    .collect::<Vec<_>>();
  for window in windows {
    set_alpha(state, window, minimum);
  }
}

pub(crate) fn set_alpha(
  state: &mut CompositorState,
  window: WindowId,
  alpha: f32,
) {
  let opacity = alpha.clamp(0.0, 1.0);
  let mut changed = false;
  for surface in state.surfaces.values_mut() {
    if let Some(toplevel) = surface.role.toplevel_mut()
      && toplevel.window() == Some(window)
      && toplevel.opacity != opacity
    {
      toplevel.opacity = opacity;
      changed = true;
    }
  }
  if changed {
    state.events.push(Event::OpacityChanged { window, opacity });
  }
}

pub(crate) fn snap_window(
  state: &mut CompositorState,
  window: WindowId,
  slot: &str,
) -> Result<bool, crate::Error> {
  let workarea = state
    .core
    .current_workspace(&state.active_output)
    .ok()
    .and_then(|workspace| state.core.tree(&state.active_output, workspace).ok())
    .map_or(state.active().rect, |tree| tree.workarea());
  let Some(geometry) = snap_geometry(workarea, slot) else {
    return Ok(false);
  };
  let edges = match slot {
    "left" => 1,
    "right" => 2,
    "top" => 4,
    "bottom" => 8,
    "top_left" => 5,
    "top_right" => 6,
    "bottom_left" => 9,
    "bottom_right" => 10,
    _ => 15,
  };
  snap_window_to_geometry(state, window, geometry, edges)?;
  Ok(true)
}

pub(crate) fn snap_window_to_geometry(
  state: &mut CompositorState,
  window: WindowId,
  geometry: Rect,
  edges: u32,
) -> Result<(), crate::Error> {
  let previous = state.core.window_info(window)?.state;
  state.core.set_snapped(window, Some(geometry))?;
  state.core.set_tiled_edges(window, edges)?;
  let mut attached = None;
  for surface in state.surfaces.values_mut() {
    if let Some(toplevel) = surface.role.toplevel_mut()
      && toplevel.window() == Some(window)
      && !toplevel.tiled
    {
      toplevel.tiled = true;
      attached = Some(surface.id);
    }
  }
  reconfigure_all(state);
  if attached.is_some() || previous.tiled_edges != edges {
    let surface = attached.or_else(|| {
      state.surfaces.values().find_map(|surface| {
        surface
          .role
          .toplevel()
          .is_some_and(|toplevel| toplevel.window() == Some(window))
          .then_some(surface.id)
      })
    });
    if let Some(surface) = surface {
      state.events.push(Event::TilingChanged {
        window,
        surface,
        tiled: true,
        old_edges: previous.tiled_edges,
        new_edges: edges,
      });
    }
  }
  state.events.push(Event::SnappingChanged {
    window,
    snapped: true,
  });
  if previous.maximized {
    state.events.push(Event::MaximizedChanged {
      window,
      maximized: false,
    });
  }
  if previous.fullscreen {
    state.events.push(Event::FullscreenChanged {
      window,
      fullscreen: false,
    });
  }
  Ok(())
}

pub(crate) fn snap_geometry(area: Rect, slot: &str) -> Option<Rect> {
  let half_width = area.width / 2;
  let half_height = area.height / 2;
  let right = area.x.saturating_add(half_width as i32);
  let bottom = area.y.saturating_add(half_height as i32);
  Some(match slot {
    "top_left" => Rect::new(area.x, area.y, half_width, half_height),
    "top" => Rect::new(area.x, area.y, area.width, half_height),
    "top_right" => Rect::new(right, area.y, half_width, half_height),
    "left" => Rect::new(area.x, area.y, half_width, area.height),
    "center" => area,
    "right" => Rect::new(right, area.y, half_width, area.height),
    "bottom_left" => Rect::new(area.x, bottom, half_width, half_height),
    "bottom" => Rect::new(area.x, bottom, area.width, half_height),
    "bottom_right" => Rect::new(right, bottom, half_width, half_height),
    _ => return None,
  })
}

/// Converts Wayfire's left/right/top/bottom bitmask into a snapped rectangle.
pub(crate) fn edges_geometry(area: Rect, edges: u32) -> Rect {
  let left = edges & 1 != 0;
  let right = edges & 2 != 0;
  let top = edges & 4 != 0;
  let bottom = edges & 8 != 0;
  let half_width = area.width / 2;
  let half_height = area.height / 2;
  Rect::new(
    if right && !left {
      area.x.saturating_add(half_width as i32)
    } else {
      area.x
    },
    if bottom && !top {
      area.y.saturating_add(half_height as i32)
    } else {
      area.y
    },
    if left == right {
      area.width
    } else {
      half_width
    },
    if top == bottom {
      area.height
    } else {
      half_height
    },
  )
}

/// Reports persistent tiling state using Wayfire's tiled-edges bit layout.
pub(crate) fn tiled_edges(state: &CompositorState, window: WindowId) -> u32 {
  state
    .core
    .window_info(window)
    .map_or(0, |info| info.state.tiled_edges)
}

pub(crate) fn grid_slot(
  area: Rect,
  x: f64,
  y: f64,
  snap_threshold: u32,
  quarter_threshold: u32,
) -> Option<&'static str> {
  let x = x - f64::from(area.x);
  let y = y - f64::from(area.y);
  let width = f64::from(area.width);
  let height = f64::from(area.height);
  let edge = f64::from(snap_threshold);
  let corner = f64::from(quarter_threshold);
  let left = x <= edge;
  let right = width - x <= edge;
  let top = y < edge;
  let bottom = height - y < edge;
  let far_left = x <= corner;
  let far_right = width - x <= corner;
  // As in Wayfire's calc_slot (plugins/grid/wayfire/plugins/grid.hpp), the
  // top and bottom bands end strictly inside their thresholds. calc_slot
  // measures the bottom from `g.x + g.height`; the workarea's own top is
  // used here, which is the same for a workarea at y = 0.
  let far_top = y < corner;
  let far_bottom = height - y < corner;
  if (left && far_top) || (far_left && top) {
    Some("top_left")
  } else if (right && far_top) || (far_right && top) {
    Some("top_right")
  } else if (right && far_bottom) || (far_right && bottom) {
    Some("bottom_right")
  } else if (left && far_bottom) || (far_left && bottom) {
    Some("bottom_left")
  } else if right {
    Some("right")
  } else if left {
    Some("left")
  } else if top {
    Some("top")
  } else if bottom {
    Some("bottom")
  } else {
    None
  }
}

/// The geometry a window rule gave tiled `window` (`ToplevelRole::windowed`).
pub(crate) fn windowed_geometry(
  state: &CompositorState,
  window: WindowId,
) -> Option<Rect> {
  state.surfaces.values().find_map(|surface| {
    surface
      .role
      .toplevel()
      .filter(|toplevel| toplevel.window() == Some(window))
      .and_then(|toplevel| toplevel.windowed)
  })
}

/// Records where tiled `window` floats when it leaves the tree.
pub(crate) fn set_windowed_geometry(
  state: &mut CompositorState,
  window: WindowId,
  geometry: Rect,
) {
  for surface in state.surfaces.values_mut() {
    if let Some(toplevel) = surface.role.toplevel_mut()
      && toplevel.window() == Some(window)
    {
      toplevel.windowed = Some(geometry);
    }
  }
}

pub(crate) fn sync_floating_geometry(
  state: &mut CompositorState,
  window: WindowId,
  geometry: Rect,
) {
  for surface in state.surfaces.values_mut() {
    if let Some(toplevel) = surface.role.toplevel_mut()
      && toplevel.window() == Some(window)
    {
      toplevel.floating_geometry = geometry;
    }
  }
}

/// Applies floating geometry and detaches a tiled XDG role atomically.
pub(crate) fn set_floating_geometry(
  state: &mut CompositorState,
  window: WindowId,
  geometry: Rect,
) -> Result<(), crate::Error> {
  let old = state.core.window_info(window)?.state;
  let old_edges = old.tiled_edges;
  state.core.set_floating(window, Some(geometry))?;
  let mut detached = None;
  for surface in state.surfaces.values_mut() {
    if let Some(toplevel) = surface.role.toplevel_mut()
      && toplevel.window() == Some(window)
    {
      toplevel.floating_geometry = geometry;
      if toplevel.tiled {
        toplevel.tiled = false;
        detached = Some(surface.id);
      }
    }
  }
  if let Some(surface) = detached {
    state.events.push(Event::TilingChanged {
      window,
      surface,
      tiled: false,
      old_edges,
      new_edges: 0,
    });
  } else if old.floating != Some(geometry) {
    // A floating window that only moved or resized has no other event
    // that would get the new rectangle saved.
    state.events.push(Event::LayoutChanged);
  }
  reconfigure_all(state);
  Ok(())
}

/// Floats `window` if it is tiled.
fn float_if_tiled(state: &mut CompositorState, window: WindowId) {
  if state
    .core
    .window_info(window)
    .is_ok_and(|info| info.state.floating.is_none())
  {
    let _ = toggle_tiling(state, window);
  }
}

/// Floats every tiled window, where its tile was, as Wayfire's simple-tile
/// leaves its views when it is unloaded.
pub(crate) fn float_tiled(state: &mut CompositorState) {
  let windows = state.core.window_ids().collect::<Vec<_>>();
  for window in windows {
    float_if_tiled(state, window);
  }
}

/// Moves `window` between the tiling tree and floating. Without
/// simple-tile (`state.tiling` false) a floating window cannot tile, and
/// this fails with [`crate::Error::TilingDisabled`].
pub(crate) fn toggle_tiling(
  state: &mut CompositorState,
  window: WindowId,
) -> Result<bool, crate::Error> {
  let Some((object, tiled)) =
    state.surfaces.iter().find_map(|(object, surface)| {
      match surface.role.toplevel() {
        Some(toplevel) if toplevel.window() == Some(window) => {
          Some((object.clone(), toplevel.tiled))
        }
        _ => None,
      }
    })
  else {
    return Err(shingle_core::ShingleError::WindowNotFound(window.0).into());
  };
  if !tiled && !state.tiling {
    return Err(crate::Error::TilingDisabled);
  }
  let geometry = if tiled {
    let info = state.core.window_info(window)?;
    // A held view has no tile yet, so it floats at its configured size,
    // or at its buffer's while its configures left the size to it.
    let tile = state
      .core
      .window_layout(window)
      .map(|layout| layout.geometry)
      .or_else(|error| {
        if info.state.held {
          let configured = configured_geometry(state, &object);
          Ok(if configured.width == 0 || configured.height == 0 {
            buffer_rect(state, &object)
          } else {
            configured
          })
        } else {
          Err(error)
        }
      });
    let windowed = state
      .surfaces
      .get(&object)
      .and_then(|surface| surface.role.toplevel())
      .and_then(|toplevel| toplevel.windowed);
    Some(
      windowed
        .map_or_else(|| info.state.restore_geometry.map_or(tile, Ok), Ok)?,
    )
  } else {
    None
  };
  let old = state.core.window_info(window)?.state;
  let old_edges = old.tiled_edges;
  state.core.set_floating(window, geometry)?;
  if let Some(surface) = state.surfaces.get_mut(&object)
    && let Some(toplevel) = surface.role.toplevel_mut()
  {
    toplevel.tiled = !tiled;
    if let Some(geometry) = geometry {
      toplevel.floating_geometry = geometry;
    }
    // Tiling again keeps where it floated, for the next untiling.
    if !tiled && let Some(floating) = old.floating {
      toplevel.windowed = Some(floating);
    }
  }
  state.events.push(Event::TilingChanged {
    window,
    surface: state.surfaces[&object].id,
    tiled: !tiled,
    old_edges,
    new_edges: if tiled { 0 } else { 15 },
  });
  reconfigure_all(state);
  crate::input::focus_window(state, window);
  Ok(!tiled)
}

/// A rectangle at the origin of the size of `object`'s buffer in surface
/// coordinates, at least 1x1; placement then sizes and places it.
fn buffer_rect(state: &CompositorState, object: &ObjectId) -> Rect {
  let (width, height) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.buffer.as_ref())
    .map_or((1, 1), |buffer| {
      let scale = buffer.scale.max(1) as u32;
      (buffer.width / scale, buffer.height / scale)
    });
  Rect::new(0, 0, width.max(1), height.max(1))
}

fn configured_geometry(state: &CompositorState, object: &ObjectId) -> Rect {
  state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
    .and_then(|toplevel| toplevel.configured_geometry)
    .unwrap_or(state.active().rect)
}

pub(crate) fn assign_pending_popup(
  state: &mut CompositorState,
  popup: &XdgPopup,
  parent: &ObjectId,
) {
  let object = state.surfaces.iter().find_map(|(object, surface)| {
    let pending = match &surface.role {
      Role::Xdg {
        kind: Some(XdgKind::PendingPopup { resource, .. }),
        ..
      } => resource == popup,
      _ => false,
    };
    pending.then_some(object.clone())
  });
  let Some(object) = object else {
    return;
  };
  let Some(positioner) =
    state
      .surfaces
      .get(&object)
      .and_then(|surface| match &surface.role {
        Role::Xdg {
          kind: Some(XdgKind::PendingPopup { positioner, .. }),
          ..
        } => Some(positioner.clone()),
        _ => None,
      })
  else {
    return;
  };
  let Some(rules) = positioner
    .data::<Mutex<Positioner>>()
    .and_then(|positioner| positioner.lock().ok().map(|rules| rules.clone()))
  else {
    return;
  };
  let relative = popup_geometry(state, parent, &rules);
  if let Some(surface) = state.surfaces.get_mut(&object)
    && let Role::Xdg {
      kind,
      serials,
      initial_acked,
      ..
    } = &mut surface.role
  {
    *kind = Some(XdgKind::Popup(PopupRole {
      resource: popup.clone(),
      parent: parent.clone(),
      relative,
    }));
    serials.clear();
    *initial_acked = false;
  }
  let serial = state.serial();
  if let Some(surface) = state.surfaces.get_mut(&object)
    && let Role::Xdg {
      resource,
      kind: Some(XdgKind::Popup(PopupRole { relative, .. })),
      serials,
      ..
    } = &mut surface.role
  {
    popup.configure(
      relative.x,
      relative.y,
      relative.width.min(i32::MAX as u32) as i32,
      relative.height.min(i32::MAX as u32) as i32,
    );
    resource.configure(serial);
    serials.insert(serial);
  }
}

struct XdgSurfaceData;
struct XdgToplevelData;
struct XdgPopupData;

impl GlobalDispatch<XdgWmBase, ()> for CompositorState {
  crate::security_context::protocol_can_view!("xdg_wm_base");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<XdgWmBase>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<XdgWmBase, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &XdgWmBase,
    request: xdg_wm_base::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      xdg_wm_base::Request::Destroy => {}
      xdg_wm_base::Request::CreatePositioner { id } => {
        data_init.init(
          id,
          Mutex::new(Positioner {
            size: (0, 0),
            anchor_rect: Rect::default(),
            anchor: xdg_positioner::Anchor::None,
            gravity: xdg_positioner::Gravity::None,
            reactive: false,
            parent_size: None,
            parent_configure: None,
          }),
        );
      }
      xdg_wm_base::Request::GetXdgSurface { id, surface } => {
        let object = surface.id();
        let xdg = data_init.init(id, XdgSurfaceData);
        let valid = state
          .surfaces
          .get(&object)
          .is_some_and(|surface| matches!(surface.role, Role::None));
        if !valid {
          xdg.post_error(
            xdg_wm_base::Error::Role,
            "wl_surface already has a role",
          );
          return;
        }
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::Xdg {
            resource: xdg,
            kind: None,
            serials: BTreeSet::new(),
            initial_acked: false,
          };
        }
      }
      xdg_wm_base::Request::Pong { .. } => {}
      _ => {}
    }
  }
}

impl Dispatch<XdgPositioner, Mutex<Positioner>> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    resource: &XdgPositioner,
    request: xdg_positioner::Request,
    data: &Mutex<Positioner>,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let Ok(mut positioner) = data.lock() else {
      return;
    };
    match request {
      xdg_positioner::Request::Destroy => {}
      xdg_positioner::Request::SetSize { width, height } => {
        if width <= 0 || height <= 0 {
          resource.post_error(
            xdg_positioner::Error::InvalidInput,
            "positioner size must be positive",
          );
        } else {
          positioner.size = (width, height);
        }
      }
      xdg_positioner::Request::SetAnchorRect {
        x,
        y,
        width,
        height,
      } => {
        if width < 0 || height < 0 {
          resource.post_error(
            xdg_positioner::Error::InvalidInput,
            "positioner anchor rectangle cannot have negative dimensions",
          );
        } else {
          positioner.anchor_rect = Rect::new(x, y, width as u32, height as u32);
        }
      }
      xdg_positioner::Request::SetAnchor {
        anchor: WEnum::Value(anchor),
      } => {
        positioner.anchor = anchor;
      }
      xdg_positioner::Request::SetGravity {
        gravity: WEnum::Value(gravity),
      } => {
        positioner.gravity = gravity;
      }
      xdg_positioner::Request::SetConstraintAdjustment { .. } => {}
      xdg_positioner::Request::SetOffset { .. } => {}
      xdg_positioner::Request::SetReactive => positioner.reactive = true,
      xdg_positioner::Request::SetParentSize {
        parent_width,
        parent_height,
      } => {
        positioner.parent_size = Some((parent_width, parent_height));
      }
      xdg_positioner::Request::SetParentConfigure { serial } => {
        positioner.parent_configure = Some(serial);
      }
      _ => {}
    }
  }
}

impl Dispatch<XdgSurface, XdgSurfaceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &XdgSurface,
    request: xdg_surface::Request,
    _data: &XdgSurfaceData,
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let object = state.surfaces.iter().find_map(|(object, surface)| {
      let matches = matches!(
          &surface.role,
          Role::Xdg { resource: candidate, .. } if candidate == resource
      );
      matches.then_some(object.clone())
    });
    let Some(object) = object else {
      return;
    };
    match request {
      xdg_surface::Request::Destroy => {
        destroy_role_at(state, &object);
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::Dead;
          surface.buffer = None;
        }
      }
      xdg_surface::Request::GetToplevel { id } => {
        let toplevel = data_init.init(id, XdgToplevelData);
        let can_assign = state.surfaces.get(&object).is_some_and(|surface| {
          matches!(&surface.role, Role::Xdg { kind: None, .. })
        });
        if !can_assign {
          resource.post_error(
            xdg_wm_base::Error::Role,
            "xdg_surface already has a role",
          );
          return;
        }
        let identity = state
          .surfaces
          .get(&object)
          .and_then(|surface| state.launch_templates.get(&surface.client))
          .cloned()
          .unwrap_or_default();
        let app_id = identity.app_id.clone();
        let pid = state
          .surfaces
          .get(&object)
          .and_then(|surface| state.client_pids.get(&surface.client))
          .copied();
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::Xdg {
            resource: resource.clone(),
            kind: Some(XdgKind::Toplevel(Box::new(ToplevelRole {
              handle: ToplevelHandle::Xdg(toplevel),
              window: WindowId(0),
              identity,
              app_id,
              title: String::new(),
              parent: None,
              tiled: true,
              opacity: 1.0,
              floating_geometry: Rect::new(0, 0, 0, 0),
              min_size: (0, 0),
              max_size: (0, 0),
              configured_geometry: None,
              configure: ConfigureState::default(),
              created_rules_pending: None,
              windowed: None,
              unmapped: false,
              pid,
              client_decorated: true,
              decorated: None,
              created_output: state.active_output.clone(),
            }))),
            serials: BTreeSet::new(),
            initial_acked: false,
          };
        }
        // A KDE server decoration made for the surface before its role.
        if let Some(mode) = crate::server_decoration::mode_of(state, &object) {
          crate::decorator::set_client_decorated(
            state,
            &object,
            mode == crate::server_decoration::Mode::Client,
          );
        }
        reconfigure_all(state);
      }
      xdg_surface::Request::GetPopup {
        id,
        parent,
        positioner,
      } => {
        let popup = data_init.init(id, XdgPopupData);
        let can_assign = state.surfaces.get(&object).is_some_and(|surface| {
          matches!(&surface.role, Role::Xdg { kind: None, .. })
        });
        let parent = parent.and_then(|parent| {
          state.surfaces.iter().find_map(|(object, surface)| {
            let matches = matches!(
                &surface.role,
                Role::Xdg { resource: candidate, .. }
                    if candidate == &parent
            );
            matches.then_some(object.clone())
          })
        });
        let Some(rules) =
          positioner
            .data::<Mutex<Positioner>>()
            .and_then(|positioner| {
              positioner.lock().ok().map(|rules| rules.clone())
            })
        else {
          return;
        };
        if !can_assign || rules.size.0 <= 0 || rules.size.1 <= 0 {
          popup.post_error(
            xdg_wm_base::Error::InvalidPositioner,
            "positioner is incomplete or surface already has a popup role",
          );
          return;
        }
        let Some(parent) = parent else {
          if let Some(surface) = state.surfaces.get_mut(&object)
            && let Role::Xdg { kind, .. } = &mut surface.role
          {
            *kind = Some(XdgKind::PendingPopup {
              resource: popup,
              positioner,
            });
          }
          return;
        };
        let relative = popup_geometry(state, &parent, &rules);
        if let Some(surface) = state.surfaces.get_mut(&object)
          && let Role::Xdg {
            kind,
            serials,
            initial_acked,
            ..
          } = &mut surface.role
        {
          *kind = Some(XdgKind::Popup(PopupRole {
            resource: popup.clone(),
            parent,
            relative,
          }));
          serials.clear();
          *initial_acked = false;
        }
        let serial = state.serial();
        if let Some(surface) = state.surfaces.get_mut(&object)
          && let Role::Xdg {
            resource,
            kind:
              Some(XdgKind::Popup(PopupRole {
                resource: popup,
                relative,
                ..
              })),
            serials,
            ..
          } = &mut surface.role
        {
          popup.configure(
            relative.x,
            relative.y,
            relative.width.min(i32::MAX as u32) as i32,
            relative.height.min(i32::MAX as u32) as i32,
          );
          resource.configure(serial);
          serials.insert(serial);
        }
      }
      xdg_surface::Request::SetWindowGeometry {
        x,
        y,
        width,
        height,
      } => {
        if width <= 0 || height <= 0 {
          resource.post_error(
            xdg_surface::Error::InvalidSize,
            "window geometry must have a positive size",
          );
          return;
        }
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.pending.window_geometry =
            Some(Rect::new(x, y, width as u32, height as u32));
        }
      }
      xdg_surface::Request::AckConfigure { serial } => {
        if let Some(surface) = state.surfaces.get_mut(&object)
          && let Role::Xdg {
            serials,
            initial_acked,
            ..
          } = &mut surface.role
        {
          if !serials.remove(&serial) {
            resource.post_error(
              xdg_surface::Error::InvalidSerial,
              "configure serial was not sent or was already acknowledged",
            );
          } else {
            serials.retain(|candidate| *candidate > serial);
            *initial_acked = true;
            crate::decoration::acknowledged(state, &object, serial);
          }
        }
      }
      _ => {}
    }
  }
}

impl Dispatch<XdgToplevel, XdgToplevelData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &XdgToplevel,
    request: xdg_toplevel::Request,
    _data: &XdgToplevelData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let object = state.surfaces.iter().find_map(|(object, surface)| {
      let matches = surface
        .role
        .toplevel()
        .is_some_and(|toplevel| toplevel.handle.xdg() == Some(resource));
      matches.then_some(object.clone())
    });
    let Some(object) = object else {
      return;
    };
    match request {
      xdg_toplevel::Request::Destroy => {
        destroy_role_at(state, &object);
        if let Some(surface) = state.surfaces.get_mut(&object) {
          if let Role::Xdg { kind, .. } = &mut surface.role {
            *kind = None;
          }
          surface.buffer = None;
        }
      }
      xdg_toplevel::Request::SetParent { parent } => {
        let parent_window = parent.and_then(|parent| {
          state.surfaces.values().find_map(|surface| {
            match surface.role.toplevel() {
              Some(toplevel) if toplevel.handle.xdg() == Some(&parent) => {
                toplevel.window()
              }
              _ => None,
            }
          })
        });
        set_toplevel_parent(state, &object, parent_window);
        crate::foreign::clear_import_tracking(state, &object);
      }
      xdg_toplevel::Request::SetTitle { title } => {
        update_title(state, &object, title);
      }
      xdg_toplevel::Request::SetAppId { app_id } => {
        update_app_id(state, &object, app_id)
      }
      xdg_toplevel::Request::ShowWindowMenu { .. } => {}
      xdg_toplevel::Request::SetMinimized => {
        let Some(window) = materialize_toplevel(state, &object) else {
          return;
        };
        let _ = set_minimized(state, window, true);
      }
      xdg_toplevel::Request::Move { .. } => {
        let Some(window) = materialize_toplevel(state, &object) else {
          return;
        };
        state.events.push(Event::BeginMove { window });
      }
      xdg_toplevel::Request::Resize { edges, .. } => {
        let Some(window) = materialize_toplevel(state, &object) else {
          return;
        };
        let edges = match edges {
          WEnum::Value(edges) => edges as u32,
          WEnum::Unknown(value) => value,
        };
        state.events.push(Event::BeginResize { window, edges });
      }
      xdg_toplevel::Request::SetMaxSize { width, height }
      | xdg_toplevel::Request::SetMinSize { width, height } => {
        if width < 0 || height < 0 {
          resource.post_error(
            xdg_toplevel::Error::InvalidSize,
            "sizes cannot be negative",
          );
          return;
        }
        if let Some(surface) = state.surfaces.get_mut(&object)
          && let Some(toplevel) = surface.role.toplevel_mut()
        {
          if matches!(request, xdg_toplevel::Request::SetMaxSize { .. }) {
            toplevel.max_size = (width, height);
          } else {
            toplevel.min_size = (width, height);
          }
        }
      }
      xdg_toplevel::Request::SetMaximized
      | xdg_toplevel::Request::UnsetMaximized => {
        let maximized = matches!(request, xdg_toplevel::Request::SetMaximized);
        let Some(window) = materialize_toplevel(state, &object) else {
          return;
        };
        // simple-tile carries out a tiled view's maximize requests by
        // ignoring them, since it manages the tiled state itself
        // (wayfire:plugins/tile/tile-plugin.cpp, `on_tile_request`):
        // inkscape asks to be maximized at start and would otherwise
        // cover the tiles beside it. The client still gets a configure.
        let tiled = state.tiling
          && state
            .surfaces
            .get(&object)
            .and_then(|surface| surface.role.toplevel())
            .is_some_and(|toplevel| toplevel.tiled);
        if !tiled {
          let _ = set_maximized(state, window, maximized);
        }
        request_configure(state, &object);
      }
      xdg_toplevel::Request::SetFullscreen { .. }
      | xdg_toplevel::Request::UnsetFullscreen => {
        let output = match &request {
          xdg_toplevel::Request::SetFullscreen { output } => output.clone(),
          _ => None,
        };
        let fullscreen =
          matches!(request, xdg_toplevel::Request::SetFullscreen { .. });
        let Some(window) = materialize_toplevel(state, &object) else {
          return;
        };
        if let Some(output) = output {
          fullscreen_on(state, window, &output);
        }
        let _ = set_fullscreen(state, window, fullscreen);
        request_configure(state, &object);
      }
      _ => {}
    }
  }
}

impl Dispatch<XdgPopup, XdgPopupData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &XdgPopup,
    request: xdg_popup::Request,
    _data: &XdgPopupData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let object = state.surfaces.iter().find_map(|(object, surface)| {
      let matches = surface
        .role
        .popup()
        .is_some_and(|popup| &popup.resource == resource);
      matches.then_some(object.clone())
    });
    match request {
      xdg_popup::Request::Destroy => {
        if let Some(object) = object
          && let Some(surface) = state.surfaces.get_mut(&object)
        {
          if let Role::Xdg { kind, .. } = &mut surface.role {
            *kind = None;
          }
          surface.buffer = None;
        }
      }
      xdg_popup::Request::Grab { .. } => {}
      xdg_popup::Request::Reposition { positioner, token } => {
        let Some(object) = object else {
          return;
        };
        let Some(rules) =
          positioner
            .data::<Mutex<Positioner>>()
            .and_then(|positioner| {
              positioner.lock().ok().map(|rules| rules.clone())
            })
        else {
          return;
        };
        let Some(parent) =
          state
            .surfaces
            .get(&object)
            .and_then(|surface| match &surface.role {
              Role::Xdg {
                kind: Some(XdgKind::Popup(popup)),
                ..
              } => Some(popup.parent.clone()),
              _ => None,
            })
        else {
          return;
        };
        let relative = popup_geometry(state, &parent, &rules);
        let serial = state.serial();
        if let Some(surface) = state.surfaces.get_mut(&object)
          && let Role::Xdg {
            resource: xdg,
            kind: Some(XdgKind::Popup(popup)),
            serials,
            ..
          } = &mut surface.role
        {
          popup.relative = relative;
          popup.resource.repositioned(token);
          popup.resource.configure(
            relative.x,
            relative.y,
            relative.width.min(i32::MAX as u32) as i32,
            relative.height.min(i32::MAX as u32) as i32,
          );
          xdg.configure(serial);
          serials.insert(serial);
        }
      }
      _ => {}
    }
  }
}

#[derive(Clone)]
struct Positioner {
  size: (i32, i32),
  anchor_rect: Rect,
  anchor: xdg_positioner::Anchor,
  gravity: xdg_positioner::Gravity,
  reactive: bool,
  parent_size: Option<(i32, i32)>,
  parent_configure: Option<u32>,
}

fn popup_geometry(
  state: &CompositorState,
  parent: &ObjectId,
  positioner: &Positioner,
) -> Rect {
  use xdg_positioner::{Anchor, Gravity};

  let rect = positioner.anchor_rect;
  let (anchor_x, anchor_y) = match positioner.anchor {
    Anchor::None => (rect.width as i64 / 2, rect.height as i64 / 2),
    Anchor::Top => (rect.width as i64 / 2, 0),
    Anchor::Bottom => (rect.width as i64 / 2, rect.height as i64),
    Anchor::Left => (0, rect.height as i64 / 2),
    Anchor::Right => (rect.width as i64, rect.height as i64 / 2),
    Anchor::TopLeft => (0, 0),
    Anchor::BottomLeft => (0, rect.height as i64),
    Anchor::TopRight => (rect.width as i64, 0),
    Anchor::BottomRight => (rect.width as i64, rect.height as i64),
    _ => (0, 0),
  };
  let (gravity_x, gravity_y) = match positioner.gravity {
    Gravity::None => (
      -i64::from(positioner.size.0) / 2,
      -i64::from(positioner.size.1) / 2,
    ),
    Gravity::Top => (
      -i64::from(positioner.size.0) / 2,
      -i64::from(positioner.size.1),
    ),
    Gravity::Bottom => (-i64::from(positioner.size.0) / 2, 0),
    Gravity::Left => (
      -i64::from(positioner.size.0),
      -i64::from(positioner.size.1) / 2,
    ),
    Gravity::Right => (0, -i64::from(positioner.size.1) / 2),
    Gravity::TopLeft => {
      (-i64::from(positioner.size.0), -i64::from(positioner.size.1))
    }
    Gravity::BottomLeft => (-i64::from(positioner.size.0), 0),
    Gravity::TopRight => (0, -i64::from(positioner.size.1)),
    Gravity::BottomRight => (0, 0),
    _ => (0, 0),
  };
  let mut x = i64::from(rect.x) + anchor_x + gravity_x;
  let mut y = i64::from(rect.y) + anchor_y + gravity_y;
  if let Some(parent_geometry) = surface_geometry(state, parent) {
    let output = state.active().rect;
    let global_x = i64::from(parent_geometry.x) + x;
    let global_y = i64::from(parent_geometry.y) + y;
    let max_x = i64::from(output.x) + i64::from(output.width)
      - i64::from(positioner.size.0);
    let max_y = i64::from(output.y) + i64::from(output.height)
      - i64::from(positioner.size.1);
    x =
      global_x.clamp(i64::from(output.x), max_x) - i64::from(parent_geometry.x);
    y =
      global_y.clamp(i64::from(output.y), max_y) - i64::from(parent_geometry.y);
  }
  Rect::new(
    x.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    y.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    positioner.size.0 as u32,
    positioner.size.1 as u32,
  )
}

struct SessionLockData;
struct SessionLockSurfaceData;

impl GlobalDispatch<ExtSessionLockManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("ext_session_lock_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtSessionLockManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ExtSessionLockManagerV1,
    request: ext_session_lock_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_session_lock_manager_v1::Request::Destroy => {}
      ext_session_lock_manager_v1::Request::Lock { id } => {
        let lock = data_init.init(id, SessionLockData);
        if state.active_lock.is_some() {
          lock.finished();
          return;
        }
        state.active_lock = Some(lock);
        state.idle.client_lock_requested(state.now());
        state.emit_idle_changes();
        crate::input::sync_focus(state);
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtSessionLockV1, SessionLockData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtSessionLockV1,
    request: ext_session_lock_v1::Request,
    _data: &SessionLockData,
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_session_lock_v1::Request::Destroy => {
        if state.idle.lock_state() == LockState::Locked {
          resource.post_error(
            ext_session_lock_v1::Error::InvalidDestroy,
            "a locked session must use unlock_and_destroy",
          );
        } else {
          state.active_lock = None;
        }
      }
      ext_session_lock_v1::Request::UnlockAndDestroy => {
        if state.idle.lock_state() != LockState::Locked {
          resource.post_error(
            ext_session_lock_v1::Error::InvalidUnlock,
            "the locked event has not been sent",
          );
        } else {
          state.unlock_session();
        }
      }
      ext_session_lock_v1::Request::GetLockSurface {
        id,
        surface,
        output,
      } => {
        let lock_surface = data_init.init(id, SessionLockSurfaceData);
        let object = surface.id();
        let on = state
          .outputs
          .of_resource(&output)
          .map(|output| output.id.clone());
        // One lock surface per output, as the protocol requires.
        let duplicate = on.as_ref().is_some_and(|on| {
          state.surfaces.values().any(|surface| {
            matches!(
              &surface.role,
              Role::SessionLock { output, .. } if output == on
            )
          })
        });
        let invalid = state.surfaces.get(&object).is_none_or(|surface| {
          !matches!(surface.role, Role::None)
            || surface.committed_once
            || !matches!(surface.pending.buffer, PendingBuffer::Unchanged)
        });
        if duplicate {
          resource.post_error(
            ext_session_lock_v1::Error::DuplicateOutput,
            "the output already has a session lock surface",
          );
          return;
        }
        if invalid {
          resource.post_error(
            ext_session_lock_v1::Error::Role,
            "lock surface must be a new, uncommitted wl_surface without a role",
          );
          return;
        }
        // An output removed after the client bound it is a race the
        // client cannot avoid: its lock surface stays inert, without a
        // configure, until the client sees the global go and destroys it.
        let Some(on) = on else {
          return;
        };
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::SessionLock {
            resource: lock_surface,
            output: on,
            serials: BTreeMap::new(),
            acked: None,
            shown: false,
          };
        }
        configure_lock_surface(state, &object);
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtSessionLockV1,
    _data: &SessionLockData,
  ) {
    if state
      .active_lock
      .as_ref()
      .is_some_and(|active| active == resource)
    {
      if matches!(
        state.idle.lock_state(),
        LockState::Pending | LockState::Locked
      ) {
        state.idle.lock_client_failed(state.now());
        state.emit_idle_changes();
      }
      state.active_lock = None;
      crate::input::sync_focus(state);
    }
  }
}

impl Dispatch<ExtSessionLockSurfaceV1, SessionLockSurfaceData>
  for CompositorState
{
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtSessionLockSurfaceV1,
    request: ext_session_lock_surface_v1::Request,
    _data: &SessionLockSurfaceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let object = state.surfaces.iter().find_map(|(object, surface)| {
      let matches = matches!(
          &surface.role,
          Role::SessionLock { resource: candidate, .. }
              if candidate == resource
      );
      matches.then_some(object.clone())
    });
    let Some(object) = object else {
      return;
    };
    match request {
      ext_session_lock_surface_v1::Request::Destroy => {
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::Dead;
          surface.buffer = None;
        }
      }
      ext_session_lock_surface_v1::Request::AckConfigure { serial } => {
        if let Some(surface) = state.surfaces.get_mut(&object)
          && let Role::SessionLock { serials, acked, .. } = &mut surface.role
        {
          if let Some((width, height)) = serials.remove(&serial) {
            serials.retain(|candidate, _| *candidate > serial);
            *acked = Some((serial, width, height));
          } else {
            resource.post_error(
              ext_session_lock_surface_v1::Error::InvalidSerial,
              "configure serial was not sent or was already acknowledged",
            );
          }
        }
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
  };

  use shingle_core::{LaunchIdentity, Rect, WindowId, WindowPresence};
  use wayland_server::Display;

  use super::unblock_map;
  use crate::{
    state::{ClientDataImpl, CompositorState},
    test_client::TestClient,
  };

  #[test]
  fn a_refocus_of_the_same_window_keeps_its_held_keys() {
    use wayland_client::protocol::wl_seat::WlSeat;
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let seat = client.bind::<WlSeat>(5);
    let _keyboard = seat.get_keyboard(&client.handle, ());
    let toplevel = client.toplevel(&mut compositor, "held");
    client.attach_buffer(&mut compositor, &toplevel.surface, 8, 8);
    let device = compositor.add_input_device(crate::InputDeviceInfo {
      name: "test keyboard".into(),
      vendor: None,
      product: None,
      kind: crate::InputDeviceKind::Keyboard,
    });
    // KEY_LEFTALT, held while a binding moves the window and the focus is
    // chosen again, as a vswitch `with_win` binding does.
    compositor.keyboard_key(device, 56, true);
    client.roundtrip(&mut compositor);
    client.events.keyboard.clear();
    compositor.state.input.keyboard_focus = None;
    crate::input::sync_focus(&mut compositor.state);
    compositor.keyboard_key(device, 56, false);
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.keyboard, ["key 56 released"]);
    drop(client);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn keys_held_across_a_focus_change_go_to_the_new_window() {
    use wayland_client::protocol::wl_seat::WlSeat;
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let seat = client.bind::<WlSeat>(5);
    let _keyboard = seat.get_keyboard(&client.handle, ());
    let first = client.toplevel(&mut compositor, "first");
    client.attach_buffer(&mut compositor, &first.surface, 8, 8);
    let device = compositor.add_input_device(crate::InputDeviceInfo {
      name: "test keyboard".into(),
      vendor: None,
      product: None,
      kind: crate::InputDeviceKind::Keyboard,
    });
    compositor.keyboard_key(device, 56, true);
    client.roundtrip(&mut compositor);
    client.events.keyboard.clear();
    // A new window takes the focus while the key is held.
    let second = client.toplevel(&mut compositor, "second");
    client.attach_buffer(&mut compositor, &second.surface, 8, 8);
    compositor.keyboard_key(device, 56, false);
    client.roundtrip(&mut compositor);
    assert_eq!(
      client.events.keyboard,
      ["leave", "enter [56]", "key 56 released"]
    );
    // A press whose release a grab took is pressed again, as in wlroots.
    client.events.keyboard.clear();
    compositor.keyboard_key(device, 56, true);
    compositor.keyboard_key(device, 56, true);
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.keyboard, ["key 56 pressed", "key 56 pressed"]);
    drop(client);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn an_xdg_toplevel_has_its_clients_pid_in_list_views() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "with-pid");
    client.attach_buffer(&mut compositor, &toplevel.surface, 8, 8);
    let request = serde_json::json!({"method": "window-rules/list-views"});
    let views = crate::ipc::respond(&mut compositor.state, &request);
    // The test client is this process, at the other end of the socket.
    assert_eq!(views[0]["pid"], std::process::id());
    drop(client);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_lock_surface_buffer_covers_opaquely_by_format_or_alpha() {
    use wayland_client::protocol::wl_compositor::WlCompositor;

    use super::covers_opaquely;
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.set_shm_upload(true);
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let surface = wl_compositor.create_surface(&client.handle, ());
    // The test client's buffers are ARGB8888 and zeroed: transparent.
    let buffer = client.shm_buffer(4, 4);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    let shown = compositor
      .state
      .surfaces
      .values()
      .find_map(|surface| surface.buffer.clone())
      .expect("a shown buffer");
    let content = shown.shm().expect("an shm buffer to upload");
    assert!(!covers_opaquely(&shown), "read while locked: transparent");
    content.release();
    assert!(!covers_opaquely(&shown), "unreadable after release");
    let mut pixels = shown.clone();
    pixels.content = crate::SnapshotContent::Pixels(vec![9, 9, 9, 255].into());
    assert!(covers_opaquely(&pixels));
    pixels.content = crate::SnapshotContent::Pixels(vec![9, 9, 9, 254].into());
    assert!(!covers_opaquely(&pixels));
  }

  /// Minimal wire-protocol client on one end of a socket pair.
  struct WireClient {
    stream: UnixStream,
    globals: HashMap<String, (u32, u32)>,
  }

  fn string_arg(value: &str) -> Vec<u8> {
    let length = value.len() + 1;
    let mut bytes = (length as u32).to_ne_bytes().to_vec();
    bytes.extend_from_slice(value.as_bytes());
    bytes.resize(4 + length.next_multiple_of(4), 0);
    bytes
  }

  impl WireClient {
    fn send(&mut self, object: u32, opcode: u16, args: &[u8]) {
      let size = (8 + args.len()) as u32;
      let mut message = object.to_ne_bytes().to_vec();
      message.extend_from_slice(&((size << 16) | opcode as u32).to_ne_bytes());
      message.extend_from_slice(args);
      self.stream.write_all(&message).unwrap();
    }

    /// Reads pending events and records the globals that the registry,
    /// object 2, announced.
    fn read_globals(&mut self) {
      let mut bytes = Vec::new();
      let mut buffer = [0_u8; 4096];
      while let Ok(read) = self.stream.read(&mut buffer) {
        bytes.extend_from_slice(&buffer[..read]);
        if read < buffer.len() {
          break;
        }
      }
      let word =
        |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
      let mut at = 0;
      while at + 8 <= bytes.len() {
        let (object, header) = (word(at), word(at + 4));
        if object == 2 && header & 0xffff == 0 {
          let length = word(at + 12) as usize;
          let interface = &bytes[at + 16..at + 15 + length];
          let version = word(at + 16 + length.next_multiple_of(4));
          self.globals.insert(
            String::from_utf8_lossy(interface).into_owned(),
            (word(at + 8), version),
          );
        }
        at += (header >> 16) as usize;
      }
    }

    fn bind(&mut self, interface: &str, version: u32, id: u32) {
      let (name, available) = self.globals[interface];
      let mut args = name.to_ne_bytes().to_vec();
      args.extend(string_arg(interface));
      args.extend_from_slice(&version.min(available).to_ne_bytes());
      args.extend_from_slice(&id.to_ne_bytes());
      self.send(2, 0, &args);
    }
  }

  fn roundtrip(
    display: &mut Display<CompositorState>,
    state: &mut CompositorState,
  ) {
    display.dispatch_clients(state).unwrap();
    display.flush_clients().unwrap();
  }

  /// The IPC client id the map-blocking tests subscribe as.
  const SUBSCRIBER: u64 = 7;

  #[test]
  fn toplevel_states_encode_as_xdg_states_in_order() {
    use wayland_protocols::xdg::shell::server::xdg_toplevel::State;

    let decode = |bytes: Vec<u8>| {
      bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_ne_bytes(*chunk))
        .collect::<Vec<_>>()
    };
    assert!(super::ToplevelStates::default().xdg_states().is_empty());
    let all = super::ToplevelStates {
      tiled: true,
      activated: true,
      maximized: true,
      fullscreen: true,
    };
    assert_eq!(
      decode(all.xdg_states()),
      [
        State::TiledLeft,
        State::TiledRight,
        State::TiledTop,
        State::TiledBottom,
        State::Activated,
        State::Maximized,
        State::Fullscreen,
      ]
      .map(|state| state as u32)
    );
    let floating = super::ToplevelStates {
      activated: true,
      fullscreen: true,
      ..Default::default()
    };
    assert_eq!(
      decode(floating.xdg_states()),
      [State::Activated as u32, State::Fullscreen as u32]
    );
  }

  #[test]
  fn toplevel_and_toplevel_mut_find_the_xdg_toplevel_role() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "role");
    let object = wayland_client::Proxy::id(&toplevel.surface).protocol_id();
    let surface = compositor
      .state
      .surfaces
      .values_mut()
      .find(|surface| {
        wayland_server::Resource::id(&surface.resource).protocol_id() == object
      })
      .expect("the toplevel surface");
    assert_eq!(surface.role.toplevel().unwrap().app_id, "role");
    surface.role.toplevel_mut().unwrap().title = "changed".into();
    assert_eq!(surface.role.toplevel().unwrap().title, "changed");
    assert!(surface.role.popup().is_none());
    assert!(super::Role::None.toplevel().is_none());
    assert!(super::Role::Cursor.toplevel_mut().is_none());
  }

  #[test]
  fn close_window_asks_the_xdg_toplevel_to_close() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "closing");
    client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
    let window = windows_with(&compositor.state, "closing")[0];
    assert!(
      super::close_window(&mut compositor.state, WindowId(999)).is_none()
    );
    assert!(client.events.closes.is_empty());
    assert!(super::close_window(&mut compositor.state, window).is_some());
    client.roundtrip(&mut compositor);
    assert_eq!(
      client.events.closes,
      [wayland_client::Proxy::id(&toplevel.toplevel)]
    );
    assert_eq!(presence(&compositor.state, window), WindowPresence::Mapped);
  }

  /// Lists the tiles `simple-tile/get-layout` reports for the first
  /// workspace, as `(view-id, width)` in layout order.
  fn layout_tiles(state: &mut CompositorState) -> Vec<(u64, u64)> {
    fn collect(node: &serde_json::Value, tiles: &mut Vec<(u64, u64)>) {
      if let Some(id) = node.get("view-id").and_then(|id| id.as_u64()) {
        let width = node["geometry"]["width"].as_u64().unwrap();
        tiles.push((id, width));
      }
      for key in ["vertical-split", "horizontal-split"] {
        for child in node.get(key).and_then(|v| v.as_array()).into_iter() {
          for child in child {
            collect(child, tiles);
          }
        }
      }
    }
    let reply = crate::ipc::respond(
      state,
      &serde_json::json!({
        "method": "simple-tile/get-layout",
        "data": {"wset-index": 1, "workspace": {"x": 0, "y": 0}},
      }),
    );
    assert_eq!(reply["result"], "ok", "{reply}");
    let mut tiles = Vec::new();
    collect(&reply["layout"], &mut tiles);
    tiles
  }

  /// Returns the windows whose identity has `app_id`, in id order.
  fn windows_with(state: &CompositorState, app_id: &str) -> Vec<WindowId> {
    state
      .core
      .window_ids()
      .filter(|window| {
        state
          .core
          .window_info(*window)
          .is_ok_and(|info| info.identity.app_id == app_id)
      })
      .collect()
  }

  fn presence(state: &CompositorState, window: WindowId) -> WindowPresence {
    state.core.window_info(window).unwrap().presence
  }

  #[test]
  fn grid_corners_end_strictly_inside_the_quarter_threshold() {
    use shingle_core::Rect;
    let area = Rect::new(0, 0, 1000, 600);
    let slot = |x, y| super::grid_slot(area, x, y, 10, 50);
    // Wayfire's calc_slot: is_far_top is `y < quarter_snap_threshold`, and
    // is_far_bottom likewise, while left and right use `<=`.
    assert_eq!(slot(2.0, 49.0), Some("top_left"));
    assert_eq!(slot(2.0, 50.0), Some("left"));
    assert_eq!(slot(998.0, 550.0), Some("right"));
    assert_eq!(slot(998.0, 551.0), Some("bottom_right"));
    assert_eq!(slot(50.0, 2.0), Some("top_left"));
    assert_eq!(slot(51.0, 2.0), Some("top"));
  }

  #[test]
  fn null_buffer_unmaps_the_toplevel_and_a_new_buffer_maps_it_again() {
    use crate::Event;

    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let width = u64::from(compositor.state.active().rect.width);
    let stays = client.toplevel(&mut compositor, "stays");
    client.attach_buffer(&mut compositor, &stays.surface, 64, 64);
    let hides = client.toplevel(&mut compositor, "hides");
    client.attach_buffer(&mut compositor, &hides.surface, 64, 64);
    let stays_id = windows_with(&compositor.state, "stays")[0];
    let hides_id = windows_with(&compositor.state, "hides")[0];
    assert_eq!(layout_tiles(&mut compositor.state).len(), 2);
    compositor.drain_events();

    // xdg-shell: committing a null buffer unmaps the toplevel.
    hides.surface.attach(None, 0, 0);
    hides.surface.commit();
    client.roundtrip(&mut compositor);
    let destroyed = compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::WindowDestroyed { window, .. } => Some(window),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(destroyed, [hides_id]);
    assert!(windows_with(&compositor.state, "hides").is_empty());
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(stays_id.0, width)]
    );
    assert_eq!(
      crate::input::focused_window(&compositor.state),
      Some(stays_id)
    );

    // Remapping starts over: an initial commit, answered with a configure,
    // then a buffer.
    let configures = client.events.configures.len();
    hides.surface.commit();
    client.roundtrip(&mut compositor);
    assert!(
      client.events.configures[configures..]
        .iter()
        .any(|(toplevel, ..)| *toplevel
          == wayland_client::Proxy::id(&hides.toplevel)),
      "the initial commit after the unmap gets a configure"
    );
    client.attach_buffer(&mut compositor, &hides.surface, 64, 64);
    let remapped = windows_with(&compositor.state, "hides");
    assert_eq!(remapped.len(), 1);
    assert_eq!(
      presence(&compositor.state, remapped[0]),
      WindowPresence::Mapped
    );
    assert!(compositor.drain_events().iter().any(|event| matches!(
      event,
      Event::WindowCreated { window, .. } if *window == remapped[0]
    )));
    assert_eq!(layout_tiles(&mut compositor.state).len(), 2);
  }

  #[test]
  fn blocked_new_view_joins_the_layout_when_released() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let width = u64::from(compositor.state.active().rect.width);
    let first = client.toplevel(&mut compositor, "first");
    client.attach_buffer(&mut compositor, &first.surface, 64, 64);
    let first_id = windows_with(&compositor.state, "first")[0];
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, width)]
    );

    compositor.state.pre_map_clients.insert(SUBSCRIBER);
    let second = client.toplevel(&mut compositor, "second");
    let second_id = windows_with(&compositor.state, "second")[0];
    // Wayfire's tile plugin attaches a view on view-mapped, so a view
    // that will be held takes no tile before its first buffer either.
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, width)]
    );
    client.attach_buffer(&mut compositor, &second.surface, 64, 64);
    assert!(compositor.state.delayed_maps.contains(&second_id));
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, width)]
    );
    assert_eq!(
      client.events.last_configure(&first.toplevel),
      Some((width as i32, compositor.state.active().rect.height as i32)),
      "the neighbour of a blocked view keeps its size"
    );
    // The blocked view still answers state queries.
    let info = crate::ipc::respond(
      &mut compositor.state,
      &serde_json::json!({
        "method": "window-rules/view-info",
        "data": {"id": second_id.0},
      }),
    );
    assert_eq!(info["info"]["app-id"], "second", "{info}");

    // A second client with the same app ID gets its own window instead of
    // claiming the blocked one.
    let third = client.toplevel(&mut compositor, "second");
    client.attach_buffer(&mut compositor, &third.surface, 64, 64);
    assert_eq!(windows_with(&compositor.state, "second").len(), 2);
    let third_id = windows_with(&compositor.state, "second")[1];
    unblock_map(&mut compositor.state, third_id, SUBSCRIBER).unwrap();
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, width / 2), (third_id.0, width / 2)]
    );

    unblock_map(&mut compositor.state, second_id, SUBSCRIBER).unwrap();
    client.roundtrip(&mut compositor);
    assert_eq!(
      presence(&compositor.state, second_id),
      WindowPresence::Mapped
    );
    // The released view joins the tree last, after the view that mapped
    // while it was blocked.
    let tiles = layout_tiles(&mut compositor.state);
    assert_eq!(
      tiles.iter().map(|tile| tile.0).collect::<Vec<_>>(),
      vec![first_id.0, third_id.0, second_id.0]
    );
    assert_eq!(tiles.iter().map(|tile| tile.1).sum::<u64>(), width);
    assert!(tiles.iter().all(|tile| tile.1.abs_diff(width / 3) <= 1));
    assert_eq!(
      client
        .events
        .last_configure(&second.toplevel)
        .map(|size| size.0 as u64),
      Some(tiles[2].1)
    );
  }

  #[test]
  fn view_blocked_after_creation_gives_its_tile_back() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let width = u64::from(compositor.state.active().rect.width);
    let first = client.toplevel(&mut compositor, "first");
    client.attach_buffer(&mut compositor, &first.surface, 64, 64);
    let first_id = windows_with(&compositor.state, "first")[0];

    // Subscribing between the app ID and the first buffer still blocks.
    let second = client.toplevel(&mut compositor, "second");
    compositor.state.pre_map_clients.insert(SUBSCRIBER);
    client.attach_buffer(&mut compositor, &second.surface, 64, 64);
    let second_id = windows_with(&compositor.state, "second")[0];
    assert!(compositor.state.delayed_maps.contains(&second_id));
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, width)]
    );

    // The subscriber leaving releases the view, as unblock-map does.
    compositor.state.pre_map_clients.remove(&SUBSCRIBER);
    unblock_map(&mut compositor.state, second_id, SUBSCRIBER).unwrap();
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, width / 2), (second_id.0, width / 2)]
    );
  }

  /// The sizes `toplevel` was configured with, in order.
  fn configured_sizes(
    client: &TestClient,
    xdg: &wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel,
  ) -> Vec<(i32, i32)> {
    let id = wayland_client::Proxy::id(xdg);
    client
      .events
      .configures
      .iter()
      .filter(|(object, ..)| *object == id)
      .map(|(_, width, height)| (*width, *height))
      .collect()
  }

  #[test]
  fn a_new_toplevel_chooses_its_size_and_tiles_when_it_maps() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let width = compositor.state.active().rect.width;
    let first = client.toplevel(&mut compositor, "first");
    client.attach_buffer(&mut compositor, &first.surface, 320, 240);
    let first_id = windows_with(&compositor.state, "first")[0];
    let second = client.toplevel(&mut compositor, "second");
    // As in Wayfire, the initial configure is 0x0, and the window that
    // has not mapped takes no tile from the one beside it yet.
    assert_eq!(configured_sizes(&client, &second.toplevel), [(0, 0)]);
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![(first_id.0, u64::from(width))]
    );
    client.attach_buffer(&mut compositor, &second.surface, 320, 240);
    let second_id = windows_with(&compositor.state, "second")[0];
    assert_eq!(
      layout_tiles(&mut compositor.state),
      vec![
        (first_id.0, u64::from(width / 2)),
        (second_id.0, u64::from(width / 2))
      ]
    );
    let height = compositor.state.active().rect.height as i32;
    assert_eq!(
      configured_sizes(&client, &second.toplevel),
      [(0, 0), (width as i32 / 2, height)]
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_new_window_opens_on_the_output_active_when_its_toplevel_was_made() {
    use wayland_client::protocol::wl_compositor::WlCompositor;
    use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor
      .add_output(
        crate::OutputInfo {
          name: "HEADLESS-2".into(),
          ..crate::OutputInfo::default()
        },
        (800, 600),
      )
      .expect("a second output");
    let first = compositor.state.active_output.clone();
    let second = shingle_core::OutputId::new("HEADLESS-2");
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let wm_base = client.bind::<XdgWmBase>(1);
    // A toplevel made while the first output is active, without an app ID
    // yet, and another made after the focus moved to the second.
    let early_surface = wl_compositor.create_surface(&client.handle, ());
    let early_xdg = wm_base.get_xdg_surface(&early_surface, &client.handle, ());
    let early = early_xdg.get_toplevel(&client.handle, ());
    early_surface.commit();
    client.roundtrip(&mut compositor);
    assert!(crate::output::focus_output(&mut compositor.state, &second));
    let late = client.toplevel(&mut compositor, "late");
    early.set_app_id("early".into());
    client.roundtrip(&mut compositor);
    client.attach_buffer(&mut compositor, &early_surface, 64, 64);
    client.attach_buffer(&mut compositor, &late.surface, 64, 64);
    let output_of = |compositor: &crate::Compositor, app_id| {
      let window = windows_with(&compositor.state, app_id)[0];
      compositor
        .state
        .core
        .window_location(window)
        .unwrap()
        .output
    };
    // As in Wayfire, where a view takes the focused output when it is
    // made, not when it maps.
    assert_eq!(output_of(&compositor, "early"), first);
    assert_eq!(output_of(&compositor, "late"), second);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn untiling_a_view_tiled_from_its_map_gives_back_its_own_size() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.placement_mode = "none".into();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "own");
    client.attach_buffer(&mut compositor, &view.surface, 320, 240);
    let window = windows_with(&compositor.state, "own")[0];
    // It mapped at 320x240 and then took the whole output as its tile.
    let tile = compositor
      .state
      .core
      .window_layout(window)
      .unwrap()
      .geometry;
    assert_eq!(tile, compositor.state.active().rect);
    client.attach_buffer(
      &mut compositor,
      &view.surface,
      tile.width as i32,
      tile.height as i32,
    );
    super::toggle_tiling(&mut compositor.state, window).unwrap();
    client.roundtrip(&mut compositor);
    // Untiled, it floats at the size it mapped with, where Wayfire
    // without its place plugin puts a view: 100, 100 into its output.
    assert_eq!(
      compositor
        .state
        .core
        .window_info(window)
        .unwrap()
        .state
        .floating,
      Some(Rect::new(100, 100, 320, 240))
    );
    assert_eq!(
      client.events.last_configure(&view.toplevel),
      Some((320, 240))
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn untiling_centres_the_own_size_under_the_place_plugins_default() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "centred");
    client.attach_buffer(&mut compositor, &view.surface, 320, 240);
    let window = windows_with(&compositor.state, "centred")[0];
    super::toggle_tiling(&mut compositor.state, window).unwrap();
    let area = compositor.state.active().rect;
    assert_eq!(
      compositor
        .state
        .core
        .window_info(window)
        .unwrap()
        .state
        .floating,
      Some(Rect::new(
        area.x + (area.width as i32 - 320) / 2,
        area.y + (area.height as i32 - 240) / 2,
        320,
        240
      ))
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_view_outside_tile_by_default_floats_at_its_own_size() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.tile_by_default =
      shingle_config::ViewMatcher::try_from("none".to_owned()).unwrap();
    compositor.state.placement_mode = "none".into();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "floats");
    client.attach_buffer(&mut compositor, &view.surface, 320, 240);
    let window = windows_with(&compositor.state, "floats")[0];
    assert_eq!(
      compositor
        .state
        .core
        .window_info(window)
        .unwrap()
        .state
        .floating,
      Some(Rect::new(100, 100, 320, 240))
    );
    assert!(layout_tiles(&mut compositor.state).is_empty());
    // Never configured to a tile's size.
    assert!(
      configured_sizes(&client, &view.toplevel)
        .iter()
        .all(|size| *size == (0, 0) || *size == (320, 240))
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_floating_view_larger_than_the_workarea_is_clamped_to_it() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.tile_by_default =
      shingle_config::ViewMatcher::try_from("none".to_owned()).unwrap();
    compositor.state.placement_mode = "none".into();
    let mut client = TestClient::connect(&mut compositor, &path);
    let area = compositor.state.active().rect;
    let view = client.toplevel(&mut compositor, "large");
    client.attach_buffer(&mut compositor, &view.surface, 4000, 200);
    let window = windows_with(&compositor.state, "large")[0];
    assert_eq!(
      compositor
        .state
        .core
        .window_info(window)
        .unwrap()
        .state
        .floating,
      Some(Rect::new(area.x, area.y + 100, area.width, 200))
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn blocked_view_outside_tile_by_default_floats() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.tile_by_default =
      shingle_config::ViewMatcher::try_from("none".to_owned()).unwrap();
    compositor.state.pre_map_clients.insert(SUBSCRIBER);
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "float");
    client.attach_buffer(&mut compositor, &view.surface, 200, 100);
    let window = windows_with(&compositor.state, "float")[0];
    let floating = compositor.state.core.window_info(window).unwrap();
    assert_eq!(
      floating
        .state
        .floating
        .map(|rect| (rect.width, rect.height)),
      Some((200, 100))
    );

    unblock_map(&mut compositor.state, window, SUBSCRIBER).unwrap();
    assert_eq!(presence(&compositor.state, window), WindowPresence::Mapped);
    assert!(layout_tiles(&mut compositor.state).is_empty());
    let info = compositor.state.core.window_info(window).unwrap();
    assert_eq!(info.state.floating, floating.state.floating);
  }

  /// The floating rectangle of the one window with `app_id`.
  fn floating_of(state: &CompositorState, app_id: &str) -> Option<Rect> {
    let window = windows_with(state, app_id)[0];
    state.core.window_info(window).unwrap().state.floating
  }

  #[test]
  fn an_xdg_dialog_floats_centred_on_its_parent() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let main = client.toplevel(&mut compositor, "main");
    client.attach_buffer(&mut compositor, &main.surface, 640, 480);
    let parent = windows_with(&compositor.state, "main")[0];
    let dialog = client.toplevel(&mut compositor, "dialog");
    dialog.toplevel.set_parent(Some(&main.toplevel));
    client.roundtrip(&mut compositor);
    client.attach_buffer(&mut compositor, &dialog.surface, 200, 100);
    // Only the parent tiles, as Wayfire's can_tile_view refuses a view
    // with a parent.
    let tiles = layout_tiles(&mut compositor.state);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].0, parent.0);
    let parent_geometry = compositor
      .state
      .core
      .window_layout(parent)
      .unwrap()
      .geometry;
    let floating = floating_of(&compositor.state, "dialog").unwrap();
    assert_eq!((floating.width, floating.height), (200, 100));
    assert_eq!(
      (floating.x + 100, floating.y + 50,),
      (
        parent_geometry.x + parent_geometry.width as i32 / 2,
        parent_geometry.y + parent_geometry.height as i32 / 2,
      )
    );
    assert_eq!(
      client.events.last_configure(&dialog.toplevel),
      Some((200, 100))
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_dialog_parented_before_its_first_commit_chooses_its_size() {
    use wayland_client::protocol::wl_compositor::WlCompositor;
    use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let main = client.toplevel(&mut compositor, "main");
    client.attach_buffer(&mut compositor, &main.surface, 640, 480);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let wm_base = client.bind::<XdgWmBase>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &client.handle, ());
    let toplevel = xdg_surface.get_toplevel(&client.handle, ());
    toplevel.set_app_id("dialog".into());
    toplevel.set_parent(Some(&main.toplevel));
    surface.commit();
    client.roundtrip(&mut compositor);
    // As in Wayfire, the initial configure of a view that will not tile
    // leaves the size to the client.
    assert_eq!(client.events.last_configure(&toplevel), Some((0, 0)));
    client.attach_buffer(&mut compositor, &surface, 200, 100);
    let floating = floating_of(&compositor.state, "dialog").unwrap();
    assert_eq!((floating.width, floating.height), (200, 100));
    assert_eq!(layout_tiles(&mut compositor.state).len(), 1);
    assert!(matches!(
      client.events.last_configure(&toplevel),
      Some((0, 0) | (200, 100))
    ));
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_tiled_xdg_toplevel_given_a_parent_leaves_its_tile() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let main = client.toplevel(&mut compositor, "main");
    client.attach_buffer(&mut compositor, &main.surface, 640, 480);
    let late = client.toplevel(&mut compositor, "late");
    client.attach_buffer(&mut compositor, &late.surface, 300, 200);
    assert_eq!(layout_tiles(&mut compositor.state).len(), 2);
    late.toplevel.set_parent(Some(&main.toplevel));
    client.roundtrip(&mut compositor);
    assert_eq!(layout_tiles(&mut compositor.state).len(), 1);
    let floating = floating_of(&compositor.state, "late").unwrap();
    assert_eq!((floating.width, floating.height), (300, 200));
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_window_geometry_places_the_surface_around_the_view() {
    use super::around_window_geometry;
    // GTK's 45-pixel shadows around a 1232x672 tile.
    assert_eq!(
      around_window_geometry(
        Rect::new(24, 24, 1232, 672),
        Rect::new(45, 45, 1232, 672),
        (1322, 762),
      ),
      Rect::new(-21, -21, 1322, 762)
    );
    // A client still at its old size is scaled as a whole.
    assert_eq!(
      around_window_geometry(
        Rect::new(0, 0, 200, 100),
        Rect::new(10, 10, 100, 50),
        (120, 70),
      ),
      Rect::new(-20, -20, 240, 140)
    );
    assert_eq!(
      around_window_geometry(
        Rect::new(5, 5, 10, 10),
        Rect::new(0, 0, 0, 0),
        (10, 10)
      ),
      Rect::new(5, 5, 10, 10)
    );
  }

  #[test]
  fn a_toplevels_window_geometry_applies_on_commit() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "shadowed");
    // The initial configure leaves the size to the client, and the tile's
    // comes once it maps.
    assert_eq!(client.events.last_configure(&view.toplevel), Some((0, 0)));
    client.attach_buffer(&mut compositor, &view.surface, 64, 64);
    let (width, height) = client
      .events
      .last_configure(&view.toplevel)
      .expect("the tile's configure");
    // The client draws its configured size plus 20-pixel shadows, as GTK
    // does, and names the part inside them its window geometry.
    view._xdg_surface.set_window_geometry(20, 20, width, height);
    client.attach_buffer(
      &mut compositor,
      &view.surface,
      width + 40,
      height + 40,
    );
    let window = windows_with(&compositor.state, "shadowed")[0];
    let object = compositor
      .state
      .surfaces
      .iter()
      .find_map(|(object, surface)| {
        surface
          .role
          .toplevel()
          .is_some_and(|toplevel| toplevel.window() == Some(window))
          .then(|| object.clone())
      })
      .unwrap();
    let content = super::surface_geometry(&compositor.state, &object).unwrap();
    assert_eq!(
      (content.width as i32, content.height as i32),
      (width, height)
    );
    let drawn = compositor.state.snapshot(&object).unwrap().geometry;
    assert_eq!(
      drawn,
      Rect::new(
        content.x - 20,
        content.y - 20,
        content.width + 40,
        content.height + 40
      )
    );
    // Pending until the next commit.
    view
      ._xdg_surface
      .set_window_geometry(0, 0, width + 40, height + 40);
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.snapshot(&object).unwrap().geometry, drawn);
    view.surface.commit();
    client.roundtrip(&mut compositor);
    let whole = compositor.state.snapshot(&object).unwrap().geometry;
    assert_eq!((whole.x, whole.y), (content.x, content.y));
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn the_pointer_enters_a_shadowed_toplevel_at_its_content() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "shadowed");
    // The initial configure leaves the size to the client, and the tile's
    // comes once it maps.
    assert_eq!(client.events.last_configure(&view.toplevel), Some((0, 0)));
    client.attach_buffer(&mut compositor, &view.surface, 64, 64);
    let (width, height) = client
      .events
      .last_configure(&view.toplevel)
      .expect("the tile's configure");
    view._xdg_surface.set_window_geometry(20, 20, width, height);
    client.attach_buffer(
      &mut compositor,
      &view.surface,
      width + 40,
      height + 40,
    );
    let object = compositor
      .state
      .surfaces
      .iter()
      .find_map(|(object, surface)| {
        (surface.role.toplevel().is_some()).then(|| object.clone())
      })
      .unwrap();
    // As GTK does, an input region that leaves out most of the shadow,
    // set while the pointer is in the shadow.
    crate::input::pointer_motion(&mut compositor.state, 0.0, 0.0);
    let wl_compositor =
      client.bind::<wayland_client::protocol::wl_compositor::WlCompositor>(4);
    let region = wl_compositor.create_region(&client.handle, ());
    region.add(10, 10, width + 20, height + 20);
    view.surface.set_input_region(Some(&region));
    // The region is copied when it is set: GTK destroys it before the
    // commit, which once left the window with an empty input region.
    region.destroy();
    view.surface.commit();
    client.roundtrip(&mut compositor);
    let content = super::surface_geometry(&compositor.state, &object).unwrap();
    let centre = (
      f64::from(content.x) + f64::from(content.width) / 2.0,
      f64::from(content.y) + f64::from(content.height) / 2.0,
    );
    crate::input::pointer_motion(&mut compositor.state, centre.0, centre.1);
    assert_eq!(compositor.state.input.pointer_focus, Some(object.clone()));
    // Surface-local: the content starts 20 pixels into the surface.
    let local = crate::input::local_position(&compositor.state, &object);
    assert_eq!(
      local,
      Some((
        f64::from(width) / 2.0 + 20.0,
        f64::from(height) / 2.0 + 20.0
      ))
    );
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn an_empty_window_geometry_is_a_protocol_error() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "bad");
    view._xdg_surface.set_window_geometry(0, 0, 0, 10);
    let error = client.protocol_error(&mut compositor).expect("an error");
    assert_eq!(error.code, 5);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn a_tiled_toplevels_maximize_request_keeps_its_tile() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let first = client.toplevel(&mut compositor, "first");
    // As inkscape does before it first draws.
    first.toplevel.set_maximized();
    client.roundtrip(&mut compositor);
    client.attach_buffer(&mut compositor, &first.surface, 640, 480);
    let second = client.toplevel(&mut compositor, "second");
    client.attach_buffer(&mut compositor, &second.surface, 640, 480);
    first.toplevel.set_maximized();
    client.roundtrip(&mut compositor);
    let window = windows_with(&compositor.state, "first")[0];
    let info = compositor.state.core.window_info(window).unwrap();
    assert!(!info.state.maximized);
    let tiles = layout_tiles(&mut compositor.state);
    assert_eq!(tiles.len(), 2);
    let width = compositor.state.active().rect.width;
    assert!(tiles.iter().all(|(_, tile)| *tile < u64::from(width)));
    // A floating window still maximizes.
    super::toggle_tiling(&mut compositor.state, window).unwrap();
    first.toplevel.set_maximized();
    client.roundtrip(&mut compositor);
    let info = compositor.state.core.window_info(window).unwrap();
    assert!(info.state.maximized);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn an_xdg_toplevel_of_fixed_size_floats() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let main = client.toplevel(&mut compositor, "main");
    client.attach_buffer(&mut compositor, &main.surface, 640, 480);
    let fixed = client.toplevel(&mut compositor, "fixed");
    fixed.toplevel.set_min_size(320, 240);
    fixed.toplevel.set_max_size(320, 240);
    client.roundtrip(&mut compositor);
    client.attach_buffer(&mut compositor, &fixed.surface, 320, 240);
    assert_eq!(layout_tiles(&mut compositor.state).len(), 1);
    let floating = floating_of(&compositor.state, "fixed").unwrap();
    assert_eq!((floating.width, floating.height), (320, 240));
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn without_simple_tile_views_float_and_cannot_tile() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.tiling = false;
    compositor.state.placement_mode = "none".into();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "float");
    client.attach_buffer(&mut compositor, &view.surface, 200, 100);
    let window = windows_with(&compositor.state, "float")[0];
    assert!(layout_tiles(&mut compositor.state).is_empty());
    let info = compositor.state.core.window_info(window).unwrap();
    let workarea = compositor.state.active().rect;
    // Without place, where a Wayfire view starts: 100, 100 into its
    // output.
    assert_eq!(
      info.state.floating.map(|rect| (rect.x, rect.y, rect.width)),
      Some((workarea.x + 100, workarea.y + 100, 200))
    );
    assert!(matches!(
      super::toggle_tiling(&mut compositor.state, window),
      Err(crate::Error::TilingDisabled)
    ));
    assert!(layout_tiles(&mut compositor.state).is_empty());
  }

  #[test]
  fn a_restored_tile_floats_without_simple_tile() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.tiling = false;
    let output = compositor.state.active_output.clone();
    let placeholder = compositor
      .state
      .core
      .attach_window(&output, LaunchIdentity::new("restore.test"))
      .unwrap();
    compositor
      .state
      .core
      .reconcile_unmapped(placeholder)
      .unwrap();
    let mut client = TestClient::connect(&mut compositor, &path);
    let restored = client.toplevel(&mut compositor, "restore.test");
    client.attach_buffer(&mut compositor, &restored.surface, 64, 64);
    assert_eq!(
      windows_with(&compositor.state, "restore.test"),
      [placeholder]
    );
    assert!(layout_tiles(&mut compositor.state).is_empty());
  }

  #[test]
  fn a_reload_without_simple_tile_floats_the_tiles() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "tiled");
    client.attach_buffer(&mut compositor, &view.surface, 64, 64);
    assert_eq!(layout_tiles(&mut compositor.state).len(), 1);
    compositor
      .apply_config(crate::Config {
        tiling: false,
        ..crate::Config::default()
      })
      .unwrap();
    assert!(layout_tiles(&mut compositor.state).is_empty());
    let window = windows_with(&compositor.state, "tiled")[0];
    assert!(
      compositor
        .state
        .core
        .window_info(window)
        .unwrap()
        .state
        .floating
        .is_some()
    );
  }

  #[test]
  fn blocked_restored_placeholder_keeps_its_slot() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let output = compositor.state.active_output.clone();
    let width = u64::from(compositor.state.active().rect.width);
    let placeholder = compositor
      .state
      .core
      .attach_window(&output, LaunchIdentity::new("restore.test"))
      .unwrap();
    compositor
      .state
      .core
      .reconcile_unmapped(placeholder)
      .unwrap();
    let mut client = TestClient::connect(&mut compositor, &path);
    let first = client.toplevel(&mut compositor, "first");
    client.attach_buffer(&mut compositor, &first.surface, 64, 64);
    let first_id = windows_with(&compositor.state, "first")[0];
    let tiles = vec![(placeholder.0, width / 2), (first_id.0, width / 2)];
    assert_eq!(layout_tiles(&mut compositor.state), tiles);

    compositor.state.pre_map_clients.insert(SUBSCRIBER);
    let restored = client.toplevel(&mut compositor, "restore.test");
    client.attach_buffer(&mut compositor, &restored.surface, 64, 64);
    assert_eq!(
      windows_with(&compositor.state, "restore.test"),
      [placeholder]
    );
    assert!(compositor.state.delayed_maps.contains(&placeholder));
    assert_eq!(
      presence(&compositor.state, placeholder),
      WindowPresence::Placeholder
    );
    assert_eq!(layout_tiles(&mut compositor.state), tiles);

    unblock_map(&mut compositor.state, placeholder, SUBSCRIBER).unwrap();
    assert_eq!(
      presence(&compositor.state, placeholder),
      WindowPresence::Mapped
    );
    assert_eq!(layout_tiles(&mut compositor.state), tiles);
  }

  #[test]
  fn title_before_app_id_claims_the_restored_placeholder() {
    let mut state = crate::input_device::tests::test_state();
    let output = state.active_output.clone();
    let identity = LaunchIdentity {
      app_id: "restore.test".into(),
      launch_id: Some("shingle-restore-1".into()),
      ..LaunchIdentity::default()
    };
    let placeholder = state.core.attach_window(&output, identity).unwrap();
    state.core.reconcile_unmapped(placeholder).unwrap();

    let mut display = Display::<CompositorState>::new().unwrap();
    state.create_globals(&display.handle());
    let (server, client) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    let client_id = display
      .handle()
      .insert_client(
        server,
        Arc::new(ClientDataImpl {
          disconnected: Arc::new(Mutex::new(Vec::new())),
          sandbox: None,
          privileged_protocols: Arc::default(),
          xwayland: false,
        }),
      )
      .unwrap()
      .id();
    // launch::capture names the program, not the app ID, as a real
    // restored client's template does.
    state.launch_templates.insert(
      client_id,
      LaunchIdentity {
        app_id: "python3".into(),
        launch_id: Some("shingle-restore-1".into()),
        ..LaunchIdentity::default()
      },
    );
    let mut wire = WireClient {
      stream: client,
      globals: HashMap::new(),
    };
    // wl_display.get_registry(2).
    wire.send(1, 1, &2_u32.to_ne_bytes());
    roundtrip(&mut display, &mut state);
    wire.read_globals();
    wire.bind("wl_compositor", 4, 3);
    wire.bind("xdg_wm_base", 2, 4);
    // wl_compositor.create_surface(5), xdg_wm_base.get_xdg_surface(6, 5)
    // and xdg_surface.get_toplevel(7).
    wire.send(3, 0, &5_u32.to_ne_bytes());
    let mut args = 6_u32.to_ne_bytes().to_vec();
    args.extend_from_slice(&5_u32.to_ne_bytes());
    wire.send(4, 2, &args);
    wire.send(6, 1, &7_u32.to_ne_bytes());
    // xdg_toplevel.set_title before set_app_id.
    wire.send(7, 2, &string_arg("Restored"));
    roundtrip(&mut display, &mut state);
    assert_eq!(
      state.core.window_ids().collect::<Vec<_>>(),
      vec![placeholder],
      "a title alone must not create a window"
    );
    // xdg_toplevel.set_app_id, then wl_surface.commit.
    wire.send(7, 3, &string_arg("restore.test"));
    wire.send(5, 6, &[]);
    roundtrip(&mut display, &mut state);

    assert_eq!(
      state.core.window_ids().collect::<Vec<_>>(),
      vec![placeholder]
    );
    // The toplevel claimed the placeholder; it stays unmapped until its
    // first buffer.
    assert!(state.surfaces.values().any(|surface| {
      surface
        .role
        .toplevel()
        .is_some_and(|toplevel| toplevel.window() == Some(placeholder))
    }));
    let info = state.core.window_info(placeholder).unwrap();
    assert_eq!(info.identity.title.as_deref(), Some("Restored"));
  }

  /// The configures `toplevel` got from index `from` of the client's
  /// events on, as (width, height).
  fn configures_of(
    client: &TestClient,
    toplevel: &crate::test_client::Toplevel,
    from: usize,
  ) -> Vec<(i32, i32)> {
    let id = wayland_client::Proxy::id(&toplevel.toplevel);
    client.events.configures[from..]
      .iter()
      .filter(|(object, ..)| *object == id)
      .map(|(_, width, height)| (*width, *height))
      .collect()
  }

  #[test]
  fn a_pointer_resize_configures_once_per_turn_and_only_changed_windows() {
    use shingle_core::Direction;

    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let mut toplevels = Vec::new();
    for app_id in ["left", "right", "floater"] {
      let toplevel = client.toplevel(&mut compositor, app_id);
      client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
      toplevels.push(toplevel);
    }
    let floater = windows_with(&compositor.state, "floater")[0];
    let reply = crate::ipc::respond(
      &mut compositor.state,
      &serde_json::json!({
        "method": "window-rules/configure-view",
        "data": {"id": floater.0, "geometry":
          {"x": 900, "y": 400, "width": 300, "height": 200}},
      }),
    );
    assert_eq!(reply["result"], "ok", "{reply}");
    client.roundtrip(&mut compositor);
    let left = windows_with(&compositor.state, "left")[0];
    let width = layout_tiles(&mut compositor.state)[0].1 as i32;
    let from = client.events.configures.len();
    // Eight pointer motions between two frames, as a fast mouse sends.
    for _ in 0..8 {
      crate::input::resize_window(
        &mut compositor.state,
        left,
        Direction::Right,
        5,
        1,
      )
      .expect("the tile resizes");
    }
    client.roundtrip(&mut compositor);
    assert_eq!(
      configures_of(&client, &toplevels[0], from),
      [(width + 40, 720)],
      "one configure with the last size"
    );
    assert_eq!(configures_of(&client, &toplevels[1], from).len(), 1);
    assert_eq!(
      configures_of(&client, &toplevels[2], from),
      [],
      "the floating window, whose size and states stay, gets none"
    );
  }

  #[test]
  fn state_requests_get_a_configure_even_when_nothing_changes() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "solo");
    client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
    client.roundtrip(&mut compositor);
    let from = client.events.configures.len();
    // A tiled window that is not fullscreen asks not to be fullscreen.
    toplevel.toplevel.unset_fullscreen();
    client.roundtrip(&mut compositor);
    assert_eq!(configures_of(&client, &toplevel, from).len(), 1);
    // Nothing else changed, so nothing else is sent.
    let from = client.events.configures.len();
    crate::shell::reconfigure_all(&mut compositor.state);
    client.roundtrip(&mut compositor);
    assert_eq!(configures_of(&client, &toplevel, from), []);
  }
}
