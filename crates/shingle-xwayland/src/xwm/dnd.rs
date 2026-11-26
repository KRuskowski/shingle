//! Drag and drop between X and Wayland over Xdnd version 5
//! (<https://freedesktop.org/wiki/Specifications/XDND/>).
//!
//! - **Wayland to X** is a port of wlroots' `xwayland/selection/dnd.c`.
//!   When a Wayland drag starts, the window manager's `XdndSelection`
//!   window takes the selection for the drag source's MIME types
//!   ([`super::selection`] serves the X target's conversions). While the
//!   drag is over an X window that is `XdndAware`, the window manager is
//!   the Xdnd source: it sends `XdndEnter`, `XdndPosition` (one at a time,
//!   each waiting for its `XdndStatus`), `XdndLeave` and `XdndDrop`, and
//!   reports the target's `XdndStatus` and `XdndFinished` as
//!   [`XwmEvent::DragStatus`] and [`XwmEvent::DragFinished`]. Unlike
//!   wlroots, it checks `XdndAware` and follows `XdndProxy` before it sends
//!   anything, as the specification and smithay's `xwm/dnd.rs` do.
//! - **X to Wayland** has no wlroots counterpart (its `incoming.c` leaves
//!   `XdndSelection` as a TODO); it follows smithay. When an X client takes
//!   `XdndSelection` ([`XwmEvent::DragOwnerChanged`]) and the compositor
//!   starts a Wayland drag for it, the window manager maps an `InputOnly`
//!   proxy window over the whole screen, above every X window, whenever
//!   the drag is not over an X window, so that the X source finds an
//!   `XdndAware` window under the pointer and sends its messages there.
//!   The source's types and actions become the Wayland drag's
//!   ([`XwmEvent::DragOffered`], [`XwmEvent::DragSourceAction`]); every
//!   `XdndPosition` is answered with the Wayland target's verdict, and a
//!   change of verdict is sent at once. `XdndDrop` becomes
//!   [`XwmEvent::DragDropped`]; the data is converted from `XdndSelection`
//!   at the drop's time, and [`super::Xwm::x11_drag_end`] sends
//!   `XdndFinished`.

use x11rb::protocol::xproto::{ClientMessageEvent, Window};

use super::{
  XwmEvent,
  atoms::{Atoms, Name},
  connection::{ATOM_TYPE, WINDOW_TYPE, XConnection, XwmError},
  mime,
  selection::{Selection, Selections},
};

/// The Xdnd version the window manager speaks, wlroots' `XDND_VERSION`.
pub const XDND_VERSION: u32 = 5;

/// The oldest version of a target the window manager talks to: version 3
/// is the first whose `XdndPosition` carries a time and an action.
const MIN_VERSION: u32 = 3;

/// The proxy covers this much of the root, as wlroots' `XdndSelection`
/// window does.
const PROXY_GEOMETRY: (i16, i16, u16, u16) = (0, 0, 8192, 8192);

/// `XdndEnter`'s flag: the types are in `XdndTypeList`, not the message.
const MORE_TYPES: u32 = 1;

/// `XdndStatus`'s flags: the target accepts the drop, and wants an
/// `XdndPosition` for every motion.
const STATUS_ACCEPT: u32 = 1;
const STATUS_WANT_POSITIONS: u32 = 2;

/// An Xdnd action, and the `wl_data_device_manager` action it maps to.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum DndAction {
  /// No action: the target does not accept the drop.
  #[default]
  None,
  /// `XdndActionCopy`.
  Copy,
  /// `XdndActionMove`.
  Move,
  /// `XdndActionAsk`.
  Ask,
}

impl DndAction {
  /// The action's atom, or `None` (0).
  fn atom(self, atoms: &Atoms) -> u32 {
    match self {
      Self::None => x11rb::NONE,
      Self::Copy => atoms.get(Name::XDND_ACTION_COPY),
      Self::Move => atoms.get(Name::XDND_ACTION_MOVE),
      Self::Ask => atoms.get(Name::XDND_ACTION_ASK),
    }
  }

