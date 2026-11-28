//! Whole views: a view's surface, its subsurfaces and its server-side
//! frame drawn into a texture of their own while an animate effect runs,
//! which the effect then draws as one, as Wayfire's view transformers
//! draw a view's node (wayfire:src/api/wayfire/view-transform.hpp,
//! `transformer_render_instance_t::get_texture`).
//!
//! A view is its root surface, a toplevel, layer surface, popup or X
//! window, with the subsurfaces below it; a popup is a view of its own,
//! as Wayfire's `wayfire_xdg_popup` is, which it puts into the output's
//! unmanaged layer rather than into its parent's node
//! (wayfire:src/view/xdg-shell.cpp:184-214), and the decoration is in the
//! view's surface root node (wayfire:plugins/decor/deco-subsurface.cpp:
//! 361). A view of one surface without a frame keeps its surface's
//! texture, as Wayfire's `zero_copy_texture` does, so that only views
//! with more than one piece cost a pass.

use std::collections::HashMap;

use shingle::{
  BufferSnapshot, OutputHandle, SurfaceId, SurfaceRole, SurfaceSnapshot,
  frame::FrameSnapshot,
};
use shingle_core::Rect;
use shingle_render::{GpuTexture, RenderRect, RenderTile};

use super::{
  Frontend,
  animation::{ClosingImage, rect_of},
  frames::FrameTextures,
  outputs::translate,
  scene::apply_buffer,
};
use crate::{animate::Effect, render_target::RenderTarget};

/// Where an animated view is: its geometry, the window's with its frame,
/// whose centre 2D effects turn and scale about, as
/// `view_2d_transformer_t` takes the toplevel's geometry
/// (wayfire:src/view/view-3d.cpp:174-189); and the bounds of everything
/// drawn of it, the children's bounding box that fire and squeezimize
/// work in (wayfire:plugins/animate/fire/fire.cpp:179-192,
/// wayfire:plugins/animate/squeezimize.hpp:344).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct AnimatedView {
  /// The window's geometry with its frame.
  pub(super) geometry: Rect,
  /// The bounds of its surfaces and frame.
  pub(super) bounds: Rect,
}

impl AnimatedView {
  /// A view of one surface at `rect`.
  pub(super) fn surface(rect: Rect) -> Self {
    Self {
      geometry: rect,
      bounds: rect,
    }
  }
}

/// A view drawn into a texture of its own, and where the texture goes.
#[derive(Clone)]
pub(super) struct ViewImage {
  /// The view's pieces.
  pub(super) texture: GpuTexture,
  /// The layout rectangle the texture covers.
  pub(super) area: RenderRect,
}

/// One surface of a view, as last drawn.
#[derive(Clone)]
pub(super) struct Piece {
  /// The surface.
  pub(super) surface: SurfaceId,
  /// Where it was drawn.
  pub(super) geometry: RenderRect,
  /// Its texture.
  pub(super) texture: GpuTexture,
  /// Its buffer, which keeps an imported texture's buffer and how it is
  /// sampled.
  pub(super) buffer: BufferSnapshot,
  /// The opacity it was drawn with.
  pub(super) opacity: f32,
}

/// Everything of a view to draw into its texture: its surfaces bottom to
/// top, the root among them, and its frame.
#[derive(Clone)]
pub(super) struct ViewPieces {
  /// The surfaces, bottom to top.
  pub(super) surfaces: Vec<Piece>,
  /// The root surface.
  pub(super) root: SurfaceId,
  /// The frame, drawn below them.
  pub(super) frame: Option<FrameSnapshot>,
}

impl ViewPieces {
  /// Whether the view is more than its root surface, so that it needs a
  /// texture of its own.
  pub(super) fn is_whole(&self) -> bool {
    self.surfaces.len() > 1 || self.frame.is_some()
  }

