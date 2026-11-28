//! Server-side frames through the in-process test client: which toplevels
//! get one, how it changes their configured size and geometry, and what
//! pointer input on it does.

use serde_json::{Value, json};
use shingle_core::{Rect, WindowId};
use wayland_client::{Proxy, protocol::wl_pointer, protocol::wl_seat::WlSeat};
use wayland_protocols::xdg::decoration::zv1::client::{
  zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
  zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};

use crate::{
  Compositor, Config, Event, InputDeviceId, InputDeviceInfo, InputDeviceKind,
  frame::{FrameAreaKind, FrameButton, FrameSnapshot, Margins},
  test_client::{TestClient, Toplevel},
  tests::test_compositor_with,
};

const BTN_LEFT: u32 = 0x110;

struct Fixture {
  compositor: Compositor,
  client: TestClient,
  pointer: InputDeviceId,
  _path: std::path::PathBuf,
}

impl Fixture {
  fn new(config: Config) -> Self {
    let (mut compositor, path) = test_compositor_with(config);
    let pointer = compositor.add_input_device(InputDeviceInfo {
      name: "test pointer".into(),
      vendor: None,
      product: None,
      kind: InputDeviceKind::Pointer,
    });
    let client = TestClient::connect(&mut compositor, &path);
    Self {
      compositor,
      client,
      pointer,
      _path: path,
    }
  }

  /// Two round trips: the client acknowledges a configure while it reads
  /// the first one's events, and the compositor applies a decoration
  /// mode with that acknowledgement in the second.
  fn roundtrip(&mut self) {
    self.client.roundtrip(&mut self.compositor);
    self.client.roundtrip(&mut self.compositor);
  }

  /// A mapped toplevel, with a decoration object in `mode` if any.
  fn window(
    &mut self,
    app_id: &str,
    mode: Option<zxdg_toplevel_decoration_v1::Mode>,
  ) -> (Toplevel, Option<ZxdgToplevelDecorationV1>, WindowId) {
    let toplevel = self.client.toplevel(&mut self.compositor, app_id);
    let decoration = mode.map(|mode| {
      let manager = self.client.bind::<ZxdgDecorationManagerV1>(1);
      let decoration = manager.get_toplevel_decoration(
        &toplevel.toplevel,
        &self.client.handle,
        (),
      );
      decoration.set_mode(mode);
      decoration
    });
    self.roundtrip();
    self
      .client
      .attach_buffer(&mut self.compositor, &toplevel.surface, 64, 64);
    self.roundtrip();
    let window = self.compositor.focused_window().expect("it maps");
    (toplevel, decoration, window)
  }

  fn ssd(&mut self, app_id: &str) -> (Toplevel, WindowId) {
    let (toplevel, decoration, window) =
      self.window(app_id, Some(zxdg_toplevel_decoration_v1::Mode::ServerSide));
    std::mem::forget(decoration);
    (toplevel, window)
  }

  fn frame(&self, window: WindowId) -> Option<FrameSnapshot> {
    self
      .compositor
      .frames()
      .into_values()
      .find(|frame| frame.window == window)
  }

  fn geometry(&self, window: WindowId) -> Rect {
    crate::shell::window_geometry(
      &self.compositor.state,
      &crate::input::object_for_window(&self.compositor.state, window)
        .expect("the window has a surface"),
    )
    .expect("the window has a geometry")
  }

  fn surface_rect(&self, window: WindowId) -> Rect {
    self
      .compositor
      .window_surfaces(window)
      .first()
      .expect("the window is drawn")
      .geometry
  }

  fn ipc(&mut self, method: &str, data: Value) -> Value {
    crate::ipc::respond(
      &mut self.compositor.state,
      &json!({"method": method, "data": data}),
    )
  }

  /// Moves the pointer to the frame point (`x`, `y`) of `window`.
  fn point_at(&mut self, window: WindowId, x: f64, y: f64) {
    let rect = self.geometry(window);
    self.compositor.pointer_motion(
      self.pointer,
      f64::from(rect.x) + x,
      f64::from(rect.y) + y,
    );
    self.roundtrip();
  }

  /// Clicks the left button where the pointer is.
  fn click(&mut self) {
    self.compositor.pointer_button(self.pointer, BTN_LEFT, true);
    self
      .compositor
      .pointer_button(self.pointer, BTN_LEFT, false);
    self.roundtrip();
  }

