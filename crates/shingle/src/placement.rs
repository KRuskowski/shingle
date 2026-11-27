use rustix::rand::{GetRandomFlags, getrandom};
use shingle_core::{Rect, WindowId};

use crate::{shell, state::CompositorState};

/// Places floating `window` at its client's size, as Wayfire's place
/// plugin places a view that maps floating
/// (wayfire:plugins/single_plugins/place.cpp), and a dialog centred on its
/// parent.
pub(crate) fn place(state: &mut CompositorState, window: WindowId) {
  let Some((geometry, fullscreen)) = state
    .core
    .window_info(window)
    .ok()
    .map(|info| (info.state.floating, info.state.fullscreen))
  else {
    return;
  };
  let Some(current) = geometry else {
    return;
  };
  if fullscreen {
    return;
  }
  // An X transient was centred on its parent at its requested size
  // (crates/shingle/src/xwayland/requests.rs, `float`).
  let (parent, x11) = toplevel_kind(state, window);
  if parent.is_some() && x11 {
    return;
  }
  let current = with_natural_size(state, window, current);
  let workarea = workarea_of(state, window);
  // A dialog is centred on its parent and kept inside the workarea,
  // whatever the placement mode, as Wayfire's
  // `reposition_relative_to_parent` places it (src/view/toplevel-view.cpp).
  if let Some(parent) = parent {
    let Some(parent_geometry) = parent_geometry(state, parent) else {
      return;
    };
    let geometry = clamp(centre_on(current, parent_geometry), workarea);
    if let Ok(geometry) = geometry.validate()
      && state.core.set_floating(window, Some(geometry)).is_ok()
    {
      shell::sync_floating_geometry(state, window, geometry);
      shell::reconfigure_all(state);
    }
    return;
  }
  if state.placement_mode == "maximize" {
    let _ = shell::set_maximized(state, window, true);
    return;
  }
  let Some(geometry) = placed(state, window, current, workarea) else {
    return;
  };
  if state.core.set_floating(window, Some(geometry)).is_ok() {
    shell::sync_floating_geometry(state, window, geometry);
    shell::reconfigure_all(state);
  }
}

/// Where `window`, which maps into its tile, would float: its client's
/// size, placed as [`place`] places a floating view. Wayfire places a view
/// in its map transaction, before simple-tile tiles it on `view-mapped`,
/// and tiling it records that geometry as the one untiling returns to
/// (wayfire:plugins/tile/tree.cpp, `view_node_t::set_geometry`, which
/// calls `update_last_windowed_geometry` while the view is not tiled yet).
/// None for a dialog, under `place/mode = maximize` or without the
/// client's size.
pub(crate) fn windowed_on_map(
  state: &mut CompositorState,
  window: WindowId,
) -> Option<Rect> {
  let (parent, _) = toplevel_kind(state, window);
  if parent.is_some() || state.placement_mode == "maximize" {
    return None;
  }
  let size = natural_size(state, window)?;
  let current = Rect::new(0, 0, size.width, size.height);
  let workarea = workarea_of(state, window);
  placed(state, window, current, workarea).and_then(|rect| rect.validate().ok())
}

/// `current`, the geometry of `window`, inside `workarea`, placed by
/// `place/mode`. As Wayfire's `adjust_view_pending_geometry_on_start_map`,
/// the view is first clamped to the workarea, size included.
fn placed(
  state: &mut CompositorState,
  window: WindowId,
  current: Rect,
  workarea: Rect,
) -> Option<Rect> {
  let current = clamp_size(current, workarea);
  match state.placement_mode.as_str() {
    "cascade" => cascade(state, current, workarea),
    "random" => random(current, workarea),
    "pointer" => pointer(state, current, workarea),
    // Wayfire without its place plugin keeps the view's own position,
    // which a toplevel's state starts at 100, 100 in its output
    // (wayfire:src/api/wayfire/toplevel.hpp, `toplevel_state_t`), clamped
    // to the workarea.
    "none" => {
      let output = state.output_of_window(window).rect;
      Some(clamp(
        Rect {
          x: output.x.saturating_add(UNPLACED_OFFSET),
          y: output.y.saturating_add(UNPLACED_OFFSET),
          ..current
        },
        workarea,
      ))
    }
    _ => center(current, workarea),
  }
}

