use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::core::{OutputState, WindowRecord};
use crate::tree::{Child, Node, Split};
use crate::{
  LaunchIdentity, LeafState, OutputId, Rect, Result, Shingle, ShingleError,
  SplitDirection, TileTree, WindowId, WorkspaceId, WorkspaceRef,
};

/// Version written to new snapshots and the only one accepted when restoring.
pub const SNAPSHOT_VERSION: u32 = 1;
const MAX_TREE_DEPTH: usize = 64;
const MAX_WINDOWS: usize = 65_536;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The serializable form of a [`Shingle`] model.
///
/// A snapshot is valid only if it restores; [`SessionSnapshot::validate`]
/// checks the same invariants as restoring. Window presence is not stored, so
/// every window restores as a placeholder.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
  /// Must be [`SNAPSHOT_VERSION`].
  pub version: u32,
  /// Outputs with unique, nonempty ids, sorted when produced by
  /// [`Shingle::snapshot`].
  pub outputs: Vec<OutputSnapshot>,
  /// One record per window, each matching exactly one leaf in one workspace
  /// tree.
  pub windows: Vec<WindowSnapshot>,
}

/// One output's workarea, workspaces and current workspace.
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
}

impl Shingle {
  /// Captures the whole model. Presence is not captured.
  pub fn snapshot(&self) -> SessionSnapshot {
    let outputs = self
      .outputs
      .iter()
      .map(|(output, state)| OutputSnapshot {
        output: output.clone(),
        workarea: state.workarea,
        current_workspace: state.current_workspace,
        workspaces: state
          .workspaces
          .iter()
          .map(|(workspace, tree)| WorkspaceSnapshot {
            workspace: *workspace,
            root: split_snapshot(&tree.root),
            focused: tree.focused(),
            stack: tree.stack_order().to_vec(),
          })
          .collect(),
      })
      .collect();
    let windows = self
      .windows
      .iter()
      .map(|(id, record)| WindowSnapshot {
        id: *id,
        identity: record.identity.clone(),
        parent: record.parent,
      })
      .collect();
    SessionSnapshot {
      version: SNAPSHOT_VERSION,
      outputs,
      windows,
    }
  }

  /// Serializes the model as pretty-printed JSON ending in a newline.
  pub fn to_json(&self) -> Result<String> {
    self.snapshot().to_json()
  }

  /// Restores a model from JSON.
  ///
  /// Tile order, weights, state, focus, stacking and current workspaces come
  /// back exactly. Every window starts as a placeholder, and new window ids
  /// continue after the highest restored id.
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
  /// Parses a snapshot and checks that it restores.
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
}

