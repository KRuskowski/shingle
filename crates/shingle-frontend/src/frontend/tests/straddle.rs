//! A window hanging over the edge of its workspace, drawn on the
//! neighbour too: in the scene, in vswitch's slide, in Expo and with its
//! frame, and taking clicks where it is drawn.

use super::*;

/// A 2x1 grid without animations to wait for; vswitch's gap is 20.
fn grid() -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.workspaces.width = 2;
  config.workspaces.height = 1;
  config.animate.open_animation = "none".into();
  config.tiling.animation_duration = 0;
  config.vswitch.duration_ms = 0;
  config.vswitch.gap = 20;
  config.expo.duration_ms = 0;
  config.expo.offset = 10;
  config
}

impl Fixture {
  /// Floats the focused window at `rect` over IPC, relative to the
  /// current workspace; it goes to the workspace holding its centre.
  fn place_focused(&mut self, rect: shingle_core::Rect) -> TileWindowId {
    let window = self.focused().expect("a focused window");
    let request = format!(
      concat!(
        r#"{{"method":"window-rules/configure-view","data":{{"id":{},"#,
        r#""geometry":{{"x":{},"y":{},"width":{},"height":{}}}}}}}"#
      ),
      window.0, rect.x, rect.y, rect.width, rect.height
    );
    assert_eq!(self.ipc(&request), r#"{"result":"ok"}"#);
    window
  }

  fn switch_to(&mut self, index: usize) {
    self
      .frontend
      .compositor_mut()
      .switch_workspace(index)
      .expect("the workspace exists");
    self.process();
  }
}

/// The textured tiles of the last frame drawn.
fn textured(target: &RecordingTarget) -> Vec<RenderRect> {
  target
    .frames
    .last()
    .expect("a frame")
    .iter()
    .filter(|(_, textured)| *textured)
    .map(|(rect, _)| *rect)
    .collect()
}

fn rect(x: i32, y: i32, width: u32, height: u32) -> RenderRect {
  RenderRect {
    x,
    y,
    width,
    height,
  }
}

/// A window on the first workspace from x 850 to 1050 of the 1000-wide
/// output, its centre at 950.
fn hanging_over(fixture: &mut Fixture) -> TileWindowId {
  fixture.window("over");
  let window =
    fixture.place_focused(shingle_core::Rect::new(850, 100, 200, 150));
  fixture.process();
  window
}

#[test]
fn the_neighbour_shows_the_part_hanging_over_without_animating() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(grid());
  let window = hanging_over(&mut fixture);
  render_until_idle(&mut fixture, &mut target);
  assert_eq!(textured(&target), vec![rect(850, 100, 200, 150)]);
  fixture.switch_to(1);
  render_until_idle(&mut fixture, &mut target);
  // One output width to the left: its last 50 pixels at the left edge.
  assert_eq!(textured(&target), vec![rect(-150, 100, 200, 150)]);
  assert!(
    fixture.frontend.scenes[&primary()].animations.is_empty(),
    "the switch is no move of the window"
  );
  let info = fixture.frontend.compositor().core().window_info(window);
  assert_eq!(info.unwrap().location.workspace.x, 0, "it stays home");
  fixture.switch_to(0);
  render_until_idle(&mut fixture, &mut target);
  assert_eq!(textured(&target), vec![rect(850, 100, 200, 150)]);
  assert!(fixture.frontend.scenes[&primary()].animations.is_empty());
}

#[test]
fn a_click_on_the_part_shown_focuses_the_window() {
  let mut fixture = Fixture::new(grid());
  let window = hanging_over(&mut fixture);
  fixture.switch_to(1);
  fixture.window("here");
  let here = fixture.focused().expect("the new window is focused");
  assert_ne!(here, window);
  // The tiled window fills the output; the one hanging over was focused
  // before it, so it is below it there.
  fixture.move_pointer(20.0, 150.0);
  fixture.button(BTN_LEFT, true);
  fixture.button(BTN_LEFT, false);
  assert_eq!(fixture.focused(), Some(here));
  // Floated over the right half, the new window leaves the edge free.
  fixture.place_focused(shingle_core::Rect::new(500, 0, 500, 800));
  fixture.move_pointer(20.0, 150.0);
  fixture.button(BTN_LEFT, true);
  fixture.button(BTN_LEFT, false);
  assert_eq!(fixture.focused(), Some(window));
  assert_eq!(fixture.workspace(), 1, "the click switches nothing");
}

