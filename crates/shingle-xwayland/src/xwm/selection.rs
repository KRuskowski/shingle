//! The clipboard and primary selection bridge, a port of wlroots'
//! `xwayland/selection/` (selection.c, incoming.c and outgoing.c).
//!
//! The window manager owns one hidden window per bridged selection and
//! asks XFixes to report every owner change of `CLIPBOARD` and `PRIMARY`.
//!
//! - **Wayland to X.** When the compositor reports a Wayland selection
//!   ([`super::Xwm::set_selection`]), the window takes the X selection.
//!   `TARGETS` and `TIMESTAMP` requests are answered at once. A data
//!   request opens a pipe whose write end the compositor hands to the
//!   Wayland source ([`SelectionSend`]); what the source writes goes into
//!   the requestor's property, in [`INCR_CHUNK_SIZE`] chunks with `INCR`
//!   when there is more than one chunk.
//! - **X to Wayland.** When an X client takes the selection, the window
//!   converts `TARGETS` and reports the MIME types
//!   ([`super::XwmEvent::SelectionOffered`]), which the compositor installs
//!   as an X11 source. A Wayland client's request
//!   ([`super::Xwm::receive_selection`]) converts the matching target into
//!   a transfer window of its own and copies the property, or the `INCR`
//!   chunks, into the client's descriptor.
//!
//! Every descriptor is non-blocking. The compositor watches the ones
//! [`super::Xwm::transfer_watches`] lists and calls
//! [`super::Xwm::pump_transfers`] each turn, so a slow writer or reader
//! stalls only its own transfer.

use std::{
  collections::HashMap,
  os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use rustix::io::Errno;
use x11rb::protocol::xproto::{
  EventMask, PropertyNotifyEvent, SelectionNotifyEvent, SelectionRequestEvent,
  Window,
};

use super::{
  atoms::{Atoms, Name},
  connection::{ATOM_TYPE, INTEGER_TYPE, STRING_ATOM, XConnection, XwmError},
  mime,
};

/// wlroots' `INCR_CHUNK_SIZE` (include/xwayland/selection.h): the most
/// written to a property at once, and the size above which a transfer to
/// X uses `INCR`.
pub const INCR_CHUNK_SIZE: usize = 64 * 1024;

/// How much a transfer buffers before it stops reading from its source.
const BUFFER_LIMIT: usize = 2 * INCR_CHUNK_SIZE;

/// The geometry of the selection and transfer windows, as wlroots creates
/// them.
const WINDOW_GEOMETRY: (i16, i16, u16, u16) = (0, 0, 10, 10);

/// A selection the window manager bridges.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Selection {
  /// `CLIPBOARD` and `wl_data_device`'s selection.
  Clipboard,
  /// `PRIMARY` and the primary selection protocol.
  Primary,
}

impl Selection {
  /// The selection's atom.
  fn atom(self, atoms: &Atoms) -> u32 {
    match self {
      Self::Clipboard => atoms.get(Name::CLIPBOARD),
      Self::Primary => atoms.get(Name::PRIMARY),
    }
  }
}

/// An X client asked for the Wayland selection as `mime_type`: the
/// compositor passes `fd` to the Wayland source's `send`, and the window
/// manager copies what arrives on the other end to the X client.
#[derive(Debug)]
pub struct SelectionSend {
  /// The selection asked for.
  pub selection: Selection,
  /// The MIME type of the Wayland source to send.
  pub mime_type: String,
  /// The write end of the transfer's pipe.
  pub fd: OwnedFd,
}

/// A descriptor of a running transfer for the compositor to watch.
#[derive(Debug)]
pub struct TransferWatch<'a> {
  /// The transfer, unique for the window manager's lifetime.
  pub id: u64,
  /// The descriptor.
  pub fd: BorrowedFd<'a>,
  /// Watch for writable rather than readable.
  pub writable: bool,
}

