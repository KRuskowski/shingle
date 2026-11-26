//! Touchpad workspace swipes (docs/design/wayfire-gaps.md, WG-5): every
//! vswipe option through the frontend's input, and swipes vswipe does not
//! take reaching clients.

use shingle::Gesture;

use super::*;

/// A `width` by `height` grid without animations to wait for: vswipe's
/// slide and vswitch's take no time.
fn grid(width: u32, height: u32) -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.workspaces.width = width;
  config.workspaces.height = height;
  config.animate.open_animation = "none".into();
  config.tiling.animation_duration = 0;
  config.vswitch.duration_ms = 0;
  config.vswipe.duration_ms = 0;
  config
}

impl Fixture {
  fn gesture(&mut self, gesture: Gesture) {
    self.input(self.pointer, InputEvent::Swipe(gesture));
  }

  fn begin(&mut self, fingers: u32) {
    self.gesture(Gesture::Begin { fingers });
  }

  fn update(&mut self, dx: f64, dy: f64) {
    self.gesture(Gesture::Update {
      dx,
      dy,
      scale: 1.0,
      rotation: 0.0,
    });
  }

  fn end(&mut self) {
    self.gesture(Gesture::End { cancelled: false });
  }

  /// A swipe of `fingers`: `steps` updates of (`dx`, `dy`), then two
  /// updates of a pixel or less, so that it ends slowly, without a
  /// flick.
  fn swipe(&mut self, fingers: u32, steps: usize, (dx, dy): (f64, f64)) {
    self.begin(fingers);
    for _ in 0..steps {
      self.update(dx, dy);
    }
    let slow = dx.abs().max(dy.abs());
    for _ in 0..2 {
      self.update(dx / slow, dy / slow);
    }
    self.end();
  }

  /// Makes workspace `index` current.
  fn go_to(&mut self, index: usize) {
    self
      .frontend
      .compositor_mut()
      .switch_workspace(index)
      .expect("the workspace exists");
    self.process();
  }

  fn viewport(&self) -> Option<(f64, f64)> {
    self.frontend.vswipe_viewport(&primary(), Instant::now())
  }

  fn swiping(&self) -> bool {
    self
      .frontend
      .vswipe
      .as_ref()
      .is_some_and(|state| state.active)
  }
}

/// 12.8 pixels are 0.05 of a workspace at the default speed factor of
/// 256, the most one update moves. Twenty such updates go most of the way
/// to the next workspace, five not far.
const STEP: f64 = 12.8;
const LONG: usize = 20;
const SHORT: usize = 5;

#[test]
fn a_long_swipe_switches_and_a_short_one_returns() {
  let mut config = grid(3, 1);
  // vswitch would slide too, but vswipe's slide shows the switch.
  config.vswitch.duration_ms = 300;
  let mut fixture = Fixture::new(config);
  fixture.begin(4);
  for _ in 0..LONG {
    fixture.update(-STEP, 0.0);
  }
  assert!(fixture.swiping(), "the swipe started");
  let (x, y) = fixture.viewport().expect("the wall is drawn");
  assert!(x > 0.35 && x < 1.0 && y == 0.0, "{x} {y}");
  assert_eq!(fixture.workspace(), 0, "nothing switches before the end");
  fixture.update(-1.0, 0.0);
  fixture.update(-1.0, 0.0);
  fixture.end();
  assert_eq!(fixture.workspace(), 1);
  assert!(!fixture.swiping(), "a slide of 0 ms ends at once");
  assert!(
    fixture.frontend.scenes[&primary()]
      .workspace_transition
      .is_none(),
    "vswitch does not slide again"
  );
  fixture.swipe(4, SHORT, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 1, "a short swipe returns");
  // Back to the right.
  fixture.swipe(4, LONG, (STEP, 0.0));
  assert_eq!(fixture.workspace(), 0);
}

