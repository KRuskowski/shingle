//! Client protocols with several outputs: surfaces entering and leaving
//! outputs, preferred scales, layer and lock surfaces per output, and
//! presentation, FIFO and commit timing by primary output
//! (docs/design/multi-output.md, MO-3).

use std::collections::{BTreeMap, HashMap};

use shingle_core::{OutputId, WindowId, WorkspaceId};
use wayland_client::{
  Proxy,
  protocol::{
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_surface,
  },
};
use wayland_protocols::{
  ext::session_lock::v1::client::ext_session_lock_manager_v1 as lock_manager,
  ext::workspace::v1::client::ext_workspace_manager_v1 as workspaces,
  wp::{
    fifo::v1::client::wp_fifo_manager_v1::WpFifoManagerV1,
    fractional_scale::v1::client::wp_fractional_scale_manager_v1 as scales,
    presentation_time::client::{
      wp_presentation::WpPresentation, wp_presentation_feedback,
    },
    viewporter::client::wp_viewporter::WpViewporter,
  },
};
use wayland_protocols_wlr::{
  foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1 as toplevels,
  gamma_control::v1::client::zwlr_gamma_control_manager_v1 as gamma,
  layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{Anchor, ZwlrLayerSurfaceV1},
  },
  output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1,
};

use super::{OutputHandle, OutputInfo};
use crate::{
  Compositor, Config,
  test_client::{TestClient, Toplevel},
  tests::test_compositor_with,
};

fn id(name: &str) -> OutputId {
  OutputId::new(name)
}

/// A compositor with HEADLESS-1 (1280x720) and HEADLESS-2 (800x600) to
/// its right.
fn two_outputs() -> (Compositor, std::path::PathBuf) {
  let (mut compositor, path) = test_compositor_with(Config::default());
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .expect("a second output");
  (compositor, path)
}

/// Binds every `wl_output` global the client sees, by output name.
fn bind_outputs(
  compositor: &mut Compositor,
  client: &mut TestClient,
) -> HashMap<String, WlOutput> {
  let globals = client
    .events
    .globals
    .iter()
    .filter(|(_, interface, _)| interface == "wl_output")
    .map(|(name, ..)| *name)
    .collect::<Vec<_>>();
  let mut outputs = HashMap::new();
  for global in globals {
    client.events.output.clear();
    let output = client.bind_name::<WlOutput>(global, 4);
    client.roundtrip(compositor);
    let name = client.events.output.iter().find_map(|event| match event {
      wl_output::Event::Name { name } => Some(name.clone()),
      _ => None,
    });
    outputs.insert(name.expect("a version 4 output is named"), output);
  }
  outputs
}

/// The outputs a surface entered and left, in order, by name.
fn enters_and_leaves(
  client: &TestClient,
  surface: &wl_surface::WlSurface,
  outputs: &HashMap<String, WlOutput>,
) -> Vec<String> {
  let name = |output: &WlOutput| {
    outputs
      .iter()
      .find(|(_, candidate)| candidate.id() == output.id())
      .map_or_else(|| "?".to_owned(), |(name, _)| name.clone())
  };
  client
    .surface_events(surface)
    .into_iter()
    .filter_map(|event| match event {
      wl_surface::Event::Enter { output } => {
        Some(format!("enter {}", name(output)))
      }
      wl_surface::Event::Leave { output } => {
        Some(format!("leave {}", name(output)))
      }
      _ => None,
    })
    .collect()
}

/// A mapped 64x64 toplevel and its window.
fn mapped(
  compositor: &mut Compositor,
  client: &mut TestClient,
  app_id: &str,
) -> (Toplevel, WindowId) {
  let toplevel = client.toplevel(compositor, app_id);
  client.attach_buffer(compositor, &toplevel.surface, 64, 64);
  client.roundtrip(compositor);
  let window = compositor.focused_window().expect("the toplevel maps");
  (toplevel, window)
}

/// Moves `window` to the current workspace of `output`'s set.
fn move_to(compositor: &mut Compositor, window: WindowId, output: &str) {
  let target = compositor.core().wset_of(&id(output)).unwrap();
  compositor
    .state
    .core
    .move_window_to_wset(window, target, WorkspaceId::ORIGIN)
    .unwrap();
  crate::shell::reconfigure_all(&mut compositor.state);
}

