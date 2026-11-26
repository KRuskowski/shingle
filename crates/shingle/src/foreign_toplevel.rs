use std::collections::{HashMap, HashSet};

use shingle_core::WindowId;
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
  zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
  zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ObjectId, protocol::wl_output::WlOutput,
};

use crate::{input, shell, state::CompositorState};

struct Manager {
  resource: ZwlrForeignToplevelManagerV1,
  stopped: bool,
}

struct Handle {
  resource: ZwlrForeignToplevelHandleV1,
  window: Option<WindowId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ForeignWindow {
  pub(crate) title: String,
  pub(crate) app_id: String,
  maximized: bool,
  minimized: bool,
  fullscreen: bool,
  activated: bool,
}

#[derive(Default)]
pub(crate) struct ForeignToplevels {
  managers: HashMap<ObjectId, Manager>,
  handles: HashMap<ObjectId, Handle>,
  known: HashMap<WindowId, ForeignWindow>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle
    .create_global::<CompositorState, ZwlrForeignToplevelManagerV1, ()>(3, ());
}

/// Tells the toplevel handles of `output`'s client that their toplevels
/// left it.
pub(crate) fn output_removed(state: &mut CompositorState, output: &WlOutput) {
  for_client_handles(state, output, |resource| resource.output_leave(output));
}

/// Tells the toplevel handles of `output`'s client that their toplevels
/// are on the newly bound `output`.
pub(crate) fn output_bound(state: &mut CompositorState, output: &WlOutput) {
  for_client_handles(state, output, |resource| resource.output_enter(output));
}

fn for_client_handles(
  state: &CompositorState,
  output: &WlOutput,
  mut send: impl FnMut(&ZwlrForeignToplevelHandleV1),
) {
  let Some(client) = output.client().map(|client| client.id()) else {
    return;
  };
  for handle in state.foreign_toplevels.handles.values() {
    if handle.window.is_some()
      && handle.resource.client().map(|owner| owner.id())
        == Some(client.clone())
    {
      send(&handle.resource);
      handle.resource.done();
    }
  }
}

pub(crate) fn sync(state: &mut CompositorState, handle: &DisplayHandle) {
  let windows = windows(state);
  let current = windows.keys().copied().collect::<HashSet<_>>();
  let removed = state
    .foreign_toplevels
    .known
    .keys()
    .filter(|window| !current.contains(window))
    .copied()
    .collect::<Vec<_>>();
  for window in removed {
    for handle in &mut state.foreign_toplevels.handles.values_mut() {
      if handle.window == Some(window) {
        handle.resource.closed();
        handle.window = None;
      }
    }
    state.foreign_toplevels.known.remove(&window);
  }

  crate::ext_toplevel::sync(state, handle, &windows);
  for (window, info) in windows {
    let previous = state.foreign_toplevels.known.get(&window).cloned();
    if previous.is_none() {
      let managers = state
        .foreign_toplevels
        .managers
        .keys()
        .cloned()
        .collect::<Vec<_>>();
      for manager in managers {
        create_handle(state, handle, &manager, window, &info);
      }
    } else if let Some(previous) = previous.filter(|previous| previous != &info)
    {
      update_handles(state, window, &previous, &info);
    }
    state.foreign_toplevels.known.insert(window, info);
  }
}

pub(crate) fn windows(
  state: &CompositorState,
) -> HashMap<WindowId, ForeignWindow> {
  let focused = input::focused_window(state);
  state
    .surfaces
    .values()
    .filter_map(|surface| match surface.role.toplevel() {
      Some(toplevel) => toplevel.window().map(|window| {
        let leaf = shell::leaf_state(state, window);
        (
          window,
          ForeignWindow {
            title: toplevel.title.clone(),
            app_id: toplevel.app_id.clone(),
            maximized: leaf.is_some_and(|state| state.maximized),
            minimized: leaf.is_some_and(|state| state.minimized),
            fullscreen: leaf.is_some_and(|state| state.fullscreen),
            activated: focused == Some(window),
          },
        )
      }),
      _ => None,
    })
    .collect()
}

fn create_handle(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  manager_id: &ObjectId,
  window: WindowId,
  info: &ForeignWindow,
) {
  let Some(manager) = state.foreign_toplevels.managers.get(manager_id) else {
    return;
  };
  if manager.stopped {
    return;
  }
  let Some(client) = manager.resource.client() else {
    return;
  };
  let Ok(resource) = client
    .create_resource::<ZwlrForeignToplevelHandleV1, (), CompositorState>(
      handle,
      manager.resource.version(),
      (),
    )
  else {
    return;
  };
  manager.resource.toplevel(&resource);
  resource.title(info.title.clone());
  resource.app_id(info.app_id.clone());
  if let Some(output) = state.outputs.bound().find(|output| {
    output.client().as_ref().map(Client::id) == Some(client.id())
  }) {
    resource.output_enter(output);
  }
  send_state(&resource, info);
  resource.done();
  state.foreign_toplevels.handles.insert(
    resource.id(),
    Handle {
      resource,
      window: Some(window),
    },
  );
}

fn update_handles(
  state: &mut CompositorState,
  window: WindowId,
  previous: &ForeignWindow,
  current: &ForeignWindow,
) {
  for handle in state.foreign_toplevels.handles.values_mut() {
    if handle.window != Some(window) {
      continue;
    }
    if previous.title != current.title {
      handle.resource.title(current.title.clone());
    }
    if previous.app_id != current.app_id {
      handle.resource.app_id(current.app_id.clone());
    }
    send_state(&handle.resource, current);
    handle.resource.done();
  }
}

fn send_state(resource: &ZwlrForeignToplevelHandleV1, info: &ForeignWindow) {
  let mut states = Vec::new();
  if info.maximized {
    states.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
  }
  if info.minimized {
    states.push(zwlr_foreign_toplevel_handle_v1::State::Minimized as u32);
  }
  if info.activated {
    states.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
  }
  if info.fullscreen {
    states.push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32);
  }
  resource.state(
    states
      .into_iter()
      .flat_map(u32::to_ne_bytes)
      .collect::<Vec<_>>(),
  );
}

