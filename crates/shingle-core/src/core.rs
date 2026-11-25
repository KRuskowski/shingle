use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
  Direction, Gaps, LeafState, Rect, Result, ShingleError, SplitDirection,
  TileTree, WindowId, WindowLayout,
};

#[derive(
  Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct OutputId(pub String);

impl OutputId {
  pub fn new(value: impl Into<String>) -> Self {
    Self(value.into())
  }

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
  pub x: u32,
  pub y: u32,
}

impl WorkspaceId {
  pub const ORIGIN: Self = Self { x: 0, y: 0 };

  pub const fn new(x: u32, y: u32) -> Self {
    Self { x, y }
  }
}

#[derive(
  Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRef {
  pub output: OutputId,
  pub workspace: WorkspaceId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchIdentity {
  pub app_id: String,
  pub launch_id: Option<String>,
  #[serde(default)]
  pub command: Vec<String>,
  #[serde(default)]
  pub working_directory: Option<String>,
  #[serde(default)]
  pub title: Option<String>,
  #[serde(default)]
  pub launch_group: Option<String>,
}

impl LaunchIdentity {
  pub fn new(app_id: impl Into<String>) -> Self {
    Self {
      app_id: app_id.into(),
      ..Self::default()
    }
  }

  pub fn with_launch_id(mut self, launch_id: impl Into<String>) -> Self {
    self.launch_id = Some(launch_id.into());
    self
  }

  pub fn with_launch_group(mut self, launch_group: impl Into<String>) -> Self {
    self.launch_group = Some(launch_group.into());
    self
  }

  pub fn with_command<I, S>(mut self, command: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    self.command = command.into_iter().map(Into::into).collect();
    self
  }

  pub fn with_working_directory(
    mut self,
    working_directory: impl Into<String>,
  ) -> Self {
    self.working_directory = Some(working_directory.into());
    self
  }

  pub fn with_title(mut self, title: impl Into<String>) -> Self {
    self.title = Some(title.into());
    self
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowPresence {
  Mapped,
  Placeholder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowInfo {
  pub id: WindowId,
  pub identity: LaunchIdentity,
  pub presence: WindowPresence,
  pub location: WorkspaceRef,
  pub state: LeafState,
  pub parent: Option<WindowId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachedWindow {
  pub id: WindowId,
  pub identity: LaunchIdentity,
  pub presence: WindowPresence,
  pub location: WorkspaceRef,
  pub state: LeafState,
  pub parent: Option<WindowId>,
}

#[derive(Clone, Debug)]
pub(crate) struct OutputState {
  pub workarea: Rect,
  pub current_workspace: WorkspaceId,
  pub workspaces: BTreeMap<WorkspaceId, TileTree>,
}

#[derive(Clone, Debug)]
pub(crate) struct WindowRecord {
  pub identity: LaunchIdentity,
  pub mapped: bool,
  pub location: WorkspaceRef,
  pub parent: Option<WindowId>,
}

#[derive(Clone, Debug)]
pub struct Shingle {
  pub(crate) outputs: BTreeMap<OutputId, OutputState>,
  pub(crate) windows: BTreeMap<WindowId, WindowRecord>,
  pub(crate) next_window_id: u64,
}

impl Default for Shingle {
  fn default() -> Self {
    Self::new()
  }
}

impl Shingle {
  pub fn new() -> Self {
    Self {
      outputs: BTreeMap::new(),
      windows: BTreeMap::new(),
      next_window_id: 1,
    }
  }

  pub fn add_output(&mut self, output: OutputId, workarea: Rect) -> Result<()> {
    if output.0.is_empty() {
      return Err(ShingleError::InvalidOutputId);
    }
    if self.outputs.contains_key(&output) {
      return Err(ShingleError::OutputExists(output.0));
    }
    let workarea = workarea.validate()?;
    self.outputs.insert(
      output,
      OutputState {
        workarea,
        current_workspace: WorkspaceId::ORIGIN,
        workspaces: BTreeMap::from([(
          WorkspaceId::ORIGIN,
          TileTree::new(SplitDirection::Vertical, workarea)?,
        )]),
      },
    );
    Ok(())
  }

  pub fn outputs(&self) -> impl Iterator<Item = &OutputId> {
    self.outputs.keys()
  }

  pub fn output_workarea(&self, output: &OutputId) -> Result<Rect> {
    Ok(self.output(output)?.workarea)
  }

  pub fn set_output_workarea(
    &mut self,
    output: &OutputId,
    workarea: Rect,
  ) -> Result<()> {
    let workarea = workarea.validate()?;
    let output = self.output_mut(output)?;
    output.workarea = workarea;
    for tree in output.workspaces.values_mut() {
      tree.set_workarea(workarea)?;
    }
    Ok(())
  }

  pub fn current_workspace(&self, output: &OutputId) -> Result<WorkspaceId> {
    Ok(self.output(output)?.current_workspace)
  }

  pub fn set_current_workspace(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<()> {
    let output = self.output_mut(output)?;
    if !output.workspaces.contains_key(&workspace) {
      return Err(ShingleError::WorkspaceNotFound);
    }
    output.current_workspace = workspace;
    Ok(())
  }

  pub fn ensure_workspace(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<bool> {
    let output = self.output_mut(output)?;
    if output.workspaces.contains_key(&workspace) {
      return Ok(false);
    }
    output.workspaces.insert(
      workspace,
      TileTree::new(SplitDirection::Vertical, output.workarea)?,
    );
    Ok(true)
  }

  pub fn workspaces(&self, output: &OutputId) -> Result<Vec<WorkspaceId>> {
    Ok(self.output(output)?.workspaces.keys().copied().collect())
  }

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

  pub fn attach_window(
    &mut self,
    output: &OutputId,
    identity: LaunchIdentity,
  ) -> Result<WindowId> {
    let workspace = self.current_workspace(output)?;
    self.attach_window_on(output, workspace, identity)
  }

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

  pub fn attach_window_with_state_and_parent(
    &mut self,
    output: &OutputId,
    workspace: WorkspaceId,
    identity: LaunchIdentity,
    state: LeafState,
    parent: Option<WindowId>,
  ) -> Result<WindowId> {
    let location = WorkspaceRef {
      output: output.clone(),
      workspace,
    };
    let id = self.allocate_window_id()?;
    self.tree_mut(&location)?.attach_with_state(id, state)?;
    self.windows.insert(
      id,
      WindowRecord {
        identity,
        mapped: true,
        location,
        parent,
      },
    );
    Ok(id)
  }

  pub fn detach_window(&mut self, window: WindowId) -> Result<DetachedWindow> {
    let record = self
      .windows
      .get(&window)
      .cloned()
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    let state = self.tree_mut(&record.location)?.detach(window)?;
    self.windows.remove(&window);
    Ok(DetachedWindow {
      id: window,
      identity: record.identity,
      presence: if record.mapped {
        WindowPresence::Mapped
      } else {
        WindowPresence::Placeholder
      },
      location: record.location,
      state,
      parent: record.parent,
    })
  }

  pub fn reattach_window(
    &mut self,
    detached: DetachedWindow,
    output: &OutputId,
    workspace: WorkspaceId,
  ) -> Result<()> {
    if self.windows.contains_key(&detached.id) {
      return Err(ShingleError::DuplicateWindow(detached.id.0));
    }
    let location = WorkspaceRef {
      output: output.clone(),
      workspace,
    };
    self
      .tree_mut(&location)?
      .attach_with_state(detached.id, detached.state)?;
    self.windows.insert(
      detached.id,
      WindowRecord {
        identity: detached.identity,
        mapped: detached.presence == WindowPresence::Mapped,
        location,
        parent: detached.parent,
      },
    );
    Ok(())
  }

  pub fn reattach_window_at(
    &mut self,
    detached: DetachedWindow,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    if self.windows.contains_key(&detached.id) {
      return Err(ShingleError::DuplicateWindow(detached.id.0));
    }
    let location = self.location(target)?.clone();
    self.tree_mut(&location)?.attach_at_with_state(
      detached.id,
      detached.state,
      target,
      position,
    )?;
    self.windows.insert(
      detached.id,
      WindowRecord {
        identity: detached.identity,
        mapped: detached.presence == WindowPresence::Mapped,
        location,
        parent: detached.parent,
      },
    );
    Ok(())
  }

  pub fn drop_window(
    &mut self,
    source: WindowId,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    if source == target {
      return Err(ShingleError::SameWindow);
    }
    let source_location = self.location(source)?.clone();
    let target_location = self.location(target)?.clone();
    if source_location == target_location {
      return self
        .tree_mut(&source_location)?
        .drop(source, target, position);
    }

    let source_backup = self
      .tree(&source_location.output, source_location.workspace)?
      .clone();
    let target_backup = self
      .tree(&target_location.output, target_location.workspace)?
      .clone();
    let state = self.tree_mut(&source_location)?.detach(source)?;
    if let Err(error) = self
      .tree_mut(&target_location)?
      .insert_detached(source, state, target, position)
    {
      self.restore_tree(&source_location, source_backup)?;
      self.restore_tree(&target_location, target_backup)?;
      return Err(error);
    }
    self
      .windows
      .get_mut(&source)
      .ok_or(ShingleError::WindowNotFound(source.0))?
      .location = target_location;
    Ok(())
  }

  pub fn swap_windows(
    &mut self,
    first: WindowId,
    second: WindowId,
  ) -> Result<()> {
    if first == second {
      return Err(ShingleError::SameWindow);
    }
    let first_location = self.location(first)?.clone();
    let second_location = self.location(second)?.clone();
    if first_location == second_location {
      return self.tree_mut(&first_location)?.swap(first, second);
    }

    let first_backup = self
      .tree(&first_location.output, first_location.workspace)?
      .clone();
    let second_backup = self
      .tree(&second_location.output, second_location.workspace)?
      .clone();
    let first_state = self.tree_mut(&first_location)?.leaf_state(first)?;
    let second_state = self.tree_mut(&second_location)?.leaf_state(second)?;
    self.tree_mut(&first_location)?.replace_leaf(
      first,
      second,
      second_state,
    )?;
    if let Err(error) =
      self
        .tree_mut(&second_location)?
        .replace_leaf(second, first, first_state)
    {
      self.restore_tree(&first_location, first_backup)?;
      self.restore_tree(&second_location, second_backup)?;
      return Err(error);
    }
    self
      .windows
      .get_mut(&first)
      .ok_or(ShingleError::WindowNotFound(first.0))?
      .location = second_location;
    self
      .windows
      .get_mut(&second)
      .ok_or(ShingleError::WindowNotFound(second.0))?
      .location = first_location;
    Ok(())
  }

  pub fn focus_window(&mut self, window: WindowId) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_focus(window)
  }

  pub fn set_stacking(
    &mut self,
    window: WindowId,
    mode: crate::StackingMode,
  ) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_stacking(window, mode)
  }

  pub fn raise_window(&mut self, window: WindowId) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.raise(window)
  }

  pub fn lower_window(&mut self, window: WindowId) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.lower(window)
  }

  pub fn focus_directional(
    &mut self,
    from: WindowId,
    direction: Direction,
  ) -> Result<Option<WindowId>> {
    let location = self.location(from)?.clone();
    self.tree_mut(&location)?.focus_directional(from, direction)
  }

  pub fn resize_window(
    &mut self,
    window: WindowId,
    direction: Direction,
    delta: i32,
    minimum_extent: u32,
  ) -> Result<bool> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.resize_adjacent(
      window,
      direction,
      delta,
      minimum_extent,
    )
  }

  pub fn set_maximized(
    &mut self,
    window: WindowId,
    maximized: bool,
  ) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_maximized(window, maximized)
  }

  pub fn set_minimized(
    &mut self,
    window: WindowId,
    minimized: bool,
  ) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_minimized(window, minimized)
  }

  pub fn set_floating(
    &mut self,
    window: WindowId,
    geometry: Option<Rect>,
  ) -> Result<()> {
    if let Some(geometry) = geometry {
      geometry.validate()?;
    }
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_floating(window, geometry)
  }

  pub fn set_snapped(
    &mut self,
    window: WindowId,
    geometry: Option<Rect>,
  ) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_snapped(window, geometry)
  }

  pub fn set_tiled_edges(
    &mut self,
    window: WindowId,
    edges: u32,
  ) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_tiled_edges(window, edges)
  }

  pub fn set_fullscreen(
    &mut self,
    window: WindowId,
    fullscreen: bool,
  ) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_fullscreen(window, fullscreen)
  }

  pub fn set_sticky(&mut self, window: WindowId, sticky: bool) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_sticky(window, sticky)
  }

  pub fn set_gaps(&mut self, window: WindowId, gaps: Gaps) -> Result<()> {
    let location = self.location(window)?.clone();
    self.tree_mut(&location)?.set_gaps(window, gaps)
  }

  pub fn window_layout(&self, window: WindowId) -> Result<WindowLayout> {
    let location = self.location(window)?;
    self
      .tree(&location.output, location.workspace)?
      .window_layout(window)
  }

  pub fn set_window_app_id(
    &mut self,
    window: WindowId,
    app_id: String,
  ) -> Result<()> {
    self
      .windows
      .get_mut(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?
      .identity
      .app_id = app_id;
    Ok(())
  }

  pub fn set_window_title(
    &mut self,
    window: WindowId,
    title: Option<String>,
  ) -> Result<()> {
    self
      .windows
      .get_mut(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?
      .identity
      .title = title;
    Ok(())
  }

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
    self
      .windows
      .get_mut(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?
      .parent = parent;
    Ok(())
  }

  pub fn window_info(&self, window: WindowId) -> Result<WindowInfo> {
    let record = self
      .windows
      .get(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    let state = self
      .tree(&record.location.output, record.location.workspace)?
      .leaf_state(window)?;
    Ok(WindowInfo {
      id: window,
      identity: record.identity.clone(),
      presence: if record.mapped {
        WindowPresence::Mapped
      } else {
        WindowPresence::Placeholder
      },
      location: record.location.clone(),
      state,
      parent: record.parent,
    })
  }

  /// Iterates over mapped and placeholder windows on every workspace.
  pub fn window_ids(&self) -> impl Iterator<Item = WindowId> + '_ {
    self.windows.keys().copied()
  }

  pub fn window_location(&self, window: WindowId) -> Result<&WorkspaceRef> {
    self.location(window)
  }

  pub fn placeholders(&self) -> Vec<WindowId> {
    self
      .windows
      .iter()
      .filter_map(|(id, record)| (!record.mapped).then_some(*id))
      .collect()
  }

  /// Restores the mapped flag after a validated structural transformation.
  pub fn mark_window_mapped(&mut self, window: WindowId) -> Result<()> {
    self
      .windows
      .get_mut(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?
      .mapped = true;
    Ok(())
  }

  pub fn reconcile_mapped(
    &mut self,
    identity: &LaunchIdentity,
  ) -> Option<WindowId> {
    let mut candidates: Vec<(WindowId, &WindowRecord)> = self
      .windows
      .iter()
      .filter(|(_, record)| {
        !record.mapped && record.identity.app_id == identity.app_id
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

  pub fn reconcile_unmapped(&mut self, window: WindowId) -> Result<()> {
    self
      .windows
      .get_mut(&window)
      .ok_or(ShingleError::WindowNotFound(window.0))?
      .mapped = false;
    Ok(())
  }

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

  pub(crate) fn tree_mut(
    &mut self,
    location: &WorkspaceRef,
  ) -> Result<&mut TileTree> {
    self
      .outputs
      .get_mut(&location.output)
      .ok_or_else(|| ShingleError::OutputNotFound(location.output.0.clone()))?
      .workspaces
      .get_mut(&location.workspace)
      .ok_or(ShingleError::WorkspaceNotFound)
  }

  fn restore_tree(
    &mut self,
    location: &WorkspaceRef,
    tree: TileTree,
  ) -> Result<()> {
    *self.tree_mut(location)? = tree;
    Ok(())
  }

  fn output(&self, output: &OutputId) -> Result<&OutputState> {
    self
      .outputs
      .get(output)
      .ok_or_else(|| ShingleError::OutputNotFound(output.0.clone()))
  }

  fn output_mut(&mut self, output: &OutputId) -> Result<&mut OutputState> {
    self
      .outputs
      .get_mut(output)
      .ok_or_else(|| ShingleError::OutputNotFound(output.0.clone()))
  }

  fn location(&self, window: WindowId) -> Result<&WorkspaceRef> {
    Ok(
      &self
        .windows
        .get(&window)
        .ok_or(ShingleError::WindowNotFound(window.0))?
        .location,
    )
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
