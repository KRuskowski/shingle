use std::collections::{BTreeSet, HashMap};

use wayland_protocols::wp::input_method::zv1::server::{
    zwp_input_method_context_v1::{self, ZwpInputMethodContextV1},
    zwp_input_method_v1::{self, ZwpInputMethodV1},
};
use wayland_protocols_misc::zwp_input_method_v2::server::
    zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2;
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
    protocol::wl_keyboard::{self, WlKeyboard},
};

use crate::{
  state::CompositorState,
  text_input::{self, ContextEvent},
};

struct InputMethod {
  resource: ZwpInputMethodV1,
  active_input: Option<ObjectId>,
  context: Option<ObjectId>,
  synced_serial: Option<u32>,
}

struct Context {
  resource: ZwpInputMethodContextV1,
  method: ObjectId,
  preedit_cursor: i32,
}

pub(crate) enum GrabKeyboard {
  V1(WlKeyboard),
  V2(ZwpInputMethodKeyboardGrabV2),
}

impl GrabKeyboard {
  fn object(&self) -> ObjectId {
    match self {
      Self::V1(keyboard) => Resource::id(keyboard),
      Self::V2(keyboard) => Resource::id(keyboard),
    }
  }

  fn key(
    &self,
    serial: u32,
    time: u32,
    key: u32,
    state: wl_keyboard::KeyState,
  ) {
    match self {
      Self::V1(keyboard) => keyboard.key(serial, time, key, state),
      Self::V2(keyboard) => keyboard.key(serial, time, key, state),
    }
  }

  fn modifiers(&self, serial: u32, modifiers: crate::input::Modifiers) {
    match self {
      Self::V1(keyboard) => keyboard.modifiers(
        serial,
        modifiers.depressed,
        modifiers.latched,
        modifiers.locked,
        modifiers.group,
      ),
      Self::V2(keyboard) => keyboard.modifiers(
        serial,
        modifiers.depressed,
        modifiers.latched,
        modifiers.locked,
        modifiers.group,
      ),
    }
  }
}

struct Grab {
  keyboard: GrabKeyboard,
  forwarded_pressed: BTreeSet<u32>,
}

#[derive(Default)]
pub(crate) struct InputMethods {
  methods: HashMap<ObjectId, InputMethod>,
  contexts: HashMap<ObjectId, Context>,
  grabs: HashMap<ObjectId, Grab>,
  forwarding: bool,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpInputMethodV1, ()>(1, ());
}

/// Routes a hardware key to an active input-method grab.
pub(crate) fn hardware_key(
  state: &mut CompositorState,
  key: u32,
  pressed: bool,
  event_time: Option<u32>,
) -> bool {
  if state.input_methods.forwarding || state.input_methods.grabs.is_empty() {
    return false;
  }
  let time = event_time.unwrap_or_else(|| crate::input::timestamp(state));
  let serial = state.serial();
  let state_value = if pressed {
    wl_keyboard::KeyState::Pressed
  } else {
    wl_keyboard::KeyState::Released
  };
  for grab in state.input_methods.grabs.values() {
    grab.keyboard.key(serial, time, key, state_value);
  }
  true
}

/// Routes modifier changes to the active grab instead of the application.
pub(crate) fn hardware_modifiers(
  state: &mut CompositorState,
  modifiers: crate::input::Modifiers,
) -> bool {
  if state.input_methods.forwarding || state.input_methods.grabs.is_empty() {
    return false;
  }
  let serial = state.serial();
  for grab in state.input_methods.grabs.values() {
    grab.keyboard.modifiers(serial, modifiers);
  }
  true
}

pub(crate) fn forward_key(
  state: &mut CompositorState,
  context: &ObjectId,
  key: u32,
  pressed: bool,
  time: u32,
) {
  state.input_methods.forwarding = true;
  crate::input::forward_keyboard_key(state, key, pressed, time);
  state.input_methods.forwarding = false;
  if let Some(grab) = state.input_methods.grabs.get_mut(context) {
    if pressed {
      grab.forwarded_pressed.insert(key);
    } else {
      grab.forwarded_pressed.remove(&key);
    }
  }
}

