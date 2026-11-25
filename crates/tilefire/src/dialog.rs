use std::collections::HashMap;

use tilefire_core::WindowId;
use wayland_protocols::xdg::dialog::v1::server::{
    xdg_dialog_v1::{self, XdgDialogV1},
    xdg_wm_dialog_v1::{self, XdgWmDialogV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
};

use crate::{
    shell::{Role, XdgKind},
    state::CompositorState,
};

struct Dialog {
    toplevel: ObjectId,
    modal: bool,
}

#[derive(Default)]
pub(crate) struct Dialogs {
    objects: HashMap<ObjectId, Dialog>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, XdgWmDialogV1, ()>(1, ());
}

/// Returns whether a toplevel explicitly requested modal dialog behavior.
pub(crate) fn is_modal(state: &CompositorState, window: WindowId) -> bool {
    let toplevel =
        state
            .surfaces
            .values()
            .find_map(|surface| match &surface.role {
                Role::Xdg {
                    kind: Some(XdgKind::Toplevel(toplevel)),
                    ..
                } if toplevel.window() == Some(window) => {
                    Some(Resource::id(&toplevel.resource))
                }
                _ => None,
            });
    toplevel.is_some_and(|toplevel| {
        state
            .dialogs
            .objects
            .values()
            .any(|dialog| dialog.modal && dialog.toplevel == toplevel)
    })
}

/// Dialog objects become inert when their associated toplevel is destroyed.
pub(crate) fn toplevel_destroyed(
    state: &mut CompositorState,
    toplevel: &ObjectId,
) {
    state
        .dialogs
        .objects
        .retain(|_, dialog| dialog.toplevel != *toplevel);
}

impl GlobalDispatch<XdgWmDialogV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("xdg_wm_dialog_v1");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgWmDialogV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgWmDialogV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &XdgWmDialogV1,
        request: xdg_wm_dialog_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_dialog_v1::Request::GetXdgDialog { id, toplevel } => {
                let dialog = data_init.init(id, ());
                let toplevel = Resource::id(&toplevel);
                if state
                    .dialogs
                    .objects
                    .values()
                    .any(|dialog| dialog.toplevel == toplevel)
                {
                    resource.post_error(
                        xdg_wm_dialog_v1::Error::AlreadyUsed,
                        "xdg_toplevel already has an xdg_dialog_v1",
                    );
                    return;
                }
                state.dialogs.objects.insert(
                    Resource::id(&dialog),
                    Dialog {
                        toplevel,
                        modal: false,
                    },
                );
            }
            xdg_wm_dialog_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<XdgDialogV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &XdgDialogV1,
        request: xdg_dialog_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(dialog) =
            state.dialogs.objects.get_mut(&Resource::id(resource))
        else {
            return;
        };
        match request {
            xdg_dialog_v1::Request::SetModal => dialog.modal = true,
            xdg_dialog_v1::Request::UnsetModal => dialog.modal = false,
            xdg_dialog_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &XdgDialogV1,
        _data: &(),
    ) {
        state.dialogs.objects.remove(&Resource::id(resource));
    }
}
