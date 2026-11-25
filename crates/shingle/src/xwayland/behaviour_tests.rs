//! XW-4 behaviour of X windows in the compositor: focus and input models,
//! client requests, transients and dialogs, stacking, rules and Scale,
//! with window manager events applied directly (docs/design/xwayland.md,
//! section 7, layer 1).

use serde_json::json;
use shingle_core::WindowId;
use shingle_xwayland::{
  Geometry, MoveResize, NetWmState, StateChange, Window, WindowInfo,
  WindowType, XwmEvent,
};
use wayland_server::backend::ObjectId;

use super::{fixture::*, focus, windows::XRequest};
use crate::{Event, test_client::TestClient};

const DIALOG: Window = 0x400200;

/// The surface object of an X window.
fn object_of(fixture: &mut Fixture, window: Window) -> ObjectId {
  let state = fixture.state();
  let surface = state.xwayland.windows[&window].surface.unwrap();
  state.surface_ids[&surface].clone()
}

/// The Shingle window of an X window.
fn window_of(fixture: &mut Fixture, window: Window) -> WindowId {
  super::windows::shingle_window(fixture.state(), window).unwrap()
}

#[test]
fn x_focus_follows_the_keyboard_between_x_and_wayland_windows() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  let xterm = window_of(&mut fixture, XTERM);
  assert_eq!(focus::target(fixture.state()), Some(XTERM));
  let mut wayland = TestClient::connect(&mut fixture.compositor, &fixture.path);
  let toplevel = wayland.toplevel(&mut fixture.compositor, "foot");
  wayland.attach_buffer(&mut fixture.compositor, &toplevel.surface, 64, 64);
  assert_eq!(focus::target(fixture.state()), None);
  crate::input::focus_window(fixture.state(), xterm);
  assert_eq!(focus::target(fixture.state()), Some(XTERM));
}

/// A plain window with the given input hint and `WM_TAKE_FOCUS`.
fn with_model(
  class: &str,
  input: bool,
  take_focus: bool,
  types: &[WindowType],
) -> Box<WindowInfo> {
  let mut info = info(class, class);
  info.hints.input = input;
  info.protocols.take_focus = take_focus;
  info.types = types.to_vec();
  info
}

#[test]
fn input_models_decide_who_is_focused_on_map() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  let xterm = object_of(&mut fixture, XTERM);
  fixture.requests();
  // No input: not focused.
  map_info(
    &mut fixture,
    0x500001,
    with_model("None", false, false, &[]),
    6,
  );
  assert_eq!(fixture.state().input.keyboard_focus.as_ref(), Some(&xterm));
  // Globally active NORMAL: offered the focus, not given it.
  map_info(
    &mut fixture,
    0x600001,
    with_model("Global", false, true, &[WindowType::Normal]),
    7,
  );
  assert_eq!(fixture.state().input.keyboard_focus.as_ref(), Some(&xterm));
  assert!(fixture.requests().contains(&XRequest::OfferFocus(0x600001)));
  // Globally active without a type: not even offered it.
  map_info(
    &mut fixture,
    0x700001,
    with_model("Bare", false, true, &[]),
    8,
  );
  assert!(!fixture.requests().contains(&XRequest::OfferFocus(0x700001)));
  // Locally active: focused.
  map_info(
    &mut fixture,
    0x800001,
    with_model("Local", true, true, &[]),
    9,
  );
  let local = object_of(&mut fixture, 0x800001);
  assert_eq!(fixture.state().input.keyboard_focus.as_ref(), Some(&local));
  // A click still focuses a no-input window, as Wayfire's X toplevels are
  // always focusable; the window manager then sets no X focus on it.
  let none = window_of(&mut fixture, 0x500001);
  assert_eq!(
    crate::input::focus_window(fixture.state(), none),
    Some(none)
  );
}

/// The leaf state of an X window's Shingle window.
fn leaf(fixture: &mut Fixture, window: Window) -> shingle_core::LeafState {
  let window = window_of(fixture, window);
  fixture.state().core.window_info(window).unwrap().state
}

#[test]
fn client_state_requests_reach_shingle() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  let request = |change| XwmEvent::StateRequest {
    window: XTERM,
    change,
  };
  fixture.apply(request(StateChange {
    fullscreen: Some(true),
    ..StateChange::default()
  }));
  assert!(leaf(&mut fixture, XTERM).fullscreen);
  let output = fixture.state().output;
  assert!(fixture.requests().contains(&XRequest::Configure {
    window: XTERM,
    geometry: super::windows::geometry(output),
    force: false,
  }));
  fixture.apply(request(StateChange {
    fullscreen: Some(false),
    maximized: Some(true),
    ..StateChange::default()
  }));
  let state = leaf(&mut fixture, XTERM);
  assert!(!state.fullscreen && state.maximized);
  fixture.apply(request(StateChange {
    minimized: Some(true),
    ..StateChange::default()
  }));
  assert!(leaf(&mut fixture, XTERM).minimized);
  assert!(fixture.requests().iter().any(|request| matches!(request,
    XRequest::State { window: XTERM, state } if state.hidden)));
  fixture.apply(request(StateChange {
    minimized: Some(false),
    ..StateChange::default()
  }));
  assert!(!leaf(&mut fixture, XTERM).minimized);
}