pub(crate) fn forward_modifiers(
  state: &mut CompositorState,
  modifiers: crate::input::Modifiers,
) {
  state.input_methods.forwarding = true;
  crate::input::keyboard_modifiers(state, modifiers);
  state.input_methods.forwarding = false;
}

/// Registers either protocol version's keyboard grab with shared input routing.
pub(crate) fn register_keyboard_grab(
  state: &mut CompositorState,
  context: ObjectId,
  keyboard: GrabKeyboard,
) {
  if !state.input_methods.grabs.is_empty() {
    return;
  }
  match &keyboard {
    GrabKeyboard::V1(resource) => {
      crate::input::send_keymap(state, resource);
      crate::input::send_repeat(&state.input, resource);
    }
    GrabKeyboard::V2(resource) => {
      crate::input::send_keymap_v2(state, resource);
      crate::input::send_repeat_v2(&state.input, resource);
    }
  }
  let modifiers = state.input.modifiers;
  keyboard.modifiers(state.serial(), modifiers);
  state.input_methods.grabs.insert(
    context,
    Grab {
      keyboard,
      forwarded_pressed: BTreeSet::new(),
    },
  );
}

/// Releases forwarded keys when a grab object is destroyed directly.
pub(crate) fn keyboard_destroyed(
  state: &mut CompositorState,
  keyboard: &ObjectId,
) {
  let context = state
    .input_methods
    .grabs
    .iter()
    .find_map(|(context, grab)| {
      (&grab.keyboard.object() == keyboard).then(|| context.clone())
    });
  if let Some(context) = context {
    release_context(state, &context);
  }
}

pub(crate) fn keymaps_changed(state: &CompositorState) {
  for grab in state.input_methods.grabs.values() {
    match &grab.keyboard {
      GrabKeyboard::V1(keyboard) => {
        crate::input::send_keymap(state, keyboard);
      }
      GrabKeyboard::V2(keyboard) => {
        crate::input::send_keymap_v2(state, keyboard);
      }
    }
  }
}

pub(crate) fn repeats_changed(state: &CompositorState) {
  for grab in state.input_methods.grabs.values() {
    match &grab.keyboard {
      GrabKeyboard::V1(keyboard) => {
        crate::input::send_repeat(&state.input, keyboard);
      }
      GrabKeyboard::V2(keyboard) => {
        crate::input::send_repeat_v2(&state.input, keyboard);
      }
    }
  }
}

/// Keeps each input-method context tied to the active text-input commit state.
pub(crate) fn sync(state: &mut CompositorState, handle: &DisplayHandle) {
  let active = state.text_inputs.active();
  let methods = state
    .input_methods
    .methods
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for method_id in methods {
    let current = state.input_methods.methods[&method_id].active_input.clone();
    if current != active {
      deactivate(state, &method_id);
      if let Some(input) = &active {
        activate(state, handle, &method_id, input.clone());
      }
    }
    let Some(input) =
      state.input_methods.methods[&method_id].active_input.clone()
    else {
      continue;
    };
    let Some(snapshot) = state.text_inputs.snapshot(&input) else {
      continue;
    };
    if state.input_methods.methods[&method_id].synced_serial
      == Some(snapshot.generation)
    {
      continue;
    }
    sync_context(state, &method_id, snapshot);
  }
}

