//! Unit tests of `wp_linux_drm_syncobj_manager_v1`: the global, every
//! protocol error, and acquire and release through a fake timeline whose
//! eventfds are real, so that waits go through the event loop.

use std::{
  cell::{Cell, RefCell},
  io,
  os::fd::{AsFd, BorrowedFd, OwnedFd},
  rc::Rc,
  sync::{Arc, Mutex, PoisonError},
};

use wayland_client::{
  Proxy,
  protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_surface::WlSurface,
  },
};
use wayland_protocols::wp::{
  linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
  },
  linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1::{self, WpLinuxDrmSyncobjManagerV1},
    wp_linux_drm_syncobj_surface_v1::{self, WpLinuxDrmSyncobjSurfaceV1},
    wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
  },
};
use wayland_server::Resource as _;

use super::{CommitBuffer, SyncPoint, SyncobjDevice, Timeline, check_commit};
use crate::{
  Compositor, DmabufBuffer, DmabufFeedback, DmabufFormat, DmabufImporter,
  test_client::TestClient, tests::test_compositor,
};

const XR24: u32 = u32::from_le_bytes(*b"XR24");

/// A timeline that tests signal by hand. Its eventfds are real, and are
/// written when the test advances the timeline past their point.
#[derive(Default)]
struct FakeTimeline(Mutex<FakeState>);

#[derive(Default)]
struct FakeState {
  value: u64,
  waiters: Vec<(u64, OwnedFd)>,
  /// The points the compositor signalled, in order.
  signals: Vec<u64>,
  /// Makes `eventfd` fail.
  no_eventfd: bool,
}

impl FakeTimeline {
  fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
    self.0.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Signals every point up to `point`, as the client's GPU would.
  fn advance(&self, point: u64) {
    let mut state = self.state();
    state.value = state.value.max(point);
    let value = state.value;
    for (waiting, fd) in &state.waiters {
      if *waiting <= value {
        rustix::io::write(fd, &1u64.to_ne_bytes())
          .expect("the eventfd is written");
      }
    }
    state.waiters.retain(|(waiting, _)| *waiting > value);
  }

  fn signals(&self) -> Vec<u64> {
    self.state().signals.clone()
  }
}

impl Timeline for FakeTimeline {
  fn signalled(&self, point: u64) -> io::Result<bool> {
    Ok(self.state().value >= point)
  }

  fn eventfd(&self, point: u64) -> io::Result<OwnedFd> {
    let mut state = self.state();
    if state.no_eventfd {
      return Err(io::Error::other("the fake refuses eventfds"));
    }
    let fd = rustix::event::eventfd(
      0,
      rustix::event::EventfdFlags::CLOEXEC
        | rustix::event::EventfdFlags::NONBLOCK,
    )?;
    state.waiters.push((point, fd.try_clone()?));
    Ok(fd)
  }

  fn signal(&self, point: u64) -> io::Result<()> {
    self.state().signals.push(point);
    Ok(())
  }
}

/// Imports every fd as a new fake timeline, unless told to refuse.
#[derive(Clone, Default)]
struct FakeDevice {
  timelines: Rc<RefCell<Vec<Arc<FakeTimeline>>>>,
  refuse: Rc<Cell<bool>>,
}

impl SyncobjDevice for FakeDevice {
  fn import(&self, _fd: BorrowedFd<'_>) -> io::Result<Arc<dyn Timeline>> {
    if self.refuse.get() {
      return Err(io::Error::other("not a syncobj"));
    }
    let timeline = Arc::new(FakeTimeline::default());
    self.timelines.borrow_mut().push(Arc::clone(&timeline));
    Ok(timeline)
  }
}

/// Accepts every DMA-BUF.
struct AcceptAll;

impl DmabufImporter for AcceptAll {
  fn import(&mut self, _buffer: &Arc<DmabufBuffer>) -> Result<(), String> {
    Ok(())
  }
}

fn enable_dmabuf(compositor: &mut Compositor) {
  compositor
    .enable_dmabuf(
      DmabufFeedback {
        main_device: (226 << 8) | 128,
        formats: vec![DmabufFormat {
          format: XR24,
          modifier: 0,
          max_width: 4096,
          max_height: 4096,
        }],
      },
      Box::new(AcceptAll),
    )
    .expect("the feedback is offered");
}

