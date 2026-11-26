//! The Xdnd state machine against the recording connection: a Wayland
//! drag over X windows, and an X drag over Wayland surfaces through the
//! proxy.

use x11rb::protocol::{
  Event,
  xfixes::{SelectionEvent, SelectionNotifyEvent as OwnerNotify},
  xproto::{ClientMessageEvent, SelectionRequestEvent},
};

use super::*;

/// The `XdndSelection` window, the fake's second ID after the check
/// window.
const DND_WINDOW: Window = 0x200002;
/// The proxy.
const PROXY: Window = 0x200003;
/// An X drop target.
const TARGET: Window = 0x400001;
/// Another X drop target.
const OTHER: Window = 0x500001;
/// An X drag source's window.
const SOURCE: Window = 0x600001;

/// A window manager with drag and drop bridged, requests cleared.
fn bridged() -> Xwm<Recording> {
  let mut xwm = started();
  xwm.bridge_dnd().unwrap();
  assert_eq!(xwm.dnd_windows(), (Some(DND_WINDOW), Some(PROXY)));
  xwm.connection.requests.clear();
  xwm
}

/// Makes `window` `XdndAware` with `version` on the fake server.
fn aware(xwm: &mut Xwm<Recording>, window: Window, version: u32) {
  xwm.connection.properties.insert(
    (window, atom(AtomName::XDND_AWARE)),
    Property {
      kind: ATOM_TYPE,
      format: 32,
      value: version.to_ne_bytes().to_vec(),
    },
  );
}

/// An Xdnd message of type `name` to `window`.
fn message(window: Window, name: AtomName, data: [u32; 5]) -> Event {
  Event::ClientMessage(ClientMessageEvent::new(32, window, atom(name), data))
}

/// A request for an Xdnd message of type `name` to `window`.
fn sent(window: Window, name: AtomName, data: [u32; 5]) -> Request {
  Request::ClientMessage(window, atom(name), data)
}

/// The client messages among the requests, which are then cleared.
fn messages(xwm: &mut Xwm<Recording>) -> Vec<Request> {
  std::mem::take(&mut xwm.connection.requests)
    .into_iter()
    .filter(|request| matches!(request, Request::ClientMessage(..)))
    .collect()
}

const UTF8: &str = mime::UTF8_TEXT;

/// Starts a Wayland drag of UTF-8 and plain text, copy or move.
fn wayland_drag(xwm: &mut Xwm<Recording>) {
  xwm
    .drag_start(
      vec![UTF8.into(), mime::PLAIN_TEXT.into()],
      DndActions::COPY | DndActions::MOVE,
    )
    .unwrap();
}

#[test]
fn bridging_makes_an_aware_selection_window_and_a_proxy() {
  let mut xwm = started();
  xwm.bridge_dnd().unwrap();
  let aware = |window| {
    Request::Property32(
      window,
      atom(AtomName::XDND_AWARE),
      ATOM_TYPE,
      vec![XDND_VERSION],
    )
  };
  assert_eq!(
    xwm.connection.requests,
    [
      Request::CreateWindow(DND_WINDOW, ROOT),
      Request::Watch(DND_WINDOW, EventMask::PROPERTY_CHANGE),
      Request::SelectSelection(DND_WINDOW, atom(AtomName::XDND_SELECTION)),
      aware(DND_WINDOW),
      Request::CreateInputWindow(PROXY, ROOT),
      aware(PROXY),
    ]
  );
  // XdndSelection is not a Wayland selection, and the proxy is not an X
  // window to manage.
  assert!(xwm.bridged_selections().is_empty());
  let events = feed(&mut xwm, vec![create(PROXY, Geometry::default(), false)]);
  assert!(events.is_empty());
  assert!(xwm.window(PROXY).is_none());
  // Bridging twice does nothing.
  xwm.connection.requests.clear();
  xwm.bridge_dnd().unwrap();
  assert!(xwm.connection.requests.is_empty());
}

