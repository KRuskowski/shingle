use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::core::{Place, WindowRecord, WsetState};
use crate::tree::{Child, Node, Split};
use crate::{
  LaunchIdentity, LeafState, OutputId, OutputIdentity, Rect, Result, Shingle,
  ShingleError, SplitDirection, TileTree, WindowId, WorkspaceId, WsetId,
};

/// Version written to new snapshots. Version 1, which kept one record per
/// output instead of per workspace set, is still read.
pub const SNAPSHOT_VERSION: u32 = 2;
const MAX_TREE_DEPTH: usize = 64;
const MAX_WINDOWS: usize = 65_536;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The serializable form of a [`Shingle`] model.
///
/// A snapshot is valid only if it restores; [`SessionSnapshot::validate`]
/// checks the same invariants as restoring. Window presence is not stored, so
/// every window restores as a placeholder.
///
/// Deserializing accepts version 1 files too: each saved output becomes a
/// workspace set shown on that output, numbered from 1 in file order, and
/// the snapshot is then at [`SNAPSHOT_VERSION`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
  /// Must be [`SNAPSHOT_VERSION`].
  pub version: u32,
  /// The focused output when the session was saved; nonempty if present.
  pub active_output: Option<OutputId>,
  /// Workspace sets with unique, nonzero indices, sorted by index when
  /// produced by [`Shingle::snapshot`].
  pub wsets: Vec<WsetSnapshot>,
  /// One record per window, each matching exactly one leaf in one workspace
  /// tree.
  pub windows: Vec<WindowSnapshot>,
}

/// One workspace set: where it is shown, its workarea, workspaces and
/// current workspace, and the copy of its trees kept for windows that are
/// away from it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WsetSnapshot {
  /// The set's index; nonzero.
  pub wset: WsetId,
  /// The output showing the set, or `None` while it is parked. Each output
  /// shows at most one set, and a shown set's `home` is that output.
  #[serde(default)]
  pub output: Option<OutputId>,
  /// The output the set was last shown on; nonempty if present.
  #[serde(default)]
  pub home: Option<OutputId>,
  /// That output's EDID identity.
  #[serde(default)]
  pub identity: Option<OutputIdentity>,
  /// The set's workarea; must be valid.
  pub workarea: Rect,
  /// The current workspace; must be one of `workspaces`.
  pub current_workspace: WorkspaceId,
  /// At least one workspace, with unique ids.
  pub workspaces: Vec<WorkspaceSnapshot>,
  /// The trees as they were when an unplug moved the set's windows away;
  /// see [`Shingle::return_windows`]. They follow the rules of
  /// `workspaces`, except that their windows need not exist: windows that
  /// were closed or moved since are left out when the copy is used.
  #[serde(default)]
  pub away: Option<Vec<WorkspaceSnapshot>>,
}

/// One output's workarea, workspaces and current workspace: the record of
/// a version 1 snapshot, read when loading one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSnapshot {
  /// The output.
  pub output: OutputId,
  /// The output's workarea; must be valid.
  pub workarea: Rect,
  /// The current workspace; must be one of `workspaces`.
  pub current_workspace: WorkspaceId,
  /// At least one workspace, with unique ids.
  pub workspaces: Vec<WorkspaceSnapshot>,
}

/// One workspace's tiling tree, focus and stacking order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
  /// The workspace.
  pub workspace: WorkspaceId,
  /// The root split. It may be empty; nested splits need at least two children,
  /// and nesting is limited to 64 levels.
  pub root: SplitSnapshot,
  /// The focused window, which must be a leaf of this tree.
  pub focused: Option<WindowId>,
  /// Stacking order, bottom first, listing every leaf of this tree exactly
  /// once.
  pub stack: Vec<WindowId>,
}

/// A split and its children.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitSnapshot {
  /// The axis the children are arranged on.
  pub direction: SplitDirection,
  /// The children, in order.
  pub children: Vec<ChildSnapshot>,
}

/// A child of a split with its weight.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildSnapshot {
  /// Nonzero relative size. The child's share is its weight over the total
  /// weight of its visible siblings.
  pub weight: u64,
  /// The child node.
  pub node: NodeSnapshot,
}

/// A tree node, tagged by `kind` in JSON.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeSnapshot {
  /// A nested split.
  Split(SplitSnapshot),
  /// A window's leaf.
  Leaf {
    /// The window, which needs a nonzero id that occurs once in the session and
    /// a matching [`WindowSnapshot`].
    window: WindowId,
    /// The window's state. Floating and snapped are exclusive, restore geometry
    /// requires a snap, geometry must be valid and the tiled-edges mask must
    /// not exceed 15.
    state: LeafState,
  },
}

/// A window's identity, stored separately from its leaf.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSnapshot {
  /// The window.
  pub id: WindowId,
  /// How the window was launched.
  pub identity: LaunchIdentity,
  /// Parent window, which must exist and must not form a cycle.
  #[serde(default)]
  pub parent: Option<WindowId>,
  /// The set the window returns to when its output comes back; it must
  /// exist and differ from the set holding the window.
  #[serde(default)]
  pub away_from: Option<WsetId>,
}

/// The current layout, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotLayout {
  version: u32,
  #[serde(default)]
  active_output: Option<OutputId>,
  wsets: Vec<WsetSnapshot>,
  windows: Vec<WindowSnapshot>,
}

/// Version 1's layout.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotLayoutV1 {
  #[allow(dead_code)]
  version: u32,
  outputs: Vec<OutputSnapshot>,
  windows: Vec<WindowSnapshot>,
}

impl<'de> Deserialize<'de> for SessionSnapshot {
  fn deserialize<D: serde::Deserializer<'de>>(
    deserializer: D,
  ) -> std::result::Result<Self, D::Error> {
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.get("version").and_then(serde_json::Value::as_u64) == Some(1) {
      let old: SnapshotLayoutV1 =
        serde_json::from_value(value).map_err(D::Error::custom)?;
      return Ok(Self::from_version_1(old.outputs, old.windows));
    }
    let snapshot: SnapshotLayout =
      serde_json::from_value(value).map_err(D::Error::custom)?;
    Ok(Self {
      version: snapshot.version,
      active_output: snapshot.active_output,
      wsets: snapshot.wsets,
      windows: snapshot.windows,
    })
  }
}

impl Shingle {
  /// Captures the whole model. Presence is not captured.
  pub fn snapshot(&self) -> SessionSnapshot {
    let wsets = self
      .wsets
      .iter()
      .map(|(wset, state)| WsetSnapshot {
        wset: *wset,
        output: state.output.clone(),
        home: state.home.clone(),
        identity: state.identity.clone(),
        workarea: state.workarea,
        current_workspace: state.current_workspace,
        workspaces: state
          .workspaces
          .iter()
          .map(|(workspace, tree)| workspace_snapshot(*workspace, tree))
          .collect(),
        away: state.away.clone(),
      })
      .collect();
    let windows = self
      .windows
      .iter()
      .map(|(id, record)| WindowSnapshot {
        id: *id,
        identity: record.identity.clone(),
        parent: record.parent,
        away_from: record.away_from,
      })
      .collect();
    SessionSnapshot {
      version: SNAPSHOT_VERSION,
      active_output: self.active_output.clone(),
      wsets,
      windows,
    }
  }

