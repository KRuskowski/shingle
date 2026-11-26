//! wlr-output-management: heads, modes and configurations.
//!
//! Every manager gets one head per output, with the backend's mode list
//! ([`crate::OutputInfo::modes`]) or, without one, a single mode that is the
//! output rectangle in physical pixels. Heads come and go with outputs;
//! an output turned off by a configuration stays listed as a disabled head
//! and can be turned on again (docs/design/multi-output.md, section 8). A
//! configuration must name every head. Turning an output off removes it
//! under `output_layout.unplug`, and turning every output off fails. An
//! applied or tested configuration changes position, scale and transform
//! itself, and config reloads leave the outputs it configured alone until
//! Shingle restarts, as Wayfire's `is_externally_managed`. A mode change
//! goes to the
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

use shingle_config::OutputPosition;
use shingle_core::OutputId;

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
  /// The output the head stands for.
  output: OutputId,
  /// Whether the head was last told it is enabled.
  enabled: bool,
}

/// What a head describes: an output, or one turned off.
struct Source {
  info: crate::OutputInfo,
  /// The size of the mode that stands for the output, in physical pixels.
  mode_size: (i32, i32),
  enabled: bool,
}

/// The output called `id`, live or turned off, as its heads describe it.
fn source(state: &CompositorState, id: &OutputId) -> Option<Source> {
  if let Some(output) = state.outputs.get(id) {
    return Some(Source {
      info: output.info.clone(),
      mode_size: output.mode_size(),
      enabled: true,
    });
  }
  let off = state.outputs.disabled(id)?;
  Some(Source {
    info: off.info.clone(),
    mode_size: (
      off.physical.0.min(i32::MAX as u32) as i32,
      off.physical.1.min(i32::MAX as u32) as i32,
    ),
    enabled: false,
  })
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
  /// The placement of each configured output, applied once the backend
  /// made the mode change.
  placements: Vec<(OutputId, OutputPlacement)>,
  test: bool,
}

