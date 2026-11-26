//! The outputs the compositor drives, in the order they were added, each
//! with its description, place in the layout, IPC id and `wl_output`
//! global (docs/design/multi-output.md, section 1).

use std::{collections::HashMap, time::Instant};

use shingle_config::OutputPosition;
use shingle_core::{OutputId, OutputIdentity, Rect};
use wayland_server::{
  Resource,
  backend::{GlobalId, ObjectId},
  protocol::wl_output::{self, WlOutput},
};

use crate::OutputInfo;

/// Where an output comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputKind {
  /// The backend's: a connector, a nested window or one of the headless
  /// backend's outputs.
  Backend,
  /// Created by IPC `wayfire/create-headless-output`, owned by the
  /// compositor.
  Virtual,
}

/// One output: what clients are told about it and where it is.
pub(crate) struct OutputState {
  /// The core's output id, which is the output's name.
  pub(crate) id: OutputId,
  /// The description clients see.
  pub(crate) info: OutputInfo,
  /// The rectangle in the layout, in logical pixels after the transform.
  pub(crate) rect: Rect,
  /// The scale; finite and positive.
  pub(crate) scale: f64,
  /// The transform, as `wl_output` reports it.
  pub(crate) transform: wl_output::Transform,
  /// Where the layout puts it: configured, or set by an output-management
  /// client or by [`crate::Compositor::set_output`].
  pub(crate) position: OutputPosition,
  /// The IPC id, never reused while the compositor runs.
  pub(crate) ipc_id: u32,
  /// Where it comes from.
  pub(crate) kind: OutputKind,
  /// The backend reported it gone, but it is the last output, so it stays
  /// described without frames ([`crate::Compositor::remove_output`]).
  pub(crate) dark: bool,
  /// The `wl_output` global, once the display exists.
  pub(crate) global: Option<GlobalId>,
  /// The generation of the global; outputs bound from another generation
  /// are inert.
  pub(crate) generation: u32,
  /// The `wl_output` resources clients bound from the current global.
  pub(crate) bound: HashMap<ObjectId, WlOutput>,
  /// An output-management client configured it, so config reloads leave
  /// its placement alone until Shingle restarts, as Wayfire's
  /// `is_externally_managed` does.
  pub(crate) managed: bool,
  /// Whether it is lit: a wlr-output-power client can turn it off.
  pub(crate) powered: bool,
}

/// An output turned off through output management: still a connector,
/// window or IPC output, listed as a disabled head, with what it takes to
/// turn it on again.
#[derive(Clone)]
pub(crate) struct DisabledOutput {
  /// The description clients saw.
  pub(crate) info: OutputInfo,
  /// Its size in physical pixels, before the transform.
  pub(crate) physical: (u32, u32),
  /// Where it comes from.
  pub(crate) kind: OutputKind,
  /// Its position, scale and transform when it was turned off.
  pub(crate) position: OutputPosition,
  /// Its scale.
  pub(crate) scale: f64,
  /// Its transform.
  pub(crate) transform: wl_output::Transform,
}

impl DisabledOutput {
  /// What turning `output` off keeps of it.
  pub(crate) fn new(output: &OutputState) -> Self {
    Self {
      info: output.info.clone(),
      physical: output.physical_size(),
      kind: output.kind,
      position: output.position,
      scale: output.scale,
      transform: output.transform,
    }
  }
}

impl OutputState {
  /// The EDID identity that finds a parked workspace set again. `None` for
  /// an output created over IPC, which never returns, and for outputs
  /// Shingle describes itself (nested windows and headless outputs, with
  /// the default make and model), which all look alike and are found by
  /// name only.
  pub(crate) fn identity(&self) -> Option<OutputIdentity> {
    let default = OutputInfo::default();
    let described = self.info.make != default.make
      || self.info.model != default.model
      || self.info.serial.is_some();
    (self.kind == OutputKind::Backend && described).then(|| {
      OutputIdentity::new(
        self.info.make.clone(),
        self.info.model.clone(),
        self.info.serial.clone(),
      )
    })
  }

  /// The size in physical pixels: the current mode's, or the rectangle
  /// scaled and transformed back.
  pub(crate) fn physical_size(&self) -> (u32, u32) {
    if let Some(mode) = self.info.current() {
      return (mode.width, mode.height);
    }
    crate::output_transform::physical_size(
      self.rect.width,
      self.rect.height,
      self.scale,
      self.transform,
    )
  }

  /// The physical size of the mode clients are told is current, clamped
  /// for the protocol.
  pub(crate) fn mode_size(&self) -> (i32, i32) {
    let (width, height) = crate::output_transform::physical_size(
      self.rect.width,
      self.rect.height,
      self.scale,
      self.transform,
    );
    (
      width.min(i32::MAX as u32) as i32,
      height.min(i32::MAX as u32) as i32,
    )
  }

