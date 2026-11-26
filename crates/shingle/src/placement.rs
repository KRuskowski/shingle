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
  let has_parent = state.surfaces.values().any(|surface| {
    surface.role.toplevel().is_some_and(|toplevel| {
      toplevel.window() == Some(window) && toplevel.parent.is_some()
    })
  });
  if has_parent {
    return;
  }
  let natural_size = state.surfaces.iter().find_map(|(object, surface)| {
    if surface
      .role
      .toplevel()
      .is_none_or(|toplevel| toplevel.window() != Some(window))
    {
      return None;
    }
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
    current.width = width;
    current.height = height;
  }
  let workarea = state
    .core
    .current_workspace(&state.active_output)
    .ok()
    .and_then(|workspace| state.core.tree(&state.active_output, workspace).ok())
    .map_or(state.active().rect, |tree| tree.workarea());
  if state.placement_mode == "maximize" {
    let _ = shell::set_maximized(state, window, true);
    return;
  }
  let placed = match state.placement_mode.as_str() {
    "cascade" => cascade(state, current, workarea),
    "random" => random(current, workarea),
    "pointer" => pointer(state, current, workarea),
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
