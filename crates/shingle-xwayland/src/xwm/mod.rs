//! Shingle's X window manager.
//!
//! A port of the wlroots XWM (`xwayland/xwm.c`) that Wayfire relies on. It
//! connects over Xwayland's `-wm` socket, interns its atoms, redirects the
//! root's children, takes the window manager selections and advertises
//! what it supports. Then it tracks every child of the root, reads its
//! properties, maps managed windows when they ask, keeps `WM_STATE` and
//! the client lists, and reports what the compositor needs as
//! [`XwmEvent`]s, including what clients ask for in their EWMH and ICCCM
//! client messages. The compositor answers with [`Xwm::configure`],
//! [`Xwm::set_state`], [`Xwm::close`], [`Xwm::set_focus`] (with the ICCCM
//! input models and the `FocusIn` guard), [`Xwm::restack`] and
//! [`Xwm::set_cursor`].
//!
//! It also bridges the clipboard and primary selection between X and
//! Wayland ([`selection`]): [`Xwm::bridge_selections`],
//! [`Xwm::set_selection`], [`Xwm::receive_selection`],
//! [`Xwm::take_selection_sends`], [`Xwm::pump_transfers`] and
//! [`Xwm::transfer_watches`].

mod atoms;
mod connection;
pub mod cursor;
pub mod messages;
pub mod mime;
pub mod pairing;
pub mod policy;
pub mod props;
pub mod selection;
pub mod window;

use std::collections::HashMap;

pub use atoms::{Atoms, NAMES as ATOM_NAMES, Name as AtomName, SUPPORTED};
pub use connection::{
  ATOM_TYPE, COMPOSITE, WINDOW_TYPE, X11Connection, XConnection, XFIXES,
  XwmError,
};
pub use cursor::CursorImage;
pub use messages::{MoveResize, StateChange};
pub use pairing::{Paired, Pairing};
pub use policy::{Placement, configure_reply};
pub use props::Property;
pub use selection::{INCR_CHUNK_SIZE, Selection, SelectionSend, TransferWatch};
pub use window::{
  Geometry, InputModel, Kind, NetWmState, Protocols, WindowInfo, WindowType,
};
pub use x11rb::protocol::xproto::Window;

use x11rb::protocol::{
  Event,
  xproto::{
    AtomEnum, ClientMessageEvent, ConfigWindow, EventMask, NotifyDetail,
    NotifyMode,
  },
};

use props::{SizeHints, WmHints};
use selection::Selections;

/// The name the window manager gives itself in `_NET_WM_NAME` on its check
/// window.
pub const WM_NAME: &str = "Shingle";

/// The geometry of the check window, as wlroots creates it.
const CHECK_WINDOW_GEOMETRY: (i16, i16, u16, u16) = (0, 0, 10, 10);

/// ICCCM `WM_STATE` values.
const WITHDRAWN_STATE: u32 = 0;
const NORMAL_STATE: u32 = 1;
const ICONIC_STATE: u32 = 3;

/// Something the window manager saw that the compositor should know about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XwmEvent {
  /// The server reported an error for one of the window manager's
  /// requests. It is only logged.
  RequestFailed(String),
  /// Something worth logging once, such as a client message from an
  /// Xwayland too old for Shingle.
  Notice(String),
  /// A window was mapped: a managed one after its `MapRequest`, which the
  /// window manager granted, or an override-redirect one on `MapNotify`.
  /// It becomes a Shingle window once its surface is associated and has
  /// a buffer.
  Mapped {
    /// The X window.
    window: Window,
    /// Its properties at map time.
    info: Box<WindowInfo>,
  },
  /// A property of a tracked window changed.
  Changed {
    /// The X window.
    window: Window,
    /// Its properties now.
    info: Box<WindowInfo>,
  },
  /// A mapped window was unmapped. Xwayland destroys its surface too.
  Unmapped {
    /// The X window.
    window: Window,
  },
  /// A tracked window was destroyed.
  Destroyed {
    /// The X window.
    window: Window,
  },
  /// `WL_SURFACE_SERIAL`: the serial the window's surface commits through
  /// `xwayland_surface_v1.set_serial`.
  Serial {
    /// The X window.
    window: Window,
    /// The 64-bit serial.
    serial: u64,
  },
  /// The client asked for a geometry; the compositor answers with
  /// [`Xwm::configure`].
  ConfigureRequest {
    /// The X window.
    window: Window,
    /// The current geometry with the requested fields applied.
    geometry: Geometry,
  },
  /// `_NET_WM_STATE` or `WM_CHANGE_STATE`: the client asked to change
  /// fullscreen, maximized or minimized. Before the window is mapped the
  /// change is also kept in its info, as its initial state.
  StateRequest {
    /// The X window.
    window: Window,
    /// What to change.
    change: StateChange,
  },
  /// `_NET_ACTIVE_WINDOW`: the client asked for the window to be focused.
  ActivateRequest {
    /// The X window.
    window: Window,
  },
  /// `_NET_WM_MOVERESIZE`: the client asked for an interactive move or
  /// resize.
  MoveResizeRequest {
    /// The X window.
    window: Window,
    /// Move, or resize from which edges.
    request: MoveResize,
  },
  /// `_NET_CLOSE_WINDOW`: a client asked for the window to be closed.
  CloseRequest {
    /// The X window.
    window: Window,
  },
  /// An X client took a bridged selection and offers these MIME types;
  /// the compositor makes it the Wayland selection.
  SelectionOffered {
    /// The selection.
    selection: Selection,
    /// The MIME types, from the owner's `TARGETS`.
    mime_types: Vec<String>,
  },
  /// The X client that owned a bridged selection gave it up or went away;
  /// the compositor clears the Wayland selection if it still comes from X.
  SelectionCleared {
    /// The selection.
    selection: Selection,
  },
}

