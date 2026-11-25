use wayland_protocols::xdg::decoration::zv1::server::{
  zxdg_decoration_manager_v1::{self, ZxdgDecorationManagerV1},
  zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
};

use crate::state::CompositorState;

pub(crate) struct Decoration {
  resource: ZxdgToplevelDecorationV1,
  requested: Option<zxdg_toplevel_decoration_v1::Mode>,
}

pub(crate) fn preferred_mode(text: &str) -> zxdg_toplevel_decoration_v1::Mode {
  if text == "server" {
    zxdg_toplevel_decoration_v1::Mode::ServerSide
  } else {
    zxdg_toplevel_decoration_v1::Mode::ClientSide
  }
}

fn effective_mode(
  state: &CompositorState,
  requested: Option<zxdg_toplevel_decoration_v1::Mode>,
) -> zxdg_toplevel_decoration_v1::Mode {
  if state.force_preferred_decoration_mode {
    state.preferred_decoration_mode
  } else {
    requested.unwrap_or(state.preferred_decoration_mode)
  }
}

/// Recomputes existing clients when decoration preferences are reloaded.
pub(crate) fn apply_preferences(state: &mut CompositorState) {
  for decoration in state.decorations.values() {
    decoration
      .resource
      .configure(effective_mode(state, decoration.requested));
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
      ..
    } = request
    {
      let decoration = data_init.init(id, ());
      decoration.configure(state.preferred_decoration_mode);
      state.decorations.insert(
        decoration.id(),
        Decoration {
          resource: decoration,
          requested: None,
        },
      );
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
    match request {
      zxdg_toplevel_decoration_v1::Request::SetMode { mode } => match mode {
        WEnum::Value(mode)
          if matches!(
            mode,
            zxdg_toplevel_decoration_v1::Mode::ClientSide
              | zxdg_toplevel_decoration_v1::Mode::ServerSide
          ) =>
        {
          let requested = Some(mode);
          resource.configure(effective_mode(state, requested));
          if let Some(decoration) = state.decorations.get_mut(&resource.id()) {
            decoration.requested = requested;
          }
        }
        _ => resource.post_error(
          zxdg_toplevel_decoration_v1::Error::InvalidMode,
          "unknown decoration mode",
        ),
      },
      zxdg_toplevel_decoration_v1::Request::UnsetMode => {
        resource.configure(effective_mode(state, None));
        if let Some(decoration) = state.decorations.get_mut(&resource.id()) {
          decoration.requested = None;
        }
      }
      zxdg_toplevel_decoration_v1::Request::Destroy => {
        state.decorations.remove(&resource.id());
      }
      _ => {}
    }
  }
}
