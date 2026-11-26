//! `wp_commit_timing_v1`: content updates that wait for a time.
//!
//! As wlroots' implementation does (merge request !4617,
//! "commit-timing-v1: new protocol implementation"), a timestamp belongs
//! to the content update it is committed with, and an update whose time
//! has not come waits in the surface's commit queue with its own deadline,
//! in order, never merged with another. The deadline is one refresh and
//! 500 µs before the timestamp, so that the update is applied before the
//! frame that is to show it; a timestamp already that close is not waited
//! for. Waiting updates hold their buffers. Deadlines are checked when a
//! frame is presented, and while one waits [`has_pending`] asks for
//! frames.

use std::{cell::Cell, collections::HashMap, rc::Rc, time::Duration};

use wayland_protocols::wp::commit_timing::v1::server::{
  wp_commit_timer_v1::{self, WpCommitTimerV1},
  wp_commit_timing_manager_v1::{self, WpCommitTimingManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{commit_queue::Blocker, state::CompositorState};

/// The slop wlroots subtracts so that a deadline is not missed.
const SLOP: Duration = Duration::from_micros(500);
/// Timestamps closer than this are not waited for, as in wlroots.
const MINIMUM_WAIT: Duration = Duration::from_millis(1);

/// The deadlines of one surface's waiting updates.
#[derive(Default)]
struct CommitTimer {
  /// Each waiting update's deadline, with whether it was applied.
  deadlines: Vec<(Duration, Rc<Cell<bool>>)>,
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

/// When an update timed for `timestamp` may be applied, on an output
/// refreshing every `refresh`: one refresh and the slop before it.
pub(crate) fn deadline(timestamp: Duration, refresh: Duration) -> Duration {
  timestamp.saturating_sub(refresh + SLOP)
}

/// A waiting update's deadline.
struct Deadline {
  at: Duration,
  /// Set when the blocker goes, so that [`has_pending`] stops counting it.
  done: Rc<Cell<bool>>,
}

impl Blocker for Deadline {
  fn ready(&self) -> bool {
    now() >= self.at
  }
}

impl Drop for Deadline {
  fn drop(&mut self) {
    self.done.set(true);
  }
}

/// What the pending update of `object` waits for: its timestamp's
/// deadline, if it has one that has not come.
pub(crate) fn blocker(
  state: &mut CompositorState,
  object: &ObjectId,
) -> Option<Box<dyn Blocker>> {
  let timestamp = state.surfaces.get(object)?.pending.timestamp?;
  let refresh = Duration::from_nanos(u64::from(
    crate::presentation::refresh_ns(state.output_info.refresh_mhz),
  ));
  let at = deadline(timestamp, refresh);
  if at <= now() + MINIMUM_WAIT {
    return None;
  }
  let done = Rc::new(Cell::new(false));
  state
    .commit_timing
    .surfaces
    .get_mut(object)?
    .deadlines
    .push((at, Rc::clone(&done)));
  Some(Box::new(Deadline { at, done }))
}

/// A frame was presented: applies, in order, the updates whose deadlines
/// have come, before frame callbacks.
pub(crate) fn frame(state: &mut CompositorState) {
  let now = now();
  let mut due = Vec::new();
  for (object, timer) in &mut state.commit_timing.surfaces {
    timer.deadlines.retain(|(_, done)| !done.get());
    if timer.deadlines.iter().any(|(at, _)| *at <= now) {
      due.push(object.clone());
    }
  }
  for object in due {
    crate::commit_queue::process(state, &object);
  }
}

/// Whether an update waits for its deadline.
pub(crate) fn has_pending(state: &CompositorState) -> bool {
  state
    .commit_timing
    .surfaces
    .values()
    .any(|timer| timer.deadlines.iter().any(|(_, done)| !done.get()))
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
        let Some(surface) = state.surfaces.get_mut(&surface) else {
          return;
        };
        // Only the pending content update counts. Earlier updates may still
        // wait for their deadline, as Mesa's Vulkan WSI queues them.
        if surface.pending.timestamp.is_some() {
          resource.post_error(
            wp_commit_timer_v1::Error::TimestampExists,
            "a timestamp is already pending on this surface",
          );
          return;
        }
        let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
        surface.pending.timestamp = Some(Duration::new(seconds, tv_nsec));
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

  use crate::{test_client::TestClient, tests::test_compositor};

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
  fn a_deadline_is_one_refresh_and_the_slop_before_the_timestamp() {
    let refresh = Duration::from_nanos(16_666_667);
    assert_eq!(
      super::deadline(Duration::from_secs(1), refresh),
      Duration::from_secs(1) - refresh - Duration::from_micros(500)
    );
    assert_eq!(super::deadline(Duration::ZERO, refresh), Duration::ZERO);
  }

  /// The buffer widths of the content updates applied since the last
  /// call, in order.
  fn applied_widths(compositor: &mut crate::Compositor) -> Vec<u32> {
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        crate::Event::SurfaceCommitted(snapshot) => {
          snapshot.buffer.map(|buffer| buffer.width)
        }
        _ => None,
      })
      .collect()
  }

  /// A client that commits faster than the output refreshes, each update
  /// timed a little ahead of the clock, as NVIDIA's Vulkan WSI does once
  /// it gets its buffers back at once. Every update has its own deadline,
  /// so the first is shown once its deadline has come, and the others
  /// follow in order. Merging them into one update that keeps the latest
  /// deadline would push that deadline ahead forever and show nothing.
  #[test]
  fn a_burst_timed_ahead_of_the_clock_still_shows_its_updates_in_order() {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let wl_compositor = client.bind::<WlCompositor>(4);
    let manager = client.bind::<WpCommitTimingManagerV1>(1);
    let surface = wl_compositor.create_surface(&client.handle, ());
    let timer = manager.get_timer(&surface, &client.handle, ());
    client.roundtrip(&mut compositor);
    applied_widths(&mut compositor);
    let mut shown = Vec::new();
    for width in 10..30 {
      let buffer = client.shm_buffer(width, 10);
      let target = crate::clock::monotonic_now() + Duration::from_millis(40);
      let seconds = target.as_secs();
      timer.set_timestamp(
        (seconds >> 32) as u32,
        seconds as u32,
        target.subsec_nanos(),
      );
      surface.attach(Some(&buffer), 0, 0);
      surface.commit();
      client.roundtrip(&mut compositor);
      std::thread::sleep(Duration::from_millis(10));
      compositor.frame();
      client.roundtrip(&mut compositor);
      shown.extend(applied_widths(&mut compositor));
    }
    assert!(
      shown.first() == Some(&10),
      "the first update is shown once its own deadline came: {shown:?}"
    );
    // Each applied once, in order, none merged away.
    assert!(
      shown.windows(2).all(|pair| pair[1] == pair[0] + 1),
      "{shown:?}"
    );
    assert!(shown.len() >= 10, "{shown:?}");
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
