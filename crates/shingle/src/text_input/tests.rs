//! text-input-v1 through the in-process test client: every request and
//! event, with input-method v1 and v2 on the other side.

use wayland_client::{Proxy, backend::ObjectId, protocol::wl_seat::WlSeat};
use wayland_protocols::wp::{
  input_method::zv1::client::{
    zwp_input_method_context_v1::{self, ZwpInputMethodContextV1},
    zwp_input_method_v1::{self, ZwpInputMethodV1},
  },
  text_input::zv1::client::{
    zwp_text_input_manager_v1::ZwpTextInputManagerV1,
    zwp_text_input_v1::{self, ZwpTextInputV1},
  },
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
  zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
  zwp_input_method_v2::{self, ZwpInputMethodV2},
};

use crate::{
  Compositor, Config,
  test_client::{TestClient, Toplevel},
  tests::{test_compositor, test_compositor_with},
};

/// An application with a mapped, focused toplevel and a text input.
struct App {
  client: TestClient,
  seat: WlSeat,
  window: Toplevel,
  input: ZwpTextInputV1,
}

impl App {
  fn new(compositor: &mut Compositor, path: &std::path::Path) -> Self {
    let mut client = TestClient::connect(compositor, path);
    let seat = client.bind::<WlSeat>(7);
    let window = client.toplevel(compositor, "app");
    client.attach_buffer(compositor, &window.surface, 100, 100);
    let manager = client.bind::<ZwpTextInputManagerV1>(1);
    let input = manager.create_text_input(&client.handle, ());
    client.roundtrip(compositor);
    assert!(compositor.state.input.keyboard_focus.is_some());
    Self {
      client,
      seat,
      window,
      input,
    }
  }

  fn activate(&mut self, compositor: &mut Compositor) {
    self.input.activate(&self.seat, &self.window.surface);
    self.client.roundtrip(compositor);
  }

  /// Takes the events this text input received since the last call.
  fn take(&mut self) -> Vec<zwp_text_input_v1::Event> {
    take_for(&mut self.client.events.text_input_v1, &self.input.id())
  }
}

fn take_for<E>(events: &mut Vec<(ObjectId, E)>, id: &ObjectId) -> Vec<E> {
  let (mine, others) = std::mem::take(events)
    .into_iter()
    .partition::<Vec<_>, _>(|(object, _)| object == id);
  *events = others;
  mine.into_iter().map(|(_, event)| event).collect()
}

/// An input-method-v1 client.
struct MethodV1 {
  client: TestClient,
  _method: ZwpInputMethodV1,
}

impl MethodV1 {
  fn new(compositor: &mut Compositor, path: &std::path::Path) -> Self {
    let mut client = TestClient::connect(compositor, path);
    let method = client.bind::<ZwpInputMethodV1>(1);
    client.roundtrip(compositor);
    Self {
      client,
      _method: method,
    }
  }

  /// The context of the last `activate`, while it was not deactivated.
  fn context(&self) -> Option<ZwpInputMethodContextV1> {
    match self.client.events.input_method_v1.last()? {
      zwp_input_method_v1::Event::Activate { id } => Some(id.clone()),
      _ => None,
    }
  }

  fn take(&mut self) -> Vec<zwp_input_method_context_v1::Event> {
    std::mem::take(&mut self.client.events.input_method_context)
      .into_iter()
      .map(|(_, event)| event)
      .collect()
  }
}

#[test]
fn the_global_follows_its_option() {
  let interfaces = |config: Config| {
    let (mut compositor, path) = test_compositor_with(config);
    let client = TestClient::connect(&mut compositor, &path);
    client
      .events
      .globals
      .iter()
      .map(|(_, interface, _)| interface.clone())
      .collect::<Vec<_>>()
  };
  let has = |all: &[String], name: &str| all.iter().any(|item| item == name);
  let all = interfaces(Config::default());
  assert!(has(&all, "zwp_text_input_manager_v1"));
  assert!(has(&all, "zwp_text_input_manager_v3"));
  let without = interfaces(Config {
    enable_text_input_v1: false,
    ..Config::default()
  });
  assert!(!has(&without, "zwp_text_input_manager_v1"));
  assert!(has(&without, "zwp_input_method_v1"));
  // text-input-v1 alone still has input methods to serve it.
  let alone = interfaces(Config {
    enable_text_input_v3: false,
    ..Config::default()
  });
  assert!(has(&alone, "zwp_text_input_manager_v1"));
  assert!(!has(&alone, "zwp_text_input_manager_v3"));
  assert!(has(&alone, "zwp_input_method_v1"));
  assert!(has(&alone, "zwp_input_method_manager_v2"));
  assert!(has(&alone, "zwp_input_panel_v1"));
  let neither = interfaces(Config {
    enable_text_input_v1: false,
    enable_text_input_v3: false,
    ..Config::default()
  });
  assert!(!has(&neither, "zwp_input_method_v1"));
}