  /// Where the view is.
  pub(super) fn view(&self) -> Option<AnimatedView> {
    let root = self
      .surfaces
      .iter()
      .find(|piece| piece.surface == self.root)?;
    let content = rect_of(root.geometry);
    let geometry = self.frame.as_ref().map_or(content, |frame| frame.rect);
    let bounds = bounds(
      self
        .surfaces
        .iter()
        .map(|piece| rect_of(piece.geometry))
        .chain(Some(geometry)),
    )?;
    Some(AnimatedView { geometry, bounds })
  }

  /// The tiles of the view, in layout coordinates: the frame, then the
  /// surfaces.
  pub(super) fn tiles<'a>(
    &'a self,
    frame_textures: &'a FrameTextures,
    config: &shingle_config::DecorationConfig,
    scale: f64,
  ) -> Vec<RenderTile<'a>> {
    let surface = |piece: &'a Piece| {
      let mut tile = RenderTile::textured(piece.geometry, &piece.texture);
      tile.color = [piece.opacity; 4];
      apply_buffer(&mut tile, &piece.buffer);
      tile
    };
    let mut tiles = Vec::with_capacity(self.surfaces.len() + 8);
    if let Some(frame) = &self.frame
      && let Some(root) = self
        .surfaces
        .iter()
        .find(|piece| piece.surface == self.root)
    {
      let content = surface(root);
      tiles.extend(frame_textures.tiles(
        frame,
        root.geometry,
        &content,
        config,
        scale,
      ));
    }
    tiles.extend(self.surfaces.iter().map(surface));
    tiles
  }
}

/// The root of each subsurface among `snapshots`: its parent, or its
/// parent's root while that is a subsurface too. A subsurface whose
/// parent is not drawn is left out.
pub(super) fn subsurface_roots(
  snapshots: &[SurfaceSnapshot],
) -> HashMap<SurfaceId, SurfaceId> {
  let parents = snapshots
    .iter()
    .filter(|snapshot| snapshot.role == SurfaceRole::Subsurface)
    .filter_map(|snapshot| Some((snapshot.id, snapshot.parent?)))
    .collect::<HashMap<_, _>>();
  if parents.is_empty() {
    return HashMap::new();
  }
  let drawn = snapshots
    .iter()
    .map(|snapshot| snapshot.id)
    .collect::<std::collections::HashSet<_>>();
  roots_of(&parents)
    .into_iter()
    .filter(|(_, root)| drawn.contains(root))
    .collect()
}

/// [`subsurface_roots`] over a map of each subsurface to its parent: the
/// first ancestor that is no subsurface. A cycle, which the compositor
/// refuses, ends at the subsurface where it closes.
pub(super) fn roots_of(
  parents: &HashMap<SurfaceId, SurfaceId>,
) -> HashMap<SurfaceId, SurfaceId> {
  parents
    .keys()
    .map(|&surface| {
      let mut root = parents[&surface];
      for _ in 0..parents.len() {
        match parents.get(&root) {
          Some(&parent) => root = parent,
          None => break,
        }
      }
      (surface, root)
    })
    .collect()
}

/// The members of each view among `snapshots` that has subsurfaces, root
/// included, bottom to top as drawn.
pub(super) fn view_members(
  snapshots: &[SurfaceSnapshot],
  roots: &HashMap<SurfaceId, SurfaceId>,
) -> HashMap<SurfaceId, Vec<SurfaceId>> {
  let mut members = HashMap::<SurfaceId, Vec<SurfaceId>>::new();
  if roots.is_empty() {
    return members;
  }
  let with_members = roots
    .values()
    .copied()
    .collect::<std::collections::HashSet<_>>();
  for snapshot in snapshots {
    if snapshot.buffer.is_none() {
      continue;
    }
    let root = roots
      .get(&snapshot.id)
      .copied()
      .or_else(|| with_members.contains(&snapshot.id).then_some(snapshot.id));
    if let Some(root) = root {
      members.entry(root).or_default().push(snapshot.id);
    }
  }
  members
}

