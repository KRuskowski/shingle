use shingle_config::{RuleAction, RuleEvent, ViewProperties};
use shingle_core::{LaunchIdentity, Rect, WindowId, WorkspaceId};

use crate::{Event, shell, state::CompositorState};

pub(crate) fn apply_created(
  state: &mut CompositorState,
  window: WindowId,
  identity: &LaunchIdentity,
) {
  apply(state, window, RuleEvent::Created, identity);
}

pub(crate) fn apply_event(
  state: &mut CompositorState,
  window: WindowId,
  event: RuleEvent,
) {
  let Ok(identity) = state.core.window_info(window).map(|info| info.identity)
  else {
    return;
  };
  apply(state, window, event, &identity);
}

fn apply(
  state: &mut CompositorState,
  window: WindowId,
  event: RuleEvent,
  identity: &LaunchIdentity,
) {
  if state.applying_window_rules {
    return;
  }
  let view = view_properties(state, window, identity);
  let rules = state.window_rules.clone();
  state.applying_window_rules = true;
  for rule in rules {
    if !rule.applies_to(event) || !rule.matches_view(&view) {
      continue;
    }
    match rule.action {
      RuleAction::Maximize(maximized) => {
        let _ = shell::set_maximized(state, window, maximized);
      }
      RuleAction::Minimize(minimized) => {
        let _ = shell::set_minimized(state, window, minimized);
      }
      RuleAction::Snap(slot) => {
        let _ = shell::snap_window(state, window, &slot);
      }
      RuleAction::Alpha(alpha) => shell::set_alpha(state, window, alpha),
      RuleAction::Sticky => {
        let _ = shell::set_sticky(state, window, true);
      }
      RuleAction::AlwaysOnTop => {
        let _ = shell::set_stacking(
          state,
          window,
          shingle_core::StackingMode::AlwaysOnTop,
        );
      }
      RuleAction::AlwaysOnBottom => {
        let _ = shell::set_stacking(
          state,
          window,
          shingle_core::StackingMode::AlwaysOnBottom,
        );
      }
      RuleAction::StartOnOutput(name) => start_on_output(state, window, &name),
      RuleAction::AssignWorkspace { x, y } => {
        assign_workspace(state, window, WorkspaceId::new(x, y));
      }
      RuleAction::SetGeometry(geometry) => {
        set_geometry(state, window, geometry)
      }
      RuleAction::SetGeometryPercent {
        x,
        y,
        width,
        height,
      } => {
        if let Some(geometry) = percent_geometry(state, x, y, width, height) {
          set_geometry(state, window, geometry);
        }
      }
      RuleAction::SetGeometryX11 {
        width,
        height,
        x,
        y,
        x_from_right,
        y_from_bottom,
      } => {
        if let Some(geometry) = x11_geometry(
          state,
          window,
          width,
          height,
          x,
          y,
          x_from_right,
          y_from_bottom,
        ) {
          set_geometry(state, window, geometry);
        }
      }
      RuleAction::Move { x, y } => {
        if let Some(mut geometry) = floating_geometry(state, window) {
          geometry.x = x;
          geometry.y = y;
          set_geometry(state, window, geometry);
        }
      }
      RuleAction::Resize { width, height } => {
        if let Some(mut geometry) = floating_geometry(state, window) {
          geometry.width = width;
          geometry.height = height;
          set_geometry(state, window, geometry);
        }
      }
    }
  }
  state.applying_window_rules = false;
}

pub(crate) fn view_properties<'a>(
  state: &CompositorState,
  window: WindowId,
  identity: &'a LaunchIdentity,
) -> ViewProperties<'a> {
  let Ok(info) = state.core.window_info(window) else {
    return ViewProperties {
      app_id: &identity.app_id,
      title: identity.title.as_deref().unwrap_or(""),
      ..ViewProperties::default()
    };
  };
  let floating = info.state.floating.is_some() && !info.state.maximized;
  let activated = state
    .core
    .tree(&info.location.output, info.location.workspace)
    .is_ok_and(|tree| tree.focused() == Some(window));
  let tiled = !floating;
  ViewProperties {
    app_id: &identity.app_id,
    title: identity.title.as_deref().unwrap_or(""),
    view_type: "toplevel",
    fullscreen: info.state.fullscreen,
    activated,
    minimized: info.state.minimized,
    focusable: true,
    mapped: true,
    tiled_left: tiled,
    tiled_right: tiled,
    tiled_top: tiled,
    tiled_bottom: tiled,
    maximized: info.state.maximized,
    floating,
  }
}

fn floating_geometry(
  state: &CompositorState,
  window: WindowId,
) -> Option<Rect> {
  state
    .core
    .window_info(window)
    .ok()
    .and_then(|info| info.state.floating)
    .or_else(|| {
      state
        .core
        .window_layout(window)
        .ok()
        .map(|layout| layout.geometry)
    })
}