/// What the window manager keeps per child of the root.
#[derive(Clone, Debug)]
struct Tracked {
  info: WindowInfo,
  /// Mapped, as far as the window manager is concerned.
  mapped: bool,
  /// The serial of the current surface, once announced.
  serial: Option<u64>,
  /// The geometry last sent with [`Xwm::configure`].
  configured: Option<Geometry>,
  /// The `_NET_WM_STATE` last written.
  state: Option<NetWmState>,
  /// The state the compositor asked for last with [`Xwm::set_state`];
  /// `_NET_WM_STATE_FOCUSED` and `_MODAL` are the window manager's.
  wanted: Option<NetWmState>,
  /// `WM_NAME`.
  wm_name: String,
  /// `_NET_WM_NAME`, which wins over `WM_NAME` when set.
  net_wm_name: Option<String>,
}

impl Tracked {
  fn new(geometry: Geometry, override_redirect: bool) -> Self {
    Self {
      info: WindowInfo {
        geometry,
        override_redirect,
        ..WindowInfo::default()
      },
      mapped: false,
      serial: None,
      configured: None,
      state: None,
      wanted: None,
      wm_name: String::new(),
      net_wm_name: None,
    }
  }
}

/// The window manager on one Xwayland connection.
pub struct Xwm<C: XConnection> {
  connection: C,
  atoms: Atoms,
  root: Window,
  check_window: Window,
  terminate_without_clients: bool,
  windows: HashMap<Window, Tracked>,
  /// `_NET_CLIENT_LIST`, in map order.
  client_list: Vec<Window>,
  /// `_NET_CLIENT_LIST_STACKING`, bottom to top.
  stacking: Vec<Window>,
  /// The window the compositor focused last with [`Xwm::set_focus`].
  requested_focus: Option<Window>,
  /// The window that has the X focus as far as the window manager is
  /// concerned. It differs from `requested_focus` only after a client
  /// moved the focus between its own windows.
  focused: Option<Window>,
  /// `_NET_ACTIVE_WINDOW` as last written.
  active: Option<Window>,
  /// The sequence number of the last `SetInputFocus`.
  focus_sequence: u16,
  warned_surface_id: bool,
  /// The clipboard and primary selection bridge.
  selections: Selections,
}

impl<C: XConnection> Xwm<C> {
  /// Brings the window manager up on `connection`:
  ///
  /// 1. interns [`ATOM_NAMES`] in one batch, checks for Composite and
  ///    XFixes, and makes the server ignore this client for `-terminate`;
  /// 2. redirects the root's children (Composite, `Manual`);
  /// 3. selects `SubstructureRedirect`, `SubstructureNotify` and
  ///    `PropertyChange` on the root;
  /// 4. creates the check window and takes `WM_S0` and `_NET_WM_CM_S0`;
  /// 5. sets `_NET_SUPPORTING_WM_CHECK`, `_NET_SUPPORTED`, an empty
  ///    `_NET_ACTIVE_WINDOW` and empty client lists on the root.
  ///
  /// Then it flushes. The compositor sets the root cursor with
  /// [`Xwm::set_cursor`] and sets up the selection bridge with
  /// [`Xwm::bridge_selections`].
  ///
  /// # Errors
  ///
  /// When Composite is missing, a request fails or another window manager
  /// holds substructure redirection on the root.
  pub fn start(mut connection: C) -> Result<Self, XwmError> {
    let atoms = Atoms::from_values(connection.intern_atoms(ATOM_NAMES)?)
      .ok_or_else(|| {
        XwmError::Request("wrong number of interned atoms".into())
      })?;
    if !connection.has_extension(COMPOSITE)? {
      return Err(XwmError::MissingExtension(COMPOSITE));
    }
    if !connection.has_extension(XFIXES)? {
      return Err(XwmError::MissingExtension(XFIXES));
    }
    // As wlroots' xwm_create does: the window manager must not keep a
    // lazily started server alive on its own. Older servers just never
    // exit when idle.
    let terminate_without_clients = connection.terminate_without_clients()?;
    let root = connection.root();
    connection.redirect_subwindows(root)?;
    connection.select_events(
      root,
      EventMask::SUBSTRUCTURE_REDIRECT
        | EventMask::SUBSTRUCTURE_NOTIFY
        | EventMask::PROPERTY_CHANGE,
    )?;
    let check_window = connection.generate_id()?;
    connection.create_window(check_window, root, CHECK_WINDOW_GEOMETRY)?;
    connection.change_property8(
      check_window,
      atoms.get(AtomName::NET_WM_NAME),
      atoms.get(AtomName::UTF8_STRING),
      WM_NAME.as_bytes(),
    )?;
    for window in [root, check_window] {
      connection.change_property32(
        window,
        atoms.get(AtomName::NET_SUPPORTING_WM_CHECK),
        WINDOW_TYPE,
        &[check_window],
      )?;
    }
    connection.set_selection_owner(check_window, atoms.get(AtomName::WM_S0))?;
    connection
      .set_selection_owner(check_window, atoms.get(AtomName::NET_WM_CM_S0))?;
    let supported = SUPPORTED
      .iter()
      .map(|name| atoms.get(*name))
      .collect::<Vec<_>>();
    connection.change_property32(
      root,
      atoms.get(AtomName::NET_SUPPORTED),
      ATOM_TYPE,
      &supported,
    )?;
    connection.change_property32(
      root,
      atoms.get(AtomName::NET_ACTIVE_WINDOW),
      WINDOW_TYPE,
      &[x11rb::NONE],
    )?;
    for list in [
      AtomName::NET_CLIENT_LIST,
      AtomName::NET_CLIENT_LIST_STACKING,
    ] {
      connection.change_property32(root, atoms.get(list), WINDOW_TYPE, &[])?;
    }
    connection.flush()?;
    Ok(Self {
      connection,
      atoms,
      root,
      check_window,
      terminate_without_clients,
      windows: HashMap::new(),
      client_list: Vec::new(),
      stacking: Vec::new(),
      requested_focus: None,
      focused: None,
      active: None,
      focus_sequence: 0,
      warned_surface_id: false,
      selections: Selections::default(),
    })
  }

