//! Managed X windows as Shingle toplevels (docs/design/xwayland.md,
//! sections 3 and 4).
//!
//! The window manager reports X windows as [`XwmEvent`]s ([`apply`]); the
//! `xwayland_shell_v1` side reports committed serials ([`committed`]).
//! [`Pairing`](shingle_xwayland::Pairing) joins the two. A managed window
//! becomes a [`ToplevelRole`] with [`ToplevelHandle::X11`] on its surface
//! once it is paired and the window manager has mapped it, and a Shingle
//! window once the surface has a buffer, through the same
//! `materialize_toplevel` and `request_map` as an xdg toplevel. What
//! Shingle asks of the X side waits in [`XRequest`]s until the window
//! manager flushes.

use shingle_core::{Rect, WindowId};
use shingle_xwayland::{
  Geometry, Kind, NetWmState, Paired, Placement, Window, WindowInfo, XwmEvent,
  configure_reply,
};
use wayland_server::backend::ObjectId;

use crate::{
  Event,
  shell::{self, Role, ToplevelHandle, ToplevelRole, ToplevelStates},
  state::{CompositorState, Surface, SurfaceId},
};

/// What Shingle knows about one X window.
#[derive(Clone, Debug)]
pub(crate) struct XWindowState {
  /// Its properties, as the window manager last reported them.
  pub(crate) info: WindowInfo,
  /// The window manager has mapped it.
  pub(crate) mapped: bool,
  /// Its current surface, once paired.
  pub(crate) surface: Option<SurfaceId>,
  /// The geometry it asked for before it became a Shingle window, which
  /// is its start position, as in Wayfire's `handle_client_configure`.
  pub(crate) start: Option<Geometry>,
  /// The active output when the window manager first reported it mapped,
  /// which it opens on: Wayfire gives an X view the focused output when
  /// it creates the view for a new X surface
  /// (wayfire:src/view/xwayland/xwayland-toplevel-view.hpp, `create`).
  pub(crate) created_output: Option<shingle_core::OutputId>,
}

impl XWindowState {
  /// A window Shingle has heard of but not seen mapped.
  fn unmapped() -> Self {
    Self {
      info: WindowInfo::default(),
      mapped: false,
      surface: None,
      start: None,
      created_output: None,
    }
  }
}

/// A request for the window manager, sent on its next flush.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum XRequest {
  /// `Xwm::configure`.
  Configure {
    window: Window,
    geometry: Geometry,
    force: bool,
  },
  /// `Xwm::set_state`.
  State { window: Window, state: NetWmState },
  /// `Xwm::close`.
  Close(Window),
  /// `Xwm::offer_focus`.
  OfferFocus(Window),
}

/// The geometry of a Shingle rectangle.
pub(crate) fn geometry(rect: Rect) -> Geometry {
  Geometry::new(rect.x, rect.y, rect.width, rect.height)
}

/// The Shingle rectangle of a geometry.
fn rect(geometry: Geometry) -> Rect {
  Rect::new(geometry.x, geometry.y, geometry.width, geometry.height)
}

/// Applies one window manager event.
pub(crate) fn apply(state: &mut CompositorState, event: XwmEvent) {
  match event {
    XwmEvent::RequestFailed(error) => {
      eprintln!("shingle: X request failed: {error}");
    }
    XwmEvent::Notice(notice) => eprintln!("shingle: {notice}"),
    XwmEvent::Mapped { window, info } => {
      let entry = state
        .xwayland
        .windows
        .entry(window)
        .or_insert_with(XWindowState::unmapped);
      entry.info = *info;
      entry.mapped = true;
      if entry.created_output.is_none() {
        entry.created_output = Some(state.active_output.clone());
      }
      try_map(state, window, false);
    }
    XwmEvent::Changed { window, info } => changed(state, window, *info),
    XwmEvent::Unmapped { window } => {
      // Mapped again, it opens where it was, as a Wayfire view keeps its
      // output while it is unmapped.
      let output = shingle_window(state, window)
        .and_then(|shingle| state.core.window_location(shingle).ok())
        .map(|location| location.output);
      dissociate(state, window);
      if let Some(entry) = state.xwayland.windows.get_mut(&window) {
        entry.mapped = false;
        entry.start = None;
        if output.is_some() {
          entry.created_output = output;
        }
      }
    }
    XwmEvent::Destroyed { window } => {
      dissociate(state, window);
      state.xwayland.windows.remove(&window);
    }
    XwmEvent::Serial { window, serial } => {
      if let Paired::Now { surface, .. } =
        state.xwayland.pairing.window_serial(window, serial)
      {
        associate(state, window, surface, false);
      }
    }
    XwmEvent::ConfigureRequest { window, geometry } => {
      configure_request(state, window, geometry);
    }
    XwmEvent::StateRequest { window, change } => {
      super::requests::state(state, window, change);
    }
    XwmEvent::ActivateRequest { window } => {
      super::requests::activate(state, window);
    }
    XwmEvent::MoveResizeRequest { window, request } => {
      super::requests::move_resize(state, window, request);
    }
    XwmEvent::CloseRequest { window } => {
      super::requests::close(state, window);
    }
    XwmEvent::SelectionOffered {
      selection,
      mime_types,
    } => super::selection::offered(state, selection, mime_types),
    XwmEvent::SelectionCleared { selection } => {
      super::selection::cleared(state, selection);
    }
    XwmEvent::DragOwnerChanged { .. }
    | XwmEvent::DragStatus { .. }
    | XwmEvent::DragFinished { .. }
    | XwmEvent::DragOffered { .. }
    | XwmEvent::DragSourceAction { .. }
    | XwmEvent::DragLeft
    | XwmEvent::DragDropped => super::dnd::apply(state, event),
  }
}

