//! The server-side frame of Wayfire's `decoration` plugin
//! (wayfire:plugins/decor/): its margins, the layout of the title bar,
//! buttons and borders, and what pointer input on it does.
//!
//! Everything here is in frame coordinates, whose origin is the frame's
//! top-left corner, the corner of the window's geometry: in Wayfire, and
//! so in Shingle, a decorated window's geometry includes its frame, and the
//! client's surface is that geometry shrunk by [`Margins`]
//! (wayfire:plugins/decor/decoration.cpp, `adjust_new_decorations`).
//! Compositor glue is in `decorator.rs`; the frontend draws the frame from
//! a [`FrameSnapshot`].

use std::time::Duration;

use shingle_config::DecorationConfig;
use shingle_core::{Rect, WindowId};

/// `xdg_toplevel` resize edges, which are wlroots' `WLR_EDGE_*`.
const EDGE_TOP: u32 = 1;
const EDGE_BOTTOM: u32 = 2;
const EDGE_LEFT: u32 = 4;
const EDGE_RIGHT: u32 = 8;
/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;
/// How far a press on the title bar moves before it starts a move, as
/// Wayfire's `handle_motion` checks.
const MOVE_THRESHOLD: f64 = 5.0;
/// Wayfire's double-click timer on the title bar.
const DOUBLE_CLICK: Duration = Duration::from_millis(300);

/// A title bar button.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrameButton {
  /// Minimizes the window.
  Minimize,
  /// Toggles maximize.
  Maximize,
  /// Asks the client to close.
  Close,
}

/// What an area of the frame is for, as Wayfire's
/// `decoration_area_type_t`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameAreaKind {
  /// A button.
  Button(FrameButton),
  /// The title bar around the buttons, which moves the window.
  Move,
  /// The title, drawn and moving the window.
  Title,
  /// A border that resizes on the given `xdg_toplevel` edges.
  Resize(u32),
  /// A border of a window that cannot be resized; it takes input but
  /// does nothing.
  Empty,
}

/// One area of a [`FrameLayout`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameArea {
  /// What it is for.
  pub kind: FrameAreaKind,
  /// Where it is, in frame coordinates.
  pub rect: Rect,
}

/// How far a frame reaches beyond the client's surface on each side.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Margins {
  /// Left border.
  pub left: u32,
  /// Right border.
  pub right: u32,
  /// Title bar and top border.
  pub top: u32,
  /// Bottom border.
  pub bottom: u32,
}

impl Margins {
  /// No frame.
  pub const ZERO: Margins = Margins {
    left: 0,
    right: 0,
    top: 0,
    bottom: 0,
  };

  /// The client's rectangle inside `outer`, Wayfire's
  /// `shrink_geometry_by_margins`. It keeps at least one pixel each way,
  /// because a zero size would let an xdg client choose its own.
  pub fn shrink(self, outer: Rect) -> Rect {
    Rect::new(
      outer.x.saturating_add(self.left as i32),
      outer.y.saturating_add(self.top as i32),
      outer
        .width
        .saturating_sub(self.left.saturating_add(self.right))
        .max(1),
      outer
        .height
        .saturating_sub(self.top.saturating_add(self.bottom))
        .max(1),
    )
  }

  /// The frame around the client's rectangle `inner`, Wayfire's
  /// `expand_geometry_by_margins`.
  pub fn expand(self, inner: Rect) -> Rect {
    Rect::new(
      inner.x.saturating_sub(self.left as i32),
      inner.y.saturating_sub(self.top as i32),
      inner
        .width
        .saturating_add(self.left.saturating_add(self.right)),
      inner
        .height
        .saturating_add(self.top.saturating_add(self.bottom)),
    )
  }
}

/// The sizes of a frame, from `[decoration]`, as Wayfire's
/// `decoration_theme_t` and the constants of `decoration_layout_t`.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameTheme {
  /// Height of the title bar.
  pub title_height: u32,
  /// Width of the borders.
  pub border_size: u32,
  /// Width and height of a button: `title_height` times `button_scale`,
  /// truncated.
  pub button_size: u32,
  /// Horizontal padding on each side of a button.
  pub button_padding: i32,
  /// The buttons of `button_order` from left to right.
  pub button_order: Vec<FrameButton>,
}

