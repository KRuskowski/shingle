use std::{collections::HashMap, time::Duration};

use wayland_protocols::wp::commit_timing::v1::server::{
  wp_commit_timer_v1::{self, WpCommitTimerV1},
  wp_commit_timing_manager_v1::{self, WpCommitTimingManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::state::{CompositorState, PendingSurface};

#[derive(Default)]
struct CommitTimer {
  pending_target: Option<Duration>,
  target: Option<Duration>,
  deferred: Option<PendingSurface>,
}

#[derive(Default)]
pub(crate) struct CommitTiming {
  surfaces: HashMap<ObjectId, CommitTimer>,
  objects: HashMap<ObjectId, ObjectId>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpCommitTimingManagerV1, ()>(1, ());
}

fn now() -> Duration {
  crate::clock::monotonic_now()
}

/// Defers a commit until its timestamp, preserving content order.
pub(crate) fn commit(state: &mut CompositorState, object: &ObjectId) -> bool {
  let Some(timer) = state.commit_timing.surfaces.get_mut(object) else {
    return false;
  };
  if let Some(target) = timer.pending_target.take() {
    timer.target = Some(target);
  }
  if timer.deferred.is_some()
    || timer.target.is_some_and(|target| now() < target)
  {
    return true;
  }
  timer.target = None;
  false
}

pub(crate) fn defer(
  state: &mut CompositorState,
  object: &ObjectId,
  mut pending: PendingSurface,
) {
  let Some(timer) = state.commit_timing.surfaces.get_mut(object) else {
    return;
  };
  if let Some(cached) = &mut timer.deferred {
    crate::state::merge_pending(cached, &mut pending);
  } else {
    timer.deferred = Some(pending);
  }
}

/// Applies all timestamp-constrained updates whose deadlines have arrived.
pub(crate) fn frame(state: &mut CompositorState) {
  let now = now();
  let ready = state
    .commit_timing
    .surfaces
    .iter_mut()
    .filter_map(|(object, timer)| {
      if timer.deferred.is_some()
        && timer.target.is_some_and(|target| now >= target)
      {
        timer.target = None;
        timer
          .deferred
          .take()
          .map(|pending| (object.clone(), pending))
      } else {
        None
      }
    })
    .collect::<Vec<_>>();
  for (object, pending) in ready {
    if let Some(surface) = state.surfaces.get_mut(&object) {
      surface.pending = pending;
      state.commit_fifo(&object);
    }
  }
}

pub(crate) fn has_pending(state: &CompositorState) -> bool {
  state
    .commit_timing
    .surfaces
    .values()
    .any(|timer| timer.deferred.is_some())
}

pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  state.commit_timing.surfaces.remove(object);
  state
    .commit_timing
    .objects
    .retain(|_, surface| surface != object);
}

impl GlobalDispatch<WpCommitTimingManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wp_commit_timing_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpCommitTimingManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpCommitTimingManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpCommitTimingManagerV1,
    request: wp_commit_timing_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wp_commit_timing_manager_v1::Request::GetTimer { id, surface } => {
        let timer = data_init.init(id, ());
        let surface = Resource::id(&surface);
        if state.commit_timing.surfaces.contains_key(&surface) {
          resource.post_error(
            wp_commit_timing_manager_v1::Error::CommitTimerExists,
            "surface already has a commit timer",
          );
          return;
        }
        state
          .commit_timing
          .surfaces
          .insert(surface.clone(), CommitTimer::default());
        state
          .commit_timing
          .objects
          .insert(Resource::id(&timer), surface);
      }
      wp_commit_timing_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<WpCommitTimerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpCommitTimerV1,
    request: wp_commit_timer_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let Some(surface) = state
      .commit_timing
      .objects
      .get(&Resource::id(resource))
      .cloned()
    else {
      return;
    };
    if !state.surfaces.contains_key(&surface) {
      resource.post_error(
        wp_commit_timer_v1::Error::SurfaceDestroyed,
        "the commit-timer surface was destroyed",
      );
      return;
    }
    match request {
      wp_commit_timer_v1::Request::SetTimestamp {
        tv_sec_hi,
        tv_sec_lo,
        tv_nsec,
      } => {
        if tv_nsec >= 1_000_000_000 {
          resource.post_error(
            wp_commit_timer_v1::Error::InvalidTimestamp,
            "timestamp nanoseconds must be below 1000000000",
          );
          return;
        }
        let Some(timer) = state.commit_timing.surfaces.get_mut(&surface) else {
          return;
        };
        if timer.pending_target.is_some() || timer.target.is_some() {
          resource.post_error(
            wp_commit_timer_v1::Error::TimestampExists,
            "a timestamp is already pending on this surface",
          );
          return;
        }
        let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
        timer.pending_target = Some(Duration::new(seconds, tv_nsec));
      }
      wp_commit_timer_v1::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WpCommitTimerV1,
    _data: &(),
  ) {
    state.commit_timing.objects.remove(&Resource::id(resource));
  }
}
