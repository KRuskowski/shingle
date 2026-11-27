//! The minimal style (`lock.style: minimal`, the default): over the
//! blur, the time large and centred at rest; on typing it fades and
//! slides gently up while a slim white rounded outline, transparent
//! inside, fades and rises into the centre, with a white dot per
//! character. Caps Lock and the keyboard layout are small, semi-
//! transparent white text below it. Verifying breathes the outline's
//! opacity; a wrong password shakes the input a few pixels sideways,
//! fades its dots out and warms the outline for a moment; success fades
//! everything out over the blur before the unlock
//! (docs/design/lock-screen.md).
//!
//! [`Scene`] holds when things happened and answers, for any moment, how
//! everything looks then ([`Look`]); [`rasterize_input`] and
//! [`rasterize_clock`] draw the images the GPU places, fades and moves.

use std::time::{Duration, Instant};

use ab_glyph::FontArc;

use crate::{
  gpu::{Gpu, Placement, Target},
  motion::{self, Shake, Tween, breathing, ease_out, progress},
  ui::{Raster, rasterize_text, rounded_rect_distance},
};

/// How long the clock takes to give the centre to the input.
pub const FOCUS_IN: Duration = Duration::from_millis(420);
/// How long the input takes to give it back after the timeout.
pub const FOCUS_OUT: Duration = Duration::from_millis(650);
/// A dot's fade and scale in, and out on Backspace.
pub const DOT: Duration = Duration::from_millis(150);
/// The dots' fade on Escape.
pub const CLEAR: Duration = Duration::from_millis(240);
/// The dots' fade after a wrong password.
pub const WRONG: Duration = Duration::from_millis(380);
/// The wrong password's shake.
pub const SHAKE: Shake = Shake {
  amplitude: 6.0,
  frequency: 7.0,
  damping: Duration::from_millis(140),
  length: Duration::from_millis(560),
};
/// How long the outline's warm tint takes to fade after a wrong
/// password, and how strong it starts.
pub const TINT: Duration = Duration::from_millis(1100);
const TINT_STRENGTH: f32 = 0.55;
/// The colour the outline warms towards.
const WARM: [f32; 3] = [255.0, 168.0, 140.0];
/// One breath of the outline while verifying, and how faint it gets.
pub const BREATH: Duration = Duration::from_millis(1600);
const BREATH_LOW: f32 = 0.35;
/// How long the outline takes to come back to full after verifying.
const BREATH_BACK: Duration = Duration::from_millis(250);
/// The fade of everything over the blur before the unlock.
pub const LEAVE: Duration = Duration::from_millis(380);
/// A dot starts at this fraction of its size.
const DOT_START_SCALE: f32 = 0.35;
/// The outline's width in logical pixels.
pub const BORDER: f32 = 1.5;
/// The labels' opacity.
const LABEL_ALPHA: f32 = 0.6;

/// One dot: when it was typed, and when and how fast it goes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Dot {
  born: Instant,
  gone: Option<(Instant, Duration)>,
}

impl Dot {
  /// How present it is at `now`, 0 to 1.
  fn presence(&self, now: Instant) -> f32 {
    let entering = ease_out(progress(self.born, DOT, now));
    let leaving = self.gone.map_or(1.0, |(at, length)| {
      1.0 - ease_out(progress(at, length, now))
    });
    entering * leaving
  }

  fn moving(&self, now: Instant) -> bool {
    now < self.born + DOT
      || self.gone.is_some_and(|(at, length)| now < at + length)
  }

  fn finished(&self, now: Instant) -> bool {
    self.gone.is_some_and(|(at, length)| now >= at + length)
  }
}

/// How one dot looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DotLook {
  /// How much of its place in the row it takes, 0 to 1.
  pub presence: f32,
  /// Its opacity.
  pub alpha: f32,
  /// Its size, a fraction of the full dot.
  pub scale: f32,
}

/// How the minimal style looks at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
  /// The clock's opacity.
  pub clock_opacity: f32,
  /// How far the clock has left, 0 at rest to 1 gone: it moves up and
  /// shrinks a little with it.
  pub clock_away: f32,
  /// The input's opacity.
  pub input_opacity: f32,
  /// How far the input still has to rise, 1 below the centre to 0 in it.
  pub input_below: f32,
  /// The shake's sideways offset in logical pixels.
  pub shake: f32,
  /// The outline's opacity.
  pub outline_alpha: f32,
  /// How warm the outline is, 0 to 1.
  pub tint: f32,
  /// The dots, in order.
  pub dots: Vec<DotLook>,
}

/// The minimal style's state in time.
#[derive(Clone, Debug)]
pub struct Scene {
  /// 0 with the clock in the centre, 1 with the input there.
  focus: Tween,
  last_activity: Option<Instant>,
  timeout: Duration,
  /// Whether the input shows at all (not `no-unlock-indicator`).
  input: bool,
  dots: Vec<Dot>,
  verifying: Option<Instant>,
  /// When verifying ended and the outline's opacity then.
  breath_end: Option<(Instant, f32)>,
  wrong: Option<Instant>,
  leaving: Option<Instant>,
}

