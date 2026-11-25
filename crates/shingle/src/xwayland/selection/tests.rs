//! The Wayland half of the selection bridge with the in-process test
//! client as a wlr-data-control client.

use std::{
  io::{Read, Write},
  os::fd::AsFd,
};

use shingle_xwayland::{Selection, SelectionSend, xwm::mime};
use wayland_client::{Proxy, protocol::wl_seat};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
  zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
  zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

use super::*;
use crate::{Compositor, test_client::TestClient};

/// A compositor with a wlr-data-control client and its device.
struct Fixture {
  compositor: Compositor,
  client: TestClient,
  manager: ZwlrDataControlManagerV1,
  device: ZwlrDataControlDeviceV1,
}

impl Fixture {
  fn new() -> Self {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let manager = client.bind::<ZwlrDataControlManagerV1>(2);
    let seat = client.bind::<wl_seat::WlSeat>(5);
    let device = manager.get_data_device(&seat, &client.handle, ());
    client.roundtrip(&mut compositor);
    Self {
      compositor,
      client,
      manager,
      device,
    }
  }

  fn state(&mut self) -> &mut CompositorState {
    &mut self.compositor.state
  }

  fn roundtrip(&mut self) {
    self.client.roundtrip(&mut self.compositor);
  }

  /// Makes a data-control source offering `mime_types` the clipboard, or
  /// the primary selection.
  fn copy(
    &mut self,
    mime_types: &[&str],
    primary: bool,
  ) -> ZwlrDataControlSourceV1 {
    let source = self.manager.create_data_source(&self.client.handle, ());
    for mime_type in mime_types {
      source.offer((*mime_type).to_owned());
    }
    if primary {
      self.device.set_primary_selection(Some(&source));
    } else {
      self.device.set_selection(Some(&source));
    }
    self.roundtrip();
    source
  }
}

/// A pipe as files: (read end, write end).
fn pipe() -> (std::fs::File, std::fs::File) {
  let (read, write) = rustix::pipe::pipe().unwrap();
  (read.into(), write.into())
}

/// Reads `file` to its end.
fn read_all(mut file: std::fs::File) -> Vec<u8> {
  let mut data = Vec::new();
  file.read_to_end(&mut data).unwrap();
  data
}

#[test]
fn a_new_data_control_device_hears_both_selections_at_once() {
  // The protocol sends both events when the device is created, even with
  // no selection; wl-copy takes the primary one as the sign that primary
  // is supported.
  let fixture = Fixture::new();
  assert_eq!(fixture.client.events.control_selections, [None]);
  assert_eq!(fixture.client.events.control_primary, [None]);
}

#[test]
fn an_x_selection_reaches_data_control_and_its_reads_wait_for_x() {
  let mut fixture = Fixture::new();
  offered(
    fixture.state(),
    Selection::Clipboard,
    vec![mime::UTF8_TEXT.into(), mime::PLAIN_TEXT.into()],
  );
  fixture.roundtrip();
  let offer = fixture
    .client
    .events
    .control_selections
    .last()
    .cloned()
    .flatten()
    .expect("an offer");
  assert_eq!(
    fixture.client.events.offer_types[&offer.id()],
    [mime::UTF8_TEXT, mime::PLAIN_TEXT]
  );
  let (read, write) = pipe();
  offer.receive(mime::UTF8_TEXT.into(), write.as_fd());
  drop(write);
  // A type the X owner does not offer goes nowhere.
  let (unoffered, write) = pipe();
  offer.receive("image/png".into(), write.as_fd());
  drop(write);
  fixture.roundtrip();
  let receives =
    std::mem::take(&mut fixture.state().xwayland.selection_receives);
  let [(Selection::Clipboard, mime_type, fd)] = &receives[..] else {
    panic!("not one clipboard receive: {receives:?}");
  };
  assert_eq!(mime_type, mime::UTF8_TEXT);
  // The descriptor is the client's pipe: what the window manager writes
  // arrives.
  let mut fd = std::fs::File::from(fd.try_clone().unwrap());
  fd.write_all(b"from X").unwrap();
  drop((fd, receives));
  assert_eq!(read_all(read), b"from X");
  assert!(read_all(unoffered).is_empty());
  // The X owner going away clears it.
  cleared(fixture.state(), Selection::Clipboard);
  fixture.roundtrip();
  assert!(
    fixture
      .client
      .events
      .control_selections
      .last()
      .unwrap()
      .is_none()
  );
  assert_eq!(
    wayland_side(fixture.state(), Selection::Clipboard),
    (None, None)
  );
}

#[test]
fn an_x_primary_selection_reaches_data_control() {
  let mut fixture = Fixture::new();
  offered(
    fixture.state(),
    Selection::Primary,
    vec![mime::UTF8_TEXT.into()],
  );
  fixture.roundtrip();
  let offer = fixture
    .client
    .events
    .control_primary
    .last()
    .cloned()
    .flatten()
    .expect("an offer");
  let (_read, write) = pipe();
  offer.receive(mime::UTF8_TEXT.into(), write.as_fd());
  fixture.roundtrip();
  let receives = &fixture.state().xwayland.selection_receives;
  assert_eq!(receives.len(), 1);
  assert_eq!(receives[0].0, Selection::Primary);
  // The clipboard is untouched: only its first, empty announcement.
  assert_eq!(fixture.client.events.control_selections, [None]);
}

