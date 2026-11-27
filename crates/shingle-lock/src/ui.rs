//! What the lock screen shows over its background: the clock and the
//! password input, as CPU-rasterized, premultiplied RGBA images, and the
//! input's state, fades and highlights in time.
//!
//! At rest only the clock shows. Typing fades in an outlined input with
//! one dot per character; it shows the clear, verify and wrong states in
//! swaylock's per-state colours (docs/design/lock-screen.md) and fades
//! out again after `lock.input_timeout` without a key.

use std::time::{Duration, Instant};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use shingle_config::{InputState, LockColor, LockOptions, StateColors};

/// How long the input takes to fade in or out.
pub const FADE: Duration = Duration::from_millis(180);
/// How long a key or backspace highlight shows.
pub const HIGHLIGHT: Duration = Duration::from_millis(400);

/// A premultiplied RGBA image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raster {
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// `width * height` premultiplied RGBA pixels, row by row.
  pub rgba: Vec<u8>,
}

impl Raster {
  fn new(width: u32, height: u32) -> Self {
    Self {
      width: width.max(1),
      height: height.max(1),
      rgba: vec![0; width.max(1) as usize * height.max(1) as usize * 4],
    }
  }

  /// Blends `color` with `coverage` (0 to 1) over the pixel at (x, y).
  fn blend(&mut self, x: i32, y: i32, color: LockColor, coverage: f32) {
    if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
      return;
    }
    let alpha = f32::from(color[3]) / 255.0 * coverage.clamp(0.0, 1.0);
    if alpha <= 0.0 {
      return;
    }
    let offset = (y as usize * self.width as usize + x as usize) * 4;
    let pixel = &mut self.rgba[offset..offset + 4];
    for channel in 0..3 {
      let source = f32::from(color[channel]) * alpha;
      let destination = f32::from(pixel[channel]) * (1.0 - alpha);
      pixel[channel] = (source + destination).round().min(255.0) as u8;
    }
    let destination = f32::from(pixel[3]) * (1.0 - alpha);
    pixel[3] = (alpha * 255.0 + destination).round().min(255.0) as u8;
  }

  /// The premultiplied pixel at (x, y).
  #[cfg(test)]
  pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
    let offset = (y as usize * self.width as usize + x as usize) * 4;
    self.rgba[offset..offset + 4].try_into().unwrap()
  }
}

/// Where things go on one output, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
  /// The output's size.
  pub size: (u32, u32),
  /// The clock's top edge; it is centred horizontally.
  pub clock_top: i32,
  /// The input's rectangle: x, y, width, height.
  pub input: (i32, i32, u32, u32),
  /// The pixel sizes of the time, the date and the input's text.
  pub text_sizes: (f32, f32, f32),
}

impl Layout {
  /// The layout of an output of `size` for `options`. The clock keeps
  /// its place whether the input shows or not.
  pub fn new(size: (u32, u32), options: &LockOptions) -> Self {
    let radius = options.indicator_radius as f32;
    let (time, date, text) = match options.font_size {
      Some(size) => (size as f32 * 2.4, size as f32, size as f32),
      None => (radius * 1.3, radius * 0.42, radius * 0.36),
    };
    let height = radius.round().max(8.0) as u32;
    let width = ((radius * 6.0) as u32)
      .min(size.0.saturating_sub(32))
      .max(height * 2);
    let clock_height = if options.clock {
      (time * 1.2 + date * 1.4) as u32
    } else {
      0
    };
    let gap = if options.clock {
      (radius * 0.6) as u32
    } else {
      0
    };
    let total = clock_height + gap + height;
    let top = (size.1.saturating_sub(total) / 2) as i32;
    Self {
      size,
      clock_top: top,
      input: (
        (size.0.saturating_sub(width) / 2) as i32,
        top + (clock_height + gap) as i32,
        width,
        height,
      ),
      text_sizes: (time, date, text),
    }
  }

  /// The input's border width: half of `indicator-thickness`, at least
  /// 1 and at most a quarter of its height.
  pub fn border(&self, options: &LockOptions) -> f32 {
    (options.indicator_thickness as f32 / 2.0)
      .clamp(1.0, (self.input.3 as f32 / 4.0).max(1.0))
  }
}

