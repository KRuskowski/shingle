use rustix::rand::{GetRandomFlags, getrandom};
use shingle_core::{Rect, WindowId};

use crate::{shell, state::CompositorState};

pub(crate) fn place(state: &mut CompositorState, window: WindowId) {
  let Some((geometry, fullscreen)) = state
    .core
    .window_info(window)
    .ok()
    .map(|info| (info.state.floating, info.state.fullscreen))
  else {
    return;
  };
  let Some(mut current) = geometry else {
    return;
  };
  if fullscreen {
    return;
  }
  // An X transient was centred on its parent at its requested size
  // (crates/shingle/src/xwayland/requests.rs, `float`).
  let (parent, x11) = state
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
    .unwrap_or((None, false));
  if parent.is_some() && x11 {
    return;
  }
  // The client's size, and its server-side frame's margins, which a
  // Wayfire view's geometry includes.
  let mut margins = crate::frame::Margins::ZERO;
  let natural_size = state.surfaces.iter().find_map(|(object, surface)| {
    if surface
      .role
      .toplevel()
      .is_none_or(|toplevel| toplevel.window() != Some(window))
    {
      return None;
    }
    margins = crate::decorator::margins(state, object);
    if let Some(dimensions) = crate::viewport::dimensions(state, object) {
      return Some(dimensions);
    }
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
    Some(dimensions)
  });
  if let Some((width, height)) = natural_size
    && width > 0
    && height > 0
  {
    current.width = width.saturating_add(margins.left + margins.right);
    current.height = height.saturating_add(margins.top + margins.bottom);
  }
  // The workarea of the window's own set, which a dialog shares with its
  // parent on whichever output that is.
  let workarea = state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| {
      state
        .core
        .wset_tree(info.wset, info.location.workspace)
        .ok()
    })
    .map_or(state.active().rect, |tree| tree.workarea());
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
  let placed = match state.placement_mode.as_str() {
    "cascade" => cascade(state, current, workarea),
    "random" => random(current, workarea),
    "pointer" => pointer(state, current, workarea),
    // Wayfire without its place plugin clamps the view's own position,
    // which starts at the origin, to the workarea.
    "none" => Some(Rect {
      x: workarea.x,
      y: workarea.y,
      ..current
    }),
    _ => center(current, workarea),
  };
  let Some(geometry) = placed.and_then(|geometry| geometry.validate().ok())
  else {
    return;
  };
  if state.core.set_floating(window, Some(geometry)).is_ok() {
    shell::sync_floating_geometry(state, window, geometry);
    shell::reconfigure_all(state);
  }
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