fn enable_syncobj(
  compositor: &mut Compositor,
  device: FakeDevice,
) -> Result<(), String> {
  super::enable(
    &mut compositor.state,
    &compositor.display.handle(),
    &compositor.event_loop.handle(),
    Box::new(device),
  )
}

/// A memfd standing in for a DMA-BUF or a syncobj fd.
fn memfd(size: u64) -> OwnedFd {
  let fd = rustix::fs::memfd_create(
    "shingle-test-syncobj",
    rustix::fs::MemfdFlags::CLOEXEC,
  )
  .expect("memfd_create succeeds");
  rustix::fs::ftruncate(&fd, size).expect("ftruncate succeeds");
  fd
}

struct Fixture {
  compositor: Compositor,
  client: TestClient,
  device: FakeDevice,
  dmabuf: ZwpLinuxDmabufV1,
  manager: WpLinuxDrmSyncobjManagerV1,
  surface: WlSurface,
}

fn setup() -> Fixture {
  let (mut compositor, path) = test_compositor();
  enable_dmabuf(&mut compositor);
  let device = FakeDevice::default();
  enable_syncobj(&mut compositor, device.clone())
    .expect("the fake device is offered");
  let client = TestClient::connect(&mut compositor, &path);
  let dmabuf = client.bind::<ZwpLinuxDmabufV1>(5);
  let manager = client.bind::<WpLinuxDrmSyncobjManagerV1>(1);
  let surface = client
    .bind::<WlCompositor>(4)
    .create_surface(&client.handle, ());
  Fixture {
    compositor,
    client,
    device,
    dmabuf,
    manager,
    surface,
  }
}

impl Fixture {
  /// A 16 by 16 XR24 DMA-BUF of `fd`.
  fn dmabuf_of(&mut self, fd: &OwnedFd) -> WlBuffer {
    let params = self.dmabuf.create_params(&self.client.handle, ());
    params.add(fd.as_fd(), 0, 0, 64, 0, 0);
    let buffer = params.create_immed(
      16,
      16,
      XR24,
      zwp_linux_buffer_params_v1::Flags::empty(),
      &self.client.handle,
      (),
    );
    params.destroy();
    buffer
  }

  /// A 16 by 16 DMA-BUF whose implicit fences have signalled.
  fn dmabuf(&mut self) -> WlBuffer {
    self.dmabuf_of(&memfd(16 * 16 * 4))
  }

  /// A new timeline, with the fake the compositor imported it as.
  fn timeline(&mut self) -> (WpLinuxDrmSyncobjTimelineV1, Arc<FakeTimeline>) {
    let fd = memfd(0);
    let timeline =
      self
        .manager
        .import_timeline(fd.as_fd(), &self.client.handle, ());
    self.client.roundtrip(&mut self.compositor);
    let fake = self
      .device
      .timelines
      .borrow()
      .last()
      .cloned()
      .expect("the timeline was imported");
    (timeline, fake)
  }

  fn syncobj_surface(&mut self) -> WpLinuxDrmSyncobjSurfaceV1 {
    self
      .manager
      .get_surface(&self.surface, &self.client.handle, ())
  }

  fn error(&mut self) -> Option<(String, u32)> {
    self
      .client
      .protocol_error(&mut self.compositor)
      .map(|error| (error.object_interface, error.code))
  }

  /// The width of the buffer the surface shows.
  fn shown(&self) -> Option<u32> {
    let object = self.object();
    self.compositor.state.surfaces[&object]
      .buffer
      .as_ref()
      .map(|buffer| buffer.width)
  }

  fn object(&self) -> wayland_server::backend::ObjectId {
    self
      .compositor
      .state
      .surfaces
      .values()
      .find(|candidate| {
        candidate.resource.id().protocol_id() == self.surface.id().protocol_id()
      })
      .map(|surface| surface.resource.id())
      .expect("the surface exists")
  }

