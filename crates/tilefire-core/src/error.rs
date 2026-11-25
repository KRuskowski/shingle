use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum TilefireError {
    InvalidWindowId,
    WindowNotFound(u64),
    DuplicateWindow(u64),
    InvalidOutputId,
    OutputExists(String),
    OutputNotFound(String),
    WorkspaceNotFound,
    InvalidWorkarea,
    InvalidTiledEdges(u32),
    InvalidTree(String),
    SameWindow,
    NoAdjacentWindow,
    MinimumExtent,
    IdExhausted,
    WindowStillMapped(u64),
    UnsupportedSnapshotVersion(u32),
    MalformedSnapshot(String),
    InvalidPersistencePath,
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl Display for TilefireError {
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

impl std::error::Error for TilefireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TilefireError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for TilefireError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
