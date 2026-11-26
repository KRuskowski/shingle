//! The clipboard and primary selection shared with the host session.
//!
//! Nested Shingle opens a second Wayland connection to its host and uses
//! the host's data-control protocol ([`host`]), which needs no keyboard
//! focus or input serial. [`Bridge`] keeps the two sides in step for each
//! selection it shares:
//!
//! - A host selection becomes a host source in nested
//!   ([`NestedSide::set_host`]), which nested's clients read like any
//!   other; each read's descriptor goes to the host offer
//!   ([`HostSide::receive`]).
//! - A nested selection is offered to the host through a data-control
//!   source ([`HostSide::offer`]); each read the host sends it goes to the
//!   nested source with the host reader's descriptor ([`NestedSide::send`]).
//!
//! The writer always gets the reader's own descriptor, so no data passes
//! through Shingle and no transfer can block either compositor, whatever
//! its size and however slow its reader. Nothing mirrored one way is
//! mirrored back: while one of Shingle's sources holds the host's
//! selection, the host's report of that selection is its own echo and is
//! dropped, and a nested selection owned by the host is not offered to
//! it.

pub(crate) mod host;

use std::{
  os::{fd::OwnedFd, unix::net::UnixStream},
  path::Path,
};

use shingle::{
  Compositor, HostSelectionRequest, SelectionKind, SelectionOwner,
  SelectionSourceId,
};

use host::HostClipboard;

/// The selections nested shares with its host: the clipboard, and the
/// primary selection unless `disable_primary_selection` is set.
pub(crate) fn wanted(primary_enabled: bool) -> Vec<SelectionKind> {
  if primary_enabled {
    vec![SelectionKind::Clipboard, SelectionKind::Primary]
  } else {
    vec![SelectionKind::Clipboard]
  }
}

/// Connects the bridge to the host at `socket` for `compositor`, when
/// `behavior.host_clipboard` (`enabled`) asks for one. `None`, after a log
/// line, when the host is not a Wayland session or cannot be reached.
pub(crate) fn share_with_host(
  compositor: &Compositor,
  enabled: bool,
  socket: Option<&Path>,
) -> Option<HostClipboard<SelectionSourceId>> {
  if !enabled {
    return None;
  }
  let Some(socket) = socket else {
    eprintln!(
      "shingle: the host is not a Wayland session; the clipboard is not \
       shared with it"
    );
    return None;
  };
  let wanted = wanted(compositor.primary_selection_enabled());
  let connected = UnixStream::connect(socket)
    .map_err(Into::into)
    .and_then(|stream| HostClipboard::connect(stream, wanted));
  match connected {
    Ok(clipboard) => Some(clipboard),
    Err(error) => {
      eprintln!(
        "shingle: cannot reach the host at {} to share the clipboard: \
         {error}",
        socket.display()
      );
      None
    }
  }
}

/// Who owns a selection in nested.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Owner<Id> {
  /// Nobody.
  None,
  /// The host, through [`NestedSide::set_host`].
  Host(Id),
  /// A nested client, offering `mime_types`.
  Local {
    /// The nested source.
    source: Id,
    /// Its MIME types, in its order.
    mime_types: Vec<String>,
  },
}

/// A nested client's read of a host source.
#[derive(Debug)]
pub(crate) struct Read<Id> {
  /// The selection read.
  pub(crate) kind: SelectionKind,
  /// The host source read.
  pub(crate) source: Id,
  /// The MIME type asked for.
  pub(crate) mime_type: String,
  /// The reader's descriptor.
  pub(crate) fd: OwnedFd,
}

/// The nested compositor, as the bridge uses it.
pub(crate) trait NestedSide {
  /// Identifies a nested selection source.
  type Id: Clone + PartialEq;

  /// Who owns selection `kind`.
  fn owner(&self, kind: SelectionKind) -> Owner<Self::Id>;

  /// Makes the host's selection, offering `mime_types`, nested's
  /// selection `kind`, and returns the new host source.
  fn set_host(
    &mut self,
    kind: SelectionKind,
    mime_types: Vec<String>,
  ) -> Self::Id;

  /// Clears selection `kind` if a host source holds it.
  fn clear_host(&mut self, kind: SelectionKind);

  /// Asks nested source `source` to write `mime_type` into `fd`, if it
  /// still holds selection `kind`; otherwise `fd` is dropped.
  fn send(
    &mut self,
    kind: SelectionKind,
    source: &Self::Id,
    mime_type: String,
    fd: OwnedFd,
  );

  /// Takes the reads of host sources made since the last call.
  fn take_reads(&mut self) -> Vec<Read<Self::Id>>;
}