#[test]
fn activate_needs_the_focused_surface_and_enters_it() {
  let (mut compositor, path) = test_compositor();
  let mut app = App::new(&mut compositor, &path);
  let mut method = MethodV1::new(&mut compositor, &path);
  // Another surface of the client has no keyboard focus.
  let other = app.client.toplevel(&mut compositor, "other-unmapped");
  app.input.activate(&app.seat, &other.surface);
  app.client.roundtrip(&mut compositor);
  assert!(app.take().is_empty());
  method.client.roundtrip(&mut compositor);
  assert!(method.context().is_none());

  app.activate(&mut compositor);
  let events = app.take();
  assert!(
    matches!(
      &events[..],
      [zwp_text_input_v1::Event::Enter { surface }]
        if *surface == app.window.surface
    ),
    "{events:?}"
  );
  method.client.roundtrip(&mut compositor);
  assert!(method.context().is_some());
}

#[test]
fn state_requests_reach_input_method_v1() {
  let (mut compositor, path) = test_compositor();
  let mut app = App::new(&mut compositor, &path);
  let mut method = MethodV1::new(&mut compositor, &path);
  app.activate(&mut compositor);
  method.client.roundtrip(&mut compositor);
  method.take();

  app.input.set_surrounding_text("hello".into(), 5, 2);
  // `date` is 9 in text-input v1; the model keeps v3's 10 and hands
  // input-method v1 its own 9 back.
  app.input.set_content_type(
    zwp_text_input_v1::ContentHint::AutoCompletion
      | zwp_text_input_v1::ContentHint::Latin,
    zwp_text_input_v1::ContentPurpose::Date,
  );
  app.input.set_cursor_rectangle(10, 20, 2, 16);
  app.input.commit_state(41);
  app.client.roundtrip(&mut compositor);
  method.client.roundtrip(&mut compositor);
  let events = method.take();
  assert!(
    matches!(
      &events[..],
      [
        zwp_input_method_context_v1::Event::SurroundingText {
          text,
          cursor: 5,
          anchor: 2,
        },
        zwp_input_method_context_v1::Event::ContentType {
          hint: 0x101,
          purpose: 9,
        },
        zwp_input_method_context_v1::Event::CommitState { serial: 41 },
      ] if text == "hello"
    ),
    "{events:?}"
  );
  let active = compositor.state.text_inputs.active().unwrap();
  let snapshot = compositor.state.text_inputs.snapshot(&active).unwrap();
  assert_eq!(snapshot.content_purpose, 10);
  assert_eq!(
    snapshot.cursor_rectangle,
    Some(shingle_core::Rect::new(10, 20, 2, 16))
  );

  app.input.reset();
  app.input.set_preferred_language("de".into());
  app.input.invoke_action(1, 3);
  app.input.show_input_panel();
  app.input.hide_input_panel();
  app.client.roundtrip(&mut compositor);
  method.client.roundtrip(&mut compositor);
  let events = method.take();
  assert!(
    matches!(
      &events[..],
      [
        zwp_input_method_context_v1::Event::Reset,
        zwp_input_method_context_v1::Event::PreferredLanguage { language },
        zwp_input_method_context_v1::Event::InvokeAction {
          button: 1,
          index: 3,
        },
      ] if language == "de"
    ),
    "{events:?}"
  );
}