fn close_window(state: &mut CompositorState, window: WindowId) {
  shell::close_window(state, window);
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for CompositorState {
  fn bind(
    state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwlrForeignToplevelManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let manager = data_init.init(resource, ());
    state.foreign_toplevels.managers.insert(
      manager.id(),
      Manager {
        resource: manager,
        stopped: false,
      },
    );
  }

  fn can_view(client: Client, _: &()) -> bool {
    crate::security_context::can_view(
      client,
      "zwlr_foreign_toplevel_manager_v1",
    )
  }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrForeignToplevelManagerV1,
    request: zwlr_foreign_toplevel_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request
      && let Some(manager) =
        state.foreign_toplevels.managers.get_mut(&resource.id())
    {
      manager.stopped = true;
      manager.resource.finished();
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZwlrForeignToplevelManagerV1,
    _data: &(),
  ) {
    state.foreign_toplevels.managers.remove(&resource.id());
  }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrForeignToplevelHandleV1,
    request: zwlr_foreign_toplevel_handle_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let window = state
      .foreign_toplevels
      .handles
      .get(&resource.id())
      .and_then(|handle| handle.window);
    let Some(window) = window else {
      return;
    };
    match request {
      zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
        input::focus_window(state, window);
      }
      zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
        let _ = shell::set_maximized(state, window, true);
      }
      zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
        let _ = shell::set_maximized(state, window, false);
      }
      zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. } => {
        let _ = shell::set_fullscreen(state, window, true);
      }
      zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
        let _ = shell::set_fullscreen(state, window, false);
      }
      zwlr_foreign_toplevel_handle_v1::Request::Close => {
        close_window(state, window)
      }
      zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
        let _ = shell::set_minimized(state, window, true);
      }
      zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
        let _ = shell::set_minimized(state, window, false);
      }
      zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. }
      | zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZwlrForeignToplevelHandleV1,
    _data: &(),
  ) {
    state.foreign_toplevels.handles.remove(&resource.id());
  }
}
