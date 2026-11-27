//! The fast switcher and the regular (Alt+Tab) switcher, whose view of
//! thumbnails is Wayfire's `switcher` plugin (crates/shingle-frontend/src/
//! switcher_view.rs).

use std::{collections::HashMap, time::Instant};

use shingle::{
  Compositor, OutputHandle, SurfaceId, SurfaceSnapshot, frame::FrameSnapshot,
};
use shingle_config::Modifiers as ConfigModifiers;
use shingle_core::{Rect, WindowId as TileWindowId};
use shingle_render::{GpuTexture, Projection, RenderRect, RenderTile};

use super::{Frontend, frames::FrameTextures, scene::apply_buffer};
use crate::{
  bindings::{binding_modifiers, modifiers_empty},
  switcher_view::{self, Direction, FrameState, SwitcherView, Views},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SwitcherKind {
  Fast,
  Regular,
}

/// A switcher held by its modifiers.
pub(super) struct SwitcherState {
  pub(super) kind: SwitcherKind,
  /// The windows it switches between, most recently focused first.
  pub(super) windows: Vec<TileWindowId>,
  /// The index of the selected one in `windows`.
  pub(super) selected: usize,
  pub(super) modifiers: ConfigModifiers,
  /// The fast switcher's windows' opacities before it dimmed them.
  pub(super) original_opacity: HashMap<TileWindowId, f32>,
}

/// The regular switcher's view on the output it started on, from the
/// first press until its thumbnails are back in place, which is after
/// [`SwitcherState`] has gone.
pub(super) struct ShownSwitcher {
  pub(super) output: OutputHandle,
  pub(super) view: SwitcherView,
}

impl Frontend {
  pub(super) fn start_switcher(
    &mut self,
    reverse: bool,
    binding: shingle_config::Binding,
    kind: SwitcherKind,
  ) {
    if kind == SwitcherKind::Regular {
      self.switch_regular(reverse, binding);
      return;
    }
    if self.switcher.is_none() {
      self.compositor.set_show_desktop(false);
      let mut windows = self.compositor.switchable_windows();
      windows.retain(|window| {
        self
          .compositor
          .core()
          .window_info(*window)
          .is_ok_and(|info| !info.state.minimized)
      });
      if windows.is_empty() {
        return;
      }
      let selected = self
        .compositor
        .focused_window()
        .and_then(|focused| {
          windows.iter().position(|window| *window == focused)
        })
        .unwrap_or(0);
      let original_opacity = windows
        .iter()
        .filter_map(|window| {
          self
            .compositor
            .window_opacity(*window)
            .map(|opacity| (*window, opacity))
        })
        .collect();
      self.switcher = Some(SwitcherState {
        kind,
        windows,
        selected,
        modifiers: binding_modifiers(&binding),
        original_opacity,
      });
      // Wayfire's fast-switcher activates its plugin while it is held.
      self
        .compositor
        .notify_plugin_activation("fast-switcher", true);
    }
    self.advance_switcher(reverse);
    if self
      .switcher
      .as_ref()
      .is_some_and(|state| modifiers_empty(state.modifiers))
    {
      self.finish_switcher();
    }
  }

  /// Wayfire's `handle_switch_request` for the regular switcher: the
  /// first press arranges the thumbnails of the active output's current
  /// workspace with the selection moved one step, or again during their
  /// exit animation; a binding without modifiers lets go at once.
  fn switch_regular(
    &mut self,
    reverse: bool,
    binding: shingle_config::Binding,
  ) {
    self.compositor.set_show_desktop(false);
    let output = self.compositor.active_output();
    let views = self.switcher_views(&output);
    if views.windows.is_empty() {
      return;
    }
    if self
      .switcher_view
      .as_ref()
      .is_some_and(|shown| shown.output != output)
    {
      self.close_switcher_view();
    }
    if self.switcher_view.is_none() {
      self.compositor.notify_plugin_activation("switcher", true);
      self.switcher_view = Some(ShownSwitcher {
        output,
        view: SwitcherView::new(&self.switcher_config),
      });
    }
    // The modifiers of the press that started it end it.
    let held = self
      .switcher
      .as_ref()
      .filter(|state| state.kind == SwitcherKind::Regular)
      .map(|state| state.modifiers);
    let modifiers = held.unwrap_or_else(|| binding_modifiers(&binding));
    let direction = if reverse {
      Direction::Previous
    } else {
      Direction::Next
    };
    let shown = self.switcher_view.as_mut().expect("made above");
    let raise = shown.view.switch(direction, &views, Instant::now());
    let selected = shown.view.selected();
    self.switcher = Some(SwitcherState {
      kind: SwitcherKind::Regular,
      selected: selected
        .and_then(|selected| {
          views.windows.iter().position(|window| *window == selected)
        })
        .unwrap_or(0),
      windows: views.windows,
      modifiers,
      original_opacity: HashMap::new(),
    });
    if let Some(window) = raise
      && let Err(error) = self.compositor.raise_window(window)
    {
      eprintln!("shingle: switcher raise failed: {error}");
    }
    self.request_redraw();
    if held.is_none() && modifiers_empty(modifiers) {
      self.finish_switcher();
    }
  }

  /// What the regular switcher shows of `output`: the windows of its
  /// current workspace, minimized ones too, as Wayfire's
  /// `get_workspace_views` lists them, each with its bounding box: its
  /// surfaces, subsurfaces, popups and frame.
  pub(super) fn switcher_views(&self, output: &OutputHandle) -> Views {
    let Some(view) = self.compositor.output_view(output) else {
      return Views::default();
    };
    let origin = view.rect;
    let frames = self.compositor.frames();
    let mut views = Views {
      output: (f64::from(origin.width), f64::from(origin.height)),
      ..Views::default()
    };
    for window in self.compositor.switchable_windows() {
      let Some(bounds) = window_bounds(&self.compositor, window, &frames)
      else {
        continue;
      };
      if self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| info.state.minimized)
      {
        views.minimized.insert(window);
      }
      views.windows.push(window);
      views.bounds.insert(
        window,
        Rect::new(
          bounds.x - origin.x,
          bounds.y - origin.y,
          bounds.width,
          bounds.height,
        ),
      );
    }
    views
  }

  pub(super) fn advance_switcher(&mut self, reverse: bool) {
    let Some(state) = &mut self.switcher else {
      return;
    };
    if state.kind == SwitcherKind::Regular {
      let binding = if reverse {
        self.keybindings.focus_previous.clone()
      } else {
        self.keybindings.focus_next.clone()
      };
      self.switch_regular(reverse, binding);
      return;
    }
    if state.windows.is_empty() {
      self.switcher = None;
      return;
    }
    state.selected = if reverse {
      (state.selected + state.windows.len() - 1) % state.windows.len()
    } else {
      (state.selected + 1) % state.windows.len()
    };
    self.apply_switcher();
  }

  pub(super) fn apply_switcher(&mut self) {
    let Some(state) = &self.switcher else {
      return;
    };
    let selected = state.windows.get(state.selected).copied();
    let updates = state
      .original_opacity
      .iter()
      .map(|(window, opacity)| (*window, *opacity))
      .collect::<Vec<_>>();
    for (window, original) in updates {
      self.compositor.set_window_opacity(
        window,
        if Some(window) == selected || state.kind == SwitcherKind::Regular {
          original
        } else {
          original * self.switcher_inactive_alpha
        },
      );
    }
    if let Some(selected) = selected
      && let Err(error) = self.compositor.raise_window(selected)
    {
      eprintln!("shingle: fast-switcher raise failed: {error}");
    }
    self.request_redraw();
  }

  pub(super) fn finish_switcher(&mut self) {
    let Some(state) = self.switcher.take() else {
      return;
    };
    for (window, opacity) in state.original_opacity {
      self.compositor.set_window_opacity(window, opacity);
    }
    if state.kind != SwitcherKind::Regular {
      self
        .compositor
        .notify_plugin_activation("fast-switcher", false);
    }
    let selected = if state.kind == SwitcherKind::Regular {
      // Wayfire's `handle_done`: the thumbnails move back, and the one in
      // the middle is focused and raised, restored if minimized.
      let output = self
        .switcher_view
        .as_ref()
        .map(|shown| shown.output.clone());
      let views = output
        .map(|output| self.switcher_views(&output))
        .unwrap_or_default();
      self
        .switcher_view
        .as_mut()
        .and_then(|shown| shown.view.done(&views, Instant::now()))
    } else {
      state.windows.get(state.selected).copied()
    };
    if let Some(window) = selected {
      if self
        .compositor
        .core()
        .window_info(window)
        .is_ok_and(|info| info.state.minimized)
        && let Err(error) = self.compositor.set_minimized(window, false)
      {
        eprintln!("shingle: switcher restore failed: {error}");
      }
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: fast-switcher focus failed: {error}");
      }
    }
    self.save();
    self.request_redraw();
  }

  /// Lets go of the switcher and drops its view at once, without the exit
  /// animation, as when Scale or Expo starts, the configuration is
  /// reloaded or its output goes.
  pub(super) fn cancel_switcher(&mut self) {
    self.finish_switcher();
    self.close_switcher_view();
  }

  /// Drops the regular switcher's view, which deactivates the plugin.
  fn close_switcher_view(&mut self) {
    if let Some(shown) = self.switcher_view.take() {
      self.compositor.notify_plugin_activation("switcher", false);
      self.request_redraw_on(&shown.output);
    }
  }

  /// Wayfire's `pre_hook` after a frame of `output` drawn at `now`: asks
  /// for another while thumbnails move, and ends the view once they are
  /// back in place after the switcher was let go.
  pub(super) fn end_switcher_frame(
    &mut self,
    output: &OutputHandle,
    now: Instant,
  ) {
    let Some(shown) = self
      .switcher_view
      .as_mut()
      .filter(|shown| shown.output == *output)
    else {
      return;
    };
    match shown.view.frame(now) {
      FrameState::Running => self.request_redraw_on(output),
      FrameState::Settled => {}
      FrameState::Finished => self.close_switcher_view(),
    }
  }

  pub(super) fn switcher_window_destroyed(&mut self, window: TileWindowId) {
    if let Some(output) = self
      .switcher_view
      .as_ref()
      .map(|shown| shown.output.clone())
    {
      let views = self.switcher_views(&output);
      let now = Instant::now();
      let selected = self.switcher_view.as_mut().and_then(|shown| {
        shown.view.view_removed(window, &views, now);
        shown.view.selected()
      });
      self.request_redraw();
      if let Some(state) = self
        .switcher
        .as_mut()
        .filter(|state| state.kind == SwitcherKind::Regular)
      {
        state.selected = selected
          .and_then(|selected| {
            views.windows.iter().position(|window| *window == selected)
          })
          .unwrap_or(0);
        state.windows = views.windows;
        if state.windows.is_empty() {
          self.finish_switcher();
        }
        return;
      }
    }
    let Some(state) = &mut self.switcher else {
      return;
    };
    state.original_opacity.remove(&window);
    if let Some(removed) = state
      .windows
      .iter()
      .position(|candidate| *candidate == window)
    {
      state.windows.remove(removed);
      if removed < state.selected {
        state.selected -= 1;
      }
    }
    if state.windows.is_empty() {
      self.finish_switcher();
      return;
    }
    state.selected = state.selected.min(state.windows.len() - 1);
    self.apply_switcher();
  }
}