/// Applies a surface's pending serial on commit, before the commit is
/// handled as a toplevel's, and focuses an unmanaged window whose first
/// buffer this is, if it wants focus.
pub(crate) fn committed(state: &mut CompositorState, object: &ObjectId) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let id = surface.id;
  let Role::Xwayland(role) = &mut surface.role else {
    return;
  };
  if let Some(serial) = role.pending_serial.take() {
    role.serial = Some(serial);
    if let Paired::Now { window, .. } =
      state.xwayland.pairing.surface_serial(id, serial)
    {
      associate(state, window, id, true);
    }
  }
  super::unmanaged::committed(state, object);
}

/// Forgets a destroyed surface. Its toplevel, if any, is detached by
/// `shell::destroy_role` like any other.
pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  surface: &Surface,
) {
  let Role::Xwayland(role) = &surface.role else {
    return;
  };
  state.xwayland.pairing.surface_destroyed(surface.id);
  if let Some(window) = role.window
    && let Some(entry) = state.xwayland.windows.get_mut(&window)
    && entry.surface == Some(surface.id)
  {
    entry.surface = None;
  }
}

/// Pairs `window` with `surface` and maps it if it can.
fn associate(
  state: &mut CompositorState,
  window: Window,
  surface: SurfaceId,
  from_commit: bool,
) {
  let Some(object) = state.surface_ids.get(&surface).cloned() else {
    return;
  };
  // A window has one surface at a time.
  if state
    .xwayland
    .windows
    .get(&window)
    .and_then(|entry| entry.surface)
    .is_some_and(|current| current != surface)
  {
    dissociate(state, window);
  }
  // The window manager reports serials only for windows it tracks, and a
  // serial may come before the map.
  let entry = state
    .xwayland
    .windows
    .entry(window)
    .or_insert_with(XWindowState::unmapped);
  entry.surface = Some(surface);
  if let Some(Surface {
    role: Role::Xwayland(role),
    ..
  }) = state.surfaces.get_mut(&object)
  {
    role.window = Some(window);
  }
  try_map(state, window, from_commit);
}

/// Gives a paired, mapped, managed window its toplevel role and, when the
/// surface already has a buffer, maps it. During a commit the commit
/// itself maps it next. A paired, mapped, unmanaged window is drawn from
/// then on.
fn try_map(state: &mut CompositorState, window: Window, from_commit: bool) {
  let Some(entry) = state.xwayland.windows.get(&window) else {
    return;
  };
  if !entry.mapped {
    return;
  }
  let Some(object) = entry
    .surface
    .and_then(|surface| state.surface_ids.get(&surface))
    .cloned()
  else {
    return;
  };
  match entry.info.kind() {
    Kind::Managed { .. } => {}
    Kind::Unmanaged => {
      super::unmanaged::map(state, window, &object, from_commit);
      return;
    }
    Kind::DragIcon => {
      super::unmanaged::map_icon(state, &object);
      return;
    }
  }
  let toplevel = new_toplevel(state, window, entry);
  let Some(surface) = state.surfaces.get_mut(&object) else {
    return;
  };
  let Role::Xwayland(role) = &mut surface.role else {
    return;
  };
  if role.toplevel.is_some() {
    return;
  }
  role.toplevel = Some(Box::new(toplevel));
  let has_buffer = surface.buffer.is_some();
  crate::decorator::decide_x_toplevel(state, &object);
  if from_commit || !has_buffer {
    return;
  }
  shell::committed(state, &object);
  if let Some(snapshot) = state.snapshot(&object) {
    state
      .events
      .push(Event::SurfaceCommitted(Box::new(snapshot)));
  }
}

/// The toplevel role of a managed X window, with its launch identity
/// read from `/proc` for the X client's PID, the class as its app ID, and
/// its title, parent, size hints and start geometry.
fn new_toplevel(
  state: &CompositorState,
  window: Window,
  entry: &XWindowState,
) -> ToplevelRole {
  let info = &entry.info;
  let pid = info
    .pid
    .and_then(|pid| i32::try_from(pid).ok())
    .filter(|pid| *pid > 0);
  let mut identity = pid.map(crate::launch::capture_pid).unwrap_or_default();
  identity.app_id = info.class.clone();
  identity.title = (!info.title.is_empty()).then(|| info.title.clone());
  ToplevelRole {
    handle: ToplevelHandle::X11(window),
    window: WindowId(0),
    identity,
    app_id: info.class.clone(),
    title: info.title.clone(),
    parent: info
      .toplevel_parent()
      .and_then(|parent| shingle_window(state, parent)),
    tiled: true,
    opacity: 1.0,
    floating_geometry: rect(entry.start.unwrap_or(info.geometry)),
    min_size: info.size_hints.min.unwrap_or((0, 0)),
    max_size: info.size_hints.max.unwrap_or((0, 0)),
    configured_geometry: None,
    configure: crate::shell::ConfigureState::default(),
    created_rules_pending: None,
    windowed: None,
    unmapped: false,
    pid,
    client_decorated: info.draws_own_frame(),
    decorated: None,
    created_output: entry
      .created_output
      .clone()
      .unwrap_or_else(|| state.active_output.clone()),
  }
}

