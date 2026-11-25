use std::{collections::BTreeSet, sync::Mutex};

use shingle_config::RuleEvent;
use shingle_core::{LaunchIdentity, LeafState, Rect, WindowId, WindowPresence};
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
    serials: BTreeSet<u32>,
    acked: Option<(u32, u32, u32)>,
  },
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

#[derive(Clone)]
pub(crate) struct ToplevelRole {
  pub(crate) resource: XdgToplevel,
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
  pub(crate) created_rules_pending: Option<bool>,
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

pub(crate) fn create_globals(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, XdgWmBase, ()>(6, ());
  handle.create_global::<CompositorState, ExtSessionLockManagerV1, ()>(1, ());
  crate::dialog::create_global(handle);
  crate::layer_shell::create_global(handle);
}

pub(crate) fn has_live_role(surface: &Surface) -> bool {
  !matches!(surface.role, Role::None | Role::Dead | Role::Cursor)
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
  match &surface.role {
        Role::Xdg {
            kind: Some(XdgKind::Toplevel(toplevel)),
            ..
        } => (
            SurfaceRole::Toplevel,
            toplevel.window(),
            toplevel.parent.and_then(|parent| {
                state
                    .surfaces
                    .values()
                    .find(|surface| {
                        matches!(&surface.role, Role::Xdg { kind: Some(XdgKind::Toplevel(candidate)), .. } if candidate.window == parent)
                    })
                    .map(|surface| surface.id)
            }),
            Some(toplevel.app_id.clone()),
            Some(toplevel.title.clone()),
        ),
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
        Role::SessionLock { .. } => (SurfaceRole::SessionLock, None, None, None, None),
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
    match &surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } => return modifier * toplevel.opacity,
      Role::Xdg {
        kind: Some(XdgKind::Popup(popup)),
        ..
      } => current = popup.parent.clone(),
      Role::Subsurface { parent, .. } => current = parent.clone(),
      _ => return modifier,
    }
  }
}

