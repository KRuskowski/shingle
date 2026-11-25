//! Bring-up against a recording connection.

use std::collections::VecDeque;

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
  Redirect(Window),
  SelectEvents(Window, EventMask),
  CreateWindow(Window, Window),
  SelectionOwner(Window, u32),
  Property32(Window, u32, u32, Vec<u32>),
  Property8(Window, u32, u32, Vec<u8>),
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
  refuse_redirect: bool,
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
    Ok(!self.without_composite)
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
fn dispatch_drains_events_and_reports_errors() {
  let mut xwm = Xwm::start(Recording::default()).unwrap();
  let error = X11Error {
    error_kind: x11rb::protocol::ErrorKind::Window,
    error_code: 3,
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
