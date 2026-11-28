//! Open, close, minimize and restore animations, Wayfire's animate
//! plugin (wayfire:plugins/animate/animate.cpp) over the effects of
//! [`crate::animate`], and the end of Scale's exit.

use std::time::Instant;

use shingle::{
  BufferSnapshot, ShellLayer, SurfaceId, SurfaceRole, SurfaceSnapshot,
};
use shingle_config::{AnimationDuration, ViewProperties};
use shingle_core::{Rect, WindowId as TileWindowId};
use shingle_render::{GpuTexture, RenderRect, RenderTile, TileEffect};

use super::{
  Frontend,
  whole_view::{AnimatedView, ViewImage, ViewPieces},
};
use crate::animate::{Animation, Effect, FireOptions, Kind, Timing, ViewInfo};

pub(super) struct GeometryAnimation {
  pub(super) started: Instant,
  pub(super) old_geometry: RenderRect,
  pub(super) old_texture: GpuTexture,
}

/// A view that went, drawn from its last frame while it animates away:
/// closing or minimizing.
pub(super) struct ClosingAnimation {
  /// The window of a toplevel.
  pub(super) window: Option<TileWindowId>,
  /// The output it is drawn on, where its surface was drawn last.
  pub(super) output: shingle::OutputHandle,
  /// Where its root surface was drawn last, and its buffer.
  pub(super) geometry: RenderRect,
  pub(super) buffer: BufferSnapshot,
  /// Where the view was.
  pub(super) view: AnimatedView,
  /// What is drawn of it.
  pub(super) image: ClosingImage,
  pub(super) animation: Animation,
}

/// What a closing or minimizing view draws: its root surface's last
/// buffer, or for a view of more pieces the texture its last frame is
/// drawn into, as Wayfire's animate draws a snapshot of the view taken
/// when it unmaps (wayfire:plugins/animate/unmapped-view-node.hpp,
/// wayfire:src/view/view.cpp:94-116, `take_snapshot`).
pub(super) enum ClosingImage {
  /// The root surface alone, from its texture.
  Surface,
  /// The pieces, until a frame of the output draws them into a texture.
  Pending(Box<ViewPieces>),
  /// The texture.
  Drawn(ViewImage),
}

impl ClosingAnimation {
  /// The animation of the view of a root surface last drawn at `geometry`
  /// with `buffer`, of `window`, on `output`; `pieces` are the view's
  /// when it is more than that surface.
  pub(super) fn new(
    (window, output): (Option<TileWindowId>, shingle::OutputHandle),
    (geometry, buffer): (RenderRect, BufferSnapshot),
    animation: Animation,
    pieces: Option<ViewPieces>,
  ) -> Self {
    let view = pieces
      .as_ref()
      .and_then(ViewPieces::view)
      .unwrap_or_else(|| AnimatedView::surface(rect_of(geometry)));
    let image = match pieces {
      Some(pieces) if pieces.is_whole() => {
        ClosingImage::Pending(Box::new(pieces))
      }
      _ => ClosingImage::Surface,
    };
    Self {
      window,
      output,
      geometry,
      buffer,
      view,
      image,
      animation,
    }
  }

  /// Whether the view's pieces are still to be drawn, and the frame title
  /// of `window` with them.
  pub(super) fn draws_title_of(&self, window: TileWindowId) -> bool {
    matches!(&self.image, ClosingImage::Pending(pieces)
      if pieces.frame.as_ref().is_some_and(|frame| frame.window == window))
  }
}

/// Which of the three animations of a view: `open`, `close` or
/// `minimize`, the prefix of its options and view properties.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Which {
  Open,
  Close,
  Minimize,
}

impl Which {
  fn index(self) -> usize {
    self as usize
  }

  fn name(self) -> &'static str {
    match self {
      Self::Open => "open",
      Self::Close => "close",
      Self::Minimize => "minimize",
    }
  }
}

/// What the `[animate]` matchers said of a surface when it was last
/// drawn, for choosing its animation after it went, and its window's view
/// properties, read when the animation is chosen.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ViewMatches {
  /// `enabled_for`.
  pub(super) enabled: bool,
  /// `fade_enabled_for`.
  pub(super) fade: bool,
  /// `zoom_enabled_for`.
  pub(super) zoom: bool,
  /// `fire_enabled_for`.
  pub(super) fire: bool,
  /// The `open-`, `close-` and `minimize-animation-type` view properties.
  pub(super) types: [Option<String>; 3],
  /// The `*-animation-duration` view properties, as set.
  pub(super) durations: [Option<String>; 3],
}

