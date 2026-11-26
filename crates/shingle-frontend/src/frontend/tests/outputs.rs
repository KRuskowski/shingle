//! Scenes per output (docs/design/multi-output.md, MO-5): each output
//! draws its own windows relative to its origin, asks for frames and
//! sends frame callbacks on its own, runs its own zoom and label, and
//! Scale stays on the output it started on.

use shingle::MoveGeometry;

use crate::frontend::outputs::translate;

use super::*;

/// HEADLESS-2, which [`add_second_output`] adds right of HEADLESS-1.
fn second() -> OutputHandle {
  OutputHandle::new("HEADLESS-2")
}

/// A fixture without open or geometry animations, so that every frame
/// shows windows where they are.
fn still() -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.tiling.animation_duration = 0;
  config.scale.duration_ms = 0;
  config
}

/// The frames drawn last into `target`, for the outputs in the order
/// they were drawn.
fn last_frames(
  target: &RecordingTarget,
  count: usize,
) -> &[Vec<(RenderRect, bool)>] {
  &target.frames[target.frames.len() - count..]
}

fn render(
  fixture: &mut Fixture,
  output: &OutputHandle,
  target: &mut RecordingTarget,
) {
  fixture
    .frontend
    .render(&mut fixture.host, output, target)
    .expect("the frame renders");
}

/// `window`'s geometry, tiled or floating, as a render rectangle moved by
/// (`dx`, `dy`).
fn drawn_at(
  fixture: &Fixture,
  window: TileWindowId,
  dx: i32,
  dy: i32,
) -> RenderRect {
  let geometry = fixture
    .frontend
    .window_geometry(window)
    .expect("the window has a geometry");
  RenderRect {
    x: geometry.x + dx,
    y: geometry.y + dy,
    width: geometry.width,
    height: geometry.height,
  }
}

/// Alpha on HEADLESS-2, bravo and charlie on HEADLESS-1, which is active.
fn split_windows(
  config: ShingleConfig,
) -> (Fixture, [TileWindowId; 3], [Toplevel; 3]) {
  let mut fixture = Fixture::new(config);
  let toplevels = ["alpha", "bravo", "charlie"].map(|id| fixture.window(id));
  let tiles = fixture.tiles();
  add_second_output(&mut fixture);
  let compositor = fixture.frontend.compositor_mut();
  compositor
    .move_window_to_output(tiles[0], &second(), MoveGeometry::Scale)
    .expect("alpha moves");
  compositor.focus_output(&primary());
  fixture.process();
  (fixture, [tiles[0], tiles[1], tiles[2]], toplevels)
}

#[test]
fn each_output_draws_its_own_windows_relative_to_its_origin() {
  // One target draws both outputs, as a host draws them on one device:
  // a texture belongs to the device that made it.
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, [alpha, bravo, charlie], _toplevels) =
    split_windows(still());
  render(&mut fixture, &primary(), &mut target);
  render(&mut fixture, &second(), &mut target);
  let alpha_rect = drawn_at(&fixture, alpha, -1000, 0);
  assert_eq!(alpha_rect.x, 0, "HEADLESS-2 starts at x = 1000");
  assert_eq!(
    last_frames(&target, 2),
    [
      vec![
        (drawn_at(&fixture, bravo, 0, 0), true),
        (drawn_at(&fixture, charlie, 0, 0), true),
      ],
      vec![(alpha_rect, true)],
    ]
  );
}

#[test]
fn a_commit_asks_for_frames_of_the_outputs_its_window_is_on_only() {
  // One target draws both outputs, as a host draws them on one device:
  // a texture belongs to the device that made it.
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, _, toplevels) = split_windows(still());
  render(&mut fixture, &primary(), &mut target);
  render(&mut fixture, &second(), &mut target);
  fixture.host.redrawn.clear();
  fixture.client.attach_buffer(
    fixture.frontend.compositor_mut(),
    &toplevels[0].surface,
    800,
    600,
  );
  fixture.frontend.process(&mut fixture.host);
  assert_eq!(fixture.host.redrawn, [second()]);
}

#[test]
fn presenting_one_output_sends_only_its_frame_callbacks() {
  // One target draws both outputs, as a host draws them on one device:
  // a texture belongs to the device that made it.
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, _, toplevels) = split_windows(still());
  render(&mut fixture, &primary(), &mut target);
  render(&mut fixture, &second(), &mut target);
  let compositor = fixture.frontend.compositor_mut();
  fixture
    .client
    .request_frame(compositor, &toplevels[0].surface, 1);
  fixture
    .client
    .request_frame(compositor, &toplevels[1].surface, 2);
  fixture.process();
  render(&mut fixture, &primary(), &mut target);
  fixture.process();
  assert_eq!(fixture.client.frames_done(), [2], "bravo is on HEADLESS-1");
  render(&mut fixture, &second(), &mut target);
  fixture.process();
  assert_eq!(fixture.client.frames_done(), [2, 1]);
}

