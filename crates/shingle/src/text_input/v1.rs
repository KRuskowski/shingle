//! `zwp_text_input_manager_v1`, the text input Chromium and Electron use
//! by default, as Wayfire's input-method-v1 plugin offers it
//! (wayfire:plugins/protocols/input-method-v1.cpp).
//!
//! A text input may be activated only on the surface that has the
//! keyboard focus of its client. Its state is not double-buffered: each
//! request changes what the input method sees, and `commit_state` names
//! the serial the input method's answers carry back.

use shingle_core::Rect;
pub(crate) use wayland_protocols::wp::text_input::zv1::server::
  zwp_text_input_v1::ZwpTextInputV1;
use wayland_protocols::wp::text_input::zv1::server::{
  zwp_text_input_manager_v1::{self, ZwpTextInputManagerV1},
  zwp_text_input_v1::{self, PreeditStyle, TextDirection},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use super::{ContentState, ContextEvent, Protocol, TextInput};
use crate::{input_method, state::CompositorState};

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpTextInputManagerV1, ()>(1, ());
}

/// text-input-v1's content purposes from 9 on (`date` to `terminal`) are
/// one below v3's, which has `pin` at 9; the model keeps v3's numbers.
pub(crate) fn purpose_to_v3(purpose: u32) -> u32 {
  match purpose {
    0..=8 => purpose,
    9..=12 => purpose + 1,
    _ => 0,
  }
}

/// The text-input-v1 content purpose input-method v1 expects for v3's
/// `purpose`: `pin` becomes `digits`, the purposes after it move down.
pub(crate) fn purpose_from_v3(purpose: u32) -> u32 {
  match purpose {
    0..=8 => purpose,
    9 => 2,
    10..=13 => purpose - 1,
    _ => 0,
  }
}

/// The v1 `delete_surrounding_text` arguments for deleting `before` bytes
/// before the cursor and `after` bytes after it: an index relative to the
/// cursor and a length.
pub(crate) fn deletion(before: u32, after: u32) -> (i32, u32) {
  let index = -(i64::from(before).min(i64::from(i32::MAX))) as i32;
  (index, before.saturating_add(after))
}

/// Sends an input-method-v1 context's event to a v1 text input as it is.
pub(super) fn send(resource: &ZwpTextInputV1, event: ContextEvent) {
  match event {
    ContextEvent::CommitString { serial, text } => {
      resource.commit_string(serial, text);
    }
    ContextEvent::PreeditString {
      serial,
      text,
      commit,
    } => resource.preedit_string(serial, text, commit),
    ContextEvent::PreeditStyling {
      index,
      length,
      style,
    } => {
      let style =
        PreeditStyle::try_from(style).unwrap_or(PreeditStyle::Default);
      resource.preedit_styling(index, length, style);
    }
    ContextEvent::PreeditCursor { index } => resource.preedit_cursor(index),
    ContextEvent::DeleteSurroundingText { index, length } => {
      resource.delete_surrounding_text(index, length);
    }
    ContextEvent::CursorPosition { index, anchor } => {
      resource.cursor_position(index, anchor);
    }
    ContextEvent::ModifiersMap { map } => resource.modifiers_map(map),
    ContextEvent::Keysym {
      serial,
      time,
      sym,
      state,
      modifiers,
    } => resource.keysym(serial, time, sym, state, modifiers),
    ContextEvent::Language { serial, language } => {
      resource.language(serial, language);
    }
    ContextEvent::TextDirection { serial, direction } => {
      let direction =
        TextDirection::try_from(direction).unwrap_or(TextDirection::Auto);
      resource.text_direction(serial, direction);
    }
  }
}

impl GlobalDispatch<ZwpTextInputManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_text_input_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpTextInputManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpTextInputManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpTextInputManagerV1,
    request: zwp_text_input_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_text_input_manager_v1::Request::CreateTextInput { id } = request
    {
      let input = data_init.init(id, ());
      state.text_inputs.resources.insert(
        Resource::id(&input),
        TextInput::new(Protocol::V1(input), None),
      );
      super::sync(state);
    }
  }
}

impl Dispatch<ZwpTextInputV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpTextInputV1,
    request: zwp_text_input_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let id = Resource::id(resource);
    match request {
      zwp_text_input_v1::Request::Activate { seat, surface } => {
        activate(state, &id, Resource::id(&seat), &surface);
      }
      zwp_text_input_v1::Request::Deactivate { .. } => deactivate(state, &id),
      zwp_text_input_v1::Request::Reset => {
        if enabled(state, &id) {
          input_method::text_input_request(
            state,
            &id,
            input_method::TextInputRequest::Reset,
          );
        }
      }
      zwp_text_input_v1::Request::SetPreferredLanguage { language } => {
        if enabled(state, &id) {
          input_method::text_input_request(
            state,
            &id,
            input_method::TextInputRequest::PreferredLanguage(language),
          );
        }
      }
      zwp_text_input_v1::Request::InvokeAction { button, index } => {
        if enabled(state, &id) {
          input_method::text_input_request(
            state,
            &id,
            input_method::TextInputRequest::InvokeAction { button, index },
          );
        }
      }
      request => update(state, &id, request),
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpTextInputV1,
    _data: &(),
  ) {
    state.text_inputs.resources.remove(&Resource::id(resource));
  }
}