impl ViewMatches {
  /// Whether no matcher matched, so the surface need not be kept.
  pub(super) fn is_empty(&self) -> bool {
    !(self.enabled || self.fade || self.zoom || self.fire)
  }
}

impl Frontend {
  /// Expo's zoom: `[expo] duration`.
  pub(super) fn expo_timing(&self) -> Timing {
    Timing::new(self.expo_config.duration_ms, self.expo_config.easing)
  }

  /// Scale's transitions: `[scale] duration`.
  pub(super) fn scale_timing(&self) -> Timing {
    Timing::new(self.scale_config.duration_ms, self.scale_config.easing)
  }

  /// Vswitch's slide: `[vswitch] duration`.
  pub(super) fn vswitch_timing(&self) -> Timing {
    Timing::new(self.vswitch.duration_ms, self.vswitch.easing)
  }

  /// Wayfire's name for the type of the view `snapshot` shows
  /// (wayfire:src/core/view-access-interface.cpp:101-145); `None` for a
  /// Wayland drag icon or a subsurface, which are no views: a subsurface
  /// animates with its view.
  fn view_type(snapshot: &SurfaceSnapshot) -> Option<&'static str> {
    if snapshot.role == SurfaceRole::Subsurface {
      return None;
    }
    // A Wayland client's drag icon, which has no X class, is no view in
    // Wayfire and never animates.
    if snapshot.role == SurfaceRole::DragIcon && snapshot.app_id.is_none() {
      return None;
    }
    Some(match snapshot.role {
      SurfaceRole::Toplevel => "toplevel",
      SurfaceRole::Popup => "popup",
      // Wayfire names layer views after their layer
      // (wayfire:src/core/view-access-interface.cpp:131-141).
      SurfaceRole::LayerShell => match snapshot.layer {
        Some(ShellLayer::Background | ShellLayer::Bottom) => "background",
        Some(ShellLayer::Top) => "panel",
        _ => "overlay",
      },
      SurfaceRole::Cursor => "cursor",
      // Wayfire's name for override-redirect X views, which its drag
      // icons are too.
      SurfaceRole::Unmanaged | SurfaceRole::DragIcon => "x-or",
      _ => "other",
    })
  }

  /// What the four `[animate]` matchers say of `snapshot`.
  pub(super) fn view_matches(&self, snapshot: &SurfaceSnapshot) -> ViewMatches {
    let Some(view_type) = Self::view_type(snapshot) else {
      return ViewMatches::default();
    };
    let mut view = ViewProperties {
      app_id: snapshot.app_id.as_deref().unwrap_or(""),
      title: snapshot.title.as_deref().unwrap_or(""),
      view_type,
      activated: self.compositor.focused_window() == snapshot.window,
      focusable: snapshot.role == SurfaceRole::Toplevel,
      mapped: snapshot.buffer.is_some(),
      ..ViewProperties::default()
    };
    if let Some(window) = snapshot.window
      && let Ok(info) = self.compositor.core().window_info(window)
    {
      view.fullscreen = info.state.fullscreen;
      view.minimized = info.state.minimized;
      view.maximized = info.state.maximized;
      view.floating = info.state.floating.is_some() && !info.state.maximized;
      view.tiled_left = !view.floating;
      view.tiled_right = !view.floating;
      view.tiled_top = !view.floating;
      view.tiled_bottom = !view.floating;
    }
    ViewMatches {
      enabled: self.animate.enabled_for.matches_view(&view),
      fade: self.animate.fade_enabled_for.matches_view(&view),
      zoom: self.animate.zoom_enabled_for.matches_view(&view),
      fire: self.animate.fire_enabled_for.matches_view(&view),
      ..ViewMatches::default()
    }
  }

  /// `matches` with `window`'s `*-animation-type` and
  /// `*-animation-duration` view properties as they are now.
  pub(super) fn with_properties(
    &self,
    mut matches: ViewMatches,
    window: Option<TileWindowId>,
  ) -> ViewMatches {
    let Some(window) = window else {
      return matches;
    };
    for which in [Which::Open, Which::Close, Which::Minimize] {
      let name = which.name();
      let property = |kind: &str| {
        self
          .compositor
          .view_string_property(window, &format!("{name}-animation-{kind}"))
          .map(str::to_owned)
      };
      matches.types[which.index()] = property("type");
      matches.durations[which.index()] = property("duration");
    }
    matches
  }

  /// Whether `[animate] enabled_for` matches `snapshot`.
  #[cfg(test)]
  pub(super) fn animation_matches(&self, snapshot: &SurfaceSnapshot) -> bool {
    self.view_matches(snapshot).enabled
  }

  /// The animation `which` of a view `matches` describes, and its length,
  /// as Wayfire's `get_animation_for_view` chooses them
  /// (wayfire:plugins/animate/animate.cpp:316-390): the option's own
  /// animation where `enabled_for` matches, then fade, zoom and fire where
  /// their matchers do, each with its own duration, and the view's
  /// properties over all of them. The name is `none` for no animation.
  pub(super) fn choose_animation(
    &self,
    which: Which,
    matches: &ViewMatches,
  ) -> (String, AnimationDuration) {
    let config = &self.animate;
    let mut name = "none".to_owned();
    let mut duration = None;
    if matches.enabled {
      name = match which {
        Which::Open => config.open_animation.clone(),
        Which::Close => config.close_animation.clone(),
        Which::Minimize => config.minimize_animation.clone(),
      };
    }
    for (matched, effect, ms, easing) in [
      (
        matches.fade,
        "fade",
        config.fade_duration_ms,
        config.fade_easing,
      ),
      (
        matches.zoom,
        "zoom",
        config.zoom_duration_ms,
        config.zoom_easing,
      ),
      (
        matches.fire,
        "fire",
        config.fire_duration_ms,
        config.fire_easing,
      ),
    ] {
      if matched {
        name = effect.to_owned();
        duration = Some(AnimationDuration { ms, easing });
      }
    }
    if let Some(chosen) = &matches.types[which.index()] {
      name = chosen.clone();
    }
    if let Some(text) = &matches.durations[which.index()] {
      match AnimationDuration::parse(text) {
        Ok(chosen) => duration = Some(chosen),
        Err(_) => {
          eprintln!("shingle: invalid animation duration property {text:?}")
        }
      }
    }
    let none = AnimationDuration {
      ms: 0,
      easing: shingle_config::Easing::Circle,
    };
    if name == "none" {
      return (name, none);
    }
    if Effect::from_name(&name).is_none() {
      eprintln!("shingle: unknown animation type {name:?}");
      return ("none".to_owned(), none);
    }
    let duration = duration.unwrap_or_else(|| config.default_duration(&name));
    (name, duration)
  }

  /// What an animation of the view at `geometry` of `window` starts from.
  pub(super) fn view_info(
    &self,
    surface: SurfaceId,
    window: Option<TileWindowId>,
    geometry: Rect,
  ) -> ViewInfo {
    let center = (
      f64::from(geometry.x) + f64::from(geometry.width) / 2.0,
      f64::from(geometry.y) + f64::from(geometry.height) / 2.0,
    );
    let output = self.output_under(center.0, center.1);
    ViewInfo {
      geometry,
      output: self.output_rect(&output),
      minimize_hint: window
        .and_then(|window| self.compositor.minimize_hint(window)),
      spin_rotations: self.animate.spin_rotations,
      fire: FireOptions::of(&self.animate),
      seed: surface.get() ^ 0x5eed,
    }
  }

  /// Starts the open animation of `snapshot`, a surface that showed its
  /// first buffer (`on_view_mapped`,
  /// wayfire:plugins/animate/animate.cpp:427-457).
  pub(super) fn start_open_animation(&mut self, snapshot: &SurfaceSnapshot) {
    if self.open_animations.contains_key(&snapshot.id) {
      return;
    }
    // A view minimized before it mapped is not shown at all.
    if let Some(window) = snapshot.window
      && self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| info.state.minimized)
    {
      return;
    }
    let matches =
      self.with_properties(self.view_matches(snapshot), snapshot.window);
    let (name, duration) = self.choose_animation(Which::Open, &matches);
    let Some(effect) = Effect::from_name(&name) else {
      return;
    };
    let geometry = self.framed_geometry(snapshot);
    let info = self.view_info(snapshot.id, snapshot.window, geometry);
    let animation = Animation::start(
      effect,
      Kind::Map,
      Timing::new(duration.ms, duration.easing),
      Instant::now(),
      &info,
    );
    self.open_animations.insert(snapshot.id, animation);
  }

  /// The close animation of `surface`, which went, drawn last at
  /// `geometry` (`on_view_pre_unmap`,
  /// wayfire:plugins/animate/animate.cpp:459-465). An open animation of
  /// the same effect still running turns around instead.
  pub(super) fn close_animation(
    &mut self,
    surface: SurfaceId,
    window: Option<TileWindowId>,
    geometry: RenderRect,
    pieces: Option<&ViewPieces>,
  ) -> Option<Animation> {
    let matches = self.with_properties(self.view_matches_of(surface), window);
    let (name, duration) = self.choose_animation(Which::Close, &matches);
    let effect = Effect::from_name(&name)?;
    let now = Instant::now();
    if let Some(mut opening) = self.open_animations.remove(&surface)
      && opening.effect == effect
      && opening.kind == Kind::Map
    {
      opening.set_kind(Kind::Unmap, now);
      return Some(opening);
    }
    let view = pieces
      .and_then(ViewPieces::view)
      .unwrap_or_else(|| AnimatedView::surface(rect_of(geometry)));
    let info = self.view_info(surface, window, view.geometry);
    Some(Animation::start(
      effect,
      Kind::Unmap,
      Timing::new(duration.ms, duration.easing),
      now,
      &info,
    ))
  }

  /// What the matchers said of `surface` when it was last drawn; none
  /// matched if it was not.
  pub(super) fn view_matches_of(&self, surface: SurfaceId) -> ViewMatches {
    self
      .scenes
      .values()
      .find_map(|scene| scene.animation_enabled.get(&surface).cloned())
      .unwrap_or_default()
  }

  /// The minimize animation's effect and length for a view `matches`
  /// describes: Wayfire runs `minimize_animation` itself whatever was
  /// chosen, with the chosen length
  /// (wayfire:plugins/animate/animate.cpp:477-480); a view nothing
  /// chose gets no length and so no animation.
  fn minimize_choice(&self, matches: &ViewMatches) -> Option<(Effect, Timing)> {
    let (name, duration) = self.choose_animation(Which::Minimize, matches);
    if name == "none" {
      return None;
    }
    let effect = Effect::from_name(&self.animate.minimize_animation)?;
    Some((effect, Timing::new(duration.ms, duration.easing)))
  }

  /// Animates `window` minimizing or being restored
  /// (`on_minimize_request`, wayfire:plugins/animate/animate.cpp:467-483).
  pub(super) fn minimize_window_changed(
    &mut self,
    window: TileWindowId,
    minimized: bool,
  ) {
    let now = Instant::now();
    if !minimized {
      self.restore_window(window, now);
      return;
    }
    if self.scale_config.include_minimized
      && self
        .scale
        .as_ref()
        .is_some_and(|state| state.windows.contains(&window))
    {
      self
        .minimize_animations
        .retain(|_, animation| animation.window != Some(window));
      return;
    }
    let Some(surface) = self.window_surface(window) else {
      return;
    };
    let matches =
      self.with_properties(self.view_matches_of(surface), Some(window));
    let Some((effect, timing)) = self.minimize_choice(&matches) else {
      return;
    };
    if self.closing_animations.contains_key(&surface)
      || !self.textures.contains_key(&surface)
    {
      return;
    }
    let Some((output, (geometry, buffer))) = self.previous_of(surface) else {
      return;
    };
    let pieces = self.view_pieces(surface);
    let animation = match self.open_animations.remove(&surface) {
      Some(mut restoring)
        if restoring.effect == effect && restoring.kind == Kind::Restore =>
      {
        restoring.set_kind(Kind::Minimize, now);
        restoring
      }
      _ => {
        let view = pieces
          .as_ref()
          .and_then(ViewPieces::view)
          .unwrap_or_else(|| AnimatedView::surface(rect_of(geometry)));
        let info = self.view_info(surface, Some(window), view.geometry);
        Animation::start(effect, Kind::Minimize, timing, now, &info)
      }
    };
    self.minimize_animations.insert(
      surface,
      ClosingAnimation::new(
        (Some(window), output),
        (geometry, buffer),
        animation,
        pieces,
      ),
    );
  }

  /// Animates `window` coming back from being minimized: a minimize
  /// animation of the same effect turns around, else a restore starts.
  fn restore_window(&mut self, window: TileWindowId, now: Instant) {
    let minimizing = self
      .minimize_animations
      .iter()
      .find(|(_, animation)| animation.window == Some(window))
      .map(|(surface, _)| *surface);
    let snapshot = self
      .compositor
      .window_surfaces(window)
      .into_iter()
      .find(|snapshot| snapshot.window == Some(window));
    let Some(snapshot) = snapshot else {
      if let Some(surface) = minimizing {
        self.minimize_animations.remove(&surface);
      }
      return;
    };
    let matches =
      self.with_properties(self.view_matches(&snapshot), Some(window));
    let choice = self.minimize_choice(&matches);
    let taken = minimizing.and_then(|surface| {
      let closing = self.minimize_animations.remove(&surface)?;
      Some(closing.animation)
    });
    let Some((effect, timing)) = choice else {
      return;
    };
    let animation = match taken {
      Some(mut minimizing) if minimizing.effect == effect => {
        minimizing.set_kind(Kind::Restore, now);
        minimizing
      }
      _ => {
        // Never shown, so nothing to animate (wayfire:plugins/animate/
        // animate.cpp:470-475).
        if snapshot.buffer.is_none()
          || !self.textures.contains_key(&snapshot.id)
        {
          return;
        }
        let geometry = self.framed_geometry(&snapshot);
        let info = self.view_info(snapshot.id, Some(window), geometry);
        Animation::start(effect, Kind::Restore, timing, now, &info)
      }
    };
    self.open_animations.insert(snapshot.id, animation);
  }

  /// `[animate] startup_duration`.
  pub(super) fn startup_timing(&self) -> Timing {
    startup_timing(&self.animate)
  }

  /// Moves an output's fade-in, `startup`, on for a frame at `now`, which
  /// shows a
  /// background-layer surface if `background_shown`, and returns how black
  /// the frame is: [`StartupFrame::Black`] while the output waits for its
  /// background, then a black overlay fading from 1 to 0 over
  /// `startup_duration`; `None` once over
  /// (wayfire:plugins/animate/animate.cpp:242-256, 429-442, 485-489,
  /// wayfire:plugins/animate/system_fade.hpp:259-283).
  pub(super) fn startup_frame(
    config: &shingle_config::AnimateConfig,
    startup: &mut super::outputs::Startup,
    background_shown: bool,
    now: Instant,
  ) -> Option<StartupFrame> {
    use super::outputs::{STARTUP_INHIBIT, Startup};
    if *startup == Startup::Pending {
      *startup = if config.startup_duration_ms == 0 {
        Startup::Done
      } else {
        Startup::Inhibited(now)
      };
    }
    if let Startup::Inhibited(since) = *startup
      && (background_shown || now >= since + STARTUP_INHIBIT)
    {
      *startup = Startup::Fading(now);
    }
    match *startup {
      Startup::Inhibited(_) => Some(StartupFrame::Black),
      // From 1 to 0: 1 - easing(t).
      Startup::Fading(started) => Some(StartupFrame::Fade(
        1.0 - startup_timing(config).progress(started, now),
      )),
      Startup::Pending | Startup::Done => None,
    }
  }

  /// Whether any fire animation runs, so that a frame steps it.
  pub(super) fn has_fire(&self) -> bool {
    self
      .open_animations
      .values()
      .chain(
        self
          .closing_animations
          .values()
          .map(|closing| &closing.animation),
      )
      .chain(
        self
          .minimize_animations
          .values()
          .map(|closing| &closing.animation),
      )
      .any(|animation| animation.effect == Effect::Fire)
  }

  /// Runs a frame of the fire animations drawn on `output` at `now`, the
  /// live ones of `drawn` views with their bounds.
  pub(super) fn step_animations(
    &mut self,
    output: &shingle::OutputHandle,
    drawn: &[(SurfaceId, Rect)],
    now: Instant,
  ) {
    let size = |rect: Rect| (f64::from(rect.width), f64::from(rect.height));
    for (surface, geometry) in drawn {
      if let Some(animation) = self.open_animations.get_mut(surface)
        && animation.effect == Effect::Fire
      {
        animation.step(now, size(*geometry));
      }
    }
    for closing in self
      .closing_animations
      .values_mut()
      .chain(self.minimize_animations.values_mut())
      .filter(|closing| closing.output == *output)
      .filter(|closing| closing.animation.effect == Effect::Fire)
    {
      closing.animation.step(now, size(closing.view.bounds));
    }
  }

  pub(super) fn expire_closing_animations(&mut self, now: Instant) {
    let completed = self
      .closing_animations
      .iter()
      .filter(|(_, closing)| !closing.animation.running(now))
      .map(|(surface, _)| *surface)
      .collect::<Vec<_>>();
    for surface in completed {
      self.closing_animations.remove(&surface);
      self.textures.remove(&surface);
    }
    // Frame titles go once no closing view is still to be drawn with one.
    let departed = std::mem::take(&mut self.departed_titles);
    for window in departed {
      if self
        .closing_animations
        .values()
        .chain(self.minimize_animations.values())
        .any(|closing| closing.draws_title_of(window))
      {
        self.departed_titles.push(window);
      } else {
        self.frame_textures.window_destroyed(window);
      }
    }
  }

  /// The geometry of the window `snapshot` is the root surface of: its
  /// frame's rectangle if it has one, else its own.
  pub(super) fn framed_geometry(&self, snapshot: &SurfaceSnapshot) -> Rect {
    self
      .compositor
      .frames()
      .get(&snapshot.id)
      .map_or(snapshot.geometry, |frame| frame.rect)
  }

  pub(super) fn expire_minimize_animations(&mut self, now: Instant) {
    self
      .minimize_animations
      .retain(|_, closing| closing.animation.running(now));
  }

  /// Releases the retained exit snapshots after the final transition frame.
  pub(super) fn expire_scale_exit(&mut self, now: Instant) {
    if self.scale_exit.as_ref().is_some_and(|state| {
      self
        .scale_timing()
        .linear_progress(state.animation_started, now)
        >= 1.0
    }) {
      self.scale_exit = None;
    }
  }
}