  /// The action for `atom`, wlroots'
  /// `data_device_manager_dnd_action_from_atom`: `XdndActionPrivate`
  /// counts as copy, and an unknown atom as none.
  pub fn from_atom(atoms: &Atoms, atom: u32) -> Self {
    if atom == atoms.get(Name::XDND_ACTION_COPY)
      || atom == atoms.get(Name::XDND_ACTION_PRIVATE)
    {
      Self::Copy
    } else if atom == atoms.get(Name::XDND_ACTION_MOVE) {
      Self::Move
    } else if atom == atoms.get(Name::XDND_ACTION_ASK) {
      Self::Ask
    } else {
      Self::None
    }
  }
}

/// The actions a Wayland drag source offers, as `wl_data_device_manager`
/// encodes them: copy 1, move 2, ask 4.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct DndActions(u32);

impl DndActions {
  /// Copy.
  pub const COPY: Self = Self(1);
  /// Move.
  pub const MOVE: Self = Self(2);
  /// Ask.
  pub const ASK: Self = Self(4);

  /// The actions in `wl_data_device_manager`'s bits; unknown bits are
  /// dropped.
  pub fn from_bits(bits: u32) -> Self {
    Self(bits & 7)
  }

  /// The `wl_data_device_manager` bits.
  pub fn bits(self) -> u32 {
    self.0
  }

  /// Whether `action` is among these.
  pub fn contains(self, action: DndAction) -> bool {
    let bit = match action {
      DndAction::None => return false,
      DndAction::Copy => Self::COPY,
      DndAction::Move => Self::MOVE,
      DndAction::Ask => Self::ASK,
    };
    self.0 & bit.0 != 0
  }

  /// The one action an Xdnd source proposes for these, wlroots'
  /// `data_device_manager_dnd_action_to_atom`: copy, else move, else ask.
  pub fn proposed(self) -> DndAction {
    [DndAction::Copy, DndAction::Move, DndAction::Ask]
      .into_iter()
      .find(|action| self.contains(*action))
      .unwrap_or_default()
  }
}

impl std::ops::BitOr for DndActions {
  type Output = Self;

  fn bitor(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }
}

/// A position, in root coordinates, with the server time of the motion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Position {
  x: i32,
  y: i32,
  time: u32,
}

impl Position {
  /// `XdndPosition`'s packed root coordinates, `x << 16 | y`.
  fn packed(self) -> u32 {
    let x = self.x.clamp(0, i32::from(u16::MAX)) as u32;
    let y = self.y.clamp(0, i32::from(u16::MAX)) as u32;
    (x << 16) | y
  }
}

/// The X window a Wayland drag is over.
#[derive(Clone, Debug)]
struct Target {
  /// The window, which the messages name.
  window: Window,
  /// Where the messages go: the window, or its `XdndProxy`.
  destination: Window,
  /// The version both sides speak; `None` when the window is not
  /// `XdndAware`, or too old, and nothing is sent to it.
  version: Option<u32>,
  /// An `XdndPosition` waits for its `XdndStatus`.
  waiting: bool,
  /// The latest position while one waits.
  queued: Option<Position>,
  /// The position sent last.
  sent: Option<Position>,
  /// The target's verdict from its last `XdndStatus`.
  accepted: bool,
  action: DndAction,
}

/// A drag from a Wayland source.
#[derive(Clone, Debug)]
struct WaylandDrag {
  /// The source's types as atoms, for `XdndEnter` and `XdndTypeList`.
  types: Vec<u32>,
  /// The action proposed in `XdndPosition`.
  action: DndAction,
  /// The X window under the pointer.
  target: Option<Target>,
  /// The X window the drop went to, until its `XdndFinished`.
  dropped: Option<Window>,
}

/// A drag from an X source.
#[derive(Clone, Debug)]
struct XDrag {
  /// The source window `XdndEnter` named; `None` while the source is not
  /// over the proxy.
  source: Option<Window>,
  /// The version the source speaks.
  version: u32,
  /// The source's types, with the target to convert for each.
  offer: Vec<(String, u32)>,
  /// The time of the last `XdndPosition`, or of the drop.
  time: u32,
  /// The action the source proposes.
  action: DndAction,
  /// The compositor's verdict, from the Wayland target.
  answer: (bool, DndAction),
  /// The verdict last sent in an `XdndStatus`.
  answered: Option<(bool, DndAction)>,
  /// `XdndDrop` arrived.
  dropped: bool,
}

