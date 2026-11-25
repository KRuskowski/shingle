mod core;
mod error;
mod geometry;
mod session;
mod tree;

pub use core::{
  DetachedWindow, LaunchIdentity, OutputId, Shingle, WindowInfo,
  WindowPresence, WorkspaceId, WorkspaceRef,
};
pub use error::ShingleError;
pub use geometry::{Direction, Gaps, Rect, SplitDirection};
pub use session::{
  ChildSnapshot, NodeSnapshot, OutputSnapshot, SNAPSHOT_VERSION,
  SessionSnapshot, SplitSnapshot, WindowSnapshot, WorkspaceSnapshot,
};
pub use tree::{LeafState, StackingMode, TileTree, WindowId, WindowLayout};

pub type Result<T> = std::result::Result<T, ShingleError>;