/// What an output starting up draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum StartupFrame {
  /// Black, nothing else: rendering is inhibited.
  Black,
  /// Everything under black of this alpha.
  Fade(f32),
}

/// `config`'s `startup_duration`.
fn startup_timing(config: &shingle_config::AnimateConfig) -> Timing {
  Timing::new(config.startup_duration_ms, config.startup_easing)
}

/// The layout rectangle of `rect`.
pub(super) fn rect_of(rect: RenderRect) -> Rect {
  Rect::new(rect.x, rect.y, rect.width, rect.height)
}

/// Draws `tile`, the view at `view` or the texture it is drawn into, as
/// `animation` shows it at `now`; fire's particles come back as a tile to
/// draw above it. The 2D effects scale and turn about the centre of the
/// view's geometry, fire and squeezimize work in its bounds.
pub(super) fn apply_animation<'a>(
  tile: &mut RenderTile<'a>,
  animation: &'a Animation,
  now: Instant,
  view: AnimatedView,
) -> Option<RenderTile<'a>> {
  match animation.effect {
    Effect::Squeezimize => {
      if let Some((area, squeeze)) = animation.squeeze(now, view.bounds) {
        tile.geometry = area;
        tile.effect = TileEffect::Squeeze(squeeze);
      }
      None
    }
    Effect::Fire => {
      let (line, particles) = animation.fire()?;
      let view = view.bounds;
      // The view is drawn above its progress line, and the particles
      // over it (wayfire:plugins/animate/fire/fire.cpp:185-211).
      let height = (f64::from(view.height) * line).floor().max(0.0) as u32;
      tile.crop(RenderRect {
        x: view.x,
        y: view.y,
        width: view.width,
        height,
      });
      let mut fire = RenderTile::solid(
        RenderRect {
          x: view.x,
          y: view.y,
          width: 0,
          height: 0,
        },
        [1.0; 4],
      );
      fire.effect = TileEffect::Particles(particles);
      Some(fire)
    }
    _ => {
      let transform = animation.transform(now);
      let view = view.geometry;
      let center = (
        f64::from(view.x) + f64::from(view.width) / 2.0,
        f64::from(view.y) + f64::from(view.height) / 2.0,
      );
      // Colours are premultiplied.
      for component in &mut tile.color {
        *component *= transform.alpha as f32;
      }
      tile.geometry = transform.apply(tile.geometry, center);
      tile.rotation += transform.angle as f32;
      None
    }
  }
}