#[test]
fn a_wayland_selection_is_reported_once_and_x_requests_reach_it() {
  let mut fixture = Fixture::new();
  let mut bridge = Bridge::default();
  let both = [Selection::Clipboard, Selection::Primary];
  assert!(bridge.changes(fixture.state(), &both).is_empty());
  let source = fixture.copy(&[mime::UTF8_TEXT], false);
  assert_eq!(
    bridge.changes(fixture.state(), &both),
    [(Selection::Clipboard, Some(vec![mime::UTF8_TEXT.to_owned()]))]
  );
  assert!(bridge.changes(fixture.state(), &both).is_empty());
  // An X client asks for it.
  let (read, write) = pipe();
  send(
    fixture.state(),
    SelectionSend {
      selection: Selection::Clipboard,
      mime_type: mime::UTF8_TEXT.into(),
      fd: write.into(),
    },
  );
  fixture.roundtrip();
  let (mime_type, fd) = fixture.client.events.control_sends.pop().unwrap();
  assert_eq!(mime_type, mime::UTF8_TEXT);
  std::fs::File::from(fd).write_all(b"to X").unwrap();
  assert_eq!(read_all(read), b"to X");
  // An X owner replaces it: the Wayland source is cancelled and the
  // change is reported as no Wayland selection.
  offered(
    fixture.state(),
    Selection::Clipboard,
    vec![mime::UTF8_TEXT.into()],
  );
  fixture.roundtrip();
  assert!(
    fixture
      .client
      .events
      .control_cancelled
      .contains(&source.id())
  );
  assert_eq!(
    bridge.changes(fixture.state(), &both),
    [(Selection::Clipboard, None)]
  );
  // A request that arrives after the change gets nothing.
  let (read, write) = pipe();
  send(
    fixture.state(),
    SelectionSend {
      selection: Selection::Clipboard,
      mime_type: mime::UTF8_TEXT.into(),
      fd: write.into(),
    },
  );
  fixture.roundtrip();
  assert!(fixture.client.events.control_sends.is_empty());
  assert!(read_all(read).is_empty());
}

#[test]
fn a_replaced_x_source_is_forgotten_and_a_late_clear_is_ignored() {
  let mut fixture = Fixture::new();
  offered(
    fixture.state(),
    Selection::Clipboard,
    vec![mime::UTF8_TEXT.into()],
  );
  let (first, _) = wayland_side(fixture.state(), Selection::Clipboard);
  let first = first.unwrap();
  assert!(first.is_x11());
  // A second X owner replaces the first.
  offered(
    fixture.state(),
    Selection::Clipboard,
    vec![mime::PLAIN_TEXT.into()],
  );
  let (second, offer) = wayland_side(fixture.state(), Selection::Clipboard);
  assert!(second.as_ref().is_some_and(SourceKey::is_x11));
  assert_ne!(second.as_ref(), Some(&first));
  assert_eq!(offer, None);
  // The first is gone: reading it does nothing.
  let (_read, write) = pipe();
  data_device::receive_from(
    fixture.state(),
    &first,
    mime::UTF8_TEXT.into(),
    write.into(),
  );
  assert!(fixture.state().xwayland.selection_receives.is_empty());
  // A Wayland client copies; the X owner's later end leaves it alone.
  fixture.copy(&[mime::UTF8_TEXT], false);
  cleared(fixture.state(), Selection::Clipboard);
  let (key, offer) = wayland_side(fixture.state(), Selection::Clipboard);
  assert!(key.is_some_and(|key| !key.is_x11()));
  assert_eq!(offer, Some(vec![mime::UTF8_TEXT.to_owned()]));
}

#[test]
fn losing_the_window_manager_forgets_x_selections_only() {
  let mut fixture = Fixture::new();
  offered(
    fixture.state(),
    Selection::Clipboard,
    vec![mime::UTF8_TEXT.into()],
  );
  fixture.copy(&[mime::UTF8_TEXT], true);
  let (_read, write) = pipe();
  fixture.state().xwayland.selection_receives.push((
    Selection::Clipboard,
    mime::UTF8_TEXT.into(),
    write.into(),
  ));
  forget_x11(fixture.state());
  assert_eq!(
    wayland_side(fixture.state(), Selection::Clipboard),
    (None, None)
  );
  assert!(
    wayland_side(fixture.state(), Selection::Primary)
      .1
      .is_some()
  );
  assert!(fixture.state().xwayland.selection_receives.is_empty());
}

#[test]
fn disable_primary_selection_bridges_only_the_clipboard() {
  let mut fixture = Fixture::new();
  assert_eq!(
    bridged(fixture.state()),
    [Selection::Clipboard, Selection::Primary]
  );
  fixture.state().disable_primary_selection = true;
  assert_eq!(bridged(fixture.state()), [Selection::Clipboard]);
}
