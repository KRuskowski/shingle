//! Whole views (docs/design/wayfire-gaps.md, "Whole views as built"):
//! an animated window's subsurfaces and frame are drawn into one texture
//! and transformed with it, its popups are not; a closing window's view
//! is drawn once; nothing is drawn so without an animation or for a
//! window of one surface; and the overviews move subsurfaces with their
//! windows.

use std::time::Instant;

use super::*;
use crate::test_client::{Popup, Subsurface};

/// Red, as the client's `bgra` bytes.
const RED: [u8; 4] = [0, 0, 255, 255];
/// Green.
const GREEN: [u8; 4] = [0, 255, 0, 255];
/// White.
const WHITE: [u8; 4] = [255, 255, 255, 255];
/// The subsurface: at the content's top left corner, 100 by 100.
const SUBSURFACE: (i32, i32, i32, i32) = (0, 0, 100, 100);
/// The popup: 60 by 40 at (20, 20) in the window.
const POPUP: (i32, i32, i32, i32) = (20, 20, 60, 40);
/// How long the effects run in these tests, linear: long enough that a
/// frame half way through is well inside them.
const EFFECT_MS: u64 = 2000;

/// `effect` opening and closing windows over `EFFECT_MS`, linearly, with
/// blue frames on every window and no tiling animation.
fn animated(open: &str, close: &str) -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = open.into();
  config.animate.close_animation = close.into();
  config.animate.minimize_animation = "zoom".into();
  config.animate.duration_ms = EFFECT_MS;
  config.animate.easing = shingle_config::Easing::Linear;
  // Spin runs over its own length.
  config.animate.spin_duration_ms = EFFECT_MS;
  config.animate.spin_easing = shingle_config::Easing::Linear;
  config.tiling.animation_duration = 0;
  config.decoration.forced_views = "all".to_owned().try_into().unwrap();
  config.decoration.active_color = [0.0, 0.0, 1.0, 1.0];
  config.decoration.inactive_color = [0.0, 0.0, 1.0, 1.0];
  // Borders wide enough to sample when zoomed.
  config.decoration.border_size = 24;
  config
}

/// A red window with a green subsurface and, if `popup`, a white popup.
struct Composed {
  toplevel: Toplevel,
  subsurface: Subsurface,
  _popup: Option<Popup>,
  window: TileWindowId,
}

impl Fixture {
  /// Maps a [`Composed`] window named `app_id`.
  fn composed(&mut self, app_id: &str, popup: bool) -> Composed {
    let toplevel = self.window(app_id);
    let window = self.focused().expect("the window is focused");
    let content = self.content(window);
    let size = (content.width as i32, content.height as i32);
    let compositor = self.frontend.compositor_mut();
    self
      .client
      .attach_colour(compositor, &toplevel.surface, size, RED);
    let (x, y, width, height) = SUBSURFACE;
    let subsurface = self.client.subsurface(
      compositor,
      &toplevel.surface,
      (x, y),
      (width, height),
      GREEN,
    );
    let popup = popup.then(|| {
      let (x, y, width, height) = POPUP;
      self
        .client
        .popup(compositor, &toplevel, (x, y), (width, height), WHITE)
    });
    self.process();
    Composed {
      toplevel,
      subsurface,
      _popup: popup,
      window,
    }
  }

  /// The rectangle of `window`'s toplevel surface.
  fn content(&self, window: TileWindowId) -> RenderRect {
    let snapshot = self
      .frontend
      .compositor()
      .window_surfaces(window)
      .into_iter()
      .find(|snapshot| snapshot.window == Some(window))
      .expect("the toplevel");
    whole_view_rect(snapshot.geometry)
  }

  /// The rectangle of `window`'s frame.
  fn frame_rect(&self, window: TileWindowId) -> RenderRect {
    let frames = self.frontend.compositor().frames();
    let frame = frames
      .values()
      .find(|frame| frame.window == window)
      .expect("a frame");
    whole_view_rect(frame.rect)
  }

  /// Captures the whole output in the frame it draws next, and returns
  /// the frame's pixels, B, G, R, A, with its textured tiles.
  fn capture_frame(
    &mut self,
    target: &mut RecordingTarget,
  ) -> (Vec<u8>, Vec<RenderRect>) {
    let capture = self
      .client
      .capture(self.frontend.compositor_mut(), (0, 0, 1000, 800));
    self.frontend.process(&mut self.host);
    self
      .frontend
      .render(&mut self.host, &primary(), target)
      .expect("the frame renders");
    self.process();
    let textured = target
      .frames
      .last()
      .expect("a frame")
      .iter()
      .filter(|(_, textured)| *textured)
      .map(|(rect, _)| *rect)
      .collect();
    (capture.bytes(), textured)
  }
}