#[test]
fn the_direction_locks_to_the_first_axis() {
  let mut fixture = Fixture::new(grid(3, 3));
  fixture.go_to(4);
  // Started horizontally, the vertical motion that follows is ignored.
  fixture.begin(4);
  for _ in 0..3 {
    fixture.update(-STEP * 2.0, 0.0);
  }
  for _ in 0..LONG {
    fixture.update(-STEP, -STEP * 3.0);
  }
  fixture.update(-1.0, 0.0);
  fixture.update(-1.0, 0.0);
  fixture.end();
  assert_eq!(fixture.workspace(), 5, "one column right, same row");
  // A vertical swipe moves along the column.
  fixture.swipe(4, LONG, (0.0, -STEP));
  assert_eq!(fixture.workspace(), 8);
  // Without free movement a diagonal swipe takes its larger axis.
  fixture.swipe(4, LONG, (STEP, STEP * 0.9));
  assert_eq!(fixture.workspace(), 7);
}

#[test]
fn disabled_axes_leave_the_swipe_to_clients() {
  let mut config = grid(3, 3);
  config.vswipe.enable_horizontal = false;
  let mut fixture = Fixture::new(config);
  fixture.go_to(4);
  fixture.swipe(4, LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 4, "horizontal swipes are off");
  fixture.swipe(4, LONG, (0.0, -STEP));
  assert_eq!(fixture.workspace(), 7, "vertical ones still work");
  // With neither axis vswipe takes nothing, not even diagonally.
  let mut config = grid(3, 3);
  config.vswipe.enable_horizontal = false;
  config.vswipe.enable_vertical = false;
  config.vswipe.enable_free_movement = true;
  let mut fixture = Fixture::new(config);
  fixture.go_to(4);
  fixture.swipe(4, LONG, (-STEP, -STEP));
  assert_eq!(fixture.workspace(), 4);
  assert!(fixture.frontend.vswipe.is_none());
}

#[test]
fn the_grid_edges_stop_a_swipe_with_or_without_wraparound() {
  for wraparound in ["false", "true", "on_end"] {
    let mut config = grid(3, 1);
    config.workspaces.wraparound = wraparound.into();
    let mut fixture = Fixture::new(config);
    // Right of the first workspace there is nothing; vswipe never
    // wraps.
    fixture.begin(4);
    for _ in 0..LONG {
      fixture.update(STEP, 0.0);
    }
    let (x, _) = fixture.viewport().expect("the wall is drawn");
    assert!(x < 0.0 && x > -0.2, "{wraparound}: a rubber band at {x}");
    fixture.update(1.0, 0.0);
    fixture.update(1.0, 0.0);
    fixture.end();
    assert_eq!(fixture.workspace(), 0, "{wraparound}");
    fixture.go_to(2);
    fixture.swipe(4, LONG, (-STEP, 0.0));
    assert_eq!(fixture.workspace(), 2, "{wraparound}");
    // A flick at the edge stays too.
    fixture.begin(4);
    fixture.update(-STEP * 4.0, 0.0);
    fixture.update(-STEP * 4.0, 0.0);
    fixture.end();
    assert_eq!(fixture.workspace(), 2, "{wraparound}");
  }
}

#[test]
fn the_thresholds_decide_where_a_swipe_ends() {
  // A short swipe moves on with a lower threshold.
  let mut config = grid(3, 1);
  config.vswipe.threshold = 0.1;
  let mut fixture = Fixture::new(config);
  fixture.swipe(4, SHORT, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 1);
  // A flick moves on however short: its last two updates are 60 pixels,
  // beyond the delta threshold of 24.
  let mut fixture = Fixture::new(grid(3, 1));
  fixture.begin(4);
  fixture.update(-STEP, 0.0);
  fixture.update(-30.0, 0.0);
  fixture.update(-30.0, 0.0);
  fixture.end();
  assert_eq!(fixture.workspace(), 1);
  // Not with a higher delta threshold.
  let mut config = grid(3, 1);
  config.vswipe.delta_threshold = 100.0;
  let mut fixture = Fixture::new(config);
  fixture.begin(4);
  fixture.update(-STEP, 0.0);
  fixture.update(-30.0, 0.0);
  fixture.update(-30.0, 0.0);
  fixture.end();
  assert_eq!(fixture.workspace(), 0);
}

