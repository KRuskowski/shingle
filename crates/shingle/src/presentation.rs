use std::collections::HashSet;

use wayland_protocols::wp::presentation_time::server::{
  wp_presentation::{self, WpPresentation},
  wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ClientId, protocol::wl_surface::WlSurface,
};

use crate::{SurfaceId, state::CompositorState};

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

pub(crate) fn frame(state: &mut CompositorState) {
  let visible = state
    .visible_surfaces()
    .into_iter()
    .map(|surface| surface.id)
    .collect::<HashSet<_>>();
  let pending = std::mem::take(&mut state.presentation.pending);
  let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
  let seconds = time.tv_sec as u64;
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
      (seconds >> 32) as u32,
      seconds as u32,
      time.tv_nsec as u32,
      16_666_667,
      0,
      0,
      wp_presentation_feedback::Kind::empty(),
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