fn whole_view_rect(rect: shingle_core::Rect) -> RenderRect {
  RenderRect {
    x: rect.x,
    y: rect.y,
    width: rect.width,
    height: rect.height,
  }
}

/// The pixel at (`x`, `y`) of a 1000 pixel wide BGRA capture, as RGB.
fn pixel_at(bytes: &[u8], (x, y): (f64, f64)) -> (u8, u8, u8) {
  let index = ((y.round() as usize) * 1000 + x.round() as usize) * 4;
  (bytes[index + 2], bytes[index + 1], bytes[index])
}

/// Where `point` goes when `from` is drawn at `to`.
fn carried((x, y): (f64, f64), from: RenderRect, to: RenderRect) -> (f64, f64) {
  let scale_x = f64::from(to.width) / f64::from(from.width);
  let scale_y = f64::from(to.height) / f64::from(from.height);
  (
    f64::from(to.x) + (x - f64::from(from.x)) * scale_x,
    f64::from(to.y) + (y - f64::from(from.y)) * scale_y,
  )
}

/// Waits until `since` is half of [`EFFECT_MS`] ago.
fn half_way(since: Instant) {
  let half = Duration::from_millis(EFFECT_MS / 2);
  if let Some(left) = half.checked_sub(since.elapsed()) {
    thread::sleep(left);
  }
}

#[test]
fn zoom_scales_the_subsurface_and_frame_with_the_window_but_no_popup() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(animated("zoom", "none"));
  let opened = Instant::now();
  let composed = fixture.composed("composed", true);
  let content = fixture.content(composed.window);
  let frame = fixture.frame_rect(composed.window);
  half_way(opened);
  let (pixels, textured) = fixture.capture_frame(&mut target);
  // The view's pieces went into one texture: the frame's bands, title
  // and buttons, the window and its subsurface.
  let drawn = target.views.borrow().last().cloned().expect("a view drawn");
  assert!(drawn.len() >= 3, "{drawn:?}");
  let (x, y, width, height) = SUBSURFACE;
  let subsurface = RenderRect {
    x: content.x + x,
    y: content.y + y,
    width: width as u32,
    height: height as u32,
  };
  // The texture covers the frame, which holds everything, so its
  // pieces are drawn relative to the frame's corner.
  let local = |rect: RenderRect| RenderRect {
    x: rect.x - frame.x,
    y: rect.y - frame.y,
    ..rect
  };
  assert!(drawn.contains(&local(subsurface)), "{drawn:?}");
  assert!(drawn.contains(&local(content)), "{drawn:?}");
  // On the output: the view's one tile, zoomed about the frame's centre,
  // and the popup as it is.
  let (px, py, pw, ph) = POPUP;
  let popup = RenderRect {
    x: content.x + px,
    y: content.y + py,
    width: pw as u32,
    height: ph as u32,
  };
  assert_eq!(textured.len(), 2, "{textured:?}");
  assert_eq!(textured[1], popup, "{textured:?}");
  let zoomed = textured[0];
  let scale = f64::from(zoomed.width) / f64::from(frame.width);
  assert!((0.45..0.9).contains(&scale), "{zoomed:?} of {frame:?}");
  let centre = |rect: RenderRect| {
    (
      f64::from(rect.x) + f64::from(rect.width) / 2.0,
      f64::from(rect.y) + f64::from(rect.height) / 2.0,
    )
  };
  let (zx, zy) = centre(zoomed);
  let (fx, fy) = centre(frame);
  assert!((zx - fx).abs() <= 1.0 && (zy - fy).abs() <= 1.0);
  // Half faded: the subsurface's green where the zoom puts it, the
  // frame's blue on its top band, and nothing where the subsurface was.
  let green = pixel_at(&pixels, carried(centre(subsurface), frame, zoomed));
  assert!(
    green.1 > green.0 + 60 && green.1 > green.2 + 60,
    "{green:?}"
  );
  let band = (f64::from(frame.x) + 12.0, fy);
  let blue = pixel_at(&pixels, carried(band, frame, zoomed));
  assert!(blue.2 > blue.0 + 60 && blue.2 > blue.1 + 60, "{blue:?}");
  let (sx, sy) = (f64::from(subsurface.x) + 70.0, f64::from(subsurface.y));
  let gone = pixel_at(&pixels, (sx, sy + 90.0));
  assert!(gone.1 < 40, "the subsurface stayed: {gone:?}");
  // The popup, a view of its own, is not animated with its parent.
  let white = pixel_at(&pixels, centre(popup));
  assert_eq!(white, (255, 255, 255));
  drop(composed);
}

