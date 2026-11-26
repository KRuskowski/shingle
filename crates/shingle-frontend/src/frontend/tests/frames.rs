//! Server-side frames through the frontend (docs/design/wayfire-gaps.md,
//! WG-6): their tiles below the window and their colours on the GPU, and
//! the title bar's drag and buttons through the frontend's input.

use shingle::frame::{FrameAreaKind, FrameButton, FrameSnapshot};

use super::*;

const BTN_LEFT: u32 = 0x110;

/// Every window framed, in opaque red when focused and blue otherwise,
/// without animations to wait for.
fn framed() -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.tiling.animation_duration = 0;
  config.decoration.forced_views = "all".to_owned().try_into().unwrap();
  config.decoration.active_color = [1.0, 0.0, 0.0, 1.0];
  config.decoration.inactive_color = [0.0, 0.0, 1.0, 1.0];
  config
}

impl Fixture {
  /// The frame of the only window with one, or of `window`.
  fn frame_of(&mut self, window: Option<TileWindowId>) -> FrameSnapshot {
    let frames = self.frontend.compositor().frames();
    frames
      .into_values()
      .find(|frame| window.is_none_or(|window| frame.window == window))
      .expect("a frame")
  }

  /// The pixel at layout point (`x`, `y`) as drawn, as (r, g, b).
  fn pixel(
    &mut self,
    target: &mut RecordingTarget,
    x: i32,
    y: i32,
  ) -> (u8, u8, u8) {
    let capture = self
      .client
      .capture(self.frontend.compositor_mut(), (x, y, 1, 1));
    self.frontend.process(&mut self.host);
    self
      .frontend
      .render(&mut self.host, &primary(), target)
      .expect("the frame renders");
    self.process();
    first_pixel(&capture.bytes())
  }
}

fn rect(frame: shingle_core::Rect) -> RenderRect {
  RenderRect {
    x: frame.x,
    y: frame.y,
    width: frame.width,
    height: frame.height,
  }
}

#[test]
fn a_frame_is_drawn_below_its_window_in_the_focus_colours() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut fixture = Fixture::new(framed());
  fixture.window("first");
  render_until_idle(&mut fixture, &mut target);
  let frame = fixture.frame_of(None);
  let drawn = target.frames.last().expect("a frame").clone();
  // Four bands, the title, three buttons, then the client's surface.
  assert_eq!(drawn.len(), 9, "{drawn:?}");
  let outer = frame.rect;
  let margins = frame.margins;
  assert_eq!(
    drawn[0],
    (
      RenderRect {
        x: outer.x,
        y: outer.y,
        width: outer.width,
        height: margins.top
      },
      false
    )
  );
  assert!(drawn[..4].iter().all(|(_, textured)| !textured));
  let title = frame.layout.title().unwrap();
  assert_eq!(
    drawn[4],
    (
      RenderRect {
        x: outer.x + title.x,
        y: outer.y + title.y,
        width: title.width,
        height: title.height
      },
      true
    )
  );
  assert!(drawn[5..8].iter().all(|(_, textured)| *textured));
  assert_eq!(drawn[8], (rect(margins.shrink(outer)), true));
  // The border, and the title bar between the buttons, are the focused
  // colour's bytes.
  let close = frame
    .layout
    .areas
    .iter()
    .find(|area| area.kind == FrameAreaKind::Button(FrameButton::Close))
    .unwrap()
    .rect;
  assert_eq!(
    fixture.pixel(&mut target, outer.x + 1, outer.y + 1),
    (255, 0, 0)
  );
  let between = (outer.x + close.x - 2, outer.y + close.y + 2);
  assert_eq!(
    fixture.pixel(&mut target, between.0, between.1),
    (255, 0, 0)
  );
  let bottom = outer.y + outer.height as i32 - 1;
  assert_eq!(
    fixture.pixel(&mut target, outer.x + 100, bottom),
    (255, 0, 0)
  );
  // A second window takes the focus: the first frame turns blue.
  let first = frame.window;
  fixture.window("second");
  render_until_idle(&mut fixture, &mut target);
  let frame = fixture.frame_of(Some(first));
  assert!(!frame.activated);
  let outer = frame.rect;
  assert_eq!(
    fixture.pixel(&mut target, outer.x + 1, outer.y + 1),
    (0, 0, 255)
  );
}

#[test]
fn dragging_the_title_bar_moves_a_floating_window() {
  let mut fixture = Fixture::new(framed());
  fixture.window("floating");
  let window = fixture.frame_of(None).window;
  fixture
    .frontend
    .compositor_mut()
    .toggle_tiling(window)
    .unwrap();
  fixture.process();
  let before = fixture.frame_of(None).rect;
  let (x, y) = (f64::from(before.x) + 60.0, f64::from(before.y) + 20.0);
  fixture.move_pointer(x, y);
  fixture.button(BTN_LEFT, true);
  // Five pixels do not move it yet; past them the frontend drags it.
  fixture.move_pointer(x + 4.0, y);
  fixture.move_pointer(x + 10.0, y);
  fixture.move_pointer(x + 60.0, y + 30.0);
  fixture.button(BTN_LEFT, false);
  let after = fixture.frame_of(None).rect;
  assert_eq!((after.x - before.x, after.y - before.y), (50, 30));
  assert_eq!((after.width, after.height), (before.width, before.height));
}

#[test]
fn the_maximize_button_works_through_the_frontend() {
  let mut fixture = Fixture::new(framed());
  fixture.window("floating");
  let window = fixture.frame_of(None).window;
  fixture
    .frontend
    .compositor_mut()
    .toggle_tiling(window)
    .unwrap();
  fixture.process();
  let frame = fixture.frame_of(None);
  let button = frame
    .layout
    .areas
    .iter()
    .find(|area| area.kind == FrameAreaKind::Button(FrameButton::Maximize))
    .unwrap()
    .rect;
  let x = f64::from(frame.rect.x + button.x) + f64::from(button.width) / 2.0;
  let y = f64::from(frame.rect.y + button.y) + f64::from(button.height) / 2.0;
  fixture.move_pointer(x, y);
  assert_eq!(
    fixture.frame_of(None).hovered,
    frame
      .layout
      .area_at(x - f64::from(frame.rect.x), y - f64::from(frame.rect.y))
  );
  fixture.button(BTN_LEFT, true);
  assert!(fixture.frame_of(None).pressed.is_some());
  fixture.button(BTN_LEFT, false);
  let info = fixture
    .frontend
    .compositor()
    .core()
    .window_info(window)
    .unwrap();
  assert!(info.state.maximized);
  assert_eq!(
    fixture.frame_of(None).rect,
    fixture.frontend.compositor().workarea()
  );
}