/// The layout rectangle around `window`'s surfaces and frame, or `None`
/// without a mapped buffer: Wayfire's `view_bounding_box_up_to` below
/// the switcher's transformer.
fn window_bounds(
  compositor: &Compositor,
  window: TileWindowId,
  frames: &HashMap<SurfaceId, FrameSnapshot>,
) -> Option<Rect> {
  let surfaces = compositor.window_surfaces(window);
  bounds_of(&surfaces, frames)
}

/// The rectangle around `surfaces` that have a buffer and their frames in
/// `frames`.
fn bounds_of(
  surfaces: &[SurfaceSnapshot],
  frames: &HashMap<SurfaceId, FrameSnapshot>,
) -> Option<Rect> {
  let rects = surfaces
    .iter()
    .filter(|snapshot| snapshot.buffer.is_some())
    .map(|snapshot| snapshot.geometry)
    .chain(
      surfaces
        .iter()
        .filter_map(|snapshot| frames.get(&snapshot.id))
        .map(|frame| frame.rect),
    )
    .filter(|rect| rect.width > 0 && rect.height > 0)
    .collect::<Vec<_>>();
  let left = rects.iter().map(|rect| rect.x).min()?;
  let top = rects.iter().map(|rect| rect.y).min()?;
  let right = rects
    .iter()
    .map(|rect| i64::from(rect.x) + i64::from(rect.width))
    .max()?;
  let bottom = rects
    .iter()
    .map(|rect| i64::from(rect.y) + i64::from(rect.height))
    .max()?;
  Some(Rect::new(
    left,
    top,
    u32::try_from(right - i64::from(left)).ok()?,
    u32::try_from(bottom - i64::from(top)).ok()?,
  ))
}