#[test]
fn the_speed_factor_and_cap_scale_the_motion() {
  // Half the speed factor: half the pixels go as far.
  let mut config = grid(3, 1);
  config.vswipe.speed_factor = 128.0;
  let mut fixture = Fixture::new(config);
  fixture.begin(4);
  for _ in 0..LONG {
    fixture.update(-STEP / 2.0, 0.0);
  }
  let (half, _) = fixture.viewport().expect("the wall is drawn");
  let mut fixture = Fixture::new(grid(3, 1));
  fixture.begin(4);
  for _ in 0..LONG {
    fixture.update(-STEP, 0.0);
  }
  let (full, _) = fixture.viewport().expect("the wall is drawn");
  assert!((half - full).abs() < 1e-9, "{half} {full}");
  // A lower cap holds every update back.
  let mut config = grid(3, 1);
  config.vswipe.speed_cap = 0.01;
  let mut fixture = Fixture::new(config);
  fixture.swipe(4, LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 0, "20 updates of 0.01 are too short");
}

#[test]
fn free_movement_crosses_diagonally_and_several_workspaces() {
  let mut config = grid(3, 3);
  config.vswipe.enable_free_movement = true;
  let mut fixture = Fixture::new(config);
  fixture.go_to(4);
  fixture.swipe(4, LONG, (-STEP, -STEP));
  assert_eq!(fixture.workspace(), 8, "one down and right");
  fixture.go_to(0);
  fixture.swipe(4, 2 * LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 2, "two columns at once");
}

#[test]
fn smooth_and_immediate_swipes_commit_over_the_duration() {
  for smooth in [false, true] {
    let mut config = grid(3, 1);
    config.vswipe.enable_smooth_transition = smooth;
    config.vswipe.duration_ms = 60_000;
    let mut fixture = Fixture::new(config);
    fixture.begin(4);
    for _ in 0..LONG {
      fixture.update(-STEP, 0.0);
    }
    let state = fixture.frontend.vswipe.as_ref().expect("a swipe");
    let now = Instant::now();
    let (offset, end) = (state.swipe.offset(now).0, state.swipe.offset_end().0);
    if smooth {
      assert!(offset > end + 0.1, "the wall trails: {offset} {end}");
    } else {
      assert_eq!(offset, end, "the wall follows at once");
    }
    fixture.update(-1.0, 0.0);
    fixture.update(-1.0, 0.0);
    fixture.end();
    // The switch happens at once; the slide there takes a minute.
    assert_eq!(fixture.workspace(), 1);
    assert!(fixture.swiping(), "the slide runs");
    let (x, _) = fixture.viewport().expect("the wall is drawn");
    assert!(x < 0.99, "{x}");
    // A swipe while the slide runs is the clients'.
    fixture.swipe(4, LONG, (-STEP, 0.0));
    assert_eq!(fixture.workspace(), 1);
    let state = fixture.frontend.vswipe.as_ref().expect("the slide");
    assert!(!state.swipe.swiping());
  }
}

#[test]
fn swipes_with_other_fingers_reach_clients_and_vswipe_takes_its_own() {
  let mut fixture = Fixture::new(grid(3, 1));
  fixture
    .client
    .listen_for_swipes(fixture.frontend.compositor_mut());
  fixture.window("alpha");
  let window = fixture.tiles()[0];
  let (x, y) = fixture.center(window);
  fixture.move_pointer(x, y);
  fixture.begin(3);
  fixture.update(-16.0, 0.0);
  fixture.update(-16.0, 2.0);
  fixture.end();
  fixture.process();
  assert_eq!(
    fixture.client.swipes(),
    ["begin 3", "update -16 0", "update -16 2", "end 0"]
  );
  assert_eq!(fixture.workspace(), 0);
  assert!(fixture.frontend.vswipe.is_none());
  // Four fingers: the client sees the swipe until vswipe starts it on
  // the second update, then its cancelled end and nothing more.
  fixture.begin(4);
  fixture.update(-8.0, 0.0);
  fixture.update(-8.0, 0.0);
  for _ in 0..LONG {
    fixture.update(-STEP, 0.0);
  }
  fixture.end();
  fixture.process();
  assert_eq!(
    fixture.client.swipes()[4..],
    ["begin 4", "update -8 0", "end 1"]
  );
  assert_eq!(fixture.workspace(), 1);
  // A swipe that never decides a direction stays the client's.
  fixture.go_to(0);
  fixture.move_pointer(x, y);
  fixture.begin(4);
  fixture.update(-2.0, 0.0);
  fixture.end();
  fixture.process();
  assert_eq!(
    fixture.client.swipes()[7..],
    ["begin 4", "update -2 0", "end 0"]
  );
}

