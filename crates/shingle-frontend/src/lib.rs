//! Shingle's interactive frontend, shared by the nested and native hosts.
//!
//! It knows nothing about winit, DRM or libinput. A host binary creates a
//! [`Frontend`], registers its seat devices with the compositor, feeds it
//! [`host::InputEvent`]s and implements [`host::Host`] for the few things
//! only it can do; see docs/design/native-backend.md, "Frontend split".

#![warn(missing_docs)]

pub mod animate;
pub mod bindings;
mod dmabuf;
pub mod expo;
pub mod extra_gestures;
pub mod frame_raster;
mod frontend;
pub mod host;
pub mod icon_theme;
pub mod keyboard;
pub mod recorder;
pub mod render_target;
pub mod scale;
pub mod scale_title;
mod shm;
mod switcher_view;
#[cfg(test)]
mod test_client;
#[cfg(test)]
mod test_gpu;
pub mod vswipe;
pub mod workspace_transition;

pub use frontend::{CursorImage, CursorPlane, Frontend};
