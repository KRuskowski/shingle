//! wayfire-shell-unstable-v2 (`zwf_shell_manager_v2` version 3), the
//! protocol of wf-shell's panel and dock, following
//! wayfire:plugins/protocols/wayfire-shell.cpp
//! (docs/design/wayfire-gaps.md, WG-4).
//!
//! A `zwf_output_v2` gets `enter_fullscreen` when a fullscreen view is
//! drawn above its output's top layer and `leave_fullscreen` when that
//! ends, `toggle_menu` from the `wayfire-shell/toggle_menu` activator, and
//! can inhibit its output, which then draws black. Hotspots, on an edge or
//! corner or around a rectangle, send `enter` once the pointer or the
//! first touch point stayed in them for their timeout, `proximity_changed`
//! while it moves inside, and `leave` when it goes. `zwf_surface_v2`'s
//! `interactive_move` starts a move of its view.

use std::{
  collections::HashMap,
  os::fd::{AsFd, OwnedFd},
  time::{Duration, Instant},
};

use shingle_core::OutputId;
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{Event, ModifierBindingTracker, state::CompositorState};

use self::protocol::server::{
  zwf_hotspot_v2::ZwfHotspotV2,
  zwf_output_v2::{self, ZwfOutputV2},
  zwf_shell_manager_v2::{self, ZwfShellManagerV2},
  zwf_surface_v2::{self, ZwfSurfaceV2},
};

/// The generated code of protocols/wayfire-shell-unstable-v2.xml.
#[allow(missing_docs, clippy::all, dead_code, non_upper_case_globals)]
#[allow(non_camel_case_types, unused_imports)]
pub(crate) mod protocol {
  /// The compositor's side.
  pub(crate) mod server {
    use wayland_server;
    use wayland_server::protocol::*;

    pub(crate) mod __interfaces {
      use wayland_server::backend as wayland_backend;
      use wayland_server::protocol::__interfaces::*;
      wayland_scanner::generate_interfaces!(
        "protocols/wayfire-shell-unstable-v2.xml"
      );
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!(
      "protocols/wayfire-shell-unstable-v2.xml"
    );
  }

  /// The client's side, for the in-process test client.
  #[cfg(test)]
  pub(crate) mod client {
    use wayland_client;
    use wayland_client::protocol::*;

    pub(crate) mod __interfaces {
      use wayland_client::backend as wayland_backend;
      use wayland_client::protocol::__interfaces::*;
      wayland_scanner::generate_interfaces!(
        "protocols/wayfire-shell-unstable-v2.xml"
      );
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!(
      "protocols/wayfire-shell-unstable-v2.xml"
    );
  }
}

/// The version of `zwf_shell_manager_v2` Shingle offers, as Wayfire.
const VERSION: u32 = 3;

/// `zwf_output_v2.hotspot_edge` bits.
const EDGE_BOTTOM: u32 = 2;
const EDGE_RIGHT: u32 = 8;

/// A rectangle in layout coordinates, which may be empty: Wayfire's
/// `wf::geometry_t`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Area {
  pub(crate) x: i64,
  pub(crate) y: i64,
  pub(crate) width: i64,
  pub(crate) height: i64,
}

impl Area {
  fn of(rect: shingle_core::Rect) -> Self {
    Self {
      x: rect.x.into(),
      y: rect.y.into(),
      width: rect.width.into(),
      height: rect.height.into(),
    }
  }

  /// Whether `(x, y)` is inside, the right and bottom edges excluded.
  fn contains(&self, x: f64, y: f64) -> bool {
    x >= self.x as f64
      && x < (self.x + self.width) as f64
      && y >= self.y as f64
      && y < (self.y + self.height) as f64
  }