#[test]
fn an_unloaded_plugin_or_other_finger_count_takes_nothing() {
  let mut config = grid(3, 1);
  config.plugins = Some(vec!["vswitch".into()]);
  let mut fixture = Fixture::new(config);
  fixture.swipe(4, LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 0, "vswipe is not loaded");
  let mut config = grid(3, 1);
  config.vswipe.fingers = 3;
  let mut fixture = Fixture::new(config);
  fixture.swipe(4, LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 0, "four fingers are not three");
  fixture.swipe(3, LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 1);
}

#[test]
fn scale_or_a_disabled_pointer_stops_a_swipe() {
  let mut fixture = Fixture::new(grid(3, 1));
  fixture.window("alpha");
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  assert!(fixture.frontend.scale.is_some());
  fixture.swipe(4, LONG, (-STEP, 0.0));
  assert_eq!(fixture.workspace(), 0, "Scale holds the output");
  fixture.tap(KEY_ESC, &[]);
  assert!(fixture.frontend.scale.is_none());
  // Its exit animation ends with a frame, which this test draws none of.
  fixture.frontend.scale_exit = None;
  fixture.begin(4);
  for _ in 0..LONG {
    fixture.update(-STEP, 0.0);
  }
  assert!(fixture.swiping());
  let pointer = fixture.pointer;
  let reply = fixture.ipc(&format!(
    concat!(
      r#"{{"method":"input/configure-device","#,
      r#""data":{{"id":{},"enabled":false}}}}"#
    ),
    pointer.get()
  ));
  assert_eq!(reply, r#"{"result":"ok"}"#);
  assert!(fixture.frontend.vswipe.is_none(), "the swipe is dropped");
  assert_eq!(fixture.workspace(), 0);
}

#[test]
fn the_wall_is_drawn_with_the_gap_and_background() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = grid(3, 1);
  config.vswipe.gap = 40.0;
  config.vswipe.background = [0.5, 0.25, 0.0, 1.0];
  let mut fixture = Fixture::new(config);
  fixture.window("alpha");
  fixture.go_to(1);
  fixture.window("bravo");
  fixture.go_to(0);
  render_until_idle(&mut fixture, &mut target);
  let settled = target.frames.last().expect("a frame").clone();
  assert_eq!(settled.len(), 1, "only alpha is on workspace 0");
  let original = settled[0].0;
  fixture.begin(4);
  for _ in 0..LONG {
    fixture.update(-STEP, 0.0);
  }
  let (x, _) = fixture.viewport().expect("the wall is drawn");
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert_eq!(target.clears.last(), Some(&[0.5, 0.25, 0.0, 1.0]));
  let frame = target.frames.last().expect("a frame");
  let step = 1000.0 + 40.0;
  let shifted = |workspace: f64| {
    (f64::from(original.x) + (workspace - x) * step).round() as i32
  };
  let mut xs = frame.iter().map(|(rect, _)| rect.x).collect::<Vec<_>>();
  xs.sort();
  // Alpha slides left, bravo comes in from the right a gap after it.
  assert_eq!(xs, [shifted(0.0), shifted(1.0)], "{frame:?}");
  fixture.update(-1.0, 0.0);
  fixture.update(-1.0, 0.0);
  fixture.end();
  render_until_idle(&mut fixture, &mut target);
  assert!(fixture.frontend.vswipe.is_none());
  assert_ne!(target.clears.last(), Some(&[0.5, 0.25, 0.0, 1.0]));
  let last = target.frames.last().expect("a frame");
  assert_eq!(last.len(), 1, "only bravo: {last:?}");
  assert_eq!(last[0].0.x, original.x);
}
