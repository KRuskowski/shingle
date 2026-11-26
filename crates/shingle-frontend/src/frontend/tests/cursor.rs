//! The cursor: frames where the drawn cursor moves, none for a cursor on
//! a cursor plane, and what such a plane is told to show (NB-10).

use wayland_server::protocol::wl_output;

use super::*;
use crate::{CursorImage, CursorPlane};

/// A fixture whose compositor draws the default arrow, as native's.
fn with_default_cursor() -> Fixture {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.frontend.compositor_mut().set_default_cursor(true);
  fixture.process();
  fixture
}

/// The default arrow's place in the layout.
fn cursor_geometry(fixture: &Fixture) -> shingle_core::Rect {
  fixture
    .frontend
    .compositor()
    .cursor_on(&primary())
    .expect("the default arrow is on the output")
    .geometry
}

#[test]
fn a_moving_drawn_cursor_asks_for_a_frame() {
  let mut fixture = with_default_cursor();
  fixture.host.redrawn.clear();
  fixture.move_pointer(100.0, 50.0);
  assert!(
    fixture.host.redrawn.contains(&primary()),
    "the frontend draws the cursor, so it needs a frame where it moved"
  );
  fixture.host.redrawn.clear();
  fixture.process();
  assert!(fixture.host.redrawn.is_empty(), "standing still needs none");
}

#[test]
fn a_cursor_on_a_plane_moves_without_a_frame() {
  let mut fixture = with_default_cursor();
  fixture.host.redrawn.clear();
  fixture.frontend.set_hardware_cursor(&primary(), true);
  fixture.process();
  assert_eq!(
    fixture.host.redrawn,
    [primary()],
    "one frame leaves the drawn cursor out"
  );
  assert!(fixture.frontend.hardware_cursor(&primary()));
  fixture.host.redrawn.clear();
  fixture.move_pointer(200.0, 120.0);
  fixture.move_pointer(210.0, 130.0);
  assert!(
    fixture.host.redrawn.is_empty(),
    "{:?}",
    fixture.host.redrawn
  );
  fixture.frontend.set_hardware_cursor(&primary(), false);
  fixture.process();
  assert_eq!(
    fixture.host.redrawn,
    [primary()],
    "the drawn cursor returns"
  );
}

#[test]
fn the_cursor_plane_shows_the_cursor_where_the_pointer_is() {
  let mut fixture = with_default_cursor();
  fixture.move_pointer(100.0, 50.0);
  let geometry = cursor_geometry(&fixture);
  let CursorPlane::Image(image) =
    fixture.frontend.cursor_plane(&primary(), (256, 256))
  else {
    panic!("the arrow fits the plane");
  };
  let CursorImage {
    width,
    height,
    rgba,
    position,
  } = image;
  assert_eq!((width, height), (geometry.width, geometry.height));
  assert_eq!(rgba.len(), (width * height * 4) as usize);
  assert_eq!(position, (geometry.x, geometry.y));
  // The same image after a move, at its new place.
  fixture.move_pointer(300.0, 70.0);
  let CursorPlane::Image(moved) =
    fixture.frontend.cursor_plane(&primary(), (256, 256))
  else {
    panic!("the arrow still fits");
  };
  assert!(
    std::sync::Arc::ptr_eq(&moved.rgba, &rgba),
    "the same pixels"
  );
  assert_eq!(moved.position, (geometry.x + 200, geometry.y + 20));
}

#[test]
fn the_cursor_plane_is_empty_without_a_cursor_and_off_its_output() {
  let mut fixture = Fixture::new(ShingleConfig::default());
  fixture.process();
  assert_eq!(
    fixture.frontend.cursor_plane(&primary(), (64, 64)),
    CursorPlane::Hidden,
    "no default cursor and no client cursor"
  );
  assert_eq!(
    fixture
      .frontend
      .cursor_plane(&OutputHandle::new("NOPE-1"), (64, 64)),
    CursorPlane::Hidden
  );
}

#[test]
fn what_the_plane_cannot_show_stays_drawn() {
  let mut fixture = with_default_cursor();
  let software = |fixture: &Fixture, max| match fixture
    .frontend
    .cursor_plane(&primary(), max)
  {
    CursorPlane::Software(reason) => reason,
    other => panic!("drawn in software, not {other:?}"),
  };
  let geometry = cursor_geometry(&fixture);
  assert_eq!(
    software(&fixture, (geometry.width - 1, 256)),
    format!(
      "the cursor ({}x{}) is larger than the cursor plane ({}x256)",
      geometry.width,
      geometry.height,
      geometry.width - 1
    )
  );
  fixture.frontend.inverted = true;
  assert_eq!(software(&fixture, (256, 256)), "colours are inverted");
  fixture.frontend.inverted = false;
  let compositor = fixture.frontend.compositor_mut();
  compositor.set_output_transform(wl_output::Transform::_90);
  assert_eq!(
    software(&fixture, (256, 256)),
    "the output is rotated or flipped"
  );
  let compositor = fixture.frontend.compositor_mut();
  compositor.set_output_transform(wl_output::Transform::Normal);
  compositor.set_output_scale(2.0).unwrap();
  assert_eq!(
    software(&fixture, (256, 256)),
    "the cursor's buffer scale 1 is not the output's scale 2"
  );
}

#[test]
fn frames_leave_out_a_cursor_on_a_plane() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = with_default_cursor();
  fixture.move_pointer(100.0, 50.0);
  let geometry = cursor_geometry(&fixture);
  let drawn = |target: &RecordingTarget| {
    let frame = target.frames.last().expect("a frame was drawn");
    frame.iter().any(|(rect, _)| {
      (rect.x, rect.y, rect.width, rect.height)
        == (geometry.x, geometry.y, geometry.width, geometry.height)
    })
  };
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert!(drawn(&target), "the frontend draws the cursor");
  fixture.frontend.set_hardware_cursor(&primary(), true);
  fixture
    .frontend
    .render(&mut fixture.host, &primary(), &mut target)
    .expect("the frame renders");
  assert!(!drawn(&target), "the plane shows it");
}