#[test]
fn a_window_enters_the_output_it_is_shown_on_and_leaves_it() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "mover");
  assert_eq!(
    enters_and_leaves(&client, &toplevel.surface, &outputs),
    ["enter HEADLESS-1"],
    "only its own output, once, however often it commits"
  );
  client.attach_buffer(&mut compositor, &toplevel.surface, 64, 64);
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  assert_eq!(
    enters_and_leaves(&client, &toplevel.surface, &outputs),
    ["enter HEADLESS-1", "leave HEADLESS-1", "enter HEADLESS-2"]
  );
  // Another workspace of HEADLESS-2 hides it there.
  let second = WorkspaceId::new(1, 0);
  let core = &mut compositor.state.core;
  core.ensure_workspace(&id("HEADLESS-2"), second).unwrap();
  core
    .set_current_workspace(&id("HEADLESS-2"), second)
    .unwrap();
  client.roundtrip(&mut compositor);
  assert_eq!(
    enters_and_leaves(&client, &toplevel.surface, &outputs).last(),
    Some(&"leave HEADLESS-2".to_owned())
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_surface_leaves_an_output_that_goes() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "unplugged");
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  // Under `return` the window joins the active output.
  assert_eq!(
    enters_and_leaves(&client, &toplevel.surface, &outputs),
    [
      "enter HEADLESS-1",
      "leave HEADLESS-1",
      "enter HEADLESS-2",
      "leave HEADLESS-2",
      "enter HEADLESS-1",
    ]
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn preferred_scales_are_those_of_the_windows_output() {
  let (mut compositor, path) = two_outputs();
  compositor
    .state
    .set_scale_of(&id("HEADLESS-2"), 2.0)
    .unwrap();
  let mut client = TestClient::connect(&mut compositor, &path);
  let wl_compositor = client.bind::<WlCompositor>(6);
  let manager = client.bind::<scales::WpFractionalScaleManagerV1>(1);
  let wm_base =
    client
      .bind::<wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase>(1);
  let surface = wl_compositor.create_surface(&client.handle, ());
  let scale = manager.get_fractional_scale(&surface, &client.handle, ());
  let xdg_surface = wm_base.get_xdg_surface(&surface, &client.handle, ());
  let _toplevel = xdg_surface.get_toplevel(&client.handle, ());
  surface.commit();
  client.roundtrip(&mut compositor);
  client.attach_buffer(&mut compositor, &surface, 64, 64);
  let window = compositor.focused_window().unwrap();
  let scales = |client: &TestClient| {
    client
      .events
      .preferred_scales
      .iter()
      .filter(|(object, _)| *object == scale.id())
      .map(|(_, value)| *value)
      .collect::<Vec<_>>()
  };
  let buffer_scales = |client: &TestClient| {
    client
      .surface_events(&surface)
      .into_iter()
      .filter_map(|event| match event {
        wl_surface::Event::PreferredBufferScale { factor } => Some(*factor),
        _ => None,
      })
      .collect::<Vec<_>>()
  };
  assert_eq!(scales(&client), [120], "HEADLESS-1 has scale 1");
  assert!(buffer_scales(&client).is_empty(), "1 is the default");
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  assert_eq!(scales(&client), [120, 240]);
  assert_eq!(buffer_scales(&client), [2]);
  let _ = std::fs::remove_file(path);
}

/// A layer surface on `output` (or the active output) anchored to the top
/// edge, 30 pixels high with an exclusive zone of 30.
fn panel(
  compositor: &mut Compositor,
  client: &mut TestClient,
  output: Option<&WlOutput>,
) -> (wl_surface::WlSurface, ZwlrLayerSurfaceV1) {
  let wl_compositor = client.bind::<WlCompositor>(4);
  let shell = client.bind::<ZwlrLayerShellV1>(4);
  let surface = wl_compositor.create_surface(&client.handle, ());
  let layer = shell.get_layer_surface(
    &surface,
    output,
    Layer::Top,
    "panel".into(),
    &client.handle,
    (),
  );
  layer.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
  layer.set_size(0, 30);
  layer.set_exclusive_zone(30);
  surface.commit();
  client.roundtrip(compositor);
  (surface, layer)
}

#[test]
fn a_layer_surface_covers_its_logical_size_at_any_scale() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let geometry = |compositor: &Compositor| {
    compositor
      .visible_surfaces_on(&OutputHandle::new("HEADLESS-2"))
      .into_iter()
      .find(|snapshot| snapshot.role == crate::SurfaceRole::LayerShell)
      .unwrap()
      .geometry
  };
  let (surface, layer) =
    panel(&mut compositor, &mut client, Some(&outputs["HEADLESS-2"]));
  let &(_, serial, _, _) = client.events.layer_configures.last().unwrap();
  layer.ack_configure(serial);
  // A buffer at scale 2 covers half its pixels.
  surface.set_buffer_scale(2);
  client.attach_buffer(&mut compositor, &surface, 1600, 60);
  assert_eq!(
    geometry(&compositor),
    shingle_core::Rect::new(1280, 0, 800, 30)
  );
  // A buffer of 1.5 times the size covers its viewport's destination.
  let viewporter = client.bind::<WpViewporter>(1);
  let viewport = viewporter.get_viewport(&surface, &client.handle, ());
  surface.set_buffer_scale(1);
  viewport.set_destination(800, 30);
  client.attach_buffer(&mut compositor, &surface, 1200, 45);
  assert_eq!(
    geometry(&compositor),
    shingle_core::Rect::new(1280, 0, 800, 30)
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_layer_surface_is_on_its_output_and_closed_when_it_goes() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let (surface, layer) =
    panel(&mut compositor, &mut client, Some(&outputs["HEADLESS-2"]));
  let &(_, serial, width, height) = client
    .events
    .layer_configures
    .last()
    .expect("the panel is configured");
  assert_eq!((width, height), (800, 30), "HEADLESS-2's width");
  layer.ack_configure(serial);
  client.attach_buffer(&mut compositor, &surface, 800, 30);
  client.roundtrip(&mut compositor);
  let workarea = |compositor: &Compositor, output: &str| {
    compositor
      .core()
      .tree(&id(output), WorkspaceId::ORIGIN)
      .unwrap()
      .workarea()
  };
  assert_eq!(
    workarea(&compositor, "HEADLESS-2"),
    shingle_core::Rect::new(1280, 30, 800, 570)
  );
  assert_eq!(
    workarea(&compositor, "HEADLESS-1"),
    shingle_core::Rect::new(0, 0, 1280, 720)
  );
  let drawn = |compositor: &Compositor, output: &str| {
    compositor
      .visible_surfaces_on(&OutputHandle::new(output))
      .iter()
      .any(|snapshot| snapshot.role == crate::SurfaceRole::LayerShell)
  };
  assert!(
    drawn(&compositor, "HEADLESS-2") && !drawn(&compositor, "HEADLESS-1")
  );
  let geometry = compositor
    .visible_surfaces_on(&OutputHandle::new("HEADLESS-2"))
    .into_iter()
    .find(|snapshot| snapshot.role == crate::SurfaceRole::LayerShell)
    .unwrap()
    .geometry;
  assert_eq!(geometry, shingle_core::Rect::new(1280, 0, 800, 30));
  assert_eq!(
    enters_and_leaves(&client, &surface, &outputs),
    ["enter HEADLESS-2"]
  );
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  assert_eq!(client.events.layer_closed, [layer.id()]);
  assert!(!drawn(&compositor, "HEADLESS-1"));
  // Without an output, the next panel goes on the active output.
  let (_surface, _layer) = panel(&mut compositor, &mut client, None);
  let (_, _, width, _) = *client.events.layer_configures.last().unwrap();
  assert_eq!(width, 1280);
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_fullscreen_window_hides_only_its_own_outputs_panels() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let mut panels = Vec::new();
  for name in ["HEADLESS-1", "HEADLESS-2"] {
    let (surface, layer) =
      panel(&mut compositor, &mut client, Some(&outputs[name]));
    let &(_, serial, width, _) = client.events.layer_configures.last().unwrap();
    layer.ack_configure(serial);
    client.attach_buffer(&mut compositor, &surface, width as i32, 30);
    panels.push((surface, layer));
  }
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "full");
  move_to(&mut compositor, window, "HEADLESS-2");
  compositor.focus_window(window);
  compositor.toggle_fullscreen(window).unwrap();
  client.roundtrip(&mut compositor);
  let drawn = |compositor: &Compositor, output: &str| {
    let surfaces = compositor.visible_surfaces_on(&OutputHandle::new(output));
    let panel = surfaces
      .iter()
      .any(|snapshot| snapshot.role == crate::SurfaceRole::LayerShell);
    let full = surfaces
      .iter()
      .any(|snapshot| snapshot.window == Some(window));
    (panel, full)
  };
  // Before, the focused fullscreen window hid the top layer of every
  // output and was drawn on each.
  assert_eq!(drawn(&compositor, "HEADLESS-2"), (false, true));
  assert_eq!(drawn(&compositor, "HEADLESS-1"), (true, false));
  // It stays above HEADLESS-2's panel while a window on HEADLESS-1 has
  // the keyboard.
  crate::output::focus_output(&mut compositor.state, &id("HEADLESS-1"));
  let (_other, other) = mapped(&mut compositor, &mut client, "other");
  assert_eq!(compositor.focused_window(), Some(other));
  assert_eq!(drawn(&compositor, "HEADLESS-2"), (false, true));
  assert_eq!(drawn(&compositor, "HEADLESS-1"), (true, false));
  let _ = std::fs::remove_file(path);
}

