use std::collections::HashMap;

use shingle_core::Rect;
use wayland_protocols::wp::{
    pointer_constraints::zv1::server::{
        zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1},
        zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
        zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::server::{
        zwp_relative_pointer_manager_v1::{self, ZwpRelativePointerManagerV1},
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    WEnum,
    backend::{ClientId, ObjectId},
    protocol::{
        wl_pointer::WlPointer, wl_region::WlRegion, wl_surface::WlSurface,
    },
};

use crate::state::CompositorState;

#[derive(Default)]
pub(crate) struct PointerProtocols {
    constraints: HashMap<ObjectId, Constraint>,
    relative_pointers: HashMap<ObjectId, Vec<ZwpRelativePointerV1>>,
}

struct Constraint {
    surface: ObjectId,
    pointer: ObjectId,
    resource: ConstraintResource,
    lifetime: zwp_pointer_constraints_v1::Lifetime,
    region: Option<Vec<Rect>>,
    pending_region: Option<Option<Vec<Rect>>>,
    cursor_hint: Option<(f64, f64)>,
    pending_cursor_hint: Option<(f64, f64)>,
    active: bool,
}

enum ConstraintResource {
    Locked(ZwpLockedPointerV1),
    Confined(ZwpConfinedPointerV1),
}

impl ConstraintResource {
    fn activate(&self) {
        match self {
            Self::Locked(resource) => resource.locked(),
            Self::Confined(resource) => resource.confined(),
        }
    }

    fn deactivate(&self) {
        match self {
            Self::Locked(resource) => resource.unlocked(),
            Self::Confined(resource) => resource.unconfined(),
        }
    }
}

pub(crate) fn create_globals(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, ZwpPointerConstraintsV1, ()>(1, ());
    handle.create_global::<CompositorState, ZwpRelativePointerManagerV1, ()>(
        1,
        (),
    );
}

pub(crate) fn relative_motion(
    state: &mut CompositorState,
    dx: f64,
    dy: f64,
    dx_unaccel: f64,
    dy_unaccel: f64,
) {
    let Some(client) = state
        .input
        .pointer_focus
        .as_ref()
        .and_then(|surface| state.surfaces.get(surface))
        .map(|surface| surface.client.clone())
    else {
        return;
    };
    let micros = state.now().as_micros() as u64;
    for pointer in state
        .input
        .pointers
        .values()
        .filter(|pointer| {
            pointer.client().map(|client| client.id()).as_ref() == Some(&client)
        })
        .filter_map(|pointer| {
            state.pointer_protocols.relative_pointers.get(&pointer.id())
        })
        .flatten()
    {
        pointer.relative_motion(
            (micros >> 32) as u32,
            micros as u32,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        );
    }
}

pub(crate) fn constrained(state: &CompositorState) -> bool {
    state
        .pointer_protocols
        .constraints
        .values()
        .any(|constraint| constraint.active)
}

pub(crate) fn locked(state: &CompositorState) -> bool {
    state
        .pointer_protocols
        .constraints
        .values()
        .any(|constraint| {
            constraint.active
                && matches!(constraint.resource, ConstraintResource::Locked(_))
        })
}

pub(crate) fn constrain(state: &CompositorState, x: f64, y: f64) -> (f64, f64) {
    let Some(constraint) = state
        .pointer_protocols
        .constraints
        .values()
        .find(|constraint| constraint.active)
    else {
        return (x, y);
    };
    match constraint.resource {
        ConstraintResource::Locked(_) => {
            (state.input.pointer_x, state.input.pointer_y)
        }
        ConstraintResource::Confined(_) => {
            closest_point(state, constraint, x, y)
                .unwrap_or((state.input.pointer_x, state.input.pointer_y))
        }
    }
}

pub(crate) fn commit(state: &mut CompositorState, surface: &ObjectId) {
    for constraint in state.pointer_protocols.constraints.values_mut() {
        if &constraint.surface != surface {
            continue;
        }
        if let Some(region) = constraint.pending_region.take() {
            constraint.region = region;
        }
        if let Some(hint) = constraint.pending_cursor_hint.take() {
            constraint.cursor_hint = Some(hint);
        }
    }
    sync(state);
}

pub(crate) fn sync(state: &mut CompositorState) {
    let focus = state.input.pointer_focus.clone();
    let changes = state
        .pointer_protocols
        .constraints
        .iter()
        .map(|(id, constraint)| {
            (
                id.clone(),
                constraint.active,
                focus.as_ref() == Some(&constraint.surface),
            )
        })
        .collect::<Vec<_>>();
    for (id, active, focused) in changes {
        if active && !focused {
            deactivate(state, &id);
        } else if !active && focused {
            let region = state
                .pointer_protocols
                .constraints
                .get(&id)
                .map(|constraint| region_rects(state, constraint));
            if region.as_ref().is_some_and(|region| region.is_empty()) {
                continue;
            }
            if let Some(constraint) =
                state.pointer_protocols.constraints.get(&id)
                && let Some((x, y)) = closest_point(
                    state,
                    constraint,
                    state.input.pointer_x,
                    state.input.pointer_y,
                )
                && (x != state.input.pointer_x || y != state.input.pointer_y)
            {
                crate::input::pointer_warp(state, x, y);
            }
            if let Some(constraint) =
                state.pointer_protocols.constraints.get_mut(&id)
            {
                constraint.active = true;
                constraint.resource.activate();
            }
        } else if active
            && let Some(constraint) =
                state.pointer_protocols.constraints.get(&id)
            && matches!(constraint.resource, ConstraintResource::Confined(_))
            && let Some((x, y)) = closest_point(
                state,
                constraint,
                state.input.pointer_x,
                state.input.pointer_y,
            )
            && (x != state.input.pointer_x || y != state.input.pointer_y)
        {
            crate::input::pointer_warp(state, x, y);
        }
    }
}

fn deactivate(state: &mut CompositorState, id: &ObjectId) {
    let oneshot = if let Some(constraint) =
        state.pointer_protocols.constraints.get_mut(id)
    {
        constraint.active = false;
        constraint.resource.deactivate();
        constraint.lifetime == zwp_pointer_constraints_v1::Lifetime::Oneshot
    } else {
        return;
    };
    if oneshot {
        state.pointer_protocols.constraints.remove(id);
    }
}

fn destroy(state: &mut CompositorState, id: &ObjectId) {
    let Some(constraint) = state.pointer_protocols.constraints.remove(id)
    else {
        return;
    };
    if constraint.active
        && let Some((x, y)) = constraint.cursor_hint
        && let Some(geometry) =
            crate::shell::surface_geometry(state, &constraint.surface)
    {
        crate::input::pointer_warp(
            state,
            f64::from(geometry.x) + x,
            f64::from(geometry.y) + y,
        );
    }
}

pub(crate) fn surface_destroyed(
    state: &mut CompositorState,
    surface: &ObjectId,
) {
    state
        .pointer_protocols
        .constraints
        .retain(|_, constraint| &constraint.surface != surface);
}

pub(crate) fn pointer_destroyed(
    state: &mut CompositorState,
    pointer: &ObjectId,
) {
    state.pointer_protocols.relative_pointers.remove(pointer);
    let constraints = state
        .pointer_protocols
        .constraints
        .iter()
        .filter(|(_, constraint)| &constraint.pointer == pointer)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in constraints {
        destroy(state, &id);
    }
}

fn region_rects(state: &CompositorState, constraint: &Constraint) -> Vec<Rect> {
    let Some(surface) = state.surfaces.get(&constraint.surface) else {
        return Vec::new();
    };
    let requested = constraint.region.clone();
    match (requested, &surface.input_region) {
        (Some(requested), Some(input)) => requested
            .into_iter()
            .flat_map(|requested| {
                input
                    .iter()
                    .filter_map(move |input| intersection(requested, *input))
            })
            .collect(),
        (Some(requested), None) => requested,
        (None, Some(input)) => input.clone(),
        (None, None) => {
            crate::shell::surface_geometry(state, &constraint.surface)
                .map(|geometry| {
                    vec![Rect::new(0, 0, geometry.width, geometry.height)]
                })
                .unwrap_or_default()
        }
    }
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 =
        a.x.saturating_add(a.width.min(i32::MAX as u32) as i32)
            .min(b.x.saturating_add(b.width.min(i32::MAX as u32) as i32));
    let y1 =
        a.y.saturating_add(a.height.min(i32::MAX as u32) as i32)
            .min(b.y.saturating_add(b.height.min(i32::MAX as u32) as i32));
    (x0 < x1 && y0 < y1)
        .then(|| Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

fn closest_point(
    state: &CompositorState,
    constraint: &Constraint,
    x: f64,
    y: f64,
) -> Option<(f64, f64)> {
    let geometry = crate::shell::surface_geometry(state, &constraint.surface)?;
    let local_x = x - f64::from(geometry.x);
    let local_y = y - f64::from(geometry.y);
    region_rects(state, constraint)
        .into_iter()
        .map(|rect| {
            let right = f64::from(rect.x) + f64::from(rect.width);
            let bottom = f64::from(rect.y) + f64::from(rect.height);
            let px = local_x.clamp(f64::from(rect.x), right - 1.0 / 256.0);
            let py = local_y.clamp(f64::from(rect.y), bottom - 0.0001);
            let distance = (px - local_x).powi(2) + (py - local_y).powi(2);
            (
                px + f64::from(geometry.x),
                py + f64::from(geometry.y),
                distance,
            )
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(x, y, _)| (x, y))
}

struct ManagerData;
struct RelativeManagerData;
struct RelativePointerData;

impl GlobalDispatch<ZwpPointerConstraintsV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("zwp_pointer_constraints_v1");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpPointerConstraintsV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ManagerData);
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ManagerData> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpPointerConstraintsV1,
        request: zwp_pointer_constraints_v1::Request,
        _data: &ManagerData,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_pointer_constraints_v1::Request::Destroy => {}
            zwp_pointer_constraints_v1::Request::LockPointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => create_constraint(
                state,
                resource,
                ConstraintResource::Locked(data_init.init(id, ManagerData)),
                surface,
                pointer,
                region,
                lifetime,
            ),
            zwp_pointer_constraints_v1::Request::ConfinePointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => create_constraint(
                state,
                resource,
                ConstraintResource::Confined(data_init.init(id, ManagerData)),
                surface,
                pointer,
                region,
                lifetime,
            ),
            _ => {}
        }
    }
}

