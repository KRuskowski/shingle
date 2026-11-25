use wayland_protocols::wp::fractional_scale::v1::server::{
    wp_fractional_scale_manager_v1::{self, WpFractionalScaleManagerV1},
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ObjectId,
};

use crate::state::CompositorState;

pub(crate) fn sync(state: &mut CompositorState) {
    let scale = (state.output_scale * 120.0)
        .round()
        .clamp(1.0, u32::MAX as f64) as u32;
    for resource in state.fractional_scales.values() {
        resource.preferred_scale(scale);
    }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, WpFractionalScaleManagerV1, ()>(
        1,
        (),
    );
}

fn object_for_resource(
    state: &CompositorState,
    resource: &WpFractionalScaleV1,
) -> Option<ObjectId> {
    state
        .fractional_scales
        .iter()
        .find_map(|(object, candidate)| {
            (candidate == resource).then_some(object.clone())
        })
}

impl GlobalDispatch<WpFractionalScaleManagerV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!(
        "wp_fractional_scale_manager_v1"
    );

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpFractionalScaleManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpFractionalScaleManagerV1,
        request: wp_fractional_scale_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_fractional_scale_manager_v1::Request::Destroy => {}
            wp_fractional_scale_manager_v1::Request::GetFractionalScale {
                id,
                surface,
            } => {
                let object = surface.id();
                if state.fractional_scales.contains_key(&object) {
                    resource.post_error(
                        wp_fractional_scale_manager_v1::Error::FractionalScaleExists,
                        "surface already has a fractional scale object",
                    );
                    return;
                }
                let scale = data_init.init(id, ());
                scale.preferred_scale(
                    (state.output_scale * 120.0)
                        .round()
                        .clamp(1.0, u32::MAX as f64) as u32,
                );
                state.fractional_scales.insert(object, scale);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WpFractionalScaleV1,
        request: wp_fractional_scale_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_fractional_scale_v1::Request::Destroy = request
            && let Some(object) = object_for_resource(state, resource)
        {
            state.fractional_scales.remove(&object);
        }
    }
}
