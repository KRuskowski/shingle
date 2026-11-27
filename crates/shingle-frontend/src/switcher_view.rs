//! Wayfire's switcher view, ported from
//! wayfire:plugins/single_plugins/switcher.cpp at the commit
//! docs/design/wayfire-gaps.md pins: the views of the current workspace
//! as thumbnails in three slots, the selected one in the middle and the
//! others to its left and right, smaller, further back and turned towards
//! it, all moving over `[switcher] speed`.
//!
//! This module is the arithmetic, free of the compositor: the frontend
//! (crates/shingle-frontend/src/frontend/switcher.rs) tells it the views
//! with [`Views`], feeds it the key presses and draws what
//! [`SwitcherView::thumbnails`] returns, each thumbnail through
//! [`projection`], as Wayfire's `view_3d_transformer_t` draws it.

use std::{
  cmp::Ordering,
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use shingle_core::{Rect, WindowId};

/// How far the background and bottom layers are dimmed while the switcher
/// is up (`background_dim_factor`).
pub(crate) const BACKGROUND_DIM_FACTOR: f64 = 0.6;
/// The scale of a view one slot away from the middle (`get_back_scale`).
const BACK_SCALE: f64 = 0.66;
/// How far back a view one slot away from the middle is (`get_z_offset`).
const Z_OFFSET: f64 = -1.0;
/// The share of the output's width and height a thumbnail may take
/// (`calculate_scaling_factor`).
const SCREEN_PERCENTAGE: f64 = 0.45;

/// The slots, as Wayfire's `SwitcherViewPosition`. A thumbnail moved past
/// either side has expired and fades out where it went.
const LEFT: i32 = 0;
const CENTER: i32 = 1;
const RIGHT: i32 = 2;

/// Wayfire's `view_expired`.
fn expired(position: i32) -> bool {
  !(LEFT..=RIGHT).contains(&position)
}

/// The way the selection moves, as Wayfire's key callbacks pass it:
/// `next_view` moves it with `-1`, `prev_view` with `1`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Direction {
  /// `[switcher] next_view`: the next view in focus order.
  Next,
  /// `[switcher] prev_view`: the previous one.
  Previous,
}

impl Direction {
  fn sign(self) -> i32 {
    match self {
      Direction::Next => -1,
      Direction::Previous => 1,
    }
  }
}

/// What the switcher is told about its output and the views of its
/// current workspace whenever it arranges them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Views {
  /// The views, most recently focused first, as Wayfire sorts them by
  /// `get_focus_timestamp`.
  pub(crate) windows: Vec<WindowId>,
  /// Each view's bounding box, its frame, subsurfaces and popups
  /// included, relative to the output, in logical pixels. Views without
  /// one are left out.
  pub(crate) bounds: HashMap<WindowId, Rect>,
  /// The views that are minimized.
  pub(crate) minimized: HashSet<WindowId>,
  /// The output's size in logical pixels.
  pub(crate) output: (f64, f64),
}

/// One `timed_transition_t`: a value from `start` to `end` as its view's
/// timer runs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Transition {
  start: f64,
  end: f64,
}

impl Transition {
  fn new(start: f64, end: f64) -> Self {
    Self { start, end }
  }

  /// The value at `progress` (0 to 1) of the timer.
  fn value(self, progress: f64) -> f64 {
    (1.0 - progress) * self.start + progress * self.end
  }

  /// `restart_with_end`: from the value at `progress` to `end`.
  fn restart_with_end(&mut self, progress: f64, end: f64) {
    self.start = self.value(progress);
    self.end = end;
  }

  /// `restart_same_end`: from the value at `progress` to the same end.
  fn restart_same_end(&mut self, progress: f64) {
    self.start = self.value(progress);
  }
}

/// Linear progress of a timer started at `started` (Wayfire's
/// `duration_t`): 1 for a timer never started, as Wayfire's is ready from
/// the start, and for a zero `duration`.
fn progress(started: Option<Instant>, now: Instant, duration: Duration) -> f64 {
  let Some(started) = started else {
    return 1.0;
  };
  if duration.is_zero() {
    return 1.0;
  }
  (now.saturating_duration_since(started).as_secs_f64()
    / duration.as_secs_f64())
  .min(1.0)
}

/// How a thumbnail is drawn at one moment (`SwitcherPaintAttribs`'s
/// values), as [`projection`] turns it into a transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Paint {
  /// Horizontal scale of the view.
  pub(crate) scale_x: f64,
  /// Vertical scale of the view.
  pub(crate) scale_y: f64,
  /// Horizontal offset of the view's centre, in logical pixels.
  pub(crate) off_x: f64,
  /// Vertical offset of the view's centre, in logical pixels with `y`
  /// pointing up, as Wayfire's.
  pub(crate) off_y: f64,
  /// Depth, which only orders the thumbnails: Wayfire translates by it
  /// after the projection, where it changes nothing drawn.
  pub(crate) off_z: f64,
  /// Turn around the vertical axis, in radians.
  pub(crate) rotation: f64,
  /// Opacity, 0 to 1.
  pub(crate) alpha: f64,
}

/// `SwitcherPaintAttribs`: one timer and the transitions it drives.
#[derive(Clone, Debug)]
struct Attribs {
  started: Option<Instant>,
  scale_x: Transition,
  scale_y: Transition,
  off_x: Transition,
  off_y: Transition,
  off_z: Transition,
  rotation: Transition,
  alpha: Transition,
}

