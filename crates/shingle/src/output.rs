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

use std::{
  collections::BTreeMap,
  time::{Duration, Instant},
};

use shingle_core::{OutputId, Rect};
use wayland_server::{
  DisplayHandle, Resource,
  backend::GlobalId,
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
  /// Horizontal position of the top-left corner in the layout.
  pub x: i32,
  /// Vertical position of the top-left corner in the layout.
  pub y: i32,
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
    let (width, height) = config.size();
    Self {
      x: config.x,
      y: config.y,
      width,
      height,
      scale: config.scale,
      transform: crate::parse_output_transform(&config.transform),
      refresh_mhz: explicit_refresh(config),
    }
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
        x: config.output.x,
        y: config.output.y,
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

/// The `wl_output` global and the globals it replaced.
#[derive(Default)]
pub(crate) struct OutputGlobals {
  current: Option<GlobalId>,
  /// Bumped with each new global; outputs bound from an older one are
  /// inert.
  pub(crate) generation: u32,
  retired: Vec<(GlobalId, Instant)>,
}

/// The data of a `wl_output` global: which generation it is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputGlobal {
  pub(crate) generation: u32,
}

/// Creates the first `wl_output` global.
pub(crate) fn create_global(
  state: &mut CompositorState,
  handle: &DisplayHandle,
) {
  let generation = state.output_globals.generation;
  state.output_globals.current = Some(
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
  state.output_globals.retired.retain(|(global, since)| {
    if now.duration_since(*since) < RETIRED_GLOBAL_DELAY {
      return true;
    }
    handle.remove_global::<CompositorState>(global.clone());
    false
  });
}

/// Gives the output the name `name`: renames the core output, replaces the
/// `wl_output` global, and finishes everything tied to the outputs clients
/// bound so far. The caller sets the new [`OutputInfo`] first.
pub(crate) fn rename(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  name: &str,
) -> Result<(), crate::Error> {
  let new_id = OutputId::new(name);
  state.core.rename_output(&state.output_id, new_id.clone())?;
  state.output_id = new_id;
  if let Some(old) = state.output_globals.current.take() {
    handle.disable_global::<CompositorState>(old.clone());
    state.output_globals.retired.push((old, Instant::now()));
  }
  state.output_globals.generation =
    state.output_globals.generation.wrapping_add(1);
  create_global(state, handle);
  let retired: Vec<WlOutput> =
    state.outputs.drain().map(|(_, output)| output).collect();
  for output in &retired {
    retire(state, output);
  }
  crate::output_power::output_removed(state);
  crate::gamma_control::output_removed(state);
  crate::output_management::output_replaced(state, handle);
  Ok(())
}

/// Tells the client of `output` that its surfaces, toplevels and workspace
/// groups left it, and forgets its xdg-outputs.
fn retire(state: &mut CompositorState, output: &WlOutput) {
  let Some(client) = output.client().map(|client| client.id()) else {
    return;
  };
  for surface in state.surfaces.values() {
    if surface.client == client {
      surface.resource.leave(output);
    }
  }
  crate::foreign_toplevel::output_removed(state, output);
  crate::ext_workspace::output_removed(state, output);
  state.xdg_outputs.retain(|_, xdg_output| {
    xdg_output
      .data::<WlOutput>()
      .is_none_or(|bound| bound.id() != output.id())
  });
}

/// Applies `placement` to the output, keeping its physical size: the
/// transform, the scale and the position, with the logical size computed
/// again from the physical one.
pub(crate) fn place(
  state: &mut CompositorState,
  placement: OutputPlacement,
  physical: (u32, u32),
) -> Result<(), crate::Error> {
  state.set_output_transform(placement.transform);
  state.set_output_scale(placement.scale)?;
  let (width, height) = crate::output_transform::logical_size(
    physical.0,
    physical.1,
    placement.scale,
    placement.transform,
  );
  state.set_output(Rect::new(placement.x, placement.y, width, height))
}

/// The output's size in physical pixels: the current mode's, or the
/// logical rectangle scaled and transformed back.
pub(crate) fn physical_size(state: &CompositorState) -> (u32, u32) {
  if let Some(mode) = state.output_info.current() {
    return (mode.width, mode.height);
  }
  crate::output_transform::physical_size(
    state.output.width,
    state.output.height,
    state.output_scale,
    state.output_transform,
  )
}

#[cfg(test)]
mod tests;