  /// The centre of `window`'s `button`, in frame coordinates.
  fn button_centre(&self, window: WindowId, button: FrameButton) -> (f64, f64) {
    let frame = self.frame(window).expect("the window has a frame");
    let rect = frame
      .layout
      .areas
      .iter()
      .find(|area| area.kind == FrameAreaKind::Button(button))
      .expect("the frame has the button")
      .rect;
    (
      f64::from(rect.x) + f64::from(rect.width) / 2.0,
      f64::from(rect.y) + f64::from(rect.height) / 2.0,
    )
  }
}

fn default_margins() -> Margins {
  crate::frame::FrameTheme::default().margins()
}

#[test]
fn a_server_side_toplevel_is_configured_inside_its_framed_tile() {
  let mut fixture = Fixture::new(Config::default());
  let (toplevel, window) = fixture.ssd("framed");
  let modes = fixture.client.events.decoration_modes.clone();
  assert_eq!(
    modes.last().map(|(_, mode)| *mode),
    Some(zxdg_toplevel_decoration_v1::Mode::ServerSide as u32)
  );
  let tile = fixture
    .compositor
    .core()
    .window_layout(window)
    .unwrap()
    .geometry;
  let content = default_margins().shrink(tile);
  assert_eq!(fixture.geometry(window), tile);
  assert_eq!(
    fixture.client.events.last_configure(&toplevel.toplevel),
    Some((content.width as i32, content.height as i32))
  );
  assert_eq!(fixture.surface_rect(window), content);
  let frame = fixture.frame(window).expect("a frame");
  assert_eq!(frame.rect, tile);
  assert_eq!(frame.margins, default_margins());
  assert!(frame.activated);
  // Borders of a tile take input but do not resize.
  assert!(
    frame
      .layout
      .areas
      .iter()
      .all(|area| !matches!(area.kind, FrameAreaKind::Resize(_)))
  );
  // IPC describes the frame as the view's geometry and the client as its
  // base geometry, as Wayfire's view_to_json does.
  let info = fixture.ipc("window-rules/view-info", json!({"id": window.0}));
  let rect = |value: &Value| {
    Rect::new(
      value["x"].as_i64().unwrap() as i32,
      value["y"].as_i64().unwrap() as i32,
      value["width"].as_u64().unwrap() as u32,
      value["height"].as_u64().unwrap() as u32,
    )
  };
  assert_eq!(rect(&info["info"]["geometry"]), tile, "{info}");
  assert_eq!(rect(&info["info"]["bbox"]), tile);
  assert_eq!(rect(&info["info"]["base-geometry"]), content);
}

#[test]
fn client_side_toplevels_and_an_unloaded_plugin_have_no_frame() {
  let mut fixture = Fixture::new(Config::default());
  let (plain, _, plain_window) = fixture.window("plain", None);
  let (csd, _decoration, csd_window) =
    fixture.window("csd", Some(zxdg_toplevel_decoration_v1::Mode::ClientSide));
  for (toplevel, window) in [(&plain, plain_window), (&csd, csd_window)] {
    assert!(fixture.frame(window).is_none());
    let tile = fixture
      .compositor
      .core()
      .window_layout(window)
      .unwrap()
      .geometry;
    assert_eq!(
      fixture.client.events.last_configure(&toplevel.toplevel),
      Some((tile.width as i32, tile.height as i32))
    );
  }
  let mut fixture = Fixture::new(Config {
    decorations: false,
    ..Config::default()
  });
  let (_, window) = fixture.ssd("unloaded");
  assert!(fixture.frame(window).is_none());
}

