//! `org_kde_kwin_server_decoration_manager`: KDE's older protocol for which
//! side draws a surface's decorations, which Wayfire offers through
//! wlroots beside xdg-decoration (wayfire:src/core/core.cpp,
//! `wlr_server_decoration_manager_create`).
//!
//! As in wlroots, the manager announces Wayfire's default mode on bind, a
//! new decoration object is told that mode, and a `request_mode` is
//! echoed back with `mode`. As in Wayfire's `wf_server_decoration_t`
//! (src/view/xdg-shell/xdg-toplevel-view.cpp), a toplevel draws its own
//! decorations exactly when its mode is `Client`, and the mode applies at
//! once, without a configure. The default is `Server` when
//! `preferred_decoration_mode` is `server`, else `Client`, as Wayfire sets
//! it from `core/preferred_decoration_mode`.

use std::collections::HashMap;

use wayland_protocols_misc::server_decoration::server::{
  org_kde_kwin_server_decoration::{self, OrgKdeKwinServerDecoration},
  org_kde_kwin_server_decoration_manager::{
    self, OrgKdeKwinServerDecorationManager,
  },
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum, backend::ObjectId,
};

use crate::state::CompositorState;

/// A decoration mode of the protocol.
pub(crate) type Mode = org_kde_kwin_server_decoration::Mode;

/// The decoration objects of the protocol, by object, with the surface
/// each decorates and its current mode.
#[derive(Default)]
pub(crate) struct ServerDecorations {
  decorations: HashMap<ObjectId, (ObjectId, Mode)>,
}

impl ServerDecorations {
  /// How many decoration objects exist.
  pub(crate) fn len(&self) -> usize {
    self.decorations.len()
  }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle
    .create_global::<CompositorState, OrgKdeKwinServerDecorationManager, ()>(
      1,
      (),
    );
}

/// The mode new decorations get: Wayfire's default.
pub(crate) fn default_mode(state: &CompositorState) -> Mode {
  if state.preferred_decoration_mode == crate::decoration::Mode::ServerSide {
    Mode::Server
  } else {
    Mode::Client
  }
}

/// Applies `mode` to the toplevel of `surface`, if it is one; Wayfire's
/// `use_csd = mode == CLIENT`.
fn apply(state: &mut CompositorState, surface: &ObjectId, mode: Mode) {
  let toplevel = state
    .surfaces
    .get(surface)
    .is_some_and(|surface| surface.role.toplevel().is_some());
  if toplevel {
    crate::decorator::set_client_decorated(
      state,
      surface,
      mode == Mode::Client,
    );
    crate::shell::reconfigure_all(state);
  }
}

/// The mode the decoration of toplevel surface `surface` asks for, once
/// it has one; xdg toplevels created after the decoration take it when
/// they get their role.
pub(crate) fn mode_of(
  state: &CompositorState,
  surface: &ObjectId,
) -> Option<Mode> {
  state
    .server_decorations
    .decorations
    .values()
    .find(|(decorated, _)| decorated == surface)
    .map(|(_, mode)| *mode)
}

impl GlobalDispatch<OrgKdeKwinServerDecorationManager, ()> for CompositorState {
  crate::security_context::protocol_can_view!(
    "org_kde_kwin_server_decoration_manager"
  );

  fn bind(
    state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<OrgKdeKwinServerDecorationManager>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let manager = data_init.init(resource, ());
    manager.default_mode(match default_mode(state) {
      Mode::Server => org_kde_kwin_server_decoration_manager::Mode::Server,
      _ => org_kde_kwin_server_decoration_manager::Mode::Client,
    });
  }
}

impl Dispatch<OrgKdeKwinServerDecorationManager, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &OrgKdeKwinServerDecorationManager,
    request: org_kde_kwin_server_decoration_manager::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let org_kde_kwin_server_decoration_manager::Request::Create {
      id,
      surface,
    } = request
    {
      let decoration = data_init.init(id, ());
      let mode = default_mode(state);
      let surface = surface.id();
      state
        .server_decorations
        .decorations
        .insert(decoration.id(), (surface.clone(), mode));
      decoration.mode(mode);
      apply(state, &surface, mode);
    }
  }
}

impl Dispatch<OrgKdeKwinServerDecoration, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &OrgKdeKwinServerDecoration,
    request: org_kde_kwin_server_decoration::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      org_kde_kwin_server_decoration::Request::RequestMode { mode } => {
        // wlroots ignores modes it does not know.
        let WEnum::Value(mode) = mode else {
          return;
        };
        let Some(entry) =
          state.server_decorations.decorations.get_mut(&resource.id())
        else {
          return;
        };
        entry.1 = mode;
        let surface = entry.0.clone();
        resource.mode(mode);
        apply(state, &surface, mode);
      }
      org_kde_kwin_server_decoration::Request::Release => {
        state.server_decorations.decorations.remove(&resource.id());
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &OrgKdeKwinServerDecoration,
    _data: &(),
  ) {
    state.server_decorations.decorations.remove(&resource.id());
  }
}

#[cfg(test)]
mod tests {
  use wayland_protocols_misc::server_decoration::client::{
    org_kde_kwin_server_decoration::Mode as ClientMode,
    org_kde_kwin_server_decoration_manager::OrgKdeKwinServerDecorationManager,
  };

  use crate::test_client::TestClient;

  /// Whether the one toplevel draws its own decorations.
  fn client_decorated(compositor: &crate::Compositor) -> bool {
    compositor
      .state
      .surfaces
      .values()
      .find_map(|surface| surface.role.toplevel())
      .expect("a toplevel")
      .client_decorated
  }

  #[test]
  fn a_requested_mode_is_echoed_and_applied_at_once() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let manager = client.bind::<OrgKdeKwinServerDecorationManager>(1);
    let view = client.toplevel(&mut compositor, "kde");
    client.attach_buffer(&mut compositor, &view.surface, 100, 80);
    let decoration = manager.create(&view.surface, &client.handle, ());
    client.roundtrip(&mut compositor);
    // Wayfire's default preferred_decoration_mode is client.
    assert_eq!(client.events.kde_decoration, ["default 1", "mode 1"]);
    assert!(client_decorated(&compositor));
    decoration.request_mode(ClientMode::Server);
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.kde_decoration.last().unwrap(), "mode 2");
    assert!(!client_decorated(&compositor));
    decoration.request_mode(ClientMode::Client);
    client.roundtrip(&mut compositor);
    assert!(client_decorated(&compositor));
    decoration.release();
    client.roundtrip(&mut compositor);
    assert_eq!(compositor.state.server_decorations.len(), 0);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn the_default_follows_the_preferred_mode() {
    let (mut compositor, path) = crate::tests::test_compositor();
    compositor.state.preferred_decoration_mode =
      crate::decoration::Mode::ServerSide;
    let mut client = TestClient::connect(&mut compositor, &path);
    let manager = client.bind::<OrgKdeKwinServerDecorationManager>(1);
    let view = client.toplevel(&mut compositor, "kde");
    manager.create(&view.surface, &client.handle, ());
    client.attach_buffer(&mut compositor, &view.surface, 100, 80);
    assert_eq!(client.events.kde_decoration, ["default 2", "mode 2"]);
    assert!(!client_decorated(&compositor));
    let _ = std::fs::remove_file(path);
  }
}
