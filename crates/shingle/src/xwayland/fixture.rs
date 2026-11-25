//! The in-process test fixture for the XWayland glue: a compositor with
//! the test client connected as Xwayland, and window manager events
//! applied directly.

use serde_json::{Value, json};
use shingle_core::WindowId;
use shingle_xwayland::{Geometry, Window, WindowInfo, XwmEvent};
use wayland_client::protocol::{wl_compositor, wl_surface};
use wayland_protocols::xwayland::shell::v1::client::{
  xwayland_shell_v1, xwayland_surface_v1,
};

use super::windows::{XRequest, apply};
use crate::{Compositor, state::CompositorState, test_client::TestClient};

pub(crate) const XTERM: Window = 0x400001;
pub(crate) const XEYES: Window = 0x600001;

/// A compositor, the test Xwayland connected to it, and its globals.
pub(crate) struct Fixture {
  pub(crate) compositor: Compositor,
  pub(crate) client: TestClient,
  pub(crate) wl_compositor: wl_compositor::WlCompositor,
  pub(crate) shell: xwayland_shell_v1::XwaylandShellV1,
  /// The compositor's socket, for Wayland clients.
  pub(crate) path: std::path::PathBuf,
}

impl Fixture {
  pub(crate) fn new() -> Self {
    let (mut compositor, path) = crate::tests::test_compositor();
    let client = TestClient::connect_xwayland(&mut compositor);
    let wl_compositor = client.bind::<wl_compositor::WlCompositor>(4);
    let shell = client.bind::<xwayland_shell_v1::XwaylandShellV1>(1);
    Self {
      compositor,
      client,
      wl_compositor,
      shell,
      path,
    }
  }

  pub(crate) fn state(&mut self) -> &mut CompositorState {
    &mut self.compositor.state
  }

  /// A surface with the `xwayland_surface_v1` role and `serial` set but
  /// not committed.
  pub(crate) fn surface(
    &mut self,
    serial: u64,
  ) -> (
    wl_surface::WlSurface,
    xwayland_surface_v1::XwaylandSurfaceV1,
  ) {
    let handle = &self.client.handle;
    let surface = self.wl_compositor.create_surface(handle, ());
    let role = self.shell.get_xwayland_surface(&surface, handle, ());
    role.set_serial(serial as u32, (serial >> 32) as u32);
    self.client.roundtrip(&mut self.compositor);
    (surface, role)
  }

  /// Commits a buffer, which also applies a pending serial.
  pub(crate) fn commit_buffer(&mut self, surface: &wl_surface::WlSurface) {
    self
      .client
      .attach_buffer(&mut self.compositor, surface, 64, 48);
  }

  pub(crate) fn apply(&mut self, event: XwmEvent) {
    apply(self.state(), event);
  }

  pub(crate) fn call(&mut self, method: &str, data: Value) -> Value {
    crate::ipc::respond(self.state(), &json!({"method": method, "data": data}))
  }

  /// The Shingle windows with `app_id`.
  pub(crate) fn windows(&mut self, app_id: &str) -> Vec<WindowId> {
    let state = self.state();
    let mut windows = state
      .surfaces
      .values()
      .filter_map(|surface| {
        let toplevel = surface.role.toplevel()?;
        (toplevel.app_id == app_id)
          .then_some(toplevel.window())
          .flatten()
      })
      .collect::<Vec<_>>();
    windows.sort();
    windows
  }

  pub(crate) fn requests(&mut self) -> Vec<XRequest> {
    std::mem::take(&mut self.state().xwayland.requests)
  }
}

/// What the window manager reports for a plain window of `class`.
pub(crate) fn info(class: &str, title: &str) -> Box<WindowInfo> {
  Box::new(WindowInfo {
    class: class.into(),
    instance: class.to_lowercase(),
    title: title.into(),
    geometry: Geometry::new(0, 0, 484, 316),
    pid: Some(std::process::id()),
    ..WindowInfo::default()
  })
}

pub(crate) fn mapped(window: Window, class: &str) -> XwmEvent {
  XwmEvent::Mapped {
    window,
    info: info(class, &format!("{class} title")),
  }
}

/// Maps a window with `info` fully: surface, buffer, map and serial.
pub(crate) fn map_info(
  fixture: &mut Fixture,
  window: Window,
  info: Box<WindowInfo>,
  serial: u64,
) -> (
  wl_surface::WlSurface,
  xwayland_surface_v1::XwaylandSurfaceV1,
) {
  let (surface, role) = fixture.surface(serial);
  fixture.commit_buffer(&surface);
  fixture.apply(XwmEvent::Mapped { window, info });
  fixture.apply(XwmEvent::Serial { window, serial });
  (surface, role)
}

/// Maps a window of `class` fully: surface, buffer, map and serial.
pub(crate) fn map(
  fixture: &mut Fixture,
  window: Window,
  class: &str,
  serial: u64,
) -> (
  wl_surface::WlSurface,
  xwayland_surface_v1::XwaylandSurfaceV1,
) {
  let (surface, role) = fixture.surface(serial);
  fixture.commit_buffer(&surface);
  fixture.apply(mapped(window, class));
  fixture.apply(XwmEvent::Serial { window, serial });
  (surface, role)
}
