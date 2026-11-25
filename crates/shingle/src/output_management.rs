//! wlr-output-management: heads, modes and configurations.
//!
//! Every manager gets one head for the output, with the backend's mode list
//! ([`crate::OutputInfo::modes`]) or, without one, a single mode that is the
//! output rectangle in physical pixels. An applied or tested configuration
//! changes position, scale and transform itself. A mode change goes to the
//! backend as a [`crate::OutputRequest`] when the backend lists modes, and
//! the configuration is answered when the backend finishes it
//! ([`finish_request`]); without a list, as in nested and headless, a
//! custom mode resizes the output directly.

use std::collections::{HashMap, HashSet};

use wayland_protocols_wlr::output_management::v1::server::{
  zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
  zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
  zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
  zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
  zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, ObjectId},
  protocol::wl_output,
};

use crate::{
  OutputPlacement, OutputRequest, OutputRequestId, RequestedMode,
  state::CompositorState,
};

#[derive(Default)]
pub(crate) struct OutputManagement {
  serial: u32,
  managers: HashMap<ObjectId, ZwlrOutputManagerV1>,
  heads: HashMap<ObjectId, Head>,
  modes: HashMap<ObjectId, Mode>,
  configurations: HashMap<ObjectId, Configuration>,
  configuration_heads: HashMap<ObjectId, ConfigurationHead>,
  /// Configurations waiting for the backend, by request.
  pending: HashMap<OutputRequestId, Pending>,
}

struct Head {
  resource: ZwlrOutputHeadV1,
  manager: ObjectId,
}

struct Mode {
  resource: ZwlrOutputModeV1,
  head: ObjectId,
  /// The index in the backend's mode list, or `None` for the single mode
  /// that stands for the output rectangle.
  index: Option<usize>,
}

struct Configuration {
  manager: ObjectId,
  serial: u32,
  used: bool,
  configured: HashSet<ObjectId>,
  disabled: HashSet<ObjectId>,
}

#[derive(Clone, Copy)]
enum ModeChoice {
  /// A mode object; `None` is the mode that stands for the rectangle.
  Listed(Option<usize>),
  Custom(u32, u32, u32),
}

struct ConfigurationHead {
  configuration: ObjectId,
  head: ObjectId,
  mode: Option<ModeChoice>,
  position: Option<(i32, i32)>,
  transform: Option<wl_output::Transform>,
  scale: Option<f64>,
  adaptive_sync: Option<bool>,
}

/// A configuration whose mode change the backend carries out.
struct Pending {
  resource: ZwlrOutputConfigurationV1,
  placement: OutputPlacement,
  test: bool,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrOutputManagerV1, ()>(4, ());
}

fn next_serial(state: &mut CompositorState) -> u32 {
  state.output_management.serial =
    state.output_management.serial.wrapping_add(1);
  if state.output_management.serial == 0 {
    state.output_management.serial = 1;
  }
  state.output_management.serial
}

/// The physical size of the mode that stands for the output rectangle.
fn rectangle_mode_size(state: &CompositorState) -> (i32, i32) {
  let (width, height) = crate::output_transform::physical_size(
    state.output.width,
    state.output.height,
    state.output_scale,
    state.output_transform,
  );
  (
    width.min(i32::MAX as u32) as i32,
    height.min(i32::MAX as u32) as i32,
  )
}

/// Sends the current mode, position, transform and scale to every head,
/// and `done` to every manager.
pub(crate) fn sync(state: &mut CompositorState) {
  if state.output_management.managers.is_empty() {
    return;
  }
  let serial = next_serial(state);
  let (width, height) = rectangle_mode_size(state);
  let current = state.output_info.current_mode;
  let output = state.output;
  for mode in state.output_management.modes.values() {
    let Some(head) = state.output_management.heads.get(&mode.head) else {
      continue;
    };
    match mode.index {
      None => {
        mode.resource.size(width, height);
        mode
          .resource
          .refresh(state.output_info.refresh_mhz.min(i32::MAX as u32) as i32);
        head.resource.current_mode(&mode.resource);
      }
      Some(index) if Some(index) == current => {
        head.resource.current_mode(&mode.resource);
      }
      Some(_) => {}
    }
  }
  for head in state.output_management.heads.values() {
    head.resource.position(output.x, output.y);
    head.resource.transform(state.output_transform);
    head.resource.scale(state.output_scale);
  }
  for manager in state.output_management.managers.values() {
    manager.done(serial);
  }
}

