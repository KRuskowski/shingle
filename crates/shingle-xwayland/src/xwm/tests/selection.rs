//! The selection bridge against the recording connection, with real pipes
//! standing in for the Wayland side.

use std::{
  io::{Read, Write},
  os::fd::OwnedFd,
};

use x11rb::protocol::{
  Event,
  xfixes::{SelectionEvent, SelectionNotifyEvent as OwnerNotify},
  xproto::{PropertyNotifyEvent, SelectionNotifyEvent, SelectionRequestEvent},
};

use super::*;
use crate::xwm::connection::{INTEGER_TYPE, STRING_ATOM};

/// An X client's window.
const XCLIP: Window = 0x800001;
/// Another X client's window.
const OTHER: Window = 0x900001;

/// The bridge's clipboard window, the fake's second ID after the check
/// window.
const CLIPBOARD_WINDOW: Window = 0x200002;
/// The bridge's primary selection window.
const PRIMARY_WINDOW: Window = 0x200003;

/// A window manager bridging both selections, requests cleared.
fn bridged() -> Xwm<Recording> {
  let mut xwm = started();
  xwm
    .bridge_selections(&[Selection::Clipboard, Selection::Primary])
    .unwrap();
  assert_eq!(clipboard_window(&xwm), CLIPBOARD_WINDOW);
  assert_eq!(
    xwm.selection_window(Selection::Primary),
    Some(PRIMARY_WINDOW)
  );
  xwm.connection.requests.clear();
  xwm
}

fn clipboard_window(xwm: &Xwm<Recording>) -> Window {
  xwm.selection_window(Selection::Clipboard).unwrap()
}

/// XFixes: `owner` now owns `selection`.
fn owner(selection: Selection, owner: Window) -> Event {
  let (window, atom_name) = match selection {
    Selection::Clipboard => (CLIPBOARD_WINDOW, AtomName::CLIPBOARD),
    Selection::Primary => (PRIMARY_WINDOW, AtomName::PRIMARY),
  };
  Event::XfixesSelectionNotify(OwnerNotify {
    subtype: SelectionEvent::SET_SELECTION_OWNER,
    window,
    owner,
    selection: atom(atom_name),
    timestamp: 77,
    selection_timestamp: 66,
    ..Default::default()
  })
}

/// `SelectionNotify` to `requestor` for `target` in `_WL_SELECTION`, or a
/// refusal with `property` 0.
fn converted(requestor: Window, target: u32, property: u32) -> Event {
  Event::SelectionNotify(SelectionNotifyEvent {
    requestor,
    selection: atom(AtomName::CLIPBOARD),
    target,
    property,
    ..Default::default()
  })
}

/// A `SelectionRequest` from `XCLIP` for the clipboard as `target`.
fn request(target: u32, property: u32) -> Event {
  Event::SelectionRequest(SelectionRequestEvent {
    owner: CLIPBOARD_WINDOW,
    requestor: XCLIP,
    selection: atom(AtomName::CLIPBOARD),
    target,
    property,
    time: 9,
    ..Default::default()
  })
}

/// A property of 32-bit `values` of type `kind`.
fn property32(kind: u32, values: &[u32]) -> Property {
  Property {
    kind,
    format: 32,
    value: values
      .iter()
      .flat_map(|value| value.to_ne_bytes())
      .collect(),
  }
}

/// A property of `bytes` of type `kind`.
fn property8(kind: u32, bytes: &[u8]) -> Property {
  Property {
    kind,
    format: 8,
    value: bytes.to_vec(),
  }
}

/// `PropertyNotify` with `state` for `atom` on `window`.
fn property_event(window: Window, atom: u32, deleted: bool) -> Event {
  use x11rb::protocol::xproto::Property as State;
  Event::PropertyNotify(PropertyNotifyEvent {
    window,
    atom,
    state: if deleted {
      State::DELETE
    } else {
      State::NEW_VALUE
    },
    ..Default::default()
  })
}

