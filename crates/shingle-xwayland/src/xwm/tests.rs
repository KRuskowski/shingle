//! Bring-up and window management against a recording connection.

use std::collections::{HashMap, VecDeque};

use x11rb::{
  protocol::{
    Event,
    xproto::{AtomEnum, EventMask, Window},
  },
  x11_utils::X11Error,
};

use super::*;

/// The root window the fake server reports.
const ROOT: Window = 0x100;

/// A request the window manager made.
#[derive(Clone, Debug, PartialEq)]
enum Request {
  InternAtoms(usize),
  HasExtension(&'static str),
  TerminateWithoutClients,
  Redirect(Window),
  SelectEvents(Window, EventMask),
  CreateWindow(Window, Window),
  SelectionOwner(Window, u32),
  Property32(Window, u32, u32, Vec<u32>),
  Property8(Window, u32, u32, Vec<u8>),
  Watch(Window, EventMask),
  GetProperties(Window, usize),
  ClientPid(Window),
  Map(Window),
  Configure(Window, Geometry),
  SyntheticConfigure(Window, Geometry),
  ClientMessage(Window, u32, [u32; 5]),
  KillClient(Window),
  Flush,
}

/// Records requests, interns atom `i` as `1000 + i` and replays queued
/// events.
#[derive(Default)]
struct Recording {
  requests: Vec<Request>,
  next_id: u32,
  events: VecDeque<Result<Event, XwmError>>,
  without_composite: bool,
  without_xfixes: bool,
  old_xfixes: bool,
  refuse_redirect: bool,
  /// Properties the fake server holds, by window and atom.
  properties: HashMap<(Window, u32), Property>,
  /// PIDs XRes reports, by window.
  pids: HashMap<Window, u32>,
}

impl XConnection for Recording {
  fn root(&self) -> Window {
    ROOT
  }

  fn generate_id(&mut self) -> Result<u32, XwmError> {
    self.next_id += 1;
    Ok(0x200000 + self.next_id)
  }

  fn has_extension(&mut self, name: &'static str) -> Result<bool, XwmError> {
    self.requests.push(Request::HasExtension(name));
    Ok(match name {
      "Composite" => !self.without_composite,
      "XFIXES" => !self.without_xfixes,
      _ => true,
    })
  }

  fn terminate_without_clients(&mut self) -> Result<bool, XwmError> {
    if self.old_xfixes {
      return Ok(false);
    }
    self.requests.push(Request::TerminateWithoutClients);
    Ok(true)
  }

  fn intern_atoms(&mut self, names: &[&str]) -> Result<Vec<u32>, XwmError> {
    self.requests.push(Request::InternAtoms(names.len()));
    Ok((0..names.len() as u32).map(|index| 1000 + index).collect())
  }

  fn redirect_subwindows(&mut self, window: Window) -> Result<(), XwmError> {
    self.requests.push(Request::Redirect(window));
    if self.refuse_redirect {
      return Err(XwmError::Request("BadAccess".into()));
    }
    Ok(())
  }

  fn select_events(
    &mut self,
    window: Window,
    mask: EventMask,
  ) -> Result<(), XwmError> {
    self.requests.push(Request::SelectEvents(window, mask));
    Ok(())
  }

  fn create_window(
    &mut self,
    id: Window,
    parent: Window,
    _geometry: (i16, i16, u16, u16),
  ) -> Result<(), XwmError> {
    self.requests.push(Request::CreateWindow(id, parent));
    Ok(())
  }

  fn set_selection_owner(
    &mut self,
    owner: Window,
    selection: u32,
  ) -> Result<(), XwmError> {
    self
      .requests
      .push(Request::SelectionOwner(owner, selection));
    Ok(())
  }

  fn change_property32(
    &mut self,
    window: Window,
    property: u32,
    kind: u32,
    data: &[u32],
  ) -> Result<(), XwmError> {
    self.requests.push(Request::Property32(
      window,
      property,
      kind,
      data.to_vec(),
    ));
    Ok(())
  }