impl NestedSide for Compositor {
  type Id = SelectionSourceId;

  fn owner(&self, kind: SelectionKind) -> Owner<SelectionSourceId> {
    match self.selection_owner(kind) {
      SelectionOwner::None => Owner::None,
      SelectionOwner::Host(source) => Owner::Host(source),
      SelectionOwner::Local { source, mime_types } => {
        Owner::Local { source, mime_types }
      }
    }
  }

  fn set_host(
    &mut self,
    kind: SelectionKind,
    mime_types: Vec<String>,
  ) -> SelectionSourceId {
    self.set_host_selection(kind, mime_types)
  }

  fn clear_host(&mut self, kind: SelectionKind) {
    self.clear_host_selection(kind);
  }

  fn send(
    &mut self,
    kind: SelectionKind,
    source: &SelectionSourceId,
    mime_type: String,
    fd: OwnedFd,
  ) {
    self.send_selection(kind, source, mime_type, fd);
  }

  fn take_reads(&mut self) -> Vec<Read<SelectionSourceId>> {
    self
      .take_host_selection_requests()
      .into_iter()
      .map(
        |HostSelectionRequest {
           kind,
           source,
           mime_type,
           fd,
         }| Read {
          kind,
          source,
          mime_type,
          fd,
        },
      )
      .collect()
  }
}

/// The host's data-control protocol, as the bridge uses it.
pub(crate) trait HostSide {
  /// One of Shingle's data-control sources on the host.
  type Source: PartialEq;
  /// A data-control offer of the host's selection.
  type Offer;

  /// Creates a source offering `mime_types` and makes it the host's
  /// selection `kind`.
  fn offer(
    &mut self,
    kind: SelectionKind,
    mime_types: &[String],
  ) -> Self::Source;

  /// Destroys `source`; the host's selection goes with it if `source`
  /// still holds it.
  fn withdraw(&mut self, source: Self::Source);

  /// Asks the host to write `mime_type` of `offer` into `fd`.
  fn receive(&mut self, offer: &Self::Offer, mime_type: String, fd: OwnedFd);

  /// Destroys `offer`.
  fn discard(&mut self, offer: Self::Offer);
}

/// What the host's data-control device and sources report.
#[derive(Debug)]
pub(crate) enum HostEvent<S, O> {
  /// The host's selection `kind` is now `offer` with `mime_types`, or
  /// empty.
  Selection {
    /// The selection.
    kind: SelectionKind,
    /// The offer, `None` for no selection.
    offer: Option<O>,
    /// The offer's MIME types, in the host's order.
    mime_types: Vec<String>,
  },
  /// The host asks `source` to write `mime_type` into `fd`.
  Send {
    /// One of Shingle's sources.
    source: S,
    /// The MIME type asked for.
    mime_type: String,
    /// The host reader's descriptor.
    fd: OwnedFd,
  },
  /// `source` no longer holds the host's selection.
  Cancelled {
    /// One of Shingle's sources.
    source: S,
  },
}

/// The bridge's state for one selection.
struct Slot<S, O, Id> {
  kind: SelectionKind,
  /// Shingle's source on the host while it holds the host's selection.
  source: Option<S>,
  /// The nested source last offered to the host, so that each is offered
  /// once.
  mirrored: Option<Id>,
  /// The host's selection installed in nested: its offer and the nested
  /// host source made for it.
  imported: Option<(O, Id)>,
}

/// Keeps the shared selections of nested and its host in step; see the
/// module documentation.
pub(crate) struct Bridge<S, O, Id> {
  slots: Vec<Slot<S, O, Id>>,
}

impl<S: PartialEq, O, Id: Clone + PartialEq> Bridge<S, O, Id> {
  /// A bridge for selections `kinds`; others are left alone.
  pub(crate) fn new(kinds: &[SelectionKind]) -> Self {
    Self {
      slots: kinds
        .iter()
        .map(|&kind| Slot {
          kind,
          source: None,
          mirrored: None,
          imported: None,
        })
        .collect(),
    }
  }

  /// The selections shared.
  #[cfg(test)]
  pub(crate) fn kinds(&self) -> Vec<SelectionKind> {
    self.slots.iter().map(|slot| slot.kind).collect()
  }

  fn slot(&mut self, kind: SelectionKind) -> Option<&mut Slot<S, O, Id>> {
    self.slots.iter_mut().find(|slot| slot.kind == kind)
  }