/// Whether `tile` is drawn with a shader of its own, which a frame cannot
/// follow.
pub(super) fn has_effect(tile: &RenderTile<'_>) -> bool {
  !matches!(tile.effect, TileEffect::None)
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use shingle_config::Easing;
  use shingle_render::{RenderRect, RenderTile};

  use super::*;

  fn info(geometry: Rect) -> ViewInfo {
    ViewInfo {
      geometry,
      output: Rect::new(0, 0, 1000, 800),
      minimize_hint: None,
      spin_rotations: 1,
      fire: FireOptions::of(&shingle_config::AnimateConfig::default()),
      seed: 1,
    }
  }

  #[test]
  fn opening_animation_fades_and_zooms_from_center() {
    let geometry = RenderRect {
      x: 100,
      y: 50,
      width: 200,
      height: 100,
    };
    let view = rect_of(geometry);
    let now = Instant::now();
    let linear = Timing::new(400, Easing::Linear);
    let fade =
      Animation::start(Effect::Fade, Kind::Map, linear, now, &info(view));
    let mut tile = RenderTile::solid(geometry, [1.0; 4]);
    apply_animation(
      &mut tile,
      &fade,
      now + Duration::from_millis(100),
      AnimatedView::surface(view),
    );
    assert_eq!(tile.geometry, geometry);
    assert_eq!(tile.color, [0.25; 4]);

    let zoom =
      Animation::start(Effect::Zoom, Kind::Map, linear, now, &info(view));
    let mut tile = RenderTile::solid(geometry, [1.0; 4]);
    apply_animation(
      &mut tile,
      &zoom,
      now + Duration::from_millis(200),
      AnimatedView::surface(view),
    );
    assert_eq!(
      tile.geometry,
      // Two thirds of the size about the centre, rounded at the edges.
      RenderRect {
        x: 133,
        y: 67,
        width: 134,
        height: 66,
      }
    );
    assert_eq!(tile.color[3], 0.5);
    let mut start = RenderTile::solid(geometry, [1.0; 4]);
    apply_animation(&mut start, &zoom, now, AnimatedView::surface(view));
    assert_eq!(start.geometry.width, 66);
    assert_eq!(start.geometry.height, 34);
    assert_eq!(start.color[3], 0.0);
  }

  #[test]
  fn spin_turns_the_tile_and_fire_cuts_it_at_its_line() {
    let geometry = RenderRect {
      x: 0,
      y: 0,
      width: 400,
      height: 400,
    };
    let view = rect_of(geometry);
    let now = Instant::now();
    let linear = Timing::new(400, Easing::Linear);
    let spin =
      Animation::start(Effect::Spin, Kind::Map, linear, now, &info(view));
    let mut tile = RenderTile::solid(geometry, [1.0; 4]);
    apply_animation(
      &mut tile,
      &spin,
      now + Duration::from_millis(100),
      AnimatedView::surface(view),
    );
    assert!((tile.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    let mut fire =
      Animation::start(Effect::Fire, Kind::Map, linear, now, &info(view));
    fire.step(now + Duration::from_millis(100), (400.0, 400.0));
    let mut tile = RenderTile::solid(geometry, [1.0; 4]);
    let particles = apply_animation(
      &mut tile,
      &fire,
      now + Duration::from_millis(100),
      AnimatedView::surface(view),
    );
    assert_eq!(tile.geometry.height, 100);
    assert!(matches!(
      particles.map(|tile| tile.effect),
      Some(TileEffect::Particles(_))
    ));
  }
}
