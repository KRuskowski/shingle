use serde::{Deserialize, Serialize};

use crate::geometry::{Direction, Gaps, InternalEdges, Rect, SplitDirection};
use crate::{Result, ShingleError};

#[derive(
  Clone,
  Copy,
  Debug,
  Eq,
  Hash,
  Ord,
  PartialEq,
  PartialOrd,
  Serialize,
  Deserialize,
)]
#[serde(transparent)]
pub struct WindowId(pub u64);

impl WindowId {
  pub fn new(value: u64) -> Result<Self> {
    if value == 0 {
      return Err(ShingleError::InvalidWindowId);
    }
    Ok(Self(value))
  }

  pub const fn get(self) -> u64 {
    self.0
  }
}

#[derive(
  Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StackingMode {
  #[default]
  Normal,
  AlwaysOnTop,
  AlwaysOnBottom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeafState {
  pub gaps: Gaps,
  pub maximized: bool,
  #[serde(default)]
  pub stacking: StackingMode,
  #[serde(default)]
  pub minimized: bool,
  #[serde(default)]
  pub floating: Option<Rect>,
  #[serde(default)]
  pub fullscreen: bool,
  #[serde(default)]
  pub sticky: bool,
  #[serde(default)]
  pub snapped: Option<Rect>,
  #[serde(default)]
  pub restore_geometry: Option<Rect>,
  #[serde(default = "default_tiled_edges")]
  pub tiled_edges: u32,
}

const fn default_tiled_edges() -> u32 {
  15
}

impl Default for LeafState {
  fn default() -> Self {
    Self {
      gaps: Gaps::default(),
      maximized: false,
      stacking: StackingMode::Normal,
      minimized: false,
      floating: None,
      fullscreen: false,
      sticky: false,
      snapped: None,
      restore_geometry: None,
      tiled_edges: default_tiled_edges(),
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowLayout {
  pub window: WindowId,
  pub node: Rect,
  pub geometry: Rect,
  pub gaps: Gaps,
  pub maximized: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Split {
  pub direction: SplitDirection,
  pub children: Vec<Child>,
}

#[derive(Clone, Debug)]
pub(crate) struct Child {
  pub weight: u64,
  pub node: Node,
}

#[derive(Clone, Debug)]
pub(crate) enum Node {
  Split(Split),
  Leaf(WindowId, LeafState),
}

#[derive(Clone, Debug)]
pub struct TileTree {
  pub(crate) root: Split,
  pub(crate) workarea: Rect,
  pub(crate) focused: Option<WindowId>,
  pub(crate) stack: Vec<WindowId>,
}

impl TileTree {
  pub fn new(direction: SplitDirection, workarea: Rect) -> Result<Self> {
    Ok(Self {
      root: Split {
        direction,
        children: Vec::new(),
      },
      workarea: workarea.validate()?,
      focused: None,
      stack: Vec::new(),
    })
  }

  pub fn workarea(&self) -> Rect {
    self.workarea
  }

  pub fn set_workarea(&mut self, workarea: Rect) -> Result<()> {
    self.workarea = workarea.validate()?;
    Ok(())
  }

  pub fn direction(&self) -> SplitDirection {
    self.root.direction
  }

  pub fn len(&self) -> usize {
    self
      .root
      .children
      .iter()
      .map(|child| count_leaves(&child.node))
      .sum()
  }

  pub fn is_empty(&self) -> bool {
    self.root.children.is_empty()
  }

  pub fn contains(&self, window: WindowId) -> bool {
    self
      .root
      .children
      .iter()
      .any(|child| contains(&child.node, window))
  }

  pub fn windows(&self) -> Vec<WindowId> {
    let mut windows = Vec::with_capacity(self.len());
    for child in &self.root.children {
      collect_windows(&child.node, &mut windows);
    }
    windows
  }

  pub fn focused(&self) -> Option<WindowId> {
    self.focused
  }

  pub fn stack_order(&self) -> &[WindowId] {
    &self.stack
  }

  pub fn set_focus(&mut self, window: WindowId) -> Result<()> {
    if self.leaf_state(window)?.minimized {
      return Err(ShingleError::WindowNotFound(window.0));
    }
    self.focused = Some(window);
    Ok(())
  }

  pub fn raise(&mut self, window: WindowId) -> Result<()> {
    let Some(index) =
      self.stack.iter().position(|candidate| *candidate == window)
    else {
      return Err(ShingleError::WindowNotFound(window.0));
    };
    self.stack.remove(index);
    self.stack.push(window);
    Ok(())
  }

  pub fn lower(&mut self, window: WindowId) -> Result<()> {
    let Some(index) =
      self.stack.iter().position(|candidate| *candidate == window)
    else {
      return Err(ShingleError::WindowNotFound(window.0));
    };
    self.stack.remove(index);
    self.stack.insert(0, window);
    Ok(())
  }

  pub fn attach(&mut self, window: WindowId) -> Result<()> {
    self.attach_with_state(window, LeafState::default())
  }

  pub fn attach_with_state(
    &mut self,
    window: WindowId,
    state: LeafState,
  ) -> Result<()> {
    WindowId::new(window.0)?;
    if self.contains(window) {
      return Err(ShingleError::DuplicateWindow(window.0));
    }
    let minimized = state.minimized;
    let weight = added_child_weight(&self.root);
    self.root.children.push(Child {
      weight,
      node: Node::Leaf(window, state),
    });
    if minimized {
      self.focus_fallback();
    } else {
      self.focused = Some(window);
    }
    self.stack.push(window);
    Ok(())
  }

  pub fn attach_at(
    &mut self,
    window: WindowId,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    self.attach_at_with_state(window, LeafState::default(), target, position)
  }

  pub fn attach_at_with_state(
    &mut self,
    window: WindowId,
    state: LeafState,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    WindowId::new(window.0)?;
    if self.contains(window) {
      return Err(ShingleError::DuplicateWindow(window.0));
    }
    let minimized = state.minimized;
    self.insert_leaf(target, window, state, position)?;
    if minimized {
      self.focus_fallback();
    } else {
      self.focused = Some(window);
    }
    self.stack.push(window);
    Ok(())
  }

  pub fn detach(&mut self, window: WindowId) -> Result<LeafState> {
    let state = self.take_leaf(window)?;
    self.flatten();
    self.focus_fallback();
    Ok(state)
  }

  pub fn drop(
    &mut self,
    source: WindowId,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    if source == target {
      return Err(ShingleError::SameWindow);
    }
    if !self.contains(source) {
      return Err(ShingleError::WindowNotFound(source.0));
    }
    if !self.contains(target) {
      return Err(ShingleError::WindowNotFound(target.0));
    }
    let state = self.take_leaf(source)?;
    let minimized = state.minimized;
    self.flatten();
    self.insert_leaf(target, source, state, position)?;
    if minimized {
      self.focus_fallback();
    } else {
      self.focused = Some(source);
    }
    self.stack.push(source);
    Ok(())
  }

  pub fn swap(&mut self, first: WindowId, second: WindowId) -> Result<()> {
    if first == second {
      return Err(ShingleError::SameWindow);
    }
    if !self.contains(first) {
      return Err(ShingleError::WindowNotFound(first.0));
    }
    if !self.contains(second) {
      return Err(ShingleError::WindowNotFound(second.0));
    }
    let first_state = self.leaf_state(first)?;
    let second_state = self.leaf_state(second)?;
    exchange_leaf_ids(&mut self.root, first, second);
    let first_leaf = find_leaf_mut(&mut self.root, first)
      .ok_or(ShingleError::WindowNotFound(first.0))?;
    if let Node::Leaf(_, state) = first_leaf {
      *state = first_state;
    }
    let second_leaf = find_leaf_mut(&mut self.root, second)
      .ok_or(ShingleError::WindowNotFound(second.0))?;
    if let Node::Leaf(_, state) = second_leaf {
      *state = second_state;
    }
    Ok(())
  }

  pub fn focus_directional(
    &mut self,
    from: WindowId,
    direction: Direction,
  ) -> Result<Option<WindowId>> {
    let layouts = self.layout();
    let source = layouts
      .iter()
      .find(|layout| layout.window == from)
      .ok_or(ShingleError::WindowNotFound(from.0))?;
    let next = layouts
      .iter()
      .filter(|layout| layout.window != from)
      .filter_map(|layout| {
        direction
          .score(source.node, layout.node)
          .map(|(primary, cross)| (primary, cross, layout.window))
      })
      .min_by_key(|(primary, cross, window)| (*primary, *cross, *window))
      .map(|(_, _, window)| window);
    if let Some(next) = next {
      self.focused = Some(next);
    }
    Ok(next)
  }

  pub fn resize_adjacent(
    &mut self,
    window: WindowId,
    direction: Direction,
    delta: i32,
    minimum_extent: u32,
  ) -> Result<bool> {
    if minimum_extent == 0 {
      return Err(ShingleError::MinimumExtent);
    }
    if !self.contains(window) {
      return Err(ShingleError::WindowNotFound(window.0));
    }
    resize_node(
      &mut self.root,
      self.workarea,
      window,
      direction,
      delta,
      minimum_extent,
    )?
    .ok_or(ShingleError::NoAdjacentWindow)
  }

  pub fn set_maximized(
    &mut self,
    window: WindowId,
    maximized: bool,
  ) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.maximized = maximized;
    Ok(())
  }

  pub fn set_minimized(
    &mut self,
    window: WindowId,
    minimized: bool,
  ) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.minimized = minimized;
    if minimized && self.focused == Some(window) {
      self.focus_fallback();
    }
    Ok(())
  }

  pub fn set_floating(
    &mut self,
    window: WindowId,
    geometry: Option<Rect>,
  ) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.floating = geometry;
    state.tiled_edges = if geometry.is_some() { 0 } else { 15 };
    if geometry.is_some() {
      state.snapped = None;
      state.restore_geometry = None;
    }
    Ok(())
  }

  pub fn set_snapped(
    &mut self,
    window: WindowId,
    geometry: Option<Rect>,
  ) -> Result<()> {
    if let Some(geometry) = geometry {
      geometry.validate()?;
    }
    let current = self.leaf_state(window)?;
    let restore_geometry = if current.snapped.is_some() {
      current.restore_geometry
    } else {
      current.floating.or_else(|| {
        self
          .window_layout(window)
          .ok()
          .map(|layout| layout.geometry)
      })
    };
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.snapped = geometry;
    state.restore_geometry = geometry.and(restore_geometry);
    if geometry.is_some() {
      state.floating = None;
      state.fullscreen = false;
      state.maximized = false;
    }
    Ok(())
  }

  pub fn set_tiled_edges(
    &mut self,
    window: WindowId,
    edges: u32,
  ) -> Result<()> {
    if edges > 15 {
      return Err(ShingleError::InvalidTiledEdges(edges));
    }
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.tiled_edges = edges;
    Ok(())
  }

  pub fn set_fullscreen(
    &mut self,
    window: WindowId,
    fullscreen: bool,
  ) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.fullscreen = fullscreen;
    Ok(())
  }

  pub fn set_sticky(&mut self, window: WindowId, sticky: bool) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.sticky = sticky;
    Ok(())
  }

  pub fn set_stacking(
    &mut self,
    window: WindowId,
    mode: StackingMode,
  ) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.stacking = mode;
    self.raise(window)
  }

  pub fn set_gaps(&mut self, window: WindowId, gaps: Gaps) -> Result<()> {
    let (_, state) = find_leaf_mut(&mut self.root, window)
      .and_then(|node| match node {
        Node::Leaf(window, state) => Some((*window, state)),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    state.gaps = gaps;
    Ok(())
  }

  pub fn leaf_state(&self, window: WindowId) -> Result<LeafState> {
    find_leaf(&self.root, window)
      .and_then(|node| match node {
        Node::Leaf(_, state) => Some(*state),
        Node::Split(_) => None,
      })
      .ok_or(ShingleError::WindowNotFound(window.0))
  }

  pub fn layout(&self) -> Vec<WindowLayout> {
    let mut layouts = Vec::with_capacity(self.len());
    layout_children(
      &self.root,
      self.workarea,
      InternalEdges::default(),
      self.workarea,
      &mut layouts,
    );
    layouts
  }

  pub fn window_layout(&self, window: WindowId) -> Result<WindowLayout> {
    self
      .layout()
      .into_iter()
      .find(|layout| layout.window == window)
      .ok_or(ShingleError::WindowNotFound(window.0))
  }

  pub fn window_at(&self, x: i32, y: i32) -> Option<WindowId> {
    self.layout().into_iter().find_map(|layout| {
      let rect = layout.node;
      (x >= rect.x
        && y >= rect.y
        && (x as i64) < rect.x as i64 + rect.width as i64
        && (y as i64) < rect.y as i64 + rect.height as i64)
        .then_some(layout.window)
    })
  }

  pub fn flatten(&mut self) {
    let children = std::mem::take(&mut self.root.children);
    self.root.children = children
      .into_iter()
      .filter_map(|mut child| {
        flatten_node(&mut child.node)?;
        Some(child)
      })
      .collect();
    if self.root.children.len() == 1
      && let Node::Split(_) = &self.root.children[0].node
    {
      let child = self.root.children.remove(0);
      if let Node::Split(split) = child.node {
        self.root = split;
      }
    }
  }

  pub(crate) fn replace_leaf(
    &mut self,
    old: WindowId,
    new: WindowId,
    state: LeafState,
  ) -> Result<LeafState> {
    if self.contains(new) {
      return Err(ShingleError::DuplicateWindow(new.0));
    }
    let node = find_leaf_mut(&mut self.root, old)
      .ok_or(ShingleError::WindowNotFound(old.0))?;
    let previous = match node {
      Node::Leaf(_, state) => *state,
      Node::Split(_) => return Err(ShingleError::WindowNotFound(old.0)),
    };
    *node = Node::Leaf(new, state);
    self.focused = self
      .focused
      .map(|focused| if focused == old { new } else { focused });
    for stacked in &mut self.stack {
      if *stacked == old {
        *stacked = new;
      }
    }
    Ok(previous)
  }

  pub(crate) fn insert_detached(
    &mut self,
    window: WindowId,
    state: LeafState,
    target: WindowId,
    position: Direction,
  ) -> Result<()> {
    self.attach_at_with_state(window, state, target, position)
  }

  fn insert_leaf(
    &mut self,
    target: WindowId,
    window: WindowId,
    state: LeafState,
    position: Direction,
  ) -> Result<()> {
    let direction = SplitDirection::for_direction(position);
    let parent = find_parent_mut(&mut self.root, target)
      .ok_or(ShingleError::WindowNotFound(target.0))?;
    let target_index = parent
      .children
      .iter()
      .position(|child| matches!(child.node, Node::Leaf(id, _) if id == target))
      .ok_or(ShingleError::WindowNotFound(target.0))?;
    if parent.direction == direction {
      let weight = added_child_weight(parent);
      let insertion_index = target_index + usize::from(position.is_positive());
      parent.children.insert(
        insertion_index,
        Child {
          weight,
          node: Node::Leaf(window, state),
        },
      );
    } else {
      // The new split takes over the target's slot, so it keeps the target's
      // weight and its siblings keep their extents.
      let Child {
        weight,
        node: target_node,
      } = parent.children.remove(target_index);
      let first = Child {
        weight: 1,
        node: target_node,
      };
      let second = Child {
        weight: 1,
        node: Node::Leaf(window, state),
      };
      let children = if position.is_positive() {
        vec![first, second]
      } else {
        vec![second, first]
      };
      parent.children.insert(
        target_index,
        Child {
          weight,
          node: Node::Split(Split {
            direction,
            children,
          }),
        },
      );
    }
    self.flatten();
    Ok(())
  }

  fn focus_fallback(&mut self) {
    self.focused = self
      .stack
      .iter()
      .rev()
      .copied()
      .find(|window| {
        self.leaf_state(*window).is_ok_and(|state| !state.minimized)
      })
      .or_else(|| {
        self.windows().into_iter().find(|window| {
          self.leaf_state(*window).is_ok_and(|state| !state.minimized)
        })
      });
  }

  fn take_leaf(&mut self, window: WindowId) -> Result<LeafState> {
    let state = take_child(&mut self.root, window)
      .ok_or(ShingleError::WindowNotFound(window.0))?;
    if let Some(index) =
      self.stack.iter().position(|candidate| *candidate == window)
    {
      self.stack.remove(index);
    }
    if self.focused == Some(window) {
      self.focused = None;
    }
    Ok(state)
  }
}

fn count_leaves(node: &Node) -> usize {
  match node {
    Node::Leaf(_, _) => 1,
    Node::Split(split) => split
      .children
      .iter()
      .map(|child| count_leaves(&child.node))
      .sum(),
  }
}

fn contains(node: &Node, window: WindowId) -> bool {
  match node {
    Node::Leaf(candidate, _) => *candidate == window,
    Node::Split(split) => split
      .children
      .iter()
      .any(|child| contains(&child.node, window)),
  }
}

fn collect_windows(node: &Node, windows: &mut Vec<WindowId>) {
  match node {
    Node::Leaf(window, _) => windows.push(*window),
    Node::Split(split) => {
      for child in &split.children {
        collect_windows(&child.node, windows);
      }
    }
  }
}

fn added_child_weight(split: &Split) -> u64 {
  let count = split.children.len() as u128;
  if count == 0 {
    return 1;
  }
  let total = split
    .children
    .iter()
    .map(|child| child.weight as u128)
    .sum::<u128>();
  total.div_ceil(count) as u64
}

fn find_parent_mut(split: &mut Split, window: WindowId) -> Option<&mut Split> {
  if split
    .children
    .iter()
    .any(|child| matches!(&child.node, Node::Leaf(id, _) if *id == window))
  {
    return Some(split);
  }
  for child in &mut split.children {
    if let Node::Split(child_split) = &mut child.node
      && let Some(parent) = find_parent_mut(child_split, window)
    {
      return Some(parent);
    }
  }
  None
}

fn find_leaf(node: &Split, window: WindowId) -> Option<&Node> {
  node
    .children
    .iter()
    .find_map(|child| find_leaf_node(&child.node, window))
}

fn find_leaf_node(node: &Node, window: WindowId) -> Option<&Node> {
  match node {
    Node::Leaf(candidate, _) if *candidate == window => Some(node),
    Node::Leaf(_, _) => None,
    Node::Split(split) => find_leaf(split, window),
  }
}

fn find_leaf_mut(node: &mut Split, window: WindowId) -> Option<&mut Node> {
  node
    .children
    .iter_mut()
    .find_map(|child| find_leaf_node_mut(&mut child.node, window))
}

fn find_leaf_node_mut(node: &mut Node, window: WindowId) -> Option<&mut Node> {
  match node {
    Node::Leaf(candidate, _) if *candidate == window => Some(node),
    Node::Leaf(_, _) => None,
    Node::Split(split) => find_leaf_mut(split, window),
  }
}

fn exchange_leaf_ids(split: &mut Split, first: WindowId, second: WindowId) {
  for child in &mut split.children {
    match &mut child.node {
      Node::Leaf(window, _) if *window == first => *window = second,
      Node::Leaf(window, _) if *window == second => *window = first,
      Node::Leaf(_, _) => {}
      Node::Split(split) => exchange_leaf_ids(split, first, second),
    }
  }
}

fn take_child(split: &mut Split, window: WindowId) -> Option<LeafState> {
  let index = split.children.iter().position(|child| {
    matches!(&child.node, Node::Leaf(candidate, _) if *candidate == window)
  });
  if let Some(index) = index {
    let child = split.children.remove(index);
    return match child.node {
      Node::Leaf(_, state) => Some(state),
      Node::Split(_) => None,
    };
  }
  for child in &mut split.children {
    if let Node::Split(child_split) = &mut child.node
      && let Some(state) = take_child(child_split, window)
    {
      return Some(state);
    }
  }
  None
}

fn flatten_node(node: &mut Node) -> Option<()> {
  let Node::Split(split) = node else {
    return Some(());
  };
  let children = std::mem::take(&mut split.children);
  split.children = children
    .into_iter()
    .filter_map(|mut child| {
      flatten_node(&mut child.node)?;
      Some(child)
    })
    .collect();
  match split.children.len() {
    0 => None,
    1 => {
      let child = split.children.remove(0);
      *node = child.node;
      Some(())
    }
    _ => Some(()),
  }
}

fn layout_children(
  split: &Split,
  rect: Rect,
  edges: InternalEdges,
  workarea: Rect,
  layouts: &mut Vec<WindowLayout>,
) {
  let visible_children = split
    .children
    .iter()
    .filter(|child| child_visible(&child.node))
    .count();
  let total_weight: u128 = split
    .children
    .iter()
    .filter(|child| child_visible(&child.node))
    .map(|child| child.weight as u128)
    .sum();
  if total_weight == 0 {
    return;
  }
  let vertical = split.direction.is_vertical();
  let extent = rect.axis_extent(vertical);
  let origin = rect.axis_origin(vertical);
  let mut cumulative = 0_u128;
  let mut start = 0_u32;
  let mut visible_index = 0;
  for child in &split.children {
    if !child_visible(&child.node) {
      continue;
    }
    cumulative += child.weight as u128;
    visible_index += 1;
    let end = if visible_index == visible_children {
      extent
    } else {
      (cumulative * extent as u128 / total_weight) as u32
    };
    let child_rect =
      rect.with_axis(vertical, origin + start as i64, end - start);
    start = end;
    let mut child_edges = edges;
    if visible_children > 1 {
      match split.direction {
        SplitDirection::Horizontal => {
          child_edges.top |= visible_index > 1;
          child_edges.bottom |= visible_index < visible_children;
        }
        SplitDirection::Vertical => {
          child_edges.left |= visible_index > 1;
          child_edges.right |= visible_index < visible_children;
        }
      }
    }
    layout_node(&child.node, child_rect, child_edges, workarea, layouts);
  }
}

fn child_visible(node: &Node) -> bool {
  match node {
    Node::Leaf(_, state) => !state.minimized && state.floating.is_none(),
    Node::Split(split) => split
      .children
      .iter()
      .any(|child| child_visible(&child.node)),
  }
}

fn layout_node(
  node: &Node,
  rect: Rect,
  edges: InternalEdges,
  workarea: Rect,
  layouts: &mut Vec<WindowLayout>,
) {
  match node {
    Node::Split(split) => {
      layout_children(split, rect, edges, workarea, layouts)
    }
    Node::Leaf(_, state) if state.minimized || state.floating.is_some() => {}
    Node::Leaf(window, state) => {
      let tiled_gaps = state.gaps.effective(edges);
      let gaps = if state.maximized {
        state.gaps
      } else {
        tiled_gaps
      };
      layouts.push(WindowLayout {
        window: *window,
        node: rect,
        geometry: if state.maximized {
          workarea.inset(state.gaps)
        } else if let Some(snapped) = state.snapped {
          snapped
        } else {
          rect.inset(tiled_gaps)
        },
        gaps,
        maximized: state.maximized,
      });
    }
  }
}

fn resize_node(
  split: &mut Split,
  rect: Rect,
  window: WindowId,
  direction: Direction,
  delta: i32,
  minimum: u32,
) -> Result<Option<bool>> {
  let initial_child_rects = child_rects(split, rect);
  for (index, child) in split.children.iter_mut().enumerate() {
    if !contains(&child.node, window) {
      continue;
    }
    if let Node::Split(child_split) = &mut child.node
      && let Some(changed) = resize_node(
        child_split,
        initial_child_rects[index],
        window,
        direction,
        delta,
        minimum,
      )?
    {
      return Ok(Some(changed));
    }
  }

  if split.direction != direction.axis() {
    return Ok(None);
  }
  let Some(window_index) = split
    .children
    .iter()
    .position(|child| contains(&child.node, window))
  else {
    return Ok(None);
  };
  // Minimized and floating children take no space, so the edge to move is
  // the one between the window and its nearest visible sibling.
  let visible: Vec<usize> = (0..split.children.len())
    .filter(|index| child_visible(&split.children[*index].node))
    .collect();
  let Some(position) = visible.iter().position(|index| *index == window_index)
  else {
    return Ok(None);
  };
  let first_position = match direction {
    Direction::Left | Direction::Up => match position.checked_sub(1) {
      Some(position) => position,
      None => return Ok(None),
    },
    Direction::Right | Direction::Down => position,
  };
  if first_position + 1 >= visible.len() {
    return Ok(None);
  }
  let first_index = visible[first_position];
  let second_index = visible[first_position + 1];

  let vertical = split.direction.is_vertical();
  let total = rect.axis_extent(vertical);
  let first_minimum =
    minimum_extent(&split.children[first_index].node, split.direction, minimum);
  let second_minimum = minimum_extent(
    &split.children[second_index].node,
    split.direction,
    minimum,
  );
  let old_first = initial_child_rects[first_index].axis_extent(vertical);
  let pair_extent =
    old_first + initial_child_rects[second_index].axis_extent(vertical);
  if first_minimum as u64 + second_minimum as u64 > pair_extent as u64 {
    return Err(ShingleError::MinimumExtent);
  }
  let signed_delta = if direction.is_positive() {
    delta as i64
  } else {
    -(delta as i64)
  };
  let desired = old_first as i64 + signed_delta;
  let maximum_first = pair_extent - second_minimum;
  let new_first =
    desired.clamp(first_minimum as i64, maximum_first as i64) as u32;
  if new_first == old_first {
    return Ok(Some(false));
  }

  let scale = total as u64;
  let old_visible_weight: u128 = visible
    .iter()
    .map(|index| split.children[*index].weight as u128)
    .sum();
  let mut new_visible_weight = 0_u128;
  for index in visible.iter().copied() {
    let extent = if index == first_index {
      new_first
    } else if index == second_index {
      pair_extent - new_first
    } else {
      initial_child_rects[index].axis_extent(vertical)
    };
    let weight = if extent == 0 {
      1
    } else {
      extent as u64 * scale
    };
    split.children[index].weight = weight;
    new_visible_weight += weight as u128;
  }
  // Hidden children keep their share of the visible total, so they return
  // at the same relative size when they are shown again.
  for (index, child) in split.children.iter_mut().enumerate() {
    if visible.contains(&index) {
      continue;
    }
    child.weight = (child.weight as u128)
      .checked_mul(new_visible_weight)
      .map_or(u64::MAX as u128, |product| product / old_visible_weight)
      .clamp(1, u64::MAX as u128) as u64;
  }
  Ok(Some(true))
}

/// Returns each child's rectangle as `layout_children` places it. Hidden
/// children get an empty extent at their position.
fn child_rects(split: &Split, rect: Rect) -> Vec<Rect> {
  let vertical = split.direction.is_vertical();
  let extent = rect.axis_extent(vertical);
  let origin = rect.axis_origin(vertical);
  let visible: Vec<bool> = split
    .children
    .iter()
    .map(|child| child_visible(&child.node))
    .collect();
  let visible_children = visible.iter().filter(|visible| **visible).count();
  let total_weight: u128 = split
    .children
    .iter()
    .zip(&visible)
    .filter(|(_, visible)| **visible)
    .map(|(child, _)| child.weight as u128)
    .sum();
  if total_weight == 0 {
    return vec![rect; split.children.len()];
  }
  let mut result = Vec::with_capacity(split.children.len());
  let mut cumulative = 0_u128;
  let mut start = 0_u32;
  let mut visible_index = 0;
  for (child, visible) in split.children.iter().zip(&visible) {
    if !visible {
      result.push(rect.with_axis(vertical, origin + start as i64, 0));
      continue;
    }
    cumulative += child.weight as u128;
    visible_index += 1;
    let end = if visible_index == visible_children {
      extent
    } else {
      (cumulative * extent as u128 / total_weight) as u32
    };
    result.push(rect.with_axis(vertical, origin + start as i64, end - start));
    start = end;
  }
  result
}

fn minimum_extent(node: &Node, direction: SplitDirection, minimum: u32) -> u32 {
  match node {
    Node::Leaf(_, _) => minimum,
    Node::Split(split) if split.direction == direction => split
      .children
      .iter()
      .filter(|child| child_visible(&child.node))
      .fold(0_u32, |total, child| {
        total.saturating_add(minimum_extent(&child.node, direction, minimum))
      }),
    Node::Split(split) => split
      .children
      .iter()
      .filter(|child| child_visible(&child.node))
      .map(|child| minimum_extent(&child.node, direction, minimum))
      .max()
      .unwrap_or(minimum),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const WORKAREA: Rect = Rect::new(0, 0, 1200, 800);

  fn id(value: u64) -> WindowId {
    WindowId(value)
  }

  fn tree_with(count: u64) -> TileTree {
    let mut tree = TileTree::new(SplitDirection::Vertical, WORKAREA).unwrap();
    for value in 1..=count {
      tree.attach(id(value)).unwrap();
    }
    tree
  }

  fn nodes(tree: &TileTree) -> Vec<(u64, Rect)> {
    tree
      .layout()
      .into_iter()
      .map(|layout| (layout.window.get(), layout.node))
      .collect()
  }

  fn widths(tree: &TileTree) -> Vec<u32> {
    tree
      .layout()
      .into_iter()
      .map(|layout| layout.node.width)
      .collect()
  }

  fn floating() -> Option<Rect> {
    Some(Rect::new(100, 100, 300, 200))
  }

  #[test]
  fn window_id_rejects_zero() {
    assert!(matches!(
      WindowId::new(0),
      Err(ShingleError::InvalidWindowId)
    ));
    assert_eq!(WindowId::new(7).unwrap().get(), 7);
  }

  #[test]
  fn new_rejects_empty_and_overflowing_workareas() {
    for workarea in [
      Rect::new(0, 0, 0, 800),
      Rect::new(0, 0, 1200, 0),
      Rect::new(i32::MAX, 0, 1, 1),
    ] {
      assert!(matches!(
        TileTree::new(SplitDirection::Vertical, workarea),
        Err(ShingleError::InvalidWorkarea)
      ));
    }
    let mut tree = tree_with(1);
    assert!(tree.set_workarea(Rect::new(0, 0, 0, 0)).is_err());
    assert_eq!(tree.workarea(), WORKAREA);
  }

  #[test]
  fn attach_appends_in_order_with_equal_extents() {
    let tree = tree_with(3);
    assert_eq!(tree.windows(), vec![id(1), id(2), id(3)]);
    assert_eq!(
      nodes(&tree),
      vec![
        (1, Rect::new(0, 0, 400, 800)),
        (2, Rect::new(400, 0, 400, 800)),
        (3, Rect::new(800, 0, 400, 800)),
      ]
    );
    assert_eq!(tree.focused(), Some(id(3)));
    assert_eq!(tree.stack_order(), &[id(1), id(2), id(3)]);
    assert_eq!(tree.len(), 3);
  }

  #[test]
  fn last_child_absorbs_rounding_remainder() {
    let mut tree = tree_with(3);
    tree.set_workarea(Rect::new(10, 0, 1000, 800)).unwrap();
    assert_eq!(widths(&tree), vec![333, 333, 334]);
    let layout = tree.layout();
    assert_eq!(layout[0].node.x, 10);
    assert_eq!(layout[2].node.x + layout[2].node.width as i32, 1010);
  }

  #[test]
  fn attach_rejects_zero_and_duplicate_ids() {
    let mut tree = tree_with(1);
    assert!(matches!(
      tree.attach(id(0)),
      Err(ShingleError::InvalidWindowId)
    ));
    assert!(matches!(
      tree.attach(id(1)),
      Err(ShingleError::DuplicateWindow(1))
    ));
    assert!(matches!(
      tree.attach_at(id(1), id(1), Direction::Right),
      Err(ShingleError::DuplicateWindow(1))
    ));
    assert!(matches!(
      tree.attach_at(id(2), id(9), Direction::Right),
      Err(ShingleError::WindowNotFound(9))
    ));
    assert_eq!(tree.windows(), vec![id(1)]);
  }

  #[test]
  fn attach_minimized_keeps_focus() {
    let mut tree = tree_with(2);
    tree
      .attach_with_state(
        id(3),
        LeafState {
          minimized: true,
          ..LeafState::default()
        },
      )
      .unwrap();
    assert_eq!(tree.focused(), Some(id(2)));
    assert_eq!(tree.stack_order(), &[id(1), id(2), id(3)]);
    assert_eq!(widths(&tree), vec![600, 600]);
  }

  #[test]
  fn new_child_takes_average_share_and_others_shrink_proportionally() {
    let mut tree = tree_with(2);
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, -300, 1)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![300, 900]);
    tree.attach(id(3)).unwrap();
    assert_eq!(widths(&tree), vec![200, 600, 400]);
  }

  #[test]
  fn attach_at_same_axis_inserts_beside_target() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(1), Direction::Right).unwrap();
    tree.attach_at(id(4), id(1), Direction::Left).unwrap();
    assert_eq!(tree.windows(), vec![id(4), id(1), id(3), id(2)]);
    assert_eq!(widths(&tree), vec![300, 300, 300, 300]);
    assert_eq!(tree.focused(), Some(id(4)));
  }

  #[test]
  fn attach_at_cross_axis_splits_target_slot() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    tree.attach_at(id(4), id(1), Direction::Up).unwrap();
    assert_eq!(tree.windows(), vec![id(4), id(1), id(2), id(3)]);
    assert_eq!(
      nodes(&tree),
      vec![
        (4, Rect::new(0, 0, 600, 400)),
        (1, Rect::new(0, 400, 600, 400)),
        (2, Rect::new(600, 0, 600, 400)),
        (3, Rect::new(600, 400, 600, 400)),
      ]
    );
  }

  #[test]
  fn cross_axis_split_keeps_target_extent() {
    let mut tree = tree_with(2);
    tree
      .resize_adjacent(id(1), Direction::Right, -300, 1)
      .unwrap();
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    assert_eq!(
      nodes(&tree),
      vec![
        (1, Rect::new(0, 0, 300, 800)),
        (2, Rect::new(300, 0, 900, 400)),
        (3, Rect::new(300, 400, 900, 400)),
      ]
    );
    tree.drop(id(1), id(3), Direction::Right).unwrap();
    assert_eq!(
      nodes(&tree),
      vec![
        (2, Rect::new(0, 0, 1200, 400)),
        (3, Rect::new(0, 400, 600, 400)),
        (1, Rect::new(600, 400, 600, 400)),
      ]
    );
  }

  #[test]
  fn detach_returns_state_and_siblings_keep_proportions() {
    let mut tree = tree_with(2);
    tree
      .resize_adjacent(id(1), Direction::Right, -300, 1)
      .unwrap();
    tree.attach(id(3)).unwrap();
    tree.set_sticky(id(3), true).unwrap();
    let state = tree.detach(id(3)).unwrap();
    assert!(state.sticky);
    assert_eq!(widths(&tree), vec![300, 900]);
    assert!(!tree.contains(id(3)));
    assert_eq!(tree.stack_order(), &[id(1), id(2)]);
    assert!(matches!(
      tree.detach(id(3)),
      Err(ShingleError::WindowNotFound(3))
    ));
  }

  #[test]
  fn detach_collapses_single_child_splits() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    tree.detach(id(3)).unwrap();
    assert_eq!(
      nodes(&tree),
      vec![
        (1, Rect::new(0, 0, 600, 800)),
        (2, Rect::new(600, 0, 600, 800)),
      ]
    );
    assert!(
      tree
        .root
        .children
        .iter()
        .all(|child| matches!(child.node, Node::Leaf(_, _)))
    );
  }

  #[test]
  fn detach_promotes_lone_split_to_root() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    tree.detach(id(1)).unwrap();
    assert_eq!(tree.direction(), SplitDirection::Horizontal);
    assert_eq!(
      nodes(&tree),
      vec![
        (2, Rect::new(0, 0, 1200, 400)),
        (3, Rect::new(0, 400, 1200, 400)),
      ]
    );
    tree.detach(id(2)).unwrap();
    tree.detach(id(3)).unwrap();
    assert!(tree.is_empty());
    assert_eq!(tree.focused(), None);
  }

  #[test]
  fn focus_falls_back_to_topmost_unminimized_window() {
    let mut tree = tree_with(3);
    tree.raise(id(1)).unwrap();
    tree.detach(id(3)).unwrap();
    assert_eq!(tree.focused(), Some(id(1)));
    tree.attach(id(3)).unwrap();
    tree.set_minimized(id(1), true).unwrap();
    tree.detach(id(3)).unwrap();
    assert_eq!(tree.focused(), Some(id(2)));
    tree.set_minimized(id(2), true).unwrap();
    assert_eq!(tree.focused(), None);
  }

  #[test]
  fn set_focus_rejects_minimized_and_unknown_windows() {
    let mut tree = tree_with(2);
    tree.set_minimized(id(1), true).unwrap();
    assert!(tree.set_focus(id(1)).is_err());
    assert!(tree.set_focus(id(9)).is_err());
    assert_eq!(tree.focused(), Some(id(2)));
    tree.set_minimized(id(1), false).unwrap();
    tree.set_focus(id(1)).unwrap();
    assert_eq!(tree.focused(), Some(id(1)));
  }

  #[test]
  fn minimizing_unfocused_window_keeps_focus() {
    let mut tree = tree_with(3);
    tree.set_minimized(id(1), true).unwrap();
    assert_eq!(tree.focused(), Some(id(3)));
    assert_eq!(
      nodes(&tree),
      vec![
        (2, Rect::new(0, 0, 600, 800)),
        (3, Rect::new(600, 0, 600, 800)),
      ]
    );
  }

  #[test]
  fn raise_and_lower_reorder_stack_only() {
    let mut tree = tree_with(3);
    tree.raise(id(1)).unwrap();
    assert_eq!(tree.stack_order(), &[id(2), id(3), id(1)]);
    tree.lower(id(3)).unwrap();
    assert_eq!(tree.stack_order(), &[id(3), id(2), id(1)]);
    assert_eq!(tree.windows(), vec![id(1), id(2), id(3)]);
    assert!(tree.raise(id(9)).is_err());
    assert!(tree.lower(id(9)).is_err());
    tree.set_stacking(id(2), StackingMode::AlwaysOnTop).unwrap();
    assert_eq!(tree.stack_order(), &[id(3), id(1), id(2)]);
    assert_eq!(
      tree.leaf_state(id(2)).unwrap().stacking,
      StackingMode::AlwaysOnTop
    );
  }

  #[test]
  fn drop_moves_window_and_raises_it() {
    let mut tree = tree_with(3);
    tree.set_focus(id(2)).unwrap();
    tree.drop(id(1), id(3), Direction::Right).unwrap();
    assert_eq!(tree.windows(), vec![id(2), id(3), id(1)]);
    assert_eq!(widths(&tree), vec![400, 400, 400]);
    assert_eq!(tree.focused(), Some(id(1)));
    assert_eq!(tree.stack_order(), &[id(2), id(3), id(1)]);
  }

  #[test]
  fn drop_cross_axis_stacks_onto_target() {
    let mut tree = tree_with(3);
    tree.drop(id(3), id(1), Direction::Down).unwrap();
    assert_eq!(
      nodes(&tree),
      vec![
        (1, Rect::new(0, 0, 600, 400)),
        (3, Rect::new(0, 400, 600, 400)),
        (2, Rect::new(600, 0, 600, 800)),
      ]
    );
  }

  #[test]
  fn drop_rejects_same_or_unknown_windows() {
    let mut tree = tree_with(2);
    assert!(matches!(
      tree.drop(id(1), id(1), Direction::Left),
      Err(ShingleError::SameWindow)
    ));
    assert!(matches!(
      tree.drop(id(9), id(1), Direction::Left),
      Err(ShingleError::WindowNotFound(9))
    ));
    assert!(matches!(
      tree.drop(id(1), id(9), Direction::Left),
      Err(ShingleError::WindowNotFound(9))
    ));
    assert_eq!(tree.windows(), vec![id(1), id(2)]);
  }

  #[test]
  fn drop_minimized_window_keeps_focus() {
    let mut tree = tree_with(3);
    tree.set_minimized(id(1), true).unwrap();
    tree.drop(id(1), id(3), Direction::Right).unwrap();
    assert_eq!(tree.windows(), vec![id(2), id(3), id(1)]);
    assert_eq!(tree.focused(), Some(id(3)));
  }

  #[test]
  fn swap_exchanges_slots_and_keeps_window_state() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    tree
      .resize_adjacent(id(1), Direction::Right, -200, 1)
      .unwrap();
    tree.set_gaps(id(1), Gaps::new(5, 2)).unwrap();
    tree.set_focus(id(2)).unwrap();
    tree.swap(id(1), id(3)).unwrap();
    assert_eq!(
      nodes(&tree),
      vec![
        (3, Rect::new(0, 0, 400, 800)),
        (2, Rect::new(400, 0, 800, 400)),
        (1, Rect::new(400, 400, 800, 400)),
      ]
    );
    assert_eq!(tree.leaf_state(id(1)).unwrap().gaps, Gaps::new(5, 2));
    assert_eq!(tree.leaf_state(id(3)).unwrap().gaps, Gaps::default());
    assert_eq!(tree.focused(), Some(id(2)));
    assert!(matches!(
      tree.swap(id(1), id(1)),
      Err(ShingleError::SameWindow)
    ));
    assert!(tree.swap(id(1), id(9)).is_err());
  }

  #[test]
  fn focus_directional_without_neighbour_keeps_focus() {
    let mut tree = tree_with(2);
    tree.set_focus(id(1)).unwrap();
    assert_eq!(
      tree.focus_directional(id(1), Direction::Left).unwrap(),
      None
    );
    assert_eq!(tree.focus_directional(id(1), Direction::Up).unwrap(), None);
    assert_eq!(tree.focused(), Some(id(1)));
  }

  #[test]
  fn focus_directional_skips_hidden_windows() {
    let mut tree = tree_with(4);
    tree.set_minimized(id(2), true).unwrap();
    tree.set_floating(id(3), floating()).unwrap();
    assert_eq!(
      tree.focus_directional(id(1), Direction::Right).unwrap(),
      Some(id(4))
    );
    assert!(matches!(
      tree.focus_directional(id(3), Direction::Left),
      Err(ShingleError::WindowNotFound(3))
    ));
  }

  #[test]
  fn resize_moves_only_the_shared_edge() {
    let mut tree = tree_with(3);
    assert!(
      tree
        .resize_adjacent(id(2), Direction::Right, 100, 50)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![400, 500, 300]);
    assert!(
      tree
        .resize_adjacent(id(2), Direction::Left, 100, 50)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![300, 600, 300]);
    assert!(
      tree
        .resize_adjacent(id(3), Direction::Left, -50, 50)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![300, 650, 250]);
  }

  #[test]
  fn resize_clamps_to_minimum_extent() {
    let mut tree = tree_with(2);
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, 10_000, 100)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![1100, 100]);
    assert!(
      !tree
        .resize_adjacent(id(1), Direction::Right, 10_000, 100)
        .unwrap()
    );
    assert!(matches!(
      tree.resize_adjacent(id(1), Direction::Right, 1, 700),
      Err(ShingleError::MinimumExtent)
    ));
    assert!(matches!(
      tree.resize_adjacent(id(1), Direction::Right, 1, 0),
      Err(ShingleError::MinimumExtent)
    ));
    assert_eq!(widths(&tree), vec![1100, 100]);
  }

  #[test]
  fn resize_without_adjacent_split_fails() {
    let mut tree = tree_with(2);
    for (window, direction) in [
      (1, Direction::Left),
      (2, Direction::Right),
      (1, Direction::Up),
      (1, Direction::Down),
    ] {
      assert!(matches!(
        tree.resize_adjacent(id(window), direction, 10, 1),
        Err(ShingleError::NoAdjacentWindow)
      ));
    }
    assert!(matches!(
      tree.resize_adjacent(id(9), Direction::Left, 10, 1),
      Err(ShingleError::WindowNotFound(9))
    ));
  }

  #[test]
  fn resize_nested_split_uses_innermost_matching_axis() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    assert!(
      tree
        .resize_adjacent(id(2), Direction::Down, 100, 50)
        .unwrap()
    );
    assert!(
      tree
        .resize_adjacent(id(3), Direction::Left, 100, 50)
        .unwrap()
    );
    assert_eq!(
      nodes(&tree),
      vec![
        (1, Rect::new(0, 0, 500, 800)),
        (2, Rect::new(500, 0, 700, 500)),
        (3, Rect::new(500, 500, 700, 300)),
      ]
    );
  }

  #[test]
  fn resize_respects_minimum_of_nested_split() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    tree.attach_at(id(4), id(3), Direction::Right).unwrap();
    // The right column holds 2 above a row of 3 and 4, so it needs two
    // minimum extents of width.
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, 10_000, 100)
        .unwrap()
    );
    assert_eq!(tree.window_layout(id(1)).unwrap().node.width, 1000);
    assert_eq!(tree.window_layout(id(2)).unwrap().node.width, 200);
    assert_eq!(tree.window_layout(id(3)).unwrap().node.width, 100);
    assert_eq!(tree.window_layout(id(4)).unwrap().node.width, 100);
  }

  #[test]
  fn resize_ignores_floating_sibling() {
    let mut tree = tree_with(3);
    tree.set_floating(id(2), floating()).unwrap();
    assert_eq!(widths(&tree), vec![600, 600]);
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, 100, 50)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![700, 500]);
    assert!(
      tree
        .resize_adjacent(id(3), Direction::Left, 100, 50)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![600, 600]);
    // The tiled pair may use the whole width; the floating window's slot
    // does not reserve a minimum extent.
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, 10_000, 100)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![1100, 100]);
  }

  #[test]
  fn resize_keeps_hidden_window_share() {
    let mut tree = tree_with(3);
    tree.set_minimized(id(2), true).unwrap();
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, 100, 50)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![700, 500]);
    tree.set_minimized(id(2), false).unwrap();
    assert_eq!(tree.window_layout(id(2)).unwrap().node.width, 400);
  }

  #[test]
  fn resize_hidden_window_has_no_edge() {
    let mut tree = tree_with(2);
    tree.set_minimized(id(1), true).unwrap();
    assert!(matches!(
      tree.resize_adjacent(id(1), Direction::Right, 100, 50),
      Err(ShingleError::NoAdjacentWindow)
    ));
    tree.set_minimized(id(1), false).unwrap();
    assert_eq!(widths(&tree), vec![600, 600]);
  }

  #[test]
  fn resize_minimum_ignores_hidden_windows_in_nested_split() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    tree.attach_at(id(4), id(3), Direction::Right).unwrap();
    tree.set_minimized(id(4), true).unwrap();
    assert!(
      tree
        .resize_adjacent(id(1), Direction::Right, 10_000, 100)
        .unwrap()
    );
    assert_eq!(widths(&tree), vec![1100, 100, 100]);
  }

  #[test]
  fn float_and_unfloat_restores_tile_slot() {
    let mut tree = tree_with(3);
    tree
      .resize_adjacent(id(1), Direction::Right, 100, 50)
      .unwrap();
    let before = nodes(&tree);
    tree.set_floating(id(2), floating()).unwrap();
    let state = tree.leaf_state(id(2)).unwrap();
    assert_eq!(state.floating, floating());
    assert_eq!(state.tiled_edges, 0);
    assert!(tree.window_layout(id(2)).is_err());
    assert_eq!(tree.windows(), vec![id(1), id(2), id(3)]);
    tree.set_floating(id(2), None).unwrap();
    assert_eq!(tree.leaf_state(id(2)).unwrap().tiled_edges, 15);
    assert_eq!(nodes(&tree), before);
  }

  #[test]
  fn floating_clears_snap() {
    let mut tree = tree_with(2);
    tree
      .set_snapped(id(1), Some(Rect::new(0, 0, 600, 400)))
      .unwrap();
    tree.set_floating(id(1), floating()).unwrap();
    let state = tree.leaf_state(id(1)).unwrap();
    assert_eq!(state.snapped, None);
    assert_eq!(state.restore_geometry, None);
  }

  #[test]
  fn snap_records_restore_geometry_once() {
    let mut tree = tree_with(2);
    tree.set_maximized(id(1), true).unwrap();
    tree.set_fullscreen(id(1), true).unwrap();
    tree.set_maximized(id(1), false).unwrap();
    let tiled = tree.window_layout(id(1)).unwrap().geometry;
    let first = Rect::new(0, 0, 600, 400);
    tree.set_snapped(id(1), Some(first)).unwrap();
    let state = tree.leaf_state(id(1)).unwrap();
    assert_eq!(state.snapped, Some(first));
    assert_eq!(state.restore_geometry, Some(tiled));
    assert!(!state.fullscreen && !state.maximized);
    let layout = tree.window_layout(id(1)).unwrap();
    assert_eq!(layout.geometry, first);
    assert_eq!(layout.node, Rect::new(0, 0, 600, 800));

    let second = Rect::new(0, 400, 600, 400);
    tree.set_snapped(id(1), Some(second)).unwrap();
    assert_eq!(
      tree.leaf_state(id(1)).unwrap().restore_geometry,
      Some(tiled)
    );
    tree.set_snapped(id(1), None).unwrap();
    let state = tree.leaf_state(id(1)).unwrap();
    assert_eq!(state.snapped, None);
    assert_eq!(state.restore_geometry, None);
    assert!(
      tree
        .set_snapped(id(1), Some(Rect::new(0, 0, 0, 1)))
        .is_err()
    );
  }

  #[test]
  fn snapping_floating_window_restores_floating_geometry() {
    let mut tree = tree_with(2);
    tree.set_floating(id(1), floating()).unwrap();
    tree
      .set_snapped(id(1), Some(Rect::new(0, 0, 600, 400)))
      .unwrap();
    let state = tree.leaf_state(id(1)).unwrap();
    assert_eq!(state.restore_geometry, floating());
    assert_eq!(state.floating, None);
  }

  #[test]
  fn tiled_edges_mask_is_bounded() {
    let mut tree = tree_with(1);
    tree.set_tiled_edges(id(1), 5).unwrap();
    assert_eq!(tree.leaf_state(id(1)).unwrap().tiled_edges, 5);
    assert!(matches!(
      tree.set_tiled_edges(id(1), 16),
      Err(ShingleError::InvalidTiledEdges(16))
    ));
    assert_eq!(tree.leaf_state(id(1)).unwrap().tiled_edges, 5);
    assert_eq!(LeafState::default().tiled_edges, 15);
  }

  #[test]
  fn gaps_use_inner_size_on_shared_edges() {
    let mut tree = tree_with(2);
    tree.attach_at(id(3), id(2), Direction::Down).unwrap();
    let gaps = Gaps::new(10, 4);
    for window in 1..=3 {
      tree.set_gaps(id(window), gaps).unwrap();
    }
    let geometry: Vec<Rect> =
      tree.layout().into_iter().map(|l| l.geometry).collect();
    assert_eq!(
      geometry,
      vec![
        Rect::new(10, 10, 586, 780),
        Rect::new(604, 10, 586, 386),
        Rect::new(604, 404, 586, 386),
      ]
    );
    let layout = tree.window_layout(id(2)).unwrap();
    assert_eq!(
      layout.gaps,
      Gaps {
        top: 10,
        right: 10,
        bottom: 4,
        left: 4,
        inner: 4,
      }
    );
  }

  #[test]
  fn maximized_window_fills_workarea_but_keeps_slot() {
    let mut tree = tree_with(2);
    tree.set_gaps(id(2), Gaps::new(8, 2)).unwrap();
    tree.set_maximized(id(2), true).unwrap();
    let layout = tree.window_layout(id(2)).unwrap();
    assert!(layout.maximized);
    assert_eq!(layout.node, Rect::new(600, 0, 600, 800));
    assert_eq!(layout.geometry, Rect::new(8, 8, 1184, 784));
    assert_eq!(layout.gaps, Gaps::new(8, 2));
    assert_eq!(tree.window_layout(id(1)).unwrap().node.width, 600);
  }

  #[test]
  fn window_at_hit_tests_tile_nodes() {
    let tree = tree_with(2);
    assert_eq!(tree.window_at(0, 0), Some(id(1)));
    assert_eq!(tree.window_at(599, 799), Some(id(1)));
    assert_eq!(tree.window_at(600, 0), Some(id(2)));
    assert_eq!(tree.window_at(1200, 0), None);
    assert_eq!(tree.window_at(0, 800), None);
    assert_eq!(tree.window_at(-1, 0), None);
  }

  #[test]
  fn setters_reject_unknown_windows() {
    let mut tree = tree_with(1);
    let missing = id(9);
    assert!(tree.set_maximized(missing, true).is_err());
    assert!(tree.set_minimized(missing, true).is_err());
    assert!(tree.set_floating(missing, None).is_err());
    assert!(tree.set_snapped(missing, None).is_err());
    assert!(tree.set_fullscreen(missing, true).is_err());
    assert!(tree.set_sticky(missing, true).is_err());
    assert!(tree.set_stacking(missing, StackingMode::Normal).is_err());
    assert!(tree.set_gaps(missing, Gaps::default()).is_err());
    assert!(tree.set_tiled_edges(missing, 0).is_err());
    assert!(tree.leaf_state(missing).is_err());
    assert!(tree.window_layout(missing).is_err());
  }

  #[test]
  fn replace_leaf_keeps_slot_focus_and_stack_position() {
    let mut tree = tree_with(3);
    tree.set_focus(id(2)).unwrap();
    let previous = tree
      .replace_leaf(id(2), id(7), LeafState::default())
      .unwrap();
    assert_eq!(previous, LeafState::default());
    assert_eq!(tree.windows(), vec![id(1), id(7), id(3)]);
    assert_eq!(tree.focused(), Some(id(7)));
    assert_eq!(tree.stack_order(), &[id(1), id(7), id(3)]);
    assert!(matches!(
      tree.replace_leaf(id(1), id(3), LeafState::default()),
      Err(ShingleError::DuplicateWindow(3))
    ));
  }
}
