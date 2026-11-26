use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
  Direction, Gaps, LeafState, OutputIdentity, Rect, Result, ShingleError,
  SplitDirection, TileTree, WindowId, WindowLayout, WorkspaceSnapshot, WsetId,
};

/// Stable identity of an output, such as its connector name.
#[derive(
  Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct OutputId(pub String);

impl OutputId {
  /// Creates an output id.
  pub fn new(value: impl Into<String>) -> Self {
    Self(value.into())
  }

  /// Returns the identity as a string.
  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl From<&str> for OutputId {
  fn from(value: &str) -> Self {
    Self(value.to_owned())
  }
}

impl From<String> for OutputId {
  fn from(value: String) -> Self {
    Self(value)
  }
}

/// Position of a workspace in a workspace set's grid, zero based. Ordered
/// by column, then row.
#[derive(
  Clone,
  Copy,
  Debug,
  Default,
  Eq,
  Hash,
  Ord,
  PartialEq,
  PartialOrd,
  Serialize,
  Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceId {
  /// Column.
  pub x: u32,
  /// Row.
  pub y: u32,
}

impl WorkspaceId {
  /// The top-left workspace, which every new workspace set starts on.
  pub const ORIGIN: Self = Self { x: 0, y: 0 };

  /// Creates a workspace id.
  pub const fn new(x: u32, y: u32) -> Self {
    Self { x, y }
  }
}

/// A workspace as the output-keyed calls see it: the output showing the
/// workspace set that holds it, and the workspace in that set.
///
/// While the set is parked (shown on no output), `output` is the output
/// the set was last shown on, its home; for a set that was never shown it
/// is empty. [`WindowInfo::wset`] names the set itself.
#[derive(
  Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
  /// The output.
  pub output: OutputId,
  /// The workspace on that output.
  pub workspace: WorkspaceId,
}

/// What is known about how a window was launched. Sessions persist it so that
/// restored placeholders can be matched to newly mapped clients; see
/// [`Shingle::reconcile_mapped`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchIdentity {
  /// Wayland app id. Placeholders only match clients with the same one.
  pub app_id: String,
  /// Per-launch token. Reconciliation prefers a placeholder with the same
  /// value.
  pub launch_id: Option<String>,
  /// Command line that launched the window.
  #[serde(default)]
  pub command: Vec<String>,
  /// Working directory of the launch.
  #[serde(default)]
  pub working_directory: Option<String>,
  /// Window title.
  #[serde(default)]
  pub title: Option<String>,
  /// Group of windows launched together.
  #[serde(default)]
  pub launch_group: Option<String>,
}

impl LaunchIdentity {
  /// Creates an identity with only an app id.
  pub fn new(app_id: impl Into<String>) -> Self {
    Self {
      app_id: app_id.into(),
      ..Self::default()
    }
  }

  /// Sets the launch id.
  pub fn with_launch_id(mut self, launch_id: impl Into<String>) -> Self {
    self.launch_id = Some(launch_id.into());
    self
  }

  /// Sets the launch group.
  pub fn with_launch_group(mut self, launch_group: impl Into<String>) -> Self {
    self.launch_group = Some(launch_group.into());
    self
  }

  /// Sets the command line.
  pub fn with_command<I, S>(mut self, command: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self.command = command.into_iter().map(Into::into).collect();
    self
  }

  /// Sets the working directory.
  pub fn with_working_directory(
    mut self,
    working_directory: impl Into<String>,
  ) -> Self {
    self.working_directory = Some(working_directory.into());
    self
  }

  /// Sets the title.
  pub fn with_title(mut self, title: impl Into<String>) -> Self {
    self.title = Some(title.into());
    self
  }
}

/// Whether a window has a live client.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowPresence {
  /// A client surface is mapped for the window.
  Mapped,
  /// The window was restored from a session or its client unmapped. It keeps
  /// its slot until a matching client maps. The compositor must not render,
  /// focus, include in Scale or report placeholders as mapped.
  Placeholder,
}

/// A copy of one window's model state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowInfo {
  /// The window.
  pub id: WindowId,
  /// How the window was launched.
  pub identity: LaunchIdentity,
  /// Whether the window has a live client.
  pub presence: WindowPresence,
  /// The workspace holding the window.
  pub location: WorkspaceRef,
  /// The workspace set holding the window.
  pub wset: WsetId,
  /// The set the window was moved away from when its output went, and
  /// returns to when the output comes back; see
  /// [`Shingle::transfer_wset`].
  pub away_from: Option<WsetId>,
  /// The window's leaf state.
  pub state: LeafState,
  /// Parent window, for dialogs and other transient windows.
  pub parent: Option<WindowId>,
}

/// A window removed from the model, with everything needed to put it back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachedWindow {
  /// The window.
  pub id: WindowId,
  /// How the window was launched.
  pub identity: LaunchIdentity,
  /// Whether the window had a live client.
  pub presence: WindowPresence,
  /// The workspace the window was removed from.
  pub location: WorkspaceRef,
  /// The window's leaf state.
  pub state: LeafState,
  /// Parent window.
  pub parent: Option<WindowId>,
}

/// A workspace set: a grid of workspaces with one tiling tree each, shown
/// on at most one output.
#[derive(Clone, Debug)]
pub(crate) struct WsetState {
  pub workarea: Rect,
  pub current_workspace: WorkspaceId,
  pub workspaces: BTreeMap<WorkspaceId, TileTree>,
  /// The output showing the set; `None` while it is parked.
  pub output: Option<OutputId>,
  /// The output the set was last shown on.
  pub home: Option<OutputId>,
  /// That output's EDID identity.
  pub identity: Option<OutputIdentity>,
  /// The trees as they were when an unplug moved the set's windows away,
  /// for [`Shingle::return_windows`].
  pub away: Option<Vec<WorkspaceSnapshot>>,
}

impl WsetState {
  /// A set with one empty, current workspace, [`WorkspaceId::ORIGIN`].
  pub fn new(workarea: Rect) -> Result<Self> {
    let workarea = workarea.validate()?;
    Ok(Self {
      workarea,
      current_workspace: WorkspaceId::ORIGIN,
      workspaces: BTreeMap::from([(
        WorkspaceId::ORIGIN,
        TileTree::new(SplitDirection::Vertical, workarea)?,
      )]),
      output: None,
      home: None,
      identity: None,
      away: None,
    })
  }

  /// Whether no workspace of the set holds a window.
  pub fn is_empty(&self) -> bool {
    self.workspaces.values().all(TileTree::is_empty)
  }

  /// The workspace's tree, created empty in the set's workarea if missing.
  pub fn ensure_tree(&mut self, workspace: WorkspaceId) -> Result<()> {
    if !self.workspaces.contains_key(&workspace) {
      self.workspaces.insert(
        workspace,
        TileTree::new(SplitDirection::Vertical, self.workarea)?,
      );
    }
    Ok(())
  }
}