/// A highlight on the input's border after a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Highlight {
  /// A character was typed; `key-hl-color` under the newest dot.
  Key,
  /// One was deleted; `bs-hl-color` where it was.
  Backspace,
}

/// The input's state in time.
#[derive(Clone, Debug)]
pub struct Indicator {
  state: InputState,
  /// Characters typed.
  chars: usize,
  /// Failed attempts so far.
  attempts: u32,
  /// The last key, PAM answer or other change; the input fades out
  /// `timeout` after it.
  last_activity: Option<Instant>,
  /// When the input last started to show after being hidden.
  shown_since: Option<Instant>,
  highlight: Option<(Highlight, Instant)>,
  timeout: Duration,
  always: bool,
}

impl Indicator {
  /// A hidden input that fades out `timeout` after the last key, or that
  /// always shows.
  pub fn new(timeout: Duration, always: bool, now: Instant) -> Self {
    Self {
      state: InputState::Input,
      chars: 0,
      attempts: 0,
      last_activity: None,
      shown_since: always.then_some(now),
      highlight: None,
      timeout,
      always,
    }
  }

  /// The state shown.
  pub fn state(&self) -> InputState {
    self.state
  }

  /// Failed attempts so far.
  pub fn attempts(&self) -> u32 {
    self.attempts
  }

  fn touch(&mut self, now: Instant) {
    if self.opacity(now) <= 0.0 {
      self.shown_since = Some(now);
    }
    self.last_activity = Some(now);
  }

  /// A character was typed; `chars` are typed now.
  pub fn typed(&mut self, chars: usize, now: Instant) {
    self.touch(now);
    self.state = InputState::Input;
    self.chars = chars;
    self.highlight = Some((Highlight::Key, now));
  }

  /// One was deleted; `chars` are left. Deleting the last one clears.
  pub fn deleted(&mut self, chars: usize, now: Instant) {
    self.touch(now);
    self.chars = chars;
    if chars == 0 {
      self.state = InputState::Clear;
      self.highlight = None;
    } else {
      self.state = InputState::Input;
      self.highlight = Some((Highlight::Backspace, now));
    }
  }

  /// Everything typed was cleared (Escape).
  pub fn cleared(&mut self, now: Instant) {
    self.touch(now);
    self.chars = 0;
    self.state = InputState::Clear;
    self.highlight = None;
  }

  /// PAM checks the password.
  pub fn verifying(&mut self, now: Instant) {
    self.touch(now);
    self.state = InputState::Verify;
    self.highlight = None;
  }

  /// PAM refused it; the password is gone.
  pub fn denied(&mut self, now: Instant) {
    self.touch(now);
    self.attempts += 1;
    self.chars = 0;
    self.state = InputState::Wrong;
  }

  /// A key that is not typed, such as a modifier, shows the input
  /// without changing it.
  pub fn poked(&mut self, now: Instant) {
    self.touch(now);
  }

  /// How visible the input is at `now`, from 0 to 1.
  pub fn opacity(&self, now: Instant) -> f32 {
    if self.always || self.state == InputState::Verify {
      return 1.0;
    }
    let Some(last) = self.last_activity else {
      return 0.0;
    };
    let shown = self.shown_since.unwrap_or(last);
    let fading_in = fraction(now.saturating_duration_since(shown), FADE);
    let idle = now.saturating_duration_since(last);
    let fading_out = 1.0 - fraction(idle.saturating_sub(self.timeout), FADE);
    fading_in.min(fading_out)
  }

  /// The highlight shown at `now`, if any.
  pub fn highlight(&self, now: Instant) -> Option<Highlight> {
    self
      .highlight
      .filter(|(_, since)| now.saturating_duration_since(*since) < HIGHLIGHT)
      .map(|(highlight, _)| highlight)
  }

  /// Whether the input is fading at `now`, which wants frames at the
  /// display's rate.
  pub fn animating(&self, now: Instant) -> bool {
    let opacity = self.opacity(now);
    opacity > 0.0 && opacity < 1.0
  }

