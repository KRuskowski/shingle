use std::collections::HashMap;

use wayland_protocols::wp::alpha_modifier::v1::server::{
    wp_alpha_modifier_surface_v1::{self, WpAlphaModifierSurfaceV1},
    wp_alpha_modifier_v1::{self, WpAlphaModifierV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
};

use crate::state::CompositorState;

struct AlphaModifier {
    surface: ObjectId,
}

#[derive(Default)]
pub(crate) struct AlphaModifiers {
    objects: HashMap<ObjectId, AlphaModifier>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, WpAlphaModifierV1, ()>(1, ());
}

impl GlobalDispatch<WpAlphaModifierV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("wp_alpha_modifier_v1");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpAlphaModifierV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpAlphaModifierV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpAlphaModifierV1,
        request: wp_alpha_modifier_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_alpha_modifier_v1::Request::GetSurface { id, surface } => {
                let modifier = data_init.init(id, ());
                let surface = Resource::id(&surface);
                if state
                    .alpha_modifiers
                    .objects
                    .values()
                    .any(|modifier| modifier.surface == surface)
                {
                    resource.post_error(
                        wp_alpha_modifier_v1::Error::AlreadyConstructed,
                        "wl_surface already has an alpha modifier",
                    );
                    return;
                }
                state
                    .alpha_modifiers
                    .objects
                    .insert(Resource::id(&modifier), AlphaModifier { surface });
            }
            wp_alpha_modifier_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpAlphaModifierSurfaceV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpAlphaModifierSurfaceV1,
        request: wp_alpha_modifier_surface_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(surface) = state
            .alpha_modifiers
            .objects
            .get(&Resource::id(resource))
            .map(|modifier| modifier.surface.clone())
        else {
            return;
        };
        let factor = match request {
            wp_alpha_modifier_surface_v1::Request::SetMultiplier { factor } => {
                factor
            }
            wp_alpha_modifier_surface_v1::Request::Destroy => u32::MAX,
            _ => return,
        };
        let Some(surface) = state.surfaces.get_mut(&surface) else {
            resource.post_error(
                wp_alpha_modifier_surface_v1::Error::NoSurface,
                "the associated wl_surface was destroyed",
            );
            return;
        };
        surface.pending.alpha = Some(factor);
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &WpAlphaModifierSurfaceV1,
        _data: &(),
    ) {
        state
            .alpha_modifiers
            .objects
            .remove(&Resource::id(resource));
    }
}