fn create_constraint(
    state: &mut CompositorState,
    manager: &ZwpPointerConstraintsV1,
    resource: ConstraintResource,
    surface: WlSurface,
    pointer: WlPointer,
    region: Option<WlRegion>,
    lifetime: WEnum<zwp_pointer_constraints_v1::Lifetime>,
) {
    let surface = surface.id();
    if state
        .pointer_protocols
        .constraints
        .values()
        .any(|constraint| constraint.surface == surface)
    {
        manager.post_error(
            zwp_pointer_constraints_v1::Error::AlreadyConstrained,
            "surface already has a pointer constraint",
        );
        return;
    }
    let id = match &resource {
        ConstraintResource::Locked(resource) => resource.id(),
        ConstraintResource::Confined(resource) => resource.id(),
    };
    let lifetime = match lifetime {
        WEnum::Value(zwp_pointer_constraints_v1::Lifetime::Persistent) => {
            zwp_pointer_constraints_v1::Lifetime::Persistent
        }
        _ => zwp_pointer_constraints_v1::Lifetime::Oneshot,
    };
    state.pointer_protocols.constraints.insert(
        id,
        Constraint {
            surface,
            pointer: pointer.id(),
            resource,
            lifetime,
            region: region.as_ref().map(|region| {
                state.regions.get(&region.id()).cloned().unwrap_or_default()
            }),
            pending_region: None,
            cursor_hint: None,
            pending_cursor_hint: None,
            active: false,
        },
    );
    sync(state);
}

