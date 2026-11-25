//! `xwayland_shell_v1`: Xwayland names the X window behind each of its
//! `wl_surface`s by a serial, which [`super::windows`] pairs with the
//! window manager's `WL_SURFACE_SERIAL` (docs/design/xwayland.md,
//! section 3).
//!
//! The global is visible to, and may be bound by, only the Wayland client
//! Shingle started Xwayland with. The serial is double-buffered: it takes
//! effect on the surface's next commit ([`super::windows::committed`]).

use shingle_xwayland::Window;
use wayland_protocols::xwayland::shell::v1::server::{
  xwayland_shell_v1::{self, XwaylandShellV1},
  xwayland_surface_v1::{self, XwaylandSurfaceV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::{
  shell::{Role, ToplevelRole},
  state::{ClientDataImpl, CompositorState},
};

/// The `xwayland_surface_v1` role of a `wl_surface`.
#[derive(Clone)]
pub(crate) struct XwaylandRole {
  pub(crate) resource: XwaylandSurfaceV1,
  /// `xwayland_surface_v1` was destroyed; the association stays, as the
  /// protocol says, but the role object is gone.
  pub(crate) resource_destroyed: bool,
  /// Set by `set_serial`, applied on the next commit.
  pub(crate) pending_serial: Option<u64>,
  /// The committed serial.
  pub(crate) serial: Option<u64>,
  /// The X window, once paired.
  pub(crate) window: Option<Window>,
  /// The toplevel of a managed window, from the moment it can map.
  pub(crate) toplevel: Option<Box<ToplevelRole>>,
}

impl XwaylandRole {
  /// Whether the role object is still alive, which a `wl_surface` must
  /// not outlive.
  pub(crate) fn is_live(&self) -> bool {
    !self.resource_destroyed
  }
}

/// Creates the `xwayland_shell_v1` global.
pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, XwaylandShellV1, ()>(1, ());
}

/// Whether `client` is Shingle's Xwayland.
fn is_xwayland(client: &Client) -> bool {
  client
    .get_data::<ClientDataImpl>()
    .is_some_and(|data| data.xwayland)
}

impl GlobalDispatch<XwaylandShellV1, ()> for CompositorState {
  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    client: &Client,
    resource: New<XwaylandShellV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let shell = data_init.init(resource, ());
    // The global is hidden from other clients, but they may still guess
    // its name; refuse them as wlroots' shell_bind does.
    if !is_xwayland(client) {
      shell.post_error(
        xwayland_shell_v1::Error::Role,
        "only Xwayland may bind xwayland_shell_v1",
      );
    }
  }

  fn can_view(client: Client, _global_data: &()) -> bool {
    is_xwayland(&client)
  }
}

impl Dispatch<XwaylandShellV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &XwaylandShellV1,
    request: xwayland_shell_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      xwayland_shell_v1::Request::GetXwaylandSurface { id, surface } => {
        let role = data_init.init(id, ());
        let Some(surface) = state.surfaces.get_mut(&surface.id()) else {
          return;
        };
        if !matches!(surface.role, Role::None) {
          resource.post_error(
            xwayland_shell_v1::Error::Role,
            "the wl_surface already has a role",
          );
          return;
        }
        surface.role = Role::Xwayland(Box::new(XwaylandRole {
          resource: role,
          resource_destroyed: false,
          pending_serial: None,
          serial: None,
          window: None,
          toplevel: None,
        }));
      }
      xwayland_shell_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<XwaylandSurfaceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &XwaylandSurfaceV1,
    request: xwayland_surface_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let Some(role) =
      state
        .surfaces
        .values_mut()
        .find_map(|surface| match &mut surface.role {
          Role::Xwayland(role) if role.resource == *resource => Some(role),
          _ => None,
        })
    else {
      return;
    };
    match request {
      xwayland_surface_v1::Request::SetSerial {
        serial_lo,
        serial_hi,
      } => {
        let serial = u64::from(serial_lo) | (u64::from(serial_hi) << 32);
        if serial == 0 {
          resource.post_error(
            xwayland_surface_v1::Error::InvalidSerial,
            "the serial must not be zero",
          );
        } else if role.serial.is_some() {
          resource.post_error(
            xwayland_surface_v1::Error::AlreadyAssociated,
            "the wl_surface already committed a serial",
          );
        } else {
          role.pending_serial = Some(serial);
        }
      }
      xwayland_surface_v1::Request::Destroy => {
        role.resource_destroyed = true;
      }
      _ => {}
    }
  }
}
