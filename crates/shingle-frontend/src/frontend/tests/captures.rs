//! Captures as the frontend answers them: waiting for damage, with and
//! without the cursor, and of one window alone.

use super::*;

/// Draws a frame of the primary output into `target` and lets the
/// clients read the result.
fn draw(fixture: &mut Fixture, target: &mut RecordingTarget) {
  fixture.frontend.process(&mut fixture.host);
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), target)
    .expect("the frame renders");
  fixture.process();
}

#[test]
fn copy_with_damage_waits_for_a_frame_that_changed_something() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  let mut fixture = Fixture::new(config);
  let compositor = fixture.frontend.compositor_mut();
  let _first =
    fixture
      .client
      .capture_with(compositor, (0, 0, 2, 2), false, true);
  draw(&mut fixture, &mut target);
  assert_eq!(fixture.client.captures(), ["ready"]);
  let compositor = fixture.frontend.compositor_mut();
  let _second =
    fixture
      .client
      .capture_with(compositor, (0, 0, 2, 2), false, true);
  draw(&mut fixture, &mut target);
  draw(&mut fixture, &mut target);
  assert_eq!(
    fixture.client.captures(),
    ["ready"],
    "frames drawn for the capture alone are no damage"
  );
  fixture.window("damage");
  draw(&mut fixture, &mut target);
  assert_eq!(fixture.client.captures(), ["ready", "ready"]);
}

#[test]
fn the_cursor_is_in_a_capture_only_when_asked_for() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.frontend.compositor_mut().set_default_cursor(true);
  fixture.move_pointer(100.0, 100.0);
  let cursor = fixture
    .frontend
    .compositor()
    .cursor_on(&primary())
    .expect("the default arrow is drawn")
    .geometry;
  let region = (
    cursor.x,
    cursor.y,
    cursor.width as i32,
    cursor.height as i32,
  );
  let compositor = fixture.frontend.compositor_mut();
  let without = fixture
    .client
    .capture_with(compositor, region, false, false);
  let compositor = fixture.frontend.compositor_mut();
  let with = fixture.client.capture_with(compositor, region, true, false);
  draw(&mut fixture, &mut target);
  assert_eq!(fixture.client.captures(), ["ready", "ready"]);
  let background = |pixel: &[u8]| {
    pixel[0].abs_diff(26) <= 1
      && pixel[1].abs_diff(26) <= 1
      && pixel[2].abs_diff(26) <= 1
  };
  assert!(
    without.bytes().chunks(4).all(background),
    "only the background without the cursor"
  );
  assert!(!with.bytes().chunks(4).all(background), "the arrow with it");
}

#[test]
fn a_window_capture_shows_that_window_alone() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  let mut fixture = Fixture::new(config);
  let red = fixture.window("red");
  let green = fixture.window("green");
  let compositor = fixture.frontend.compositor_mut();
  fixture.client.attach_colour(
    compositor,
    &red.surface,
    (8, 8),
    [0, 0, 255, 255],
  );
  let compositor = fixture.frontend.compositor_mut();
  fixture.client.attach_colour(
    compositor,
    &green.surface,
    (8, 8),
    [0, 255, 0, 255],
  );
  draw(&mut fixture, &mut target);
  let compositor = fixture.frontend.compositor_mut();
  let (capture, (width, height)) =
    fixture.client.capture_window(compositor, "red");
  assert!(width < 1000, "one tile of two: {width}x{height}");
  draw(&mut fixture, &mut target);
  assert_eq!(fixture.client.captures(), ["ready"]);
  let bytes = capture.bytes();
  assert_eq!(bytes.len(), (width * height * 4) as usize);
  assert!(
    bytes.chunks(4).all(|pixel| pixel == [0, 0, 255, 255]),
    "red only, without green or the background: {:?}",
    &bytes[..8]
  );
}