  /// Reads every event that has arrived, without waiting, handles it and
  /// reports what the compositor needs to know.
  ///
  /// # Errors
  ///
  /// When the connection failed or closed; the window manager is then
  /// unusable.
  pub fn dispatch(&mut self) -> Result<Vec<XwmEvent>, XwmError> {
    let mut events = Vec::new();
    while let Some(event) = self.connection.poll_for_event()? {
      self.handle(event, &mut events)?;
    }
    Ok(events)
  }

  /// Handles one event.
  fn handle(
    &mut self,
    event: Event,
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    match event {
      Event::Error(error) => {
        // A request for a window that has been destroyed since, such as
        // WM_STATE on the UnmapNotify that precedes its DestroyNotify, is
        // expected to fail; wlroots ignores those too.
        let bad_window = error.error_kind == x11rb::protocol::ErrorKind::Window;
        if bad_window {
          // A selection requestor that went away mid-transfer.
          self.selections.window_gone(error.bad_value);
        }
        let gone = bad_window && !self.windows.contains_key(&error.bad_value);
        if !gone {
          events.push(XwmEvent::RequestFailed(format!("{error:?}")));
        }
      }
      Event::CreateNotify(event) => {
        if event.window == self.check_window
          || event.parent != self.root
          || self.selections.owns_window(event.window)
        {
          return Ok(());
        }
        let geometry = Geometry::new(
          event.x.into(),
          event.y.into(),
          event.width.into(),
          event.height.into(),
        );
        self.windows.insert(
          event.window,
          Tracked::new(geometry, event.override_redirect),
        );
        // As wlroots' xwayland_surface_create: property changes for
        // PropertyNotify, focus changes for the FocusIn guard (XW-4).
        self.connection.watch_window(
          event.window,
          EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE,
        )?;
      }
      Event::DestroyNotify(event) => {
        if self.windows.remove(&event.window).is_some() {
          self.forget_focus(event.window)?;
          self.remove_client(event.window)?;
          events.push(XwmEvent::Destroyed {
            window: event.window,
          });
        }
      }
      Event::MapRequest(event) => self.map_request(event.window, events)?,
      Event::MapNotify(event) => {
        let window = event.window;
        let Some(tracked) = self.windows.get_mut(&window) else {
          return Ok(());
        };
        tracked.info.override_redirect = event.override_redirect;
        if !event.override_redirect || tracked.mapped {
          return Ok(());
        }
        self.read_properties(window)?;
        let Some(tracked) = self.windows.get_mut(&window) else {
          return Ok(());
        };
        tracked.mapped = true;
        events.push(XwmEvent::Mapped {
          window,
          info: Box::new(tracked.info.clone()),
        });
      }
      Event::UnmapNotify(event) => {
        let window = event.window;
        let Some(tracked) = self.windows.get_mut(&window) else {
          return Ok(());
        };
        if !tracked.mapped {
          return Ok(());
        }
        tracked.mapped = false;
        // The next map brings a new surface with a new serial.
        tracked.serial = None;
        tracked.configured = None;
        tracked.state = None;
        tracked.wanted = None;
        if !tracked.info.override_redirect {
          self.set_wm_state(window, WITHDRAWN_STATE)?;
          self.remove_client(window)?;
        }
        self.forget_focus(window)?;
        events.push(XwmEvent::Unmapped { window });
      }
      Event::ConfigureRequest(event) => {
        let Some(tracked) = self.windows.get(&event.window) else {
          return Ok(());
        };
        let mut geometry = tracked.info.geometry;
        let mask = event.value_mask;
        if mask.contains(ConfigWindow::X) {
          geometry.x = event.x.into();
        }
        if mask.contains(ConfigWindow::Y) {
          geometry.y = event.y.into();
        }
        if mask.contains(ConfigWindow::WIDTH) {
          geometry.width = event.width.into();
        }
        if mask.contains(ConfigWindow::HEIGHT) {
          geometry.height = event.height.into();
        }
        events.push(XwmEvent::ConfigureRequest {
          window: event.window,
          geometry,
        });
      }
      Event::ConfigureNotify(event) => {
        if let Some(tracked) = self.windows.get_mut(&event.window) {
          let geometry = Geometry::new(
            event.x.into(),
            event.y.into(),
            event.width.into(),
            event.height.into(),
          );
          let moved = tracked.info.geometry != geometry;
          tracked.info.geometry = geometry;
          tracked.info.override_redirect = event.override_redirect;
          // Override-redirect windows place themselves; the compositor
          // draws them where they are.
          if moved && tracked.mapped && event.override_redirect {
            events.push(XwmEvent::Changed {
              window: event.window,
              info: Box::new(tracked.info.clone()),
            });
          }
        }
      }
      Event::FocusIn(event) => self.focus_in(&event)?,
      Event::PropertyNotify(event) => {
        self.selections.property_notify(
          &mut self.connection,
          &self.atoms,
          &event,
        )?;
        let window = event.window;
        if !self.windows.contains_key(&window)
          || !self.property_atoms().contains(&event.atom)
        {
          return Ok(());
        }
        let value = self
          .connection
          .get_properties(window, &[event.atom])?
          .pop()
          .flatten();
        let before = self.windows.get(&window).map(|t| t.info.clone());
        self.apply_property(window, event.atom, value.as_ref());
        let Some(tracked) = self.windows.get(&window) else {
          return Ok(());
        };
        if before.as_ref() != Some(&tracked.info) {
          events.push(XwmEvent::Changed {
            window,
            info: Box::new(tracked.info.clone()),
          });
        }
      }
      Event::ClientMessage(event) => self.client_message(&event, events)?,
      Event::XfixesSelectionNotify(event) => {
        if let Some(event) = self.selections.owner_changed(
          &mut self.connection,
          &self.atoms,
          &event,
        )? {
          events.push(event);
        }
      }
      Event::SelectionNotify(event) => {
        let (_, event) = self.selections.selection_notify(
          &mut self.connection,
          &self.atoms,
          &event,
        )?;
        events.extend(event);
        self.selections.pump(&mut self.connection, &self.atoms)?;
      }
      Event::SelectionRequest(event) => {
        let windows = &self.windows;
        let tracked = |window: Window| windows.contains_key(&window);
        if !self.selections.selection_request(
          &mut self.connection,
          &self.atoms,
          &event,
          &tracked,
        )? {
          // Not a selection the bridge owns: refuse, as ICCCM asks.
          self.connection.send_selection_notify(
            event.requestor,
            event.selection,
            event.target,
            x11rb::NONE,
            event.time,
          )?;
        }
      }
      _ => {}
    }
    Ok(())
  }

