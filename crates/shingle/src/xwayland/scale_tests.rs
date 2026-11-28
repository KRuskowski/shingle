//! Zero scaling (XW-7) in the compositor: what Xwayland is told about the
//! outputs, how X windows are configured and placed, how big their
//! surfaces are and where input reaches them at an X scale above 1, with
//! window manager events applied directly.

use serde_json::json;
use shingle_core::Rect;
use shingle_xwayland::{Geometry, WindowType, XwmEvent};
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat};
use wayland_protocols::xdg::xdg_output::zv1::client::{
  zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1,
};
use wayland_server::backend::ObjectId;

use super::{
  fixture::*,
  scale::{self, XScale},
  windows::XRequest,
};
use crate::{shell, test_client::TestClient};

const MENU: shingle_xwayland::Window = 0x400100;

/// Gives the output scale `output_scale` and X clients X scale `x_scale`,
/// as `Xwayland::sync_scale` does with zero scaling.
fn set_scales(fixture: &mut Fixture, output_scale: f64, x_scale: f64) {
  let state = fixture.state();
  let output = state.active_output.clone();
  state.set_scale_of(&output, output_scale).unwrap();
  state.xwayland.scale = XScale::new(x_scale);
  scale::changed(state);
}

/// The surface object of an X window.
fn object_of(
  fixture: &mut Fixture,
  window: shingle_xwayland::Window,
) -> ObjectId {
  let state = fixture.state();
  let surface = state.xwayland.windows[&window].surface.unwrap();
  state.surface_ids[&surface].clone()
}

/// The logical geometry IPC reports for `app_id`'s view.
fn view_geometry(fixture: &mut Fixture, app_id: &str) -> Rect {
  let window = fixture.windows(app_id)[0];
  let info = fixture.call("window-rules/view-info", json!({"id": window.0}));
  let geometry = &info["info"]["geometry"];
  let value = |key: &str| geometry[key].as_i64().unwrap();
  Rect::new(
    value("x") as i32,
    value("y") as i32,
    value("width") as u32,
    value("height") as u32,
  )
}

/// The last X geometry Shingle asked for `window`.
fn last_configure(
  requests: &[XRequest],
  window: shingle_xwayland::Window,
) -> Option<Geometry> {
  requests.iter().rev().find_map(|request| match request {
    XRequest::Configure {
      window: candidate,
      geometry,
      ..
    } if *candidate == window => Some(*geometry),
    _ => None,
  })
}

/// What a client hears about an output.
struct OutputSeen {
  /// `wl_output.geometry`'s position.
  position: (i32, i32),
  /// `wl_output.scale`.
  scale: i32,
  /// xdg-output's logical position.
  logical_position: (i32, i32),
  /// xdg-output's logical size.
  logical_size: (i32, i32),
}

/// What `client` hears about the output after binding it and its
/// xdg-output.
fn output_values(
  compositor: &mut crate::Compositor,
  client: &mut TestClient,
) -> OutputSeen {
  let output = client.bind::<wl_output::WlOutput>(4);
  let manager = client.bind::<ZxdgOutputManagerV1>(3);
  manager.get_xdg_output(&output, &client.handle, ());
  client.roundtrip(compositor);
  let mut position = (0, 0);
  let mut wl_scale = 0;
  for event in &client.events.output {
    match event {
      wl_output::Event::Geometry { x, y, .. } => position = (*x, *y),
      wl_output::Event::Scale { factor } => wl_scale = *factor,
      _ => {}
    }
  }
  let (mut logical_position, mut logical_size) = ((0, 0), (0, 0));
  for event in &client.events.xdg_output {
    match event {
      zxdg_output_v1::Event::LogicalPosition { x, y } => {
        logical_position = (*x, *y);
      }
      zxdg_output_v1::Event::LogicalSize { width, height } => {
        logical_size = (*width, *height);
      }
      _ => {}
    }
  }
  OutputSeen {
    position,
    scale: wl_scale,
    logical_position,
    logical_size,
  }
}