fn percent_geometry(
  state: &CompositorState,
  x: i32,
  y: i32,
  width: i32,
  height: i32,
) -> Option<Rect> {
  let output = state.active().rect;
  let scale = |value: i32, extent: u32| {
    (u64::from(extent) * i64::from(value.clamp(0, 100)) as u64 / 100) as u32
  };
  Some(Rect {
    x: i32::try_from(i64::from(output.x) + i64::from(scale(x, output.width)))
      .ok()?,
    y: i32::try_from(i64::from(output.y) + i64::from(scale(y, output.height)))
      .ok()?,
    width: scale(width, output.width),
    height: scale(height, output.height),
  })
}

#[allow(clippy::too_many_arguments)]
fn x11_geometry(
  state: &CompositorState,
  window: WindowId,
  width: Option<u32>,
  height: Option<u32>,
  x: Option<i32>,
  y: Option<i32>,
  x_from_right: bool,
  y_from_bottom: bool,
) -> Option<Rect> {
  let current = floating_geometry(state, window)?;
  let width = width.unwrap_or(current.width);
  let height = height.unwrap_or(current.height);
  let workarea = state
    .core
    .current_workspace(&state.active_output)
    .ok()
    .and_then(|workspace| state.core.tree(&state.active_output, workspace).ok())
    .map_or(state.active().rect, |tree| tree.workarea());
  let x = match x {
    Some(x) if x_from_right => {
      (i64::from(workarea.width) - i64::from(width) - i64::from(x)).max(0)
    }
    Some(x) => i64::from(x),
    None => {
      return Some(Rect {
        x: current.x,
        y: current.y,
        width,
        height,
      });
    }
  };
  let y = match y {
    Some(y) if y_from_bottom => {
      (i64::from(workarea.height) - i64::from(height) - i64::from(y)).max(0)
    }
    Some(y) => i64::from(y),
    None => {
      return Some(Rect {
        x: i32::try_from(i64::from(workarea.x) + x).ok()?,
        y: current.y,
        width,
        height,
      });
    }
  };
  Some(Rect {
    x: i32::try_from(i64::from(workarea.x) + x).ok()?,
    y: i32::try_from(i64::from(workarea.y) + y).ok()?,
    width,
    height,
  })
}

fn set_geometry(state: &mut CompositorState, window: WindowId, geometry: Rect) {
  let Ok(geometry) = geometry.validate() else {
    return;
  };
  if state
    .core
    .window_info(window)
    .is_ok_and(|info| info.state.floating.is_none())
    && shell::toggle_tiling(state, window).is_err()
  {
    return;
  }
  if state.core.set_floating(window, Some(geometry)).is_ok() {
    shell::sync_floating_geometry(state, window, geometry);
    shell::reconfigure_all(state);
  }
}

/// The output a `start_on_output` rule names: by connector name, else by
/// `make model` or `make model serial`, as Wayfire's `_start_on_output`
/// looks it up.
fn named_output(
  state: &CompositorState,
  name: &str,
) -> Option<shingle_core::OutputId> {
  let by_name = shingle_core::OutputId::new(name);
  if state.outputs.get(&by_name).is_some() {
    return Some(by_name);
  }
  state
    .outputs
    .iter()
    .find(|output| {
      let make_model = format!("{} {}", output.info.make, output.info.model);
      let full = match output.info.serial.as_deref() {
        Some(serial) if !serial.is_empty() => format!("{make_model} {serial}"),
        _ => make_model.clone(),
      };
      name == make_model || name == full
    })
    .map(|output| output.id.clone())
}

/// Moves `window` to the output a `start_on_output` rule names, as
/// `move_view_to_output` with `RECONFIGURE`. Dialogs stay with their
/// parent, and a name no output has does nothing.
fn start_on_output(state: &mut CompositorState, window: WindowId, name: &str) {
  let Some(output) = named_output(state, name) else {
    eprintln!("shingle: window-rules: no output matches {name:?}");
    return;
  };
  if state
    .core
    .window_info(window)
    .is_ok_and(|info| info.parent.is_some())
  {
    return;
  }
  if let Err(error) = crate::output::move_window_to_output(
    state,
    window,
    &output,
    crate::output::MoveGeometry::Scale,
  ) {
    eprintln!("shingle: window-rules: start_on_output failed: {error}");
  }
}

fn assign_workspace(
  state: &mut CompositorState,
  window: WindowId,
  workspace: WorkspaceId,
) {
  if state
    .core
    .ensure_workspace(&state.active_output, workspace)
    .is_err()
  {
    return;
  }
  let Ok(previous) = state.core.window_location(window) else {
    return;
  };
  let from =
    previous.workspace.y * state.workspace_width + previous.workspace.x + 1;
  let Ok(detached) = state.core.detach_window(window) else {
    return;
  };
  if state
    .core
    .reattach_window(detached, &state.active_output, workspace)
    .is_ok()
  {
    let index = workspace.y * state.workspace_width + workspace.x;
    state.events.push(Event::WindowMoved {
      window,
      from,
      workspace: index + 1,
    });
  }
}