impl Attribs {
  fn new() -> Self {
    Self {
      started: None,
      scale_x: Transition::new(1.0, 1.0),
      scale_y: Transition::new(1.0, 1.0),
      off_x: Transition::new(0.0, 0.0),
      off_y: Transition::new(0.0, 0.0),
      off_z: Transition::new(0.0, 0.0),
      rotation: Transition::new(0.0, 0.0),
      alpha: Transition::new(1.0, 1.0),
    }
  }

  fn transitions(&mut self) -> [&mut Transition; 7] {
    [
      &mut self.off_x,
      &mut self.off_y,
      &mut self.off_z,
      &mut self.scale_x,
      &mut self.scale_y,
      &mut self.alpha,
      &mut self.rotation,
    ]
  }

  fn paint(&self, progress: f64) -> Paint {
    Paint {
      scale_x: self.scale_x.value(progress),
      scale_y: self.scale_y.value(progress),
      off_x: self.off_x.value(progress),
      off_y: self.off_y.value(progress),
      off_z: self.off_z.value(progress),
      rotation: self.rotation.value(progress),
      alpha: self.alpha.value(progress),
    }
  }
}

/// One `SwitcherView`: a thumbnail of a view in a slot. A view can have
/// two thumbnails at once, one fading out and its copy coming in.
#[derive(Clone, Debug)]
struct Thumbnail {
  window: WindowId,
  attribs: Attribs,
  position: i32,
  /// When the thumbnail expired (`expired_timestamp`).
  expired_at: Option<Instant>,
}

impl Thumbnail {
  /// `create_switcher_view`: untransformed, in the middle slot.
  fn new(window: WindowId) -> Self {
    Self {
      window,
      attribs: Attribs::new(),
      position: CENTER,
      expired_at: None,
    }
  }
}

/// What a frame of the switcher leaves to do, from [`SwitcherView::frame`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameState {
  /// Thumbnails are moving: draw another frame.
  Running,
  /// Everything is where it goes and the switcher is still held.
  Settled,
  /// The switcher was let go and has moved everything back: it is over.
  Finished,
}

/// The state of Wayfire's `WayfireSwitcher` on one output, from the first
/// `next_view` or `prev_view` until the thumbnails are back in place.
#[derive(Clone, Debug)]
pub(crate) struct SwitcherView {
  duration: Duration,
  thumbnail_scale: f64,
  rotation_degrees: f64,
  /// If a thumbnail comes before another, it is above it in its slot.
  views: Vec<Thumbnail>,
  dim: Transition,
  dim_started: Option<Instant>,
  /// The switcher is held, rather than animating its exit.
  active: bool,
  /// The output's width, which the slots are a third of.
  output_width: f64,
}

impl SwitcherView {
  /// A switcher with `config`'s options, showing nothing yet.
  pub(crate) fn new(config: &shingle_config::SwitcherConfig) -> Self {
    Self {
      duration: Duration::from_millis(config.duration_ms),
      thumbnail_scale: config.view_thumbnail_scale,
      rotation_degrees: f64::from(config.view_thumbnail_rotation),
      views: Vec::new(),
      dim: Transition::new(1.0, 1.0),
      dim_started: None,
      active: false,
      output_width: 0.0,
    }
  }

  /// Whether the switcher is held, rather than animating its exit.
  #[cfg(test)]
  pub(crate) fn is_active(&self) -> bool {
    self.active
  }

  /// The selected view: the thumbnail in the middle, which takes the
  /// focus when the switcher is let go.
  pub(crate) fn selected(&self) -> Option<WindowId> {
    self.views.first().map(|view| view.window)
  }

  /// `handle_switch_request` once the plugin is active: arranges `views`
  /// with the selection already moved one step in `direction` when the
  /// switcher is not held (also during its exit animation), and moves
  /// the selection otherwise. Returns the view to raise, which Wayfire's
  /// `next_view` brings to the front.
  pub(crate) fn switch(
    &mut self,
    direction: Direction,
    views: &Views,
    now: Instant,
  ) -> Option<WindowId> {
    if self.active {
      self.next_view(direction.sign(), views, now)
    } else {
      self.active = true;
      self.arrange(direction.sign(), views, now);
      None
    }
  }

  /// `handle_done`: the switcher is let go and everything moves back.
  /// Returns the view to focus and raise.
  pub(crate) fn done(
    &mut self,
    views: &Views,
    now: Instant,
  ) -> Option<WindowId> {
    self.cleanup_expired(now);
    self.dearrange(views, now)
  }

  /// `handle_view_removed`: `window` is gone. While held the views are
  /// arranged again around the focused one; while animating the exit its
  /// thumbnails just go.
  pub(crate) fn view_removed(
    &mut self,
    window: WindowId,
    views: &Views,
    now: Instant,
  ) {
    if !self.views.iter().any(|view| view.window == window) {
      return;
    }
    if self.active {
      self.arrange(0, views, now);
    } else {
      self.views.retain(|view| view.window != window);
    }
  }

  /// Wayfire's `pre_hook`, after a frame drawn at `now`: expired
  /// thumbnails go once nothing moves, and a switcher let go is over
  /// then.
  pub(crate) fn frame(&mut self, now: Instant) -> FrameState {
    if self.animations_running(now) {
      return FrameState::Running;
    }
    self.cleanup_expired(now);
    if self.active {
      FrameState::Settled
    } else {
      FrameState::Finished
    }
  }

  /// Whether any thumbnail is still moving at `now`.
  pub(crate) fn animations_running(&self, now: Instant) -> bool {
    self
      .views
      .iter()
      .any(|view| self.progress(&view.attribs, now) < 1.0)
  }

  /// The brightness of the background and bottom layers at `now`, from 1
  /// down to [`BACKGROUND_DIM_FACTOR`] while held and back after.
  pub(crate) fn background_dim(&self, now: Instant) -> f64 {
    self
      .dim
      .value(progress(self.dim_started, now, self.duration))
  }

