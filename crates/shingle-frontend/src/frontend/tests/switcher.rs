//! The regular switcher's view (docs/design/wayfire-gaps.md, WG-7)
//! through the frontend's input: selection order both ways, letting go
//! when a modifier comes up, views closing while it runs, one view, and
//! the thumbnails drawn over black on the GPU.

use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;

use super::*;
use crate::switcher_view::BACKGROUND_DIM_FACTOR;

/// Windows that appear at once and tiles that do not slide.
fn config() -> ShingleConfig {
  let mut config = ShingleConfig::default();
  config.animate.open_animation = "none".into();
  config.tiling.animation_duration = 0;
  config
}

impl Fixture {
  /// The switcher's selection, while it is held.
  fn selected(&self) -> Option<TileWindowId> {
    let state = self.frontend.switcher.as_ref()?;
    assert_eq!(state.kind, SwitcherKind::Regular);
    state.windows.get(state.selected).copied()
  }

  /// Maps a window for each of `app_ids`, in order, and returns them.
  fn windows<const N: usize>(
    &mut self,
    app_ids: [&str; N],
  ) -> [TileWindowId; N] {
    app_ids.map(|app_id| {
      self.window(app_id);
      self.focused().expect("a new window takes the focus")
    })
  }
}

#[test]
fn the_switcher_selects_in_focus_order_both_ways() {
  let mut fixture = Fixture::new(config());
  let [alpha, bravo, charlie] = fixture.windows(["alpha", "bravo", "charlie"]);
  // Focused last first: charlie, bravo, alpha.
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(bravo));
  assert_eq!(fixture.focused(), Some(charlie), "focus waits for release");
  let mut order = Vec::new();
  for _ in 0..3 {
    fixture.tap(KEY_TAB, &[]);
    order.push(fixture.selected().expect("switching"));
  }
  assert_eq!(order, [alpha, charlie, bravo]);
  fixture.key(KEY_LEFTALT, false);
  assert!(fixture.frontend.switcher.is_none());
  assert_eq!(fixture.focused(), Some(bravo));
  assert!(
    fixture.frontend.switcher_view.is_some(),
    "the thumbnails move back first"
  );
  // Now bravo, charlie, alpha. The first Shift press selects the next
  // view, as Wayfire's does, while the thumbnails still move back; the
  // next one goes backwards.
  fixture.key(KEY_LEFTALT, true);
  fixture.key(KEY_LEFTSHIFT, true);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(charlie));
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(bravo));
  // Forward again without Shift; the switcher still ends with it.
  fixture.key(KEY_LEFTSHIFT, false);
  assert!(fixture.frontend.switcher.is_none(), "Shift started it");
  assert_eq!(fixture.focused(), Some(bravo));
  fixture.key(KEY_LEFTALT, false);
}

#[test]
fn the_first_press_keeps_its_modifiers_and_any_of_them_lets_go() {
  let mut fixture = Fixture::new(config());
  let [alpha, bravo, charlie] = fixture.windows(["alpha", "bravo", "charlie"]);
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(bravo));
  // Alt+Shift+Tab moves back without making Shift one of them.
  fixture.tap(KEY_TAB, &[KEY_LEFTSHIFT]);
  assert_eq!(fixture.selected(), Some(charlie));
  fixture.tap(KEY_TAB, &[]);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(alpha));
  // Other keys go nowhere while it is held.
  fixture.tap(KEY_A, &[]);
  assert_eq!(fixture.selected(), Some(alpha));
  fixture.key(KEY_LEFTALT, false);
  assert_eq!(fixture.focused(), Some(alpha));
}

#[test]
fn a_view_closing_while_switching_rearranges_around_the_focused_one() {
  let mut fixture = Fixture::new(config());
  let toplevels = ["alpha", "bravo", "charlie"].map(|app_id| {
    let toplevel = fixture.window(app_id);
    (toplevel, fixture.focused().expect("focused"))
  });
  let [(_, alpha), (bravo_toplevel, bravo), (_, charlie)] = &toplevels;
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(*bravo));
  // The selected view goes: Wayfire arranges the rest anew, with the
  // focused view in the middle.
  bravo_toplevel.surface.attach(None, 0, 0);
  bravo_toplevel.surface.commit();
  fixture.process();
  let state = fixture.frontend.switcher.as_ref().expect("still held");
  assert_eq!(state.windows, [*charlie, *alpha]);
  assert_eq!(fixture.selected(), Some(*charlie));
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(*alpha));
  fixture.key(KEY_LEFTALT, false);
  assert_eq!(fixture.focused(), Some(*alpha));
}

