//! Windows straddling workspaces.
//!
//! Wayfire keeps every view of a workspace set in one plane, in which
//! workspace `(x, y)` lies `(x - current.x)` output widths and
//! `(y - current.y)` output heights from the current one, with no gap, and
//! an output shows whatever of that plane lies over it
//! (wayfire:src/output/workspace-impl.cpp:501-516, `view_visible_on`). A
//! floating view that hangs over the edge of its workspace is so drawn on
//! the neighbouring workspace too, and takes input there. Shingle keeps
//! each window's rectangle on its own workspace, the one holding its
//! centre, laid over the output; the window's surfaces are drawn on the
//! workspace its output shows moved by the distance between the two
//! ([`window_shift`]), and input finds them there.

use shingle_core::{Rect, StackingMode, WindowId, WorkspaceId};
use wayland_server::backend::ObjectId;

use crate::{shell, state::CompositorState};

/// A window of another workspace drawn on the one its output shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Straddler {
  /// The window's toplevel surface.
  pub(crate) object: ObjectId,
  /// The window.
  pub(crate) window: WindowId,
  /// Its stacking layer.
  pub(crate) stacking: StackingMode,
  /// How far it is drawn from its place on its own workspace.
  pub(crate) shift: (i32, i32),
}

/// How far a window of workspace `own` is drawn from its place there while
/// its output, of `size`, shows workspace `shown`: the output's size times
/// the difference in columns and rows.
pub(crate) fn shift_between(
  own: WorkspaceId,
  shown: WorkspaceId,
  size: (u32, u32),
) -> (i32, i32) {
  let axis = |own: u32, shown: u32, size: u32| {
    let distance = (i64::from(own) - i64::from(shown)) * i64::from(size);
    distance.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
  };
  (axis(own.x, shown.x, size.0), axis(own.y, shown.y, size.1))
}

/// How far `window` is drawn from its place on its own workspace on the
/// output that shows its set: `(0, 0)` on the current workspace, for a
/// sticky window, which every workspace shows in place, and for a window
/// of a set no output shows.
pub(crate) fn window_shift(
  state: &CompositorState,
  window: WindowId,
) -> (i32, i32) {
  let Ok(info) = state.core.window_info(window) else {
    return (0, 0);
  };
  if info.state.sticky {
    return (0, 0);
  }
  let Some((wset, output)) = state.core.wset(info.wset).ok().and_then(|wset| {
    let output = state.outputs.get(wset.output.as_ref()?)?;
    Some((wset, output))
  }) else {
    return (0, 0);
  };
  shift_between(
    info.location.workspace,
    wset.current_workspace,
    (output.rect.width, output.rect.height),
  )
}

/// [`window_shift`] of the window surface `object` belongs to, through
/// popups and subsurfaces; `(0, 0)` for surfaces of no window.
pub(crate) fn surface_shift(
  state: &CompositorState,
  object: &ObjectId,
) -> (i32, i32) {
  let mut current = object.clone();
  // A parent chain is short; the bound only guards against a cycle.
  for _ in 0..64 {
    let Some(surface) = state.surfaces.get(&current) else {
      return (0, 0);
    };
    if let Some(toplevel) = surface.role.toplevel() {
      return toplevel
        .window()
        .map_or((0, 0), |window| window_shift(state, window));
    }
    current = match &surface.role {
      crate::shell::Role::Xdg {
        kind: Some(crate::shell::XdgKind::Popup(popup)),
        ..
      } => popup.parent.clone(),
      crate::shell::Role::Subsurface { parent, .. } => parent.clone(),
      _ => return (0, 0),
    };
  }
  (0, 0)
}

/// `rect` moved by `shift`, saturated.
pub(crate) fn shifted(rect: Rect, shift: (i32, i32)) -> Rect {
  Rect::new(
    rect.x.saturating_add(shift.0),
    rect.y.saturating_add(shift.1),
    rect.width,
    rect.height,
  )
}

/// Where surface `object` is drawn and takes input now:
/// [`shell::surface_rect`] moved by [`surface_shift`].
pub(crate) fn drawn_rect(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  shell::surface_rect(state, object)
    .map(|rect| shifted(rect, surface_shift(state, object)))
}

/// The windows of output `id`'s set on workspaces other than `shown`, the
/// one it shows, whose geometry, frame included, overlaps the output once
/// moved by their [`window_shift`], as `view_visible_on` decides from a
/// view's geometry. Minimized, sticky, unmapped and bufferless windows are
/// left out, and so is everything while the desktop is shown. In window
/// order.
pub(crate) fn straddlers(
  state: &CompositorState,
  id: &shingle_core::OutputId,
  shown: WorkspaceId,
) -> Vec<Straddler> {
  if state.show_desktop {
    return Vec::new();
  }
  let Some(output) = state.outputs.get(id) else {
    return Vec::new();
  };
  let size = (output.rect.width, output.rect.height);
  let mut found = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      surface.buffer.as_ref()?;
      let window = surface.role.toplevel()?.window()?;
      let info = state.core.window_info(window).ok()?;
      if info.presence != shingle_core::WindowPresence::Mapped
        || info.location.output != *id
        || info.location.workspace == shown
        || info.state.sticky
        || info.state.minimized
      {
        return None;
      }
      let shift = shift_between(info.location.workspace, shown, size);
      let geometry = shell::window_geometry(state, object)?;
      crate::surface_outputs::intersection(
        shifted(geometry, shift),
        output.rect,
      )?;
      Some(Straddler {
        object: object.clone(),
        window,
        stacking: info.state.stacking,
        shift,
      })
    })
    .collect::<Vec<_>>();
  found.sort_by_key(|straddler| straddler.window);
  found
}

/// One entry of a stacking order merged by [`merge_by_recency`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stacked<T> {
  /// A window of the workspace shown.
  Own(T),
  /// A window of another workspace drawn on it.
  Foreign(T),
}

/// `own`, bottom to top, with each of `foreign` put directly below the
/// lowest entry focused after it, or on top when none was. Wayfire stacks
/// all the views of a set's layer together and raises a view when it is
/// focused; Shingle stacks each workspace on its own, so a window drawn on
/// another workspace goes where the order of focus puts it. `own` keeps
/// its order, and the foreign windows go in the order of their focus.
pub(crate) fn merge_by_recency<T: Copy>(
  own: &[T],
  foreign: &[T],
  recency: impl Fn(T) -> u64,
) -> Vec<Stacked<T>> {
  let mut merged = own.iter().copied().map(Stacked::Own).collect::<Vec<_>>();
  if foreign.is_empty() {
    return merged;
  }
  let mut foreign = foreign.to_vec();
  // Stable: windows focused equally long ago keep their order.
  foreign.sort_by_key(|window| recency(*window));
  for window in foreign {
    let stamp = recency(window);
    let position = merged
      .iter()
      .position(|entry| {
        let (Stacked::Own(other) | Stacked::Foreign(other)) = *entry;
        recency(other) > stamp
      })
      .unwrap_or(merged.len());
    merged.insert(position, Stacked::Foreign(window));
  }
  merged
}

#[cfg(test)]
mod tests;
