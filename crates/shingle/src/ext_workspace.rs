//! ext-workspace-v1: one workspace group per output, holding the
//! workspaces of the workspace set it shows (docs/design/multi-output.md,
//! section 8). Workspace ids carry the output name, so a renamed output
//! gets new workspaces, as a replaced output does in Wayfire; when an
//! output shows another set, its group's workspaces are made again.

use std::collections::{HashMap, HashSet};

use shingle_core::{OutputId, WsetId};
use wayland_protocols::ext::workspace::v1::server::{
  ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
  ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
  ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
  protocol::wl_output::WlOutput,
};

use crate::state::CompositorState;

const ACTIVATE_CAPABILITY: ext_workspace_handle_v1::WorkspaceCapabilities =
  ext_workspace_handle_v1::WorkspaceCapabilities::Activate;
const ACTIVE_STATE: ext_workspace_handle_v1::State =
  ext_workspace_handle_v1::State::Active;

struct Manager {
  resource: ExtWorkspaceManagerV1,
  /// The workspace to show at the next commit: its output and index.
  pending_activation: Option<(OutputId, usize)>,
  grid: (u32, u32),
  stopped: bool,
}

struct Group {
  resource: ExtWorkspaceGroupHandleV1,
  manager: ObjectId,
  /// The output whose workspaces the group holds.
  output: OutputId,
  /// The workspace set the output showed when the workspaces were made.
  wset: Option<WsetId>,
  /// The `wl_output`s the group entered.
  outputs: HashMap<ObjectId, WlOutput>,
}

struct Workspace {
  resource: ExtWorkspaceHandleV1,
  manager: ObjectId,
  group: ObjectId,
  index: usize,
  active: bool,
}

#[derive(Default)]
pub(crate) struct ExtWorkspaces {
  managers: HashMap<ObjectId, Manager>,
  groups: HashMap<ObjectId, Group>,
  workspaces: HashMap<ObjectId, Workspace>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ExtWorkspaceManagerV1, ()>(1, ());
}

/// Synchronizes groups with the outputs, their workspace sets and the
/// client's bindings, active state, and grid changes.
pub(crate) fn sync(state: &mut CompositorState, handle: &DisplayHandle) {
  let grid = (state.workspace_width, state.workspace_height);
  let managers = state
    .ext_workspaces
    .managers
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for manager_id in managers {
    let Some(manager) = state.ext_workspaces.managers.get(&manager_id) else {
      continue;
    };
    if manager.stopped {
      continue;
    }
    let mut dirty = false;
    if manager.grid != grid {
      let groups = groups_of(state, &manager_id);
      for group in groups {
        remove_group(state, &group);
      }
      if let Some(manager) = state.ext_workspaces.managers.get_mut(&manager_id)
      {
        manager.grid = grid;
      }
      dirty = true;
    }
    // Groups of outputs that are gone or show another set.
    for group_id in groups_of(state, &manager_id) {
      let Some(group) = state.ext_workspaces.groups.get(&group_id) else {
        continue;
      };
      let Some(_) = state.outputs.get(&group.output) else {
        remove_group(state, &group_id);
        dirty = true;
        continue;
      };
      let wset = state.core.wset_of(&group.output);
      if group.wset != wset {
        remove_workspaces(state, &group_id);
        create_workspaces(state, handle, &manager_id, &group_id);
        if let Some(group) = state.ext_workspaces.groups.get_mut(&group_id) {
          group.wset = wset;
        }
        dirty = true;
      }
    }
    // Outputs without a group.
    let outputs = state
      .outputs
      .iter()
      .map(|output| output.id.clone())
      .collect::<Vec<_>>();
    for output in outputs {
      let has_group = state
        .ext_workspaces
        .groups
        .values()
        .any(|group| group.manager == manager_id && group.output == output);
      if !has_group {
        create_group(state, handle, &manager_id, &output);
        dirty = true;
      }
    }
    dirty |= sync_outputs(state, &manager_id);
    dirty |= sync_active(state, &manager_id);
    if dirty
      && let Some(manager) = state.ext_workspaces.managers.get(&manager_id)
    {
      manager.resource.done();
    }
  }
}

/// Tells workspace groups that `output` is gone.
pub(crate) fn output_removed(state: &mut CompositorState, output: &WlOutput) {
  let mut dirty = HashSet::new();
  for group in state.ext_workspaces.groups.values_mut() {
    if group.outputs.remove(&output.id()).is_some() {
      group.resource.output_leave(output);
      dirty.insert(group.manager.clone());
    }
  }
  for manager in dirty {
    if let Some(manager) = state.ext_workspaces.managers.get(&manager) {
      manager.resource.done();
    }
  }
}