  /// Serializes the model as pretty-printed JSON ending in a newline.
  pub fn to_json(&self) -> Result<String> {
    self.snapshot().to_json()
  }

  /// Restores a model from JSON, of this version or version 1.
  ///
  /// Tile order, weights, state, focus, stacking, current workspaces and
  /// which output shows which set come back exactly. Every window starts as
  /// a placeholder, and new window ids continue after the highest restored
  /// id.
  ///
  /// # Errors
  ///
  /// [`ShingleError::Json`] for invalid JSON or unknown fields,
  /// [`ShingleError::UnsupportedSnapshotVersion`] or
  /// [`ShingleError::MalformedSnapshot`].
  pub fn from_json(json: &str) -> Result<Self> {
    let snapshot = SessionSnapshot::from_json(json)?;
    Self::from_snapshot(snapshot)
  }

  /// Restores a model from a snapshot; see [`Shingle::from_json`].
  pub fn from_snapshot(snapshot: SessionSnapshot) -> Result<Self> {
    restore_snapshot(snapshot)
  }

  /// Writes the model to `path` atomically.
  ///
  /// The JSON is written to a temporary file in the same directory, synced and
  /// renamed over `path`, then the directory is synced. On failure the
  /// temporary file is removed and `path` is left as it was.
  ///
  /// # Errors
  ///
  /// [`ShingleError::InvalidPersistencePath`] if `path` has no file name,
  /// [`ShingleError::Io`] if writing fails.
  pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<()> {
    self.snapshot().save_atomic(path)
  }

  /// Reads and restores a model saved by [`Shingle::save_atomic`]; see
  /// [`Shingle::from_json`].
  pub fn load(path: impl AsRef<Path>) -> Result<Self> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let snapshot: SessionSnapshot = serde_json::from_slice(&bytes)?;
    Self::from_snapshot(snapshot)
  }
}

impl SessionSnapshot {
  /// Parses a snapshot, of this version or version 1, and checks that it
  /// restores.
  pub fn from_json(json: &str) -> Result<Self> {
    let snapshot: Self = serde_json::from_str(json)?;
    restore_snapshot(snapshot.clone())?;
    Ok(snapshot)
  }

  /// Checks that the snapshot would restore.
  pub fn validate(&self) -> Result<()> {
    restore_snapshot(self.clone()).map(|_| ())
  }

  /// Validates the snapshot and serializes it as pretty-printed JSON.
  pub fn to_json(&self) -> Result<String> {
    self.validate()?;
    serialize_snapshot(self)
  }

  /// Validates the snapshot and writes it atomically; see
  /// [`Shingle::save_atomic`].
  pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<()> {
    self.validate()?;
    let json = serialize_snapshot(self)?;
    atomic_write(path.as_ref(), json.as_bytes())
  }

  /// Converts version 1's records: each output becomes a set shown on it,
  /// numbered from 1 in order.
  fn from_version_1(
    outputs: Vec<OutputSnapshot>,
    windows: Vec<WindowSnapshot>,
  ) -> Self {
    let wsets = outputs
      .into_iter()
      .zip(1u32..)
      .map(|(output, index)| WsetSnapshot {
        wset: WsetId(index),
        output: Some(output.output.clone()),
        home: Some(output.output),
        identity: None,
        workarea: output.workarea,
        current_workspace: output.current_workspace,
        workspaces: output.workspaces,
        away: None,
      })
      .collect();
    Self {
      version: SNAPSHOT_VERSION,
      active_output: None,
      wsets,
      windows,
    }
  }
}

fn serialize_snapshot(snapshot: &SessionSnapshot) -> Result<String> {
  let mut json = serde_json::to_string_pretty(snapshot)?;
  json.push('\n');
  Ok(json)
}

/// A workspace's tree as a snapshot.
pub(crate) fn workspace_snapshot(
  workspace: WorkspaceId,
  tree: &TileTree,
) -> WorkspaceSnapshot {
  WorkspaceSnapshot {
    workspace,
    root: split_snapshot(&tree.root),
    focused: tree.focused(),
    stack: tree.stack_order().to_vec(),
  }
}

/// A copy of `snapshot` with only the windows `keep` accepts. A nested
/// split left with one child is replaced by it, in the split's slot and
/// with its weight, and one left empty is removed; the root stays even
/// when empty. Focus is kept only on a kept window.
pub(crate) fn prune_workspace(
  snapshot: &WorkspaceSnapshot,
  keep: impl Fn(WindowId) -> bool,
) -> WorkspaceSnapshot {
  fn prune_split(
    split: &SplitSnapshot,
    keep: &dyn Fn(WindowId) -> bool,
  ) -> SplitSnapshot {
    let children = split
      .children
      .iter()
      .filter_map(|child| {
        let node = match &child.node {
          NodeSnapshot::Leaf { window, .. } => {
            keep(*window).then(|| child.node.clone())?
          }
          NodeSnapshot::Split(nested) => {
            let mut pruned = prune_split(nested, keep);
            match pruned.children.len() {
              0 => return None,
              1 => pruned.children.remove(0).node,
              _ => NodeSnapshot::Split(pruned),
            }
          }
        };
        Some(ChildSnapshot {
          weight: child.weight,
          node,
        })
      })
      .collect();
    SplitSnapshot {
      direction: split.direction,
      children,
    }
  }
  WorkspaceSnapshot {
    workspace: snapshot.workspace,
    root: prune_split(&snapshot.root, &keep),
    focused: snapshot.focused.filter(|window| keep(*window)),
    stack: snapshot
      .stack
      .iter()
      .copied()
      .filter(|window| keep(*window))
      .collect(),
  }
}

/// Builds a tree from a workspace snapshot laid out in `workarea`, with the
/// checks of a restore except those that need the session's window
/// records.
pub(crate) fn tree_from_snapshot(
  snapshot: WorkspaceSnapshot,
  workarea: Rect,
) -> Result<TileTree> {
  let mut seen = BTreeSet::new();
  build_tree(snapshot, workarea, &mut |window, state| {
    check_leaf(window, state)?;
    if !seen.insert(window) {
      return Err(malformed(format!(
        "window {} occurs more than once",
        window.0
      )));
    }
    Ok(())
  })
}

fn split_snapshot(split: &Split) -> SplitSnapshot {
  SplitSnapshot {
    direction: split.direction,
    children: split
      .children
      .iter()
      .map(|child| ChildSnapshot {
        weight: child.weight,
        node: node_snapshot(&child.node),
      })
      .collect(),
  }
}

fn node_snapshot(node: &Node) -> NodeSnapshot {
  match node {
    Node::Split(split) => NodeSnapshot::Split(split_snapshot(split)),
    Node::Leaf(window, state) => NodeSnapshot::Leaf {
      window: *window,
      state: *state,
    },
  }
}

fn malformed(message: impl Into<String>) -> ShingleError {
  ShingleError::MalformedSnapshot(message.into())
}

/// A callback for each leaf of a tree being built.
type LeafSink<'a> = dyn FnMut(WindowId, LeafState) -> Result<()> + 'a;