#[test]
fn input_method_v1_events_reach_the_text_input_as_they_are() {
  use zwp_text_input_v1::Event;
  let (mut compositor, path) = test_compositor();
  let mut app = App::new(&mut compositor, &path);
  let mut method = MethodV1::new(&mut compositor, &path);
  app.activate(&mut compositor);
  app.input.commit_state(7);
  app.client.roundtrip(&mut compositor);
  method.client.roundtrip(&mut compositor);
  let context = method.context().expect("the method was activated");
  app.take();

  context.preedit_cursor(1);
  context.preedit_styling(0, 2, 5);
  context.preedit_string(7, "ab".into(), "ab".into());
  context.delete_surrounding_text(-1, 2);
  context.cursor_position(-1, 0);
  context.commit_string(7, "ok".into());
  context.modifiers_map(b"Shift\0".to_vec());
  context.keysym(7, 100, 0xff0d, 1, 0);
  context.language(7, "en".into());
  context.text_direction(7, 2);
  method.client.roundtrip(&mut compositor);
  app.client.roundtrip(&mut compositor);
  let events = app.take();
  assert!(
    matches!(
      &events[..],
      [
        Event::PreeditCursor { index: 1 },
        Event::PreeditStyling { index: 0, length: 2, style: _ },
        Event::PreeditString { serial: 7, text, commit },
        Event::DeleteSurroundingText { index: -1, length: 2 },
        Event::CursorPosition { index: -1, anchor: 0 },
        Event::CommitString { serial: 7, text: committed },
        Event::ModifiersMap { map },
        Event::Keysym {
          serial: 7,
          time: 100,
          sym: 0xff0d,
          state: 1,
          modifiers: 0,
        },
        Event::Language { serial: 7, language },
        Event::TextDirection { serial: 7, direction: _ },
      ] if text == "ab" && commit == "ab" && committed == "ok"
        && map == b"Shift\0" && language == "en"
    ),
    "{events:?}"
  );
  let style = events.iter().find_map(|event| match event {
    Event::PreeditStyling { style, .. } => Some(*style),
    _ => None,
  });
  assert_eq!(
    style,
    Some(wayland_client::WEnum::Value(
      zwp_text_input_v1::PreeditStyle::Underline
    ))
  );
}

#[test]
fn deactivate_and_focus_loss_leave_and_end_the_context() {
  let (mut compositor, path) = test_compositor();
  let mut app = App::new(&mut compositor, &path);
  let mut method = MethodV1::new(&mut compositor, &path);
  app.activate(&mut compositor);
  method.client.roundtrip(&mut compositor);
  assert!(method.context().is_some());
  app.take();

  app.input.deactivate(&app.seat);
  app.client.roundtrip(&mut compositor);
  assert!(matches!(&app.take()[..], [zwp_text_input_v1::Event::Leave]));
  method.client.roundtrip(&mut compositor);
  assert!(matches!(
    method.client.events.input_method_v1.last(),
    Some(zwp_input_method_v1::Event::Deactivate { .. })
  ));
  // Requests of a deactivated text input reach nobody.
  method.take();
  app.input.set_surrounding_text("x".into(), 0, 0);
  app.input.reset();
  app.client.roundtrip(&mut compositor);
  method.client.roundtrip(&mut compositor);
  assert!(method.take().is_empty());

  app.activate(&mut compositor);
  method.client.roundtrip(&mut compositor);
  assert!(method.context().is_some());
  app.take();
  // A window of another client takes the keyboard focus.
  let mut other = TestClient::connect(&mut compositor, &path);
  let window = other.toplevel(&mut compositor, "other");
  other.attach_buffer(&mut compositor, &window.surface, 100, 100);
  app.client.roundtrip(&mut compositor);
  assert!(matches!(&app.take()[..], [zwp_text_input_v1::Event::Leave]));
  method.client.roundtrip(&mut compositor);
  assert!(matches!(
    method.client.events.input_method_v1.last(),
    Some(zwp_input_method_v1::Event::Deactivate { .. })
  ));
}

#[test]
fn a_second_text_input_takes_over_the_seat() {
  let (mut compositor, path) = test_compositor();
  let mut app = App::new(&mut compositor, &path);
  let manager = app.client.bind::<ZwpTextInputManagerV1>(1);
  let second = manager.create_text_input(&app.client.handle, ());
  app.activate(&mut compositor);
  app.take();
  second.activate(&app.seat, &app.window.surface);
  app.client.roundtrip(&mut compositor);
  assert!(matches!(&app.take()[..], [zwp_text_input_v1::Event::Leave]));
  let events = take_for(&mut app.client.events.text_input_v1, &second.id());
  assert!(matches!(
    &events[..],
    [zwp_text_input_v1::Event::Enter { .. }]
  ));
  assert_eq!(
    compositor.state.text_inputs.active(),
    Some(server_id(&compositor, &second))
  );
}