/// Replaces the mode objects of every head after the backend's mode list
/// changed, then sends the current state.
pub(crate) fn modes_changed(
  state: &mut CompositorState,
  handle: &DisplayHandle,
) {
  let heads: Vec<(ObjectId, ZwlrOutputHeadV1)> = state
    .output_management
    .heads
    .iter()
    .map(|(id, head)| (id.clone(), head.resource.clone()))
    .collect();
  for (id, head) in heads {
    state.output_management.modes.retain(|_, mode| {
      if mode.head != id {
        return true;
      }
      mode.resource.finished();
      false
    });
    if let Some(client) = head.client() {
      send_modes(state, handle, &client, &head);
    }
  }
  sync(state);
}

/// Finishes every head, which stood for an output that was replaced, and
/// announces a head for the new output to every manager.
pub(crate) fn output_replaced(
  state: &mut CompositorState,
  handle: &DisplayHandle,
) {
  for (_, mode) in state.output_management.modes.drain() {
    mode.resource.finished();
  }
  for (_, head) in state.output_management.heads.drain() {
    head.resource.finished();
  }
  let managers: Vec<ZwlrOutputManagerV1> =
    state.output_management.managers.values().cloned().collect();
  for manager in managers {
    if let Some(client) = manager.client() {
      announce_head(state, handle, &client, &manager);
    }
  }
  sync(state);
}

/// Creates the mode objects of `head`: the backend's modes, or the mode
/// that stands for the output rectangle.
fn send_modes(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  client: &Client,
  head: &ZwlrOutputHeadV1,
) {
  let version = head.version().min(3);
  let listed: Vec<(Option<usize>, crate::OutputModeInfo)> =
    if state.output_info.modes.is_empty() {
      let (width, height) = rectangle_mode_size(state);
      vec![(
        None,
        crate::OutputModeInfo {
          width: width as u32,
          height: height as u32,
          refresh_mhz: state.output_info.refresh_mhz,
          preferred: true,
        },
      )]
    } else {
      state
        .output_info
        .modes
        .iter()
        .enumerate()
        .map(|(index, mode)| (Some(index), *mode))
        .collect()
    };
  for (index, info) in listed {
    let Ok(mode) = client
      .create_resource::<ZwlrOutputModeV1, (), CompositorState>(
        handle,
        version,
        (),
      )
    else {
      return;
    };
    head.mode(&mode);
    mode.size(
      info.width.min(i32::MAX as u32) as i32,
      info.height.min(i32::MAX as u32) as i32,
    );
    if info.refresh_mhz > 0 {
      mode.refresh(info.refresh_mhz.min(i32::MAX as u32) as i32);
    }
    if info.preferred {
      mode.preferred();
    }
    state.output_management.modes.insert(
      mode.id(),
      Mode {
        resource: mode,
        head: head.id(),
        index,
      },
    );
  }
}

/// Announces the output's head with its description and modes to
/// `manager`. The caller sends the current state and `done`.
fn announce_head(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  client: &Client,
  manager: &ZwlrOutputManagerV1,
) {
  let version = manager.version();
  let Ok(head) = client
    .create_resource::<ZwlrOutputHeadV1, (), CompositorState>(
      handle,
      version,
      (),
    )
  else {
    return;
  };
  manager.head(&head);
  let info = &state.output_info;
  head.name(info.name.clone());
  head.description(info.description());
  let (width_mm, height_mm) = info.physical_size_mm;
  if width_mm > 0 && height_mm > 0 {
    head.physical_size(
      width_mm.min(i32::MAX as u32) as i32,
      height_mm.min(i32::MAX as u32) as i32,
    );
  }
  if version >= 2 {
    head.make(info.make.clone());
    head.model(info.model.clone());
    if let Some(serial) = &info.serial {
      head.serial_number(serial.clone());
    }
  }
  head.enabled(1);
  if version >= 4 {
    head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
  }
  state.output_management.heads.insert(
    head.id(),
    Head {
      resource: head.clone(),
      manager: manager.id(),
    },
  );
  send_modes(state, handle, client, &head);
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwlr_output_manager_v1");

  fn bind(
    state: &mut Self,
    handle: &DisplayHandle,
    client: &Client,
    resource: New<ZwlrOutputManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let manager = data_init.init(resource, ());
    state
      .output_management
      .managers
      .insert(manager.id(), manager.clone());
    announce_head(state, handle, client, &manager);
    sync(state);
  }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrOutputManagerV1,
    request: zwlr_output_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
        let configuration = data_init.init(id, ());
        state.output_management.configurations.insert(
          configuration.id(),
          Configuration {
            manager: resource.id(),
            serial,
            used: false,
            configured: HashSet::new(),
            disabled: HashSet::new(),
          },
        );
      }
      zwlr_output_manager_v1::Request::Stop => {
        resource.finished();
        state.output_management.managers.remove(&resource.id());
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwlrOutputManagerV1,
    _data: &(),
  ) {
    state.output_management.managers.remove(&resource.id());
  }
}