  /// The thumbnails at `now` in the order they are drawn, bottom first,
  /// as Wayfire's `calculate_render_order`: the ones further back first,
  /// and within the same depth in the views' stacking order, `stacking`,
  /// which lists the topmost first; expired ones above, in the order they
  /// expired.
  pub(crate) fn thumbnails(
    &self,
    now: Instant,
    stacking: &[WindowId],
  ) -> Vec<(WindowId, Paint)> {
    let order = stacking
      .iter()
      .enumerate()
      .map(|(index, window)| (*window, index))
      .collect::<HashMap<_, _>>();
    let painted = self
      .views
      .iter()
      .map(|view| (view, view.attribs.paint(self.progress(&view.attribs, now))))
      .collect::<Vec<_>>();
    let comes_before = |a: &(&Thumbnail, Paint), b: &(&Thumbnail, Paint)| {
      let a_expired = expired(a.0.position);
      let b_expired = expired(b.0.position);
      if a_expired != b_expired {
        return !a_expired;
      }
      if a_expired {
        return a.0.expired_at < b.0.expired_at;
      }
      if (a.1.off_z - b.1.off_z).abs() > 0.1 {
        return a.1.off_z < b.1.off_z;
      }
      let rank = |window| order.get(window).copied().unwrap_or(0);
      rank(&a.0.window) > rank(&b.0.window)
    };
    // The comparison is not transitive, so Wayfire bubble-sorts with a
    // fixed number of passes rather than sorting.
    let mut order = painted;
    for _ in 0..2 * order.len() {
      for index in 1..order.len() {
        if comes_before(&order[index], &order[index - 1]) {
          order.swap(index, index - 1);
        }
      }
    }
    order
      .into_iter()
      .map(|(view, paint)| (view.window, paint))
      .collect()
  }

  fn progress(&self, attribs: &Attribs, now: Instant) -> f64 {
    progress(attribs.started, now, self.duration)
  }

  /// `get_center_offset`: the distance between neighbouring slots.
  fn center_offset(&self) -> f64 {
    self.output_width / 3.0
  }

  /// `move`: the thumbnail at `index` goes one slot in `dir` (-1 left, 1
  /// right), from wherever it is at `now`.
  fn move_view(&mut self, index: usize, dir: i32, now: Instant) {
    let center_offset = self.center_offset();
    let rotation = -self.rotation_degrees.to_radians();
    let progress = self.progress(&self.views[index].attribs, now);
    let view = &mut self.views[index];
    let attribs = &mut view.attribs;
    attribs.off_x.restart_with_end(
      progress,
      attribs.off_x.end + center_offset * f64::from(dir),
    );
    attribs.off_y.restart_same_end(progress);
    let z_sign = if view.position == CENTER {
      // From the middle to either side, so further back.
      1
    } else if expired(view.position + dir) {
      // Expires, and stays at its depth.
      0
    } else {
      // From a side to the middle.
      -1
    };
    attribs.off_z.restart_with_end(
      progress,
      attribs.off_z.end + Z_OFFSET * f64::from(z_sign),
    );
    let scale = BACK_SCALE.powi(z_sign);
    attribs
      .scale_x
      .restart_with_end(progress, attribs.scale_x.end * scale);
    attribs
      .scale_y
      .restart_with_end(progress, attribs.scale_y.end * scale);
    attribs.rotation.restart_with_end(
      progress,
      attribs.rotation.end + rotation * f64::from(dir),
    );
    // Wayfire tests the slot the thumbnail leaves, which never expired,
    // so every move ends at 0.7.
    let alpha = if expired(view.position) { 0.3 } else { 0.7 };
    attribs.alpha.restart_with_end(progress, alpha);
    view.position += dir;
    attribs.started = Some(now);
    if expired(view.position) {
      view.expired_at = Some(now);
    }
  }

  /// `calculate_scaling_factor`: what fits `bounds` into 45 % of the
  /// output either way, never enlarged, times `view_thumbnail_scale`.
  fn scaling_factor(&self, bounds: Rect, output: (f64, f64)) -> f64 {
    let max_width = output.0 * SCREEN_PERCENTAGE;
    let max_height = output.1 * SCREEN_PERCENTAGE;
    let needed = (max_width / f64::from(bounds.width))
      .min(max_height / f64::from(bounds.height));
    needed.min(1.0) * self.thumbnail_scale
  }

  /// `get_view_normal_alpha`: a minimized view is invisible unless it is
  /// the selected one, which the switcher restores.
  fn normal_alpha(&self, window: WindowId, views: &Views) -> f64 {
    let selected = self.views.first().map(|view| view.window);
    if views.minimized.contains(&window) && selected != Some(window) {
      0.0
    } else {
      1.0
    }
  }

  /// `arrange_center_view`: the untransformed view moves to the middle of
  /// the output, scaled to fit.
  fn arrange_center_view(&mut self, index: usize, views: &Views, now: Instant) {
    let window = self.views[index].window;
    let Some(bounds) = views.bounds.get(&window).copied() else {
      return;
    };
    let (width, height) = views.output;
    let x = f64::from(bounds.x);
    let y = f64::from(bounds.y);
    let w = f64::from(bounds.width);
    let h = f64::from(bounds.height);
    let dx = (width / 2.0 - w / 2.0) - x;
    let dy = y - (height / 2.0 - h / 2.0);
    let scale = self.scaling_factor(bounds, views.output);
    let alpha = self.normal_alpha(window, views);
    let attribs = &mut self.views[index].attribs;
    attribs.off_x = Transition::new(0.0, dx);
    attribs.off_y = Transition::new(0.0, dy);
    attribs.scale_x = Transition::new(1.0, scale);
    attribs.scale_y = Transition::new(1.0, scale);
    attribs.alpha = Transition::new(alpha, 1.0);
    attribs.started = Some(now);
  }