/// A pipe as files: (read end, write end).
fn pipe() -> (std::fs::File, std::fs::File) {
  let (read, write) = rustix::pipe::pipe().unwrap();
  (read.into(), write.into())
}

/// Lets an X client own the clipboard with `targets` and returns the
/// MIME types reported.
fn x_owns(xwm: &mut Xwm<Recording>, targets: &[&str]) -> Vec<String> {
  let events = feed(xwm, vec![owner(Selection::Clipboard, XCLIP)]);
  assert!(events.is_empty());
  let window = clipboard_window(xwm);
  let values = targets
    .iter()
    .map(|name| xwm.connection.intern(name))
    .collect::<Vec<_>>();
  xwm.connection.properties.insert(
    (window, atom(AtomName::WL_SELECTION)),
    property32(ATOM_TYPE, &values),
  );
  let events = feed(
    xwm,
    vec![converted(
      window,
      atom(AtomName::TARGETS),
      atom(AtomName::WL_SELECTION),
    )],
  );
  let [
    XwmEvent::SelectionOffered {
      selection: Selection::Clipboard,
      mime_types,
    },
  ] = &events[..]
  else {
    panic!("no offer: {events:?}");
  };
  mime_types.clone()
}

#[test]
fn bridging_creates_a_window_per_selection_and_selects_owner_events() {
  let mut xwm = started();
  xwm.bridge_selections(&[Selection::Clipboard]).unwrap();
  let window = clipboard_window(&xwm);
  assert_eq!(
    xwm.connection.requests,
    [
      Request::CreateWindow(window, ROOT),
      Request::Watch(window, EventMask::PROPERTY_CHANGE),
      Request::SelectSelection(window, atom(AtomName::CLIPBOARD)),
    ]
  );
  assert_eq!(xwm.bridged_selections(), [Selection::Clipboard]);
  assert_eq!(xwm.selection_window(Selection::Primary), None);
  // Bridging again adds only what is missing.
  xwm
    .bridge_selections(&[Selection::Clipboard, Selection::Primary])
    .unwrap();
  assert_eq!(
    xwm.bridged_selections(),
    [Selection::Clipboard, Selection::Primary]
  );
  // The bridge's own windows are not tracked as X windows.
  let events = feed(&mut xwm, vec![create(window, Geometry::default(), false)]);
  assert!(events.is_empty());
  assert!(xwm.window(window).is_none());
}

#[test]
fn a_wayland_selection_takes_the_x_selection_and_answers_targets() {
  let mut xwm = bridged();
  let window = clipboard_window(&xwm);
  let offer = vec![
    mime::PLAIN_TEXT.to_owned(),
    mime::UTF8_TEXT.to_owned(),
    "image/png".to_owned(),
  ];
  xwm
    .set_selection(Selection::Clipboard, Some(offer))
    .unwrap();
  assert_eq!(
    xwm.connection.requests.last(),
    Some(&Request::SelectionOwner(window, atom(AtomName::CLIPBOARD)))
  );
  // Our own ownership only records the time.
  assert!(feed(&mut xwm, vec![owner(Selection::Clipboard, window)]).is_empty());
  xwm.connection.requests.clear();
  let property = 0x4000;
  feed(&mut xwm, vec![request(atom(AtomName::TARGETS), property)]);
  let png = xwm.connection.intern("image/png");
  assert_eq!(
    xwm.connection.requests,
    [
      Request::Property32(
        XCLIP,
        property,
        ATOM_TYPE,
        vec![
          atom(AtomName::TIMESTAMP),
          atom(AtomName::TARGETS),
          atom(AtomName::TEXT),
          STRING_ATOM,
          atom(AtomName::UTF8_STRING),
          png,
        ],
      ),
      Request::SelectionNotify(
        XCLIP,
        atom(AtomName::CLIPBOARD),
        atom(AtomName::TARGETS),
        property,
      ),
    ]
  );
  xwm.connection.requests.clear();
  // TIMESTAMP is the server time of the ownership, into the target
  // itself for a client that gives no property.
  feed(&mut xwm, vec![request(atom(AtomName::TIMESTAMP), 0)]);
  assert_eq!(
    xwm.connection.requests[0],
    Request::Property32(
      XCLIP,
      atom(AtomName::TIMESTAMP),
      INTEGER_TYPE,
      vec![66]
    )
  );
}