/// One bridged selection.
#[derive(Debug)]
struct Bridged {
  kind: Selection,
  atom: u32,
  /// The window that owns the X selection for Wayland and receives
  /// `TARGETS` from X owners.
  window: Window,
  /// The Wayland selection's MIME types while the window manager owns the
  /// X selection for it.
  offer: Option<Vec<String>>,
  /// The targets of `offer`, for `TARGETS`.
  offer_targets: Vec<u32>,
  /// How often the window manager has taken the selection.
  own_requests: u64,
  /// The server time the window manager's ownership began, for
  /// `TIMESTAMP`.
  timestamp: u32,
  /// Whether an X client owns the selection, as far as XFixes said.
  x_owned: bool,
  /// `own_requests` when the X client took it; a `TARGETS` reply after a
  /// later request of the window manager's own is stale.
  x_generation: u64,
  /// The X owner's MIME types and the target for each.
  x_offer: Vec<(String, u32)>,
}

/// A transfer from a Wayland source to an X requestor.
#[derive(Debug)]
struct Outgoing {
  id: u64,
  selection: u32,
  requestor: Window,
  property: u32,
  target: u32,
  time: u32,
  /// The read end of the pipe the Wayland source writes.
  fd: OwnedFd,
  buffer: Vec<u8>,
  eof: bool,
  /// `INCR` started: the requestor reads chunk by chunk.
  incr: bool,
  /// `INCR`: the requestor deleted the last chunk and waits for another.
  ready: bool,
  /// The window manager does not watch the requestor otherwise, so it
  /// may select `PropertyChange` on it for `INCR`.
  own_mask: bool,
  /// It did, and resets the mask at the end.
  masked: bool,
  done: bool,
}

/// A transfer from an X owner to a Wayland client.
#[derive(Debug)]
struct Incoming {
  id: u64,
  /// The window the selection is converted into.
  window: Window,
  /// The Wayland client's descriptor.
  fd: OwnedFd,
  buffer: Vec<u8>,
  /// `INCR`: chunks follow as new values of the property.
  incr: bool,
  /// `INCR`: a new chunk waits until the buffer has room.
  chunk_pending: bool,
  /// Everything has arrived.
  finished: bool,
  done: bool,
}

/// What reading a descriptor gave.
enum Read {
  Data,
  WouldBlock,
  End,
}

/// Reads into `buffer` until it holds `limit` bytes, the descriptor would
/// block or ends. A read error counts as the end.
fn read_into(fd: BorrowedFd<'_>, buffer: &mut Vec<u8>, limit: usize) -> Read {
  let mut total = 0;
  let mut chunk = [0u8; 16 * 1024];
  while buffer.len() < limit {
    let room = (limit - buffer.len()).min(chunk.len());
    match rustix::io::read(fd, &mut chunk[..room]) {
      Ok(0) => return Read::End,
      Ok(count) => {
        buffer.extend_from_slice(&chunk[..count]);
        total += count;
      }
      Err(Errno::INTR) => {}
      Err(Errno::AGAIN) => break,
      Err(_) => return Read::End,
    }
  }
  if total == 0 {
    Read::WouldBlock
  } else {
    Read::Data
  }
}

/// Writes as much of `buffer` as `fd` takes and removes it. Returns
/// `false` when the reader has gone or writing failed.
fn write_from(fd: BorrowedFd<'_>, buffer: &mut Vec<u8>) -> bool {
  while !buffer.is_empty() {
    match rustix::io::write(fd, buffer) {
      Ok(count) => {
        buffer.drain(..count);
      }
      Err(Errno::INTR) => {}
      Err(Errno::AGAIN) => return true,
      Err(_) => return false,
    }
  }
  true
}

/// The selection bridge's state, owned by the window manager.
#[derive(Debug, Default)]
pub(super) struct Selections {
  bridged: Vec<Bridged>,
  outgoing: Vec<Outgoing>,
  incoming: Vec<Incoming>,
  sends: Vec<SelectionSend>,
  next_id: u64,
  /// Atom names learned from the server, both ways.
  names: HashMap<u32, String>,
  atoms: HashMap<String, u32>,
}