/// Where an unplaced Wayfire view starts, from its output's corner.
const UNPLACED_OFFSET: i32 = 100;

/// `geometry` no larger than `workarea`.
fn clamp_size(geometry: Rect, workarea: Rect) -> Rect {
  Rect {
    width: geometry.width.min(workarea.width),
    height: geometry.height.min(workarea.height),
    ..geometry
  }
}

/// `window`'s toplevel's parent, and whether it is an X window.
fn toplevel_kind(
  state: &CompositorState,
  window: WindowId,
) -> (Option<WindowId>, bool) {
  state
    .surfaces
    .values()
    .find_map(|surface| {
      surface
        .role
        .toplevel()
        .filter(|toplevel| toplevel.window() == Some(window))
        .map(|toplevel| {
          (
            toplevel.parent,
            matches!(toplevel.handle, shell::ToplevelHandle::X11(_)),
          )
        })
    })
    .unwrap_or((None, false))
}

/// The size of `window`'s client in surface coordinates, from its
/// viewport or buffer, with its server-side frame's margins, which a
/// Wayfire view's geometry includes; None without a buffer.
fn natural_size(state: &CompositorState, window: WindowId) -> Option<Rect> {
  state.surfaces.iter().find_map(|(object, surface)| {
    if surface
      .role
      .toplevel()
      .is_none_or(|toplevel| toplevel.window() != Some(window))
    {
      return None;
    }
    let margins = crate::decorator::margins(state, object);
    let (width, height) = match crate::viewport::dimensions(state, object) {
      Some(dimensions) => dimensions,
      None => {
        let buffer = surface.buffer.as_ref()?;
        let scale = buffer.scale.max(1) as u32;
        let mut dimensions = (buffer.width / scale, buffer.height / scale);
        if matches!(
          buffer.transform,
          wayland_server::protocol::wl_output::Transform::Flipped90
            | wayland_server::protocol::wl_output::Transform::Flipped270
            | wayland_server::protocol::wl_output::Transform::_90
            | wayland_server::protocol::wl_output::Transform::_270
        ) {
          std::mem::swap(&mut dimensions.0, &mut dimensions.1);
        }
        dimensions
      }
    };
    (width > 0 && height > 0).then(|| {
      Rect::new(
        0,
        0,
        width.saturating_add(margins.left + margins.right),
        height.saturating_add(margins.top + margins.bottom),
      )
    })
  })
}

/// `current` with the size of `window`'s client, if it has one.
fn with_natural_size(
  state: &CompositorState,
  window: WindowId,
  current: Rect,
) -> Rect {
  natural_size(state, window).map_or(current, |size| Rect {
    width: size.width,
    height: size.height,
    ..current
  })
}

/// The workarea of `window`'s own set, which a dialog shares with its
/// parent on whichever output that is.
fn workarea_of(state: &CompositorState, window: WindowId) -> Rect {
  state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| {
      state
        .core
        .wset_tree(info.wset, info.location.workspace)
        .ok()
    })
    .map_or(state.active().rect, |tree| tree.workarea())
}

/// The geometry of `parent`'s toplevel, frame included.
fn parent_geometry(state: &CompositorState, parent: WindowId) -> Option<Rect> {
  let object = state.surfaces.iter().find_map(|(object, surface)| {
    surface
      .role
      .toplevel()
      .is_some_and(|toplevel| toplevel.window() == Some(parent))
      .then(|| object.clone())
  })?;
  shell::window_geometry(state, &object)
}

/// `geometry`'s size centred on `parent`.
fn centre_on(geometry: Rect, parent: Rect) -> Rect {
  let half = |outer: u32, inner: u32| (i64::from(outer) - i64::from(inner)) / 2;
  let to_i32 = |value: i64| {
    i32::try_from(value.clamp(i32::MIN.into(), i32::MAX.into()))
      .unwrap_or_default()
  };
  Rect {
    x: to_i32(i64::from(parent.x) + half(parent.width, geometry.width)),
    y: to_i32(i64::from(parent.y) + half(parent.height, geometry.height)),
    ..geometry
  }
}

