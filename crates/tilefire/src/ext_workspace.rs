use std::collections::{HashMap, HashSet};

use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
};

use crate::{OUTPUT_NAME, state::CompositorState};

const ACTIVATE_CAPABILITY: ext_workspace_handle_v1::WorkspaceCapabilities =
    ext_workspace_handle_v1::WorkspaceCapabilities::Activate;
const ACTIVE_STATE: ext_workspace_handle_v1::State =
    ext_workspace_handle_v1::State::Active;

struct Manager {
    resource: ExtWorkspaceManagerV1,
    pending_activation: Option<usize>,
    grid: (u32, u32),
    stopped: bool,
}

struct Group {
    resource: ExtWorkspaceGroupHandleV1,
    manager: ObjectId,
    outputs: HashSet<ObjectId>,
}

struct Workspace {
    resource: ExtWorkspaceHandleV1,
    manager: ObjectId,
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

/// Synchronizes active state, output membership, and configured grid changes.
pub(crate) fn sync(state: &mut CompositorState, handle: &DisplayHandle) {
    let current = current_workspace(state);
    let grid = (state.workspace_width, state.workspace_height);
    let managers = state
        .ext_workspaces
        .managers
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut dirty = HashSet::new();
    let mut rebuilt = HashSet::new();
    for manager_id in managers {
        if state
            .ext_workspaces
            .managers
            .get(&manager_id)
            .is_none_or(|manager| manager.stopped)
        {
            continue;
        }
        if state.ext_workspaces.managers[&manager_id].grid != grid {
            rebuild_manager(state, handle, &manager_id);
            rebuilt.insert(manager_id);
            continue;
        }
        dirty.extend(sync_outputs(state, &manager_id));
        let workspace_ids = state
            .ext_workspaces
            .workspaces
            .values()
            .filter(|workspace| workspace.manager == manager_id)
            .map(|workspace| Resource::id(&workspace.resource))
            .collect::<Vec<_>>();
        for workspace_id in workspace_ids {
            let Some(workspace) =
                state.ext_workspaces.workspaces.get_mut(&workspace_id)
            else {
                continue;
            };
            let active = workspace.index == current;
            if workspace.active != active {
                workspace.active = active;
                workspace.resource.state(if active {
                    ACTIVE_STATE
                } else {
                    ext_workspace_handle_v1::State::empty()
                });
                dirty.insert(manager_id.clone());
            }
        }
    }
    for manager_id in dirty.difference(&rebuilt) {
        if let Some(manager) = state.ext_workspaces.managers.get(manager_id) {
            manager.resource.done();
        }
    }
}

fn sync_outputs(
    state: &mut CompositorState,
    manager_id: &ObjectId,
) -> HashSet<ObjectId> {
    let mut dirty = HashSet::new();
    let Some(manager) = state.ext_workspaces.managers.get(manager_id) else {
        return dirty;
    };
    let Some(client) = manager.resource.client() else {
        return dirty;
    };
    let outputs = state
        .outputs
        .iter()
        .filter(|(_, output)| {
            output
                .client()
                .is_some_and(|owner| owner.id() == client.id())
        })
        .map(|(id, output)| (id.clone(), output.clone()))
        .collect::<Vec<_>>();
    for group in state.ext_workspaces.groups.values_mut() {
        if &group.manager != manager_id {
            continue;
        }
        for (id, output) in &outputs {
            if group.outputs.insert(id.clone()) {
                group.resource.output_enter(output);
                dirty.insert(manager_id.clone());
            }
        }
    }
    dirty
}

fn rebuild_manager(
    state: &mut CompositorState,
    handle: &DisplayHandle,
    manager_id: &ObjectId,
) {
    let workspace_ids = state
        .ext_workspaces
        .workspaces
        .values()
        .filter(|workspace| &workspace.manager == manager_id)
        .map(|workspace| Resource::id(&workspace.resource))
        .collect::<Vec<_>>();
    for workspace_id in workspace_ids {
        if let Some(workspace) =
            state.ext_workspaces.workspaces.remove(&workspace_id)
        {
            for group in state.ext_workspaces.groups.values() {
                if &group.manager == manager_id {
                    group.resource.workspace_leave(&workspace.resource);
                }
            }
            workspace.resource.removed();
        }
    }
    let group_ids = state
        .ext_workspaces
        .groups
        .values()
        .filter(|group| &group.manager == manager_id)
        .map(|group| Resource::id(&group.resource))
        .collect::<Vec<_>>();
    for group_id in group_ids {
        if let Some(group) = state.ext_workspaces.groups.remove(&group_id) {
            group.resource.removed();
        }
    }
    create_manager_contents(state, handle, manager_id);
}

fn create_manager_contents(
    state: &mut CompositorState,
    handle: &DisplayHandle,
    manager_id: &ObjectId,
) {
    let Some(manager_resource) = state
        .ext_workspaces
        .managers
        .get(manager_id)
        .map(|manager| manager.resource.clone())
    else {
        return;
    };
    let Some(client) = manager_resource.client() else {
        return;
    };
    let grid = (state.workspace_width, state.workspace_height);
    if let Some(manager) = state.ext_workspaces.managers.get_mut(manager_id) {
        manager.grid = grid;
    }
    let Ok(group) = client
        .create_resource::<ExtWorkspaceGroupHandleV1, (), CompositorState>(
            handle,
            manager_resource.version(),
            (),
        )
    else {
        return;
    };
    manager_resource.workspace_group(&group);
    group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
    let outputs = state
        .outputs
        .iter()
        .filter(|(_, output)| {
            output
                .client()
                .is_some_and(|owner| owner.id() == client.id())
        })
        .map(|(id, output)| (id.clone(), output.clone()))
        .collect::<Vec<_>>();
    let mut group_outputs = HashSet::new();
    for (id, output) in outputs {
        group.output_enter(&output);
        group_outputs.insert(id);
    }
    state.ext_workspaces.groups.insert(
        Resource::id(&group),
        Group {
            resource: group.clone(),
            manager: manager_id.clone(),
            outputs: group_outputs,
        },
    );
    for index in 0..grid.0 as usize * grid.1 as usize {
        create_workspace(
            state, handle, &client, manager_id, &group, index, grid,
        );
    }
    manager_resource.done();
}

fn create_workspace(
    state: &mut CompositorState,
    handle: &DisplayHandle,
    client: &Client,
    manager_id: &ObjectId,
    group: &ExtWorkspaceGroupHandleV1,
    index: usize,
    grid: (u32, u32),
) {
    let current = current_workspace(state);
    let Ok(workspace) = client
        .create_resource::<ExtWorkspaceHandleV1, (), CompositorState>(
            handle,
            1,
            (),
        )
    else {
        return;
    };
    let Some(manager) = state.ext_workspaces.managers.get(manager_id) else {
        return;
    };
    let x = index as u32 % grid.0;
    let y = index as u32 / grid.0;
    let id = format!("{OUTPUT_NAME}:workspace-{x}-{y}");
    manager.resource.workspace(&workspace);
    workspace.id(id.clone());
    workspace.name(format!("Workspace {}", index + 1));
    workspace.coordinates(
        [x, y]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>(),
    );
    let active = index == current;
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
            index,
            active,
        },
    );
}

fn current_workspace(state: &CompositorState) -> usize {
    state
        .core
        .current_workspace(&state.output_id)
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
                grid: (0, 0),
                stopped: false,
            },
        );
        create_manager_contents(state, handle, &id);
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
                if let Some(index) = activation
                    && let Err(error) = state.switch_workspace(index)
                {
                    eprintln!("tilefire: workspace activation failed: {error}");
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
                .map(|workspace| (workspace.manager.clone(), workspace.index));
            if let Some((manager_id, index)) = activation
                && let Some(manager) =
                    state.ext_workspaces.managers.get_mut(&manager_id)
                && !manager.stopped
            {
                manager.pending_activation = Some(index);
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
