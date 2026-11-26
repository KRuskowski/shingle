//! Outputs that are windows of a host session, which STIPC opens and
//! closes as Wayfire's does for wlroots' Wayland backend
//! (`stipc/create_wayland_output` and `stipc/destroy_wayland_output`,
//! plugins/ipc/stipc.cpp; docs/design/multi-output.md, section 7).
//!
//! Only a backend that shows each output in a window of its own, nested,
//! turns them on ([`crate::Compositor::set_host_outputs`]). The compositor
//! adds or removes the output at once, so the IPC reply can name it, and
//! asks the host to open or close its window with
//! [`crate::Event::CreateHostOutput`] and
//! [`crate::Event::DestroyHostOutput`].

use shingle_core::OutputId;

use super::{OutputHandle, OutputInfo, OutputKind, add, remove};
use crate::{Error, state::CompositorState};

/// Why [`destroy`] refused an output, with Wayfire's error texts.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum DestroyError {
  /// No live output has the name.
  #[error("Could not find output: \"{0}\"")]
  NotFound(String),
  /// The output is not a host window: an IPC headless output, or any
  /// output of a backend without host windows.
  #[error("Output is not a wayland output!")]
  NotHostOutput,
  /// It is the last host window. Closing that one quits nested Shingle,
  /// which IPC does not do.
  #[error("Cannot destroy the last wayland output!")]
  Last,
  /// Removing the output failed.
  #[error("{0}")]
  Failed(String),
}

/// The physical size output `name`'s configuration entry asks for, as
/// [`crate::Compositor::configured_physical_size`] gives it.
pub(crate) fn configured_physical_size(
  state: &CompositorState,
  name: &str,
) -> (u32, u32) {
  let placement = state.placements.get(name);
  let scale = |extent: u32| (f64::from(extent) * placement.scale).round();
  (
    scale(placement.width).clamp(1.0, f64::from(u32::MAX)) as u32,
    scale(placement.height).clamp(1.0, f64::from(u32::MAX)) as u32,
  )
}

/// Adds the output of a new host window, named after the lowest free
/// `HEADLESS-N` and sized by its configuration entry until the window
/// reports its size, and asks the host to open the window.
pub(crate) fn create(state: &mut CompositorState) -> Result<OutputId, Error> {
  let name = state.outputs.free_headless_name();
  let physical = configured_physical_size(state, &name);
  let id = add(
    state,
    OutputInfo {
      name,
      ..OutputInfo::default()
    },
    physical,
    OutputKind::Backend,
  )?;
  state.events.push(crate::Event::CreateHostOutput {
    output: OutputHandle::from(&id),
  });
  Ok(id)
}

