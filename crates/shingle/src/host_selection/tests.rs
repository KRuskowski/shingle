//! Host selections with the in-process test client as a wlr-data-control
//! client of the nested compositor.

use std::{
  io::{Read, Write},
  os::fd::AsFd,
  time::{Duration, Instant},
};

use shingle_xwayland::{Selection, SelectionSend};
use wayland_client::{Proxy, protocol::wl_seat};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
  zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
  zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
  zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

use super::*;
use crate::{Config, test_client::TestClient, xwayland::selection as x11};

const UTF8: &str = "text/plain;charset=utf-8";
const PLAIN: &str = "text/plain";

/// A compositor with a wlr-data-control client and its device.
struct Fixture {
  compositor: Compositor,
  client: TestClient,
  manager: ZwlrDataControlManagerV1,
  device: ZwlrDataControlDeviceV1,
}

impl Fixture {
  fn new() -> Self {
    Self::with(Config::default())
  }

  fn with(config: Config) -> Self {
    let (mut compositor, path) = crate::tests::test_compositor_with(config);
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

  /// The offer the client heard of last for the clipboard or primary.
  fn last_offer(&self, primary: bool) -> Option<ZwlrDataControlOfferV1> {
    let events = &self.client.events;
    let offers = if primary {
      &events.control_primary
    } else {
      &events.control_selections
    };
    offers.last().cloned().flatten()
  }

  /// The MIME types `offer` announced.
  fn offer_types(&self, offer: &ZwlrDataControlOfferV1) -> Vec<String> {
    self.client.events.offer_types[&offer.id()].clone()
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

fn strings(mime_types: &[&str]) -> Vec<String> {
  mime_types.iter().map(|&mime| mime.to_owned()).collect()
}

#[test]
fn a_host_selection_is_offered_with_its_types_and_reads_wait_for_it() {
  let mut fixture = Fixture::new();
  let id = fixture.compositor.set_host_selection(
    SelectionKind::Clipboard,
    strings(&[UTF8, PLAIN, UTF8, ""]),
  );
  fixture.roundtrip();
  assert_eq!(
    fixture.compositor.selection_owner(SelectionKind::Clipboard),
    SelectionOwner::Host(id.clone())
  );
  let offer = fixture.last_offer(false).expect("an offer");
  // Repeats and empty names are left out.
  assert_eq!(fixture.offer_types(&offer), [UTF8, PLAIN]);
  let (read, write) = pipe();
  offer.receive(PLAIN.into(), write.as_fd());
  drop(write);
  // A type the host does not offer goes nowhere.
  let (unoffered, write) = pipe();
  offer.receive("image/png".into(), write.as_fd());
  drop(write);
  fixture.roundtrip();
  let requests = fixture.compositor.take_host_selection_requests();
  let [request] = &requests[..] else {
    panic!("not one request: {requests:?}");
  };
  assert_eq!(request.kind, SelectionKind::Clipboard);
  assert_eq!(request.source, id);
  assert_eq!(request.mime_type, PLAIN);
  // The descriptor is the reader's own pipe.
  let mut fd = std::fs::File::from(request.fd.try_clone().unwrap());
  fd.write_all(b"from the host").unwrap();
  drop((fd, requests));
  assert_eq!(read_all(read), b"from the host");
  assert!(read_all(unoffered).is_empty());
  assert!(fixture.compositor.take_host_selection_requests().is_empty());
  // The primary selection is untouched.
  assert_eq!(
    fixture.compositor.selection_owner(SelectionKind::Primary),
    SelectionOwner::None
  );
}

#[test]
fn a_host_primary_selection_reaches_data_control() {
  let mut fixture = Fixture::new();
  let id = fixture
    .compositor
    .set_host_selection(SelectionKind::Primary, strings(&[UTF8]));
  fixture.roundtrip();
  let offer = fixture.last_offer(true).expect("an offer");
  let (_read, write) = pipe();
  offer.receive(UTF8.into(), write.as_fd());
  fixture.roundtrip();
  let requests = fixture.compositor.take_host_selection_requests();
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].kind, SelectionKind::Primary);
  assert_eq!(requests[0].source, id);
  assert_eq!(fixture.client.events.control_selections, [None]);
}

#[test]
fn a_local_copy_replaces_the_host_source_and_a_late_clear_leaves_it() {
  let mut fixture = Fixture::new();
  let host = fixture
    .compositor
    .set_host_selection(SelectionKind::Clipboard, strings(&[UTF8]));
  let source = fixture.copy(&[PLAIN, UTF8], false);
  let SelectionOwner::Local {
    source: local,
    mime_types,
  } = fixture.compositor.selection_owner(SelectionKind::Clipboard)
  else {
    panic!("not a local selection");
  };
  assert_ne!(local, host);
  assert_eq!(mime_types, [PLAIN, UTF8]);
  // The host clearing its selection now does not clear the local one.
  fixture
    .compositor
    .clear_host_selection(SelectionKind::Clipboard);
  assert!(matches!(
    fixture.compositor.selection_owner(SelectionKind::Clipboard),
    SelectionOwner::Local { .. }
  ));
  // The replaced host source is forgotten: nothing reads it.
  let (_read, write) = pipe();
  data_device::receive_from(
    &mut fixture.compositor.state,
    &host.0,
    UTF8.into(),
    write.into(),
  );
  assert!(fixture.compositor.take_host_selection_requests().is_empty());
  // A new host selection cancels the local source; clearing it leaves
  // no selection.
  fixture
    .compositor
    .set_host_selection(SelectionKind::Clipboard, strings(&[UTF8]));
  fixture.roundtrip();
  assert!(
    fixture
      .client
      .events
      .control_cancelled
      .contains(&source.id())
  );
  fixture
    .compositor
    .clear_host_selection(SelectionKind::Clipboard);
  fixture.roundtrip();
  assert_eq!(
    fixture.compositor.selection_owner(SelectionKind::Clipboard),
    SelectionOwner::None
  );
  assert!(fixture.last_offer(false).is_none());
}

#[test]
fn a_second_host_selection_has_a_new_identity() {
  let mut fixture = Fixture::new();
  let first = fixture
    .compositor
    .set_host_selection(SelectionKind::Clipboard, strings(&[UTF8]));
  let second = fixture
    .compositor
    .set_host_selection(SelectionKind::Clipboard, strings(&[UTF8]));
  assert_ne!(first, second);
  assert_eq!(
    fixture.compositor.selection_owner(SelectionKind::Clipboard),
    SelectionOwner::Host(second)
  );
}

#[test]
fn host_reads_reach_the_local_source_only_while_it_holds_the_selection() {
  let mut fixture = Fixture::new();
  fixture.copy(&[UTF8], true);
  let SelectionOwner::Local { source, .. } =
    fixture.compositor.selection_owner(SelectionKind::Primary)
  else {
    panic!("not a local selection");
  };
  let (read, write) = pipe();
  assert!(fixture.compositor.send_selection(
    SelectionKind::Primary,
    &source,
    UTF8.into(),
    write.into(),
  ));
  fixture.roundtrip();
  let (mime_type, fd) = fixture.client.events.control_sends.pop().unwrap();
  assert_eq!(mime_type, UTF8);
  std::fs::File::from(fd).write_all(b"to the host").unwrap();
  assert_eq!(read_all(read), b"to the host");
  // Not a type it offers, not the clipboard, and not after a newer copy.
  let (_read, write) = pipe();
  assert!(!fixture.compositor.send_selection(
    SelectionKind::Primary,
    &source,
    PLAIN.into(),
    write.into(),
  ));
  let (_read, write) = pipe();
  assert!(!fixture.compositor.send_selection(
    SelectionKind::Clipboard,
    &source,
    UTF8.into(),
    write.into(),
  ));
  fixture.copy(&[UTF8], true);
  let (read, write) = pipe();
  assert!(!fixture.compositor.send_selection(
    SelectionKind::Primary,
    &source,
    UTF8.into(),
    write.into(),
  ));
  fixture.roundtrip();
  assert!(fixture.client.events.control_sends.is_empty());
  assert!(read_all(read).is_empty());
  // A host source is never sent to: it is the host's own.
  let host = fixture
    .compositor
    .set_host_selection(SelectionKind::Primary, strings(&[UTF8]));
  let (_read, write) = pipe();
  assert!(!fixture.compositor.send_selection(
    SelectionKind::Primary,
    &host,
    UTF8.into(),
    write.into(),
  ));
  assert!(fixture.compositor.take_host_selection_requests().is_empty());
}

#[test]
fn a_mebibyte_reaches_the_host_through_a_slow_reader_without_a_stall() {
  let mut fixture = Fixture::new();
  fixture.copy(&[UTF8], false);
  let SelectionOwner::Local { source, .. } =
    fixture.compositor.selection_owner(SelectionKind::Clipboard)
  else {
    panic!("not a local selection");
  };
  let (read, write) = pipe();
  assert!(fixture.compositor.send_selection(
    SelectionKind::Clipboard,
    &source,
    UTF8.into(),
    write.into(),
  ));
  fixture.roundtrip();
  let (_, fd) = fixture.client.events.control_sends.pop().unwrap();
  let payload = (0..1 << 20).map(|i| (i % 251) as u8).collect::<Vec<_>>();
  let expected = payload.clone();
  // The local source writes the whole mebibyte, far more than a pipe
  // holds, into the host reader's own descriptor.
  let writer = std::thread::spawn(move || {
    std::fs::File::from(fd).write_all(&payload).unwrap();
  });
  // The reader takes its time; meanwhile every turn of the compositor
  // returns at once, since no data passes through it.
  let reader = std::thread::spawn(move || {
    let mut read = read;
    let mut data = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
      std::thread::sleep(Duration::from_millis(5));
      match read.read(&mut chunk).unwrap() {
        0 => break,
        count => data.extend_from_slice(&chunk[..count]),
      }
    }
    data
  });
  let deadline = Instant::now() + Duration::from_secs(20);
  while !reader.is_finished() {
    assert!(Instant::now() < deadline, "the transfer did not finish");
    let started = Instant::now();
    fixture.compositor.poll(Some(Duration::ZERO)).unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    std::thread::sleep(Duration::from_millis(2));
  }
  writer.join().unwrap();
  assert_eq!(reader.join().unwrap(), expected);
}