  /// Handles a client message: `WL_SURFACE_SERIAL` for association, and
  /// the EWMH and ICCCM requests wlroots' `xwm_handle_client_message`
  /// handles, plus `_NET_CLOSE_WINDOW`.
  fn client_message(
    &mut self,
    event: &ClientMessageEvent,
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    let data = event.data.as_data32();
    let window = event.window;
    let kind = event.type_;
    let atom = |name| self.atoms.get(name);
    if kind == atom(AtomName::WL_SURFACE_SERIAL) {
      let Some(tracked) = self.windows.get_mut(&window) else {
        return Ok(());
      };
      // A repeated serial for the same surface is ignored, as in
      // wlroots' xwm_handle_surface_serial_message.
      if tracked.serial.is_some() {
        return Ok(());
      }
      let serial = u64::from(data[0]) | (u64::from(data[1]) << 32);
      tracked.serial = Some(serial);
      events.push(XwmEvent::Serial { window, serial });
      return Ok(());
    }
    if self.atoms.name_of(kind).is_none() {
      if !self.warned_surface_id {
        // Possibly WL_SURFACE_ID from an Xwayland before 23.1, which
        // Shingle does not support.
        self.warned_surface_id = true;
        events.push(XwmEvent::Notice(format!(
          "ignoring X client message type {kind}; if it is WL_SURFACE_ID, \
           this Xwayland is older than 23.1, which Shingle needs"
        )));
      }
      return Ok(());
    }
    if !self.windows.contains_key(&window) {
      return Ok(());
    }
    if kind == atom(AtomName::NET_WM_STATE) {
      if event.format != 32 {
        return Ok(());
      }
      self.state_message(window, data, events)?;
    } else if kind == atom(AtomName::WM_CHANGE_STATE) {
      if let Some(minimized) = messages::change_state(data[0]) {
        let change = StateChange {
          minimized: Some(minimized),
          ..StateChange::default()
        };
        self.request_state(window, change, events);
      }
    } else if kind == atom(AtomName::NET_ACTIVE_WINDOW) {
      events.push(XwmEvent::ActivateRequest { window });
    } else if kind == atom(AtomName::NET_WM_MOVERESIZE) {
      if let Some(request) = messages::moveresize(data[2]) {
        events.push(XwmEvent::MoveResizeRequest { window, request });
      }
    } else if kind == atom(AtomName::NET_CLOSE_WINDOW) {
      events.push(XwmEvent::CloseRequest { window });
    }
    Ok(())
  }

  /// A `_NET_WM_STATE` message: `_NET_WM_STATE_MODAL` is the window
  /// manager's to keep; fullscreen, maximized and hidden are requests.
  fn state_message(
    &mut self,
    window: Window,
    data: [u32; 5],
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    let Some(tracked) = self.windows.get_mut(&window) else {
      return Ok(());
    };
    let before = tracked.info.state;
    let after = messages::net_wm_state(&self.atoms, before, data);
    if after.modal != before.modal {
      tracked.info.state.modal = after.modal;
      self.write_state(window)?;
    }
    self.request_state(window, messages::state_change(before, after), events);
    Ok(())
  }

  /// Reports a state change the client asked for. Before the compositor
  /// manages the window, the change also becomes its initial state.
  fn request_state(
    &mut self,
    window: Window,
    change: StateChange,
    events: &mut Vec<XwmEvent>,
  ) {
    let Some(tracked) = self.windows.get_mut(&window) else {
      return;
    };
    if change.is_empty() {
      return;
    }
    if tracked.wanted.is_none() {
      let state = &mut tracked.info.state;
      if let Some(fullscreen) = change.fullscreen {
        state.fullscreen = fullscreen;
      }
      if let Some(maximized) = change.maximized {
        state.maximized_vert = maximized;
        state.maximized_horz = maximized;
      }
      if let Some(minimized) = change.minimized {
        state.hidden = minimized;
      }
    }
    events.push(XwmEvent::StateRequest { window, change });
  }