#[test]
fn the_initial_state_applies_when_the_window_maps() {
  let mut fixture = Fixture::new();
  let mut fullscreen = info("XTerm", "x");
  fullscreen.state = NetWmState {
    fullscreen: true,
    ..NetWmState::default()
  };
  map_info(&mut fixture, XTERM, fullscreen, 5);
  assert!(leaf(&mut fixture, XTERM).fullscreen);
  // A request before the map counts as initial state too.
  let (surface, _role) = fixture.surface(6);
  fixture.apply(mapped(XEYES, "XEyes"));
  fixture.apply(XwmEvent::StateRequest {
    window: XEYES,
    change: StateChange {
      maximized: Some(true),
      ..StateChange::default()
    },
  });
  fixture.commit_buffer(&surface);
  fixture.apply(XwmEvent::Serial {
    window: XEYES,
    serial: 6,
  });
  assert!(leaf(&mut fixture, XEYES).maximized);
}

#[test]
fn activate_move_resize_and_close_requests() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  map(&mut fixture, XEYES, "XEyes", 6);
  let xterm = window_of(&mut fixture, XTERM);
  assert_ne!(crate::input::focused_window(fixture.state()), Some(xterm));
  fixture.apply(XwmEvent::ActivateRequest { window: XTERM });
  assert_eq!(crate::input::focused_window(fixture.state()), Some(xterm));
  fixture.compositor.drain_events();
  fixture.apply(XwmEvent::MoveResizeRequest {
    window: XTERM,
    request: MoveResize::Move,
  });
  fixture.apply(XwmEvent::MoveResizeRequest {
    window: XTERM,
    request: MoveResize::Resize(10),
  });
  let events = fixture.compositor.drain_events();
  assert!(events.iter().any(|event| matches!(event,
    Event::BeginMove { window } if *window == xterm)));
  assert!(events.iter().any(|event| matches!(event,
    Event::BeginResize { window, edges: 10 } if *window == xterm)));
  fixture.requests();
  fixture.apply(XwmEvent::CloseRequest { window: XTERM });
  assert_eq!(fixture.requests(), [XRequest::Close(XTERM)]);
}

#[test]
fn transient_dialogs_float_centred_on_their_parent() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  let xterm = window_of(&mut fixture, XTERM);
  let mut dialog = info("XTerm", "dialog");
  dialog.transient_for = Some(XTERM);
  dialog.geometry = Geometry::new(0, 0, 200, 100);
  map_info(&mut fixture, DIALOG, dialog, 6);
  let window = window_of(&mut fixture, DIALOG);
  let state = fixture.state();
  let info = state.core.window_info(window).unwrap();
  assert_eq!(info.parent, Some(xterm));
  let output = state.output;
  let x = output.x + (output.width as i32 - 200) / 2;
  let y = output.y + (output.height as i32 - 100) / 2;
  assert_eq!(
    info.state.floating,
    Some(shingle_core::Rect::new(x, y, 200, 100))
  );
  // It is focused, and the parent still fills the output.
  assert_eq!(crate::input::focused_window(state), Some(window));
  assert_eq!(state.core.window_layout(xterm).unwrap().geometry, output);
}

#[test]
fn fixed_size_windows_float() {
  let mut fixture = Fixture::new();
  let mut fixed = info("XCalc", "calculator");
  fixed.size_hints.min = Some((300, 200));
  fixed.size_hints.max = Some((300, 200));
  fixed.geometry = Geometry::new(0, 0, 300, 200);
  map_info(&mut fixture, XTERM, fixed, 5);
  assert!(leaf(&mut fixture, XTERM).floating.is_some());
  // Only a minimum: it tiles.
  let mut resizable = info("XEyes", "eyes");
  resizable.size_hints.min = Some((300, 200));
  map_info(&mut fixture, XEYES, resizable, 6);
  assert!(leaf(&mut fixture, XEYES).floating.is_none());
}

#[test]
fn normal_windows_with_a_transient_for_have_no_parent_and_tile() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  let mut normal = info("XTerm", "second");
  normal.transient_for = Some(XTERM);
  normal.types = vec![WindowType::Normal];
  map_info(&mut fixture, XEYES, normal, 6);
  let window = window_of(&mut fixture, XEYES);
  let info = fixture.state().core.window_info(window).unwrap();
  assert_eq!(info.parent, None);
  assert!(info.state.floating.is_none());
}

#[test]
fn stacking_follows_focus() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  map(&mut fixture, XEYES, "XEyes", 6);
  assert_eq!(focus::stacking(fixture.state()), [XTERM, XEYES]);
  let xterm = window_of(&mut fixture, XTERM);
  crate::input::focus_window(fixture.state(), xterm);
  assert_eq!(focus::stacking(fixture.state()), [XEYES, XTERM]);
}

#[test]
fn an_app_id_rule_applies_to_x_windows() {
  let mut fixture = Fixture::new();
  let rule = shingle_config::WindowRule::try_from(
    r#"on created if app_id is "XTerm" then set alpha 0.5"#.to_string(),
  )
  .unwrap();
  fixture.state().window_rules = vec![rule];
  map(&mut fixture, XTERM, "XTerm", 5);
  map(&mut fixture, XEYES, "XEyes", 6);
  let alpha = |fixture: &mut Fixture, window: Window| {
    let id = window_of(fixture, window).0;
    fixture.call("wf/alpha/get-view-alpha", json!({"view-id": id}))["alpha"]
      .as_f64()
      .unwrap()
  };
  assert_eq!(alpha(&mut fixture, XTERM), 0.5);
  assert_eq!(alpha(&mut fixture, XEYES), 1.0);
}

#[test]
fn scale_and_expo_surfaces_include_x_windows() {
  let mut fixture = Fixture::new();
  map(&mut fixture, XTERM, "XTerm", 5);
  let xterm = window_of(&mut fixture, XTERM);
  for all_workspaces in [false, true] {
    let surfaces = fixture.compositor.scale_surfaces(all_workspaces, false);
    assert_eq!(surfaces.len(), 1);
    assert_eq!(surfaces[0].window, Some(xterm));
  }
}