impl Scene {
  /// The clock alone in the centre; the input, unless `input` is false,
  /// comes on typing and goes `timeout` after the last key.
  pub fn new(timeout: Duration, input: bool, now: Instant) -> Self {
    Self {
      focus: Tween::at_rest(0.0, now),
      last_activity: None,
      timeout,
      input,
      dots: Vec::new(),
      verifying: None,
      breath_end: None,
      wrong: None,
      leaving: None,
    }
  }

  /// When the input starts to give the centre back, if it does without
  /// a key.
  fn fade_out_at(&self) -> Option<Instant> {
    if self.verifying.is_some() || self.leaving.is_some() {
      return None;
    }
    self
      .last_activity
      .map(|last| last + self.timeout)
      .filter(|_| self.focus.target() > 0.0)
  }

  /// The focus at `now`, with the fade back to the clock after the
  /// timeout.
  fn focus_at(&self, now: Instant) -> f32 {
    match self.fade_out_at() {
      Some(out) if now >= out => {
        let from = self.focus.value(out);
        from * (1.0 - motion::ease_in_out(progress(out, FOCUS_OUT, now)))
      }
      _ => self.focus.value(now),
    }
  }

  fn live_dots(&self) -> usize {
    self.dots.iter().filter(|dot| dot.gone.is_none()).count()
  }

  fn forget_finished(&mut self, now: Instant) {
    self.dots.retain(|dot| !dot.finished(now));
  }

  /// Any key: the input comes to the centre, or stays.
  fn touch(&mut self, now: Instant) {
    self.forget_finished(now);
    if self.input && self.leaving.is_none() {
      let fading = self.fade_out_at().is_some_and(|out| now >= out);
      if self.focus.target() < 1.0 || fading {
        let current = self.focus_at(now);
        self.focus = Tween::at_rest(current, now);
        self.focus.retarget(1.0, now, FOCUS_IN);
      }
    }
    self.last_activity = Some(now);
  }

  fn drop_dots(&mut self, keep: usize, now: Instant, length: Duration) {
    let mut live = self.live_dots();
    for dot in self.dots.iter_mut().rev() {
      if live <= keep {
        break;
      }
      if dot.gone.is_none() {
        dot.gone = Some((now, length));
        live -= 1;
      }
    }
  }

  fn end_verifying(&mut self, now: Instant) {
    if let Some(since) = self.verifying.take() {
      let alpha =
        breathing(now.saturating_duration_since(since), BREATH, BREATH_LOW);
      self.breath_end = Some((now, alpha));
    }
  }

  /// A character was typed; `chars` are typed now.
  pub fn typed(&mut self, chars: usize, now: Instant) {
    self.touch(now);
    self.wrong = None;
    for _ in self.live_dots()..chars {
      self.dots.push(Dot {
        born: now,
        gone: None,
      });
    }
  }

  /// Characters were deleted; `chars` are left.
  pub fn deleted(&mut self, chars: usize, now: Instant) {
    self.touch(now);
    self.drop_dots(chars, now, DOT);
  }

  /// Everything typed was cleared (Escape).
  pub fn cleared(&mut self, now: Instant) {
    self.touch(now);
    self.drop_dots(0, now, CLEAR);
  }

  /// A key that types nothing.
  pub fn poked(&mut self, now: Instant) {
    self.touch(now);
  }

  /// PAM checks the password.
  pub fn verifying(&mut self, now: Instant) {
    self.touch(now);
    self.verifying = Some(now);
    self.breath_end = None;
  }

  /// PAM refused it: the shake, the tint, and the dots fade out.
  pub fn denied(&mut self, now: Instant) {
    self.end_verifying(now);
    self.touch(now);
    self.wrong = Some(now);
    self.drop_dots(0, now, WRONG);
  }

  /// PAM accepted it: everything fades out over the blur.
  pub fn accepted(&mut self, now: Instant) {
    self.leaving = Some(now);
  }

  /// Whether the fade after success is over at `now`.
  pub fn left(&self, now: Instant) -> bool {
    self.leaving.is_some_and(|at| now >= at + LEAVE)
  }

  /// Whether anything moves at `now`, which wants frames at the output's
  /// rate.
  pub fn animating(&self, now: Instant) -> bool {
    if let Some(at) = self.leaving {
      // Nothing moves once the fade is over, so that the frame callback
      // of the last frame can say every output showed it.
      return now < at + LEAVE;
    }
    let fading_out = self
      .fade_out_at()
      .is_some_and(|out| now >= out && now < out + FOCUS_OUT);
    self.focus.moving(now)
      || fading_out
      || self.verifying.is_some()
      || self.leaving.is_some_and(|at| now < at + LEAVE)
      || self
        .breath_end
        .is_some_and(|(at, _)| now < at + BREATH_BACK)
      || self
        .wrong
        .is_some_and(|at| now < at + TINT.max(SHAKE.length))
      || self.dots.iter().any(|dot| dot.moving(now))
  }

