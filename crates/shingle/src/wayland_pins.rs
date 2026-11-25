//! Pins what Wayland toplevels look like from outside the compositor.
//!
//! One scripted session of xdg toplevels is recorded as text: every IPC
//! reply and event (`window-rules/list-views`, `view-info`, the
//! `view-*` events), the renderer events, the surface snapshots the
//! frontend draws from, every `xdg_toplevel.configure` with its states,
//! the session snapshot, and a restore of that snapshot. The transcript
//! must match `testdata/wayland_pins.txt` byte for byte. It was recorded
//! before the toplevel role was made shell-neutral (XW-1), so any change
//! there that a Wayland client or IPC script could notice fails here.
//!
//! Run with `SHINGLE_BLESS_PINS=1` to rewrite the file after an intended
//! change, and review the diff.

use std::{
  mem,
  path::PathBuf,
  sync::atomic::{AtomicU32, Ordering},
};

use serde_json::{Value, json};
use shingle_core::Shingle;
use wayland_client::Proxy;
use wayland_server::ListeningSocket;

use crate::{
  Compositor, Config,
  test_client::{TestClient, Toplevel},
};

/// The recorded transcript.
const GOLDEN: &str = include_str!("../testdata/wayland_pins.txt");

/// Where [`GOLDEN`] lives, for rewriting it.
const GOLDEN_PATH: &str =
  concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/wayland_pins.txt");

/// Lines of `label<TAB>text`, in the order they happened.
#[derive(Default)]
struct Transcript {
  lines: Vec<String>,
}

impl Transcript {
  fn record(&mut self, label: &str, text: impl AsRef<str>) {
    self.lines.push(format!("{label}\t{}", text.as_ref()));
  }

  /// Records an IPC request and its reply as sent on the wire.
  fn ipc(
    &mut self,
    compositor: &mut Compositor,
    method: &str,
    data: Value,
  ) -> Value {
    let request = json!({"method": method, "data": data});
    let reply = crate::ipc::respond(&mut compositor.state, &request);
    self.record("ipc-request", request.to_string());
    self.record("ipc-reply", reply.to_string());
    reply
  }

  /// Records the renderer events and the IPC events they produced.
  fn events(&mut self, compositor: &mut Compositor) {
    for event in compositor.drain_events() {
      self.record("event", format!("{event:?}"));
    }
    for message in mem::take(&mut compositor.state.ipc_events) {
      self.record("ipc-event", message.to_string());
    }
  }

  /// Records everything a frontend or IPC script can read at this point.
  fn views(&mut self, compositor: &mut Compositor) {
    self.ipc(compositor, "window-rules/list-views", json!({}));
    self.ipc(compositor, "window-rules/get-focused-view", json!({}));
    let windows = compositor.state.core.window_ids().collect::<Vec<_>>();
    for window in windows {
      self.ipc(
        compositor,
        "window-rules/view-info",
        json!({"id": window.0}),
      );
    }
    for surface in compositor.visible_surfaces() {
      self.record("visible", snapshot_text(&surface));
    }
    for surface in compositor.scale_surfaces(true, true) {
      self.record("scale", snapshot_text(&surface));
    }
  }

  fn step(&mut self, name: &str) {
    self.record("step", name);
  }
}

/// A surface snapshot without its buffer contents.
fn snapshot_text(surface: &crate::SurfaceSnapshot) -> String {
  format!(
    "id={} window={:?} parent={:?} role={:?} app_id={:?} title={:?} \
     geometry={:?} opacity={} buffer={:?}",
    surface.id.0,
    surface.window.map(|window| window.0),
    surface.parent.map(|parent| parent.0),
    surface.role,
    surface.app_id,
    surface.title,
    surface.geometry,
    surface.opacity,
    surface
      .buffer
      .as_ref()
      .map(|buffer| (buffer.width, buffer.height)),
  )
}

/// The session snapshot as JSON, with the parts of each launch identity
/// that depend on the test process (its command line, directory and PID)
/// replaced by placeholders.
fn session_text(core: &Shingle) -> String {
  let json = core.snapshot().to_json().expect("the snapshot serializes");
  let mut value: Value = serde_json::from_str(&json).expect("valid JSON");
  if let Some(windows) = value["windows"].as_array_mut() {
    for window in windows {
      let identity = &mut window["identity"];
      for key in ["command", "working_directory", "launch_group"] {
        if identity.get(key).is_some_and(|value| !value.is_null()) {
          identity[key] = json!(format!("<{key}>"));
        }
      }
    }
  }
  value.to_string()
}