/// The drag and drop bridge's state, owned by the window manager.
#[derive(Debug, Default)]
pub(super) struct Dnd {
  /// The window that owns `XdndSelection` for Wayland and is the source
  /// the messages to X targets name.
  window: Option<Window>,
  /// The `InputOnly` window an X source finds over Wayland surfaces.
  proxy: Option<Window>,
  proxy_mapped: bool,
  /// The X client window that owns `XdndSelection`, as XFixes said.
  owner: Option<Window>,
  wayland: Option<WaylandDrag>,
  x11: Option<XDrag>,
}

/// Sends a format-32 Xdnd message of type `kind` to `destination`, about
/// `window`.
fn send<C: XConnection>(
  connection: &mut C,
  destination: Window,
  kind: u32,
  data: [u32; 5],
) -> Result<(), XwmError> {
  connection.send_client_message(destination, kind, data)
}

impl Dnd {
  /// Bridges `XdndSelection`: its selection window, made `XdndAware` as
  /// wlroots makes it, and the proxy, created unmapped.
  pub(super) fn bridge<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selections: &mut Selections,
  ) -> Result<(), XwmError> {
    if self.window.is_some() {
      return Ok(());
    }
    selections.bridge(connection, atoms, &[Selection::Dnd])?;
    let Some(window) = selections.window(Selection::Dnd) else {
      return Ok(());
    };
    let aware = atoms.get(Name::XDND_AWARE);
    connection.change_property32(window, aware, ATOM_TYPE, &[XDND_VERSION])?;
    let proxy = connection.generate_id()?;
    connection.create_input_window(proxy, connection.root(), PROXY_GEOMETRY)?;
    connection.change_property32(proxy, aware, ATOM_TYPE, &[XDND_VERSION])?;
    self.window = Some(window);
    self.proxy = Some(proxy);
    Ok(())
  }

  /// The `XdndSelection` window and the proxy.
  pub(super) fn windows(&self) -> (Option<Window>, Option<Window>) {
    (self.window, self.proxy)
  }

  /// Whether `window` is the proxy, which the window manager does not
  /// manage.
  pub(super) fn owns_window(&self, window: Window) -> bool {
    self.proxy == Some(window)
  }

  /// Whether the proxy is mapped.
  pub(super) fn proxy_mapped(&self) -> bool {
    self.proxy_mapped
  }

  /// XFixes reported a new owner of `XdndSelection`: an X client's
  /// window, or none. A drag from an earlier owner ends.
  pub(super) fn owner_changed<C: XConnection>(
    &mut self,
    connection: &mut C,
    owner: Option<Window>,
  ) -> Result<(), XwmError> {
    self.owner = owner;
    if self.x11.is_some() {
      self.x11 = None;
      self.map_proxy(connection, false)?;
    }
    Ok(())
  }

  /// Starts a drag from a Wayland source offering `mime_types`: the
  /// selection window takes `XdndSelection` for them, as wlroots'
  /// `seat_handle_start_drag` does. A drag already running ends first.
  pub(super) fn wayland_start<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selections: &mut Selections,
    mime_types: Vec<String>,
    actions: DndActions,
  ) -> Result<(), XwmError> {
    self.wayland_end(connection, atoms, selections)?;
    if self.window.is_none() {
      return Ok(());
    }
    let mut names = Vec::<String>::new();
    for mime_type in &mime_types {
      // The first X name of each type, as wlroots'
      // `xwm_mime_type_to_atom` gives it.
      let name = mime::targets_for_mime(mime_type)[0].to_owned();
      if !names.contains(&name) {
        names.push(name);
      }
    }
    let types = selections.intern(connection, atoms, &names)?;
    selections.set_selection(
      connection,
      atoms,
      Selection::Dnd,
      Some(mime_types),
    )?;
    self.wayland = Some(WaylandDrag {
      types,
      action: actions.proposed(),
      target: None,
      dropped: None,
    });
    Ok(())
  }

  /// Follows a Wayland drag to `target`, the X window under the pointer,
  /// or none, at root position (`x`, `y`) and server time `time`. Leaving
  /// a window sends it `XdndLeave` and reports that nothing accepts the
  /// drag; entering an `XdndAware` one sends `XdndEnter`; and every
  /// motion over one sends an `XdndPosition`, or keeps it until the last
  /// one's `XdndStatus`.
  #[allow(clippy::too_many_arguments)]
  pub(super) fn wayland_motion<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    target: Option<Window>,
    x: i32,
    y: i32,
    time: u32,
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    let Some(source) = self.window else {
      return Ok(());
    };
    let Some(drag) = &mut self.wayland else {
      return Ok(());
    };
    if drag.dropped.is_some() {
      return Ok(());
    }
    if drag.target.as_ref().map(|target| target.window) != target {
      if let Some(old) = drag.target.take() {
        leave(connection, atoms, source, &old)?;
        if old.accepted || old.action != DndAction::None {
          events.push(XwmEvent::DragStatus {
            accepted: false,
            action: DndAction::None,
          });
        }
      }
      if let Some(window) = target {
        let entered = enter(connection, atoms, source, drag, window)?;
        drag.target = Some(entered);
      }
    }
    let action = drag.action;
    let Some(target) = &mut drag.target else {
      return Ok(());
    };
    if target.version.is_none() {
      return Ok(());
    }
    let position = Position { x, y, time };
    if target.waiting {
      target.queued = Some(position);
      return Ok(());
    }
    if target.sent.is_some_and(|sent| (sent.x, sent.y) == (x, y)) {
      return Ok(());
    }
    send_position(connection, atoms, source, target, position, action)
  }

  /// The Wayland drag was dropped at server time `time`. Over an X window
  /// that accepted it, the window gets `XdndDrop` and the drop waits for
  /// its `XdndFinished`; otherwise the window gets `XdndLeave`. Returns
  /// whether the drop was sent.
  pub(super) fn wayland_drop<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    time: u32,
  ) -> Result<bool, XwmError> {
    let Some(source) = self.window else {
      return Ok(false);
    };
    let Some(drag) = &mut self.wayland else {
      return Ok(false);
    };
    let Some(target) = drag.target.take() else {
      return Ok(false);
    };
    if target.version.is_none() {
      return Ok(false);
    }
    if !target.accepted {
      leave(connection, atoms, source, &target)?;
      return Ok(false);
    }
    send(
      connection,
      target.destination,
      atoms.get(Name::XDND_DROP),
      [source, 0, time, 0, 0],
    )?;
    drag.dropped = Some(target.window);
    Ok(true)
  }

  /// Ends the Wayland drag: an X window it is still over gets
  /// `XdndLeave`, and the selection window gives up `XdndSelection`.
  pub(super) fn wayland_end<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selections: &mut Selections,
  ) -> Result<(), XwmError> {
    let Some(drag) = self.wayland.take() else {
      return Ok(());
    };
    if let (Some(source), Some(target)) = (self.window, &drag.target) {
      leave(connection, atoms, source, target)?;
    }
    selections.set_selection(connection, atoms, Selection::Dnd, None)
  }

  /// Whether a Wayland drag runs.
  pub(super) fn wayland_active(&self) -> bool {
    self.wayland.is_some()
  }

  /// Updates the X drag for this turn: the proxy is mapped while the drag
  /// is not over an X window, and `accepted` and `action` are the Wayland
  /// target's verdict, which the source gets with the next `XdndStatus`,
  /// or at once when it changed. The first call starts the drag of the
  /// current `XdndSelection` owner; without one it does nothing.
  pub(super) fn x11_update<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    proxy: bool,
    accepted: bool,
    action: DndAction,
  ) -> Result<(), XwmError> {
    if self.owner.is_none() {
      return Ok(());
    }
    let drag = self.x11.get_or_insert_with(|| XDrag {
      source: None,
      version: XDND_VERSION,
      offer: Vec::new(),
      time: x11rb::CURRENT_TIME,
      action: DndAction::Copy,
      answer: (false, DndAction::None),
      answered: None,
      dropped: false,
    });
    let action = if accepted { action } else { DndAction::None };
    drag.answer = (accepted, action);
    if !drag.dropped
      && drag.source.is_some()
      && drag
        .answered
        .is_some_and(|answered| answered != drag.answer)
    {
      self.send_status(connection, atoms)?;
    }
    let dropped = self.x11.as_ref().is_some_and(|drag| drag.dropped);
    self.map_proxy(connection, proxy && !dropped)
  }

  /// Whether an X drag runs.
  pub(super) fn x11_active(&self) -> bool {
    self.x11.is_some()
  }

  /// Ends the X drag: after a drop, the source gets `XdndFinished`,
  /// performed with `action` when it is `Some` and refused otherwise. The
  /// proxy is unmapped.
  pub(super) fn x11_end<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    performed: Option<DndAction>,
  ) -> Result<(), XwmError> {
    let Some(drag) = self.x11.take() else {
      return Ok(());
    };
    if let (Some(source), Some(proxy), true) =
      (drag.source, self.proxy, drag.dropped)
    {
      let (flags, action) = match performed {
        Some(action) => (1, action.atom(atoms)),
        None => (0, x11rb::NONE),
      };
      send(
        connection,
        source,
        atoms.get(Name::XDND_FINISHED),
        [proxy, flags, action, 0, 0],
      )?;
    }
    self.map_proxy(connection, false)
  }

  /// Maps the proxy above every X window, or unmaps it.
  fn map_proxy<C: XConnection>(
    &mut self,
    connection: &mut C,
    mapped: bool,
  ) -> Result<(), XwmError> {
    let Some(proxy) = self.proxy else {
      return Ok(());
    };
    if self.proxy_mapped == mapped {
      return Ok(());
    }
    self.proxy_mapped = mapped;
    if mapped {
      connection.map_window(proxy)?;
      connection.stack_above(proxy, None)
    } else {
      connection.unmap_window(proxy)
    }
  }

  /// Sends the X source an `XdndStatus` with the compositor's verdict.
  fn send_status<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
  ) -> Result<(), XwmError> {
    let (Some(drag), Some(proxy)) = (&mut self.x11, self.proxy) else {
      return Ok(());
    };
    let Some(source) = drag.source else {
      return Ok(());
    };
    let (accepted, action) = drag.answer;
    let accept = if accepted { STATUS_ACCEPT } else { 0 };
    let flags = STATUS_WANT_POSITIONS | accept;
    drag.answered = Some(drag.answer);
    send(
      connection,
      source,
      atoms.get(Name::XDND_STATUS),
      [proxy, flags, 0, 0, action.atom(atoms)],
    )
  }

  /// Handles an Xdnd client message, if `event` is one: from an X target
  /// to the selection window, or from an X source to the proxy. Returns
  /// whether it was.
  pub(super) fn client_message<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selections: &mut Selections,
    event: &ClientMessageEvent,
    events: &mut Vec<XwmEvent>,
  ) -> Result<bool, XwmError> {
    let kind = event.type_;
    let data = event.data.as_data32();
    let is = |name| kind == atoms.get(name);
    let known = [
      Name::XDND_ENTER,
      Name::XDND_POSITION,
      Name::XDND_LEAVE,
      Name::XDND_DROP,
      Name::XDND_STATUS,
      Name::XDND_FINISHED,
    ];
    if !known.into_iter().any(is) {
      return Ok(false);
    }
    if event.format != 32 {
      return Ok(true);
    }
    if Some(event.window) == self.window {
      if is(Name::XDND_STATUS) {
        self.wayland_status(connection, atoms, data, events)?;
      } else if is(Name::XDND_FINISHED) {
        self.wayland_finished(atoms, data, events);
      }
    } else if Some(event.window) == self.proxy {
      if is(Name::XDND_ENTER) {
        self.x11_enter(connection, atoms, selections, data, events)?;
      } else if is(Name::XDND_POSITION) {
        self.x11_position(connection, atoms, data, events)?;
      } else if is(Name::XDND_LEAVE) {
        self.x11_leave(data, events);
      } else if is(Name::XDND_DROP) {
        self.x11_drop(selections, data, events);
      }
    }
    Ok(true)
  }

  /// `XdndStatus` from the X target of a Wayland drag: its verdict, and
  /// room for the next `XdndPosition`.
  fn wayland_status<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    data: [u32; 5],
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    let Some(source) = self.window else {
      return Ok(());
    };
    let Some(drag) = &mut self.wayland else {
      return Ok(());
    };
    let action = drag.action;
    // As wlroots: a status from anything but the current target is
    // ignored.
    let Some(target) = drag
      .target
      .as_mut()
      .filter(|target| target.window == data[0])
    else {
      return Ok(());
    };
    target.waiting = false;
    let accepted = data[1] & STATUS_ACCEPT != 0;
    let verdict = if accepted {
      DndAction::from_atom(atoms, data[4])
    } else {
      DndAction::None
    };
    if (target.accepted, target.action) != (accepted, verdict) {
      target.accepted = accepted;
      target.action = verdict;
      events.push(XwmEvent::DragStatus {
        accepted,
        action: verdict,
      });
    }
    if let Some(position) = target.queued.take() {
      send_position(connection, atoms, source, target, position, action)?;
    }
    Ok(())
  }

  /// `XdndFinished` from the X window a Wayland drag was dropped on.
  fn wayland_finished(
    &mut self,
    atoms: &Atoms,
    data: [u32; 5],
    events: &mut Vec<XwmEvent>,
  ) {
    let Some(drag) = &mut self.wayland else {
      return;
    };
    if drag.dropped != Some(data[0]) {
      return;
    }
    drag.dropped = None;
    events.push(XwmEvent::DragFinished {
      performed: data[1] & 1 != 0,
      action: DndAction::from_atom(atoms, data[2]),
    });
  }

  /// `XdndEnter` from an X source to the proxy: its window, version and
  /// types, from the message or from its `XdndTypeList`.
  fn x11_enter<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    selections: &mut Selections,
    data: [u32; 5],
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    let Some(drag) = &mut self.x11 else {
      return Ok(());
    };
    let version = data[1] >> 24;
    if !(MIN_VERSION..=XDND_VERSION).contains(&version) {
      events.push(XwmEvent::Notice(format!(
        "ignoring an X drag with Xdnd version {version}"
      )));
      return Ok(());
    }
    let source = data[0];
    let types = if data[1] & MORE_TYPES == 0 {
      data[2..]
        .iter()
        .copied()
        .filter(|atom| *atom != 0)
        .collect()
    } else {
      connection
        .get_properties(source, &[atoms.get(Name::XDND_TYPE_LIST)])?
        .pop()
        .flatten()
        .filter(|value| value.kind == ATOM_TYPE)
        .map(|value| value.values32())
        .unwrap_or_default()
    };
    let names = selections.names_of(connection, atoms, &types)?;
    let targets = types
      .into_iter()
      .zip(names)
      .filter_map(|(atom, name)| Some((atom, name?)))
      .collect::<Vec<_>>();
    let offer = mime::offer_from_targets(&targets);
    drag.source = Some(source);
    drag.version = version;
    drag.answered = None;
    let changed = drag.offer != offer;
    drag.offer = offer.clone();
    selections.set_x_offer(Selection::Dnd, offer.clone(), drag.time);
    if changed {
      events.push(XwmEvent::DragOffered {
        mime_types: offer.into_iter().map(|(mime_type, _)| mime_type).collect(),
      });
    }
    Ok(())
  }

  /// `XdndPosition` from the X source: the time and the action it
  /// proposes, answered with an `XdndStatus`.
  fn x11_position<C: XConnection>(
    &mut self,
    connection: &mut C,
    atoms: &Atoms,
    data: [u32; 5],
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    let Some(drag) = &mut self.x11 else {
      return Ok(());
    };
    if drag.source != Some(data[0]) || drag.dropped {
      return Ok(());
    }
    drag.time = data[3];
    let action = match DndAction::from_atom(atoms, data[4]) {
      // Anything but move and ask is copy, which every source supports.
      DndAction::None => DndAction::Copy,
      action => action,
    };
    if action != drag.action {
      drag.action = action;
      events.push(XwmEvent::DragSourceAction { action });
    }
    self.send_status(connection, atoms)
  }

  /// `XdndLeave` from the X source: it is no longer over the proxy.
  fn x11_leave(&mut self, data: [u32; 5], events: &mut Vec<XwmEvent>) {
    let Some(drag) = &mut self.x11 else {
      return;
    };
    if drag.source != Some(data[0]) || drag.dropped {
      return;
    }
    drag.source = None;
    drag.answered = None;
    events.push(XwmEvent::DragLeft);
  }

  /// `XdndDrop` from the X source: the data is converted at the drop's
  /// time from now on.
  fn x11_drop(
    &mut self,
    selections: &mut Selections,
    data: [u32; 5],
    events: &mut Vec<XwmEvent>,
  ) {
    let Some(drag) = &mut self.x11 else {
      return;
    };
    if drag.source != Some(data[0]) || drag.dropped {
      return;
    }
    drag.dropped = true;
    drag.time = data[2];
    selections.set_x_offer(Selection::Dnd, drag.offer.clone(), drag.time);
    events.push(XwmEvent::DragDropped);
  }
}