  /// Lets the compositor drop the committed events, whose snapshots lock
  /// their buffers, and flush the releases.
  fn settle(&mut self) {
    self.client.roundtrip(&mut self.compositor);
    self.compositor.drain_events();
    self.client.roundtrip(&mut self.compositor);
  }
}

/// Sets both points, attaches `buffer` and commits.
fn commit_with(
  syncobj: &WpLinuxDrmSyncobjSurfaceV1,
  surface: &WlSurface,
  buffer: &WlBuffer,
  acquire: (&WpLinuxDrmSyncobjTimelineV1, u64),
  release: (&WpLinuxDrmSyncobjTimelineV1, u64),
) {
  syncobj.set_acquire_point(
    acquire.0,
    (acquire.1 >> 32) as u32,
    acquire.1 as u32,
  );
  syncobj.set_release_point(
    release.0,
    (release.1 >> 32) as u32,
    release.1 as u32,
  );
  surface.attach(Some(buffer), 0, 0);
  surface.commit();
}

const SURFACE: &str = "wp_linux_drm_syncobj_surface_v1";

fn code(error: wp_linux_drm_syncobj_surface_v1::Error) -> u32 {
  error as u32
}

#[test]
fn the_global_needs_linux_dmabuf_and_is_version_1() {
  let (mut compositor, path) = test_compositor();
  assert!(enable_syncobj(&mut compositor, FakeDevice::default()).is_err());
  assert!(!compositor.syncobj_enabled());
  // Without linux-dmabuf the real call refuses too, before any device.
  assert!(compositor.enable_syncobj().is_err());
  enable_dmabuf(&mut compositor);
  enable_syncobj(&mut compositor, FakeDevice::default())
    .expect("the fake device is offered");
  // Enabling again keeps the one global.
  enable_syncobj(&mut compositor, FakeDevice::default())
    .expect("the fake device is offered again");
  assert!(compositor.syncobj_enabled());
  let client = TestClient::connect(&mut compositor, &path);
  let globals = client
    .events
    .globals
    .iter()
    .filter(|(_, interface, _)| interface == "wp_linux_drm_syncobj_manager_v1")
    .map(|(_, _, version)| *version)
    .collect::<Vec<_>>();
  assert_eq!(globals, [1]);
}

#[test]
fn a_second_syncobj_object_for_a_surface_is_surface_exists() {
  let mut fixture = setup();
  let _first = fixture.syncobj_surface();
  let _second = fixture.syncobj_surface();
  assert_eq!(
    fixture.error(),
    Some((
      "wp_linux_drm_syncobj_manager_v1".into(),
      wp_linux_drm_syncobj_manager_v1::Error::SurfaceExists as u32
    ))
  );
}

#[test]
fn a_new_syncobj_object_may_follow_a_destroyed_one() {
  let mut fixture = setup();
  fixture.syncobj_surface().destroy();
  let _second = fixture.syncobj_surface();
  fixture.client.roundtrip(&mut fixture.compositor);
  assert_eq!(fixture.error(), None);
}

#[test]
fn a_timeline_the_device_refuses_is_invalid_timeline() {
  let mut fixture = setup();
  fixture.device.refuse.set(true);
  let fd = memfd(0);
  let _timeline =
    fixture
      .manager
      .import_timeline(fd.as_fd(), &fixture.client.handle, ());
  assert_eq!(
    fixture.error(),
    Some((
      "wp_linux_drm_syncobj_manager_v1".into(),
      wp_linux_drm_syncobj_manager_v1::Error::InvalidTimeline as u32
    ))
  );
}