  fn change_property8(
    &mut self,
    window: Window,
    property: u32,
    kind: u32,
    data: &[u8],
  ) -> Result<(), XwmError> {
    self.requests.push(Request::Property8(
      window,
      property,
      kind,
      data.to_vec(),
    ));
    Ok(())
  }

  fn watch_window(
    &mut self,
    window: Window,
    mask: EventMask,
  ) -> Result<(), XwmError> {
    self.requests.push(Request::Watch(window, mask));
    Ok(())
  }

  fn get_properties(
    &mut self,
    window: Window,
    properties: &[u32],
  ) -> Result<Vec<Option<Property>>, XwmError> {
    self
      .requests
      .push(Request::GetProperties(window, properties.len()));
    Ok(
      properties
        .iter()
        .map(|atom| self.properties.get(&(window, *atom)).cloned())
        .collect(),
    )
  }

  fn client_pid(&mut self, window: Window) -> Result<Option<u32>, XwmError> {
    self.requests.push(Request::ClientPid(window));
    Ok(self.pids.get(&window).copied())
  }

  fn map_window(&mut self, window: Window) -> Result<(), XwmError> {
    self.requests.push(Request::Map(window));
    Ok(())
  }

  fn configure_window(
    &mut self,
    window: Window,
    geometry: Geometry,
  ) -> Result<(), XwmError> {
    self.requests.push(Request::Configure(window, geometry));
    Ok(())
  }

  fn send_configure_notify(
    &mut self,
    window: Window,
    geometry: Geometry,
  ) -> Result<(), XwmError> {
    self
      .requests
      .push(Request::SyntheticConfigure(window, geometry));
    Ok(())
  }

  fn send_client_message(
    &mut self,
    window: Window,
    kind: u32,
    data: [u32; 5],
  ) -> Result<(), XwmError> {
    self
      .requests
      .push(Request::ClientMessage(window, kind, data));
    Ok(())
  }

  fn kill_client(&mut self, window: Window) -> Result<(), XwmError> {
    self.requests.push(Request::KillClient(window));
    Ok(())
  }

  fn flush(&mut self) -> Result<(), XwmError> {
    self.requests.push(Request::Flush);
    Ok(())
  }

