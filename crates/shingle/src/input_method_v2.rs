use std::collections::HashMap;

use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
  ChangeCause, ContentHint, ContentPurpose,
};
use wayland_protocols_misc::zwp_input_method_v2::server::{
  zwp_input_method_keyboard_grab_v2::{self, ZwpInputMethodKeyboardGrabV2},
  zwp_input_method_manager_v2::{self, ZwpInputMethodManagerV2},
  zwp_input_method_v2::{self, ZwpInputMethodV2},
  zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{input_method, state::CompositorState, text_input};

#[derive(Default)]
struct PendingEdit {
  commit_string: String,
  preedit_string: String,
  cursor_begin: i32,
  cursor_end: i32,
  delete_before: u32,
  delete_after: u32,
}

struct InputMethod {
  resource: ZwpInputMethodV2,
  seat: ObjectId,
  active_input: Option<ObjectId>,
  synced_serial: Option<u32>,
  done_serial: u32,
  pending: PendingEdit,
  unavailable: bool,
}

#[derive(Default)]
pub(crate) struct InputMethods {
  methods: HashMap<ObjectId, InputMethod>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpInputMethodManagerV2, ()>(1, ());
}

pub(crate) fn active_methods(state: &CompositorState) -> Vec<ObjectId> {
  state
    .input_methods_v2
    .methods
    .values()
    .filter(|method| method.active_input.is_some())
    .map(|method| Resource::id(&method.resource))
    .collect()
}

/// Sends activation, text state, and done events for each available method.
pub(crate) fn sync(state: &mut CompositorState) {
  let active = state.text_inputs.active();
  let methods = state
    .input_methods_v2
    .methods
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for id in methods {
    if state.input_methods_v2.methods[&id].unavailable {
      continue;
    }
    let current = state.input_methods_v2.methods[&id].active_input.clone();
    if current != active {
      if current.is_some() {
        deactivate(state, &id);
      }
      if let Some(input) = &active {
        activate(state, &id, input.clone());
      }
    }
    if state.input_methods_v2.methods[&id].active_input.is_some() {
      sync_method(state, &id);
    }
  }
}

fn activate(state: &mut CompositorState, id: &ObjectId, input: ObjectId) {
  if let Some(method) = state.input_methods_v2.methods.get_mut(id) {
    method.active_input = Some(input);
    method.synced_serial = None;
    method.pending = PendingEdit::default();
    method.resource.activate();
  }
}

fn deactivate(state: &mut CompositorState, id: &ObjectId) {
  input_method::release_context(state, id);
  if let Some(method) = state.input_methods_v2.methods.get_mut(id) {
    method.resource.deactivate();
    method.active_input = None;
    method.synced_serial = None;
    method.pending = PendingEdit::default();
    done(method);
  }
}

fn sync_method(state: &mut CompositorState, id: &ObjectId) {
  let Some(input) = state.input_methods_v2.methods[id].active_input.clone()
  else {
    return;
  };
  let Some(snapshot) = state.text_inputs.snapshot(&input) else {
    return;
  };
  let Some(method) = state.input_methods_v2.methods.get_mut(id) else {
    return;
  };
  if method.synced_serial == Some(snapshot.generation) {
    return;
  }
  if let Some((text, cursor, anchor)) = snapshot.surrounding {
    method.resource.surrounding_text(
      text,
      cursor.max(0) as u32,
      anchor.max(0) as u32,
    );
  }
  let cause = ChangeCause::try_from(snapshot.change_cause)
    .unwrap_or(ChangeCause::InputMethod);
  let hint = ContentHint::from_bits_truncate(snapshot.content_hint);
  let purpose = ContentPurpose::try_from(snapshot.content_purpose)
    .unwrap_or(ContentPurpose::Normal);
  method.resource.text_change_cause(cause);
  method.resource.content_type(hint, purpose);
  method.synced_serial = Some(snapshot.generation);
  done(method);
}

fn done(method: &mut InputMethod) {
  method.resource.done();
  method.done_serial = method.done_serial.wrapping_add(1);
}

impl GlobalDispatch<ZwpInputMethodManagerV2, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_input_method_manager_v2");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpInputMethodManagerV2>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpInputMethodManagerV2,
    request: zwp_input_method_manager_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_input_method_manager_v2::Request::GetInputMethod {
      seat,
      input_method,
    } = request
    {
      let seat = Resource::id(&seat);
      let unavailable = state
        .input_methods_v2
        .methods
        .values()
        .any(|method| method.seat == seat);
      let resource = data_init.init(input_method, ());
      if unavailable {
        resource.unavailable();
      }
      state.input_methods_v2.methods.insert(
        Resource::id(&resource),
        InputMethod {
          resource,
          seat,
          active_input: None,
          synced_serial: None,
          done_serial: 0,
          pending: PendingEdit::default(),
          unavailable,
        },
      );
    }
  }
}

