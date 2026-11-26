//! Wayland drag icons through the in-process test client: drawn at the
//! pointer moved by the icon's offsets, out of input, redrawn as the
//! pointer moves, and gone after the drop.

use shingle_core::Rect;
use wayland_client::{
  Proxy,
  protocol::{
    wl_compositor::WlCompositor,
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{DndAction, WlDataDeviceManager},
    wl_data_source::WlDataSource,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
  },
};

use crate::{
  Compositor, Event, SurfaceRole, SurfaceSnapshot,
  test_client::{TestClient, Toplevel},
  tests::test_compositor,
};

const LEFT: u32 = 0x110;

/// A client with a 200x200 window under the pointer, a data device and a
/// surface to drag as the icon.
struct Fixture {
  compositor: Compositor,
  client: TestClient,
  window: Toplevel,
  manager: WlDataDeviceManager,
  device: WlDataDevice,
  icon: WlSurface,
}

impl Fixture {
  fn new() -> Self {
    let (mut compositor, path) = test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let window = client.toplevel(&mut compositor, "source");
    client.attach_buffer(&mut compositor, &window.surface, 200, 200);
    let seat = client.bind::<WlSeat>(5);
    let manager = client.bind::<WlDataDeviceManager>(3);
    let device = manager.get_data_device(&seat, &client.handle, ());
    // wl_surface.offset needs version 5.
    let surfaces = client.bind::<WlCompositor>(5);
    let icon = surfaces.create_surface(&client.handle, ());
    client.roundtrip(&mut compositor);
    let mut fixture = Self {
      compositor,
      client,
      window,
      manager,
      device,
      icon,
    };
    let rect = fixture.window_rect();
    fixture.move_to(f64::from(rect.x) + 50.0, f64::from(rect.y) + 50.0);
    fixture
  }

  fn window_rect(&self) -> Rect {
    let object = self.server(&self.window.surface);
    crate::shell::surface_geometry(&self.compositor.state, &object).unwrap()
  }

  /// The compositor's object for the client's surface `surface`.
  fn server(&self, surface: &WlSurface) -> wayland_server::backend::ObjectId {
    let protocol_id = surface.id().protocol_id();
    self
      .compositor
      .state
      .surfaces
      .keys()
      .find(|id| id.protocol_id() == protocol_id)
      .cloned()
      .unwrap()
  }

  fn move_to(&mut self, x: f64, y: f64) {
    crate::input::pointer_motion(&mut self.compositor.state, x, y);
    self.client.roundtrip(&mut self.compositor);
  }

  fn button(&mut self, pressed: bool) {
    crate::input::pointer_button(&mut self.compositor.state, LEFT, pressed);
    self.client.roundtrip(&mut self.compositor);
  }

  /// Presses the button and starts a drag of text with `icon`.
  fn start(&mut self, icon: Option<&WlSurface>) -> WlDataSource {
    self.button(true);
    let source = self.manager.create_data_source(&self.client.handle, ());
    source.offer("text/plain".into());
    source.set_actions(DndAction::Copy);
    self
      .device
      .start_drag(Some(&source), &self.window.surface, icon, 1);
    self.client.roundtrip(&mut self.compositor);
    source
  }

  /// Gives the icon a 16x16 buffer, moved by (`dx`, `dy`).
  fn draw_icon(&mut self, dx: i32, dy: i32) {
    let buffer = self.client.shm_buffer(16, 16);
    self.icon.attach(Some(&buffer), 0, 0);
    self.icon.offset(dx, dy);
    self.icon.damage(0, 0, 16, 16);
    self.icon.commit();
    self.client.roundtrip(&mut self.compositor);
  }

  /// What the primary output draws, bottom to top.
  fn drawn(&self) -> Vec<SurfaceSnapshot> {
    let state = &self.compositor.state;
    state.visible_surfaces_on(&state.primary().id)
  }

  fn icon_rect(&self) -> Option<Rect> {
    self
      .drawn()
      .into_iter()
      .find(|snapshot| snapshot.role == SurfaceRole::DragIcon)
      .map(|snapshot| snapshot.geometry)
  }