#[test]
fn a_wayland_drag_enters_positions_drops_and_finishes() {
  let mut xwm = bridged();
  wayland_drag(&mut xwm);
  assert!(xwm.drag_active());
  assert_eq!(
    xwm.connection.requests.last(),
    Some(&Request::SelectionOwner(
      DND_WINDOW,
      atom(AtomName::XDND_SELECTION)
    ))
  );
  xwm.connection.requests.clear();
  aware(&mut xwm, TARGET, 5);
  let events = xwm.drag_motion(Some(TARGET), 300, 20, 7).unwrap();
  assert!(events.is_empty());
  let copy = atom(AtomName::XDND_ACTION_COPY);
  let utf8 = atom(AtomName::UTF8_STRING);
  let text = atom(AtomName::TEXT);
  assert_eq!(
    messages(&mut xwm),
    [
      sent(
        TARGET,
        AtomName::XDND_ENTER,
        [DND_WINDOW, 5 << 24, utf8, text, 0]
      ),
      sent(
        TARGET,
        AtomName::XDND_POSITION,
        [DND_WINDOW, 0, (300 << 16) | 20, 7, copy]
      ),
    ]
  );
  // One position at a time: the next waits for the status.
  xwm.drag_motion(Some(TARGET), 310, 25, 8).unwrap();
  xwm.drag_motion(Some(TARGET), 320, 30, 9).unwrap();
  assert!(messages(&mut xwm).is_empty());
  let events = feed(
    &mut xwm,
    vec![message(
      DND_WINDOW,
      AtomName::XDND_STATUS,
      [TARGET, 3, 0, 0, copy],
    )],
  );
  assert_eq!(
    events,
    [XwmEvent::DragStatus {
      accepted: true,
      action: DndAction::Copy,
    }]
  );
  assert_eq!(
    messages(&mut xwm),
    [sent(
      TARGET,
      AtomName::XDND_POSITION,
      [DND_WINDOW, 0, (320 << 16) | 30, 9, copy]
    )]
  );
  // The same verdict again is not reported.
  let events = feed(
    &mut xwm,
    vec![message(
      DND_WINDOW,
      AtomName::XDND_STATUS,
      [TARGET, 3, 0, 0, copy],
    )],
  );
  assert!(events.is_empty());
  assert!(xwm.drag_drop(11).unwrap());
  assert_eq!(
    messages(&mut xwm),
    [sent(TARGET, AtomName::XDND_DROP, [DND_WINDOW, 0, 11, 0, 0])]
  );
  // Motion after the drop sends nothing.
  xwm.drag_motion(Some(OTHER), 0, 0, 12).unwrap();
  assert!(xwm.connection.requests.is_empty());
  // The target converts XdndSelection, which goes to the drag source.
  feed(
    &mut xwm,
    vec![Event::SelectionRequest(SelectionRequestEvent {
      owner: DND_WINDOW,
      requestor: TARGET,
      selection: atom(AtomName::XDND_SELECTION),
      target: utf8,
      property: 0x4000,
      time: 11,
      ..Default::default()
    })],
  );
  let sends = xwm.take_selection_sends();
  assert_eq!(sends.len(), 1);
  assert_eq!(sends[0].selection, Selection::Dnd);
  assert_eq!(sends[0].mime_type, UTF8);
  // XdndFinished from another window is ignored; the target's ends it.
  let finished = |window| {
    message(DND_WINDOW, AtomName::XDND_FINISHED, [window, 1, copy, 0, 0])
  };
  assert!(feed(&mut xwm, vec![finished(OTHER)]).is_empty());
  let events = feed(&mut xwm, vec![finished(TARGET)]);
  assert_eq!(
    events,
    [XwmEvent::DragFinished {
      performed: true,
      action: DndAction::Copy,
    }]
  );
  xwm.connection.requests.clear();
  xwm.drag_end().unwrap();
  assert!(!xwm.drag_active());
  assert_eq!(
    xwm.connection.requests,
    [Request::SelectionOwner(0, atom(AtomName::XDND_SELECTION))]
  );
}

#[test]
fn leaving_or_refusing_sends_xdnd_leave() {
  let mut xwm = bridged();
  wayland_drag(&mut xwm);
  aware(&mut xwm, TARGET, 5);
  xwm.drag_motion(Some(TARGET), 1, 2, 3).unwrap();
  let copy = atom(AtomName::XDND_ACTION_COPY);
  feed(
    &mut xwm,
    vec![message(
      DND_WINDOW,
      AtomName::XDND_STATUS,
      [TARGET, 3, 0, 0, copy],
    )],
  );
  xwm.connection.requests.clear();
  // Leaving a window that accepted reports that nothing does.
  let events = xwm.drag_motion(None, 5, 5, 4).unwrap();
  assert_eq!(
    events,
    [XwmEvent::DragStatus {
      accepted: false,
      action: DndAction::None,
    }]
  );
  assert_eq!(
    messages(&mut xwm),
    [sent(TARGET, AtomName::XDND_LEAVE, [DND_WINDOW, 0, 0, 0, 0])]
  );
  // A window that refuses gets XdndLeave on the drop.
  xwm.drag_motion(Some(TARGET), 1, 2, 5).unwrap();
  feed(
    &mut xwm,
    vec![message(
      DND_WINDOW,
      AtomName::XDND_STATUS,
      [TARGET, 2, 0, 0, 0],
    )],
  );
  xwm.connection.requests.clear();
  assert!(!xwm.drag_drop(6).unwrap());
  assert_eq!(
    messages(&mut xwm),
    [sent(TARGET, AtomName::XDND_LEAVE, [DND_WINDOW, 0, 0, 0, 0])]
  );
  // A status from a window the drag is not over is ignored.
  let events = feed(
    &mut xwm,
    vec![message(
      DND_WINDOW,
      AtomName::XDND_STATUS,
      [OTHER, 3, 0, 0, copy],
    )],
  );
  assert!(events.is_empty());
}