/// The surface object of an X window's toplevel.
fn toplevel_object(
  state: &CompositorState,
  window: Window,
) -> Option<ObjectId> {
  state.surfaces.iter().find_map(|(object, surface)| {
    surface
      .role
      .toplevel()
      .is_some_and(|toplevel| {
        matches!(toplevel.handle, ToplevelHandle::X11(candidate)
          if candidate == window)
      })
      .then(|| object.clone())
  })
}

/// The Shingle window of an X window, once it exists.
pub(super) fn shingle_window(
  state: &CompositorState,
  window: Window,
) -> Option<WindowId> {
  let object = toplevel_object(state, window)?;
  state.surfaces.get(&object)?.role.toplevel()?.window()
}

/// Removes the toplevel or unmanaged role of `object`'s X window, and its
/// Shingle window with it, keeping the pairing.
fn drop_toplevel(state: &mut CompositorState, object: &ObjectId) {
  super::unmanaged::unmap(state, object);
  let Some(surface) = state.surfaces.get(object) else {
    return;
  };
  if surface.role.toplevel().is_none() {
    return;
  }
  let (id, role) = (surface.id, surface.role.clone());
  shell::detach_toplevel(state, id, &role);
  if let Some(Surface {
    role: Role::Xwayland(role),
    ..
  }) = state.surfaces.get_mut(object)
  {
    role.toplevel = None;
  }
  crate::input::sync_focus(state);
}

/// Ends the association of `window` with its surface: its Shingle window
/// goes away, and a waiting serial can no longer pair. Xwayland destroys
/// the surface on unmap and brings a new one on the next map.
fn dissociate(state: &mut CompositorState, window: Window) {
  state.xwayland.pairing.window_destroyed(window);
  let Some(surface) = state
    .xwayland
    .windows
    .get_mut(&window)
    .and_then(|entry| entry.surface.take())
  else {
    return;
  };
  let Some(object) = state.surface_ids.get(&surface).cloned() else {
    return;
  };
  drop_toplevel(state, &object);
  if let Some(Surface {
    role: Role::Xwayland(role),
    ..
  }) = state.surfaces.get_mut(&object)
  {
    role.window = None;
  }
}

/// Applies changed properties: a new kind maps the window again as that
/// kind, as Wayfire's `recreate_view` does; otherwise the title, app ID,
/// parent and size hints follow.
fn changed(state: &mut CompositorState, window: Window, info: WindowInfo) {
  let Some(entry) = state.xwayland.windows.get_mut(&window) else {
    return;
  };
  let old = std::mem::replace(&mut entry.info, info.clone());
  let Some(object) = entry
    .surface
    .and_then(|surface| state.surface_ids.get(&surface))
    .cloned()
  else {
    return;
  };
  if old.kind() != info.kind() {
    drop_toplevel(state, &object);
    try_map(state, window, false);
    return;
  }
  if matches!(info.kind(), Kind::Unmanaged | Kind::DragIcon) {
    let moved = old.geometry != info.geometry;
    super::unmanaged::changed(state, window, moved);
    return;
  }
  if state
    .surfaces
    .get(&object)
    .and_then(|surface| surface.role.toplevel())
    .is_none()
  {
    return;
  }
  if old.title != info.title {
    shell::update_title(state, &object, info.title.clone());
  }
  if old.class != info.class {
    set_app_id(state, &object, info.class.clone());
  }
  if old.toplevel_parent() != info.toplevel_parent() {
    let parent = info
      .toplevel_parent()
      .and_then(|parent| shingle_window(state, parent));
    shell::set_toplevel_parent(state, &object, parent);
  }
  if old.draws_own_frame() != info.draws_own_frame() {
    crate::decorator::set_client_decorated(
      state,
      &object,
      info.draws_own_frame(),
    );
  }
  if old.size_hints != info.size_hints
    && let Some(toplevel) = state
      .surfaces
      .get_mut(&object)
      .and_then(|surface| surface.role.toplevel_mut())
  {
    toplevel.min_size = info.size_hints.min.unwrap_or((0, 0));
    toplevel.max_size = info.size_hints.max.unwrap_or((0, 0));
  }
}

/// Changes a mapped X toplevel's app ID after a `WM_CLASS` change.
fn set_app_id(state: &mut CompositorState, object: &ObjectId, app_id: String) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let id = surface.id;
  let Some(toplevel) = surface.role.toplevel_mut() else {
    return;
  };
  toplevel.app_id = app_id.clone();
  toplevel.identity.app_id = app_id.clone();
  let Some(window) = toplevel.window() else {
    return;
  };
  let _ = state.core.set_window_app_id(window, app_id.clone());
  state.events.push(Event::AppIdChanged {
    window,
    surface: id,
    app_id,
  });
}