#[test]
fn the_slide_shows_both_parts_with_the_gap_between_them() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = grid();
  // Long enough to draw a frame in the middle of it.
  config.vswitch.duration_ms = 60_000;
  let mut fixture = Fixture::new(config);
  hanging_over(&mut fixture);
  render_until_idle(&mut fixture, &mut target);
  fixture.switch_to(1);
  assert!(
    fixture.frontend.scenes[&primary()]
      .workspace_transition
      .is_some(),
    "vswitch slides"
  );
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  let drawn = textured(&target);
  // Cut at the first workspace's edge and drawn again from the second's,
  // 20 pixels on, wherever the slide is.
  assert_eq!(drawn.len(), 2, "{drawn:?}");
  let (left, right) = (drawn[0], drawn[1]);
  assert_eq!((left.width, right.width), (150, 50), "{drawn:?}");
  assert_eq!(right.x, left.x + 150 + 20, "{drawn:?}");
  assert_eq!((left.y, right.y), (100, 100));
}

#[test]
fn expo_shows_each_part_in_its_workspaces_cell() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(grid());
  hanging_over(&mut fixture);
  fixture.frontend.toggle_expo();
  fixture.process();
  // Expo redraws while it is open.
  for _ in 0..2 {
    fixture
      .frontend
      .render(&mut fixture.host, &primary(), &mut target)
      .expect("the frame renders");
  }
  let cells = fixture
    .frontend
    .expo
    .as_ref()
    .expect("Expo is open")
    .cells
    .clone();
  // Two cells of 485 by 780 of the 1000x800 output: the workspace is
  // drawn at 0.485 of its width, the window 97 wide, from 422 into the
  // first cell, so 73 of it there and 24 in the second.
  let drawn = textured(&target);
  assert_eq!(drawn.len(), 2, "{drawn:?} in {cells:?}");
  let (left, right) = (drawn[0], drawn[1]);
  let first_end = cells[0].x + cells[0].width as i32;
  assert_eq!(left.x + left.width as i32, first_end, "{drawn:?}");
  assert_eq!(right.x, cells[1].x, "{drawn:?}");
  assert_eq!(left.width + right.width, 97, "{drawn:?}");
  assert!((72..=74).contains(&left.width), "{drawn:?}");
}

#[test]
fn a_frame_hangs_over_with_its_window() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = grid();
  config.decoration.forced_views = "all".to_owned().try_into().unwrap();
  let mut fixture = Fixture::new(config);
  let window = hanging_over(&mut fixture);
  fixture.switch_to(1);
  render_until_idle(&mut fixture, &mut target);
  let frame = fixture
    .frontend
    .compositor()
    .frames()
    .into_values()
    .find(|frame| frame.window == window)
    .expect("a frame");
  let drawn = target.frames.last().expect("a frame").clone();
  // The frame's top band first, a workspace to the left of the frame.
  let outer = frame.rect;
  assert_eq!(outer, shingle_core::Rect::new(850, 100, 200, 150));
  assert_eq!(
    drawn[0],
    (rect(-150, 100, 200, frame.margins.top), false),
    "{drawn:?}"
  );
  // The client's surface last, inside the moved frame.
  let content = frame.margins.shrink(outer);
  assert_eq!(
    drawn.last().copied(),
    Some((
      rect(content.x - 1000, content.y, content.width, content.height),
      true
    ))
  );
}

#[test]
fn a_window_dragged_by_the_part_shown_lands_where_it_is_dropped() {
  let mut fixture = Fixture::new(grid());
  let window = hanging_over(&mut fixture);
  fixture.switch_to(1);
  // Drawn from -150 to 50; dragged by it 400 to the right.
  fixture.move_pointer(30.0, 150.0);
  fixture.key(KEY_LEFTMETA, true);
  fixture.button(BTN_LEFT, true);
  assert!(matches!(
    fixture.frontend.drag,
    Some(DragState::Move { source, .. }) if source == window
  ));
  fixture.move_pointer(230.0, 150.0);
  fixture.move_pointer(430.0, 150.0);
  fixture.button(BTN_LEFT, false);
  fixture.key(KEY_LEFTMETA, false);
  // Its centre is on the workspace shown now, which takes it, at the
  // place it was dropped.
  let compositor = fixture.frontend.compositor();
  let info = compositor.core().window_info(window).unwrap();
  assert_eq!(info.location.workspace.x, 1);
  assert_eq!(
    info.state.floating,
    Some(shingle_core::Rect::new(250, 100, 200, 150))
  );
  assert_eq!(fixture.focused(), Some(window));
}