/// `geometry` shrunk to fit `workarea` and moved inside it, as
/// `wf::clamp` does.
fn clamp(geometry: Rect, workarea: Rect) -> Rect {
  let width = geometry.width.min(workarea.width);
  let height = geometry.height.min(workarea.height);
  let fit = |position: i32, size: u32, start: i32, outer: u32| {
    let end = i64::from(start) + i64::from(outer) - i64::from(size);
    i32::try_from(i64::from(position).clamp(i64::from(start), end))
      .unwrap_or(start)
  };
  Rect {
    x: fit(geometry.x, width, workarea.x, workarea.width),
    y: fit(geometry.y, height, workarea.y, workarea.height),
    width,
    height,
  }
}

fn center(mut geometry: Rect, workarea: Rect) -> Option<Rect> {
  geometry.x = i32::try_from(
    i64::from(workarea.x)
      + (i64::from(workarea.width) - i64::from(geometry.width)) / 2,
  )
  .ok()?;
  geometry.y = i32::try_from(
    i64::from(workarea.y)
      + (i64::from(workarea.height) - i64::from(geometry.height)) / 2,
  )
  .ok()?;
  Some(geometry)
}

fn cascade(
  state: &mut CompositorState,
  mut geometry: Rect,
  workarea: Rect,
) -> Option<Rect> {
  let right = i64::from(workarea.x) + i64::from(workarea.width);
  let bottom = i64::from(workarea.y) + i64::from(workarea.height);
  let (mut x, mut y) = state.cascade_position;
  if i64::from(x) < i64::from(workarea.x) || i64::from(x) > right {
    x = workarea.x;
  }
  if i64::from(y) < i64::from(workarea.y) || i64::from(y) > bottom {
    y = workarea.y;
  }
  if i64::from(x) + i64::from(geometry.width) > right
    || i64::from(y) + i64::from(geometry.height) > bottom
  {
    x = workarea.x;
    y = workarea.y;
  }
  geometry.x = x;
  geometry.y = y;
  state.cascade_position = (
    i32::try_from(i64::from(x) + (f64::from(workarea.width) * 0.03) as i64)
      .ok()?,
    i32::try_from(i64::from(y) + (f64::from(workarea.height) * 0.03) as i64)
      .ok()?,
  );
  Some(geometry)
}

fn random(mut geometry: Rect, workarea: Rect) -> Option<Rect> {
  let width = i64::from(workarea.width) - i64::from(geometry.width);
  let height = i64::from(workarea.height) - i64::from(geometry.height);
  if width <= 0 || height <= 0 {
    return center(geometry, workarea);
  }
  let mut bytes = [0_u8; 16];
  if getrandom(&mut bytes[..], GetRandomFlags::empty()).is_err() {
    return center(geometry, workarea);
  }
  let x = u64::from_ne_bytes(bytes[..8].try_into().ok()?) % width as u64;
  let y = u64::from_ne_bytes(bytes[8..].try_into().ok()?) % height as u64;
  geometry.x = i32::try_from(i64::from(workarea.x) + x as i64).ok()?;
  geometry.y = i32::try_from(i64::from(workarea.y) + y as i64).ok()?;
  Some(geometry)
}

fn pointer(
  state: &CompositorState,
  mut geometry: Rect,
  workarea: Rect,
) -> Option<Rect> {
  let max_x = (f64::from(workarea.width) - f64::from(geometry.width)).max(0.0);
  let max_y =
    (f64::from(workarea.height) - f64::from(geometry.height)).max(0.0);
  let x =
    (state.input.pointer_x - f64::from(geometry.width) / 2.0).clamp(0.0, max_x);
  let y = (state.input.pointer_y - f64::from(geometry.height) / 2.0)
    .clamp(0.0, max_y);
  geometry.x = i32::try_from(workarea.x as i64 + x as i64).ok()?;
  geometry.y = i32::try_from(workarea.y as i64 + y as i64).ok()?;
  Some(geometry)
}