  /// When what [`Indicator::opacity`] or [`Indicator::highlight`] return
  /// changes next after `now`, if it does without input.
  pub fn next_change(&self, now: Instant) -> Option<Instant> {
    let mut next: Option<Instant> = None;
    let mut consider = |at: Instant| {
      if at > now {
        next = Some(next.map_or(at, |next| next.min(at)));
      }
    };
    if let Some((_, since)) = self.highlight {
      consider(since + HIGHLIGHT);
    }
    if !self.always
      && self.state != InputState::Verify
      && let Some(last) = self.last_activity
    {
      consider(last + self.timeout);
    }
    next
  }

  /// What the input shows inside: dots, or a message.
  pub fn content(&self, show_attempts: bool) -> Content {
    let attempts = |text: &str| {
      if show_attempts && self.attempts > 0 {
        format!("{text} ({})", self.attempts)
      } else {
        text.to_owned()
      }
    };
    match self.state {
      InputState::Verify => Content::Message("Verifying…".into()),
      InputState::Wrong => Content::Message(attempts("Wrong password")),
      InputState::Clear => Content::Message("Cleared".into()),
      InputState::Input if self.chars == 0 => {
        if show_attempts && self.attempts > 0 {
          let plural = if self.attempts == 1 { "" } else { "s" };
          Content::Message(format!("{} failed attempt{plural}", self.attempts))
        } else {
          Content::Dots(0)
        }
      }
      InputState::Input => Content::Dots(self.chars),
    }
  }
}

/// What the input shows inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
  /// One dot per character typed.
  Dots(usize),
  /// A message, such as `Wrong password`.
  Message(String),
}

fn fraction(elapsed: Duration, of: Duration) -> f32 {
  (elapsed.as_secs_f32() / of.as_secs_f32()).clamp(0.0, 1.0)
}

/// The signed distance from (x, y) to a rounded rectangle centred at
/// (cx, cy) with half extents (hw, hh) and corner `radius`; negative
/// inside.
fn rounded_rect_distance(
  (x, y): (f32, f32),
  (cx, cy): (f32, f32),
  (hw, hh): (f32, f32),
  radius: f32,
) -> f32 {
  let qx = (x - cx).abs() - (hw - radius);
  let qy = (y - cy).abs() - (hh - radius);
  let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
  outside + qx.max(qy).min(0.0) - radius
}

/// Rasterizes the input of `layout` in `colors` with `content` and the
/// highlight `highlight`, at its own size: the inside colour filling a
/// rounded rectangle, the ring colour as its border, the line colour as
/// the border's inner edge, the text colour for the dots and the
/// message, and a highlight segment on the bottom border with separator
/// coloured ends.
pub fn rasterize_input(
  layout: &Layout,
  options: &LockOptions,
  colors: StateColors,
  content: &Content,
  highlight: Option<Highlight>,
  font: &FontArc,
) -> Raster {
  let (_, _, width, height) = layout.input;
  let mut raster = Raster::new(width, height);
  let border = layout.border(options);
  let (w, h) = (width as f32, height as f32);
  let centre = (w / 2.0, h / 2.0);
  let corner = (h * 0.3).max(border + 1.0);
  for y in 0..height {
    for x in 0..width {
      let point = (x as f32 + 0.5, y as f32 + 0.5);
      let distance =
        rounded_rect_distance(point, centre, (w / 2.0, h / 2.0), corner);
      let coverage = (0.5 - distance).clamp(0.0, 1.0);
      if coverage <= 0.0 {
        continue;
      }
      let depth = -distance;
      let color = if depth < border {
        colors.ring
      } else if depth < border + 1.0 {
        colors.line
      } else {
        colors.inside
      };
      raster.blend(x as i32, y as i32, color, coverage);
    }
  }
  let inner = w - 2.0 * (border + h * 0.2);
  match content {
    Content::Dots(count) => {
      let diameter = (h * 0.22).max(3.0);
      let spacing = diameter * 1.9;
      let fitting = ((inner / spacing).floor() as usize).max(1);
      let shown = (*count).min(fitting);
      let start = centre.0 - spacing * (shown as f32 - 1.0) / 2.0;
      for index in 0..shown {
        let cx = start + spacing * index as f32;
        fill_circle(&mut raster, (cx, centre.1), diameter / 2.0, colors.text);
      }
      if let Some(highlight) = highlight {
        let color = match highlight {
          Highlight::Key => options.colors.key_highlight,
          Highlight::Backspace => options.colors.backspace_highlight,
        };
        // Under the newest dot, or where the deleted one was.
        let index = match highlight {
          Highlight::Key => shown.saturating_sub(1),
          Highlight::Backspace => shown,
        };
        let cx = start + spacing * index as f32;
        let half = spacing * 0.75;
        draw_highlight(
          &mut raster,
          (cx - half, cx + half),
          border,
          color,
          options.colors.separator,
        );
      }
    }
    Content::Message(text) => {
      let text_raster =
        rasterize_text(text, layout.text_sizes.2, colors.text, font);
      let x = ((w - text_raster.width as f32) / 2.0).round() as i32;
      let y = ((h - text_raster.height as f32) / 2.0).round() as i32;
      composite(&mut raster, &text_raster, (x, y));
    }
  }
  raster
}