#[test]
fn requests_without_a_wayland_selection_or_type_are_refused() {
  let mut xwm = bridged();
  feed(&mut xwm, vec![request(atom(AtomName::TARGETS), 0x4000)]);
  let refusal = |target| {
    Request::SelectionNotify(XCLIP, atom(AtomName::CLIPBOARD), target, 0)
  };
  assert_eq!(xwm.connection.requests, [refusal(atom(AtomName::TARGETS))]);
  xwm
    .set_selection(Selection::Clipboard, Some(vec!["image/png".into()]))
    .unwrap();
  xwm.connection.requests.clear();
  feed(&mut xwm, vec![request(atom(AtomName::UTF8_STRING), 0x4000)]);
  assert_eq!(
    xwm.connection.requests,
    [refusal(atom(AtomName::UTF8_STRING))]
  );
  assert!(xwm.take_selection_sends().is_empty());
  // A selection the bridge does not own, such as WM_S0, is refused too.
  xwm.connection.requests.clear();
  feed(
    &mut xwm,
    vec![Event::SelectionRequest(SelectionRequestEvent {
      owner: 0x200001,
      requestor: XCLIP,
      selection: atom(AtomName::WM_S0),
      target: atom(AtomName::TARGETS),
      property: 0x4000,
      ..Default::default()
    })],
  );
  assert_eq!(
    xwm.connection.requests,
    [Request::SelectionNotify(
      XCLIP,
      atom(AtomName::WM_S0),
      atom(AtomName::TARGETS),
      0
    )]
  );
}

/// Starts a transfer of the clipboard as UTF-8 text to `XCLIP` into
/// `property` and returns the Wayland source's end of its pipe.
fn start_outgoing(xwm: &mut Xwm<Recording>, property: u32) -> std::fs::File {
  xwm
    .set_selection(Selection::Clipboard, Some(vec![mime::UTF8_TEXT.into()]))
    .unwrap();
  feed(xwm, vec![request(atom(AtomName::UTF8_STRING), property)]);
  let mut sends = xwm.take_selection_sends();
  assert_eq!(sends.len(), 1);
  let send = sends.pop().unwrap();
  assert_eq!(send.selection, Selection::Clipboard);
  assert_eq!(send.mime_type, mime::UTF8_TEXT);
  xwm.connection.requests.clear();
  send.fd.into()
}

#[test]
fn a_small_transfer_to_x_is_one_property() {
  let mut xwm = bridged();
  let property = 0x4000;
  let mut source = start_outgoing(&mut xwm, property);
  assert_eq!(xwm.transfers(), (1, 0));
  // Nothing written yet: nothing sent, and the pipe is watched.
  xwm.pump_transfers().unwrap();
  assert!(xwm.connection.requests.is_empty());
  let watches = xwm.transfer_watches();
  assert_eq!(watches.len(), 1);
  assert!(!watches[0].writable);
  source.write_all(b"0123456789").unwrap();
  xwm.pump_transfers().unwrap();
  assert!(xwm.connection.requests.is_empty(), "waits for the end");
  drop(source);
  xwm.pump_transfers().unwrap();
  assert_eq!(
    xwm.connection.requests,
    [
      Request::Property8(
        XCLIP,
        property,
        atom(AtomName::UTF8_STRING),
        b"0123456789".to_vec()
      ),
      Request::SelectionNotify(
        XCLIP,
        atom(AtomName::CLIPBOARD),
        atom(AtomName::UTF8_STRING),
        property,
      ),
    ]
  );
  assert_eq!(xwm.transfers(), (0, 0));
  assert!(xwm.transfer_watches().is_empty());
}