  fn poll_for_event(&mut self) -> Result<Option<Event>, XwmError> {
    self.events.pop_front().transpose()
  }
}

/// The fake's value for `name`.
fn atom(name: AtomName) -> u32 {
  1000 + name as u32
}

#[test]
fn bring_up_follows_wlroots() {
  let xwm = Xwm::start(Recording::default()).unwrap();
  let check = xwm.check_window();
  assert_eq!(xwm.root(), ROOT);
  assert_eq!(xwm.atoms().get(AtomName::WM_S0), atom(AtomName::WM_S0));
  let supported = SUPPORTED.iter().map(|name| atom(*name)).collect();
  let window_list = |name| Request::Property32(ROOT, atom(name), 33, vec![]);
  assert_eq!(
    xwm.connection().requests,
    [
      Request::InternAtoms(ATOM_NAMES.len()),
      Request::HasExtension("Composite"),
      Request::HasExtension("XFIXES"),
      Request::TerminateWithoutClients,
      Request::Redirect(ROOT),
      Request::SelectEvents(
        ROOT,
        EventMask::SUBSTRUCTURE_REDIRECT
          | EventMask::SUBSTRUCTURE_NOTIFY
          | EventMask::PROPERTY_CHANGE,
      ),
      Request::CreateWindow(check, ROOT),
      Request::Property8(
        check,
        atom(AtomName::NET_WM_NAME),
        atom(AtomName::UTF8_STRING),
        b"Shingle".to_vec(),
      ),
      Request::Property32(
        ROOT,
        atom(AtomName::NET_SUPPORTING_WM_CHECK),
        33,
        vec![check],
      ),
      Request::Property32(
        check,
        atom(AtomName::NET_SUPPORTING_WM_CHECK),
        33,
        vec![check],
      ),
      Request::SelectionOwner(check, atom(AtomName::WM_S0)),
      Request::SelectionOwner(check, atom(AtomName::NET_WM_CM_S0)),
      Request::Property32(ROOT, atom(AtomName::NET_SUPPORTED), 4, supported),
      Request::Property32(ROOT, atom(AtomName::NET_ACTIVE_WINDOW), 33, vec![0]),
      window_list(AtomName::NET_CLIENT_LIST),
      window_list(AtomName::NET_CLIENT_LIST_STACKING),
      Request::Flush,
    ]
  );
}

#[test]
fn supported_lists_wlroots_eleven_atoms() {
  assert_eq!(SUPPORTED.len(), 11);
  assert!(SUPPORTED.contains(&AtomName::NET_WM_STATE_FULLSCREEN));
  assert!(SUPPORTED.contains(&AtomName::NET_CLIENT_LIST_STACKING));
}

#[test]
fn property_types_are_the_predefined_atoms() {
  assert_eq!(WINDOW_TYPE, u32::from(AtomEnum::WINDOW));
  assert_eq!(ATOM_TYPE, u32::from(AtomEnum::ATOM));
}

#[test]
fn missing_composite_fails_bring_up() {
  let connection = Recording {
    without_composite: true,
    ..Recording::default()
  };
  assert_eq!(
    Xwm::start(connection).err(),
    Some(XwmError::MissingExtension("Composite"))
  );
}

#[test]
fn bring_up_needs_xfixes_and_reports_terminate_mode() {
  let xwm = Xwm::start(Recording::default()).unwrap();
  assert!(xwm.terminate_without_clients());
  let old = Recording {
    old_xfixes: true,
    ..Recording::default()
  };
  let xwm = Xwm::start(old).unwrap();
  assert!(!xwm.terminate_without_clients());
  assert!(
    !xwm
      .connection()
      .requests
      .contains(&Request::TerminateWithoutClients)
  );
  let missing = Recording {
    without_xfixes: true,
    ..Recording::default()
  };
  assert_eq!(
    Xwm::start(missing).err(),
    Some(XwmError::MissingExtension("XFIXES"))
  );
}

#[test]
fn a_refused_redirect_fails_bring_up() {
  let connection = Recording {
    refuse_redirect: true,
    ..Recording::default()
  };
  assert!(matches!(
    Xwm::start(connection).err(),
    Some(XwmError::Request(_))
  ));
}

#[test]
fn errors_for_destroyed_windows_are_not_reported() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let error = |bad_value| {
    Event::Error(X11Error {
      error_kind: x11rb::protocol::ErrorKind::Window,
      error_code: 3,
      sequence: 9,
      bad_value,
      minor_opcode: 0,
      major_opcode: 18,
      extension_name: None,
      request_name: Some("ChangeProperty"),
    })
  };
  // Still tracked: reported.
  assert_eq!(feed(&mut xwm, vec![error(XTERM)]).len(), 1);
  // Destroyed meanwhile: expected, and quiet.
  let events = feed(&mut xwm, vec![destroy(XTERM), error(XTERM)]);
  assert_eq!(events, [XwmEvent::Destroyed { window: XTERM }]);
}

#[test]
fn dispatch_drains_events_and_reports_errors() {
  let mut xwm = Xwm::start(Recording::default()).unwrap();
  let error = X11Error {
    error_kind: x11rb::protocol::ErrorKind::Value,
    error_code: 2,
    sequence: 7,
    bad_value: 0x42,
    minor_opcode: 0,
    major_opcode: 12,
    extension_name: None,
    request_name: Some("ConfigureWindow"),
  };
  xwm.connection.events.push_back(Ok(Event::Error(error)));
  xwm
    .connection
    .events
    .push_back(Ok(Event::MapRequest(Default::default())));
  let events = xwm.dispatch().unwrap();
  // A MapRequest for window 0 (None) is ignored.
  assert_eq!(events.len(), 1);
  assert!(matches!(&events[0], XwmEvent::RequestFailed(text)
    if text.contains("ConfigureWindow")));
  assert!(xwm.dispatch().unwrap().is_empty());
  xwm
    .connection
    .events
    .push_back(Err(XwmError::Connection("closed".into())));
  assert!(xwm.dispatch().is_err());
}

