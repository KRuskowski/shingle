//! Configured colours are drawn as the bytes Wayfire writes for them
//! (crates/shingle-render/src/color.rs): the background, Expo's
//! background and the drop preview, on the GPU.

use shingle_render::srgb_fill;

use super::*;

/// `#336699FF`: mid-tones, which a linear reading draws much lighter.
fn colour() -> [f64; 4] {
  shingle_config::parse_color("#336699FF").expect("a colour")
}

/// The bytes Wayfire writes for [`colour`].
const BYTES: (u8, u8, u8) = (0x33, 0x66, 0x99);

/// No animations to wait for.
fn config() -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.tiling.animation_duration = 0;
  config.expo.duration_ms = 0;
  config
}

#[test]
fn the_background_colour_is_drawn_as_its_bytes() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = config();
  config.background_color = colour();
  let mut fixture = Fixture::new(config);
  assert_eq!(fixture.pixel(&mut target, 10, 10), BYTES);
  assert_eq!(fixture.pixel(&mut target, 999, 799), BYTES);
}

#[test]
fn expos_background_is_drawn_as_its_bytes() {
  let Some(mut target) = recording_target() else {
    return;
  };
  let mut config = config();
  config.expo.background = colour();
  let mut fixture = Fixture::new(config);
  fixture.window("alpha");
  fixture.tap(KEY_E, &[KEY_LEFTMETA]);
  let empty = fixture.frontend.expo.as_ref().expect("Expo is open").cells[2];
  // Around the cells, and inside an empty workspace's cell.
  assert_eq!(fixture.pixel(&mut target, 0, 0), BYTES);
  let centre = (
    empty.x + empty.width as i32 / 2,
    empty.y + empty.height as i32 / 2,
  );
  assert_eq!(fixture.pixel(&mut target, centre.0, centre.1), BYTES);
}

#[test]
fn the_drop_preview_is_drawn_in_its_premultiplied_bytes() {
  let mut config = config();
  config.tiling.preview_base_color = [0.5, 0.5, 1.0, 0.5];
  config.tiling.preview_border_color = colour();
  config.tiling.preview_border_width = 3;
  let mut fixture = Fixture::new(config);
  let rect = RenderRect {
    x: 100,
    y: 100,
    width: 200,
    height: 100,
  };
  fixture.frontend.drop_preview = Some(rect);
  let mut tiles = Vec::new();
  fixture.frontend.append_drop_preview(&mut tiles);
  // Premultiplied as Wayfire's colour rectangles are, and linear.
  assert_eq!(tiles[0].color, srgb_fill([0.5, 0.5, 1.0, 0.5]));
  assert_eq!(tiles[0].color[2], 0.5);
  assert!(
    tiles[1..]
      .iter()
      .all(|tile| tile.color == srgb_fill(colour()))
  );
  let Some(mut target) = recording_target() else {
    return;
  };
  // The border, opaque, shows its bytes.
  assert_eq!(fixture.pixel(&mut target, 101, 150), BYTES);
}