#[test]
fn the_preferred_mode_applies_unless_the_client_asks_otherwise() {
  use zxdg_toplevel_decoration_v1::Mode;
  let mut fixture = Fixture::new(Config {
    preferred_decoration_mode: "server".into(),
    ..Config::default()
  });
  let (toplevel, _, window) = fixture.window("preferring", None);
  assert!(fixture.frame(window).is_none(), "no object, no frame");
  let manager = fixture.client.bind::<ZxdgDecorationManagerV1>(1);
  let decoration = manager.get_toplevel_decoration(
    &toplevel.toplevel,
    &fixture.client.handle,
    (),
  );
  fixture.roundtrip();
  let last_mode = |fixture: &Fixture| {
    fixture
      .client
      .events
      .decoration_modes
      .iter()
      .rev()
      .find(|(id, _)| *id == decoration.id())
      .map(|(_, mode)| *mode)
  };
  assert_eq!(last_mode(&fixture), Some(Mode::ServerSide as u32));
  assert!(fixture.frame(window).is_some());
  decoration.set_mode(Mode::ClientSide);
  fixture.roundtrip();
  assert_eq!(last_mode(&fixture), Some(Mode::ClientSide as u32));
  assert!(fixture.frame(window).is_none());
  decoration.unset_mode();
  fixture.roundtrip();
  assert!(fixture.frame(window).is_some());
  // Forcing the preferred mode ignores the client's wish.
  let mut config = Config {
    preferred_decoration_mode: "server".into(),
    force_preferred_decoration_mode: true,
    ..Config::default()
  };
  fixture.compositor.apply_config(config.clone()).unwrap();
  decoration.set_mode(Mode::ClientSide);
  fixture.roundtrip();
  assert_eq!(last_mode(&fixture), Some(Mode::ServerSide as u32));
  assert!(fixture.frame(window).is_some());
  config.force_preferred_decoration_mode = false;
  fixture.compositor.apply_config(config).unwrap();
  fixture.roundtrip();
  assert_eq!(last_mode(&fixture), Some(Mode::ClientSide as u32));
  assert!(fixture.frame(window).is_none());
}

#[test]
fn forced_and_ignored_views_override_the_client() {
  let decoration = shingle_config::DecorationConfig {
    forced_views: "app_id is \"forced\"".to_owned().try_into().unwrap(),
    ignore_views: "app_id is \"ignored\"".to_owned().try_into().unwrap(),
    ..shingle_config::DecorationConfig::default()
  };
  let mut fixture = Fixture::new(Config {
    decoration,
    ..Config::default()
  });
  let (_, _, forced) = fixture.window("forced", None);
  assert!(fixture.frame(forced).is_some());
  let (_, ignored) = fixture.ssd("ignored");
  assert!(fixture.frame(ignored).is_none());
}

#[test]
fn the_frame_buttons_close_maximize_and_minimize() {
  let mut fixture = Fixture::new(Config::default());
  let seat = fixture.client.bind::<WlSeat>(7);
  let _pointer = seat.get_pointer(&fixture.client.handle, ());
  let (toplevel, window) = fixture.ssd("buttons");
  shell_float(&mut fixture, window);
  // On the frame, the client has no pointer focus.
  let (x, y) = fixture.button_centre(window, FrameButton::Close);
  fixture.client.events.pointer.clear();
  fixture.point_at(window, x, 2.0);
  fixture.point_at(window, x, y);
  assert!(
    !fixture
      .client
      .events
      .pointer
      .iter()
      .any(|event| matches!(event, wl_pointer::Event::Enter { .. }))
  );
  assert_eq!(fixture.frame(window).unwrap().hovered, Some(0));
  fixture.client.events.pointer.clear();
  fixture.click();
  assert!(
    !fixture
      .client
      .events
      .pointer
      .iter()
      .any(|event| matches!(event, wl_pointer::Event::Button { .. })),
    "the client got the click"
  );
  assert_eq!(fixture.client.events.closes, [toplevel.toplevel.id()]);
  let (x, y) = fixture.button_centre(window, FrameButton::Maximize);
  fixture.point_at(window, x, y);
  fixture.click();
  assert!(
    fixture
      .compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .maximized
  );
  // The maximized frame is the workarea, and the client is inside it.
  let workarea = fixture.compositor.workarea();
  assert_eq!(fixture.geometry(window), workarea);
  assert_eq!(
    fixture.surface_rect(window),
    default_margins().shrink(workarea)
  );
  let (x, y) = fixture.button_centre(window, FrameButton::Maximize);
  fixture.point_at(window, x, y);
  fixture.click();
  assert!(
    !fixture
      .compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .maximized
  );
  let (x, y) = fixture.button_centre(window, FrameButton::Minimize);
  fixture.point_at(window, x, y);
  fixture.click();
  assert!(
    fixture
      .compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .minimized
  );
}

fn shell_float(fixture: &mut Fixture, window: WindowId) {
  crate::shell::toggle_tiling(&mut fixture.compositor.state, window).unwrap();
  fixture.roundtrip();
}

