use std::collections::{HashMap, HashSet};

use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    WEnum,
    backend::{ClientId, ObjectId},
    protocol::wl_output,
};

use crate::{OUTPUT_NAME, state::CompositorState};

#[derive(Default)]
pub(crate) struct OutputManagement {
    serial: u32,
    managers: HashMap<ObjectId, ZwlrOutputManagerV1>,
    heads: HashMap<ObjectId, Head>,
    modes: HashMap<ObjectId, Mode>,
    configurations: HashMap<ObjectId, Configuration>,
    configuration_heads: HashMap<ObjectId, ConfigurationHead>,
}

struct Head {
    resource: ZwlrOutputHeadV1,
    manager: ObjectId,
}

struct Mode {
    resource: ZwlrOutputModeV1,
    head: ObjectId,
}

struct Configuration {
    manager: ObjectId,
    serial: u32,
    used: bool,
    configured: HashSet<ObjectId>,
    disabled: HashSet<ObjectId>,
}

enum ModeChoice {
    Current,
    Custom(u32, u32),
}

struct ConfigurationHead {
    configuration: ObjectId,
    head: ObjectId,
    mode: Option<ModeChoice>,
    position: Option<(i32, i32)>,
    transform: Option<wl_output::Transform>,
    scale: Option<f64>,
    adaptive_sync: Option<bool>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, ZwlrOutputManagerV1, ()>(4, ());
}

pub(crate) fn sync(state: &mut CompositorState) {
    if state.output_management.managers.is_empty() {
        return;
    }
    state.output_management.serial =
        state.output_management.serial.wrapping_add(1);
    if state.output_management.serial == 0 {
        state.output_management.serial = 1;
    }
    let serial = state.output_management.serial;
    let output = state.output;
    let managers = state
        .output_management
        .managers
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for manager in managers {
        let heads = state
            .output_management
            .heads
            .iter()
            .filter(|(_, head)| head.manager == manager.id())
            .map(|(id, head)| (id.clone(), head.resource.clone()))
            .collect::<Vec<_>>();
        for (id, head) in heads {
            if let Some(mode) = state
                .output_management
                .modes
                .get(&id)
                .map(|mode| mode.resource.clone())
            {
                let (width, height) = crate::output_transform::physical_size(
                    output.width,
                    output.height,
                    state.output_scale,
                    state.output_transform,
                );
                mode.size(width as i32, height as i32);
                head.current_mode(&mode);
            }
            head.position(output.x, output.y);
            head.transform(state.output_transform);
            head.scale(state.output_scale);
        }
        manager.done(serial);
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!("zwlr_output_manager_v1");

    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let version = manager.version();
        state.output_management.serial =
            state.output_management.serial.wrapping_add(1);
        if state.output_management.serial == 0 {
            state.output_management.serial = 1;
        }
        let serial = state.output_management.serial;
        let head = match client
            .create_resource::<ZwlrOutputHeadV1, (), CompositorState>(
                handle,
                version,
                (),
            ) {
            Ok(head) => head,
            Err(_) => return,
        };
        let mode = match client
            .create_resource::<ZwlrOutputModeV1, (), CompositorState>(
                handle,
                version.min(3),
                (),
            ) {
            Ok(mode) => mode,
            Err(_) => return,
        };
        manager.head(&head);
        head.name(OUTPUT_NAME.into());
        head.description(format!("Tilefire {OUTPUT_NAME}"));
        head.enabled(1);
        head.mode(&mode);
        let (width, height) = crate::output_transform::physical_size(
            state.output.width,
            state.output.height,
            state.output_scale,
            state.output_transform,
        );
        mode.size(width as i32, height as i32);
        mode.refresh(60_000);
        mode.preferred();
        head.current_mode(&mode);
        head.position(state.output.x, state.output.y);
        head.transform(state.output_transform);
        head.scale(state.output_scale);
        if version >= 4 {
            head.adaptive_sync(
                zwlr_output_head_v1::AdaptiveSyncState::Disabled,
            );
        }
        manager.done(serial);
        state
            .output_management
            .managers
            .insert(manager.id(), manager.clone());
        state.output_management.heads.insert(
            head.id(),
            Head {
                resource: head.clone(),
                manager: manager.id(),
            },
        );
        state.output_management.modes.insert(
            head.id(),
            Mode {
                resource: mode,
                head: head.id(),
            },
        );
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration {
                id,
                serial,
            } => {
                let configuration = data_init.init(id, ());
                state.output_management.configurations.insert(
                    configuration.id(),
                    Configuration {
                        manager: resource.id(),
                        serial,
                        used: false,
                        configured: HashSet::new(),
                        disabled: HashSet::new(),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                resource.finished();
                state.output_management.managers.remove(&resource.id());
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputManagerV1,
        _data: &(),
    ) {
        state.output_management.managers.remove(&resource.id());
    }
}

impl Dispatch<ZwlrOutputHeadV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state.output_management.heads.remove(&resource.id());
        state.output_management.modes.remove(&resource.id());
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputHeadV1,
        _data: &(),
    ) {
        state.output_management.heads.remove(&resource.id());
        state.output_management.modes.remove(&resource.id());
    }
}

impl Dispatch<ZwlrOutputModeV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state
            .output_management
            .modes
            .retain(|_, mode| mode.resource.id() != resource.id());
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputModeV1,
        _data: &(),
    ) {
        state
            .output_management
            .modes
            .retain(|_, mode| mode.resource.id() != resource.id());
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::Destroy => {
                destroy_configuration(state, resource.id())
            }
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let configuration_head = data_init.init(id, ());
                enable_head(state, resource, head, configuration_head);
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                if !mark_configurable(state, resource, &head.id()) {
                    return;
                }
                if let Some(configuration) = state
                    .output_management
                    .configurations
                    .get_mut(&resource.id())
                {
                    configuration.disabled.insert(head.id());
                }
            }
            zwlr_output_configuration_v1::Request::Apply => {
                finish(state, resource, false)
            }
            zwlr_output_configuration_v1::Request::Test => {
                finish(state, resource, true)
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwlrOutputConfigurationV1,
        _data: &(),
    ) {
        destroy_configuration(state, resource.id());
    }
}

