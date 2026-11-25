use wayland_protocols::wp::viewporter::server::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ObjectId,
};

use crate::{BufferSnapshot, state::CompositorState};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ViewportState {
    pub(crate) source: Option<[f64; 4]>,
    pub(crate) destination: Option<(u32, u32)>,
}

pub(crate) struct Viewport {
    resource: Option<WpViewport>,
    pending: ViewportState,
    current: ViewportState,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, WpViewporter, ()>(1, ());
}

pub(crate) fn commit(
    state: &mut CompositorState,
    object: &ObjectId,
    scale: i32,
) -> Option<ViewportState> {
    let viewport = state.viewports.get(object)?;
    let pending = viewport.pending;
    if let Some(source) = pending.source {
        let [_, _, width, height] = source;
        if pending.destination.is_none()
            && (width.fract() != 0.0 || height.fract() != 0.0)
        {
            if let Some(resource) = &viewport.resource {
                resource.post_error(
                    wp_viewport::Error::BadSize,
                    "source size must be integral when destination is unset",
                );
            }
            return None;
        }
        if let Some(surface) = state.surfaces.get(object)
            && let Some(buffer) = &surface.buffer
        {
            let scale = f64::from(scale.max(1));
            let swapped = matches!(
                buffer.transform,
                wayland_server::protocol::wl_output::Transform::Flipped90
                    | wayland_server::protocol::wl_output::Transform::Flipped270
                    | wayland_server::protocol::wl_output::Transform::_90
                    | wayland_server::protocol::wl_output::Transform::_270
            );
            let (width, height) = if swapped {
                (buffer.height, buffer.width)
            } else {
                (buffer.width, buffer.height)
            };
            let available_width = f64::from(width) / scale;
            let available_height = f64::from(height) / scale;
            let [x, y, width, height] = source;
            if x + width > available_width || y + height > available_height {
                if let Some(resource) = &viewport.resource {
                    resource.post_error(
                        wp_viewport::Error::OutOfBuffer,
                        "source rectangle extends beyond the buffer",
                    );
                }
                return None;
            }
        }
    }
    let remove = viewport.resource.is_none();
    if let Some(viewport) = state.viewports.get_mut(object) {
        viewport.current = pending;
    }
    if remove {
        state.viewports.remove(object);
    }
    Some(pending)
}

pub(crate) fn apply(
    buffer: &mut BufferSnapshot,
    state: ViewportState,
    scale: i32,
) {
    buffer.source = state.source.map(|mut source| {
        let scale = f64::from(scale.max(1));
        for coordinate in &mut source {
            *coordinate *= scale;
        }
        source
    });
}

pub(crate) fn dimensions(
    state: &CompositorState,
    object: &ObjectId,
) -> Option<(u32, u32)> {
    state.viewports.get(object).and_then(|viewport| {
        viewport.current.destination.or_else(|| {
            viewport.current.source.map(|source| {
                (source[2].max(1.0) as u32, source[3].max(1.0) as u32)
            })
        })
    })
}

fn object_for_resource(
    state: &CompositorState,
    resource: &WpViewport,
) -> Option<ObjectId> {
    state.viewports.iter().find_map(|(object, viewport)| {
        (viewport.resource.as_ref() == Some(resource)).then_some(object.clone())
    })
}

impl GlobalDispatch<WpViewporter, ()> for CompositorState {
    crate::security_context::protocol_can_view!("wp_viewporter");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpViewporter>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpViewporter, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpViewporter,
        request: wp_viewporter::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_viewporter::Request::Destroy => {}
            wp_viewporter::Request::GetViewport { id, surface } => {
                let object = surface.id();
                if state.viewports.contains_key(&object) {
                    resource.post_error(
                        wp_viewporter::Error::ViewportExists,
                        "surface already has a viewport",
                    );
                    return;
                }
                let viewport = data_init.init(id, ());
                state.viewports.insert(
                    object,
                    Viewport {
                        resource: Some(viewport),
                        pending: ViewportState::default(),
                        current: ViewportState::default(),
                    },
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WpViewport, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpViewport,
        request: wp_viewport::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(object) = object_for_resource(state, resource) else {
            return;
        };
        if !state.surfaces.contains_key(&object) {
            resource.post_error(
                wp_viewport::Error::NoSurface,
                "surface was destroyed",
            );
            return;
        }
        match request {
            wp_viewport::Request::Destroy => {
                if let Some(viewport) = state.viewports.get_mut(&object) {
                    viewport.resource = None;
                    viewport.pending = ViewportState::default();
                }
            }
            wp_viewport::Request::SetSource {
                x,
                y,
                width,
                height,
            } => {
                let source = if x == -1.0
                    && y == -1.0
                    && width == -1.0
                    && height == -1.0
                {
                    None
                } else {
                    if x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0 {
                        resource.post_error(
                            wp_viewport::Error::BadValue,
                            "source coordinates must be nonnegative and size positive",
                        );
                        return;
                    }
                    Some([x, y, width, height])
                };
                if let Some(viewport) = state.viewports.get_mut(&object) {
                    viewport.pending.source = source;
                }
            }
            wp_viewport::Request::SetDestination { width, height } => {
                let destination = if width == -1 && height == -1 {
                    None
                } else {
                    if width <= 0 || height <= 0 {
                        resource.post_error(
                            wp_viewport::Error::BadValue,
                            "destination size must be positive",
                        );
                        return;
                    }
                    Some((width as u32, height as u32))
                };
                if let Some(viewport) = state.viewports.get_mut(&object) {
                    viewport.pending.destination = destination;
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &WpViewport,
        _data: &(),
    ) {
        if let Some(object) = object_for_resource(state, resource)
            && let Some(viewport) = state.viewports.get_mut(&object)
        {
            viewport.resource = None;
            viewport.pending = ViewportState::default();
        }
    }
}