/// `rect` as a render rectangle.
pub(super) fn render_rect(rect: Rect) -> RenderRect {
  RenderRect {
    x: rect.x,
    y: rect.y,
    width: rect.width,
    height: rect.height,
  }
}

/// `frame` moved by `shift`, as the scene draws the frame of a window of
/// another workspace with the window
/// ([`shingle::SurfaceSnapshot::shift`]).
pub(super) fn shifted_frame(
  frame: &FrameSnapshot,
  shift: (i32, i32),
) -> FrameSnapshot {
  let mut frame = frame.clone();
  frame.rect.x = frame.rect.x.saturating_add(shift.0);
  frame.rect.y = frame.rect.y.saturating_add(shift.1);
  frame
}

/// The smallest rectangle around `rects`, or `None` without any.
pub(super) fn bounds(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
  let mut rects = rects
    .into_iter()
    .filter(|rect| rect.width > 0 && rect.height > 0);
  let first = rects.next()?;
  let right = |rect: Rect| i64::from(rect.x) + i64::from(rect.width);
  let bottom = |rect: Rect| i64::from(rect.y) + i64::from(rect.height);
  let (mut left, mut top) = (i64::from(first.x), i64::from(first.y));
  let (mut far_right, mut far_bottom) = (right(first), bottom(first));
  for rect in rects {
    left = left.min(i64::from(rect.x));
    top = top.min(i64::from(rect.y));
    far_right = far_right.max(right(rect));
    far_bottom = far_bottom.max(bottom(rect));
  }
  Some(Rect::new(
    i32::try_from(left).ok()?,
    i32::try_from(top).ok()?,
    u32::try_from(far_right - left).ok()?,
    u32::try_from(far_bottom - top).ok()?,
  ))
}

/// The smallest rectangle around `bounds` whose centre is `geometry`'s,
/// so that a tile drawn there turns and scales about the window's centre
/// as Wayfire's transformer does, while it covers all of the view.
pub(super) fn centred(bounds: Rect, geometry: Rect) -> Rect {
  // Twice the centre, to stay in integers.
  let centre_x = 2 * i64::from(geometry.x) + i64::from(geometry.width);
  let centre_y = 2 * i64::from(geometry.y) + i64::from(geometry.height);
  let reach = |start: i32, length: u32, centre: i64| {
    let start = 2 * i64::from(start);
    let end = start + 2 * i64::from(length);
    (centre - start).max(end - centre).max(0)
  };
  let half_width = reach(bounds.x, bounds.width, centre_x);
  let half_height = reach(bounds.y, bounds.height, centre_y);
  // Rounded out to whole pixels on both sides.
  let left = (centre_x - half_width).div_euclid(2);
  let right = (centre_x + half_width + 1).div_euclid(2);
  let top = (centre_y - half_height).div_euclid(2);
  let bottom = (centre_y + half_height + 1).div_euclid(2);
  let clamp = |value: i64| {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
  };
  Rect::new(
    clamp(left),
    clamp(top),
    u32::try_from(right - left).unwrap_or(u32::MAX),
    u32::try_from(bottom - top).unwrap_or(u32::MAX),
  )
}

/// The layout rectangle a view's texture covers for `effect`: its bounds
/// for fire and squeezimize, which work in them, and for the 2D effects
/// the bounds widened to be centred on its geometry.
pub(super) fn texture_area(effect: Effect, view: AnimatedView) -> Rect {
  match effect {
    Effect::Fire | Effect::Squeezimize => view.bounds,
    _ => centred(view.bounds, view.geometry),
  }
}

/// The size in texels of a texture for `area` at `scale`, or `None` if
/// it is empty or larger than `limit` on a side.
pub(super) fn texture_size(
  area: Rect,
  scale: f64,
  limit: u32,
) -> Option<(u32, u32)> {
  let texels = |length: u32| {
    let texels = (f64::from(length) * scale).ceil();
    (texels >= 1.0 && texels <= f64::from(limit)).then_some(texels as u32)
  };
  Some((texels(area.width)?, texels(area.height)?))
}