impl Selections {
  /// Creates a window for each of `selections` and selects XFixes owner
  /// events on it. A selection bridged already is left alone.
  pub(super) fn bridge<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selections: &[Selection],
  ) -> Result<(), XwmError> {
    for kind in selections {
      if self.bridged.iter().any(|bridged| bridged.kind == *kind) {
        continue;
      }
      let atom = kind.atom(atoms);
      let window = connection.generate_id()?;
      connection.create_window(window, connection.root(), WINDOW_GEOMETRY)?;
      connection.watch_window(window, EventMask::PROPERTY_CHANGE)?;
      connection.select_selection_input(window, atom)?;
      self.bridged.push(Bridged {
        kind: *kind,
        atom,
        window,
        offer: None,
        offer_targets: Vec::new(),
        own_requests: 0,
        timestamp: x11rb::CURRENT_TIME,
        x_owned: false,
        x_generation: 0,
        x_offer: Vec::new(),
      });
    }
    Ok(())
  }

  /// The selections bridged.
  pub(super) fn bridged(&self) -> Vec<Selection> {
    self.bridged.iter().map(|bridged| bridged.kind).collect()
  }

  /// The window that owns `selection` for Wayland.
  pub(super) fn window(&self, selection: Selection) -> Option<Window> {
    self
      .bridged
      .iter()
      .find(|bridged| bridged.kind == selection)
      .map(|bridged| bridged.window)
  }

  /// Whether `window` is one of the bridge's own windows.
  pub(super) fn owns_window(&self, window: Window) -> bool {
    self.bridged.iter().any(|bridged| bridged.window == window)
      || self
        .incoming
        .iter()
        .any(|incoming| incoming.window == window)
  }

  /// The atoms for target `names`, from what is known and one pipelined
  /// batch for the rest.
  fn intern<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    names: &[String],
  ) -> Result<Vec<u32>, XwmError> {
    let unknown = names
      .iter()
      .filter(|name| self.known_atom(atoms, name).is_none())
      .map(String::as_str)
      .collect::<Vec<_>>();
    if !unknown.is_empty() {
      let values = connection.intern_atoms(&unknown)?;
      for (name, value) in unknown.iter().zip(values) {
        self.atoms.insert((*name).to_owned(), value);
        self.names.insert(value, (*name).to_owned());
      }
    }
    Ok(
      names
        .iter()
        .filter_map(|name| self.known_atom(atoms, name))
        .collect(),
    )
  }

  /// The atom for `name`, when it is predefined, interned at bring-up or
  /// learned since.
  fn known_atom(&self, atoms: &Atoms, name: &str) -> Option<u32> {
    if name == "STRING" {
      return Some(STRING_ATOM);
    }
    atoms
      .by_name(name)
      .or_else(|| self.atoms.get(name).copied())
  }

  /// The names of `values`, from what is known and one pipelined batch
  /// for the rest; `None` for an atom the server does not know.
  fn names_of<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    values: &[u32],
  ) -> Result<Vec<Option<String>>, XwmError> {
    let known = |selections: &Self, value: u32| {
      if value == STRING_ATOM {
        return Some("STRING".to_owned());
      }
      atoms
        .name_of(value)
        .map(str::to_owned)
        .or_else(|| selections.names.get(&value).cloned())
    };
    let unknown = values
      .iter()
      .copied()
      .filter(|value| known(self, *value).is_none())
      .collect::<Vec<_>>();
    if !unknown.is_empty() {
      let names = connection.atom_names(&unknown)?;
      for (value, name) in unknown.iter().zip(names) {
        if let Some(name) = name {
          self.atoms.insert(name.clone(), *value);
          self.names.insert(*value, name);
        }
      }
    }
    Ok(values.iter().map(|value| known(self, *value)).collect())
  }

  /// The Wayland selection changed: with MIME types, the window manager
  /// takes the X selection for it; with `None` (no selection, or one an X
  /// client owns) it gives up an ownership of its own.
  pub(super) fn set_selection<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selection: Selection,
    mime_types: Option<Vec<String>>,
  ) -> Result<(), XwmError> {
    let Some(index) = self
      .bridged
      .iter()
      .position(|bridged| bridged.kind == selection)
    else {
      return Ok(());
    };
    match mime_types {
      Some(mime_types) => {
        let names = mime::targets_for_offer(&mime_types);
        let targets = self.intern(connection, atoms, &names)?;
        let bridged = &mut self.bridged[index];
        bridged.offer = Some(mime_types);
        bridged.offer_targets = targets;
        bridged.own_requests += 1;
        connection.set_selection_owner(bridged.window, bridged.atom)?;
      }
      None => {
        let bridged = &mut self.bridged[index];
        if bridged.offer.take().is_some() && !bridged.x_owned {
          connection.set_selection_owner(x11rb::NONE, bridged.atom)?;
        }
        bridged.offer_targets.clear();
      }
    }
    Ok(())
  }

  /// An XFixes `SelectionNotify`: the owner of a bridged selection
  /// changed. Returns the event for the compositor, if any.
  pub(super) fn owner_changed<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    event: &x11rb::protocol::xfixes::SelectionNotifyEvent,
  ) -> Result<Option<super::XwmEvent>, XwmError> {
    let Some(bridged) = self.bridged.iter_mut().find(|bridged| {
      bridged.atom == event.selection && bridged.window == event.window
    }) else {
      return Ok(None);
    };
    if event.owner == bridged.window {
      bridged.timestamp = event.selection_timestamp;
      bridged.x_owned = false;
      bridged.x_offer.clear();
      return Ok(None);
    }
    if event.owner == x11rb::NONE {
      // The X owner went away, or gave the selection up.
      if std::mem::take(&mut bridged.x_owned) {
        bridged.x_offer.clear();
        return Ok(Some(super::XwmEvent::SelectionCleared {
          selection: bridged.kind,
        }));
      }
      return Ok(None);
    }
    bridged.x_owned = true;
    bridged.x_generation = bridged.own_requests;
    bridged.x_offer.clear();
    connection.convert_selection(
      bridged.window,
      bridged.atom,
      atoms.get(Name::TARGETS),
      atoms.get(Name::WL_SELECTION),
      event.timestamp,
    )?;
    Ok(None)
  }

  /// A `SelectionNotify` for one of the bridge's windows: the `TARGETS`
  /// of a new X owner, or the start of an incoming transfer. Returns
  /// whether it was the bridge's, and the event for the compositor, if
  /// any.
  pub(super) fn selection_notify<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    event: &SelectionNotifyEvent,
  ) -> Result<(bool, Option<super::XwmEvent>), XwmError> {
    if let Some(index) = self
      .bridged
      .iter()
      .position(|bridged| bridged.window == event.requestor)
    {
      let offered = self.targets_reply(connection, atoms, index, event)?;
      return Ok((true, offered));
    }
    let Some(incoming) = self
      .incoming
      .iter_mut()
      .find(|incoming| incoming.window == event.requestor)
    else {
      return Ok((false, None));
    };
    if event.property == x11rb::NONE {
      // The owner could not convert it.
      incoming.finished = true;
      return Ok((true, None));
    }
    let property = atoms.get(Name::WL_SELECTION);
    match connection.take_property(incoming.window, property)? {
      Some(value) if value.kind == atoms.get(Name::INCR) => {
        // Deleting the property, which taking it did, starts INCR.
        incoming.incr = true;
      }
      Some(value) => {
        incoming.buffer.extend_from_slice(&value.value);
        incoming.finished = true;
      }
      None => incoming.finished = true,
    }
    Ok((true, None))
  }

  /// The reply to the `TARGETS` conversion for bridged selection `index`.
  fn targets_reply<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    index: usize,
    event: &SelectionNotifyEvent,
  ) -> Result<Option<super::XwmEvent>, XwmError> {
    let bridged = &self.bridged[index];
    if event.target != atoms.get(Name::TARGETS) || event.property == x11rb::NONE
    {
      return Ok(None);
    }
    let window = bridged.window;
    let stale =
      !bridged.x_owned || bridged.x_generation != bridged.own_requests;
    let value =
      connection.take_property(window, atoms.get(Name::WL_SELECTION))?;
    if stale {
      return Ok(None);
    }
    let Some(value) = value.filter(|value| value.kind == ATOM_TYPE) else {
      return Ok(None);
    };
    let values = value.values32();
    let names = self.names_of(connection, atoms, &values)?;
    let targets = values
      .into_iter()
      .zip(names)
      .filter_map(|(value, name)| Some((value, name?)))
      .collect::<Vec<_>>();
    let offer = mime::offer_from_targets(&targets);
    let bridged = &mut self.bridged[index];
    bridged.x_offer = offer.clone();
    Ok(Some(super::XwmEvent::SelectionOffered {
      selection: bridged.kind,
      mime_types: offer.into_iter().map(|(mime_type, _)| mime_type).collect(),
    }))
  }

  /// A `SelectionRequest` from an X client for a selection the window
  /// manager owns for Wayland, wlroots' `xwm_handle_selection_request`.
  /// `tracked` says whether the window manager watches a window already.
  /// Returns whether it was the bridge's.
  pub(super) fn selection_request<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    event: &SelectionRequestEvent,
    tracked: &dyn Fn(Window) -> bool,
  ) -> Result<bool, XwmError> {
    let Some(index) = self.bridged.iter().position(|bridged| {
      bridged.window == event.owner && bridged.atom == event.selection
    }) else {
      return Ok(false);
    };
    // Obsolete clients leave the property `None` (ICCCM 2.2).
    let property = if event.property == x11rb::NONE {
      event.target
    } else {
      event.property
    };
    let notify = |connection: &mut C, property: u32| {
      connection.send_selection_notify(
        event.requestor,
        event.selection,
        event.target,
        property,
        event.time,
      )
    };
    let bridged = &self.bridged[index];
    let Some(offer) = bridged.offer.clone() else {
      notify(connection, x11rb::NONE)?;
      return Ok(true);
    };
    if event.target == atoms.get(Name::TARGETS) {
      connection.change_property32(
        event.requestor,
        property,
        ATOM_TYPE,
        &bridged.offer_targets,
      )?;
      notify(connection, property)?;
      return Ok(true);
    }
    if event.target == atoms.get(Name::TIMESTAMP) {
      connection.change_property32(
        event.requestor,
        property,
        INTEGER_TYPE,
        &[bridged.timestamp],
      )?;
      notify(connection, property)?;
      return Ok(true);
    }
    if event.target == atoms.get(Name::DELETE) {
      notify(connection, property)?;
      return Ok(true);
    }
    let kind = bridged.kind;
    let name = self
      .names_of(connection, atoms, &[event.target])?
      .pop()
      .flatten();
    let Some(mime_type) =
      name.and_then(|name| mime::mime_for_request(&name, &offer))
    else {
      notify(connection, x11rb::NONE)?;
      return Ok(true);
    };
    let Ok((read, write)) =
      rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
    else {
      notify(connection, x11rb::NONE)?;
      return Ok(true);
    };
    if rustix::io::ioctl_fionbio(&read, true).is_err() {
      notify(connection, x11rb::NONE)?;
      return Ok(true);
    }
    let own_mask = !tracked(event.requestor);
    self.next_id += 1;
    self.outgoing.push(Outgoing {
      id: self.next_id,
      selection: event.selection,
      requestor: event.requestor,
      property,
      target: event.target,
      time: event.time,
      fd: read,
      buffer: Vec::new(),
      eof: false,
      incr: false,
      ready: false,
      own_mask,
      masked: false,
      done: false,
    });
    self.sends.push(SelectionSend {
      selection: kind,
      mime_type,
      fd: write,
    });
    Ok(true)
  }

  /// A `PropertyNotify`: an `INCR` requestor deleted a chunk, or an `INCR`
  /// owner stored one. Returns whether it was the bridge's.
  pub(super) fn property_notify<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    event: &PropertyNotifyEvent,
  ) -> Result<bool, XwmError> {
    use x11rb::protocol::xproto::Property;
    let mut handled = false;
    if event.state == Property::DELETE {
      for outgoing in &mut self.outgoing {
        if outgoing.incr
          && outgoing.requestor == event.window
          && outgoing.property == event.atom
        {
          outgoing.ready = true;
          handled = true;
        }
      }
    }
    if let Some(incoming) = self
      .incoming
      .iter_mut()
      .find(|incoming| incoming.window == event.window)
    {
      handled = true;
      if event.state == Property::NEW_VALUE
        && event.atom == atoms.get(Name::WL_SELECTION)
        && incoming.incr
        && !incoming.finished
      {
        incoming.chunk_pending = true;
        read_chunk(connection, atoms, incoming)?;
      }
    }
    if handled {
      self.pump(connection, atoms)?;
    }
    Ok(handled)
  }

  /// A request failed with `BadWindow` for `window`: an outgoing transfer
  /// to it ends, since its requestor has gone.
  pub(super) fn window_gone(&mut self, window: Window) {
    for outgoing in &mut self.outgoing {
      if outgoing.requestor == window {
        outgoing.done = true;
        outgoing.masked = false;
      }
    }
    self.outgoing.retain(|outgoing| !outgoing.done);
  }

  /// A Wayland client wants the X owner's `selection` as `mime_type` in
  /// `fd`. The matching target is converted into a new transfer window;
  /// with no match, `fd` is closed at once, which ends the client's read.
  pub(super) fn receive<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selection: Selection,
    mime_type: &str,
    fd: OwnedFd,
  ) -> Result<(), XwmError> {
    let Some(bridged) = self
      .bridged
      .iter()
      .find(|bridged| bridged.kind == selection)
    else {
      return Ok(());
    };
    let Some(target) = bridged
      .x_offer
      .iter()
      .find(|(offered, _)| offered == mime_type)
      .map(|(_, target)| *target)
    else {
      return Ok(());
    };
    if rustix::io::ioctl_fionbio(&fd, true).is_err() {
      return Ok(());
    }
    let selection_atom = bridged.atom;
    let window = connection.generate_id()?;
    connection.create_window(window, connection.root(), WINDOW_GEOMETRY)?;
    connection.watch_window(window, EventMask::PROPERTY_CHANGE)?;
    connection.convert_selection(
      window,
      selection_atom,
      target,
      atoms.get(Name::WL_SELECTION),
      x11rb::CURRENT_TIME,
    )?;
    self.next_id += 1;
    self.incoming.push(Incoming {
      id: self.next_id,
      window,
      fd,
      buffer: Vec::new(),
      incr: false,
      chunk_pending: false,
      finished: false,
      done: false,
    });
    Ok(())
  }

  /// The data requests for Wayland sources that X clients made since the
  /// last call.
  pub(super) fn take_sends(&mut self) -> Vec<SelectionSend> {
    std::mem::take(&mut self.sends)
  }

  /// Moves data on every transfer as far as its descriptors allow without
  /// blocking, and ends the transfers that are done.
  pub(super) fn pump<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
  ) -> Result<(), XwmError> {
    for outgoing in &mut self.outgoing {
      pump_outgoing(connection, atoms, outgoing)?;
    }
    for incoming in &mut self.incoming {
      pump_incoming(connection, atoms, incoming)?;
    }
    let mut result = Ok(());
    let mut finished = Vec::new();
    self.outgoing.retain(|outgoing| {
      if outgoing.done && outgoing.masked {
        finished.push(outgoing.requestor);
      }
      !outgoing.done
    });
    for requestor in finished {
      // Another INCR transfer to the same requestor still needs it.
      if !self
        .outgoing
        .iter()
        .any(|other| other.requestor == requestor && other.masked)
      {
        result = result.and_then(|()| {
          connection.watch_window(requestor, EventMask::NO_EVENT)
        });
      }
    }
    for incoming in self.incoming.iter().filter(|incoming| incoming.done) {
      result = result.and_then(|()| connection.destroy_window(incoming.window));
    }
    self.incoming.retain(|incoming| !incoming.done);
    result
  }

  /// The descriptors of running transfers that can make progress once
  /// they are ready: outgoing pipes with room in their buffer, incoming
  /// descriptors with data to write.
  pub(super) fn watches(&self) -> Vec<TransferWatch<'_>> {
    let outgoing = self
      .outgoing
      .iter()
      .filter(|outgoing| {
        !outgoing.done && !outgoing.eof && outgoing.buffer.len() < BUFFER_LIMIT
      })
      .map(|outgoing| TransferWatch {
        id: outgoing.id,
        fd: outgoing.fd.as_fd(),
        writable: false,
      });
    let incoming = self
      .incoming
      .iter()
      .filter(|incoming| !incoming.done && !incoming.buffer.is_empty())
      .map(|incoming| TransferWatch {
        id: incoming.id,
        fd: incoming.fd.as_fd(),
        writable: true,
      });
    outgoing.chain(incoming).collect()
  }

  /// How many transfers run, outgoing and incoming.
  pub(super) fn transfers(&self) -> (usize, usize) {
    (self.outgoing.len(), self.incoming.len())
  }
}

