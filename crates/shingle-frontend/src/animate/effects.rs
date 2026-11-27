//! The effects of Wayfire's animate plugin, as what each does to a view
//! at a point of its animation: a 2D transform for fade, zoom, zap and
//! spin, the warp of squeezimize and the burning line and particles of
//! fire.

use std::time::Instant;

use shingle_core::Rect;
use shingle_render::{RenderRect, Squeeze};

use super::{
  Clock, Timing, Transition,
  fire::{self, FireOptions, ParticleSystem, Random},
};

/// One of the animations of Wayfire's animate
/// (wayfire:plugins/animate/animate.cpp:227-235).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Effect {
  /// `fade` (wayfire:plugins/animate/basic_animations.hpp:8-56).
  Fade,
  /// `zoom` (wayfire:plugins/animate/basic_animations.hpp:70-156).
  Zoom,
  /// `fire` (wayfire:plugins/animate/fire/fire.cpp).
  Fire,
  /// `zap` (wayfire:plugins/animate/zap.hpp).
  Zap,
  /// `spin` (wayfire:plugins/animate/spin.hpp).
  Spin,
  /// `squeezimize` (wayfire:plugins/animate/squeezimize.hpp).
  Squeezimize,
}

impl Effect {
  /// The effect named `name`; `None` for `none` and unknown names, which
  /// Wayfire does not animate (wayfire:plugins/animate/animate.cpp:369-378).
  pub fn from_name(name: &str) -> Option<Self> {
    Some(match name {
      "fade" => Self::Fade,
      "zoom" => Self::Zoom,
      "fire" => Self::Fire,
      "zap" => Self::Zap,
      "spin" => Self::Spin,
      "squeezimize" => Self::Squeezimize,
      _ => return None,
    })
  }
}

/// Why a view animates (wayfire:plugins/animate/animate.hpp:17-23).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
  /// It mapped.
  Map,
  /// It unmapped.
  Unmap,
  /// It was minimized.
  Minimize,
  /// It was restored from being minimized.
  Restore,
}

impl Kind {
  /// Whether the view goes away: `WF_ANIMATE_HIDING_ANIMATION`.
  pub fn hiding(self) -> bool {
    matches!(self, Self::Unmap | Self::Minimize)
  }

  /// Whether minimizing or restoring: `WF_ANIMATE_MINIMIZE_STATE_ANIMATION`.
  pub fn minimize_state(self) -> bool {
    matches!(self, Self::Minimize | Self::Restore)
  }
}

/// Wayfire's `view_2d_transformer_t`: opacity, a scale about the view's
/// centre, a turn and a translation (wayfire:src/view/view-3d.cpp:211-223).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTransform {
  /// Opacity, 0 to 1.
  pub alpha: f64,
  /// Horizontal scale.
  pub scale_x: f64,
  /// Vertical scale.
  pub scale_y: f64,
  /// Translation in logical pixels.
  pub translation: (f64, f64),
  /// The turn in radians, counterclockwise on screen.
  pub angle: f64,
}

impl ViewTransform {
  /// No change.
  pub const IDENTITY: Self = Self {
    alpha: 1.0,
    scale_x: 1.0,
    scale_y: 1.0,
    translation: (0.0, 0.0),
    angle: 0.0,
  };

  /// Where `rect` goes when the view's centre is `center`: scaled about
  /// it, then moved by the translation. The turn is left to the tile.
  pub fn apply(&self, rect: RenderRect, center: (f64, f64)) -> RenderRect {
    let left = center.0
      + (f64::from(rect.x) - center.0) * self.scale_x
      + self.translation.0;
    let top = center.1
      + (f64::from(rect.y) - center.1) * self.scale_y
      + self.translation.1;
    let width = f64::from(rect.width) * self.scale_x;
    let height = f64::from(rect.height) * self.scale_y;
    let right = (left + width).round();
    let bottom = (top + height).round();
    RenderRect {
      x: left.round() as i32,
      y: top.round() as i32,
      width: (right - left.round()).max(0.0) as u32,
      height: (bottom - top.round()).max(0.0) as u32,
    }
  }
}

