use std::collections::HashMap;

use shingle_core::Rect;
use wayland_protocols::wp::text_input::zv3::server::{
  zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
  zwp_text_input_v3::{self, ZwpTextInputV3},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
};

use crate::state::CompositorState;

#[derive(Clone, Debug, Default)]
struct ContentState {
  surrounding: Option<(String, i32, i32)>,
  change_cause: u32,
  content_hint: u32,
  content_purpose: u32,
  cursor_rectangle: Option<Rect>,
}

struct TextInput {
  resource: ZwpTextInputV3,
  seat: ObjectId,
  focused: Option<ObjectId>,
  enabled: bool,
  pending_enabled: bool,
  pending: ContentState,
  current: ContentState,
  committed_cursor: Option<Rect>,
  applied_cursor: Option<Rect>,
  commit_serial: u32,
}

#[derive(Default)]
pub(crate) struct TextInputs {
  resources: HashMap<ObjectId, TextInput>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpTextInputManagerV3, ()>(1, ());
}

/// Sends leave before enter and invalidates editing state on either transition.
pub(crate) fn sync(state: &mut CompositorState) {
  let focus = state.input.keyboard_focus.clone();
  for input in state.text_inputs.resources.values_mut() {
    let target = focus.as_ref().filter(|object| {
      state.surfaces.get(*object).is_some_and(|surface| {
        input
          .resource
          .client()
          .is_some_and(|client| client.id() == surface.client)
      })
    });
    if input.focused == target.cloned() {
      continue;
    }
    if let Some(previous) = input.focused.take()
      && let Some(surface) = state.surfaces.get(&previous)
    {
      input.resource.leave(&surface.resource);
    }
    input.enabled = false;
    input.pending_enabled = false;
    input.pending = ContentState::default();
    input.current = ContentState::default();
    input.committed_cursor = None;
    input.applied_cursor = None;
    if let Some(target) = target
      && let Some(surface) = state.surfaces.get(target)
    {
      input.resource.enter(&surface.resource);
    }
    input.focused = target.cloned();
  }
}

pub(crate) struct TextInputSnapshot {
  pub(crate) surrounding: Option<(String, i32, i32)>,
  pub(crate) change_cause: u32,
  pub(crate) content_hint: u32,
  pub(crate) content_purpose: u32,
  pub(crate) cursor_rectangle: Option<Rect>,
  pub(crate) surface: ObjectId,
  pub(crate) commit_serial: u32,
}

impl TextInputs {
  pub(crate) fn active(&self) -> Option<ObjectId> {
    self
      .resources
      .values()
      .find(|input| input.enabled && input.focused.is_some())
      .map(|input| Resource::id(&input.resource))
  }

  pub(crate) fn snapshot(&self, id: &ObjectId) -> Option<TextInputSnapshot> {
    let input = self.resources.get(id)?;
    Some(TextInputSnapshot {
      surrounding: input.current.surrounding.clone(),
      change_cause: input.current.change_cause,
      content_hint: input.current.content_hint,
      content_purpose: input.current.content_purpose,
      cursor_rectangle: input.applied_cursor,
      surface: input.focused.clone()?,
      commit_serial: input.commit_serial,
    })
  }

  fn for_event(
    &mut self,
    id: &ObjectId,
    serial: u32,
  ) -> Option<&mut TextInput> {
    self.resources.get_mut(id).filter(|input| {
      input.enabled && input.focused.is_some() && input.commit_serial == serial
    })
  }
}

/// One atomic edit submitted by an input-method-v2 commit request.
pub(crate) struct InputMethodEdit {
  pub(crate) commit_string: String,
  pub(crate) preedit_string: String,
  pub(crate) cursor_begin: i32,
  pub(crate) cursor_end: i32,
  pub(crate) delete_before: u32,
  pub(crate) delete_after: u32,
}

pub(crate) fn surface_commit(state: &mut CompositorState, object: &ObjectId) {
  for input in state.text_inputs.resources.values_mut() {
    if input.enabled && input.focused.as_ref() == Some(object) {
      input.applied_cursor = input.committed_cursor;
    }
  }
}

/// Applies deletion, insertion, and preedit as one text-input-v3 commit.
pub(crate) fn commit_input_method(
  state: &mut CompositorState,
  id: &ObjectId,
  edit: InputMethodEdit,
) {
  let Some(input) = state
    .text_inputs
    .resources
    .get_mut(id)
    .filter(|input| input.enabled && input.focused.is_some())
  else {
    return;
  };
  if edit.delete_before > 0 || edit.delete_after > 0 {
    input
      .resource
      .delete_surrounding_text(edit.delete_before, edit.delete_after);
  }
  if !edit.commit_string.is_empty() {
    input.resource.commit_string(Some(edit.commit_string));
  }
  let preedit =
    (!edit.preedit_string.is_empty()).then_some(edit.preedit_string);
  input
    .resource
    .preedit_string(preedit, edit.cursor_begin, edit.cursor_end);
  input.resource.done(input.commit_serial);
}

