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
    LaunchIdentity, LeafState, OutputId, Rect, Result, SplitDirection,
    TileTree, Tilefire, TilefireError, WindowId, WorkspaceId, WorkspaceRef,
};

pub const SNAPSHOT_VERSION: u32 = 1;
const MAX_TREE_DEPTH: usize = 64;
const MAX_WINDOWS: usize = 65_536;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub version: u32,
    pub outputs: Vec<OutputSnapshot>,
    pub windows: Vec<WindowSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSnapshot {
    pub output: OutputId,
    pub workarea: Rect,
    pub current_workspace: WorkspaceId,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub workspace: WorkspaceId,
    pub root: SplitSnapshot,
    pub focused: Option<WindowId>,
    pub stack: Vec<WindowId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitSnapshot {
    pub direction: SplitDirection,
    pub children: Vec<ChildSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildSnapshot {
    pub weight: u64,
    pub node: NodeSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeSnapshot {
    Split(SplitSnapshot),
    Leaf { window: WindowId, state: LeafState },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSnapshot {
    pub id: WindowId,
    pub identity: LaunchIdentity,
    #[serde(default)]
    pub parent: Option<WindowId>,
}

impl Tilefire {
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

    pub fn to_json(&self) -> Result<String> {
        self.snapshot().to_json()
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let snapshot = SessionSnapshot::from_json(json)?;
        Self::from_snapshot(snapshot)
    }

    pub fn from_snapshot(snapshot: SessionSnapshot) -> Result<Self> {
        restore_snapshot(snapshot)
    }

    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<()> {
        self.snapshot().save_atomic(path)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        let snapshot: SessionSnapshot = serde_json::from_slice(&bytes)?;
        Self::from_snapshot(snapshot)
    }
}

impl SessionSnapshot {
    pub fn from_json(json: &str) -> Result<Self> {
        let snapshot: Self = serde_json::from_str(json)?;
        restore_snapshot(snapshot.clone())?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<()> {
        restore_snapshot(self.clone()).map(|_| ())
    }

    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        serialize_snapshot(self)
    }

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
            return Err(self
                .malformed("non-root splits must have at least two children"));
        }
        let mut children = Vec::with_capacity(snapshot.children.len());
        for child in snapshot.children {
            if child.weight == 0 {
                return Err(self.malformed("split weights must be nonzero"));
            }
            let node =
                match child.node {
                    NodeSnapshot::Split(split) => Node::Split(
                        self.build_split(split, false, depth + 1, location)?,
                    ),
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
        mut state: LeafState,
        location: &WorkspaceRef,
    ) -> Result<()> {
        if state.tiled_edges > 15 {
            return Err(self.malformed("tiled-edges mask exceeds 15"));
        }
        if state.floating.is_some() {
            state.tiled_edges = 0;
        } else if state.snapped.is_none() {
            state.tiled_edges = 15;
        }
        if let Some(geometry) = state.floating {
            geometry.validate().map_err(|_| {
                self.malformed(
                    "floating geometry must be nonempty and representable",
                )
            })?;
        }
        if let Some(geometry) = state.snapped {
            geometry.validate().map_err(|_| {
                self.malformed(
                    "snapped geometry must be nonempty and representable",
                )
            })?;
            if state.floating.is_some() {
                return Err(self
                    .malformed("window cannot be both floating and snapped"));
            }
        }
        if let Some(geometry) = state.restore_geometry {
            geometry.validate().map_err(|_| {
                self.malformed(
                    "restore geometry must be nonempty and representable",
                )
            })?;
            if state.snapped.is_none() {
                return Err(self
                    .malformed("restore geometry requires a snapped window"));
            }
        }
        if window.0 == 0 {
            return Err(self.malformed("window id must be nonzero"));
        }
        if self.windows.contains_key(&window) {
            return Err(self.malformed(format!(
                "window {} occurs more than once",
                window.0
            )));
        }
        self.leaf_count += 1;
        if self.leaf_count > MAX_WINDOWS {
            return Err(self.malformed("snapshot contains too many windows"));
        }
        let (identity, parent) =
            self.identities.get(&window).cloned().ok_or_else(|| {
                self.malformed(format!(
                    "window {} has no identity record",
                    window.0
                ))
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

    fn malformed(&self, message: impl Into<String>) -> TilefireError {
        TilefireError::MalformedSnapshot(message.into())
    }
}

fn restore_snapshot(snapshot: SessionSnapshot) -> Result<Tilefire> {
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(TilefireError::UnsupportedSnapshotVersion(
            snapshot.version,
        ));
    }
    let mut identities = BTreeMap::new();
    for window in snapshot.windows {
        if window.id.0 == 0 {
            return Err(TilefireError::MalformedSnapshot(
                "window id must be nonzero".into(),
            ));
        }
        if identities
            .insert(window.id, (window.identity, window.parent))
            .is_some()
        {
            return Err(TilefireError::MalformedSnapshot(format!(
                "window {} has duplicate identity records",
                window.id.0
            )));
        }
    }
    for (id, (_, parent)) in &identities {
        if parent.is_some_and(|parent| !identities.contains_key(&parent)) {
            return Err(TilefireError::MalformedSnapshot(format!(
                "window {} has a missing parent",
                id.0
            )));
        }
        let mut visited = BTreeSet::new();
        let mut current = *id;
        loop {
            if !visited.insert(current) {
                return Err(TilefireError::MalformedSnapshot(
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
        return Err(TilefireError::MalformedSnapshot(
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
                restore.malformed("output must contain at least one workspace")
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
            let root =
                restore.build_split(workspace.root, true, 0, &location)?;
            let mut leaf_ids = BTreeSet::new();
            for child in &root.children {
                collect_leaf_ids(&child.node, &mut leaf_ids);
            }
            if workspace
                .focused
                .is_some_and(|window| !leaf_ids.contains(&window))
            {
                return Err(
                    restore.malformed("focused window is not in its workspace")
                );
            }
            if workspace.stack.len() != leaf_ids.len()
                || workspace
                    .stack
                    .iter()
                    .any(|window| !leaf_ids.contains(window))
                || workspace.stack.iter().copied().collect::<BTreeSet<_>>()
                    != leaf_ids
            {
                return Err(restore
                    .malformed("stack order must contain every workspace window exactly once"));
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
        return Err(restore
            .malformed("every identity record must have exactly one corresponding tree leaf"));
    }
    let next_window_id = match restore.windows.keys().next_back() {
        Some(id) => id.0.checked_add(1).unwrap_or(0),
        None => 1,
    };
    Ok(Tilefire {
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
        .ok_or(TilefireError::InvalidPersistencePath)?;
    if file_name.is_empty() {
        return Err(TilefireError::InvalidPersistencePath);
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
            temporary_name.push(format!(
                ".tmp.{}.{}",
                std::process::id(),
                serial
            ));
            let temporary_path = parent.join(temporary_name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => Some(Ok((temporary_path, file))),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    last_collision = Some(error);
                    None
                }
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| {
            TilefireError::Io(last_collision.unwrap_or_else(|| {
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