fn remove_head(state: &mut CompositorState, head: &ObjectId) {
  state.output_management.heads.remove(head);
  state
    .output_management
    .modes
    .retain(|_, mode| &mode.head != head);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrOutputHeadV1,
    _request: zwlr_output_head_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    remove_head(state, &resource.id());
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwlrOutputHeadV1,
    _data: &(),
  ) {
    remove_head(state, &resource.id());
  }
}

impl Dispatch<ZwlrOutputModeV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrOutputModeV1,
    _request: zwlr_output_mode_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    state.output_management.modes.remove(&resource.id());
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwlrOutputModeV1,
    _data: &(),
  ) {
    state.output_management.modes.remove(&resource.id());
  }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrOutputConfigurationV1,
    request: zwlr_output_configuration_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_output_configuration_v1::Request::Destroy => {
        destroy_configuration(state, resource.id())
      }
      zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
        let configuration_head = data_init.init(id, ());
        enable_head(state, resource, head, configuration_head);
      }
      zwlr_output_configuration_v1::Request::DisableHead { head } => {
        if !mark_configurable(state, resource, &head.id()) {
          return;
        }
        if let Some(configuration) = state
          .output_management
          .configurations
          .get_mut(&resource.id())
        {
          configuration.disabled.insert(head.id());
        }
      }
      zwlr_output_configuration_v1::Request::Apply => {
        finish(state, resource, false)
      }
      zwlr_output_configuration_v1::Request::Test => {
        finish(state, resource, true)
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwlrOutputConfigurationV1,
    _data: &(),
  ) {
    destroy_configuration(state, resource.id());
  }
}

fn mark_configurable(
  state: &mut CompositorState,
  resource: &ZwlrOutputConfigurationV1,
  head: &ObjectId,
) -> bool {
  let Some(configuration) = state
    .output_management
    .configurations
    .get_mut(&resource.id())
  else {
    return false;
  };
  if configuration.used {
    resource.post_error(
      zwlr_output_configuration_v1::Error::AlreadyUsed,
      "configuration was already tested or applied",
    );
    return false;
  }
  if !configuration.configured.insert(head.clone()) {
    resource.post_error(
      zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
      "head was already configured",
    );
    return false;
  }
  true
}

fn enable_head(
  state: &mut CompositorState,
  resource: &ZwlrOutputConfigurationV1,
  head: ZwlrOutputHeadV1,
  configuration_head: ZwlrOutputConfigurationHeadV1,
) {
  if !mark_configurable(state, resource, &head.id()) {
    return;
  }
  state.output_management.configuration_heads.insert(
    configuration_head.id(),
    ConfigurationHead {
      configuration: resource.id(),
      head: head.id(),
      mode: None,
      position: None,
      transform: None,
      scale: None,
      adaptive_sync: None,
    },
  );
}

