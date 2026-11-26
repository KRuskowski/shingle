//! The per-surface commit queue (docs/design/dmabuf.md, section 4).
//!
//! A commit that may not apply yet, because a fence has not signalled,
//! a FIFO barrier is set or its commit-timing deadline has not come,
//! waits here with its blockers, and every later commit of the surface
//! waits behind it. When the blockers of the first entry clear, it and
//! the ready entries after it go through the usual commit path in order,
//! where synchronized subsurfaces still apply. Entries are never merged:
//! a later buffer may be ready before an earlier one. Destroying the
//! surface drops its entries, which releases their buffers.
//!
//! FIFO barriers (crate::fifo) and commit-timing deadlines
//! (crate::commit_timing) wait here too, as wlroots queues them, and
//! DB-4's implicit-sync wait (crate::linux_dmabuf) is a [`BlockerSource`];
//! DB-5's syncobj waits will be another.

use std::collections::{HashMap, VecDeque};

use wayland_server::backend::ObjectId;

use crate::state::{CompositorState, PendingSurface};

/// Something a queued commit waits for, such as a fence.
pub(crate) trait Blocker {
  /// Whether the commit may apply as far as this blocker is concerned.
  fn ready(&self) -> bool;
}

/// Decides what a commit waits for.
pub(crate) trait BlockerSource {
  /// The blockers of `pending`, a commit of `surface`; ready ones may be
  /// left out.
  fn blockers(
    &self,
    surface: &ObjectId,
    pending: &PendingSurface,
  ) -> Vec<Box<dyn Blocker>>;
}

/// One waiting commit.
struct Entry {
  pending: PendingSurface,
  blockers: Vec<Box<dyn Blocker>>,
}

impl Entry {
  fn ready(&self) -> bool {
    self.blockers.iter().all(|blocker| blocker.ready())
  }
}

/// The commit queues of all surfaces and the sources of their blockers.
#[derive(Default)]
pub(crate) struct CommitQueues {
  surfaces: HashMap<ObjectId, VecDeque<Entry>>,
  sources: Vec<Box<dyn BlockerSource>>,
}

impl CommitQueues {
  /// Adds a source that every later commit is asked about.
  pub(crate) fn add_source(&mut self, source: Box<dyn BlockerSource>) {
    self.sources.push(source);
  }

  /// How many commits of `surface` wait.
  #[cfg(test)]
  pub(crate) fn waiting(&self, surface: &ObjectId) -> usize {
    self.surfaces.get(surface).map_or(0, VecDeque::len)
  }
}

/// `wl_surface.commit`: applies the pending state at once when nothing
/// blocks it and nothing waits before it, and queues it otherwise.
pub(crate) fn commit(state: &mut CompositorState, object: &ObjectId) {
  if !state.surfaces.contains_key(object) {
    return;
  }
  // FIFO barriers and commit-timing deadlines, as wlroots queues them.
  let mut blockers = crate::fifo::blocker(state, object)
    .into_iter()
    .chain(crate::commit_timing::blocker(state, object))
    .collect::<Vec<_>>();
  let Some(surface) = state.surfaces.get(object) else {
    return;
  };
  blockers.extend(
    state
      .commit_queue
      .sources
      .iter()
      .flat_map(|source| source.blockers(object, &surface.pending)),
  );
  blockers.retain(|blocker| !blocker.ready());
  if blockers.is_empty() && !state.commit_queue.surfaces.contains_key(object) {
    state.commit_inner(object, false);
    return;
  }
  let Some(pending) = state.take_pending(object) else {
    return;
  };
  state
    .commit_queue
    .surfaces
    .entry(object.clone())
    .or_default()
    .push_back(Entry { pending, blockers });
  process(state, object);
}

/// Applies the commits of `object` that are ready, in order, up to the
/// first that is still blocked. Whatever signals a blocker calls this.
pub(crate) fn process(state: &mut CompositorState, object: &ObjectId) {
  loop {
    let Some(queue) = state.commit_queue.surfaces.get_mut(object) else {
      return;
    };
    if !queue.front().is_some_and(Entry::ready) {
      return;
    }
    let entry = queue.pop_front().expect("the queue has a front entry");
    if queue.is_empty() {
      state.commit_queue.surfaces.remove(object);
    }
    state.commit_state(object, entry.pending, false);
  }
}

