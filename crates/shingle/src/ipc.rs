use std::{
  collections::HashSet,
  io::{Read, Write},
  os::unix::net::{UnixListener, UnixStream},
  path::{Path, PathBuf},
};

use serde_json::{Value, json};
use shingle_core::{Direction, Rect, StackingMode, WindowId, WindowPresence};
use wayland_server::Resource;

use crate::{
  decoration::Mode as DecorationMode,
  input,
  input_device::{self, InputDeviceId, InputDeviceKind, stipc_device},
  shell,
  state::CompositorState,
};

const MAX_MESSAGE: usize = 1 << 20;

pub(crate) struct IpcServer {
  listener: UnixListener,
  path: PathBuf,
  clients: Vec<IpcClient>,
  next_client: u64,
  next_binding: u64,
}

struct IpcClient {
  id: u64,
  stream: UnixStream,
  input: Vec<u8>,
  output: Vec<u8>,
  subscriptions: Option<HashSet<String>>,
  bindings: HashSet<u64>,
  closed: bool,
}

impl IpcServer {
  pub(crate) fn new(path: &Path) -> std::io::Result<Self> {
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(Self {
      listener,
      path: path.into(),
      clients: Vec::new(),
      next_client: 1,
      next_binding: 1,
    })
  }

  /// Accepts and serves pending clients without blocking the compositor.
  pub(crate) fn poll(&mut self, state: &mut CompositorState) {
    loop {
      match self.listener.accept() {
        Ok((stream, _)) => {
          let _ = stream.set_nonblocking(true);
          let id = self.next_client;
          self.next_client = id + 1;
          self.clients.push(IpcClient {
            id,
            stream,
            input: Vec::new(),
            output: Vec::new(),
            subscriptions: None,
            bindings: HashSet::new(),
            closed: false,
          });
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          break;
        }
        Err(_) => break,
      }
    }
    for client in &mut self.clients {
      client.flush();
      client.read(state, &mut self.next_binding);
      client.flush();
    }
    self.publish(state);
    self.cleanup_clients(state);
    self.clients.retain(|client| !client.closed);
  }

  fn cleanup_clients(&mut self, state: &mut CompositorState) {
    for client in self.clients.iter().filter(|client| client.closed) {
      for binding in &client.bindings {
        if state
          .runtime_bindings
          .get(binding)
          .is_some_and(|binding| binding.client_id == Some(client.id))
        {
          state.runtime_bindings.remove(binding);
        }
      }
      state.pre_map_clients.remove(&client.id);
      let pending = state
        .pending_maps
        .iter()
        .filter(|(_, pending)| pending.waiting.contains(&client.id))
        .map(|(window, _)| *window)
        .collect::<Vec<_>>();
      for window in pending {
        let _ = shell::unblock_map(state, window, client.id);
      }
    }
  }

  fn publish(&mut self, state: &mut CompositorState) {
    let events = std::mem::take(&mut state.ipc_events);
    for event in events {
      let Some(name) = event.get("event").and_then(Value::as_str) else {
        continue;
      };
      let target = event.get("client-id").and_then(Value::as_u64);
      for client in &mut self.clients {
        let subscribed = client
          .subscriptions
          .as_ref()
          .is_some_and(|events| events.contains(name));
        if target.is_some_and(|id| id == client.id)
          || (target.is_none() && subscribed)
        {
          let mut event = event.clone();
          if let Some(object) = event.as_object_mut() {
            object.remove("client-id");
          }
          client.queue(event);
        }
      }
    }
    for client in &mut self.clients {
      client.flush();
    }
  }
}

impl Drop for IpcServer {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(&self.path);
  }
}

impl IpcClient {
  fn flush(&mut self) {
    while !self.output.is_empty() {
      match self.stream.write(&self.output) {
        Ok(0) => {
          self.closed = true;
          return;
        }
        Ok(written) => {
          self.output.drain(..written);
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          return;
        }
        Err(_) => {
          self.closed = true;
          return;
        }
      }
    }
  }

  fn read(&mut self, state: &mut CompositorState, next_binding: &mut u64) {
    let mut buffer = [0_u8; 8192];
    loop {
      match self.stream.read(&mut buffer) {
        Ok(0) => {
          self.closed = true;
          break;
        }
        Ok(read) => {
          self.input.extend_from_slice(&buffer[..read]);
          if self.input.len() > MAX_MESSAGE + 4 {
            self.closed = true;
            return;
          }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
          break;
        }
        Err(_) => {
          self.closed = true;
          break;
        }
      }
    }
    loop {
      if self.input.len() < 4 {
        return;
      }
      let length =
        u32::from_le_bytes(self.input[..4].try_into().unwrap()) as usize;
      if length > MAX_MESSAGE {
        self.closed = true;
        return;
      }
      if self.input.len() < length + 4 {
        return;
      }
      let message = self.input[4..length + 4].to_vec();
      self.input.drain(..length + 4);
      let response = match serde_json::from_slice(&message) {
        Ok(message) => self.dispatch(state, &message, next_binding),
        Err(error) => json_error(&format!("invalid JSON: {error}")),
      };
      self.queue(response);
      if self.output.len() > 2 * MAX_MESSAGE {
        self.closed = true;
        return;
      }
    }
  }

  fn dispatch(
    &mut self,
    state: &mut CompositorState,
    message: &Value,
    next_binding: &mut u64,
  ) -> Value {
    let method = message.get("method").and_then(Value::as_str);
    let data = message.get("data").unwrap_or(&Value::Null);
    match method {
      Some("window-rules/events/watch") => self.watch(state, data),
      Some("window-rules/unblock-map") => self.unblock_map(state, data),
      Some("command/register-binding") => {
        self.register_binding(state, data, next_binding)
      }
      Some("command/unregister-binding") => {
        self.unregister_binding(state, data)
      }
      Some("command/clear-bindings") => {
        state
          .runtime_bindings
          .retain(|_, binding| binding.client_id.is_some());
        json_ok()
      }
      _ => respond(state, message),
    }
  }

  fn unblock_map(&self, state: &mut CompositorState, data: &Value) -> Value {
    let window = match find_view(state, data) {
      Ok(window) => window,
      Err(error) => return error,
    };
    match shell::unblock_map(state, window, self.id) {
      Ok(()) => json_ok(),
      Err(error) => json_error(error),
    }
  }

  fn register_binding(
    &mut self,
    state: &mut CompositorState,
    data: &Value,
    next_binding: &mut u64,
  ) -> Value {
    let call_method = match data.get("call-method") {
      Some(value) => match value.as_str() {
        Some(method) if !method.is_empty() => {
          if data.get("call-data").is_none() {
            return json_error("call-method requires call-data");
          }
          Some(method.to_owned())
        }
        _ => return json_error("invalid call-method"),
      },
      None => None,
    };
    let call_data = data.get("call-data").cloned();
    if call_method.is_some() && data.get("command").is_some() {
      return json_error("command and call-method are mutually exclusive");
    }
    let Some(binding) = data.get("binding").and_then(Value::as_str) else {
      return json_error("missing or invalid binding");
    };
    let Some(binding) = normalize_runtime_binding(binding) else {
      return json_error("invalid binding");
    };
    let command = match data.get("command") {
      Some(value) => match value.as_str() {
        Some(command) if !command.trim().is_empty() => Some(command.to_owned()),
        _ => return json_error("invalid command"),
      },
      None => None,
    };
    let mode = match data.get("mode").and_then(Value::as_str) {
      None => crate::RuntimeBindingMode::Normal,
      Some("repeat") => crate::RuntimeBindingMode::Repeat,
      Some("release") => crate::RuntimeBindingMode::Release,
      _ => return json_error("invalid mode"),
    };
    let exec_always = data
      .get("exec-always")
      .and_then(Value::as_bool)
      .unwrap_or(false);
    let id = *next_binding;
    *next_binding = next_binding.saturating_add(1);
    let client_id =
      (command.is_none() && call_method.is_none()).then_some(self.id);
    state.runtime_bindings.insert(
      id,
      crate::RuntimeBinding {
        id,
        binding,
        command,
        call_method,
        call_data,
        mode,
        exec_always,
        client_id,
      },
    );
    if client_id.is_some() {
      self.bindings.insert(id);
    }
    json!({"result": "ok", "binding-id": id})
  }

  fn unregister_binding(
    &mut self,
    state: &mut CompositorState,
    data: &Value,
  ) -> Value {
    let Some(id) = data.get("binding-id").and_then(Value::as_u64) else {
      return json_error("missing or invalid binding-id");
    };
    state.runtime_bindings.remove(&id);
    self.bindings.remove(&id);
    json_ok()
  }

  fn watch(&mut self, state: &mut CompositorState, data: &Value) -> Value {
    if self.subscriptions.is_some() {
      return json_error("client is already watching events");
    }
    let mut events = HashSet::new();
    match data.get("events") {
      Some(Value::Array(requested)) => {
        for event in requested {
          let Some(event) = event.as_str() else {
            return json_error("event list contains non-string entries");
          };
          if !known_event(event) && !event.ends_with('#') {
            return json_error("unknown event");
          }
          events.insert(event.into());
        }
      }
      Some(_) => return json_error("event list is not an array"),
      None => {
        for event in KNOWN_EVENTS {
          if *event != "view-pre-map" {
            events.insert((*event).into());
          }
        }
      }
    }
    if events.contains("view-pre-map") {
      state.pre_map_clients.insert(self.id);
    }
    self.subscriptions = Some(events);
    json_ok()
  }

  fn queue(&mut self, value: Value) {
    let message = value.to_string();
    self
      .output
      .extend_from_slice(&(message.len() as u32).to_le_bytes());
    self.output.extend_from_slice(message.as_bytes());
  }
}

const SUPPORTED_METHODS: &[&str] = &[
  "command/clear-bindings",
  "command/register-binding",
  "command/unregister-binding",
  "expo/toggle",
  "input/configure-device",
  "input/list-devices",
  "invert/toggle",
  "list-methods",
  "scale/toggle",
  "scale/toggle_all",
  "simple-tile/get-layout",
  "simple-tile/set-layout",
  "simple-tile/set-show-maximized",
  "stipc/feed_button",
  "stipc/feed_key",
  "stipc/get_display",
  "stipc/get_xwayland_display",
  "stipc/get_xwayland_pid",
  "stipc/layout_views",
  "stipc/move_cursor",
  "stipc/ping",
  "stipc/run",
  "stipc/touch",
  "stipc/touch_release",
  "vswitch/binding_down",
  "vswitch/binding_last",
  "vswitch/binding_left",
  "vswitch/binding_right",
  "vswitch/binding_up",
  "vswitch/send-view",
  "vswitch/send_win_down",
  "vswitch/send_win_last",
  "vswitch/send_win_left",
  "vswitch/send_win_right",
  "vswitch/send_win_up",
  "vswitch/set-workspace",
  "vswitch/with_win_down",
  "vswitch/with_win_last",
  "vswitch/with_win_left",
  "vswitch/with_win_right",
  "vswitch/with_win_up",
  "wayfire/get-config-option",
  "wayfire/get-keyboard-state",
  "wayfire/list-config-options",
  "wayfire/set-config-options",
  "wayfire/set-keyboard-state",
  "wf/alpha/get-view-alpha",
  "wf/alpha/set-view-alpha",
  "window-rules/close-view",
  "window-rules/configure-view",
  "window-rules/events/watch",
  "window-rules/focus-view",
  "window-rules/get-cursor-position",
  "window-rules/get-focused-output",
  "window-rules/get-focused-view",
  "window-rules/get-view-property",
  "window-rules/get_cursor_position",
  "window-rules/list-outputs",
  "window-rules/list-views",
  "window-rules/list-wsets",
  "window-rules/output-info",
  "window-rules/set-view-property",
  "window-rules/unblock-map",
  "window-rules/view-info",
  "window-rules/wset-info",
  "wm-actions/bring-to-front",
  "wm-actions/minimize",
  "wm-actions/send-to-back",
  "wm-actions/set-always-on-bottom",
  "wm-actions/set-always-on-top",
  "wm-actions/set-fullscreen",
  "wm-actions/set-minimized",
  "wm-actions/set-sticky",
  "wm-actions/toggle_always_on_bottom",
  "wm-actions/toggle_always_on_top",
  "wm-actions/toggle_fullscreen",
  "wm-actions/toggle_maximize",
  "wm-actions/toggle_showdesktop",
  "wm-actions/toggle_sticky",
];