fn mark_configurable(
    state: &mut CompositorState,
    resource: &ZwlrOutputConfigurationV1,
    head: &ObjectId,
) -> bool {
    let Some(configuration) = state
        .output_management
        .configurations
        .get_mut(&resource.id())
    else {
        return false;
    };
    if configuration.used {
        resource.post_error(
            zwlr_output_configuration_v1::Error::AlreadyUsed,
            "configuration was already tested or applied",
        );
        return false;
    }
    if !configuration.configured.insert(head.clone()) {
        resource.post_error(
            zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
            "head was already configured",
        );
        return false;
    }
    true
}

fn enable_head(
    state: &mut CompositorState,
    resource: &ZwlrOutputConfigurationV1,
    head: ZwlrOutputHeadV1,
    configuration_head: ZwlrOutputConfigurationHeadV1,
) {
    if !mark_configurable(state, resource, &head.id()) {
        return;
    }
    state.output_management.configuration_heads.insert(
        configuration_head.id(),
        ConfigurationHead {
            configuration: resource.id(),
            head: head.id(),
            mode: None,
            position: None,
            transform: None,
            scale: None,
            adaptive_sync: None,
        },
    );
}

fn finish(
    state: &mut CompositorState,
    resource: &ZwlrOutputConfigurationV1,
    test: bool,
) {
    let Some(configuration) = state
        .output_management
        .configurations
        .get_mut(&resource.id())
    else {
        return;
    };
    if configuration.used {
        resource.post_error(
            zwlr_output_configuration_v1::Error::AlreadyUsed,
            "configuration was already tested or applied",
        );
        return;
    }
    configuration.used = true;
    let serial = configuration.serial;
    let manager = configuration.manager.clone();
    let configured = configuration.configured.clone();
    let disabled = configuration.disabled.clone();
    if serial != state.output_management.serial {
        resource.cancelled();
        return;
    }
    let current_heads = state
        .output_management
        .heads
        .iter()
        .filter(|(_, head)| head.manager == manager)
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    if configured != current_heads {
        resource.post_error(
            zwlr_output_configuration_v1::Error::UnconfiguredHead,
            "configuration must include every current head",
        );
        return;
    }
    if !disabled.is_empty() {
        resource.failed();
        return;
    }
    let mut output = state.output;
    let mut scale = state.output_scale;
    let mut transform = state.output_transform;
    let mut custom_mode = None;
    for head in &configured {
        let Some(configuration_head) =
            state.output_management.configuration_heads.values().find(
                |configuration_head| {
                    configuration_head.configuration == resource.id()
                        && &configuration_head.head == head
                },
            )
        else {
            continue;
        };
        if let Some(ModeChoice::Custom(width, height)) = configuration_head.mode
        {
            custom_mode = Some((width, height));
        }
        if let Some((x, y)) = configuration_head.position {
            output.x = x;
            output.y = y;
        }
        if let Some(requested) = configuration_head.scale {
            scale = requested;
        }
        if let Some(requested) = configuration_head.transform {
            transform = requested;
        }
        if configuration_head.adaptive_sync == Some(true) {
            resource.failed();
            return;
        }
    }
    if crate::output_transform::is_quarter_turn(state.output_transform)
        != crate::output_transform::is_quarter_turn(transform)
    {
        std::mem::swap(&mut output.width, &mut output.height);
    }
    if let Some((width, height)) = custom_mode {
        (output.width, output.height) = crate::output_transform::logical_size(
            width, height, scale, transform,
        );
    }
    if test {
        resource.succeeded();
        return;
    }
    state.set_output_transform(transform);
    match state
        .set_output_scale(scale)
        .and_then(|()| state.set_output(output))
    {
        Ok(()) => resource.succeeded(),
        Err(_) => resource.failed(),
    }
}