fn serialize_snapshot(snapshot: &SessionSnapshot) -> Result<String> {
  let mut json = serde_json::to_string_pretty(snapshot)?;
  json.push('\n');
  Ok(json)
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

struct Restore {
  identities: BTreeMap<WindowId, (LaunchIdentity, Option<WindowId>)>,
  windows: BTreeMap<WindowId, WindowRecord>,
  leaf_count: usize,
}

impl Restore {
  fn build_split(
    &mut self,
    snapshot: SplitSnapshot,
    root: bool,
    depth: usize,
    location: &WorkspaceRef,
  ) -> Result<Split> {
    if depth > MAX_TREE_DEPTH {
      return Err(self.malformed("tree exceeds the maximum depth"));
    }
    if !root && snapshot.children.len() < 2 {
      return Err(
        self.malformed("non-root splits must have at least two children"),
      );
    }
    let mut children = Vec::with_capacity(snapshot.children.len());
    for child in snapshot.children {
      if child.weight == 0 {
        return Err(self.malformed("split weights must be nonzero"));
      }
      let node = match child.node {
        NodeSnapshot::Split(split) => {
          Node::Split(self.build_split(split, false, depth + 1, location)?)
        }
        NodeSnapshot::Leaf { window, state } => {
          self.add_leaf(window, state, location)?;
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

  fn add_leaf(
    &mut self,
    window: WindowId,
    state: LeafState,
    location: &WorkspaceRef,
  ) -> Result<()> {
    if state.tiled_edges > 15 {
      return Err(self.malformed("tiled-edges mask exceeds 15"));
    }
    if let Some(geometry) = state.floating {
      geometry.validate().map_err(|_| {
        self.malformed("floating geometry must be nonempty and representable")
      })?;
    }
    if let Some(geometry) = state.snapped {
      geometry.validate().map_err(|_| {
        self.malformed("snapped geometry must be nonempty and representable")
      })?;
      if state.floating.is_some() {
        return Err(
          self.malformed("window cannot be both floating and snapped"),
        );
      }
    }
    if let Some(geometry) = state.restore_geometry {
      geometry.validate().map_err(|_| {
        self.malformed("restore geometry must be nonempty and representable")
      })?;
      if state.snapped.is_none() {
        return Err(
          self.malformed("restore geometry requires a snapped window"),
        );
      }
    }
    if window.0 == 0 {
      return Err(self.malformed("window id must be nonzero"));
    }
    if self.windows.contains_key(&window) {
      return Err(
        self.malformed(format!("window {} occurs more than once", window.0)),
      );
    }
    self.leaf_count += 1;
    if self.leaf_count > MAX_WINDOWS {
      return Err(self.malformed("snapshot contains too many windows"));
    }
    let (identity, parent) =
      self.identities.get(&window).cloned().ok_or_else(|| {
        self.malformed(format!("window {} has no identity record", window.0))
      })?;
    self.windows.insert(
      window,
      WindowRecord {
        identity,
        mapped: false,
        location: location.clone(),
        parent,
      },
    );
    Ok(())
  }

  fn malformed(&self, message: impl Into<String>) -> ShingleError {
    ShingleError::MalformedSnapshot(message.into())
  }
}

fn restore_snapshot(snapshot: SessionSnapshot) -> Result<Shingle> {
  if snapshot.version != SNAPSHOT_VERSION {
    return Err(ShingleError::UnsupportedSnapshotVersion(snapshot.version));
  }
  let mut identities = BTreeMap::new();
  for window in snapshot.windows {
    if window.id.0 == 0 {
      return Err(ShingleError::MalformedSnapshot(
        "window id must be nonzero".into(),
      ));
    }
    if identities
      .insert(window.id, (window.identity, window.parent))
      .is_some()
    {
      return Err(ShingleError::MalformedSnapshot(format!(
        "window {} has duplicate identity records",
        window.id.0
      )));
    }
  }
  for (id, (_, parent)) in &identities {
    if parent.is_some_and(|parent| !identities.contains_key(&parent)) {
      return Err(ShingleError::MalformedSnapshot(format!(
        "window {} has a missing parent",
        id.0
      )));
    }
    let mut visited = BTreeSet::new();
    let mut current = *id;
    loop {
      if !visited.insert(current) {
        return Err(ShingleError::MalformedSnapshot(
          "window parent chain contains a cycle".into(),
        ));
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
    return Err(ShingleError::MalformedSnapshot(
      "snapshot contains too many windows".into(),
    ));
  }

  let mut restore = Restore {
    identities,
    windows: BTreeMap::new(),
    leaf_count: 0,
  };
  let mut outputs = BTreeMap::new();
  for output in snapshot.outputs {
    if output.output.0.is_empty() {
      return Err(restore.malformed("output identity cannot be empty"));
    }
    let workarea = output
      .workarea
      .validate()
      .map_err(|_| restore.malformed("output has invalid workarea"))?;
    if output.workspaces.is_empty() {
      return Err(
        restore.malformed("output must contain at least one workspace"),
      );
    }
    let output_id = output.output;
    if outputs.contains_key(&output_id) {
      return Err(restore.malformed("duplicate output identity"));
    }
    let mut workspaces = BTreeMap::new();
    let current_workspace = output.current_workspace;
    for workspace in output.workspaces {
      if workspaces.contains_key(&workspace.workspace) {
        return Err(restore.malformed("duplicate workspace identity"));
      }
      let location = WorkspaceRef {
        output: output_id.clone(),
        workspace: workspace.workspace,
      };
      let root = restore.build_split(workspace.root, true, 0, &location)?;
      let mut leaf_ids = BTreeSet::new();
      for child in &root.children {
        collect_leaf_ids(&child.node, &mut leaf_ids);
      }
      if workspace
        .focused
        .is_some_and(|window| !leaf_ids.contains(&window))
      {
        return Err(
          restore.malformed("focused window is not in its workspace"),
        );
      }
      if workspace.stack.len() != leaf_ids.len()
        || workspace
          .stack
          .iter()
          .any(|window| !leaf_ids.contains(window))
        || workspace.stack.iter().copied().collect::<BTreeSet<_>>() != leaf_ids
      {
        return Err(restore.malformed(
          "stack order must contain every workspace window exactly once",
        ));
      }
      workspaces.insert(
        workspace.workspace,
        TileTree {
          root,
          workarea,
          focused: workspace.focused,
          stack: workspace.stack,
        },
      );
    }
    if !workspaces.contains_key(&current_workspace) {
      return Err(restore.malformed("current workspace does not exist"));
    }
    outputs.insert(
      output_id,
      OutputState {
        workarea,
        current_workspace,
        workspaces,
      },
    );
  }
  if restore.windows.len() != restore.identities.len() {
    return Err(restore.malformed(
      "every identity record must have exactly one corresponding tree leaf",
    ));
  }
  let next_window_id = match restore.windows.keys().next_back() {
    Some(id) => id.0.checked_add(1).unwrap_or(0),
    None => 1,
  };
  Ok(Shingle {
    outputs,
    windows: restore.windows,
    next_window_id,
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
    &mut snapshot.outputs[0].workspaces[0].root
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
    snapshot.version = 2;
    std::fs::write(&path, serialize_snapshot(&snapshot).unwrap()).unwrap();
    assert!(matches!(
      Shingle::load(&path),
      Err(ShingleError::UnsupportedSnapshotVersion(2))
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
    let state = &mut value["outputs"][0]["workspaces"][0]["root"]["children"]
      [0]["node"]["state"];
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
    let workspace = &mut snapshot.outputs[0].workspaces[0];
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
    let workspace = &mut snapshot.outputs[0].workspaces[0];
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
      |snapshot| snapshot.outputs[0].workspaces[0].focused = Some(WindowId(9)),
      "focused window",
    );
    assert_malformed(
      |snapshot| {
        snapshot.outputs[0].workspaces[0].stack.pop();
      },
      "stack order",
    );
    assert_malformed(
      |snapshot| {
        let stack = &mut snapshot.outputs[0].workspaces[0].stack;
        stack[1] = stack[0];
      },
      "stack order",
    );
    assert_malformed(
      |snapshot| {
        snapshot.outputs[0].workspaces[0].stack.push(WindowId(1));
      },
      "stack order",
    );
  }

  #[test]
  fn outputs_and_workspaces_must_be_consistent() {
    assert_malformed(
      |snapshot| snapshot.outputs[0].output = OutputId::new(""),
      "output identity",
    );
    assert_malformed(
      |snapshot| snapshot.outputs[0].workarea = Rect::new(0, 0, 0, 0),
      "invalid workarea",
    );
    assert_malformed(
      |snapshot| snapshot.outputs[0].workspaces.clear(),
      "at least one workspace",
    );
    assert_malformed(
      |snapshot| snapshot.outputs[0].current_workspace = WorkspaceId::new(3, 3),
      "current workspace",
    );
    assert_malformed(
      |snapshot| {
        let mut empty = snapshot.outputs[0].workspaces[0].clone();
        empty.root.children.clear();
        empty.focused = None;
        empty.stack.clear();
        snapshot.outputs[0].workspaces.push(empty);
      },
      "duplicate workspace",
    );
    assert_malformed(
      |snapshot| {
        let mut copy = snapshot.outputs[0].clone();
        for workspace in &mut copy.workspaces {
          workspace.root.children.clear();
          workspace.focused = None;
          workspace.stack.clear();
        }
        snapshot.outputs.push(copy);
      },
      "duplicate output",
    );
  }

  #[test]
  fn window_in_two_workspaces_is_rejected() {
    assert_malformed(
      |snapshot| {
        let output = &mut snapshot.outputs[0];
        let mut copy = output.workspaces[0].clone();
        copy.workspace = SECOND;
        output.workspaces.push(copy);
      },
      "occurs more than once",
    );
  }
}