  /// Describes the output to `resource`: geometry, mode, scale and
  /// description, the name too when `bind`, then `done`.
  pub(crate) fn send(&self, resource: &WlOutput, bind: bool) {
    let info = &self.info;
    let (width_mm, height_mm) = info.physical_size_mm;
    resource.geometry(
      self.rect.x,
      self.rect.y,
      width_mm.min(i32::MAX as u32) as i32,
      height_mm.min(i32::MAX as u32) as i32,
      info.subpixel,
      info.make.clone(),
      info.model.clone(),
      self.transform,
    );
    let (width, height) = self.mode_size();
    resource.mode(
      wl_output::Mode::Current | wl_output::Mode::Preferred,
      width,
      height,
      info.refresh_mhz as i32,
    );
    if resource.version() >= 2 {
      resource.scale(self.scale.ceil().clamp(1.0, i32::MAX as f64) as i32);
    }
    if resource.version() >= 4 {
      if bind {
        resource.name(info.name.clone());
      }
      resource.description(info.description());
    }
    if resource.version() >= 2 {
      resource.done();
    }
  }

  /// Sends the current state to every bound `wl_output`.
  pub(crate) fn send_all(&self) {
    for resource in self.bound.values() {
      self.send(resource, false);
    }
  }
}

/// The outputs, in the order they were added, and the counters that keep
/// IPC ids and global generations unique.
pub(crate) struct Outputs {
  list: Vec<OutputState>,
  next_ipc_id: u32,
  next_generation: u32,
  /// The highest `N` of a `HEADLESS-N` name handed out by
  /// [`Outputs::next_headless_name`].
  last_headless: u32,
  /// Replaced or removed globals, bindable until
  /// [`crate::output::RETIRED_GLOBAL_DELAY`] has passed.
  pub(crate) retired: Vec<(GlobalId, Instant)>,
  /// Outputs turned off through output management, in the order they
  /// were turned off.
  pub(crate) disabled: Vec<DisabledOutput>,
}

impl Outputs {
  /// No outputs; the compositor adds the first one at once.
  pub(crate) fn new() -> Self {
    Self {
      list: Vec::new(),
      next_ipc_id: 1,
      next_generation: 0,
      last_headless: 0,
      retired: Vec::new(),
      disabled: Vec::new(),
    }
  }

  /// Adds an output at the end of the add order, with the next IPC id and
  /// generation, and no global yet.
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn push(
    &mut self,
    info: OutputInfo,
    rect: Rect,
    scale: f64,
    transform: wl_output::Transform,
    position: OutputPosition,
    kind: OutputKind,
  ) -> &mut OutputState {
    let ipc_id = self.next_ipc_id;
    self.next_ipc_id = self.next_ipc_id.saturating_add(1);
    let generation = self.next_generation();
    self.list.push(OutputState {
      id: OutputId::new(info.name.clone()),
      info,
      rect,
      scale,
      transform,
      position,
      ipc_id,
      kind,
      dark: false,
      global: None,
      generation,
      bound: HashMap::new(),
      managed: false,
      powered: true,
    });
    self.list.last_mut().expect("an output was just pushed")
  }

  /// A new global generation, unique among every output's.
  pub(crate) fn next_generation(&mut self) -> u32 {
    let generation = self.next_generation;
    self.next_generation = self.next_generation.wrapping_add(1);
    generation
  }

  /// Takes the output out of the list.
  pub(crate) fn remove(&mut self, id: &OutputId) -> Option<OutputState> {
    let index = self.list.iter().position(|output| output.id == *id)?;
    Some(self.list.remove(index))
  }

  /// The output called `id`.
  pub(crate) fn get(&self, id: &OutputId) -> Option<&OutputState> {
    self.list.iter().find(|output| output.id == *id)
  }

  /// The output called `id`, to change.
  pub(crate) fn get_mut(&mut self, id: &OutputId) -> Option<&mut OutputState> {
    self.list.iter_mut().find(|output| output.id == *id)
  }

  /// The output with IPC id `ipc_id`.
  pub(crate) fn by_ipc_id(&self, ipc_id: u64) -> Option<&OutputState> {
    self
      .list
      .iter()
      .find(|output| u64::from(output.ipc_id) == ipc_id)
  }

  /// The output whose current global has `generation`.
  pub(crate) fn by_generation(&self, generation: u32) -> Option<&OutputState> {
    self
      .list
      .iter()
      .find(|output| output.generation == generation)
  }

  /// The output whose current global has `generation`, to change.
  pub(crate) fn by_generation_mut(
    &mut self,
    generation: u32,
  ) -> Option<&mut OutputState> {
    self
      .list
      .iter_mut()
      .find(|output| output.generation == generation)
  }

