//! The bridge with a fake host and a fake nested compositor.

use std::{
  io::{Read as _, Write as _},
  os::{
    fd::{AsFd, OwnedFd},
    unix::fs::MetadataExt,
  },
  time::{Duration, Instant},
};

use super::*;

pub(crate) const UTF8: &str = "text/plain;charset=utf-8";
pub(crate) const PLAIN: &str = "text/plain";

pub(crate) fn strings(mime_types: &[&str]) -> Vec<String> {
  mime_types.iter().map(|&mime| mime.to_owned()).collect()
}

/// What the bridge asked of the host, in order.
#[derive(Debug, PartialEq)]
enum Call {
  Offer(u32, SelectionKind, Vec<String>),
  Withdraw(u32),
  Receive(u32, String),
  Discard(u32),
}

/// A host whose sources and offers are numbers.
#[derive(Default)]
struct FakeHost {
  calls: Vec<Call>,
  next: u32,
  /// The descriptors of `receive` calls, in order.
  fds: Vec<OwnedFd>,
}

impl HostSide for FakeHost {
  type Source = u32;
  type Offer = u32;

  fn offer(&mut self, kind: SelectionKind, mime_types: &[String]) -> u32 {
    self.next += 1;
    self
      .calls
      .push(Call::Offer(self.next, kind, mime_types.to_vec()));
    self.next
  }

  fn withdraw(&mut self, source: u32) {
    self.calls.push(Call::Withdraw(source));
  }

  fn receive(&mut self, offer: &u32, mime_type: String, fd: OwnedFd) {
    self.calls.push(Call::Receive(*offer, mime_type));
    self.fds.push(fd);
  }

  fn discard(&mut self, offer: u32) {
    self.calls.push(Call::Discard(offer));
  }
}

/// A nested compositor whose sources are numbers.
#[derive(Default)]
pub(crate) struct FakeNested {
  pub(crate) clipboard: Option<Owner<u32>>,
  pub(crate) primary: Option<Owner<u32>>,
  next: u32,
  /// `set_host` calls: kind and MIME types.
  pub(crate) imported: Vec<(SelectionKind, Vec<String>)>,
  /// `send` calls: kind, source and MIME type, with the descriptor.
  pub(crate) sent: Vec<(SelectionKind, u32, String, OwnedFd)>,
  pub(crate) reads: Vec<Read<u32>>,
}

impl FakeNested {
  fn owner_mut(&mut self, kind: SelectionKind) -> &mut Option<Owner<u32>> {
    match kind {
      SelectionKind::Clipboard => &mut self.clipboard,
      SelectionKind::Primary => &mut self.primary,
    }
  }

  /// A nested client copies `mime_types`; returns its source.
  pub(crate) fn copy(
    &mut self,
    kind: SelectionKind,
    mime_types: &[&str],
  ) -> u32 {
    self.next += 1;
    *self.owner_mut(kind) = Some(Owner::Local {
      source: self.next,
      mime_types: strings(mime_types),
    });
    self.next
  }

  /// A nested client reads `mime_type` of host source `source`; returns
  /// the read end of its pipe.
  pub(crate) fn read(
    &mut self,
    kind: SelectionKind,
    source: u32,
    mime_type: &str,
  ) -> std::io::PipeReader {
    let (reader, writer) = std::io::pipe().unwrap();
    self.reads.push(Read {
      kind,
      source,
      mime_type: mime_type.into(),
      fd: writer.into(),
    });
    reader
  }

  /// The host source installed last for `kind`.
  pub(crate) fn host_source(&self, kind: SelectionKind) -> Option<u32> {
    let owner = match kind {
      SelectionKind::Clipboard => &self.clipboard,
      SelectionKind::Primary => &self.primary,
    };
    match owner {
      Some(Owner::Host(source)) => Some(*source),
      _ => None,
    }
  }
}

impl NestedSide for FakeNested {
  type Id = u32;

  fn owner(&self, kind: SelectionKind) -> Owner<u32> {
    let owner = match kind {
      SelectionKind::Clipboard => &self.clipboard,
      SelectionKind::Primary => &self.primary,
    };
    owner.clone().unwrap_or(Owner::None)
  }