fn activate(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  method_id: &ObjectId,
  input: ObjectId,
) {
  let Some(method) = state.input_methods.methods.get(method_id) else {
    return;
  };
  let Some(client) = method.resource.client() else {
    return;
  };
  let Ok(context) = client
    .create_resource::<ZwpInputMethodContextV1, (), CompositorState>(
      handle,
      1,
      (),
    )
  else {
    return;
  };
  let context_id = Resource::id(&context);
  state.input_methods.contexts.insert(
    context_id.clone(),
    Context {
      resource: context.clone(),
      method: method_id.clone(),
      preedit_cursor: 0,
    },
  );
  if let Some(method) = state.input_methods.methods.get_mut(method_id) {
    method.active_input = Some(input);
    method.context = Some(context_id);
    method.synced_serial = None;
    method.resource.activate(&context);
  }
}

fn deactivate(state: &mut CompositorState, method_id: &ObjectId) {
  let context_id = state
    .input_methods
    .methods
    .get(method_id)
    .and_then(|method| method.context.clone());
  if let Some(context_id) = context_id {
    release_context(state, &context_id);
  }
  let Some(method) = state.input_methods.methods.get_mut(method_id) else {
    return;
  };
  if let Some(context_id) = method.context.take()
    && let Some(context) = state.input_methods.contexts.remove(&context_id)
  {
    method.resource.deactivate(&context.resource);
  }
  method.active_input = None;
  method.synced_serial = None;
}

fn sync_context(
  state: &mut CompositorState,
  method_id: &ObjectId,
  snapshot: text_input::TextInputSnapshot,
) {
  let context = state
    .input_methods
    .methods
    .get(method_id)
    .and_then(|method| {
      method
        .context
        .as_ref()
        .and_then(|id| state.input_methods.contexts.get(id))
    });
  let Some(context) = context else {
    return;
  };
  if let Some((text, cursor, anchor)) = snapshot.surrounding {
    context.resource.surrounding_text(
      text,
      cursor.max(0) as u32,
      anchor.max(0) as u32,
    );
  }
  // input-method v1 numbers content purposes as text-input v1 does.
  context.resource.content_type(
    snapshot.content_hint,
    text_input::v1::purpose_from_v3(snapshot.content_purpose),
  );
  context.resource.commit_state(snapshot.commit_serial);
  if let Some(method) = state.input_methods.methods.get_mut(method_id) {
    method.synced_serial = Some(snapshot.generation);
  }
}

impl GlobalDispatch<ZwpInputMethodV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_input_method_v1");

  fn bind(
    state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpInputMethodV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let method = data_init.init(resource, ());
    state.input_methods.methods.insert(
      Resource::id(&method),
      InputMethod {
        resource: method,
        active_input: None,
        context: None,
        synced_serial: None,
      },
    );
  }
}

impl Dispatch<ZwpInputMethodV1, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZwpInputMethodV1,
    _request: zwp_input_method_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpInputMethodV1,
    _data: &(),
  ) {
    let id = Resource::id(resource);
    deactivate(state, &id);
    state.input_methods.methods.remove(&id);
  }
}

impl Dispatch<ZwpInputMethodContextV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpInputMethodContextV1,
    request: zwp_input_method_context_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let Some((input, cursor)) = state
      .input_methods
      .contexts
      .get(&Resource::id(resource))
      .and_then(|context| {
        state
          .input_methods
          .methods
          .get(&context.method)
          .and_then(|method| method.active_input.clone())
          .map(|input| (input, context.preedit_cursor))
      })
    else {
      return;
    };
    let event = match request {
      zwp_input_method_context_v1::Request::GrabKeyboard { keyboard } => {
        grab_keyboard(state, resource, keyboard, data_init);
        return;
      }
      zwp_input_method_context_v1::Request::Key {
        time,
        key,
        state: key_state,
        ..
      } => {
        let pressed = key_state == wl_keyboard::KeyState::Pressed as u32;
        forward_key(state, &Resource::id(resource), key, pressed, time);
        return;
      }
      zwp_input_method_context_v1::Request::Modifiers {
        mods_depressed,
        mods_latched,
        mods_locked,
        group,
        ..
      } => {
        forward_modifiers(
          state,
          crate::input::Modifiers {
            depressed: mods_depressed,
            latched: mods_latched,
            locked: mods_locked,
            group,
          },
        );
        return;
      }
      zwp_input_method_context_v1::Request::PreeditCursor { index } => {
        if let Some(context) = state
          .input_methods
          .contexts
          .get_mut(&Resource::id(resource))
        {
          context.preedit_cursor = index;
        }
        ContextEvent::PreeditCursor { index }
      }
      request => match context_event(request) {
        Some(event) => event,
        None => return,
      },
    };
    text_input::context_event(state, &input, event, cursor);
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpInputMethodContextV1,
    _data: &(),
  ) {
    let id = Resource::id(resource);
    release_context(state, &id);
    state.input_methods.contexts.remove(&id);
  }
}

