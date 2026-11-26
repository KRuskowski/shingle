//! Server-side decorations as Wayfire's `decoration` plugin gives them
//! (wayfire:plugins/decor/decoration.cpp and deco-subsurface.cpp): which
//! toplevels have a frame, the margins between a window's geometry and its
//! client, the frames the frontend draws, and pointer input on them.
//!
//! A decorated window's geometry is its frame, as in Wayfire: tiles,
//! floating rectangles, IPC's `geometry` and sessions describe the frame,
//! and the client is configured with that rectangle shrunk by the margins
//! (`shell::configure_toplevel`). A fullscreen window has no frame.

use shingle_core::{Rect, WindowId};
use wayland_server::backend::ObjectId;

use crate::{
  Event,
  cursor_shape::Shape,
  frame::{FrameAction, FrameInput, FrameLayout, FrameSnapshot, Margins},
  shell::{self, ToplevelHandle},
  state::{CompositorState, SurfaceId},
};

/// What the compositor keeps about the frame under the pointer.
#[derive(Default)]
pub(crate) struct FramePointer {
  /// The decorated window whose frame the pointer is on, with the
  /// pointer's state there.
  pub(crate) input: Option<(WindowId, FrameInput)>,
  /// The resize cursor the frame shows, when the compositor draws the
  /// cursor.
  pub(crate) cursor: Option<Shape>,
}

/// Whether the toplevel on `object` should have a frame now, Wayfire's
/// `should_decorate_view`: the plugin is loaded, `ignore_views` does not
/// match it, and `forced_views` matches it or its client asked for
/// server-side decorations.
fn wanted(state: &CompositorState, object: &ObjectId) -> bool {
  if !state.decorations_enabled {
    return false;
  }
  let Some(toplevel) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
  else {
    return false;
  };
  let properties = crate::window_rules::view_properties(
    state,
    toplevel.window,
    &toplevel.identity,
  );
  let config = &state.decoration;
  !config.ignore_views.matches_view(&properties)
    && (config.forced_views.matches_view(&properties)
      || !toplevel.client_decorated)
}

/// The margins of the toplevel on `object`: its frame's when it is
/// decorated and not fullscreen, as `simple_decorator_t::get_margins`.
pub(crate) fn margins(state: &CompositorState, object: &ObjectId) -> Margins {
  let Some(toplevel) = state
    .surfaces
    .get(object)
    .and_then(|surface| surface.role.toplevel())
  else {
    return Margins::ZERO;
  };
  let fullscreen = toplevel.window().is_some_and(|window| {
    state
      .core
      .window_info(window)
      .is_ok_and(|info| info.state.fullscreen)
  });
  if toplevel.decorated == Some(true) && !fullscreen {
    state.frame_theme.margins()
  } else {
    Margins::ZERO
  }
}

/// Decides whether the toplevel on `object` has a frame. Run for every
/// toplevel whenever Shingle reconfigures its windows. The first decision
/// changes no geometry, since a new window's rectangle is Shingle's to
/// choose; later, a floating window that gains or loses its frame keeps
/// its client where it is, its rectangle grown into the workarea or
/// shrunk, as Wayfire's `adjust_new_decorations` and `remove_decoration`
/// do. Tiled, maximized and fullscreen windows keep their rectangle.
pub(crate) fn refresh(state: &mut CompositorState, object: &ObjectId) {
  let wanted = wanted(state, object);
  let theme_margins = state.frame_theme.margins();
  let Some(toplevel) = state
    .surfaces
    .get_mut(object)
    .and_then(|surface| surface.role.toplevel_mut())
  else {
    return;
  };
  let previous = toplevel.decorated.replace(wanted);
  if previous.is_none_or(|previous| previous == wanted) {
    return;
  }
  let window = toplevel.window();
  let tiled = toplevel.tiled;
  let floating = toplevel.floating_geometry;
  let Some(window) = window else {
    return;
  };
  state.events.push(Event::DecorationChanged { window });
  let Ok(info) = state.core.window_info(window) else {
    return;
  };
  if tiled || info.state.maximized || info.state.fullscreen {
    return;
  }
  let geometry = if wanted {
    let workarea = state
      .core
      .tree(&info.location.output, info.location.workspace)
      .map_or(state.active().rect, |tree| tree.workarea());
    clamp(theme_margins.expand(floating), workarea)
  } else {
    theme_margins.shrink(floating)
  };
  if let Some(toplevel) = state
    .surfaces
    .get_mut(object)
    .and_then(|surface| surface.role.toplevel_mut())
  {
    toplevel.floating_geometry = geometry;
  }
  let _ = state.core.set_floating(window, Some(geometry));
  state.events.push(Event::LayoutChanged);
}

