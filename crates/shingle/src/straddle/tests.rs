//! Windows drawn and hit on every workspace they overlap, through the
//! in-process test client.

use serde_json::{Value, json};
use shingle_core::{Rect, WindowId, WorkspaceId};
use wayland_client::protocol::wl_pointer;

use super::{Stacked, merge_by_recency, shift_between};
use crate::{
  Compositor, Config, OutputHandle, SurfaceRole,
  test_client::{TestClient, Toplevel},
  tests::test_compositor_with,
};

const LEFT: u32 = 0x110;

fn ipc(compositor: &mut Compositor, method: &str, data: Value) -> Value {
  crate::ipc::respond(
    &mut compositor.state,
    &json!({"method": method, "data": data}),
  )
}

/// A compositor on one 1280x720 output with a 3x2 grid, and a client.
fn fixture() -> (Compositor, TestClient, std::path::PathBuf) {
  let config = Config {
    workspace_width: 3,
    workspace_height: 2,
    ..Config::default()
  };
  let (mut compositor, path) = test_compositor_with(config);
  let client = TestClient::connect(&mut compositor, &path);
  (compositor, client, path)
}

/// A mapped toplevel with an opaque black 64x64 buffer, and its window.
fn mapped(
  compositor: &mut Compositor,
  client: &mut TestClient,
  app_id: &str,
) -> (Toplevel, WindowId) {
  let toplevel = client.toplevel(compositor, app_id);
  let buffer = client.opaque_buffer(64, 64);
  toplevel.surface.attach(Some(&buffer), 0, 0);
  toplevel.surface.commit();
  client.roundtrip(compositor);
  let window = compositor.focused_window().expect("the toplevel maps");
  (toplevel, window)
}