#[test]
fn scale_runs_on_the_output_that_was_active() {
  // One target draws both outputs, as a host draws them on one device:
  // a texture belongs to the device that made it.
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, [alpha, bravo, charlie], _toplevels) =
    split_windows(still());
  fixture.frontend.compositor_mut().focus_window(alpha);
  fixture.process();
  assert_eq!(fixture.frontend.compositor().active_output(), second());
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let state = fixture.frontend.scale.as_ref().expect("Scale runs");
  assert_eq!(state.windows, [alpha], "only HEADLESS-2's windows");
  let thumbnail = state.geometry[&alpha];
  assert!(thumbnail.x >= 1000, "laid out on HEADLESS-2: {thumbnail:?}");
  render(&mut fixture, &primary(), &mut target);
  assert_eq!(
    target.frames.last().expect("a frame"),
    &[
      (drawn_at(&fixture, bravo, 0, 0), true),
      (drawn_at(&fixture, charlie, 0, 0), true),
    ],
    "HEADLESS-1 is drawn as it is"
  );
  render(&mut fixture, &second(), &mut target);
  let frame = target.frames.last().expect("a frame");
  assert!(
    frame.contains(&(translate(thumbnail, -1000, 0), true)),
    "alpha's thumbnail on HEADLESS-2: {frame:?}"
  );
  // Another output becoming active ends Scale.
  fixture.frontend.compositor_mut().focus_output(&primary());
  fixture.process();
  assert!(fixture.frontend.scale.is_none());
}

#[test]
fn the_wset_label_shows_on_its_output_for_its_duration() {
  // One target draws both outputs, as a host draws them on one device:
  // a texture belongs to the device that made it.
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = still();
  config.wsets.label_duration = 150;
  let (mut fixture, _, _toplevels) = split_windows(config);
  render(&mut fixture, &primary(), &mut target);
  fixture.host.redrawn.clear();
  fixture
    .frontend
    .compositor_mut()
    .show_wset(&second(), 2)
    .expect("HEADLESS-2 shows set 2");
  fixture.process();
  assert!(fixture.host.redrawn.contains(&second()));
  let deadline = fixture.frontend.next_deadline().expect("the label ends");
  assert!(deadline <= Instant::now() + Duration::from_millis(150));
  render(&mut fixture, &second(), &mut target);
  let label = |target: &RecordingTarget| {
    target
      .frames
      .last()
      .expect("a frame")
      .iter()
      .any(|(rect, textured)| *textured && (rect.x, rect.y) == (10, 10))
  };
  assert!(
    label(&target),
    "top left on HEADLESS-2: {:?}",
    target.frames
  );
  render(&mut fixture, &primary(), &mut target);
  assert!(!label(&target), "not on HEADLESS-1");
  thread::sleep(Duration::from_millis(160));
  fixture.host.redrawn.clear();
  fixture.process();
  assert!(
    fixture.host.redrawn.contains(&second()),
    "a frame without it"
  );
  render(&mut fixture, &second(), &mut target);
  assert!(!label(&target));
  assert!(fixture.frontend.next_deadline().is_none());
}

#[test]
fn a_dragged_window_is_drawn_on_every_output_it_crosses() {
  // One target draws both outputs: textures belong to its device, and
  // the window is drawn on both.
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, [_, bravo, _], _toplevels) = split_windows(still());
  let reply = fixture.ipc(&format!(
    concat!(
      r#"{{"method":"window-rules/configure-view","data":{{"id":{},"#,
      r#""geometry":{{"x":700,"y":100,"width":200,"height":150}}}}}}"#
    ),
    bravo.0
  ));
  assert_eq!(reply, r#"{"result":"ok"}"#);
  fixture
    .frontend
    .compositor_mut()
    .raise_window(bravo)
    .expect("bravo is raised");
  fixture.process();
  fixture.move_pointer(750.0, 150.0);
  fixture.key(KEY_LEFTMETA, true);
  fixture.button(BTN_LEFT, true);
  assert!(matches!(
    fixture.frontend.drag,
    Some(DragState::Move { .. })
  ));
  fixture.move_pointer(950.0, 150.0);
  let dragged = drawn_at(&fixture, bravo, 0, 0);
  assert_eq!(
    (dragged.x, dragged.width),
    (900, 200),
    "across the boundary"
  );
  render(&mut fixture, &primary(), &mut target);
  assert!(target.frames.last().unwrap().contains(&(dragged, true)));
  render(&mut fixture, &second(), &mut target);
  assert!(
    target
      .frames
      .last()
      .unwrap()
      .contains(&(translate(dragged, -1000, 0), true)),
    "its part on HEADLESS-2: {:?}",
    target.frames.last()
  );
  // Dropped there, it is on HEADLESS-1 and drawn only there.
  fixture.button(BTN_LEFT, false);
  fixture.key(KEY_LEFTMETA, false);
  render(&mut fixture, &second(), &mut target);
  assert!(
    !target
      .frames
      .last()
      .unwrap()
      .iter()
      .any(|(rect, _)| rect.x < 0),
    "clipped to its own output: {:?}",
    target.frames.last()
  );
}

