use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ObjectId,
    protocol::{
        wl_subcompositor::{self, WlSubcompositor},
        wl_subsurface::{self, WlSubsurface},
        wl_surface::WlSurface,
    },
};

use crate::{shell::Role, state::CompositorState};

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, WlSubcompositor, ()>(1, ());
}

fn object_for_subsurface(
    state: &CompositorState,
    resource: &WlSubsurface,
) -> Option<ObjectId> {
    state.surfaces.iter().find_map(|(object, surface)| {
        matches!(&surface.role, Role::Subsurface { resource: candidate, .. } if candidate == resource)
            .then_some(object.clone())
    })
}

fn restack(
    state: &mut CompositorState,
    object: &ObjectId,
    sibling: &WlSurface,
    above: bool,
) {
    let Some(parent) =
        state
            .surfaces
            .get(object)
            .and_then(|surface| match &surface.role {
                Role::Subsurface { parent, .. } => Some(parent.clone()),
                _ => None,
            })
    else {
        return;
    };
    let sibling = sibling.id();
    if sibling == *object || state.surfaces.get(&sibling).is_none_or(|surface| {
        matches!(&surface.role, Role::Subsurface { parent: candidate, .. } if *candidate != parent)
            || !matches!(surface.role, Role::Subsurface { .. })
    }) {
        return;
    }
    let Some(children) = state.subsurface_order.get_mut(&parent) else {
        return;
    };
    let Some(index) = children.iter().position(|child| child == object) else {
        return;
    };
    children.remove(index);
    let index = children.iter().position(|child| *child == sibling).map_or(
        if above { children.len() } else { 0 },
        |index| {
            if above { index + 1 } else { index }
        },
    );
    children.insert(index, object.clone());
}

pub(crate) fn commit_children(state: &mut CompositorState, object: &ObjectId) {
    let Some(children) = state.subsurface_order.get(object).cloned() else {
        return;
    };
    for child in children {
        if state
            .surfaces
            .get(&child)
            .is_some_and(|surface| surface.cached_commit.is_some())
        {
            state.commit_cached(&child);
        } else {
            commit_children(state, &child);
        }
    }
}

impl GlobalDispatch<WlSubcompositor, ()> for CompositorState {
    crate::security_context::protocol_can_view!("wl_subcompositor");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlSubcompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlSubcompositor, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WlSubcompositor,
        request: wl_subcompositor::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_subcompositor::Request::Destroy => {}
            wl_subcompositor::Request::GetSubsurface {
                id,
                surface,
                parent,
            } => {
                let object = surface.id();
                let parent_object = parent.id();
                let valid = object != parent_object
                    && state.surfaces.get(&object).is_some_and(|surface| {
                        matches!(surface.role, Role::None)
                    })
                    && state.surfaces.contains_key(&parent_object);
                let subsurface = data_init.init(id, ());
                if !valid {
                    resource.post_error(
                        wl_subcompositor::Error::BadSurface,
                        "subsurface and parent must be distinct valid surfaces without another role",
                    );
                    return;
                }
                if let Some(surface) = state.surfaces.get_mut(&object) {
                    surface.role = Role::Subsurface {
                        resource: subsurface,
                        parent: parent_object.clone(),
                        relative: (0, 0),
                        synchronized: true,
                    };
                }
                state
                    .subsurface_order
                    .entry(parent_object)
                    .or_default()
                    .push(object);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSubsurface, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WlSubsurface,
        request: wl_subsurface::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(object) = object_for_subsurface(state, resource) else {
            return;
        };
        match request {
            wl_subsurface::Request::Destroy => {
                let parent = state.surfaces.get(&object).and_then(|surface| {
                    match &surface.role {
                        Role::Subsurface { parent, .. } => Some(parent.clone()),
                        _ => None,
                    }
                });
                if let Some(parent) = parent
                    && let Some(children) =
                        state.subsurface_order.get_mut(&parent)
                {
                    children.retain(|child| child != &object);
                }
                if let Some(surface) = state.surfaces.get_mut(&object) {
                    surface.role = Role::None;
                    surface.cached_commit = None;
                }
            }
            wl_subsurface::Request::SetPosition { x, y } => {
                if let Some(surface) = state.surfaces.get_mut(&object)
                    && let Role::Subsurface { relative, .. } = &mut surface.role
                {
                    *relative = (x, y);
                }
            }
            wl_subsurface::Request::PlaceAbove { sibling } => {
                restack(state, &object, &sibling, true);
            }
            wl_subsurface::Request::PlaceBelow { sibling } => {
                restack(state, &object, &sibling, false);
            }
            wl_subsurface::Request::SetSync => {
                if let Some(surface) = state.surfaces.get_mut(&object)
                    && let Role::Subsurface { synchronized, .. } =
                        &mut surface.role
                {
                    *synchronized = true;
                }
            }
            wl_subsurface::Request::SetDesync => {
                if let Some(surface) = state.surfaces.get_mut(&object)
                    && let Role::Subsurface { synchronized, .. } =
                        &mut surface.role
                {
                    *synchronized = false;
                }
                if state
                    .surfaces
                    .get(&object)
                    .is_some_and(|surface| surface.cached_commit.is_some())
                {
                    state.commit_cached(&object);
                }
            }
            _ => {}
        }
    }
}