#[test]
fn the_last_view_closing_lets_go() {
  let mut fixture = Fixture::new(config());
  let toplevel = fixture.window("alpha");
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert!(fixture.frontend.switcher.is_some());
  toplevel.surface.attach(None, 0, 0);
  toplevel.surface.commit();
  fixture.process();
  assert!(fixture.frontend.switcher.is_none());
  fixture.key(KEY_LEFTALT, false);
}

#[test]
fn one_view_is_selected_and_stays_selected() {
  let mut fixture = Fixture::new(config());
  // Nothing to switch to: no switcher.
  fixture.tap(KEY_TAB, &[KEY_LEFTALT]);
  assert!(fixture.frontend.switcher.is_none());
  assert!(fixture.frontend.switcher_view.is_none());
  let [alpha] = fixture.windows(["alpha"]);
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(alpha));
  fixture.tap(KEY_TAB, &[]);
  fixture.tap(KEY_TAB, &[KEY_LEFTSHIFT]);
  assert_eq!(fixture.selected(), Some(alpha));
  fixture.key(KEY_LEFTALT, false);
  assert_eq!(fixture.focused(), Some(alpha));
}

#[test]
fn minimizing_the_focused_view_leaves_the_focus_order_alone() {
  let mut fixture = Fixture::new(config());
  let [alpha, bravo, charlie] = fixture.windows(["alpha", "bravo", "charlie"]);
  // Most recently focused first, by `last-focus-timestamp`.
  let order =
    |fixture: &Fixture| fixture.frontend.compositor().switchable_windows();
  assert_eq!(order(&fixture), [charlie, bravo, alpha]);
  fixture
    .frontend
    .compositor_mut()
    .set_minimized(charlie, true)
    .expect("charlie minimizes");
  fixture.process();
  // Bravo gets the focus but, as in Wayfire, not a newer focus time.
  assert_eq!(fixture.focused(), Some(bravo));
  assert_eq!(order(&fixture), [charlie, bravo, alpha]);
}

#[test]
fn a_minimized_view_can_be_selected_and_is_restored() {
  let mut fixture = Fixture::new(config());
  let [alpha, bravo, charlie] = fixture.windows(["alpha", "bravo", "charlie"]);
  fixture
    .frontend
    .compositor_mut()
    .set_minimized(charlie, true)
    .expect("charlie minimizes");
  fixture.process();
  assert_eq!(fixture.focused(), Some(bravo));
  // The refocus after a minimize does not count as focusing bravo, as in
  // Wayfire, whose `seat_t::refocus` leaves the focus timestamps alone:
  // by focus order the minimized charlie is in the middle, so the first
  // press selects bravo, as Wayfire 0.9 does (tools/gpu_test/
  // focus_checks.py), then alpha and charlie.
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(bravo));
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(alpha));
  fixture.tap(KEY_TAB, &[]);
  assert_eq!(fixture.selected(), Some(charlie));
  fixture.key(KEY_LEFTALT, false);
  assert_eq!(fixture.focused(), Some(charlie));
  let info = fixture.frontend.compositor().core().window_info(charlie);
  assert!(info.is_ok_and(|info| !info.state.minimized), "restored");
}

#[test]
fn the_switcher_is_a_plugin_while_its_view_is_up() {
  let mut fixture = Fixture::new(config());
  fixture.windows(["alpha", "bravo"]);
  fixture.tap(KEY_TAB, &[KEY_LEFTALT]);
  assert!(fixture.frontend.switcher.is_none());
  assert!(fixture.frontend.switcher_view.is_some());
  // Scale ends it at once.
  fixture.tap(KEY_P, &[KEY_LEFTMETA]);
  assert!(fixture.frontend.scale.is_some());
  assert!(fixture.frontend.switcher_view.is_none());
}

