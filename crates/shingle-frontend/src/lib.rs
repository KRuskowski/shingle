//! Shingle's interactive frontend, shared by the nested and native hosts.
//!
//! It knows nothing about winit, DRM or libinput. A host binary creates a
//! [`Frontend`], registers its seat devices with the compositor, feeds it
//! [`host::InputEvent`]s and implements [`host::Host`] for the few things
//! only it can do; see docs/design/native-backend.md, "Frontend split".

#![warn(missing_docs)]

pub mod bindings;
pub mod expo;
pub mod extra_gestures;
mod frontend;
pub mod host;
pub mod icon_theme;
pub mod keyboard;
pub mod render_target;
pub mod scale;
pub mod scale_title;
pub mod workspace_transition;

pub use frontend::Frontend;