/// A compositor around `core` on a socket in the temporary directory.
fn compositor_with(core: Shingle) -> (Compositor, PathBuf) {
  static NEXT: AtomicU32 = AtomicU32::new(0);
  let name = format!(
    "shingle-pins-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  );
  let path = std::env::temp_dir().join(&name);
  let socket = ListeningSocket::bind_absolute(path.clone())
    .expect("the temporary directory accepts a test socket");
  let compositor =
    Compositor::with_socket(Config::default(), core, socket, name.into())
      .expect("the default config builds a compositor");
  (compositor, path)
}

/// Creates a toplevel with `app_id` and `title` and gives it a buffer.
fn map(
  client: &mut TestClient,
  compositor: &mut Compositor,
  app_id: &str,
  title: &str,
  parent: Option<&Toplevel>,
) -> Toplevel {
  let toplevel = client.toplevel(compositor, app_id);
  toplevel.toplevel.set_title(title.into());
  if let Some(parent) = parent {
    toplevel.toplevel.set_parent(Some(&parent.toplevel));
  }
  client.roundtrip(compositor);
  client.attach_buffer(compositor, &toplevel.surface, 100, 80);
  toplevel
}

/// Records the configures each named toplevel received, in order.
fn configures(
  transcript: &mut Transcript,
  client: &TestClient,
  toplevels: &[(&str, &Toplevel)],
) {
  for (id, width, height, states) in &client.events.configure_states {
    let name = toplevels
      .iter()
      .find(|(_, toplevel)| toplevel.toplevel.id() == *id)
      .map_or("?", |(name, _)| name);
    transcript.record(
      "configure",
      format!("{name} {width}x{height} states={states:?}"),
    );
  }
  for id in &client.events.closes {
    let name = toplevels
      .iter()
      .find(|(_, toplevel)| toplevel.toplevel.id() == *id)
      .map_or("?", |(name, _)| name);
    transcript.record("close", name);
  }
}

/// Plays the scripted session and returns its transcript.
fn transcript() -> String {
  let mut out = Transcript::default();
  let (mut compositor, path) = compositor_with(Shingle::new());
  let mut client = TestClient::connect(&mut compositor, &path);

  out.step("map a");
  let a = map(&mut client, &mut compositor, "pin-a", "Alpha", None);
  out.events(&mut compositor);
  out.step("map b with size hints");
  let b = client.toplevel(&mut compositor, "pin-b");
  b.toplevel.set_title("Beta".into());
  b.toplevel.set_min_size(50, 40);
  b.toplevel.set_max_size(900, 700);
  client.roundtrip(&mut compositor);
  client.attach_buffer(&mut compositor, &b.surface, 100, 80);
  out.events(&mut compositor);
  out.views(&mut compositor);

  out.step("fullscreen a");
  let (wa, wb) = (1, 2);
  out.ipc(
    &mut compositor,
    "wm-actions/set-fullscreen",
    json!({"view_id": wa, "state": true}),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);
  out.views(&mut compositor);
  out.ipc(
    &mut compositor,
    "wm-actions/set-fullscreen",
    json!({"view_id": wa, "state": false}),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);

  out.step("minimize b");
  out.ipc(
    &mut compositor,
    "wm-actions/set-minimized",
    json!({"view_id": wb, "state": true}),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);
  out.views(&mut compositor);
  out.ipc(
    &mut compositor,
    "wm-actions/set-minimized",
    json!({"view_id": wb, "state": false}),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);

  out.step("stacking, sticky, alpha");
  out.ipc(
    &mut compositor,
    "wm-actions/set-always-on-top",
    json!({"view_id": wa, "state": true}),
  );
  out.ipc(
    &mut compositor,
    "wm-actions/set-sticky",
    json!({"view_id": wb, "state": true}),
  );
  out.ipc(
    &mut compositor,
    "wf/alpha/set-view-alpha",
    json!({"view-id": wa, "alpha": 0.5}),
  );
  out.ipc(
    &mut compositor,
    "wf/alpha/get-view-alpha",
    json!({"view-id": wa}),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);

  out.step("float b by configure-view");
  out.ipc(
    &mut compositor,
    "window-rules/configure-view",
    json!({
      "id": wb,
      "geometry": {"x": 40, "y": 30, "width": 300, "height": 200},
    }),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);
  out.ipc(
    &mut compositor,
    "simple-tile/get-layout",
    json!({"wset-index": 1, "workspace": {"x": 0, "y": 0}}),
  );

  out.step("retitle a");
  a.toplevel.set_title("Alpha 2".into());
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);

  out.step("map c as a child of a");
  let c = map(&mut client, &mut compositor, "pin-c", "Child", Some(&a));
  out.events(&mut compositor);
  out.views(&mut compositor);

  out.step("focus a");
  out.ipc(
    &mut compositor,
    "window-rules/focus-view",
    json!({"id": wa}),
  );
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);

  out.step("session");
  let saved = compositor.state.core.snapshot();
  out.record("session", session_text(&compositor.state.core));

  out.step("close b");
  out.ipc(
    &mut compositor,
    "window-rules/close-view",
    json!({"id": wb}),
  );
  client.roundtrip(&mut compositor);
  b.toplevel.destroy();
  b._xdg_surface.destroy();
  b.surface.destroy();
  client.roundtrip(&mut compositor);
  out.events(&mut compositor);
  out.views(&mut compositor);
  out.record("session", session_text(&compositor.state.core));
  configures(&mut out, &client, &[("a", &a), ("b", &b), ("c", &c)]);
  drop(client);
  drop(compositor);

  out.step("restore");
  let core = Shingle::from_snapshot(saved).expect("the snapshot restores");
  let (mut compositor, path) = compositor_with(core);
  let mut client = TestClient::connect(&mut compositor, &path);
  out.views(&mut compositor);
  let b = map(&mut client, &mut compositor, "pin-b", "Beta again", None);
  out.events(&mut compositor);
  out.views(&mut compositor);
  out.record("session", session_text(&compositor.state.core));
  configures(&mut out, &client, &[("b", &b)]);

  let mut text = out.lines.join("\n");
  text.push('\n');
  text
}

#[test]
fn wayland_toplevels_look_unchanged() {
  let text = transcript();
  if std::env::var_os("SHINGLE_BLESS_PINS").is_some() {
    std::fs::write(GOLDEN_PATH, &text).expect("the pin file is writable");
    return;
  }
  for (number, (actual, expected)) in
    text.lines().zip(GOLDEN.lines()).enumerate()
  {
    assert_eq!(actual, expected, "line {} differs", number + 1);
  }
  assert_eq!(text.lines().count(), GOLDEN.lines().count(), "line count");
  assert_eq!(text, GOLDEN);
}

#[test]
fn the_transcript_is_deterministic() {
  assert_eq!(transcript(), transcript());
}