  /// The `FocusIn` guard, wlroots' `xwm_handle_focus_in`: X clients may
  /// not move the focus behind the compositor's back, except between
  /// windows of the same client (by PID) after the window manager's last
  /// `SetInputFocus`, which some applications rely on. Anything else gets
  /// the focus put back. Grabs and pointer focus are left alone.
  fn focus_in(
    &mut self,
    event: &x11rb::protocol::xproto::FocusInEvent,
  ) -> Result<(), XwmError> {
    if event.mode == NotifyMode::GRAB
      || event.mode == NotifyMode::UNGRAB
      || event.detail == NotifyDetail::POINTER
    {
      return Ok(());
    }
    let requested = event.event;
    if self.focused == Some(requested) {
      return Ok(());
    }
    let pid = |window: Window| {
      self
        .windows
        .get(&window)
        .and_then(|tracked| tracked.info.pid)
    };
    let same_client = self.focused.is_some_and(|focused| {
      pid(focused).is_some() && pid(focused) == pid(requested)
    });
    if same_client && sequence_after(self.focus_sequence, event.sequence) {
      self.focus_window(Some(requested))
    } else {
      self.focus_window(self.focused)
    }
  }

  /// Grants a `MapRequest`: reads the window's properties, maps it, sets
  /// `WM_STATE` to Normal and appends it to the client lists, as wlroots'
  /// `xwm_handle_map_request` does.
  fn map_request(
    &mut self,
    window: Window,
    events: &mut Vec<XwmEvent>,
  ) -> Result<(), XwmError> {
    if window == self.check_window || window == x11rb::NONE {
      return Ok(());
    }
    // A window created before the window manager started.
    self
      .windows
      .entry(window)
      .or_insert_with(|| Tracked::new(Geometry::default(), false));
    self.read_properties(window)?;
    self.connection.map_window(window)?;
    self.set_wm_state(window, NORMAL_STATE)?;
    if !self.client_list.contains(&window) {
      self.client_list.push(window);
      self.stacking.push(window);
      self.write_client_lists()?;
    }
    let Some(tracked) = self.windows.get_mut(&window) else {
      return Ok(());
    };
    tracked.mapped = true;
    events.push(XwmEvent::Mapped {
      window,
      info: Box::new(tracked.info.clone()),
    });
    Ok(())
  }

  /// The properties the window manager reads, in the order
  /// [`Xwm::read_properties`] asks for them.
  fn property_atoms(&self) -> [u32; 12] {
    [
      AtomEnum::WM_CLASS.into(),
      AtomEnum::WM_NAME.into(),
      self.atoms.get(AtomName::NET_WM_NAME),
      AtomEnum::WM_TRANSIENT_FOR.into(),
      self.atoms.get(AtomName::WM_NORMAL_HINTS),
      self.atoms.get(AtomName::WM_HINTS),
      self.atoms.get(AtomName::WM_PROTOCOLS),
      self.atoms.get(AtomName::NET_WM_WINDOW_TYPE),
      self.atoms.get(AtomName::NET_WM_STATE),
      self.atoms.get(AtomName::NET_WM_PID),
      self.atoms.get(AtomName::MOTIF_WM_HINTS),
      self.atoms.get(AtomName::NET_STARTUP_ID),
    ]
  }

  /// Reads every property of `window` in one pipelined batch, and its PID
  /// through XRes.
  fn read_properties(&mut self, window: Window) -> Result<(), XwmError> {
    let atoms = self.property_atoms();
    let values = self.connection.get_properties(window, &atoms)?;
    for (atom, value) in atoms.iter().zip(&values) {
      self.apply_property(window, *atom, value.as_ref());
    }
    let pid = self.connection.client_pid(window)?;
    if let Some(tracked) = self.windows.get_mut(&window)
      && pid.is_some()
    {
      tracked.info.pid = pid;
    }
    Ok(())
  }

  /// Stores one property's value, or its absence, in the window's info.
  fn apply_property(
    &mut self,
    window: Window,
    atom: u32,
    value: Option<&Property>,
  ) {
    let atoms = &self.atoms;
    let known = |window: u32| self.windows.contains_key(&window);
    let transient_for = (atom == u32::from(AtomEnum::WM_TRANSIENT_FOR))
      .then(|| value.and_then(Property::first32).filter(|w| known(*w)))
      .flatten();
    let Some(tracked) = self.windows.get_mut(&window) else {
      return;
    };
    let words = || value.map(Property::values32).unwrap_or_default();
    if atom == u32::from(AtomEnum::WM_NAME) {
      tracked.wm_name = value.map(props::text).unwrap_or_default();
    } else if atom == atoms.get(AtomName::NET_WM_NAME) {
      tracked.net_wm_name = value.map(props::text);
    }
    // `_NET_WM_NAME`, when set, takes precedence.
    tracked.info.title = tracked
      .net_wm_name
      .clone()
      .unwrap_or_else(|| tracked.wm_name.clone());
    let info = &mut tracked.info;
    if atom == u32::from(AtomEnum::WM_CLASS) {
      let (instance, class) = value
        .and_then(|value| props::wm_class(&value.value))
        .unwrap_or_default();
      info.instance = instance;
      info.class = class;
    } else if atom == u32::from(AtomEnum::WM_TRANSIENT_FOR) {
      info.transient_for = transient_for;
    } else if atom == atoms.get(AtomName::WM_NORMAL_HINTS) {
      info.size_hints = SizeHints::parse(&words());
    } else if atom == atoms.get(AtomName::WM_HINTS) {
      info.hints = WmHints::parse(&words());
    } else if atom == atoms.get(AtomName::WM_PROTOCOLS) {
      info.protocols = Protocols::parse(atoms, &words());
    } else if atom == atoms.get(AtomName::NET_WM_WINDOW_TYPE) {
      info.types = words()
        .into_iter()
        .map(|atom| WindowType::from_atom(atoms, atom))
        .collect();
    } else if atom == atoms.get(AtomName::NET_WM_STATE) {
      info.state = NetWmState::parse(atoms, &words());
    } else if atom == atoms.get(AtomName::NET_WM_PID) {
      // XRes, read after the properties, wins when it knows the PID.
      info.pid = value.and_then(Property::first32).filter(|pid| *pid > 0);
    } else if atom == atoms.get(AtomName::MOTIF_WM_HINTS) {
      info.decorations = props::motif_decorations(&words());
    } else if atom == atoms.get(AtomName::NET_STARTUP_ID) {
      info.startup_id = value.map(props::text);
    }
  }