/// A started window manager with its bring-up requests cleared.
fn started() -> Xwm<Recording> {
  let mut xwm = Xwm::start(Recording::default()).unwrap();
  xwm.connection.requests.clear();
  xwm
}

/// Feeds `events` and returns what the window manager reports.
fn feed(xwm: &mut Xwm<Recording>, events: Vec<Event>) -> Vec<XwmEvent> {
  xwm.connection.events.extend(events.into_iter().map(Ok));
  xwm.dispatch().unwrap()
}

/// `CreateNotify` for a child of the root.
fn create(
  window: Window,
  geometry: Geometry,
  override_redirect: bool,
) -> Event {
  Event::CreateNotify(x11rb::protocol::xproto::CreateNotifyEvent {
    parent: ROOT,
    window,
    x: geometry.x as i16,
    y: geometry.y as i16,
    width: geometry.width as u16,
    height: geometry.height as u16,
    override_redirect,
    ..Default::default()
  })
}

/// `MapRequest` for `window`.
fn map_request(window: Window) -> Event {
  Event::MapRequest(x11rb::protocol::xproto::MapRequestEvent {
    parent: ROOT,
    window,
    ..Default::default()
  })
}

/// `UnmapNotify` for `window`, as the root sees it.
fn unmap(window: Window) -> Event {
  Event::UnmapNotify(x11rb::protocol::xproto::UnmapNotifyEvent {
    event: ROOT,
    window,
    ..Default::default()
  })
}

/// `DestroyNotify` for `window`.
fn destroy(window: Window) -> Event {
  Event::DestroyNotify(x11rb::protocol::xproto::DestroyNotifyEvent {
    event: ROOT,
    window,
    ..Default::default()
  })
}

/// `PropertyNotify` for `atom` on `window`.
fn property_notify(window: Window, atom: u32) -> Event {
  Event::PropertyNotify(x11rb::protocol::xproto::PropertyNotifyEvent {
    window,
    atom,
    ..Default::default()
  })
}

/// `WL_SURFACE_SERIAL` with `serial` for `window`.
fn serial_message(window: Window, serial: u64) -> Event {
  Event::ClientMessage(x11rb::protocol::xproto::ClientMessageEvent::new(
    32,
    window,
    atom(AtomName::WL_SURFACE_SERIAL),
    [serial as u32, (serial >> 32) as u32, 0, 0, 0],
  ))
}

/// The predefined `WM_CLASS`, `WM_NAME` and `WM_TRANSIENT_FOR` atoms.
const WM_CLASS: u32 = 67;
const WM_NAME: u32 = 39;
const WM_TRANSIENT_FOR: u32 = 68;

const XTERM: Window = 0x400001;
const GEOMETRY: Geometry = Geometry::new(10, 20, 484, 316);

/// Gives the fake server an xterm-like window's properties.
fn xterm_properties(xwm: &mut Xwm<Recording>, window: Window) {
  let properties = &mut xwm.connection.properties;
  properties.insert((window, WM_CLASS), Property::bytes(31, b"xterm\0XTerm\0"));
  properties.insert((window, WM_NAME), Property::bytes(31, b"user@host"));
  properties.insert(
    (window, atom(AtomName::WM_PROTOCOLS)),
    Property::words(4, &[atom(AtomName::WM_DELETE_WINDOW)]),
  );
  properties.insert(
    (window, atom(AtomName::NET_WM_PID)),
    Property::words(6, &[4242]),
  );
}

/// Creates and maps an xterm-like window; returns the reported events.
fn map_xterm(xwm: &mut Xwm<Recording>) -> Vec<XwmEvent> {
  xterm_properties(xwm, XTERM);
  feed(
    xwm,
    vec![create(XTERM, GEOMETRY, false), map_request(XTERM)],
  )
}

