use std::collections::HashMap;

use wayland_protocols::xdg::activation::v1::server::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::{self, XdgActivationV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ObjectId,
};

use crate::{
    input,
    shell::{Role, XdgKind},
    state::CompositorState,
};

struct PendingToken {
    resource: XdgActivationTokenV1,
    app_id: Option<String>,
    surface: Option<ObjectId>,
    committed: bool,
}

struct CommittedToken {
    app_id: Option<String>,
    created: std::time::Duration,
}

#[derive(Default)]
pub(crate) struct Activation {
    config: tilefire_config::ActivationConfig,
    pending: HashMap<ObjectId, PendingToken>,
    tokens: HashMap<String, CommittedToken>,
    latest: Option<String>,
}

impl Activation {
    pub(crate) fn new(config: tilefire_config::ActivationConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(crate) fn set_config(
        &mut self,
        config: tilefire_config::ActivationConfig,
    ) {
        self.config = config;
    }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, XdgActivationV1, ()>(1, ());
}

fn token(state: &CompositorState) -> Option<String> {
    loop {
        let mut bytes = [0_u8; 16];
        rustix::rand::getrandom(
            &mut bytes,
            rustix::rand::GetRandomFlags::empty(),
        )
        .ok()?;
        let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        if !state.activation.tokens.contains_key(&token) {
            return Some(token);
        }
    }
}

impl GlobalDispatch<XdgActivationV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("xdg_activation_v1");

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgActivationV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgActivationV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &XdgActivationV1,
        request: xdg_activation_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_activation_v1::Request::GetActivationToken { id } => {
                let token = data_init.init(id, ());
                state.activation.pending.insert(
                    token.id(),
                    PendingToken {
                        resource: token,
                        app_id: None,
                        surface: None,
                        committed: false,
                    },
                );
            }
            xdg_activation_v1::Request::Activate { token, surface } => {
                let Some(committed) = state.activation.tokens.remove(&token)
                else {
                    return;
                };
                let only_last = state.activation.config.only_last_request;
                if only_last
                    && state.activation.latest.as_deref()
                        != Some(token.as_str())
                {
                    return;
                }
                state.activation.latest = None;
                let timeout = std::time::Duration::from_secs(
                    state.activation.config.timeout_seconds,
                );
                if state.now().saturating_sub(committed.created) >= timeout {
                    return;
                }
                let Some(surface) = state.surfaces.get(&surface.id()) else {
                    return;
                };
                if surface.buffer.is_none() {
                    return;
                }
                let Role::Xdg {
                    kind: Some(XdgKind::Toplevel(toplevel)),
                    ..
                } = &surface.role
                else {
                    return;
                };
                if committed
                    .app_id
                    .as_ref()
                    .is_some_and(|app_id| app_id != &toplevel.app_id)
                {
                    return;
                }
                if let Some(window) = toplevel.window() {
                    input::focus_window(state, window);
                }
            }
            xdg_activation_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &XdgActivationTokenV1,
        request: xdg_activation_token_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_activation_token_v1::Request::SetSerial { .. } => {}
            xdg_activation_token_v1::Request::SetAppId { app_id } => {
                if let Some(token) =
                    state.activation.pending.get_mut(&resource.id())
                    && !token.committed
                {
                    token.app_id = Some(app_id);
                }
            }
            xdg_activation_token_v1::Request::SetSurface { surface } => {
                if let Some(token) =
                    state.activation.pending.get_mut(&resource.id())
                    && !token.committed
                {
                    token.surface = Some(surface.id());
                }
            }
            xdg_activation_token_v1::Request::Commit => {
                let Some(token) = token(state) else {
                    return;
                };
                let Some(pending) =
                    state.activation.pending.get_mut(&resource.id())
                else {
                    return;
                };
                if pending.committed {
                    resource.post_error(
                        xdg_activation_token_v1::Error::AlreadyUsed,
                        "activation token has already been committed",
                    );
                    return;
                }
                pending.committed = true;
                pending.resource.done(token.clone());
                let app_id = pending.app_id.clone();
                let source = pending.surface.clone();
                let valid = !state.activation.config.check_surface
                    || source.is_some_and(|surface| {
                        state.surfaces.contains_key(&surface)
                    });
                if valid {
                    state.activation.latest = Some(token.clone());
                    state.activation.tokens.insert(
                        token,
                        CommittedToken {
                            app_id,
                            created: state.now(),
                        },
                    );
                }
            }
            xdg_activation_token_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &XdgActivationTokenV1,
        _data: &(),
    ) {
        state.activation.pending.remove(&resource.id());
    }
}