  /// Sets ICCCM `WM_STATE` with no icon window.
  fn set_wm_state(
    &mut self,
    window: Window,
    state: u32,
  ) -> Result<(), XwmError> {
    let kind = self.atoms.get(AtomName::WM_STATE);
    self
      .connection
      .change_property32(window, kind, kind, &[state, x11rb::NONE])
  }

  /// Removes `window` from the client lists, if it is there.
  fn remove_client(&mut self, window: Window) -> Result<(), XwmError> {
    let before = self.client_list.len();
    self.client_list.retain(|candidate| *candidate != window);
    self.stacking.retain(|candidate| *candidate != window);
    if self.client_list.len() != before {
      self.write_client_lists()?;
    }
    Ok(())
  }

  /// Writes `_NET_CLIENT_LIST` in map order and
  /// `_NET_CLIENT_LIST_STACKING` bottom to top.
  fn write_client_lists(&mut self) -> Result<(), XwmError> {
    self.connection.change_property32(
      self.root,
      self.atoms.get(AtomName::NET_CLIENT_LIST),
      WINDOW_TYPE,
      &self.client_list,
    )?;
    self.write_stacking()
  }

  /// Writes `_NET_CLIENT_LIST_STACKING`.
  fn write_stacking(&mut self) -> Result<(), XwmError> {
    self.connection.change_property32(
      self.root,
      self.atoms.get(AtomName::NET_CLIENT_LIST_STACKING),
      WINDOW_TYPE,
      &self.stacking,
    )
  }

  /// Forgets the focus of a window that was unmapped or destroyed. The
  /// server reverts the X focus itself; `_NET_ACTIVE_WINDOW` becomes
  /// `None` until the compositor focuses another window.
  fn forget_focus(&mut self, window: Window) -> Result<(), XwmError> {
    if self.focused == Some(window) {
      self.focused = None;
    }
    if self.requested_focus == Some(window) {
      self.requested_focus = None;
    }
    self.write_active()
  }

  /// Writes `_NET_ACTIVE_WINDOW`, the window the compositor focused, when
  /// it changed.
  fn write_active(&mut self) -> Result<(), XwmError> {
    let window = self.requested_focus;
    if self.active == window {
      return Ok(());
    }
    self.active = window;
    self.connection.change_property32(
      self.root,
      self.atoms.get(AtomName::NET_ACTIVE_WINDOW),
      WINDOW_TYPE,
      &[window.unwrap_or(x11rb::NONE)],
    )
  }

  /// Moves the X focus to `window`, or to `None`, following the window's
  /// input model ([`InputModel`]), and updates `_NET_WM_STATE_FOCUSED`
  /// on the old and the new window: wlroots' `xwm_set_focus_window`.
  /// A no-input window gets the X focus set to `None`, so that keys do
  /// not reach the window that had it.
  fn focus_window(&mut self, window: Option<Window>) -> Result<(), XwmError> {
    let previous = std::mem::replace(&mut self.focused, window);
    if let Some(previous) =
      previous.filter(|previous| Some(*previous) != window)
    {
      self.write_state(previous)?;
    }
    let Some(window) = window else {
      self.focus_sequence = self.connection.set_input_focus(x11rb::NONE)?;
      return Ok(());
    };
    let Some(model) = self.windows.get(&window).map(|t| t.info.input_model())
    else {
      return Ok(());
    };
    if model.sends_take_focus() {
      self.take_focus(window)?;
    }
    if model.sets_input_focus() {
      self.focus_sequence = self.connection.set_input_focus(window)?;
    } else if model == InputModel::NoInput {
      self.focus_sequence = self.connection.set_input_focus(x11rb::NONE)?;
    }
    self.write_state(window)
  }

  /// Sends `WM_TAKE_FOCUS`.
  fn take_focus(&mut self, window: Window) -> Result<(), XwmError> {
    let protocols = self.atoms.get(AtomName::WM_PROTOCOLS);
    let take_focus = self.atoms.get(AtomName::WM_TAKE_FOCUS);
    self.connection.send_client_message(
      window,
      protocols,
      [take_focus, x11rb::CURRENT_TIME, 0, 0, 0],
    )
  }