#[test]
fn every_output_gets_a_lock_surface_before_the_session_is_locked() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let wl_compositor = client.bind::<WlCompositor>(4);
  let manager = client.bind::<lock_manager::ExtSessionLockManagerV1>(1);
  let lock = manager.lock(&client.handle, ());
  let mut surfaces = Vec::new();
  for name in ["HEADLESS-1", "HEADLESS-2"] {
    let surface = wl_compositor.create_surface(&client.handle, ());
    let lock_surface =
      lock.get_lock_surface(&surface, &outputs[name], &client.handle, ());
    surfaces.push((surface, lock_surface));
  }
  client.roundtrip(&mut compositor);
  let sizes = client
    .events
    .lock_configures
    .iter()
    .map(|(_, _, width, height)| (*width, *height))
    .collect::<Vec<_>>();
  assert_eq!(sizes, [(1280, 720), (800, 600)]);
  let configures = client.events.lock_configures.clone();
  for ((surface, lock_surface), (_, serial, width, height)) in
    surfaces.iter().zip(&configures)
  {
    lock_surface.ack_configure(*serial);
    let buffer = client.opaque_buffer(*width as i32, *height as i32);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    client.roundtrip(&mut compositor);
    if lock_surface.id() == surfaces[0].1.id() {
      assert!(client.events.lock.is_empty(), "HEADLESS-2 is not covered");
    }
  }
  assert_eq!(client.events.lock, ["locked"]);
  for name in ["HEADLESS-1", "HEADLESS-2"] {
    let drawn = compositor.visible_surfaces_on(&OutputHandle::new(name));
    assert_eq!(drawn.len(), 1, "{name} shows its own lock surface");
    assert_eq!(drawn[0].role, crate::SurfaceRole::SessionLock);
  }
  // An output that goes takes its lock surface; the session stays locked.
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  assert!(compositor.is_locked());
  // A second lock surface on one output is a protocol error.
  let surface = wl_compositor.create_surface(&client.handle, ());
  let _second =
    lock.get_lock_surface(&surface, &outputs["HEADLESS-1"], &client.handle, ());
  // Under load the error may take more than one read to arrive, and its
  // message may be lost; code 3 is duplicate_output.
  let error = (0..100).find_map(|_| client.protocol_error(&mut compositor));
  assert!(
    error
      .as_ref()
      .is_some_and(|error| error.object_interface == "ext_session_lock_v1"
        && error.code == 3),
    "duplicate_output, not {error:?}"
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn presentation_feedback_follows_the_primary_output() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "paced");
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  let presentation = client.bind::<WpPresentation>(2);
  presentation.feedback(&toplevel.surface, &client.handle, ());
  toplevel.surface.commit();
  client.roundtrip(&mut compositor);
  client.events.feedback.clear();
  compositor.frame_now(&OutputHandle::new("HEADLESS-1"));
  client.roundtrip(&mut compositor);
  assert!(client.events.feedback.is_empty(), "not HEADLESS-1's frame");
  compositor.frame_now(&OutputHandle::new("HEADLESS-2"));
  client.roundtrip(&mut compositor);
  let synced = client.events.feedback.iter().find_map(|event| match event {
    wp_presentation_feedback::Event::SyncOutput { output } => Some(output.id()),
    _ => None,
  });
  assert_eq!(synced, Some(outputs["HEADLESS-2"].id()));
  assert!(client.events.feedback.iter().any(|event| matches!(
    event,
    wp_presentation_feedback::Event::Presented { .. }
  )));
  let object = compositor
    .state
    .surfaces
    .keys()
    .find(|object| {
      compositor.state.surfaces[*object]
        .role
        .toplevel()
        .and_then(|toplevel| toplevel.window())
        == Some(window)
    })
    .cloned()
    .unwrap();
  assert_eq!(
    crate::surface_outputs::primary_of(&compositor.state, &object),
    id("HEADLESS-2")
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn fifo_barriers_wait_for_the_frames_of_the_primary_output() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "fifo");
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  let manager = client.bind::<WpFifoManagerV1>(1);
  let fifo = manager.get_fifo(&toplevel.surface, &client.handle, ());
  let buffers = [0; 2].map(|_| client.shm_buffer(64, 64));
  client.roundtrip(&mut compositor);
  let surface = &toplevel.surface;
  surface.attach(Some(&buffers[0]), 0, 0);
  fifo.set_barrier();
  surface.commit();
  surface.attach(Some(&buffers[1]), 0, 0);
  fifo.wait_barrier();
  surface.commit();
  client.roundtrip(&mut compositor);
  assert!(compositor.needs_frame(), "the second update waits");
  compositor.frame_now(&OutputHandle::new("HEADLESS-1"));
  client.roundtrip(&mut compositor);
  assert!(compositor.needs_frame(), "HEADLESS-1 does not show it");
  compositor.frame_now(&OutputHandle::new("HEADLESS-2"));
  client.roundtrip(&mut compositor);
  assert!(!compositor.needs_frame());
  let _ = std::fs::remove_file(path);
}