  /// The output a bound `wl_output` stands for, if it is live.
  pub(crate) fn of_resource(
    &self,
    resource: &WlOutput,
  ) -> Option<&OutputState> {
    self
      .list
      .iter()
      .find(|output| output.bound.contains_key(&resource.id()))
  }

  /// The first output added that is still there. Until the frontend draws
  /// every output (MO-5), hosts draw and present this one.
  pub(crate) fn first(&self) -> &OutputState {
    self
      .list
      .first()
      .expect("the compositor always has an output")
  }

  /// Every output in add order.
  pub(crate) fn iter(&self) -> impl Iterator<Item = &OutputState> {
    self.list.iter()
  }

  /// Every output in add order, to change.
  pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut OutputState> {
    self.list.iter_mut()
  }

  /// The name of the next output created over IPC: `HEADLESS-N` with `N`
  /// above every `HEADLESS-N` added so far, so names are not reused.
  pub(crate) fn next_headless_name(&mut self) -> String {
    let highest = self
      .list
      .iter()
      .filter_map(|output| headless_number(output.id.as_str()))
      .max()
      .unwrap_or(0)
      .max(self.last_headless);
    self.last_headless = highest + 1;
    format!("HEADLESS-{}", self.last_headless)
  }

  /// The output called `id` that output management turned off.
  pub(crate) fn disabled(&self, id: &OutputId) -> Option<&DisabledOutput> {
    self
      .disabled
      .iter()
      .find(|output| output.info.name == id.as_str())
  }

  /// Every live `wl_output` binding of every output.
  #[cfg(test)]
  pub(crate) fn bound(&self) -> impl Iterator<Item = &WlOutput> {
    self.list.iter().flat_map(|output| output.bound.values())
  }

  /// Takes the output called `id` out of the turned-off ones.
  pub(crate) fn take_disabled(
    &mut self,
    id: &OutputId,
  ) -> Option<DisabledOutput> {
    let index = self
      .disabled
      .iter()
      .position(|output| output.info.name == id.as_str())?;
    Some(self.disabled.remove(index))
  }
}

/// The `N` of an output named `HEADLESS-N`.
fn headless_number(name: &str) -> Option<u32> {
  name.strip_prefix("HEADLESS-")?.parse().ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn push(outputs: &mut Outputs, name: &str) -> (u32, u32) {
    let output = outputs.push(
      OutputInfo {
        name: name.into(),
        ..OutputInfo::default()
      },
      Rect::new(0, 0, 100, 100),
      1.0,
      wl_output::Transform::Normal,
      OutputPosition::Auto,
      OutputKind::Backend,
    );
    (output.ipc_id, output.generation)
  }

  #[test]
  fn ipc_ids_and_generations_are_never_reused() {
    let mut outputs = Outputs::new();
    assert_eq!(push(&mut outputs, "A"), (1, 0));
    assert_eq!(push(&mut outputs, "B"), (2, 1));
    outputs.remove(&OutputId::new("A")).unwrap();
    assert_eq!(push(&mut outputs, "A"), (3, 2));
    let order = outputs
      .iter()
      .map(|output| output.id.as_str().to_owned())
      .collect::<Vec<_>>();
    assert_eq!(order, ["B", "A"]);
    assert_eq!(outputs.first().id.as_str(), "B");
    assert_eq!(outputs.by_ipc_id(3).unwrap().id.as_str(), "A");
    assert!(outputs.by_ipc_id(1).is_none());
    assert_eq!(outputs.by_generation(1).unwrap().id.as_str(), "B");
  }

  #[test]
  fn headless_names_are_not_reused() {
    let mut outputs = Outputs::new();
    push(&mut outputs, "HEADLESS-1");
    push(&mut outputs, "DP-1");
    assert_eq!(outputs.next_headless_name(), "HEADLESS-2");
    assert_eq!(outputs.next_headless_name(), "HEADLESS-3");
    push(&mut outputs, "HEADLESS-7");
    assert_eq!(outputs.next_headless_name(), "HEADLESS-8");
  }

  #[test]
  fn only_described_backend_outputs_have_an_identity() {
    let mut outputs = Outputs::new();
    push(&mut outputs, "A");
    let output = outputs.get_mut(&OutputId::new("A")).unwrap();
    assert_eq!(output.identity(), None, "a headless output");
    output.info.make = "Dell Inc.".into();
    assert_eq!(
      output.identity(),
      Some(OutputIdentity::new("Dell Inc.", "Headless output", None))
    );
    output.kind = OutputKind::Virtual;
    assert_eq!(output.identity(), None);
  }
}