/// Starts a Wayland drag's visit to `window`: follows `XdndProxy`, reads
/// `XdndAware` and, when the window speaks Xdnd 3 or newer, sends
/// `XdndEnter` with the types, in the message when there are three or
/// fewer and in the selection window's `XdndTypeList` otherwise.
fn enter<C: XConnection>(
  connection: &mut C,
  atoms: &Atoms,
  source: Window,
  drag: &WaylandDrag,
  window: Window,
) -> Result<Target, XwmError> {
  let proxy_atom = atoms.get(Name::XDND_PROXY);
  let aware_atom = atoms.get(Name::XDND_AWARE);
  let mut destination = window;
  let values = connection.get_properties(window, &[proxy_atom, aware_atom])?;
  let mut aware = values.get(1).cloned().flatten();
  if let Some(proxy) = values
    .first()
    .cloned()
    .flatten()
    .filter(|value| value.kind == WINDOW_TYPE)
    .and_then(|value| value.first32())
  {
    // A proxy counts only when its own XdndProxy names itself.
    let values = connection.get_properties(proxy, &[proxy_atom, aware_atom])?;
    let names_itself = values
      .first()
      .cloned()
      .flatten()
      .and_then(|value| value.first32())
      == Some(proxy);
    if names_itself {
      destination = proxy;
      aware = values.get(1).cloned().flatten();
    }
  }
  let version = aware
    .filter(|value| value.kind == ATOM_TYPE)
    .and_then(|value| value.first32())
    .filter(|version| *version >= MIN_VERSION)
    .map(|version| version.min(XDND_VERSION));
  let target = Target {
    window,
    destination,
    version,
    waiting: false,
    queued: None,
    sent: None,
    accepted: false,
    action: DndAction::None,
  };
  let Some(version) = version else {
    return Ok(target);
  };
  let mut message = [source, version << 24, 0, 0, 0];
  if drag.types.len() > 3 {
    message[1] |= MORE_TYPES;
    connection.change_property32(
      source,
      atoms.get(Name::XDND_TYPE_LIST),
      ATOM_TYPE,
      &drag.types,
    )?;
  }
  for (slot, atom) in message[2..].iter_mut().zip(&drag.types) {
    *slot = *atom;
  }
  send(
    connection,
    destination,
    atoms.get(Name::XDND_ENTER),
    message,
  )?;
  Ok(target)
}