#[test]
fn x_clients_see_host_selections_and_their_reads_go_to_the_host() {
  let mut fixture = Fixture::new();
  let id = fixture
    .compositor
    .set_host_selection(SelectionKind::Clipboard, strings(&[UTF8]));
  // The XWayland bridge reports it to X as a Wayland-side selection.
  let (key, offer) =
    x11::wayland_side(&fixture.compositor.state, Selection::Clipboard);
  assert_eq!(key, Some(id.0.clone()));
  assert_eq!(offer, Some(strings(&[UTF8])));
  // An X client's request for it becomes a host read.
  let (_read, write) = pipe();
  x11::send(
    &mut fixture.compositor.state,
    SelectionSend {
      selection: Selection::Clipboard,
      mime_type: UTF8.into(),
      fd: write.into(),
    },
  );
  let requests = fixture.compositor.take_host_selection_requests();
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].source, id);
  // An X client's own selection is local, and the host sees it as such.
  x11::offered(
    &mut fixture.compositor.state,
    Selection::Clipboard,
    strings(&[UTF8]),
  );
  assert!(matches!(
    fixture.compositor.selection_owner(SelectionKind::Clipboard),
    SelectionOwner::Local { .. }
  ));
}

#[test]
fn disable_primary_selection_keeps_primary_from_the_host() {
  let fixture = Fixture::new();
  assert!(fixture.compositor.primary_selection_enabled());
  let fixture = Fixture::with(Config {
    disable_primary_selection: true,
    ..Config::default()
  });
  assert!(!fixture.compositor.primary_selection_enabled());
}