  /// Applies one host event.
  pub(crate) fn host_event<H, N>(
    &mut self,
    host: &mut H,
    nested: &mut N,
    event: HostEvent<S, O>,
  ) where
    H: HostSide<Source = S, Offer = O>,
    N: NestedSide<Id = Id>,
  {
    match event {
      HostEvent::Selection {
        kind,
        offer,
        mime_types,
      } => {
        let Some(slot) = self.slot(kind) else {
          // A selection not shared, such as primary under
          // `disable_primary_selection`.
          if let Some(offer) = offer {
            host.discard(offer);
          }
          return;
        };
        if slot.source.is_some() {
          // Shingle's own source holds the host's selection, so this is
          // its echo, or a selection it has already replaced.
          if let Some(offer) = offer {
            host.discard(offer);
          }
          return;
        }
        if let Some((previous, _)) = slot.imported.take() {
          host.discard(previous);
        }
        match offer {
          Some(offer) if !mime_types.is_empty() => {
            let id = nested.set_host(kind, mime_types);
            slot.imported = Some((offer, id));
          }
          // An offer of nothing cannot be read: no selection.
          offer => {
            if let Some(offer) = offer {
              host.discard(offer);
            }
            nested.clear_host(kind);
          }
        }
      }
      HostEvent::Send {
        source,
        mime_type,
        fd,
      } => {
        let target = self.slots.iter().find_map(|slot| {
          (slot.source.as_ref() == Some(&source))
            .then(|| slot.mirrored.clone().map(|id| (slot.kind, id)))
            .flatten()
        });
        // A send to a source that no longer mirrors anything closes the
        // reader's descriptor, which ends its transfer.
        if let Some((kind, id)) = target {
          nested.send(kind, &id, mime_type, fd);
        }
      }
      HostEvent::Cancelled { source } => {
        if let Some(slot) = self
          .slots
          .iter_mut()
          .find(|slot| slot.source.as_ref() == Some(&source))
        {
          // `mirrored` stays, so that the nested source is not offered
          // again and the host keeps its new selection.
          slot.source = None;
        }
        host.withdraw(source);
      }
    }
  }

  /// Passes nested's reads of host sources to the host, then offers each
  /// nested selection that changed hands to the host, or withdraws
  /// Shingle's source when nested's selection went.
  pub(crate) fn sync<H, N>(&mut self, host: &mut H, nested: &mut N)
  where
    H: HostSide<Source = S, Offer = O>,
    N: NestedSide<Id = Id>,
  {
    // Reads first: they were made of the host source current when they
    // were made, which a change below may replace.
    for read in nested.take_reads() {
      let Some(slot) = self.slot(read.kind) else {
        continue;
      };
      match &slot.imported {
        Some((offer, id)) if *id == read.source => {
          host.receive(offer, read.mime_type, read.fd);
        }
        // A read of a replaced host source: its descriptor closes.
        _ => {}
      }
    }
    for slot in &mut self.slots {
      match nested.owner(slot.kind) {
        Owner::Local { source, mime_types } if !mime_types.is_empty() => {
          if let Some((offer, _)) = slot.imported.take() {
            host.discard(offer);
          }
          if slot.mirrored.as_ref() == Some(&source) {
            continue;
          }
          slot.mirrored = Some(source);
          // The new source first, so that the host's selection never
          // goes empty in between.
          let offered = host.offer(slot.kind, &mime_types);
          if let Some(previous) = slot.source.replace(offered) {
            host.withdraw(previous);
          }
        }
        Owner::Host(source) => {
          slot.mirrored = None;
          if slot
            .imported
            .as_ref()
            .is_some_and(|(_, imported)| *imported != source)
            && let Some((offer, _)) = slot.imported.take()
          {
            host.discard(offer);
          }
        }
        Owner::None | Owner::Local { .. } => {
          if let Some((offer, _)) = slot.imported.take() {
            host.discard(offer);
          }
          if slot.mirrored.take().is_some()
            && let Some(source) = slot.source.take()
          {
            // The nested selection went, so the host's goes too, as it
            // would with its own client.
            host.withdraw(source);
          }
        }
      }
    }
  }

  /// Lets go of everything when the host's device goes: the host's
  /// selections leave nested, and Shingle's sources and offers are
  /// destroyed.
  pub(crate) fn clear<H, N>(&mut self, host: &mut H, nested: &mut N)
  where
    H: HostSide<Source = S, Offer = O>,
    N: NestedSide<Id = Id>,
  {
    for slot in &mut self.slots {
      if let Some((offer, _)) = slot.imported.take() {
        host.discard(offer);
        nested.clear_host(slot.kind);
      }
      if let Some(source) = slot.source.take() {
        host.withdraw(source);
      }
      slot.mirrored = None;
    }
  }
}

#[cfg(test)]
pub(crate) mod tests;