/// What a view's animation needs to know of the view when it starts.
#[derive(Clone, Copy, Debug)]
pub struct ViewInfo {
  /// The view's geometry, in layout coordinates.
  pub geometry: Rect,
  /// The rectangle of its output.
  pub output: Rect,
  /// Its minimize hint, if a taskbar set one.
  pub minimize_hint: Option<Rect>,
  /// `spin_rotations`.
  pub spin_rotations: i32,
  /// The fire options.
  pub fire: FireOptions,
  /// Seeds the fire's particles.
  pub seed: u64,
}

/// The state of each effect besides its clock.
#[derive(Clone, Debug)]
enum State {
  Fade {
    alpha: Transition,
  },
  Zoom {
    alpha: Transition,
    zoom: Transition,
    offset_x: Transition,
    offset_y: Transition,
  },
  Zap,
  Spin {
    rotations: i32,
  },
  Squeezimize {
    target: Rect,
    upward: bool,
    horizontal: bool,
  },
  Fire(Box<Fire>),
}

/// Fire's line and particles (wayfire:plugins/animate/fire/fire.cpp).
#[derive(Clone, Debug)]
struct Fire {
  line: Transition,
  progress_line: f64,
  system: ParticleSystem,
  random: Random,
  options: FireOptions,
  /// The view's size at the last step.
  size: (f64, f64),
  /// The live particles as the last step left them, for drawing.
  drawn: Vec<shingle_render::Particle>,
}

/// One view's animation: Wayfire's `animation_hook` with its effect.
#[derive(Clone, Debug)]
pub struct Animation {
  /// The effect.
  pub effect: Effect,
  /// Why it runs; a change of direction reverses it.
  pub kind: Kind,
  clock: Clock,
  state: State,
}

impl Animation {
  /// Starts `effect` for `kind` at `now`, as the effect's `init` sets
  /// itself up for `view`.
  pub fn start(
    effect: Effect,
    kind: Kind,
    timing: Timing,
    now: Instant,
    view: &ViewInfo,
  ) -> Self {
    let hiding = kind.hiding();
    let mut timing = timing;
    let (state, reversed) = match effect {
      Effect::Fade => {
        let mut alpha = Transition::new(0.0, 1.0);
        if hiding {
          alpha.flip();
        }
        (State::Fade { alpha }, false)
      }
      Effect::Zoom => (zoom_state(kind, view), false),
      Effect::Zap => (State::Zap, hiding),
      Effect::Spin => (
        State::Spin {
          rotations: view.spin_rotations,
        },
        hiding,
      ),
      Effect::Squeezimize => {
        let (target, upward, horizontal) = squeeze_target(view);
        (
          State::Squeezimize {
            target,
            upward,
            horizontal,
          },
          // init_animation(squeeze) reverses it for a showing animation.
          !hiding,
        )
      }
      Effect::Fire => {
        // The length grows with the view's height, in whole
        // milliseconds as Wayfire's int.
        let factor =
          fire::duration_factor_for_height(f64::from(view.geometry.height));
        let ms = (timing.duration.as_millis() as f64 * factor) as u64;
        timing.duration = std::time::Duration::from_millis(ms);
        let mut line = Transition::new(0.0, 1.0);
        if hiding {
          line.flip();
        }
        (
          State::Fire(Box::new(Fire {
            line,
            progress_line: line.start,
            system: ParticleSystem::new(1),
            random: Random::new(view.seed),
            options: view.fire,
            size: (
              f64::from(view.geometry.width),
              f64::from(view.geometry.height),
            ),
            drawn: Vec::new(),
          })),
          false,
        )
      }
    };
    // Reversed before it starts, the clock runs from the end
    // (`reverse()` then `start()`).
    let mut clock = Clock::start(now, timing);
    clock.reversed = reversed;
    Self {
      effect,
      kind,
      clock,
      state,
    }
  }

  /// Changes why the animation runs, reversing it when the view now goes
  /// the other way (`set_animation_type`,
  /// wayfire:plugins/animate/animate.cpp:144-161).
  pub fn set_kind(&mut self, kind: Kind, now: Instant) {
    if self.kind.hiding() != kind.hiding() {
      self.clock.reverse(now);
    }
    self.kind = kind;
  }

  /// Whether it still runs at `now`: until its clock ends, and for fire
  /// until the last particle died.
  pub fn running(&self, now: Instant) -> bool {
    !self.clock.ready(now)
      || matches!(&self.state, State::Fire(fire) if fire.system.alive() > 0)
  }

  /// When the clock ends.
  pub fn clock_ready(&self, now: Instant) -> bool {
    self.clock.ready(now)
  }

