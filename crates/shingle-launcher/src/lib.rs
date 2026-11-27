//! shingle-launcher: an application launcher with rofi's type-to-run and
//! the KDE menu's in-place editing (docs/design/launcher.md).
//!
//! The library holds everything that needs neither Wayland nor a GPU, so
//! that it is unit-tested: discovery ([`index`]), matching ([`matching`],
//! [`search`]), launch history ([`history`]), the user's layout
//! ([`layout`]), what is shown with it applied ([`catalog`]), the state
//! keys and the pointer change ([`model`]), the panel as Gloomy widgets
//! ([`view`], [`theme`]) and entries' icons ([`icons`] finds them in the
//! icon theme, [`icon_cache`] decodes and caches them). [`render`] draws
//! that with Gloomy, [`shm`] holds the pixels for the compositor and
//! [`launch`] runs the choice; the binary adds the Wayland client.

pub mod catalog;
pub mod desktop;
pub mod files;
pub mod history;
pub mod icon_cache;
pub mod icons;
pub mod index;
pub mod launch;
pub mod layout;
pub mod matching;
pub mod model;
pub mod render;
pub mod search;
pub mod shm;
pub mod theme;
pub mod view;