fn grab_keyboard(
  state: &mut CompositorState,
  resource: &ZwpInputMethodContextV1,
  keyboard: New<WlKeyboard>,
  data_init: &mut DataInit<'_, CompositorState>,
) {
  let context = Resource::id(resource);
  if !state.input_methods.contexts.contains_key(&context) {
    return;
  }
  let keyboard = data_init.init(keyboard, crate::input::KeyboardData);
  register_keyboard_grab(state, context, GrabKeyboard::V1(keyboard));
}

pub(crate) fn release_context(state: &mut CompositorState, context: &ObjectId) {
  let Some(grab) = state.input_methods.grabs.remove(context) else {
    return;
  };
  let time = crate::input::timestamp(state);
  for key in grab.forwarded_pressed {
    forward_key(state, context, key, false, time);
  }
}

/// The text-input event of a context request that is one.
fn context_event(
  request: zwp_input_method_context_v1::Request,
) -> Option<ContextEvent> {
  use zwp_input_method_context_v1::Request;
  Some(match request {
    Request::CommitString { serial, text } => {
      ContextEvent::CommitString { serial, text }
    }
    Request::PreeditString {
      serial,
      text,
      commit,
    } => ContextEvent::PreeditString {
      serial,
      text,
      commit,
    },
    Request::PreeditStyling {
      index,
      length,
      style,
    } => ContextEvent::PreeditStyling {
      index,
      length,
      style,
    },
    Request::DeleteSurroundingText { index, length } => {
      ContextEvent::DeleteSurroundingText { index, length }
    }
    Request::CursorPosition { index, anchor } => {
      ContextEvent::CursorPosition { index, anchor }
    }
    Request::ModifiersMap { map } => ContextEvent::ModifiersMap { map },
    Request::Keysym {
      serial,
      time,
      sym,
      state,
      modifiers,
    } => ContextEvent::Keysym {
      serial,
      time,
      sym,
      state,
      modifiers,
    },
    Request::Language { serial, language } => {
      ContextEvent::Language { serial, language }
    }
    Request::TextDirection { serial, direction } => {
      ContextEvent::TextDirection { serial, direction }
    }
    _ => return None,
  })
}

/// A request of a text-input-v1 client that only input-method v1 has a
/// counterpart for.
pub(crate) enum TextInputRequest {
  Reset,
  PreferredLanguage(String),
  InvokeAction { button: u32, index: u32 },
}

/// Passes `request` of text input `input` to the contexts serving it.
pub(crate) fn text_input_request(
  state: &CompositorState,
  input: &ObjectId,
  request: TextInputRequest,
) {
  let contexts = state
    .input_methods
    .methods
    .values()
    .filter(|method| method.active_input.as_ref() == Some(input))
    .filter_map(|method| method.context.as_ref())
    .filter_map(|id| state.input_methods.contexts.get(id));
  for context in contexts {
    match &request {
      TextInputRequest::Reset => context.resource.reset(),
      TextInputRequest::PreferredLanguage(language) => {
        context.resource.preferred_language(language.clone());
      }
      TextInputRequest::InvokeAction { button, index } => {
        context.resource.invoke_action(*button, *index);
      }
    }
  }
}