/// Reads an `INCR` chunk the owner stored, when the buffer has room for
/// it; an empty chunk ends the transfer.
fn read_chunk<C: XConnection>(
  connection: &mut C,
  atoms: &Atoms,
  incoming: &mut Incoming,
) -> Result<(), XwmError> {
  if !incoming.chunk_pending || incoming.buffer.len() >= INCR_CHUNK_SIZE {
    return Ok(());
  }
  incoming.chunk_pending = false;
  let property = atoms.get(Name::WL_SELECTION);
  match connection.take_property(incoming.window, property)? {
    Some(value) if !value.value.is_empty() => {
      incoming.buffer.extend_from_slice(&value.value);
    }
    _ => incoming.finished = true,
  }
  Ok(())
}

/// Writes what an incoming transfer holds to the Wayland client, reads
/// the next `INCR` chunk once there is room, and ends the transfer when
/// everything is written or the client has gone.
fn pump_incoming<C: XConnection>(
  connection: &mut C,
  atoms: &Atoms,
  incoming: &mut Incoming,
) -> Result<(), XwmError> {
  loop {
    if !write_from(incoming.fd.as_fd(), &mut incoming.buffer) {
      incoming.done = true;
      return Ok(());
    }
    if !incoming.buffer.is_empty() {
      return Ok(());
    }
    if incoming.finished {
      incoming.done = true;
      return Ok(());
    }
    if !incoming.chunk_pending {
      return Ok(());
    }
    read_chunk(connection, atoms, incoming)?;
  }
}

