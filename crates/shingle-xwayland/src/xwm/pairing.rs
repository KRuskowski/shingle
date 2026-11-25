//! Pairing X windows with `wl_surface`s by the `xwayland_shell_v1` serial.
//!
//! Xwayland announces the same serial on both of its connections: to the
//! window manager in a `WL_SURFACE_SERIAL` client message on the window,
//! and to the compositor in `xwayland_surface_v1.set_serial`, which takes
//! effect on the surface's next commit. The two sockets have no ordering
//! between them, so either half may come first. This is wlroots'
//! `unpaired_surfaces` list (xwayland/xwm.c), with the surface half kept
//! too, since Shingle's protocol side and window manager are separate.

use std::{collections::HashMap, hash::Hash};

use x11rb::protocol::xproto::Window;

/// The result of offering one half of a pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Paired<S> {
  /// Both halves are known now.
  Now {
    /// The X window.
    window: Window,
    /// The surface.
    surface: S,
  },
  /// The other half has not arrived.
  Waiting,
}

/// Serial-keyed halves waiting for their partner. `S` names a surface.
#[derive(Clone, Debug)]
pub struct Pairing<S> {
  windows: HashMap<u64, Window>,
  surfaces: HashMap<u64, S>,
}

impl<S> Default for Pairing<S> {
  fn default() -> Self {
    Self {
      windows: HashMap::new(),
      surfaces: HashMap::new(),
    }
  }
}

impl<S: Copy + Eq + Hash> Pairing<S> {
  /// An X window announced `serial`. A window announces one serial per
  /// surface, so an older unpaired serial of the same window is dropped.
  pub fn window_serial(&mut self, window: Window, serial: u64) -> Paired<S> {
    if let Some(surface) = self.surfaces.remove(&serial) {
      return Paired::Now { window, surface };
    }
    self.windows.retain(|_, candidate| *candidate != window);
    self.windows.insert(serial, window);
    Paired::Waiting
  }

  /// A surface committed `serial`.
  pub fn surface_serial(&mut self, surface: S, serial: u64) -> Paired<S> {
    if let Some(window) = self.windows.remove(&serial) {
      return Paired::Now { window, surface };
    }
    self.surfaces.retain(|_, candidate| *candidate != surface);
    self.surfaces.insert(serial, surface);
    Paired::Waiting
  }

  /// The window was unmapped or destroyed: its waiting serial, if any,
  /// can no longer pair.
  pub fn window_destroyed(&mut self, window: Window) {
    self.windows.retain(|_, candidate| *candidate != window);
  }

  /// The surface was destroyed: its waiting serial can no longer pair.
  pub fn surface_destroyed(&mut self, surface: S) {
    self.surfaces.retain(|_, candidate| *candidate != surface);
  }

  /// Whether nothing is waiting.
  pub fn is_empty(&self) -> bool {
    self.windows.is_empty() && self.surfaces.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn window_first_then_surface() {
    let mut pairing = Pairing::default();
    assert_eq!(pairing.window_serial(0x10, 7), Paired::Waiting);
    assert_eq!(
      pairing.surface_serial(3u64, 7),
      Paired::Now {
        window: 0x10,
        surface: 3
      }
    );
    assert!(pairing.is_empty());
  }

  #[test]
  fn surface_first_then_window() {
    let mut pairing = Pairing::default();
    assert_eq!(pairing.surface_serial(3u64, 7), Paired::Waiting);
    assert_eq!(
      pairing.window_serial(0x10, 7),
      Paired::Now {
        window: 0x10,
        surface: 3
      }
    );
    assert!(pairing.is_empty());
  }

  #[test]
  fn different_serials_do_not_pair() {
    let mut pairing = Pairing::default();
    assert_eq!(pairing.window_serial(0x10, 7), Paired::Waiting);
    assert_eq!(pairing.surface_serial(3u64, 8), Paired::Waiting);
    assert!(!pairing.is_empty());
  }

  #[test]
  fn a_destroyed_window_never_pairs() {
    let mut pairing = Pairing::default();
    pairing.window_serial(0x10, 7);
    pairing.window_destroyed(0x10);
    assert_eq!(pairing.surface_serial(3u64, 7), Paired::Waiting);
    pairing.surface_destroyed(3);
    assert!(pairing.is_empty());
  }

  #[test]
  fn a_destroyed_surface_never_pairs() {
    let mut pairing = Pairing::default();
    pairing.surface_serial(3u64, 7);
    pairing.surface_destroyed(3);
    assert_eq!(pairing.window_serial(0x10, 7), Paired::Waiting);
    pairing.window_destroyed(0x10);
    assert!(pairing.is_empty());
  }

  #[test]
  fn a_remapped_window_pairs_with_its_new_surface() {
    let mut pairing = Pairing::default();
    // First map: surface 3 with serial 7.
    pairing.window_serial(0x10, 7);
    pairing.surface_serial(3u64, 7);
    // Unmap destroys surface 3; the remap brings surface 4 with serial 9.
    pairing.surface_destroyed(3);
    assert_eq!(pairing.surface_serial(4, 9), Paired::Waiting);
    assert_eq!(
      pairing.window_serial(0x10, 9),
      Paired::Now {
        window: 0x10,
        surface: 4
      }
    );
  }

  #[test]
  fn a_newer_serial_replaces_a_waiting_one() {
    let mut pairing = Pairing::default();
    pairing.window_serial(0x10, 7);
    pairing.window_serial(0x10, 9);
    assert_eq!(pairing.surface_serial(3u64, 7), Paired::Waiting);
    pairing.surface_destroyed(3);
    assert!(matches!(pairing.surface_serial(4, 9), Paired::Now { .. }));
    assert!(pairing.is_empty());
  }
}