  fn set_host(&mut self, kind: SelectionKind, mime_types: Vec<String>) -> u32 {
    self.next += 1;
    self.imported.push((kind, mime_types));
    *self.owner_mut(kind) = Some(Owner::Host(self.next));
    self.next
  }

  fn clear_host(&mut self, kind: SelectionKind) {
    let owner = self.owner_mut(kind);
    if matches!(owner, Some(Owner::Host(_))) {
      *owner = None;
    }
  }

  fn send(
    &mut self,
    kind: SelectionKind,
    source: &u32,
    mime_type: String,
    fd: OwnedFd,
  ) {
    self.sent.push((kind, *source, mime_type, fd));
  }

  fn take_reads(&mut self) -> Vec<Read<u32>> {
    std::mem::take(&mut self.reads)
  }
}

/// A bridge for both selections with its two fakes.
struct Fixture {
  bridge: Bridge<u32, u32, u32>,
  host: FakeHost,
  nested: FakeNested,
  /// The number of the next host offer; far from the fake host's own
  /// source numbers.
  next_offer: u32,
}

impl Fixture {
  fn new(kinds: &[SelectionKind]) -> Self {
    Self {
      bridge: Bridge::new(kinds),
      host: FakeHost::default(),
      nested: FakeNested::default(),
      next_offer: 100,
    }
  }

  fn both() -> Self {
    Self::new(&[SelectionKind::Clipboard, SelectionKind::Primary])
  }

  fn event(&mut self, event: HostEvent<u32, u32>) {
    self
      .bridge
      .host_event(&mut self.host, &mut self.nested, event);
  }

  fn sync(&mut self) {
    self.bridge.sync(&mut self.host, &mut self.nested);
  }

  /// The host announces a new selection offering `mime_types`; returns
  /// the offer.
  fn host_copies(&mut self, kind: SelectionKind, mime_types: &[&str]) -> u32 {
    self.next_offer += 1;
    let offer = self.next_offer;
    self.event(HostEvent::Selection {
      kind,
      offer: Some(offer),
      mime_types: strings(mime_types),
    });
    offer
  }

  /// The host cancels `source` and announces a selection of its own.
  fn host_takes_over(&mut self, kind: SelectionKind, source: u32) -> u32 {
    self.event(HostEvent::Cancelled { source });
    self.host_copies(kind, &[UTF8])
  }

  fn calls(&mut self) -> Vec<Call> {
    std::mem::take(&mut self.host.calls)
  }
}

#[test]
fn a_nested_copy_is_offered_once_and_its_echo_is_not_taken_back() {
  let mut fixture = Fixture::both();
  fixture
    .nested
    .copy(SelectionKind::Clipboard, &[UTF8, PLAIN]);
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [Call::Offer(
      1,
      SelectionKind::Clipboard,
      strings(&[UTF8, PLAIN])
    )]
  );
  // The host reports its new selection, which is Shingle's own source:
  // the echo goes, and nested keeps its own selection.
  let echo = fixture.host_copies(SelectionKind::Clipboard, &[UTF8, PLAIN]);
  assert_eq!(fixture.calls(), [Call::Discard(echo)]);
  assert!(fixture.nested.imported.is_empty());
  // Nothing changed, so nothing more is offered.
  fixture.sync();
  fixture.sync();
  assert!(fixture.calls().is_empty());
}

#[test]
fn a_host_selection_is_installed_and_not_offered_back() {
  let mut fixture = Fixture::both();
  let offer = fixture.host_copies(SelectionKind::Primary, &[PLAIN, UTF8]);
  assert_eq!(
    fixture.nested.imported,
    [(SelectionKind::Primary, strings(&[PLAIN, UTF8]))]
  );
  fixture.sync();
  fixture.sync();
  // Nested's selection is the host's, so nothing goes back.
  assert!(fixture.calls().is_empty());
  // Nested's reads of it go to the host offer, in the reader's order.
  let source = fixture.nested.host_source(SelectionKind::Primary).unwrap();
  let _first = fixture.nested.read(SelectionKind::Primary, source, UTF8);
  let _second = fixture.nested.read(SelectionKind::Primary, source, PLAIN);
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [
      Call::Receive(offer, UTF8.into()),
      Call::Receive(offer, PLAIN.into())
    ]
  );
  assert!(fixture.nested.clipboard.is_none());
}