impl FrameTheme {
  /// The theme of `config`. Unknown names in `button_order` are dropped,
  /// as Wayfire's `create_buttons` drops them; repeated ones stay.
  pub fn new(config: &DecorationConfig) -> Self {
    let button_order = config
      .button_order
      .split_whitespace()
      .filter_map(|name| match name {
        "minimize" => Some(FrameButton::Minimize),
        "maximize" => Some(FrameButton::Maximize),
        "close" => Some(FrameButton::Close),
        _ => None,
      })
      .collect();
    Self {
      title_height: config.title_height,
      border_size: config.border_size,
      button_size: (f64::from(config.title_height) * config.button_scale)
        as u32,
      button_padding: config.button_padding,
      button_order,
    }
  }

  /// The margins of a frame, Wayfire's `simple_decorator_t::get_margins`
  /// for a window that is not fullscreen.
  pub fn margins(&self) -> Margins {
    Margins {
      left: self.border_size,
      right: self.border_size,
      top: self.title_height.saturating_add(self.border_size),
      bottom: self.border_size,
    }
  }
}

impl Default for FrameTheme {
  fn default() -> Self {
    Self::new(&DecorationConfig::default())
  }
}

/// The areas of one frame, in Wayfire's order: buttons from right to
/// left, the title bar around them, the title, then the left, right, top
/// and bottom borders (`decoration_layout_t::resize`). The first area
/// containing a point is the one it hits.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameLayout {
  /// The areas, first match first.
  pub areas: Vec<FrameArea>,
}

impl FrameLayout {
  /// The layout of a `width` by `height` frame. A dialog has no minimize
  /// button, as Wayfire gives a view with a parent only maximize and
  /// close; without `allow_resize` (a tiled or maximized window) the
  /// borders do nothing. Areas without width or height are left out:
  /// Wayfire keeps them, but they contain no point.
  pub fn new(
    theme: &FrameTheme,
    width: u32,
    height: u32,
    dialog: bool,
    allow_resize: bool,
  ) -> Self {
    let width = i64::from(width);
    let height = i64::from(height);
    let border = i64::from(theme.border_size);
    let title_height = i64::from(theme.title_height);
    let button = i64::from(theme.button_size);
    let padding = i64::from(theme.button_padding);
    let mut areas = Vec::new();
    let mut push = |kind, x: i64, y: i64, w: i64, h: i64| {
      if w > 0 && h > 0 {
        let clamp = |value: i64| {
          value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        areas.push(FrameArea {
          kind,
          rect: Rect::new(clamp(x), clamp(y), w as u32, h as u32),
        });
      }
    };
    if title_height > 0 {
      let buttons = theme
        .button_order
        .iter()
        .copied()
        .filter(|button| !dialog || *button != FrameButton::Minimize)
        .collect::<Vec<_>>();
      let per_button = 2 * padding + button;
      // One more padding to start with, as in Wayfire.
      let mut x = width - border + padding;
      let y = (title_height - button) / 2 + border;
      for kind in buttons.iter().rev() {
        x -= per_button;
        push(FrameAreaKind::Button(*kind), x, y, button, button);
      }
      let total = -padding + buttons.len() as i64 * per_button;
      push(FrameAreaKind::Move, x, border, total, title_height);
      push(
        FrameAreaKind::Title,
        border,
        border,
        x - border,
        title_height,
      );
    }
    let resize = |edges| {
      if allow_resize {
        FrameAreaKind::Resize(edges)
      } else {
        FrameAreaKind::Empty
      }
    };
    push(resize(EDGE_LEFT), 0, 0, border, height);
    push(resize(EDGE_RIGHT), width - border, 0, border, height);
    push(resize(EDGE_TOP), 0, 0, width, border);
    push(resize(EDGE_BOTTOM), 0, height - border, width, border);
    Self { areas }
  }

  /// The index of the first area containing frame point (`x`, `y`).
  pub fn area_at(&self, x: f64, y: f64) -> Option<usize> {
    self.areas.iter().position(|area| contains(area.rect, x, y))
  }

  /// Whether frame point (`x`, `y`) is on the frame: Wayfire's
  /// `calculate_region`, the union of the areas.
  pub fn contains(&self, x: f64, y: f64) -> bool {
    self.area_at(x, y).is_some()
  }

  /// The edges of every resize area containing (`x`, `y`), so a corner
  /// resizes two, as Wayfire's `calculate_resize_edges`.
  pub fn resize_edges_at(&self, x: f64, y: f64) -> u32 {
    self
      .areas
      .iter()
      .filter(|area| contains(area.rect, x, y))
      .filter_map(|area| match area.kind {
        FrameAreaKind::Resize(edges) => Some(edges),
        _ => None,
      })
      .fold(0, |all, edges| all | edges)
  }

  /// The title's rectangle, if the frame has one.
  pub fn title(&self) -> Option<Rect> {
    self
      .areas
      .iter()
      .find(|area| area.kind == FrameAreaKind::Title)
      .map(|area| area.rect)
  }
}

fn contains(rect: Rect, x: f64, y: f64) -> bool {
  x >= f64::from(rect.x)
    && y >= f64::from(rect.y)
    && x < f64::from(rect.x) + f64::from(rect.width)
    && y < f64::from(rect.y) + f64::from(rect.height)
}

/// What a frame needs drawn, for the frontend: one per decorated window
/// that is not fullscreen, from `Compositor::frames`.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSnapshot {
  /// The window.
  pub window: WindowId,
  /// The frame, which is the window's geometry, in layout coordinates.
  pub rect: Rect,
  /// Where the client's surface is inside it.
  pub margins: Margins,
  /// The window is focused, and is drawn in `active_color`.
  pub activated: bool,
  /// The window's title.
  pub title: String,
  /// The frame's areas.
  pub layout: FrameLayout,
  /// The area of the button under the pointer.
  pub hovered: Option<usize>,
  /// The area of the button being pressed.
  pub pressed: Option<usize>,
}