/// Where a window's leaf is: a workspace of a workspace set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Place {
  pub wset: WsetId,
  pub workspace: WorkspaceId,
}

#[derive(Clone, Debug)]
pub(crate) struct WindowRecord {
  pub identity: LaunchIdentity,
  pub mapped: bool,
  pub place: Place,
  pub parent: Option<WindowId>,
  /// See [`WindowInfo::away_from`]. Never the set holding the window.
  pub away_from: Option<WsetId>,
}

/// The window model: workspace sets, their workspaces and tiling trees,
/// which outputs show them, and a record for every window.
///
/// Every window record has exactly one leaf, in the tree of the workspace
/// its place names. Each output shows at most one set and each set is
/// shown on at most one output; a set on no output is parked and keeps its
/// trees. New windows are mapped; restored windows start as placeholders.
///
/// The output-keyed calls ([`Shingle::add_output`],
/// [`Shingle::current_workspace`], [`Shingle::tree`],
/// [`Shingle::attach_window`] and the others taking an [`OutputId`]) work
/// on the set the output shows, so code that knows only outputs keeps
/// working; the calls taking a [`WsetId`] also reach parked sets.
#[derive(Clone, Debug)]
pub struct Shingle {
  pub(crate) wsets: BTreeMap<WsetId, WsetState>,
  pub(crate) windows: BTreeMap<WindowId, WindowRecord>,
  pub(crate) next_window_id: u64,
  pub(crate) active_output: Option<OutputId>,
}

impl Default for Shingle {
  fn default() -> Self {
    Self::new()
  }
}

impl Shingle {
  /// Creates an empty model with no outputs.
  pub fn new() -> Self {
    Self {
      wsets: BTreeMap::new(),
      windows: BTreeMap::new(),
      next_window_id: 1,
      active_output: None,
    }
  }

  /// Adds an output showing a new workspace set, with the lowest free
  /// index, that has one empty workspace, [`WorkspaceId::ORIGIN`], which is
  /// current.
  ///
  /// # Errors
  ///
  /// [`ShingleError::InvalidOutputId`], [`ShingleError::OutputExists`] if a
  /// set is already shown on the output, or
  /// [`ShingleError::InvalidWorkarea`].
  pub fn add_output(&mut self, output: OutputId, workarea: Rect) -> Result<()> {
    if output.0.is_empty() {
      return Err(ShingleError::InvalidOutputId);
    }
    if self.wset_of(&output).is_some() {
      return Err(ShingleError::OutputExists(output.0));
    }
    let mut state = WsetState::new(workarea)?;
    state.output = Some(output.clone());
    state.home = Some(output);
    let id = self.free_wset_id()?;
    self.wsets.insert(id, state);
    Ok(())
  }

  /// Iterates over the outputs showing a workspace set, in sorted order.
  pub fn outputs(&self) -> impl Iterator<Item = &OutputId> {
    let mut outputs = self
      .wsets
      .values()
      .filter_map(|state| state.output.as_ref())
      .collect::<Vec<_>>();
    outputs.sort();
    outputs.into_iter()
  }

  /// The output that is focused, as last set with
  /// [`Shingle::set_active_output`] or restored from a session.
  pub fn active_output(&self) -> Option<&OutputId> {
    self.active_output.as_ref()
  }

  /// Records the focused output, so that sessions remember it. The output
  /// need not show a set.
  pub fn set_active_output(&mut self, output: Option<OutputId>) {
    self.active_output = output;
  }

  /// Moves `output`'s workspace set to the output `to`, keeping its
  /// workspaces, tiling trees and windows, whose locations follow. A
  /// backend calls this when the physical output behind the same layout
  /// changes name, such as a native output that is described for the
  /// first time or replaced by another connector.
  ///
  /// If `to` shows a set already, as when a restored session saved it, the
  /// two are merged into `to`'s set: it takes `output`'s workarea, and each
  /// of `output`'s workspaces either replaces an empty workspace of `to`,
  /// tree and all, or has its windows appended to the non-empty one. `to`
  /// keeps its current workspace if it holds any window, else takes
  /// `output`'s. `output`'s set is then gone, and windows away from it are
  /// away from the merged set. Renaming an output to itself does nothing.
  ///
  /// # Errors
  ///
  /// [`ShingleError::InvalidOutputId`] for an empty `to`, or
  /// [`ShingleError::OutputNotFound`] for an `output` showing no set.
  pub fn rename_output(
    &mut self,
    output: &OutputId,
    to: OutputId,
  ) -> Result<()> {
    if to.0.is_empty() {
      return Err(ShingleError::InvalidOutputId);
    }
    let source_id = self.wset_for(output)?;
    if *output == to {
      return Ok(());
    }
    let Some(target_id) = self.wset_of(&to) else {
      let source = self.wset_state_mut(source_id)?;
      source.output = Some(to.clone());
      source.home = Some(to);
      return Ok(());
    };
    let source = self
      .wsets
      .remove(&source_id)
      .ok_or(ShingleError::WsetNotFound(source_id.0))?;
    let target = self.wset_state_mut(target_id)?;
    let target_used = !target.is_empty();
    if !target_used {
      target.current_workspace = source.current_workspace;
    }
    target.workarea = source.workarea;
    for tree in target.workspaces.values_mut() {
      tree.set_workarea(source.workarea)?;
    }
    if target.away.is_none() {
      target.away = source.away;
    }
    for (workspace, mut tree) in source.workspaces {
      match target.workspaces.get_mut(&workspace) {
        Some(existing) if !existing.is_empty() => {
          for window in tree.windows() {
            let state = tree.detach(window)?;
            existing.attach_with_state(window, state)?;
          }
        }
        _ => {
          target.workspaces.insert(workspace, tree);
        }
      }
    }
    for record in self.windows.values_mut() {
      if record.place.wset == source_id {
        record.place.wset = target_id;
      }
      if record.away_from == Some(source_id) {
        record.away_from = Some(target_id);
      }
      if record.away_from == Some(record.place.wset) {
        record.away_from = None;
      }
    }
    Ok(())
  }

  /// Returns the workarea of the set shown on the output.
  pub fn output_workarea(&self, output: &OutputId) -> Result<Rect> {
    Ok(self.output(output)?.workarea)
  }

  /// Sets the workarea of the set shown on the output and lays out all of
  /// its workspaces in it, keeping tile proportions.
  pub fn set_output_workarea(
    &mut self,
    output: &OutputId,
    workarea: Rect,
  ) -> Result<()> {
    let wset = self.wset_for(output)?;
    self.set_wset_workarea(wset, workarea)
  }

  /// Returns the output's current workspace.
  pub fn current_workspace(&self, output: &OutputId) -> Result<WorkspaceId> {
    Ok(self.output(output)?.current_workspace)
  }