  /// `arrange_view`: from the view's own place to the slot `position`.
  fn arrange_view(
    &mut self,
    index: usize,
    position: i32,
    views: &Views,
    now: Instant,
  ) {
    self.arrange_center_view(index, views, now);
    if position != CENTER {
      self.move_view(index, position - CENTER, now);
    }
  }

  /// `arrange`: every view of `views` gets a thumbnail, and the one in
  /// the middle is the next in focus order for `focus_direction` -1, the
  /// last for 1 and the focused one for 0. The others go to the right
  /// in focus order, the one before the selection to the left; for 1 the
  /// one after it goes right and the others left.
  fn arrange(&mut self, focus_direction: i32, views: &Views, now: Instant) {
    self.views.clear();
    self.output_width = views.output.0;
    self.dim = Transition::new(1.0, BACKGROUND_DIM_FACTOR);
    self.dim_started = Some(now);
    let windows = views
      .windows
      .iter()
      .copied()
      .filter(|window| views.bounds.contains_key(window))
      .collect::<Vec<_>>();
    self.views = windows.iter().copied().map(Thumbnail::new).collect();
    if windows.is_empty() {
      return;
    }
    if windows.len() >= 2 {
      if focus_direction == -1 {
        let first = self.views.remove(0);
        self.views.push(Thumbnail::new(first.window));
      } else if focus_direction == 1 {
        let last = self.views.pop().expect("two views");
        self.views.insert(0, Thumbnail::new(last.window));
      }
    }
    self.arrange_view(0, CENTER, views, now);
    if windows.len() == 1 {
      return;
    }
    if windows.len() == 2 {
      // The other view in both side slots.
      self.views.push(Thumbnail::new(self.views[1].window));
    }
    let count = self.views.len();
    if focus_direction == 1 {
      self.arrange_view(1, RIGHT, views, now);
      // Wayfire leaves the left slot in focus order, so that the next
      // `prev_view` would select the view after the selection; the most
      // recently focused of them goes on top, as `next_view` expects.
      self.views[2..].reverse();
      for index in 2..count {
        self.arrange_view(index, LEFT, views, now);
      }
    } else {
      self.arrange_view(count - 1, LEFT, views, now);
      for index in 1..count - 1 {
        self.arrange_view(index, RIGHT, views, now);
      }
    }
  }

  /// `dearrange`: every thumbnail moves back to its view's place and
  /// opacity, and the background brightens again. With two views, one of
  /// the two copies of the other fades where it is instead, so that the
  /// two do not overlap on the way back. Returns the selected view, which
  /// Wayfire focuses and raises (restoring it if minimized).
  fn dearrange(&mut self, views: &Views, now: Instant) -> Option<WindowId> {
    let mut fading = if self.count_different_views() == 2 {
      self.unfocused_view()
    } else {
      None
    };
    for index in 0..self.views.len() {
      if expired(self.views[index].position) {
        continue;
      }
      let progress = self.progress(&self.views[index].attribs, now);
      let window = self.views[index].window;
      let alpha = self.normal_alpha(window, views);
      let attribs = &mut self.views[index].attribs;
      if fading == Some(window) {
        // Frozen in place.
        for transition in [
          &mut attribs.off_x,
          &mut attribs.off_y,
          &mut attribs.off_z,
          &mut attribs.scale_x,
          &mut attribs.scale_y,
          &mut attribs.rotation,
        ] {
          let value = transition.value(progress);
          *transition = Transition::new(value, value);
        }
        attribs.alpha.restart_with_end(progress, 0.0);
        attribs.started = Some(now);
        fading = None;
        continue;
      }
      attribs.off_x.restart_with_end(progress, 0.0);
      attribs.off_y.restart_with_end(progress, 0.0);
      attribs.off_z.restart_with_end(progress, 0.0);
      attribs.scale_x.restart_with_end(progress, 1.0);
      attribs.scale_y.restart_with_end(progress, 1.0);
      attribs.rotation.restart_with_end(progress, 0.0);
      attribs.alpha.restart_with_end(progress, alpha);
      attribs.started = Some(now);
    }
    let dim = progress(self.dim_started, now, self.duration);
    self.dim.restart_with_end(dim, 1.0);
    self.dim_started = Some(now);
    self.active = false;
    self.selected()
  }

  /// `cleanup_expired`: expired thumbnails that stopped moving go.
  fn cleanup_expired(&mut self, now: Instant) {
    let duration = self.duration;
    self.views.retain(|view| {
      !expired(view.position)
        || progress(view.attribs.started, now, duration) < 1.0
    });
  }

  /// `rebuild_view_list`: the selected thumbnail first, then the others
  /// left before right, then the expired ones, keeping the order within
  /// each slot.
  fn rebuild_view_list(&mut self) {
    let category = |view: &Thumbnail| {
      if view.position == CENTER {
        0
      } else if expired(view.position) {
        2
      } else {
        1
      }
    };
    self
      .views
      .sort_by(|a, b| match category(a).cmp(&category(b)) {
        Ordering::Equal => a.position.cmp(&b.position),
        other => other,
      });
  }