/// Reads what the Wayland source wrote and hands it to the X requestor:
/// the whole of it in one property when it ends within one chunk, else
/// through `INCR`, one chunk each time the requestor deletes the last.
fn pump_outgoing<C: XConnection>(
  connection: &mut C,
  atoms: &Atoms,
  outgoing: &mut Outgoing,
) -> Result<(), XwmError> {
  if outgoing.done {
    return Ok(());
  }
  if !outgoing.eof
    && matches!(
      read_into(outgoing.fd.as_fd(), &mut outgoing.buffer, BUFFER_LIMIT),
      Read::End
    )
  {
    outgoing.eof = true;
  }
  if !outgoing.incr {
    if outgoing.eof && outgoing.buffer.len() <= INCR_CHUNK_SIZE {
      connection.change_property8(
        outgoing.requestor,
        outgoing.property,
        outgoing.target,
        &outgoing.buffer,
      )?;
      notify_outgoing(connection, outgoing, outgoing.property)?;
      outgoing.done = true;
      return Ok(());
    }
    if outgoing.buffer.len() <= INCR_CHUNK_SIZE {
      return Ok(());
    }
    if outgoing.own_mask {
      connection
        .watch_window(outgoing.requestor, EventMask::PROPERTY_CHANGE)?;
      outgoing.masked = true;
    }
    let size = u32::try_from(outgoing.buffer.len()).unwrap_or(u32::MAX);
    connection.change_property32(
      outgoing.requestor,
      outgoing.property,
      atoms.get(Name::INCR),
      &[size],
    )?;
    notify_outgoing(connection, outgoing, outgoing.property)?;
    outgoing.incr = true;
    return Ok(());
  }
  if !outgoing.ready {
    return Ok(());
  }
  if !outgoing.buffer.is_empty() {
    let length = outgoing.buffer.len().min(INCR_CHUNK_SIZE);
    connection.change_property8(
      outgoing.requestor,
      outgoing.property,
      outgoing.target,
      &outgoing.buffer[..length],
    )?;
    outgoing.buffer.drain(..length);
    outgoing.ready = false;
  } else if outgoing.eof {
    // The empty chunk that ends INCR.
    connection.change_property8(
      outgoing.requestor,
      outgoing.property,
      outgoing.target,
      &[],
    )?;
    outgoing.ready = false;
    outgoing.done = true;
  }
  Ok(())
}

/// Sends an outgoing transfer's requestor its `SelectionNotify`.
fn notify_outgoing<C: XConnection>(
  connection: &mut C,
  outgoing: &Outgoing,
  property: u32,
) -> Result<(), XwmError> {
  connection.send_selection_notify(
    outgoing.requestor,
    outgoing.selection,
    outgoing.target,
    property,
    outgoing.time,
  )
}
