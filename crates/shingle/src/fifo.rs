use std::collections::HashMap;

use wayland_protocols::wp::fifo::v1::server::{
  wp_fifo_manager_v1::{self, WpFifoManagerV1},
  wp_fifo_v1::{self, WpFifoV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::state::{CompositorState, PendingSurface};

#[derive(Default)]
struct FifoSurface {
  barrier: bool,
  pending_barrier: bool,
  pending_wait: bool,
  deferred: Option<PendingSurface>,
}

#[derive(Default)]
pub(crate) struct FifoState {
  surfaces: HashMap<ObjectId, FifoSurface>,
  objects: HashMap<ObjectId, ObjectId>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpFifoManagerV1, ()>(1, ());
}

/// Applies barrier state or reports that an update must wait for a frame.
pub(crate) fn commit(state: &mut CompositorState, object: &ObjectId) -> bool {
  let visible = state
    .visible_surfaces()
    .into_iter()
    .any(|snapshot| state.surface_ids.get(&snapshot.id) == Some(object));
  let synchronized = state.surfaces.get(object).is_some_and(|surface| {
    matches!(
      surface.role,
      crate::shell::Role::Subsurface {
        synchronized: true,
        ..
      }
    )
  });
  let tearing = state
    .surfaces
    .get(object)
    .is_some_and(|surface| surface.tearing);
  let Some(fifo) = state.fifo.surfaces.get_mut(object) else {
    return false;
  };
  if fifo.barrier
    && (fifo.pending_wait || fifo.deferred.is_some())
    && visible
    && !synchronized
    && !tearing
  {
    return true;
  }
  if fifo.pending_barrier {
    fifo.barrier = true;
    fifo.pending_barrier = false;
  }
  fifo.pending_wait = false;
  false
}

/// Stores or coalesces a deferred pending surface state.
pub(crate) fn defer(
  state: &mut CompositorState,
  object: &ObjectId,
  mut pending: PendingSurface,
) {
  let Some(fifo) = state.fifo.surfaces.get_mut(object) else {
    return;
  };
  if let Some(cached) = &mut fifo.deferred {
    crate::state::merge_pending(cached, &mut pending);
  } else {
    fifo.deferred = Some(pending);
  }
}

/// Clears refresh barriers and applies deferred updates before callbacks.
pub(crate) fn frame(state: &mut CompositorState) {
  for fifo in state.fifo.surfaces.values_mut() {
    fifo.barrier = false;
  }
  let deferred = state
    .fifo
    .surfaces
    .iter_mut()
    .filter_map(|(object, fifo)| {
      fifo
        .deferred
        .take()
        .map(|pending| (object.clone(), pending))
    })
    .collect::<Vec<_>>();
  for (object, pending) in deferred {
    if let Some(surface) = state.surfaces.get_mut(&object) {
      surface.pending = pending;
      state.commit_fifo(&object);
    }
  }
}

pub(crate) fn has_pending(state: &CompositorState) -> bool {
  state
    .fifo
    .surfaces
    .values()
    .any(|surface| surface.deferred.is_some())
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
        if state.fifo.surfaces.contains_key(&surface) {
          resource.post_error(
            wp_fifo_manager_v1::Error::AlreadyExists,
            "surface already has a FIFO object",
          );
          return;
        }
        state
          .fifo
          .surfaces
          .insert(surface.clone(), Default::default());
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
    let Some(fifo) = state.fifo.surfaces.get_mut(&surface) else {
      return;
    };
    match request {
      wp_fifo_v1::Request::SetBarrier => fifo.pending_barrier = true,
      wp_fifo_v1::Request::WaitBarrier => fifo.pending_wait = true,
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
