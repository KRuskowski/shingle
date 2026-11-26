//! The text inputs of clients, `zwp_text_input_v3` and
//! `zwp_text_input_v1`, in one model that both input-method versions read.
//!
//! Each text input follows the keyboard focus of its own client. The
//! enabled one an input method serves is the one enabled most recently.
//! Its state is kept in text-input-v3's terms, whichever protocol it came
//! from, and the input method's edits go back in the protocol of the text
//! input: [`commit_input_method`] for input-method v2, [`context_event`]
//! for input-method v1.

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

pub(crate) mod v1;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Default)]
struct ContentState {
  surrounding: Option<(String, i32, i32)>,
  change_cause: u32,
  content_hint: u32,
  content_purpose: u32,
  cursor_rectangle: Option<Rect>,
}

/// The protocol object behind a text input.
enum Protocol {
  V3(ZwpTextInputV3),
  V1(v1::ZwpTextInputV1),
}

impl Protocol {
  fn client(&self) -> Option<Client> {
    match self {
      Self::V3(resource) => resource.client(),
      Self::V1(resource) => resource.client(),
    }
  }

  fn id(&self) -> ObjectId {
    match self {
      Self::V3(resource) => Resource::id(resource),
      Self::V1(resource) => Resource::id(resource),
    }
  }
}

struct TextInput {
  protocol: Protocol,
  /// The seat it was created for (v3) or last activated on (v1).
  seat: Option<ObjectId>,
  focused: Option<ObjectId>,
  enabled: bool,
  pending_enabled: bool,
  pending: ContentState,
  current: ContentState,
  committed_cursor: Option<Rect>,
  applied_cursor: Option<Rect>,
  /// The serial the client knows its state by: v3 counts its commits,
  /// v1 names it in `commit_state`.
  commit_serial: u32,
  /// Changes whenever the state an input method sees changes, so that
  /// each change reaches it once.
  generation: u32,
  /// When it was last enabled, by [`TextInputs::activations`].
  activated: u64,
  /// Whether a v1 text input shows a preedit from input-method v2.
  preedit_shown: bool,
}

impl TextInput {
  fn new(protocol: Protocol, seat: Option<ObjectId>) -> Self {
    Self {
      protocol,
      seat,
      focused: None,
      enabled: false,
      pending_enabled: false,
      pending: ContentState::default(),
      current: ContentState::default(),
      committed_cursor: None,
      applied_cursor: None,
      commit_serial: 0,
      generation: 0,
      activated: 0,
      preedit_shown: false,
    }
  }

  /// Forgets everything the client set, as on a focus change.
  fn reset(&mut self) {
    self.enabled = false;
    self.pending_enabled = false;
    self.pending = ContentState::default();
    self.current = ContentState::default();
    self.committed_cursor = None;
    self.applied_cursor = None;
    self.preedit_shown = false;
    self.generation = self.generation.wrapping_add(1);
  }
}

#[derive(Default)]
pub(crate) struct TextInputs {
  resources: HashMap<ObjectId, TextInput>,
  /// Counts enables, so that the most recent one wins.
  activations: u64,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpTextInputManagerV3, ()>(1, ());
}

/// Follows the keyboard focus: v3 gets leave before enter, an activated v1
/// text input gets leave, and either forgets its state on a change.
pub(crate) fn sync(state: &mut CompositorState) {
  let focus = state.input.keyboard_focus.clone();
  for input in state.text_inputs.resources.values_mut() {
    let target = focus.as_ref().filter(|object| {
      state.surfaces.get(*object).is_some_and(|surface| {
        input
          .protocol
          .client()
          .is_some_and(|client| client.id() == surface.client)
      })
    });
    if input.focused == target.cloned() {
      continue;
    }
    let previous = input.focused.take();
    match &input.protocol {
      Protocol::V3(resource) => {
        if let Some(previous) = previous
          && let Some(surface) = state.surfaces.get(&previous)
        {
          resource.leave(&surface.resource);
        }
      }
      Protocol::V1(resource) => {
        if input.enabled {
          resource.leave();
        }
      }
    }
    input.reset();
    if let Some(target) = target
      && let Some(surface) = state.surfaces.get(target)
      && let Protocol::V3(resource) = &input.protocol
    {
      resource.enter(&surface.resource);
    }
    input.focused = target.cloned();
  }
}