/// A highlight on the bottom border from `x0` to `x1`, with separator
/// ends 2 pixels wide.
fn draw_highlight(
  raster: &mut Raster,
  (x0, x1): (f32, f32),
  border: f32,
  color: LockColor,
  separator: LockColor,
) {
  let bottom = raster.height as i32;
  let top = bottom - border.ceil() as i32;
  for y in top.max(0)..bottom {
    for x in x0.floor() as i32..x1.ceil() as i32 {
      // Only where the border itself is drawn.
      let offset = (y.max(0) as usize * raster.width as usize
        + x.clamp(0, raster.width as i32 - 1) as usize)
        * 4;
      if raster.rgba.get(offset + 3).is_none_or(|alpha| *alpha == 0) {
        continue;
      }
      let end = (x as f32) < x0 + 2.0 || (x as f32) >= x1 - 2.0;
      raster.blend(x, y, if end { separator } else { color }, 1.0);
    }
  }
}

fn fill_circle(
  raster: &mut Raster,
  (cx, cy): (f32, f32),
  radius: f32,
  color: LockColor,
) {
  let x0 = (cx - radius - 1.0).floor() as i32;
  let x1 = (cx + radius + 1.0).ceil() as i32;
  let y0 = (cy - radius - 1.0).floor() as i32;
  let y1 = (cy + radius + 1.0).ceil() as i32;
  for y in y0..y1 {
    for x in x0..x1 {
      let dx = x as f32 + 0.5 - cx;
      let dy = y as f32 + 0.5 - cy;
      let distance = (dx * dx + dy * dy).sqrt() - radius;
      raster.blend(x, y, color, (0.5 - distance).clamp(0.0, 1.0));
    }
  }
}

/// Draws `source` over `target` with its top-left corner at `at`.
pub fn composite(target: &mut Raster, source: &Raster, (x0, y0): (i32, i32)) {
  for y in 0..source.height {
    for x in 0..source.width {
      let (tx, ty) = (x0 + x as i32, y0 + y as i32);
      if tx < 0
        || ty < 0
        || tx >= target.width as i32
        || ty >= target.height as i32
      {
        continue;
      }
      let from = (y as usize * source.width as usize + x as usize) * 4;
      let to = (ty as usize * target.width as usize + tx as usize) * 4;
      let alpha = f32::from(source.rgba[from + 3]) / 255.0;
      for channel in 0..4 {
        let value = f32::from(source.rgba[from + channel])
          + f32::from(target.rgba[to + channel]) * (1.0 - alpha);
        target.rgba[to + channel] = value.round().min(255.0) as u8;
      }
    }
  }
}