pub(crate) fn commit_string(
  state: &mut CompositorState,
  id: &ObjectId,
  serial: u32,
  text: String,
) {
  if let Some(input) = state.text_inputs.for_event(id, serial) {
    input.resource.commit_string(Some(text));
    input.resource.done(input.commit_serial);
  }
}

pub(crate) fn preedit_string(
  state: &mut CompositorState,
  id: &ObjectId,
  serial: u32,
  text: String,
  cursor: i32,
) {
  if let Some(input) = state.text_inputs.for_event(id, serial) {
    let text = (!text.is_empty()).then_some(text);
    input.resource.preedit_string(text, cursor, cursor);
    input.resource.done(input.commit_serial);
  }
}

pub(crate) fn delete_surrounding_text(
  state: &mut CompositorState,
  id: &ObjectId,
  serial: u32,
  index: i32,
  length: u32,
) {
  if index > 0 {
    return;
  }
  let before = -i64::from(index);
  let after = before + i64::from(length);
  if after < 0 {
    return;
  }
  if let Some(input) = state.text_inputs.for_event(id, serial) {
    input
      .resource
      .delete_surrounding_text(before as u32, after as u32);
    input.resource.done(input.commit_serial);
  }
}

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_text_input_manager_v3");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpTextInputManagerV3>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpTextInputManagerV3,
    request: zwp_text_input_manager_v3::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_text_input_manager_v3::Request::GetTextInput { id, seat } =
      request
    {
      let input = data_init.init(id, ());
      state.text_inputs.resources.insert(
        Resource::id(&input),
        TextInput {
          resource: input,
          seat: Resource::id(&seat),
          focused: None,
          enabled: false,
          pending_enabled: false,
          pending: ContentState::default(),
          current: ContentState::default(),
          committed_cursor: None,
          applied_cursor: None,
          commit_serial: 0,
        },
      );
      sync(state);
    }
  }
}

impl Dispatch<ZwpTextInputV3, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpTextInputV3,
    request: zwp_text_input_v3::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let competing = if matches!(&request, zwp_text_input_v3::Request::Enable) {
      let Some(current) =
        state.text_inputs.resources.get(&Resource::id(resource))
      else {
        return;
      };
      state.text_inputs.resources.values().any(|other| {
        other.seat == current.seat
          && other.enabled
          && Resource::id(&other.resource) != Resource::id(resource)
      })
    } else {
      false
    };
    let Some(input) =
      state.text_inputs.resources.get_mut(&Resource::id(resource))
    else {
      return;
    };
    if matches!(request, zwp_text_input_v3::Request::Destroy) {
      state.text_inputs.resources.remove(&Resource::id(resource));
      return;
    }
    if input.focused.is_none() {
      return;
    }
    match request {
      zwp_text_input_v3::Request::Enable => {
        if !competing {
          input.pending_enabled = true;
          input.pending = ContentState::default();
        }
      }
      zwp_text_input_v3::Request::Disable => {
        input.pending_enabled = false;
      }
      zwp_text_input_v3::Request::SetSurroundingText {
        text,
        cursor,
        anchor,
      } => {
        input.pending.surrounding = Some((text, cursor, anchor));
      }
      zwp_text_input_v3::Request::SetTextChangeCause { cause } => {
        input.pending.change_cause = match cause {
          WEnum::Value(cause) => cause as u32,
          WEnum::Unknown(value) => value,
        };
      }
      zwp_text_input_v3::Request::SetContentType { hint, purpose } => {
        input.pending.content_hint = hint.into();
        input.pending.content_purpose = purpose.into();
      }
      zwp_text_input_v3::Request::SetCursorRectangle {
        x,
        y,
        width,
        height,
      } => {
        input.pending.cursor_rectangle = (width > 0 && height > 0)
          .then_some(Rect::new(x, y, width as u32, height as u32));
      }
      zwp_text_input_v3::Request::Commit => {
        input.enabled = input.pending_enabled;
        if !input.enabled {
          input.current = ContentState::default();
          input.pending = ContentState::default();
          input.pending_enabled = false;
        } else {
          input.current = input.pending.clone();
          input.pending.change_cause = 0;
        }
        input.committed_cursor = input.current.cursor_rectangle;
        input.commit_serial = input.commit_serial.wrapping_add(1);
        input.resource.done(input.commit_serial);
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpTextInputV3,
    _data: &(),
  ) {
    state.text_inputs.resources.remove(&Resource::id(resource));
  }
}