  /// `next_view`: the selection moves one step in `dir`. The thumbnail in
  /// the middle and the top one of the slot the next comes from move one
  /// slot; when that empties a side while the other holds two or more,
  /// the bottom one there expires and a copy of it fills the empty side.
  /// Returns the new selection, which Wayfire raises.
  fn next_view(
    &mut self,
    dir: i32,
    views: &Views,
    now: Instant,
  ) -> Option<WindowId> {
    self.cleanup_expired(now);
    if self.count_different_views() <= 1 {
      return None;
    }
    let comes_from = 1 - dir;
    let mut to_move = (1 << CENTER) | (1 << comes_from);
    let mut count_left = 0;
    let mut count_right = 0;
    for index in 0..self.views.len() {
      let position = self.views[index].position;
      if !expired(position) && (1 << position) & to_move != 0 {
        // Only the top one of its slot.
        to_move ^= 1 << position;
        self.move_view(index, dir, now);
      }
      count_left += usize::from(self.views[index].position == LEFT);
      count_right += usize::from(self.views[index].position == RIGHT);
    }
    if ((count_left > 0) != (count_right > 0)) && count_left + count_right >= 2
    {
      self.fill_empty_slot(1 - dir, views, now);
    }
    self.rebuild_view_list();
    self.selected()
  }

  /// `count_different_active_views`: how many views have thumbnails.
  fn count_different_views(&self) -> usize {
    self
      .views
      .iter()
      .map(|view| view.window)
      .collect::<HashSet<_>>()
      .len()
  }

  /// `invalidate_last_in_slot`: the bottom thumbnail of `slot` moves out
  /// past its side; returns its view.
  fn invalidate_last_in_slot(
    &mut self,
    slot: i32,
    now: Instant,
  ) -> Option<WindowId> {
    let index = self.views.iter().rposition(|view| view.position == slot)?;
    self.move_view(index, if slot == LEFT { -1 } else { 1 }, now);
    Some(self.views[index].window)
  }

  /// `get_unfocused_view`: the first thumbnail beside the middle.
  fn unfocused_view(&self) -> Option<WindowId> {
    self
      .views
      .iter()
      .find(|view| !expired(view.position) && view.position != CENTER)
      .map(|view| view.window)
  }

  /// `fill_empty_slot`: a new thumbnail for `empty_slot`, of the view
  /// that just expired from the other side, or of the other view when
  /// there are only two. It appears there at once and fades in.
  fn fill_empty_slot(&mut self, empty_slot: i32, views: &Views, now: Instant) {
    let full_slot = 2 - empty_slot;
    let mut window = self.invalidate_last_in_slot(full_slot, now);
    if self.count_different_views() == 2 {
      window = self.unfocused_view();
    }
    let Some(window) =
      window.filter(|window| views.bounds.contains_key(window))
    else {
      return;
    };
    self.views.push(Thumbnail::new(window));
    let index = self.views.len() - 1;
    self.arrange_view(index, empty_slot, views, now);
    let attribs = &mut self.views[index].attribs;
    for transition in attribs.transitions() {
      *transition = Transition::new(transition.end, transition.end);
    }
    attribs.alpha = Transition::new(0.0, 1.0);
    attribs.started = Some(now);
  }
}

/// The 4x4 transform, column-major, that draws a view whose bounding box
/// is `size` logical pixels with `paint`, around the box's centre, as
/// Wayfire's `view_3d_transformer_t::calculate_total_transform` and its
/// render instance compute it: `translation * projection * view *
/// depth_scale * rotation * scaling`, the projection a 45° perspective
/// with the eye where a point at depth 0 keeps its size, and the depth
/// scaled so that the box's larger side spans 2. The result is for
/// [`shingle_render::Projection`], in logical pixels with `y` up.
pub(crate) fn projection(paint: &Paint, size: (f64, f64)) -> [[f32; 4]; 4] {
  let fov = std::f64::consts::FRAC_PI_4;
  let eye = 1.0 / (fov / 2.0).tan();
  let depth = 2.0 / size.0.max(size.1).max(1.0);
  let scaling = diagonal([paint.scale_x, paint.scale_y, 1.0, 1.0]);
  let (sin, cos) = paint.rotation.sin_cos();
  // glm::rotate about the y axis.
  let rotation = [
    [cos, 0.0, -sin, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [sin, 0.0, cos, 0.0],
    [0.0, 0.0, 0.0, 1.0],
  ];
  let depth_scale = diagonal([1.0, 1.0, depth, 1.0]);
  // glm::lookAt from (0, 0, eye) to the origin, y up.
  let view = translation([0.0, 0.0, -eye]);
  // glm::perspective(fov, 1, 0.1, 100); its depth rows do not matter.
  let (near, far) = (0.1, 100.0);
  let perspective = [
    [eye, 0.0, 0.0, 0.0],
    [0.0, eye, 0.0, 0.0],
    [0.0, 0.0, -(far + near) / (far - near), -1.0],
    [0.0, 0.0, -2.0 * far * near / (far - near), 0.0],
  ];
  let translate = translation([paint.off_x, paint.off_y, paint.off_z]);
  let total = [perspective, view, depth_scale, rotation, scaling]
    .iter()
    .fold(translate, |product, next| multiply(&product, next));
  total.map(|column| column.map(|value| value as f32))
}

type Matrix = [[f64; 4]; 4];

fn diagonal(values: [f64; 4]) -> Matrix {
  let mut matrix = [[0.0; 4]; 4];
  for (index, value) in values.into_iter().enumerate() {
    matrix[index][index] = value;
  }
  matrix
}

fn translation(offset: [f64; 3]) -> Matrix {
  let mut matrix = diagonal([1.0; 4]);
  matrix[3][..3].copy_from_slice(&offset);
  matrix
}

/// `a * b` of column-major matrices.
fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
  let mut product = [[0.0; 4]; 4];
  for (column, out) in product.iter_mut().enumerate() {
    for (row, value) in out.iter_mut().enumerate() {
      *value = (0..4).map(|k| a[k][row] * b[column][k]).sum();
    }
  }
  product
}