#[test]
fn an_empty_host_selection_clears_the_host_source_only() {
  let mut fixture = Fixture::both();
  let offer = fixture.host_copies(SelectionKind::Clipboard, &[UTF8]);
  // An offer of no types, then no selection at all.
  let empty = fixture.host_copies(SelectionKind::Clipboard, &[]);
  assert_eq!(
    fixture.calls(),
    [Call::Discard(offer), Call::Discard(empty)]
  );
  assert!(fixture.nested.clipboard.is_none());
  assert_eq!(fixture.nested.imported.len(), 1);
  // A nested copy made since is not cleared by the host's empty one.
  fixture.nested.copy(SelectionKind::Primary, &[UTF8]);
  fixture.event(HostEvent::Selection {
    kind: SelectionKind::Primary,
    offer: None,
    mime_types: Vec::new(),
  });
  assert!(matches!(fixture.nested.primary, Some(Owner::Local { .. })));
}

#[test]
fn a_new_nested_copy_replaces_the_host_source_without_a_gap() {
  let mut fixture = Fixture::both();
  fixture.nested.copy(SelectionKind::Clipboard, &[UTF8]);
  fixture.sync();
  fixture.nested.copy(SelectionKind::Clipboard, &[PLAIN]);
  fixture.sync();
  // The new source takes the host's selection before the old one goes.
  assert_eq!(
    fixture.calls(),
    [
      Call::Offer(1, SelectionKind::Clipboard, strings(&[UTF8])),
      Call::Offer(2, SelectionKind::Clipboard, strings(&[PLAIN])),
      Call::Withdraw(1),
    ]
  );
  // The host's cancel of the old one is answered with nothing more.
  fixture.event(HostEvent::Cancelled { source: 1 });
  assert_eq!(fixture.calls(), [Call::Withdraw(1)]);
  // Host reads reach only the current source's nested source.
  let (_reader, writer) = std::io::pipe().unwrap();
  fixture.event(HostEvent::Send {
    source: 2,
    mime_type: PLAIN.into(),
    fd: writer.into(),
  });
  let (reader, writer) = std::io::pipe().unwrap();
  fixture.event(HostEvent::Send {
    source: 1,
    mime_type: UTF8.into(),
    fd: writer.into(),
  });
  let sent = &fixture.nested.sent;
  assert_eq!(sent.len(), 1);
  assert_eq!(
    (sent[0].0, sent[0].1, sent[0].2.as_str()),
    (SelectionKind::Clipboard, 2, PLAIN)
  );
  // The stale read's descriptor was closed: its reader sees the end.
  let mut data = Vec::new();
  let mut reader = reader;
  reader.read_to_end(&mut data).unwrap();
  assert!(data.is_empty());
}

#[test]
fn a_host_copy_wins_and_the_nested_source_is_not_offered_again() {
  let mut fixture = Fixture::both();
  fixture.nested.copy(SelectionKind::Clipboard, &[UTF8]);
  fixture.sync();
  fixture.calls();
  fixture.host_takes_over(SelectionKind::Clipboard, 1);
  assert_eq!(fixture.calls(), [Call::Withdraw(1)]);
  // The host's selection replaced nested's.
  assert_eq!(
    fixture.nested.imported,
    [(SelectionKind::Clipboard, strings(&[UTF8]))]
  );
  assert_ne!(fixture.nested.host_source(SelectionKind::Clipboard), None);
  fixture.sync();
  assert!(fixture.calls().is_empty());
  // A host send for the cancelled source reaches nothing.
  let (_reader, writer) = std::io::pipe().unwrap();
  fixture.event(HostEvent::Send {
    source: 1,
    mime_type: UTF8.into(),
    fd: writer.into(),
  });
  assert!(fixture.nested.sent.is_empty());
  // Had nested kept its source (a host that cancels but whose selection
  // nested never installs), it would still not be offered again.
  let mut fixture = Fixture::both();
  fixture.nested.copy(SelectionKind::Clipboard, &[UTF8]);
  fixture.sync();
  fixture.event(HostEvent::Cancelled { source: 1 });
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [
      Call::Offer(1, SelectionKind::Clipboard, strings(&[UTF8])),
      Call::Withdraw(1)
    ]
  );
  // A new nested copy is offered again.
  fixture.nested.copy(SelectionKind::Clipboard, &[PLAIN]);
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [Call::Offer(2, SelectionKind::Clipboard, strings(&[PLAIN]))]
  );
}