/// Floats `window` at `rect`, relative to the current workspace as IPC's
/// rectangles are; it goes to the workspace holding the centre.
fn place(compositor: &mut Compositor, window: WindowId, rect: Rect) {
  let reply = ipc(
    compositor,
    "window-rules/configure-view",
    json!({"id": window.0, "geometry": {
      "x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height,
    }}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
}

fn switch(compositor: &mut Compositor, x: u32, y: u32) {
  let reply = ipc(
    compositor,
    "vswitch/set-workspace",
    json!({"output-id": 1, "x": x, "y": y}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
}

fn workspace_of(compositor: &Compositor, window: WindowId) -> WorkspaceId {
  compositor
    .core()
    .window_info(window)
    .unwrap()
    .location
    .workspace
}

/// The toplevel snapshots drawn on the output, bottom to top, as
/// (window, geometry, shift).
fn drawn(compositor: &Compositor) -> Vec<(WindowId, Rect, (i32, i32))> {
  compositor
    .visible_surfaces_on(&OutputHandle::new("HEADLESS-1"))
    .into_iter()
    .filter(|snapshot| snapshot.role == SurfaceRole::Toplevel)
    .filter_map(|snapshot| {
      Some((snapshot.window?, snapshot.geometry, snapshot.shift))
    })
    .collect()
}

fn click(compositor: &mut Compositor, client: &mut TestClient, x: f64, y: f64) {
  crate::input::pointer_motion(&mut compositor.state, x, y);
  crate::input::pointer_button(&mut compositor.state, LEFT, true);
  crate::input::pointer_button(&mut compositor.state, LEFT, false);
  client.roundtrip(compositor);
}

#[test]
fn the_shift_is_the_output_size_per_workspace_and_saturates() {
  let size = (1280, 720);
  let at = WorkspaceId::new;
  assert_eq!(shift_between(at(0, 0), at(0, 0), size), (0, 0));
  assert_eq!(shift_between(at(0, 0), at(1, 0), size), (-1280, 0));
  assert_eq!(shift_between(at(2, 1), at(0, 0), size), (2560, 720));
  assert_eq!(shift_between(at(0, 0), at(1, 1), size), (-1280, -720));
  let huge = (u32::MAX, u32::MAX);
  assert_eq!(
    shift_between(at(0, 0), at(5, 5), huge),
    (i32::MIN, i32::MIN)
  );
  assert_eq!(
    shift_between(at(5, 5), at(0, 0), huge),
    (i32::MAX, i32::MAX)
  );
}

#[test]
fn foreign_windows_go_below_the_first_window_focused_after_them() {
  let recency = |window: u32| u64::from(window);
  // Nothing to merge keeps the order, also one against focus order.
  assert_eq!(
    merge_by_recency(&[3, 1], &[], recency),
    vec![Stacked::Own(3), Stacked::Own(1)]
  );
  // 2 was focused after 1 and before 3.
  assert_eq!(
    merge_by_recency(&[1, 3], &[2], recency),
    vec![Stacked::Own(1), Stacked::Foreign(2), Stacked::Own(3)]
  );
  // Focused last, it is on top; focused first, at the bottom.
  assert_eq!(
    merge_by_recency(&[1, 3], &[5, 0], recency),
    vec![
      Stacked::Foreign(0),
      Stacked::Own(1),
      Stacked::Own(3),
      Stacked::Foreign(5),
    ]
  );
  // The own order stays even where focus would reorder it.
  assert_eq!(
    merge_by_recency(&[4, 1], &[2], recency),
    vec![Stacked::Foreign(2), Stacked::Own(4), Stacked::Own(1)]
  );
  // Two foreign windows go in their order of focus.
  assert_eq!(
    merge_by_recency(&[], &[7, 6], recency),
    vec![Stacked::Foreign(6), Stacked::Foreign(7)]
  );
}

#[test]
fn a_window_hanging_over_is_drawn_on_the_neighbour_too() {
  let (mut compositor, mut client, path) = fixture();
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  // Its centre, 1250, is on the first workspace; it ends at 1400.
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  assert_eq!(workspace_of(&compositor, left), WorkspaceId::new(0, 0));
  assert_eq!(
    drawn(&compositor),
    vec![(left, Rect::new(1100, 100, 300, 200), (0, 0))]
  );
  switch(&mut compositor, 1, 0);
  // The part past the first workspace's edge shows at the left edge.
  assert_eq!(
    drawn(&compositor),
    vec![(left, Rect::new(-180, 100, 300, 200), (-1280, 0))]
  );
  // It stays on its own workspace, for IPC and the session.
  assert_eq!(workspace_of(&compositor, left), WorkspaceId::new(0, 0));
  let info = ipc(
    &mut compositor,
    "window-rules/view-info",
    json!({"id": left.0}),
  );
  assert_eq!(info["info"]["geometry"]["x"], -180);
  // Workspaces it does not overlap do not draw it.
  switch(&mut compositor, 2, 0);
  assert!(drawn(&compositor).is_empty());
  switch(&mut compositor, 0, 1);
  assert!(drawn(&compositor).is_empty());
  let _ = std::fs::remove_file(path);
}

#[test]
fn every_workspace_a_window_overlaps_draws_its_part() {
  let (mut compositor, mut client, path) = fixture();
  switch(&mut compositor, 1, 1);
  let (_corner, corner) = mapped(&mut compositor, &mut client, "corner");
  // Over the top-left corner of the second row's middle workspace.
  place(&mut compositor, corner, Rect::new(-100, -50, 300, 200));
  assert_eq!(workspace_of(&compositor, corner), WorkspaceId::new(1, 1));
  let rect = Rect::new(-100, -50, 300, 200);
  for (x, y, shift) in [
    (0, 0, (1280, 720)),
    (1, 0, (0, 720)),
    (0, 1, (1280, 0)),
    (1, 1, (0, 0)),
  ] {
    switch(&mut compositor, x, y);
    assert_eq!(
      drawn(&compositor),
      vec![(corner, super::shifted(rect, shift), shift)],
      "on workspace {x}, {y}"
    );
  }
  for (x, y) in [(2, 0), (2, 1)] {
    switch(&mut compositor, x, y);
    assert!(drawn(&compositor).is_empty(), "on workspace {x}, {y}");
  }
  let _ = std::fs::remove_file(path);
}

#[test]
fn sticky_minimized_and_contained_windows_are_not_drawn_twice() {
  let (mut compositor, mut client, path) = fixture();
  let (_inside, inside) = mapped(&mut compositor, &mut client, "inside");
  place(&mut compositor, inside, Rect::new(100, 100, 300, 200));
  let (_hidden, hidden) = mapped(&mut compositor, &mut client, "hidden");
  place(&mut compositor, hidden, Rect::new(1100, 100, 300, 200));
  crate::shell::set_minimized(&mut compositor.state, hidden, true).unwrap();
  let (_sticky, sticky) = mapped(&mut compositor, &mut client, "sticky");
  place(&mut compositor, sticky, Rect::new(1100, 400, 300, 200));
  let reply = ipc(
    &mut compositor,
    "wm-actions/set-sticky",
    json!({"view_id": sticky.0, "state": true}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  switch(&mut compositor, 1, 0);
  // The sticky window is drawn in place, once.
  assert_eq!(
    drawn(&compositor),
    vec![(sticky, Rect::new(1100, 400, 300, 200), (0, 0))]
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_click_on_the_part_shown_focuses_it_without_switching() {
  let (mut compositor, mut client, path) = fixture();
  let seat = client.bind::<wayland_client::protocol::wl_seat::WlSeat>(7);
  let _pointer = seat.get_pointer(&client.handle, ());
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  switch(&mut compositor, 1, 0);
  let (_here, here) = mapped(&mut compositor, &mut client, "here");
  place(&mut compositor, here, Rect::new(400, 400, 300, 200));
  assert_eq!(compositor.focused_window(), Some(here));
  client.events.pointer.clear();
  click(&mut compositor, &mut client, 50.0, 150.0);
  assert_eq!(compositor.focused_window(), Some(left));
  assert_eq!(
    compositor
      .core()
      .current_workspace(&"HEADLESS-1".into())
      .unwrap(),
    WorkspaceId::new(1, 0),
    "the click switches nothing"
  );
  assert_eq!(workspace_of(&compositor, left), WorkspaceId::new(0, 0));
  // The client gets the point on its surface where it is drawn.
  let entered = client.events.pointer.iter().find_map(|event| match event {
    wl_pointer::Event::Enter {
      surface_x,
      surface_y,
      ..
    } => Some((*surface_x, *surface_y)),
    _ => None,
  });
  assert_eq!(entered, Some((230.0, 50.0)));
  crate::input::pointer_motion(&mut compositor.state, 60.0, 160.0);
  client.roundtrip(&mut compositor);
  let moved =
    client
      .events
      .pointer
      .iter()
      .rev()
      .find_map(|event| match event {
        wl_pointer::Event::Motion {
          surface_x,
          surface_y,
          ..
        } => Some((*surface_x, *surface_y)),
        _ => None,
      });
  assert_eq!(moved, Some((240.0, 60.0)));
  // Past the part shown, the click is on the empty workspace.
  click(&mut compositor, &mut client, 150.0, 150.0);
  assert_eq!(compositor.focused_window(), Some(left));
  let _ = std::fs::remove_file(path);
}

#[test]
fn the_window_focused_last_is_on_top_where_they_overlap() {
  let (mut compositor, mut client, path) = fixture();
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  switch(&mut compositor, 1, 0);
  let (_here, here) = mapped(&mut compositor, &mut client, "here");
  place(&mut compositor, here, Rect::new(0, 250, 300, 200));
  let order = |compositor: &Compositor| {
    drawn(compositor)
      .into_iter()
      .map(|(window, ..)| window)
      .collect::<Vec<_>>()
  };
  // Focused after the window hanging over, `here` is above it.
  assert_eq!(order(&compositor), vec![left, here]);
  click(&mut compositor, &mut client, 50.0, 150.0);
  assert_eq!(compositor.focused_window(), Some(left));
  assert_eq!(order(&compositor), vec![here, left]);
  // Where both are drawn, the one on top takes the click.
  click(&mut compositor, &mut client, 50.0, 280.0);
  assert_eq!(compositor.focused_window(), Some(left));
  click(&mut compositor, &mut client, 200.0, 280.0);
  assert_eq!(compositor.focused_window(), Some(here));
  assert_eq!(order(&compositor), vec![left, here]);
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_screencopy_of_the_neighbour_shows_the_part_hanging_over() {
  let (mut compositor, mut client, path) = fixture();
  let outputs =
    crate::output::protocol_tests::bind_outputs(&mut compositor, &mut client);
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  switch(&mut compositor, 1, 0);
  client.roundtrip(&mut compositor);
  let output = &outputs["HEADLESS-1"];
  let (size, bytes) = crate::output::protocol_tests::capture(
    &mut compositor,
    &mut client,
    output,
    Some((110, 150, 20, 1)),
  )
  .expect("a capture");
  assert_eq!(size, (20, 1));
  // The black window to x 120, then the background, the byte 26.
  let pixels = bytes.chunks(4).collect::<Vec<_>>();
  assert!(pixels[..10].iter().all(|pixel| *pixel == [0, 0, 0, 255]));
  assert!(pixels[10..].iter().all(|pixel| *pixel == [26, 26, 26, 255]));
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_window_adopted_by_the_workspace_shown_stays_where_it_is_drawn() {
  let (mut compositor, mut client, path) = fixture();
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  // Its own workspace shown, nothing moves.
  assert!(!compositor.adopt_on_current_workspace(left).unwrap());
  switch(&mut compositor, 1, 0);
  click(&mut compositor, &mut client, 50.0, 150.0);
  assert!(compositor.adopt_on_current_workspace(left).unwrap());
  assert_eq!(workspace_of(&compositor, left), WorkspaceId::new(1, 0));
  assert_eq!(
    drawn(&compositor),
    vec![(left, Rect::new(-180, 100, 300, 200), (0, 0))]
  );
  assert_eq!(
    compositor.focused_window(),
    Some(left),
    "it keeps the focus"
  );
  switch(&mut compositor, 0, 0);
  assert_eq!(
    drawn(&compositor),
    vec![(left, Rect::new(1100, 100, 300, 200), (1280, 0))]
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_window_settles_on_the_workspace_holding_its_centre() {
  let (mut compositor, mut client, path) = fixture();
  let (_moved, moved) = mapped(&mut compositor, &mut client, "moved");
  place(&mut compositor, moved, Rect::new(100, 100, 300, 200));
  // Moved as a drag moves it, with its centre past the edge.
  crate::shell::set_floating_geometry(
    &mut compositor.state,
    moved,
    Rect::new(1200, 100, 300, 200),
  )
  .unwrap();
  assert_eq!(workspace_of(&compositor, moved), WorkspaceId::new(0, 0));
  assert!(compositor.settle_on_centre_workspace(moved).unwrap());
  assert_eq!(workspace_of(&compositor, moved), WorkspaceId::new(1, 0));
  // Drawn where it was put, from the neighbour.
  assert_eq!(
    drawn(&compositor),
    vec![(moved, Rect::new(1200, 100, 300, 200), (1280, 0))]
  );
  assert_eq!(compositor.focused_window(), Some(moved));
  assert!(!compositor.settle_on_centre_workspace(moved).unwrap());
  let _ = std::fs::remove_file(path);
}

#[test]
fn the_centre_rounds_down_and_the_far_edges_are_outside() {
  let area = Rect::new(0, 0, 1280, 720);
  let next = Rect::new(1280, 0, 1280, 720);
  // A centre at 1250 is on the first, one at 1350 on the second.
  assert!(super::centre_in(Rect::new(1100, 100, 300, 200), area));
  assert!(!super::centre_in(Rect::new(1100, 100, 300, 200), next));
  assert!(super::centre_in(Rect::new(1200, 100, 300, 200), next));
  // 1129 + 301 / 2 = 1279 is the last column.
  assert!(super::centre_in(Rect::new(1129, 0, 301, 10), area));
  assert!(!super::centre_in(Rect::new(1130, 0, 301, 10), area));
  assert!(super::centre_in(Rect::new(-150, -100, 300, 200), area));
  assert!(!super::centre_in(Rect::new(-151, 0, 300, 10), area));
}

/// Scale's windows of the current workspace on the output.
fn scaled(compositor: &Compositor, minimized: bool) -> Vec<WindowId> {
  let mut windows = compositor
    .scale_surfaces(false, minimized)
    .into_iter()
    .filter_map(|snapshot| snapshot.window)
    .collect::<Vec<_>>();
  windows.sort_unstable();
  windows
}

fn switchable(compositor: &Compositor) -> Vec<WindowId> {
  let mut windows = compositor.switchable_windows();
  windows.sort_unstable();
  windows
}

#[test]
fn the_switchers_list_a_window_on_every_workspace_it_overlaps() {
  let (mut compositor, mut client, path) = fixture();
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  let (_hidden, hidden) = mapped(&mut compositor, &mut client, "hidden");
  place(&mut compositor, hidden, Rect::new(1100, 400, 300, 200));
  crate::shell::set_minimized(&mut compositor.state, hidden, true).unwrap();
  let (_inside, inside) = mapped(&mut compositor, &mut client, "inside");
  place(&mut compositor, inside, Rect::new(100, 100, 300, 200));
  assert_eq!(switchable(&compositor), vec![left, hidden, inside]);
  switch(&mut compositor, 1, 0);
  let (_here, here) = mapped(&mut compositor, &mut client, "here");
  place(&mut compositor, here, Rect::new(400, 400, 300, 200));
  // Both windows hanging over are listed, the minimized one too, as
  // Wayfire's switcher lists them; the one inside its workspace is not.
  assert_eq!(switchable(&compositor), vec![left, hidden, here]);
  // Most recently focused first.
  assert_eq!(compositor.switchable_windows()[0], here);
  // Cycling back focuses the window focused first, which hangs over,
  // without a switch.
  assert_eq!(compositor.cycle_focus(true), Some(left));
  assert_eq!(compositor.focused_window(), Some(left));
  assert_eq!(
    compositor
      .core()
      .current_workspace(&"HEADLESS-1".into())
      .unwrap(),
    WorkspaceId::new(1, 0)
  );
  switch(&mut compositor, 2, 0);
  assert!(switchable(&compositor).is_empty());
  let _ = std::fs::remove_file(path);
}

#[test]
fn scale_lists_the_windows_whose_centre_is_on_the_output() {
  let (mut compositor, mut client, path) = fixture();
  let (_left, left) = mapped(&mut compositor, &mut client, "left");
  // Its centre is on the first workspace; it hangs over the second.
  place(&mut compositor, left, Rect::new(1100, 100, 300, 200));
  let (_moved, moved) = mapped(&mut compositor, &mut client, "moved");
  place(&mut compositor, moved, Rect::new(100, 400, 300, 200));
  // Moved by its client with its centre past the edge, without a drop
  // that would settle it on the neighbour.
  crate::shell::set_floating_geometry(
    &mut compositor.state,
    moved,
    Rect::new(1200, 400, 300, 200),
  )
  .unwrap();
  assert_eq!(workspace_of(&compositor, moved), WorkspaceId::new(0, 0));
  assert_eq!(scaled(&compositor, false), vec![left]);
  switch(&mut compositor, 1, 0);
  let (_here, here) = mapped(&mut compositor, &mut client, "here");
  place(&mut compositor, here, Rect::new(400, 100, 300, 200));
  // `left` is drawn here in part, its centre is not; `moved` has its
  // centre here, as Wayfire's Scale counts the workspace.
  assert_eq!(scaled(&compositor, false), vec![moved, here]);
  // All workspaces lists every window of the set.
  let mut all = compositor
    .scale_surfaces(true, false)
    .into_iter()
    .filter_map(|snapshot| snapshot.window)
    .collect::<Vec<_>>();
  all.sort_unstable();
  assert_eq!(all, vec![left, moved, here]);
  // A minimized window counts on its own workspace.
  crate::shell::set_minimized(&mut compositor.state, moved, true).unwrap();
  assert_eq!(scaled(&compositor, true), vec![here]);
  switch(&mut compositor, 0, 0);
  assert_eq!(scaled(&compositor, true), vec![left, moved]);
  let _ = std::fs::remove_file(path);
}