pub(crate) fn surface_geometry(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  let surface = state.surfaces.get(object)?;
  match &surface.role {
    Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } => {
      if toplevel.window().is_some_and(|window| {
        state
          .core
          .window_info(window)
          .is_ok_and(|info| info.state.fullscreen)
      }) {
        Some(state.output)
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
          .map_or(state.output, |tree| tree.workarea())
          .into()
      } else if !toplevel.tiled {
        Some(toplevel.floating_geometry)
      } else {
        Some(
          toplevel
            .window()
            .and_then(|window| state.core.window_layout(window).ok())
            .map_or(state.output, |layout| layout.geometry),
        )
      }
    }
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
      let parent = surface_geometry(state, parent)?;
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
    Role::SessionLock { .. } => Some(state.output),
    _ => {
      let (width, height) = crate::viewport::dimensions(state, object)
        .unwrap_or_else(|| logical_buffer_dimensions(surface));
      Some(Rect::new(0, 0, width, height))
    }
  }
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
        && matches!(surface.pending.buffer, PendingBuffer::Attach(_))
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
      if let PendingBuffer::Attach(attachment) = &surface.pending.buffer
        && (attachment.width != width || attachment.height != height)
      {
        resource.post_error(
                    ext_session_lock_surface_v1::Error::DimensionsMismatch,
                    "lock surface buffer dimensions must match the acknowledged configure",
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
    matches!(&surface.role, Role::Xdg {
            kind: Some(XdgKind::Toplevel(toplevel)), ..
        } if toplevel.window() == Some(window))
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
  if state.core.mark_window_mapped(window).is_err() {
    return;
  }
  state.pending_maps.remove(&window);
  disable_show_desktop(state);
  state.events.push(Event::WindowCreated { window, surface });
  reconfigure_all(state);
  if state
    .core
    .current_workspace(&state.output_id)
    .is_ok_and(|workspace| {
      state.core.window_info(window).is_ok_and(|info| {
        info.location.output == state.output_id
          && info.location.workspace == workspace
      })
    })
  {
    crate::input::focus_window(state, window);
  }
}

fn request_map(state: &mut CompositorState, object: &ObjectId) {
  let Some((window, has_buffer, mapped)) =
    state
      .surfaces
      .get(object)
      .and_then(|surface| match &surface.role {
        Role::Xdg {
          kind: Some(XdgKind::Toplevel(toplevel)),
          ..
        } => toplevel.window().map(|window| {
          (
            window,
            surface.buffer.is_some(),
            state
              .core
              .window_info(window)
              .is_ok_and(|info| info.presence == WindowPresence::Mapped),
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
    && matches!(&state.surfaces.get(object).map(|surface| &surface.role), Some(Role::Xdg { kind: Some(XdgKind::Toplevel(toplevel)), .. }) if toplevel.window().is_none())
  {
    materialize_toplevel(state, object);
  }
  if has_buffer {
    request_map(state, object);
  }
  let pending = state.surfaces.get_mut(object).and_then(|surface| {
    surface.buffer.as_ref()?;
    match &mut surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } => toplevel.created_rules_pending.take().and_then(|created| {
        toplevel
          .window()
          .map(|window| (created, window, toplevel.identity.clone()))
      }),
      _ => None,
    }
  });
  if let Some((created, window, identity)) = pending {
    if created {
      let properties =
        crate::window_rules::view_properties(state, window, &identity);
      if !state.tile_by_default.matches_view(&properties) {
        let _ = toggle_tiling(state, window);
      }
      crate::placement::place(state, window);
      crate::window_rules::apply_created(state, window, &identity);
    }
    reconfigure_all(state);
    if state
      .core
      .current_workspace(&state.output_id)
      .is_ok_and(|workspace| {
        state.core.window_info(window).is_ok_and(|info| {
          info.presence == WindowPresence::Mapped
            && info.location.output == state.output_id
            && info.location.workspace == workspace
        })
      })
    {
      crate::input::focus_window(state, window);
    }
  }
  crate::layer_shell::committed(state, object);
  if state.idle.lock_state() != LockState::Pending
    || !matches!(
      state.surfaces.get(object).map(|surface| &surface.role),
      Some(Role::SessionLock { .. })
    )
  {
    return;
  }
  let Some(active) = state.active_lock.clone() else {
    return;
  };
  let Some(active_client) = active.client().map(|client| client.id()) else {
    return;
  };
  let ready = state.surfaces.values().any(|surface| {
    surface.client == active_client
      && matches!(surface.role, Role::SessionLock { .. })
      && surface.buffer.as_ref().is_some_and(|buffer| {
        buffer.width == state.output.width
          && buffer.height == state.output.height
          && buffer
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255)
      })
  });
  if ready {
    state.idle.lock_surface_ready(state.now());
    active.locked();
    state.emit_idle_changes();
    crate::input::sync_focus(state);
  }
}

pub(crate) fn destroy_role(state: &mut CompositorState, surface: &Surface) {
  crate::foreign::surface_destroyed(state, &Resource::id(&surface.resource));
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

fn detach_toplevel(
  state: &mut CompositorState,
  surface: SurfaceId,
  role: &Role,
) {
  if let Role::Xdg {
    kind: Some(XdgKind::Toplevel(toplevel)),
    ..
  } = role
  {
    crate::dialog::toplevel_destroyed(state, &Resource::id(&toplevel.resource));
    crate::toplevel_icon::toplevel_destroyed(
      state,
      &Resource::id(&toplevel.resource),
    );
    if let Some(window) = toplevel.window() {
      let mapped = state
        .core
        .window_info(window)
        .is_ok_and(|info| info.presence == WindowPresence::Mapped);
      if state.core.detach_window(window).is_ok() {
        state.pending_maps.remove(&window);
        state.delayed_maps.remove(&window);
        state.focus_recency.remove(&window);
        state.view_properties.remove(&window);
        if mapped {
          state
            .events
            .push(Event::WindowDestroyed { window, surface });
        }
        reconfigure_all(state);
      }
    }
  }
}

/// Updates both the XDG role and persistent core parent relationship.
pub(crate) fn set_toplevel_parent(
  state: &mut CompositorState,
  object: &ObjectId,
  parent: Option<WindowId>,
) {
  if let Some(surface) = state.surfaces.get_mut(object)
    && let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
  {
    toplevel.parent = parent;
  }
  let window =
    state
      .surfaces
      .get(object)
      .and_then(|surface| match &surface.role {
        Role::Xdg {
          kind: Some(XdgKind::Toplevel(toplevel)),
          ..
        } => toplevel.window(),
        _ => None,
      });
  if let Some(window) = window {
    let _ = state.core.set_window_parent(window, parent);
  }
}

fn activated_window(state: &CompositorState) -> Option<WindowId> {
  if let Some(window) = crate::input::focused_window(state) {
    return Some(window);
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
  let workspace = state.core.current_workspace(&state.output_id).ok()?;
  state.core.tree(&state.output_id, workspace).ok()?.focused()
}

pub(crate) fn reconfigure_all(state: &mut CompositorState) {
  let mut targets = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      let Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } = &surface.role
      else {
        return None;
      };
      Some((object.clone(), toplevel.window()))
    })
    .collect::<Vec<_>>();
  targets.sort_by_key(|(_, window)| window.unwrap_or(WindowId(u64::MAX)));
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
      .and_then(|surface| match &surface.role {
        Role::Xdg {
          kind: Some(XdgKind::Toplevel(toplevel)),
          ..
        } => Some((toplevel.tiled, toplevel.floating_geometry)),
        _ => None,
      })
      .unwrap_or((true, state.output));
    let geometry = if fullscreen {
      state.output
    } else if tiled {
      layout.map_or(state.output, |layout| layout.geometry)
    } else if maximized {
      window
        .and_then(|window| state.core.window_location(window).ok())
        .and_then(|location| {
          state.core.tree(&location.output, location.workspace).ok()
        })
        .map_or(state.output, |tree| tree.workarea())
    } else {
      floating_geometry
    };
    let mut states = if tiled {
      vec![
        xdg_toplevel::State::TiledLeft as u32,
        xdg_toplevel::State::TiledRight as u32,
        xdg_toplevel::State::TiledTop as u32,
        xdg_toplevel::State::TiledBottom as u32,
      ]
    } else {
      Vec::new()
    };
    if focused {
      states.push(xdg_toplevel::State::Activated as u32);
    }
    if maximized {
      states.push(xdg_toplevel::State::Maximized as u32);
    }
    if fullscreen {
      states.push(xdg_toplevel::State::Fullscreen as u32);
    }
    let states = states
      .into_iter()
      .flat_map(u32::to_ne_bytes)
      .collect::<Vec<_>>();
    configure_toplevel(state, &object, geometry, states);
  }
  sync_fullscreen_idle(state);
}

fn sync_fullscreen_idle(state: &mut CompositorState) {
  let fullscreen = !state.show_desktop
        && state.surfaces.values().any(|surface| {
            surface.buffer.is_some()
                && matches!(&surface.role, Role::Xdg { kind: Some(XdgKind::Toplevel(toplevel)), .. } if toplevel.window().is_some_and(|window| {
                    state.core.current_workspace(&state.output_id).is_ok_and(|workspace| {
                        state.core.window_info(window).is_ok_and(|info| {
                            info.state.fullscreen
                                && !info.state.minimized
                                && info.location.output == state.output_id
                                && (info.state.sticky || info.location.workspace == workspace)
                        })
                    })
                }))
        });
  let commands = state.idle.set_fullscreen(fullscreen, state.now());
  state.process_idle_commands(commands);
}

fn configure_toplevel(
  state: &mut CompositorState,
  object: &ObjectId,
  geometry: Rect,
  states: Vec<u8>,
) {
  let serial = state.serial();
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::Xdg {
    resource,
    kind: Some(XdgKind::Toplevel(toplevel)),
    serials,
    ..
  } = &mut surface.role
  else {
    return;
  };
  let previous = toplevel.configured_geometry.replace(geometry);
  let window = toplevel.window();
  toplevel.resource.configure(
    geometry.width.min(i32::MAX as u32) as i32,
    geometry.height.min(i32::MAX as u32) as i32,
    states,
  );
  resource.configure(serial);
  serials.insert(serial);
  if let (Some(window), Some(old_geometry)) = (window, previous)
    && old_geometry != geometry
  {
    state.events.push(Event::GeometryChanged {
      window,
      old_geometry,
    });
  }
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
  let serial = state.serial();
  let output = state.output;
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::SessionLock {
    resource, serials, ..
  } = &mut surface.role
  else {
    return;
  };
  resource.configure(serial, output.width, output.height);
  serials.insert(serial);
}

fn update_app_id(
  state: &mut CompositorState,
  object: &ObjectId,
  app_id: String,
) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::Xdg {
    kind: Some(XdgKind::Toplevel(toplevel)),
    ..
  } = &mut surface.role
  else {
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

fn update_title(state: &mut CompositorState, object: &ObjectId, title: String) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::Xdg {
    kind: Some(XdgKind::Toplevel(toplevel)),
    ..
  } = &mut surface.role
  else {
    return;
  };
  toplevel.title = title.clone();
  toplevel.identity.title = Some(title.clone());
  let surface = surface.id;
  if let Some(window) = materialize_toplevel(state, object) {
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
  let (existing, identity, parent) =
    state
      .surfaces
      .get(object)
      .and_then(|surface| match &surface.role {
        Role::Xdg {
          kind: Some(XdgKind::Toplevel(toplevel)),
          ..
        } => Some((
          toplevel.window(),
          toplevel.identity.clone(),
          toplevel.parent,
        )),
        _ => None,
      })?;
  if let Some(window) = existing {
    return Some(window);
  }
  let (window, created) = match state.core.reconcile_mapped(&identity) {
    Some(window) => (window, false),
    None => {
      let workspace = state.core.current_workspace(&state.output_id).ok()?;
      let window = state
        .core
        .attach_window_with_state_and_parent(
          &state.output_id,
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
          },
          parent,
        )
        .ok()?;
      (window, true)
    }
  };
  state.core.reconcile_unmapped(window).ok()?;
  let restored_state =
    state.core.window_info(window).ok().map(|info| info.state);
  let restored_parent = state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| info.parent);
  if let Some(surface) = state.surfaces.get_mut(object)
    && let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
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
  reconfigure_all(state);
  request_map(state, object);
  Some(window)
}