impl Dispatch<ZwpLockedPointerV1, ManagerData> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpLockedPointerV1,
        request: zwp_locked_pointer_v1::Request,
        _data: &ManagerData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_locked_pointer_v1::Request::Destroy => {
                destroy(state, &resource.id())
            }
            zwp_locked_pointer_v1::Request::SetCursorPositionHint {
                surface_x,
                surface_y,
            } => {
                if let Some(constraint) =
                    state.pointer_protocols.constraints.get_mut(&resource.id())
                {
                    constraint.pending_cursor_hint =
                        Some((surface_x, surface_y));
                }
            }
            zwp_locked_pointer_v1::Request::SetRegion { region } => {
                if let Some(constraint) =
                    state.pointer_protocols.constraints.get_mut(&resource.id())
                {
                    constraint.pending_region =
                        Some(region.as_ref().map(|region| {
                            state
                                .regions
                                .get(&region.id())
                                .cloned()
                                .unwrap_or_default()
                        }));
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwpLockedPointerV1,
        _data: &ManagerData,
    ) {
        state.pointer_protocols.constraints.remove(&resource.id());
    }
}

impl Dispatch<ZwpConfinedPointerV1, ManagerData> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpConfinedPointerV1,
        request: zwp_confined_pointer_v1::Request,
        _data: &ManagerData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_confined_pointer_v1::Request::Destroy => {
                destroy(state, &resource.id())
            }
            zwp_confined_pointer_v1::Request::SetRegion { region } => {
                if let Some(constraint) =
                    state.pointer_protocols.constraints.get_mut(&resource.id())
                {
                    constraint.pending_region =
                        Some(region.as_ref().map(|region| {
                            state
                                .regions
                                .get(&region.id())
                                .cloned()
                                .unwrap_or_default()
                        }));
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwpConfinedPointerV1,
        _data: &ManagerData,
    ) {
        state.pointer_protocols.constraints.remove(&resource.id());
    }
}

impl GlobalDispatch<ZwpRelativePointerManagerV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!(
        "zwp_relative_pointer_manager_v1"
    );

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpRelativePointerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, RelativeManagerData);
    }
}

impl Dispatch<ZwpRelativePointerManagerV1, RelativeManagerData>
    for CompositorState
{
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpRelativePointerManagerV1,
        request: zwp_relative_pointer_manager_v1::Request,
        _data: &RelativeManagerData,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_relative_pointer_manager_v1::Request::Destroy => {}
            zwp_relative_pointer_manager_v1::Request::GetRelativePointer {
                id,
                pointer,
            } => {
                let relative = data_init.init(id, RelativePointerData);
                state
                    .pointer_protocols
                    .relative_pointers
                    .entry(pointer.id())
                    .or_default()
                    .push(relative);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, RelativePointerData> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpRelativePointerV1,
        _request: zwp_relative_pointer_v1::Request,
        _data: &RelativePointerData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        for pointers in state.pointer_protocols.relative_pointers.values_mut() {
            pointers.retain(|pointer| pointer.id() != resource.id());
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwpRelativePointerV1,
        _data: &RelativePointerData,
    ) {
        for pointers in state.pointer_protocols.relative_pointers.values_mut() {
            pointers.retain(|pointer| pointer.id() != resource.id());
        }
    }
}
