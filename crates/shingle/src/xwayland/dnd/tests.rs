//! The drag and drop bridge's Wayland half with an X window from the
//! fixture and the in-process test client as the Wayland side: what each
//! turn asks of the window manager, and what its events do.

use std::{os::fd::AsFd, time::Duration};

use shingle_xwayland::{DndAction, SelectionSend, xwm::mime};
use wayland_client::Proxy;
use wayland_client::protocol::{
  wl_data_device::WlDataDevice,
  wl_data_device_manager::{DndAction as Action, WlDataDeviceManager},
  wl_data_source::WlDataSource,
  wl_seat,
};

use super::*;
use crate::{
  test_client::{TestClient, Toplevel},
  xwayland::fixture::{Fixture, XTERM, map},
};

const UTF8: &str = mime::UTF8_TEXT;
const LEFT: u32 = 0x110;

/// The fixture with xterm and a Wayland window side by side, and the
/// Wayland client's data device.
struct Drag {
  fixture: Fixture,
  wayland: TestClient,
  toplevel: Toplevel,
  manager: WlDataDeviceManager,
  device: WlDataDevice,
  bridge: DragBridge,
}

impl Drag {
  fn new() -> Self {
    let mut fixture = Fixture::new();
    map(&mut fixture, XTERM, "XTerm", 5);
    let mut wayland =
      TestClient::connect(&mut fixture.compositor, &fixture.path);
    let toplevel = wayland.toplevel(&mut fixture.compositor, "foot");
    wayland.attach_buffer(&mut fixture.compositor, &toplevel.surface, 64, 64);
    let seat = wayland.bind::<wl_seat::WlSeat>(5);
    let manager = wayland.bind::<WlDataDeviceManager>(3);
    let device = manager.get_data_device(&seat, &wayland.handle, ());
    wayland.roundtrip(&mut fixture.compositor);
    let mut drag = Self {
      fixture,
      wayland,
      toplevel,
      manager,
      device,
      bridge: DragBridge::default(),
    };
    assert_ne!(drag.centre(true), drag.centre(false));
    drag
  }

  fn state(&mut self) -> &mut CompositorState {
    self.fixture.state()
  }

  /// The surface of xterm or of the Wayland window.
  fn object(&mut self, x: bool) -> ObjectId {
    if x {
      let state = self.state();
      let surface = state.xwayland.windows[&XTERM].surface.unwrap();
      state.surface_ids[&surface].clone()
    } else {
      let state = self.state();
      state
        .surfaces
        .iter()
        .find(|(_, surface)| {
          surface
            .role
            .toplevel()
            .is_some_and(|toplevel| toplevel.app_id == "foot")
        })
        .map(|(object, _)| object.clone())
        .unwrap()
    }
  }

  /// The centre of xterm or of the Wayland window.
  fn centre(&mut self, x: bool) -> (f64, f64) {
    let object = self.object(x);
    let rect = crate::shell::surface_geometry(self.state(), &object).unwrap();
    (
      f64::from(rect.x) + f64::from(rect.width) / 2.0,
      f64::from(rect.y) + f64::from(rect.height) / 2.0,
    )
  }

  /// Moves the pointer to the centre of xterm or of the Wayland window.
  fn move_to(&mut self, x: bool) {
    let (px, py) = self.centre(x);
    crate::input::pointer_motion(self.state(), px, py);
  }

  /// Runs this turn's drag sync and returns the bridge's plan.
  fn turn(&mut self) -> Plan {
    let compositor = &mut self.fixture.compositor;
    crate::data_device::sync_drag(
      &mut compositor.state,
      &compositor.display.handle(),
    );
    let plan = self.bridge.plan(&mut compositor.state);
    self.wayland.roundtrip(compositor);
    plan
  }

  fn roundtrip(&mut self) {
    self.wayland.roundtrip(&mut self.fixture.compositor);
  }

  /// Starts a Wayland drag of UTF-8 text from the Wayland window.
  fn start_wayland(&mut self) -> WlDataSource {
    self.move_to(false);
    crate::input::pointer_button(self.state(), LEFT, true);
    let source = self.manager.create_data_source(&self.wayland.handle, ());
    source.offer(UTF8.into());
    source.set_actions(Action::Copy | Action::Move);
    self
      .device
      .start_drag(Some(&source), &self.toplevel.surface, None, 1);
    self.roundtrip();
    source
  }

  /// Starts an X client's drag from xterm.
  fn start_x11(&mut self) {
    self.move_to(true);
    crate::input::pointer_button(self.state(), LEFT, true);
    apply(
      self.state(),
      XwmEvent::DragOwnerChanged { owner: Some(XTERM) },
    );
  }
}