#[test]
fn zoom_is_per_output() {
  let (mut fixture, _, _toplevels) = split_windows(still());
  fixture.move_pointer(1400.0, 200.0);
  fixture.host.redrawn.clear();
  fixture.key(KEY_LEFTMETA, true);
  fixture.input(
    fixture.pointer,
    InputEvent::PointerAxis(Axis {
      vertical: -10.0,
      vertical_steps: -1,
      ..Axis::default()
    }),
  );
  fixture.key(KEY_LEFTMETA, false);
  let scene = &fixture.frontend.scenes[&second()];
  assert!(scene.zoom_target > 1.0, "HEADLESS-2 zooms in");
  assert_eq!(scene.zoom_origin, (400.0, 200.0), "relative to HEADLESS-2");
  assert!(
    fixture
      .frontend
      .scenes
      .get(&primary())
      .is_none_or(|scene| scene.zoom_target == 1.0)
  );
  assert!(fixture.host.redrawn.contains(&second()));
  assert!(!fixture.host.redrawn.contains(&primary()));
}

#[test]
fn a_capture_is_answered_by_a_frame_of_its_own_output() {
  // One target draws both outputs, as a host draws them on one device:
  // a texture belongs to the device that made it.
  let Some(mut target) = recording_target() else {
    return;
  };
  let (mut fixture, _, _toplevels) = split_windows(still());
  fixture.host.redrawn.clear();
  let _capture = fixture.client.capture_on(
    fixture.frontend.compositor_mut(),
    1,
    (0, 0, 4, 3),
  );
  fixture.frontend.process(&mut fixture.host);
  assert!(fixture.host.redrawn.contains(&second()));
  render(&mut fixture, &primary(), &mut target);
  fixture.process();
  assert!(
    fixture.client.captures().is_empty(),
    "not HEADLESS-1's frame"
  );
  assert!(target.captures.is_empty());
  render(&mut fixture, &second(), &mut target);
  fixture.process();
  assert_eq!(fixture.client.captures(), ["ready"]);
  assert_eq!(target.captures, [(800, 600)], "drawn at HEADLESS-2's size");
}

#[test]
fn outputs_created_over_ipc_get_frames_and_are_drawn_for_captures() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(still());
  let toplevel = fixture.window("alpha");
  let window = fixture.tiles()[0];
  let reply = fixture.ipc(concat!(
    r#"{"method":"wayfire/create-headless-output","#,
    r#""data":{"width":640,"height":480}}"#
  ));
  assert!(reply.contains("HEADLESS-2"), "{reply}");
  fixture
    .frontend
    .compositor_mut()
    .move_window_to_output(window, &second(), MoveGeometry::Scale)
    .expect("alpha moves");
  fixture.process();
  assert!(fixture.frontend.next_deadline().is_some(), "paced at 60 Hz");
  fixture.client.request_frame(
    fixture.frontend.compositor_mut(),
    &toplevel.surface,
    7,
  );
  let deadline = Instant::now() + Duration::from_secs(2);
  while fixture.client.frames_done().is_empty() && Instant::now() < deadline {
    thread::sleep(Duration::from_millis(5));
    fixture.process();
  }
  assert_eq!(fixture.client.frames_done(), [7], "without any host frame");
  fixture.host.redrawn.clear();
  let _capture = fixture.client.capture_on(
    fixture.frontend.compositor_mut(),
    1,
    (0, 0, 4, 3),
  );
  fixture.frontend.process(&mut fixture.host);
  assert_eq!(
    fixture.host.redrawn,
    [primary()],
    "the host is asked for its own outputs only"
  );
  render(&mut fixture, &primary(), &mut target);
  fixture.process();
  assert_eq!(fixture.client.captures(), ["ready"]);
  assert_eq!(target.captures, [(640, 480)], "drawn at its own size");
}

#[test]
fn stipc_pointer_input_moves_the_frontends_pointer() {
  let mut fixture = Fixture::new(still());
  let reply =
    fixture.ipc(r#"{"method":"stipc/move_cursor","data":{"x":300,"y":40}}"#);
  assert_eq!(reply, r#"{"result":"ok"}"#);
  assert_eq!(
    (fixture.frontend.pointer_x, fixture.frontend.pointer_y),
    (300.0, 40.0)
  );
}
