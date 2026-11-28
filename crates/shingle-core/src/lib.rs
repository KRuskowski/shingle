//! Shingle's persistent window model, with no Wayland dependency.
//!
//! [`Shingle`] owns workspace sets, their workspaces and every known window,
//! and knows which output shows which set ([`WsetId`]; a set on no output
//! is parked and keeps its trees). Each workspace holds a [`TileTree`]: the
//! source of truth for tile order, split weights, focus, stacking and
//! per-window flags such as floating, snapped or minimized. The compositor
//! derives on-screen geometry from the tree with [`TileTree::layout`];
//! temporary visual geometry (overviews, animations, rotation) must never
//! be written back into it.
//!
//! The model is persisted as a [`SessionSnapshot`]. Restoring a snapshot
//! reproduces tile order and sizes exactly, with every window as a
//! placeholder ([`WindowPresence::Placeholder`]) until a client with a
//! matching [`LaunchIdentity`] maps and is claimed by
//! [`Shingle::reconcile_mapped`].
//!
//! Directions follow Wayfire's naming: a [`SplitDirection::Vertical`] split
//! has vertical dividers and arranges children left to right, a
//! [`SplitDirection::Horizontal`] split arranges them top to bottom.

#![warn(missing_docs)]

mod core;
mod error;
mod geometry;
mod session;
mod tree;
mod wset;

pub use core::{
  DetachedWindow, LaunchIdentity, OutputId, Shingle, WindowInfo,
  WindowPresence, WorkspaceId, WorkspaceRef,
};
pub use error::ShingleError;
pub use geometry::{Direction, Gaps, Rect, SplitDirection};
pub use session::{
  ChildSnapshot, NodeSnapshot, OutputSnapshot, SNAPSHOT_VERSION,
  SessionSnapshot, SplitSnapshot, SyncTarget, WindowSnapshot,
  WorkspaceSnapshot, WsetSnapshot, sync_in_place,
};
pub use tree::{LeafState, StackingMode, TileTree, WindowId, WindowLayout};
pub use wset::{OutputIdentity, WsetId, WsetInfo};

/// Result type for fallible `shingle-core` operations.
pub type Result<T> = std::result::Result<T, ShingleError>;