#[test]
fn a_wayland_drag_over_an_x_window_is_reported_and_dropped() {
  let mut drag = Drag::new();
  let source = drag.start_wayland();
  let plan = drag.turn();
  assert_eq!(plan.start, Some((vec![UTF8.to_owned()], 3)));
  assert_eq!(plan.motion, Some(None));
  // The Wayland window is entered through its own data device.
  assert_eq!(drag.wayland.events.drag, ["enter"]);
  drag.move_to(true);
  let plan = drag.turn();
  assert_eq!(plan.start, None);
  assert_eq!(plan.motion, Some(Some(XTERM)));
  assert_eq!(drag.wayland.events.drag, ["enter", "leave"]);
  // xterm's verdict reaches the source.
  apply(
    drag.state(),
    XwmEvent::DragStatus {
      accepted: true,
      action: DndAction::Copy,
    },
  );
  drag.roundtrip();
  assert_eq!(
    drag.wayland.events.data_source,
    [format!("target {UTF8}"), "action 1".into()]
  );
  // An X target's data request goes to the source.
  let (_read, write) = rustix::pipe::pipe().unwrap();
  super::super::selection::send(
    drag.state(),
    SelectionSend {
      selection: shingle_xwayland::Selection::Dnd,
      mime_type: UTF8.into(),
      fd: write,
    },
  );
  drag.roundtrip();
  assert_eq!(
    drag.wayland.events.data_source.last(),
    Some(&format!("send {UTF8}"))
  );
  crate::input::pointer_button(drag.state(), LEFT, false);
  let plan = drag.turn();
  assert!(plan.drop);
  assert_eq!(plan.motion, None);
  assert_eq!(
    drag.wayland.events.data_source.last().map(String::as_str),
    Some("dnd_drop_performed")
  );
  // The drop is reported once.
  assert!(!drag.turn().drop);
  apply(
    drag.state(),
    XwmEvent::DragFinished {
      performed: true,
      action: DndAction::Copy,
    },
  );
  drag.roundtrip();
  assert_eq!(
    drag.wayland.events.data_source.last().map(String::as_str),
    Some("dnd_finished")
  );
  source.destroy();
  drag.roundtrip();
  assert!(drag.turn().end);
}

#[test]
fn a_refused_wayland_drop_on_an_x_window_is_cancelled() {
  let mut drag = Drag::new();
  let _source = drag.start_wayland();
  drag.turn();
  drag.move_to(true);
  drag.turn();
  apply(
    drag.state(),
    XwmEvent::DragStatus {
      accepted: true,
      action: DndAction::Copy,
    },
  );
  // Taken back, as leaving the window reports.
  apply(
    drag.state(),
    XwmEvent::DragStatus {
      accepted: false,
      action: DndAction::None,
    },
  );
  crate::input::pointer_button(drag.state(), LEFT, false);
  assert!(drag.turn().drop);
  assert_eq!(
    drag.wayland.events.data_source,
    [
      format!("target {UTF8}"),
      "action 1".into(),
      "target -".into(),
      "action 0".into(),
      "cancelled".into(),
    ]
  );
}

#[test]
fn an_x_drag_keeps_the_pointer_and_is_offered_to_wayland() {
  let mut drag = Drag::new();
  let xterm = drag.object(true);
  drag.start_x11();
  assert_eq!(
    crate::data_device::pointer_grab(drag.state()),
    Some(xterm.clone())
  );
  // Over the Wayland window the pointer stays on xterm, so that Xwayland
  // sees the motion.
  drag.move_to(false);
  assert_eq!(drag.state().input.pointer_focus.as_ref(), Some(&xterm));
  // No types yet: nothing is offered, and the proxy is mapped.
  let plan = drag.turn();
  assert_eq!(plan.x11, Some((true, 0)));
  assert!(drag.wayland.events.drag.is_empty());
  apply(
    drag.state(),
    XwmEvent::DragOffered {
      mime_types: vec![UTF8.into()],
    },
  );
  apply(
    drag.state(),
    XwmEvent::DragSourceAction {
      action: DndAction::Move,
    },
  );
  drag.turn();
  assert_eq!(drag.wayland.events.drag, ["enter"]);
  let offer = drag.wayland.events.drag_offers[0].clone().unwrap();
  assert_eq!(drag.wayland.events.offer_types[&offer.id()], [UTF8]);
  offer.accept(1, Some(UTF8.into()));
  offer.set_actions(Action::Copy | Action::Move, Action::Move);
  drag.roundtrip();
  assert_eq!(drag.turn().x11, Some((true, Action::Move.bits())));
  // Back over xterm the proxy goes; X does the rest there.
  drag.move_to(true);
  assert_eq!(drag.turn().x11, Some((false, 0)));
  drag.move_to(false);
  drag.turn();
  let offer = drag.wayland.events.drag_offers.last().cloned().flatten();
  let offer = offer.unwrap();
  offer.accept(2, Some(UTF8.into()));
  offer.set_actions(Action::Copy, Action::Copy);
  drag.roundtrip();
  // The release lets the pointer go; the drop comes from X.
  crate::input::pointer_button(drag.state(), LEFT, false);
  let toplevel = drag.object(false);
  assert_eq!(drag.state().input.pointer_focus.as_ref(), Some(&toplevel));
  assert!(crate::data_device::pointer_grab(drag.state()).is_none());
  let plan = drag.turn();
  assert_eq!(plan.x11, Some((true, 1)));
  assert!(!drag.wayland.events.drag.contains(&"drop"));
  apply(drag.state(), XwmEvent::DragDropped);
  drag.roundtrip();
  assert_eq!(drag.wayland.events.drag.last(), Some(&"drop"));
  // The Wayland client's request goes to the window manager as
  // XdndSelection.
  let (_read, write) = rustix::pipe::pipe().unwrap();
  offer.receive(UTF8.into(), write.as_fd());
  drag.roundtrip();
  let receives = &drag.state().xwayland.selection_receives;
  assert_eq!(receives.len(), 1);
  assert_eq!(receives[0].0, shingle_xwayland::Selection::Dnd);
  assert_eq!(receives[0].1, UTF8);
  offer.finish();
  drag.roundtrip();
  let plan = drag.turn();
  assert_eq!(plan.x11_end, Some(Some(1)));
  assert_eq!(plan.x11, None);
  assert!(crate::data_device::drag_view(drag.state()).is_none());
}

