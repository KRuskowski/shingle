use std::collections::HashMap;

use tilefire_core::WindowId;
use wayland_protocols::ext::foreign_toplevel_list::v1::server::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
};

use crate::{foreign_toplevel::ForeignWindow, state::CompositorState};

struct Manager {
    resource: ExtForeignToplevelListV1,
    stopped: bool,
}

struct Handle {
    resource: ExtForeignToplevelHandleV1,
    window: Option<WindowId>,
}

#[derive(Default)]
pub(crate) struct ExtToplevels {
    managers: HashMap<ObjectId, Manager>,
    handles: HashMap<ObjectId, Handle>,
    known: HashMap<WindowId, (String, String)>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle
        .create_global::<CompositorState, ExtForeignToplevelListV1, ()>(1, ());
}

/// Shares map, metadata, and close updates with the existing foreign-toplevel
/// snapshot while keeping protocol objects independent for each client.
pub(crate) fn sync(
    state: &mut CompositorState,
    handle: &DisplayHandle,
    windows: &HashMap<WindowId, ForeignWindow>,
) {
    let removed = state
        .ext_toplevels
        .known
        .keys()
        .filter(|window| !windows.contains_key(window))
        .copied()
        .collect::<Vec<_>>();
    for window in removed {
        for handle in &mut state.ext_toplevels.handles.values_mut() {
            if handle.window == Some(window) {
                handle.resource.closed();
                handle.window = None;
            }
        }
        state.ext_toplevels.known.remove(&window);
    }

    for (window, info) in windows {
        let metadata = (info.title.clone(), info.app_id.clone());
        match state.ext_toplevels.known.get(window).cloned() {
            None => {
                let managers = state
                    .ext_toplevels
                    .managers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                for manager in managers {
                    create_handle(state, handle, &manager, *window, info);
                }
            }
            Some(previous) if previous != metadata => {
                update_handles(state, *window, &previous, info);
            }
            _ => {}
        }
        state.ext_toplevels.known.insert(*window, metadata);
    }
}

fn create_handle(
    state: &mut CompositorState,
    handle: &DisplayHandle,
    manager_id: &ObjectId,
    window: WindowId,
    info: &ForeignWindow,
) {
    let Some(manager) = state.ext_toplevels.managers.get(manager_id) else {
        return;
    };
    if manager.stopped {
        return;
    }
    let Some(client) = manager.resource.client() else {
        return;
    };
    let Ok(resource) = client
        .create_resource::<ExtForeignToplevelHandleV1, (), CompositorState>(
            handle,
            1,
            (),
        )
    else {
        return;
    };
    manager.resource.toplevel(&resource);
    resource.identifier(format!("tilefire-{}", window.get()));
    resource.title(info.title.clone());
    resource.app_id(info.app_id.clone());
    resource.done();
    state.ext_toplevels.handles.insert(
        Resource::id(&resource),
        Handle {
            resource,
            window: Some(window),
        },
    );
}

fn update_handles(
    state: &mut CompositorState,
    window: WindowId,
    previous: &(String, String),
    current: &ForeignWindow,
) {
    for handle in state.ext_toplevels.handles.values_mut() {
        if handle.window != Some(window) {
            continue;
        }
        if previous.0 != current.title {
            handle.resource.title(current.title.clone());
        }
        if previous.1 != current.app_id {
            handle.resource.app_id(current.app_id.clone());
        }
        handle.resource.done();
    }
}

impl GlobalDispatch<ExtForeignToplevelListV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("ext_foreign_toplevel_list_v1");

    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        _client: &Client,
        resource: New<ExtForeignToplevelListV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let id = Resource::id(&manager);
        state.ext_toplevels.managers.insert(
            id,
            Manager {
                resource: manager,
                stopped: false,
            },
        );
        crate::foreign_toplevel::sync(state, handle);
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtForeignToplevelListV1,
        request: ext_foreign_toplevel_list_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Request::Stop = request
            && let Some(manager) = state
                .ext_toplevels
                .managers
                .get_mut(&Resource::id(resource))
        {
            manager.stopped = true;
            manager.resource.finished();
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtForeignToplevelListV1,
        _data: &(),
    ) {
        state.ext_toplevels.managers.remove(&Resource::id(resource));
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtForeignToplevelHandleV1,
        request: ext_foreign_toplevel_handle_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_handle_v1::Request::Destroy = request {
            state.ext_toplevels.handles.remove(&Resource::id(resource));
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtForeignToplevelHandleV1,
        _data: &(),
    ) {
        state.ext_toplevels.handles.remove(&Resource::id(resource));
    }
}