const KNOWN_EVENTS: &[&str] = &[
  "command-binding",
  "view-pre-map",
  "view-mapped",
  "view-unmapped",
  "view-focused",
  "view-title-changed",
  "view-app-id-changed",
  "view-geometry-changed",
  "view-workspace-changed",
  "plugin-activation-state-changed",
  "keyboard-modifier-state-changed",
  "view-minimized",
  "view-fullscreen",
  "view-sticky",
  "view-tiled",
  "view-always-on-top",
  "view-always-on-bottom",
  "wset-workspace-changed",
  "output-layout-changed",
  "xwayland-ready",
];

fn known_event(event: &str) -> bool {
  KNOWN_EVENTS.contains(&event)
}

#[derive(Clone, Debug)]
pub(crate) enum ViewProperty {
  Bool(bool),
  Signed(i64),
  Unsigned(u64),
  String(String),
  Float(f64),
}

impl ViewProperty {
  fn from_json(value: &Value) -> Option<Self> {
    match value {
      Value::Bool(value) => Some(Self::Bool(*value)),
      Value::String(value) => Some(Self::String(value.clone())),
      Value::Number(value) if value.is_i64() => {
        Some(Self::Signed(value.as_i64().unwrap()))
      }
      Value::Number(value) if value.is_u64() => {
        Some(Self::Unsigned(value.as_u64().unwrap()))
      }
      Value::Number(value) => value.as_f64().map(Self::Float),
      _ => None,
    }
  }

  fn to_json(&self) -> Value {
    match self {
      Self::Bool(value) => json!(value),
      Self::Signed(value) => json!(value),
      Self::Unsigned(value) => json!(value),
      Self::String(value) => json!(value),
      Self::Float(value) => json!(value),
    }
  }
}

fn property_name(data: &Value) -> Result<&str, Value> {
  data
    .get("property")
    .and_then(Value::as_str)
    .ok_or_else(|| json_error("missing or invalid property name"))
}

fn get_view_property(state: &CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let property = match property_name(data) {
    Ok(property) => property,
    Err(error) => return error,
  };
  let Some(value) = state
    .view_properties
    .get(&window)
    .and_then(|properties| properties.get(property))
  else {
    return json_error("property not found on given view");
  };
  json!({"result": "ok", "value": value.to_json()})
}

fn set_view_property(state: &mut CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let property = match property_name(data) {
    Ok(property) => property.to_owned(),
    Err(error) => return error,
  };
  let Some(raw) = data.get("value") else {
    return json_error("missing value field");
  };
  let Some(value) = ViewProperty::from_json(raw) else {
    return json_error("unsupported value type");
  };
  let properties = state.view_properties.entry(window).or_default();
  if properties.get(&property).is_some_and(|old| {
    std::mem::discriminant(old) != std::mem::discriminant(&value)
  }) {
    return json_error("property already exists with a different type");
  }
  properties.insert(property, value);
  json_ok()
}

fn event_view(state: &CompositorState, window: WindowId) -> Value {
  let view = view_json(state, window);
  if view.is_null() {
    json!({"id": window.0})
  } else {
    view
  }
}

pub(crate) fn view_event(
  state: &CompositorState,
  name: &str,
  window: WindowId,
) -> Value {
  json!({"event": name, "view": event_view(state, window)})
}

/// Copies renderer events into the independent IPC outbox.
pub(crate) fn record_events(
  state: &mut CompositorState,
  events: &[crate::Event],
) {
  use crate::Event;
  for event in events {
    let message = match event {
      Event::WindowCreated { window, .. } => {
        view_event(state, "view-mapped", *window)
      }
      Event::WindowDestroyed { window, .. } => {
        view_event(state, "view-unmapped", *window)
      }
      Event::FocusChanged { window } => {
        let view =
          window.map_or(Value::Null, |window| event_view(state, window));
        json!({
            "event": "view-focused",
            "view": view,
        })
      }
      Event::TitleChanged { window, .. } => {
        view_event(state, "view-title-changed", *window)
      }
      Event::AppIdChanged { window, .. } => {
        view_event(state, "view-app-id-changed", *window)
      }
      Event::GeometryChanged {
        window,
        old_geometry,
      } => json!({
          "event": "view-geometry-changed",
          "old-geometry": geometry(*old_geometry),
          "view": event_view(state, *window),
      }),
      Event::WindowMoved {
        window,
        from,
        workspace,
      } => json!({
          "event": "view-workspace-changed",
          "from": {
              "x": from.saturating_sub(1) % state.workspace_width,
              "y": from.saturating_sub(1) / state.workspace_width,
          },
          "to": {
              "x": workspace.saturating_sub(1) % state.workspace_width,
              "y": workspace.saturating_sub(1) / state.workspace_width,
          },
          "view": event_view(state, *window),
      }),
      Event::MinimizedChanged { window, .. } => {
        view_event(state, "view-minimized", *window)
      }
      Event::FullscreenChanged { window, .. } => {
        view_event(state, "view-fullscreen", *window)
      }
      Event::StickyChanged { window, .. } => {
        view_event(state, "view-sticky", *window)
      }
      Event::TilingChanged {
        window,
        old_edges,
        new_edges,
        ..
      } => json!({
          "event": "view-tiled",
          "old-edges": old_edges,
          "new-edges": new_edges,
          "view": event_view(state, *window),
      }),
      Event::StackingChanged {
        window,
        previous,
        current,
      } => {
        let event = if *current == StackingMode::AlwaysOnBottom
          || (*current == StackingMode::Normal
            && *previous == StackingMode::AlwaysOnBottom)
        {
          "view-always-on-bottom"
        } else {
          "view-always-on-top"
        };
        view_event(state, event, *window)
      }
      Event::WorkspaceChanged { .. } => json!({
          "event": "wset-workspace-changed",
          "wset": output_json(state),
      }),
      Event::OutputChanged { .. } => json!({
          "event": "output-layout-changed",
          "configuration": [output_json(state)],
      }),
      _ => continue,
    };
    state.ipc_events.push(message);
  }
}

fn json_ok() -> Value {
  json!({"result": "ok"})
}

fn json_error(message: &str) -> Value {
  json!({"error": message})
}