/// Captures `region` of `output`, all of it for `None`, into an shm
/// buffer of the size the compositor gives. Returns that size and the
/// pixels as BGRA bytes, or `None` if the capture failed.
fn capture(
  compositor: &mut Compositor,
  client: &mut TestClient,
  output: &WlOutput,
  region: Option<(i32, i32, i32, i32)>,
) -> Option<((i32, i32), Vec<u8>)> {
  use std::os::fd::AsFd;
  use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::Event,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
  };
  let manager = client.bind::<ZwlrScreencopyManagerV1>(3);
  client.events.screencopy.clear();
  let frame = match region {
    Some((x, y, width, height)) => manager.capture_output_region(
      0,
      output,
      x,
      y,
      width,
      height,
      &client.handle,
      (),
    ),
    None => manager.capture_output(0, output, &client.handle, ()),
  };
  client.roundtrip(compositor);
  let (width, height) =
    client
      .events
      .screencopy
      .iter()
      .find_map(|event| match event {
        Event::Buffer { width, height, .. } => {
          Some((*width as i32, *height as i32))
        }
        _ => None,
      })?;
  let size = width * height * 4;
  let fd = rustix::fs::memfd_create(
    "shingle-capture-test",
    rustix::fs::MemfdFlags::CLOEXEC,
  )
  .unwrap();
  rustix::fs::ftruncate(&fd, size as u64).unwrap();
  let shm = client.bind::<wayland_client::protocol::wl_shm::WlShm>(1);
  let pool = shm.create_pool(fd.as_fd(), size, &client.handle, ());
  let buffer = pool.create_buffer(
    0,
    width,
    height,
    width * 4,
    wayland_client::protocol::wl_shm::Format::Argb8888,
    &client.handle,
    (),
  );
  frame.copy(&buffer);
  client.roundtrip(compositor);
  if !client
    .events
    .screencopy
    .iter()
    .any(|event| matches!(event, Event::Ready { .. }))
  {
    return None;
  }
  let mut bytes = vec![0; size as usize];
  rustix::io::pread(&fd, &mut bytes, 0).unwrap();
  Some(((width, height), bytes))
}