  /// The overlap of two rectangles, empty at the origin without one.
  fn intersection(&self, other: &Self) -> Self {
    let x = self.x.max(other.x);
    let y = self.y.max(other.y);
    let right = (self.x + self.width).min(other.x + other.width);
    let bottom = (self.y + self.height).min(other.y + other.height);
    if right <= x || bottom <= y {
      return Self::default();
    }
    Self {
      x,
      y,
      width: right - x,
      height: bottom - y,
    }
  }
}

/// The trigger of an edge or corner hotspot on an output at `output`, as
/// Wayfire's `calculate_trigger_geometry`: an edge is a line of no
/// thickness on its last row or column, two edges the point of their
/// corner, and any other mask the layout's origin.
pub(crate) fn edge_trigger(output: Area, edges: u32) -> Area {
  let right = output.x + output.width - 1;
  let bottom = output.y + output.height - 1;
  match edges.count_ones() {
    1 => match edges {
      1 => Area {
        height: 0,
        ..output
      },
      EDGE_BOTTOM => Area {
        y: bottom,
        height: 0,
        ..output
      },
      4 => Area { width: 0, ..output },
      _ => Area {
        x: right,
        width: 0,
        ..output
      },
    },
    2 => Area {
      x: if edges & EDGE_RIGHT != 0 {
        right
      } else {
        output.x
      },
      y: if edges & EDGE_BOTTOM != 0 {
        bottom
      } else {
        output.y
      },
      width: 0,
      height: 0,
    },
    _ => Area::default(),
  }
}

/// Where a hotspot triggers and how long the input must stay.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HotspotArea {
  /// The trigger rectangle, or line or point, in layout coordinates.
  pub(crate) trigger: Area,
  /// The trigger grown by the threshold and cut to its output.
  pub(crate) bounds: Area,
  pub(crate) threshold: u32,
  pub(crate) timeout: Duration,
}

impl HotspotArea {
  /// The hotspot of `trigger` on an output at `output`, as Wayfire's
  /// `calculate_hotspot_geometry`.
  pub(crate) fn new(
    output: Area,
    trigger: Area,
    threshold: u32,
    timeout_ms: u32,
  ) -> Self {
    let grow = i64::from(threshold);
    let expanded = Area {
      x: trigger.x - grow,
      y: trigger.y - grow,
      width: trigger.width + 2 * grow,
      height: trigger.height + 2 * grow,
    };
    Self {
      trigger,
      bounds: expanded.intersection(&output),
      threshold,
      timeout: Duration::from_millis(timeout_ms.into()),
    }
  }

  /// The distance in whole pixels from `(x, y)` to the trigger, at most
  /// the threshold: 0 on or inside it, Euclidean towards a corner.
  pub(crate) fn proximity(&self, x: f64, y: f64) -> u32 {
    let rect = &self.trigger;
    let (left, top) = (rect.x as f64, rect.y as f64);
    let (right, bottom) =
      ((rect.x + rect.width) as f64, (rect.y + rect.height) as f64);
    let dx = if x < left {
      left - x
    } else if x > right {
      x - right
    } else {
      0.0
    };
    let dy = if y < top {
      top - y
    } else if y > bottom {
      y - bottom
    } else {
      0.0
    };
    let distance = (dx * dx + dy * dy).sqrt().floor();
    if distance >= f64::from(self.threshold) {
      self.threshold
    } else {
      distance as u32
    }
  }

  /// Whether `(x, y)` is in the hotspot.
  pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
    self.bounds.contains(x, y) && self.proximity(x, y) <= self.threshold
  }
}

/// What a hotspot's state machine asks to send.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HotspotEvent {
  Enter,
  Leave,
  Proximity(u32),
}

/// One hotspot's state, as Wayfire's `wfs_hotspot`: a timer while the
/// input is inside and has not triggered it, and whether it triggered.
#[derive(Clone, Debug)]
pub(crate) struct HotspotState {
  /// `None` once its output is gone.
  pub(crate) area: Option<HotspotArea>,
  pub(crate) triggered: bool,
  pub(crate) deadline: Option<Instant>,
  /// Where the input was last seen inside.
  last: (f64, f64),
}

impl HotspotState {
  pub(crate) fn new(area: Option<HotspotArea>) -> Self {
    Self {
      area,
      triggered: false,
      deadline: None,
      last: (-1.0, -1.0),
    }
  }