/// Draws `tiles`, in layout coordinates, into a texture for `area` at
/// `scale`: `reuse` if it has the size, else a new one.
pub(super) fn draw_image(
  target: &dyn RenderTarget,
  tiles: &[RenderTile<'_>],
  area: Rect,
  scale: f64,
  reuse: Option<GpuTexture>,
) -> Option<ViewImage> {
  let limit = target
    .gpu_context()
    .device()
    .limits()
    .max_texture_dimension_2d;
  let (width, height) = texture_size(area, scale, limit)?;
  let texture = match reuse {
    Some(texture)
      if texture.width() == width
        && texture.height() == height
        && shingle_render::is_view_texture(&texture) =>
    {
      texture
    }
    _ => target.create_view_texture(width, height).ok()?,
  };
  let local = tiles
    .iter()
    .cloned()
    .map(|mut tile| {
      tile.geometry = translate(tile.geometry, -area.x, -area.y);
      tile
    })
    .collect::<Vec<_>>();
  if let Err(error) = target.draw_view(&texture, scale, &local) {
    eprintln!("shingle: a view could not be drawn for its animation: {error}");
    return None;
  }
  Some(ViewImage {
    texture,
    area: RenderRect {
      x: area.x,
      y: area.y,
      width: area.width,
      height: area.height,
    },
  })
}

/// The surfaces of `window`'s view with a buffer: its toplevel and the
/// subsurfaces below it, without its popups, which are views of their
/// own.
pub(super) fn window_view(
  compositor: &shingle::Compositor,
  window: shingle_core::WindowId,
) -> Vec<SurfaceSnapshot> {
  let surfaces = compositor.window_surfaces(window);
  let Some(toplevel) = surfaces
    .iter()
    .find(|snapshot| snapshot.window == Some(window))
    .map(|snapshot| snapshot.id)
  else {
    return surfaces;
  };
  let roots = subsurface_roots(&surfaces);
  surfaces
    .into_iter()
    .filter(|snapshot| {
      snapshot.id == toplevel || roots.get(&snapshot.id) == Some(&toplevel)
    })
    .collect()
}

impl Frontend {
  /// `snapshots` with the subsurfaces of each toplevel among them after
  /// it, for the scenes that list toplevels only, so that a window's
  /// subsurfaces move with it there.
  pub(super) fn with_subsurfaces(
    &self,
    snapshots: Vec<SurfaceSnapshot>,
  ) -> Vec<SurfaceSnapshot> {
    let mut all = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
      let window = snapshot
        .window
        .filter(|_| snapshot.role == SurfaceRole::Toplevel);
      let id = snapshot.id;
      all.push(snapshot);
      if let Some(window) = window {
        all.extend(
          window_view(&self.compositor, window)
            .into_iter()
            .filter(|member| member.id != id),
        );
      }
    }
    all
  }

  /// Draws the views animating on the output `view` shows into textures
  /// of their own, before its frame: each opening or restoring view among
  /// `snapshots` that has subsurfaces in `members` or a frame in `frames`,
  /// from its pieces as they are now, into a texture kept from frame to
  /// frame; and each closing or minimizing view there whose last frame
  /// is still to be drawn, once. Returns the images of the opening views
  /// by root surface with where each view is, and the locks of the
  /// buffers the closing views were drawn from, which the frame keeps.
  pub(super) fn draw_whole_views(
    &mut self,
    target: &dyn RenderTarget,
    (view, snapshots): (&shingle::OutputView, &[SurfaceSnapshot]),
    frames: &HashMap<SurfaceId, FrameSnapshot>,
    members: &HashMap<SurfaceId, Vec<SurfaceId>>,
  ) -> (
    HashMap<SurfaceId, (ViewImage, AnimatedView)>,
    Vec<shingle::BufferLock>,
  ) {
    let output = &view.handle;
    let mut whole = HashMap::new();
    if !self.open_animations.is_empty() {
      let by_id = snapshots
        .iter()
        .map(|snapshot| (snapshot.id, snapshot))
        .collect::<HashMap<_, _>>();
      for snapshot in snapshots {
        let Some(animation) = self.open_animations.get(&snapshot.id) else {
          continue;
        };
        let ids = members.get(&snapshot.id);
        let frame = frames.get(&snapshot.id);
        if snapshot.buffer.is_none() || (ids.is_none() && frame.is_none()) {
          continue;
        }
        let ids = ids.map_or(std::slice::from_ref(&snapshot.id), Vec::as_slice);
        let pieces = ViewPieces {
          surfaces: ids
            .iter()
            .filter_map(|id| {
              let member = by_id.get(id)?;
              Some(Piece {
                surface: *id,
                geometry: render_rect(member.geometry),
                texture: self.textures.get(id)?.clone(),
                buffer: member.buffer.clone()?,
                opacity: member.opacity,
              })
            })
            .collect(),
          root: snapshot.id,
          frame: frame.map(|frame| shifted_frame(frame, snapshot.shift)),
        };
        let Some(placed) = pieces.view() else {
          continue;
        };
        let area = texture_area(animation.effect, placed);
        let tiles = pieces.tiles(
          &self.frame_textures,
          &self.decoration_config,
          view.scale,
        );
        let reuse = self.view_textures.take(output, snapshot.id);
        if let Some(image) = draw_image(target, &tiles, area, view.scale, reuse)
        {
          self
            .view_textures
            .keep(output, snapshot.id, image.texture.clone());
          whole.insert(snapshot.id, (image, placed));
        }
      }
    }
    let kept = whole.keys().copied().collect();
    self.view_textures.retain_on(output, &kept);
    let mut locks = Vec::new();
    for closing in self
      .closing_animations
      .values_mut()
      .chain(self.minimize_animations.values_mut())
      .filter(|closing| closing.output == *output)
    {
      let ClosingImage::Pending(pieces) = &closing.image else {
        continue;
      };
      let area = texture_area(closing.animation.effect, closing.view);
      let image = {
        let tiles = pieces.tiles(
          &self.frame_textures,
          &self.decoration_config,
          view.scale,
        );
        draw_image(target, &tiles, area, view.scale, None)
      };
      locks.extend(
        pieces
          .surfaces
          .iter()
          .filter_map(|piece| piece.buffer.dmabuf())
          .map(|content| content.lock.clone()),
      );
      closing.image = image.map_or(ClosingImage::Surface, ClosingImage::Drawn);
    }
    (whole, locks)
  }

  /// The pieces of the view of `root` as the last frame that drew it drew
  /// them, if it is more than that one surface: its subsurfaces and its
  /// frame, with the textures they had. `None` for a surface drawn alone
  /// or not drawn.
  pub(super) fn view_pieces(&self, root: SurfaceId) -> Option<ViewPieces> {
    let mut scenes = self.scenes.iter().collect::<Vec<_>>();
    scenes.sort_by(|first, second| first.0.cmp(second.0));
    let scene = scenes
      .into_iter()
      .map(|(_, scene)| scene)
      .find(|scene| scene.previous_surfaces.contains_key(&root))?;
    let frame = scene.previous_frames.get(&root).cloned();
    let members = scene.view_members.get(&root);
    if members.is_none() && frame.is_none() {
      return None;
    }
    let members = members.map_or(std::slice::from_ref(&root), Vec::as_slice);
    let surfaces = members
      .iter()
      .filter_map(|surface| {
        let (geometry, buffer) = scene.previous_surfaces.get(surface)?;
        Some(Piece {
          surface: *surface,
          geometry: *geometry,
          texture: self.textures.get(surface)?.clone(),
          buffer: buffer.clone(),
          opacity: scene
            .previous_opacities
            .get(surface)
            .copied()
            .unwrap_or(1.0),
        })
      })
      .collect::<Vec<_>>();
    let pieces = ViewPieces {
      surfaces,
      root,
      frame,
    };
    pieces.is_whole().then_some(pieces)
  }
}