/// What pointer input on a frame asks for, Wayfire's
/// `decoration_layout_action_t` without the half maximizes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameAction {
  /// Nothing.
  None,
  /// An interactive move.
  Move,
  /// An interactive resize on these edges.
  Resize(u32),
  /// Close the window.
  Close,
  /// Maximize it, or restore it.
  ToggleMaximize,
  /// Minimize it.
  Minimize,
}

/// The pointer's state on one frame, Wayfire's `decoration_layout_t`
/// input handling (`handle_motion`, `handle_press_event`,
/// `handle_focus_lost`). Positions are in frame coordinates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameInput {
  current: Option<(f64, f64)>,
  grabbed: bool,
  grab_origin: (f64, f64),
  pressed: Option<usize>,
  double_click_until: Option<Duration>,
  double_click_at_release: bool,
}

impl FrameInput {
  /// The pointer moved to (`x`, `y`). A press on the title bar that has
  /// moved more than five pixels within the area it started in starts a
  /// move.
  pub fn motion(
    &mut self,
    layout: &FrameLayout,
    x: f64,
    y: f64,
  ) -> FrameAction {
    let previous = self.current.and_then(|(x, y)| layout.area_at(x, y));
    let area = layout.area_at(x, y);
    self.current = Some((x, y));
    if previous == area
      && self.grabbed
      && area.is_some_and(|index| moves(layout.areas[index].kind))
    {
      let (dx, dy) = (x - self.grab_origin.0, y - self.grab_origin.1);
      if dx.hypot(dy) > MOVE_THRESHOLD {
        self.grabbed = false;
        return FrameAction::Move;
      }
    }
    FrameAction::None
  }