pub(crate) struct TextInputSnapshot {
  pub(crate) surrounding: Option<(String, i32, i32)>,
  pub(crate) change_cause: u32,
  pub(crate) content_hint: u32,
  /// The content purpose in text-input-v3's numbering.
  pub(crate) content_purpose: u32,
  pub(crate) cursor_rectangle: Option<Rect>,
  pub(crate) surface: ObjectId,
  pub(crate) commit_serial: u32,
  /// Changes with every change an input method should hear about.
  pub(crate) generation: u32,
}

impl TextInputs {
  /// The enabled, focused text input enabled last.
  pub(crate) fn active(&self) -> Option<ObjectId> {
    self
      .resources
      .values()
      .filter(|input| input.enabled && input.focused.is_some())
      .max_by_key(|input| input.activated)
      .map(|input| input.protocol.id())
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
      generation: input.generation,
    })
  }

  fn next_activation(&mut self) -> u64 {
    self.activations += 1;
    self.activations
  }

  fn for_event(
    &mut self,
    id: &ObjectId,
    serial: u32,
  ) -> Option<(&ZwpTextInputV3, u32)> {
    self
      .resources
      .get_mut(id)
      .filter(|input| {
        input.enabled
          && input.focused.is_some()
          && input.commit_serial == serial
      })
      .and_then(|input| match &input.protocol {
        Protocol::V3(resource) => Some((resource, input.commit_serial)),
        Protocol::V1(_) => None,
      })
  }

  fn enabled_mut(&mut self, id: &ObjectId) -> Option<&mut TextInput> {
    self
      .resources
      .get_mut(id)
      .filter(|input| input.enabled && input.focused.is_some())
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
    if input.enabled
      && input.focused.as_ref() == Some(object)
      && matches!(input.protocol, Protocol::V3(_))
    {
      input.applied_cursor = input.committed_cursor;
    }
  }
}

/// Applies deletion, insertion, and preedit as one text-input commit: one
/// `done` for v3, and for v1 the deletion with the commit string it
/// belongs to, then the preedit.
pub(crate) fn commit_input_method(
  state: &mut CompositorState,
  id: &ObjectId,
  edit: InputMethodEdit,
) {
  let Some(input) = state.text_inputs.enabled_mut(id) else {
    return;
  };
  match &input.protocol {
    Protocol::V3(resource) => {
      if edit.delete_before > 0 || edit.delete_after > 0 {
        resource.delete_surrounding_text(edit.delete_before, edit.delete_after);
      }
      if !edit.commit_string.is_empty() {
        resource.commit_string(Some(edit.commit_string));
      }
      let preedit =
        (!edit.preedit_string.is_empty()).then_some(edit.preedit_string);
      resource.preedit_string(preedit, edit.cursor_begin, edit.cursor_end);
      resource.done(input.commit_serial);
    }
    Protocol::V1(resource) => {
      let serial = input.commit_serial;
      let deletes = edit.delete_before > 0 || edit.delete_after > 0;
      if deletes {
        let (index, length) =
          v1::deletion(edit.delete_before, edit.delete_after);
        resource.delete_surrounding_text(index, length);
      }
      // v1 applies a deletion with the next commit string, so a deletion
      // alone still needs one.
      if deletes || !edit.commit_string.is_empty() {
        resource.commit_string(serial, edit.commit_string);
        input.preedit_shown = false;
      }
      if !edit.preedit_string.is_empty() {
        resource.preedit_cursor(edit.cursor_begin);
        resource.preedit_string(serial, edit.preedit_string, String::new());
        input.preedit_shown = true;
      } else if input.preedit_shown {
        resource.preedit_string(serial, String::new(), String::new());
        input.preedit_shown = false;
      }
    }
  }
}

