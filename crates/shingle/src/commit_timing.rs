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

/// Attaches the pending timestamp to the content update just committed,
/// so the client may set one for its next update. An update merged into
/// one that is still waiting keeps the later of the two deadlines, so
/// neither is shown early. FIFO calls this when it queues an update, which
/// passes through [`commit`] again once its barrier clears.
pub(crate) fn latch(state: &mut CompositorState, object: &ObjectId) {
  let Some(timer) = state.commit_timing.surfaces.get_mut(object) else {
    return;
  };
  if let Some(target) = timer.pending_target.take() {
    timer.target = Some(timer.target.map_or(target, |old| old.max(target)));
  }
}

/// Defers a commit until its timestamp, preserving content order.
pub(crate) fn commit(state: &mut CompositorState, object: &ObjectId) -> bool {
  latch(state, object);
  let Some(timer) = state.commit_timing.surfaces.get_mut(object) else {
    return false;
  };
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
    state.commit_state(&object, pending, false);
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
        // Only a live timer object counts: a client may destroy its timer
        // and create a new one, as Mesa's Vulkan WSI does when it recreates
        // a swapchain. A pending target time stays with the surface.
        if state
          .commit_timing
          .objects
          .values()
          .any(|object| *object == surface)
        {
          resource.post_error(
            wp_commit_timing_manager_v1::Error::CommitTimerExists,
            "surface already has a commit timer",
          );
          return;
        }
        state
          .commit_timing
          .surfaces
          .entry(surface.clone())
          .or_default();
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
        // Only the pending content update counts. Earlier updates may still
        // wait for their deadline, as Mesa's Vulkan WSI queues them.
        if timer.pending_target.is_some() {
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

#[cfg(test)]
mod tests {
  use wayland_client::protocol::wl_compositor::WlCompositor;
  use wayland_protocols::wp::commit_timing::v1::client::{
    wp_commit_timing_manager_v1::WpCommitTimingManagerV1,
  };

  use std::time::Duration;

  use crate::{
    state::CompositorState, test_client::TestClient, tests::test_compositor,
  };

  #[test]
  fn a_surface_can_get_a_new_timer_after_destroying_the_old_one() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client.bind::<WpCommitTimingManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let timer = manager.get_timer(&surface, &client.handle, ());
    surface.commit();
    client.roundtrip(&mut compositor);
    timer.destroy();
    let _timer = manager.get_timer(&surface, &client.handle, ());
    surface.commit();
    // A protocol error would end the connection and fail the round trip.
    client.roundtrip(&mut compositor);
  }

  #[test]
  fn latch_attaches_the_pending_timestamp_and_keeps_the_later_deadline() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client.bind::<WpCommitTimingManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let _timer = manager.get_timer(&surface, &client.handle, ());
    client.roundtrip(&mut compositor);
    let state = &mut compositor.state;
    let object = state
      .commit_timing
      .objects
      .values()
      .next()
      .cloned()
      .expect("the timer is registered");
    let set = |state: &mut CompositorState, target: u64| {
      let timer = state.commit_timing.surfaces.get_mut(&object).unwrap();
      timer.pending_target = Some(Duration::from_secs(target));
    };
    set(state, 20);
    super::latch(state, &object);
    set(state, 10);
    super::latch(state, &object);
    let timer = &state.commit_timing.surfaces[&object];
    assert_eq!(timer.pending_target, None);
    assert_eq!(timer.target, Some(Duration::from_secs(20)));
  }

  #[test]
  fn a_timestamp_may_follow_an_update_that_still_waits() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client.bind::<WpCommitTimingManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let timer = manager.get_timer(&surface, &client.handle, ());
    let later =
      crate::clock::monotonic_now() + std::time::Duration::from_secs(60);
    let seconds = later.as_secs();
    timer.set_timestamp((seconds >> 32) as u32, seconds as u32, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    assert!(super::has_pending(&compositor.state));
    timer.set_timestamp((seconds >> 32) as u32, seconds as u32, 1);
    surface.commit();
    client.roundtrip(&mut compositor);
  }
}