  /// Button `button` was pressed or released at time `now`. A left press
  /// on a border resizes at once; a release on the button it was pressed
  /// on acts; two presses on the title bar within 300 ms toggle maximize
  /// on the second release.
  pub fn button(
    &mut self,
    layout: &FrameLayout,
    pressed: bool,
    button: u32,
    now: Duration,
  ) -> FrameAction {
    let area = self.current.and_then(|(x, y)| layout.area_at(x, y));
    let kind = area.map(|index| layout.areas[index].kind);
    if pressed {
      if kind.is_some_and(moves) {
        if self.double_click_until.is_some_and(|until| now < until) {
          self.double_click_at_release = true;
        } else {
          self.double_click_until = Some(now + DOUBLE_CLICK);
        }
      }
      if let Some(FrameAreaKind::Resize(_)) = kind
        && button == BTN_LEFT
      {
        let (x, y) = self.current.unwrap_or_default();
        return FrameAction::Resize(layout.resize_edges_at(x, y));
      }
      if let Some(FrameAreaKind::Button(_)) = kind {
        self.pressed = area;
      }
      self.grabbed = true;
      self.grab_origin = self.current.unwrap_or_default();
      return FrameAction::None;
    }
    if self.double_click_at_release {
      self.double_click_at_release = false;
      self.grabbed = false;
      return if button == BTN_LEFT {
        FrameAction::ToggleMaximize
      } else {
        FrameAction::None
      };
    }
    if !self.grabbed {
      return FrameAction::None;
    }
    self.grabbed = false;
    let begin = layout.area_at(self.grab_origin.0, self.grab_origin.1);
    let Some(FrameAreaKind::Button(kind)) =
      begin.map(|index| layout.areas[index].kind)
    else {
      return FrameAction::None;
    };
    self.pressed = None;
    if begin != area || button != BTN_LEFT {
      return FrameAction::None;
    }
    match kind {
      FrameButton::Close => FrameAction::Close,
      FrameButton::Maximize => FrameAction::ToggleMaximize,
      FrameButton::Minimize => FrameAction::Minimize,
    }
  }

  /// The pointer left the frame: a press in progress is dropped.
  pub fn focus_lost(&mut self) {
    self.grabbed = false;
    self.pressed = None;
    self.current = None;
  }

  /// The button area under the pointer.
  pub fn hovered(&self, layout: &FrameLayout) -> Option<usize> {
    let (x, y) = self.current?;
    layout.area_at(x, y).filter(|index| {
      matches!(layout.areas[*index].kind, FrameAreaKind::Button(_))
    })
  }

  /// The button area being pressed.
  pub fn pressed(&self) -> Option<usize> {
    self.pressed
  }

  /// The resize edges under the pointer, for its cursor.
  pub fn cursor_edges(&self, layout: &FrameLayout) -> u32 {
    self
      .current
      .map_or(0, |(x, y)| layout.resize_edges_at(x, y))
  }
}

fn moves(kind: FrameAreaKind) -> bool {
  matches!(kind, FrameAreaKind::Move | FrameAreaKind::Title)
}

#[cfg(test)]
mod tests {
  use super::*;

  const BTN_MIDDLE: u32 = 0x112;

  fn layout(width: u32, height: u32) -> FrameLayout {
    FrameLayout::new(&FrameTheme::default(), width, height, false, true)
  }

  fn kinds(layout: &FrameLayout) -> Vec<FrameAreaKind> {
    layout.areas.iter().map(|area| area.kind).collect()
  }

  #[test]
  fn the_default_theme_has_wayfires_sizes_and_margins() {
    let theme = FrameTheme::default();
    // 70 % of the 30-pixel title bar, as Wayfire's comment says.
    assert_eq!(theme.button_size, 21);
    assert_eq!(
      theme.margins(),
      Margins {
        left: 4,
        right: 4,
        top: 34,
        bottom: 4
      }
    );
    assert_eq!(
      theme.button_order,
      [
        FrameButton::Minimize,
        FrameButton::Maximize,
        FrameButton::Close
      ]
    );
  }

