use std::{collections::HashMap, fs::File, io::Read, os::fd::OwnedFd};

use wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
    protocol::wl_keyboard,
};

use crate::{
    input::{self, Modifiers},
    state::CompositorState,
};

const MAX_KEYMAP_SIZE: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct VirtualKeyboards {
    has_keymap: HashMap<ObjectId, bool>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, ZwpVirtualKeyboardManagerV1, ()>(
        1,
        (),
    );
}

impl GlobalDispatch<ZwpVirtualKeyboardManagerV1, ()> for CompositorState {
    crate::security_context::protocol_can_view!(
        "zwp_virtual_keyboard_manager_v1"
    );

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { id, .. } = request
        {
            let keyboard = data_init.init(id, ());
            state
                .virtual_keyboards
                .has_keymap
                .insert(keyboard.id(), false);
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_virtual_keyboard_v1::Request::Destroy => {
                state.virtual_keyboards.has_keymap.remove(&resource.id());
            }
            zwp_virtual_keyboard_v1::Request::Keymap { format, fd, size } => {
                if format != wl_keyboard::KeymapFormat::XkbV1 as u32 {
                    keymap_error(resource, "only XKB v1 keymaps are supported");
                    return;
                }
                match read_keymap(fd, size)
                    .and_then(|keymap| input::set_keymap(state, &keymap))
                {
                    Ok(()) => {
                        state
                            .virtual_keyboards
                            .has_keymap
                            .insert(resource.id(), true);
                    }
                    Err(error) => keymap_error(resource, &error),
                }
            }
            zwp_virtual_keyboard_v1::Request::Key {
                time,
                key,
                state: key_state,
            } => {
                if !has_keymap(state, resource) {
                    return;
                }
                let pressed = match key_state {
                    value if value == wl_keyboard::KeyState::Pressed as u32 => {
                        true
                    }
                    value
                        if value == wl_keyboard::KeyState::Released as u32 =>
                    {
                        false
                    }
                    _ => return,
                };
                input::virtual_keyboard_key(state, key, pressed, time);
            }
            zwp_virtual_keyboard_v1::Request::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if !has_keymap(state, resource) {
                    return;
                }
                input::keyboard_modifiers(
                    state,
                    Modifiers {
                        depressed: mods_depressed,
                        latched: mods_latched,
                        locked: mods_locked,
                        group,
                    },
                );
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ZwpVirtualKeyboardV1,
        _data: &(),
    ) {
        state.virtual_keyboards.has_keymap.remove(&resource.id());
    }
}

fn has_keymap(
    state: &mut CompositorState,
    resource: &ZwpVirtualKeyboardV1,
) -> bool {
    if state
        .virtual_keyboards
        .has_keymap
        .get(&resource.id())
        .copied()
        .unwrap_or(false)
    {
        true
    } else {
        resource.post_error(
            zwp_virtual_keyboard_v1::Error::NoKeymap,
            "upload a keymap before sending keyboard input",
        );
        false
    }
}

fn keymap_error(resource: &ZwpVirtualKeyboardV1, message: &str) {
    resource.post_error(zwp_virtual_keyboard_v1::Error::NoKeymap, message);
}

fn read_keymap(fd: OwnedFd, size: u32) -> Result<String, String> {
    let size = size as usize;
    if size == 0 || size > MAX_KEYMAP_SIZE {
        return Err(format!(
            "keymap size must be between 1 and {MAX_KEYMAP_SIZE} bytes"
        ));
    }
    let mut file = File::from(fd);
    let mut bytes = vec![0; size];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("failed to read keymap: {error}"))?;
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map_err(|_| "keymap must contain UTF-8 XKB text".into())
}