  /// The input moved to `(x, y)` at `now`: inside, a triggered hotspot
  /// reports its proximity and an untriggered one starts its timer unless
  /// it runs; outside, a triggered hotspot leaves and the timer stops.
  pub(crate) fn input_at(
    &mut self,
    x: f64,
    y: f64,
    now: Instant,
  ) -> Vec<HotspotEvent> {
    let inside = self.area.as_ref().filter(|area| area.contains(x, y));
    if let Some(area) = inside {
      self.last = (x, y);
      if self.triggered {
        return vec![HotspotEvent::Proximity(area.proximity(x, y))];
      }
      if self.deadline.is_none() {
        self.deadline = Some(now + area.timeout);
      }
      return Vec::new();
    }
    self.deadline = None;
    if self.triggered {
      self.triggered = false;
      return vec![HotspotEvent::Leave];
    }
    Vec::new()
  }

  /// Triggers the hotspot if its timer ran out by `now`.
  pub(crate) fn tick(&mut self, now: Instant) -> Vec<HotspotEvent> {
    match (self.deadline, &self.area) {
      (Some(deadline), Some(area)) if deadline <= now => {
        self.deadline = None;
        self.triggered = true;
        let (x, y) = self.last;
        vec![
          HotspotEvent::Enter,
          HotspotEvent::Proximity(area.proximity(x, y)),
        ]
      }
      _ => Vec::new(),
    }
  }

  /// Its output went: it never triggers again, and leaves if it was
  /// triggered, as Wayfire's `on_output_removed`.
  pub(crate) fn output_gone(&mut self) -> Vec<HotspotEvent> {
    self.area = None;
    self.input_at(-1.0, -1.0, Instant::now())
  }
}

struct ShellOutput {
  resource: ZwfOutputV2,
  /// `None` once the output is gone; the object then does nothing.
  output: Option<OutputId>,
  /// How often the client inhibited the output without ending it.
  inhibits: u32,
  /// Whether `enter_fullscreen` was the last fullscreen event sent.
  fullscreen: bool,
}

struct Hotspot {
  resource: ZwfHotspotV2,
  output: Option<OutputId>,
  state: HotspotState,
}

struct ShellSurface {
  surface: ObjectId,
}

/// The shell objects, and what drives them.
pub(crate) struct WayfireShell {
  outputs: HashMap<ObjectId, ShellOutput>,
  hotspots: HashMap<ObjectId, Hotspot>,
  surfaces: HashMap<ObjectId, ShellSurface>,
  /// Where the pointer or first touch point moved since the last turn.
  moved: Option<(f64, f64)>,
  /// A duplicate of the one-shot timerfd that wakes the event loop at the
  /// next hotspot deadline.
  pub(crate) timer: Option<OwnedFd>,
  /// The `wayfire-shell/toggle_menu` activator.
  pub(crate) toggle_menu: shingle_config::Binding,
  /// `input/modifier_binding_timeout` in milliseconds.
  pub(crate) modifier_timeout: u32,
  /// Modifier-alone bindings of STIPC keys, for compositors without a
  /// frontend.
  pub(crate) modifiers: ModifierBindingTracker,
}

impl WayfireShell {
  /// No shell objects yet, with the menu key `toggle_menu` and the
  /// modifier timeout `modifier_timeout` in milliseconds.
  pub(crate) fn new(
    toggle_menu: shingle_config::Binding,
    modifier_timeout: u32,
  ) -> Self {
    Self {
      outputs: HashMap::new(),
      hotspots: HashMap::new(),
      surfaces: HashMap::new(),
      moved: None,
      timer: None,
      toggle_menu,
      modifier_timeout,
      modifiers: ModifierBindingTracker::default(),
    }
  }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwfShellManagerV2, ()>(VERSION, ());
}

/// Records that the pointer or the first touch point is at `(x, y)` now;
/// the hotspots look at it at the end of the turn, as Wayfire's idle call
/// after each motion.
pub(crate) fn input_moved(state: &mut CompositorState, x: f64, y: f64) {
  state.wayfire_shell.moved = Some((x, y));
}