pub(crate) fn size_constraints(
  state: &CompositorState,
  window: WindowId,
) -> Option<((i32, i32), (i32, i32))> {
  state
    .surfaces
    .values()
    .find_map(|surface| match &surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } if toplevel.window() == Some(window) => {
        Some((toplevel.min_size, toplevel.max_size))
      }
      _ => None,
    })
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
    .current_workspace(&state.output_id)
    .ok()
    .and_then(|workspace| state.core.tree(&state.output_id, workspace).ok())
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
  state.core.set_maximized(window, maximized)?;
  reconfigure_all(state);
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
  state.core.set_fullscreen(window, fullscreen)?;
  reconfigure_all(state);
  state
    .events
    .push(Event::FullscreenChanged { window, fullscreen });
  if fullscreen {
    crate::window_rules::apply_event(state, window, RuleEvent::Fullscreened);
  }
  Ok(())
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

pub(crate) fn set_sticky(
  state: &mut CompositorState,
  window: WindowId,
  sticky: bool,
) -> Result<(), crate::Error> {
  state.core.set_sticky(window, sticky)?;
  reconfigure_all(state);
  state.events.push(Event::StickyChanged { window, sticky });
  Ok(())
}

pub(crate) fn window_alpha(
  state: &CompositorState,
  window: WindowId,
) -> Option<f32> {
  state
    .surfaces
    .values()
    .find_map(|surface| match &surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } if toplevel.window() == Some(window) => Some(toplevel.opacity),
      _ => None,
    })
}

