//! Shingle's interactive frontend, shared by the nested and native hosts.
//!
//! It knows nothing about winit, DRM or libinput. So far it holds the
//! self-contained parts: Expo, workspace transitions, extra gestures, Scale's
//! layout and titles, the icon theme, binding helpers and the xkb keyboard.
//! The rest of the nested `App` moves here in NB-4 (see the NB-4 entry in
//! docs/design/native-backend.md).

#![warn(missing_docs)]

pub mod bindings;
pub mod expo;
pub mod extra_gestures;
pub mod icon_theme;
pub mod keyboard;
pub mod scale;
pub mod scale_title;
pub mod workspace_transition;