#[test]
fn a_frame_hanging_over_takes_input_where_it_is_drawn() {
  let mut fixture = Fixture::new(Config {
    workspace_width: 2,
    workspace_height: 1,
    ..Config::default()
  });
  let seat = fixture.client.bind::<WlSeat>(7);
  let _pointer = seat.get_pointer(&fixture.client.handle, ());
  let (toplevel, window) = fixture.ssd("over");
  shell_float(&mut fixture, window);
  // Its centre, 1230, is on the first workspace, its right part on the
  // second.
  crate::shell::set_floating_geometry(
    &mut fixture.compositor.state,
    window,
    Rect::new(1080, 100, 300, 200),
  )
  .unwrap();
  fixture.roundtrip();
  let reply = fixture.ipc(
    "vswitch/set-workspace",
    json!({"output-id": 1, "x": 1, "y": 0}),
  );
  assert_eq!(reply, json!({"result": "ok"}));
  fixture.roundtrip();
  // The close button is drawn at the second workspace's left edge.
  let (x, y) = fixture.button_centre(window, FrameButton::Close);
  let drawn_x = 1080.0 - 1280.0 + x;
  assert!(drawn_x > 0.0, "the button is on the output");
  fixture
    .compositor
    .pointer_motion(fixture.pointer, drawn_x, 100.0 + y);
  fixture.roundtrip();
  assert_eq!(fixture.frame(window).unwrap().hovered, Some(0));
  fixture.click();
  assert_eq!(fixture.client.events.closes, [toplevel.toplevel.id()]);
}

#[test]
fn dragging_the_title_moves_and_a_border_press_resizes() {
  let mut fixture = Fixture::new(Config::default());
  let (_, window) = fixture.ssd("drag");
  shell_float(&mut fixture, window);
  // Untiled, it is back at its own 64x64; a title bar long enough to
  // drag needs a wider window.
  crate::shell::set_floating_geometry(
    &mut fixture.compositor.state,
    window,
    Rect::new(0, 0, 400, 300),
  )
  .unwrap();
  fixture.roundtrip();
  fixture.compositor.drain_events();
  fixture.point_at(window, 60.0, 20.0);
  fixture
    .compositor
    .pointer_button(fixture.pointer, BTN_LEFT, true);
  fixture.point_at(window, 63.0, 20.0);
  let moves = |fixture: &mut Fixture| {
    fixture
      .compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::BeginMove { window } => Some((window, 0)),
        Event::BeginResize { window, edges } => Some((window, edges)),
        _ => None,
      })
      .collect::<Vec<_>>()
  };
  assert!(moves(&mut fixture).is_empty());
  fixture.point_at(window, 70.0, 20.0);
  assert_eq!(moves(&mut fixture), [(window, 0)]);
  fixture
    .compositor
    .pointer_button(fixture.pointer, BTN_LEFT, false);
  // The bottom right corner resizes both edges.
  let rect = fixture.geometry(window);
  fixture.point_at(
    window,
    f64::from(rect.width) - 1.0,
    f64::from(rect.height) - 1.0,
  );
  assert_eq!(
    fixture.compositor.state.frame_pointer.cursor,
    Some(crate::cursor_shape::Shape::SeResize)
  );
  fixture
    .compositor
    .pointer_button(fixture.pointer, BTN_LEFT, true);
  assert_eq!(moves(&mut fixture), [(window, 2 | 8)]);
  fixture
    .compositor
    .pointer_button(fixture.pointer, BTN_LEFT, false);
  // A double click on the title toggles maximize. Going through the
  // client first starts afresh, without the presses above.
  fixture.point_at(window, 200.0, 150.0);
  fixture.point_at(window, 60.0, 20.0);
  fixture.click();
  fixture.click();
  assert!(
    fixture
      .compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .maximized
  );
}

#[test]
fn a_floating_window_keeps_its_client_as_its_frame_comes_and_goes() {
  let mut fixture = Fixture::new(Config::default());
  let (toplevel, decoration, window) = fixture.window(
    "floating",
    Some(zxdg_toplevel_decoration_v1::Mode::ClientSide),
  );
  let decoration = decoration.unwrap();
  shell_float(&mut fixture, window);
  let floating = Rect::new(200, 150, 400, 300);
  crate::shell::set_floating_geometry(
    &mut fixture.compositor.state,
    window,
    floating,
  )
  .unwrap();
  fixture.roundtrip();
  assert!(fixture.frame(window).is_none());
  decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
  fixture.roundtrip();
  let framed = default_margins().expand(floating);
  assert_eq!(fixture.geometry(window), framed);
  assert_eq!(
    fixture
      .compositor
      .core()
      .window_info(window)
      .unwrap()
      .state
      .floating,
    Some(framed)
  );
  assert_eq!(fixture.surface_rect(window), floating);
  assert_eq!(
    fixture.client.events.last_configure(&toplevel.toplevel),
    Some((400, 300))
  );
  // Destroying the decoration object gives the client its decorations
  // back, and the window its old rectangle.
  decoration.destroy();
  fixture.roundtrip();
  assert!(fixture.frame(window).is_none());
  assert_eq!(fixture.geometry(window), floating);
  assert_eq!(
    fixture.client.events.last_configure(&toplevel.toplevel),
    Some((400, 300))
  );
}