/// The compositor's object for the client's text input `input`.
fn server_id(
  compositor: &Compositor,
  input: &ZwpTextInputV1,
) -> wayland_server::backend::ObjectId {
  let protocol_id = input.id().protocol_id();
  compositor
    .state
    .text_inputs
    .resources
    .keys()
    .find(|id| id.protocol_id() == protocol_id)
    .cloned()
    .expect("the compositor has the text input")
}

#[test]
fn input_method_v2_edits_arrive_in_v1_terms() {
  use zwp_text_input_v1::Event;
  let (mut compositor, path) = test_compositor();
  let mut app = App::new(&mut compositor, &path);
  let mut im = TestClient::connect(&mut compositor, &path);
  let seat = im.bind::<WlSeat>(7);
  let manager = im.bind::<ZwpInputMethodManagerV2>(1);
  let method: ZwpInputMethodV2 =
    manager.get_input_method(&seat, &im.handle, ());
  im.roundtrip(&mut compositor);
  app.activate(&mut compositor);
  app.input.set_surrounding_text("abc".into(), 3, 3);
  app.input.commit_state(5);
  app.client.roundtrip(&mut compositor);
  im.roundtrip(&mut compositor);
  let events = std::mem::take(&mut im.events.input_method_v2);
  assert!(matches!(events[0], zwp_input_method_v2::Event::Activate));
  // One `done` for the activation, one for the state.
  let dones = events
    .iter()
    .filter(|event| matches!(event, zwp_input_method_v2::Event::Done))
    .count() as u32;
  assert_eq!(dones, 2);
  assert!(
    matches!(
      &events[events.len() - 4..],
      [
        zwp_input_method_v2::Event::SurroundingText {
          text,
          cursor: 3,
          anchor: 3,
        },
        zwp_input_method_v2::Event::TextChangeCause { .. },
        zwp_input_method_v2::Event::ContentType { .. },
        zwp_input_method_v2::Event::Done,
      ] if text == "abc"
    ),
    "{events:?}"
  );
  app.take();

  method.delete_surrounding_text(2, 1);
  method.commit_string("xy".into());
  method.set_preedit_string("pre".into(), 1, 1);
  method.commit(dones);
  im.roundtrip(&mut compositor);
  app.client.roundtrip(&mut compositor);
  let events = app.take();
  assert!(
    matches!(
      &events[..],
      [
        Event::DeleteSurroundingText { index: -2, length: 3 },
        Event::CommitString { serial: 5, text },
        Event::PreeditCursor { index: 1 },
        Event::PreeditString { serial: 5, text: preedit, commit },
      ] if text == "xy" && preedit == "pre" && commit.is_empty()
    ),
    "{events:?}"
  );

  // A commit string replaces the preedit in v1 by itself.
  method.commit_string("z".into());
  method.commit(dones);
  im.roundtrip(&mut compositor);
  app.client.roundtrip(&mut compositor);
  let events = app.take();
  assert!(
    matches!(
      &events[..],
      [Event::CommitString { serial: 5, text }] if text == "z"
    ),
    "{events:?}"
  );
  // A commit with neither clears the preedit shown.
  method.set_preedit_string("q".into(), 0, 0);
  method.commit(dones);
  method.commit(dones);
  im.roundtrip(&mut compositor);
  app.client.roundtrip(&mut compositor);
  let events = app.take();
  assert!(
    matches!(
      &events[..],
      [
        Event::PreeditCursor { index: 0 },
        Event::PreeditString { serial: 5, text: shown, .. },
        Event::PreeditString { serial: 5, text: cleared, .. },
      ] if shown == "q" && cleared.is_empty()
    ),
    "{events:?}"
  );
  // And a deletion alone comes with an empty commit string.
  method.delete_surrounding_text(1, 0);
  method.commit(dones);
  im.roundtrip(&mut compositor);
  app.client.roundtrip(&mut compositor);
  let events = app.take();
  assert!(
    matches!(
      &events[..],
      [
        Event::DeleteSurroundingText { index: -1, length: 1 },
        Event::CommitString { serial: 5, text },
      ] if text.is_empty()
    ),
    "{events:?}"
  );
}