fn build_split(
  snapshot: SplitSnapshot,
  root: bool,
  depth: usize,
  leaf: &mut LeafSink<'_>,
) -> Result<Split> {
  if depth > MAX_TREE_DEPTH {
    return Err(malformed("tree exceeds the maximum depth"));
  }
  if !root && snapshot.children.len() < 2 {
    return Err(malformed("non-root splits must have at least two children"));
  }
  let mut children = Vec::with_capacity(snapshot.children.len());
  for child in snapshot.children {
    if child.weight == 0 {
      return Err(malformed("split weights must be nonzero"));
    }
    let node = match child.node {
      NodeSnapshot::Split(split) => {
        Node::Split(build_split(split, false, depth + 1, leaf)?)
      }
      NodeSnapshot::Leaf { window, state } => {
        leaf(window, state)?;
        Node::Leaf(window, state)
      }
    };
    children.push(Child {
      weight: child.weight,
      node,
    });
  }
  Ok(Split {
    direction: snapshot.direction,
    children,
  })
}

/// Builds one workspace's tree and checks its focus and stacking order.
fn build_tree(
  snapshot: WorkspaceSnapshot,
  workarea: Rect,
  leaf: &mut LeafSink<'_>,
) -> Result<TileTree> {
  let root = build_split(snapshot.root, true, 0, leaf)?;
  let mut leaf_ids = BTreeSet::new();
  for child in &root.children {
    collect_leaf_ids(&child.node, &mut leaf_ids);
  }
  if snapshot
    .focused
    .is_some_and(|window| !leaf_ids.contains(&window))
  {
    return Err(malformed("focused window is not in its workspace"));
  }
  if snapshot.stack.len() != leaf_ids.len()
    || snapshot
      .stack
      .iter()
      .any(|window| !leaf_ids.contains(window))
    || snapshot.stack.iter().copied().collect::<BTreeSet<_>>() != leaf_ids
  {
    return Err(malformed(
      "stack order must contain every workspace window exactly once",
    ));
  }
  Ok(TileTree {
    root,
    workarea,
    focused: snapshot.focused,
    stack: snapshot.stack,
  })
}

/// The checks of one leaf that need nothing but the leaf.
fn check_leaf(window: WindowId, state: LeafState) -> Result<()> {
  if state.tiled_edges > 15 {
    return Err(malformed("tiled-edges mask exceeds 15"));
  }
  if let Some(geometry) = state.floating {
    geometry.validate().map_err(|_| {
      malformed("floating geometry must be nonempty and representable")
    })?;
  }
  if let Some(geometry) = state.snapped {
    geometry.validate().map_err(|_| {
      malformed("snapped geometry must be nonempty and representable")
    })?;
    if state.floating.is_some() {
      return Err(malformed("window cannot be both floating and snapped"));
    }
  }
  if let Some(geometry) = state.restore_geometry {
    geometry.validate().map_err(|_| {
      malformed("restore geometry must be nonempty and representable")
    })?;
    if state.snapped.is_none() {
      return Err(malformed("restore geometry requires a snapped window"));
    }
  }
  if window.0 == 0 {
    return Err(malformed("window id must be nonzero"));
  }
  Ok(())
}

struct Restore {
  identities: BTreeMap<WindowId, (LaunchIdentity, Option<WindowId>)>,
  windows: BTreeMap<WindowId, WindowRecord>,
  leaf_count: usize,
}

impl Restore {
  fn add_leaf(
    &mut self,
    window: WindowId,
    state: LeafState,
    place: Place,
  ) -> Result<()> {
    check_leaf(window, state)?;
    if self.windows.contains_key(&window) {
      return Err(malformed(format!(
        "window {} occurs more than once",
        window.0
      )));
    }
    self.leaf_count += 1;
    if self.leaf_count > MAX_WINDOWS {
      return Err(malformed("snapshot contains too many windows"));
    }
    let (identity, parent) =
      self.identities.get(&window).cloned().ok_or_else(|| {
        malformed(format!("window {} has no identity record", window.0))
      })?;
    self.windows.insert(
      window,
      WindowRecord {
        identity,
        mapped: false,
        place,
        parent,
        away_from: None,
      },
    );
    Ok(())
  }
}

/// Checks a set's kept tree copy and returns it.
fn restore_away(
  away: Option<Vec<WorkspaceSnapshot>>,
  workarea: Rect,
) -> Result<Option<Vec<WorkspaceSnapshot>>> {
  let Some(away) = away else {
    return Ok(None);
  };
  let mut workspaces = BTreeSet::new();
  let mut seen = BTreeSet::new();
  for workspace in &away {
    if !workspaces.insert(workspace.workspace) {
      return Err(malformed("duplicate workspace in a return copy"));
    }
    build_tree(workspace.clone(), workarea, &mut |window, state| {
      check_leaf(window, state)?;
      if !seen.insert(window) {
        return Err(malformed(format!(
          "window {} occurs more than once in a return copy",
          window.0
        )));
      }
      Ok(())
    })?;
  }
  Ok(Some(away))
}

fn restore_snapshot(snapshot: SessionSnapshot) -> Result<Shingle> {
  if snapshot.version != SNAPSHOT_VERSION {
    return Err(ShingleError::UnsupportedSnapshotVersion(snapshot.version));
  }
  if snapshot
    .active_output
    .as_ref()
    .is_some_and(|output| output.0.is_empty())
  {
    return Err(malformed("active output identity cannot be empty"));
  }
  let mut identities = BTreeMap::new();
  let mut away_records = BTreeMap::new();
  for window in snapshot.windows {
    if window.id.0 == 0 {
      return Err(malformed("window id must be nonzero"));
    }
    if identities
      .insert(window.id, (window.identity, window.parent))
      .is_some()
    {
      return Err(malformed(format!(
        "window {} has duplicate identity records",
        window.id.0
      )));
    }
    if let Some(wset) = window.away_from {
      away_records.insert(window.id, wset);
    }
  }
  for (id, (_, parent)) in &identities {
    if parent.is_some_and(|parent| !identities.contains_key(&parent)) {
      return Err(malformed(format!("window {} has a missing parent", id.0)));
    }
    let mut visited = BTreeSet::new();
    let mut current = *id;
    loop {
      if !visited.insert(current) {
        return Err(malformed("window parent chain contains a cycle"));
      }
      let Some(parent) =
        identities.get(&current).and_then(|(_, parent)| *parent)
      else {
        break;
      };
      current = parent;
    }
  }
  if identities.len() > MAX_WINDOWS {
    return Err(malformed("snapshot contains too many windows"));
  }

  let mut restore = Restore {
    identities,
    windows: BTreeMap::new(),
    leaf_count: 0,
  };
  let mut wsets = BTreeMap::new();
  let mut shown = BTreeSet::new();
  for wset in snapshot.wsets {
    let id = wset.wset;
    if id.0 == 0 {
      return Err(malformed("workspace set index must be nonzero"));
    }
    if wsets.contains_key(&id) {
      return Err(malformed("duplicate workspace set index"));
    }
    if wset.home.as_ref().is_some_and(|home| home.0.is_empty()) {
      return Err(malformed("output identity cannot be empty"));
    }
    if let Some(output) = &wset.output {
      if output.0.is_empty() {
        return Err(malformed("output identity cannot be empty"));
      }
      if !shown.insert(output.clone()) {
        return Err(malformed("duplicate output identity"));
      }
      if wset.home.as_ref() != Some(output) {
        return Err(malformed("a shown workspace set's home is its output"));
      }
    }
    let workarea = wset
      .workarea
      .validate()
      .map_err(|_| malformed("output has invalid workarea"))?;
    if wset.workspaces.is_empty() {
      return Err(malformed("output must contain at least one workspace"));
    }
    let mut workspaces = BTreeMap::new();
    for workspace in wset.workspaces {
      if workspaces.contains_key(&workspace.workspace) {
        return Err(malformed("duplicate workspace identity"));
      }
      let place = Place {
        wset: id,
        workspace: workspace.workspace,
      };
      let key = workspace.workspace;
      let tree = build_tree(workspace, workarea, &mut |window, state| {
        restore.add_leaf(window, state, place)
      })?;
      workspaces.insert(key, tree);
    }
    if !workspaces.contains_key(&wset.current_workspace) {
      return Err(malformed("current workspace does not exist"));
    }
    wsets.insert(
      id,
      WsetState {
        workarea,
        current_workspace: wset.current_workspace,
        workspaces,
        output: wset.output,
        home: wset.home,
        identity: wset.identity,
        away: restore_away(wset.away, workarea)?,
      },
    );
  }
  if restore.windows.len() != restore.identities.len() {
    return Err(malformed(
      "every identity record must have exactly one corresponding tree leaf",
    ));
  }
  for (window, wset) in away_records {
    let record = restore
      .windows
      .get_mut(&window)
      .ok_or_else(|| malformed("away record of an unknown window"))?;
    if !wsets.contains_key(&wset) {
      return Err(malformed(format!(
        "window {} is away from a missing workspace set",
        window.0
      )));
    }
    if record.place.wset == wset {
      return Err(malformed(format!(
        "window {} is away from the set holding it",
        window.0
      )));
    }
    record.away_from = Some(wset);
  }
  let next_window_id = match restore.windows.keys().next_back() {
    Some(id) => id.0.checked_add(1).unwrap_or(0),
    None => 1,
  };
  Ok(Shingle {
    wsets,
    windows: restore.windows,
    next_window_id,
    active_output: snapshot.active_output,
  })
}