/// `rect` moved, and shrunk if it must be, into `area`, as Wayfire's
/// `wf::clamp`.
fn clamp(rect: Rect, area: Rect) -> Rect {
  let width = rect.width.min(area.width);
  let height = rect.height.min(area.height);
  let max_x = i64::from(area.x) + i64::from(area.width) - i64::from(width);
  let max_y = i64::from(area.y) + i64::from(area.height) - i64::from(height);
  let x = i64::from(rect.x).clamp(i64::from(area.x), max_x);
  let y = i64::from(rect.y).clamp(i64::from(area.y), max_y);
  Rect::new(x as i32, y as i32, width, height)
}

/// Decides at once whether a new X toplevel has a frame, and grows the
/// rectangle its client asked for by the frame, as Wayfire's
/// `configure_request` expands a client's geometry by the margins.
pub(crate) fn decide_x_toplevel(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  let wanted = wanted(state, object);
  let margins = state.frame_theme.margins();
  if let Some(toplevel) = state
    .surfaces
    .get_mut(object)
    .and_then(|surface| surface.role.toplevel_mut())
    && toplevel.decorated.is_none()
  {
    toplevel.decorated = Some(wanted);
    if wanted {
      toplevel.floating_geometry = margins.expand(toplevel.floating_geometry);
    }
  }
}

/// Sets whether the client of the toplevel on `object` draws its own
/// decorations, after an xdg-decoration mode was acknowledged or an X
/// window's `_MOTIF_WM_HINTS` changed, and reconfigures when that may
/// change its frame.
pub(crate) fn set_client_decorated(
  state: &mut CompositorState,
  object: &ObjectId,
  client_decorated: bool,
) {
  let Some(toplevel) = state
    .surfaces
    .get_mut(object)
    .and_then(|surface| surface.role.toplevel_mut())
  else {
    return;
  };
  if toplevel.client_decorated == client_decorated {
    return;
  }
  toplevel.client_decorated = client_decorated;
  shell::reconfigure_all(state);
}

/// The frame of the toplevel on `object`, if it has one: decorated, a
/// window, and not fullscreen.
pub(crate) fn frame(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<FrameSnapshot> {
  let toplevel = state.surfaces.get(object)?.role.toplevel()?;
  let window = toplevel.window()?;
  if toplevel.decorated != Some(true) {
    return None;
  }
  let info = state.core.window_info(window).ok()?;
  if info.state.fullscreen {
    return None;
  }
  let rect = shell::window_geometry(state, object)?;
  // Wayfire's borders resize unless the view is tiled on every edge.
  let allow_resize = info.state.tiled_edges != 15
    && !info.state.maximized
    && !(toplevel.tiled && info.state.floating.is_none());
  let layout = FrameLayout::new(
    &state.frame_theme,
    rect.width,
    rect.height,
    toplevel.parent.is_some(),
    allow_resize,
  );
  let (hovered, pressed) = match &state.frame_pointer.input {
    Some((pointer_window, input)) if *pointer_window == window => {
      (input.hovered(&layout), input.pressed())
    }
    _ => (None, None),
  };
  Some(FrameSnapshot {
    window,
    rect,
    margins: state.frame_theme.margins(),
    activated: shell::activated_window(state) == Some(window),
    title: toplevel.title.clone(),
    layout,
    hovered,
    pressed,
  })
}

/// The frame of `window`, found by its toplevel.
fn frame_of_window(
  state: &CompositorState,
  window: WindowId,
) -> Option<FrameSnapshot> {
  let object = state.surfaces.iter().find_map(|(object, surface)| {
    surface
      .role
      .toplevel()
      .is_some_and(|toplevel| toplevel.window() == Some(window))
      .then(|| object.clone())
  })?;
  frame(state, &object)
}

/// Every frame, by the surface of its toplevel.
pub(crate) fn frames(
  state: &CompositorState,
) -> std::collections::HashMap<SurfaceId, FrameSnapshot> {
  state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| Some((surface.id, frame(state, object)?)))
    .collect()
}

/// The window whose frame is at layout point (`x`, `y`), if the toplevel
/// on `object` has a frame there.
pub(crate) fn frame_at(
  state: &CompositorState,
  object: &ObjectId,
  x: f64,
  y: f64,
) -> Option<WindowId> {
  let frame = frame(state, object)?;
  frame
    .layout
    .contains(x - f64::from(frame.rect.x), y - f64::from(frame.rect.y))
    .then_some(frame.window)
}

/// What the frame of `window` shows of the pointer.
fn shown(
  state: &CompositorState,
  window: WindowId,
) -> (Option<usize>, Option<usize>) {
  frame_of_window(state, window)
    .map_or((None, None), |frame| (frame.hovered, frame.pressed))
}