#[test]
fn a_nested_copy_over_a_host_selection_drops_the_host_offer() {
  let mut fixture = Fixture::both();
  let first = fixture.host_copies(SelectionKind::Clipboard, &[UTF8]);
  let second = fixture.host_copies(SelectionKind::Clipboard, &[PLAIN]);
  // A newer host selection drops the older offer.
  assert_eq!(fixture.calls(), [Call::Discard(first)]);
  let old_source = fixture.nested.host_source(SelectionKind::Clipboard);
  fixture.nested.copy(SelectionKind::Clipboard, &[UTF8]);
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [
      Call::Discard(second),
      Call::Offer(1, SelectionKind::Clipboard, strings(&[UTF8]))
    ]
  );
  // A read of the replaced host source is closed, not sent.
  let mut reader =
    fixture
      .nested
      .read(SelectionKind::Clipboard, old_source.unwrap(), UTF8);
  fixture.sync();
  assert!(fixture.calls().is_empty());
  let mut data = Vec::new();
  reader.read_to_end(&mut data).unwrap();
  assert!(data.is_empty());
}

#[test]
fn a_nested_selection_that_goes_takes_the_host_selection_with_it() {
  let mut fixture = Fixture::both();
  fixture.nested.copy(SelectionKind::Primary, &[UTF8]);
  fixture.sync();
  fixture.nested.primary = None;
  fixture.sync();
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [
      Call::Offer(1, SelectionKind::Primary, strings(&[UTF8])),
      Call::Withdraw(1)
    ]
  );
  // A nested copy offering nothing is no selection either.
  fixture.nested.copy(SelectionKind::Primary, &[UTF8]);
  fixture.sync();
  fixture.nested.copy(SelectionKind::Primary, &[]);
  fixture.sync();
  assert_eq!(
    fixture.calls(),
    [
      Call::Offer(2, SelectionKind::Primary, strings(&[UTF8])),
      Call::Withdraw(2)
    ]
  );
  // Nested clearing the host's selection drops the host offer.
  let offer = fixture.host_copies(SelectionKind::Primary, &[UTF8]);
  fixture.nested.primary = None;
  fixture.sync();
  assert_eq!(fixture.calls(), [Call::Discard(offer)]);
}

#[test]
fn only_the_shared_selections_cross() {
  // As under `disable_primary_selection`, or a wlr-data-control host at
  // version 1.
  let mut fixture = Fixture::new(&[SelectionKind::Clipboard]);
  assert_eq!(fixture.bridge.kinds(), [SelectionKind::Clipboard]);
  let offer = fixture.host_copies(SelectionKind::Primary, &[UTF8]);
  assert_eq!(fixture.calls(), [Call::Discard(offer)]);
  assert!(fixture.nested.imported.is_empty());
  fixture.nested.copy(SelectionKind::Primary, &[UTF8]);
  let (_reader, writer) = std::io::pipe().unwrap();
  fixture.nested.reads.push(Read {
    kind: SelectionKind::Primary,
    source: 1,
    mime_type: UTF8.into(),
    fd: writer.into(),
  });
  fixture.sync();
  assert!(fixture.calls().is_empty());
}

#[test]
fn losing_the_host_device_lets_go_of_everything() {
  let mut fixture = Fixture::both();
  fixture.nested.copy(SelectionKind::Clipboard, &[UTF8]);
  let offer = fixture.host_copies(SelectionKind::Primary, &[UTF8]);
  fixture.sync();
  fixture.calls();
  fixture.bridge.clear(&mut fixture.host, &mut fixture.nested);
  assert_eq!(fixture.calls(), [Call::Withdraw(1), Call::Discard(offer)]);
  assert!(fixture.nested.primary.is_none());
  assert!(matches!(
    fixture.nested.clipboard,
    Some(Owner::Local { .. })
  ));
}

/// Inode and device of a descriptor, which a duplicate shares.
fn identity(fd: impl AsFd) -> (u64, u64) {
  let file = std::fs::File::from(fd.as_fd().try_clone_to_owned().unwrap());
  let metadata = file.metadata().unwrap();
  (metadata.dev(), metadata.ino())
}