  /// When something next starts to move without input, after `now`.
  pub fn next_change(&self, now: Instant) -> Option<Instant> {
    self.fade_out_at().filter(|out| *out > now)
  }

  /// How everything looks at `now`.
  pub fn look(&self, now: Instant) -> Look {
    let focus = self.focus_at(now);
    let leaving = self
      .leaving
      .map_or(1.0, |at| 1.0 - ease_out(progress(at, LEAVE, now)));
    let outline_alpha = match (self.verifying, self.breath_end) {
      (Some(since), _) => {
        breathing(now.saturating_duration_since(since), BREATH, BREATH_LOW)
      }
      (None, Some((at, alpha))) if now < at + BREATH_BACK => {
        alpha + (1.0 - alpha) * ease_out(progress(at, BREATH_BACK, now))
      }
      _ => 1.0,
    };
    let (shake, tint) = self.wrong.map_or((0.0, 0.0), |at| {
      let elapsed = now.saturating_duration_since(at);
      let tint =
        TINT_STRENGTH * (1.0 - motion::ease_in_out(progress(at, TINT, now)));
      (SHAKE.offset(elapsed), tint)
    });
    Look {
      // The clock goes a little faster than the input comes, so that
      // the two cross in the middle of the move.
      clock_opacity: (1.0 - focus * 1.5).clamp(0.0, 1.0) * leaving,
      clock_away: focus,
      input_opacity: ((focus - 0.2) / 0.8).clamp(0.0, 1.0) * leaving,
      input_below: 1.0 - focus,
      shake,
      outline_alpha,
      tint,
      dots: self
        .dots
        .iter()
        .map(|dot| {
          let presence = dot.presence(now);
          DotLook {
            presence,
            alpha: presence,
            scale: DOT_START_SCALE + (1.0 - DOT_START_SCALE) * presence,
          }
        })
        .filter(|dot| dot.presence > 0.0)
        .collect(),
    }
  }
}

/// Where the minimal style puts things on one output, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
  /// The output's size.
  pub size: (u32, u32),
  /// The unit everything is sized by: 1 on a 1080 pixel high output.
  pub unit: f32,
  /// The input's outline: width and height; its centre is the output's.
  pub input: (f32, f32),
  /// The pixel sizes of the time, the date and the labels.
  pub text_sizes: (f32, f32, f32),
  /// How far the clock moves up as it leaves, and the input rises as
  /// it comes.
  pub travel: (f32, f32),
}

impl Layout {
  /// The layout of an output of `size`; `font_size` sets the labels'
  /// size, and everything else with it.
  pub fn new(size: (u32, u32), font_size: Option<u32>) -> Self {
    let unit = match font_size {
      Some(points) => points as f32 / 15.0,
      None => (size.1 as f32 / 1080.0).clamp(0.75, 2.0),
    };
    let width = (300.0 * unit).min(size.0 as f32 - 32.0).max(60.0);
    Self {
      size,
      unit,
      input: (width.round(), (44.0 * unit).round()),
      text_sizes: (144.0 * unit, 24.0 * unit, 15.0 * unit),
      travel: (64.0 * unit, 28.0 * unit),
    }
  }

  /// The outline's corner radius.
  pub fn corner(&self) -> f32 {
    (self.input.1 * 0.3).round()
  }

  /// The dots' diameter and the distance from one to the next.
  pub fn dot_size(&self) -> (f32, f32) {
    (8.0 * self.unit, 18.0 * self.unit)
  }
}

/// Where the clock and the input go in `look` on `layout`: the clock,
/// an image of `clock` pixels, centred at rest and moving up and
/// shrinking a little as it leaves; the input, an image of `input`'s
/// size with its outline at `input`'s offset, with the outline centred
/// once it has risen, shaken sideways. A part that does not show, or
/// is fully transparent, has none.
pub fn placements(
  layout: &Layout,
  look: &Look,
  clock: Option<(u32, u32)>,
  input: Option<((u32, u32), (f32, f32))>,
) -> (Option<Placement>, Option<Placement>) {
  let (width, height) = (layout.size.0 as f32, layout.size.1 as f32);
  let clock = clock.filter(|_| look.clock_opacity > 0.0).map(|size| {
    // At rest on whole pixels, so that the text is drawn as rasterized.
    let rest = (
      ((width - size.0 as f32) / 2.0).round(),
      ((height - size.1 as f32) / 2.0).round(),
    );
    Placement {
      at: (rest.0, rest.1 - layout.travel.0 * look.clock_away),
      scale: 1.0 - 0.04 * look.clock_away,
      alpha: look.clock_opacity,
    }
  });
  let input = input
    .filter(|_| look.input_opacity > 0.0)
    .map(|(_, outline)| {
      let rest = (
        (width / 2.0 - layout.input.0 / 2.0 - outline.0).round(),
        (height / 2.0 - layout.input.1 / 2.0 - outline.1).round(),
      );
      Placement {
        at: (
          rest.0 + look.shake,
          rest.1 + layout.travel.1 * look.input_below,
        ),
        scale: 1.0,
        alpha: look.input_opacity,
      }
    });
  (clock, input)
}