/// What an input-method-v1 context sends to the text input it serves.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ContextEvent {
  CommitString {
    serial: u32,
    text: String,
  },
  PreeditString {
    serial: u32,
    text: String,
    commit: String,
  },
  PreeditStyling {
    index: u32,
    length: u32,
    style: u32,
  },
  PreeditCursor {
    index: i32,
  },
  DeleteSurroundingText {
    index: i32,
    length: u32,
  },
  CursorPosition {
    index: i32,
    anchor: i32,
  },
  ModifiersMap {
    map: Vec<u8>,
  },
  Keysym {
    serial: u32,
    time: u32,
    sym: u32,
    state: u32,
    modifiers: u32,
  },
  Language {
    serial: u32,
    language: String,
  },
  TextDirection {
    serial: u32,
    direction: u32,
  },
}

/// Hands an input-method-v1 context's event to text input `id`: a v1 text
/// input gets it as it is, as Wayfire passes it on; a v3 text input gets
/// what v3 can say of it, with the preedit cursor `cursor` the context
/// last set.
pub(crate) fn context_event(
  state: &mut CompositorState,
  id: &ObjectId,
  event: ContextEvent,
  cursor: i32,
) {
  let is_v1 = state
    .text_inputs
    .resources
    .get(id)
    .is_some_and(|input| matches!(input.protocol, Protocol::V1(_)));
  if is_v1 {
    if let Some(input) = state.text_inputs.enabled_mut(id)
      && let Protocol::V1(resource) = &input.protocol
    {
      v1::send(resource, event);
    }
    return;
  }
  match event {
    ContextEvent::CommitString { serial, text } => {
      if let Some((resource, serial)) = state.text_inputs.for_event(id, serial)
      {
        resource.commit_string(Some(text));
        resource.done(serial);
      }
    }
    ContextEvent::PreeditString { serial, text, .. } => {
      if let Some((resource, serial)) = state.text_inputs.for_event(id, serial)
      {
        let text = (!text.is_empty()).then_some(text);
        resource.preedit_string(text, cursor, cursor);
        resource.done(serial);
      }
    }
    ContextEvent::DeleteSurroundingText { index, length } => {
      let serial = state
        .text_inputs
        .resources
        .get(id)
        .map_or(0, |input| input.commit_serial);
      delete_surrounding_text(state, id, serial, index, length);
    }
    _ => {}
  }
}

fn delete_surrounding_text(
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
  if let Some((resource, serial)) = state.text_inputs.for_event(id, serial) {
    resource.delete_surrounding_text(before as u32, after as u32);
    resource.done(serial);
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
        TextInput::new(Protocol::V3(input), Some(Resource::id(&seat))),
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
        matches!(other.protocol, Protocol::V3(_))
          && other.seat == current.seat
          && other.enabled
          && other.protocol.id() != Resource::id(resource)
      })
    } else {
      false
    };
    if matches!(request, zwp_text_input_v3::Request::Destroy) {
      state.text_inputs.resources.remove(&Resource::id(resource));
      return;
    }
    let activation = if matches!(request, zwp_text_input_v3::Request::Commit) {
      state.text_inputs.next_activation()
    } else {
      0
    };
    let Some(input) =
      state.text_inputs.resources.get_mut(&Resource::id(resource))
    else {
      return;
    };
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
        let was_enabled = input.enabled;
        input.enabled = input.pending_enabled;
        if !input.enabled {
          input.current = ContentState::default();
          input.pending = ContentState::default();
          input.pending_enabled = false;
        } else {
          input.current = input.pending.clone();
          input.pending.change_cause = 0;
          if !was_enabled {
            input.activated = activation;
          }
        }
        input.committed_cursor = input.current.cursor_rectangle;
        input.commit_serial = input.commit_serial.wrapping_add(1);
        input.generation = input.generation.wrapping_add(1);
        if let Protocol::V3(resource) = &input.protocol {
          resource.done(input.commit_serial);
        }
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