#[test]
fn a_large_transfer_to_x_uses_incr_chunks() {
  let mut xwm = bridged();
  let property = 0x4000;
  let source = start_outgoing(&mut xwm, property);
  let data = (0..INCR_CHUNK_SIZE * 3 + 100)
    .map(|index| (index % 251) as u8)
    .collect::<Vec<_>>();
  let writer = {
    let data = data.clone();
    let mut source = source;
    std::thread::spawn(move || source.write_all(&data).unwrap())
  };
  let mut received = Vec::new();
  let mut incr = false;
  let mut finished = false;
  let mut masks = Vec::new();
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  while !finished {
    assert!(std::time::Instant::now() < deadline, "the transfer stalled");
    xwm.pump_transfers().unwrap();
    let requests = std::mem::take(&mut xwm.connection.requests);
    let mut deleted = false;
    for request in requests {
      match request {
        Request::Property32(XCLIP, p, kind, value) if p == property => {
          assert_eq!(kind, atom(AtomName::INCR));
          assert!(value[0] as usize > INCR_CHUNK_SIZE);
          incr = true;
        }
        Request::SelectionNotify(XCLIP, _, _, p) => assert_eq!(p, property),
        Request::Watch(XCLIP, mask) => masks.push(mask),
        Request::Property8(XCLIP, p, _, chunk) if p == property => {
          assert!(incr, "data before INCR");
          assert!(chunk.len() <= INCR_CHUNK_SIZE);
          if chunk.is_empty() {
            finished = true;
          } else {
            received.extend(chunk);
            deleted = true;
          }
        }
        other => panic!("unexpected {other:?}"),
      }
    }
    if incr && !finished && (deleted || received.is_empty()) {
      // The requestor reads and deletes each chunk, INCR's first value
      // included.
      feed(&mut xwm, vec![property_event(XCLIP, property, true)]);
    }
  }
  writer.join().unwrap();
  assert_eq!(received.len(), data.len());
  assert!(received == data);
  // The requestor's event mask is set for INCR and reset at the end,
  // since the window manager does not watch it otherwise.
  xwm.pump_transfers().unwrap();
  masks.extend(xwm.connection.requests.iter().filter_map(|request| {
    let Request::Watch(XCLIP, mask) = request else {
      return None;
    };
    Some(*mask)
  }));
  assert_eq!(masks, [EventMask::PROPERTY_CHANGE, EventMask::NO_EVENT]);
  assert_eq!(xwm.transfers(), (0, 0));
}

#[test]
fn a_slow_wayland_writer_stalls_only_its_own_transfer() {
  let mut xwm = bridged();
  let _slow = start_outgoing(&mut xwm, 0x4000);
  // A second requestor's transfer completes while the first source has
  // written nothing.
  feed(
    &mut xwm,
    vec![Event::SelectionRequest(SelectionRequestEvent {
      owner: CLIPBOARD_WINDOW,
      requestor: OTHER,
      selection: atom(AtomName::CLIPBOARD),
      target: atom(AtomName::UTF8_STRING),
      property: 0x5000,
      ..Default::default()
    })],
  );
  let send = xwm.take_selection_sends().pop().unwrap();
  let mut fast: std::fs::File = send.fd.into();
  fast.write_all(b"fast").unwrap();
  drop(fast);
  xwm.connection.requests.clear();
  xwm.pump_transfers().unwrap();
  assert!(xwm.connection.requests.contains(&Request::Property8(
    OTHER,
    0x5000,
    atom(AtomName::UTF8_STRING),
    b"fast".to_vec()
  )));
  assert_eq!(xwm.transfers(), (1, 0));
}