impl Dispatch<ZwpInputMethodV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpInputMethodV2,
    request: zwp_input_method_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let id = Resource::id(resource);
    let Some(method) = state.input_methods_v2.methods.get_mut(&id) else {
      return;
    };
    if method.unavailable {
      match request {
        zwp_input_method_v2::Request::GetInputPopupSurface { id, .. } => {
          data_init.init(id, ());
        }
        zwp_input_method_v2::Request::GrabKeyboard { keyboard } => {
          data_init.init(keyboard, ());
        }
        _ => {}
      }
      return;
    }
    match request {
      zwp_input_method_v2::Request::CommitString { text } => {
        method.pending.commit_string = text;
      }
      zwp_input_method_v2::Request::SetPreeditString {
        text,
        cursor_begin,
        cursor_end,
      } => {
        method.pending.preedit_string = text;
        method.pending.cursor_begin = cursor_begin;
        method.pending.cursor_end = cursor_end;
      }
      zwp_input_method_v2::Request::DeleteSurroundingText {
        before_length,
        after_length,
      } => {
        method.pending.delete_before = before_length;
        method.pending.delete_after = after_length;
      }
      zwp_input_method_v2::Request::Commit { serial } => {
        let valid = serial == method.done_serial;
        let active = method.active_input.clone();
        let pending = std::mem::take(&mut method.pending);
        if valid && let Some(input) = active {
          text_input::commit_input_method(
            state,
            &input,
            text_input::InputMethodEdit {
              commit_string: pending.commit_string,
              preedit_string: pending.preedit_string,
              cursor_begin: pending.cursor_begin,
              cursor_end: pending.cursor_end,
              delete_before: pending.delete_before,
              delete_after: pending.delete_after,
            },
          );
        }
      }
      zwp_input_method_v2::Request::GrabKeyboard { keyboard } => {
        let keyboard = data_init.init(keyboard, ());
        input_method::register_keyboard_grab(
          state,
          id,
          input_method::GrabKeyboard::V2(keyboard),
        );
      }
      zwp_input_method_v2::Request::GetInputPopupSurface {
        id: popup,
        surface,
      } => {
        let popup: ZwpInputPopupSurfaceV2 = data_init.init(popup, ());
        crate::input_panel::create_v2(state, resource, popup, &surface);
      }
      zwp_input_method_v2::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpInputMethodV2,
    _data: &(),
  ) {
    let id = Resource::id(resource);
    input_method::release_context(state, &id);
    crate::input_panel::method_destroyed(state, &id);
    state.input_methods_v2.methods.remove(&id);
  }
}

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZwpInputMethodKeyboardGrabV2,
    request: zwp_input_method_keyboard_grab_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_input_method_keyboard_grab_v2::Request::Release = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpInputMethodKeyboardGrabV2,
    _data: &(),
  ) {
    input_method::keyboard_destroyed(state, &Resource::id(resource));
  }
}
