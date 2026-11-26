//! `wp_fifo_v1`: content updates that wait for the previous one to have
//! been presented.
//!
//! As wlroots' implementation does (merge request !4463, "fifo-v1: new
//! protocol implementation"), the barrier requests belong to the content
//! update they are committed with, and an update that waits for a set
//! barrier is queued in the surface's commit queue, in order, never merged
//! with another. Each presented frame releases the queued updates up to
//! and including the first that sets the barrier again, or clears the
//! barrier when none does. A queued update holds its buffer, so a client
//! that commits faster than the output refreshes runs out of buffers
//! instead of running ahead of the display.

use std::{
  cell::Cell,
  collections::{HashMap, VecDeque},
  rc::Rc,
};

use wayland_protocols::wp::fifo::v1::server::{
  wp_fifo_manager_v1::{self, WpFifoManagerV1},
  wp_fifo_v1::{self, WpFifoV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{commit_queue::Blocker, state::CompositorState};

/// One queued update's wait for the barrier.
struct Wait {
  open: Rc<Cell<bool>>,
  /// Whether the update sets the barrier again.
  sets_barrier: bool,
}

#[derive(Default)]
struct FifoSurface {
  /// Set by an applied update with `set_barrier`, cleared by a frame.
  barrier: bool,
  /// The waits of queued updates, oldest first.
  waiting: VecDeque<Wait>,
}

#[derive(Default)]
pub(crate) struct FifoState {
  surfaces: HashMap<ObjectId, FifoSurface>,
  objects: HashMap<ObjectId, ObjectId>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpFifoManagerV1, ()>(1, ());
}

/// A queued update's wait, open once a frame released it.
struct FifoGate(Rc<Cell<bool>>);

impl Blocker for FifoGate {
  fn ready(&self) -> bool {
    self.0.get()
  }
}

/// What the pending update of `object` waits for: the barrier, if it asked
/// to wait, one is set and the update is not exempt. Synchronized
/// subsurfaces, invisible surfaces and tearing surfaces do not wait, as
/// the protocol allows.
pub(crate) fn blocker(
  state: &mut CompositorState,
  object: &ObjectId,
) -> Option<Box<dyn Blocker>> {
  let surface = state.surfaces.get(object)?;
  let (wait, sets_barrier) = (
    surface.pending.fifo_wait_barrier,
    surface.pending.fifo_set_barrier,
  );
  let synchronized = matches!(
    surface.role,
    crate::shell::Role::Subsurface {
      synchronized: true,
      ..
    }
  );
  let tearing = surface.tearing;
  if !wait
    || synchronized
    || tearing
    || !state.fifo.surfaces.get(object)?.barrier
  {
    return None;
  }
  let visible = state
    .visible_surfaces()
    .into_iter()
    .any(|snapshot| state.surface_ids.get(&snapshot.id) == Some(object));
  if !visible {
    return None;
  }
  let open = Rc::new(Cell::new(false));
  state
    .fifo
    .surfaces
    .get_mut(object)?
    .waiting
    .push_back(Wait {
      open: Rc::clone(&open),
      sets_barrier,
    });
  Some(Box::new(FifoGate(open)))
}

/// An update of `object` was applied: one that set the barrier makes
/// later waiting updates wait for the next frame.
pub(crate) fn applied(
  state: &mut CompositorState,
  object: &ObjectId,
  sets_barrier: bool,
) {
  if sets_barrier && let Some(fifo) = state.fifo.surfaces.get_mut(object) {
    fifo.barrier = true;
  }
}

/// A frame was presented: on every surface with a set barrier, releases
/// the queued updates up to and including the first that sets the
/// barrier again, or clears the barrier if none does, and applies what is
/// ready, before frame callbacks.
pub(crate) fn frame(state: &mut CompositorState) {
  let mut released = Vec::new();
  for (object, fifo) in &mut state.fifo.surfaces {
    if !fifo.barrier {
      continue;
    }
    let mut sets_barrier = false;
    while let Some(wait) = fifo.waiting.pop_front() {
      wait.open.set(true);
      sets_barrier = wait.sets_barrier;
      if sets_barrier {
        break;
      }
    }
    if !sets_barrier {
      fifo.barrier = false;
    }
    released.push(object.clone());
  }
  for object in released {
    crate::commit_queue::process(state, &object);
  }
}

/// Whether an update waits for a frame.
pub(crate) fn has_pending(state: &CompositorState) -> bool {
  state
    .fifo
    .surfaces
    .values()
    .any(|surface| !surface.waiting.is_empty())
}

pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  state.fifo.surfaces.remove(object);
  state.fifo.objects.retain(|_, surface| surface != object);
}

impl GlobalDispatch<WpFifoManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wp_fifo_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpFifoManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpFifoManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpFifoManagerV1,
    request: wp_fifo_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wp_fifo_manager_v1::Request::GetFifo { id, surface } => {
        let fifo = data_init.init(id, ());
        let surface = Resource::id(&surface);
        // Only a live FIFO object counts: a client may destroy its object
        // and create a new one, as Mesa's Vulkan WSI does when it recreates
        // a swapchain. The surface keeps its barrier state meanwhile.
        if state.fifo.objects.values().any(|object| *object == surface) {
          resource.post_error(
            wp_fifo_manager_v1::Error::AlreadyExists,
            "surface already has a FIFO object",
          );
          return;
        }
        state.fifo.surfaces.entry(surface.clone()).or_default();
        state.fifo.objects.insert(Resource::id(&fifo), surface);
      }
      wp_fifo_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<WpFifoV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpFifoV1,
    request: wp_fifo_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let Some(surface) =
      state.fifo.objects.get(&Resource::id(resource)).cloned()
    else {
      return;
    };
    if !state.surfaces.contains_key(&surface) {
      resource.post_error(
        wp_fifo_v1::Error::SurfaceDestroyed,
        "the FIFO surface was destroyed",
      );
      return;
    }
    let Some(surface) = state.surfaces.get_mut(&surface) else {
      return;
    };
    // Both requests belong to the pending content update.
    match request {
      wp_fifo_v1::Request::SetBarrier => {
        surface.pending.fifo_set_barrier = true
      }
      wp_fifo_v1::Request::WaitBarrier => {
        surface.pending.fifo_wait_barrier = true
      }
      wp_fifo_v1::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WpFifoV1,
    _data: &(),
  ) {
    state.fifo.objects.remove(&Resource::id(resource));
  }
}

#[cfg(test)]
mod tests {
  use wayland_client::protocol::wl_compositor::WlCompositor;
  use wayland_protocols::wp::fifo::v1::client::wp_fifo_manager_v1;

  use crate::{test_client::TestClient, tests::test_compositor};

  #[test]
  fn a_surface_can_get_a_new_fifo_after_destroying_the_old_one() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client.bind::<wp_fifo_manager_v1::WpFifoManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let fifo = manager.get_fifo(&surface, &client.handle, ());
    fifo.set_barrier();
    surface.commit();
    client.roundtrip(&mut compositor);
    fifo.destroy();
    let fifo = manager.get_fifo(&surface, &client.handle, ());
    fifo.wait_barrier();
    surface.commit();
    // A protocol error would end the connection and fail the round trip.
    client.roundtrip(&mut compositor);
  }
}
