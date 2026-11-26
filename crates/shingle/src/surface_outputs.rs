//! Which outputs each surface is on (docs/design/multi-output.md, sections
//! 7 and 8): `wl_surface.enter` and `leave` follow the outputs a surface
//! is visible on, its primary output paces its presentation feedback,
//! FIFO barriers and commit-timing deadlines, and its preferred scales
//! are those of the output it belongs to.
//!
//! A toplevel is on its workspace set's output while that output shows
//! the set, its workspace is the current one (or it is sticky) and it is
//! not minimized, since Shingle clips windows to their own output as
//! Wayfire does; with `remove_output_limits` it is also on every other
//! output it overlaps while it is shown. Layer and lock surfaces are on
//! their output; an unmanaged X window is on every output it overlaps;
//! the cursor is on the output under the pointer; popups and subsurfaces
//! are on their parent's outputs. [`sync`] runs once per turn and sends
//! what changed.

use std::collections::HashMap;

use shingle_core::{OutputId, Rect, WindowPresence};
use wayland_server::{
  Resource, backend::ObjectId, protocol::wl_output::WlOutput,
};

use crate::{
  shell::{self, Role},
  state::CompositorState,
};

/// How deep popups and subsurfaces are followed to their root.
const MAX_DEPTH: usize = 32;

/// What one surface was last told.
struct Tracked {
  /// The `wl_output`s it entered.
  entered: HashMap<ObjectId, WlOutput>,
  /// Its primary output, once it was visible somewhere.
  primary: Option<OutputId>,
  /// The `wl_surface.preferred_buffer_scale` last sent; 1 until then.
  buffer_scale: i32,
  /// The `wp_fractional_scale_v1.preferred_scale` last sent, in 120ths.
  fractional: Option<u32>,
}

impl Default for Tracked {
  fn default() -> Self {
    Self {
      entered: HashMap::new(),
      primary: None,
      buffer_scale: 1,
      fractional: None,
    }
  }
}

/// The outputs of every surface, as last sent.
#[derive(Default)]
pub(crate) struct SurfaceOutputs {
  surfaces: HashMap<ObjectId, Tracked>,
}

/// The intersection of two rectangles, if they share a pixel.
pub(crate) fn intersection(first: Rect, second: Rect) -> Option<Rect> {
  let left = first.x.max(second.x);
  let top = first.y.max(second.y);
  let right = (first.x.saturating_add(first.width as i32))
    .min(second.x.saturating_add(second.width as i32));
  let bottom = (first.y.saturating_add(first.height as i32))
    .min(second.y.saturating_add(second.height as i32));
  if right <= left || bottom <= top {
    return None;
  }
  Some(Rect::new(
    left,
    top,
    (right - left) as u32,
    (bottom - top) as u32,
  ))
}

/// The area of `rect` in pixels.
fn area(rect: Rect) -> u64 {
  u64::from(rect.width) * u64::from(rect.height)
}

/// The primary output of a surface at `rect` among `outputs`, the outputs
/// it is on with their rectangles, as wlroots' scene chooses it: the one
/// it overlaps most, if that is at least a tenth of its area. Otherwise,
/// or for a surface without a rectangle, `previous` stays if the surface
/// is still on it, else the first output it is on. A surface on no output
/// keeps `previous`.
pub(crate) fn choose_primary(
  rect: Option<Rect>,
  outputs: &[(OutputId, Rect)],
  previous: Option<&OutputId>,
) -> Option<OutputId> {
  if outputs.is_empty() {
    return previous.cloned();
  }
  let kept = previous
    .filter(|previous| outputs.iter().any(|(id, _)| id == *previous))
    .cloned();
  let Some(rect) = rect.filter(|rect| area(*rect) > 0) else {
    return kept.or_else(|| Some(outputs[0].0.clone()));
  };
  let mut best: Option<(&OutputId, u64)> = None;
  for (id, output) in outputs {
    let overlap = intersection(rect, *output).map_or(0, area);
    if best.is_none_or(|(_, most)| overlap > most) {
      best = Some((id, overlap));
    }
  }
  match best {
    Some((id, overlap)) if overlap * 10 >= area(rect) => Some(id.clone()),
    _ => kept.or_else(|| Some(outputs[0].0.clone())),
  }
}