/// The input image a target shows: what it shows, its size and where
/// its outline sits in it.
pub type ShownInput = (InputLook, (u32, u32), (f32, f32));

/// Brings `target`'s input to `look` with `labels` below it, in `font`:
/// its image is redrawn only when what it shows differs from `shown`,
/// which then becomes it. Returns where the clock, an image of `clock`
/// pixels, and the input go ([`placements`]), and whether the input's
/// image was redrawn.
#[allow(clippy::too_many_arguments)]
pub fn update_overlays(
  gpu: &Gpu,
  target: &mut Target,
  layout: &Layout,
  look: &Look,
  labels: Vec<String>,
  font: &FontArc,
  shown: &mut Option<ShownInput>,
  clock: Option<(u32, u32)>,
) -> (Option<Placement>, Option<Placement>, bool) {
  let wanted = InputLook {
    outline_alpha: look.outline_alpha,
    tint: look.tint,
    dots: look.dots.clone(),
    labels,
  };
  let redraw = look.input_opacity > 0.0
    && shown.as_ref().is_none_or(|(drawn, _, _)| *drawn != wanted);
  if redraw {
    let drawn = rasterize_input(layout, &wanted, font);
    target.set_input(gpu, Some(&drawn.raster));
    let size = (drawn.raster.width, drawn.raster.height);
    *shown = Some((wanted, size, drawn.outline_at));
  }
  let input = shown.as_ref().map(|(_, size, outline)| (*size, *outline));
  let (clock, input) = placements(layout, look, clock, input);
  (clock, input, redraw)
}

/// What the input image shows: its [`Look`]'s drawn parts and the
/// labels below it.
#[derive(Clone, Debug, PartialEq)]
pub struct InputLook {
  /// The outline's opacity.
  pub outline_alpha: f32,
  /// Its warmth.
  pub tint: f32,
  /// The dots.
  pub dots: Vec<DotLook>,
  /// The labels below, such as `Caps Lock` and the layout's name.
  pub labels: Vec<String>,
}

/// The input's image and where its outline sits in it.
pub struct InputRaster {
  /// The image.
  pub raster: Raster,
  /// The outline's top-left corner in the image.
  pub outline_at: (f32, f32),
}

/// Space left around the outline for its anti-aliased edge.
const PAD: f32 = 2.0;

/// The input of `layout` showing `look`, rasterized: the outline, the
/// dots centred in it, the labels below it in `font`.
pub fn rasterize_input(
  layout: &Layout,
  look: &InputLook,
  font: &FontArc,
) -> InputRaster {
  let (box_width, box_height) = layout.input;
  let label = (!look.labels.is_empty()).then(|| {
    rasterize_text(
      &look.labels.join("  ·  "),
      layout.text_sizes.2,
      [255, 255, 255, (255.0 * LABEL_ALPHA) as u8],
      font,
    )
  });
  let gap = (14.0 * layout.unit).round();
  let label_width = label.as_ref().map_or(0.0, |label| label.width as f32);
  let width = box_width.max(label_width) + 2.0 * PAD;
  let height = box_height
    + 2.0 * PAD
    + label
      .as_ref()
      .map_or(0.0, |label| gap + label.height as f32);
  let mut raster = Raster::new(width.ceil() as u32, height.ceil() as u32);
  let outline_at = (((width - box_width) / 2.0).round(), PAD);
  let centre = (
    outline_at.0 + box_width / 2.0,
    outline_at.1 + box_height / 2.0,
  );
  let half = (box_width / 2.0, box_height / 2.0);
  let corner = layout.corner();
  let mix = |channel: usize| 255.0 + (WARM[channel] - 255.0) * look.tint;
  let outline = [mix(0), mix(1), mix(2)].map(|value| value.round() as u8);
  let alpha = (255.0 * look.outline_alpha.clamp(0.0, 1.0)).round() as u8;
  let color = [outline[0], outline[1], outline[2], alpha];
  let rows = outline_at.1 as i32 - 1..(outline_at.1 + box_height) as i32 + 1;
  for y in rows {
    for x in outline_at.0 as i32 - 1..(outline_at.0 + box_width) as i32 + 1 {
      let point = (x as f32 + 0.5, y as f32 + 0.5);
      let distance = rounded_rect_distance(point, centre, half, corner);
      // Inside the outer edge and outside the inner one.
      let outer = (0.5 - distance).clamp(0.0, 1.0);
      let inner = (0.5 - distance - BORDER).clamp(0.0, 1.0);
      raster.blend(x, y, color, outer - inner);
    }
  }
  let (diameter, spacing) = layout.dot_size();
  let inner = box_width - 2.0 * box_height * 0.4;
  let fitting = (inner / spacing).floor().max(1.0);
  // The newest dots that fit, by the room they take.
  let mut room = 0.0;
  let first = look
    .dots
    .iter()
    .rposition(|dot| {
      room += dot.presence;
      room > fitting
    })
    .map_or(0, |index| index + 1);
  let shown = &look.dots[first..];
  let total = shown.iter().map(|dot| dot.presence).sum::<f32>();
  let mut x = centre.0 - spacing * total / 2.0;
  for dot in shown {
    let cx = x + spacing * dot.presence / 2.0;
    x += spacing * dot.presence;
    let alpha = (255.0 * dot.alpha.clamp(0.0, 1.0)).round() as u8;
    fill_circle(
      &mut raster,
      (cx, centre.1),
      diameter / 2.0 * dot.scale,
      [255, 255, 255, alpha],
    );
  }
  if let Some(label) = &label {
    let at = (
      ((width - label.width as f32) / 2.0).round() as i32,
      (outline_at.1 + box_height + gap).round() as i32,
    );
    crate::ui::composite(&mut raster, label, at);
  }
  InputRaster { raster, outline_at }
}