#[test]
fn captures_are_in_output_coordinates_and_physical_pixels() {
  let (mut compositor, path) = two_outputs();
  compositor
    .state
    .set_scale_of(&id("HEADLESS-2"), 2.0)
    .unwrap();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let toplevel = client.toplevel(&mut compositor, "black");
  let black = client.opaque_buffer(64, 64);
  toplevel.surface.attach(Some(&black), 0, 0);
  toplevel.surface.commit();
  client.roundtrip(&mut compositor);
  let window = compositor.focused_window().unwrap();
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  let second = &outputs["HEADLESS-2"];
  let (size, bytes) =
    capture(&mut compositor, &mut client, second, None).expect("a capture");
  assert_eq!(size, (1600, 1200), "800x600 logical at scale 2");
  assert_eq!(bytes[..4], [0, 0, 0, 255], "the black window fills it");
  let (size, bytes) =
    capture(&mut compositor, &mut client, second, Some((10, 20, 5, 4)))
      .expect("a region capture");
  assert_eq!(size, (10, 8));
  assert!(bytes.chunks(4).all(|pixel| pixel == [0, 0, 0, 255]));
  // HEADLESS-1 shows only the background, 0.1 or the byte 26.
  let first = &outputs["HEADLESS-1"];
  let (size, bytes) =
    capture(&mut compositor, &mut client, first, Some((1278, 0, 10, 1)))
      .expect("a region clipped to the output");
  assert_eq!(size, (2, 1));
  assert_eq!(bytes, [26, 26, 26, 255, 26, 26, 26, 255]);
  assert!(
    capture(&mut compositor, &mut client, first, Some((1280, 0, 10, 10)))
      .is_none(),
    "a region beside the output fails"
  );
  let _ = std::fs::remove_file(path);
}

/// A client bound to output management, with its manager.
fn manager_client(
  compositor: &mut Compositor,
  path: &std::path::Path,
) -> (TestClient, ZwlrOutputManagerV1) {
  let mut client = TestClient::connect(compositor, path);
  let manager = client.bind::<ZwlrOutputManagerV1>(4);
  client.roundtrip(compositor);
  (client, manager)
}

/// The live heads a client sees, by name, with whether each is enabled.
fn heads(client: &TestClient) -> BTreeMap<String, bool> {
  client
    .events
    .head_objects
    .iter()
    .filter(|(_, finished)| !finished)
    .map(|(head, _)| {
      (
        client.events.head_names[&head.id()].clone(),
        client.events.head_enabled[&head.id()],
      )
    })
    .collect()
}

/// Applies a configuration of every live head: those named in `off` are
/// disabled, the others enabled, at the position `positions` gives them.
/// Returns the outcome.
fn apply(
  compositor: &mut Compositor,
  client: &mut TestClient,
  manager: &ZwlrOutputManagerV1,
  off: &[&str],
  positions: &[(&str, (i32, i32))],
) -> &'static str {
  let configuration = manager.create_configuration(
    client.events.output_serial,
    &client.handle,
    (),
  );
  let live = client
    .events
    .head_objects
    .iter()
    .filter(|(_, finished)| !finished)
    .map(|(head, _)| head.clone())
    .collect::<Vec<_>>();
  for head in live {
    let name = client.events.head_names[&head.id()].as_str();
    if off.contains(&name) {
      configuration.disable_head(&head);
      continue;
    }
    let configured = configuration.enable_head(&head, &client.handle, ());
    if let Some((_, (x, y))) = positions.iter().find(|(on, _)| *on == name) {
      configured.set_position(*x, *y);
    }
  }
  configuration.apply();
  client.events.configuration_results.clear();
  client.roundtrip(compositor);
  client
    .events
    .configuration_results
    .last()
    .copied()
    .unwrap_or("none")
}

fn output_names(compositor: &Compositor) -> Vec<String> {
  compositor
    .outputs()
    .into_iter()
    .map(|output| output.handle.name().to_owned())
    .collect()
}

#[test]
fn a_configuration_turns_outputs_off_and_on_again() {
  let (mut compositor, path) = two_outputs();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "moved");
  move_to(&mut compositor, window, "HEADLESS-2");
  let both = BTreeMap::from([
    ("HEADLESS-1".to_owned(), true),
    ("HEADLESS-2".to_owned(), true),
  ]);
  assert_eq!(heads(&client), both);
  let result =
    apply(&mut compositor, &mut client, &manager, &["HEADLESS-2"], &[]);
  assert_eq!(result, "succeeded");
  assert_eq!(output_names(&compositor), ["HEADLESS-1"]);
  let mut off = both.clone();
  off.insert("HEADLESS-2".into(), false);
  assert_eq!(heads(&client), off, "the head stays, disabled");
  let location = compositor.core().window_location(window).unwrap();
  assert_eq!(location.output, id("HEADLESS-1"), "unplugged under return");
  // A manager bound now sees the disabled head too.
  let (late, _late_manager) = manager_client(&mut compositor, &path);
  assert_eq!(heads(&late), off);
  // Its binding sent a new serial.
  client.roundtrip(&mut compositor);
  let result = apply(
    &mut compositor,
    &mut client,
    &manager,
    &[],
    &[("HEADLESS-2", (-800, 0))],
  );
  assert_eq!(result, "succeeded");
  assert_eq!(heads(&client), both);
  let second = compositor
    .outputs()
    .into_iter()
    .find(|output| output.handle.name() == "HEADLESS-2")
    .expect("HEADLESS-2 is on again");
  assert_eq!(second.rect, shingle_core::Rect::new(-800, 0, 800, 600));
  let location = compositor.core().window_location(window).unwrap();
  assert_eq!(location.output, id("HEADLESS-2"), "and its window is back");
  let _ = std::fs::remove_file(path);
}