/// The output under the pointer, else the first output.
pub(crate) fn pointer_output(state: &CompositorState) -> OutputId {
  output_at(state, state.input.pointer_x, state.input.pointer_y)
    .unwrap_or_else(|| state.primary().id.clone())
}

/// The output that is not dark and holds layout point (`x`, `y`), the
/// first in add order where outputs overlap.
pub(crate) fn output_at(
  state: &CompositorState,
  x: f64,
  y: f64,
) -> Option<OutputId> {
  state
    .outputs
    .iter()
    .filter(|output| !output.dark)
    .find(|output| {
      let rect = output.rect;
      x >= f64::from(rect.x)
        && y >= f64::from(rect.y)
        && x < f64::from(rect.x) + f64::from(rect.width)
        && y < f64::from(rect.y) + f64::from(rect.height)
    })
    .map(|output| output.id.clone())
}

/// The output an unmanaged X window at `rect` belongs to: the one under
/// its top-left corner, else the active one
/// (docs/design/multi-output.md, section 9).
pub(crate) fn unmanaged_output(
  state: &CompositorState,
  rect: Rect,
) -> OutputId {
  output_at(state, f64::from(rect.x), f64::from(rect.y))
    .unwrap_or_else(|| state.active().id.clone())
}

/// The output whose workspace set holds toplevel `object`'s window and
/// shows it now, if any.
pub(crate) fn toplevel_output(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<OutputId> {
  let surface = state.surfaces.get(object)?;
  let window = surface.role.toplevel()?.window()?;
  let info = state.core.window_info(window).ok()?;
  let output = info.location.output.clone();
  let shown = state
    .outputs
    .get(&output)
    .is_some_and(|output| !output.dark)
    && state.core.wset_of(&output) == Some(info.wset);
  let current = state.core.current_workspace(&output).ok()?;
  (shown
    && info.presence == WindowPresence::Mapped
    && !info.state.minimized
    && (info.state.sticky || info.location.workspace == current))
    .then_some(output)
}

/// The parent of a popup or subsurface.
fn parent(state: &CompositorState, object: &ObjectId) -> Option<ObjectId> {
  let surface = state.surfaces.get(object)?;
  match &surface.role {
    Role::Subsurface { parent, .. } => Some(parent.clone()),
    role => role.popup().map(|popup| popup.parent.clone()),
  }
}

/// The outputs surface `object` is visible on, in add order.
pub(crate) fn outputs_of(
  state: &CompositorState,
  object: &ObjectId,
) -> Vec<OutputId> {
  outputs_at_depth(state, object, 0)
}

fn outputs_at_depth(
  state: &CompositorState,
  object: &ObjectId,
  depth: usize,
) -> Vec<OutputId> {
  let Some(surface) = state.surfaces.get(object) else {
    return Vec::new();
  };
  if surface.role.toplevel().is_some() {
    let own = toplevel_output(state, object);
    if !state.remove_output_limits {
      return own.into_iter().collect();
    }
    // Drawn, and so entered, on every output it overlaps.
    let Some(geometry) = own
      .as_ref()
      .and_then(|_| shell::surface_geometry(state, object))
    else {
      return own.into_iter().collect();
    };
    return state
      .outputs
      .iter()
      .filter(|output| {
        Some(&output.id) == own.as_ref()
          || (!output.dark && intersection(geometry, output.rect).is_some())
      })
      .map(|output| output.id.clone())
      .collect();
  }
  match &surface.role {
    Role::Layer(layer) => vec![layer.output.clone()],
    Role::SessionLock { output, .. } => vec![output.clone()],
    Role::InputPanel(_) => vec![state.primary().id.clone()],
    Role::Cursor => {
      if state.input.cursor_surface.as_ref() == Some(object) {
        vec![pointer_output(state)]
      } else {
        Vec::new()
      }
    }
    Role::Xwayland(_) => shell::surface_geometry(state, object)
      .map(|rect| {
        state
          .outputs
          .iter()
          .filter(|output| {
            !output.dark && intersection(rect, output.rect).is_some()
          })
          .map(|output| output.id.clone())
          .collect()
      })
      .unwrap_or_default(),
    _ if depth < MAX_DEPTH => match parent(state, object) {
      Some(parent) if parent != *object => {
        outputs_at_depth(state, &parent, depth + 1)
      }
      _ => Vec::new(),
    },
    _ => Vec::new(),
  }
}

/// The output whose scale surface `object` should draw at: a window's
/// set's output (else the active one), a layer or lock surface's output,
/// a popup's or subsurface's parent's, else the first output it is on.
/// `None` for a surface without any, such as a closed layer surface.
pub(crate) fn scale_output(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<OutputId> {
  let mut current = object.clone();
  for _ in 0..MAX_DEPTH {
    let Some(surface) = state.surfaces.get(&current) else {
      break;
    };
    if let Some(window) = surface.role.toplevel().and_then(|t| t.window()) {
      return Some(state.output_of_window(window).id.clone());
    }
    match &surface.role {
      Role::Layer(layer) => return Some(layer.output.clone()),
      Role::SessionLock { output, .. } => return Some(output.clone()),
      Role::Cursor => return Some(pointer_output(state)),
      _ => {}
    }
    match parent(state, &current) {
      Some(next) if next != current => current = next,
      _ => break,
    }
  }
  outputs_of(state, object).into_iter().next()
}

/// The primary output of surface `object`, whose frames deliver its
/// presentation feedback and release its FIFO barriers and commit-timing
/// deadlines: the one last chosen while it was visible, else the first
/// output.
pub(crate) fn primary_of(
  state: &CompositorState,
  object: &ObjectId,
) -> OutputId {
  state
    .surface_outputs
    .surfaces
    .get(object)
    .and_then(|tracked| tracked.primary.clone())
    .filter(|id| state.outputs.get(id).is_some_and(|output| !output.dark))
    .unwrap_or_else(|| state.primary().id.clone())
}

/// What one surface should be told now.
struct Wanted {
  outputs: Vec<OutputId>,
  rect: Option<Rect>,
  /// The scale to prefer, or `None` to keep what was sent.
  scale: Option<f64>,
}

fn wanted(state: &CompositorState, object: &ObjectId) -> Wanted {
  let outputs = outputs_of(state, object);
  let scale = scale_output(state, object)
    .and_then(|id| state.outputs.get(&id))
    .map(|output| output.scale);
  Wanted {
    outputs,
    rect: shell::surface_geometry(state, object),
    scale,
  }
}

/// Sends every surface what changed since the last turn: `enter` for the
/// bound `wl_output`s of the outputs it is now on, `leave` for those it
/// left (also when an output went), its preferred scales, and records its
/// primary output.
pub(crate) fn sync(state: &mut CompositorState) {
  let surfaces = &state.surfaces;
  state
    .surface_outputs
    .surfaces
    .retain(|object, _| surfaces.contains_key(object));
  let objects = state.surfaces.keys().cloned().collect::<Vec<_>>();
  for object in objects {
    let wanted = wanted(state, &object);
    let Some(surface) = state.surfaces.get(&object) else {
      continue;
    };
    let client = surface.client.clone();
    let resource = surface.resource.clone();
    let rects = wanted
      .outputs
      .iter()
      .filter_map(|id| {
        state
          .outputs
          .get(id)
          .map(|output| (id.clone(), output.rect))
      })
      .collect::<Vec<_>>();
    let bound = state
      .outputs
      .iter()
      .filter(|output| wanted.outputs.contains(&output.id))
      .flat_map(|output| output.bound.iter())
      .filter(|(_, output)| {
        output.client().is_some_and(|owner| owner.id() == client)
      })
      .map(|(id, output)| (id.clone(), output.clone()))
      .collect::<HashMap<_, _>>();
    let fractional = state.fractional_scales.get(&object).cloned();
    let tracked = state
      .surface_outputs
      .surfaces
      .entry(object.clone())
      .or_default();
    tracked.entered.retain(|id, output| {
      if bound.contains_key(id) {
        return true;
      }
      if output.is_alive() {
        resource.leave(output);
      }
      false
    });
    for (id, output) in bound {
      if let std::collections::hash_map::Entry::Vacant(entry) =
        tracked.entered.entry(id)
      {
        resource.enter(&output);
        entry.insert(output);
      }
    }
    tracked.primary =
      choose_primary(wanted.rect, &rects, tracked.primary.as_ref());
    // A surface without an output of its own, such as one that has no
    // role yet, is first told the first output's scale.
    let Some(scale) = wanted.scale.or_else(|| {
      (fractional.is_some() && tracked.fractional.is_none())
        .then(|| state.outputs.first().scale)
    }) else {
      continue;
    };
    let buffer_scale = scale.ceil().clamp(1.0, i32::MAX as f64) as i32;
    if resource.version() >= 6 && buffer_scale != tracked.buffer_scale {
      resource.preferred_buffer_scale(buffer_scale);
      tracked.buffer_scale = buffer_scale;
    }
    let preferred = (scale * 120.0).round().clamp(1.0, u32::MAX as f64) as u32;
    if let Some(fractional) = fractional
      && tracked.fractional != Some(preferred)
    {
      fractional.preferred_scale(preferred);
      tracked.fractional = Some(preferred);
    }
  }
}

/// Forgets what surface `object` was told about its fractional scale, for
/// a new `wp_fractional_scale_v1`, which is told again.
pub(crate) fn fractional_scale_created(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  if let Some(tracked) = state.surface_outputs.surfaces.get_mut(object) {
    tracked.fractional = None;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn id(name: &str) -> OutputId {
    OutputId::new(name)
  }

  #[test]
  fn the_primary_output_is_the_largest_overlap_above_a_tenth() {
    let outputs = [
      (id("A"), Rect::new(0, 0, 100, 100)),
      (id("B"), Rect::new(100, 0, 100, 100)),
    ];
    let left = Rect::new(50, 0, 60, 10);
    assert_eq!(choose_primary(Some(left), &outputs, None), Some(id("A")));
    let right = Rect::new(80, 0, 100, 10);
    assert_eq!(
      choose_primary(Some(right), &outputs, Some(&id("A"))),
      Some(id("B"))
    );
    // 2.5 % on A and on B, the rest outside: the previous one stays.
    let mostly_outside = Rect::new(95, -190, 10, 200);
    assert_eq!(
      choose_primary(Some(mostly_outside), &outputs, Some(&id("B"))),
      Some(id("B"))
    );
    assert_eq!(
      choose_primary(Some(mostly_outside), &outputs, None),
      Some(id("A"))
    );
  }

  #[test]
  fn a_surface_on_no_output_keeps_its_primary_output() {
    assert_eq!(
      choose_primary(Some(Rect::new(0, 0, 1, 1)), &[], Some(&id("B"))),
      Some(id("B"))
    );
    assert_eq!(choose_primary(None, &[], None), None);
    let outputs = [(id("A"), Rect::new(0, 0, 10, 10))];
    assert_eq!(choose_primary(None, &outputs, None), Some(id("A")));
    assert_eq!(
      choose_primary(None, &outputs, Some(&id("gone"))),
      Some(id("A"))
    );
  }

  #[test]
  fn rectangles_intersect_only_when_they_share_a_pixel() {
    let first = Rect::new(0, 0, 10, 10);
    assert_eq!(
      intersection(first, Rect::new(5, 5, 10, 10)),
      Some(Rect::new(5, 5, 5, 5))
    );
    assert_eq!(intersection(first, Rect::new(10, 0, 10, 10)), None);
    assert_eq!(intersection(first, Rect::new(-5, -5, 5, 5)), None);
  }
}