/// Everything the scene borrows to draw the switcher's output.
pub(super) struct SwitcherScene<'a, 'b> {
  pub(super) compositor: &'b Compositor,
  pub(super) textures: &'a HashMap<SurfaceId, GpuTexture>,
  pub(super) frame_textures: &'a FrameTextures,
  pub(super) decoration: &'b shingle_config::DecorationConfig,
  /// The frames of the surfaces drawn, by surface.
  pub(super) frames: &'b HashMap<SurfaceId, FrameSnapshot>,
  /// The output's scale, which the frames' textures are drawn at.
  pub(super) scale: f64,
  /// Whether the cursor is drawn, rather than shown on a cursor plane.
  pub(super) cursor: bool,
}

impl<'a> SwitcherScene<'a, '_> {
  /// The tiles of `shown`'s output at `now`, bottom to top, as Wayfire's
  /// `switcher_render_node_t` draws over the whole output: the background
  /// and bottom layers dimmed, every thumbnail through its projection,
  /// with its frame, then the top and overlay layers and the cursor. The
  /// output is cleared to black below them.
  pub(super) fn tiles(
    &self,
    shown: &ShownSwitcher,
    now: Instant,
  ) -> Vec<RenderTile<'a>> {
    let mut tiles = Vec::new();
    let dim = shown.view.background_dim(now) as f32;
    for snapshot in self.compositor.layer_surfaces_on(&shown.output, false) {
      if let Some(mut tile) = self.tile(&snapshot) {
        for component in &mut tile.color[..3] {
          *component *= dim;
        }
        tiles.push(tile);
      }
    }
    // Wayfire's stacking order lists the topmost first.
    let mut stacking = self.compositor.stacking_order_on(&shown.output);
    stacking.reverse();
    for (window, paint) in shown.view.thumbnails(now, &stacking) {
      let surfaces = self.compositor.window_surfaces(window);
      let Some(bounds) = bounds_of(&surfaces, self.frames) else {
        continue;
      };
      let width = f64::from(bounds.width);
      let height = f64::from(bounds.height);
      let projection = Projection {
        origin: [
          (f64::from(bounds.x) + width / 2.0) as f32,
          (f64::from(bounds.y) + height / 2.0) as f32,
        ],
        matrix: switcher_view::projection(&paint, (width, height)),
      };
      let alpha = paint.alpha.clamp(0.0, 1.0) as f32;
      for snapshot in &surfaces {
        let Some(mut tile) = self.tile(snapshot) else {
          continue;
        };
        for component in &mut tile.color {
          *component *= alpha;
        }
        if let Some(frame) = self.frames.get(&snapshot.id) {
          let mut framed = self.frame_textures.tiles(
            frame,
            tile.geometry,
            &tile,
            self.decoration,
            self.scale,
          );
          for piece in &mut framed {
            piece.projection = Some(projection);
          }
          tiles.append(&mut framed);
        }
        tile.projection = Some(projection);
        tiles.push(tile);
      }
    }
    for snapshot in self.compositor.layer_surfaces_on(&shown.output, true) {
      tiles.extend(self.tile(&snapshot));
    }
    if self.cursor
      && let Some(snapshot) = self.compositor.cursor_on(&shown.output)
    {
      tiles.extend(self.tile(&snapshot));
    }
    tiles
  }

  /// The flat tile of a surface, at its place and opacity.
  fn tile(&self, snapshot: &SurfaceSnapshot) -> Option<RenderTile<'a>> {
    let texture = self.textures.get(&snapshot.id)?;
    let geometry = snapshot.geometry;
    let mut tile = RenderTile::textured(
      RenderRect {
        x: geometry.x,
        y: geometry.y,
        width: geometry.width,
        height: geometry.height,
      },
      texture,
    );
    tile.color = [snapshot.opacity; 4];
    if let Some(buffer) = &snapshot.buffer {
      apply_buffer(&mut tile, buffer);
    }
    Some(tile)
  }
}