/// The commit rules, in wlroots' order.
#[test]
fn the_commit_rules() {
  use wp_linux_drm_syncobj_surface_v1::Error;
  let one = Arc::new(FakeTimeline::default()) as Arc<dyn Timeline>;
  let other = Arc::new(FakeTimeline::default()) as Arc<dyn Timeline>;
  let at = |timeline: &Arc<dyn Timeline>, point| SyncPoint {
    timeline: Arc::clone(timeline),
    point,
  };
  let rule = |buffer, acquire: Option<&SyncPoint>, release| {
    check_commit(buffer, acquire, release)
      .err()
      .map(|(error, _)| error as u32)
  };
  let (a1, a2, b1) = (at(&one, 1), at(&one, 2), at(&other, 1));
  let none = CommitBuffer::None;
  let dmabuf = CommitBuffer::Dmabuf;
  assert_eq!(rule(none, None, None), None);
  assert_eq!(rule(none, Some(&a1), None), Some(code(Error::NoBuffer)));
  assert_eq!(rule(none, None, Some(&a1)), Some(code(Error::NoBuffer)));
  assert_eq!(
    rule(dmabuf, None, Some(&a2)),
    Some(code(Error::NoAcquirePoint))
  );
  assert_eq!(
    rule(dmabuf, Some(&a1), None),
    Some(code(Error::NoReleasePoint))
  );
  assert_eq!(rule(dmabuf, Some(&a1), Some(&a2)), None);
  assert_eq!(
    rule(dmabuf, Some(&a2), Some(&a2)),
    Some(code(Error::ConflictingPoints))
  );
  assert_eq!(
    rule(dmabuf, Some(&a2), Some(&a1)),
    Some(code(Error::ConflictingPoints))
  );
  // Points on different timelines are not ordered.
  assert_eq!(rule(dmabuf, Some(&a2), Some(&b1)), None);
  assert_eq!(
    rule(CommitBuffer::Other, Some(&a1), Some(&a2)),
    Some(code(Error::UnsupportedBuffer))
  );
}

/// The surface error a commit made with `prepare` raises.
fn commit_error(
  prepare: impl FnOnce(&mut Fixture, &WpLinuxDrmSyncobjSurfaceV1),
) -> Option<(String, u32)> {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  prepare(&mut fixture, &syncobj);
  fixture.surface.commit();
  fixture.error()
}

#[test]
fn points_without_a_buffer_are_no_buffer() {
  use wp_linux_drm_syncobj_surface_v1::Error;
  let acquire = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_acquire_point(&timeline, 0, 1);
  });
  assert_eq!(acquire, Some((SURFACE.into(), code(Error::NoBuffer))));
  let release = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_release_point(&timeline, 0, 1);
  });
  assert_eq!(release, Some((SURFACE.into(), code(Error::NoBuffer))));
  // A null attach is no buffer either.
  let null = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_acquire_point(&timeline, 0, 1);
    syncobj.set_release_point(&timeline, 0, 2);
    fixture.surface.attach(None, 0, 0);
  });
  assert_eq!(null, Some((SURFACE.into(), code(Error::NoBuffer))));
}

#[test]
fn a_buffer_without_points_is_no_acquire_or_no_release_point() {
  use wp_linux_drm_syncobj_surface_v1::Error;
  let acquire = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_release_point(&timeline, 0, 2);
    let buffer = fixture.dmabuf();
    fixture.surface.attach(Some(&buffer), 0, 0);
  });
  assert_eq!(acquire, Some((SURFACE.into(), code(Error::NoAcquirePoint))));
  let release = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_acquire_point(&timeline, 0, 1);
    let buffer = fixture.dmabuf();
    fixture.surface.attach(Some(&buffer), 0, 0);
  });
  assert_eq!(release, Some((SURFACE.into(), code(Error::NoReleasePoint))));
}

#[test]
fn points_out_of_order_on_one_timeline_are_conflicting_points() {
  use wp_linux_drm_syncobj_surface_v1::Error;
  let error = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_acquire_point(&timeline, 0, 3);
    syncobj.set_release_point(&timeline, 0, 3);
    let buffer = fixture.dmabuf();
    fixture.surface.attach(Some(&buffer), 0, 0);
  });
  assert_eq!(
    error,
    Some((SURFACE.into(), code(Error::ConflictingPoints)))
  );
}

#[test]
fn an_shm_buffer_is_unsupported_buffer() {
  use wp_linux_drm_syncobj_surface_v1::Error;
  let error = commit_error(|fixture, syncobj| {
    let (timeline, _) = fixture.timeline();
    syncobj.set_acquire_point(&timeline, 0, 1);
    syncobj.set_release_point(&timeline, 0, 2);
    let buffer = fixture.client.shm_buffer(16, 16);
    fixture.surface.attach(Some(&buffer), 0, 0);
  });
  assert_eq!(
    error,
    Some((SURFACE.into(), code(Error::UnsupportedBuffer)))
  );
}