/// The layout rectangle of output `id`.
fn output_area(state: &CompositorState, id: &OutputId) -> Option<Area> {
  state.outputs.get(id).map(|output| Area::of(output.rect))
}

/// Sends `events` to `resource`; `proximity_changed` only from version 3.
fn send(resource: &ZwfHotspotV2, events: Vec<HotspotEvent>) {
  for event in events {
    match event {
      HotspotEvent::Enter => resource.enter(),
      HotspotEvent::Leave => resource.leave(),
      HotspotEvent::Proximity(proximity) => {
        if resource.version() >= 3 {
          resource.proximity_changed(proximity);
        }
      }
    }
  }
}

/// Runs the hotspots and fullscreen events once a turn, at `now`: the
/// latest input position, timers that ran out, the timer for the next
/// deadline, and each output's fullscreen state.
pub(crate) fn sync(state: &mut CompositorState, now: Instant) {
  let moved = state.wayfire_shell.moved.take();
  for hotspot in state.wayfire_shell.hotspots.values_mut() {
    let mut events = Vec::new();
    if let Some((x, y)) = moved {
      events.extend(hotspot.state.input_at(x, y, now));
    }
    events.extend(hotspot.state.tick(now));
    send(&hotspot.resource, events);
  }
  arm_timer(state, now);
  sync_fullscreen(state);
}

/// Arms the timerfd for the earliest hotspot deadline, or disarms it.
fn arm_timer(state: &CompositorState, now: Instant) {
  use rustix::time::{Itimerspec, TimerfdTimerFlags, Timespec};
  let Some(timer) = &state.wayfire_shell.timer else {
    return;
  };
  let next = state
    .wayfire_shell
    .hotspots
    .values()
    .filter_map(|hotspot| hotspot.state.deadline)
    .min();
  let zero = Timespec {
    tv_sec: 0,
    tv_nsec: 0,
  };
  let value = match next {
    // A deadline that passed fires at once; zero would disarm.
    Some(deadline) => {
      let wait = deadline
        .saturating_duration_since(now)
        .max(Duration::from_nanos(1));
      Timespec {
        tv_sec: wait.as_secs() as _,
        tv_nsec: wait.subsec_nanos().into(),
      }
    }
    None => zero,
  };
  let _ = rustix::time::timerfd_settime(
    timer.as_fd(),
    TimerfdTimerFlags::empty(),
    &Itimerspec {
      it_interval: zero,
      it_value: value,
    },
  );
}

/// Sends `enter_fullscreen` or `leave_fullscreen` to each shell output
/// whose output started or stopped drawing a fullscreen view above its top
/// layer.
fn sync_fullscreen(state: &mut CompositorState) {
  let mut promoted = HashMap::new();
  let outputs = state
    .wayfire_shell
    .outputs
    .values()
    .filter_map(|output| output.output.clone())
    .collect::<Vec<_>>();
  for output in outputs {
    promoted
      .entry(output.clone())
      .or_insert_with(|| state.promoted_fullscreen(&output).is_some());
  }
  for shell_output in state.wayfire_shell.outputs.values_mut() {
    let fullscreen = shell_output
      .output
      .as_ref()
      .and_then(|output| promoted.get(output))
      .copied()
      .unwrap_or(false);
    if fullscreen != shell_output.fullscreen {
      shell_output.fullscreen = fullscreen;
      if fullscreen {
        shell_output.resource.enter_fullscreen();
      } else {
        shell_output.resource.leave_fullscreen();
      }
    }
  }
}

/// Output `id` is gone, or renamed, which replaces its `wl_output`: its
/// shell outputs do nothing more, ending their inhibits and any fullscreen
/// state, and its hotspots leave.
pub(crate) fn output_removed(state: &mut CompositorState, id: &OutputId) {
  let shell = &mut state.wayfire_shell;
  let mut inhibited = false;
  for output in shell.outputs.values_mut() {
    if output.output.as_ref() == Some(id) {
      output.output = None;
      inhibited |= output.inhibits > 0;
      output.inhibits = 0;
      if output.fullscreen {
        output.fullscreen = false;
        output.resource.leave_fullscreen();
      }
    }
  }
  for hotspot in shell.hotspots.values_mut() {
    if hotspot.output.as_ref() == Some(id) {
      hotspot.output = None;
      let events = hotspot.state.output_gone();
      send(&hotspot.resource, events);
    }
  }
  if inhibited {
    inhibit_changed(state, id);
  }
}

