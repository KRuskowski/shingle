//! Buffer locks: when a client gets its `wl_buffer` back
//! (docs/design/dmabuf.md, section 4).
//!
//! Every holder of a committed or attached buffer, whether pending,
//! cached, deferred or queued state, holds a [`BufferLock`]. The locks of
//! one `wl_buffer` share a count, so a buffer attached twice is released
//! once both are gone, as wlroots' `wlr_buffer_lock` counts. When the last
//! lock drops, the buffer joins the [`ReleaseQueue`], which sends
//! `wl_buffer.release` at the end of the compositor's turn. Replacing,
//! merging or dropping state therefore releases what it held, whichever
//! path did it.

use std::{
  fmt,
  sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicUsize, Ordering},
  },
};

use wayland_server::{Resource, protocol::wl_buffer::WlBuffer};

/// Buffers whose last lock dropped, waiting for `wl_buffer.release`.
#[derive(Clone, Default)]
pub(crate) struct ReleaseQueue(Arc<Mutex<Vec<WlBuffer>>>);

impl ReleaseQueue {
  fn push(&self, buffer: WlBuffer) {
    self
      .0
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .push(buffer);
  }

  /// Sends `release` to every queued buffer that still exists, in the
  /// order their last locks dropped. A destroyed buffer gets nothing.
  pub(crate) fn flush(&self) {
    let buffers = std::mem::take(
      &mut *self.0.lock().unwrap_or_else(PoisonError::into_inner),
    );
    for buffer in buffers {
      if buffer.is_alive() {
        buffer.release();
      }
    }
  }
}

/// The lock count of one `wl_buffer`, shared by all its attachments.
pub(crate) struct BufferLocks {
  resource: WlBuffer,
  count: AtomicUsize,
  queue: ReleaseQueue,
}

impl BufferLocks {
  /// The count of `resource`, which releases into `queue`.
  pub(crate) fn new(resource: WlBuffer, queue: ReleaseQueue) -> Arc<Self> {
    Arc::new(Self {
      resource,
      count: AtomicUsize::new(0),
      queue,
    })
  }

  /// Takes a lock; the buffer stays with the compositor until it drops.
  pub(crate) fn lock(self: &Arc<Self>) -> BufferLock {
    self.count.fetch_add(1, Ordering::AcqRel);
    BufferLock(Arc::clone(self))
  }

  /// How many locks are held.
  #[cfg(test)]
  pub(crate) fn count(&self) -> usize {
    self.count.load(Ordering::Acquire)
  }
}

/// Keeps a `wl_buffer` from going back to its client. Clones are further
/// locks; the buffer is released when the last one drops.
pub(crate) struct BufferLock(Arc<BufferLocks>);

impl Clone for BufferLock {
  fn clone(&self) -> Self {
    self.0.lock()
  }
}

impl Drop for BufferLock {
  fn drop(&mut self) {
    if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
      self.0.queue.push(self.0.resource.clone());
    }
  }
}

impl fmt::Debug for BufferLock {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "BufferLock({})", self.0.resource.id())
  }
}

#[cfg(test)]
mod tests {
  use wayland_client::{
    Proxy,
    protocol::{
      wl_compositor::WlCompositor, wl_subcompositor::WlSubcompositor,
    },
  };
  use wayland_protocols::wp::{
    commit_timing::v1::client::wp_commit_timing_manager_v1,
    fifo::v1::client::wp_fifo_manager_v1::WpFifoManagerV1,
    single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1,
  };
  use wayland_server::Resource as _;

  use crate::{test_client::TestClient, tests::test_compositor};