/// Answers a `ConfigureRequest`. Before the window is a Shingle window it
/// gets what it asked, and the request is its start geometry. After that
/// tiled, maximized and fullscreen windows get their geometry back, and
/// floating ones the request clamped to the workarea, as in Wayfire. The
/// request is the X window's own rectangle, so a decorated window's
/// geometry is the request grown by its frame, and it is clamped with the
/// frame, as Wayfire's `configure_request` expands it by the margins.
fn configure_request(
  state: &mut CompositorState,
  window: Window,
  request: Geometry,
) {
  if !state.xwayland.windows.contains_key(&window) {
    return;
  }
  let margins = toplevel_object(state, window)
    .map(|object| crate::decorator::margins(state, &object))
    .unwrap_or_default();
  let placement = match shingle_window(state, window) {
    Some(shingle) => placement(state, window, shingle),
    None => {
      if let Some(entry) = state.xwayland.windows.get_mut(&window) {
        entry.start = Some(request);
      }
      if let Some(object) = toplevel_object(state, window)
        && let Some(toplevel) = state
          .surfaces
          .get_mut(&object)
          .and_then(|surface| surface.role.toplevel_mut())
      {
        toplevel.floating_geometry = margins.expand(rect(request));
      }
      Placement::Unmapped
    }
  };
  let reply = match placement {
    Placement::Floating { .. } => {
      let frame =
        configure_reply(geometry(margins.expand(rect(request))), placement);
      if let Some(shingle) = shingle_window(state, window) {
        let _ = shell::set_floating_geometry(state, shingle, rect(frame));
      }
      geometry(margins.shrink(rect(frame)))
    }
    _ => configure_reply(request, placement),
  };
  state.xwayland.requests.push(XRequest::Configure {
    window,
    geometry: reply,
    force: true,
  });
}

/// Where Shingle has put a mapped X toplevel.
fn placement(
  state: &CompositorState,
  window: Window,
  shingle: WindowId,
) -> Placement {
  let info = state.core.window_info(shingle).ok();
  let tiled = toplevel_object(state, window)
    .and_then(|object| state.surfaces.get(&object))
    .and_then(|surface| surface.role.toplevel())
    .is_none_or(|toplevel| toplevel.tiled);
  let fixed = tiled
    || info
      .as_ref()
      .is_some_and(|info| info.state.fullscreen || info.state.maximized);
  if fixed {
    let current = toplevel_object(state, window)
      .and_then(|object| shell::surface_geometry(state, &object))
      .unwrap_or(state.active().rect);
    return Placement::Fixed(geometry(current));
  }
  let workarea = info
    .and_then(|info| {
      state
        .core
        .tree(&info.location.output, info.location.workspace)
        .ok()
    })
    .map_or(state.active().rect, |tree| tree.workarea());
  Placement::Floating {
    workarea: geometry(workarea),
  }
}

/// Queues the X half of a toplevel configure: the geometry and
/// `_NET_WM_STATE`. Nothing is sent before the window is a Shingle
/// window, so an X client is never configured to a placeholder size.
pub(crate) fn configure(
  state: &mut CompositorState,
  window: Window,
  shingle: Option<WindowId>,
  rect: Rect,
  states: ToplevelStates,
) {
  let Some(shingle) = shingle else {
    return;
  };
  let Ok(info) = state.core.window_info(shingle) else {
    return;
  };
  let state_value = NetWmState {
    fullscreen: states.fullscreen,
    maximized_vert: states.maximized,
    maximized_horz: states.maximized,
    hidden: info.state.minimized,
    focused: states.activated,
    modal: false,
  };
  state.xwayland.requests.push(XRequest::Configure {
    window,
    geometry: geometry(rect),
    force: false,
  });
  state.xwayland.requests.push(XRequest::State {
    window,
    state: state_value,
  });
}

/// Asks an X window to close.
pub(crate) fn close(state: &mut CompositorState, window: Window) {
  state.xwayland.requests.push(XRequest::Close(window));
}

/// Detaches every X toplevel and forgets every X window, as when the
/// server exits: the Shingle windows go away as if their surfaces had
/// been destroyed.
pub(crate) fn forget_all(state: &mut CompositorState) {
  let windows = state.xwayland.windows.keys().copied().collect::<Vec<_>>();
  for window in windows {
    dissociate(state, window);
  }
  state.xwayland.windows.clear();
  state.xwayland.pairing = shingle_xwayland::Pairing::default();
  state.xwayland.requests.clear();
}

#[cfg(test)]
mod tests {
  use serde_json::json;
  use shingle_core::{LaunchIdentity, WindowPresence};
  use shingle_xwayland::{Geometry, Kind, XwmEvent};
  use wayland_protocols::xwayland::shell::v1::client::{
    xwayland_shell_v1, xwayland_surface_v1,
  };

  use super::*;
  use crate::{test_client::TestClient, xwayland::fixture::*};