/// The groups of manager `manager_id`.
fn groups_of(state: &CompositorState, manager_id: &ObjectId) -> Vec<ObjectId> {
  state
    .ext_workspaces
    .groups
    .iter()
    .filter(|(_, group)| &group.manager == manager_id)
    .map(|(id, _)| id.clone())
    .collect()
}

/// Sends each group of `manager_id` `output_enter` for the client's
/// bindings of its output, and `output_leave` for those that went.
fn sync_outputs(state: &mut CompositorState, manager_id: &ObjectId) -> bool {
  let Some(client) = state
    .ext_workspaces
    .managers
    .get(manager_id)
    .and_then(|manager| manager.resource.client())
  else {
    return false;
  };
  let mut dirty = false;
  for group_id in groups_of(state, manager_id) {
    let Some(group) = state.ext_workspaces.groups.get(&group_id) else {
      continue;
    };
    let bound = state
      .outputs
      .get(&group.output)
      .into_iter()
      .flat_map(|output| output.bound.iter())
      .filter(|(_, output)| {
        output
          .client()
          .is_some_and(|owner| owner.id() == client.id())
      })
      .map(|(id, output)| (id.clone(), output.clone()))
      .collect::<HashMap<_, _>>();
    let Some(group) = state.ext_workspaces.groups.get_mut(&group_id) else {
      continue;
    };
    group.outputs.retain(|id, output| {
      if bound.contains_key(id) {
        return true;
      }
      if output.is_alive() {
        group.resource.output_leave(output);
      }
      dirty = true;
      false
    });
    for (id, output) in bound {
      if let std::collections::hash_map::Entry::Vacant(entry) =
        group.outputs.entry(id)
      {
        group.resource.output_enter(&output);
        entry.insert(output);
        dirty = true;
      }
    }
  }
  dirty
}

/// Sends each workspace of `manager_id` whether it is its output's
/// current one.
fn sync_active(state: &mut CompositorState, manager_id: &ObjectId) -> bool {
  let mut dirty = false;
  let current = state
    .ext_workspaces
    .groups
    .iter()
    .filter(|(_, group)| &group.manager == manager_id)
    .map(|(id, group)| (id.clone(), current_workspace(state, &group.output)))
    .collect::<HashMap<_, _>>();
  for workspace in state.ext_workspaces.workspaces.values_mut() {
    let Some(current) = current.get(&workspace.group) else {
      continue;
    };
    let active = workspace.index == *current;
    if workspace.active != active {
      workspace.active = active;
      workspace.resource.state(if active {
        ACTIVE_STATE
      } else {
        ext_workspace_handle_v1::State::empty()
      });
      dirty = true;
    }
  }
  dirty
}

/// Removes the workspaces of group `group_id`.
fn remove_workspaces(state: &mut CompositorState, group_id: &ObjectId) {
  let Some(group) = state
    .ext_workspaces
    .groups
    .get(group_id)
    .map(|group| group.resource.clone())
  else {
    return;
  };
  state.ext_workspaces.workspaces.retain(|_, workspace| {
    if &workspace.group != group_id {
      return true;
    }
    group.workspace_leave(&workspace.resource);
    workspace.resource.removed();
    false
  });
}

/// Removes group `group_id` with its workspaces.
fn remove_group(state: &mut CompositorState, group_id: &ObjectId) {
  remove_workspaces(state, group_id);
  if let Some(group) = state.ext_workspaces.groups.remove(group_id) {
    for output in group.outputs.values() {
      if output.is_alive() {
        group.resource.output_leave(output);
      }
    }
    group.resource.removed();
  }
}

/// Announces a group for `output` to manager `manager_id`, with its
/// workspaces. Its outputs follow in [`sync_outputs`].
fn create_group(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  manager_id: &ObjectId,
  output: &OutputId,
) {
  let Some(manager) = state
    .ext_workspaces
    .managers
    .get(manager_id)
    .map(|manager| manager.resource.clone())
  else {
    return;
  };
  let Some(client) = manager.client() else {
    return;
  };
  let Ok(group) = client
    .create_resource::<ExtWorkspaceGroupHandleV1, (), CompositorState>(
      handle,
      manager.version(),
      (),
    )
  else {
    return;
  };
  manager.workspace_group(&group);
  group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
  let group_id = Resource::id(&group);
  state.ext_workspaces.groups.insert(
    group_id.clone(),
    Group {
      resource: group,
      manager: manager_id.clone(),
      output: output.clone(),
      wset: state.core.wset_of(output),
      outputs: HashMap::new(),
    },
  );
  create_workspaces(state, handle, manager_id, &group_id);
}

/// Announces the workspaces of the grid in group `group_id`.
fn create_workspaces(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  manager_id: &ObjectId,
  group_id: &ObjectId,
) {
  let grid = (state.workspace_width, state.workspace_height);
  for index in 0..grid.0 as usize * grid.1 as usize {
    create_workspace(state, handle, manager_id, group_id, index, grid);
  }
}