#[test]
fn turning_every_output_off_fails() {
  let (mut compositor, path) = two_outputs();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let result = apply(
    &mut compositor,
    &mut client,
    &manager,
    &["HEADLESS-1", "HEADLESS-2"],
    &[],
  );
  assert_eq!(result, "failed");
  assert_eq!(output_names(&compositor), ["HEADLESS-1", "HEADLESS-2"]);
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_configured_output_keeps_its_place_across_config_reloads() {
  let (mut compositor, path) = two_outputs();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let result = apply(
    &mut compositor,
    &mut client,
    &manager,
    &[],
    &[("HEADLESS-2", (0, 720))],
  );
  assert_eq!(result, "succeeded");
  let place = |compositor: &Compositor| {
    compositor
      .outputs()
      .into_iter()
      .map(|output| (output.handle.name().to_owned(), output.rect))
      .collect::<Vec<_>>()
  };
  let configured = place(&compositor);
  compositor.apply_config(Config::default()).unwrap();
  assert_eq!(place(&compositor), configured);
  let _ = std::fs::remove_file(path);
}

#[test]
fn outputs_are_powered_off_and_on_one_at_a_time() {
  use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::Mode,
  };
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let manager = client.bind::<ZwlrOutputPowerManagerV1>(1);
  let first =
    manager.get_output_power(&outputs["HEADLESS-1"], &client.handle, ());
  let second =
    manager.get_output_power(&outputs["HEADLESS-2"], &client.handle, ());
  client.roundtrip(&mut compositor);
  let last = |client: &TestClient,
              control: wayland_client::backend::ObjectId| {
    client
      .events
      .power
      .iter()
      .rev()
      .find(|(id, _)| *id == control)
      .map(|(_, seen)| *seen)
  };
  assert_eq!(last(&client, first.id()), Some("on"));
  second.set_mode(Mode::Off);
  client.roundtrip(&mut compositor);
  assert_eq!(last(&client, second.id()), Some("off"));
  assert_eq!(last(&client, first.id()), Some("on"));
  assert_eq!(compositor.dpms_state(), crate::DpmsState::On);
  assert!(!compositor.output_powered(&OutputHandle::new("HEADLESS-2")));
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "dark");
  move_to(&mut compositor, window, "HEADLESS-2");
  assert!(
    compositor
      .visible_surfaces_on(&OutputHandle::new("HEADLESS-2"))
      .is_empty()
  );
  // Turning the last lit output off is DPMS off; waking lights both.
  first.set_mode(Mode::Off);
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.dpms_state(), crate::DpmsState::Off);
  first.set_mode(Mode::On);
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.dpms_state(), crate::DpmsState::On);
  assert_eq!(last(&client, second.id()), Some("on"));
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  assert_eq!(last(&client, second.id()), Some("failed"));
  assert_eq!(last(&client, first.id()), Some("on"));
  let _ = std::fs::remove_file(path);
}

#[test]
fn every_output_has_a_gamma_ramp_of_its_own() {
  use std::os::fd::AsFd;
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let manager = client.bind::<gamma::ZwlrGammaControlManagerV1>(1);
  let first =
    manager.get_gamma_control(&outputs["HEADLESS-1"], &client.handle, ());
  let second =
    manager.get_gamma_control(&outputs["HEADLESS-2"], &client.handle, ());
  let again =
    manager.get_gamma_control(&outputs["HEADLESS-1"], &client.handle, ());
  client.roundtrip(&mut compositor);
  let seen = |control: wayland_client::backend::ObjectId| {
    client
      .events
      .gamma
      .iter()
      .filter(|(id, _)| *id == control)
      .map(|(_, seen)| *seen)
      .collect::<Vec<_>>()
  };
  assert_eq!(seen(first.id()), ["size"]);
  assert_eq!(seen(second.id()), ["size"]);
  assert_eq!(seen(again.id()), ["failed"], "one control per output");
  let identity = compositor.gamma_ramp().to_vec();
  let fd =
    rustix::fs::memfd_create("shingle-gamma", rustix::fs::MemfdFlags::CLOEXEC)
      .unwrap();
  let ramp = vec![0x12u8; 256 * 3 * 2];
  rustix::io::write(&fd, &ramp).unwrap();
  rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0)).unwrap();
  second.set_gamma(fd.as_fd());
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.gamma_ramp(), identity, "HEADLESS-1 is unchanged");
  let changed = compositor.gamma_ramp_of(&OutputHandle::new("HEADLESS-2"));
  assert!(changed.iter().all(|value| *value == 0x1212));
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  let seen = client
    .events
    .gamma
    .iter()
    .filter(|(id, _)| *id == second.id())
    .count();
  assert_eq!(seen, 2, "size, then failed");
  let _ = std::fs::remove_file(path);
}