#[test]
fn a_map_request_reads_properties_maps_and_lists_the_window() {
  let mut xwm = started();
  xwm.connection.pids.insert(XTERM, 777);
  let events = map_xterm(&mut xwm);
  let list = |name| Request::Property32(ROOT, atom(name), 33, vec![XTERM]);
  assert_eq!(
    xwm.connection.requests,
    [
      Request::Watch(
        XTERM,
        EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE
      ),
      Request::GetProperties(XTERM, 12),
      Request::ClientPid(XTERM),
      Request::Map(XTERM),
      Request::Property32(
        XTERM,
        atom(AtomName::WM_STATE),
        atom(AtomName::WM_STATE),
        vec![1, 0],
      ),
      list(AtomName::NET_CLIENT_LIST),
      list(AtomName::NET_CLIENT_LIST_STACKING),
    ]
  );
  let [XwmEvent::Mapped { window, info }] = &events[..] else {
    panic!("unexpected events {events:?}");
  };
  assert_eq!(*window, XTERM);
  assert_eq!(info.class, "XTerm");
  assert_eq!(info.instance, "xterm");
  assert_eq!(info.title, "user@host");
  assert_eq!(info.geometry, GEOMETRY);
  assert!(info.protocols.delete_window);
  // XRes wins over _NET_WM_PID.
  assert_eq!(info.pid, Some(777));
  assert_eq!(info.kind(), Kind::Managed { dialog: false });
  assert_eq!(xwm.client_list(), [XTERM]);
}

#[test]
fn without_xres_the_pid_comes_from_net_wm_pid() {
  let mut xwm = started();
  let events = map_xterm(&mut xwm);
  assert!(matches!(&events[..], [XwmEvent::Mapped { info, .. }]
    if info.pid == Some(4242)));
}

#[test]
fn net_wm_name_wins_and_property_changes_are_reported() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let net_wm_name = atom(AtomName::NET_WM_NAME);
  xwm.connection.properties.insert(
    (XTERM, net_wm_name),
    Property::bytes(atom(AtomName::UTF8_STRING), "vim – ü".as_bytes()),
  );
  let events = feed(&mut xwm, vec![property_notify(XTERM, net_wm_name)]);
  assert!(matches!(&events[..], [XwmEvent::Changed { window, info }]
    if *window == XTERM && info.title == "vim – ü"));
  // WM_NAME changes no longer show while _NET_WM_NAME is set.
  xwm
    .connection
    .properties
    .insert((XTERM, WM_NAME), Property::bytes(31, b"other"));
  assert!(feed(&mut xwm, vec![property_notify(XTERM, WM_NAME)]).is_empty());
  assert_eq!(xwm.window(XTERM).unwrap().title, "vim – ü");
  // Deleting _NET_WM_NAME falls back to WM_NAME.
  xwm.connection.properties.remove(&(XTERM, net_wm_name));
  let events = feed(&mut xwm, vec![property_notify(XTERM, net_wm_name)]);
  assert!(matches!(&events[..], [XwmEvent::Changed { info, .. }]
    if info.title == "other"));
  // Properties the window manager does not read are not fetched.
  xwm.connection.requests.clear();
  assert!(feed(&mut xwm, vec![property_notify(XTERM, 9999)]).is_empty());
  assert!(xwm.connection.requests.is_empty());
}

#[test]
fn transient_for_only_names_known_windows() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let dialog = 0x400010;
  xwm
    .connection
    .properties
    .insert((dialog, WM_TRANSIENT_FOR), Property::words(33, &[XTERM]));
  let events = feed(
    &mut xwm,
    vec![create(dialog, GEOMETRY, false), map_request(dialog)],
  );
  assert!(matches!(&events[..], [XwmEvent::Mapped { info, .. }]
    if info.transient_for == Some(XTERM)
      && info.kind() == Kind::Managed { dialog: true }));
  let orphan = 0x400020;
  xwm
    .connection
    .properties
    .insert((orphan, WM_TRANSIENT_FOR), Property::words(33, &[0x999999]));
  let events = feed(
    &mut xwm,
    vec![create(orphan, GEOMETRY, false), map_request(orphan)],
  );
  assert!(matches!(&events[..], [XwmEvent::Mapped { info, .. }]
    if info.transient_for.is_none()));
}