/// Sends `XdndLeave` to a target that was entered.
fn leave<C: XConnection>(
  connection: &mut C,
  atoms: &Atoms,
  source: Window,
  target: &Target,
) -> Result<(), XwmError> {
  if target.version.is_none() {
    return Ok(());
  }
  send(
    connection,
    target.destination,
    atoms.get(Name::XDND_LEAVE),
    [source, 0, 0, 0, 0],
  )
}

/// Sends `XdndPosition` and waits for its `XdndStatus`.
fn send_position<C: XConnection>(
  connection: &mut C,
  atoms: &Atoms,
  source: Window,
  target: &mut Target,
  position: Position,
  action: DndAction,
) -> Result<(), XwmError> {
  target.waiting = true;
  target.sent = Some(position);
  send(
    connection,
    target.destination,
    atoms.get(Name::XDND_POSITION),
    [
      source,
      0,
      position.packed(),
      position.time,
      action.atom(atoms),
    ],
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn actions_propose_copy_then_move_then_ask() {
    let all = DndActions::COPY | DndActions::MOVE | DndActions::ASK;
    assert_eq!(all.proposed(), DndAction::Copy);
    let rest = DndActions::MOVE | DndActions::ASK;
    assert_eq!(rest.proposed(), DndAction::Move);
    assert_eq!(DndActions::ASK.proposed(), DndAction::Ask);
    assert_eq!(DndActions::default().proposed(), DndAction::None);
    assert_eq!(DndActions::from_bits(0xff).bits(), 7);
    assert!(!all.contains(DndAction::None));
  }

  #[test]
  fn positions_pack_root_coordinates() {
    let position = Position {
      x: 300,
      y: 20,
      time: 0,
    };
    assert_eq!(position.packed(), (300 << 16) | 20);
    let outside = Position {
      x: -5,
      y: 70_000,
      time: 0,
    };
    assert_eq!(outside.packed(), 0xffff);
  }
}