fn finish(
  state: &mut CompositorState,
  resource: &ZwlrOutputConfigurationV1,
  test: bool,
) {
  let Some(configuration) = state
    .output_management
    .configurations
    .get_mut(&resource.id())
  else {
    return;
  };
  if configuration.used {
    resource.post_error(
      zwlr_output_configuration_v1::Error::AlreadyUsed,
      "configuration was already tested or applied",
    );
    return;
  }
  configuration.used = true;
  let serial = configuration.serial;
  let manager = configuration.manager.clone();
  let configured = configuration.configured.clone();
  let disabled = configuration.disabled.clone();
  if serial != state.output_management.serial {
    resource.cancelled();
    return;
  }
  let current_heads = state
    .output_management
    .heads
    .iter()
    .filter(|(_, head)| head.manager == manager)
    .map(|(id, _)| id.clone())
    .collect::<HashSet<_>>();
  if configured != current_heads {
    resource.post_error(
      zwlr_output_configuration_v1::Error::UnconfiguredHead,
      "configuration must include every current head",
    );
    return;
  }
  // Shingle drives one output; turning it off is DPMS's job.
  if !disabled.is_empty() {
    resource.failed();
    return;
  }
  let mut placement = OutputPlacement {
    x: state.output.x,
    y: state.output.y,
    width: 0,
    height: 0,
    scale: state.output_scale,
    transform: state.output_transform,
    refresh_mhz: 0,
  };
  let mut mode = None;
  for head in &configured {
    let Some(configuration_head) =
      state.output_management.configuration_heads.values().find(
        |configuration_head| {
          configuration_head.configuration == resource.id()
            && &configuration_head.head == head
        },
      )
    else {
      continue;
    };
    if configuration_head.mode.is_some() {
      mode = configuration_head.mode;
    }
    if let Some((x, y)) = configuration_head.position {
      placement.x = x;
      placement.y = y;
    }
    if let Some(requested) = configuration_head.scale {
      placement.scale = requested;
    }
    if let Some(requested) = configuration_head.transform {
      placement.transform = requested;
    }
    if configuration_head.adaptive_sync == Some(true) {
      resource.failed();
      return;
    }
  }
  match requested_mode(state, mode) {
    Some(mode) => {
      let id = OutputRequestId(state.next_output_request);
      state.next_output_request += 1;
      state.output_requests.push(OutputRequest {
        id,
        mode,
        test_only: test,
      });
      state.output_management.pending.insert(
        id,
        Pending {
          resource: resource.clone(),
          placement,
          test,
        },
      );
    }
    None => apply_locally(state, resource, placement, mode, test),
  }
}

/// The mode change the backend has to make, if the backend lists modes and
/// `mode` is not the current one.
fn requested_mode(
  state: &CompositorState,
  mode: Option<ModeChoice>,
) -> Option<RequestedMode> {
  let info = &state.output_info;
  if info.modes.is_empty() {
    return None;
  }
  match mode? {
    ModeChoice::Listed(None) => None,
    ModeChoice::Listed(Some(index)) => {
      (Some(index) != info.current_mode).then_some(RequestedMode::Listed(index))
    }
    ModeChoice::Custom(width, height, refresh_mhz) => {
      let current = info.current().is_some_and(|current| {
        (current.width, current.height) == (width, height)
          && (refresh_mhz == 0 || current.refresh_mhz == refresh_mhz)
      });
      (!current).then_some(RequestedMode::Custom {
        width,
        height,
        refresh_mhz,
      })
    }
  }
}

/// Tests or applies a configuration without the backend: the placement,
/// and a custom mode as the new output size and refresh rate.
fn apply_locally(
  state: &mut CompositorState,
  resource: &ZwlrOutputConfigurationV1,
  placement: OutputPlacement,
  mode: Option<ModeChoice>,
  test: bool,
) {
  let mut output = state.output;
  output.x = placement.x;
  output.y = placement.y;
  if crate::output_transform::is_quarter_turn(state.output_transform)
    != crate::output_transform::is_quarter_turn(placement.transform)
  {
    std::mem::swap(&mut output.width, &mut output.height);
  }
  let mut refresh = None;
  if let Some(ModeChoice::Custom(width, height, refresh_mhz)) = mode {
    (output.width, output.height) = crate::output_transform::logical_size(
      width,
      height,
      placement.scale,
      placement.transform,
    );
    refresh = (refresh_mhz > 0).then_some(refresh_mhz);
  }
  if test {
    resource.succeeded();
    return;
  }
  if let Some(refresh_mhz) = refresh {
    state.output_info.refresh_mhz = refresh_mhz;
  }
  state.set_output_transform(placement.transform);
  match state
    .set_output_scale(placement.scale)
    .and_then(|()| state.set_output(output))
  {
    Ok(()) => resource.succeeded(),
    Err(_) => resource.failed(),
  }
}

/// Answers the configuration behind request `id` once the backend tested
/// or made its mode change: `failed` if it could not, else `succeeded`,
/// after applying position, scale and transform around the new mode's
/// size unless the client only tested.
pub(crate) fn finish_request(
  state: &mut CompositorState,
  id: OutputRequestId,
  ok: bool,
) {
  let Some(pending) = state.output_management.pending.remove(&id) else {
    return;
  };
  if !pending.resource.is_alive() {
    return;
  }
  if !ok {
    pending.resource.failed();
    return;
  }
  if pending.test {
    pending.resource.succeeded();
    return;
  }
  let physical = crate::output::physical_size(state);
  match crate::output::place(state, pending.placement, physical) {
    Ok(()) => pending.resource.succeeded(),
    Err(_) => pending.resource.failed(),
  }
}