/// Whether a shell client inhibits output `id`, which then draws black.
pub(crate) fn inhibited(state: &CompositorState, id: &OutputId) -> bool {
  state
    .wayfire_shell
    .outputs
    .values()
    .any(|output| output.output.as_ref() == Some(id) && output.inhibits > 0)
}

fn inhibit_changed(state: &mut CompositorState, id: &OutputId) {
  state.events.push(Event::OutputInhibitChanged {
    output: crate::OutputHandle::from(id),
    inhibited: inhibited(state, id),
  });
}

/// Sends `toggle_menu` to every shell output of `output` whose client
/// bound version 2 or later, as the `wayfire-shell/toggle_menu`
/// activator does.
pub(crate) fn toggle_menu(state: &CompositorState, output: &OutputId) {
  for shell_output in state.wayfire_shell.outputs.values() {
    if shell_output.output.as_ref() == Some(output)
      && shell_output.resource.version() >= 2
    {
      shell_output.resource.toggle_menu();
    }
  }
}

/// Follows a STIPC key of `keyboard` for modifier-alone bindings, in a
/// compositor without a frontend, and toggles the active output's menu
/// when `toggle_menu` triggers. Called before the key is delivered, so
/// that the keyboard still holds the keys pressed before it.
pub(crate) fn stipc_key(
  state: &mut CompositorState,
  keyboard: crate::InputDeviceId,
  code: u32,
  modifiers: shingle_config::Modifiers,
  pressed: bool,
) {
  let now = Instant::now();
  if pressed {
    let held = crate::input_device::held_keys(state, keyboard);
    let only_modifiers = held
      .iter()
      .filter(|held| **held != code)
      .all(|held| crate::modifier_binding::modifier_of_key(*held).is_some())
      && !crate::input_device::any_button_or_touch(state);
    state
      .wayfire_shell
      .modifiers
      .press(code, only_modifiers, now);
    return;
  }
  let timeout = state.wayfire_shell.modifier_timeout;
  let Some(released) = state
    .wayfire_shell
    .modifiers
    .release(code, modifiers, timeout, now)
  else {
    return;
  };
  let unlocked = state.idle.lock_state() == shingle_idle::LockState::Unlocked;
  if unlocked
    && !crate::shortcuts::active(state)
    && state
      .wayfire_shell
      .toggle_menu
      .matches_modifiers_alone(released)
  {
    let output = state.active_output.clone();
    toggle_menu(state, &output);
  }
}

impl GlobalDispatch<ZwfShellManagerV2, ()> for CompositorState {
  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwfShellManagerV2>,
    _data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwfShellManagerV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwfShellManagerV2,
    request: zwf_shell_manager_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwf_shell_manager_v2::Request::GetWfOutput { output, id } => {
        let resource = data_init.init(id, ());
        // An output that is gone gives an object that does nothing, where
        // Wayfire creates none.
        let output = state
          .outputs
          .of_resource(&output)
          .map(|output| output.id.clone());
        // The protocol pairs every leave with an enter: a panel created
        // while a view is fullscreen hears of it at once.
        let fullscreen = output
          .as_ref()
          .is_some_and(|output| state.promoted_fullscreen(output).is_some());
        if fullscreen {
          resource.enter_fullscreen();
        }
        state.wayfire_shell.outputs.insert(
          resource.id(),
          ShellOutput {
            resource,
            output,
            inhibits: 0,
            fullscreen,
          },
        );
      }
      zwf_shell_manager_v2::Request::GetWfSurface { surface, id } => {
        let resource = data_init.init(id, ());
        state.wayfire_shell.surfaces.insert(
          resource.id(),
          ShellSurface {
            surface: surface.id(),
          },
        );
      }
    }
  }
}