#[test]
fn windows_that_are_not_aware_or_too_old_get_nothing() {
  let mut xwm = bridged();
  wayland_drag(&mut xwm);
  xwm.connection.requests.clear();
  xwm.drag_motion(Some(TARGET), 1, 2, 3).unwrap();
  xwm.drag_motion(Some(TARGET), 2, 2, 4).unwrap();
  assert!(messages(&mut xwm).is_empty());
  aware(&mut xwm, OTHER, 2);
  xwm.drag_motion(Some(OTHER), 1, 2, 5).unwrap();
  assert!(messages(&mut xwm).is_empty());
  assert!(!xwm.drag_drop(6).unwrap());
  assert!(messages(&mut xwm).is_empty());
  // An older version that is new enough is spoken.
  xwm.drag_end().unwrap();
  wayland_drag(&mut xwm);
  aware(&mut xwm, TARGET, 4);
  xwm.connection.requests.clear();
  xwm.drag_motion(Some(TARGET), 1, 2, 3).unwrap();
  let enter = &messages(&mut xwm)[0];
  let Request::ClientMessage(TARGET, kind, data) = enter else {
    panic!("no XdndEnter: {enter:?}");
  };
  assert_eq!(*kind, atom(AtomName::XDND_ENTER));
  assert_eq!(data[1] >> 24, 4);
}

#[test]
fn a_proxy_that_names_itself_gets_the_messages() {
  let mut xwm = bridged();
  wayland_drag(&mut xwm);
  let window_property = |value: Window| Property {
    kind: connection::WINDOW_TYPE,
    format: 32,
    value: value.to_ne_bytes().to_vec(),
  };
  let proxy_atom = atom(AtomName::XDND_PROXY);
  let properties = &mut xwm.connection.properties;
  properties.insert((TARGET, proxy_atom), window_property(OTHER));
  properties.insert((OTHER, proxy_atom), window_property(OTHER));
  aware(&mut xwm, OTHER, 5);
  xwm.connection.requests.clear();
  xwm.drag_motion(Some(TARGET), 1, 2, 3).unwrap();
  let messages = messages(&mut xwm);
  assert_eq!(messages.len(), 2);
  // Sent to the proxy, about the window.
  assert!(
    messages
      .iter()
      .all(|request| matches!(request, Request::ClientMessage(OTHER, ..)))
  );
  // A proxy that does not name itself is not used.
  let properties = &mut xwm.connection.properties;
  properties.insert((OTHER, proxy_atom), window_property(SOURCE));
  xwm.drag_end().unwrap();
  wayland_drag(&mut xwm);
  xwm.connection.requests.clear();
  xwm.drag_motion(Some(TARGET), 1, 2, 3).unwrap();
  assert!(self::messages(&mut xwm).is_empty());
}

#[test]
fn more_than_three_types_go_in_the_type_list() {
  let mut xwm = bridged();
  xwm
    .drag_start(
      vec![
        UTF8.into(),
        mime::PLAIN_TEXT.into(),
        "text/uri-list".into(),
        "image/png".into(),
      ],
      DndActions::COPY,
    )
    .unwrap();
  aware(&mut xwm, TARGET, 5);
  xwm.connection.requests.clear();
  xwm.drag_motion(Some(TARGET), 1, 2, 3).unwrap();
  let uris = xwm.connection.intern("text/uri-list");
  let png = xwm.connection.intern("image/png");
  let types =
    vec![atom(AtomName::UTF8_STRING), atom(AtomName::TEXT), uris, png];
  let requests = &xwm.connection.requests;
  assert!(requests.contains(&Request::Property32(
    DND_WINDOW,
    atom(AtomName::XDND_TYPE_LIST),
    ATOM_TYPE,
    types.clone(),
  )));
  assert!(requests.contains(&sent(
    TARGET,
    AtomName::XDND_ENTER,
    [DND_WINDOW, (5 << 24) | 1, types[0], types[1], types[2]],
  )));
}