fn destroy_configuration(state: &mut CompositorState, id: ObjectId) {
    state.output_management.configurations.remove(&id);
    state
        .output_management
        .configuration_heads
        .retain(|_, head| head.configuration != id);
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                let head = state
                    .output_management
                    .configuration_heads
                    .get(&resource.id())
                    .map(|head| head.head.clone());
                let valid = head.is_some_and(|head| {
                    state.output_management.modes.get(&head).is_some_and(
                        |entry| {
                            entry.resource.id() == mode.id()
                                && entry.head == head
                        },
                    )
                });
                if !valid {
                    resource.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidMode,
                        "mode does not belong to this head",
                    );
                    return;
                }
                set_property(
                    state,
                    resource,
                    |head| &mut head.mode,
                    Some(ModeChoice::Current),
                );
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                if width <= 0 || height <= 0 || refresh < 0 {
                    resource.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidCustomMode,
                        "custom mode dimensions and refresh are invalid",
                    );
                    return;
                }
                set_property(
                    state,
                    resource,
                    |head| &mut head.mode,
                    Some(ModeChoice::Custom(width as u32, height as u32)),
                );
            }
            zwlr_output_configuration_head_v1::Request::SetPosition {
                x,
                y,
            } => {
                set_property(
                    state,
                    resource,
                    |head| &mut head.position,
                    Some((x, y)),
                );
            }
            zwlr_output_configuration_head_v1::Request::SetTransform {
                transform,
            } => {
                let WEnum::Value(transform) = transform else {
                    resource.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidTransform,
                        "unknown output transform",
                    );
                    return;
                };
                set_property(
                    state,
                    resource,
                    |head| &mut head.transform,
                    Some(transform),
                );
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                if !scale.is_finite() || scale <= 0.0 {
                    resource.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidScale,
                        "output scale must be positive",
                    );
                    return;
                }
                set_property(
                    state,
                    resource,
                    |head| &mut head.scale,
                    Some(scale),
                );
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync {
                state: adaptive,
            } => {
                let WEnum::Value(adaptive) = adaptive else {
                    resource.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidAdaptiveSyncState,
                        "unknown adaptive sync state",
                    );
                    return;
                };
                let enabled = match adaptive {
                    zwlr_output_head_v1::AdaptiveSyncState::Disabled => false,
                    zwlr_output_head_v1::AdaptiveSyncState::Enabled => true,
                    _ => return,
                };
                set_property(
                    state,
                    resource,
                    |head| &mut head.adaptive_sync,
                    Some(enabled),
                );
            }
            _ => {}
        }
    }
}

fn set_property<T>(
    state: &mut CompositorState,
    resource: &ZwlrOutputConfigurationHeadV1,
    field: impl FnOnce(&mut ConfigurationHead) -> &mut Option<T>,
    value: Option<T>,
) {
    let Some(head) = state
        .output_management
        .configuration_heads
        .get_mut(&resource.id())
    else {
        return;
    };
    let field = field(head);
    if field.is_some() {
        resource.post_error(
            zwlr_output_configuration_head_v1::Error::AlreadySet,
            "head property was already set",
        );
        return;
    }
    *field = value;
}

pub(crate) fn cleanup_client(state: &mut CompositorState, client: &ClientId) {
    state.output_management.managers.retain(|_, resource| {
        resource.client().is_none_or(|item| item.id() != *client)
    });
    state.output_management.heads.retain(|_, head| {
        head.resource
            .client()
            .is_none_or(|item| item.id() != *client)
    });
    state.output_management.modes.retain(|_, mode| {
        mode.resource
            .client()
            .is_none_or(|item| item.id() != *client)
    });
    state.output_management.configurations.retain(|id, _| {
        state
            .output_management
            .configuration_heads
            .values()
            .all(|head| &head.configuration != id)
    });
}
