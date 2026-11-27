//! The lock scene: from the lock request on, nothing but the lock's
//! surface over black, whatever the frontend was drawing.

use super::*;

/// Draws a frame of the primary output; returns its tiles and the RGBA
/// pixel at the middle of the output.
fn frame(
  fixture: &mut Fixture,
  target: &mut RecordingTarget,
) -> (Vec<(RenderRect, bool)>, [u8; 4]) {
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), target)
    .expect("the frame renders");
  let pixels = target.target.read_rgba(target.gpu()).unwrap();
  let middle = (400 * 1000 + 500) * 4;
  let pixel = pixels[middle..middle + 4].try_into().unwrap();
  (target.frames.last().cloned().unwrap_or_default(), pixel)
}

#[test]
fn a_lock_request_hides_scale_until_the_lock_shows_a_full_frame() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.background_color = [0.5, 0.5, 0.5, 1.0];
  let mut fixture = Fixture::new(config);
  for app_id in ["alpha", "bravo"] {
    fixture.window(app_id);
  }
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  assert!(fixture.frontend.scale.is_some(), "Scale is open");
  let (tiles, _) = frame(&mut fixture, &mut target);
  assert!(!tiles.is_empty());

  let base = fixture.base.clone();
  let mut locker =
    TestClient::connect(fixture.frontend.compositor_mut(), &base);
  let lock = locker.lock_session(fixture.frontend.compositor_mut());
  fixture.host.redrawn.clear();
  fixture.process();
  assert!(
    fixture.host.redrawn.contains(&primary()),
    "a frame is asked for"
  );
  let (tiles, pixel) = frame(&mut fixture, &mut target);
  assert!(tiles.is_empty(), "nothing is drawn: {tiles:?}");
  assert_eq!(pixel, [0, 0, 0, 255], "black, not the background colour");
  assert_eq!(target.clears.last(), Some(&[0.0, 0.0, 0.0, 1.0]));

  // A frame that does not cover the output keeps it black.
  let (width, height) = lock.size;
  locker.attach_buffer(
    fixture.frontend.compositor_mut(),
    &lock.surface,
    width,
    height,
  );
  let (tiles, pixel) = frame(&mut fixture, &mut target);
  assert!(tiles.is_empty());
  assert_eq!(pixel, [0, 0, 0, 255]);

  // An opaque red one is drawn, alone.
  locker.attach_colour(
    fixture.frontend.compositor_mut(),
    &lock.surface,
    lock.size,
    [0, 0, 255, 255],
  );
  let (tiles, pixel) = frame(&mut fixture, &mut target);
  assert_eq!(tiles.len(), 1, "{tiles:?}");
  assert_eq!(pixel, [255, 0, 0, 255]);
  assert!(fixture.frontend.compositor().is_locked());

  // After the lock client crashed, the output is black again.
  drop(lock);
  drop(locker);
  for _ in 0..10 {
    fixture.process();
  }
  assert_eq!(
    fixture.frontend.compositor().lock_state(),
    shingle::LockState::Failed
  );
  let (tiles, pixel) = frame(&mut fixture, &mut target);
  assert!(tiles.is_empty());
  assert_eq!(pixel, [0, 0, 0, 255]);
}