fn fill_circle(
  raster: &mut Raster,
  (cx, cy): (f32, f32),
  radius: f32,
  color: [u8; 4],
) {
  if radius <= 0.0 {
    return;
  }
  for y in (cy - radius - 1.0).floor() as i32..(cy + radius + 1.0).ceil() as i32
  {
    for x in
      (cx - radius - 1.0).floor() as i32..(cx + radius + 1.0).ceil() as i32
    {
      let dx = x as f32 + 0.5 - cx;
      let dy = y as f32 + 0.5 - cy;
      let distance = (dx * dx + dy * dy).sqrt() - radius;
      raster.blend(x, y, color, (0.5 - distance).clamp(0.0, 1.0));
    }
  }
}

/// The clock: `time` in the light `clock_font` over `date` in
/// `text_font`, white, centred, with a soft wide shadow so that white
/// reads over a light blur too. Empty lines are left out; `None` when
/// both are.
pub fn rasterize_clock(
  time: &str,
  date: &str,
  layout: &Layout,
  clock_font: &FontArc,
  text_font: &FontArc,
) -> Option<Raster> {
  let white = [255, 255, 255, 255];
  let mut lines = Vec::new();
  if !time.is_empty() {
    lines.push(rasterize_text(time, layout.text_sizes.0, white, clock_font));
  }
  if !date.is_empty() {
    lines.push(rasterize_text(
      date,
      layout.text_sizes.1,
      [255, 255, 255, 220],
      text_font,
    ));
  }
  if lines.is_empty() {
    return None;
  }
  let blur = (6.0 * layout.unit).round().max(1.0) as u32;
  let margin = blur * 3;
  let width = lines.iter().map(|line| line.width).max().unwrap_or(1);
  // The time's line box has room above and below its digits; the date
  // tucks up into the room below.
  let overlap = (layout.text_sizes.0 * 0.12) as i32;
  let height = lines.iter().map(|line| line.height).sum::<u32>()
    - overlap.max(0) as u32 * (lines.len() as u32 - 1);
  let mut text = Raster::new(width + 2 * margin, height + 2 * margin);
  let mut y = margin as i32;
  for line in &lines {
    let x = (margin + (width - line.width) / 2) as i32;
    crate::ui::composite(&mut text, line, (x, y));
    y += line.height as i32 - overlap;
  }
  let mut shadow = text.clone();
  shadow_from_alpha(&mut shadow, blur, 0.28);
  let offset = (2.0 * layout.unit).round() as i32;
  let mut raster = Raster::new(text.width, text.height);
  crate::ui::composite(&mut raster, &shadow, (0, offset));
  crate::ui::composite(&mut raster, &text, (0, 0));
  Some(raster)
}

/// Turns `raster` into a black shadow of its alpha, blurred by three box
/// passes of `radius` each way, at `strength` of its opacity.
fn shadow_from_alpha(raster: &mut Raster, radius: u32, strength: f32) {
  let (width, height) = (raster.width as usize, raster.height as usize);
  let mut alpha = raster
    .rgba
    .chunks_exact(4)
    .map(|pixel| f32::from(pixel[3]))
    .collect::<Vec<_>>();
  let mut scratch = vec![0.0; alpha.len()];
  for _ in 0..3 {
    box_blur(&alpha, &mut scratch, width, height, radius as usize, true);
    box_blur(&scratch, &mut alpha, width, height, radius as usize, false);
  }
  for (pixel, value) in raster.rgba.chunks_exact_mut(4).zip(alpha) {
    let value = (value * strength).round().min(255.0) as u8;
    pixel.copy_from_slice(&[0, 0, 0, value]);
  }
}