/// One line of `text` at `size` pixels in `color`, tightly around its
/// line height.
pub fn rasterize_text(
  text: &str,
  size: f32,
  color: LockColor,
  font: &FontArc,
) -> Raster {
  let scaled = font.as_scaled(PxScale::from(size.max(1.0)));
  let mut width = 0.0;
  let mut previous = None;
  for character in text.chars() {
    let id = scaled.glyph_id(character);
    if let Some(previous) = previous {
      width += scaled.kern(previous, id);
    }
    width += scaled.h_advance(id);
    previous = Some(id);
  }
  let height = scaled.height().ceil() as u32;
  let mut raster = Raster::new(width.ceil() as u32 + 2, height + 2);
  let baseline = 1.0 + scaled.ascent();
  let mut x = 1.0;
  let mut previous = None;
  for character in text.chars() {
    let id = scaled.glyph_id(character);
    if let Some(previous) = previous {
      x += scaled.kern(previous, id);
    }
    let mut glyph = scaled.scaled_glyph(character);
    glyph.position = point(x, baseline);
    if let Some(outlined) = font.outline_glyph(glyph) {
      let bounds = outlined.px_bounds();
      outlined.draw(|gx, gy, coverage| {
        raster.blend(
          bounds.min.x as i32 + gx as i32,
          bounds.min.y as i32 + gy as i32,
          color,
          coverage,
        );
      });
    }
    x += scaled.h_advance(id);
    previous = Some(id);
  }
  raster
}