/// XFixes: `owner` now owns `XdndSelection`.
fn dnd_owner(owner: Window) -> Event {
  Event::XfixesSelectionNotify(OwnerNotify {
    subtype: SelectionEvent::SET_SELECTION_OWNER,
    window: DND_WINDOW,
    owner,
    selection: atom(AtomName::XDND_SELECTION),
    timestamp: 77,
    selection_timestamp: 66,
    ..Default::default()
  })
}

/// An X client's drag over the proxy, entered with UTF-8 text.
fn x11_drag(xwm: &mut Xwm<Recording>) {
  let events = feed(xwm, vec![dnd_owner(SOURCE)]);
  assert_eq!(
    events,
    [XwmEvent::DragOwnerChanged {
      owner: Some(SOURCE),
    }]
  );
  xwm.x11_drag(true, false, DndAction::None).unwrap();
  assert!(xwm.x11_drag_active());
  assert!(xwm.dnd_proxy_mapped());
  assert_eq!(
    xwm.connection.requests,
    [Request::Map(PROXY), Request::StackAbove(PROXY, None)]
  );
  xwm.connection.requests.clear();
  let utf8 = atom(AtomName::UTF8_STRING);
  let events = feed(
    xwm,
    vec![message(
      PROXY,
      AtomName::XDND_ENTER,
      [SOURCE, 5 << 24, utf8, 0, 0],
    )],
  );
  assert_eq!(
    events,
    [XwmEvent::DragOffered {
      mime_types: vec![UTF8.into()],
    }]
  );
}

#[test]
fn an_x_drag_is_answered_through_the_proxy() {
  let mut xwm = bridged();
  // Without an X owner there is no X drag.
  xwm.x11_drag(true, false, DndAction::None).unwrap();
  assert!(!xwm.x11_drag_active());
  assert!(xwm.connection.requests.is_empty());
  x11_drag(&mut xwm);
  let copy = atom(AtomName::XDND_ACTION_COPY);
  let position = |time| {
    message(
      PROXY,
      AtomName::XDND_POSITION,
      [SOURCE, 0, (300 << 16) | 20, time, copy],
    )
  };
  assert!(feed(&mut xwm, vec![position(50)]).is_empty());
  assert_eq!(
    messages(&mut xwm),
    [sent(SOURCE, AtomName::XDND_STATUS, [PROXY, 2, 0, 0, 0])]
  );
  // A new verdict is sent at once, and with every position after.
  xwm.x11_drag(true, true, DndAction::Copy).unwrap();
  let accepted = sent(SOURCE, AtomName::XDND_STATUS, [PROXY, 3, 0, 0, copy]);
  assert_eq!(messages(&mut xwm), std::slice::from_ref(&accepted));
  xwm.x11_drag(true, true, DndAction::Copy).unwrap();
  assert!(messages(&mut xwm).is_empty());
  feed(&mut xwm, vec![position(51)]);
  assert_eq!(messages(&mut xwm), [accepted]);
  // A move proposal is reported.
  let moving = message(
    PROXY,
    AtomName::XDND_POSITION,
    [SOURCE, 0, 0, 52, atom(AtomName::XDND_ACTION_MOVE)],
  );
  assert_eq!(
    feed(&mut xwm, vec![moving]),
    [XwmEvent::DragSourceAction {
      action: DndAction::Move,
    }]
  );
  xwm.connection.requests.clear();
  // The drop: the proxy goes, and the data is converted at the drop's
  // time.
  let events = feed(
    &mut xwm,
    vec![message(PROXY, AtomName::XDND_DROP, [SOURCE, 0, 66, 0, 0])],
  );
  assert_eq!(events, [XwmEvent::DragDropped]);
  xwm.x11_drag(true, true, DndAction::Copy).unwrap();
  assert_eq!(xwm.connection.requests, [Request::Unmap(PROXY)]);
  xwm.connection.requests.clear();
  let (_read, write) = rustix::pipe::pipe().unwrap();
  xwm.receive_selection(Selection::Dnd, UTF8, write).unwrap();
  let transfer = 0x200004;
  assert_eq!(
    xwm.connection.requests,
    [
      Request::CreateWindow(transfer, ROOT),
      Request::Watch(transfer, EventMask::PROPERTY_CHANGE),
      Request::Convert(
        transfer,
        atom(AtomName::XDND_SELECTION),
        atom(AtomName::UTF8_STRING),
        atom(AtomName::WL_SELECTION),
      ),
    ]
  );
  xwm.connection.requests.clear();
  xwm.x11_drag_end(Some(DndAction::Copy)).unwrap();
  assert!(!xwm.x11_drag_active());
  assert_eq!(
    messages(&mut xwm),
    [sent(
      SOURCE,
      AtomName::XDND_FINISHED,
      [PROXY, 1, copy, 0, 0]
    )]
  );
}

