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
  // What the lock binding starts after the crash.
  config.lock.command = "true".into();
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
  // The lock binding still starts a new lock client then, and the output
  // stays black meanwhile.
  assert!(fixture.frontend.lock_client.is_none());
  fixture.tap(KEY_F8, &[KEY_LEFTMETA]);
  assert!(
    fixture.frontend.lock_client.is_some(),
    "lock.command started"
  );
  assert_eq!(
    fixture.frontend.compositor().lock_state(),
    shingle::LockState::Failed
  );
  let (tiles, _) = frame(&mut fixture, &mut target);
  assert!(tiles.is_empty());
}

#[test]
fn a_proper_unlock_fades_the_lock_frame_into_the_desktop() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.background_color = [0.5, 0.5, 0.5, 1.0];
  config.lock.unlock_fade = 0.25;
  let mut fixture = Fixture::new(config);
  let base = fixture.base.clone();
  let mut locker =
    TestClient::connect(fixture.frontend.compositor_mut(), &base);
  let lock = locker.lock_session(fixture.frontend.compositor_mut());
  locker.attach_colour(
    fixture.frontend.compositor_mut(),
    &lock.surface,
    lock.size,
    [0, 0, 255, 255],
  );
  let (_, pixel) = frame(&mut fixture, &mut target);
  assert_eq!(pixel, [255, 0, 0, 255]);
  let (_, desktop) = {
    // What the desktop looks like: the background colour.
    let mut unlocked = Fixture::new(ShingleConfig {
      background_color: [0.5, 0.5, 0.5, 1.0],
      ..ShingleConfig::default()
    });
    frame(&mut unlocked, &mut target)
  };
  fixture.frontend.compositor_mut().unlock();
  // The first frame after the unlock is still the lock's, over the
  // desktop drawn under it; then it gives way.
  let (tiles, first) = frame(&mut fixture, &mut target);
  assert!(!tiles.is_empty());
  assert!(first[0] > 200 && first[1] < 60, "{first:?}");
  assert!(
    fixture.host.redrawn.contains(&primary()),
    "the fade asks for frames"
  );
  std::thread::sleep(Duration::from_millis(100));
  let (_, middle) = frame(&mut fixture, &mut target);
  assert!(
    middle[0] < first[0] && middle[0] > desktop[0],
    "{middle:?} between {first:?} and {desktop:?}"
  );
  std::thread::sleep(Duration::from_millis(200));
  let (_, last) = frame(&mut fixture, &mut target);
  assert_eq!(last, desktop, "the desktop once the fade is over");
  let (_, after) = frame(&mut fixture, &mut target);
  assert_eq!(after, desktop);
  drop(lock);
}

#[test]
fn the_unlock_fade_eases_out() {
  use super::super::outputs::unlock_fade_alpha;
  let length = Duration::from_millis(400);
  assert_eq!(unlock_fade_alpha(Duration::ZERO, length), 1.0);
  assert!(unlock_fade_alpha(Duration::from_millis(100), length) > 0.5);
  assert!(unlock_fade_alpha(Duration::from_millis(200), length) < 0.3);
  assert_eq!(unlock_fade_alpha(length, length), 0.0);
  assert_eq!(unlock_fade_alpha(Duration::ZERO, Duration::ZERO), 0.0);
}