/// The clock: `time` over `date`, centred, in `color`, with a soft dark
/// shadow so that it reads over any background. Empty lines are left
/// out; `None` when both are.
pub fn rasterize_clock(
  time: &str,
  date: &str,
  layout: &Layout,
  color: LockColor,
  font: &FontArc,
) -> Option<Raster> {
  let lines = [(time, layout.text_sizes.0), (date, layout.text_sizes.1)]
    .into_iter()
    .filter(|(text, _)| !text.is_empty())
    .map(|(text, size)| rasterize_text(text, size, color, font))
    .collect::<Vec<_>>();
  if lines.is_empty() {
    return None;
  }
  let width = lines.iter().map(|line| line.width).max().unwrap_or(1);
  let height = lines.iter().map(|line| line.height).sum::<u32>();
  let shadow_offset = 2;
  let mut raster = Raster::new(width + shadow_offset, height + shadow_offset);
  let mut y = 0;
  for line in &lines {
    let x = ((width - line.width) / 2) as i32;
    let mut shadow = line.clone();
    for pixel in shadow.rgba.chunks_exact_mut(4) {
      let alpha = pixel[3] / 2;
      pixel.copy_from_slice(&[0, 0, 0, alpha]);
    }
    composite(&mut raster, &shadow, (x + shadow_offset as i32, y + 2));
    composite(&mut raster, line, (x, y));
    y += line.height as i32;
  }
  Some(raster)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn font() -> FontArc {
    crate::font::bundled()
  }

  fn at(start: Instant, millis: u64) -> Instant {
    start + Duration::from_millis(millis)
  }

  #[test]
  fn the_input_fades_in_on_typing_and_out_after_the_timeout() {
    let start = Instant::now();
    let mut indicator = Indicator::new(Duration::from_secs(2), false, start);
    assert_eq!(indicator.opacity(start), 0.0, "only the clock at rest");
    assert_eq!(indicator.next_change(start), None);
    indicator.typed(1, at(start, 100));
    assert_eq!(indicator.opacity(at(start, 100)), 0.0);
    assert!(indicator.animating(at(start, 190)));
    assert_eq!(indicator.opacity(at(start, 400)), 1.0);
    assert_eq!(indicator.highlight(at(start, 400)), Some(Highlight::Key));
    assert_eq!(indicator.highlight(at(start, 600)), None);
    // Fully shown until the timeout, then fading out, then gone.
    assert_eq!(indicator.opacity(at(start, 2100)), 1.0);
    assert!(indicator.animating(at(start, 2190)));
    assert_eq!(indicator.opacity(at(start, 2400)), 0.0);
    assert_eq!(
      indicator.next_change(at(start, 1000)),
      Some(at(start, 2100))
    );
    // Typing again while it fades out does not fade it in from zero.
    indicator.typed(2, at(start, 2190));
    assert!(indicator.opacity(at(start, 2200)) > 0.4);
  }

  #[test]
  fn states_follow_keys_and_pam() {
    let start = Instant::now();
    let mut indicator = Indicator::new(Duration::from_secs(5), false, start);
    indicator.typed(1, start);
    indicator.typed(2, start);
    assert_eq!(indicator.content(true), Content::Dots(2));
    indicator.deleted(1, start);
    assert_eq!(indicator.state(), InputState::Input);
    assert_eq!(indicator.highlight(start), Some(Highlight::Backspace));
    indicator.deleted(0, start);
    assert_eq!(indicator.state(), InputState::Clear);
    assert_eq!(indicator.content(true), Content::Message("Cleared".into()));
    indicator.typed(1, start);
    indicator.verifying(start);
    assert_eq!(indicator.state(), InputState::Verify);
    // Verifying never fades out.
    assert_eq!(indicator.opacity(at(start, 60_000)), 1.0);
    indicator.denied(start);
    assert_eq!(indicator.state(), InputState::Wrong);
    assert_eq!(
      indicator.content(true),
      Content::Message("Wrong password (1)".into())
    );
    assert_eq!(
      indicator.content(false),
      Content::Message("Wrong password".into())
    );
    indicator.typed(1, start);
    indicator.cleared(start);
    indicator.typed(0, start);
    assert_eq!(
      indicator.content(true),
      Content::Message("1 failed attempt".into())
    );
  }

  #[test]
  fn an_always_shown_input_never_fades() {
    let start = Instant::now();
    let indicator = Indicator::new(Duration::from_secs(1), true, start);
    assert_eq!(indicator.opacity(at(start, 10_000)), 1.0);
    assert_eq!(indicator.next_change(start), None);
  }

  #[test]
  fn the_layout_keeps_the_clock_and_centres_the_input() {
    let options = LockOptions {
      clock: true,
      ..LockOptions::default()
    };
    let layout = Layout::new((1000, 800), &options);
    let (x, y, width, height) = layout.input;
    assert_eq!((width, height), (300, 50));
    assert_eq!(x, 350);
    assert!(y > layout.clock_top + layout.text_sizes.0 as i32);
    assert_eq!(layout.border(&options), 5.0);
    let without = Layout::new((1000, 800), &LockOptions::default());
    assert_eq!(without.input.1, (800 - 50) / 2);
    // A narrow output keeps a margin.
    assert_eq!(Layout::new((200, 400), &options).input.2, 168);
  }

  #[test]
  fn the_input_uses_its_state_colours() {
    let options = LockOptions::default();
    let layout = Layout::new((1000, 800), &options);
    let colors = StateColors {
      ring: [255, 0, 0, 255],
      inside: [0, 0, 255, 255],
      line: [0, 255, 0, 255],
      text: [255, 255, 255, 255],
    };
    let raster = rasterize_input(
      &layout,
      &options,
      colors,
      &Content::Dots(1),
      Some(Highlight::Key),
      &font(),
    );
    let (_, _, width, height) = layout.input;
    assert_eq!((raster.width, raster.height), (width, height));
    // The left border, the line inside it, the inside, the dot.
    assert_eq!(raster.pixel(2, height / 2), [255, 0, 0, 255]);
    assert_eq!(raster.pixel(5, height / 2), [0, 255, 0, 255]);
    assert_eq!(raster.pixel(20, height / 2), [0, 0, 255, 255]);
    assert_eq!(raster.pixel(width / 2, height / 2), [255, 255, 255, 255]);
    // The key highlight under the dot, with separator ends.
    let hl = options.colors.key_highlight;
    assert_eq!(raster.pixel(width / 2, height - 2)[..3], hl[..3]);
    // A corner stays transparent.
    assert_eq!(raster.pixel(0, 0), [0, 0, 0, 0]);
  }

  #[test]
  fn text_and_the_clock_are_drawn_in_their_colour() {
    let options = LockOptions {
      clock: true,
      ..LockOptions::default()
    };
    let layout = Layout::new((1000, 800), &options);
    let clock =
      rasterize_clock("12:34", "Mon", &layout, [255, 0, 0, 255], &font())
        .unwrap();
    assert!(clock.width > 100 && clock.height > 60);
    assert!(clock.rgba.chunks_exact(4).any(|p| p == [255, 0, 0, 255]));
    assert!(rasterize_clock("", "", &layout, [255; 4], &font()).is_none());
  }
}