  #[test]
  fn margins_shrink_and_expand_a_rectangle() {
    let margins = FrameTheme::default().margins();
    let outer = Rect::new(10, 20, 400, 300);
    let inner = margins.shrink(outer);
    assert_eq!(inner, Rect::new(14, 54, 392, 262));
    assert_eq!(margins.expand(inner), outer);
    assert_eq!(Margins::ZERO.shrink(outer), outer);
    // A frame larger than the window leaves the client one pixel.
    assert_eq!(margins.shrink(Rect::new(0, 0, 5, 5)).width, 1);
  }

  #[test]
  fn buttons_title_and_borders_are_where_wayfire_puts_them() {
    let layout = layout(400, 300);
    assert_eq!(
      kinds(&layout),
      [
        FrameAreaKind::Button(FrameButton::Close),
        FrameAreaKind::Button(FrameButton::Maximize),
        FrameAreaKind::Button(FrameButton::Minimize),
        FrameAreaKind::Move,
        FrameAreaKind::Title,
        FrameAreaKind::Resize(EDGE_LEFT),
        FrameAreaKind::Resize(EDGE_RIGHT),
        FrameAreaKind::Resize(EDGE_TOP),
        FrameAreaKind::Resize(EDGE_BOTTOM),
      ]
    );
    let rects = layout
      .areas
      .iter()
      .map(|area| area.rect)
      .collect::<Vec<_>>();
    // Each button takes 21 pixels and 4 of padding on each side; the
    // rightmost ends 4 pixels short of the right border, and they are
    // centred in the title bar below the top border: (30 - 21) / 2 + 4.
    assert_eq!(rects[0], Rect::new(371, 8, 21, 21));
    assert_eq!(rects[1], Rect::new(342, 8, 21, 21));
    assert_eq!(rects[2], Rect::new(313, 8, 21, 21));
    assert_eq!(rects[3], Rect::new(313, 4, 83, 30));
    assert_eq!(rects[4], Rect::new(4, 4, 309, 30));
    assert_eq!(layout.title(), Some(rects[4]));
    assert_eq!(rects[5], Rect::new(0, 0, 4, 300));
    assert_eq!(rects[6], Rect::new(396, 0, 4, 300));
    assert_eq!(rects[7], Rect::new(0, 0, 400, 4));
    assert_eq!(rects[8], Rect::new(0, 296, 400, 4));
  }

  #[test]
  fn button_order_scale_and_padding_follow_the_options() {
    let config = DecorationConfig {
      button_order: "close bogus close maximize".into(),
      button_scale: 0.5,
      button_padding: 2,
      title_height: 20,
      border_size: 1,
      ..DecorationConfig::default()
    };
    let theme = FrameTheme::new(&config);
    let layout = FrameLayout::new(&theme, 200, 100, false, true);
    let buttons = layout
      .areas
      .iter()
      .filter_map(|area| match area.kind {
        FrameAreaKind::Button(button) => Some((button, area.rect)),
        _ => None,
      })
      .collect::<Vec<_>>();
    // Listed right to left; per button 10 + 2 * 2 pixels.
    assert_eq!(
      buttons,
      [
        (FrameButton::Maximize, Rect::new(187, 6, 10, 10)),
        (FrameButton::Close, Rect::new(173, 6, 10, 10)),
        (FrameButton::Close, Rect::new(159, 6, 10, 10)),
      ]
    );
    assert_eq!(layout.title(), Some(Rect::new(1, 1, 158, 20)));
  }

  #[test]
  fn dialogs_have_no_minimize_and_a_zero_title_bar_only_borders() {
    let theme = FrameTheme::default();
    let dialog = FrameLayout::new(&theme, 400, 300, true, true);
    assert!(
      !kinds(&dialog).contains(&FrameAreaKind::Button(FrameButton::Minimize))
    );
    assert_eq!(dialog.title(), Some(Rect::new(4, 4, 338, 30)));
    let flat = FrameTheme::new(&DecorationConfig {
      title_height: 0,
      ..DecorationConfig::default()
    });
    let layout = FrameLayout::new(&flat, 100, 100, false, true);
    assert_eq!(layout.areas.len(), 4);
    assert_eq!(layout.title(), None);
    assert_eq!(flat.margins().top, 4);
    // No buttons: the title runs up to the right border plus a padding.
    let empty = FrameTheme::new(&DecorationConfig {
      button_order: String::new(),
      ..DecorationConfig::default()
    });
    let layout = FrameLayout::new(&empty, 100, 100, false, true);
    assert_eq!(layout.title(), Some(Rect::new(4, 4, 96, 30)));
  }