#[test]
fn an_x_owner_becomes_an_offer_and_its_end_clears_it() {
  let mut xwm = bridged();
  let window = clipboard_window(&xwm);
  let events = feed(&mut xwm, vec![owner(Selection::Clipboard, XCLIP)]);
  assert!(events.is_empty());
  assert_eq!(
    xwm.connection.requests,
    [Request::Convert(
      window,
      atom(AtomName::CLIPBOARD),
      atom(AtomName::TARGETS),
      atom(AtomName::WL_SELECTION),
    )]
  );
  xwm.connection.requests.clear();
  let mime_types = x_owns(
    &mut xwm,
    &["TARGETS", "UTF8_STRING", "STRING", "TEXT", "text/html"],
  );
  assert_eq!(mime_types, [mime::UTF8_TEXT, mime::PLAIN_TEXT, "text/html"]);
  // Names come from the server only for atoms the bridge does not know.
  let html = xwm.connection.intern("text/html");
  assert!(
    xwm
      .connection
      .requests
      .contains(&Request::AtomNames(vec![html]))
  );
  // Giving up a selection it does not own leaves the X owner alone.
  xwm.connection.requests.clear();
  xwm.set_selection(Selection::Clipboard, None).unwrap();
  assert!(xwm.connection.requests.is_empty());
  // The owner going away clears it.
  let gone = Event::XfixesSelectionNotify(OwnerNotify {
    subtype: SelectionEvent::SELECTION_CLIENT_CLOSE,
    window,
    owner: x11rb::NONE,
    selection: atom(AtomName::CLIPBOARD),
    ..Default::default()
  });
  assert_eq!(
    feed(&mut xwm, vec![gone.clone()]),
    [XwmEvent::SelectionCleared {
      selection: Selection::Clipboard
    }]
  );
  assert!(feed(&mut xwm, vec![gone]).is_empty());
}

#[test]
fn a_newer_wayland_selection_wins_over_a_stale_x_owner() {
  let mut xwm = bridged();
  let window = clipboard_window(&xwm);
  // X takes the clipboard, then a Wayland client does before the X
  // owner's TARGETS reply arrives.
  feed(&mut xwm, vec![owner(Selection::Clipboard, XCLIP)]);
  xwm
    .set_selection(Selection::Clipboard, Some(vec![mime::UTF8_TEXT.into()]))
    .unwrap();
  let targets = xwm.connection.intern("UTF8_STRING");
  xwm.connection.properties.insert(
    (window, atom(AtomName::WL_SELECTION)),
    property32(ATOM_TYPE, &[targets]),
  );
  let events = feed(
    &mut xwm,
    vec![converted(
      window,
      atom(AtomName::TARGETS),
      atom(AtomName::WL_SELECTION),
    )],
  );
  assert!(events.is_empty(), "{events:?}");
  // The property is read anyway, so that it does not linger.
  assert!(
    !xwm
      .connection
      .properties
      .contains_key(&(window, atom(AtomName::WL_SELECTION)))
  );
  // Our ownership arrives, and requests are served.
  feed(&mut xwm, vec![owner(Selection::Clipboard, window)]);
  xwm.connection.requests.clear();
  feed(&mut xwm, vec![request(atom(AtomName::UTF8_STRING), 0x4000)]);
  assert_eq!(xwm.take_selection_sends().len(), 1);
}

#[test]
fn a_new_x_owner_replaces_the_wayland_offer() {
  let mut xwm = bridged();
  let window = clipboard_window(&xwm);
  xwm
    .set_selection(Selection::Clipboard, Some(vec![mime::UTF8_TEXT.into()]))
    .unwrap();
  feed(&mut xwm, vec![owner(Selection::Clipboard, window)]);
  // An X client takes it after us: its offer is reported.
  assert_eq!(x_owns(&mut xwm, &["UTF8_STRING"]), [mime::UTF8_TEXT]);
  // The compositor reports the X11 source as no Wayland selection, which
  // does not disown the X client.
  xwm.connection.requests.clear();
  xwm.set_selection(Selection::Clipboard, None).unwrap();
  assert!(xwm.connection.requests.is_empty());
  // A later Wayland selection takes it back.
  xwm
    .set_selection(Selection::Clipboard, Some(vec![mime::UTF8_TEXT.into()]))
    .unwrap();
  assert_eq!(
    xwm.connection.requests.last(),
    Some(&Request::SelectionOwner(window, atom(AtomName::CLIPBOARD)))
  );
  // And clearing it while we own it disowns.
  feed(&mut xwm, vec![owner(Selection::Clipboard, window)]);
  xwm.connection.requests.clear();
  xwm.set_selection(Selection::Clipboard, None).unwrap();
  assert_eq!(
    xwm.connection.requests,
    [Request::SelectionOwner(0, atom(AtomName::CLIPBOARD))]
  );
}