/// The textures of the views animating on each output, kept from frame
/// to frame while their size stays.
#[derive(Default)]
pub(super) struct ViewTextures {
  textures: HashMap<(OutputHandle, SurfaceId), GpuTexture>,
}

impl ViewTextures {
  /// Takes the texture of `root` on `output` for another frame.
  pub(super) fn take(
    &mut self,
    output: &OutputHandle,
    root: SurfaceId,
  ) -> Option<GpuTexture> {
    self.textures.remove(&(output.clone(), root))
  }

  /// Keeps `texture` for `root` on `output`.
  pub(super) fn keep(
    &mut self,
    output: &OutputHandle,
    root: SurfaceId,
    texture: GpuTexture,
  ) {
    self.textures.insert((output.clone(), root), texture);
  }

  /// Forgets the textures of `output` but those of `kept`.
  pub(super) fn retain_on(
    &mut self,
    output: &OutputHandle,
    kept: &std::collections::HashSet<SurfaceId>,
  ) {
    self
      .textures
      .retain(|(on, root), _| on != output || kept.contains(root));
  }

  /// How many textures are kept.
  #[cfg(test)]
  pub(super) fn len(&self) -> usize {
    self.textures.len()
  }

  /// Forgets every texture, after the loss of the GPU.
  pub(super) fn clear(&mut self) {
    self.textures.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn id(value: u64) -> SurfaceId {
    SurfaceId(value)
  }

  #[test]
  fn a_subsurface_belongs_to_the_first_ancestor_that_is_none() {
    let parents =
      HashMap::from([(id(3), id(2)), (id(2), id(1)), (id(5), id(4))]);
    let roots = roots_of(&parents);
    assert_eq!(roots[&id(3)], id(1));
    assert_eq!(roots[&id(2)], id(1));
    assert_eq!(roots[&id(5)], id(4));
    assert_eq!(roots.len(), 3);
    // A cycle ends instead of looping.
    let cycle = HashMap::from([(id(1), id(2)), (id(2), id(1))]);
    assert_eq!(roots_of(&cycle).len(), 2);
  }

  #[test]
  fn bounds_cover_every_piece_and_skip_empty_ones() {
    let rects = [
      Rect::new(100, 100, 200, 100),
      Rect::new(250, 80, 100, 40),
      Rect::new(0, 0, 0, 0),
    ];
    assert_eq!(bounds(rects), Some(Rect::new(100, 80, 250, 120)));
    assert_eq!(bounds([]), None);
  }

  #[test]
  fn the_area_is_centred_on_the_window() {
    let geometry = Rect::new(100, 100, 200, 100);
    // Inside the window, the area is the window.
    assert_eq!(centred(geometry, geometry), geometry);
    // A piece 50 pixels past the right edge widens it on both sides.
    let bounds = Rect::new(100, 100, 250, 100);
    assert_eq!(centred(bounds, geometry), Rect::new(50, 100, 300, 100));
    // An odd centre rounds out.
    let odd = Rect::new(0, 0, 3, 3);
    assert_eq!(centred(Rect::new(0, 0, 4, 3), odd), Rect::new(-1, 0, 5, 3));
    let view = AnimatedView { geometry, bounds };
    assert_eq!(texture_area(Effect::Fire, view), bounds);
    assert_eq!(texture_area(Effect::Squeezimize, view), bounds);
    assert_eq!(
      texture_area(Effect::Spin, view),
      Rect::new(50, 100, 300, 100)
    );
  }

  #[test]
  fn texture_sizes_round_up_and_respect_the_limit() {
    let area = Rect::new(0, 0, 101, 50);
    assert_eq!(texture_size(area, 1.0, 8192), Some((101, 50)));
    assert_eq!(texture_size(area, 1.5, 8192), Some((152, 75)));
    assert_eq!(texture_size(area, 100.0, 8192), None);
    assert_eq!(texture_size(Rect::new(0, 0, 0, 10), 1.0, 8192), None);
  }
}
