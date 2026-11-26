//! The output as clients see it: its description, its modes, where the
//! configuration places it, and mode changes a backend carries out.
//!
//! A backend describes the physical output with [`OutputInfo`]
//! ([`crate::Compositor::set_output_info`]). When the name changes, as when
//! a native output is replaced by another connector, [`rename`] renames the
//! core output, so windows keep their tiling trees, and replaces the
//! `wl_output` global, because the protocols send an output's name once per
//! binding: clients see the old global go and a new one appear, and every
//! object tied to the old output (xdg-output, power and gamma controls,
//! output-management heads, workspace groups) is finished.
//!
//! A backend that lists modes in [`OutputInfo::modes`] chooses modes
//! itself: wlr-output-management requests that change the mode become
//! [`OutputRequest`]s, which the backend tests (for example with a KMS
//! `TEST_ONLY` commit) and answers with
//! [`crate::Compositor::finish_output_request`]. Without a mode list, as in
//! nested and headless, the compositor applies custom modes itself.

mod hotplug;
mod registry;

use std::{
  collections::BTreeMap,
  time::{Duration, Instant},
};

pub(crate) use hotplug::{add, park_restored, remove, restore};
pub use registry::OutputKind;
pub(crate) use registry::{DisabledOutput, OutputState, Outputs};
use shingle_config::OutputPosition;
use shingle_core::{OutputId, Rect};
use wayland_server::{
  DisplayHandle, Resource,
  protocol::wl_output::{self, WlOutput},
};

use crate::state::CompositorState;

/// How long a replaced `wl_output` global stays bindable, though no longer
/// advertised, before it is removed. A client that saw the old global and
/// binds it meanwhile gets an inert output instead of a protocol error;
/// wlroots waits as long (`wlr_global_destroy_safe`).
pub(crate) const RETIRED_GLOBAL_DELAY: Duration = Duration::from_secs(5);

/// One mode of the output, as wlr-output-management lists it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputModeInfo {
  /// Width in physical pixels.
  pub width: u32,
  /// Height in physical pixels.
  pub height: u32,
  /// Refresh rate in millihertz, or zero when unknown.
  pub refresh_mhz: u32,
  /// Whether the display prefers this mode.
  pub preferred: bool,
}

/// What clients are told about the physical output: its identity, size,
/// refresh rate and modes. A backend describes its output with
/// [`crate::Compositor::set_output_info`]; nested and headless compositors
/// keep the default, a 60 Hz output named [`crate::OUTPUT_NAME`] without a
/// mode list.
///
/// The output's position and logical size still come from
/// [`crate::Compositor::set_output`], and its scale and transform from
/// their own setters. Without a mode list, the mode clients see is the
/// output rectangle in physical pixels at `refresh_mhz`.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
  /// Connector name, such as `DP-1`. Reported as the `wl_output`,
  /// xdg-output, output-management and IPC output name, and the core's
  /// output id.
  pub name: String,
  /// Manufacturer, from the EDID on real hardware.
  pub make: String,
  /// Model name, from the EDID on real hardware.
  pub model: String,
  /// Serial number, if the EDID has one.
  pub serial: Option<String>,
  /// Physical width and height in millimetres, or zero when unknown.
  pub physical_size_mm: (u32, u32),
  /// Subpixel layout of the panel.
  pub subpixel: wl_output::Subpixel,
  /// Refresh rate of the current mode in millihertz, or zero when unknown.
  pub refresh_mhz: u32,
  /// The modes the display offers, in the backend's order. Empty when the
  /// backend has no list, as nested and headless.
  pub modes: Vec<OutputModeInfo>,
  /// The index of the current mode in `modes`, if one is set.
  pub current_mode: Option<usize>,
}

impl Default for OutputInfo {
  fn default() -> Self {
    Self {
      name: crate::OUTPUT_NAME.into(),
      make: "Shingle".into(),
      model: "Headless output".into(),
      serial: None,
      physical_size_mm: (310, 175),
      subpixel: wl_output::Subpixel::Unknown,
      refresh_mhz: 60_000,
      modes: Vec::new(),
      current_mode: None,
    }
  }
}

impl OutputInfo {
  /// Returns the human-readable description sent to clients, built like
  /// wlroots' output description: make, model, serial and name.
  pub fn description(&self) -> String {
    match &self.serial {
      Some(serial) => {
        format!("{} {} {} ({})", self.make, self.model, serial, self.name)
      }
      None => format!("{} {} ({})", self.make, self.model, self.name),
    }
  }

  /// The current mode, if the backend lists modes and one is current.
  pub fn current(&self) -> Option<&OutputModeInfo> {
    self.modes.get(self.current_mode?)
  }
}