#[test]
fn points_on_a_destroyed_surface_are_no_surface() {
  use wp_linux_drm_syncobj_surface_v1::Error;
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (timeline, _) = fixture.timeline();
  fixture.surface.destroy();
  fixture.client.roundtrip(&mut fixture.compositor);
  syncobj.set_release_point(&timeline, 0, 1);
  assert_eq!(
    fixture.error(),
    Some((SURFACE.into(), code(Error::NoSurface)))
  );
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (timeline, _) = fixture.timeline();
  fixture.surface.destroy();
  syncobj.set_acquire_point(&timeline, 0, 1);
  assert_eq!(
    fixture.error(),
    Some((SURFACE.into(), code(Error::NoSurface)))
  );
}

#[test]
fn destroying_the_syncobj_object_discards_its_points() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (timeline, _) = fixture.timeline();
  syncobj.set_acquire_point(&timeline, 0, 1);
  syncobj.destroy();
  // Without the object the surface is back to implicit sync: no points,
  // no rules, and any buffer goes.
  let buffer = fixture.client.shm_buffer(16, 16);
  fixture.surface.attach(Some(&buffer), 0, 0);
  fixture.surface.commit();
  fixture.client.roundtrip(&mut fixture.compositor);
  assert_eq!(fixture.error(), None);
  assert_eq!(fixture.shown(), Some(16));
}

#[test]
fn a_signalled_acquire_point_applies_at_once() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (acquire, acquire_fake) = fixture.timeline();
  let (release, _) = fixture.timeline();
  acquire_fake.advance(1);
  let buffer = fixture.dmabuf();
  commit_with(
    &syncobj,
    &fixture.surface,
    &buffer,
    (&acquire, 1),
    (&release, 1),
  );
  fixture.client.roundtrip(&mut fixture.compositor);
  assert_eq!(fixture.error(), None);
  assert_eq!(fixture.shown(), Some(16));
  assert_eq!(
    fixture
      .compositor
      .state
      .commit_queue
      .waiting(&fixture.object()),
    0
  );
}

#[test]
fn a_commit_waits_for_its_acquire_point_and_later_commits_wait_behind() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (acquire, acquire_fake) = fixture.timeline();
  let (release, release_fake) = fixture.timeline();
  let first = fixture.dmabuf();
  let second = fixture.dmabuf();
  // Points above 32 bits, to check that both halves arrive.
  let base = 1 << 32;
  commit_with(
    &syncobj,
    &fixture.surface,
    &first,
    (&acquire, base + 1),
    (&release, 1),
  );
  // The second is ready at once but waits behind the first.
  acquire_fake.advance(0);
  let (ready, ready_fake) = fixture.timeline();
  ready_fake.advance(7);
  commit_with(
    &syncobj,
    &fixture.surface,
    &second,
    (&ready, 7),
    (&release, 2),
  );
  fixture.client.roundtrip(&mut fixture.compositor);
  let object = fixture.object();
  assert_eq!(fixture.shown(), None);
  assert_eq!(fixture.compositor.state.commit_queue.waiting(&object), 2);
  acquire_fake.advance(base);
  fixture.client.roundtrip(&mut fixture.compositor);
  assert_eq!(fixture.compositor.state.commit_queue.waiting(&object), 2);
  // The eventfd fires in the event loop and both apply, in order.
  acquire_fake.advance(base + 1);
  fixture.settle();
  assert_eq!(fixture.compositor.state.commit_queue.waiting(&object), 0);
  assert_eq!(fixture.shown(), Some(16));
  // The first was replaced: released, with its release point signalled;
  // the second is shown and keeps its point.
  assert_eq!(fixture.client.events.buffer_releases, [first.id()]);
  assert_eq!(release_fake.signals(), [1]);
}

