use std::collections::HashSet;

use wayland_protocols::wp::presentation_time::server::{
  wp_presentation::{self, WpPresentation},
  wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ClientId, protocol::wl_surface::WlSurface,
};

use crate::{PresentInfo, SurfaceId, state::CompositorState};

#[derive(Default)]
pub(crate) struct Presentation {
  pending: Vec<PendingFeedback>,
}

struct PendingFeedback {
  surface: SurfaceId,
  resource: WpPresentationFeedback,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpPresentation, ()>(2, ());
}

/// Returns the refresh interval in nanoseconds for a refresh rate in
/// millihertz, rounded to the nearest nanosecond, or zero when the rate is
/// unknown.
pub(crate) fn refresh_ns(refresh_mhz: u32) -> u32 {
  if refresh_mhz == 0 {
    return 0;
  }
  let mhz = u64::from(refresh_mhz);
  ((1_000_000_000_000 + mhz / 2) / mhz).min(u64::from(u32::MAX)) as u32
}

/// The arguments of `wp_presentation_feedback.presented`.
#[derive(Debug, Eq, PartialEq)]
struct Presented {
  tv_sec_hi: u32,
  tv_sec_lo: u32,
  tv_nsec: u32,
  refresh: u32,
  seq_hi: u32,
  seq_lo: u32,
  flags: wp_presentation_feedback::Kind,
}

impl Presented {
  /// Splits a presented frame into the protocol's 32-bit fields.
  fn new(info: &PresentInfo) -> Self {
    let seconds = info.time.as_secs();
    Self {
      tv_sec_hi: (seconds >> 32) as u32,
      tv_sec_lo: seconds as u32,
      tv_nsec: info.time.subsec_nanos(),
      refresh: info.refresh_ns,
      seq_hi: (info.seq >> 32) as u32,
      seq_lo: info.seq as u32,
      flags: info.flags,
    }
  }
}

/// Sends feedback for a presented frame: `presented` for surfaces in it,
/// `discarded` for the rest.
pub(crate) fn frame(state: &mut CompositorState, info: &PresentInfo) {
  let visible = state
    .visible_surfaces()
    .into_iter()
    .map(|surface| surface.id)
    .collect::<HashSet<_>>();
  let pending = std::mem::take(&mut state.presentation.pending);
  let presented = Presented::new(info);
  for feedback in pending {
    if !visible.contains(&feedback.surface) {
      feedback.resource.discarded();
      continue;
    }
    let client = state.surface_ids.get(&feedback.surface).and_then(|object| {
      state
        .surfaces
        .get(object)
        .map(|surface| surface.client.clone())
    });
    for output in state.outputs.values().filter(|output| {
      client.as_ref().is_some_and(|client| {
        output
          .client()
          .is_some_and(|output_client| output_client.id() == *client)
      })
    }) {
      feedback.resource.sync_output(output);
    }
    feedback.resource.presented(
      presented.tv_sec_hi,
      presented.tv_sec_lo,
      presented.tv_nsec,
      presented.refresh,
      presented.seq_hi,
      presented.seq_lo,
      presented.flags,
    );
  }
}

pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  surface: SurfaceId,
) {
  let mut discarded = Vec::new();
  state.presentation.pending.retain(|feedback| {
    if feedback.surface == surface {
      discarded.push(feedback.resource.clone());
      false
    } else {
      true
    }
  });
  for feedback in discarded {
    feedback.discarded();
  }
}

struct PresentationData;
struct FeedbackData;

impl GlobalDispatch<WpPresentation, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wp_presentation");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpPresentation>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let resource = data_init.init(resource, PresentationData);
    resource.clock_id(1);
  }
}

impl Dispatch<WpPresentation, PresentationData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &WpPresentation,
    request: wp_presentation::Request,
    _data: &PresentationData,
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wp_presentation::Request::Destroy => {}
      wp_presentation::Request::Feedback { surface, callback } => {
        let callback = data_init.init(callback, FeedbackData);
        feedback(state, &surface, callback);
      }
      _ => {}
    }
  }
}

fn feedback(
  state: &mut CompositorState,
  surface: &WlSurface,
  resource: WpPresentationFeedback,
) {
  let Some(surface) =
    state.surfaces.get(&surface.id()).map(|surface| surface.id)
  else {
    resource.discarded();
    return;
  };
  state
    .presentation
    .pending
    .push(PendingFeedback { surface, resource });
}

impl Dispatch<WpPresentationFeedback, FeedbackData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WpPresentationFeedback,
    _request: wp_presentation_feedback::Request,
    _data: &FeedbackData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WpPresentationFeedback,
    _data: &FeedbackData,
  ) {
    state
      .presentation
      .pending
      .retain(|feedback| feedback.resource.id() != resource.id());
  }
}

pub(crate) fn cleanup_client(
  state: &mut CompositorState,
  disconnected: &ClientId,
) {
  state.presentation.pending.retain(|feedback| {
    feedback
      .resource
      .client()
      .is_none_or(|client| client.id() != *disconnected)
  });
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  #[test]
  fn refresh_ns_rounds_millihertz_to_nanoseconds() {
    assert_eq!(refresh_ns(60_000), 16_666_667);
    assert_eq!(refresh_ns(59_951), 16_680_289);
    assert_eq!(refresh_ns(144_000), 6_944_444);
    assert_eq!(refresh_ns(0), 0);
    assert_eq!(refresh_ns(1), u32::MAX);
  }

  #[test]
  fn presented_splits_time_and_sequence_into_protocol_fields() {
    use wp_presentation_feedback::Kind;
    let info = PresentInfo {
      time: Duration::new((7 << 32) + 5, 250_000_123),
      refresh_ns: 16_666_667,
      seq: (3 << 32) + 9,
      flags: Kind::Vsync | Kind::HwClock | Kind::HwCompletion,
    };
    assert_eq!(
      Presented::new(&info),
      Presented {
        tv_sec_hi: 7,
        tv_sec_lo: 5,
        tv_nsec: 250_000_123,
        refresh: 16_666_667,
        seq_hi: 3,
        seq_lo: 9,
        flags: Kind::Vsync | Kind::HwClock | Kind::HwCompletion,
      }
    );
  }
}