/// Where the configuration puts an output: its layout position, logical
/// size, scale and transform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputPlacement {
  /// The position of the top-left corner in the layout: fixed, or `auto`,
  /// placed by [`shingle_config::layout`] after the fixed outputs.
  pub position: OutputPosition,
  /// Logical width before the transform, used when the backend gives no
  /// mode, as in nested and headless.
  pub width: u32,
  /// Logical height before the transform, like `width`.
  pub height: u32,
  /// Scale; must be finite and positive.
  pub scale: f64,
  /// Transform, as `wl_output` reports it.
  pub transform: wl_output::Transform,
  /// Refresh rate in millihertz that clients are told when the backend
  /// gives no mode, as in nested and headless; zero keeps the default.
  pub refresh_mhz: u32,
}

impl OutputPlacement {
  /// Converts a configured output entry.
  pub fn from_config(config: &shingle_config::OutputConfig) -> Self {
    let (width, height) = configured_size(config);
    Self {
      position: config.position,
      width,
      height,
      scale: config.scale,
      transform: crate::parse_output_transform(&config.transform),
      refresh_mhz: explicit_refresh(config),
    }
  }
}

/// The logical size before the transform of a configured output: its
/// explicit mode, which is in physical pixels, divided by its scale, else
/// its `width` by `height`.
pub(crate) fn configured_size(
  config: &shingle_config::OutputConfig,
) -> (u32, u32) {
  match config.mode.size() {
    Some((width, height)) => crate::output_transform::logical_size(
      width,
      height,
      config.scale,
      wl_output::Transform::Normal,
    ),
    None => (config.width, config.height),
  }
}

/// The refresh rate an explicit configured mode names, or zero.
pub(crate) fn explicit_refresh(config: &shingle_config::OutputConfig) -> u32 {
  match config.mode {
    shingle_config::OutputMode::Explicit { refresh_mhz, .. } => refresh_mhz,
    _ => 0,
  }
}

/// Identifies an [`OutputRequest`] for
/// [`crate::Compositor::finish_output_request`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutputRequestId(pub u64);

/// The mode a client asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestedMode {
  /// One of [`OutputInfo::modes`], by index.
  Listed(usize),
  /// A mode that is not listed, by size and refresh rate in millihertz
  /// (zero for any).
  Custom {
    /// Width in physical pixels.
    width: u32,
    /// Height in physical pixels.
    height: u32,
    /// Refresh rate in millihertz, or zero.
    refresh_mhz: u32,
  },
}

/// A mode change a wlr-output-management client asked for, which the
/// backend tests and, unless `test_only`, applies. The rest of the
/// configuration (position, scale, transform) is the compositor's and is
/// applied when the backend finishes the request successfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputRequest {
  /// The request, for [`crate::Compositor::finish_output_request`].
  pub id: OutputRequestId,
  /// The mode to test or set.
  pub mode: RequestedMode,
  /// Whether the client only tests the configuration.
  pub test_only: bool,
}

/// The configured placements: the default and one per output name.
#[derive(Clone, Debug)]
pub(crate) struct Placements {
  pub(crate) default: OutputPlacement,
  pub(crate) named: BTreeMap<String, OutputPlacement>,
}

impl Placements {
  /// The default placement from `config`'s output fields and the named
  /// ones from its `output_placements`.
  pub(crate) fn new(config: &crate::Config) -> Self {
    Self {
      default: OutputPlacement {
        position: config.output_position,
        width: config.output.width,
        height: config.output.height,
        scale: config.output_scale,
        transform: config.output_transform,
        refresh_mhz: config.output_refresh_mhz,
      },
      named: config.output_placements.clone(),
    }
  }

  /// The placement for the output called `name`.
  pub(crate) fn get(&self, name: &str) -> OutputPlacement {
    self.named.get(name).copied().unwrap_or(self.default)
  }
}

/// Names one output for the per-output calls of [`crate::Compositor`]:
/// the output's name, which is unique among the outputs that are there.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutputHandle(String);

impl OutputHandle {
  /// The handle of the output called `name`.
  pub fn new(name: impl Into<String>) -> Self {
    Self(name.into())
  }

  /// The output's name.
  pub fn name(&self) -> &str {
    &self.0
  }

  pub(crate) fn id(&self) -> OutputId {
    OutputId::new(self.0.clone())
  }
}

impl From<&OutputId> for OutputHandle {
  fn from(id: &OutputId) -> Self {
    Self(id.as_str().to_owned())
  }
}