  /// The geometries of the drag icon commit events since the last call.
  fn icon_events(&mut self) -> Vec<Rect> {
    self
      .compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::SurfaceCommitted(snapshot)
          if snapshot.role == SurfaceRole::DragIcon =>
        {
          Some(snapshot.geometry)
        }
        _ => None,
      })
      .collect()
  }

  fn pointer(&self) -> (i32, i32) {
    let input = &self.compositor.state.input;
    (input.pointer_x as i32, input.pointer_y as i32)
  }
}

#[test]
fn the_icon_follows_the_pointer_by_its_offsets() {
  let mut fixture = Fixture::new();
  let icon = fixture.icon.clone();
  let _source = fixture.start(Some(&icon));
  // No buffer yet, nothing to draw.
  assert_eq!(fixture.icon_rect(), None);
  fixture.draw_icon(-8, -4);
  let (x, y) = fixture.pointer();
  assert_eq!(fixture.icon_rect(), Some(Rect::new(x - 8, y - 4, 16, 16)));
  // It is drawn just below the cursor, above the window.
  let drawn = fixture.drawn();
  let roles = drawn
    .iter()
    .map(|snapshot| snapshot.role)
    .collect::<Vec<_>>();
  let icon_at = roles.iter().position(|role| *role == SurfaceRole::DragIcon);
  let window_at = roles.iter().position(|role| *role == SurfaceRole::Toplevel);
  assert!(window_at < icon_at, "{roles:?}");
  assert!(
    roles[icon_at.unwrap() + 1..]
      .iter()
      .all(|role| *role == SurfaceRole::Cursor),
    "{roles:?}"
  );
  fixture.icon_events();
  fixture.move_to(f64::from(x) + 30.0, f64::from(y) + 20.0);
  let moved = Rect::new(x + 22, y + 16, 16, 16);
  assert_eq!(fixture.icon_rect(), Some(moved));
  // The frontend hears of the new place and of the old one.
  let events = fixture.icon_events();
  assert!(events.contains(&moved), "{events:?}");
  assert!(
    events.contains(&Rect::new(x - 8, y - 4, 16, 16)),
    "{events:?}"
  );
  // Offsets add up, as they do in wlroots' scene.
  fixture.draw_icon(2, 3);
  assert_eq!(fixture.icon_rect(), Some(Rect::new(x + 24, y + 19, 16, 16)));
}

#[test]
fn the_icon_takes_no_input_and_goes_with_the_drop() {
  let mut fixture = Fixture::new();
  let icon = fixture.icon.clone();
  let _source = fixture.start(Some(&icon));
  // An icon right under the pointer.
  fixture.draw_icon(-8, -8);
  let (x, y) = fixture.pointer();
  fixture.move_to(f64::from(x) + 5.0, f64::from(y) + 5.0);
  let window = fixture.server(&fixture.window.surface);
  assert_eq!(
    crate::input::drag_target(&fixture.compositor.state),
    Some(window)
  );
  assert!(
    fixture.client.events.drag.contains(&"enter"),
    "{:?}",
    fixture.client.events.drag
  );
  let shown = fixture.icon_rect().unwrap();
  fixture.icon_events();
  fixture.button(false);
  assert_eq!(fixture.icon_rect(), None);
  assert_eq!(fixture.icon_events(), [shown]);
  // Nothing more once it is gone.
  fixture.move_to(10.0, 10.0);
  assert!(fixture.icon_events().is_empty());
}

#[test]
fn an_icon_is_reused_from_its_own_place_and_no_other_role_may_be_one() {
  let mut fixture = Fixture::new();
  let icon = fixture.icon.clone();
  let _first = fixture.start(Some(&icon));
  fixture.draw_icon(5, 5);
  fixture.button(false);
  // The next drag starts the icon at the pointer again.
  let _second = fixture.start(Some(&icon));
  let (x, y) = fixture.pointer();
  assert_eq!(fixture.icon_rect(), Some(Rect::new(x, y, 16, 16)));
  fixture.button(false);
  // A toplevel's surface cannot be the icon.
  let window = fixture.window.surface.clone();
  fixture.button(true);
  let source = fixture
    .manager
    .create_data_source(&fixture.client.handle, ());
  fixture.device.start_drag(
    Some(&source),
    &fixture.window.surface,
    Some(&window),
    1,
  );
  let error = fixture
    .client
    .protocol_error(&mut fixture.compositor)
    .expect("the compositor refuses the role");
  assert_eq!(error.code, wl_data_device::Error::Role as u32);
}