impl Dispatch<ZwfOutputV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwfOutputV2,
    request: zwf_output_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let output = state
      .wayfire_shell
      .outputs
      .get(&resource.id())
      .and_then(|shell_output| shell_output.output.clone());
    let (trigger, threshold, timeout, id) = match request {
      zwf_output_v2::Request::InhibitOutput => {
        let Some(output) = output else {
          return;
        };
        let was = inhibited(state, &output);
        if let Some(shell_output) =
          state.wayfire_shell.outputs.get_mut(&resource.id())
        {
          shell_output.inhibits = shell_output.inhibits.saturating_add(1);
        }
        if !was {
          inhibit_changed(state, &output);
        }
        return;
      }
      zwf_output_v2::Request::InhibitOutputDone => {
        let Some(output) = output else {
          return;
        };
        // More `inhibit_output_done` than `inhibit_output` are ignored.
        let was = inhibited(state, &output);
        if let Some(shell_output) =
          state.wayfire_shell.outputs.get_mut(&resource.id())
        {
          shell_output.inhibits = shell_output.inhibits.saturating_sub(1);
        }
        if was && !inhibited(state, &output) {
          inhibit_changed(state, &output);
        }
        return;
      }
      zwf_output_v2::Request::CreateHotspot {
        hotspot,
        threshold,
        timeout,
        id,
      } => {
        let trigger = output
          .as_ref()
          .and_then(|output| output_area(state, output))
          .map(|area| edge_trigger(area, hotspot));
        (trigger, threshold, timeout, id)
      }
      zwf_output_v2::Request::CreateCustomHotspot {
        x,
        y,
        width,
        height,
        threshold,
        timeout,
        id,
      } => {
        let trigger = output
          .as_ref()
          .and_then(|output| output_area(state, output))
          .map(|area| Area {
            x: area.x + i64::from(x),
            y: area.y + i64::from(y),
            width: width.into(),
            height: height.into(),
          });
        (trigger, threshold, timeout, id)
      }
    };
    let resource = data_init.init(id, ());
    let area = output
      .as_ref()
      .and_then(|output| output_area(state, output))
      .zip(trigger)
      .map(|(bounds, trigger)| {
        HotspotArea::new(bounds, trigger, threshold, timeout)
      });
    state.wayfire_shell.hotspots.insert(
      resource.id(),
      Hotspot {
        resource,
        output: output.filter(|_| area.is_some()),
        state: HotspotState::new(area),
      },
    );
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwfOutputV2,
    _data: &(),
  ) {
    let Some(removed) = state.wayfire_shell.outputs.remove(&resource.id())
    else {
      return;
    };
    // A client that goes takes its inhibits along, as Wayfire 0.9 did.
    if let Some(output) = removed.output.filter(|_| removed.inhibits > 0)
      && !inhibited(state, &output)
    {
      inhibit_changed(state, &output);
    }
  }
}

impl Dispatch<ZwfHotspotV2, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZwfHotspotV2,
    _request: <ZwfHotspotV2 as Resource>::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwfHotspotV2,
    _data: &(),
  ) {
    state.wayfire_shell.hotspots.remove(&resource.id());
  }
}

impl Dispatch<ZwfSurfaceV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwfSurfaceV2,
    request: zwf_surface_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let zwf_surface_v2::Request::InteractiveMove = request;
    {
      // Wayfire logs that it no longer supports this; Shingle moves the
      // surface's view as `xdg_toplevel.move` would.
      let window = state
        .wayfire_shell
        .surfaces
        .get(&resource.id())
        .and_then(|shell_surface| state.surfaces.get(&shell_surface.surface))
        .and_then(|surface| surface.role.toplevel())
        .and_then(|toplevel| toplevel.window());
      if let Some(window) = window {
        state.events.push(Event::BeginMove { window });
      }
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwfSurfaceV2,
    _data: &(),
  ) {
    state.wayfire_shell.surfaces.remove(&resource.id());
  }
}

#[cfg(test)]
mod tests;