/// One output as [`crate::Compositor::outputs`] reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputView {
  /// The output, for the per-output calls.
  pub handle: OutputHandle,
  /// Its IPC id: counted from 1 and never reused, so a replugged monitor
  /// gets a new one, as in Wayfire.
  pub ipc_id: u32,
  /// Its rectangle in the layout, in logical pixels after the transform.
  pub rect: Rect,
  /// Its scale.
  pub scale: f64,
  /// Its transform.
  pub transform: wl_output::Transform,
  /// What clients are told about it.
  pub info: OutputInfo,
  /// Where it comes from.
  pub kind: OutputKind,
  /// The workspace set it shows.
  pub wset: Option<shingle_core::WsetId>,
  /// Whether the backend reported it gone while it was the last output,
  /// so that it stays described but draws nothing and gets no frames.
  pub dark: bool,
}

impl OutputView {
  pub(crate) fn new(state: &CompositorState, output: &OutputState) -> Self {
    Self {
      handle: OutputHandle::from(&output.id),
      ipc_id: output.ipc_id,
      rect: output.rect,
      scale: output.scale,
      transform: output.transform,
      info: output.info.clone(),
      kind: output.kind,
      wset: state.core.wset_of(&output.id),
      dark: output.dark,
    }
  }
}

/// The data of a `wl_output` global and of the outputs bound from it: the
/// generation of the global, which names its output while it is current.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputGlobal {
  pub(crate) generation: u32,
}

/// Creates the `wl_output` global of output `id`, once the display exists.
pub(crate) fn create_global(state: &mut CompositorState, id: &OutputId) {
  let Some(handle) = state.display.clone() else {
    return;
  };
  let Some(output) = state.outputs.get_mut(id) else {
    return;
  };
  let generation = output.generation;
  output.global = Some(
    handle.create_global::<CompositorState, WlOutput, OutputGlobal>(
      4,
      OutputGlobal { generation },
    ),
  );
}

/// Removes globals retired more than [`RETIRED_GLOBAL_DELAY`] ago.
pub(crate) fn remove_retired(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  now: Instant,
) {
  state.outputs.retired.retain(|(global, since)| {
    if now.duration_since(*since) < RETIRED_GLOBAL_DELAY {
      return true;
    }
    handle.remove_global::<CompositorState>(global.clone());
    false
  });
}

/// Disables the global of output `id`, which stays bindable as an inert
/// output for [`RETIRED_GLOBAL_DELAY`], and tells every client that bound
/// it that its surfaces, toplevels and workspace groups left it.
fn retire_global(state: &mut CompositorState, id: &OutputId) {
  let generation = state.outputs.next_generation();
  let Some(output) = state.outputs.get_mut(id) else {
    return;
  };
  output.generation = generation;
  let old = output.global.take();
  let bound: Vec<WlOutput> =
    output.bound.drain().map(|(_, resource)| resource).collect();
  if let (Some(old), Some(handle)) = (old, state.display.clone()) {
    handle.disable_global::<CompositorState>(old.clone());
    state.outputs.retired.push((old, Instant::now()));
  }
  for resource in &bound {
    retire(state, resource);
  }
}

/// Gives output `id` the name `name`: renames the core output, replaces
/// the `wl_output` global, and finishes everything tied to the outputs
/// clients bound so far. The caller sets the new [`OutputInfo`] first.
///
/// When the new name has a parked workspace set, as when a session saved
/// on that connector was restored, the output shows that set, and the
/// windows of the set it showed join it.
pub(crate) fn rename(
  state: &mut CompositorState,
  id: &OutputId,
  name: &str,
) -> Result<OutputId, crate::Error> {
  let new_id = OutputId::new(name);
  if *id != new_id && state.outputs.get(&new_id).is_some() {
    return Err(crate::Error::OutputExists(name.into()));
  }
  state.core.rename_output(id, new_id.clone())?;
  retire_global(state, id);
  if let Some(output) = state.outputs.get_mut(id) {
    output.id = new_id.clone();
  }
  crate::layer_shell::output_renamed(state, id, &new_id);
  crate::shell::lock_output_renamed(state, id, &new_id);
  if state.active_output == *id {
    state.active_output = new_id.clone();
  }
  create_global(state, &new_id);
  let identity = state.outputs.get(&new_id).and_then(OutputState::identity);
  if let Some(wset) = state.core.wset_of(&new_id) {
    state
      .core
      .attach_wset(wset, Some(new_id.clone()), identity.clone())?;
  }
  claim_parked(state, &new_id, identity);
  crate::output_power::output_removed(state);
  crate::gamma_control::output_removed(state);
  crate::output_management::output_removed(state, id);
  crate::output_management::output_added(state, &new_id);
  Ok(new_id)
}

/// Tells the client of `output` that its surfaces, toplevels and workspace
/// groups left it, and forgets its xdg-outputs.
fn retire(state: &mut CompositorState, output: &WlOutput) {
  if output.client().is_none() {
    return;
  }
  // Surfaces leave it at the end of the turn (crate::surface_outputs).
  crate::foreign_toplevel::output_removed(state, output);
  crate::ext_workspace::output_removed(state, output);
  state.xdg_outputs.retain(|_, xdg_output| {
    xdg_output
      .data::<WlOutput>()
      .is_none_or(|bound| bound.id() != output.id())
  });
}