  #[test]
  fn xwayland_shell_is_visible_only_to_xwayland() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let ordinary = TestClient::connect(&mut compositor, &path);
    let has_shell = |client: &TestClient| {
      client
        .events
        .globals
        .iter()
        .any(|(_, interface, _)| interface == "xwayland_shell_v1")
    };
    assert!(!has_shell(&ordinary));
    let xwayland = TestClient::connect_xwayland(&mut compositor);
    assert!(has_shell(&xwayland));
  }

  #[test]
  fn a_zero_serial_is_a_protocol_error() {
    let mut fixture = Fixture::new();
    let handle = fixture.client.handle.clone();
    let surface = fixture.wl_compositor.create_surface(&handle, ());
    let role = fixture.shell.get_xwayland_surface(&surface, &handle, ());
    role.set_serial(0, 0);
    let error = fixture.client.protocol_error(&mut fixture.compositor);
    assert_eq!(
      error.map(|error| error.code),
      Some(xwayland_surface_v1::Error::InvalidSerial as u32)
    );
  }

  #[test]
  fn a_second_committed_serial_is_a_protocol_error() {
    let mut fixture = Fixture::new();
    let (surface, role) = fixture.surface(5);
    surface.commit();
    fixture.client.roundtrip(&mut fixture.compositor);
    role.set_serial(6, 0);
    let error = fixture.client.protocol_error(&mut fixture.compositor);
    assert_eq!(
      error.map(|error| error.code),
      Some(xwayland_surface_v1::Error::AlreadyAssociated as u32)
    );
  }

  #[test]
  fn a_surface_with_a_role_cannot_become_an_xwayland_surface() {
    let mut fixture = Fixture::new();
    let handle = fixture.client.handle.clone();
    let surface = fixture.wl_compositor.create_surface(&handle, ());
    fixture.shell.get_xwayland_surface(&surface, &handle, ());
    fixture.shell.get_xwayland_surface(&surface, &handle, ());
    let error = fixture.client.protocol_error(&mut fixture.compositor);
    assert_eq!(
      error.map(|error| error.code),
      Some(xwayland_shell_v1::Error::Role as u32)
    );
  }

  #[test]
  fn destroying_the_role_object_first_is_allowed() {
    // Xwayland destroys xwayland_surface_v1, then the wl_surface.
    let mut fixture = Fixture::new();
    let (surface, role) = fixture.surface(5);
    fixture.commit_buffer(&surface);
    fixture.apply(mapped(XTERM, "XTerm"));
    fixture.apply(XwmEvent::Serial {
      window: XTERM,
      serial: 5,
    });
    fixture.apply(XwmEvent::Unmapped { window: XTERM });
    role.destroy();
    surface.destroy();
    assert!(
      fixture
        .client
        .protocol_error(&mut fixture.compositor)
        .is_none()
    );
    assert!(fixture.state().surfaces.is_empty());
  }

  /// Every order of buffer commit, map and serial ends with one window.
  #[test]
  fn pairing_works_in_every_order() {
    #[derive(Clone, Copy, Debug)]
    enum Step {
      Commit,
      Map,
      Serial,
    }
    use Step::*;
    let orders = [
      [Commit, Map, Serial],
      [Commit, Serial, Map],
      [Map, Commit, Serial],
      [Map, Serial, Commit],
      [Serial, Commit, Map],
      [Serial, Map, Commit],
    ];
    for order in orders {
      let mut fixture = Fixture::new();
      let (surface, _) = fixture.surface(9);
      for step in order {
        match step {
          Commit => fixture.commit_buffer(&surface),
          Map => fixture.apply(mapped(XTERM, "XTerm")),
          Serial => fixture.apply(XwmEvent::Serial {
            window: XTERM,
            serial: 9,
          }),
        }
      }
      let windows = fixture.windows("XTerm");
      assert_eq!(windows.len(), 1, "order {order:?}");
      let info = fixture.state().core.window_info(windows[0]).unwrap();
      assert_eq!(info.presence, WindowPresence::Mapped, "order {order:?}");
      assert!(fixture.state().xwayland.pairing.is_empty());
    }
  }

  #[test]
  fn a_mapped_x_window_tiles_and_gets_configured() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    map(&mut fixture, XEYES, "XEyes", 6);
    let output = fixture.state().active().rect;
    let requests = fixture.requests();
    let last_geometry = |window| {
      requests.iter().rev().find_map(|request| match request {
        XRequest::Configure {
          window: candidate,
          geometry,
          ..
        } if *candidate == window => Some(*geometry),
        _ => None,
      })
    };
    // Two tiles side by side fill the output, in mapping order.
    let xterm = last_geometry(XTERM).unwrap();
    let xeyes = last_geometry(XEYES).unwrap();
    assert_eq!(xterm.x, output.x);
    assert_eq!(xterm.width + xeyes.width, output.width);
    assert_eq!(xeyes.x, xterm.x + xterm.width as i32);
    assert_eq!(xterm.height, output.height);
    // The newest window is focused, and says so in _NET_WM_STATE.
    let last_state = requests.iter().rev().find_map(|request| match request {
      XRequest::State { window, state } if *window == XEYES => Some(*state),
      _ => None,
    });
    assert!(last_state.unwrap().focused);
  }

  #[test]
  fn an_untiled_x_window_gets_back_the_size_it_mapped_with() {
    let mut fixture = Fixture::new();
    fixture.state().placement_mode = "none".into();
    // The fixture's surface commits 64x48 before the window maps.
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    let area = fixture.state().active().rect;
    assert_eq!(
      fixture.state().core.window_layout(window).unwrap().geometry,
      area
    );
    fixture.requests();
    crate::shell::toggle_tiling(fixture.state(), window).unwrap();
    assert_eq!(
      fixture
        .state()
        .core
        .window_info(window)
        .unwrap()
        .state
        .floating,
      Some(Rect::new(area.x + 100, area.y + 100, 64, 48))
    );
    assert!(fixture.requests().iter().any(|request| matches!(
      request,
      XRequest::Configure { window: XTERM, geometry, .. }
        if (geometry.width, geometry.height) == (64, 48)
    )));
  }

  #[test]
  fn list_views_shows_class_title_and_pid() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let views = fixture.call("window-rules/list-views", json!({}));
    let view = &views.as_array().unwrap()[0];
    assert_eq!(view["app-id"], "XTerm");
    assert_eq!(view["title"], "XTerm title");
    assert_eq!(view["pid"], std::process::id());
    assert_eq!(view["type"], "toplevel");
    assert_eq!(view["role"], "toplevel");
    assert_eq!(view["mapped"], true);
  }

  #[test]
  fn the_identity_comes_from_the_x_clients_pid() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    let identity = fixture
      .state()
      .core
      .window_info(window)
      .unwrap()
      .identity
      .clone();
    assert_eq!(identity.app_id, "XTerm");
    assert_eq!(
      identity.launch_group,
      Some(format!("pid-{}", std::process::id()))
    );
    assert!(!identity.command.is_empty());
    assert_eq!(identity.title.as_deref(), Some("XTerm title"));
  }

  #[test]
  fn an_x_window_claims_a_placeholder_by_class() {
    let mut fixture = Fixture::new();
    let output = fixture.state().active_output.clone();
    let placeholder = fixture
      .state()
      .core
      .attach_window(&output, LaunchIdentity::new("XTerm"))
      .unwrap();
    fixture
      .state()
      .core
      .reconcile_unmapped(placeholder)
      .unwrap();
    map(&mut fixture, XTERM, "XTerm", 5);
    assert_eq!(fixture.windows("XTerm"), [placeholder]);
    assert_eq!(
      fixture
        .state()
        .core
        .window_info(placeholder)
        .unwrap()
        .presence,
      WindowPresence::Mapped
    );
  }

  #[test]
  fn view_pre_map_holds_an_x_window() {
    let mut fixture = Fixture::new();
    fixture.state().pre_map_clients.insert(7);
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    let state = fixture.state();
    assert!(state.delayed_maps.contains(&window));
    assert_ne!(
      state.core.window_info(window).unwrap().presence,
      WindowPresence::Mapped
    );
    assert!(
      state
        .ipc_events
        .iter()
        .any(|event| event["event"] == "view-pre-map")
    );
    shell::unblock_map(state, window, 7).unwrap();
    assert_eq!(
      state.core.window_info(window).unwrap().presence,
      WindowPresence::Mapped
    );
  }

  #[test]
  fn close_view_reaches_the_window_manager() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    fixture.requests();
    let reply =
      fixture.call("window-rules/close-view", json!({"id": window.0}));
    assert_eq!(reply["result"], "ok");
    assert_eq!(fixture.requests(), [XRequest::Close(XTERM)]);
  }

  #[test]
  fn configure_view_floats_the_window_at_the_geometry() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    fixture.requests();
    let geometry = json!({"x": 100, "y": 50, "width": 300, "height": 200});
    let reply = fixture.call(
      "window-rules/configure-view",
      json!({"id": window.0, "geometry": geometry}),
    );
    assert_eq!(reply["result"], "ok", "{reply}");
    assert!(fixture.requests().contains(&XRequest::Configure {
      window: XTERM,
      geometry: Geometry::new(100, 50, 300, 200),
      force: false,
    }));
  }

  #[test]
  fn fullscreen_and_minimize_reach_net_wm_state() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    let output = fixture.state().active().rect;
    fixture.requests();
    shell::set_fullscreen(fixture.state(), window, true).unwrap();
    let requests = fixture.requests();
    assert!(requests.contains(&XRequest::Configure {
      window: XTERM,
      geometry: geometry(output),
      force: false,
    }));
    assert!(requests.iter().any(|request| matches!(request,
      XRequest::State { window: XTERM, state } if state.fullscreen)));
    shell::set_minimized(fixture.state(), window, true).unwrap();
    assert!(fixture.requests().iter().any(|request| matches!(request,
      XRequest::State { window: XTERM, state } if state.hidden)));
  }

  #[test]
  fn configure_requests_follow_the_placement() {
    let mut fixture = Fixture::new();
    // Before it maps, a window gets what it asks for, and keeps it as
    // its floating start geometry.
    let asked = Geometry::new(30, 40, 200, 100);
    fixture.apply(XwmEvent::Mapped {
      window: XTERM,
      info: info("XTerm", "x"),
    });
    fixture.apply(XwmEvent::ConfigureRequest {
      window: XTERM,
      geometry: asked,
    });
    assert_eq!(
      fixture.requests(),
      [XRequest::Configure {
        window: XTERM,
        geometry: asked,
        force: true,
      }]
    );
    let (surface, _) = fixture.surface(5);
    fixture.commit_buffer(&surface);
    fixture.apply(XwmEvent::Serial {
      window: XTERM,
      serial: 5,
    });
    let window = fixture.windows("XTerm")[0];
    // Tiled: the tile comes back.
    let output = fixture.state().active().rect;
    fixture.requests();
    fixture.apply(XwmEvent::ConfigureRequest {
      window: XTERM,
      geometry: asked,
    });
    assert_eq!(
      fixture.requests(),
      [XRequest::Configure {
        window: XTERM,
        geometry: geometry(output),
        force: true,
      }]
    );
    // Floating: clamped to the workarea.
    shell::toggle_tiling(fixture.state(), window).unwrap();
    fixture.requests();
    fixture.apply(XwmEvent::ConfigureRequest {
      window: XTERM,
      geometry: Geometry::new(-100, 10, 300, 200),
    });
    let clamped = Geometry::new(0, 10, 300, 200);
    let requests = fixture.requests();
    assert_eq!(
      requests.last(),
      Some(&XRequest::Configure {
        window: XTERM,
        geometry: clamped,
        force: true,
      })
    );
    let views = fixture.call("window-rules/view-info", json!({"id": window.0}));
    assert_eq!(views["info"]["geometry"]["x"], 0, "{views}");
  }

  #[test]
  fn decorated_x_windows_are_configured_inside_their_frame() {
    let mut fixture = Fixture::decorated();
    map(&mut fixture, XTERM, "XTerm", 5);
    let mut frameless = info("XEyes", "eyes");
    frameless.decorations = Some(false);
    map_info(&mut fixture, XEYES, frameless.clone(), 6);
    let last_geometry = |requests: &[XRequest], window| {
      requests.iter().rev().find_map(|request| match request {
        XRequest::Configure {
          window: candidate,
          geometry,
          ..
        } if *candidate == window => Some(*geometry),
        _ => None,
      })
    };
    let requests = fixture.requests();
    let xterm = fixture.windows("XTerm")[0];
    let xeyes = fixture.windows("XEyes")[0];
    let state = fixture.state();
    let margins = state.frame_theme.margins();
    let tile = |window| state.core.window_layout(window).unwrap().geometry;
    // Without _MOTIF_WM_HINTS, xterm gets a frame and is configured
    // inside it; a window that turned its decorations off gets none.
    assert_eq!(
      last_geometry(&requests, XTERM),
      Some(geometry(margins.shrink(tile(xterm))))
    );
    assert_eq!(last_geometry(&requests, XEYES), Some(geometry(tile(xeyes))));
    let frames = crate::decorator::frames(state);
    assert_eq!(frames.len(), 1);
    assert!(frames.values().all(|frame| frame.window == xterm));
    // Hints that ask for decorations again bring the frame.
    frameless.decorations = Some(true);
    fixture.apply(XwmEvent::Changed {
      window: XEYES,
      info: frameless,
    });
    let requests = fixture.requests();
    let state = fixture.state();
    let tile = state.core.window_layout(xeyes).unwrap().geometry;
    assert_eq!(
      last_geometry(&requests, XEYES),
      Some(geometry(margins.shrink(tile)))
    );
    assert_eq!(crate::decorator::frames(state).len(), 2);
  }

  #[test]
  fn a_framed_transient_is_centred_on_its_parents_frame() {
    const DIALOG: Window = 0x700001;
    let mut fixture = Fixture::decorated();
    map(&mut fixture, XTERM, "XTerm", 5);
    let mut dialog = info("XTerm", "dialog");
    dialog.transient_for = Some(XTERM);
    dialog.geometry = Geometry::new(0, 0, 200, 100);
    map_info(&mut fixture, DIALOG, dialog, 6);
    let state = fixture.state();
    let window = state
      .surfaces
      .values()
      .filter_map(|surface| surface.role.toplevel())
      .filter(|toplevel| toplevel.parent.is_some())
      .find_map(ToplevelRole::window)
      .expect("the dialog maps");
    let output = state.active().rect;
    // The frame, 208x138, is centred on the parent's frame, the output.
    let frame = Rect::new(
      output.x + (output.width as i32 - 208) / 2,
      output.y + (output.height as i32 - 138) / 2,
      208,
      138,
    );
    assert_eq!(
      state.core.window_info(window).unwrap().state.floating,
      Some(frame)
    );
  }

  #[test]
  fn a_floating_x_window_gets_its_request_inside_a_clamped_frame() {
    let mut fixture = Fixture::decorated();
    // Before it maps, the frame is around what it asked for.
    let asked = Geometry::new(30, 40, 200, 100);
    fixture.apply(XwmEvent::Mapped {
      window: XTERM,
      info: info("XTerm", "x"),
    });
    fixture.apply(XwmEvent::ConfigureRequest {
      window: XTERM,
      geometry: asked,
    });
    assert_eq!(
      fixture.requests(),
      [XRequest::Configure {
        window: XTERM,
        geometry: asked,
        force: true,
      }]
    );
    let (surface, _) = fixture.surface(5);
    fixture.commit_buffer(&surface);
    fixture.apply(XwmEvent::Serial {
      window: XTERM,
      serial: 5,
    });
    let window = fixture.windows("XTerm")[0];
    let margins = fixture.state().frame_theme.margins();
    shell::toggle_tiling(fixture.state(), window).unwrap();
    fixture.requests();
    // A request off the workarea is clamped with its frame, and the X
    // window gets the inside of the frame.
    fixture.apply(XwmEvent::ConfigureRequest {
      window: XTERM,
      geometry: Geometry::new(-100, 10, 300, 200),
    });
    let frame = Rect::new(0, 0, 308, 238);
    assert_eq!(
      fixture.requests().last(),
      Some(&XRequest::Configure {
        window: XTERM,
        geometry: geometry(margins.shrink(frame)),
        force: true,
      })
    );
    let views = fixture.call("window-rules/view-info", json!({"id": window.0}));
    assert_eq!(views["info"]["geometry"]["y"], 0, "{views}");
    assert_eq!(views["info"]["base-geometry"]["y"], 34, "{views}");
  }

  #[test]
  fn title_class_and_hints_follow_property_changes() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let window = fixture.windows("XTerm")[0];
    let mut changed = info("UXTerm", "new title");
    changed.size_hints.min = Some((100, 80));
    fixture.apply(XwmEvent::Changed {
      window: XTERM,
      info: changed,
    });
    let view = fixture.call("window-rules/view-info", json!({"id": window.0}));
    assert_eq!(view["info"]["title"], "new title");
    assert_eq!(view["info"]["app-id"], "UXTerm");
    assert_eq!(
      view["info"]["min-size"],
      json!({"width": 100, "height": 80})
    );
    assert_eq!(fixture.windows("UXTerm"), [window]);
  }

  #[test]
  fn becoming_override_redirect_unmanages_the_window() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let mut unmanaged = info("XTerm", "x");
    unmanaged.override_redirect = true;
    assert_eq!(unmanaged.kind(), Kind::Unmanaged);
    fixture.apply(XwmEvent::Changed {
      window: XTERM,
      info: unmanaged,
    });
    assert!(fixture.windows("XTerm").is_empty());
    // And back.
    fixture.apply(XwmEvent::Changed {
      window: XTERM,
      info: info("XTerm", "x"),
    });
    assert_eq!(fixture.windows("XTerm").len(), 1);
  }

  #[test]
  fn override_redirect_windows_are_tracked_but_not_windows() {
    let mut fixture = Fixture::new();
    let (surface, _) = fixture.surface(5);
    fixture.commit_buffer(&surface);
    let mut menu = info("XTerm", "");
    menu.override_redirect = true;
    fixture.apply(XwmEvent::Mapped {
      window: XTERM,
      info: menu,
    });
    fixture.apply(XwmEvent::Serial {
      window: XTERM,
      serial: 5,
    });
    assert!(fixture.state().xwayland.windows.contains_key(&XTERM));
    assert!(fixture.windows("XTerm").is_empty());
    let views = fixture.call("window-rules/list-views", json!({}));
    assert_eq!(views, json!([]));
    assert!(
      fixture
        .compositor
        .visible_surfaces()
        .iter()
        .all(|snapshot| snapshot.role != crate::SurfaceRole::Toplevel)
    );
  }

  #[test]
  fn unmap_removes_the_window_and_a_remap_brings_it_back() {
    let mut fixture = Fixture::new();
    let (surface, role) = map(&mut fixture, XTERM, "XTerm", 5);
    fixture.apply(XwmEvent::Unmapped { window: XTERM });
    assert!(fixture.windows("XTerm").is_empty());
    assert!(fixture.state().core.window_ids().next().is_none());
    role.destroy();
    surface.destroy();
    fixture.client.roundtrip(&mut fixture.compositor);
    map(&mut fixture, XTERM, "XTerm", 6);
    assert_eq!(fixture.windows("XTerm").len(), 1);
  }

  #[test]
  fn a_window_destroyed_before_its_surface_commits_leaves_nothing() {
    let mut fixture = Fixture::new();
    let (surface, role) = fixture.surface(5);
    fixture.apply(mapped(XTERM, "XTerm"));
    fixture.apply(XwmEvent::Serial {
      window: XTERM,
      serial: 5,
    });
    fixture.apply(XwmEvent::Destroyed { window: XTERM });
    fixture.commit_buffer(&surface);
    assert!(fixture.state().core.window_ids().next().is_none());
    assert!(fixture.windows("XTerm").is_empty());
    assert!(fixture.state().xwayland.windows.is_empty());
    // The surface waits, inert, until Xwayland destroys it.
    role.destroy();
    surface.destroy();
    fixture.client.roundtrip(&mut fixture.compositor);
    assert!(fixture.state().xwayland.pairing.is_empty());
  }

  #[test]
  fn a_surface_destroyed_before_the_serial_message_leaves_nothing() {
    let mut fixture = Fixture::new();
    let (surface, role) = fixture.surface(5);
    fixture.commit_buffer(&surface);
    role.destroy();
    surface.destroy();
    fixture.client.roundtrip(&mut fixture.compositor);
    fixture.apply(mapped(XTERM, "XTerm"));
    fixture.apply(XwmEvent::Serial {
      window: XTERM,
      serial: 5,
    });
    assert!(fixture.state().core.window_ids().next().is_none());
    // The window still waits for a surface; its unmap drops the serial.
    fixture.apply(XwmEvent::Unmapped { window: XTERM });
    assert!(fixture.state().xwayland.pairing.is_empty());
  }

  #[test]
  fn losing_the_window_manager_forgets_every_x_window() {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    map(&mut fixture, XEYES, "XEyes", 6);
    forget_all(fixture.state());
    assert!(fixture.state().core.window_ids().next().is_none());
    assert!(fixture.state().xwayland.windows.is_empty());
  }
}