  #[test]
  fn alignment_options_do_not_change_the_layout() {
    let centred = FrameTheme::new(&DecorationConfig {
      title_halign: "center".into(),
      title_valign: "bottom".into(),
      ..DecorationConfig::default()
    });
    assert_eq!(
      FrameLayout::new(&centred, 400, 300, false, true),
      layout(400, 300)
    );
  }

  #[test]
  fn hits_follow_the_area_order_and_corners_resize_two_edges() {
    let layout = layout(400, 300);
    let kind =
      |x, y| layout.area_at(x, y).map(|index| layout.areas[index].kind);
    assert_eq!(kind(0.0, 0.0), Some(FrameAreaKind::Resize(EDGE_LEFT)));
    assert_eq!(layout.resize_edges_at(0.0, 0.0), EDGE_LEFT | EDGE_TOP);
    assert_eq!(
      layout.resize_edges_at(399.0, 299.0),
      EDGE_RIGHT | EDGE_BOTTOM
    );
    assert_eq!(kind(200.0, 299.5), Some(FrameAreaKind::Resize(EDGE_BOTTOM)));
    assert_eq!(kind(100.0, 20.0), Some(FrameAreaKind::Title));
    assert_eq!(
      kind(381.0, 18.0),
      Some(FrameAreaKind::Button(FrameButton::Close))
    );
    // Between and around the buttons is the title bar.
    assert_eq!(kind(338.0, 18.0), Some(FrameAreaKind::Move));
    assert_eq!(kind(381.0, 31.0), Some(FrameAreaKind::Move));
    // The client's surface is not on the frame.
    assert!(!layout.contains(200.0, 150.0));
    assert!(layout.contains(200.0, 33.9));
    assert!(!layout.contains(200.0, 34.0));
    assert!(!layout.contains(-1.0, 10.0));
  }

  #[test]
  fn borders_of_a_tiled_window_take_input_but_do_not_resize() {
    let theme = FrameTheme::default();
    let tiled = FrameLayout::new(&theme, 400, 300, false, false);
    assert_eq!(
      tiled
        .area_at(0.0, 150.0)
        .map(|index| tiled.areas[index].kind),
      Some(FrameAreaKind::Empty)
    );
    assert_eq!(tiled.resize_edges_at(0.0, 0.0), 0);
    let mut input = FrameInput::default();
    input.motion(&tiled, 0.0, 150.0);
    assert_eq!(
      input.button(&tiled, true, BTN_LEFT, Duration::ZERO),
      FrameAction::None
    );
  }

  fn at(input: &mut FrameInput, layout: &FrameLayout, x: f64, y: f64) {
    assert_eq!(input.motion(layout, x, y), FrameAction::None);
  }

  fn click(
    input: &mut FrameInput,
    layout: &FrameLayout,
    button: u32,
    millis: u64,
  ) -> (FrameAction, FrameAction) {
    let now = Duration::from_millis(millis);
    let press = input.button(layout, true, button, now);
    (press, input.button(layout, false, button, now))
  }

  #[test]
  fn a_release_on_the_pressed_button_acts() {
    let layout = layout(400, 300);
    let mut input = FrameInput::default();
    for (x, action) in [
      (381.0, FrameAction::Close),
      (352.0, FrameAction::ToggleMaximize),
      (323.0, FrameAction::Minimize),
    ] {
      at(&mut input, &layout, x, 18.0);
      assert_eq!(input.hovered(&layout), layout.area_at(x, 18.0));
      assert_eq!(
        click(&mut input, &layout, BTN_LEFT, 0),
        (FrameAction::None, action)
      );
    }
    // Other mouse buttons do nothing.
    at(&mut input, &layout, 381.0, 18.0);
    assert_eq!(
      click(&mut input, &layout, BTN_MIDDLE, 1000),
      (FrameAction::None, FrameAction::None)
    );
  }