#[test]
fn xwayland_sees_the_outputs_in_x_pixels_and_others_logically() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 2.0, 2.0);
  let state = fixture.state();
  let output = state.active_output.clone();
  let moved = Rect::new(100, 50, 640, 360);
  state.set_rect_of(&output, moved, true).unwrap();
  let rect = fixture.state().active().rect;
  assert_eq!(rect, moved);
  let path = fixture.path.clone();
  let mut ordinary = TestClient::connect(&mut fixture.compositor, &path);
  let seen = output_values(&mut fixture.compositor, &mut ordinary);
  assert_eq!(seen.scale, 2);
  assert_eq!(seen.position, (100, 50));
  assert_eq!(seen.logical_position, (100, 50));
  assert_eq!(seen.logical_size, (640, 360));
  let seen = output_values(&mut fixture.compositor, &mut fixture.client);
  assert_eq!(seen.scale, 1, "one X pixel is one output pixel");
  assert_eq!(seen.position, (200, 100));
  assert_eq!(seen.logical_position, (200, 100));
  assert_eq!(seen.logical_size, (1280, 720));
}

#[test]
fn a_new_x_scale_reaches_xwaylands_outputs_only() {
  let mut fixture = Fixture::new();
  let path = fixture.path.clone();
  let mut ordinary = TestClient::connect(&mut fixture.compositor, &path);
  output_values(&mut fixture.compositor, &mut ordinary);
  output_values(&mut fixture.compositor, &mut fixture.client);
  let output = fixture.state().active_output.clone();
  fixture.state().set_scale_of(&output, 1.5).unwrap();
  ordinary.roundtrip(&mut fixture.compositor);
  fixture.client.roundtrip(&mut fixture.compositor);
  ordinary.events.xdg_output.clear();
  fixture.client.events.xdg_output.clear();
  let rect = fixture.state().active().rect;
  fixture.state().xwayland.scale = XScale::new(1.5);
  scale::changed(fixture.state());
  ordinary.roundtrip(&mut fixture.compositor);
  fixture.client.roundtrip(&mut fixture.compositor);
  assert!(ordinary.events.xdg_output.is_empty());
  let width = (f64::from(rect.width) * 1.5).round() as i32;
  assert!(
    fixture
      .client
      .events
      .xdg_output
      .iter()
      .any(|event| matches!(
        event,
        zxdg_output_v1::Event::LogicalSize { width: w, .. } if *w == width
      ))
  );
}

#[test]
fn tiled_x_windows_are_configured_in_x_pixels() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 2.0, 2.0);
  map(&mut fixture, XTERM, "XTerm", 5);
  let tile = view_geometry(&mut fixture, "XTerm");
  let requests = fixture.requests();
  assert_eq!(
    last_configure(&requests, XTERM),
    Some(Geometry::new(
      tile.x * 2,
      tile.y * 2,
      tile.width * 2,
      tile.height * 2
    ))
  );
  // A new X scale configures it again, its logical tile unchanged.
  set_scales(&mut fixture, 1.5, 1.5);
  assert_eq!(view_geometry(&mut fixture, "XTerm"), tile);
  let requests = fixture.requests();
  assert_eq!(
    last_configure(&requests, XTERM),
    Some(scale::rect_to_x(tile, XScale::new(1.5)))
  );
}

#[test]
fn x_buffers_are_their_size_over_the_x_scale() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 2.0, 2.0);
  // The fixture's buffers are 64x48 X pixels.
  map(&mut fixture, XTERM, "XTerm", 5);
  let object = object_of(&mut fixture, XTERM);
  let state = fixture.state();
  assert_eq!(crate::viewport::dimensions(state, &object), Some((32, 24)));
  state.xwayland.scale = XScale::default();
  assert_eq!(crate::viewport::dimensions(state, &object), None);
}