fn enabled(state: &mut CompositorState, id: &ObjectId) -> bool {
  state.text_inputs.enabled_mut(id).is_some()
}

/// Enables text input `id` on `surface` if that surface has its client's
/// keyboard focus, sending `enter`, and disables the seat's other v1 text
/// inputs, since only one is served at a time.
fn activate(
  state: &mut CompositorState,
  id: &ObjectId,
  seat: ObjectId,
  surface: &wayland_server::protocol::wl_surface::WlSurface,
) {
  let focused = state
    .text_inputs
    .resources
    .get(id)
    .is_some_and(|input| input.focused == Some(Resource::id(surface)));
  if !focused {
    return;
  }
  let others = state
    .text_inputs
    .resources
    .iter()
    .filter(|(other, input)| {
      *other != id
        && input.enabled
        && input.seat.as_ref() == Some(&seat)
        && matches!(input.protocol, Protocol::V1(_))
    })
    .map(|(other, _)| other.clone())
    .collect::<Vec<_>>();
  for other in others {
    deactivate(state, &other);
  }
  let activation = state.text_inputs.next_activation();
  let Some(input) = state.text_inputs.resources.get_mut(id) else {
    return;
  };
  let was_enabled = input.enabled;
  input.reset();
  input.enabled = true;
  input.seat = Some(seat);
  input.activated = activation;
  if let Protocol::V1(resource) = &input.protocol {
    if was_enabled {
      resource.leave();
    }
    resource.enter(surface);
  }
}

fn deactivate(state: &mut CompositorState, id: &ObjectId) {
  let Some(input) = state.text_inputs.enabled_mut(id) else {
    return;
  };
  input.reset();
  if let Protocol::V1(resource) = &input.protocol {
    resource.leave();
  }
}

/// Applies a state request at once, as v1 has no double-buffering.
fn update(
  state: &mut CompositorState,
  id: &ObjectId,
  request: zwp_text_input_v1::Request,
) {
  let Some(input) = state.text_inputs.enabled_mut(id) else {
    return;
  };
  let current: &mut ContentState = &mut input.current;
  match request {
    zwp_text_input_v1::Request::SetSurroundingText {
      text,
      cursor,
      anchor,
    } => {
      current.surrounding = Some((
        text,
        i32::try_from(cursor).unwrap_or(i32::MAX),
        i32::try_from(anchor).unwrap_or(i32::MAX),
      ));
    }
    zwp_text_input_v1::Request::SetContentType { hint, purpose } => {
      current.content_hint = hint.into();
      current.content_purpose = purpose_to_v3(purpose.into());
    }
    zwp_text_input_v1::Request::SetCursorRectangle {
      x,
      y,
      width,
      height,
    } => {
      current.cursor_rectangle = (width > 0 && height > 0)
        .then_some(Rect::new(x, y, width as u32, height as u32));
      // The rectangle has no commit of its own; it holds from
      // `commit_state` on, like the rest of the state.
      return;
    }
    zwp_text_input_v1::Request::CommitState { serial } => {
      input.commit_serial = serial;
      input.applied_cursor = input.current.cursor_rectangle;
    }
    // Wayfire does nothing for the input panel requests either; Shingle
    // shows an input method's panel while any text input is enabled.
    zwp_text_input_v1::Request::ShowInputPanel
    | zwp_text_input_v1::Request::HideInputPanel => return,
    _ => return,
  }
  input.generation = input.generation.wrapping_add(1);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn purposes_map_between_v1_and_v3() {
    // normal, password, date, terminal
    assert_eq!(purpose_to_v3(0), 0);
    assert_eq!(purpose_to_v3(8), 8);
    assert_eq!(purpose_to_v3(9), 10);
    assert_eq!(purpose_to_v3(12), 13);
    assert_eq!(purpose_to_v3(40), 0);
    assert_eq!(purpose_from_v3(9), 2);
    assert_eq!(purpose_from_v3(10), 9);
    assert_eq!(purpose_from_v3(13), 12);
    for purpose in 0..=12 {
      assert_eq!(purpose_from_v3(purpose_to_v3(purpose)), purpose);
    }
  }

  #[test]
  fn a_deletion_counts_from_the_cursor() {
    assert_eq!(deletion(3, 2), (-3, 5));
    assert_eq!(deletion(0, 4), (0, 4));
    assert_eq!(deletion(u32::MAX, 1), (-i32::MAX, u32::MAX));
  }
}