/// What a configuration asks of one output.
struct Change {
  output: OutputId,
  placement: OutputPlacement,
  mode: Option<ModeChoice>,
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

/// Sends the current mode, position, transform and scale of its output to
/// every head, and `done` to every manager.
pub(crate) fn sync(state: &mut CompositorState) {
  if state.output_management.managers.is_empty() {
    return;
  }
  let serial = next_serial(state);
  for mode in state.output_management.modes.values() {
    let Some(head) = state.output_management.heads.get(&mode.head) else {
      continue;
    };
    let Some(output) = state.outputs.get(&head.output) else {
      continue;
    };
    match mode.index {
      None => {
        let (width, height) = output.mode_size();
        mode.resource.size(width, height);
        mode
          .resource
          .refresh(output.info.refresh_mhz.min(i32::MAX as u32) as i32);
        head.resource.current_mode(&mode.resource);
      }
      Some(index) if Some(index) == output.info.current_mode => {
        head.resource.current_mode(&mode.resource);
      }
      Some(_) => {}
    }
  }
  for head in state.output_management.heads.values() {
    let Some(output) = state.outputs.get(&head.output) else {
      continue;
    };
    if !head.enabled {
      continue;
    }
    head.resource.position(output.rect.x, output.rect.y);
    head.resource.transform(output.transform);
    head.resource.scale(output.scale);
  }
  for manager in state.output_management.managers.values() {
    manager.done(serial);
  }
}

/// Replaces the mode objects of output `id`'s heads after the backend's
/// mode list changed, then sends the current state.
pub(crate) fn modes_changed(state: &mut CompositorState, id: &OutputId) {
  let Some(handle) = state.display.clone() else {
    return;
  };
  let heads: Vec<(ObjectId, ZwlrOutputHeadV1)> = state
    .output_management
    .heads
    .iter()
    .filter(|(_, head)| head.output == *id)
    .map(|(head_id, head)| (head_id.clone(), head.resource.clone()))
    .collect();
  for (head_id, head) in heads {
    state.output_management.modes.retain(|_, mode| {
      if mode.head != head_id {
        return true;
      }
      mode.resource.finished();
      false
    });
    if let Some(client) = head.client() {
      send_modes(state, &handle, &client, &head, id);
    }
  }
  sync(state);
}

/// Finishes the heads of output `id`, which is gone or renamed, and their
/// modes, then sends the current state. The heads of an output that a
/// configuration turned off stay, disabled.
pub(crate) fn output_removed(state: &mut CompositorState, id: &OutputId) {
  if state.outputs.disabled(id).is_some() {
    for head in state.output_management.heads.values_mut() {
      if head.output == *id && head.enabled {
        head.resource.enabled(0);
        head.enabled = false;
      }
    }
    sync(state);
    return;
  }
  let heads = state
    .output_management
    .heads
    .iter()
    .filter(|(_, head)| head.output == *id)
    .map(|(head_id, _)| head_id.clone())
    .collect::<Vec<_>>();
  for head_id in heads {
    state.output_management.modes.retain(|_, mode| {
      if mode.head != head_id {
        return true;
      }
      mode.resource.finished();
      false
    });
    if let Some(head) = state.output_management.heads.remove(&head_id) {
      head.resource.finished();
    }
  }
  sync(state);
}

/// Announces a head for the new output `id` to every manager, or enables
/// the head it has from when it was turned off.
pub(crate) fn output_added(state: &mut CompositorState, id: &OutputId) {
  let Some(handle) = state.display.clone() else {
    return;
  };
  let managers: Vec<ZwlrOutputManagerV1> =
    state.output_management.managers.values().cloned().collect();
  for manager in managers {
    let existing = state
      .output_management
      .heads
      .values_mut()
      .find(|head| head.manager == manager.id() && head.output == *id);
    if let Some(head) = existing {
      if !head.enabled {
        head.resource.enabled(1);
        head.enabled = true;
      }
      continue;
    }
    if let Some(client) = manager.client() {
      announce_head(state, &handle, &client, &manager, id);
    }
  }
  sync(state);
}

/// Creates the mode objects of `head`: the backend's modes of output `id`,
/// or the mode that stands for its rectangle.
fn send_modes(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  client: &Client,
  head: &ZwlrOutputHeadV1,
  id: &OutputId,
) {
  let Some(output) = source(state, id) else {
    return;
  };
  let version = head.version().min(3);
  let listed: Vec<(Option<usize>, crate::OutputModeInfo)> =
    if output.info.modes.is_empty() {
      let (width, height) = output.mode_size;
      vec![(
        None,
        crate::OutputModeInfo {
          width: width as u32,
          height: height as u32,
          refresh_mhz: output.info.refresh_mhz,
          preferred: true,
        },
      )]
    } else {
      output
        .info
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

/// Announces output `id`'s head with its description and modes to
/// `manager`. The caller sends the current state and `done`.
fn announce_head(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  client: &Client,
  manager: &ZwlrOutputManagerV1,
  id: &OutputId,
) {
  let Some(Source { info, enabled, .. }) = source(state, id) else {
    return;
  };
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
  head.enabled(i32::from(enabled));
  if version >= 4 {
    head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
  }
  state.output_management.heads.insert(
    head.id(),
    Head {
      resource: head.clone(),
      manager: manager.id(),
      output: id.clone(),
      enabled,
    },
  );
  send_modes(state, handle, client, &head, id);
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
    let ids = state
      .outputs
      .iter()
      .map(|output| output.id.clone())
      .chain(
        state
          .outputs
          .disabled
          .iter()
          .map(|off| OutputId::new(off.info.name.clone())),
      )
      .collect::<Vec<_>>();
    for id in &ids {
      announce_head(state, handle, client, &manager, id);
    }
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
  let mut changes = Vec::new();
  let mut turn_off = Vec::new();
  let mut turn_on = Vec::new();
  for head in &configured {
    let Some(name) = state
      .output_management
      .heads
      .get(head)
      .map(|head| head.output.clone())
    else {
      continue;
    };
    let requested = state
      .output_management
      .configuration_heads
      .values()
      .find(|configuration_head| {
        configuration_head.configuration == resource.id()
          && &configuration_head.head == head
      })
      .map(|head| (head.mode, head.position, head.scale, head.transform));
    let adaptive = state.output_management.configuration_heads.values().any(
      |configuration_head| {
        configuration_head.configuration == resource.id()
          && &configuration_head.head == head
          && configuration_head.adaptive_sync == Some(true)
      },
    );
    if adaptive {
      resource.failed();
      return;
    }
    // A backend with a mode list turns its connectors on and off itself,
    // which comes with MO-7.
    let listed = |info: &crate::OutputInfo| !info.modes.is_empty();
    if disabled.contains(head) {
      if let Some(output) = state.outputs.get(&name) {
        if listed(&output.info) {
          resource.failed();
          return;
        }
        turn_off.push(name);
      }
      continue;
    }
    let (current, kind) = if let Some(output) = state.outputs.get(&name) {
      ((output.position, output.scale, output.transform), None)
    } else if let Some(off) = state.outputs.disabled(&name) {
      if listed(&off.info) {
        resource.failed();
        return;
      }
      ((off.position, off.scale, off.transform), Some(off.clone()))
    } else {
      continue;
    };
    let mut change = Change {
      output: name,
      placement: OutputPlacement {
        position: current.0,
        width: 0,
        height: 0,
        scale: current.1,
        transform: current.2,
        refresh_mhz: 0,
      },
      mode: None,
    };
    if let Some((mode, position, scale, transform)) = requested {
      change.mode = mode;
      if let Some((x, y)) = position {
        change.placement.position = OutputPosition::Fixed { x, y };
      }
      if let Some(requested) = scale {
        change.placement.scale = requested;
      }
      if let Some(requested) = transform {
        change.placement.transform = requested;
      }
    }
    match kind {
      Some(off) => turn_on.push((change, off)),
      None => changes.push(change),
    }
  }
  // Turning every output off fails, as turning the only one off did.
  let staying = state
    .outputs
    .iter()
    .filter(|output| !output.dark && !turn_off.contains(&output.id))
    .count();
  if staying + turn_on.len() == 0 {
    resource.failed();
    return;
  }
  let requests = changes
    .iter()
    .filter_map(|change| requested_mode(state, &change.output, change.mode))
    .collect::<Vec<_>>();
  let toggles = Toggles { turn_off, turn_on };
  match requests.as_slice() {
    [] => apply_locally(state, resource, changes, toggles, test),
    [mode] if toggles.is_empty() => {
      let id = OutputRequestId(state.next_output_request);
      state.next_output_request += 1;
      state.output_requests.push(OutputRequest {
        id,
        mode: *mode,
        test_only: test,
      });
      state.output_management.pending.insert(
        id,
        Pending {
          resource: resource.clone(),
          placements: changes
            .into_iter()
            .map(|change| (change.output, change.placement))
            .collect(),
          test,
        },
      );
    }
    // A backend changes the mode of one output per configuration, and
    // never with outputs turned on or off, until requests name their
    // output (MO-7).
    _ => resource.failed(),
  }
}

/// The outputs a configuration turns off, and those it turns on with what
/// it asks of them.
struct Toggles {
  turn_off: Vec<OutputId>,
  turn_on: Vec<(Change, crate::output::DisabledOutput)>,
}

impl Toggles {
  fn is_empty(&self) -> bool {
    self.turn_off.is_empty() && self.turn_on.is_empty()
  }
}

/// The mode change the backend has to make on output `id`, if the backend
/// lists its modes and `mode` is not the current one.
fn requested_mode(
  state: &CompositorState,
  id: &OutputId,
  mode: Option<ModeChoice>,
) -> Option<RequestedMode> {
  let info = &state.outputs.get(id)?.info;
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

/// Tests or applies a configuration without the backend: outputs turned
/// off are removed under `output_layout.unplug` and listed as disabled
/// heads, outputs turned on are added again, and each output gets its
/// placement, and a custom mode as its new physical size and refresh rate.
/// As in Wayfire, the physical size stays across scale and transform
/// changes, and the logical size follows from it.
fn apply_locally(
  state: &mut CompositorState,
  resource: &ZwlrOutputConfigurationV1,
  changes: Vec<Change>,
  toggles: Toggles,
  test: bool,
) {
  if test {
    resource.succeeded();
    return;
  }
  let mut ok = true;
  for id in &toggles.turn_off {
    let (Some(output), Some(index)) =
      (state.outputs.get(id), state.outputs.index_of(id))
    else {
      continue;
    };
    let off = crate::output::DisabledOutput::new(output, index);
    state.outputs.disabled.push(off);
    ok &= crate::output::remove(state, id).is_ok();
  }
  let mut placed = Vec::new();
  for (change, off) in toggles.turn_on {
    let mut info = off.info;
    let mut physical = off.physical;
    if let Some(ModeChoice::Custom(width, height, refresh_mhz)) = change.mode {
      physical = (width, height);
      if refresh_mhz > 0 {
        info.refresh_mhz = refresh_mhz;
      }
    }
    match crate::output::add(state, info, physical, off.kind) {
      Ok(id) => {
        // Back at its place in the add order, so the first output stays
        // first and automatic positions are as before.
        state.outputs.move_to(&id, off.index);
        crate::output::relayout_and_notify(state);
        placed.push((change.output, change.placement, physical));
      }
      Err(error) => {
        eprintln!("shingle: turning an output on failed: {error}");
        ok = false;
      }
    }
  }
  for change in changes {
    let mut physical = crate::output::physical_size(state, &change.output);
    if let Some(ModeChoice::Custom(width, height, refresh_mhz)) = change.mode {
      physical = (width, height);
      if refresh_mhz > 0
        && let Some(output) = state.outputs.get_mut(&change.output)
      {
        output.info.refresh_mhz = refresh_mhz;
      }
    }
    placed.push((change.output, change.placement, physical));
  }
  for (output, placement, physical) in placed {
    ok &= crate::output::place(state, &output, placement, physical).is_ok();
    if let Some(output) = state.outputs.get_mut(&output) {
      output.managed = true;
    }
  }
  if ok {
    resource.succeeded();
  } else {
    resource.failed();
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
  let mut ok = true;
  for (output, placement) in pending.placements {
    let physical = crate::output::physical_size(state, &output);
    ok &= crate::output::place(state, &output, placement, physical).is_ok();
    if let Some(output) = state.outputs.get_mut(&output) {
      output.managed = true;
    }
  }
  if ok {
    pending.resource.succeeded();
  } else {
    pending.resource.failed();
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