fn create_workspace(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  manager_id: &ObjectId,
  group_id: &ObjectId,
  index: usize,
  grid: (u32, u32),
) {
  let Some(manager) = state
    .ext_workspaces
    .managers
    .get(manager_id)
    .map(|manager| manager.resource.clone())
  else {
    return;
  };
  let Some((group, output)) = state
    .ext_workspaces
    .groups
    .get(group_id)
    .map(|group| (group.resource.clone(), group.output.clone()))
  else {
    return;
  };
  let Some(client) = manager.client() else {
    return;
  };
  let Ok(workspace) = client
    .create_resource::<ExtWorkspaceHandleV1, (), CompositorState>(
      handle,
      1,
      (),
    )
  else {
    return;
  };
  let x = index as u32 % grid.0;
  let y = index as u32 / grid.0;
  let id = format!("{}:workspace-{x}-{y}", output.as_str());
  manager.workspace(&workspace);
  workspace.id(id);
  workspace.name(format!("Workspace {}", index + 1));
  workspace.coordinates(
    [x, y]
      .into_iter()
      .flat_map(u32::to_ne_bytes)
      .collect::<Vec<_>>(),
  );
  let active = index == current_workspace(state, &output);
  workspace.state(if active {
    ACTIVE_STATE
  } else {
    ext_workspace_handle_v1::State::empty()
  });
  workspace.capabilities(ACTIVATE_CAPABILITY);
  group.workspace_enter(&workspace);
  state.ext_workspaces.workspaces.insert(
    Resource::id(&workspace),
    Workspace {
      resource: workspace,
      manager: manager_id.clone(),
      group: group_id.clone(),
      index,
      active,
    },
  );
}

/// The index, row by row, of `output`'s current workspace.
fn current_workspace(state: &CompositorState, output: &OutputId) -> usize {
  state
    .core
    .current_workspace(output)
    .map(|workspace| {
      workspace.y as usize * state.workspace_width as usize
        + workspace.x as usize
    })
    .unwrap_or(0)
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("ext_workspace_manager_v1");

  fn bind(
    state: &mut Self,
    handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtWorkspaceManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let manager = data_init.init(resource, ());
    let id = manager.id();
    state.ext_workspaces.managers.insert(
      id.clone(),
      Manager {
        resource: manager,
        pending_activation: None,
        grid: (state.workspace_width, state.workspace_height),
        stopped: false,
      },
    );
    sync(state, handle);
  }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtWorkspaceManagerV1,
    request: ext_workspace_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      ext_workspace_manager_v1::Request::Commit => {
        let activation = state
          .ext_workspaces
          .managers
          .get_mut(&Resource::id(resource))
          .and_then(|manager| manager.pending_activation.take());
        if let Some((output, index)) = activation
          && let Err(error) = state.switch_workspace_on(&output, index)
        {
          eprintln!("shingle: workspace activation failed: {error}");
        }
      }
      ext_workspace_manager_v1::Request::Stop => {
        if let Some(manager) = state
          .ext_workspaces
          .managers
          .get_mut(&Resource::id(resource))
        {
          manager.stopped = true;
          manager.pending_activation = None;
          manager.resource.finished();
        }
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtWorkspaceManagerV1,
    _data: &(),
  ) {
    state
      .ext_workspaces
      .managers
      .remove(&Resource::id(resource));
  }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtWorkspaceHandleV1,
    request: ext_workspace_handle_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let ext_workspace_handle_v1::Request::Activate = request {
      let activation = state
        .ext_workspaces
        .workspaces
        .get(&Resource::id(resource))
        .and_then(|workspace| {
          let group = state.ext_workspaces.groups.get(&workspace.group)?;
          Some((
            workspace.manager.clone(),
            group.output.clone(),
            workspace.index,
          ))
        });
      if let Some((manager_id, output, index)) = activation
        && let Some(manager) =
          state.ext_workspaces.managers.get_mut(&manager_id)
        && !manager.stopped
      {
        manager.pending_activation = Some((output, index));
      }
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtWorkspaceHandleV1,
    _data: &(),
  ) {
    state
      .ext_workspaces
      .workspaces
      .remove(&Resource::id(resource));
  }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ExtWorkspaceGroupHandleV1,
    request: ext_workspace_group_handle_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let ext_workspace_group_handle_v1::Request::Destroy = request {
      state.ext_workspaces.groups.remove(&Resource::id(resource));
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ExtWorkspaceGroupHandleV1,
    _data: &(),
  ) {
    state.ext_workspaces.groups.remove(&Resource::id(resource));
  }
}
