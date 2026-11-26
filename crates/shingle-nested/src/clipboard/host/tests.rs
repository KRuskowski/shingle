//! The host connection: protocol choice, the host socket, and two bridges
//! with fake nested sides sharing a real Shingle as their host.

use std::{
  io::{Read as _, Write as _},
  path::PathBuf,
  sync::atomic::{AtomicU32, Ordering},
  time::{Duration, Instant},
};

use shingle::{Compositor, Config, SelectionOwner};

use super::*;
use crate::clipboard::{
  Owner,
  tests::{FakeNested, PLAIN, UTF8, strings},
};

fn global(name: u32, interface: &str, version: u32) -> Global {
  Global {
    name,
    interface: interface.into(),
    version,
  }
}

#[test]
fn ext_data_control_is_preferred_and_wlr_is_bound_at_two_at_most() {
  let seat = global(1, "wl_seat", 9);
  let wlr = global(2, WLR_MANAGER, 3);
  let ext = global(3, EXT_MANAGER, 1);
  let both = choose(&[seat.clone(), wlr.clone(), ext.clone()]).unwrap();
  assert_eq!(both.seat, 1);
  assert_eq!(both.manager, Manager::Ext(3));
  assert!(both.has_primary());
  let wlr_only = choose(&[wlr, seat.clone()]).unwrap();
  assert_eq!(wlr_only.manager, Manager::Wlr(2, 2));
  assert!(wlr_only.has_primary());
  // Version 1 has no primary selection.
  let old = choose(&[seat.clone(), global(4, WLR_MANAGER, 1)]).unwrap();
  assert_eq!(old.manager, Manager::Wlr(4, 1));
  assert!(!old.has_primary());
  // Without a manager or a seat there is nothing to bind.
  assert_eq!(choose(&[seat]), None);
  assert_eq!(choose(&[ext]), None);
  // The first seat is taken.
  let seats = [global(7, "wl_seat", 1), global(8, "wl_seat", 1)];
  let extra = [global(9, EXT_MANAGER, 1)];
  assert_eq!(choose(&[&seats[..], &extra[..]].concat()).unwrap().seat, 7);
}

#[test]
fn the_host_socket_is_found_as_libwayland_finds_it() {
  let env = |pairs: &'static [(&'static str, &'static str)]| {
    move |name: &str| {
      pairs
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| (*value).to_owned())
    }
  };
  assert_eq!(
    host_socket(env(&[
      ("WAYLAND_DISPLAY", "wayland-1"),
      ("XDG_RUNTIME_DIR", "/run/user/7"),
    ])),
    Some(PathBuf::from("/run/user/7/wayland-1"))
  );
  assert_eq!(
    host_socket(env(&[
      ("WAYLAND_DISPLAY", "/tmp/host/wayland-0"),
      ("XDG_RUNTIME_DIR", "/run/user/7"),
    ])),
    Some(PathBuf::from("/tmp/host/wayland-0"))
  );
  // X11 hosts, a lone WAYLAND_SOCKET and empty values have none.
  assert_eq!(host_socket(env(&[("DISPLAY", ":0")])), None);
  assert_eq!(host_socket(env(&[("WAYLAND_SOCKET", "5")])), None);
  assert_eq!(
    host_socket(env(&[
      ("WAYLAND_DISPLAY", ""),
      ("XDG_RUNTIME_DIR", "/run/user/7"),
    ])),
    None
  );
  assert_eq!(host_socket(env(&[("WAYLAND_DISPLAY", "wayland-1")])), None);
}

/// A real Shingle as the host, and two bridges with fake nested sides as
/// two nested Shingles on it.
struct Fixture {
  host: Compositor,
  bridges: [(HostClipboard<u32>, FakeNested); 2],
}

impl Fixture {
  fn new() -> Self {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
      "shingle-nested-clipboard-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);
    let host = Compositor::with_socket_path(Config::default(), &path)
      .expect("the temporary directory accepts a Wayland socket");
    let both = vec![SelectionKind::Clipboard, SelectionKind::Primary];
    let bridge = || {
      let stream = UnixStream::connect(&path).expect("the host accepts");
      let clipboard = HostClipboard::connect(stream, both.clone())
        .expect("the host can be talked to");
      (clipboard, FakeNested::default())
    };
    let mut fixture = Self {
      host,
      bridges: [bridge(), bridge()],
    };
    fixture.turn_until("both bridges bind", |fixture| {
      fixture
        .bridges
        .iter()
        .all(|(clipboard, _)| clipboard.shared().is_some())
    });
    for (clipboard, _) in &fixture.bridges {
      assert_eq!(
        clipboard.shared(),
        Some(vec![SelectionKind::Clipboard, SelectionKind::Primary])
      );
    }
    fixture
  }