#[test]
fn an_x_drag_dropped_on_an_x_window_ends_with_the_release() {
  let mut drag = Drag::new();
  drag.start_x11();
  crate::input::pointer_button(drag.state(), LEFT, false);
  let plan = drag.turn();
  // Not over the proxy, so nothing to finish: the window manager forgets
  // the drag.
  assert_eq!(plan.x11_end, Some(None));
  assert!(crate::data_device::drag_view(drag.state()).is_none());
  assert!(drag.wayland.events.drag.is_empty());
}

#[test]
fn a_refused_x_drop_and_a_late_one_end_unperformed() {
  let mut drag = Drag::new();
  drag.start_x11();
  apply(
    drag.state(),
    XwmEvent::DragOffered {
      mime_types: vec![UTF8.into()],
    },
  );
  drag.move_to(false);
  drag.turn();
  crate::input::pointer_button(drag.state(), LEFT, false);
  // Dropped without the Wayland client accepting.
  apply(drag.state(), XwmEvent::DragDropped);
  let plan = drag.turn();
  assert_eq!(plan.x11_end, Some(None));
  assert_eq!(drag.wayland.events.drag, ["enter", "leave"]);
  // A drop that never comes is given up.
  drag.start_x11();
  drag.move_to(false);
  crate::input::pointer_button(drag.state(), LEFT, false);
  assert_eq!(drag.turn().x11_end, None);
  crate::data_device::backdate_release(
    drag.state(),
    crate::data_device::X11_DROP_TIMEOUT + Duration::from_millis(1),
  );
  let plan = drag.turn();
  assert!(plan.x11_timed_out);
  assert_eq!(plan.x11_end, Some(None));
  // And one the X client refuses with XdndLeave after the release.
  drag.start_x11();
  drag.move_to(false);
  crate::input::pointer_button(drag.state(), LEFT, false);
  apply(drag.state(), XwmEvent::DragLeft);
  assert_eq!(drag.turn().x11_end, Some(None));
}

#[test]
fn an_owner_without_a_held_button_starts_no_drag() {
  let mut drag = Drag::new();
  drag.move_to(true);
  apply(
    drag.state(),
    XwmEvent::DragOwnerChanged { owner: Some(XTERM) },
  );
  assert!(crate::data_device::drag_view(drag.state()).is_none());
  // Nor does a button held on a Wayland window.
  drag.move_to(false);
  crate::input::pointer_button(drag.state(), LEFT, true);
  apply(
    drag.state(),
    XwmEvent::DragOwnerChanged { owner: Some(XTERM) },
  );
  assert!(crate::data_device::drag_view(drag.state()).is_none());
}

#[test]
fn a_wayland_drag_wins_until_it_is_dropped() {
  let mut drag = Drag::new();
  let _source = drag.start_wayland();
  drag.turn();
  // An X client taking XdndSelection meanwhile starts nothing.
  drag.move_to(true);
  apply(
    drag.state(),
    XwmEvent::DragOwnerChanged { owner: Some(XTERM) },
  );
  assert!(!crate::data_device::drag_view(drag.state()).unwrap().x11);
  crate::input::pointer_button(drag.state(), LEFT, false);
  drag.turn();
  // Dropped, with its source still alive: the next X drag replaces it.
  drag.start_x11();
  assert!(crate::data_device::drag_view(drag.state()).unwrap().x11);
  assert!(drag.turn().end);
}

#[test]
fn the_owner_going_ends_the_x_drag() {
  let mut drag = Drag::new();
  drag.start_x11();
  assert!(crate::data_device::drag_view(drag.state()).is_some());
  apply(drag.state(), XwmEvent::DragOwnerChanged { owner: None });
  assert!(crate::data_device::drag_view(drag.state()).is_none());
  assert!(crate::data_device::pointer_grab(drag.state()).is_none());
  // The window manager forgot it by itself.
  assert_eq!(drag.turn().x11_end, None);
}
