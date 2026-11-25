//! The fast switcher and the regular (Alt+Tab) switcher.

use std::collections::HashMap;

use shingle_config::Modifiers as ConfigModifiers;
use shingle_core::WindowId as TileWindowId;

use super::Frontend;
use crate::bindings::{binding_modifiers, modifiers_empty};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SwitcherKind {
  Fast,
  Regular,
}

pub(super) struct SwitcherState {
  pub(super) kind: SwitcherKind,
  pub(super) windows: Vec<TileWindowId>,
  pub(super) selected: usize,
  pub(super) modifiers: ConfigModifiers,
  pub(super) original_opacity: HashMap<TileWindowId, f32>,
}

impl Frontend {
  pub(super) fn start_switcher(
    &mut self,
    reverse: bool,
    binding: shingle_config::Binding,
    kind: SwitcherKind,
  ) {
    if self.switcher.is_none() {
      self.compositor.set_show_desktop(false);
      let mut windows = self.compositor.switchable_windows();
      windows.retain(|window| {
        self
          .compositor
          .core()
          .window_info(*window)
          .is_ok_and(|info| !info.state.minimized)
      });
      if windows.is_empty() {
        return;
      }
      let selected = self
        .compositor
        .focused_window()
        .and_then(|focused| {
          windows.iter().position(|window| *window == focused)
        })
        .unwrap_or(0);
      let original_opacity = windows
        .iter()
        .filter_map(|window| {
          self
            .compositor
            .window_opacity(*window)
            .map(|opacity| (*window, opacity))
        })
        .collect();
      self.switcher = Some(SwitcherState {
        kind,
        windows,
        selected,
        modifiers: binding_modifiers(&binding),
        original_opacity,
      });
    }
    self.advance_switcher(reverse);
    if self
      .switcher
      .as_ref()
      .is_some_and(|state| modifiers_empty(state.modifiers))
    {
      self.finish_switcher();
    }
  }

  pub(super) fn advance_switcher(&mut self, reverse: bool) {
    let Some(state) = &mut self.switcher else {
      return;
    };
    if state.windows.is_empty() {
      self.switcher = None;
      return;
    }
    state.selected = if reverse {
      (state.selected + state.windows.len() - 1) % state.windows.len()
    } else {
      (state.selected + 1) % state.windows.len()
    };
    self.apply_switcher();
  }

  pub(super) fn apply_switcher(&mut self) {
    let Some(state) = &self.switcher else {
      return;
    };
    let selected = state.windows.get(state.selected).copied();
    let updates = state
      .original_opacity
      .iter()
      .map(|(window, opacity)| (*window, *opacity))
      .collect::<Vec<_>>();
    for (window, original) in updates {
      self.compositor.set_window_opacity(
        window,
        if Some(window) == selected || state.kind == SwitcherKind::Regular {
          original
        } else {
          original * self.switcher_inactive_alpha
        },
      );
    }
    if let Some(selected) = selected
      && let Err(error) = self.compositor.raise_window(selected)
    {
      eprintln!("shingle: fast-switcher raise failed: {error}");
    }
    self.request_redraw();
  }

  pub(super) fn finish_switcher(&mut self) {
    let Some(state) = self.switcher.take() else {
      return;
    };
    for (window, opacity) in state.original_opacity {
      self.compositor.set_window_opacity(window, opacity);
    }
    if let Some(window) = state.windows.get(state.selected).copied() {
      self.compositor.focus_window(window);
      if let Err(error) = self.compositor.raise_window(window) {
        eprintln!("shingle: fast-switcher focus failed: {error}");
      }
    }
    self.save();
    self.request_redraw();
  }

  pub(super) fn switcher_window_destroyed(&mut self, window: TileWindowId) {
    let Some(state) = &mut self.switcher else {
      return;
    };
    state.original_opacity.remove(&window);
    if let Some(removed) = state
      .windows
      .iter()
      .position(|candidate| *candidate == window)
    {
      state.windows.remove(removed);
      if removed < state.selected {
        state.selected -= 1;
      }
    }
    if state.windows.is_empty() {
      self.finish_switcher();
      return;
    }
    state.selected = state.selected.min(state.windows.len() - 1);
    self.apply_switcher();
  }
}