  /// The 2D transform of fade, zoom, zap and spin at `now`; the identity
  /// for the others.
  pub fn transform(&self, now: Instant) -> ViewTransform {
    let progress = self.clock.progress(now);
    match &self.state {
      State::Fade { alpha } => ViewTransform {
        alpha: alpha.at(progress),
        ..ViewTransform::IDENTITY
      },
      State::Zoom {
        alpha,
        zoom,
        offset_x,
        offset_y,
      } => {
        let scale = zoom.at(progress);
        ViewTransform {
          alpha: alpha.at(progress).min(1.0),
          scale_x: scale,
          scale_y: scale,
          translation: (offset_x.at(progress), offset_y.at(progress)),
          angle: 0.0,
        }
      }
      State::Zap => {
        // wayfire:plugins/animate/zap.hpp:69-82.
        let third = 1.0 / 3.0;
        let one = progress.clamp(0.0, third) * 3.0;
        let two = (progress.clamp(third, 2.0 * third) - third) * 3.0;
        let three = (progress.clamp(2.0 * third, 1.0) - 2.0 * third) * 3.0;
        ViewTransform {
          alpha: one,
          scale_x: 0.01 + two * 0.99,
          scale_y: 0.01 + three * 0.99,
          ..ViewTransform::IDENTITY
        }
      }
      State::Spin { rotations } => ViewTransform {
        // wayfire:plugins/animate/spin.hpp:167-178.
        alpha: progress,
        scale_x: 0.01 + progress * 0.99,
        scale_y: 0.01 + progress * 0.99,
        translation: (0.0, 0.0),
        angle: progress * std::f64::consts::PI * 2.0 * f64::from(*rotations),
      },
      State::Squeezimize { .. } | State::Fire(_) => ViewTransform::IDENTITY,
    }
  }

  /// The warp of squeezimize at `now` for a view at `view`: the area the
  /// tile covers and its shader's uniforms
  /// (wayfire:plugins/animate/squeezimize.hpp:342-397).
  pub fn squeeze(
    &self,
    now: Instant,
    view: Rect,
  ) -> Option<(RenderRect, Squeeze)> {
    let State::Squeezimize {
      target,
      upward,
      horizontal,
    } = &self.state
    else {
      return None;
    };
    let area = squeeze_area(view, *target);
    if area.width == 0 || area.height == 0 {
      return None;
    }
    let fraction = |rect: Rect| {
      let width = f64::from(area.width);
      let height = f64::from(area.height);
      let left = f64::from(rect.x - area.x);
      let top = f64::from(rect.y - area.y);
      [
        (left / width) as f32,
        (top / height) as f32,
        ((left + f64::from(rect.width)) / width) as f32,
        ((top + f64::from(rect.height)) / height) as f32,
      ]
    };
    Some((
      RenderRect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
      },
      Squeeze {
        progress: self.clock.progress(now) as f32,
        source: fraction(view),
        target: fraction(*target),
        upward: *upward,
        horizontal: *horizontal,
      },
    ))
  }

  /// One frame of fire at `now` for a view `size` big: moves its line,
  /// spawns particles there while it runs and moves them all
  /// (`FireAnimation::step`, wayfire:plugins/animate/fire/fire.cpp:295-310).
  pub fn step(&mut self, now: Instant, size: (f64, f64)) {
    let running = !self.clock.ready(now);
    let progress = self.clock.progress(now);
    let State::Fire(fire) = &mut self.state else {
      return;
    };
    fire.size = size;
    fire.progress_line = fire.line.at(progress);
    if running {
      let count = fire.system.particles.len() / 10;
      let (random, options, line) =
        (&mut fire.random, fire.options, fire.progress_line);
      fire.system.spawn(count, || {
        fire::Particle::spawn(random, &options, size, line)
      });
    }
    fire.system.update();
    let count = fire::particle_count_for_width(&fire.options, size.0);
    fire.system.resize(count);
    fire.drawn = fire.system.drawn();
  }

  /// Fire's progress line, the fraction of the view from the top that is
  /// still drawn, and its live particles relative to the view's top left
  /// corner; `None` for the other effects.
  pub fn fire(&self) -> Option<(f64, &[shingle_render::Particle])> {
    let State::Fire(fire) = &self.state else {
      return None;
    };
    Some((fire.progress_line, &fire.drawn))
  }
}