#[test]
fn a_closing_view_is_drawn_once_and_zooms_away_whole() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(animated("none", "zoom"));
  let composed = fixture.composed("closing", false);
  render_until_idle(&mut fixture, &mut target);
  assert!(target.views.borrow().is_empty(), "nothing animates yet");
  let content = fixture.content(composed.window);
  let frame = fixture.frame_rect(composed.window);
  // The client goes, its subsurface first, in one batch.
  let Composed {
    toplevel,
    subsurface,
    ..
  } = composed;
  subsurface.destroy();
  toplevel.destroy();
  let closed = Instant::now();
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert_eq!(target.views.borrow().len(), 1, "the view is drawn once");
  let drawn = target.views.borrow()[0].clone();
  assert!(drawn.len() > 2, "the frame is drawn too: {drawn:?}");
  let (x, y, width, height) = SUBSURFACE;
  assert!(
    drawn.contains(&RenderRect {
      x: content.x + x - frame.x,
      y: content.y + y - frame.y,
      width: width as u32,
      height: height as u32,
    }),
    "{drawn:?}"
  );
  half_way(closed);
  let (pixels, textured) = fixture.capture_frame(&mut target);
  assert_eq!(target.views.borrow().len(), 1, "and not again");
  assert_eq!(textured.len(), 1, "{textured:?}");
  let zoomed = textured[0];
  let scale = f64::from(zoomed.width) / f64::from(frame.width);
  assert!((0.45..0.9).contains(&scale), "{zoomed:?} of {frame:?}");
  let centre = (
    f64::from(content.x + x) + f64::from(width) / 2.0,
    f64::from(content.y + y) + f64::from(height) / 2.0,
  );
  let green = pixel_at(&pixels, carried(centre, frame, zoomed));
  assert!(
    green.1 > green.0 + 60 && green.1 > green.2 + 60,
    "{green:?}"
  );
  // It ends with the effect.
  let deadline = Instant::now() + Duration::from_millis(3 * EFFECT_MS);
  while !fixture.frontend.closing_animations.is_empty() {
    assert!(Instant::now() < deadline, "the animation ends");
    thread::sleep(Duration::from_millis(50));
    fixture
      .frontend
      .render(&mut fixture.host, &primary(), &mut target)
      .expect("the frame renders");
  }
}

#[test]
fn a_minimized_view_is_drawn_whole() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(animated("none", "none"));
  let composed = fixture.composed("minimized", false);
  render_until_idle(&mut fixture, &mut target);
  fixture
    .frontend
    .compositor_mut()
    .set_minimized(composed.window, true)
    .expect("the window minimizes");
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  let views = target.views.borrow();
  assert_eq!(views.len(), 1, "{views:?}");
  // The frame's pieces, the window and its subsurface.
  assert!(views[0].len() >= 3, "{views:?}");
}

#[test]
fn spin_draws_the_frame_with_the_window() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(animated("spin", "none"));
  let opened = Instant::now();
  let toplevel = fixture.window("spinning");
  let window = fixture.focused().expect("focused");
  let frame = fixture.frame_rect(window);
  half_way(opened);
  let (_, textured) = fixture.capture_frame(&mut target);
  // A window of one surface with a frame is drawn as one too: the frame
  // turns with it, where it was not drawn before.
  let drawn = target.views.borrow().last().cloned().expect("a view drawn");
  assert!(drawn.len() > 1, "the frame is drawn: {drawn:?}");
  assert_eq!(textured.len(), 1, "{textured:?}");
  let scale = f64::from(textured[0].width) / f64::from(frame.width);
  assert!((0.45..0.9).contains(&scale), "{textured:?} of {frame:?}");
  drop(toplevel);
}

#[test]
fn nothing_is_drawn_apart_without_an_animation_or_a_second_piece() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = animated("none", "none");
  config.decoration.forced_views = "none".to_owned().try_into().unwrap();
  let mut fixture = Fixture::new(config);
  let composed = fixture.composed("still", false);
  render_until_idle(&mut fixture, &mut target);
  assert!(target.views.borrow().is_empty());
  assert_eq!(fixture.frontend.view_textures.len(), 0);
  drop(composed);
  // One surface without a frame zooms from its own texture, as
  // Wayfire's transformer takes a single surface's texture as it is.
  let mut config = animated("zoom", "none");
  config.decoration.forced_views = "none".to_owned().try_into().unwrap();
  let mut fixture = Fixture::new(config);
  fixture.window("plain");
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert!(target.views.borrow().is_empty());
  // A second piece makes it a view of its own while it opens, and its
  // texture goes once the animation ended.
  let composed = fixture.composed("composed", false);
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert_eq!(target.views.borrow().len(), 1);
  fixture.frontend.open_animations.clear();
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert_eq!(target.views.borrow().len(), 1);
  assert_eq!(fixture.frontend.view_textures.len(), 0);
  drop(composed);
}