  /// One turn of the host and of each bridge, as nested's event loop
  /// runs it, none of which may wait.
  fn turn(&mut self) {
    let started = Instant::now();
    self.host.poll(Some(Duration::ZERO)).expect("the host runs");
    for (clipboard, nested) in &mut self.bridges {
      clipboard.dispatch(nested).expect("the host is there");
      clipboard.sync(nested).expect("the host is there");
    }
    self.host.poll(Some(Duration::ZERO)).expect("the host runs");
    assert!(
      started.elapsed() < Duration::from_millis(500),
      "a turn waited"
    );
  }

  fn turn_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(self) {
      assert!(Instant::now() < deadline, "timed out waiting until {what}");
      self.turn();
      std::thread::sleep(Duration::from_millis(1));
    }
  }

  fn turns(&mut self, count: usize) {
    for _ in 0..count {
      self.turn();
      std::thread::sleep(Duration::from_millis(1));
    }
  }

  fn nested(&mut self, index: usize) -> &mut FakeNested {
    &mut self.bridges[index].1
  }
}

#[test]
fn two_nested_shingles_share_both_selections_through_a_real_host() {
  let mut fixture = Fixture::new();
  // The first nested Shingle copies; the host and the second see it.
  fixture
    .nested(0)
    .copy(SelectionKind::Clipboard, &[UTF8, PLAIN]);
  fixture.turn_until("the second imports the clipboard", |fixture| {
    !fixture.bridges[1].1.imported.is_empty()
  });
  assert_eq!(
    fixture.bridges[1].1.imported,
    [(SelectionKind::Clipboard, strings(&[UTF8, PLAIN]))]
  );
  assert!(matches!(
    fixture.host.selection_owner(SelectionKind::Clipboard),
    SelectionOwner::Local { mime_types, .. } if mime_types == [UTF8, PLAIN]
  ));
  fixture.turns(20);
  // Neither takes its own or the other's selection back.
  assert!(fixture.bridges[0].1.imported.is_empty());
  assert_eq!(fixture.bridges[1].1.imported.len(), 1);
  // The second reads a mebibyte of it; the first's source writes it
  // straight into the second's reader, which is slow, while every turn
  // goes on without waiting.
  let source = fixture
    .nested(1)
    .host_source(SelectionKind::Clipboard)
    .unwrap();
  let reader = fixture
    .nested(1)
    .read(SelectionKind::Clipboard, source, UTF8);
  fixture.turn_until("the first is asked for the data", |fixture| {
    !fixture.bridges[0].1.sent.is_empty()
  });
  let (kind, _, mime_type, fd) = fixture.nested(0).sent.pop().unwrap();
  assert_eq!((kind, mime_type.as_str()), (SelectionKind::Clipboard, UTF8));
  let payload = (0..1 << 20).map(|i| (i % 241) as u8).collect::<Vec<_>>();
  let expected = payload.clone();
  let writer = std::thread::spawn(move || {
    std::fs::File::from(fd).write_all(&payload).unwrap();
  });
  let reader = std::thread::spawn(move || {
    let mut reader = reader;
    let mut data = Vec::new();
    let mut chunk = vec![0; 32 * 1024];
    loop {
      std::thread::sleep(Duration::from_millis(2));
      match reader.read(&mut chunk).unwrap() {
        0 => break,
        count => data.extend_from_slice(&chunk[..count]),
      }
    }
    data
  });
  fixture.turn_until("the transfer ends", |_| reader.is_finished());
  writer.join().unwrap();
  assert_eq!(reader.join().unwrap(), expected);
  // The second copies a primary selection, which reaches the first.
  fixture.nested(1).copy(SelectionKind::Primary, &[PLAIN]);
  fixture.turn_until("the first imports the primary selection", |fixture| {
    !fixture.bridges[0].1.imported.is_empty()
  });
  assert_eq!(
    fixture.bridges[0].1.imported,
    [(SelectionKind::Primary, strings(&[PLAIN]))]
  );
  // The second copies to the clipboard too: it replaces the first's on
  // the host, and the first now has the second's.
  fixture.nested(1).copy(SelectionKind::Clipboard, &[PLAIN]);
  fixture.turn_until("the first imports the clipboard", |fixture| {
    fixture.bridges[0].1.imported.len() == 2
  });
  assert_eq!(
    fixture.bridges[0].1.imported[1],
    (SelectionKind::Clipboard, strings(&[PLAIN]))
  );
  assert!(matches!(
    fixture.bridges[0].1.clipboard,
    Some(Owner::Host(_))
  ));
  fixture.turns(20);
  assert_eq!(fixture.bridges[1].1.imported.len(), 1);
}
