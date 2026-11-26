//! Selections shared with a host session, for the nested backend's
//! clipboard bridge (crates/shingle-nested/src/clipboard.rs).
//!
//! The host's clipboard or primary selection becomes a host source in
//! `data_device` or `primary_selection` ([`Compositor::set_host_selection`]),
//! keyed by [`SourceKey::Host`] like the X11 sources of the XWayland bridge:
//! focused Wayland clients, wlr-data-control clients and X clients see it
//! like any other selection, and their reads of it wait in
//! [`HostSelections`] until the backend takes them
//! ([`Compositor::take_host_selection_requests`]) and passes each
//! descriptor to the host. The other way, the backend asks
//! [`Compositor::selection_owner`] who owns each selection, offers a local
//! one to the host, and hands the host's reads of it to the local source
//! with [`Compositor::send_selection`]. Either way the reader's own
//! descriptor reaches the writer, so no data passes through Shingle.

use std::os::fd::OwnedFd;

use crate::{
  Compositor, data_device, data_device::SourceKey, primary_selection,
  state::CompositorState,
};

/// One of the two selections a host bridge shares.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SelectionKind {
  /// The clipboard: `wl_data_device`'s selection.
  Clipboard,
  /// The primary selection: `zwp_primary_selection_device_v1`'s.
  Primary,
}

/// Identifies one source of a selection in this compositor, so that a
/// bridge can tell whether the selection changed hands.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SelectionSourceId(SourceKey);

/// Who owns a selection, as a host bridge sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionOwner {
  /// Nobody: the selection is empty.
  None,
  /// The host session, through [`Compositor::set_host_selection`].
  Host(SelectionSourceId),
  /// A client of this compositor: a Wayland client, a wlr-data-control
  /// client or an X client.
  Local {
    /// The source.
    source: SelectionSourceId,
    /// The MIME types it offers, in its order.
    mime_types: Vec<String>,
  },
}

/// A client's read of the host session's selection: the bridge asks the
/// host to write `mime_type` into `fd`.
#[derive(Debug)]
pub struct HostSelectionRequest {
  /// The selection read.
  pub kind: SelectionKind,
  /// The host source read, as [`Compositor::set_host_selection`] returned
  /// it; a request for one that has since been replaced is to be dropped.
  pub source: SelectionSourceId,
  /// The MIME type asked for, one the host source offers.
  pub mime_type: String,
  /// The reader's descriptor, to be written and closed by the host.
  pub fd: OwnedFd,
}

/// The reads of host sources that wait for the bridge.
#[derive(Debug, Default)]
pub(crate) struct HostSelections {
  requests: Vec<HostSelectionRequest>,
}

/// Queues a client's read of a host source for the bridge.
pub(crate) fn request(
  state: &mut CompositorState,
  kind: SelectionKind,
  source: SourceKey,
  mime_type: String,
  fd: OwnedFd,
) {
  state.host_selection.requests.push(HostSelectionRequest {
    kind,
    source: SelectionSourceId(source),
    mime_type,
    fd,
  });
}

/// `mime_types` without empty names and repeats, in their first order.
fn distinct(mime_types: Vec<String>) -> Vec<String> {
  let mut seen = Vec::with_capacity(mime_types.len());
  for mime_type in mime_types {
    if !mime_type.is_empty() && !seen.contains(&mime_type) {
      seen.push(mime_type);
    }
  }
  seen
}

/// The source of selection `kind` and its MIME types.
fn current(
  state: &CompositorState,
  kind: SelectionKind,
) -> Option<(SourceKey, Vec<String>)> {
  match kind {
    SelectionKind::Clipboard => data_device::selection(state),
    SelectionKind::Primary => primary_selection::selection(state),
  }
}

impl Compositor {
  /// Makes the host session's selection of `kind`, offering `mime_types`,
  /// this compositor's selection, replacing and cancelling the source that
  /// held it. Empty and repeated types are left out. Clients see it like
  /// any other selection, and their reads wait in
  /// [`Compositor::take_host_selection_requests`]. Returns the source's
  /// identity, which those requests carry.
  pub fn set_host_selection(
    &mut self,
    kind: SelectionKind,
    mime_types: Vec<String>,
  ) -> SelectionSourceId {
    let mime_types = distinct(mime_types);
    let key = match kind {
      SelectionKind::Clipboard => {
        data_device::set_host_selection(&mut self.state, mime_types)
      }
      SelectionKind::Primary => {
        primary_selection::set_host_selection(&mut self.state, mime_types)
      }
    };
    SelectionSourceId(key)
  }

  /// Clears selection `kind` if the host session's source still holds it;
  /// a local selection is left alone.
  pub fn clear_host_selection(&mut self, kind: SelectionKind) {
    match kind {
      SelectionKind::Clipboard => {
        data_device::clear_host_selection(&mut self.state);
      }
      SelectionKind::Primary => {
        primary_selection::clear_host_selection(&mut self.state);
      }
    }
  }

  /// Who owns selection `kind` now.
  pub fn selection_owner(&self, kind: SelectionKind) -> SelectionOwner {
    match current(&self.state, kind) {
      None => SelectionOwner::None,
      Some((key, _)) if key.is_host() => {
        SelectionOwner::Host(SelectionSourceId(key))
      }
      Some((key, mime_types)) => SelectionOwner::Local {
        source: SelectionSourceId(key),
        mime_types,
      },
    }
  }

  /// Passes the host session's read of selection `kind` to its local
  /// source: the source is asked to write `mime_type` into `fd`. Returns
  /// false, dropping `fd` and so ending the reader's transfer with no data,
  /// when `source` no longer holds the selection, is the host's own, or
  /// does not offer `mime_type`.
  pub fn send_selection(
    &mut self,
    kind: SelectionKind,
    source: &SelectionSourceId,
    mime_type: String,
    fd: OwnedFd,
  ) -> bool {
    let Some((key, mime_types)) = current(&self.state, kind) else {
      return false;
    };
    if key != source.0 || key.is_host() || !mime_types.contains(&mime_type) {
      return false;
    }
    match kind {
      SelectionKind::Clipboard => {
        data_device::receive_from(&mut self.state, &key, mime_type, fd);
      }
      SelectionKind::Primary => {
        primary_selection::receive_from(&mut self.state, &key, mime_type, fd);
      }
    }
    true
  }

  /// Takes the clients' reads of host selections made since the last
  /// call, in order.
  pub fn take_host_selection_requests(&mut self) -> Vec<HostSelectionRequest> {
    std::mem::take(&mut self.state.host_selection.requests)
  }

  /// Whether this compositor has a primary selection to share: false
  /// under `disable_primary_selection`, which also keeps it from a host.
  pub fn primary_selection_enabled(&self) -> bool {
    !self.state.disable_primary_selection
  }
}

#[cfg(test)]
mod tests;
