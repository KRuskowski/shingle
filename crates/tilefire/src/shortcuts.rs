use std::collections::{HashMap, HashSet};

use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::server::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::{
        self, ZwpKeyboardShortcutsInhibitManagerV1,
    },
    zwp_keyboard_shortcuts_inhibitor_v1::{
        self, ZwpKeyboardShortcutsInhibitorV1,
    },
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ObjectId,
};

use crate::state::CompositorState;

struct Inhibitor {
    resource: ZwpKeyboardShortcutsInhibitorV1,
    surface: ObjectId,
    active: bool,
}

#[derive(Default)]
pub(crate) struct ShortcutInhibitors {
    config: tilefire_config::ShortcutConfig,
    objects: HashMap<ObjectId, Inhibitor>,
    implicit: HashSet<ObjectId>,
    suppressed: Option<ObjectId>,
}

impl ShortcutInhibitors {
    pub(crate) fn new(config: tilefire_config::ShortcutConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(crate) fn set_config(
        &mut self,
        config: tilefire_config::ShortcutConfig,
    ) {
        self.config = config;
    }

    fn contains_surface(&self, surface: &ObjectId) -> bool {
        self.objects
            .values()
            .any(|inhibitor| inhibitor.surface == *surface)
    }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, ZwpKeyboardShortcutsInhibitManagerV1, ()>(1, ());
}

pub(crate) fn sync(state: &mut CompositorState) {
    let focus = state.input.keyboard_focus.clone();
    if state.shortcut_inhibitors.suppressed != focus {
        state.shortcut_inhibitors.suppressed = None;
    }
    let mut implicit = HashSet::new();
    let mut ignored = HashSet::new();
    for (object, surface) in &state.surfaces {
        if surface.buffer.is_none() {
            continue;
        }
        if matches_view(
            state,
            object,
            &state.shortcut_inhibitors.config.inhibit_by_default,
        ) {
            implicit.insert(object.clone());
        }
        if matches_view(
            state,
            object,
            &state.shortcut_inhibitors.config.ignore_views,
        ) {
            ignored.insert(object.clone());
        }
    }
    state.shortcut_inhibitors.implicit = implicit;
    state
        .shortcut_inhibitors
        .objects
        .retain(|_, inhibitor| state.surfaces.contains_key(&inhibitor.surface));
    for inhibitor in &mut state.shortcut_inhibitors.objects.values_mut() {
        let active = focus.as_ref() == Some(&inhibitor.surface)
            && !ignored.contains(&inhibitor.surface)
            && state.shortcut_inhibitors.suppressed.as_ref()
                != Some(&inhibitor.surface);
        if active && !inhibitor.active {
            inhibitor.resource.active();
        } else if !active && inhibitor.active {
            inhibitor.resource.inactive();
        }
        inhibitor.active = active;
    }
}

pub(crate) fn break_grab(state: &mut CompositorState) {
    let Some(focus) = state.input.keyboard_focus.clone() else {
        return;
    };
    if active(state) {
        state.shortcut_inhibitors.suppressed = Some(focus);
        sync(state);
    }
}

pub(crate) fn active(state: &CompositorState) -> bool {
    if state
        .shortcut_inhibitors
        .objects
        .values()
        .any(|inhibitor| inhibitor.active)
    {
        return true;
    }
    let Some(focus) = state.input.keyboard_focus.as_ref() else {
        return false;
    };
    state.shortcut_inhibitors.suppressed.as_ref() != Some(focus)
        && state.shortcut_inhibitors.implicit.contains(focus)
        && !matches_view(
            state,
            focus,
            &state.shortcut_inhibitors.config.ignore_views,
        )
}

fn matches_view(
    state: &CompositorState,
    object: &ObjectId,
    matcher: &tilefire_config::ViewMatcher,
) -> bool {
    let Some(window) = crate::input::window_for_object(state, object) else {
        return false;
    };
    let Ok(info) = state.core.window_info(window) else {
        return false;
    };
    matcher.matches_view(&crate::window_rules::view_properties(
        state,
        window,
        &info.identity,
    ))
}

impl GlobalDispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()>
    for CompositorState
{
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpKeyboardShortcutsInhibitManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _: &()) -> bool {
        crate::security_context::can_view(
            client,
            "zwp_keyboard_shortcuts_inhibit_manager_v1",
        )
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpKeyboardShortcutsInhibitManagerV1,
        request: zwp_keyboard_shortcuts_inhibit_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_keyboard_shortcuts_inhibit_manager_v1::Request::InhibitShortcuts {
                id,
                surface,
                ..
            } => {
                if state.shortcut_inhibitors.contains_surface(&surface.id()) {
                    resource.post_error(
                        zwp_keyboard_shortcuts_inhibit_manager_v1::Error::AlreadyInhibited,
                        "shortcuts are already inhibited for this surface",
                    );
                    return;
                }
                let inhibitor = data_init.init(id, ());
                state.shortcut_inhibitors.objects.insert(
                    inhibitor.id(),
                    Inhibitor {
                        resource: inhibitor,
                        surface: surface.id(),
                        active: false,
                    },
                );
            }
            zwp_keyboard_shortcuts_inhibit_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpKeyboardShortcutsInhibitorV1,
        request: zwp_keyboard_shortcuts_inhibitor_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_keyboard_shortcuts_inhibitor_v1::Request::Destroy = request {
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &ZwpKeyboardShortcutsInhibitorV1,
        _data: &(),
    ) {
        state.shortcut_inhibitors.objects.remove(&resource.id());
    }
}
