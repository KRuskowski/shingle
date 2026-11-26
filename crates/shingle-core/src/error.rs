use std::fmt::{Display, Formatter};

/// Errors returned by `shingle-core` operations.
#[derive(Debug)]
pub enum ShingleError {
  /// A window id was zero; ids start at 1.
  InvalidWindowId,
  /// The window is unknown, or not in the state the operation needs: for
  /// example, a minimized window cannot take focus and a floating window has no
  /// tiled layout.
  WindowNotFound(u64),
  /// The window id is already in use.
  DuplicateWindow(u64),
  /// An output identity was empty.
  InvalidOutputId,
  /// An output with this identity already exists.
  OutputExists(String),
  /// No output with this identity exists.
  OutputNotFound(String),
  /// The workspace does not exist on the output.
  WorkspaceNotFound,
  /// No workspace set has this index.
  WsetNotFound(u32),
  /// The workspace set is shown on an output, so it cannot be destroyed.
  WsetAttached(u32),
  /// The workspace set still holds windows, so it cannot be destroyed.
  WsetNotEmpty(u32),
  /// A rectangle was empty or its right or bottom edge exceeds `i32::MAX`. Used
  /// for workareas and for floating and snapped geometry.
  InvalidWorkarea,
  /// A tiled-edges mask had bits set above the four edge bits.
  InvalidTiledEdges(u32),
  /// A tree failed a structural check. No operation currently returns it.
  InvalidTree(String),
  /// An operation needed two different windows, or a parent assignment would
  /// make a window its own ancestor.
  SameWindow,
  /// Resizing found no visible tile across the requested edge.
  NoAdjacentWindow,
  /// The minimum extent was zero, or the tiles on both sides of the edge cannot
  /// both keep it.
  MinimumExtent,
  /// No window id or workspace set index is left to allocate.
  IdExhausted,
  /// A placeholder operation was given a mapped window.
  WindowStillMapped(u64),
  /// The snapshot version is neither 1 nor
  /// [`SNAPSHOT_VERSION`](crate::SNAPSHOT_VERSION).
  UnsupportedSnapshotVersion(u32),
  /// The snapshot parsed but breaks a model invariant, which the message names.
  MalformedSnapshot(String),
  /// A snapshot path has no file name.
  InvalidPersistencePath,
  /// Reading or writing a snapshot failed.
  Io(std::io::Error),
  /// A snapshot was not valid JSON or had unknown or missing fields.
  Json(serde_json::Error),
}

impl Display for ShingleError {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::InvalidWindowId => write!(f, "window id must be nonzero"),
      Self::WindowNotFound(id) => write!(f, "window {id} was not found"),
      Self::DuplicateWindow(id) => {
        write!(f, "window {id} is already attached")
      }
      Self::InvalidOutputId => {
        write!(f, "output identity cannot be empty")
      }
      Self::OutputExists(id) => write!(f, "output {id:?} already exists"),
      Self::OutputNotFound(id) => {
        write!(f, "output {id:?} was not found")
      }
      Self::WorkspaceNotFound => write!(f, "workspace was not found"),
      Self::WsetNotFound(index) => {
        write!(f, "workspace set {index} was not found")
      }
      Self::WsetAttached(index) => {
        write!(f, "workspace set {index} is shown on an output")
      }
      Self::WsetNotEmpty(index) => {
        write!(f, "workspace set {index} still holds windows")
      }
      Self::InvalidWorkarea => {
        write!(f, "workarea must have positive, representable geometry")
      }
      Self::InvalidTiledEdges(edges) => {
        write!(f, "tiled-edges mask {edges} exceeds 15")
      }
      Self::InvalidTree(message) => write!(f, "invalid tree: {message}"),
      Self::SameWindow => {
        write!(f, "source and target windows must differ")
      }
      Self::NoAdjacentWindow => write!(
        f,
        "window has no adjacent resizable split in that direction"
      ),
      Self::MinimumExtent => {
        write!(f, "resize would violate the minimum extent")
      }
      Self::IdExhausted => write!(f, "window id space is exhausted"),
      Self::WindowStillMapped(id) => {
        write!(f, "window {id} is mapped, not a placeholder")
      }
      Self::UnsupportedSnapshotVersion(version) => {
        write!(f, "unsupported snapshot version {version}")
      }
      Self::MalformedSnapshot(message) => {
        write!(f, "malformed snapshot: {message}")
      }
      Self::InvalidPersistencePath => {
        write!(f, "snapshot path must name a file")
      }
      Self::Io(error) => Display::fmt(error, f),
      Self::Json(error) => Display::fmt(error, f),
    }
  }
}

impl std::error::Error for ShingleError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Io(error) => Some(error),
      Self::Json(error) => Some(error),
      _ => None,
    }
  }
}

impl From<std::io::Error> for ShingleError {
  fn from(value: std::io::Error) -> Self {
    Self::Io(value)
  }
}

impl From<serde_json::Error> for ShingleError {
  fn from(value: serde_json::Error) -> Self {
    Self::Json(value)
  }
}