#[test]
fn a_refused_x_drop_finishes_unperformed() {
  let mut xwm = bridged();
  x11_drag(&mut xwm);
  feed(
    &mut xwm,
    vec![message(PROXY, AtomName::XDND_DROP, [SOURCE, 0, 66, 0, 0])],
  );
  xwm.connection.requests.clear();
  xwm.x11_drag_end(None).unwrap();
  assert_eq!(
    messages(&mut xwm),
    [sent(SOURCE, AtomName::XDND_FINISHED, [PROXY, 0, 0, 0, 0])]
  );
  // Ending a drag that was never dropped only unmaps the proxy.
  x11_drag(&mut xwm);
  xwm.connection.requests.clear();
  xwm.x11_drag_end(None).unwrap();
  assert_eq!(xwm.connection.requests, [Request::Unmap(PROXY)]);
}

#[test]
fn an_x_drag_follows_the_source_and_its_owner() {
  let mut xwm = bridged();
  x11_drag(&mut xwm);
  // Messages from another source are ignored.
  let stray = message(PROXY, AtomName::XDND_LEAVE, [OTHER, 0, 0, 0, 0]);
  assert!(feed(&mut xwm, vec![stray]).is_empty());
  let leave = message(PROXY, AtomName::XDND_LEAVE, [SOURCE, 0, 0, 0, 0]);
  assert_eq!(feed(&mut xwm, vec![leave]), [XwmEvent::DragLeft]);
  // Over an X window the proxy goes.
  xwm.connection.requests.clear();
  xwm.x11_drag(false, false, DndAction::None).unwrap();
  assert_eq!(xwm.connection.requests, [Request::Unmap(PROXY)]);
  // Entering again with the same types reports nothing new.
  xwm.x11_drag(true, false, DndAction::None).unwrap();
  let utf8 = atom(AtomName::UTF8_STRING);
  let enter =
    message(PROXY, AtomName::XDND_ENTER, [SOURCE, 5 << 24, utf8, 0, 0]);
  assert!(feed(&mut xwm, vec![enter]).is_empty());
  // The owner going away ends the drag and the proxy.
  xwm.connection.requests.clear();
  let events = feed(&mut xwm, vec![dnd_owner(0)]);
  assert_eq!(events, [XwmEvent::DragOwnerChanged { owner: None }]);
  assert!(!xwm.x11_drag_active());
  assert_eq!(xwm.connection.requests, [Request::Unmap(PROXY)]);
  // The window manager's own ownership is not an X drag.
  assert!(feed(&mut xwm, vec![dnd_owner(DND_WINDOW)]).is_empty());
}

#[test]
fn x_sources_list_many_types_and_old_versions_are_refused() {
  let mut xwm = bridged();
  feed(&mut xwm, vec![dnd_owner(SOURCE)]);
  xwm.x11_drag(true, false, DndAction::None).unwrap();
  let png = xwm.connection.intern("image/png");
  let list = [atom(AtomName::UTF8_STRING), atom(AtomName::TARGETS), png];
  xwm.connection.properties.insert(
    (SOURCE, atom(AtomName::XDND_TYPE_LIST)),
    Property {
      kind: ATOM_TYPE,
      format: 32,
      value: list.iter().flat_map(|atom| atom.to_ne_bytes()).collect(),
    },
  );
  let old = message(PROXY, AtomName::XDND_ENTER, [SOURCE, 2 << 24, 0, 0, 0]);
  let events = feed(&mut xwm, vec![old]);
  assert!(matches!(events[..], [XwmEvent::Notice(_)]));
  let enter = message(
    PROXY,
    AtomName::XDND_ENTER,
    [SOURCE, (5 << 24) | 1, 0, 0, 0],
  );
  assert_eq!(
    feed(&mut xwm, vec![enter]),
    [XwmEvent::DragOffered {
      mime_types: vec![UTF8.into(), "image/png".into()],
    }]
  );
}