/// Zoom's transitions (wayfire:plugins/animate/basic_animations.hpp:
/// 160-212): from a third of the size and transparent, or for minimizing
/// with a hint, from the hint's size and place.
fn zoom_state(kind: Kind, view: &ViewInfo) -> State {
  let mut alpha = Transition::new(0.0, 1.0);
  let mut zoom = Transition::new(1.0 / 3.0, 1.0);
  let mut offset_x = Transition::new(0.0, 0.0);
  let mut offset_y = Transition::new(0.0, 0.0);
  if kind.minimize_state()
    && let Some(hint) = view
      .minimize_hint
      .filter(|hint| hint.width > 0 && hint.height > 0)
  {
    // Integer centres, as Wayfire's ints.
    let hint_cx = hint.x + (hint.width / 2) as i32;
    let hint_cy = hint.y + (hint.height / 2) as i32;
    let bbox = view.geometry;
    let view_cx = bbox.x + (bbox.width / 2) as i32;
    let view_cy = bbox.y + (bbox.height / 2) as i32;
    offset_x = Transition::new(f64::from(hint_cx - view_cx), 0.0);
    offset_y = Transition::new(f64::from(hint_cy - view_cy), 0.0);
    if bbox.width > 0 && bbox.height > 0 {
      let scale_x = f64::from(hint.width) / f64::from(bbox.width);
      let scale_y = f64::from(hint.height) / f64::from(bbox.height);
      zoom = Transition::new(scale_x.min(scale_y), 1.0);
    }
  }
  if kind.hiding() {
    alpha.flip();
    zoom.flip();
    offset_x.flip();
    offset_y.flip();
  }
  State::Zoom {
    alpha,
    zoom,
    offset_x,
    offset_y,
  }
}

/// Squeezimize's target, the minimize hint or else 100 by 50 pixels below
/// the middle of the output's bottom edge, and which edge it is closest
/// to: `(target, upward, horizontal)`
/// (wayfire:plugins/animate/squeezimize.hpp:463-533).
fn squeeze_target(view: &ViewInfo) -> (Rect, bool, bool) {
  let output = view.output;
  let target = view
    .minimize_hint
    .filter(|hint| hint.width > 0 && hint.height > 0)
    .unwrap_or_else(|| {
      Rect::new(
        output.x + (output.width / 2) as i32 - 50,
        output.y + output.height as i32,
        100,
        50,
      )
    });
  // Relative to the output, as Wayfire's view coordinates are.
  let x = f64::from(target.x - output.x) + f64::from(target.width) / 2.0;
  let y = f64::from(target.y - output.y) + f64::from(target.height) / 2.0;
  let width = f64::from(output.width);
  let height = f64::from(output.height).max(1.0);
  let y2 = y * width / height;
  let (horizontal, upward) = if x < y2 {
    if x < width - y2 {
      // The left edge.
      (true, true)
    } else {
      // The bottom edge.
      (false, false)
    }
  } else if x > width - y2 {
    // The right edge.
    (true, false)
  } else {
    // The top edge.
    (false, true)
  };
  (target, upward, horizontal)
}