#[test]
fn a_mebibyte_passes_writer_to_reader_while_the_bridge_never_waits() {
  let mut fixture = Fixture::both();
  let offer = fixture.host_copies(SelectionKind::Clipboard, &[UTF8]);
  let source = fixture
    .nested
    .host_source(SelectionKind::Clipboard)
    .unwrap();
  let reader = fixture.nested.read(SelectionKind::Clipboard, source, UTF8);
  let expected = identity(&reader);
  fixture.sync();
  assert_eq!(fixture.calls(), [Call::Receive(offer, UTF8.into())]);
  // The host gets the nested reader's own pipe, not one of Shingle's.
  let fd = fixture.host.fds.pop().unwrap();
  assert_eq!(identity(&fd), expected);
  // The host's writer stalls on the full pipe while the reader sleeps;
  // the bridge goes on with other work meanwhile.
  let payload = (0..1 << 20).map(|i| (i % 253) as u8).collect::<Vec<_>>();
  let sent = payload.clone();
  let writer = std::thread::spawn(move || {
    std::fs::File::from(fd).write_all(&sent).unwrap();
  });
  std::thread::sleep(Duration::from_millis(50));
  assert!(!writer.is_finished(), "the pipe should be full");
  let started = Instant::now();
  for _ in 0..100 {
    fixture.nested.copy(SelectionKind::Primary, &[UTF8]);
    fixture.sync();
  }
  assert!(started.elapsed() < Duration::from_millis(500));
  let mut data = Vec::new();
  let mut reader = reader;
  reader.read_to_end(&mut data).unwrap();
  writer.join().unwrap();
  assert_eq!(data, payload);
  // The other way: the host reader's descriptor reaches the nested source.
  let (host_reader, host_writer) = std::io::pipe().unwrap();
  let expected = identity(&host_reader);
  let current = fixture.host.next;
  fixture.event(HostEvent::Send {
    source: current,
    mime_type: UTF8.into(),
    fd: host_writer.into(),
  });
  let (kind, _, mime_type, fd) = fixture.nested.sent.pop().unwrap();
  assert_eq!((kind, mime_type.as_str()), (SelectionKind::Primary, UTF8));
  assert_eq!(identity(&fd), expected);
}

/// A compositor with a Wayland socket in the temporary directory, and the
/// socket.
fn compositor(config: shingle::Config) -> (Compositor, std::path::PathBuf) {
  use std::sync::atomic::{AtomicU32, Ordering};
  static NEXT: AtomicU32 = AtomicU32::new(0);
  let name = format!(
    "shingle-nested-share-test-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  );
  let path = std::env::temp_dir().join(name);
  let compositor = Compositor::with_socket_path(config, &path)
    .expect("the temporary directory accepts a Wayland socket");
  (compositor, path)
}

#[test]
fn primary_is_wanted_unless_disabled() {
  assert_eq!(
    wanted(true),
    [SelectionKind::Clipboard, SelectionKind::Primary]
  );
  assert_eq!(wanted(false), [SelectionKind::Clipboard]);
}

#[test]
fn the_bridge_connects_only_when_on_and_the_host_is_there() {
  let (mut host, socket) = compositor(shingle::Config::default());
  let (mut nested, _) = compositor(shingle::Config {
    disable_primary_selection: true,
    ..shingle::Config::default()
  });
  // Off, no Wayland host, and a host that is not there.
  assert!(share_with_host(&nested, false, Some(&socket)).is_none());
  assert!(share_with_host(&nested, true, None).is_none());
  let missing = socket.with_extension("missing");
  assert!(share_with_host(&nested, true, Some(&missing)).is_none());
  // On, with a host: only the clipboard, as primary is disabled.
  let mut clipboard =
    share_with_host(&nested, true, Some(&socket)).expect("connected");
  let deadline = Instant::now() + Duration::from_secs(10);
  while clipboard.shared().is_none() {
    assert!(Instant::now() < deadline, "the bridge did not bind");
    host.poll(Some(Duration::from_millis(5))).unwrap();
    clipboard.dispatch(&mut nested).unwrap();
    clipboard.sync(&mut nested).unwrap();
  }
  assert_eq!(clipboard.shared(), Some(vec![SelectionKind::Clipboard]));
  assert!(!clipboard.idle());
}
