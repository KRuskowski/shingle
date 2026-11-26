//! `zxdg_decoration_manager_v1`: which side draws an xdg toplevel's
//! decorations.
//!
//! A mode is sent with `zxdg_toplevel_decoration_v1.configure` and takes
//! effect when the client acknowledges the `xdg_surface.configure` that
//! follows it, as wlroots applies it with the acknowledged state and
//! Wayfire reads it on the next commit (wayfire:src/view/xdg-shell/
//! xdg-toplevel-view.cpp, `wf_xdg_decoration_t`). A toplevel without a
//! decoration object, or whose object was destroyed, draws its own.

use wayland_protocols::xdg::{
  decoration::zv1::server::{
    zxdg_decoration_manager_v1::{self, ZxdgDecorationManagerV1},
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
  },
  shell::server::xdg_toplevel::XdgToplevel,
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum, backend::ObjectId,
};

use crate::state::CompositorState;

/// Decoration mode negotiated through xdg-decoration.
pub(crate) use zxdg_toplevel_decoration_v1::Mode;

pub(crate) struct Decoration {
  resource: ZxdgToplevelDecorationV1,
  /// The `xdg_toplevel` it decorates.
  toplevel: ObjectId,
  requested: Option<Mode>,
  /// The mode last sent.
  sent: Mode,
  /// The serial of the first `xdg_surface.configure` after `sent`, whose
  /// acknowledgement applies it; `None` until that configure is sent.
  serial: Option<u32>,
  /// `sent` waits for an `xdg_surface.configure`.
  awaiting: bool,
}

pub(crate) fn preferred_mode(text: &str) -> Mode {
  if text == "server" {
    Mode::ServerSide
  } else {
    Mode::ClientSide
  }
}

fn effective_mode(state: &CompositorState, requested: Option<Mode>) -> Mode {
  if state.force_preferred_decoration_mode {
    state.preferred_decoration_mode
  } else {
    requested.unwrap_or(state.preferred_decoration_mode)
  }
}

/// Sends `decoration` the mode Shingle picks for it, to be applied with
/// the next `xdg_surface.configure`.
fn send(state: &CompositorState, decoration: &mut Decoration) {
  let mode = effective_mode(state, decoration.requested);
  decoration.resource.configure(mode);
  decoration.sent = mode;
  decoration.serial = None;
  decoration.awaiting = true;
}

/// The toplevel surface of `xdg_toplevel`.
fn surface_of(
  state: &CompositorState,
  toplevel: &ObjectId,
) -> Option<ObjectId> {
  state.surfaces.iter().find_map(|(object, _)| {
    (crate::decorator::xdg_toplevel_id(state, object).as_ref()
      == Some(toplevel))
    .then(|| object.clone())
  })
}

/// Follows a decoration's new mode with an `xdg_surface.configure` of its
/// toplevel, as the protocol requires.
fn reconfigure(state: &mut CompositorState) {
  crate::shell::reconfigure_all(state);
}

/// Recomputes existing clients when decoration preferences are reloaded.
pub(crate) fn apply_preferences(state: &mut CompositorState) {
  let mut decorations = std::mem::take(&mut state.decorations);
  for decoration in decorations.values_mut() {
    send(state, decoration);
  }
  state.decorations = decorations;
  reconfigure(state);
}

/// Records that `xdg_toplevel` was sent an `xdg_surface.configure` with
/// `serial`: the modes its decorations wait with apply when the client
/// acknowledges it.
pub(crate) fn configured(
  state: &mut CompositorState,
  toplevel: &XdgToplevel,
  serial: u32,
) {
  let id = toplevel.id();
  for decoration in state.decorations.values_mut() {
    if decoration.toplevel == id && decoration.awaiting {
      decoration.awaiting = false;
      decoration.serial = Some(serial);
    }
  }
}

/// Whether a decoration of toplevel surface `object` was sent a mode that
/// waits for the next `xdg_surface.configure`.
pub(crate) fn awaiting_configure(
  state: &CompositorState,
  object: &ObjectId,
) -> bool {
  let Some(id) = crate::decorator::xdg_toplevel_id(state, object) else {
    return false;
  };
  state
    .decorations
    .values()
    .any(|decoration| decoration.toplevel == id && decoration.awaiting)
}

/// The client of toplevel surface `object` acknowledged `serial`: a mode
/// sent before it now applies, which may give the window a frame or take
/// it away.
pub(crate) fn acknowledged(
  state: &mut CompositorState,
  object: &ObjectId,
  serial: u32,
) {
  let Some(id) = crate::decorator::xdg_toplevel_id(state, object) else {
    return;
  };
  let mut applied = None;
  for decoration in state.decorations.values_mut() {
    if decoration.toplevel == id
      && decoration.serial.is_some_and(|sent| sent <= serial)
    {
      decoration.serial = None;
      applied = Some(decoration.sent);
    }
  }
  if let Some(mode) = applied {
    crate::decorator::set_client_decorated(
      state,
      object,
      mode != Mode::ServerSide,
    );
  }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZxdgDecorationManagerV1, ()>(2, ());
}

impl GlobalDispatch<ZxdgDecorationManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zxdg_decoration_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZxdgDecorationManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZxdgDecorationManagerV1,
    request: zxdg_decoration_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration {
      id,
      toplevel,
    } = request
    {
      let resource = data_init.init(id, ());
      let mut decoration = Decoration {
        resource: resource.clone(),
        toplevel: toplevel.id(),
        requested: None,
        sent: Mode::ClientSide,
        serial: None,
        awaiting: false,
      };
      send(state, &mut decoration);
      state.decorations.insert(resource.id(), decoration);
      reconfigure(state);
    }
  }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZxdgToplevelDecorationV1,
    request: zxdg_toplevel_decoration_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let requested = match request {
      zxdg_toplevel_decoration_v1::Request::SetMode { mode } => match mode {
        WEnum::Value(mode @ (Mode::ClientSide | Mode::ServerSide)) => {
          Some(mode)
        }
        _ => {
          resource.post_error(
            zxdg_toplevel_decoration_v1::Error::InvalidMode,
            "unknown decoration mode",
          );
          return;
        }
      },
      zxdg_toplevel_decoration_v1::Request::UnsetMode => None,
      zxdg_toplevel_decoration_v1::Request::Destroy => {
        // The toplevel goes back to drawing its own decorations.
        if let Some(decoration) = state.decorations.remove(&resource.id())
          && let Some(object) = surface_of(state, &decoration.toplevel)
        {
          crate::decorator::set_client_decorated(state, &object, true);
        }
        return;
      }
      _ => return,
    };
    let Some(mut decoration) = state.decorations.remove(&resource.id()) else {
      return;
    };
    decoration.requested = requested;
    send(state, &mut decoration);
    state.decorations.insert(resource.id(), decoration);
    reconfigure(state);
  }

  /// Forgets a decoration object its client never destroyed, when the
  /// client disconnects; without this every such client left an entry
  /// behind for as long as Shingle ran (found by the soak test).
  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZxdgToplevelDecorationV1,
    _data: &(),
  ) {
    state.decorations.remove(&resource.id());
  }
}