  #[test]
  fn a_press_shows_its_button_until_the_release_elsewhere() {
    let layout = layout(400, 300);
    let mut input = FrameInput::default();
    at(&mut input, &layout, 381.0, 18.0);
    input.button(&layout, true, BTN_LEFT, Duration::ZERO);
    assert_eq!(input.pressed(), Some(0));
    at(&mut input, &layout, 352.0, 18.0);
    assert_eq!(input.hovered(&layout), Some(1));
    assert_eq!(
      input.button(&layout, false, BTN_LEFT, Duration::ZERO),
      FrameAction::None
    );
    assert_eq!(input.pressed(), None);
    // Leaving the frame drops a press.
    input.button(&layout, true, BTN_LEFT, Duration::from_secs(1));
    input.focus_lost();
    assert_eq!(input.pressed(), None);
    assert_eq!(input.hovered(&layout), None);
  }

  #[test]
  fn dragging_the_title_bar_moves_after_five_pixels() {
    let layout = layout(400, 300);
    let mut input = FrameInput::default();
    at(&mut input, &layout, 100.0, 20.0);
    input.button(&layout, true, BTN_LEFT, Duration::ZERO);
    at(&mut input, &layout, 103.0, 24.0);
    assert_eq!(input.motion(&layout, 104.0, 24.0), FrameAction::Move);
    // The move took the press: neither more motion nor the release acts.
    at(&mut input, &layout, 150.0, 24.0);
    assert_eq!(
      input.button(&layout, false, BTN_LEFT, Duration::ZERO),
      FrameAction::None
    );
    // A press that wanders into another area first does not move.
    at(&mut input, &layout, 300.0, 20.0);
    input.button(&layout, true, BTN_LEFT, Duration::from_secs(1));
    at(&mut input, &layout, 330.0, 20.0);
    at(&mut input, &layout, 330.0, 26.0);
    // Pressing a button and dragging off it does not move either.
    let mut input = FrameInput::default();
    at(&mut input, &layout, 381.0, 18.0);
    input.button(&layout, true, BTN_LEFT, Duration::ZERO);
    at(&mut input, &layout, 381.0, 28.0);
  }

  #[test]
  fn a_double_click_on_the_title_bar_toggles_maximize() {
    let layout = layout(400, 300);
    let mut input = FrameInput::default();
    at(&mut input, &layout, 100.0, 20.0);
    assert_eq!(
      click(&mut input, &layout, BTN_LEFT, 1000),
      (FrameAction::None, FrameAction::None)
    );
    assert_eq!(
      click(&mut input, &layout, BTN_LEFT, 1299),
      (FrameAction::None, FrameAction::ToggleMaximize)
    );
    // Too slow is two single clicks.
    let mut input = FrameInput::default();
    at(&mut input, &layout, 100.0, 20.0);
    click(&mut input, &layout, BTN_LEFT, 1000);
    assert_eq!(
      click(&mut input, &layout, BTN_LEFT, 1300),
      (FrameAction::None, FrameAction::None)
    );
  }

  #[test]
  fn a_left_press_on_a_border_resizes_its_edges() {
    let layout = layout(400, 300);
    let mut input = FrameInput::default();
    at(&mut input, &layout, 399.0, 299.0);
    assert_eq!(input.cursor_edges(&layout), EDGE_RIGHT | EDGE_BOTTOM);
    assert_eq!(
      input.button(&layout, true, BTN_LEFT, Duration::ZERO),
      FrameAction::Resize(EDGE_RIGHT | EDGE_BOTTOM)
    );
    at(&mut input, &layout, 200.0, 1.0);
    assert_eq!(
      input.button(&layout, true, BTN_MIDDLE, Duration::ZERO),
      FrameAction::None
    );
  }
}