/// The live workspace groups a client sees: each one's output name and
/// its workspaces' ids, sorted, with the active one marked by a `*`.
fn workspace_groups(
  client: &TestClient,
  outputs: &HashMap<String, WlOutput>,
) -> BTreeMap<String, Vec<String>> {
  client
    .events
    .workspace_groups
    .values()
    .filter(|group| !group.removed)
    .map(|group| {
      let output = group
        .outputs
        .iter()
        .filter_map(|id| {
          outputs
            .iter()
            .find(|(_, output)| output.id() == *id)
            .map(|(name, _)| name.clone())
        })
        .collect::<Vec<_>>()
        .join(",");
      let mut workspaces = group
        .workspaces
        .iter()
        .map(|id| {
          let (_, seen) = &client.events.workspaces[id];
          format!("{}{}", seen.id, if seen.active { "*" } else { "" })
        })
        .collect::<Vec<_>>();
      workspaces.sort();
      (output, workspaces)
    })
    .collect()
}

#[test]
fn every_output_has_a_workspace_group_of_its_own() {
  let config = Config {
    workspace_width: 2,
    workspace_height: 1,
    ..Config::default()
  };
  let (mut compositor, path) = test_compositor_with(config);
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .unwrap();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let manager = client.bind::<workspaces::ExtWorkspaceManagerV1>(1);
  client.roundtrip(&mut compositor);
  client.roundtrip(&mut compositor);
  let expected = |second: [&str; 2]| {
    BTreeMap::from([
      (
        "HEADLESS-1".to_owned(),
        vec![
          "HEADLESS-1:workspace-0-0*".to_owned(),
          "HEADLESS-1:workspace-1-0".to_owned(),
        ],
      ),
      (
        "HEADLESS-2".to_owned(),
        second.iter().map(|id| (*id).to_owned()).collect(),
      ),
    ])
  };
  assert_eq!(
    workspace_groups(&client, &outputs),
    expected(["HEADLESS-2:workspace-0-0*", "HEADLESS-2:workspace-1-0"])
  );
  let (handle, _) = client
    .events
    .workspaces
    .values()
    .find(|(_, seen)| seen.id == "HEADLESS-2:workspace-1-0")
    .unwrap()
    .clone();
  handle.activate();
  manager.commit();
  client.roundtrip(&mut compositor);
  client.roundtrip(&mut compositor);
  assert_eq!(
    workspace_groups(&client, &outputs),
    expected(["HEADLESS-2:workspace-0-0", "HEADLESS-2:workspace-1-0*"]),
    "only HEADLESS-2 switched"
  );
  assert_eq!(
    compositor
      .core()
      .current_workspace(&id("HEADLESS-1"))
      .unwrap(),
    WorkspaceId::ORIGIN
  );
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  client.roundtrip(&mut compositor);
  let groups = workspace_groups(&client, &outputs);
  assert_eq!(groups.keys().collect::<Vec<_>>(), ["HEADLESS-1"]);
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_toplevel_handle_is_on_its_windows_output() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let _manager = client.bind::<toplevels::ZwlrForeignToplevelManagerV1>(3);
  client.roundtrip(&mut compositor);
  let (_toplevel, window) = mapped(&mut compositor, &mut client, "listed");
  client.roundtrip(&mut compositor);
  let on = |client: &TestClient| {
    let lists = client.events.toplevel_outputs.values().collect::<Vec<_>>();
    assert_eq!(lists.len(), 1, "one handle");
    lists[0]
      .iter()
      .map(|id| {
        outputs
          .iter()
          .find(|(_, output)| output.id() == *id)
          .map_or("?", |(name, _)| name.as_str())
          .to_owned()
      })
      .collect::<Vec<_>>()
  };
  assert_eq!(on(&client), ["HEADLESS-1"]);
  move_to(&mut compositor, window, "HEADLESS-2");
  client.roundtrip(&mut compositor);
  assert_eq!(on(&client), ["HEADLESS-2"]);
  compositor
    .remove_output(&OutputHandle::new("HEADLESS-2"))
    .unwrap();
  client.roundtrip(&mut compositor);
  assert_eq!(on(&client), ["HEADLESS-1"], "it joined the active output");
  let _ = std::fs::remove_file(path);
}

#[test]
fn a_taskbar_rectangle_is_the_minimize_hint() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let _manager = client.bind::<toplevels::ZwlrForeignToplevelManagerV1>(3);
  client.roundtrip(&mut compositor);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "hinted");
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.minimize_hint(window), None);
  let handle = client.events.toplevel_handles[0].clone();
  // Relative to the surface it names, as Wayfire adds its position.
  let origin = compositor
    .window_surfaces(window)
    .into_iter()
    .find(|snapshot| snapshot.window == Some(window))
    .unwrap()
    .geometry;
  handle.set_rectangle(&toplevel.surface, 10, 20, 30, 40);
  client.roundtrip(&mut compositor);
  assert_eq!(
    compositor.minimize_hint(window),
    Some(shingle_core::Rect::new(
      origin.x + 10,
      origin.y + 20,
      30,
      40
    ))
  );
  // An empty rectangle takes it back.
  handle.set_rectangle(&toplevel.surface, 0, 0, 0, 0);
  client.roundtrip(&mut compositor);
  assert_eq!(compositor.minimize_hint(window), None);
  let _ = std::fs::remove_file(path);
}