/// [`process`] for every surface with waiting commits.
#[cfg(test)]
pub(crate) fn process_all(state: &mut CompositorState) {
  let objects = state
    .commit_queue
    .surfaces
    .keys()
    .cloned()
    .collect::<Vec<_>>();
  for object in objects {
    process(state, &object);
  }
}

/// Drops the waiting commits of a destroyed surface, releasing their
/// buffers.
pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  state.commit_queue.surfaces.remove(object);
}

#[cfg(test)]
mod tests {
  use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
  };

  use wayland_client::{
    Proxy,
    protocol::{wl_compositor::WlCompositor, wl_surface::WlSurface},
  };
  use wayland_server::{Resource as _, backend::ObjectId};

  use super::{Blocker, BlockerSource};
  use crate::{
    Compositor, Event,
    state::PendingSurface,
    test_client::{Frame, TestClient},
    tests::test_compositor,
  };

  /// A blocker the test opens.
  struct Gate(Rc<Cell<bool>>);

  impl Blocker for Gate {
    fn ready(&self) -> bool {
      self.0.get()
    }
  }

  /// Gives each commit the next gate in line, or none.
  #[derive(Clone, Default)]
  struct Gates(Rc<RefCell<VecDeque<Option<Flag>>>>);

  /// Whether a [`Gate`] is open.
  type Flag = Rc<Cell<bool>>;

  impl Gates {
    /// Blocks the next commit until the returned flag is set.
    fn next_blocked(&self) -> Rc<Cell<bool>> {
      let gate = Rc::new(Cell::new(false));
      self.0.borrow_mut().push_back(Some(Rc::clone(&gate)));
      gate
    }

    /// Lets the next commit through without a blocker.
    fn next_free(&self) {
      self.0.borrow_mut().push_back(None);
    }
  }

  impl BlockerSource for Gates {
    fn blockers(
      &self,
      _surface: &ObjectId,
      _pending: &PendingSurface,
    ) -> Vec<Box<dyn Blocker>> {
      self
        .0
        .borrow_mut()
        .pop_front()
        .flatten()
        .map(|gate| Box::new(Gate(gate)) as Box<dyn Blocker>)
        .into_iter()
        .collect()
    }
  }

  fn setup() -> (Compositor, TestClient, WlSurface, Gates, ObjectId) {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let surface = wl_compositor.create_surface(&client.handle, ());
    client.roundtrip(&mut compositor);
    let gates = Gates::default();
    compositor
      .state
      .commit_queue
      .add_source(Box::new(gates.clone()));
    let object = compositor
      .state
      .surfaces
      .values()
      .find(|candidate| {
        candidate.resource.id().protocol_id() == surface.id().protocol_id()
      })
      .map(|surface| surface.resource.id())
      .expect("the surface exists");
    (compositor, client, surface, gates, object)
  }

  /// The buffer widths of the surface commits applied since the last call.
  fn applied_widths(compositor: &mut Compositor) -> Vec<u32> {
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::SurfaceCommitted(snapshot) => snapshot.buffer,
        _ => None,
      })
      .map(|buffer| buffer.width)
      .collect()
  }

  #[test]
  fn an_unblocked_commit_applies_at_once() {
    let (mut compositor, mut client, surface, gates, object) = setup();
    gates.next_free();
    let buffer = client.shm_buffer(4, 4);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 0);
    assert_eq!(applied_widths(&mut compositor), [4]);
    assert_eq!(client.events.buffer_releases, [buffer.id()]);
  }

  #[test]
  fn commits_apply_in_order_once_the_first_is_ready() {
    let (mut compositor, mut client, surface, gates, object) = setup();
    applied_widths(&mut compositor);
    let first = gates.next_blocked();
    let second = gates.next_blocked();
    gates.next_free();
    let buffers = [10, 20, 30].map(|size| client.shm_buffer(size, size));
    for buffer in &buffers {
      surface.attach(Some(buffer), 0, 0);
      surface.commit();
    }
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 3);
    assert!(applied_widths(&mut compositor).is_empty());
    assert!(client.events.buffer_releases.is_empty());
    // The second is ready first, but waits for the first.
    second.set(true);
    super::process_all(&mut compositor.state);
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 3);
    assert!(applied_widths(&mut compositor).is_empty());
    first.set(true);
    super::process(&mut compositor.state, &object);
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 0);
    // None is merged into another: each applies and is released.
    assert_eq!(applied_widths(&mut compositor), [10, 20, 30]);
    assert_eq!(
      client.events.buffer_releases,
      buffers.iter().map(Proxy::id).collect::<Vec<_>>()
    );
  }

  #[test]
  fn processing_stops_at_the_first_blocked_commit() {
    let (mut compositor, mut client, surface, gates, object) = setup();
    applied_widths(&mut compositor);
    let first = gates.next_blocked();
    let second = gates.next_blocked();
    for size in [10, 20] {
      let buffer = client.shm_buffer(size, size);
      surface.attach(Some(&buffer), 0, 0);
      surface.commit();
    }
    client.roundtrip(&mut compositor);
    first.set(true);
    super::process(&mut compositor.state, &object);
    assert_eq!(applied_widths(&mut compositor), [10]);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 1);
    second.set(true);
    super::process(&mut compositor.state, &object);
    assert_eq!(applied_widths(&mut compositor), [20]);
  }

  #[test]
  fn frame_callbacks_wait_with_their_commit() {
    let (mut compositor, mut client, surface, gates, object) = setup();
    let gate = gates.next_blocked();
    let buffer = client.shm_buffer(4, 4);
    surface.attach(Some(&buffer), 0, 0);
    surface.frame(&client.handle, Frame);
    surface.commit();
    client.roundtrip(&mut compositor);
    compositor.frame();
    client.roundtrip(&mut compositor);
    assert!(client.events.frame_done.is_empty());
    assert!(compositor.state.frame_callbacks.is_empty());
    gate.set(true);
    super::process(&mut compositor.state, &object);
    assert!(
      compositor
        .state
        .frame_callbacks
        .iter()
        .any(|callback| callback.resource.is_alive())
    );
  }

  #[test]
  fn state_set_after_a_queued_commit_stays_pending() {
    let (mut compositor, mut client, surface, gates, object) = setup();
    applied_widths(&mut compositor);
    let gate = gates.next_blocked();
    let queued = client.shm_buffer(10, 10);
    let next = client.shm_buffer(20, 20);
    surface.attach(Some(&queued), 0, 0);
    surface.commit();
    surface.attach(Some(&next), 0, 0);
    client.roundtrip(&mut compositor);
    gate.set(true);
    super::process(&mut compositor.state, &object);
    assert_eq!(applied_widths(&mut compositor), [10]);
    gates.next_free();
    surface.commit();
    client.roundtrip(&mut compositor);
    assert_eq!(applied_widths(&mut compositor), [20]);
  }

  #[test]
  fn destroying_a_surface_drops_its_queue_and_releases_the_buffers() {
    let (mut compositor, mut client, surface, gates, object) = setup();
    let _gate = gates.next_blocked();
    let buffers = [10, 20].map(|size| client.shm_buffer(size, size));
    for buffer in &buffers {
      surface.attach(Some(buffer), 0, 0);
      surface.commit();
    }
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 2);
    surface.destroy();
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.commit_queue.waiting(&object), 0);
    let ids =
      |ids: &mut dyn Iterator<Item = wayland_client::backend::ObjectId>| {
        let mut ids = ids.map(|id| id.protocol_id()).collect::<Vec<_>>();
        ids.sort_unstable();
        ids
      };
    assert_eq!(
      ids(&mut client.events.buffer_releases.iter().cloned()),
      ids(&mut buffers.iter().map(Proxy::id))
    );
  }
}