  /// Writes the `_NET_WM_STATE` of a mapped managed window from what the
  /// compositor asked for, the window manager's focus and the client's
  /// modal flag, and `WM_STATE` Iconic or Normal when minimizing changed,
  /// as wlroots' `wlr_xwayland_surface_set_minimized` does. Nothing is
  /// written before the compositor has asked for a state.
  fn write_state(&mut self, window: Window) -> Result<(), XwmError> {
    let focused = self.focused == Some(window);
    let Some(tracked) = self.windows.get_mut(&window) else {
      return Ok(());
    };
    if tracked.info.override_redirect || !tracked.mapped {
      return Ok(());
    }
    let Some(wanted) = tracked.wanted else {
      return Ok(());
    };
    let state = NetWmState {
      modal: tracked.info.state.modal,
      focused,
      ..wanted
    };
    let previous = tracked.state.replace(state);
    if previous == Some(state) {
      return Ok(());
    }
    tracked.info.state = state;
    let property = self.atoms.get(AtomName::NET_WM_STATE);
    let value = state.encode(&self.atoms);
    self
      .connection
      .change_property32(window, property, ATOM_TYPE, &value)?;
    if previous.map_or(state.hidden, |previous| previous.hidden != state.hidden)
    {
      let wm_state = if state.hidden {
        ICONIC_STATE
      } else {
        NORMAL_STATE
      };
      self.set_wm_state(window, wm_state)?;
    }
    Ok(())
  }

  /// Configures a managed window to `geometry` (made non-empty). The same
  /// geometry is sent once unless `force`d, as when answering a
  /// `ConfigureRequest`. When the size stays the same the server sends no
  /// `ConfigureNotify`, so a synthetic one follows, as ICCCM 4.1.5 asks
  /// and wlroots' `wlr_xwayland_surface_configure` does. Override-redirect
  /// and unknown windows are left alone.
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn configure(
    &mut self,
    window: Window,
    geometry: Geometry,
    force: bool,
  ) -> Result<(), XwmError> {
    let Some(tracked) = self.windows.get_mut(&window) else {
      return Ok(());
    };
    if tracked.info.override_redirect {
      return Ok(());
    }
    let geometry = geometry.nonzero();
    if !force && tracked.configured == Some(geometry) {
      return Ok(());
    }
    let previous = tracked.info.geometry;
    tracked.configured = Some(geometry);
    tracked.info.geometry = geometry;
    self.connection.configure_window(window, geometry)?;
    if previous.width == geometry.width && previous.height == geometry.height {
      self.connection.send_configure_notify(window, geometry)?;
    }
    Ok(())
  }

