//! The Wayland half of the clipboard and primary selection bridge
//! (docs/design/xwayland.md, section 5).
//!
//! An X owner's offer becomes an X11 source in `data_device` or
//! `primary_selection` ([`offered`], [`cleared`]), which focused Wayland
//! clients and wlr-data-control clients see like any other. Their
//! requests for it wait in `Status::selection_receives` until
//! [`Bridge::sync`] hands them to the window manager. The other way,
//! [`Bridge::sync`] reports each change of a Wayland-owned selection to the
//! window manager, and [`send`] passes an X client's data request to the
//! Wayland source. [`Bridge::watch`] keeps a calloop source on every
//! transfer descriptor, so that the loop wakes when a transfer can move.

use std::{collections::HashMap, os::fd::AsFd};

use calloop::{
  Interest, LoopHandle, Mode, PostAction, RegistrationToken, generic::Generic,
};
use shingle_xwayland::{
  Selection, SelectionSend, X11Connection, Xwm, XwmError,
};

use crate::{
  data_device::{self, SourceKey},
  primary_selection,
  state::CompositorState,
};

/// Installs an X owner's offer of `selection` as the Wayland selection.
pub(crate) fn offered(
  state: &mut CompositorState,
  selection: Selection,
  mime_types: Vec<String>,
) {
  match selection {
    Selection::Clipboard => {
      data_device::set_x11_selection(state, mime_types);
    }
    Selection::Primary => {
      primary_selection::set_x11_selection(state, mime_types);
    }
    // An X drag's types come with its XdndEnter.
    Selection::Dnd => {}
  }
}

/// The X owner of `selection` went away: the Wayland selection is cleared
/// if it is still the X owner's.
pub(crate) fn cleared(state: &mut CompositorState, selection: Selection) {
  match selection {
    Selection::Clipboard => data_device::clear_x11_selection(state),
    Selection::Primary => primary_selection::clear_x11_selection(state),
    Selection::Dnd => {}
  }
}

/// The Wayland side of `selection`: its source, and the MIME types when
/// the source is a Wayland client's rather than an X client's.
pub(crate) fn wayland_side(
  state: &CompositorState,
  selection: Selection,
) -> (Option<SourceKey>, Option<Vec<String>>) {
  let current = match selection {
    Selection::Clipboard => data_device::selection(state),
    Selection::Primary => primary_selection::selection(state),
    // A drag is not a selection.
    Selection::Dnd => None,
  };
  match current {
    Some((key, mime_types)) => {
      let offer = (!key.is_x11()).then_some(mime_types);
      (Some(key), offer)
    }
    None => (None, None),
  }
}

/// Passes an X client's request for the Wayland selection to its source.
/// When the selection has changed to none or to an X client's since,
/// the descriptor is dropped, which ends the transfer with no data.
pub(crate) fn send(state: &mut CompositorState, send: SelectionSend) {
  let (Some(key), Some(_)) = wayland_side(state, send.selection) else {
    return;
  };
  match send.selection {
    Selection::Clipboard => {
      data_device::receive_from(state, &key, send.mime_type, send.fd);
    }
    Selection::Primary => {
      primary_selection::receive_from(state, &key, send.mime_type, send.fd);
    }
    Selection::Dnd => {}
  }
}

/// The selections bridged: `PRIMARY` only without
/// `disable_primary_selection`.
pub(crate) fn bridged(state: &CompositorState) -> &'static [Selection] {
  if state.disable_primary_selection {
    &[Selection::Clipboard]
  } else {
    &[Selection::Clipboard, Selection::Primary]
  }
}

/// What the glue keeps for one window manager.
#[derive(Default)]
pub(crate) struct Bridge {
  /// The source of each selection as last reported to the window
  /// manager.
  reported: HashMap<Selection, Option<SourceKey>>,
  /// The calloop source of each transfer descriptor, by transfer and
  /// direction.
  watches: HashMap<(u64, bool), RegistrationToken>,
}

impl Bridge {
  /// The selections whose Wayland side changed since the last call, with
  /// what to report: MIME types for a Wayland source, `None` for no
  /// selection or an X client's.
  pub(crate) fn changes(
    &mut self,
    state: &CompositorState,
    selections: &[Selection],
  ) -> Vec<(Selection, Option<Vec<String>>)> {
    let mut changes = Vec::new();
    for selection in selections {
      let (key, offer) = wayland_side(state, *selection);
      // Nothing reported yet counts as no selection.
      if self.reported.get(selection).unwrap_or(&None) != &key {
        self.reported.insert(*selection, key);
        changes.push((*selection, offer));
      }
    }
    changes
  }

  /// Reports changed Wayland selections to the window manager and hands
  /// it the Wayland clients' requests for X selections.
  pub(crate) fn sync(
    &mut self,
    state: &mut CompositorState,
    xwm: &mut Xwm<X11Connection>,
  ) -> Result<(), XwmError> {
    let selections = xwm.bridged_selections();
    for (selection, offer) in self.changes(state, &selections) {
      xwm.set_selection(selection, offer)?;
    }
    for (selection, mime_type, fd) in
      std::mem::take(&mut state.xwayland.selection_receives)
    {
      xwm.receive_selection(selection, &mime_type, fd)?;
    }
    xwm.pump_transfers()
  }

  /// Watches the descriptors of the window manager's running transfers,
  /// and stops watching those that ended or wait for something else.
  /// Each source owns a duplicate of its descriptor, so it stays valid
  /// whenever the window manager closes the original.
  pub(crate) fn watch(
    &mut self,
    xwm: &Xwm<X11Connection>,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    let wanted = xwm.transfer_watches();
    self.watches.retain(|key, token| {
      let keep = wanted
        .iter()
        .any(|watch| (watch.id, watch.writable) == *key);
      if !keep {
        handle.remove(*token);
      }
      keep
    });
    for watch in wanted {
      let key = (watch.id, watch.writable);
      if self.watches.contains_key(&key) {
        continue;
      }
      let Ok(fd) = watch.fd.as_fd().try_clone_to_owned() else {
        continue;
      };
      let interest = if watch.writable {
        Interest::WRITE
      } else {
        Interest::READ
      };
      // The callback only wakes the loop; `Xwayland::poll` pumps every
      // transfer each turn.
      let inserted = handle.insert_source(
        Generic::new(fd, interest, Mode::Level),
        |_, _, _: &mut CompositorState| Ok(PostAction::Continue),
      );
      match inserted {
        Ok(token) => {
          self.watches.insert(key, token);
        }
        Err(error) => {
          eprintln!("shingle: cannot watch a selection transfer: {error}");
        }
      }
    }
  }

  /// Removes every transfer source and forgets what was reported, when
  /// the window manager goes.
  pub(crate) fn clear(
    &mut self,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    for (_, token) in self.watches.drain() {
      handle.remove(token);
    }
    self.reported.clear();
  }
}

/// Clears both selections where an X client's source holds them and
/// drops the requests waiting for one, when the window manager goes.
pub(crate) fn forget_x11(state: &mut CompositorState) {
  data_device::clear_x11_selection(state);
  primary_selection::clear_x11_selection(state);
  state.xwayland.selection_receives.clear();
}

#[cfg(test)]
mod tests;