#[test]
fn serials_are_reported_once_per_surface() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let serial = (3 << 32) | 5;
  let events = feed(&mut xwm, vec![serial_message(XTERM, serial)]);
  assert_eq!(
    events,
    [XwmEvent::Serial {
      window: XTERM,
      serial
    }]
  );
  // A repeat is ignored.
  assert!(feed(&mut xwm, vec![serial_message(XTERM, 9)]).is_empty());
  // An unmap ends the surface; the remap brings a new serial.
  feed(&mut xwm, vec![unmap(XTERM), map_request(XTERM)]);
  let events = feed(&mut xwm, vec![serial_message(XTERM, 11)]);
  assert_eq!(
    events,
    [XwmEvent::Serial {
      window: XTERM,
      serial: 11
    }]
  );
  // Serials for unknown windows are dropped.
  assert!(feed(&mut xwm, vec![serial_message(0x777, 12)]).is_empty());
}

#[test]
fn unmap_withdraws_the_window_once() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  xwm.connection.requests.clear();
  let events = feed(&mut xwm, vec![unmap(XTERM), unmap(XTERM)]);
  assert_eq!(events, [XwmEvent::Unmapped { window: XTERM }]);
  let list = |name| Request::Property32(ROOT, atom(name), 33, vec![]);
  assert_eq!(
    xwm.connection.requests,
    [
      Request::Property32(
        XTERM,
        atom(AtomName::WM_STATE),
        atom(AtomName::WM_STATE),
        vec![0, 0],
      ),
      list(AtomName::NET_CLIENT_LIST),
      list(AtomName::NET_CLIENT_LIST_STACKING),
    ]
  );
  assert!(xwm.client_list().is_empty());
}

#[test]
fn destroy_forgets_the_window() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let events = feed(&mut xwm, vec![destroy(XTERM), destroy(XTERM)]);
  assert_eq!(events, [XwmEvent::Destroyed { window: XTERM }]);
  assert!(xwm.window(XTERM).is_none());
  assert!(xwm.client_list().is_empty());
  // A window destroyed before it maps is reported too, but never listed.
  let events = feed(
    &mut xwm,
    vec![create(0x500001, GEOMETRY, false), destroy(0x500001)],
  );
  assert_eq!(events, [XwmEvent::Destroyed { window: 0x500001 }]);
}

#[test]
fn client_lists_keep_map_order() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let second = 0x600001;
  let third = 0x700001;
  feed(
    &mut xwm,
    vec![
      create(second, GEOMETRY, false),
      create(third, GEOMETRY, false),
      map_request(third),
      map_request(second),
    ],
  );
  assert_eq!(xwm.client_list(), [XTERM, third, second]);
  feed(&mut xwm, vec![unmap(third)]);
  assert_eq!(xwm.client_list(), [XTERM, second]);
}

#[test]
fn configure_requests_apply_the_value_mask() {
  use x11rb::protocol::xproto::{ConfigWindow, ConfigureRequestEvent};
  let mut xwm = started();
  feed(&mut xwm, vec![create(XTERM, GEOMETRY, false)]);
  let request = Event::ConfigureRequest(ConfigureRequestEvent {
    parent: ROOT,
    window: XTERM,
    x: 300,
    y: 400,
    width: 640,
    height: 480,
    value_mask: ConfigWindow::WIDTH | ConfigWindow::X,
    ..Default::default()
  });
  let events = feed(&mut xwm, vec![request]);
  assert_eq!(
    events,
    [XwmEvent::ConfigureRequest {
      window: XTERM,
      geometry: Geometry::new(300, 20, 640, 316),
    }]
  );
}