#[cfg(test)]
mod tests {
  use super::*;

  const OUTPUT: (f64, f64) = (1200.0, 900.0);
  const DURATION: Duration = Duration::from_millis(500);

  fn window(id: u64) -> WindowId {
    WindowId(id)
  }

  /// `count` views of 600x400 at (0, 0), focused in the order of their
  /// ids, the first most recently.
  fn views(count: u64) -> Views {
    Views {
      windows: (0..count).map(window).collect(),
      bounds: (0..count)
        .map(|id| (window(id), Rect::new(0, 0, 600, 400)))
        .collect(),
      minimized: HashSet::new(),
      output: OUTPUT,
    }
  }

  fn switcher() -> SwitcherView {
    SwitcherView::new(&shingle_config::SwitcherConfig::default())
  }

  /// The windows a switcher selects, one per press in `directions`.
  fn selections(count: u64, directions: &[Direction]) -> Vec<u64> {
    let views = views(count);
    let mut switcher = switcher();
    let mut now = Instant::now();
    directions
      .iter()
      .map(|direction| {
        switcher.switch(*direction, &views, now);
        // Each press when the last has settled, and once more half way.
        now += DURATION;
        switcher.frame(now);
        switcher.selected().expect("a selection").0
      })
      .collect()
  }

  fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
  }

  /// The settled paint of every thumbnail of `window`, in render order.
  fn paints(switcher: &SwitcherView, now: Instant, window: u64) -> Vec<Paint> {
    switcher
      .thumbnails(now, &[])
      .into_iter()
      .filter(|(drawn, _)| drawn.0 == window)
      .map(|(_, paint)| paint)
      .collect()
  }

  #[test]
  fn next_view_selects_in_focus_order_and_wraps() {
    use Direction::Next;
    assert_eq!(selections(3, &[Next; 5]), [1, 2, 0, 1, 2]);
    assert_eq!(selections(4, &[Next; 6]), [1, 2, 3, 0, 1, 2]);
    assert_eq!(selections(2, &[Next; 3]), [1, 0, 1]);
  }

  #[test]
  fn prev_view_selects_against_focus_order_and_wraps() {
    use Direction::Previous;
    assert_eq!(selections(3, &[Previous; 4]), [2, 1, 0, 2]);
    // Wayfire's own order would go 3, 1, 2, 3, 0 here.
    assert_eq!(selections(4, &[Previous; 5]), [3, 2, 1, 0, 3]);
    assert_eq!(selections(5, &[Previous; 6]), [4, 3, 2, 1, 0, 4]);
    assert_eq!(selections(2, &[Previous; 3]), [1, 0, 1]);
  }

  #[test]
  fn the_directions_can_be_mixed() {
    use Direction::{Next, Previous};
    assert_eq!(
      selections(4, &[Next, Next, Previous, Previous, Previous, Next]),
      [1, 2, 1, 0, 3, 0]
    );
    assert_eq!(
      selections(4, &[Previous, Next, Next, Previous]),
      [3, 0, 1, 0]
    );
  }

  #[test]
  fn one_view_is_centred_and_never_moves() {
    let views = views(1);
    let mut switcher = switcher();
    let now = Instant::now();
    assert_eq!(switcher.switch(Direction::Next, &views, now), None);
    assert_eq!(switcher.switch(Direction::Next, &views, now), None);
    let paints = paints(&switcher, now + DURATION, 0);
    assert_eq!(paints.len(), 1);
    // 600x400 fits into 45 % of 1200x900 at 0.9, and moves its centre
    // from (300, 200) to the output's.
    assert!(close(paints[0].scale_x, 0.9) && close(paints[0].scale_y, 0.9));
    assert!(close(paints[0].off_x, 300.0), "{:?}", paints[0]);
    assert!(close(paints[0].off_y, -250.0), "{:?}", paints[0]);
    assert_eq!(paints[0].rotation, 0.0);
    assert_eq!(paints[0].alpha, 1.0);
  }

  #[test]
  fn the_slots_are_wayfires() {
    let views = views(3);
    let mut switcher = switcher();
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    let settled = now + DURATION;
    assert_eq!(switcher.selected(), Some(window(1)));
    let center = paints(&switcher, settled, 1)[0];
    let left = paints(&switcher, settled, 0)[0];
    let right = paints(&switcher, settled, 2)[0];
    assert!(close(center.off_x, 300.0) && close(center.off_z, 0.0));
    assert!(close(center.scale_x, 0.9) && close(center.alpha, 1.0));
    // A third of the output to either side, back by 1, at 66 % of the
    // size and 0.7 opacity, turned by 30 degrees towards the middle.
    for (side, sign) in [(left, -1.0), (right, 1.0)] {
      assert!(close(side.off_x, 300.0 + sign * 400.0), "{side:?}");
      assert!(close(side.off_y, -250.0));
      assert!(close(side.off_z, -1.0));
      assert!(close(side.scale_x, 0.9 * 0.66));
      assert!(close(side.scale_y, 0.9 * 0.66));
      assert!(close(side.rotation, -sign * 30f64.to_radians()));
      assert!(close(side.alpha, 0.7));
    }
    // Half way there, everything is half way from the view's own place.
    let half = paints(&switcher, now + DURATION / 2, 2)[0];
    assert!(close(half.off_x, 350.0), "{half:?}");
    assert!(close(half.scale_x, (1.0 + 0.9 * 0.66) / 2.0));
    // The background dims to 60 % with them.
    assert!(close(switcher.background_dim(now), 1.0));
    assert!(close(
      switcher.background_dim(settled),
      BACKGROUND_DIM_FACTOR
    ));
  }

  #[test]
  fn two_views_fill_both_sides_with_the_other_one() {
    let views = views(2);
    let mut switcher = switcher();
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    let settled = now + DURATION;
    let others = paints(&switcher, settled, 0);
    assert_eq!(others.len(), 2, "one copy on each side");
    let mut offsets =
      others.iter().map(|paint| paint.off_x).collect::<Vec<_>>();
    offsets.sort_by(f64::total_cmp);
    assert!(close(offsets[0], -100.0) && close(offsets[1], 700.0));
    // Letting go fades one copy where it is and moves the other back.
    switcher.done(&views, settled);
    let back = settled + DURATION;
    let others = paints(&switcher, back, 0);
    assert_eq!(others.len(), 2);
    assert!(
      others
        .iter()
        .any(|paint| { close(paint.alpha, 0.0) && close(paint.off_z, -1.0) })
    );
    assert!(
      others
        .iter()
        .any(|paint| { close(paint.alpha, 1.0) && close(paint.off_x, 0.0) })
    );
  }

  #[test]
  fn a_view_moved_past_a_side_expires_and_its_copy_fills_the_other() {
    let views = views(3);
    let mut switcher = switcher();
    let mut now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    now += DURATION;
    // 1 goes left to 0, 2 to the middle: the right side is empty, so 0
    // leaves to the left and appears on the right at once, fading in.
    switcher.switch(Direction::Next, &views, now);
    let zero = paints(&switcher, now, 0);
    assert_eq!(zero.len(), 2);
    let copy = zero
      .iter()
      .find(|paint| close(paint.off_x, 700.0))
      .expect("a copy on the right");
    assert!(close(copy.alpha, 0.0) && close(copy.off_z, -1.0));
    let half = now + DURATION / 2;
    assert_eq!(paints(&switcher, half, 0).len(), 2);
    // The expired one fades to 0.7 out past the left side (Wayfire tests
    // the slot it leaves), drawn above the others, and goes once still.
    let order = switcher.thumbnails(half, &[]);
    let (last, paint) = order.last().expect("thumbnails");
    assert_eq!(*last, window(0));
    assert!(paint.off_x < -100.0, "{paint:?}");
    now += DURATION;
    assert_eq!(switcher.frame(now), FrameState::Settled);
    assert_eq!(paints(&switcher, now, 0).len(), 1);
    assert!(close(paints(&switcher, now, 0)[0].alpha, 1.0));
  }

  #[test]
  fn the_middle_is_drawn_last_and_equal_depths_in_stacking_order() {
    let views = views(4);
    let mut switcher = switcher();
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    let settled = now + DURATION;
    // 2 and 3 share the right slot: the lower in the stack goes first.
    let order = |stacking: &[WindowId]| {
      switcher
        .thumbnails(settled, stacking)
        .into_iter()
        .map(|(window, _)| window.0)
        .collect::<Vec<_>>()
    };
    let topmost_first = [3, 2, 1, 0].map(window);
    let drawn = order(&topmost_first);
    assert_eq!(drawn.last(), Some(&1), "{drawn:?}");
    let position = |id| drawn.iter().position(|drawn| *drawn == id);
    assert!(position(2) < position(3), "{drawn:?}");
    let drawn = order(&[2, 3, 1, 0].map(window));
    let position = |id| drawn.iter().position(|drawn| *drawn == id);
    assert!(position(3) < position(2), "{drawn:?}");
  }

  #[test]
  fn letting_go_moves_everything_back_and_then_finishes() {
    let views = views(3);
    let mut switcher = switcher();
    let mut now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    now += DURATION / 2;
    assert_eq!(switcher.frame(now), FrameState::Running);
    assert_eq!(switcher.done(&views, now), Some(window(1)));
    assert!(!switcher.is_active());
    assert_eq!(switcher.frame(now), FrameState::Running);
    let dim = switcher.background_dim(now);
    assert!(dim < 1.0 && dim > BACKGROUND_DIM_FACTOR, "{dim}");
    now += DURATION;
    for (_, paint) in switcher.thumbnails(now, &[]) {
      assert!(close(paint.off_x, 0.0) && close(paint.off_y, 0.0));
      assert!(close(paint.scale_x, 1.0) && close(paint.rotation, 0.0));
      assert!(close(paint.alpha, 1.0));
    }
    assert!(close(switcher.background_dim(now), 1.0));
    assert_eq!(switcher.frame(now), FrameState::Finished);
  }

  #[test]
  fn pressing_again_during_the_exit_arranges_anew() {
    let views = views(3);
    let mut switcher = switcher();
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    switcher.done(&views, now + DURATION);
    switcher.switch(Direction::Next, &views, now + DURATION * 3 / 2);
    assert!(switcher.is_active());
    assert_eq!(switcher.selected(), Some(window(1)));
  }

  #[test]
  fn a_removed_view_rearranges_around_the_focused_one_while_held() {
    let mut views = views(4);
    let mut switcher = switcher();
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    switcher.switch(Direction::Next, &views, now + DURATION);
    assert_eq!(switcher.selected(), Some(window(2)));
    views.windows.retain(|window| window.0 != 2);
    switcher.view_removed(window(2), &views, now + DURATION * 2);
    assert_eq!(switcher.selected(), Some(window(0)), "the focused one");
    assert!(paints(&switcher, now + DURATION * 3, 2).is_empty());
    // A view without thumbnails changes nothing.
    switcher.view_removed(window(9), &views, now + DURATION * 3);
    assert_eq!(switcher.thumbnails(now, &[]).len(), 3);
    // During the exit the view's thumbnails just go.
    switcher.done(&views, now + DURATION * 3);
    views.windows.retain(|window| window.0 != 1);
    switcher.view_removed(window(1), &views, now + DURATION * 3);
    assert!(!switcher.is_active());
    assert!(paints(&switcher, now + DURATION * 3, 1).is_empty());
    assert_eq!(switcher.thumbnails(now, &[]).len(), 2);
  }

  #[test]
  fn a_minimized_view_shows_only_while_switching_unless_selected() {
    let mut views = views(3);
    views.minimized.insert(window(2));
    let mut switcher = switcher();
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    // It fades in from nothing as it moves to its slot.
    assert!(close(paints(&switcher, now, 2)[0].alpha, 0.0));
    assert!(close(paints(&switcher, now + DURATION, 2)[0].alpha, 0.7));
    switcher.done(&views, now + DURATION);
    assert!(close(
      paints(&switcher, now + DURATION * 2, 2)[0].alpha,
      0.0
    ));
    // Selected, it stays.
    switcher.switch(Direction::Next, &views, now + DURATION * 3);
    switcher.switch(Direction::Next, &views, now + DURATION * 4);
    assert_eq!(switcher.selected(), Some(window(2)));
    switcher.done(&views, now + DURATION * 5);
    let paints = paints(&switcher, now + DURATION * 6, 2);
    assert!(paints.iter().any(|paint| close(paint.alpha, 1.0)));
  }

  #[test]
  fn the_options_scale_turn_and_time_the_thumbnails() {
    let views = views(3);
    let mut switcher = SwitcherView::new(&shingle_config::SwitcherConfig {
      duration_ms: 0,
      view_thumbnail_scale: 0.5,
      view_thumbnail_rotation: 60,
      ..Default::default()
    });
    let now = Instant::now();
    switcher.switch(Direction::Next, &views, now);
    // A zero duration ends every move at once.
    assert_eq!(switcher.frame(now), FrameState::Settled);
    let center = paints(&switcher, now, 1)[0];
    assert!(close(center.scale_x, 0.45));
    let right = paints(&switcher, now, 2)[0];
    assert!(close(right.rotation, -60f64.to_radians()));
    assert!(close(right.scale_x, 0.45 * 0.66));
  }

  #[test]
  fn views_without_bounds_are_left_out() {
    let mut views = views(3);
    views.bounds.remove(&window(1));
    let mut switcher = switcher();
    switcher.switch(Direction::Next, &views, Instant::now());
    assert_eq!(switcher.selected(), Some(window(2)));
    let empty = Views {
      output: OUTPUT,
      ..Views::default()
    };
    let mut switcher = self::switcher();
    switcher.switch(Direction::Next, &empty, Instant::now());
    assert_eq!(switcher.selected(), None);
    assert_eq!(switcher.done(&empty, Instant::now()), None);
  }

  /// Where `projection` draws the point (`x`, `y`) of a box of `size`
  /// centred at the origin, `y` up.
  fn project(paint: &Paint, size: (f64, f64), x: f64, y: f64) -> (f64, f64) {
    let matrix = projection(paint, size);
    let point = [x, y, 0.0, 1.0];
    let row = |index: usize| {
      (0..4)
        .map(|column| f64::from(matrix[column][index]) * point[column])
        .sum::<f64>()
    };
    (row(0) / row(3), row(1) / row(3))
  }

  fn flat() -> Paint {
    Paint {
      scale_x: 1.0,
      scale_y: 1.0,
      off_x: 0.0,
      off_y: 0.0,
      off_z: 0.0,
      rotation: 0.0,
      alpha: 1.0,
    }
  }

  #[test]
  fn the_projection_keeps_an_untransformed_view_in_place() {
    let (x, y) = project(&flat(), (600.0, 400.0), 300.0, -200.0);
    assert!((x - 300.0).abs() < 1e-3 && (y + 200.0).abs() < 1e-3);
    let moved = Paint {
      scale_x: 0.5,
      scale_y: 0.25,
      off_x: 10.0,
      off_y: -20.0,
      off_z: -1.0,
      ..flat()
    };
    let (x, y) = project(&moved, (600.0, 400.0), 300.0, 200.0);
    // Depth changes nothing drawn.
    assert!(
      (x - 160.0).abs() < 1e-3 && (y - 30.0).abs() < 1e-3,
      "{x} {y}"
    );
  }

  #[test]
  fn a_turned_view_recedes_on_the_side_it_turns_away() {
    // Turned as the left slot is, its right edge goes back and shrinks.
    let turned = Paint {
      rotation: 30f64.to_radians(),
      ..flat()
    };
    let size = (600.0, 400.0);
    let (left_x, left_y) = project(&turned, size, -300.0, 200.0);
    let (right_x, right_y) = project(&turned, size, 300.0, 200.0);
    assert!(right_y < 200.0 && left_y > 200.0, "{left_y} {right_y}");
    assert!(right_x < 300.0 * 30f64.to_radians().cos());
    assert!(-left_x > 300.0 * 30f64.to_radians().cos());
    // Wayfire's numbers: depth 2/600 per pixel, the eye at 1/tan(22.5°).
    let eye = 1.0 / (std::f64::consts::FRAC_PI_8).tan();
    let w = 1.0 + 30f64.to_radians().sin() * 300.0 * 2.0 / 600.0 / eye;
    assert!((right_y - 200.0 / w).abs() < 1e-3, "{right_y}");
  }
}
