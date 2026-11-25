use serde::{Deserialize, Serialize};

use crate::geometry::{Direction, Gaps, InternalEdges, Rect, SplitDirection};
use crate::{Result, TilefireError};

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
            return Err(TilefireError::InvalidWindowId);
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
        self.root
            .children
            .iter()
            .map(|child| count_leaves(&child.node))
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.root.children.is_empty()
    }

    pub fn contains(&self, window: WindowId) -> bool {
        self.root
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
            return Err(TilefireError::WindowNotFound(window.0));
        }
        self.focused = Some(window);
        Ok(())
    }

    pub fn raise(&mut self, window: WindowId) -> Result<()> {
        let Some(index) =
            self.stack.iter().position(|candidate| *candidate == window)
        else {
            return Err(TilefireError::WindowNotFound(window.0));
        };
        self.stack.remove(index);
        self.stack.push(window);
        Ok(())
    }

    pub fn lower(&mut self, window: WindowId) -> Result<()> {
        let Some(index) =
            self.stack.iter().position(|candidate| *candidate == window)
        else {
            return Err(TilefireError::WindowNotFound(window.0));
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
            return Err(TilefireError::DuplicateWindow(window.0));
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
        self.attach_at_with_state(
            window,
            LeafState::default(),
            target,
            position,
        )
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
            return Err(TilefireError::DuplicateWindow(window.0));
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
            return Err(TilefireError::SameWindow);
        }
        if !self.contains(source) {
            return Err(TilefireError::WindowNotFound(source.0));
        }
        if !self.contains(target) {
            return Err(TilefireError::WindowNotFound(target.0));
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
            return Err(TilefireError::SameWindow);
        }
        if !self.contains(first) {
            return Err(TilefireError::WindowNotFound(first.0));
        }
        if !self.contains(second) {
            return Err(TilefireError::WindowNotFound(second.0));
        }
        let first_state = self.leaf_state(first)?;
        let second_state = self.leaf_state(second)?;
        exchange_leaf_ids(&mut self.root, first, second);
        let first_leaf = find_leaf_mut(&mut self.root, first)
            .ok_or(TilefireError::WindowNotFound(first.0))?;
        if let Node::Leaf(_, state) = first_leaf {
            *state = first_state;
        }
        let second_leaf = find_leaf_mut(&mut self.root, second)
            .ok_or(TilefireError::WindowNotFound(second.0))?;
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
            .ok_or(TilefireError::WindowNotFound(from.0))?;
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
            return Err(TilefireError::MinimumExtent);
        }
        if !self.contains(window) {
            return Err(TilefireError::WindowNotFound(window.0));
        }
        resize_node(
            &mut self.root,
            self.workarea,
            window,
            direction,
            delta,
            minimum_extent,
        )?
        .ok_or(TilefireError::NoAdjacentWindow)
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
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
                self.window_layout(window)
                    .ok()
                    .map(|layout| layout.geometry)
            })
        };
        let (_, state) = find_leaf_mut(&mut self.root, window)
            .and_then(|node| match node {
                Node::Leaf(window, state) => Some((*window, state)),
                Node::Split(_) => None,
            })
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
            return Err(TilefireError::InvalidTiledEdges(edges));
        }
        let (_, state) = find_leaf_mut(&mut self.root, window)
            .and_then(|node| match node {
                Node::Leaf(window, state) => Some((*window, state)),
                Node::Split(_) => None,
            })
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
            .ok_or(TilefireError::WindowNotFound(window.0))?;
        state.fullscreen = fullscreen;
        Ok(())
    }

    pub fn set_sticky(&mut self, window: WindowId, sticky: bool) -> Result<()> {
        let (_, state) = find_leaf_mut(&mut self.root, window)
            .and_then(|node| match node {
                Node::Leaf(window, state) => Some((*window, state)),
                Node::Split(_) => None,
            })
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
            .ok_or(TilefireError::WindowNotFound(window.0))?;
        state.stacking = mode;
        self.raise(window)
    }

    pub fn set_gaps(&mut self, window: WindowId, gaps: Gaps) -> Result<()> {
        let (_, state) = find_leaf_mut(&mut self.root, window)
            .and_then(|node| match node {
                Node::Leaf(window, state) => Some((*window, state)),
                Node::Split(_) => None,
            })
            .ok_or(TilefireError::WindowNotFound(window.0))?;
        state.gaps = gaps;
        Ok(())
    }

    pub fn leaf_state(&self, window: WindowId) -> Result<LeafState> {
        find_leaf(&self.root, window)
            .and_then(|node| match node {
                Node::Leaf(_, state) => Some(*state),
                Node::Split(_) => None,
            })
            .ok_or(TilefireError::WindowNotFound(window.0))
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
        self.layout()
            .into_iter()
            .find(|layout| layout.window == window)
            .ok_or(TilefireError::WindowNotFound(window.0))
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
            return Err(TilefireError::DuplicateWindow(new.0));
        }
        let node = find_leaf_mut(&mut self.root, old)
            .ok_or(TilefireError::WindowNotFound(old.0))?;
        let previous = match node {
            Node::Leaf(_, state) => *state,
            Node::Split(_) => return Err(TilefireError::WindowNotFound(old.0)),
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
            .ok_or(TilefireError::WindowNotFound(target.0))?;
        let target_index = parent
            .children
            .iter()
            .position(
                |child| matches!(child.node, Node::Leaf(id, _) if id == target),
            )
            .ok_or(TilefireError::WindowNotFound(target.0))?;
        if parent.direction == direction {
            let weight = added_child_weight(parent);
            let insertion_index =
                target_index + usize::from(position.is_positive());
            parent.children.insert(
                insertion_index,
                Child {
                    weight,
                    node: Node::Leaf(window, state),
                },
            );
        } else {
            let target_node = parent.children.remove(target_index).node;
            let weight = added_child_weight(parent);
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
            .ok_or(TilefireError::WindowNotFound(window.0))?;
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
    node.children
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
    node.children
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
    if let Some(index) = split
        .children
        .iter()
        .position(|child| matches!(&child.node, Node::Leaf(candidate, _) if *candidate == window))
    {
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
        Node::Leaf(_, state) if state.minimized || state.floating.is_some() => {
        }
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
    let first_index = match direction {
        Direction::Left | Direction::Up => match window_index.checked_sub(1) {
            Some(index) => index,
            None => return Ok(None),
        },
        Direction::Right | Direction::Down => window_index,
    };
    if first_index + 1 >= split.children.len() {
        return Ok(None);
    }

    let vertical = split.direction.is_vertical();
    let total = rect.axis_extent(vertical);
    let first_minimum = minimum_extent(
        &split.children[first_index].node,
        split.direction,
        minimum,
    );
    let second_minimum = minimum_extent(
        &split.children[first_index + 1].node,
        split.direction,
        minimum,
    );
    let old_first = initial_child_rects[first_index].axis_extent(vertical);
    let pair_extent =
        old_first + initial_child_rects[first_index + 1].axis_extent(vertical);
    if first_minimum as u64 + second_minimum as u64 > pair_extent as u64 {
        return Err(TilefireError::MinimumExtent);
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
    for (index, child) in split.children.iter_mut().enumerate() {
        let extent = if index == first_index {
            new_first
        } else if index == first_index + 1 {
            pair_extent - new_first
        } else {
            initial_child_rects[index].axis_extent(vertical)
        };
        child.weight = if extent == 0 {
            1
        } else {
            extent as u64 * scale
        };
    }
    Ok(Some(true))
}

fn child_rects(split: &Split, rect: Rect) -> Vec<Rect> {
    let total_weight: u128 = split
        .children
        .iter()
        .map(|child| child.weight as u128)
        .sum();
    if total_weight == 0 {
        return vec![rect; split.children.len()];
    }
    let vertical = split.direction.is_vertical();
    let extent = rect.axis_extent(vertical);
    let origin = rect.axis_origin(vertical);
    let mut result = Vec::with_capacity(split.children.len());
    let mut cumulative = 0_u128;
    let mut start = 0_u32;
    for (index, child) in split.children.iter().enumerate() {
        cumulative += child.weight as u128;
        let end = if index + 1 == split.children.len() {
            extent
        } else {
            (cumulative * extent as u128 / total_weight) as u32
        };
        result.push(rect.with_axis(
            vertical,
            origin + start as i64,
            end - start,
        ));
        start = end;
    }
    result
}

fn minimum_extent(node: &Node, direction: SplitDirection, minimum: u32) -> u32 {
    match node {
        Node::Leaf(_, _) => minimum,
        Node::Split(split) if split.direction == direction => {
            split.children.iter().fold(0_u32, |total, child| {
                total.saturating_add(minimum_extent(
                    &child.node,
                    direction,
                    minimum,
                ))
            })
        }
        Node::Split(split) => split
            .children
            .iter()
            .map(|child| minimum_extent(&child.node, direction, minimum))
            .max()
            .unwrap_or(minimum),
    }
}