/// Applies `placement` to output `id`, keeping its physical size: the
/// transform, the scale and the position, with the logical size computed
/// again from the physical one.
pub(crate) fn place(
  state: &mut CompositorState,
  id: &OutputId,
  placement: OutputPlacement,
  physical: (u32, u32),
) -> Result<(), crate::Error> {
  state.set_transform_of(id, placement.transform);
  state.set_scale_of(id, placement.scale)?;
  let (width, height) = crate::output_transform::logical_size(
    physical.0,
    physical.1,
    placement.scale,
    placement.transform,
  );
  if let Some(output) = state.outputs.get_mut(id) {
    output.position = placement.position;
  }
  let (x, y) = match placement.position {
    OutputPosition::Fixed { x, y } => (x, y),
    OutputPosition::Auto => state
      .outputs
      .get(id)
      .map_or((0, 0), |output| (output.rect.x, output.rect.y)),
  };
  state.set_rect_of(id, Rect::new(x, y, width, height), false)
}

/// The size in physical pixels of output `id`: the current mode's, or the
/// logical rectangle scaled and transformed back.
pub(crate) fn physical_size(
  state: &CompositorState,
  id: &OutputId,
) -> (u32, u32) {
  state
    .outputs
    .get(id)
    .map_or((0, 0), OutputState::physical_size)
}

/// Places every output with [`shingle_config::layout`] and returns those
/// whose position changed, after giving them their new rectangles.
pub(crate) fn relayout(state: &mut CompositorState) -> Vec<OutputId> {
  let layout = state
    .outputs
    .iter()
    .map(|output| shingle_config::LayoutOutput {
      position: output.position,
      width: output.rect.width,
      height: output.rect.height,
    })
    .collect::<Vec<_>>();
  let rects = shingle_config::layout(&layout);
  for warning in shingle_config::layout_warnings(&layout) {
    eprintln!("shingle: output layout: {warning}");
  }
  let mut moved = Vec::new();
  for (output, rect) in state.outputs.iter_mut().zip(rects) {
    if output.rect != rect {
      output.rect = rect;
      moved.push(output.id.clone());
    }
  }
  moved
}

/// What clients and the core learn after output `id` moved or was
/// resized: `wl_output` and xdg-output geometry, output-management heads,
/// the workarea of its workspace set and the layer surfaces, and
/// [`crate::Event::OutputChanged`].
pub(crate) fn moved(state: &mut CompositorState, id: &OutputId) {
  let Some(output) = state.outputs.get(id) else {
    return;
  };
  output.send_all();
  let rect = output.rect;
  crate::layer_shell::update_workarea(state);
  crate::shell::reconfigure_lock_surfaces(state);
  crate::xdg_output::update(state);
  crate::output_management::sync(state);
  state.events.push(crate::Event::OutputChanged {
    output: OutputHandle::from(id),
    rect,
  });
}

/// Places every output again and tells clients about those that moved.
pub(crate) fn relayout_and_notify(state: &mut CompositorState) {
  for id in relayout(state) {
    moved(state, &id);
  }
}

/// Shows on output `id` the parked workspace set that belongs to it, found
/// by `identity` or name ([`shingle_core::Shingle::find_parked_wset`]),
/// and brings back the windows away from that set. The set the output
/// showed until then joins it and is gone. Does nothing without such a
/// set.
pub(crate) fn claim_parked(
  state: &mut CompositorState,
  id: &OutputId,
  identity: Option<shingle_core::OutputIdentity>,
) {
  let Some(parked) = state.core.find_parked_wset(id, identity.as_ref()) else {
    return;
  };
  let shown = state.core.wset_of(id);
  let Some(rect) = state.outputs.get(id).map(|output| output.rect) else {
    return;
  };
  let result = (|| -> Result<(), shingle_core::ShingleError> {
    state.core.attach_wset(parked, Some(id.clone()), identity)?;
    if let Some(shown) = shown.filter(|shown| *shown != parked) {
      state.core.transfer_wset(shown, parked, false)?;
      state.core.destroy_wset(shown)?;
    }
    state.core.return_windows(parked)?;
    state.core.set_wset_workarea(parked, rect)?;
    Ok(())
  })();
  if let Err(error) = result {
    eprintln!("shingle: restoring the windows of {id:?} failed: {error}");
  }
  if let Some(wset) = state.core.wset_of(id) {
    crate::ipc::output_wset_changed(state, id, wset);
  }
}

#[cfg(test)]
mod multi_tests;
#[cfg(test)]
mod protocol_tests;
#[cfg(test)]
mod tests;