#[test]
fn outputs_at_negative_positions_are_warned_about_for_x() {
  let (mut compositor, _path) = two_outputs();
  assert!(
    super::negative_positions(compositor.state.outputs.iter()).is_empty()
  );
  compositor
    .state
    .set_rect_of(
      &id("HEADLESS-2"),
      shingle_core::Rect::new(-800, 0, 800, 600),
      true,
    )
    .unwrap();
  assert_eq!(
    super::negative_positions(compositor.state.outputs.iter()),
    ["HEADLESS-2 is at -800,0; X windows cannot be shown there"]
  );
}

#[test]
fn the_first_output_turned_off_and_on_is_first_again() {
  let (mut compositor, path) = two_outputs();
  let (mut client, manager) = manager_client(&mut compositor, &path);
  let result =
    apply(&mut compositor, &mut client, &manager, &["HEADLESS-1"], &[]);
  assert_eq!(result, "succeeded");
  assert_eq!(compositor.primary_output(), OutputHandle::new("HEADLESS-2"));
  let result = apply(&mut compositor, &mut client, &manager, &[], &[]);
  assert_eq!(result, "succeeded");
  assert_eq!(output_names(&compositor), ["HEADLESS-1", "HEADLESS-2"]);
  let rects = compositor
    .outputs()
    .into_iter()
    .map(|output| output.rect)
    .collect::<Vec<_>>();
  assert_eq!(
    rects,
    [
      shingle_core::Rect::new(0, 0, 1280, 720),
      shingle_core::Rect::new(1280, 0, 800, 600)
    ],
    "automatic positions as before"
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn without_output_limits_a_window_is_drawn_and_entered_where_it_overlaps() {
  let (mut compositor, path) = test_compositor_with(Config {
    remove_output_limits: true,
    ..Config::default()
  });
  compositor
    .add_output(
      OutputInfo {
        name: "HEADLESS-2".into(),
        ..OutputInfo::default()
      },
      (800, 600),
    )
    .expect("a second output");
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let (toplevel, window) = mapped(&mut compositor, &mut client, "wide");
  // Its centre stays on HEADLESS-1's current workspace.
  let reply = crate::ipc::respond(
    &mut compositor.state,
    &serde_json::json!({
      "method": "window-rules/configure-view",
      "data": {
        "id": window.0,
        "geometry": {"x": 1170, "y": 20, "width": 200, "height": 100},
      },
    }),
  );
  assert_eq!(reply, serde_json::json!({"result": "ok"}));
  client.roundtrip(&mut compositor);
  let windows_on = |output: &str| {
    compositor
      .visible_surfaces_on(&OutputHandle::new(output))
      .into_iter()
      .filter_map(|snapshot| snapshot.window)
      .collect::<Vec<_>>()
  };
  assert_eq!(windows_on("HEADLESS-1"), [window]);
  assert_eq!(
    windows_on("HEADLESS-2"),
    [window],
    "drawn where it overlaps"
  );
  assert_eq!(
    enters_and_leaves(&client, &toplevel.surface, &outputs),
    ["enter HEADLESS-1", "enter HEADLESS-2"]
  );
  let _ = std::fs::remove_file(path);
}

#[test]
fn layer_surfaces_come_below_or_above_the_windows_and_stack_per_output() {
  let (mut compositor, path) = two_outputs();
  let mut client = TestClient::connect(&mut compositor, &path);
  let outputs = bind_outputs(&mut compositor, &mut client);
  let wl_compositor = client.bind::<WlCompositor>(4);
  let shell = client.bind::<ZwlrLayerShellV1>(4);
  // Told apart by their heights.
  let mut surfaces = Vec::new();
  for (layer, height) in [(Layer::Background, 40), (Layer::Top, 30)] {
    let surface = wl_compositor.create_surface(&client.handle, ());
    let layer_surface = shell.get_layer_surface(
      &surface,
      Some(&outputs["HEADLESS-1"]),
      layer,
      "test".into(),
      &client.handle,
      (),
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
    layer_surface.set_size(0, height);
    surface.commit();
    client.roundtrip(&mut compositor);
    let &(_, serial, ..) = client.events.layer_configures.last().unwrap();
    layer_surface.ack_configure(serial);
    client.attach_buffer(&mut compositor, &surface, 1280, height as i32);
    client.roundtrip(&mut compositor);
    surfaces.push((surface, layer_surface));
  }
  let (_first, first) = mapped(&mut compositor, &mut client, "first");
  let (_second, second) = mapped(&mut compositor, &mut client, "second");
  let heights = |output: &str, above: bool| {
    compositor
      .layer_surfaces_on(&OutputHandle::new(output), above)
      .into_iter()
      .map(|snapshot| snapshot.geometry.height)
      .collect::<Vec<_>>()
  };
  assert_eq!(heights("HEADLESS-1", false), [40], "the background");
  assert_eq!(heights("HEADLESS-1", true), [30], "the top layer");
  assert!(heights("HEADLESS-2", false).is_empty());
  let stacking = |compositor: &Compositor, output: &str| {
    compositor.stacking_order_on(&OutputHandle::new(output))
  };
  assert_eq!(stacking(&compositor, "HEADLESS-1"), [first, second]);
  assert!(stacking(&compositor, "HEADLESS-2").is_empty());
  compositor.raise_window(first).unwrap();
  assert_eq!(stacking(&compositor, "HEADLESS-1"), [second, first]);
  let _ = std::fs::remove_file(path);
}