#[test]
fn every_release_point_of_a_buffer_is_signalled_when_it_is_released() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (acquire, acquire_fake) = fixture.timeline();
  let (release, release_fake) = fixture.timeline();
  acquire_fake.advance(10);
  let buffer = fixture.dmabuf();
  let other = fixture.dmabuf();
  // The same buffer twice, as a client redrawing it in place would.
  for (acquire_point, release_point) in [(1, 1), (2, 2)] {
    commit_with(
      &syncobj,
      &fixture.surface,
      &buffer,
      (&acquire, acquire_point),
      (&release, release_point),
    );
  }
  fixture.settle();
  assert!(release_fake.signals().is_empty());
  assert!(fixture.client.events.buffer_releases.is_empty());
  commit_with(
    &syncobj,
    &fixture.surface,
    &other,
    (&acquire, 3),
    (&release, 9),
  );
  fixture.settle();
  assert_eq!(release_fake.signals(), [1, 2]);
  assert_eq!(fixture.client.events.buffer_releases, [buffer.id()]);
}

#[test]
fn a_stalled_surface_releases_its_buffers_when_destroyed() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (acquire, _never) = fixture.timeline();
  let (release, release_fake) = fixture.timeline();
  let buffers = [fixture.dmabuf(), fixture.dmabuf()];
  for (index, buffer) in buffers.iter().enumerate() {
    let point = index as u64 + 1;
    commit_with(
      &syncobj,
      &fixture.surface,
      buffer,
      (&acquire, point),
      (&release, point),
    );
  }
  // Another surface of the client is not held up.
  let other = fixture
    .client
    .bind::<WlCompositor>(4)
    .create_surface(&fixture.client.handle, ());
  let shm = fixture.client.shm_buffer(8, 8);
  other.attach(Some(&shm), 0, 0);
  other.commit();
  fixture.settle();
  let object = fixture.object();
  assert_eq!(fixture.compositor.state.commit_queue.waiting(&object), 2);
  assert_eq!(fixture.client.events.buffer_releases, [shm.id()]);
  syncobj.destroy();
  fixture.surface.destroy();
  fixture.settle();
  assert_eq!(fixture.compositor.state.commit_queue.waiting(&object), 0);
  let mut released = fixture.client.events.buffer_releases.clone();
  released.sort_by_key(|id| id.protocol_id());
  let mut expected = vec![shm.id(), buffers[0].id(), buffers[1].id()];
  expected.sort_by_key(|id| id.protocol_id());
  assert_eq!(released, expected);
  let mut signals = release_fake.signals();
  signals.sort_unstable();
  assert_eq!(signals, [1, 2]);
}

#[test]
fn an_acquire_point_replaces_the_implicit_fence_wait() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (acquire, acquire_fake) = fixture.timeline();
  let (release, _) = fixture.timeline();
  acquire_fake.advance(1);
  // A pipe never polls readable: an implicit wait would block forever.
  let (fence, _signal) =
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
      .expect("pipe succeeds");
  let buffer = fixture.dmabuf_of(&fence);
  commit_with(
    &syncobj,
    &fixture.surface,
    &buffer,
    (&acquire, 1),
    (&release, 2),
  );
  fixture.client.roundtrip(&mut fixture.compositor);
  assert_eq!(fixture.shown(), Some(16));
}

#[test]
fn a_wait_that_cannot_be_set_up_is_no_memory() {
  let mut fixture = setup();
  let syncobj = fixture.syncobj_surface();
  let (acquire, acquire_fake) = fixture.timeline();
  let (release, _) = fixture.timeline();
  acquire_fake.state().no_eventfd = true;
  let buffer = fixture.dmabuf();
  commit_with(
    &syncobj,
    &fixture.surface,
    &buffer,
    (&acquire, 1),
    (&release, 2),
  );
  // The Rust client backend reports the display error as a protocol
  // error; libwayland's, which `cargo test --workspace` links through
  // winit, as the fatal ENOMEM that no_memory maps to.
  let error = fixture.error();
  let fatal = fixture.client.connection_errno();
  assert!(
    error == Some(("wl_display".into(), 2))
      || (error.is_none() && fatal == Some(rustix::io::Errno::NOMEM)),
    "{error:?}, {fatal:?}"
  );
}