/// Starts a transfer of the X owner's UTF-8 text to a Wayland client and
/// returns the transfer window and the client's end of the pipe.
fn start_incoming(xwm: &mut Xwm<Recording>) -> (Window, std::fs::File) {
  x_owns(xwm, &["TARGETS", "UTF8_STRING"]);
  xwm.connection.requests.clear();
  let (read, write) = pipe();
  xwm
    .receive_selection(Selection::Clipboard, mime::UTF8_TEXT, write.into())
    .unwrap();
  let Request::CreateWindow(window, ROOT) = xwm.connection.requests[0] else {
    panic!("no transfer window: {:?}", xwm.connection.requests);
  };
  assert_eq!(
    xwm.connection.requests[1..],
    [
      Request::Watch(window, EventMask::PROPERTY_CHANGE),
      Request::Convert(
        window,
        atom(AtomName::CLIPBOARD),
        atom(AtomName::UTF8_STRING),
        atom(AtomName::WL_SELECTION),
      ),
    ]
  );
  xwm.connection.requests.clear();
  (window, read)
}

/// Reads `file` to its end.
fn read_all(mut file: std::fs::File) -> Vec<u8> {
  let mut data = Vec::new();
  file.read_to_end(&mut data).unwrap();
  data
}

#[test]
fn a_small_transfer_from_x_is_copied_and_ends() {
  let mut xwm = bridged();
  let (window, client) = start_incoming(&mut xwm);
  // The bridge's transfer windows are not tracked as X windows either.
  feed(&mut xwm, vec![create(window, Geometry::default(), false)]);
  assert!(xwm.window(window).is_none());
  xwm.connection.properties.insert(
    (window, atom(AtomName::WL_SELECTION)),
    property8(atom(AtomName::UTF8_STRING), b"from xclip"),
  );
  feed(
    &mut xwm,
    vec![converted(
      window,
      atom(AtomName::UTF8_STRING),
      atom(AtomName::WL_SELECTION),
    )],
  );
  assert_eq!(
    xwm.connection.requests,
    [
      Request::TakeProperty(window, atom(AtomName::WL_SELECTION)),
      Request::DestroyWindow(window),
    ]
  );
  assert_eq!(xwm.transfers(), (0, 0));
  assert_eq!(read_all(client), b"from xclip");
}

#[test]
fn a_refused_conversion_closes_the_client_descriptor() {
  let mut xwm = bridged();
  let (window, client) = start_incoming(&mut xwm);
  feed(
    &mut xwm,
    vec![converted(window, atom(AtomName::UTF8_STRING), 0)],
  );
  assert_eq!(xwm.connection.requests, [Request::DestroyWindow(window)]);
  assert!(read_all(client).is_empty());
  // A type the owner does not offer closes it at once.
  let (read, write) = pipe();
  xwm
    .receive_selection(Selection::Clipboard, "image/png", write.into())
    .unwrap();
  assert!(read_all(read).is_empty());
  assert_eq!(xwm.transfers(), (0, 0));
}