/// The pointer moved to layout point (`x`, `y`), which is on the frame
/// of `window` or on none. Leaving a frame drops a press on it, as
/// Wayfire's `handle_pointer_leave`; motion on one may start a move.
pub(crate) fn pointer_moved(
  state: &mut CompositorState,
  window: Option<WindowId>,
  x: f64,
  y: f64,
) {
  let previous = state
    .frame_pointer
    .input
    .as_ref()
    .map(|(window, _)| *window);
  if previous != window {
    if let Some(previous) = previous {
      let before = shown(state, previous);
      if let Some((_, input)) = state.frame_pointer.input.as_mut() {
        input.focus_lost();
      }
      if shown(state, previous) != before {
        state
          .events
          .push(Event::DecorationChanged { window: previous });
      }
    }
    state.frame_pointer.input = None;
    state.frame_pointer.cursor = None;
  }
  let Some(window) = window else {
    return;
  };
  let Some(frame) = frame_of_window(state, window) else {
    return;
  };
  let before = (frame.hovered, frame.pressed);
  let (local_x, local_y) =
    (x - f64::from(frame.rect.x), y - f64::from(frame.rect.y));
  let (_, input) = state
    .frame_pointer
    .input
    .get_or_insert_with(|| (window, FrameInput::default()));
  let action = input.motion(&frame.layout, local_x, local_y);
  let after = (input.hovered(&frame.layout), input.pressed());
  state.frame_pointer.cursor = resize_cursor(input.cursor_edges(&frame.layout));
  if after != before {
    state.events.push(Event::DecorationChanged { window });
  }
  run(state, window, action);
}

/// A pointer button was pressed or released. Returns whether it was on a
/// frame, in which case no client gets it.
pub(crate) fn pointer_button(
  state: &mut CompositorState,
  button: u32,
  pressed: bool,
) -> bool {
  let Some(window) = state
    .frame_pointer
    .input
    .as_ref()
    .map(|(window, _)| *window)
  else {
    return false;
  };
  let Some(frame) = frame_of_window(state, window) else {
    state.frame_pointer.input = None;
    state.frame_pointer.cursor = None;
    return false;
  };
  let now = crate::input::event_time(state);
  let before = (frame.hovered, frame.pressed);
  let Some((_, input)) = state.frame_pointer.input.as_mut() else {
    return false;
  };
  let action = input.button(&frame.layout, pressed, button, now);
  if (input.hovered(&frame.layout), input.pressed()) != before {
    state.events.push(Event::DecorationChanged { window });
  }
  run(state, window, action);
  true
}

/// Does what a frame asked for, as `simple_decoration_node_t`'s
/// `handle_action`: a move or resize is the frontend's drag, as for an
/// `xdg_toplevel` request.
fn run(state: &mut CompositorState, window: WindowId, action: FrameAction) {
  match action {
    FrameAction::None => {}
    FrameAction::Move => state.events.push(Event::BeginMove { window }),
    FrameAction::Resize(edges) => {
      state.events.push(Event::BeginResize { window, edges })
    }
    FrameAction::Close => {
      shell::close_window(state, window);
    }
    FrameAction::ToggleMaximize => {
      let _ = crate::input::toggle_maximized(state, window);
    }
    FrameAction::Minimize => {
      let _ = shell::set_minimized(state, window, true);
    }
  }
}

/// The cursor of resize `edges`, as wlroots' `wlr_xcursor_get_resize_name`.
fn resize_cursor(edges: u32) -> Option<Shape> {
  const TOP: u32 = 1;
  const BOTTOM: u32 = 2;
  const LEFT: u32 = 4;
  const RIGHT: u32 = 8;
  Some(match edges {
    TOP => Shape::NResize,
    BOTTOM => Shape::SResize,
    LEFT => Shape::WResize,
    RIGHT => Shape::EResize,
    edges if edges == TOP | LEFT => Shape::NwResize,
    edges if edges == TOP | RIGHT => Shape::NeResize,
    edges if edges == BOTTOM | LEFT => Shape::SwResize,
    edges if edges == BOTTOM | RIGHT => Shape::SeResize,
    _ => return None,
  })
}

/// The `xdg_toplevel` id of the toplevel on `object`, for its
/// xdg-decoration objects.
pub(crate) fn xdg_toplevel_id(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<ObjectId> {
  match &state.surfaces.get(object)?.role.toplevel()?.handle {
    ToplevelHandle::Xdg(toplevel) => {
      Some(wayland_server::Resource::id(toplevel))
    }
    ToplevelHandle::X11(_) => None,
  }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod unit_tests {
  use super::*;

  #[test]
  fn clamping_keeps_a_frame_inside_the_workarea() {
    let area = Rect::new(0, 0, 100, 100);
    assert_eq!(
      clamp(Rect::new(-4, -34, 50, 50), area),
      Rect::new(0, 0, 50, 50)
    );
    assert_eq!(
      clamp(Rect::new(90, 90, 50, 50), area),
      Rect::new(50, 50, 50, 50)
    );
    assert_eq!(
      clamp(Rect::new(10, 10, 200, 20), area),
      Rect::new(0, 10, 100, 20)
    );
  }

  #[test]
  fn resize_cursors_name_their_edges() {
    assert_eq!(resize_cursor(0), None);
    assert_eq!(resize_cursor(1), Some(Shape::NResize));
    assert_eq!(resize_cursor(10), Some(Shape::SeResize));
    assert_eq!(resize_cursor(5), Some(Shape::NwResize));
    assert_eq!(resize_cursor(3), None);
  }
}