  #[test]
  fn a_buffer_replaced_behind_a_fifo_barrier_is_released() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "fifo");
    client.attach_buffer(&mut compositor, &toplevel.surface, 100, 80);
    let manager = client.bind::<WpFifoManagerV1>(1);
    let fifo = manager.get_fifo(&toplevel.surface, &client.handle, ());
    let buffers = [0; 3].map(|_| client.shm_buffer(100, 80));
    client.roundtrip(&mut compositor);
    client.events.buffer_releases.clear();
    let surface = &toplevel.surface;
    surface.attach(Some(&buffers[0]), 0, 0);
    fifo.set_barrier();
    surface.commit();
    for buffer in &buffers[1..] {
      surface.attach(Some(buffer), 0, 0);
      fifo.wait_barrier();
      surface.commit();
    }
    client.roundtrip(&mut compositor);
    // The first commit applied; the third replaced the second while both
    // waited for the barrier.
    assert!(compositor.needs_frame());
    assert_eq!(
      client.events.buffer_releases,
      [buffers[0].id(), buffers[1].id()]
    );
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases.len(), 3);
    assert_eq!(client.events.buffer_releases[2], buffers[2].id());
  }

  #[test]
  fn a_buffer_replaced_while_waiting_for_its_timestamp_is_released() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager =
      client.bind::<wp_commit_timing_manager_v1::WpCommitTimingManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let timer = manager.get_timer(&surface, &client.handle, ());
    let first = client.shm_buffer(4, 4);
    let second = client.shm_buffer(4, 4);
    let later =
      crate::clock::monotonic_now() + std::time::Duration::from_secs(60);
    let seconds = later.as_secs();
    surface.attach(Some(&first), 0, 0);
    timer.set_timestamp((seconds >> 32) as u32, seconds as u32, 0);
    surface.commit();
    surface.attach(Some(&second), 0, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert!(compositor.needs_frame());
    assert_eq!(client.events.buffer_releases, [first.id()]);
  }

  #[test]
  fn a_buffer_replaced_in_a_synchronized_subsurface_cache_is_released() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "parent");
    client.attach_buffer(&mut compositor, &toplevel.surface, 100, 80);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let subcompositor = client.bind::<WlSubcompositor>(1);
    let child = wl_compositor.create_surface(&client.handle, ());
    let _subsurface = subcompositor.get_subsurface(
      &child,
      &toplevel.surface,
      &client.handle,
      (),
    );
    let first = client.shm_buffer(10, 10);
    let second = client.shm_buffer(10, 10);
    client.roundtrip(&mut compositor);
    client.events.buffer_releases.clear();
    child.attach(Some(&first), 0, 0);
    child.commit();
    child.attach(Some(&second), 0, 0);
    child.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [first.id()]);
    toplevel.surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [first.id(), second.id()]);
  }

  #[test]
  fn destroying_a_surface_releases_its_waiting_buffer() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager =
      client.bind::<wp_commit_timing_manager_v1::WpCommitTimingManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let timer = manager.get_timer(&surface, &client.handle, ());
    let buffer = client.shm_buffer(4, 4);
    let later =
      crate::clock::monotonic_now() + std::time::Duration::from_secs(60);
    let seconds = later.as_secs();
    surface.attach(Some(&buffer), 0, 0);
    timer.set_timestamp((seconds >> 32) as u32, seconds as u32, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert!(client.events.buffer_releases.is_empty());
    timer.destroy();
    surface.destroy();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [buffer.id()]);
  }

  /// A mapped toplevel with a FIFO object, its releases so far cleared.
  fn fifo_toplevel(
    compositor: &mut crate::Compositor,
    client: &mut TestClient,
  ) -> (
    crate::test_client::Toplevel,
    wayland_protocols::wp::fifo::v1::client::wp_fifo_v1::WpFifoV1,
  ) {
    let toplevel = client.toplevel(compositor, "fifo");
    client.attach_buffer(compositor, &toplevel.surface, 100, 80);
    let manager = client.bind::<WpFifoManagerV1>(1);
    let fifo = manager.get_fifo(&toplevel.surface, &client.handle, ());
    client.roundtrip(compositor);
    client.events.buffer_releases.clear();
    (toplevel, fifo)
  }

  /// The size of the buffer `surface` shows.
  fn shown_size(
    compositor: &crate::Compositor,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
  ) -> Option<(u32, u32)> {
    compositor
      .state
      .surfaces
      .values()
      .find(|candidate| {
        candidate.resource.id().protocol_id() == surface.id().protocol_id()
      })
      .and_then(|surface| surface.buffer.as_ref())
      .map(|buffer| (buffer.width, buffer.height))
  }

  #[test]
  fn shm_and_single_pixel_buffers_are_released_when_their_commit_applies() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client
      .bind::<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1>(
        1,
      );
    let surface = wl_compositor.create_surface(&client.handle, ());
    let shm = client.shm_buffer(4, 4);
    let pixel = manager.create_u32_rgba_buffer(
      u32::MAX,
      0,
      0,
      u32::MAX,
      &client.handle,
      (),
    );
    surface.attach(Some(&shm), 0, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [shm.id()]);
    surface.attach(Some(&pixel), 0, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [shm.id(), pixel.id()]);
    assert_eq!(shown_size(&compositor, &surface), Some((1, 1)));
  }

  #[test]
  fn a_buffer_attached_again_before_its_commit_is_not_released() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let first = client.shm_buffer(4, 4);
    let second = client.shm_buffer(8, 8);
    surface.attach(Some(&first), 0, 0);
    surface.attach(Some(&second), 0, 0);
    client.roundtrip(&mut compositor);
    // Replaced before its commit, as wlroots unlocks a pending buffer.
    assert_eq!(client.events.buffer_releases, [first.id()]);
    surface.attach(Some(&second), 0, 0);
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [first.id()]);
    let attachment = &compositor.state.buffers[&compositor
      .state
      .buffers
      .keys()
      .find(|id| id.protocol_id() == second.id().protocol_id())
      .cloned()
      .expect("the second buffer is known")];
    assert_eq!(attachment.lock_count(), 1);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [first.id(), second.id()]);
    assert_eq!(shown_size(&compositor, &surface), Some((8, 8)));
  }

  #[test]
  fn one_buffer_committed_twice_behind_a_barrier_is_released_once() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let (toplevel, fifo) = fifo_toplevel(&mut compositor, &mut client);
    let surface = &toplevel.surface;
    let barrier = client.shm_buffer(100, 80);
    let buffer = client.shm_buffer(100, 80);
    surface.attach(Some(&barrier), 0, 0);
    fifo.set_barrier();
    surface.commit();
    for _ in 0..2 {
      surface.attach(Some(&buffer), 0, 0);
      fifo.wait_barrier();
      surface.commit();
    }
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [barrier.id()]);
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [barrier.id(), buffer.id()]);
  }

  #[test]
  fn a_waiting_buffer_destroyed_by_its_client_gets_no_release() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let (toplevel, fifo) = fifo_toplevel(&mut compositor, &mut client);
    let surface = &toplevel.surface;
    let barrier = client.shm_buffer(100, 80);
    let buffer = client.shm_buffer(50, 40);
    surface.attach(Some(&barrier), 0, 0);
    fifo.set_barrier();
    surface.commit();
    surface.attach(Some(&buffer), 0, 0);
    fifo.wait_barrier();
    surface.commit();
    buffer.destroy();
    client.roundtrip(&mut compositor);
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [barrier.id()]);
    // The pixels were taken from the pool, which outlives the buffer.
    assert_eq!(shown_size(&compositor, surface), Some((50, 40)));
  }

  #[test]
  fn a_frame_keeps_what_the_client_attached_since_the_deferred_commit() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let (toplevel, fifo) = fifo_toplevel(&mut compositor, &mut client);
    let surface = &toplevel.surface;
    let barrier = client.shm_buffer(100, 80);
    let waiting = client.shm_buffer(100, 80);
    let next = client.shm_buffer(50, 40);
    surface.attach(Some(&barrier), 0, 0);
    fifo.set_barrier();
    surface.commit();
    surface.attach(Some(&waiting), 0, 0);
    fifo.wait_barrier();
    surface.commit();
    surface.attach(Some(&next), 0, 0);
    client.roundtrip(&mut compositor);
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.buffer_releases, [barrier.id(), waiting.id()]);
    assert_eq!(shown_size(&compositor, surface), Some((100, 80)));
    surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(shown_size(&compositor, surface), Some((50, 40)));
    assert_eq!(client.events.buffer_releases.last(), Some(&next.id()));
  }

  #[test]
  fn a_parent_commit_keeps_what_the_child_attached_since_its_commit() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "parent");
    client.attach_buffer(&mut compositor, &toplevel.surface, 100, 80);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let subcompositor = client.bind::<WlSubcompositor>(1);
    let child = wl_compositor.create_surface(&client.handle, ());
    let _subsurface = subcompositor.get_subsurface(
      &child,
      &toplevel.surface,
      &client.handle,
      (),
    );
    let cached = client.shm_buffer(10, 10);
    let next = client.shm_buffer(20, 20);
    child.attach(Some(&cached), 0, 0);
    child.commit();
    child.attach(Some(&next), 0, 0);
    toplevel.surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(shown_size(&compositor, &child), Some((10, 10)));
    child.commit();
    toplevel.surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(shown_size(&compositor, &child), Some((20, 20)));
  }
}