fn collect_leaf_ids(node: &Node, windows: &mut BTreeSet<WindowId>) {
  match node {
    Node::Leaf(window, _) => {
      windows.insert(*window);
    }
    Node::Split(split) => {
      for child in &split.children {
        collect_leaf_ids(&child.node, windows);
      }
    }
  }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
  let file_name = path
    .file_name()
    .ok_or(ShingleError::InvalidPersistencePath)?;
  if file_name.is_empty() {
    return Err(ShingleError::InvalidPersistencePath);
  }
  let parent = path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty());
  let parent = parent.unwrap_or_else(|| Path::new("."));
  let mut last_collision = None;
  let (temporary_path, mut file) = (0..128)
    .find_map(|_| {
      let serial = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
      let mut temporary_name = OsString::from(".");
      temporary_name.push(file_name);
      temporary_name.push(format!(".tmp.{}.{}", std::process::id(), serial));
      let temporary_path = parent.join(temporary_name);
      match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
      {
        Ok(file) => Some(Ok((temporary_path, file))),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
          last_collision = Some(error);
          None
        }
        Err(error) => Some(Err(error)),
      }
    })
    .transpose()?
    .ok_or_else(|| {
      ShingleError::Io(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
          std::io::ErrorKind::AlreadyExists,
          "could not allocate a temporary snapshot file",
        )
      }))
    })?;

  let result = (|| -> Result<()> {
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary_path, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
  })();
  if result.is_err() {
    let _ = std::fs::remove_file(&temporary_path);
  }
  result
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use super::*;
  use crate::{Direction, Gaps, StackingMode, WindowPresence};

  const SECOND: WorkspaceId = WorkspaceId::new(1, 0);

  fn primary() -> OutputId {
    OutputId::from("DP-1")
  }

  fn secondary() -> OutputId {
    OutputId::from("HDMI-A-1")
  }

  /// Builds a session exercising every persisted property: nested splits,
  /// uneven weights, hidden windows, per-window flags, focus and stacking
  /// on two outputs and several workspaces.
  fn sample() -> Shingle {
    let mut shingle = Shingle::new();
    shingle
      .add_output(primary(), Rect::new(0, 0, 1920, 1080))
      .unwrap();
    shingle
      .add_output(secondary(), Rect::new(1920, 0, 1280, 1024))
      .unwrap();
    shingle.ensure_workspace(&primary(), SECOND).unwrap();
    let attach = |shingle: &mut Shingle, app_id: &str| {
      shingle
        .attach_window(&primary(), LaunchIdentity::new(app_id))
        .unwrap()
    };
    let a = attach(&mut shingle, "a");
    let b = attach(&mut shingle, "b");
    let c = attach(&mut shingle, "c");
    let d = attach(&mut shingle, "d");
    let e = attach(&mut shingle, "e");
    let f = attach(&mut shingle, "f");
    shingle.drop_window(c, b, Direction::Down).unwrap();
    shingle.drop_window(d, c, Direction::Right).unwrap();
    shingle.set_minimized(f, true).unwrap();
    shingle.resize_window(a, Direction::Right, 137, 40).unwrap();
    shingle.resize_window(b, Direction::Down, 91, 40).unwrap();
    shingle.resize_window(c, Direction::Right, -33, 40).unwrap();
    shingle.resize_window(e, Direction::Left, 211, 40).unwrap();
    shingle
      .set_floating(e, Some(Rect::new(300, 200, 640, 480)))
      .unwrap();
    shingle.set_tiled_edges(a, 5).unwrap();
    shingle.set_gaps(b, Gaps::new(12, 6)).unwrap();
    shingle.set_sticky(d, true).unwrap();
    shingle.set_stacking(c, StackingMode::AlwaysOnTop).unwrap();
    shingle.raise_window(a).unwrap();
    shingle.focus_window(b).unwrap();

    shingle.set_current_workspace(&primary(), SECOND).unwrap();
    let g = attach(&mut shingle, "g");
    let h = attach(&mut shingle, "h");
    shingle.set_maximized(h, true).unwrap();
    shingle.set_fullscreen(h, true).unwrap();
    shingle
      .set_snapped(g, Some(Rect::new(0, 0, 960, 1080)))
      .unwrap();
    shingle.lower_window(h).unwrap();

    let identity = LaunchIdentity::new("j")
      .with_launch_id("launch-7")
      .with_launch_group("group")
      .with_command(["j", "--flag"])
      .with_working_directory("/home/user")
      .with_title("Title");
    let j = shingle.attach_window(&secondary(), identity).unwrap();
    shingle.set_window_parent(j, Some(a)).unwrap();
    shingle
      .set_stacking(j, StackingMode::AlwaysOnBottom)
      .unwrap();
    shingle
  }

  fn trees(shingle: &Shingle) -> Vec<(OutputId, WorkspaceId, &TileTree)> {
    let mut trees = Vec::new();
    for output in shingle.outputs() {
      for workspace in shingle.workspaces(output).unwrap() {
        trees.push((
          output.clone(),
          workspace,
          shingle.tree(output, workspace).unwrap(),
        ));
      }
    }
    trees
  }

  fn assert_same_layout(original: &Shingle, restored: &Shingle) {
    let original_trees = trees(original);
    let restored_trees = trees(restored);
    assert_eq!(original_trees.len(), restored_trees.len());
    for (before, after) in original_trees.iter().zip(&restored_trees) {
      assert_eq!((&before.0, before.1), (&after.0, after.1));
      assert_eq!(before.2.windows(), after.2.windows());
      assert_eq!(before.2.layout(), after.2.layout());
      assert_eq!(before.2.focused(), after.2.focused());
      assert_eq!(before.2.stack_order(), after.2.stack_order());
      assert_eq!(before.2.direction(), after.2.direction());
      for window in before.2.windows() {
        assert_eq!(
          before.2.leaf_state(window).unwrap(),
          after.2.leaf_state(window).unwrap()
        );
      }
    }
    for output in original.outputs() {
      assert_eq!(
        original.current_workspace(output).unwrap(),
        restored.current_workspace(output).unwrap()
      );
      assert_eq!(
        original.output_workarea(output).unwrap(),
        restored.output_workarea(output).unwrap()
      );
    }
  }

  fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
      "shingle-core-{name}-{}-{}",
      std::process::id(),
      TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
  }

  /// Two tiled windows on one output, as a snapshot to corrupt.
  fn simple_snapshot() -> SessionSnapshot {
    let mut shingle = Shingle::new();
    shingle
      .add_output(primary(), Rect::new(0, 0, 1000, 800))
      .unwrap();
    for app_id in ["a", "b"] {
      shingle
        .attach_window(&primary(), LaunchIdentity::new(app_id))
        .unwrap();
    }
    shingle.snapshot()
  }

  fn root(snapshot: &mut SessionSnapshot) -> &mut SplitSnapshot {
    &mut snapshot.wsets[0].workspaces[0].root
  }

  fn leaf_state(snapshot: &mut SessionSnapshot) -> &mut LeafState {
    match &mut root(snapshot).children[0].node {
      NodeSnapshot::Leaf { state, .. } => state,
      NodeSnapshot::Split(_) => unreachable!(),
    }
  }

  fn assert_malformed(
    mutate: impl FnOnce(&mut SessionSnapshot),
    expected: &str,
  ) {
    let mut snapshot = simple_snapshot();
    mutate(&mut snapshot);
    match Shingle::from_snapshot(snapshot.clone()) {
      Err(ShingleError::MalformedSnapshot(message)) => {
        assert!(
          message.contains(expected),
          "{message:?} does not mention {expected:?}"
        );
      }
      other => panic!("expected malformed snapshot, got {other:?}"),
    }
    assert!(snapshot.validate().is_err());
    assert!(snapshot.to_json().is_err());
  }

  #[test]
  fn round_trip_preserves_tile_order_sizes_and_state() {
    let original = sample();
    let json = original.to_json().unwrap();
    let restored = Shingle::from_json(&json).unwrap();
    assert_eq!(restored.snapshot(), original.snapshot());
    assert_same_layout(&original, &restored);
    assert_eq!(restored.to_json().unwrap(), json);
  }

  #[test]
  fn round_trip_preserves_identity_and_parent() {
    let original = sample();
    let restored = Shingle::from_json(&original.to_json().unwrap()).unwrap();
    for window in original.window_ids() {
      let before = original.window_info(window).unwrap();
      let after = restored.window_info(window).unwrap();
      assert_eq!(before.identity, after.identity);
      assert_eq!(before.parent, after.parent);
      assert_eq!(before.location, after.location);
      assert_eq!(before.state, after.state);
    }
    assert_eq!(
      restored.window_ids().collect::<Vec<_>>(),
      original.window_ids().collect::<Vec<_>>()
    );
  }

  #[test]
  fn sample_layout_is_uneven_and_nested() {
    // Guards the round-trip tests against a sample that silently loses
    // the structure they are meant to exercise.
    let shingle = sample();
    let tree = shingle.tree(&primary(), WorkspaceId::ORIGIN).unwrap();
    let widths: Vec<u32> = tree
      .layout()
      .iter()
      .map(|layout| layout.node.width)
      .collect();
    assert_eq!(tree.layout().len(), 4);
    assert!(widths.windows(2).any(|pair| pair[0] != pair[1]));
    assert!(
      tree
        .root
        .children
        .iter()
        .any(|child| matches!(child.node, Node::Split(_)))
    );
  }

  #[test]
  fn restored_windows_are_placeholders_until_reconciled() {
    let original = sample();
    let mut restored =
      Shingle::from_json(&original.to_json().unwrap()).unwrap();
    let all: Vec<WindowId> = original.window_ids().collect();
    assert_eq!(restored.placeholders(), all);
    for window in &all {
      let identity = restored.window_info(*window).unwrap().identity;
      assert_eq!(restored.reconcile_mapped(&identity), Some(*window));
      assert_eq!(
        restored.window_info(*window).unwrap().presence,
        WindowPresence::Mapped
      );
    }
    assert!(restored.placeholders().is_empty());
    assert_same_layout(&original, &restored);
    assert_eq!(restored.snapshot(), original.snapshot());
  }

  #[test]
  fn restored_session_continues_window_ids() {
    let original = sample();
    let mut restored =
      Shingle::from_json(&original.to_json().unwrap()).unwrap();
    let highest = original.window_ids().max().unwrap();
    let next = restored
      .attach_window(&primary(), LaunchIdentity::new("new"))
      .unwrap();
    assert_eq!(next.get(), highest.get() + 1);
    let empty = Shingle::from_snapshot(Shingle::new().snapshot()).unwrap();
    assert_eq!(empty.next_window_id, 1);
  }

  #[test]
  fn repeated_cycles_do_not_drift() {
    let original = sample();
    let mut current = original.clone();
    for _ in 0..5 {
      current = Shingle::from_json(&current.to_json().unwrap()).unwrap();
    }
    assert_same_layout(&original, &current);
  }

  #[test]
  fn save_and_load_round_trip_through_file() {
    let directory = temp_dir("save");
    let path = directory.join("session.json");
    let original = sample();
    original.save_atomic(&path).unwrap();
    Shingle::new().save_atomic(&path).unwrap();
    original.save_atomic(&path).unwrap();
    let restored = Shingle::load(&path).unwrap();
    assert_same_layout(&original, &restored);
    let entries: Vec<_> = std::fs::read_dir(&directory)
      .unwrap()
      .map(|entry| entry.unwrap().file_name())
      .collect();
    assert_eq!(entries, vec![OsString::from("session.json")]);
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.ends_with("}\n"));
    std::fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn save_rejects_paths_without_file_name() {
    let shingle = sample();
    for path in ["", "/", "relative/.."] {
      assert!(
        matches!(
          shingle.save_atomic(path),
          Err(ShingleError::InvalidPersistencePath)
        ),
        "{path:?}"
      );
    }
  }

  #[test]
  fn save_into_missing_directory_fails_without_leftovers() {
    let directory = temp_dir("missing");
    let path = directory.join("absent").join("session.json");
    assert!(matches!(
      sample().save_atomic(&path),
      Err(ShingleError::Io(_))
    ));
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    std::fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn load_reports_missing_file_and_bad_json() {
    let directory = temp_dir("load");
    let path = directory.join("session.json");
    assert!(matches!(Shingle::load(&path), Err(ShingleError::Io(_))));
    std::fs::write(&path, "{").unwrap();
    assert!(matches!(Shingle::load(&path), Err(ShingleError::Json(_))));
    let mut snapshot = simple_snapshot();
    snapshot.version = 3;
    std::fs::write(&path, serialize_snapshot(&snapshot).unwrap()).unwrap();
    assert!(matches!(
      Shingle::load(&path),
      Err(ShingleError::UnsupportedSnapshotVersion(3))
    ));
    std::fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn unsupported_versions_are_rejected() {
    for version in [0, SNAPSHOT_VERSION + 1] {
      let mut snapshot = simple_snapshot();
      snapshot.version = version;
      let json = serde_json::to_string(&snapshot).unwrap();
      assert!(matches!(
        Shingle::from_json(&json),
        Err(ShingleError::UnsupportedSnapshotVersion(v)) if v == version
      ));
      assert!(matches!(
        SessionSnapshot::from_json(&json),
        Err(ShingleError::UnsupportedSnapshotVersion(_))
      ));
    }
  }

  #[test]
  fn unknown_fields_are_rejected() {
    let json = simple_snapshot().to_json().unwrap();
    let json = json.replacen("\"version\"", "\"extra\": 1, \"version\"", 1);
    assert!(matches!(
      Shingle::from_json(&json),
      Err(ShingleError::Json(_))
    ));
  }

  #[test]
  fn optional_leaf_fields_default() {
    let mut snapshot = simple_snapshot();
    let mut value = serde_json::to_value(&snapshot).unwrap();
    let state = &mut value["wsets"][0]["workspaces"][0]["root"]["children"][0]
      ["node"]["state"];
    *state = serde_json::json!({
      "gaps": { "top": 1, "right": 2, "bottom": 3, "left": 4, "inner": 5 },
      "maximized": false,
    });
    let restored = SessionSnapshot::from_json(&value.to_string()).unwrap();
    *leaf_state(&mut snapshot) = LeafState {
      gaps: Gaps {
        top: 1,
        right: 2,
        bottom: 3,
        left: 4,
        inner: 5,
      },
      ..LeafState::default()
    };
    assert_eq!(restored, snapshot);
  }

  #[test]
  fn held_flag_is_not_persisted() {
    let mut shingle = Shingle::new();
    shingle
      .add_output(primary(), Rect::new(0, 0, 1000, 800))
      .unwrap();
    let held = shingle
      .attach_window(&primary(), LaunchIdentity::new("a"))
      .unwrap();
    shingle.set_held(held, true).unwrap();
    let json = shingle.to_json().unwrap();
    assert!(!json.contains("held"), "{json}");
    let restored = Shingle::from_json(&json).unwrap();
    assert!(!restored.window_info(held).unwrap().state.held);
  }

  #[test]
  fn custom_tiled_edges_survive_round_trip() {
    let mut snapshot = simple_snapshot();
    leaf_state(&mut snapshot).tiled_edges = 6;
    let restored = Shingle::from_snapshot(snapshot.clone()).unwrap();
    assert_eq!(restored.snapshot(), snapshot);
  }

  #[test]
  fn malformed_trees_are_rejected() {
    assert_malformed(
      |snapshot| root(snapshot).children[0].weight = 0,
      "weights must be nonzero",
    );
    assert_malformed(
      |snapshot| {
        let child = root(snapshot).children.remove(1);
        root(snapshot).children[0].node = NodeSnapshot::Split(SplitSnapshot {
          direction: SplitDirection::Horizontal,
          children: vec![child],
        });
      },
      "at least two children",
    );
    assert_malformed(
      |snapshot| {
        let duplicate = root(snapshot).children[0].clone();
        root(snapshot).children[1] = duplicate;
      },
      "occurs more than once",
    );
    assert_malformed(
      |snapshot| {
        if let NodeSnapshot::Leaf { window, .. } =
          &mut root(snapshot).children[1].node
        {
          *window = WindowId(0);
        }
      },
      "nonzero",
    );
  }

  #[test]
  fn deep_trees_are_rejected() {
    let mut snapshot = simple_snapshot();
    let mut split = SplitSnapshot {
      direction: SplitDirection::Vertical,
      children: vec![
        ChildSnapshot {
          weight: 1,
          node: NodeSnapshot::Leaf {
            window: WindowId(1),
            state: LeafState::default(),
          },
        },
        ChildSnapshot {
          weight: 1,
          node: NodeSnapshot::Leaf {
            window: WindowId(2),
            state: LeafState::default(),
          },
        },
      ],
    };
    // The innermost split ends up one level below the deepest allowed.
    for level in 0..=MAX_TREE_DEPTH as u64 {
      let window = WindowId(level + 3);
      snapshot.windows.push(WindowSnapshot {
        id: window,
        identity: LaunchIdentity::new("deep"),
        parent: None,
        away_from: None,
      });
      split = SplitSnapshot {
        direction: SplitDirection::Vertical,
        children: vec![
          ChildSnapshot {
            weight: 1,
            node: NodeSnapshot::Leaf {
              window,
              state: LeafState::default(),
            },
          },
          ChildSnapshot {
            weight: 1,
            node: NodeSnapshot::Split(split),
          },
        ],
      };
    }
    let workspace = &mut snapshot.wsets[0].workspaces[0];
    workspace.root = split.clone();
    workspace.stack = snapshot.windows.iter().map(|window| window.id).collect();
    let error = Shingle::from_snapshot(snapshot.clone()).unwrap_err();
    assert!(
      matches!(&error, ShingleError::MalformedSnapshot(message)
        if message.contains("maximum depth")),
      "{error:?}"
    );

    // One level less is accepted.
    let NodeSnapshot::Split(inner) = split.children.remove(1).node else {
      unreachable!();
    };
    let removed = split.children.remove(0);
    let NodeSnapshot::Leaf { window, .. } = removed.node else {
      unreachable!();
    };
    snapshot.windows.retain(|record| record.id != window);
    let workspace = &mut snapshot.wsets[0].workspaces[0];
    workspace.root = inner;
    workspace.stack.retain(|id| *id != window);
    Shingle::from_snapshot(snapshot).unwrap();
  }

  #[test]
  fn malformed_leaf_state_is_rejected() {
    assert_malformed(
      |snapshot| leaf_state(snapshot).tiled_edges = 16,
      "tiled-edges",
    );
    assert_malformed(
      |snapshot| leaf_state(snapshot).floating = Some(Rect::new(0, 0, 0, 1)),
      "floating geometry",
    );
    assert_malformed(
      |snapshot| leaf_state(snapshot).snapped = Some(Rect::new(0, 0, 1, 0)),
      "snapped geometry",
    );
    assert_malformed(
      |snapshot| {
        let state = leaf_state(snapshot);
        state.floating = Some(Rect::new(0, 0, 10, 10));
        state.snapped = Some(Rect::new(0, 0, 10, 10));
      },
      "both floating and snapped",
    );
    assert_malformed(
      |snapshot| {
        leaf_state(snapshot).restore_geometry = Some(Rect::new(0, 0, 10, 10))
      },
      "requires a snapped window",
    );
    assert_malformed(
      |snapshot| {
        let state = leaf_state(snapshot);
        state.snapped = Some(Rect::new(0, 0, 10, 10));
        state.restore_geometry = Some(Rect::new(0, 0, 0, 10));
      },
      "restore geometry",
    );
  }

  #[test]
  fn identity_records_must_match_leaves() {
    assert_malformed(
      |snapshot| {
        snapshot.windows.remove(1);
      },
      "has no identity record",
    );
    assert_malformed(
      |snapshot| {
        snapshot.windows.push(WindowSnapshot {
          id: WindowId(7),
          identity: LaunchIdentity::new("orphan"),
          parent: None,
          away_from: None,
        });
      },
      "exactly one corresponding tree leaf",
    );
    assert_malformed(
      |snapshot| {
        let duplicate = snapshot.windows[0].clone();
        snapshot.windows.push(duplicate);
      },
      "duplicate identity records",
    );
    assert_malformed(
      |snapshot| snapshot.windows[0].id = WindowId(0),
      "nonzero",
    );
    assert_malformed(
      |snapshot| snapshot.windows[0].parent = Some(WindowId(9)),
      "missing parent",
    );
    assert_malformed(
      |snapshot| {
        snapshot.windows[0].parent = Some(WindowId(2));
        snapshot.windows[1].parent = Some(WindowId(1));
      },
      "cycle",
    );
  }

  #[test]
  fn focus_and_stack_must_match_workspace() {
    assert_malformed(
      |snapshot| snapshot.wsets[0].workspaces[0].focused = Some(WindowId(9)),
      "focused window",
    );
    assert_malformed(
      |snapshot| {
        snapshot.wsets[0].workspaces[0].stack.pop();
      },
      "stack order",
    );
    assert_malformed(
      |snapshot| {
        let stack = &mut snapshot.wsets[0].workspaces[0].stack;
        stack[1] = stack[0];
      },
      "stack order",
    );
    assert_malformed(
      |snapshot| {
        snapshot.wsets[0].workspaces[0].stack.push(WindowId(1));
      },
      "stack order",
    );
  }

  #[test]
  fn outputs_and_workspaces_must_be_consistent() {
    assert_malformed(
      |snapshot| snapshot.wsets[0].home = Some(OutputId::new("")),
      "output identity",
    );
    assert_malformed(
      |snapshot| snapshot.wsets[0].workarea = Rect::new(0, 0, 0, 0),
      "invalid workarea",
    );
    assert_malformed(
      |snapshot| snapshot.wsets[0].workspaces.clear(),
      "at least one workspace",
    );
    assert_malformed(
      |snapshot| snapshot.wsets[0].current_workspace = WorkspaceId::new(3, 3),
      "current workspace",
    );
    assert_malformed(
      |snapshot| {
        let mut empty = snapshot.wsets[0].workspaces[0].clone();
        empty.root.children.clear();
        empty.focused = None;
        empty.stack.clear();
        snapshot.wsets[0].workspaces.push(empty);
      },
      "duplicate workspace",
    );
    assert_malformed(
      |snapshot| {
        let mut copy = snapshot.wsets[0].clone();
        copy.wset = WsetId(2);
        for workspace in &mut copy.workspaces {
          workspace.root.children.clear();
          workspace.focused = None;
          workspace.stack.clear();
        }
        snapshot.wsets.push(copy);
      },
      "duplicate output",
    );
  }

  #[test]
  fn window_in_two_workspaces_is_rejected() {
    assert_malformed(
      |snapshot| {
        let output = &mut snapshot.wsets[0];
        let mut copy = output.workspaces[0].clone();
        copy.workspace = SECOND;
        output.workspaces.push(copy);
      },
      "occurs more than once",
    );
  }

  /// A version 1 file as `master` wrote it: two outputs, a nested split
  /// and a second workspace.
  const VERSION_1: &str = r#"{
  "version": 1,
  "outputs": [
    {
      "output": "DP-1",
      "workarea": { "x": 0, "y": 0, "width": 1000, "height": 800 },
      "current_workspace": { "x": 1, "y": 0 },
      "workspaces": [
        {
          "workspace": { "x": 0, "y": 0 },
          "root": {
            "direction": "vertical",
            "children": [
              { "weight": 3, "node": { "kind": "leaf", "window": 1,
                "state": { "gaps": { "top": 0, "right": 0, "bottom": 0,
                  "left": 0, "inner": 0 }, "maximized": false,
                "tiled_edges": 15 } } },
              { "weight": 1, "node": { "kind": "split",
                "direction": "horizontal", "children": [
                  { "weight": 1, "node": { "kind": "leaf", "window": 2,
                    "state": { "gaps": { "top": 0, "right": 0,
                    "bottom": 0, "left": 0, "inner": 0 },
                    "maximized": false } } },
                  { "weight": 1, "node": { "kind": "leaf", "window": 3,
                    "state": { "gaps": { "top": 0, "right": 0,
                    "bottom": 0, "left": 0, "inner": 0 },
                    "maximized": false } } }
                ] } }
            ]
          },
          "focused": 2,
          "stack": [1, 3, 2]
        },
        {
          "workspace": { "x": 1, "y": 0 },
          "root": { "direction": "vertical", "children": [] },
          "focused": null,
          "stack": []
        }
      ]
    },
    {
      "output": "HDMI-A-1",
      "workarea": { "x": 1000, "y": 0, "width": 800, "height": 600 },
      "current_workspace": { "x": 0, "y": 0 },
      "workspaces": [
        {
          "workspace": { "x": 0, "y": 0 },
          "root": { "direction": "vertical", "children": [
            { "weight": 1, "node": { "kind": "leaf", "window": 4,
              "state": { "gaps": { "top": 0, "right": 0,
                    "bottom": 0, "left": 0, "inner": 0 },
                    "maximized": false } } }
          ] },
          "focused": 4,
          "stack": [4]
        }
      ]
    }
  ],
  "windows": [
    { "id": 1, "identity": { "app_id": "a", "launch_id": null } },
    { "id": 2, "identity": { "app_id": "b", "launch_id": null } },
    { "id": 3, "identity": { "app_id": "c", "launch_id": null },
      "parent": 2 },
    { "id": 4, "identity": { "app_id": "d", "launch_id": null } }
  ]
}"#;

  #[test]
  fn version_1_files_load_as_one_set_per_output() {
    let snapshot = SessionSnapshot::from_json(VERSION_1).unwrap();
    assert_eq!(snapshot.version, SNAPSHOT_VERSION);
    let restored = Shingle::from_json(VERSION_1).unwrap();
    let wsets = restored.wsets().collect::<Vec<_>>();
    assert_eq!(wsets.len(), 2);
    for (info, (index, output)) in
      wsets.iter().zip([(1, "DP-1"), (2, "HDMI-A-1")])
    {
      assert_eq!(info.id, WsetId(index));
      assert_eq!(info.output, Some(OutputId::from(output)));
      assert_eq!(info.home, Some(OutputId::from(output)));
      assert_eq!(info.identity, None);
      assert!(!info.keeps_return_copy);
    }
    assert_eq!(
      restored.outputs().collect::<Vec<_>>(),
      [&primary(), &secondary()]
    );
    assert_eq!(restored.current_workspace(&primary()).unwrap(), SECOND);
    let tree = restored.tree(&primary(), WorkspaceId::ORIGIN).unwrap();
    assert_eq!(tree.windows(), [WindowId(1), WindowId(2), WindowId(3)]);
    assert_eq!(tree.focused(), Some(WindowId(2)));
    assert_eq!(tree.stack_order(), [WindowId(1), WindowId(3), WindowId(2)]);
    let widths = tree
      .layout()
      .iter()
      .map(|layout| layout.node.width)
      .collect::<Vec<_>>();
    assert_eq!(widths, [750, 250, 250]);
    assert_eq!(
      restored.window_info(WindowId(3)).unwrap().parent,
      Some(WindowId(2))
    );
    assert_eq!(
      restored.window_location(WindowId(4)).unwrap().output,
      secondary()
    );
    assert_eq!(restored.placeholders().len(), 4);
    assert_eq!(restored.active_output(), None);
    // Saving writes the current version.
    let json = restored.to_json().unwrap();
    assert!(json.contains("\"version\": 2"), "{json}");
    assert!(json.contains("\"wsets\""), "{json}");
    assert!(!json.contains("\"outputs\""), "{json}");
    assert_eq!(Shingle::from_json(&json).unwrap().snapshot(), snapshot);
    // A version 1 file keeps version 1's rules.
    let broken = VERSION_1.replace("\"weight\": 3", "\"weight\": 0");
    assert!(matches!(
      Shingle::from_json(&broken),
      Err(ShingleError::MalformedSnapshot(_))
    ));
    let unknown = VERSION_1.replace("\"outputs\"", "\"wsets\"");
    assert!(matches!(
      Shingle::from_json(&unknown),
      Err(ShingleError::Json(_))
    ));
  }

  /// The sample with its second output unplugged under `return`: the set
  /// is parked with a copy of its trees and its window is away from it.
  fn unplugged_sample() -> (Shingle, WsetId, WsetId) {
    let mut shingle = sample();
    let primary_set = shingle.wset_of(&primary()).unwrap();
    let secondary_set = shingle.wset_of(&secondary()).unwrap();
    shingle
      .attach_wset(
        secondary_set,
        Some(secondary()),
        Some(OutputIdentity::new("Dell", "U2720Q", Some("A1".into()))),
      )
      .unwrap();
    shingle.attach_wset(secondary_set, None, None).unwrap();
    shingle
      .transfer_wset(secondary_set, primary_set, true)
      .unwrap();
    shingle.set_active_output(Some(primary()));
    (shingle, primary_set, secondary_set)
  }

  #[test]
  fn round_trip_preserves_parked_sets_and_away_records() {
    let (original, primary_set, secondary_set) = unplugged_sample();
    let json = original.to_json().unwrap();
    let mut restored = Shingle::from_json(&json).unwrap();
    assert_eq!(restored.snapshot(), original.snapshot());
    assert_eq!(restored.to_json().unwrap(), json);
    assert_eq!(
      restored.wsets().collect::<Vec<_>>(),
      original.wsets().collect::<Vec<_>>()
    );
    assert_eq!(restored.active_output(), Some(&primary()));
    let j = WindowId(9);
    let info = restored.window_info(j).unwrap();
    assert_eq!(
      (info.wset, info.away_from),
      (primary_set, Some(secondary_set))
    );
    // The monitor returns after the restart and takes its window back.
    let identity = OutputIdentity::new("Dell", "U2720Q", Some("A1".into()));
    assert_eq!(
      restored.find_parked_wset(&"DP-2".into(), Some(&identity)),
      Some(secondary_set)
    );
    restored
      .attach_wset(secondary_set, Some(secondary()), Some(identity))
      .unwrap();
    assert_eq!(restored.return_windows(secondary_set).unwrap(), [j]);
    let tree = restored.tree(&secondary(), WorkspaceId::ORIGIN).unwrap();
    assert_eq!(tree.windows(), [j]);
    assert_eq!(
      tree.leaf_state(j).unwrap().stacking,
      StackingMode::AlwaysOnBottom
    );
  }

  #[test]
  fn workspace_set_invariants_are_checked() {
    assert_malformed(|snapshot| snapshot.wsets[0].wset = WsetId(0), "nonzero");
    assert_malformed(
      |snapshot| {
        let mut copy = snapshot.wsets[0].clone();
        copy.output = None;
        for workspace in &mut copy.workspaces {
          workspace.root.children.clear();
          workspace.focused = None;
          workspace.stack.clear();
        }
        snapshot.wsets.push(copy);
      },
      "duplicate workspace set index",
    );
    assert_malformed(
      |snapshot| snapshot.wsets[0].home = Some(secondary()),
      "home is its output",
    );
    assert_malformed(
      |snapshot| snapshot.windows[0].away_from = Some(WsetId(5)),
      "missing workspace set",
    );
    assert_malformed(
      |snapshot| snapshot.windows[0].away_from = Some(WsetId(1)),
      "set holding it",
    );
    assert_malformed(
      |snapshot| snapshot.active_output = Some(OutputId::new("")),
      "active output",
    );
    assert_malformed(
      |snapshot| {
        let copy = snapshot.wsets[0].workspaces[0].clone();
        let mut other = copy.clone();
        other.workspace = SECOND;
        snapshot.wsets[0].away = Some(vec![copy, other]);
      },
      "more than once in a return copy",
    );
    assert_malformed(
      |snapshot| {
        let mut copy = snapshot.wsets[0].workspaces[0].clone();
        copy.root.children[0].weight = 0;
        snapshot.wsets[0].away = Some(vec![copy]);
      },
      "weights must be nonzero",
    );
    assert_malformed(
      |snapshot| {
        let copy = snapshot.wsets[0].workspaces[0].clone();
        snapshot.wsets[0].away = Some(vec![copy.clone(), copy]);
      },
      "duplicate workspace in a return copy",
    );
    // A return copy may name windows that are gone.
    let mut snapshot = simple_snapshot();
    let mut copy = snapshot.wsets[0].workspaces[0].clone();
    let NodeSnapshot::Leaf { window, .. } = &mut copy.root.children[0].node
    else {
      unreachable!();
    };
    *window = WindowId(40);
    copy.stack = vec![WindowId(40), WindowId(2)];
    copy.focused = Some(WindowId(40));
    snapshot.wsets[0].away = Some(vec![copy]);
    Shingle::from_snapshot(snapshot).unwrap();
  }

  #[test]
  fn parked_sets_need_no_output() {
    let mut snapshot = simple_snapshot();
    snapshot.wsets[0].output = None;
    let restored = Shingle::from_snapshot(snapshot.clone()).unwrap();
    assert_eq!(restored.outputs().count(), 0);
    assert_eq!(restored.wset(WsetId(1)).unwrap().home, Some(primary()));
    assert_eq!(restored.snapshot(), snapshot);
    snapshot.wsets[0].home = None;
    let restored = Shingle::from_snapshot(snapshot).unwrap();
    assert_eq!(
      restored.window_location(WindowId(1)).unwrap().output,
      OutputId::new("")
    );
  }
}