#[test]
fn configure_sends_a_synthetic_notify_when_the_size_stays() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  xwm.connection.requests.clear();
  let moved = Geometry::new(0, 0, GEOMETRY.width, GEOMETRY.height);
  xwm.configure(XTERM, moved, false).unwrap();
  // The same geometry again is not sent twice.
  xwm.configure(XTERM, moved, false).unwrap();
  let resized = Geometry::new(0, 0, 640, 720);
  xwm.configure(XTERM, resized, false).unwrap();
  // An answer to a ConfigureRequest is always sent.
  xwm.configure(XTERM, resized, true).unwrap();
  // Never an empty size.
  xwm
    .configure(XTERM, Geometry::new(0, 0, 0, 0), false)
    .unwrap();
  assert_eq!(
    xwm.connection.requests,
    [
      Request::Configure(XTERM, moved),
      Request::SyntheticConfigure(XTERM, moved),
      Request::Configure(XTERM, resized),
      Request::Configure(XTERM, resized),
      Request::SyntheticConfigure(XTERM, resized),
      Request::Configure(XTERM, Geometry::new(0, 0, 1, 1)),
    ]
  );
  // Unknown windows are left alone.
  xwm.connection.requests.clear();
  xwm.configure(0x123, moved, true).unwrap();
  assert!(xwm.connection.requests.is_empty());
}

#[test]
fn set_state_writes_net_wm_state_when_it_changes() {
  let mut xwm = started();
  xterm_properties(&mut xwm, XTERM);
  xwm.connection.properties.insert(
    (XTERM, atom(AtomName::NET_WM_STATE)),
    Property::words(4, &[atom(AtomName::NET_WM_STATE_MODAL)]),
  );
  feed(
    &mut xwm,
    vec![create(XTERM, GEOMETRY, false), map_request(XTERM)],
  );
  xwm.connection.requests.clear();
  let state = NetWmState {
    fullscreen: true,
    focused: true,
    ..NetWmState::default()
  };
  xwm.set_state(XTERM, state).unwrap();
  xwm.set_state(XTERM, state).unwrap();
  assert_eq!(
    xwm.connection.requests,
    [Request::Property32(
      XTERM,
      atom(AtomName::NET_WM_STATE),
      4,
      vec![
        atom(AtomName::NET_WM_STATE_MODAL),
        atom(AtomName::NET_WM_STATE_FULLSCREEN),
        atom(AtomName::NET_WM_STATE_FOCUSED),
      ],
    )]
  );
}

#[test]
fn close_asks_politely_or_kills_the_client() {
  let mut xwm = started();
  map_xterm(&mut xwm);
  let xeyes = 0x800001;
  feed(
    &mut xwm,
    vec![create(xeyes, GEOMETRY, false), map_request(xeyes)],
  );
  xwm.connection.requests.clear();
  xwm.close(XTERM).unwrap();
  xwm.close(xeyes).unwrap();
  assert_eq!(
    xwm.connection.requests,
    [
      Request::ClientMessage(
        XTERM,
        atom(AtomName::WM_PROTOCOLS),
        [atom(AtomName::WM_DELETE_WINDOW), 0, 0, 0, 0],
      ),
      Request::KillClient(xeyes),
    ]
  );
}

#[test]
fn override_redirect_windows_map_on_map_notify_and_stay_unlisted() {
  use x11rb::protocol::xproto::MapNotifyEvent;
  let mut xwm = started();
  let menu = 0x900001;
  let map_notify = Event::MapNotify(MapNotifyEvent {
    event: ROOT,
    window: menu,
    override_redirect: true,
    ..Default::default()
  });
  let events = feed(
    &mut xwm,
    vec![create(menu, GEOMETRY, true), map_notify.clone()],
  );
  assert!(matches!(&events[..], [XwmEvent::Mapped { window, info }]
    if *window == menu && info.kind() == Kind::Unmanaged));
  assert!(feed(&mut xwm, vec![map_notify]).is_empty());
  assert!(xwm.client_list().is_empty());
  xwm.connection.requests.clear();
  xwm.configure(menu, GEOMETRY, true).unwrap();
  xwm.set_state(menu, NetWmState::default()).unwrap();
  assert!(xwm.connection.requests.is_empty());
  let events = feed(&mut xwm, vec![unmap(menu)]);
  assert_eq!(events, [XwmEvent::Unmapped { window: menu }]);
  // No WM_STATE or list change for it.
  assert!(xwm.connection.requests.is_empty());
}
