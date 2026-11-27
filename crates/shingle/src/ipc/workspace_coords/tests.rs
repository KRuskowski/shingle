//! IPC geometries of views on other workspaces, relative to the current
//! one as in Wayfire.

use serde_json::{Value, json};
use shingle_core::{Rect, WindowId, WorkspaceId};

use super::Offset;
use crate::{
  Compositor, Config, OutputInfo, test_client::TestClient,
  tests::test_compositor_with,
};

fn ipc(compositor: &mut Compositor, method: &str, data: Value) -> Value {
  crate::ipc::respond(
    &mut compositor.state,
    &json!({"method": method, "data": data}),
  )
}

fn ipc_events(compositor: &mut Compositor) -> Vec<Value> {
  compositor.drain_events();
  std::mem::take(&mut compositor.state.ipc_events)
}

/// The names of `events`, in order, with the view id of view events.
fn names(events: &[Value]) -> Vec<(String, Option<u64>)> {
  events
    .iter()
    .map(|event| {
      (
        event["event"].as_str().unwrap_or_default().to_owned(),
        event["view"]["id"].as_u64(),
      )
    })
    .collect()
}

/// The `view-geometry-changed` events of `window`.
fn geometry_changes(events: &[Value], window: WindowId) -> Vec<&Value> {
  events
    .iter()
    .filter(|event| {
      event["event"] == "view-geometry-changed"
        && event["view"]["id"] == window.0
    })
    .collect()
}

fn rect(value: &Value) -> Rect {
  crate::ipc::rect_of(value).expect("a geometry")
}

fn shifted(rect: Rect, dx: i32, dy: i32) -> Rect {
  Rect::new(rect.x + dx, rect.y + dy, rect.width, rect.height)
}

fn view(compositor: &mut Compositor, window: WindowId) -> Value {
  ipc(
    compositor,
    "window-rules/view-info",
    json!({"id": window.0}),
  )["info"]
    .clone()
}

fn geometry_of(compositor: &mut Compositor, window: WindowId) -> Rect {
  rect(&view(compositor, window)["geometry"])
}

fn workspace_of(compositor: &Compositor, window: WindowId) -> WorkspaceId {
  compositor
    .core()
    .window_info(window)
    .unwrap()
    .location
    .workspace
}

/// A mapped 64x64 toplevel's window.
fn mapped(
  compositor: &mut Compositor,
  client: &mut TestClient,
  app_id: &str,
) -> WindowId {
  let toplevel = client.toplevel(compositor, app_id);
  client.attach_buffer(compositor, &toplevel.surface, 64, 64);
  client.roundtrip(compositor);
  compositor.focused_window().expect("the toplevel maps")
}