/// One box blur pass of `radius` from `from` into `to`, along rows or
/// columns, with zero outside.
fn box_blur(
  from: &[f32],
  to: &mut [f32],
  width: usize,
  height: usize,
  radius: usize,
  rows: bool,
) {
  let (lines, length) = if rows {
    (height, width)
  } else {
    (width, height)
  };
  let at = |line: usize, index: usize| {
    if rows {
      line * width + index
    } else {
      index * width + line
    }
  };
  let span = (2 * radius + 1) as f32;
  for line in 0..lines {
    let mut sum = 0.0;
    for index in 0..radius.min(length) {
      sum += from[at(line, index)];
    }
    for index in 0..length {
      if index + radius < length {
        sum += from[at(line, index + radius)];
      }
      to[at(line, index)] = sum / span;
      if index >= radius {
        sum -= from[at(line, index - radius)];
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(start: Instant, millis: u64) -> Instant {
    start + Duration::from_millis(millis)
  }

  #[test]
  fn at_rest_the_clock_has_the_centre() {
    let start = Instant::now();
    let scene = Scene::new(Duration::from_secs(5), true, start);
    let look = scene.look(ms(start, 10_000));
    assert_eq!((look.clock_opacity, look.input_opacity), (1.0, 0.0));
    assert_eq!((look.clock_away, look.input_below), (0.0, 1.0));
    assert!(!scene.animating(start));
    assert_eq!(scene.next_change(start), None);
  }

  #[test]
  fn typing_crossfades_the_clock_and_the_input_and_the_timeout_returns() {
    let start = Instant::now();
    let mut scene = Scene::new(Duration::from_secs(5), true, start);
    scene.typed(1, start);
    let early = scene.look(ms(start, 60));
    assert!(early.clock_opacity < 1.0 && early.clock_opacity > 0.0);
    assert!(scene.animating(ms(start, 60)));
    // Half way: the clock mostly gone and the input rising.
    let middle = scene.look(ms(start, 150));
    assert!(middle.clock_opacity < 0.4, "{middle:?}");
    assert!(middle.input_opacity > 0.3 && middle.input_below > 0.0);
    let done = scene.look(ms(start, 500));
    assert_eq!((done.clock_opacity, done.input_opacity), (0.0, 1.0));
    assert_eq!(done.input_below, 0.0);
    assert!(!scene.animating(ms(start, 500)));
    assert_eq!(scene.next_change(ms(start, 500)), Some(ms(start, 5000)));
    // After the timeout the same way back, gently.
    let back = scene.look(ms(start, 5200));
    assert!(back.input_opacity > 0.0 && back.input_opacity < 1.0);
    assert!(scene.animating(ms(start, 5200)));
    let rest = scene.look(ms(start, 5700));
    assert_eq!((rest.clock_opacity, rest.input_opacity), (1.0, 0.0));
    assert!(!scene.animating(ms(start, 5700)));
    // A key while it goes turns it round from where it is.
    let mut turned = scene.clone();
    let before = turned.look(ms(start, 5300)).input_opacity;
    turned.typed(2, ms(start, 5300));
    let after = turned.look(ms(start, 5300)).input_opacity;
    assert!((before - after).abs() < 1e-5, "{before} {after}");
    assert_eq!(turned.look(ms(start, 5800)).input_opacity, 1.0);
  }

  #[test]
  fn without_the_input_the_clock_stays() {
    let start = Instant::now();
    let mut scene = Scene::new(Duration::from_secs(5), false, start);
    scene.typed(1, start);
    let look = scene.look(ms(start, 500));
    assert_eq!((look.clock_opacity, look.input_opacity), (1.0, 0.0));
  }

  #[test]
  fn dots_fade_and_grow_in_and_out() {
    let start = Instant::now();
    let mut scene = Scene::new(Duration::from_secs(5), true, start);
    scene.typed(1, start);
    scene.typed(2, ms(start, 200));
    let look = scene.look(ms(start, 220));
    assert_eq!(look.dots.len(), 2);
    assert_eq!(look.dots[0].presence, 1.0);
    let young = look.dots[1];
    assert!(young.alpha > 0.0 && young.alpha < 1.0);
    assert!(young.scale > DOT_START_SCALE && young.scale < 1.0);
    // Backspace shrinks the newest away quickly.
    scene.deleted(1, ms(start, 400));
    let going = scene.look(ms(start, 450)).dots;
    assert_eq!(going.len(), 2);
    assert!(going[1].alpha < 1.0 && going[0].alpha == 1.0);
    assert_eq!(scene.look(ms(start, 600)).dots.len(), 1);
    // Escape takes them all, a little slower.
    scene.typed(3, ms(start, 700));
    scene.cleared(ms(start, 1000));
    assert_eq!(scene.look(ms(start, 1100)).dots.len(), 3);
    assert!(scene.look(ms(start, 1300)).dots.is_empty());
  }

  #[test]
  fn verifying_breathes_and_a_wrong_password_shakes_and_warms() {
    let start = Instant::now();
    let mut scene = Scene::new(Duration::from_secs(5), true, start);
    scene.typed(4, start);
    scene.verifying(ms(start, 500));
    assert_eq!(scene.look(ms(start, 500)).outline_alpha, 1.0);
    let faint = scene.look(ms(start, 1300)).outline_alpha;
    assert!(faint < 0.4, "{faint}");
    assert!(scene.animating(ms(start, 60_000)), "breathing goes on");
    // Verifying holds the input past the timeout.
    assert_eq!(scene.look(ms(start, 60_000)).input_opacity, 1.0);
    let wrong = ms(start, 60_000);
    scene.denied(wrong);
    let look = scene.look(wrong + Duration::from_millis(30));
    assert!(look.shake.abs() > 1.0 && look.shake.abs() <= 6.0);
    assert!(look.tint > 0.4 && look.tint <= TINT_STRENGTH);
    // The outline comes back from where its breath was, not with a jump.
    assert!(look.outline_alpha < 0.9);
    assert_eq!(look.dots.len(), 4);
    let later = scene.look(wrong + Duration::from_millis(700));
    assert_eq!(later.shake, 0.0);
    assert!(later.dots.is_empty());
    assert_eq!(later.outline_alpha, 1.0);
    assert!(later.tint > 0.0 && later.tint < 0.2);
    let settled = wrong + Duration::from_millis(1200);
    assert_eq!(scene.look(settled).tint, 0.0);
    assert!(!scene.animating(settled));
    // Typing again stops the tint at once.
    scene.typed(1, ms(start, 60_100));
    assert_eq!(scene.look(ms(start, 60_100)).tint, 0.0);
  }

  #[test]
  fn success_fades_everything_out_before_the_unlock() {
    let start = Instant::now();
    let mut scene = Scene::new(Duration::from_secs(5), true, start);
    scene.typed(3, start);
    scene.verifying(ms(start, 600));
    let accepted = ms(start, 900);
    scene.accepted(accepted);
    assert!(!scene.left(accepted));
    assert!(scene.animating(accepted));
    let half = scene.look(accepted + Duration::from_millis(100));
    assert!(half.input_opacity > 0.0 && half.input_opacity < 1.0);
    let end = accepted + LEAVE;
    assert!(scene.left(end));
    assert_eq!(scene.look(end).input_opacity, 0.0);
    assert_eq!(scene.look(end).clock_opacity, 0.0);
    assert!(!scene.animating(end), "still once faded, though verifying");
    // Keys no longer bring anything back.
    scene.typed(4, end);
    assert_eq!(scene.look(end + LEAVE).input_opacity, 0.0);
  }

  fn font() -> FontArc {
    crate::font::bundled()
  }

  fn input_look(dots: usize) -> InputLook {
    InputLook {
      outline_alpha: 1.0,
      tint: 0.0,
      dots: vec![
        DotLook {
          presence: 1.0,
          alpha: 1.0,
          scale: 1.0,
        };
        dots
      ],
      labels: Vec::new(),
    }
  }

  #[test]
  fn the_layout_scales_with_the_output() {
    let full_hd = Layout::new((1920, 1080), None);
    assert_eq!(full_hd.unit, 1.0);
    assert_eq!(full_hd.input, (300.0, 44.0));
    let uhd = Layout::new((3840, 2160), None);
    assert_eq!(uhd.unit, 2.0);
    assert_eq!(uhd.input, (600.0, 88.0));
    assert_eq!(Layout::new((200, 400), None).input.0, 168.0);
    assert_eq!(Layout::new((1920, 1080), Some(30)).unit, 2.0);
  }

  #[test]
  fn the_input_is_a_slim_white_outline_transparent_inside() {
    let layout = Layout::new((1920, 1080), None);
    let drawn = rasterize_input(&layout, &input_look(0), &font());
    let (x0, y0) = drawn.outline_at;
    let raster = &drawn.raster;
    let middle_y = (y0 + 22.0) as u32;
    // The left edge: a pixel and a half of white, then nothing.
    let edge = raster.pixel(x0 as u32, middle_y);
    assert_eq!(edge, [255, 255, 255, 255]);
    let half = raster.pixel(x0 as u32 + 1, middle_y);
    assert!(half[3] > 100 && half[3] < 160, "{half:?}");
    assert_eq!(raster.pixel(x0 as u32 + 3, middle_y), [0, 0, 0, 0]);
    // Transparent inside and outside.
    assert_eq!(raster.pixel(raster.width / 2, middle_y), [0, 0, 0, 0]);
    assert_eq!(raster.pixel(0, 0), [0, 0, 0, 0]);
    // Rounded: the outline's corner pixel is not drawn.
    assert_eq!(raster.pixel(x0 as u32, y0 as u32)[3], 0);
  }

  #[test]
  fn dots_sit_in_the_middle_and_labels_below() {
    let layout = Layout::new((1920, 1080), None);
    let mut look = input_look(3);
    look.labels = vec!["Caps Lock".into(), "English (US)".into()];
    let drawn = rasterize_input(&layout, &look, &font());
    let raster = &drawn.raster;
    let centre_y = (drawn.outline_at.1 + 22.0) as u32;
    let centre_x = raster.width / 2;
    // The middle dot, white and opaque, and the others 18 px each side.
    assert_eq!(raster.pixel(centre_x, centre_y), [255, 255, 255, 255]);
    assert_eq!(raster.pixel(centre_x + 18, centre_y)[3], 255);
    assert_eq!(raster.pixel(centre_x - 18, centre_y)[3], 255);
    assert_eq!(raster.pixel(centre_x + 36, centre_y)[3], 0);
    // The labels: white, semi-transparent, below the outline.
    let below = (drawn.outline_at.1 + 44.0 + 2.0) as u32;
    let label = (below..raster.height)
      .flat_map(|y| (0..raster.width).map(move |x| (x, y)))
      .map(|(x, y)| raster.pixel(x, y))
      .filter(|pixel| pixel[3] > 0)
      .collect::<Vec<_>>();
    assert!(label.len() > 100);
    assert!(label.iter().all(|pixel| pixel[3] <= 160));
    assert!(label.iter().all(|p| p[0] == p[1] && p[1] == p[2]));
  }

  #[test]
  fn a_warm_outline_and_its_breath() {
    let layout = Layout::new((1920, 1080), None);
    let mut look = input_look(0);
    look.tint = 0.5;
    look.outline_alpha = 0.5;
    let drawn = rasterize_input(&layout, &look, &font());
    let (x0, y0) = drawn.outline_at;
    let pixel = drawn.raster.pixel(x0 as u32, (y0 + 22.0) as u32);
    // Premultiplied: half of a warm white.
    assert!(pixel[3] == 128 && pixel[0] > pixel[1] && pixel[1] > pixel[2]);
  }

  #[test]
  fn too_many_dots_show_the_newest() {
    let layout = Layout::new((1920, 1080), None);
    let drawn = rasterize_input(&layout, &input_look(100), &font());
    let raster = &drawn.raster;
    let (x0, y0) = drawn.outline_at;
    let y = (y0 + 22.0) as u32;
    // Nothing on the outline's inner padding.
    for x in x0 as u32 + 3..x0 as u32 + 12 {
      assert_eq!(raster.pixel(x, y)[3], 0, "{x}");
    }
  }

  #[test]
  fn the_clock_is_white_with_a_soft_shadow() {
    let layout = Layout::new((1920, 1080), None);
    let clock =
      rasterize_clock("12:34", "Friday", &layout, &font(), &font()).unwrap();
    assert!(clock.width > 200 && clock.height > 120);
    let pixels = clock.rgba.chunks_exact(4).collect::<Vec<_>>();
    assert!(pixels.iter().any(|p| *p == [255, 255, 255, 255]));
    // The shadow: dark, faint pixels around the text.
    assert!(pixels.iter().any(|p| p[3] > 0 && p[3] < 80 && p[0] == 0));
    let time_only =
      rasterize_clock("12:34", "", &layout, &font(), &font()).unwrap();
    assert!(time_only.height < clock.height);
    assert!(rasterize_clock("", "", &layout, &font(), &font()).is_none());
  }

  #[test]
  fn the_clock_leaves_upwards_and_the_input_rises_into_the_centre() {
    let layout = Layout::new((1920, 1080), None);
    let start = Instant::now();
    let mut scene = Scene::new(Duration::from_secs(5), true, start);
    let clock = Some((401, 200));
    let input = Some(((304, 48), (2.0, 2.0)));
    let (at_rest, none) = placements(&layout, &scene.look(start), clock, input);
    assert_eq!(none, None, "the input does not show at rest");
    let at_rest = at_rest.unwrap();
    assert_eq!(at_rest.at, (760.0, 440.0));
    assert_eq!((at_rest.scale, at_rest.alpha), (1.0, 1.0));
    scene.typed(1, start);
    let moving = scene.look(start + Duration::from_millis(80));
    let (leaving, rising) = placements(&layout, &moving, clock, input);
    let (leaving, rising) = (leaving.unwrap(), rising.unwrap());
    assert!(leaving.at.1 < 440.0 && leaving.scale < 1.0);
    // The outline's centre on the output's, a little lower while it
    // rises.
    let outline_top = rising.at.1 + 2.0;
    assert!(outline_top > 518.0 && outline_top < 518.0 + 28.0);
    let settled = scene.look(start + Duration::from_millis(500));
    let (gone, input_at) = placements(&layout, &settled, clock, input);
    assert_eq!(gone, None);
    assert_eq!(input_at.unwrap().at, (808.0, 516.0));
  }

  #[test]
  fn a_box_blur_keeps_the_sum() {
    let mut from = vec![0.0; 25];
    from[12] = 25.0;
    let mut to = vec![0.0; 25];
    box_blur(&from, &mut to, 5, 5, 1, true);
    assert_eq!(to.iter().sum::<f32>(), 25.0);
    assert!((to[11] - 25.0 / 3.0).abs() < 1e-5);
  }
}