#[test]
fn configure_requests_are_answered_in_x_pixels() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 2.0, 2.0);
  // Unmapped: the request comes back exactly, and is the start geometry.
  let asked = Geometry::new(31, 41, 201, 101);
  fixture.apply(XwmEvent::Mapped {
    window: XTERM,
    info: info("XTerm", "x"),
  });
  fixture.apply(XwmEvent::ConfigureRequest {
    window: XTERM,
    geometry: asked,
  });
  assert_eq!(last_configure(&fixture.requests(), XTERM), Some(asked));
  let (surface, _) = fixture.surface(5);
  fixture.commit_buffer(&surface);
  fixture.apply(XwmEvent::Serial {
    window: XTERM,
    serial: 5,
  });
  let window = fixture.windows("XTerm")[0];
  // Floating: converted, clamped to the workarea and converted back.
  shell::toggle_tiling(fixture.state(), window).unwrap();
  fixture.requests();
  fixture.apply(XwmEvent::ConfigureRequest {
    window: XTERM,
    geometry: Geometry::new(-100, 10, 300, 200),
  });
  assert_eq!(
    last_configure(&fixture.requests(), XTERM),
    Some(Geometry::new(0, 10, 300, 200))
  );
  assert_eq!(
    view_geometry(&mut fixture, "XTerm"),
    Rect::new(0, 5, 150, 100)
  );
}

#[test]
fn size_hints_are_logical_sizes() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 2.0, 2.0);
  let mut hinted = info("XTerm", "x");
  hinted.size_hints.min = Some((200, 100));
  hinted.size_hints.max = Some((400, 0));
  map_info(&mut fixture, XTERM, hinted, 5);
  let object = object_of(&mut fixture, XTERM);
  let toplevel = fixture.state().surfaces[&object].role.toplevel().unwrap();
  assert_eq!(toplevel.min_size, (100, 50));
  assert_eq!(toplevel.max_size, (200, 0));
}

#[test]
fn override_redirect_windows_are_placed_from_x_pixels() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 2.0, 2.0);
  map(&mut fixture, XTERM, "XTerm", 5);
  let mut menu = info("XTerm", "");
  menu.override_redirect = true;
  menu.types = vec![WindowType::Menu];
  menu.geometry = Geometry::new(200, 100, 64, 48);
  map_info(&mut fixture, MENU, menu, 6);
  let object = object_of(&mut fixture, MENU);
  let state = fixture.state();
  assert_eq!(
    shell::surface_geometry(state, &object),
    Some(Rect::new(100, 50, 32, 24))
  );
  // The pointer over it lands on it, in X pixels.
  crate::input::pointer_motion(state, 110.0, 60.0);
  assert_eq!(state.input.pointer_focus.as_ref(), Some(&object));
}

#[test]
fn pointer_and_touch_positions_reach_x_windows_in_x_pixels() {
  let mut fixture = Fixture::new();
  set_scales(&mut fixture, 1.5, 1.5);
  let seat = fixture.client.bind::<wl_seat::WlSeat>(7);
  let handle = fixture.client.handle.clone();
  seat.get_pointer(&handle, ());
  seat.get_touch(&handle, ());
  fixture.client.roundtrip(&mut fixture.compositor);
  map(&mut fixture, XTERM, "XTerm", 5);
  let tile = view_geometry(&mut fixture, "XTerm");
  let (x, y) = (f64::from(tile.x) + 20.0, f64::from(tile.y) + 10.0);
  crate::input::pointer_motion(fixture.state(), x, y);
  crate::input::pointer_motion(fixture.state(), x + 2.0, y);
  crate::input::touch_down(fixture.state(), 0, x, y);
  fixture.client.roundtrip(&mut fixture.compositor);
  let pointer = &fixture.client.events.pointer;
  assert!(pointer.iter().any(|event| matches!(event,
    wl_pointer::Event::Motion { surface_x, surface_y, .. }
      if *surface_x == 30.0 && *surface_y == 15.0)));
  assert!(pointer.iter().any(|event| matches!(event,
    wl_pointer::Event::Motion { surface_x, .. } if *surface_x == 33.0)));
  assert!(
    fixture
      .client
      .events
      .touch
      .iter()
      .any(|event| matches!(event,
    wayland_client::protocol::wl_touch::Event::Down { x, y, .. }
      if *x == 30.0 && *y == 15.0))
  );
}