#[test]
fn scale_and_expo_move_a_subsurface_with_its_window() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = animated("none", "none");
  config.decoration.forced_views = "none".to_owned().try_into().unwrap();
  config.scale.duration_ms = 0;
  config.scale.title_overlay = "never".into();
  config.expo.duration_ms = 0;
  let mut fixture = Fixture::new(config);
  let composed = fixture.composed("scaled", false);
  let content = fixture.content(composed.window);
  let (x, y, width, height) = SUBSURFACE;
  let subsurface = RenderRect {
    x: content.x + x,
    y: content.y + y,
    width: width as u32,
    height: height as u32,
  };
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  let thumbnail = scale_geometry(&fixture, composed.window);
  render_until_idle(&mut fixture, &mut target);
  let drawn = target.frames.last().expect("a frame").clone();
  let expected = super::super::frames::map_rect(subsurface, content, thumbnail);
  assert!(drawn.contains(&(thumbnail, true)), "{drawn:?}");
  assert!(drawn.contains(&(expected, true)), "{drawn:?}");
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  render_until_idle(&mut fixture, &mut target);
  // Expo listed toplevels only, so it drew no subsurface at all.
  fixture.frontend.toggle_expo();
  fixture.process();
  for _ in 0..2 {
    fixture
      .frontend
      .render(&mut fixture.host, &primary(), &mut target)
      .expect("the frame renders");
  }
  let drawn = textured_tiles(&target);
  assert_eq!(drawn.len(), 2, "{drawn:?}");
  let (window, piece) = (drawn[0], drawn[1]);
  assert_eq!(
    piece,
    super::super::frames::map_rect(subsurface, content, window),
    "{drawn:?}"
  );
  drop(composed);
}

#[test]
fn the_switcher_leaves_popups_out_of_its_thumbnails() {
  let surfaces = |fixture: &Fixture, window| {
    super::super::whole_view::window_view(fixture.frontend.compositor(), window)
      .into_iter()
      .map(|snapshot| snapshot.role)
      .collect::<Vec<_>>()
  };
  let mut fixture = Fixture::new(animated("none", "none"));
  let composed = fixture.composed("popped", true);
  assert_eq!(
    surfaces(&fixture, composed.window),
    [
      shingle::SurfaceRole::Toplevel,
      shingle::SurfaceRole::Subsurface
    ]
  );
  assert_eq!(
    fixture
      .frontend
      .compositor()
      .window_surfaces(composed.window)
      .len(),
    3,
    "the popup is the window's"
  );
}

/// The textured tiles of the last frame.
fn textured_tiles(target: &RecordingTarget) -> Vec<RenderRect> {
  target
    .frames
    .last()
    .expect("a frame")
    .iter()
    .filter(|(_, textured)| *textured)
    .map(|(rect, _)| *rect)
    .collect()
}

/// A client that disconnects takes all its surfaces at once, its popup
/// too; the view is still drawn whole, frame included.
#[test]
fn a_view_whose_client_goes_closes_whole() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(animated("none", "zoom"));
  let base = fixture.base.clone();
  let mut client =
    TestClient::connect(fixture.frontend.compositor_mut(), &base);
  let compositor = fixture.frontend.compositor_mut();
  let toplevel = client.toplevel(compositor, "gone", "gone");
  let (x, y, width, height) = SUBSURFACE;
  let subsurface = client.subsurface(
    compositor,
    &toplevel.surface,
    (x, y),
    (width, height),
    GREEN,
  );
  let (x, y, width, height) = POPUP;
  let popup =
    client.popup(compositor, &toplevel, (x, y), (width, height), WHITE);
  fixture.process();
  render_until_idle(&mut fixture, &mut target);
  drop((toplevel, subsurface, popup));
  drop(client);
  fixture.process();
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  let views = target.views.borrow();
  assert_eq!(views.len(), 1, "{views:?}");
  // The frame's pieces, the window and its subsurface, no popup.
  assert!(views[0].len() > 2, "the frame is drawn too: {views:?}");
  assert!(
    !views[0]
      .iter()
      .any(|rect| rect.width == width as u32 && rect.height == height as u32),
    "{views:?}"
  );
}