#[test]
fn a_large_transfer_from_x_follows_incr_and_waits_for_a_slow_reader() {
  let mut xwm = bridged();
  let (window, client) = start_incoming(&mut xwm);
  let property = atom(AtomName::WL_SELECTION);
  xwm.connection.properties.insert(
    (window, property),
    property32(atom(AtomName::INCR), &[1 << 20]),
  );
  feed(
    &mut xwm,
    vec![converted(window, atom(AtomName::UTF8_STRING), property)],
  );
  // Taking the INCR property deleted it, which asks for the first chunk.
  assert_eq!(
    xwm.connection.requests,
    [Request::TakeProperty(window, property)]
  );
  let chunks = (0..20u8)
    .map(|index| vec![index; INCR_CHUNK_SIZE])
    .collect::<Vec<_>>();
  // The client does not read yet: the pipe fills, and the bridge stops
  // taking chunks until there is room again.
  let mut offered = 0;
  for chunk in &chunks {
    if xwm.connection.properties.contains_key(&(window, property)) {
      break;
    }
    xwm.connection.properties.insert(
      (window, property),
      property8(atom(AtomName::UTF8_STRING), chunk),
    );
    feed(&mut xwm, vec![property_event(window, property, false)]);
    offered += 1;
  }
  assert!(
    offered < chunks.len(),
    "the bridge took every chunk at once"
  );
  assert!(
    xwm.transfer_watches().iter().any(|watch| watch.writable),
    "the client descriptor is watched"
  );
  // The reader catches up; the remaining chunks follow, then the end.
  let reader = std::thread::spawn(move || read_all(client));
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  let mut next = offered;
  loop {
    assert!(std::time::Instant::now() < deadline, "the transfer stalled");
    xwm.pump_transfers().unwrap();
    if xwm.connection.properties.contains_key(&(window, property)) {
      std::thread::sleep(std::time::Duration::from_millis(1));
      continue;
    }
    let chunk = chunks.get(next).map_or(&[][..], Vec::as_slice);
    xwm.connection.properties.insert(
      (window, property),
      property8(atom(AtomName::UTF8_STRING), chunk),
    );
    feed(&mut xwm, vec![property_event(window, property, false)]);
    if next == chunks.len() {
      break;
    }
    next += 1;
  }
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  while xwm.transfers() != (0, 0) {
    assert!(std::time::Instant::now() < deadline, "the transfer stalled");
    xwm.pump_transfers().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1));
  }
  let received = reader.join().unwrap();
  assert_eq!(received.len(), chunks.len() * INCR_CHUNK_SIZE);
  assert!(received == chunks.concat());
  assert!(
    xwm
      .connection
      .requests
      .contains(&Request::DestroyWindow(window))
  );
}

#[test]
fn a_reader_that_goes_away_ends_the_transfer() {
  let mut xwm = bridged();
  let (window, client) = start_incoming(&mut xwm);
  drop(client);
  xwm.connection.properties.insert(
    (window, atom(AtomName::WL_SELECTION)),
    property8(atom(AtomName::UTF8_STRING), b"nobody reads this"),
  );
  feed(
    &mut xwm,
    vec![converted(
      window,
      atom(AtomName::UTF8_STRING),
      atom(AtomName::WL_SELECTION),
    )],
  );
  assert_eq!(xwm.transfers(), (0, 0));
}

#[test]
fn a_requestor_that_goes_away_ends_its_transfer() {
  let mut xwm = bridged();
  let _source: OwnedFd = start_outgoing(&mut xwm, 0x4000).into();
  assert_eq!(xwm.transfers(), (1, 0));
  let error = x11rb::x11_utils::X11Error {
    error_kind: x11rb::protocol::ErrorKind::Window,
    error_code: 3,
    sequence: 0,
    bad_value: XCLIP,
    minor_opcode: 0,
    major_opcode: 18,
    extension_name: None,
    request_name: Some("ChangeProperty"),
  };
  let events = feed(&mut xwm, vec![Event::Error(error)]);
  // Quiet, as for any window the window manager does not track.
  assert!(events.is_empty());
  assert_eq!(xwm.transfers(), (0, 0));
}