fn send_view(compositor: &mut Compositor, window: WindowId, x: u32, y: u32) {
  let reply = ipc(
    compositor,
    "vswitch/send-view",
    json!({"view-id": window.0, "x": x, "y": y}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
}

fn switch(compositor: &mut Compositor, output: u32, x: u32, y: u32) {
  let reply = ipc(
    compositor,
    "vswitch/set-workspace",
    json!({"output-id": output, "x": x, "y": y}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
}

fn configure(compositor: &mut Compositor, window: WindowId, rect: Value) {
  let reply = ipc(
    compositor,
    "window-rules/configure-view",
    json!({"id": window.0, "geometry": rect}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
}

/// Whether `events` hold a `view-workspace-changed`.
fn moved_events(events: &[Value]) -> bool {
  events
    .iter()
    .any(|event| event["event"] == "view-workspace-changed")
}

fn floating(compositor: &Compositor, window: WindowId) -> Option<Rect> {
  compositor
    .core()
    .window_info(window)
    .unwrap()
    .state
    .floating
}

#[test]
fn offsets_move_rectangles_and_saturate() {
  let offset = Offset { dx: 1280, dy: -720 };
  let rect = Rect::new(10, 20, 30, 40);
  assert_eq!(offset.apply(rect), Rect::new(1290, -700, 30, 40));
  assert_eq!(offset.remove(offset.apply(rect)), rect);
  let far = Offset {
    dx: i64::from(i32::MAX),
    dy: i64::from(i32::MIN) - 100,
  };
  assert_eq!(far.apply(rect), Rect::new(i32::MAX, i32::MIN, 30, 40));
}

#[test]
fn a_view_one_workspace_right_is_one_output_width_right() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "away");
  let home = geometry_of(&mut compositor, window);
  send_view(&mut compositor, window, 1, 0);
  let info = view(&mut compositor, window);
  for key in ["geometry", "base-geometry", "bbox"] {
    assert_eq!(rect(&info[key]), shifted(home, 1280, 0), "{key}: {info}");
  }
  let listed = ipc(&mut compositor, "window-rules/list-views", json!({}));
  assert_eq!(rect(&listed[0]["geometry"]), shifted(home, 1280, 0));
  // Its own workspace still has it where it was.
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::new(1, 0));
  send_view(&mut compositor, window, 3, 0);
  assert_eq!(geometry_of(&mut compositor, window), shifted(home, 3840, 0));
  let _ = std::fs::remove_file(path);
}

#[test]
fn sending_a_view_reports_its_moved_geometry_first() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "sent");
  let home = geometry_of(&mut compositor, window);
  ipc_events(&mut compositor);
  send_view(&mut compositor, window, 2, 0);
  let events = ipc_events(&mut compositor);
  let changes = geometry_changes(&events, window);
  assert_eq!(changes.len(), 1, "{events:?}");
  assert_eq!(rect(&changes[0]["old-geometry"]), home);
  assert_eq!(
    rect(&changes[0]["view"]["geometry"]),
    shifted(home, 2560, 0)
  );
  let order = names(&events);
  let moved = order
    .iter()
    .position(|(name, _)| name == "view-workspace-changed")
    .expect("view-workspace-changed");
  let changed = order
    .iter()
    .position(|(name, _)| name == "view-geometry-changed")
    .unwrap();
  assert!(changed < moved, "{order:?}");
  let _ = std::fs::remove_file(path);
}

#[test]
fn switching_moves_every_view_but_sticky_ones_and_reports_them_first() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let away = mapped(&mut compositor, &mut client, "away");
  let stay = mapped(&mut compositor, &mut client, "stay");
  let pinned = mapped(&mut compositor, &mut client, "pinned");
  configure(
    &mut compositor,
    pinned,
    json!({"x": 40, "y": 30, "width": 300, "height": 200}),
  );
  let reply = ipc(
    &mut compositor,
    "wm-actions/set-sticky",
    json!({"view_id": pinned.0, "state": true}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  send_view(&mut compositor, away, 1, 0);
  client.roundtrip(&mut compositor);
  ipc_events(&mut compositor);
  let away_before = geometry_of(&mut compositor, away);
  let stay_before = geometry_of(&mut compositor, stay);
  let pinned_before = geometry_of(&mut compositor, pinned);
  assert!(away_before.x >= 1280, "{away_before:?}");
  switch(&mut compositor, 1, 1, 0);
  let events = ipc_events(&mut compositor);
  assert_eq!(
    geometry_of(&mut compositor, away),
    shifted(away_before, -1280, 0)
  );
  assert_eq!(
    geometry_of(&mut compositor, stay),
    shifted(stay_before, -1280, 0)
  );
  assert_eq!(geometry_of(&mut compositor, pinned), pinned_before);
  for (window, before) in [(away, away_before), (stay, stay_before)] {
    let changes = geometry_changes(&events, window);
    assert_eq!(changes.len(), 1, "{window:?}: {events:?}");
    assert_eq!(rect(&changes[0]["old-geometry"]), before);
    assert_eq!(
      rect(&changes[0]["view"]["geometry"]),
      shifted(before, -1280, 0)
    );
  }
  assert!(geometry_changes(&events, pinned).is_empty(), "{events:?}");
  let order = names(&events);
  let switched = order
    .iter()
    .position(|(name, _)| name == "wset-workspace-changed")
    .expect("wset-workspace-changed");
  assert!(
    order[..switched]
      .iter()
      .filter(|(name, _)| name == "view-geometry-changed")
      .count()
      == 2,
    "{order:?}"
  );
  // Back again, the same views move back.
  switch(&mut compositor, 1, 0, 0);
  let events = ipc_events(&mut compositor);
  assert_eq!(geometry_changes(&events, away).len(), 1);
  assert_eq!(geometry_changes(&events, stay).len(), 1);
  assert_eq!(geometry_of(&mut compositor, away), away_before);
  assert_eq!(geometry_of(&mut compositor, stay), stay_before);
  let _ = std::fs::remove_file(path);
}

#[test]
fn rows_are_one_output_height_apart() {
  let config = Config {
    workspace_width: 2,
    workspace_height: 2,
    ..Config::default()
  };
  let (mut compositor, path) = test_compositor_with(config);
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "below");
  let home = geometry_of(&mut compositor, window);
  send_view(&mut compositor, window, 1, 1);
  assert_eq!(
    geometry_of(&mut compositor, window),
    shifted(home, 1280, 720)
  );
  switch(&mut compositor, 1, 1, 0);
  assert_eq!(geometry_of(&mut compositor, window), shifted(home, 0, 720));
  switch(&mut compositor, 1, 0, 1);
  assert_eq!(geometry_of(&mut compositor, window), shifted(home, 1280, 0));
  let _ = std::fs::remove_file(path);
}

#[test]
fn get_layout_is_relative_to_the_current_workspace() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "tiled");
  let layout = |compositor: &mut Compositor, x: u32| {
    ipc(
      compositor,
      "simple-tile/get-layout",
      json!({"wset-index": 1, "workspace": {"x": x, "y": 0}}),
    )["layout"]
      .clone()
  };
  let home = layout(&mut compositor, 0);
  let root = rect(&home["geometry"]);
  let tile = rect(&home["vertical-split"][0]["geometry"]);
  send_view(&mut compositor, window, 1, 0);
  let there = layout(&mut compositor, 1);
  assert_eq!(rect(&there["geometry"]), shifted(root, 1280, 0), "{there}");
  assert_eq!(
    rect(&there["vertical-split"][0]["geometry"]),
    shifted(tile, 1280, 0)
  );
  assert_eq!(there["vertical-split"][0]["view-id"], window.0);
  switch(&mut compositor, 1, 1, 0);
  let there = layout(&mut compositor, 1);
  assert_eq!(rect(&there["vertical-split"][0]["geometry"]), tile);
  // An empty workspace's root, to the left.
  assert_eq!(
    rect(&layout(&mut compositor, 0)["geometry"]),
    shifted(root, -1280, 0)
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_rectangle_on_another_workspace_moves_the_view_there() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "moved");
  let tile = geometry_of(&mut compositor, window);
  ipc_events(&mut compositor);
  let target = json!({"x": 1380, "y": 20, "width": 200, "height": 100});
  configure(&mut compositor, window, target.clone());
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::new(1, 0));
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(100, 20, 200, 100))
  );
  assert_eq!(view(&mut compositor, window)["geometry"], target);
  let events = ipc_events(&mut compositor);
  let changes = geometry_changes(&events, window);
  assert_eq!(changes.len(), 1, "{events:?}");
  assert_eq!(rect(&changes[0]["old-geometry"]), tile);
  assert_eq!(changes[0]["view"]["geometry"], target);
  // Wayfire's configure-view only sets the geometry, and sends no
  // view-workspace-changed (wayfire:plugins/ipc-rules/ipc-rules.cpp:302).
  assert!(!moved_events(&events), "{events:?}");
  // The centre decides: this one is mostly on the first workspace.
  configure(
    &mut compositor,
    window,
    json!({"x": 1180, "y": 20, "width": 160, "height": 100}),
  );
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::ORIGIN);
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(1180, 20, 160, 100))
  );
  // Past the grid's first column it stays on the closest workspace.
  configure(
    &mut compositor,
    window,
    json!({"x": -500, "y": 20, "width": 200, "height": 100}),
  );
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::ORIGIN);
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(-500, 20, 200, 100))
  );
  // Seen from the workspace it is on, a rectangle is its own.
  configure(
    &mut compositor,
    window,
    json!({"x": 2600, "y": 20, "width": 200, "height": 100}),
  );
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::new(2, 0));
  switch(&mut compositor, 1, 2, 0);
  configure(
    &mut compositor,
    window,
    json!({"x": 30, "y": 20, "width": 200, "height": 100}),
  );
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::new(2, 0));
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(30, 20, 200, 100))
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_sticky_view_keeps_its_workspace_and_rectangle() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "sticky");
  let reply = ipc(
    &mut compositor,
    "wm-actions/set-sticky",
    json!({"view_id": window.0, "state": true}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  configure(
    &mut compositor,
    window,
    json!({"x": 1380, "y": 20, "width": 200, "height": 100}),
  );
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::ORIGIN);
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(1380, 20, 200, 100))
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn layout_views_places_views_on_other_workspaces() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "laid");
  let reply = ipc(
    &mut compositor,
    "stipc/layout_views",
    json!({"views": [
      {"id": window.0, "x": 1300, "y": 10, "width": 100, "height": 50},
    ]}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::new(1, 0));
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(20, 10, 100, 50))
  );
  let events = ipc_events(&mut compositor);
  assert!(!moved_events(&events), "{events:?}");
  assert_eq!(geometry_changes(&events, window).len(), 1, "{events:?}");
  assert_eq!(
    geometry_of(&mut compositor, window),
    Rect::new(1300, 10, 100, 50)
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn each_output_offsets_by_its_own_size() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .expect("a second output");
  let mut client = TestClient::connect(&mut compositor, &path);
  let window = mapped(&mut compositor, &mut client, "second");
  let reply = ipc(
    &mut compositor,
    "window-rules/configure-view",
    json!({
      "id": window.0,
      "output-id": 2,
      "geometry": {"x": 1300, "y": 20, "width": 200, "height": 100},
    }),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::ORIGIN);
  send_view(&mut compositor, window, 1, 0);
  assert_eq!(
    geometry_of(&mut compositor, window),
    Rect::new(2100, 20, 200, 100)
  );
  // Switching the first output's workspace leaves it alone.
  ipc_events(&mut compositor);
  switch(&mut compositor, 1, 1, 0);
  let events = ipc_events(&mut compositor);
  assert!(geometry_changes(&events, window).is_empty(), "{events:?}");
  switch(&mut compositor, 2, 1, 0);
  let events = ipc_events(&mut compositor);
  assert_eq!(geometry_changes(&events, window).len(), 1, "{events:?}");
  assert_eq!(
    geometry_of(&mut compositor, window),
    Rect::new(1300, 20, 200, 100)
  );
  // A rectangle beyond the second output's right edge, in its own
  // terms, is its next workspace.
  configure(
    &mut compositor,
    window,
    json!({"x": 2110, "y": 20, "width": 200, "height": 100}),
  );
  assert_eq!(workspace_of(&compositor, window), WorkspaceId::new(2, 0));
  assert_eq!(
    floating(&compositor, window),
    Some(Rect::new(1310, 20, 200, 100))
  );
  let _ = std::fs::remove_file(path);
}