pub(crate) fn clamp_alpha(state: &mut CompositorState, minimum: f32) {
  let windows = state
    .surfaces
    .values()
    .filter_map(|surface| match &surface.role {
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(toplevel)),
        ..
      } => toplevel.window().filter(|_| toplevel.opacity < minimum),
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
    if let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
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
    .current_workspace(&state.output_id)
    .ok()
    .and_then(|workspace| state.core.tree(&state.output_id, workspace).ok())
    .map_or(state.output, |tree| tree.workarea());
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
    if let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
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
        matches!(
            &surface.role,
            Role::Xdg {
                kind: Some(XdgKind::Toplevel(toplevel)),
                ..
            } if toplevel.window() == Some(window)
        )
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
  let far_top = y <= corner;
  let far_bottom = height - y <= corner;
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

pub(crate) fn sync_floating_geometry(
  state: &mut CompositorState,
  window: WindowId,
  geometry: Rect,
) {
  for surface in state.surfaces.values_mut() {
    if let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
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
  let old_edges = state.core.window_info(window)?.state.tiled_edges;
  state.core.set_floating(window, Some(geometry))?;
  let mut detached = None;
  for surface in state.surfaces.values_mut() {
    if let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
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
  }
  reconfigure_all(state);
  Ok(())
}

pub(crate) fn toggle_tiling(
  state: &mut CompositorState,
  window: WindowId,
) -> Result<bool, crate::Error> {
  let Some((object, tiled)) =
    state
      .surfaces
      .iter()
      .find_map(|(object, surface)| match &surface.role {
        Role::Xdg {
          kind: Some(XdgKind::Toplevel(toplevel)),
          ..
        } if toplevel.window() == Some(window) => {
          Some((object.clone(), toplevel.tiled))
        }
        _ => None,
      })
  else {
    return Err(shingle_core::ShingleError::WindowNotFound(window.0).into());
  };
  let geometry = if tiled {
    let info = state.core.window_info(window)?;
    Some(
      info
        .state
        .restore_geometry
        .unwrap_or(state.core.window_layout(window)?.geometry),
    )
  } else {
    None
  };
  let old_edges = state.core.window_info(window)?.state.tiled_edges;
  state.core.set_floating(window, geometry)?;
  if let Some(surface) = state.surfaces.get_mut(&object)
    && let Role::Xdg {
      kind: Some(XdgKind::Toplevel(toplevel)),
      ..
    } = &mut surface.role
  {
    toplevel.tiled = !tiled;
    if let Some(geometry) = geometry {
      toplevel.floating_geometry = geometry;
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

pub(crate) fn assign_pending_popup(
  state: &mut CompositorState,
  popup: &XdgPopup,
  parent: &ObjectId,
) {
  let object = state.surfaces.iter().find_map(|(object, surface)| {
        matches!(&surface.role, Role::Xdg { kind: Some(XdgKind::PendingPopup { resource, .. }), .. } if resource == popup)
            .then_some(object.clone())
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
            matches!(&surface.role, Role::Xdg { resource: candidate, .. } if candidate == resource)
                .then_some(object.clone())
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
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::Xdg {
            resource: resource.clone(),
            kind: Some(XdgKind::Toplevel(Box::new(ToplevelRole {
              resource: toplevel,
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
              created_rules_pending: None,
            }))),
            serials: BTreeSet::new(),
            initial_acked: false,
          };
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
                        matches!(&surface.role, Role::Xdg { resource: candidate, .. } if candidate == &parent)
                            .then_some(object.clone())
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
      xdg_surface::Request::SetWindowGeometry { .. } => {}
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
            matches!(&surface.role, Role::Xdg { kind: Some(XdgKind::Toplevel(toplevel)), .. } if &toplevel.resource == resource)
                .then_some(object.clone())
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
          state
            .surfaces
            .values()
            .find_map(|surface| match &surface.role {
              Role::Xdg {
                kind: Some(XdgKind::Toplevel(toplevel)),
                ..
              } if toplevel.resource == parent => toplevel.window(),
              _ => None,
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
          && let Role::Xdg {
            kind: Some(XdgKind::Toplevel(toplevel)),
            ..
          } = &mut surface.role
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
        let _ = set_maximized(state, window, maximized);
      }
      xdg_toplevel::Request::SetFullscreen { .. }
      | xdg_toplevel::Request::UnsetFullscreen => {
        let fullscreen =
          matches!(request, xdg_toplevel::Request::SetFullscreen { .. });
        let Some(window) = materialize_toplevel(state, &object) else {
          return;
        };
        let _ = set_fullscreen(state, window, fullscreen);
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
            matches!(&surface.role, Role::Xdg { kind: Some(XdgKind::Popup(PopupRole { resource: candidate, .. })), .. } if candidate == resource)
                .then_some(object.clone())
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
    let output = state.output;
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
        state.idle.request_lock(state.now());
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
        let duplicate = state
          .surfaces
          .values()
          .any(|surface| matches!(surface.role, Role::SessionLock { .. }));
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
        if !state.outputs.contains_key(&output.id()) {
          resource.post_error(
            ext_session_lock_v1::Error::DuplicateOutput,
            "unknown or removed output",
          );
          return;
        }
        if let Some(surface) = state.surfaces.get_mut(&object) {
          surface.role = Role::SessionLock {
            resource: lock_surface,
            serials: BTreeSet::new(),
            acked: None,
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
            matches!(&surface.role, Role::SessionLock { resource: candidate, .. } if candidate == resource)
                .then_some(object.clone())
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
          if !serials.remove(&serial) {
            resource.post_error(
              ext_session_lock_surface_v1::Error::InvalidSerial,
              "configure serial was not sent or was already acknowledged",
            );
          } else {
            serials.retain(|candidate| *candidate > serial);
            *acked = Some((serial, state.output.width, state.output.height));
          }
        }
      }
      _ => {}
    }
  }
}
