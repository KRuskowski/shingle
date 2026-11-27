//! shingle-launcher: an application launcher with rofi's type-to-run and
//! the KDE menu's in-place editing (docs/design/launcher.md).
//!
//! The library holds everything that needs neither Wayland nor a GPU, so
//! that it is unit-tested: discovery ([`index`]), matching ([`matching`],
//! [`search`]), launch history ([`history`]), the user's layout
//! ([`layout`]) and what is shown with it applied ([`catalog`]).

pub mod catalog;
pub mod desktop;
pub mod files;
pub mod history;
pub mod index;
pub mod layout;
pub mod matching;
pub mod model;
pub mod search;