fn destroy_configuration(state: &mut CompositorState, id: ObjectId) {
  state.output_management.configurations.remove(&id);
  state
    .output_management
    .configuration_heads
    .retain(|_, head| head.configuration != id);
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrOutputConfigurationHeadV1,
    request: zwlr_output_configuration_head_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
        let head = state
          .output_management
          .configuration_heads
          .get(&resource.id())
          .map(|head| head.head.clone());
        let entry = state
          .output_management
          .modes
          .get(&mode.id())
          .filter(|entry| Some(&entry.head) == head.as_ref());
        let Some(index) = entry.map(|entry| entry.index) else {
          resource.post_error(
            zwlr_output_configuration_head_v1::Error::InvalidMode,
            "mode does not belong to this head",
          );
          return;
        };
        set_property(
          state,
          resource,
          |head| &mut head.mode,
          Some(ModeChoice::Listed(index)),
        );
      }
      zwlr_output_configuration_head_v1::Request::SetCustomMode {
        width,
        height,
        refresh,
      } => {
        if width <= 0 || height <= 0 || refresh < 0 {
          resource.post_error(
            zwlr_output_configuration_head_v1::Error::InvalidCustomMode,
            "custom mode dimensions and refresh are invalid",
          );
          return;
        }
        set_property(
          state,
          resource,
          |head| &mut head.mode,
          Some(ModeChoice::Custom(
            width as u32,
            height as u32,
            refresh as u32,
          )),
        );
      }
      zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
        set_property(state, resource, |head| &mut head.position, Some((x, y)));
      }
      zwlr_output_configuration_head_v1::Request::SetTransform {
        transform,
      } => {
        let WEnum::Value(transform) = transform else {
          resource.post_error(
            zwlr_output_configuration_head_v1::Error::InvalidTransform,
            "unknown output transform",
          );
          return;
        };
        set_property(
          state,
          resource,
          |head| &mut head.transform,
          Some(transform),
        );
      }
      zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
        if !scale.is_finite() || scale <= 0.0 {
          resource.post_error(
            zwlr_output_configuration_head_v1::Error::InvalidScale,
            "output scale must be positive",
          );
          return;
        }
        set_property(state, resource, |head| &mut head.scale, Some(scale));
      }
      zwlr_output_configuration_head_v1::Request::SetAdaptiveSync {
        state: adaptive,
      } => {
        let WEnum::Value(adaptive) = adaptive else {
          resource.post_error(
            zwlr_output_configuration_head_v1::Error::InvalidAdaptiveSyncState,
            "unknown adaptive sync state",
          );
          return;
        };
        let enabled = match adaptive {
          zwlr_output_head_v1::AdaptiveSyncState::Disabled => false,
          zwlr_output_head_v1::AdaptiveSyncState::Enabled => true,
          _ => return,
        };
        set_property(
          state,
          resource,
          |head| &mut head.adaptive_sync,
          Some(enabled),
        );
      }
      _ => {}
    }
  }
}

fn set_property<T>(
  state: &mut CompositorState,
  resource: &ZwlrOutputConfigurationHeadV1,
  field: impl FnOnce(&mut ConfigurationHead) -> &mut Option<T>,
  value: Option<T>,
) {
  let Some(head) = state
    .output_management
    .configuration_heads
    .get_mut(&resource.id())
  else {
    return;
  };
  let field = field(head);
  if field.is_some() {
    resource.post_error(
      zwlr_output_configuration_head_v1::Error::AlreadySet,
      "head property was already set",
    );
    return;
  }
  *field = value;
}

pub(crate) fn cleanup_client(state: &mut CompositorState, client: &ClientId) {
  state.output_management.pending.retain(|_, pending| {
    pending
      .resource
      .client()
      .is_none_or(|item| item.id() != *client)
  });
  state.output_management.managers.retain(|_, resource| {
    resource.client().is_none_or(|item| item.id() != *client)
  });
  state.output_management.heads.retain(|_, head| {
    head
      .resource
      .client()
      .is_none_or(|item| item.id() != *client)
  });
  state.output_management.modes.retain(|_, mode| {
    mode
      .resource
      .client()
      .is_none_or(|item| item.id() != *client)
  });
  state.output_management.configurations.retain(|id, _| {
    state
      .output_management
      .configuration_heads
      .values()
      .all(|head| &head.configuration != id)
  });
}