  /// Sets Shingle's state of a managed window: fullscreen, maximized and
  /// hidden (minimized) are written to `_NET_WM_STATE` when they change.
  /// `focused` is ignored, since [`Xwm::set_focus`] decides
  /// `_NET_WM_STATE_FOCUSED`, and `_NET_WM_STATE_MODAL` is kept as the
  /// client set it.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  pub fn set_state(
    &mut self,
    window: Window,
    state: NetWmState,
  ) -> Result<(), XwmError> {
    let Some(tracked) = self.windows.get_mut(&window) else {
      return Ok(());
    };
    if tracked.info.override_redirect || !tracked.mapped {
      return Ok(());
    }
    tracked.wanted = Some(state);
    self.write_state(window)
  }

  /// Gives the keyboard focus to `window`, or takes it from every X
  /// window with `None`, as wlroots' `xwm_surface_activate` does: sets
  /// `_NET_ACTIVE_WINDOW` and the X focus for the window's input model
  /// (`SetInputFocus`, `WM_TAKE_FOCUS` or both), and moves
  /// `_NET_WM_STATE_FOCUSED`. Asking for the window focused already does
  /// nothing, as do override-redirect and unknown windows.
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn set_focus(&mut self, window: Option<Window>) -> Result<(), XwmError> {
    if self.requested_focus == window {
      return Ok(());
    }
    if let Some(window) = window
      && self
        .windows
        .get(&window)
        .is_none_or(|tracked| tracked.info.override_redirect)
    {
      return Ok(());
    }
    self.requested_focus = window;
    self.write_active()?;
    self.focus_window(window)
  }

  /// Offers the focus to a globally active window with `WM_TAKE_FOCUS`
  /// without focusing it, as wlroots' `wlr_xwayland_surface_offer_focus`
  /// does when Wayfire maps one; the client may take it for itself.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  pub fn offer_focus(&mut self, window: Window) -> Result<(), XwmError> {
    let Some(tracked) = self.windows.get(&window) else {
      return Ok(());
    };
    if tracked.info.override_redirect || !tracked.info.protocols.take_focus {
      return Ok(());
    }
    self.take_focus(window)
  }

  /// Restacks the managed windows to Shingle's stacking order, bottom to
  /// top. Windows Shingle does not list stay below in map order, and
  /// windows it lists that are not mapped are skipped. When the order
  /// changed, each window is stacked just above the one below it
  /// (`ConfigureWindow` with a sibling, wlroots'
  /// `wlr_xwayland_surface_restack`) and `_NET_CLIENT_LIST_STACKING` is
  /// rewritten.
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn restack(&mut self, order: &[Window]) -> Result<(), XwmError> {
    let mut stacking = self
      .client_list
      .iter()
      .filter(|window| !order.contains(window))
      .copied()
      .collect::<Vec<_>>();
    stacking.extend(
      order
        .iter()
        .filter(|window| self.client_list.contains(window)),
    );
    if stacking == self.stacking {
      return Ok(());
    }
    for pair in stacking.windows(2) {
      self.connection.stack_above(pair[1], Some(pair[0]))?;
    }
    self.stacking = stacking;
    self.write_stacking()
  }

  /// Makes `image` the root window's cursor, which X windows without a
  /// cursor of their own show (Wayfire's
  /// `xwayland_update_default_cursor`).
  ///
  /// # Errors
  ///
  /// When the server has no ARGB32 picture format, or a request fails.
  pub fn set_cursor(&mut self, image: &CursorImage) -> Result<(), XwmError> {
    self.connection.set_root_cursor(image)
  }

  /// Closes a window: `WM_DELETE_WINDOW` when it lists that protocol,
  /// else `KillClient`, as wlroots' `wlr_xwayland_surface_close` does.
  ///
  /// # Errors
  ///
  /// When the request cannot be queued.
  pub fn close(&mut self, window: Window) -> Result<(), XwmError> {
    let Some(tracked) = self.windows.get(&window) else {
      return Ok(());
    };
    if tracked.info.protocols.delete_window {
      let protocols = self.atoms.get(AtomName::WM_PROTOCOLS);
      let delete = self.atoms.get(AtomName::WM_DELETE_WINDOW);
      self.connection.send_client_message(
        window,
        protocols,
        [delete, x11rb::CURRENT_TIME, 0, 0, 0],
      )
    } else {
      self.connection.kill_client(window)
    }
  }

  /// Bridges `selections` between X and Wayland: creates a selection
  /// window for each and selects XFixes owner events for it, bring-up
  /// step 7. The compositor leaves `PRIMARY` out when
  /// `disable_primary_selection` is set.
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn bridge_selections(
    &mut self,
    selections: &[Selection],
  ) -> Result<(), XwmError> {
    self
      .selections
      .bridge(&mut self.connection, &self.atoms, selections)
  }

  /// The selections bridged.
  pub fn bridged_selections(&self) -> Vec<Selection> {
    self.selections.bridged()
  }

  /// The window that owns `selection` in X for Wayland, if it is bridged.
  pub fn selection_window(&self, selection: Selection) -> Option<Window> {
    self.selections.window(selection)
  }

  /// Reports the Wayland side of `selection`: the MIME types of a Wayland
  /// source, which the window manager then offers X clients by taking the
  /// X selection, or `None` when there is no selection or it comes from X,
  /// in which case an ownership of the window manager's own is given up.
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn set_selection(
    &mut self,
    selection: Selection,
    mime_types: Option<Vec<String>>,
  ) -> Result<(), XwmError> {
    self.selections.set_selection(
      &mut self.connection,
      &self.atoms,
      selection,
      mime_types,
    )
  }

  /// A Wayland client wants the X owner's `selection` as `mime_type`,
  /// written to `fd`, which is closed when the transfer ends, or at once
  /// when the owner does not offer the type.
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn receive_selection(
    &mut self,
    selection: Selection,
    mime_type: &str,
    fd: std::os::fd::OwnedFd,
  ) -> Result<(), XwmError> {
    self.selections.receive(
      &mut self.connection,
      &self.atoms,
      selection,
      mime_type,
      fd,
    )
  }

  /// The data requests X clients made for Wayland sources since the last
  /// call; the compositor passes each descriptor to its source's `send`.
  pub fn take_selection_sends(&mut self) -> Vec<SelectionSend> {
    self.selections.take_sends()
  }

  /// Moves data on every selection transfer as far as it can without
  /// blocking. The compositor calls it each turn, after
  /// [`Xwm::dispatch`].
  ///
  /// # Errors
  ///
  /// When the requests cannot be queued.
  pub fn pump_transfers(&mut self) -> Result<(), XwmError> {
    self.selections.pump(&mut self.connection, &self.atoms)
  }

  /// The descriptors the compositor watches so that the next turn comes
  /// when a transfer can make progress.
  pub fn transfer_watches(&self) -> Vec<TransferWatch<'_>> {
    self.selections.watches()
  }

  /// How many selection transfers run: to X, and from X.
  pub fn transfers(&self) -> (usize, usize) {
    self.selections.transfers()
  }

  /// What the window manager knows about `window`.
  pub fn window(&self, window: Window) -> Option<&WindowInfo> {
    self.windows.get(&window).map(|tracked| &tracked.info)
  }

  /// `_NET_CLIENT_LIST`: the mapped managed windows in map order.
  pub fn client_list(&self) -> &[Window] {
    &self.client_list
  }

  /// `_NET_CLIENT_LIST_STACKING`: the mapped managed windows, bottom to
  /// top.
  pub fn stacking(&self) -> &[Window] {
    &self.stacking
  }

  /// The window with the X focus, as far as the window manager knows.
  pub fn focused(&self) -> Option<Window> {
    self.focused
  }

  /// Sends the queued requests.
  ///
  /// # Errors
  ///
  /// When writing fails.
  pub fn flush(&mut self) -> Result<(), XwmError> {
    self.connection.flush()
  }

  /// The interned atoms.
  pub fn atoms(&self) -> &Atoms {
    &self.atoms
  }

  /// The root window.
  pub fn root(&self) -> Window {
    self.root
  }

  /// The window `_NET_SUPPORTING_WM_CHECK` names, which owns `WM_S0`.
  pub fn check_window(&self) -> Window {
    self.check_window
  }

  /// Whether the server ignores the window manager when deciding to exit
  /// for `-terminate`; `false` with XFixes older than 6.
  pub fn terminate_without_clients(&self) -> bool {
    self.terminate_without_clients
  }

  /// The connection.
  pub fn connection(&self) -> &C {
    &self.connection
  }
}

/// Whether an event with sequence number `event` came after the request
/// with `request`, with wrap-around, as wlroots' `validate_focus_serial`
/// decides.
fn sequence_after(request: u16, event: u16) -> bool {
  event.wrapping_sub(request) < u16::MAX / 2
}

#[cfg(test)]
mod tests;