fn geometry(rect: Rect) -> Value {
  json!({"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height})
}

fn wset_json(state: &CompositorState) -> Value {
  json!({
      "index": 1,
      "name": state.output_info.name,
      "output-id": 1,
      "output-name": state.output_info.name,
      "workspace": output_json(state)["workspace"].clone(),
  })
}

fn output_json(state: &CompositorState) -> Value {
  let workspace = state
    .core
    .current_workspace(&state.output_id)
    .unwrap_or_default();
  let workarea = state
    .core
    .output_workarea(&state.output_id)
    .unwrap_or(state.output);
  json!({
      "id": 1,
      "name": state.output_info.name,
      "geometry": geometry(state.output),
      "workarea": geometry(workarea),
      "wset-index": 1,
      "workspace": {
          "x": workspace.x,
          "y": workspace.y,
          "grid_width": state.workspace_width,
          "grid_height": state.workspace_height,
      },
  })
}

fn view_json(state: &CompositorState, window: WindowId) -> Value {
  let Ok(info) = state.core.window_info(window) else {
    return Value::Null;
  };
  let surface = state.surfaces.values().find(|surface| {
    surface
      .role
      .toplevel()
      .is_some_and(|toplevel| toplevel.window() == Some(window))
  });
  let (geometry_value, app_id, title, min_size, max_size, pid) = surface
    .map_or_else(
      || {
        (
          geometry(info.state.floating.unwrap_or_default()),
          info.identity.app_id.clone(),
          String::new(),
          (0, 0),
          (0, 0),
          None,
        )
      },
      |surface| {
        let role = surface
          .role
          .toplevel()
          .expect("the surface was found as a toplevel");
        (
          geometry(
            shell::surface_geometry(state, &Resource::id(&surface.resource))
              .unwrap_or_default(),
          ),
          role.app_id.clone(),
          role.title.clone(),
          role.min_size,
          role.max_size,
          role.pid,
        )
      },
    );
  let last_focus = state.focus_recency.get(&window).copied().unwrap_or(0);
  let mapped = info.presence == WindowPresence::Mapped;
  json!({
      "id": window.0,
      "pid": pid.map_or(-1, i64::from),
      "title": title,
      "app-id": app_id,
      "base-geometry": geometry_value,
      "parent": info.parent.map_or(-1, |parent| parent.0 as i64),
      "geometry": geometry_value,
      "bbox": geometry_value,
      "output-id": 1,
      "output-name": info.location.output.as_str(),
      "last-focus-timestamp": last_focus,
      "role": "toplevel",
      "mapped": mapped,
      "layer": "workspace",
      "tiled-edges": shell::tiled_edges(state, window),
      "fullscreen": info.state.fullscreen,
      "minimized": info.state.minimized,
      "activated": input::focused_window(state) == Some(window),
      "sticky": info.state.sticky,
      "wset-index": 1,
      "min-size": {"width": min_size.0, "height": min_size.1},
      "max-size": {"width": max_size.0, "height": max_size.1},
      "focusable": mapped && !info.state.minimized,
      "type": "toplevel",
      "always-on-top": info.state.stacking == StackingMode::AlwaysOnTop,
      "always-on-bottom": info.state.stacking == StackingMode::AlwaysOnBottom,
  })
}

fn request_id(data: &Value) -> Result<WindowId, Value> {
  ["id", "view-id", "view_id"]
    .into_iter()
    .find_map(|key| data.get(key).and_then(Value::as_u64))
    .map(WindowId)
    .ok_or_else(|| json_error("missing or invalid view id"))
}

fn find_view_id(
  state: &CompositorState,
  window: WindowId,
) -> Result<WindowId, Value> {
  state
    .core
    .window_info(window)
    .map(|_| window)
    .map_err(|_| json_error("view id not found"))
}

fn find_view(state: &CompositorState, data: &Value) -> Result<WindowId, Value> {
  find_view_id(state, request_id(data)?)
}

fn workspace_request(
  state: &CompositorState,
  data: &Value,
) -> Result<(shingle_core::WorkspaceId, usize), Value> {
  let Some(x) = data
    .get("x")
    .and_then(Value::as_u64)
    .and_then(|x| u32::try_from(x).ok())
    .filter(|x| *x < state.workspace_width)
  else {
    return Err(json_error("invalid workspace x coordinate"));
  };
  let Some(y) = data
    .get("y")
    .and_then(Value::as_u64)
    .and_then(|y| u32::try_from(y).ok())
    .filter(|y| *y < state.workspace_height)
  else {
    return Err(json_error("invalid workspace y coordinate"));
  };
  let index = y as usize * state.workspace_width as usize + x as usize;
  Ok((shingle_core::WorkspaceId::new(x, y), index))
}

fn require_mapped(
  state: &CompositorState,
  window: WindowId,
) -> Result<(), Value> {
  let info = state
    .core
    .window_info(window)
    .map_err(|_| json_error("view id not found"))?;
  if info.presence != WindowPresence::Mapped {
    return Err(json_error("view is not mapped"));
  }
  if info.location.output != state.output_id {
    return Err(json_error("view belongs to a different output"));
  }
  Ok(())
}

fn set_workspace(state: &mut CompositorState, data: &Value) -> Value {
  if data.get("output-id").and_then(Value::as_u64) != Some(1) {
    return json_error("invalid output id");
  }
  let (workspace, index) = match workspace_request(state, data) {
    Ok(target) => target,
    Err(error) => return error,
  };
  if let Some(window) = data.get("view-id").and_then(Value::as_u64) {
    let window = WindowId(window);
    if let Err(error) = find_view_id(state, window)
      .and_then(|window| require_mapped(state, window))
    {
      return error;
    }
    if state
      .core
      .window_info(window)
      .is_ok_and(|info| info.location.workspace != workspace)
      && let Err(error) = state.move_window_to_workspace(window, index)
    {
      return json_error(&error.to_string());
    }
  }
  match state.switch_workspace(index) {
    Ok(()) => json_ok(),
    Err(error) => json_error(&error.to_string()),
  }
}

fn send_view(state: &mut CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  if let Err(error) = require_mapped(state, window) {
    return error;
  }
  let (workspace, index) = match workspace_request(state, data) {
    Ok(target) => target,
    Err(error) => return error,
  };
  if state
    .core
    .window_info(window)
    .is_ok_and(|info| info.location.workspace == workspace)
  {
    return json_ok();
  }
  match state.move_window_to_workspace(window, index) {
    Ok(()) => json_ok(),
    Err(error) => json_error(&error.to_string()),
  }
}

fn layout_bounds(
  state: &CompositorState,
  node: &shingle_core::NodeSnapshot,
) -> Option<Rect> {
  use shingle_core::NodeSnapshot;
  match node {
    NodeSnapshot::Leaf {
      window,
      state: leaf,
    } if leaf.floating.is_none() => state
      .core
      .window_layout(*window)
      .ok()
      .map(|layout| layout.node),
    NodeSnapshot::Leaf { .. } => None,
    NodeSnapshot::Split(split) => split
      .children
      .iter()
      .filter_map(|child| layout_bounds(state, &child.node))
      .reduce(|left, right| {
        let x = left.x.min(right.x);
        let y = left.y.min(right.y);
        let right_edge = (left.x as i64 + left.width as i64)
          .max(right.x as i64 + right.width as i64);
        let bottom_edge = (left.y as i64 + left.height as i64)
          .max(right.y as i64 + right.height as i64);
        Rect::new(
          x,
          y,
          (right_edge - x as i64) as u32,
          (bottom_edge - y as i64) as u32,
        )
      }),
  }
}

/// Converts a persistent tree snapshot to Wayfire's simple-tile JSON schema.
fn layout_node(
  state: &CompositorState,
  node: &shingle_core::NodeSnapshot,
  fallback: Rect,
) -> Option<Value> {
  use shingle_core::{NodeSnapshot, SplitDirection};
  let rect = layout_bounds(state, node).unwrap_or(fallback);
  match node {
    NodeSnapshot::Leaf {
      window,
      state: leaf,
    } => {
      // A held view has no tile until its first map, as in Wayfire.
      if leaf.floating.is_some() || leaf.held {
        return None;
      }
      Some(json!({
          "view-id": window.0,
          "percent": 1.0,
          "geometry": geometry(rect),
      }))
    }
    NodeSnapshot::Split(split) => {
      let horizontal = split.direction == SplitDirection::Horizontal;
      let extent = if horizontal { rect.height } else { rect.width };
      let children = split
        .children
        .iter()
        .filter_map(|child| {
          let child_rect = layout_bounds(state, &child.node).unwrap_or(rect);
          let child_extent = if horizontal {
            child_rect.height
          } else {
            child_rect.width
          };
          let percent = if extent == 0 {
            0.0
          } else {
            child_extent as f64 / extent as f64
          };
          let mut value = layout_node(state, &child.node, child_rect)?;
          value["percent"] = json!(percent);
          Some(value)
        })
        .collect::<Vec<_>>();
      let key = if horizontal {
        "horizontal-split"
      } else {
        "vertical-split"
      };
      Some(json!({
          "percent": 1.0,
          "geometry": geometry(rect),
          key: children,
      }))
    }
  }
}

fn get_layout(state: &CompositorState, data: &Value) -> Value {
  if data.get("wset-index").and_then(Value::as_u64) != Some(1) {
    return json_error("wset-index not found");
  }
  let Some(workspace) = data.get("workspace") else {
    return json_error("missing workspace field");
  };
  let Some(x) = workspace
    .get("x")
    .and_then(Value::as_u64)
    .and_then(|x| u32::try_from(x).ok())
    .filter(|x| *x < state.workspace_width)
  else {
    return json_error("invalid workspace x coordinate");
  };
  let Some(y) = workspace
    .get("y")
    .and_then(Value::as_u64)
    .and_then(|y| u32::try_from(y).ok())
    .filter(|y| *y < state.workspace_height)
  else {
    return json_error("invalid workspace y coordinate");
  };
  let workspace = shingle_core::WorkspaceId::new(x, y);
  let snapshot = state.core.snapshot();
  let Some(output) = snapshot
    .outputs
    .iter()
    .find(|output| output.output == state.output_id)
  else {
    return json_error("output not found");
  };
  let workarea = state
    .core
    .tree(&state.output_id, workspace)
    .map_or(output.workarea, |tree| tree.workarea());
  let root = output
    .workspaces
    .iter()
    .find(|item| item.workspace == workspace)
    .map(|item| item.root.clone())
    .unwrap_or(shingle_core::SplitSnapshot {
      direction: shingle_core::SplitDirection::Vertical,
      children: Vec::new(),
    });
  let node = shingle_core::NodeSnapshot::Split(root);
  let layout = layout_node(state, &node, workarea).unwrap_or(Value::Null);
  json!({"result": "ok", "layout": layout})
}

fn parse_layout(
  state: &CompositorState,
  value: &Value,
  touched: &mut HashSet<WindowId>,
) -> Result<shingle_core::NodeSnapshot, Value> {
  use shingle_core::{ChildSnapshot, NodeSnapshot, SplitDirection};
  if let Some(id) = value.get("view-id") {
    let Some(id) = id.as_u64() else {
      return Err(json_error("view-id should be an unsigned integer"));
    };
    let window = WindowId(id);
    if !touched.insert(window) {
      return Err(json_error("view occurs more than once in the layout"));
    }
    find_view_id(state, window)
      .and_then(|window| require_mapped(state, window))?;
    let mut leaf = state.core.window_info(window).unwrap().state;
    leaf.floating = None;
    leaf.snapped = None;
    leaf.restore_geometry = None;
    leaf.tiled_edges = 15;
    return Ok(NodeSnapshot::Leaf {
      window,
      state: leaf,
    });
  }
  let horizontal = value.get("horizontal-split");
  let vertical = value.get("vertical-split");
  let (direction, children) = match (horizontal, vertical) {
    (Some(Value::Array(children)), None) => {
      (SplitDirection::Horizontal, children)
    }
    (None, Some(Value::Array(children))) => {
      (SplitDirection::Vertical, children)
    }
    _ => {
      return Err(json_error("node must contain exactly one split array"));
    }
  };
  let mut parsed = Vec::with_capacity(children.len());
  for child in children {
    let weight = child
      .get("weight")
      .or_else(|| child.get("percent"))
      .and_then(Value::as_f64)
      .filter(|weight| weight.is_finite() && *weight > 0.0)
      .and_then(|weight| {
        let scaled = weight * 1_000_000.0;
        (scaled <= u64::MAX as f64).then_some(scaled.round() as u64)
      })
      .filter(|weight| *weight > 0);
    let Some(weight) = weight else {
      return Err(json_error("each split child needs a positive weight"));
    };
    parsed.push(ChildSnapshot {
      weight,
      node: parse_layout(state, child, touched)?,
    });
  }
  Ok(NodeSnapshot::Split(shingle_core::SplitSnapshot {
    direction,
    children: parsed,
  }))
}

fn transform_layout(
  node: shingle_core::NodeSnapshot,
  root: bool,
  leaf: &mut impl FnMut(
    WindowId,
    shingle_core::LeafState,
  ) -> Option<shingle_core::NodeSnapshot>,
) -> Option<shingle_core::NodeSnapshot> {
  use shingle_core::{ChildSnapshot, NodeSnapshot};
  match node {
    NodeSnapshot::Leaf { window, state } => leaf(window, state),
    NodeSnapshot::Split(split) => {
      let children = split
        .children
        .into_iter()
        .filter_map(|child| {
          Some(ChildSnapshot {
            weight: child.weight,
            node: transform_layout(child.node, false, leaf)?,
          })
        })
        .collect::<Vec<_>>();
      if !root && children.len() == 1 {
        return Some(children.into_iter().next().unwrap().node);
      }
      Some(NodeSnapshot::Split(shingle_core::SplitSnapshot {
        direction: split.direction,
        children,
      }))
    }
  }
}

fn layout_windows(node: &shingle_core::NodeSnapshot, ids: &mut Vec<WindowId>) {
  match node {
    shingle_core::NodeSnapshot::Leaf { window, .. } => ids.push(*window),
    shingle_core::NodeSnapshot::Split(split) => {
      for child in &split.children {
        layout_windows(&child.node, ids);
      }
    }
  }
}

fn set_layout(state: &mut CompositorState, data: &Value) -> Value {
  use shingle_core::{
    ChildSnapshot, NodeSnapshot, SplitDirection, SplitSnapshot,
  };
  if data.get("wset-index").and_then(Value::as_u64) != Some(1) {
    return json_error("wset-index not found");
  }
  let Some(workspace) = data.get("workspace") else {
    return json_error("missing workspace field");
  };
  let Some(x) = workspace
    .get("x")
    .and_then(Value::as_u64)
    .and_then(|x| u32::try_from(x).ok())
    .filter(|x| *x < state.workspace_width)
  else {
    return json_error("invalid workspace x coordinate");
  };
  let Some(y) = workspace
    .get("y")
    .and_then(Value::as_u64)
    .and_then(|y| u32::try_from(y).ok())
    .filter(|y| *y < state.workspace_height)
  else {
    return json_error("invalid workspace y coordinate");
  };
  let target = shingle_core::WorkspaceId::new(x, y);
  let Some(layout) = data.get("layout") else {
    return json_error("missing layout field");
  };
  let mut touched = HashSet::new();
  let imported = match parse_layout(state, layout, &mut touched) {
    Ok(layout) => layout,
    Err(error) => return error,
  };
  let imported = match imported {
    NodeSnapshot::Split(split) => split,
    leaf => SplitSnapshot {
      direction: SplitDirection::Vertical,
      children: vec![ChildSnapshot {
        weight: 1,
        node: leaf,
      }],
    },
  };
  let mut imported_ids = Vec::new();
  for child in &imported.children {
    layout_windows(&child.node, &mut imported_ids);
  }

  let old_states = state
    .core
    .window_ids()
    .filter_map(|window| {
      state
        .core
        .window_info(window)
        .ok()
        .map(|info| (window, info))
    })
    .collect::<Vec<_>>();
  let mapped = old_states
    .iter()
    .filter_map(|(window, info)| {
      (info.presence == WindowPresence::Mapped).then_some(*window)
    })
    .collect::<Vec<_>>();
  let mut snapshot = state.core.snapshot();
  let Some(output) = snapshot
    .outputs
    .iter_mut()
    .find(|output| output.output == state.output_id)
  else {
    return json_error("output not found");
  };
  for workspace in &mut output.workspaces {
    let target_workspace = workspace.workspace == target;
    let old_stack = workspace.stack.clone();
    let old_focused = workspace.focused;
    let root = NodeSnapshot::Split(workspace.root.clone());
    let transformed = transform_layout(root, true, &mut |window, mut leaf| {
      if touched.contains(&window) {
        return None;
      }
      // A held view is not part of the layout yet and joins it when
      // released, so it is left alone.
      if target_workspace && leaf.floating.is_none() && !leaf.held {
        let geometry = state
          .core
          .window_layout(window)
          .ok()
          .map(|layout| layout.geometry)
          .unwrap_or(state.output);
        leaf.floating = Some(geometry);
        leaf.snapped = None;
        leaf.restore_geometry = None;
        leaf.tiled_edges = 0;
      }
      Some(NodeSnapshot::Leaf {
        window,
        state: leaf,
      })
    });
    let Some(NodeSnapshot::Split(mut root)) = transformed else {
      return json_error("failed to transform existing layout");
    };
    if target_workspace {
      if root.direction == imported.direction {
        root.children.extend(imported.children.clone());
      } else {
        root.children.push(ChildSnapshot {
          weight: 1,
          node: NodeSnapshot::Split(imported.clone()),
        });
      }
    }
    workspace.root = root;
    workspace.stack = old_stack
      .into_iter()
      .filter(|window| !touched.contains(window))
      .collect();
    if target_workspace {
      workspace.stack.extend(imported_ids.iter().copied());
    }
    workspace.focused = old_focused
      .filter(|window| workspace.stack.contains(window))
      .or_else(|| workspace.stack.first().copied());
  }
  if !output
    .workspaces
    .iter()
    .any(|workspace| workspace.workspace == target)
  {
    output.workspaces.push(shingle_core::WorkspaceSnapshot {
      workspace: target,
      focused: imported_ids.first().copied(),
      stack: imported_ids.clone(),
      root: imported,
    });
  }
  let mut restored = match shingle_core::Shingle::from_snapshot(snapshot) {
    Ok(core) => core,
    Err(error) => return json_error(&error.to_string()),
  };
  for window in mapped {
    if restored.mark_window_mapped(window).is_err() {
      return json_error("failed to preserve mapped window state");
    }
  }
  state.core = restored;
  for surface in state.surfaces.values_mut() {
    let Some(toplevel) = surface.role.toplevel_mut() else {
      continue;
    };
    let Some(window) = toplevel.window() else {
      continue;
    };
    let Ok(info) = state.core.window_info(window) else {
      continue;
    };
    let tiled = info.state.floating.is_none();
    if toplevel.tiled != tiled {
      toplevel.tiled = tiled;
      let old_edges = old_states
        .iter()
        .find(|(old_window, _)| *old_window == window)
        .map_or(0, |(_, old)| old.state.tiled_edges);
      state.events.push(crate::Event::TilingChanged {
        window,
        surface: surface.id,
        tiled,
        old_edges,
        new_edges: info.state.tiled_edges,
      });
    }
    if let Some(geometry) = info.state.floating {
      toplevel.floating_geometry = geometry;
    }
  }
  for (window, old) in old_states {
    let Ok(info) = state.core.window_info(window) else {
      continue;
    };
    if info.location.workspace != old.location.workspace {
      let index = info.location.workspace.y * state.workspace_width
        + info.location.workspace.x;
      let from = old.location.workspace.y * state.workspace_width
        + old.location.workspace.x
        + 1;
      state.events.push(crate::Event::WindowMoved {
        window,
        from,
        workspace: index + 1,
      });
    }
  }
  state.input.keyboard_focus = None;
  input::sync_focus(state);
  shell::reconfigure_all(state);
  json_ok()
}

fn set_show_maximized(state: &mut CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let Some(maximized) = data.get("show-maximized").and_then(Value::as_bool)
  else {
    return json_error("missing or invalid show-maximized field");
  };
  let tiled = state.surfaces.values().any(|surface| {
    surface.role.toplevel().is_some_and(|toplevel| {
      toplevel.window() == Some(window) && toplevel.tiled
    })
  }) && state
    .core
    .window_info(window)
    .is_ok_and(|info| info.state.floating.is_none());
  if !tiled {
    return json_error("view is not tiled");
  }
  match shell::set_maximized(state, window, maximized) {
    Ok(()) => json_ok(),
    Err(error) => json_error(&error.to_string()),
  }
}

fn normalize_runtime_binding(input: &str) -> Option<shingle_config::Binding> {
  let mut modifiers = Vec::new();
  let mut key = None;
  for token in input.split_whitespace() {
    if token.starts_with('<') && token.ends_with('>') {
      let modifier = match token[1..token.len() - 1].to_lowercase().as_str() {
        "super" | "logo" | "win" => "super",
        "ctrl" | "control" => "ctrl",
        "alt" => "alt",
        "shift" => "shift",
        _ => return None,
      };
      if modifiers.contains(&modifier) {
        return None;
      }
      modifiers.push(modifier);
    } else if key.replace(token).is_some() {
      return None;
    }
  }
  let key = key?;
  let key = key.strip_prefix("KEY_").unwrap_or(key).to_lowercase();
  let normalized = if modifiers.is_empty() {
    key
  } else {
    format!("{}+{key}", modifiers.join("+"))
  };
  shingle_config::Binding::try_from(normalized)
    .ok()
    .filter(|binding| !binding.key.is_empty())
}

fn evdev_code(name: &str) -> Option<u32> {
  if let Some(letter) = name.strip_prefix("KEY_") {
    let mut chars = letter.chars();
    if let (Some(letter), None) = (chars.next(), chars.next()) {
      if letter.is_ascii_digit() {
        return Some(if letter == '0' {
          11
        } else {
          letter as u32 - '1' as u32 + 2
        });
      }
      for (row, first) in
        [("QWERTYUIOP", 16), ("ASDFGHJKL", 30), ("ZXCVBNM", 44)]
      {
        if let Some(offset) = row.find(letter) {
          return Some(first + offset as u32);
        }
      }
    }
    if let Some(number) = letter.strip_prefix('F').and_then(|n| n.parse().ok())
    {
      return match number {
        1..=10 => Some(58 + number),
        11 => Some(87),
        12 => Some(88),
        _ => None,
      };
    }
  }
  Some(match name {
    "KEY_ESC" => 1,
    "KEY_MINUS" => 12,
    "KEY_EQUAL" => 13,
    "KEY_BACKSPACE" => 14,
    "KEY_TAB" => 15,
    "KEY_LEFTBRACE" => 26,
    "KEY_RIGHTBRACE" => 27,
    "KEY_ENTER" => 28,
    "KEY_LEFTCTRL" => 29,
    "KEY_SEMICOLON" => 39,
    "KEY_APOSTROPHE" => 40,
    "KEY_GRAVE" => 41,
    "KEY_LEFTSHIFT" => 42,
    "KEY_BACKSLASH" => 43,
    "KEY_COMMA" => 51,
    "KEY_DOT" => 52,
    "KEY_SLASH" => 53,
    "KEY_RIGHTSHIFT" => 54,
    "KEY_LEFTALT" => 56,
    "KEY_SPACE" => 57,
    "KEY_CAPSLOCK" => 58,
    "KEY_RIGHTCTRL" => 97,
    "KEY_RIGHTALT" => 100,
    "KEY_HOME" => 102,
    "KEY_UP" => 103,
    "KEY_PAGEUP" => 104,
    "KEY_LEFT" => 105,
    "KEY_RIGHT" => 106,
    "KEY_END" => 107,
    "KEY_DOWN" => 108,
    "KEY_PAGEDOWN" => 109,
    "KEY_INSERT" => 110,
    "KEY_DELETE" => 111,
    "KEY_LEFTMETA" => 125,
    "KEY_RIGHTMETA" => 126,
    "BTN_LEFT" => 0x110,
    "BTN_RIGHT" => 0x111,
    "BTN_MIDDLE" => 0x112,
    "BTN_SIDE" => 0x113,
    "BTN_EXTRA" => 0x114,
    "BTN_FORWARD" => 0x115,
    "BTN_BACK" => 0x116,
    _ => return None,
  })
}

fn feed_key(state: &mut CompositorState, data: &Value) -> Value {
  let Some(name) = data.get("key").and_then(Value::as_str) else {
    return json_error("missing or invalid key");
  };
  let Some(key) = evdev_code(name) else {
    return json_error("failed to parse evdev key");
  };
  let pressed = data.get("state").and_then(Value::as_bool).unwrap_or(false);
  let Some(keyboard) = stipc_device(state, InputDeviceKind::Keyboard) else {
    return json_ok();
  };
  if state.stipc_key_events {
    // The frontend tracks keys from a disabled keyboard and dispatches
    // them only while it is enabled, as for a real key.
    if input_device::session_active(state) {
      state.events.push(crate::Event::StipcKey {
        device: keyboard,
        code: key,
        pressed,
      });
    }
    return json_ok();
  }
  if !input_device::delivers(state, keyboard, InputDeviceKind::Keyboard) {
    return json_ok();
  }
  if !stipc_key_binding(state, keyboard, name, pressed) {
    input_device::keyboard_key(state, keyboard, key, pressed);
  }
  json_ok()
}

/// Runs the runtime binding that matches a STIPC key, as Wayfire's input
/// pipeline does for keys from its STIPC keyboard. Returns whether a binding
/// matched; a matched key is consumed and not passed to clients, like a
/// matched key from the nested host keyboard. Only compositors without a
/// frontend get here, such as the headless `shingle` binary; a frontend
/// takes STIPC keys as [`crate::Event::StipcKey`].
fn stipc_key_binding(
  state: &mut CompositorState,
  keyboard: InputDeviceId,
  name: &str,
  pressed: bool,
) -> bool {
  if crate::shortcuts::active(state) {
    return false;
  }
  let key = name.strip_prefix("KEY_").unwrap_or(name).to_lowercase();
  let modifiers = stipc_modifiers(state, keyboard);
  let Some(binding) = state
    .runtime_bindings
    .values()
    .find(|binding| binding.binding.matches(modifiers, &key))
    .cloned()
  else {
    return false;
  };
  let locked = state.idle.lock_state() != shingle_idle::LockState::Unlocked;
  if locked && !binding.exec_always {
    return true;
  }
  let should_run = match binding.mode {
    crate::RuntimeBindingMode::Normal | crate::RuntimeBindingMode::Repeat => {
      pressed
    }
    crate::RuntimeBindingMode::Release => !pressed,
  };
  if !should_run {
    return true;
  }
  if let Some(command) = &binding.command {
    let environment = state.client_environment();
    match shingle_session::launch_command(command, &environment, false) {
      Ok(child) => state.ipc_children.push(child),
      Err(error) => eprintln!("shingle: runtime command failed: {error}"),
    }
  } else {
    trigger_runtime_binding(state, binding.id);
  }
  true
}

/// Combines the seat's depressed modifiers with modifier keys the STIPC
/// keyboard holds, since STIPC does not run them through XKB.
fn stipc_modifiers(
  state: &CompositorState,
  keyboard: InputDeviceId,
) -> shingle_config::Modifiers {
  let depressed = state.input.modifiers.depressed;
  let held = input_device::held_keys(state, keyboard);
  let any_held = |codes: [u32; 2]| codes.iter().any(|code| held.contains(code));
  shingle_config::Modifiers {
    super_key: depressed & 64 != 0 || any_held([125, 126]),
    control: depressed & 4 != 0 || any_held([29, 97]),
    alt: depressed & 8 != 0 || any_held([56, 100]),
    shift: depressed & 1 != 0 || any_held([42, 54]),
  }
}

/// Performs a runtime binding's compositor-side action: its IPC method call,
/// or a `command-binding` event for the client that registered it. Bindings
/// with a command are launched by the caller.
pub(crate) fn trigger_runtime_binding(state: &mut CompositorState, id: u64) {
  let Some(binding) = state.runtime_bindings.get(&id).cloned() else {
    return;
  };
  if let Some(method) = binding.call_method {
    let _ = respond(
      state,
      &json!({
          "method": method,
          "data": binding.call_data.unwrap_or(Value::Null),
      }),
    );
  } else if binding.command.is_none() {
    state.ipc_events.push(json!({
        "event": "command-binding",
        "binding-id": id,
        "client-id": binding.client_id,
    }));
  }
}

fn feed_button(state: &mut CompositorState, data: &Value) -> Value {
  let Some(combo) = data.get("combo").and_then(Value::as_str) else {
    return json_error("missing or invalid combo");
  };
  let (super_modifier, button) = combo
    .strip_prefix("S-")
    .map_or((false, combo), |combo| (true, combo));
  let Some(button) = evdev_code(button) else {
    return json_error("failed to parse button combo");
  };
  let mode = data.get("mode").and_then(Value::as_str).unwrap_or("");
  let keyboard = stipc_device(state, InputDeviceKind::Keyboard);
  let Some(pointer) = stipc_device(state, InputDeviceKind::Pointer) else {
    return json_ok();
  };
  let super_key = |state: &mut CompositorState, pressed| {
    if let Some(keyboard) = keyboard.filter(|_| super_modifier) {
      input_device::keyboard_key(state, keyboard, 125, pressed);
    }
  };
  if matches!(mode, "press" | "full") {
    super_key(state, true);
    input_device::pointer_button(state, pointer, button, true);
  }
  if matches!(mode, "release" | "full") {
    input_device::pointer_button(state, pointer, button, false);
    super_key(state, false);
  }
  json_ok()
}

fn move_cursor(state: &mut CompositorState, data: &Value) -> Value {
  let Some(x) = data.get("x").and_then(Value::as_f64) else {
    return json_error("missing or invalid x coordinate");
  };
  let Some(y) = data.get("y").and_then(Value::as_f64) else {
    return json_error("missing or invalid y coordinate");
  };
  if let Some(pointer) = stipc_device(state, InputDeviceKind::Pointer) {
    input_device::pointer_motion(state, pointer, x, y);
  }
  json_ok()
}

fn feed_touch(state: &mut CompositorState, data: &Value) -> Value {
  let Some(finger) = data
    .get("finger")
    .and_then(Value::as_i64)
    .and_then(|finger| i32::try_from(finger).ok())
  else {
    return json_error("missing or invalid finger");
  };
  let Some(x) = data.get("x").and_then(Value::as_f64) else {
    return json_error("missing or invalid x coordinate");
  };
  let Some(y) = data.get("y").and_then(Value::as_f64) else {
    return json_error("missing or invalid y coordinate");
  };
  if let Some(touch) = stipc_device(state, InputDeviceKind::Touch) {
    input_device::touch(state, touch, finger, x, y);
  }
  json_ok()
}

fn release_touch(state: &mut CompositorState, data: &Value) -> Value {
  let Some(finger) = data
    .get("finger")
    .and_then(Value::as_i64)
    .and_then(|finger| i32::try_from(finger).ok())
  else {
    return json_error("missing or invalid finger");
  };
  if let Some(touch) = stipc_device(state, InputDeviceKind::Touch) {
    input_device::touch_up(state, touch, finger);
  }
  json_ok()
}

fn run_command(state: &mut CompositorState, data: &Value) -> Value {
  let Some(command) = data.get("cmd").and_then(Value::as_str) else {
    return json_error("missing or invalid cmd");
  };
  if command.trim().is_empty() {
    return json_error("command cannot be empty");
  }
  let environment = state.client_environment();
  match shingle_session::launch_command(command, &environment, false) {
    Ok(child) => {
      let pid = child.id();
      state.ipc_children.push(child);
      json!({"result": "ok", "pid": pid})
    }
    Err(error) => json_error(&format!("failed to run command: {error}")),
  }
}

fn layout_views(state: &mut CompositorState, data: &Value) -> Value {
  let Some(views) = data.get("views").and_then(Value::as_array) else {
    return json_error("views not specified");
  };
  let mut updates = Vec::with_capacity(views.len());
  for view in views {
    let Some(window) = view
      .get("id")
      .and_then(Value::as_u64)
      .and_then(|id| find_view_id(state, WindowId(id)).ok())
    else {
      return json_error("unknown or invalid view id");
    };
    if let Some(output) = view.get("output")
      && output.as_str() != Some(state.output_info.name.as_str())
    {
      return json_error("unknown output");
    }
    let number = |key: &str| {
      view
        .get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
    };
    let (Some(x), Some(y), Some(width), Some(height)) =
      (number("x"), number("y"), number("width"), number("height"))
    else {
      return json_error("missing or invalid view geometry");
    };
    if x < i32::MIN as f64
      || x > i32::MAX as f64
      || y < i32::MIN as f64
      || y > i32::MAX as f64
      || width <= 0.0
      || height <= 0.0
      || width > u32::MAX as f64
      || height > u32::MAX as f64
    {
      return json_error("invalid view geometry");
    }
    let rect = Rect::new(x as i32, y as i32, width as u32, height as u32);
    if rect.validate().is_err() {
      return json_error("invalid view geometry");
    }
    updates.push((window, rect));
  }
  for (window, geometry) in updates {
    if shell::set_floating_geometry(state, window, geometry).is_err() {
      return json_error("failed to apply view geometry");
    }
  }
  json_ok()
}

pub(crate) fn keyboard_state(state: &CompositorState) -> Value {
  let layouts = input::keyboard_layouts(state);
  let index = state.input.modifiers.group as usize;
  json!({
      "layout": layouts.get(index).map_or("unknown", String::as_str),
      "layout-index": state.input.modifiers.group,
      "possible-layouts": layouts,
  })
}

pub(crate) fn plugin_activation_event(
  state: &CompositorState,
  plugin: &str,
  activated: bool,
) -> Value {
  json!({
      "event": "plugin-activation-state-changed",
      "plugin": plugin,
      "state": activated,
      "output": 1,
      "output-data": output_json(state),
  })
}

fn set_keyboard_state(state: &mut CompositorState, data: &Value) -> Value {
  let layouts = input::keyboard_layouts(state);
  let Some(index) = data
    .get("layout-index")
    .and_then(Value::as_u64)
    .and_then(|index| u32::try_from(index).ok())
    .filter(|index| (*index as usize) < layouts.len())
  else {
    return json_error("invalid layout index");
  };
  let modifiers = input::Modifiers {
    group: index,
    ..state.input.modifiers
  };
  input::keyboard_modifiers(state, modifiers);
  json_ok()
}

fn get_view_alpha(state: &CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let opacity = state
    .surfaces
    .values()
    .find_map(|surface| match surface.role.toplevel() {
      Some(toplevel) if toplevel.window() == Some(window) => {
        Some(toplevel.opacity)
      }
      _ => None,
    })
    .unwrap_or(1.0);
  json!({"result": "ok", "alpha": opacity})
}

fn set_view_alpha(state: &mut CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let Some(alpha) = data.get("alpha").and_then(Value::as_f64) else {
    return json_error("missing or invalid alpha");
  };
  shell::set_alpha(state, window, alpha as f32);
  json_ok()
}

/// Resolves Wayfire's one-based numeric workspace activator names.
fn numbered_workspace_index(
  state: &CompositorState,
  action: &str,
) -> Result<Option<usize>, crate::Error> {
  if action.is_empty() || !action.bytes().all(|byte| byte.is_ascii_digit()) {
    return Ok(None);
  }
  let workspace = action
    .parse::<usize>()
    .map_err(|_| crate::Error::InvalidWorkspace(usize::MAX))?;
  let count = state.workspace_width as usize * state.workspace_height as usize;
  if workspace == 0 || workspace > count {
    return Err(crate::Error::InvalidWorkspace(workspace));
  }
  Ok(Some(workspace - 1))
}

fn vswitch_activator(state: &mut CompositorState, method: &str) -> Value {
  let method = method.strip_prefix("vswitch/").unwrap_or(method);
  let result = (|| -> Result<(), crate::Error> {
    if let Some(action) = method.strip_prefix("binding_") {
      if action == "last" {
        if let Some(workspace) = state.last_workspace {
          let index =
            (workspace.y * state.workspace_width + workspace.x) as usize;
          state.switch_workspace(index)?;
        }
        return Ok(());
      }
      if let Some(index) = numbered_workspace_index(state, action)? {
        state.switch_workspace(index)?;
        return Ok(());
      }
      let direction = match action {
        "left" => Direction::Left,
        "right" => Direction::Right,
        "up" => Direction::Up,
        "down" => Direction::Down,
        _ => return Err(crate::Error::InvalidWorkspace(0)),
      };
      let index = state.relative_workspace_index(direction)?;
      state.switch_workspace(index)?;
      return Ok(());
    }
    let (with_window, action) =
      if let Some(action) = method.strip_prefix("with_win_") {
        (true, action)
      } else if let Some(action) = method.strip_prefix("send_win_") {
        (false, action)
      } else {
        return Err(crate::Error::InvalidWorkspace(0));
      };
    let index = if action == "last" {
      let Some(workspace) = state.last_workspace else {
        return Ok(());
      };
      (workspace.y * state.workspace_width + workspace.x) as usize
    } else if let Some(index) = numbered_workspace_index(state, action)? {
      index
    } else {
      let direction = match action {
        "left" => Direction::Left,
        "right" => Direction::Right,
        "up" => Direction::Up,
        "down" => Direction::Down,
        _ => return Err(crate::Error::InvalidWorkspace(0)),
      };
      state.relative_workspace_index(direction)?
    };
    state.move_focused_to_workspace(index)?;
    if with_window {
      state.switch_workspace(index)?;
    }
    Ok(())
  })();
  match result {
    Ok(()) => json_ok(),
    Err(error) => json_error(&error.to_string()),
  }
}

fn selected_view(
  state: &CompositorState,
  data: &Value,
) -> Result<WindowId, Value> {
  if ["id", "view-id", "view_id"]
    .into_iter()
    .any(|key| data.get(key).is_some())
  {
    find_view(state, data)
  } else {
    input::focused_window(state).ok_or_else(|| json_error("no view selected"))
  }
}

fn wm_activator(
  state: &mut CompositorState,
  method: &str,
  data: &Value,
) -> Value {
  let window = match selected_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let Ok(info) = state.core.window_info(window) else {
    return json_error("view id not found");
  };
  let result = match method {
    "wm-actions/minimize" => shell::set_minimized(state, window, true),
    "wm-actions/toggle_maximize" => {
      shell::set_maximized(state, window, !info.state.maximized)
    }
    "wm-actions/toggle_fullscreen" => {
      shell::set_fullscreen(state, window, !info.state.fullscreen)
    }
    "wm-actions/toggle_sticky" => {
      shell::set_sticky(state, window, !info.state.sticky)
    }
    "wm-actions/toggle_always_on_top"
    | "wm-actions/toggle_always_on_bottom" => {
      let target = if method.ends_with("top") {
        StackingMode::AlwaysOnTop
      } else {
        StackingMode::AlwaysOnBottom
      };
      shell::set_stacking(
        state,
        window,
        if info.state.stacking == target {
          StackingMode::Normal
        } else {
          target
        },
      )
    }
    "wm-actions/bring-to-front" => {
      state.core.raise_window(window).map_err(crate::Error::from)
    }
    "wm-actions/send-to-back" => {
      state.core.lower_window(window).map_err(crate::Error::from)
    }
    _ => return json_error("unsupported window activator"),
  };
  if result.is_err() {
    return json_error("failed to apply window activator");
  }
  if matches!(
    method,
    "wm-actions/bring-to-front" | "wm-actions/send-to-back"
  ) {
    state
      .events
      .push(crate::Event::StackingOrderChanged { window });
  }
  json_ok()
}

fn wm_action(state: &mut CompositorState, method: &str, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  let enabled = data.get("state").and_then(Value::as_bool).unwrap_or(false);
  let result = match method {
    "wm-actions/set-minimized" => shell::set_minimized(state, window, enabled),
    "wm-actions/set-fullscreen" => {
      shell::set_fullscreen(state, window, enabled)
    }
    "wm-actions/set-sticky" => shell::set_sticky(state, window, enabled),
    "wm-actions/set-always-on-top" | "wm-actions/set-always-on-bottom" => {
      let target = if method.ends_with("top") {
        StackingMode::AlwaysOnTop
      } else {
        StackingMode::AlwaysOnBottom
      };
      let current = state
        .core
        .window_info(window)
        .ok()
        .map(|info| info.state.stacking);
      if enabled || current == Some(target) {
        shell::set_stacking(
          state,
          window,
          if enabled {
            target
          } else {
            StackingMode::Normal
          },
        )
      } else {
        Ok(())
      }
    }
    "wm-actions/bring-to-front" => {
      state.core.raise_window(window).map_err(crate::Error::from)
    }
    "wm-actions/send-to-back" => {
      state.core.lower_window(window).map_err(crate::Error::from)
    }
    _ => return json_error("unsupported window action"),
  };
  if result.is_err() {
    return json_error("failed to apply window action");
  }
  if matches!(
    method,
    "wm-actions/bring-to-front" | "wm-actions/send-to-back"
  ) {
    state
      .events
      .push(crate::Event::StackingOrderChanged { window });
  }
  shell::reconfigure_all(state);
  json_ok()
}

fn configure_view(state: &mut CompositorState, data: &Value) -> Value {
  let window = match find_view(state, data) {
    Ok(window) => window,
    Err(error) => return error,
  };
  if let Some(output) = data.get("output_id").or_else(|| data.get("output-id"))
    && output.as_u64() != Some(1)
  {
    return json_error("output not found");
  }
  let tiled_edges = match data.get("tiled-edges") {
    Some(value) => {
      match value.as_u64().and_then(|edges| u32::try_from(edges).ok()) {
        Some(edges @ 0..=15) => Some(edges),
        _ => return json_error("invalid tiled-edges value"),
      }
    }
    None => None,
  };
  if let Some(value) = data.get("geometry") {
    let Some(rect) = value
      .get("x")
      .and_then(Value::as_i64)
      .and_then(|x| i32::try_from(x).ok())
      .and_then(|x| {
        Some(Rect::new(
          x,
          i32::try_from(value.get("y")?.as_i64()?).ok()?,
          u32::try_from(value.get("width")?.as_u64()?).ok()?,
          u32::try_from(value.get("height")?.as_u64()?).ok()?,
        ))
      })
    else {
      return json_error("invalid geometry");
    };
    if rect.width == 0 || rect.height == 0 {
      return json_error("geometry dimensions must be nonzero");
    }
    if shell::set_floating_geometry(state, window, rect).is_err() {
      return json_error("failed to set geometry");
    }
  }
  if let Some(edges) = tiled_edges {
    let result = if edges == 0 {
      if state
        .core
        .window_info(window)
        .is_ok_and(|info| info.state.floating.is_some())
      {
        Ok(())
      } else {
        shell::toggle_tiling(state, window).map(|_| ())
      }
    } else {
      let workarea = state
        .core
        .window_location(window)
        .and_then(|location| {
          state.core.tree(&location.output, location.workspace)
        })
        .map_or(state.output, |tree| tree.workarea());
      shell::snap_window_to_geometry(
        state,
        window,
        shell::edges_geometry(workarea, edges),
        edges,
      )
    };
    if result.is_err() {
      return json_error("failed to set tiled edges");
    }
  }
  if let Some(fullscreen) = data.get("fullscreen").and_then(Value::as_bool)
    && shell::set_fullscreen(state, window, fullscreen).is_err()
  {
    return json_error("failed to set fullscreen state");
  }
  if let Some(sticky) = data.get("sticky").and_then(Value::as_bool)
    && shell::set_sticky(state, window, sticky).is_err()
  {
    return json_error("failed to set sticky state");
  }
  json_ok()
}

const CONFIG_OPTIONS: &[&str] = &[
  "core/vwidth",
  "core/vheight",
  "core/background_color",
  "core/preferred_decoration_mode",
  "output/mode",
  "output/position",
  "output/scale",
  "output/transform",
  "input/cursor_theme",
  "input/cursor_size",
  "input/mouse_scroll_speed",
  "input/mouse_natural_scroll",
  "simple-tile/keep_fullscreen_on_adjacent",
  "simple-tile/inner_gap_size",
  "simple-tile/outer_horiz_gap_size",
  "simple-tile/outer_vert_gap_size",
  "workarounds/max_buffer_size",
  "workarounds/focus_main_surface_instead_of_popup",
];

fn output_transform_name(
  transform: wayland_server::protocol::wl_output::Transform,
) -> &'static str {
  use wayland_server::protocol::wl_output::Transform;
  match transform {
    Transform::Normal => "normal",
    Transform::_90 => "90",
    Transform::_180 => "180",
    Transform::_270 => "270",
    Transform::Flipped => "flipped",
    Transform::Flipped90 => "flipped-90",
    Transform::Flipped180 => "flipped-180",
    Transform::Flipped270 => "flipped-270",
    _ => "unknown",
  }
}

fn config_option(
  state: &CompositorState,
  name: &str,
) -> Option<(String, String)> {
  let (scroll_speed, natural_scroll) = input::scroll_config(state);
  let decoration = match state.preferred_decoration_mode {
    DecorationMode::ServerSide => "server",
    _ => "client",
  };
  Some(match name {
    "core/vwidth" => (state.workspace_width.to_string(), "10".into()),
    "core/vheight" => (state.workspace_height.to_string(), "1".into()),
    "core/background_color" => (
      state
        .background_color
        .map(|channel| channel.to_string())
        .join(" "),
      "0.1 0.1 0.1 1".into(),
    ),
    "core/preferred_decoration_mode" => (decoration.into(), "client".into()),
    "output/mode" => (
      format!("{}x{}", state.output.width, state.output.height),
      "1280x720".into(),
    ),
    "output/position" => (
      format!("{},{}", state.output.x, state.output.y),
      "0,0".into(),
    ),
    "output/scale" => (state.output_scale.to_string(), "1".into()),
    "output/transform" => (
      output_transform_name(state.output_transform).into(),
      "normal".into(),
    ),
    "input/cursor_theme" => (state.cursor_theme.clone(), "default".into()),
    "input/cursor_size" => (state.cursor_size.to_string(), "24".into()),
    "input/mouse_scroll_speed" => (scroll_speed.to_string(), "1".into()),
    "input/mouse_natural_scroll" => {
      (natural_scroll.to_string(), "false".into())
    }
    "simple-tile/keep_fullscreen_on_adjacent" => {
      (state.keep_fullscreen_on_adjacent.to_string(), "true".into())
    }
    "simple-tile/inner_gap_size" => (state.gaps.left.to_string(), "0".into()),
    "simple-tile/outer_horiz_gap_size" => {
      (state.gaps.left.to_string(), "0".into())
    }
    "simple-tile/outer_vert_gap_size" => {
      (state.gaps.top.to_string(), "0".into())
    }
    "workarounds/max_buffer_size" => {
      (state.max_buffer_size.to_string(), "16384".into())
    }
    "workarounds/focus_main_surface_instead_of_popup" => (
      state.focus_main_surface_instead_of_popup.to_string(),
      "false".into(),
    ),
    _ => return None,
  })
}

fn get_config_option(state: &CompositorState, data: &Value) -> Value {
  let Some(name) = data.get("option").and_then(Value::as_str) else {
    return json_error("missing or invalid option");
  };
  let Some((value, default)) = config_option(state, name) else {
    return json_error("option not found");
  };
  json!({"result": "ok", "value": value, "default": default})
}

fn list_config_options(state: &CompositorState) -> Value {
  let mut sections = serde_json::Map::new();
  for name in CONFIG_OPTIONS {
    let Some((value, default)) = config_option(state, name) else {
      continue;
    };
    let (section, option) = name.split_once('/').unwrap();
    let section = sections
      .entry(section)
      .or_insert_with(|| Value::Object(serde_json::Map::new()));
    section[option] = json!({"value": value, "default": default});
  }
  json!({"result": "ok", "options": Value::Object(sections)})
}

enum ConfigUpdate {
  Background([f64; 4]),
  Decoration(bool),
  Mode(u32, u32),
  Position(i32, i32),
  Scale(f64),
  Transform(wayland_server::protocol::wl_output::Transform),
  CursorTheme(String),
  CursorSize(u32),
  ScrollSpeed(f64),
  NaturalScroll(bool),
  KeepFullscreen(bool),
  Gap(String, u32),
  MaxBufferSize(u32),
  FocusPopup(bool),
  GridWidth(u32),
  GridHeight(u32),
}

fn config_text(value: &Value) -> String {
  value
    .as_str()
    .map_or_else(|| value.to_string(), str::to_owned)
}

fn parse_config_update(name: &str, text: &str) -> Option<ConfigUpdate> {
  use wayland_server::protocol::wl_output::Transform;
  let boolean = || match text {
    "true" => Some(true),
    "false" => Some(false),
    _ => None,
  };
  let unsigned = || text.parse::<u32>().ok().filter(|value| *value > 0);
  match name {
    "core/vwidth" => Some(ConfigUpdate::GridWidth(unsigned()?)),
    "core/vheight" => Some(ConfigUpdate::GridHeight(unsigned()?)),
    "core/background_color" => {
      let channels = text
        .split_whitespace()
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
      if channels.len() != 4
        || channels.iter().any(|value| !(0.0..=1.0).contains(value))
      {
        return None;
      }
      Some(ConfigUpdate::Background(channels.try_into().ok()?))
    }
    "core/preferred_decoration_mode" => match text {
      "client" => Some(ConfigUpdate::Decoration(false)),
      "server" => Some(ConfigUpdate::Decoration(true)),
      _ => None,
    },
    "output/mode" => {
      let (width, height) = text.split_once('x')?;
      let width = unsigned_for(width)?;
      let height = unsigned_for(height)?;
      Rect::new(0, 0, width, height).validate().ok()?;
      Some(ConfigUpdate::Mode(width, height))
    }
    "output/position" => {
      let (x, y) = text.split_once(',')?;
      Some(ConfigUpdate::Position(x.parse().ok()?, y.parse().ok()?))
    }
    "output/scale" => {
      let scale = text.parse::<f64>().ok()?;
      (scale.is_finite() && scale > 0.0).then_some(ConfigUpdate::Scale(scale))
    }
    "output/transform" => {
      let transform = match text {
        "normal" => Transform::Normal,
        "90" => Transform::_90,
        "180" => Transform::_180,
        "270" => Transform::_270,
        "flipped" => Transform::Flipped,
        "flipped-90" => Transform::Flipped90,
        "flipped-180" => Transform::Flipped180,
        "flipped-270" => Transform::Flipped270,
        _ => return None,
      };
      Some(ConfigUpdate::Transform(transform))
    }
    "input/cursor_theme" if !text.is_empty() => {
      Some(ConfigUpdate::CursorTheme(text.into()))
    }
    "input/cursor_size" => Some(ConfigUpdate::CursorSize(unsigned()?)),
    "input/mouse_scroll_speed" => {
      let speed = text.parse::<f64>().ok()?;
      (speed.is_finite() && speed >= 0.0)
        .then_some(ConfigUpdate::ScrollSpeed(speed))
    }
    "input/mouse_natural_scroll" => {
      Some(ConfigUpdate::NaturalScroll(boolean()?))
    }
    "simple-tile/keep_fullscreen_on_adjacent" => {
      Some(ConfigUpdate::KeepFullscreen(boolean()?))
    }
    "simple-tile/inner_gap_size"
    | "simple-tile/outer_horiz_gap_size"
    | "simple-tile/outer_vert_gap_size" => {
      Some(ConfigUpdate::Gap(name.into(), text.parse().ok()?))
    }
    "workarounds/max_buffer_size" => {
      Some(ConfigUpdate::MaxBufferSize(unsigned()?))
    }
    "workarounds/focus_main_surface_instead_of_popup" => {
      Some(ConfigUpdate::FocusPopup(boolean()?))
    }
    _ => None,
  }
}

fn unsigned_for(text: &str) -> Option<u32> {
  text.parse::<u32>().ok().filter(|value| *value > 0)
}

fn apply_config_updates(
  state: &mut CompositorState,
  updates: Vec<ConfigUpdate>,
) -> Result<(), String> {
  for update in updates {
    match update {
      ConfigUpdate::Background(color) => state.background_color = color,
      ConfigUpdate::Decoration(server) => {
        state.preferred_decoration_mode = if server {
          DecorationMode::ServerSide
        } else {
          DecorationMode::ClientSide
        };
        crate::decoration::apply_preferences(state);
      }
      ConfigUpdate::Mode(width, height) => {
        state
          .set_output(Rect {
            width,
            height,
            ..state.output
          })
          .map_err(|error| error.to_string())?;
      }
      ConfigUpdate::Position(x, y) => {
        state
          .set_output(Rect {
            x,
            y,
            ..state.output
          })
          .map_err(|error| error.to_string())?;
      }
      ConfigUpdate::Scale(scale) => {
        state
          .set_output_scale(scale)
          .map_err(|error| error.to_string())?;
      }
      ConfigUpdate::Transform(transform) => {
        state.set_output_transform(transform);
      }
      ConfigUpdate::CursorTheme(theme) => state.cursor_theme = theme,
      ConfigUpdate::CursorSize(size) => state.cursor_size = size,
      ConfigUpdate::ScrollSpeed(speed) => {
        let natural = input::scroll_config(state).1;
        input::set_scroll(state, speed, natural);
      }
      ConfigUpdate::NaturalScroll(natural) => {
        let speed = input::scroll_config(state).0;
        input::set_scroll(state, speed, natural);
      }
      ConfigUpdate::KeepFullscreen(keep) => {
        state.keep_fullscreen_on_adjacent = keep;
      }
      ConfigUpdate::Gap(name, value) => {
        match name.as_str() {
          "simple-tile/inner_gap_size" => {
            state.gaps.left = value;
            state.gaps.right = value;
            state.gaps.top = value;
            state.gaps.bottom = value;
          }
          "simple-tile/outer_horiz_gap_size" => {
            state.gaps.left = value;
            state.gaps.right = value;
          }
          _ => {
            state.gaps.top = value;
            state.gaps.bottom = value;
          }
        }
        let windows = state.core.window_ids().collect::<Vec<_>>();
        for window in windows {
          state
            .core
            .set_gaps(window, state.gaps)
            .map_err(|error| error.to_string())?;
        }
      }
      ConfigUpdate::MaxBufferSize(size) => {
        state.max_buffer_size = size;
        for pool in state.pools.values() {
          pool.set_max_buffer_size(size);
        }
      }
      ConfigUpdate::FocusPopup(enabled) => {
        state.focus_main_surface_instead_of_popup = enabled;
      }
      ConfigUpdate::GridWidth(width) => {
        let height = state.workspace_height;
        state
          .set_workspace_grid(width, height)
          .map_err(|error| error.to_string())?;
      }
      ConfigUpdate::GridHeight(height) => {
        let width = state.workspace_width;
        state
          .set_workspace_grid(width, height)
          .map_err(|error| error.to_string())?;
      }
    }
  }
  shell::reconfigure_all(state);
  Ok(())
}

fn set_config_options(state: &mut CompositorState, data: &Value) -> Value {
  let Some(options) = data.as_object() else {
    return json_error("options must be an object");
  };
  let mut updates = Vec::with_capacity(options.len());
  for (name, value) in options {
    let Some(update) = parse_config_update(name, &config_text(value)) else {
      return json_error(&format!("invalid or unsupported option {name}"));
    };
    updates.push(update);
  }
  let mut output = state.output;
  for update in &updates {
    match update {
      ConfigUpdate::Mode(width, height) => {
        output.width = *width;
        output.height = *height;
      }
      ConfigUpdate::Position(x, y) => {
        output.x = *x;
        output.y = *y;
      }
      _ => {}
    }
    if output.validate().is_err() {
      return json_error("invalid combined output geometry");
    }
  }
  match apply_config_updates(state, updates) {
    Ok(()) => json_ok(),
    Err(error) => json_error(&error),
  }
}

pub(crate) fn respond(state: &mut CompositorState, message: &Value) -> Value {
  let Some(method) = message.get("method").and_then(Value::as_str) else {
    return json_error("missing method");
  };
  let data = message.get("data").unwrap_or(&Value::Null);
  match method {
    "window-rules/list-views" => Value::Array(
      state
        .core
        .window_ids()
        .map(|window| view_json(state, window))
        .collect(),
    ),
    "window-rules/list-outputs" => json!([output_json(state)]),
    "window-rules/view-info" => match find_view(state, data) {
      Ok(window) => {
        json!({"result": "ok", "info": view_json(state, window)})
      }
      Err(error) => error,
    },
    "window-rules/get-focused-view" => {
      let info = input::focused_window(state)
        .map_or(Value::Null, |window| view_json(state, window));
      json!({
          "result": "ok",
          "info": info,
      })
    }
    "window-rules/list-wsets" => json!([wset_json(state)]),
    "window-rules/get-focused-output" => {
      json!({"result": "ok", "info": output_json(state)})
    }
    "window-rules/output-info" | "window-rules/wset-info" => {
      if data.get("id").and_then(Value::as_u64) == Some(1) {
        if method.ends_with("output-info") {
          output_json(state)
        } else {
          wset_json(state)
        }
      } else {
        json_error("output or workspace set not found")
      }
    }
    "window-rules/get_cursor_position" | "window-rules/get-cursor-position" => {
      json!({
          "result": "ok",
          "pos": {"x": state.input.pointer_x, "y": state.input.pointer_y},
      })
    }
    "window-rules/focus-view" => match find_view(state, data) {
      Ok(window) if input::focus_window(state, window).is_some() => json_ok(),
      Ok(_) => json_error("view cannot be focused"),
      Err(error) => error,
    },
    "window-rules/close-view" => match find_view(state, data) {
      Ok(window) => {
        if shell::close_window(state, window).is_some() {
          json_ok()
        } else {
          json_error("view is not mapped")
        }
      }
      Err(error) => error,
    },
    "window-rules/configure-view" => configure_view(state, data),
    "window-rules/get-view-property" => get_view_property(state, data),
    "window-rules/set-view-property" => set_view_property(state, data),
    "vswitch/set-workspace" => set_workspace(state, data),
    "vswitch/send-view" => send_view(state, data),
    "wf/alpha/get-view-alpha" => get_view_alpha(state, data),
    "wf/alpha/set-view-alpha" => set_view_alpha(state, data),
    "wayfire/get-config-option" => get_config_option(state, data),
    "wayfire/list-config-options" => list_config_options(state),
    "wayfire/get-keyboard-state" => keyboard_state(state),
    "wayfire/set-config-options" => set_config_options(state, data),
    "wayfire/set-keyboard-state" => set_keyboard_state(state, data),
    "simple-tile/get-layout" => get_layout(state, data),
    "simple-tile/set-layout" => set_layout(state, data),
    "simple-tile/set-show-maximized" => set_show_maximized(state, data),
    "wm-actions/minimize"
    | "wm-actions/toggle_maximize"
    | "wm-actions/toggle_fullscreen"
    | "wm-actions/toggle_sticky"
    | "wm-actions/toggle_always_on_top"
    | "wm-actions/toggle_always_on_bottom"
    | "wm-actions/bring-to-front"
    | "wm-actions/send-to-back" => wm_activator(state, method, data),
    "wm-actions/set-minimized"
    | "wm-actions/set-fullscreen"
    | "wm-actions/set-sticky"
    | "wm-actions/set-always-on-top"
    | "wm-actions/set-always-on-bottom" => wm_action(state, method, data),
    "wm-actions/toggle_showdesktop" => {
      if state.show_desktop {
        shell::disable_show_desktop(state);
      } else {
        shell::enable_show_desktop(state);
      }
      json_ok()
    }
    "input/list-devices" => input_device::list_json(state),
    "input/configure-device" => input_device::configure(state, data),
    "stipc/feed_key" => feed_key(state, data),
    "stipc/feed_button" => feed_button(state, data),
    "stipc/get_display" => json!({
        "wayland": state.socket_name.to_string_lossy(),
        "xwayland": state.xwayland.display.as_deref().unwrap_or_default(),
    }),
    "stipc/get_xwayland_display" => json!({
        "result": "ok",
        "display": state.xwayland.display.as_deref().unwrap_or_default(),
    }),
    "stipc/get_xwayland_pid" => json!({
        "result": "ok",
        "pid": state.xwayland.pid_or_minus_one(),
    }),
    "stipc/layout_views" => layout_views(state, data),
    "stipc/move_cursor" => move_cursor(state, data),
    "stipc/ping" => json_ok(),
    "stipc/run" => run_command(state, data),
    "stipc/touch" => feed_touch(state, data),
    "stipc/touch_release" => release_touch(state, data),
    "scale/toggle" => {
      state.events.push(crate::Event::ToggleScale(false));
      json_ok()
    }
    "scale/toggle_all" => {
      state.events.push(crate::Event::ToggleScale(true));
      json_ok()
    }
    "expo/toggle" => {
      state.events.push(crate::Event::ToggleExpo);
      json_ok()
    }
    "invert/toggle" => {
      state.events.push(crate::Event::InvertToggle);
      json_ok()
    }
    _ if method.starts_with("vswitch/binding_")
      || method.starts_with("vswitch/with_win_")
      || method.starts_with("vswitch/send_win_") =>
    {
      vswitch_activator(state, method)
    }
    "list-methods" => json!({"methods": SUPPORTED_METHODS}),
    _ => json_error("unknown or unsupported method"),
  }
}

#[cfg(test)]
mod tests {
  use serde_json::{Value, json};

  use super::{SUPPORTED_METHODS, respond};
  use crate::{
    RuntimeBinding, RuntimeBindingMode,
    input_device::tests::{KEYBOARD, test_state},
    state::CompositorState,
  };

  fn call(state: &mut CompositorState, method: &str, data: Value) -> Value {
    respond(state, &json!({"method": method, "data": data}))
  }

  fn add_binding(state: &mut CompositorState, id: u64, binding: &str) {
    state.runtime_bindings.insert(
      id,
      RuntimeBinding {
        id,
        binding: super::normalize_runtime_binding(binding).unwrap(),
        command: None,
        call_method: None,
        call_data: None,
        mode: RuntimeBindingMode::Normal,
        exec_always: false,
        client_id: Some(7),
      },
    );
  }

  fn binding_events(state: &mut CompositorState) -> Vec<u64> {
    state
      .ipc_events
      .drain(..)
      .filter(|event| event["event"] == "command-binding")
      .filter_map(|event| event["binding-id"].as_u64())
      .collect()
  }

  fn press(state: &mut CompositorState, key: &str) {
    call(state, "stipc/feed_key", json!({"key": key, "state": true}));
    call(state, "stipc/feed_key", json!({"key": key, "state": false}));
  }

  #[test]
  fn list_methods_reports_79_sorted_methods_with_input_devices() {
    let mut state = test_state();
    let reply = call(&mut state, "list-methods", Value::Null);
    let methods = reply["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 79);
    assert_eq!(SUPPORTED_METHODS.len(), 79);
    assert!(SUPPORTED_METHODS.windows(2).all(|pair| pair[0] < pair[1]));
    for method in ["input/list-devices", "input/configure-device"] {
      assert!(methods.contains(&json!(method)), "{method}");
    }
  }

  #[test]
  fn input_methods_dispatch_over_ipc() {
    let mut state = test_state();
    let devices = call(&mut state, "input/list-devices", Value::Null);
    let devices = devices.as_array().unwrap();
    assert_eq!(devices.len(), 3);
    for (device, (id, kind)) in
      devices
        .iter()
        .zip([(1, "keyboard"), (2, "pointer"), (3, "touch")])
    {
      assert_eq!(device["id"], json!(id));
      assert_eq!(device["type"], json!(kind));
      assert_eq!(device["enabled"], json!(true));
      for field in ["name", "vendor", "product"] {
        assert!(device[field].is_string(), "{field}");
      }
    }
    let reply = call(
      &mut state,
      "input/configure-device",
      json!({"id": 3, "enabled": false}),
    );
    assert_eq!(reply, json!({"result": "ok"}));
    let devices = call(&mut state, "input/list-devices", Value::Null);
    assert_eq!(devices[2]["enabled"], json!(false));
    let reply = call(
      &mut state,
      "input/configure-device",
      json!({"id": 9, "enabled": true}),
    );
    assert_eq!(reply, json!({"error": "Unknown input device!"}));
  }

  #[test]
  fn stipc_cursor_motion_is_gated_by_the_pointer() {
    let mut state = test_state();
    let configure = |state: &mut CompositorState, enabled: bool| {
      call(
        state,
        "input/configure-device",
        json!({"id": 2, "enabled": enabled}),
      )
    };
    let position = |state: &mut CompositorState| {
      call(state, "window-rules/get-cursor-position", Value::Null)["pos"]
        .clone()
    };
    call(&mut state, "stipc/move_cursor", json!({"x": 100, "y": 50}));
    assert_eq!(position(&mut state), json!({"x": 100.0, "y": 50.0}));
    configure(&mut state, false);
    let reply =
      call(&mut state, "stipc/move_cursor", json!({"x": 300, "y": 200}));
    assert_eq!(reply, json!({"result": "ok"}));
    assert_eq!(position(&mut state), json!({"x": 100.0, "y": 50.0}));
    configure(&mut state, true);
    call(&mut state, "stipc/move_cursor", json!({"x": 300, "y": 200}));
    assert_eq!(position(&mut state), json!({"x": 300.0, "y": 200.0}));
  }

  #[test]
  fn stipc_key_fires_runtime_binding_unless_keyboard_disabled() {
    let mut state = test_state();
    add_binding(&mut state, 4, "KEY_F9");
    press(&mut state, "KEY_F9");
    assert_eq!(binding_events(&mut state), [4]);
    assert!(crate::input_device::held_keys(&state, KEYBOARD).is_empty());
    call(
      &mut state,
      "input/configure-device",
      json!({"id": 1, "enabled": false}),
    );
    press(&mut state, "KEY_F9");
    assert!(binding_events(&mut state).is_empty());
    call(
      &mut state,
      "input/configure-device",
      json!({"id": 1, "enabled": true}),
    );
    press(&mut state, "KEY_F9");
    assert_eq!(binding_events(&mut state), [4]);
  }

  #[test]
  fn ipc_reports_the_described_output_name() {
    let mut state = test_state();
    let outputs = call(&mut state, "window-rules/list-outputs", json!({}));
    assert_eq!(outputs[0]["name"], json!("HEADLESS-1"));
    state.set_output_info(crate::OutputInfo {
      name: "DP-1".into(),
      ..Default::default()
    });
    let outputs = call(&mut state, "window-rules/list-outputs", json!({}));
    assert_eq!(outputs[0]["name"], json!("DP-1"));
    let wsets = call(&mut state, "window-rules/list-wsets", json!({}));
    assert_eq!(wsets[0]["output-name"], json!("DP-1"));
  }

  #[test]
  fn stipc_key_fires_no_binding_while_the_session_is_inactive() {
    let mut state = test_state();
    add_binding(&mut state, 4, "KEY_F9");
    crate::input_device::set_session_active(&mut state, false);
    press(&mut state, "KEY_F9");
    assert!(binding_events(&mut state).is_empty());
    crate::input_device::set_session_active(&mut state, true);
    press(&mut state, "KEY_F9");
    assert_eq!(binding_events(&mut state), [4]);
  }

  #[test]
  fn stipc_keys_become_events_for_a_frontend() {
    let mut state = test_state();
    state.stipc_key_events = true;
    add_binding(&mut state, 4, "KEY_F9");
    call(
      &mut state,
      "input/configure-device",
      json!({"id": 1, "enabled": false}),
    );
    state.drain_events();
    press(&mut state, "KEY_F9");
    let keys = |state: &mut CompositorState| {
      state
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
          crate::Event::StipcKey {
            device,
            code,
            pressed,
          } => Some((device, code, pressed)),
          _ => None,
        })
        .collect::<Vec<_>>()
    };
    // The frontend matches bindings and gates the disabled keyboard.
    assert_eq!(
      keys(&mut state),
      [(KEYBOARD, 67, true), (KEYBOARD, 67, false)]
    );
    assert!(binding_events(&mut state).is_empty());
    assert!(crate::input_device::held_keys(&state, KEYBOARD).is_empty());
    crate::input_device::set_session_active(&mut state, false);
    press(&mut state, "KEY_F9");
    assert!(keys(&mut state).is_empty(), "dropped while inactive");
    let reply = call(&mut state, "stipc/feed_key", json!({"key": "KEY_NOPE"}));
    assert_eq!(reply, json!({"error": "failed to parse evdev key"}));
  }

  #[test]
  fn stipc_binding_honours_held_modifier_keys_and_modes() {
    let mut state = test_state();
    add_binding(&mut state, 1, "<super> KEY_X");
    press(&mut state, "KEY_X");
    assert!(binding_events(&mut state).is_empty());
    call(
      &mut state,
      "stipc/feed_key",
      json!({"key": "KEY_LEFTMETA", "state": true}),
    );
    press(&mut state, "KEY_X");
    assert_eq!(binding_events(&mut state), [1]);
    call(
      &mut state,
      "stipc/feed_key",
      json!({"key": "KEY_LEFTMETA", "state": false}),
    );
    state.runtime_bindings.get_mut(&1).unwrap().mode =
      RuntimeBindingMode::Release;
    call(
      &mut state,
      "stipc/feed_key",
      json!({"key": "KEY_LEFTMETA", "state": true}),
    );
    call(
      &mut state,
      "stipc/feed_key",
      json!({"key": "KEY_X", "state": true}),
    );
    assert!(binding_events(&mut state).is_empty());
    call(
      &mut state,
      "stipc/feed_key",
      json!({"key": "KEY_X", "state": false}),
    );
    assert_eq!(binding_events(&mut state), [1]);
  }

  #[test]
  fn disabling_keyboard_releases_stipc_held_keys() {
    let mut state = test_state();
    call(
      &mut state,
      "stipc/feed_key",
      json!({"key": "KEY_A", "state": true}),
    );
    assert_eq!(crate::input_device::held_keys(&state, KEYBOARD).len(), 1);
    call(
      &mut state,
      "input/configure-device",
      json!({"id": 1, "enabled": false}),
    );
    assert!(crate::input_device::held_keys(&state, KEYBOARD).is_empty());
  }

  #[test]
  fn stipc_buttons_and_touches_are_gated() {
    let mut state = test_state();
    call(
      &mut state,
      "input/configure-device",
      json!({"id": 2, "enabled": false}),
    );
    call(
      &mut state,
      "input/configure-device",
      json!({"id": 3, "enabled": false}),
    );
    let reply = call(
      &mut state,
      "stipc/feed_button",
      json!({"combo": "BTN_LEFT", "mode": "press"}),
    );
    assert_eq!(reply, json!({"result": "ok"}));
    assert!(crate::input_device::holds_nothing(&state));
    call(
      &mut state,
      "stipc/touch",
      json!({"finger": 0, "x": 10, "y": 10}),
    );
    assert!(crate::input_device::holds_nothing(&state));
  }
}
