//! wlr-foreign-toplevel-management: a handle per window for task bars,
//! with `output_enter` for the output that shows the window's workspace
//! set and `output_leave` when it moves or that output goes
//! (docs/design/multi-output.md, section 8).

use std::collections::{HashMap, HashSet};

use shingle_core::{OutputId, WindowId};
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
  /// The `wl_output`s the handle was told its toplevel is on.
  entered: HashMap<ObjectId, WlOutput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ForeignWindow {
  pub(crate) title: String,
  pub(crate) app_id: String,
  maximized: bool,
  minimized: bool,
  fullscreen: bool,
  activated: bool,
  /// The output that shows the window's workspace set, if one does.
  output: Option<OutputId>,
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

/// Tells the toplevel handles on `output`, a binding of an output that
/// went, that their toplevels left it.
pub(crate) fn output_removed(state: &mut CompositorState, output: &WlOutput) {
  for handle in state.foreign_toplevels.handles.values_mut() {
    if handle.entered.remove(&output.id()).is_some() {
      handle.resource.output_leave(output);
      handle.resource.done();
    }
  }
}

/// Sends handle `id` `output_enter` for its client's bindings of its
/// window's output and `output_leave` for those it left; returns whether
/// it sent anything.
fn sync_handle_outputs(
  state: &mut CompositorState,
  id: &ObjectId,
  output: Option<&OutputId>,
) -> bool {
  let Some(client) = state
    .foreign_toplevels
    .handles
    .get(id)
    .and_then(|handle| handle.resource.client())
  else {
    return false;
  };
  let bound = output
    .and_then(|output| state.outputs.get(output))
    .into_iter()
    .flat_map(|output| output.bound.iter())
    .filter(|(_, output)| {
      output
        .client()
        .is_some_and(|owner| owner.id() == client.id())
    })
    .map(|(id, output)| (id.clone(), output.clone()))
    .collect::<HashMap<_, _>>();
  let Some(handle) = state.foreign_toplevels.handles.get_mut(id) else {
    return false;
  };
  let mut changed = false;
  handle.entered.retain(|id, output| {
    if bound.contains_key(id) {
      return true;
    }
    if output.is_alive() {
      handle.resource.output_leave(output);
    }
    changed = true;
    false
  });
  for (id, output) in bound {
    if let std::collections::hash_map::Entry::Vacant(entry) =
      handle.entered.entry(id)
    {
      handle.resource.output_enter(&output);
      entry.insert(output);
      changed = true;
    }
  }
  changed
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
  let handles = state
    .foreign_toplevels
    .handles
    .iter()
    .filter_map(|(id, handle)| {
      let window = handle.window?;
      let output = state.foreign_toplevels.known.get(&window)?.output.clone();
      Some((id.clone(), output))
    })
    .collect::<Vec<_>>();
  for (id, output) in handles {
    if sync_handle_outputs(state, &id, output.as_ref())
      && let Some(handle) = state.foreign_toplevels.handles.get(&id)
    {
      handle.resource.done();
    }
  }
}

/// The output that shows `window`'s workspace set, if one does.
fn window_output(
  state: &CompositorState,
  window: WindowId,
) -> Option<OutputId> {
  let info = state.core.window_info(window).ok()?;
  let output = info.location.output;
  (state.outputs.get(&output).is_some()
    && state.core.wset_of(&output) == Some(info.wset))
  .then_some(output)
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
            output: window_output(state, window),
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
  send_state(&resource, info);
  let id = resource.id();
  state.foreign_toplevels.handles.insert(
    id.clone(),
    Handle {
      resource: resource.clone(),
      window: Some(window),
      entered: HashMap::new(),
    },
  );
  sync_handle_outputs(state, &id, info.output.as_ref());
  resource.done();
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
      zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
        if let Some(output) = output {
          shell::fullscreen_on(state, window, &output);
        }
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