/// Removes host window output `name` under `output_layout.unplug` and
/// asks the host to close its window. Outputs created over IPC and the
/// last host window are refused.
pub(crate) fn destroy(
  state: &mut CompositorState,
  name: &str,
) -> Result<(), DestroyError> {
  let id = OutputId::new(name);
  let Some(output) = state.outputs.get(&id) else {
    return Err(DestroyError::NotFound(name.to_owned()));
  };
  if !state.host_outputs || output.kind != OutputKind::Backend {
    return Err(DestroyError::NotHostOutput);
  }
  if !state
    .outputs
    .iter()
    .any(|other| other.id != id && other.kind == OutputKind::Backend)
  {
    return Err(DestroyError::Last);
  }
  remove(state, &id)
    .map_err(|error| DestroyError::Failed(error.to_string()))?;
  state.events.push(crate::Event::DestroyHostOutput {
    output: OutputHandle::from(&id),
  });
  Ok(())
}

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};
  use shingle_core::{LaunchIdentity, WindowId};

  use super::*;
  use crate::{Compositor, Event, tests::test_compositor};

  fn ipc(compositor: &mut Compositor, method: &str, data: Value) -> Value {
    crate::ipc::respond(
      &mut compositor.state,
      &json!({"method": method, "data": data}),
    )
  }

  fn create(compositor: &mut Compositor) -> Value {
    ipc(compositor, "stipc/create_wayland_output", json!({}))
  }

  fn destroy(compositor: &mut Compositor, name: &str) -> Value {
    ipc(
      compositor,
      "stipc/destroy_wayland_output",
      json!({"output": name}),
    )
  }

  /// A nested compositor: host windows on, its first window's output
  /// `HEADLESS-1` and events drained.
  fn nested() -> Compositor {
    let (mut compositor, _path) = test_compositor();
    compositor.set_host_outputs(true);
    compositor.drain_events();
    compositor
  }

  /// What the host was asked to open and close, in order.
  fn host_requests(compositor: &mut Compositor) -> Vec<(bool, String)> {
    compositor
      .drain_events()
      .into_iter()
      .filter_map(|event| match event {
        Event::CreateHostOutput { output } => {
          Some((true, output.name().to_owned()))
        }
        Event::DestroyHostOutput { output } => {
          Some((false, output.name().to_owned()))
        }
        _ => None,
      })
      .collect()
  }

  fn saved_window(compositor: &mut Compositor, output: &str) -> WindowId {
    compositor
      .state
      .core
      .attach_window(&OutputId::new(output), LaunchIdentity::new("saved"))
      .expect("the output shows a set")
  }

  #[test]
  fn without_host_windows_both_methods_refuse_as_wayfire_does() {
    let (mut compositor, _path) = test_compositor();
    assert_eq!(
      create(&mut compositor),
      json!({"error": "Wayfire is not running in nested wayland mode!"})
    );
    assert_eq!(
      destroy(&mut compositor, "HEADLESS-1"),
      json!({"error": "Output is not a wayland output!"})
    );
    assert_eq!(compositor.outputs().len(), 1);
    assert!(host_requests(&mut compositor).is_empty());
  }

  #[test]
  fn creating_adds_the_output_at_once_and_asks_the_host_for_a_window() {
    let mut compositor = nested();
    let reply = create(&mut compositor);
    assert_eq!(reply["result"], json!("ok"));
    assert_eq!(reply["output"]["name"], json!("HEADLESS-2"));
    assert_eq!(reply["output"]["id"], json!(2));
    assert_eq!(
      reply["output"]["geometry"],
      json!({"x": 1280, "y": 0, "width": 1280, "height": 720})
    );
    let events = compositor.drain_events();
    let added = events.iter().position(|event| {
      matches!(event, Event::OutputAdded { output }
        if output.name() == "HEADLESS-2")
    });
    let window = events.iter().position(|event| {
      matches!(event, Event::CreateHostOutput { output }
        if output.name() == "HEADLESS-2")
    });
    assert!(
      added.is_some() && added < window,
      "the output is added before its window is asked for: {events:?}"
    );
    assert_eq!(create(&mut compositor)["output"]["name"], "HEADLESS-3");
    assert_eq!(
      host_requests(&mut compositor),
      [(true, "HEADLESS-3".into())]
    );
  }

  #[test]
  fn destroying_refuses_what_is_no_host_window_with_wayfires_errors() {
    let mut compositor = nested();
    let cases = [
      (json!({}), json!({"error": "Missing \"output\""})),
      (
        json!({"output": 2}),
        json!({"error":
          "Field \"output\" does not have the correct type, expected string"}),
      ),
      (
        json!({"output": "DP-9"}),
        json!({"error": "Could not find output: \"DP-9\""}),
      ),
      (
        json!({"output": "HEADLESS-1"}),
        json!({"error": "Cannot destroy the last wayland output!"}),
      ),
    ];
    for (data, expected) in cases {
      let reply = ipc(&mut compositor, "stipc/destroy_wayland_output", data);
      assert_eq!(reply, expected);
    }
    let created = ipc(
      &mut compositor,
      "wayfire/create-headless-output",
      json!({"width": 640, "height": 480}),
    );
    let name = created["output"]["name"].as_str().unwrap().to_owned();
    assert_eq!(
      destroy(&mut compositor, &name),
      json!({"error": "Output is not a wayland output!"})
    );
    assert_eq!(
      destroy(&mut compositor, "HEADLESS-1"),
      json!({"error": "Cannot destroy the last wayland output!"}),
      "an IPC output does not count as a window"
    );
    assert_eq!(compositor.outputs().len(), 2);
    assert!(host_requests(&mut compositor).is_empty());
  }

  #[test]
  fn a_closed_window_returns_its_windows_when_it_is_opened_again() {
    let mut compositor = nested();
    create(&mut compositor);
    create(&mut compositor);
    let window = saved_window(&mut compositor, "HEADLESS-2");
    compositor.drain_events();
    assert_eq!(
      destroy(&mut compositor, "HEADLESS-2"),
      json!({"result": "ok"})
    );
    assert_eq!(
      host_requests(&mut compositor),
      [(false, "HEADLESS-2".into())]
    );
    let info = compositor.core().window_info(window).unwrap();
    assert_eq!(info.location.output, OutputId::new("HEADLESS-1"));
    assert!(
      info.away_from.is_some(),
      "under return it remembers its set"
    );
    // The next window takes the free name back, and with it the set.
    let reply = create(&mut compositor);
    assert_eq!(reply["output"]["name"], json!("HEADLESS-2"));
    let info = compositor.core().window_info(window).unwrap();
    assert_eq!(
      (info.location.output, info.away_from),
      (OutputId::new("HEADLESS-2"), None)
    );
    let names = compositor
      .outputs()
      .into_iter()
      .map(|output| output.handle.name().to_owned())
      .collect::<Vec<_>>();
    assert_eq!(names, ["HEADLESS-1", "HEADLESS-3", "HEADLESS-2"]);
  }

  #[test]
  fn points_on_a_window_map_to_its_own_output() {
    let mut compositor = nested();
    create(&mut compositor);
    compositor
      .set_output_size(&OutputHandle::new("HEADLESS-2"), (800, 600))
      .unwrap();
    let second = OutputHandle::new("HEADLESS-2");
    assert_eq!(
      compositor.map_point_on(&second, 800.0, 600.0, 1600, 1200),
      Some((400.0, 300.0)),
      "relative to the output, scaled from the window's size"
    );
    assert_eq!(
      compositor.unmap_point_on(&second, 400.0, 300.0, 1600, 1200),
      Some((800.0, 600.0))
    );
    assert_eq!(
      compositor.map_vector_on(&second, 10.0, -4.0, 1600, 1200),
      Some((5.0, -2.0))
    );
    let gone = OutputHandle::new("HEADLESS-9");
    assert_eq!(compositor.map_point_on(&gone, 1.0, 1.0, 10, 10), None);
    // The first output's calls are unchanged and in layout coordinates.
    assert_eq!(
      compositor.map_output_point(640.0, 360.0, 1280, 720),
      (640.0, 360.0)
    );
  }
}