/// The area of the warp: from the leftmost and topmost of the view and
/// the target, as wide and tall as Wayfire's formula makes it
/// (wayfire:plugins/animate/squeezimize.hpp:353-364).
fn squeeze_area(view: Rect, target: Rect) -> Rect {
  let (vx, vy, vw, vh) = (
    i64::from(view.x),
    i64::from(view.y),
    i64::from(view.width),
    i64::from(view.height),
  );
  let (tx, ty, tw, th) = (
    i64::from(target.x),
    i64::from(target.y),
    i64::from(target.width),
    i64::from(target.height),
  );
  let width = vw.max(tw).max(tx + tw - vx).max(vx + vw - tx);
  let height = vh.max(th).max(ty + th - vy).max(vy + vh - ty);
  Rect::new(
    view.x.min(target.x),
    view.y.min(target.y),
    width.clamp(0, i64::from(u32::MAX)) as u32,
    height.clamp(0, i64::from(u32::MAX)) as u32,
  )
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use shingle_config::Easing;

  use super::*;

  fn view() -> ViewInfo {
    ViewInfo {
      geometry: Rect::new(100, 100, 400, 200),
      output: Rect::new(0, 0, 1000, 800),
      minimize_hint: None,
      spin_rotations: 1,
      fire: FireOptions::of(&shingle_config::AnimateConfig::default()),
      seed: 1,
    }
  }

  fn linear(ms: u64) -> Timing {
    Timing::new(ms, Easing::Linear)
  }

  fn close(actual: f64, expected: f64) {
    assert!(
      (actual - expected).abs() < 1e-9,
      "{actual} is not {expected}"
    );
  }

  #[test]
  fn fade_and_zoom_follow_wayfires_transitions() {
    let now = Instant::now();
    let at = |ms| now + Duration::from_millis(ms);
    let fade =
      Animation::start(Effect::Fade, Kind::Map, linear(400), now, &view());
    close(fade.transform(at(100)).alpha, 0.25);
    let out =
      Animation::start(Effect::Fade, Kind::Unmap, linear(400), now, &view());
    close(out.transform(at(100)).alpha, 0.75);
    // Circle easing through the flipped transition: 1 - circle(t).
    let eased = Animation::start(
      Effect::Fade,
      Kind::Unmap,
      Timing::new(400, Easing::Circle),
      now,
      &view(),
    );
    close(eased.transform(at(200)).alpha, 1.0 - 0.75f64.sqrt());
    let zoom =
      Animation::start(Effect::Zoom, Kind::Map, linear(400), now, &view());
    let half = zoom.transform(at(200));
    close(half.alpha, 0.5);
    close(half.scale_x, 1.0 / 3.0 + (2.0 / 3.0) * 0.5);
    assert_eq!(half.translation, (0.0, 0.0));
    let ended = zoom.transform(at(400));
    assert_eq!(ended, ViewTransform::IDENTITY);
    assert!(!zoom.running(at(400)));
  }

  #[test]
  fn zoom_minimizes_into_the_hint() {
    let now = Instant::now();
    let mut info = view();
    info.minimize_hint = Some(Rect::new(400, 760, 100, 40));
    let zoom =
      Animation::start(Effect::Zoom, Kind::Minimize, linear(100), now, &info);
    let start = zoom.transform(now);
    assert_eq!(start, ViewTransform::IDENTITY);
    let end = zoom.transform(now + Duration::from_millis(99));
    // Towards the hint's centre, (450, 780), from the view's (300, 200),
    // and its size, min(100 / 400, 40 / 200).
    assert!((end.translation.0 - 150.0 * 0.99).abs() < 1e-6);
    assert!((end.translation.1 - 580.0 * 0.99).abs() < 1e-6);
    assert!((end.scale_x - (1.0 + (0.2 - 1.0) * 0.99)).abs() < 1e-6);
    assert!((end.alpha - 0.01).abs() < 1e-6);
  }

  #[test]
  fn zap_and_spin_run_in_thirds_and_turns() {
    let now = Instant::now();
    let at = |ms| now + Duration::from_millis(ms);
    let zap =
      Animation::start(Effect::Zap, Kind::Map, linear(300), now, &view());
    let first = zap.transform(at(50));
    close(first.alpha, 0.5);
    close(first.scale_x, 0.01);
    close(first.scale_y, 0.01);
    let second = zap.transform(at(150));
    close(second.alpha, 1.0);
    close(second.scale_x, 0.01 + 0.5 * 0.99);
    close(second.scale_y, 0.01);
    let third = zap.transform(at(250));
    close(third.scale_x, 1.0);
    close(third.scale_y, 0.01 + 0.5 * 0.99);
    // Hiding runs the clock backwards: easing(1 - t).
    let away =
      Animation::start(Effect::Zap, Kind::Unmap, linear(300), now, &view());
    close(away.transform(at(250)).alpha, 0.5);
    let mut info = view();
    info.spin_rotations = -2;
    let spin =
      Animation::start(Effect::Spin, Kind::Map, linear(400), now, &info);
    let quarter = spin.transform(at(100));
    close(quarter.alpha, 0.25);
    close(quarter.scale_x, 0.01 + 0.25 * 0.99);
    close(quarter.angle, -std::f64::consts::PI);
  }

  #[test]
  fn a_change_of_direction_reverses_from_where_it_is() {
    let now = Instant::now();
    let at = |ms| now + Duration::from_millis(ms);
    let mut fade =
      Animation::start(Effect::Fade, Kind::Map, linear(400), now, &view());
    fade.set_kind(Kind::Unmap, at(100));
    // A quarter in, going back takes a quarter.
    close(fade.transform(at(100)).alpha, 0.25);
    close(fade.transform(at(150)).alpha, 0.125);
    assert!(!fade.running(at(200)));
    close(fade.transform(at(200)).alpha, 0.0);
  }

  #[test]
  fn squeezimize_aims_at_the_edge_its_target_is_on() {
    let now = Instant::now();
    let info = view();
    let squeeze = Animation::start(
      Effect::Squeezimize,
      Kind::Minimize,
      linear(100),
      now,
      &info,
    );
    let (area, uniforms) = squeeze
      .squeeze(now + Duration::from_millis(50), info.geometry)
      .unwrap();
    // Without a hint: 100 by 50 below the middle of the bottom edge, at
    // (450, 800), so the area reaches 550 to the right and 850 down.
    assert_eq!(
      area,
      RenderRect {
        x: 100,
        y: 100,
        width: 450,
        height: 750,
      }
    );
    assert!(!uniforms.horizontal && !uniforms.upward);
    assert_eq!(uniforms.progress, 0.5);
    assert_eq!(
      uniforms.source,
      [
        0.0,
        0.0,
        (400.0f64 / 450.0) as f32,
        (200.0f64 / 750.0) as f32
      ]
    );
    assert_eq!(
      uniforms.target,
      [
        (350.0f64 / 450.0) as f32,
        (700.0f64 / 750.0) as f32,
        1.0,
        1.0
      ]
    );
    // Restoring runs from the target back.
    let back = Animation::start(
      Effect::Squeezimize,
      Kind::Restore,
      linear(100),
      now,
      &info,
    );
    let (_, uniforms) = back
      .squeeze(now + Duration::from_millis(25), info.geometry)
      .unwrap();
    assert_eq!(uniforms.progress, 0.75);
    for (hint, upward, horizontal) in [
      (Rect::new(0, 380, 40, 40), true, true),
      (Rect::new(960, 380, 40, 40), false, true),
      (Rect::new(480, 0, 40, 40), true, false),
      (Rect::new(480, 760, 40, 40), false, false),
    ] {
      let mut info = view();
      info.minimize_hint = Some(hint);
      let (_, edge_up, edge_horizontal) = squeeze_target(&info);
      assert_eq!((edge_up, edge_horizontal), (upward, horizontal), "{hint:?}");
    }
  }

  #[test]
  fn fire_burns_down_its_line_and_waits_for_its_particles() {
    let now = Instant::now();
    let info = view();
    // 200 pixels tall: half of fire's length.
    let mut fire =
      Animation::start(Effect::Fire, Kind::Map, linear(400), now, &info);
    let size = (400.0, 200.0);
    fire.step(now, size);
    let (line, particles) = fire.fire().unwrap();
    close(line, 0.0);
    // The first frame only sizes the system: 2000 for 400 pixels.
    assert!(particles.is_empty());
    fire.step(now + Duration::from_millis(100), size);
    let (line, particles) = fire.fire().unwrap();
    close(line, 0.5);
    assert_eq!(particles.len(), 200);
    assert!(
      particles
        .iter()
        .all(|particle| (80.0..=112.0).contains(&particle.center[1]))
    );
    let end = now + Duration::from_millis(200);
    assert!(fire.clock_ready(end));
    assert!(fire.running(end), "the particles still live");
    let mut later = end;
    // The slowest particle fades by 0.024 a frame.
    for _ in 0..45 {
      later += Duration::from_millis(16);
      fire.step(later, size);
    }
    assert!(!fire.running(later));
    let burning =
      Animation::start(Effect::Fire, Kind::Unmap, linear(400), now, &info);
    assert_eq!(burning.fire().unwrap().0, 1.0);
  }

  #[test]
  fn a_transform_scales_about_the_centre_and_moves() {
    let transform = ViewTransform {
      scale_x: 0.5,
      scale_y: 0.5,
      translation: (10.0, -10.0),
      ..ViewTransform::IDENTITY
    };
    let rect = RenderRect {
      x: 100,
      y: 100,
      width: 200,
      height: 100,
    };
    assert_eq!(
      transform.apply(rect, (200.0, 150.0)),
      RenderRect {
        x: 160,
        y: 115,
        width: 100,
        height: 50,
      }
    );
  }
}