#[test]
fn a_fullscreen_window_has_no_frame() {
  let mut fixture = Fixture::new(Config::default());
  let (toplevel, window) = fixture.ssd("fullscreen");
  crate::shell::set_fullscreen(&mut fixture.compositor.state, window, true)
    .unwrap();
  fixture.roundtrip();
  assert!(fixture.frame(window).is_none());
  let output = fixture.compositor.output();
  assert_eq!(fixture.surface_rect(window), output);
  assert_eq!(
    fixture.client.events.last_configure(&toplevel.toplevel),
    Some((output.width as i32, output.height as i32))
  );
  crate::shell::set_fullscreen(&mut fixture.compositor.state, window, false)
    .unwrap();
  assert!(fixture.frame(window).is_some());
}

#[test]
fn a_reload_changes_the_frame_and_can_remove_it() {
  let mut fixture = Fixture::new(Config::default());
  let (toplevel, window) = fixture.ssd("reload");
  let tile = fixture
    .compositor
    .core()
    .window_layout(window)
    .unwrap()
    .geometry;
  let decoration = shingle_config::DecorationConfig {
    title_height: 20,
    border_size: 1,
    ..shingle_config::DecorationConfig::default()
  };
  fixture
    .compositor
    .apply_config(Config {
      decoration,
      ..Config::default()
    })
    .unwrap();
  fixture.roundtrip();
  assert_eq!(
    fixture.client.events.last_configure(&toplevel.toplevel),
    Some((tile.width as i32 - 2, tile.height as i32 - 22))
  );
  fixture
    .compositor
    .apply_config(Config {
      decorations: false,
      ..Config::default()
    })
    .unwrap();
  fixture.roundtrip();
  assert!(fixture.frame(window).is_none());
  assert_eq!(
    fixture.client.events.last_configure(&toplevel.toplevel),
    Some((tile.width as i32, tile.height as i32))
  );
}

#[test]
fn a_placed_window_and_its_size_limits_include_the_frame() {
  let mut fixture = Fixture::new(Config {
    tile_by_default: "none".to_owned().try_into().unwrap(),
    ..Config::default()
  });
  let (toplevel, window) = fixture.ssd("placed");
  // Placement centres the client's 64x64 buffer with its frame around it.
  let margins = default_margins();
  let frame = fixture.geometry(window);
  assert_eq!(
    (frame.width, frame.height),
    (
      64 + margins.left + margins.right,
      64 + margins.top + margins.bottom
    )
  );
  assert_eq!(fixture.surface_rect(window), margins.shrink(frame));
  let workarea = fixture.compositor.workarea();
  assert_eq!(
    frame.x,
    workarea.x + (workarea.width as i32 - frame.width as i32) / 2
  );
  // The client's size limits grow by the frame; no limit stays none.
  toplevel.toplevel.set_min_size(100, 50);
  toplevel.surface.commit();
  fixture.roundtrip();
  assert_eq!(
    crate::shell::size_constraints(&fixture.compositor.state, window),
    Some(((108, 88), (0, 0)))
  );
}

#[test]
fn a_decoration_its_client_never_destroyed_goes_with_the_client() {
  let (mut compositor, path) = test_compositor_with(Config::default());
  let mut client = TestClient::connect(&mut compositor, &path);
  let toplevel = client.toplevel(&mut compositor, "leaving");
  let manager = client.bind::<ZxdgDecorationManagerV1>(1);
  let decoration =
    manager.get_toplevel_decoration(&toplevel.toplevel, &client.handle, ());
  decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.state.decorations.len(), 1);
  // The client exits without destroying anything, as a killed one does.
  drop((toplevel, decoration, manager, client));
  for _ in 0..10 {
    compositor
      .dispatch_pending()
      .expect("the compositor dispatches");
  }
  assert!(compositor.state.decorations.is_empty());
}
