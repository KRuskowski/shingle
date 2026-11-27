//! What the outputs show while a lock starts, holds and fails: no client
//! from the moment a lock client asks for the lock, and black until the
//! lock's surface for an output has committed a full opaque frame
//! (docs/design/lock-screen.md).

use wayland_client::protocol::{
  wl_compositor::WlCompositor, wl_output::WlOutput, wl_surface::WlSurface,
};
use wayland_protocols::ext::session_lock::v1::client::{
  ext_session_lock_manager_v1::ExtSessionLockManagerV1,
  ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
  ext_session_lock_v1::ExtSessionLockV1,
};

use crate::{
  Compositor, Event, LockState, OutputHandle, SurfaceRole,
  test_client::TestClient, tests::test_compositor,
};

/// The roles drawn on the one output.
fn drawn(compositor: &Compositor) -> Vec<SurfaceRole> {
  compositor
    .visible_surfaces_on(&OutputHandle::new("HEADLESS-1"))
    .into_iter()
    .map(|snapshot| snapshot.role)
    .collect()
}

/// A lock client that asked for the lock and has a configured lock
/// surface on the one output, with no buffer yet.
struct Locker {
  client: TestClient,
  _lock: ExtSessionLockV1,
  surface: WlSurface,
  _lock_surface: ExtSessionLockSurfaceV1,
  size: (i32, i32),
}

impl Locker {
  fn lock(compositor: &mut Compositor, path: &std::path::Path) -> Self {
    let mut client = TestClient::connect(compositor, path);
    let output = client.bind::<WlOutput>(4);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client.bind::<ExtSessionLockManagerV1>(1);
    let lock = manager.lock(&client.handle, ());
    let surface = wl_compositor.create_surface(&client.handle, ());
    let lock_surface =
      lock.get_lock_surface(&surface, &output, &client.handle, ());
    client.roundtrip(compositor);
    let (_, serial, width, height) = *client
      .events
      .lock_configures
      .last()
      .expect("the lock surface is configured");
    lock_surface.ack_configure(serial);
    Self {
      client,
      _lock: lock,
      surface,
      _lock_surface: lock_surface,
      size: (width as i32, height as i32),
    }
  }

  /// Commits a transparent ARGB buffer of the output's size.
  fn commit_transparent(&mut self, compositor: &mut Compositor) {
    let (width, height) = self.size;
    self
      .client
      .attach_buffer(compositor, &self.surface, width, height);
  }

  /// Commits an opaque XRGB buffer of the output's size.
  fn commit_opaque(&mut self, compositor: &mut Compositor) {
    let (width, height) = self.size;
    let buffer = self.client.opaque_buffer(width, height);
    self.surface.attach(Some(&buffer), 0, 0);
    self.surface.commit();
    self.client.roundtrip(compositor);
  }
}

/// Dispatches until the compositor has seen a client go.
fn settle(compositor: &mut Compositor) {
  for _ in 0..10 {
    compositor
      .dispatch_pending()
      .expect("the compositor dispatches");
  }
}

#[test]
fn no_client_is_drawn_from_the_lock_request_until_unlocked() {
  let (mut compositor, path) = test_compositor();
  let mut app = TestClient::connect(&mut compositor, &path);
  let toplevel = app.toplevel(&mut compositor, "app");
  app.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
  assert!(drawn(&compositor).contains(&SurfaceRole::Toplevel));

  let mut locker = Locker::lock(&mut compositor, &path);
  assert_eq!(compositor.lock_state(), LockState::Pending);
  assert!(compositor.lock_hides_clients());
  assert_eq!(drawn(&compositor), [], "black before any lock frame");
  // A frame that does not cover the output leaves it black and the
  // session pending.
  locker.commit_transparent(&mut compositor);
  assert_eq!(drawn(&compositor), []);
  assert_eq!(compositor.lock_state(), LockState::Pending);
  locker.commit_opaque(&mut compositor);
  assert_eq!(compositor.lock_state(), LockState::Locked);
  assert_eq!(drawn(&compositor), [SurfaceRole::SessionLock]);
  // Once shown, later frames are drawn whatever they hold.
  locker.commit_transparent(&mut compositor);
  assert_eq!(drawn(&compositor), [SurfaceRole::SessionLock]);
  assert!(
    compositor
      .cursor_on(&OutputHandle::new("HEADLESS-1"))
      .is_none()
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_crashed_lock_client_leaves_the_outputs_black() {
  let (mut compositor, path) = test_compositor();
  let mut app = TestClient::connect(&mut compositor, &path);
  let toplevel = app.toplevel(&mut compositor, "app");
  app.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
  let mut locker = Locker::lock(&mut compositor, &path);
  locker.commit_opaque(&mut compositor);
  assert_eq!(compositor.lock_state(), LockState::Locked);
  drop(locker);
  settle(&mut compositor);
  assert_eq!(compositor.lock_state(), LockState::Failed);
  assert_eq!(drawn(&compositor), []);
  // The lock binding starts a new client and keeps the outputs black.
  compositor.drain_events();
  compositor.request_lock();
  assert!(
    compositor
      .drain_events()
      .iter()
      .any(|event| matches!(event, Event::StartLockClient))
  );
  assert_eq!(compositor.lock_state(), LockState::Failed);
  assert_eq!(drawn(&compositor), []);
  // A new lock client takes over, as Wayfire's session-lock allows after
  // a crash, and black stays until its frame.
  let mut second = Locker::lock(&mut compositor, &path);
  assert_eq!(compositor.lock_state(), LockState::Pending);
  assert_eq!(drawn(&compositor), []);
  second.commit_opaque(&mut compositor);
  assert_eq!(compositor.lock_state(), LockState::Locked);
  assert_eq!(drawn(&compositor), [SurfaceRole::SessionLock]);
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_lock_the_compositor_starts_shows_the_clients_until_the_client_asks() {
  let (mut compositor, path) = test_compositor();
  let mut app = TestClient::connect(&mut compositor, &path);
  let toplevel = app.toplevel(&mut compositor, "app");
  app.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
  compositor.request_lock();
  assert_eq!(compositor.lock_state(), LockState::Starting);
  assert!(compositor.is_locked());
  assert!(!compositor.lock_hides_clients());
  // Its lock client can still capture the clients for its background.
  assert!(drawn(&compositor).contains(&SurfaceRole::Toplevel));
  let _locker = Locker::lock(&mut compositor, &path);
  assert_eq!(compositor.lock_state(), LockState::Pending);
  assert_eq!(drawn(&compositor), []);
  let _ = std::fs::remove_file(path);
}
