//! `stipc/object_counts`: how many objects of each kind the compositor
//! and its frontend hold, so that a soak test can tell a leak of Wayland
//! objects, buffers or textures from a busy moment (tools/soak_test).
//!
//! Every count is a collection's length, so the reply costs nothing while
//! nobody asks. A count that returns to its earlier value whenever the same
//! clients run is healthy; one that grows with each client that came and
//! went is a leak.

use serde_json::{Map, Value, json};

use crate::state::CompositorState;

/// The reply to `stipc/object_counts`: `result` and one number per kind
/// of object, with the frontend's own counts under `frontend` (empty for
/// the headless backend, which has no frontend).
pub(crate) fn reply(state: &CompositorState) -> Value {
  let (queued_surfaces, queued_commits) = state.commit_queue.totals();
  let (data_sources, data_devices, data_offers) = state.data_devices.totals();
  let (control_sources, control_devices, control_offers) =
    state.data_control.totals();
  let (primary_sources, primary_devices, primary_offers) =
    state.primary_selection.totals();
  let bound_outputs = state
    .outputs
    .iter()
    .map(|output| output.bound.len())
    .sum::<usize>();
  let frontend = state
    .frontend_counts
    .iter()
    .map(|(name, count)| ((*name).to_owned(), json!(count)))
    .collect::<Map<_, _>>();
  json!({
    "result": "ok",
    "clients": state.launch_templates.len(),
    "ipc_clients": state.ipc_clients,
    "xwayland_running": usize::from(state.xwayland.pid_or_minus_one() > 0),
    "views": state.core.window_ids().count(),
    "surfaces": state.surfaces.len(),
    "surface_ids": state.surface_ids.len(),
    "surface_outputs": state.surface_outputs.len(),
    "subsurface_orders": state.subsurface_order.len(),
    "regions": state.regions.len(),
    "viewports": state.viewports.len(),
    "fractional_scales": state.fractional_scales.len(),
    "shm_pools": state.pools.len(),
    "buffers": state.buffers.len(),
    "queued_surfaces": queued_surfaces,
    "queued_commits": queued_commits,
    "frame_callbacks": state.frame_callbacks.len(),
    "presentation_feedbacks": state.presentation.pending(),
    "xdg_outputs": state.xdg_outputs.len(),
    "decorations": state.decorations.len(),
    "data_sources": data_sources,
    "data_devices": data_devices,
    "data_offers": data_offers,
    "data_control_sources": control_sources,
    "data_control_devices": control_devices,
    "data_control_offers": control_offers,
    "primary_sources": primary_sources,
    "primary_devices": primary_devices,
    "primary_offers": primary_offers,
    "keyboards": state.input.keyboards.len(),
    "pointers": state.input.pointers.len(),
    "touches": state.input.touches.len(),
    "bound_outputs": bound_outputs,
    "pending_maps": state.pending_maps.len(),
    "focus_recency": state.focus_recency.len(),
    "view_properties": state.view_properties.len(),
    "frontend": frontend,
  })
}

#[cfg(test)]
mod tests {
  use crate::{ipc, test_client::TestClient, tests::test_compositor};

  fn counts(compositor: &mut crate::Compositor) -> serde_json::Value {
    ipc::respond(
      &mut compositor.state,
      &serde_json::json!({"method": "stipc/object_counts"}),
    )
  }

  #[test]
  fn counts_return_to_their_start_when_a_client_has_gone() {
    let (mut compositor, path) = test_compositor();
    let before = counts(&mut compositor);
    assert_eq!(before["result"], "ok", "{before}");
    assert_eq!(before["clients"], 0);
    assert_eq!(before["surfaces"], 0);
    assert_eq!(before["frontend"], serde_json::json!({}));
    let mut client = TestClient::connect(&mut compositor, &path);
    let toplevel = client.toplevel(&mut compositor, "counted");
    client.attach_buffer(&mut compositor, &toplevel.surface, 40, 30);
    let during = counts(&mut compositor);
    assert_eq!(during["clients"], 1, "{during}");
    assert_eq!(during["surfaces"], 1, "{during}");
    assert_eq!(during["views"], 1, "{during}");
    assert_eq!(during["buffers"], 1, "{during}");
    drop(toplevel);
    drop(client);
    for _ in 0..10 {
      compositor
        .dispatch_pending()
        .expect("the compositor dispatches");
    }
    let after = counts(&mut compositor);
    for key in ["clients", "surfaces", "surface_ids", "views", "buffers"] {
      assert_eq!(after[key], before[key], "{key}: {after}");
    }
  }

  #[test]
  fn frame_callbacks_of_hidden_surfaces_go_with_the_surface() {
    use wayland_client::protocol::wl_compositor::WlCompositor;
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    // A surface without a role is never drawn, so its callback waits, as
    // one of a window on a hidden output or workspace does.
    let surface = client
      .bind::<WlCompositor>(4)
      .create_surface(&client.handle, ());
    surface.frame(&client.handle, crate::test_client::Frame);
    surface.commit();
    client.roundtrip(&mut compositor);
    compositor.frame();
    assert_eq!(counts(&mut compositor)["frame_callbacks"], 1);
    surface.destroy();
    client.roundtrip(&mut compositor);
    compositor.frame();
    assert_eq!(counts(&mut compositor)["frame_callbacks"], 0);
  }

  #[test]
  fn the_frontend_counts_are_reported_as_last_set() {
    let (mut compositor, _path) = test_compositor();
    compositor.set_frontend_counts(&[("textures", 3), ("dmabuf_textures", 1)]);
    let reply = counts(&mut compositor);
    assert_eq!(
      reply["frontend"],
      serde_json::json!({"textures": 3, "dmabuf_textures": 1})
    );
    compositor.set_frontend_counts(&[("textures", 0)]);
    assert_eq!(
      counts(&mut compositor)["frontend"],
      serde_json::json!({"textures": 0})
    );
  }
}