#[test]
fn the_thumbnails_are_drawn_over_black_and_go_after_letting_go() {
  let Some(mut target) = recording_target() else {
    return;
  };
  // Without the switcher's animation everything is in place at once.
  let mut config = config();
  config.switcher.duration_ms = 0;
  let mut fixture = Fixture::new(config);
  let compositor = fixture.frontend.compositor_mut();
  let (_wallpaper, _wallpaper_role) =
    fixture
      .client
      .layer_surface(compositor, Layer::Background, 40);
  let compositor = fixture.frontend.compositor_mut();
  let (_panel, _panel_role) =
    fixture.client.layer_surface(compositor, Layer::Top, 30);
  fixture.process();
  let windows = fixture.windows(["alpha", "bravo", "charlie"]);
  let frame = |fixture: &mut Fixture, target: &mut RecordingTarget| {
    fixture.process();
    fixture
      .frontend
      .render(&mut fixture.host, &primary(), target)
      .expect("the frame renders");
    (
      target.clears.last().copied().expect("a frame"),
      target.looks.last().cloned().expect("a frame"),
      target.frames.last().cloned().expect("a frame"),
    )
  };
  let (clear, looks, _) = frame(&mut fixture, &mut target);
  assert_ne!(clear, [0.0, 0.0, 0.0, 1.0]);
  assert!(looks.iter().all(|(_, projection)| projection.is_none()));
  fixture.key(KEY_LEFTALT, true);
  fixture.tap(KEY_TAB, &[]);
  let (clear, looks, frames) = frame(&mut fixture, &mut target);
  assert_eq!(clear, [0.0, 0.0, 0.0, 1.0], "over black");
  // The wallpaper, the three thumbnails, the panel: the wallpaper dimmed
  // first, the panel as it is last.
  let projected = looks
    .iter()
    .filter(|(_, projection)| projection.is_some())
    .count();
  assert_eq!(projected, 3, "{looks:?}");
  let height = |height: u32| {
    frames
      .iter()
      .position(|(rect, _)| rect.height == height)
      .expect("a layer surface")
  };
  let (wallpaper, panel) = (height(40), height(30));
  let dim = BACKGROUND_DIM_FACTOR as f32;
  let (color, projection) = looks[wallpaper];
  assert!(wallpaper == 0 && projection.is_none());
  assert!(
    (color[0] - dim).abs() < 1e-6 && color[3] == 1.0,
    "{color:?}"
  );
  let (color, projection) = looks[panel];
  assert!(projection.is_none() && color == [1.0; 4], "{looks:?}");
  assert!(
    looks[panel + 1..]
      .iter()
      .all(|(_, projection)| projection.is_none()),
    "the panel is above the thumbnails"
  );
  // The selected thumbnail is drawn last of them, its centre in the
  // output's middle; the others at 0.7.
  let (index, (color, projection)) = looks
    .iter()
    .enumerate()
    .rfind(|(_, (_, projection))| projection.is_some())
    .expect("thumbnails");
  assert_eq!(*color, [1.0; 4]);
  let rect = frames[index].0;
  let centre = projection.expect("projected").apply(
    rect.x as f32 + rect.width as f32 / 2.0,
    rect.y as f32 + rect.height as f32 / 2.0,
  );
  let ([x, y], _) = centre.expect("in front of the eye");
  assert!(
    (x - 500.0).abs() < 1.0 && (y - 400.0).abs() < 1.0,
    "{x} {y}"
  );
  for (color, projection) in &looks {
    if projection.is_some() && *color != [1.0; 4] {
      assert!((color[3] - 0.7).abs() < 1e-6, "{color:?}");
    }
  }
  assert_eq!(fixture.selected(), Some(windows[1]));
  // Letting go draws them back in place once more, then the scene is
  // the usual one.
  fixture.key(KEY_LEFTALT, false);
  let (clear, looks, _) = frame(&mut fixture, &mut target);
  assert_eq!(clear, [0.0, 0.0, 0.0, 1.0]);
  assert!(looks.iter().any(|(_, projection)| projection.is_some()));
  assert!(fixture.frontend.switcher_view.is_none(), "it is over");
  let (clear, looks, _) = frame(&mut fixture, &mut target);
  assert_ne!(clear, [0.0, 0.0, 0.0, 1.0]);
  assert!(looks.iter().all(|(_, projection)| projection.is_none()));
  assert_eq!(fixture.focused(), Some(windows[1]));
}