  /// Switches the output to an existing workspace.
  ///
  /// # Errors
  ///
  /// [`ShingleError::WorkspaceNotFound`] if it does not exist.
  pub fn set_current_workspace(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<()> {
    let wset = self.wset_for(output)?;
    self.set_wset_current_workspace(wset, workspace)
  }

  /// Creates the workspace if it does not exist and returns whether it was
  /// created.
  pub fn ensure_workspace(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<bool> {
    let wset = self.wset_for(output)?;
    self.ensure_wset_workspace(wset, workspace)
  }

  /// Returns the output's workspaces in [`WorkspaceId`] order.
  pub fn workspaces(&self, output: &OutputId) -> Result<Vec<WorkspaceId>> {
    Ok(self.output(output)?.workspaces.keys().copied().collect())
  }

  /// Returns a workspace's tiling tree.
  pub fn tree(
    &self,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<&TileTree> {
    self
      .output(output)?
      .workspaces
      .get(&workspace)
      .ok_or(ShingleError::WorkspaceNotFound)
  }

  /// Adds a mapped window to the output's current workspace and returns its new
  /// id.
  pub fn attach_window(
    &mut self,
    output: &OutputId,
    identity: LaunchIdentity,
  ) -> Result<WindowId> {
    let workspace = self.current_workspace(output)?;
    self.attach_window_on(output, workspace, identity)
  }

  /// Adds a mapped window to the given workspace and returns its new id.
  pub fn attach_window_on(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
    identity: LaunchIdentity,
  ) -> Result<WindowId> {
    self.attach_window_with_state(
      output,
      workspace,
      identity,
      LeafState::default(),
    )
  }

  /// Adds a mapped window with initial leaf state and returns its new id.
  pub fn attach_window_with_state(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
    identity: LaunchIdentity,
    state: LeafState,
  ) -> Result<WindowId> {
    self.attach_window_with_state_and_parent(
      output, workspace, identity, state, None,
    )
  }

  /// Adds a mapped window with initial leaf state and parent, and returns its
  /// new id.
  ///
  /// The window is appended to the workspace's root split as
  /// [`TileTree::attach_with_state`] describes. The parent is recorded without
  /// being checked.
  pub fn attach_window_with_state_and_parent(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
    identity: LaunchIdentity,
    state: LeafState,
    parent: Option<WindowId>,
  ) -> Result<WindowId> {
    let wset = self.wset_for(output)?;
    self.attach_window_to_wset(wset, workspace, identity, state, parent)
  }

  /// Adds a mapped window with initial leaf state and parent to an existing
  /// workspace of a workspace set, shown or parked, and returns its new id.
  pub fn attach_window_to_wset(
    &mut self,
    wset: WsetId,
    workspace: WorkspaceId,
    identity: LaunchIdentity,
    state: LeafState,
    parent: Option<WindowId>,
  ) -> Result<WindowId> {
    let place = Place { wset, workspace };
    let id = self.allocate_window_id()?;
    self.tree_mut(place)?.attach_with_state(id, state)?;
    self.windows.insert(
      id,
      WindowRecord {
        identity,
        mapped: true,
        place,
        parent,
        away_from: None,
      },
    );
    Ok(id)
  }

  /// Removes a window, mapped or placeholder, from the model and returns it for
  /// [`Shingle::reattach_window`].
  pub fn detach_window(&mut self, window: WindowId) -> Result<DetachedWindow> {
    let record = self
      .windows
      .get(&window)
      .cloned()
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    let location = self.workspace_ref(record.place);
    let state = self.tree_mut(record.place)?.detach(window)?;
    self.windows.remove(&window);
    Ok(DetachedWindow {
      id: window,
      identity: record.identity,
      presence: if record.mapped {
        WindowPresence::Mapped
      } else {
        WindowPresence::Placeholder
      },
      location,
      state,
      parent: record.parent,
    })
  }

  /// Appends a detached window to the given workspace's root split, keeping its
  /// id, identity, presence, state and parent.
  ///
  /// # Errors
  ///
  /// [`ShingleError::DuplicateWindow`] if the id is in use again.
  pub fn reattach_window(
    &mut self,
    detached: DetachedWindow,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<()> {
    if self.windows.contains_key(&detached.id) {
      return Err(ShingleError::DuplicateWindow(detached.id.0));
    }
    let place = Place {
      wset: self.wset_for(output)?,
      workspace,
    };
    self
      .tree_mut(place)?
      .attach_with_state(detached.id, detached.state)?;
    self.insert_detached(detached, place);
    Ok(())
  }

  /// Puts a detached window back beside `target`, on the target's workspace;
  /// see [`TileTree::attach_at_with_state`].
  pub fn reattach_window_at(
    &mut self,
    detached: DetachedWindow,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    if self.windows.contains_key(&detached.id) {
      return Err(ShingleError::DuplicateWindow(detached.id.0));
    }
    let place = self.place(target)?;
    self.tree_mut(place)?.attach_at_with_state(
      detached.id,
      detached.state,
      target,
      position,
    )?;
    self.insert_detached(detached, place);
    Ok(())
  }

  /// Moves `source` beside `target`, across workspaces and outputs if needed;
  /// see [`TileTree::drop`]. If insertion fails, both trees are restored. A
  /// window dropped on another workspace no longer returns to the set it
  /// was moved away from.
  pub fn drop_window(
    &mut self,
    source: WindowId,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    if source == target {
      return Err(ShingleError::SameWindow);
    }
    let source_place = self.place(source)?;
    let target_place = self.place(target)?;
    if source_place == target_place {
      return self.tree_mut(source_place)?.drop(source, target, position);
    }

    let source_backup = self.tree_at(source_place)?.clone();
    let target_backup = self.tree_at(target_place)?.clone();
    let state = self.tree_mut(source_place)?.detach(source)?;
    if let Err(error) = self
      .tree_mut(target_place)?
      .insert_detached(source, state, target, position)
    {
      *self.tree_mut(source_place)? = source_backup;
      *self.tree_mut(target_place)? = target_backup;
      return Err(error);
    }
    self.set_place(source, target_place, true)?;
    Ok(())
  }

  /// Exchanges the slots of two windows, across workspaces if needed.
  ///
  /// Each window keeps its state. Across workspaces each window also takes over
  /// the other's focus and stacking position, and the locations are updated. If
  /// either replacement fails, both trees are restored.
  pub fn swap_windows(
    &mut self,
    first: WindowId,
    second: WindowId,
  ) -> Result<()> {
    if first == second {
      return Err(ShingleError::SameWindow);
    }
    let first_place = self.place(first)?;
    let second_place = self.place(second)?;
    if first_place == second_place {
      return self.tree_mut(first_place)?.swap(first, second);
    }

    let first_backup = self.tree_at(first_place)?.clone();
    let second_backup = self.tree_at(second_place)?.clone();
    let first_state = self.tree_at(first_place)?.leaf_state(first)?;
    let second_state = self.tree_at(second_place)?.leaf_state(second)?;
    self
      .tree_mut(first_place)?
      .replace_leaf(first, second, second_state)?;
    if let Err(error) =
      self
        .tree_mut(second_place)?
        .replace_leaf(second, first, first_state)
    {
      *self.tree_mut(first_place)? = first_backup;
      *self.tree_mut(second_place)? = second_backup;
      return Err(error);
    }
    self.set_place(first, second_place, true)?;
    self.set_place(second, first_place, true)?;
    Ok(())
  }

  /// Focuses a window in its workspace; see [`TileTree::set_focus`].
  pub fn focus_window(&mut self, window: WindowId) -> Result<()> {
    self.window_tree_mut(window)?.set_focus(window)
  }

  /// Sets a window's stacking layer and raises it.
  pub fn set_stacking(
    &mut self,
    window: WindowId,
    mode: crate::StackingMode,
  ) -> Result<()> {
    self.window_tree_mut(window)?.set_stacking(window, mode)
  }

  /// Raises a window within its workspace.
  pub fn raise_window(&mut self, window: WindowId) -> Result<()> {
    self.window_tree_mut(window)?.raise(window)
  }

  /// Lowers a window within its workspace.
  pub fn lower_window(&mut self, window: WindowId) -> Result<()> {
    self.window_tree_mut(window)?.lower(window)
  }

  /// Moves focus within the window's workspace; see
  /// [`TileTree::focus_directional`].
  pub fn focus_directional(
    &mut self,
    from: WindowId,
    direction: Direction,
  ) -> Result<Option<WindowId>> {
    self
      .window_tree_mut(from)?
      .focus_directional(from, direction)
  }

  /// Resizes a tiled window; see [`TileTree::resize_adjacent`]. Floating
  /// windows are resized by setting their floating geometry.
  pub fn resize_window(
    &mut self,
    window: WindowId,
    direction: Direction,
    delta: i32,
    minimum_extent: u32,
  ) -> Result<bool> {
    self.window_tree_mut(window)?.resize_adjacent(
      window,
      direction,
      delta,
      minimum_extent,
    )
  }

  /// Sets a window's maximized flag; see [`LeafState::maximized`].
  pub fn set_maximized(
    &mut self,
    window: WindowId,
    maximized: bool,
  ) -> Result<()> {
    self
      .window_tree_mut(window)?
      .set_maximized(window, maximized)
  }

  /// Sets a window's minimized flag; see [`TileTree::set_minimized`].
  pub fn set_minimized(
    &mut self,
    window: WindowId,
    minimized: bool,
  ) -> Result<()> {
    self
      .window_tree_mut(window)?
      .set_minimized(window, minimized)
  }

  /// Floats a window or returns it to tiling; see [`TileTree::set_floating`].
  ///
  /// # Errors
  ///
  /// [`ShingleError::InvalidWorkarea`] for empty or unrepresentable geometry.
  pub fn set_floating(
    &mut self,
    window: WindowId,
    geometry: Option<Rect>,
  ) -> Result<()> {
    if let Some(geometry) = geometry {
      geometry.validate()?;
    }
    self.window_tree_mut(window)?.set_floating(window, geometry)
  }

  /// Snaps a window or ends its snap; see [`TileTree::set_snapped`].
  pub fn set_snapped(
    &mut self,
    window: WindowId,
    geometry: Option<Rect>,
  ) -> Result<()> {
    self.window_tree_mut(window)?.set_snapped(window, geometry)
  }

  /// Sets a window's tiled-edges mask; see [`LeafState::tiled_edges`].
  pub fn set_tiled_edges(
    &mut self,
    window: WindowId,
    edges: u32,
  ) -> Result<()> {
    self.window_tree_mut(window)?.set_tiled_edges(window, edges)
  }

  /// Sets a window's fullscreen flag.
  pub fn set_fullscreen(
    &mut self,
    window: WindowId,
    fullscreen: bool,
  ) -> Result<()> {
    self
      .window_tree_mut(window)?
      .set_fullscreen(window, fullscreen)
  }

  /// Sets a window's sticky flag.
  pub fn set_sticky(&mut self, window: WindowId, sticky: bool) -> Result<()> {
    self.window_tree_mut(window)?.set_sticky(window, sticky)
  }

  /// Sets a window's gaps.
  pub fn set_gaps(&mut self, window: WindowId, gaps: Gaps) -> Result<()> {
    self.window_tree_mut(window)?.set_gaps(window, gaps)
  }

  /// Returns a tiled window's placement; see [`TileTree::window_layout`].
  pub fn window_layout(&self, window: WindowId) -> Result<WindowLayout> {
    self.tree_at(self.place(window)?)?.window_layout(window)
  }

  /// Updates a window's app id, as reported by its client.
  pub fn set_window_app_id(
    &mut self,
    window: WindowId,
    app_id: String,
  ) -> Result<()> {
    self.record_mut(window)?.identity.app_id = app_id;
    Ok(())
  }

  /// Updates a window's title.
  pub fn set_window_title(
    &mut self,
    window: WindowId,
    title: Option<String>,
  ) -> Result<()> {
    self.record_mut(window)?.identity.title = title;
    Ok(())
  }

  /// Sets or clears a window's parent.
  ///
  /// # Errors
  ///
  /// [`ShingleError::SameWindow`] if the window would become its own ancestor,
  /// [`ShingleError::WindowNotFound`] if either window is unknown.
  pub fn set_window_parent(
    &mut self,
    window: WindowId,
    parent: Option<WindowId>,
  ) -> Result<()> {
    if parent == Some(window) {
      return Err(ShingleError::SameWindow);
    }
    if let Some(parent) = parent {
      self
        .windows
        .get(&parent)
        .ok_or(ShingleError::WindowNotFound(parent.0))?;
    }
    let mut ancestor = parent;
    while let Some(id) = ancestor {
      if id == window {
        return Err(ShingleError::SameWindow);
      }
      ancestor = self
        .windows
        .get(&id)
        .ok_or(ShingleError::WindowNotFound(id.0))?
        .parent;
    }
    self.record_mut(window)?.parent = parent;
    Ok(())
  }

  /// Returns a window's identity, presence, location, state and parent.
  pub fn window_info(&self, window: WindowId) -> Result<WindowInfo> {
    let record = self
      .windows
      .get(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    let state = self.tree_at(record.place)?.leaf_state(window)?;
    Ok(WindowInfo {
      id: window,
      identity: record.identity.clone(),
      presence: if record.mapped {
        WindowPresence::Mapped
      } else {
        WindowPresence::Placeholder
      },
      location: self.workspace_ref(record.place),
      wset: record.place.wset,
      away_from: record.away_from,
      state,
      parent: record.parent,
    })
  }

  /// Iterates over mapped and placeholder windows on every workspace.
  pub fn window_ids(&self) -> impl Iterator<Item = WindowId> + '_ {
    self.windows.keys().copied()
  }

  /// Returns the workspace holding a window.
  pub fn window_location(&self, window: WindowId) -> Result<WorkspaceRef> {
    Ok(self.workspace_ref(self.place(window)?))
  }

  /// Returns the placeholder windows in id order.
  pub fn placeholders(&self) -> Vec<WindowId> {
    self
      .windows
      .iter()
      .filter_map(|(id, record)| (!record.mapped).then_some(*id))
      .collect()
  }

  /// Restores the mapped flag after a validated structural transformation.
  pub fn mark_window_mapped(&mut self, window: WindowId) -> Result<()> {
    self.record_mut(window)?.mapped = true;
    Ok(())
  }

  /// Holds or releases a window's first map; see [`LeafState::held`].
  ///
  /// [`LeafState::held`]: crate::LeafState::held
  pub fn set_held(&mut self, window: WindowId, held: bool) -> Result<()> {
    self.window_tree_mut(window)?.set_held(window, held)
  }

  /// Ends a hold and lets the window join its workspace's tiling tree, as
  /// Wayfire's tile plugin attaches a view on `view-mapped`
  /// (`plugins/tile/tile-plugin.cpp`, `on_view_mapped`, which appends it to
  /// the workspace root in `tile_workspace_set_data_t::attach_view`).
  ///
  /// The window is moved to the end of the root split, after windows that
  /// mapped while it was held, keeping its id, identity, presence and other
  /// state. Returns whether the window was held; a window that was not is
  /// left untouched.
  pub fn release_held(&mut self, window: WindowId) -> Result<bool> {
    let tree = self.window_tree_mut(window)?;
    let mut state = tree.leaf_state(window)?;
    if !state.held {
      return Ok(false);
    }
    state.held = false;
    tree.detach(window)?;
    tree.attach_with_state(window, state)?;
    Ok(true)
  }

  /// Claims a placeholder for a newly mapped client and marks it mapped.
  ///
  /// Held windows already belong to a client and are never claimed. Only
  /// placeholders with the same app id are candidates. In id order, the
  /// first with an equal launch id wins; failing that, if the client has a
  /// launch id, the first placeholder without one; failing that, the lowest id.
  /// Returns `None` if no placeholder has the app id.
  pub fn reconcile_mapped(
    &mut self,
    identity: &LaunchIdentity,
  ) -> Option<WindowId> {
    let mut candidates: Vec<(WindowId, &WindowRecord)> = self
      .windows
      .iter()
      .filter(|(id, record)| {
        !record.mapped
          && record.identity.app_id == identity.app_id
          && !self
            .tree_at(record.place)
            .and_then(|tree| tree.leaf_state(**id))
            .is_ok_and(|state| state.held)
      })
      .map(|(id, record)| (*id, record))
      .collect();
    candidates.sort_by_key(|(id, _)| *id);
    let preferred = candidates
      .iter()
      .position(|(_, record)| record.identity.launch_id == identity.launch_id)
      .or_else(|| {
        if identity.launch_id.is_some() {
          candidates
            .iter()
            .position(|(_, record)| record.identity.launch_id.is_none())
        } else {
          None
        }
      });
    let index = preferred.unwrap_or(0);
    let id = candidates.get(index)?.0;
    self.windows.get_mut(&id)?.mapped = true;
    Some(id)
  }

  /// Turns a window into a placeholder when its client unmaps. It keeps its
  /// slot until [`Shingle::reconcile_mapped`] claims it again.
  pub fn reconcile_unmapped(&mut self, window: WindowId) -> Result<()> {
    self.record_mut(window)?.mapped = false;
    Ok(())
  }

  /// Removes a placeholder from the model.
  ///
  /// # Errors
  ///
  /// [`ShingleError::WindowStillMapped`] for a mapped window.
  pub fn forget_placeholder(
    &mut self,
    window: WindowId,
  ) -> Result<DetachedWindow> {
    let record = self
      .windows
      .get(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    if record.mapped {
      return Err(ShingleError::WindowStillMapped(window.0));
    }
    self.detach_window(window)
  }

  /// The tree at `place`.
  pub(crate) fn tree_at(&self, place: Place) -> Result<&TileTree> {
    self
      .wset_state(place.wset)?
      .workspaces
      .get(&place.workspace)
      .ok_or(ShingleError::WorkspaceNotFound)
  }

  /// The tree at `place`, for changing.
  pub(crate) fn tree_mut(&mut self, place: Place) -> Result<&mut TileTree> {
    self
      .wset_state_mut(place.wset)?
      .workspaces
      .get_mut(&place.workspace)
      .ok_or(ShingleError::WorkspaceNotFound)
  }

  /// The set with index `wset`.
  pub(crate) fn wset_state(&self, wset: WsetId) -> Result<&WsetState> {
    self
      .wsets
      .get(&wset)
      .ok_or(ShingleError::WsetNotFound(wset.0))
  }

  /// The set with index `wset`, for changing.
  pub(crate) fn wset_state_mut(
    &mut self,
    wset: WsetId,
  ) -> Result<&mut WsetState> {
    self
      .wsets
      .get_mut(&wset)
      .ok_or(ShingleError::WsetNotFound(wset.0))
  }

  /// The set shown on `output`.
  pub(crate) fn wset_for(&self, output: &OutputId) -> Result<WsetId> {
    self
      .wset_of(output)
      .ok_or_else(|| ShingleError::OutputNotFound(output.0.clone()))
  }

  /// The window's record.
  pub(crate) fn record_mut(
    &mut self,
    window: WindowId,
  ) -> Result<&mut WindowRecord> {
    self
      .windows
      .get_mut(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))
  }

  /// Where the window's leaf is.
  pub(crate) fn place(&self, window: WindowId) -> Result<Place> {
    Ok(
      self
        .windows
        .get(&window)
        .ok_or(ShingleError::WindowNotFound(window.0))?
        .place,
    )
  }

  /// Records that the window's leaf is now at `place`. A move by the user
  /// or a client (`moved_by_user`) to another workspace ends the window's
  /// away record, and so does arriving in the set it is away from.
  pub(crate) fn set_place(
    &mut self,
    window: WindowId,
    place: Place,
    moved_by_user: bool,
  ) -> Result<()> {
    let record = self.record_mut(window)?;
    if moved_by_user {
      record.away_from = None;
    }
    record.place = place;
    if record.away_from == Some(place.wset) {
      record.away_from = None;
    }
    Ok(())
  }

  /// The output-keyed form of `place`; see [`WorkspaceRef`].
  pub(crate) fn workspace_ref(&self, place: Place) -> WorkspaceRef {
    let output = self
      .wsets
      .get(&place.wset)
      .and_then(|state| state.output.clone().or_else(|| state.home.clone()))
      .unwrap_or_else(|| OutputId::new(""));
    WorkspaceRef {
      output,
      workspace: place.workspace,
    }
  }

  fn window_tree_mut(&mut self, window: WindowId) -> Result<&mut TileTree> {
    let place = self.place(window)?;
    self.tree_mut(place)
  }

  fn insert_detached(&mut self, detached: DetachedWindow, place: Place) {
    self.windows.insert(
      detached.id,
      WindowRecord {
        identity: detached.identity,
        mapped: detached.presence == WindowPresence::Mapped,
        place,
        parent: detached.parent,
        away_from: None,
      },
    );
  }

  fn output(&self, output: &OutputId) -> Result<&WsetState> {
    self.wset_state(self.wset_for(output)?)
  }

  fn allocate_window_id(&mut self) -> Result<WindowId> {
    if self.next_window_id == 0 {
      return Err(ShingleError::IdExhausted);
    }
    let start = self.next_window_id;
    let mut candidate = start;
    loop {
      let id = WindowId(candidate);
      if !self.windows.contains_key(&id) {
        self.next_window_id = candidate.checked_add(1).unwrap_or(0);
        return Ok(id);
      }
      candidate = candidate.checked_add(1).ok_or(ShingleError::IdExhausted)?;
      if candidate == start {
        return Err(ShingleError::IdExhausted);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const WORKAREA: Rect = Rect::new(0, 0, 1200, 800);
  const SECOND: WorkspaceId = WorkspaceId::new(1, 0);

  fn output() -> OutputId {
    OutputId::from("DP-1")
  }

  fn shingle() -> Shingle {
    let mut shingle = Shingle::new();
    shingle.add_output(output(), WORKAREA).unwrap();
    shingle.ensure_workspace(&output(), SECOND).unwrap();
    shingle
  }

  fn attach(shingle: &mut Shingle, app_id: &str) -> WindowId {
    shingle
      .attach_window(&output(), LaunchIdentity::new(app_id))
      .unwrap()
  }

  fn order(shingle: &Shingle, workspace: WorkspaceId) -> Vec<WindowId> {
    shingle.tree(&output(), workspace).unwrap().windows()
  }

  fn placeholder(shingle: &mut Shingle, window: WindowId) {
    shingle.reconcile_unmapped(window).unwrap();
  }

  #[test]
  fn add_output_validates_identity_and_workarea() {
    let mut shingle = shingle();
    assert!(matches!(
      shingle.add_output(OutputId::new(""), WORKAREA),
      Err(ShingleError::InvalidOutputId)
    ));
    assert!(matches!(
      shingle.add_output(output(), WORKAREA),
      Err(ShingleError::OutputExists(_))
    ));
    assert!(matches!(
      shingle.add_output("HDMI-A-1".into(), Rect::new(0, 0, 0, 1)),
      Err(ShingleError::InvalidWorkarea)
    ));
    assert_eq!(shingle.outputs().collect::<Vec<_>>(), vec![&output()]);
    assert_eq!(
      shingle.current_workspace(&output()).unwrap(),
      WorkspaceId::ORIGIN
    );
  }

  #[test]
  fn unknown_output_and_workspace_are_errors() {
    let mut shingle = shingle();
    let missing = OutputId::from("missing");
    assert!(matches!(
      shingle.output_workarea(&missing),
      Err(ShingleError::OutputNotFound(_))
    ));
    assert!(matches!(
      shingle.set_current_workspace(&output(), WorkspaceId::new(5, 5)),
      Err(ShingleError::WorkspaceNotFound)
    ));
    assert!(matches!(
      shingle.attach_window_on(
        &output(),
        WorkspaceId::new(5, 5),
        LaunchIdentity::new("app"),
      ),
      Err(ShingleError::WorkspaceNotFound)
    ));
    assert_eq!(shingle.window_ids().count(), 0);
  }

  #[test]
  fn ensure_workspace_creates_once() {
    let mut shingle = shingle();
    assert!(!shingle.ensure_workspace(&output(), SECOND).unwrap());
    assert!(
      shingle
        .ensure_workspace(&output(), WorkspaceId::new(0, 1))
        .unwrap()
    );
    assert_eq!(
      shingle.workspaces(&output()).unwrap(),
      vec![WorkspaceId::ORIGIN, WorkspaceId::new(0, 1), SECOND]
    );
  }

  #[test]
  fn attach_uses_current_workspace_and_sequential_ids() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    let second = attach(&mut shingle, "b");
    assert_eq!((first.get(), second.get()), (1, 2));
    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![first]);
    assert_eq!(order(&shingle, SECOND), vec![second]);
    let info = shingle.window_info(second).unwrap();
    assert_eq!(info.presence, WindowPresence::Mapped);
    assert_eq!(
      info.location,
      WorkspaceRef {
        output: output(),
        workspace: SECOND,
      }
    );
    assert_eq!(info.identity.app_id, "b");
  }

  #[test]
  fn window_ids_are_not_reused() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    shingle.detach_window(first).unwrap();
    assert_eq!(attach(&mut shingle, "b").get(), 2);
  }

  #[test]
  fn output_workarea_applies_to_every_workspace() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    let second = attach(&mut shingle, "b");
    let workarea = Rect::new(0, 30, 1000, 770);
    shingle.set_output_workarea(&output(), workarea).unwrap();
    assert_eq!(shingle.output_workarea(&output()).unwrap(), workarea);
    assert_eq!(shingle.window_layout(first).unwrap().node, workarea);
    assert_eq!(shingle.window_layout(second).unwrap().node, workarea);
    assert!(
      shingle
        .set_output_workarea(&output(), Rect::new(0, 0, 0, 0))
        .is_err()
    );
    assert_eq!(shingle.output_workarea(&output()).unwrap(), workarea);
  }

  #[test]
  fn detach_and_reattach_keep_identity_state_and_parent() {
    let mut shingle = shingle();
    let parent = attach(&mut shingle, "parent");
    let child = attach(&mut shingle, "child");
    shingle.set_window_parent(child, Some(parent)).unwrap();
    shingle.set_sticky(child, true).unwrap();
    placeholder(&mut shingle, child);
    let detached = shingle.detach_window(child).unwrap();
    assert_eq!(detached.presence, WindowPresence::Placeholder);
    assert!(detached.state.sticky);
    assert!(shingle.window_info(child).is_err());
    assert!(
      !shingle
        .tree(&output(), WorkspaceId::ORIGIN)
        .unwrap()
        .contains(child)
    );

    shingle
      .reattach_window(detached.clone(), &output(), SECOND)
      .unwrap();
    let info = shingle.window_info(child).unwrap();
    assert_eq!(info.location.workspace, SECOND);
    assert_eq!(info.presence, WindowPresence::Placeholder);
    assert_eq!(info.parent, Some(parent));
    assert!(info.state.sticky);
    assert!(matches!(
      shingle.reattach_window(detached, &output(), SECOND),
      Err(ShingleError::DuplicateWindow(_))
    ));
  }

  #[test]
  fn reattach_at_places_beside_target_on_its_workspace() {
    let mut shingle = shingle();
    let moving = attach(&mut shingle, "moving");
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    let left = attach(&mut shingle, "left");
    let right = attach(&mut shingle, "right");
    let detached = shingle.detach_window(moving).unwrap();
    shingle
      .reattach_window_at(detached, left, Direction::Right)
      .unwrap();
    assert_eq!(order(&shingle, SECOND), vec![left, moving, right]);
    assert_eq!(shingle.window_location(moving).unwrap().workspace, SECOND);
  }

  #[test]
  fn drop_window_moves_between_workspaces() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let second = attach(&mut shingle, "b");
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    let target = attach(&mut shingle, "c");
    shingle.set_gaps(first, Gaps::new(3, 1)).unwrap();
    shingle.drop_window(first, target, Direction::Left).unwrap();
    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![second]);
    assert_eq!(order(&shingle, SECOND), vec![first, target]);
    assert_eq!(shingle.window_location(first).unwrap().workspace, SECOND);
    assert_eq!(
      shingle.window_info(first).unwrap().state.gaps,
      Gaps::new(3, 1)
    );
    let tree = shingle.tree(&output(), WorkspaceId::ORIGIN).unwrap();
    assert_eq!(tree.focused(), Some(second));
    assert_eq!(tree.stack_order(), &[second]);
  }

  #[test]
  fn drop_window_within_workspace_reorders() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let second = attach(&mut shingle, "b");
    shingle
      .drop_window(first, second, Direction::Right)
      .unwrap();
    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![second, first]);
    assert!(matches!(
      shingle.drop_window(first, first, Direction::Right),
      Err(ShingleError::SameWindow)
    ));
  }

  #[test]
  fn swap_windows_across_workspaces_exchanges_slots() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let other = attach(&mut shingle, "b");
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    let second = attach(&mut shingle, "c");
    shingle
      .resize_window(first, Direction::Right, 200, 50)
      .unwrap();
    shingle.set_maximized(second, true).unwrap();
    shingle.swap_windows(first, second).unwrap();

    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![second, other]);
    assert_eq!(order(&shingle, SECOND), vec![first]);
    assert_eq!(
      shingle.window_layout(second).unwrap().node,
      Rect::new(0, 0, 800, 800)
    );
    assert!(shingle.window_info(second).unwrap().state.maximized);
    assert!(!shingle.window_info(first).unwrap().state.maximized);
    assert_eq!(shingle.window_location(first).unwrap().workspace, SECOND);
    assert_eq!(
      shingle.window_location(second).unwrap().workspace,
      WorkspaceId::ORIGIN
    );
    let tree = shingle.tree(&output(), SECOND).unwrap();
    assert_eq!(tree.focused(), Some(first));
  }

  #[test]
  fn floating_round_trip_restores_tiled_slot() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let second = attach(&mut shingle, "b");
    let third = attach(&mut shingle, "c");
    shingle
      .resize_window(first, Direction::Right, 100, 50)
      .unwrap();
    let before = shingle.window_layout(second).unwrap();
    assert!(matches!(
      shingle.set_floating(second, Some(Rect::new(0, 0, 0, 10))),
      Err(ShingleError::InvalidWorkarea)
    ));
    let geometry = Rect::new(50, 60, 300, 200);
    shingle.set_floating(second, Some(geometry)).unwrap();
    let info = shingle.window_info(second).unwrap();
    assert_eq!(info.state.floating, Some(geometry));
    assert_eq!(info.state.tiled_edges, 0);
    assert!(shingle.window_layout(second).is_err());
    assert_eq!(
      shingle.window_layout(third).unwrap().node,
      Rect::new(666, 0, 534, 800)
    );
    shingle.set_floating(second, None).unwrap();
    assert_eq!(shingle.window_layout(second).unwrap(), before);
    assert_eq!(shingle.window_info(second).unwrap().state.tiled_edges, 15);
  }

  #[test]
  fn window_parent_rejects_cycles_and_unknown_parents() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let second = attach(&mut shingle, "b");
    let third = attach(&mut shingle, "c");
    assert!(matches!(
      shingle.set_window_parent(first, Some(first)),
      Err(ShingleError::SameWindow)
    ));
    assert!(matches!(
      shingle.set_window_parent(first, Some(WindowId(99))),
      Err(ShingleError::WindowNotFound(99))
    ));
    shingle.set_window_parent(second, Some(first)).unwrap();
    shingle.set_window_parent(third, Some(second)).unwrap();
    assert!(matches!(
      shingle.set_window_parent(first, Some(third)),
      Err(ShingleError::SameWindow)
    ));
    assert_eq!(shingle.window_info(first).unwrap().parent, None);
    shingle.set_window_parent(third, None).unwrap();
    assert_eq!(shingle.window_info(third).unwrap().parent, None);
  }

  #[test]
  fn reconcile_prefers_matching_launch_id() {
    let mut shingle = shingle();
    let plain = shingle
      .attach_window(&output(), LaunchIdentity::new("term"))
      .unwrap();
    let tagged = shingle
      .attach_window(
        &output(),
        LaunchIdentity::new("term").with_launch_id("second"),
      )
      .unwrap();
    let other = attach(&mut shingle, "editor");
    for window in [plain, tagged, other] {
      placeholder(&mut shingle, window);
    }
    assert_eq!(shingle.placeholders(), vec![plain, tagged, other]);

    let identity = LaunchIdentity::new("term").with_launch_id("second");
    assert_eq!(shingle.reconcile_mapped(&identity), Some(tagged));
    let identity = LaunchIdentity::new("term").with_launch_id("third");
    assert_eq!(shingle.reconcile_mapped(&identity), Some(plain));
    assert_eq!(shingle.reconcile_mapped(&identity), None);
    assert_eq!(shingle.placeholders(), vec![other]);
    assert_eq!(
      shingle.window_info(tagged).unwrap().presence,
      WindowPresence::Mapped
    );
  }

  #[test]
  fn reconcile_without_launch_id_takes_lowest_placeholder() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "term");
    let second = attach(&mut shingle, "term");
    placeholder(&mut shingle, second);
    placeholder(&mut shingle, first);
    let identity = LaunchIdentity::new("term");
    assert_eq!(shingle.reconcile_mapped(&identity), Some(first));
    assert_eq!(shingle.reconcile_mapped(&identity), Some(second));
  }

  #[test]
  fn forget_placeholder_refuses_mapped_windows() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let second = attach(&mut shingle, "b");
    assert!(matches!(
      shingle.forget_placeholder(first),
      Err(ShingleError::WindowStillMapped(1))
    ));
    placeholder(&mut shingle, first);
    let forgotten = shingle.forget_placeholder(first).unwrap();
    assert_eq!(forgotten.presence, WindowPresence::Placeholder);
    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![second]);
    assert!(shingle.placeholders().is_empty());
  }

  #[test]
  fn mark_window_mapped_clears_placeholder() {
    let mut shingle = shingle();
    let window = attach(&mut shingle, "a");
    placeholder(&mut shingle, window);
    shingle.mark_window_mapped(window).unwrap();
    assert!(shingle.placeholders().is_empty());
    assert!(shingle.mark_window_mapped(WindowId(9)).is_err());
  }

  #[test]
  fn identity_updates_apply_to_record() {
    let mut shingle = shingle();
    let window = attach(&mut shingle, "a");
    shingle.set_window_app_id(window, "b".into()).unwrap();
    shingle
      .set_window_title(window, Some("Title".into()))
      .unwrap();
    let identity = shingle.window_info(window).unwrap().identity;
    assert_eq!(identity.app_id, "b");
    assert_eq!(identity.title.as_deref(), Some("Title"));
    assert!(shingle.set_window_app_id(WindowId(9), "c".into()).is_err());
  }

  #[test]
  fn released_window_joins_the_tree_after_windows_mapped_while_held() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "a");
    let held = attach(&mut shingle, "b");
    placeholder(&mut shingle, held);
    shingle.set_held(held, true).unwrap();
    let later = attach(&mut shingle, "c");
    let tiled = |shingle: &Shingle| {
      shingle
        .tree(&output(), WorkspaceId::ORIGIN)
        .unwrap()
        .layout()
        .into_iter()
        .map(|layout| (layout.window, layout.node.width))
        .collect::<Vec<_>>()
    };
    assert_eq!(tiled(&shingle), vec![(first, 600), (later, 600)]);

    assert!(shingle.release_held(held).unwrap());
    assert_eq!(
      order(&shingle, WorkspaceId::ORIGIN),
      vec![first, later, held]
    );
    assert_eq!(
      tiled(&shingle),
      vec![(first, 400), (later, 400), (held, 400)]
    );
    let info = shingle.window_info(held).unwrap();
    assert!(!info.state.held);
    assert_eq!(info.presence, WindowPresence::Placeholder);
    assert_eq!(info.identity.app_id, "b");

    // Releasing a window that is not held changes nothing.
    assert!(!shingle.release_held(first).unwrap());
    assert_eq!(
      order(&shingle, WorkspaceId::ORIGIN),
      vec![first, later, held]
    );
    assert!(shingle.release_held(WindowId(9)).is_err());
    assert!(shingle.set_held(WindowId(9), true).is_err());
  }

  #[test]
  fn reconcile_mapped_never_claims_a_held_window() {
    let mut shingle = shingle();
    let held = attach(&mut shingle, "a");
    placeholder(&mut shingle, held);
    shingle.set_held(held, true).unwrap();
    assert_eq!(shingle.reconcile_mapped(&LaunchIdentity::new("a")), None);

    let restored = attach(&mut shingle, "a");
    placeholder(&mut shingle, restored);
    assert_eq!(
      shingle.reconcile_mapped(&LaunchIdentity::new("a")),
      Some(restored)
    );
    assert_eq!(
      shingle.window_info(held).unwrap().presence,
      WindowPresence::Placeholder
    );
  }

  #[test]
  fn window_operations_reject_unknown_windows() {
    let mut shingle = shingle();
    let missing = WindowId(9);
    assert!(shingle.detach_window(missing).is_err());
    assert!(shingle.focus_window(missing).is_err());
    assert!(shingle.raise_window(missing).is_err());
    assert!(shingle.lower_window(missing).is_err());
    assert!(shingle.set_minimized(missing, true).is_err());
    assert!(shingle.set_snapped(missing, None).is_err());
    assert!(shingle.set_tiled_edges(missing, 0).is_err());
    assert!(shingle.set_fullscreen(missing, true).is_err());
    assert!(shingle.window_layout(missing).is_err());
    assert!(shingle.reconcile_unmapped(missing).is_err());
    assert!(shingle.forget_placeholder(missing).is_err());
  }

  #[test]
  fn renaming_an_output_keeps_its_trees() {
    let mut shingle = shingle();
    let first = attach(&mut shingle, "first");
    let second = attach(&mut shingle, "second");
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    let third = attach(&mut shingle, "third");
    let before = shingle
      .tree(&output(), WorkspaceId::ORIGIN)
      .unwrap()
      .clone();
    let hdmi = OutputId::from("HDMI-A-1");
    shingle.rename_output(&output(), hdmi.clone()).unwrap();
    assert_eq!(shingle.outputs().collect::<Vec<_>>(), vec![&hdmi]);
    let after = shingle.tree(&hdmi, WorkspaceId::ORIGIN).unwrap();
    assert_eq!(after.windows(), before.windows());
    assert_eq!(after.layout(), before.layout());
    assert_eq!(shingle.current_workspace(&hdmi).unwrap(), SECOND);
    for window in [first, second, third] {
      assert_eq!(shingle.window_location(window).unwrap().output, hdmi);
    }
    shingle.rename_output(&hdmi, hdmi.clone()).unwrap();
    assert!(matches!(
      shingle.rename_output(&output(), "x".into()),
      Err(ShingleError::OutputNotFound(_))
    ));
    assert!(matches!(
      shingle.rename_output(&hdmi, OutputId::new("")),
      Err(ShingleError::InvalidOutputId)
    ));
  }

  #[test]
  fn renaming_onto_an_existing_output_merges_them() {
    let mut shingle = shingle();
    let saved = attach(&mut shingle, "saved");
    let headless = OutputId::from("HEADLESS-1");
    let live = Rect::new(0, 0, 1920, 1080);
    shingle.add_output(headless.clone(), live).unwrap();
    // The empty live output merges into the saved one, which keeps its
    // windows and current workspace and takes the live workarea.
    shingle.set_current_workspace(&output(), SECOND).unwrap();
    shingle.rename_output(&headless, output()).unwrap();
    assert_eq!(shingle.outputs().collect::<Vec<_>>(), vec![&output()]);
    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![saved]);
    assert_eq!(shingle.current_workspace(&output()).unwrap(), SECOND);
    assert_eq!(shingle.output_workarea(&output()).unwrap(), live);
    assert_eq!(
      shingle.tree(&output(), SECOND).unwrap().workarea(),
      live,
      "every workspace is laid out in the new workarea"
    );
    // Windows on both sides of a workspace end up together.
    shingle.add_output(headless.clone(), live).unwrap();
    let other = shingle
      .attach_window(&headless, LaunchIdentity::new("other"))
      .unwrap();
    shingle.rename_output(&headless, output()).unwrap();
    assert_eq!(order(&shingle, WorkspaceId::ORIGIN), vec![saved, other]);
    assert_eq!(shingle.window_location(other).unwrap().output, output());
  }
}
