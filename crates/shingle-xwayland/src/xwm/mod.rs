//! Shingle's X window manager.
//!
//! A port of the wlroots XWM (`xwayland/xwm.c`) that Wayfire relies on. It
//! connects over Xwayland's `-wm` socket, interns its atoms, redirects the
//! root's children, takes the window manager selections and advertises
//! what it supports. Then it tracks every child of the root, reads its
//! properties, maps managed windows when they ask, keeps `WM_STATE` and
//! the client lists, and reports what the compositor needs as
//! [`XwmEvent`]s. The compositor answers with [`Xwm::configure`],
//! [`Xwm::set_state`] and [`Xwm::close`]. Focus, the client messages
//! other than `WL_SURFACE_SERIAL` and stacking are XW-4.

mod atoms;
mod connection;
pub mod pairing;
pub mod policy;
pub mod props;
pub mod window;

use std::collections::HashMap;

pub use atoms::{Atoms, NAMES as ATOM_NAMES, Name as AtomName, SUPPORTED};
pub use connection::{
  ATOM_TYPE, COMPOSITE, WINDOW_TYPE, X11Connection, XConnection, XFIXES,
  XwmError,
};
pub use pairing::{Paired, Pairing};
pub use policy::{Placement, configure_reply};
pub use props::Property;
pub use window::{
  Geometry, Kind, NetWmState, Protocols, WindowInfo, WindowType,
};
pub use x11rb::protocol::xproto::Window;

use x11rb::protocol::{
  Event,
  xproto::{AtomEnum, ConfigWindow, EventMask},
};

use props::{SizeHints, WmHints};

/// The name the window manager gives itself in `_NET_WM_NAME` on its check
/// window.
pub const WM_NAME: &str = "Shingle";

/// The geometry of the check window, as wlroots creates it.
const CHECK_WINDOW_GEOMETRY: (i16, i16, u16, u16) = (0, 0, 10, 10);

/// ICCCM `WM_STATE` values.
const WITHDRAWN_STATE: u32 = 0;
const NORMAL_STATE: u32 = 1;

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
  warned_surface_id: bool,
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
  /// Then it flushes. The root cursor (XW-4) and the selection bridge
  /// (XW-5) are not set up yet.
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
      warned_surface_id: false,
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
        let gone = error.error_kind == x11rb::protocol::ErrorKind::Window
          && !self.windows.contains_key(&error.bad_value);
        if !gone {
          events.push(XwmEvent::RequestFailed(format!("{error:?}")));
        }
      }
      Event::CreateNotify(event) => {
        if event.window == self.check_window || event.parent != self.root {
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
        if !tracked.info.override_redirect {
          self.set_wm_state(window, WITHDRAWN_STATE)?;
          self.remove_client(window)?;
        }
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
          tracked.info.geometry = Geometry::new(
            event.x.into(),
            event.y.into(),
            event.width.into(),
            event.height.into(),
          );
          tracked.info.override_redirect = event.override_redirect;
        }
      }
      Event::PropertyNotify(event) => {
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
      Event::ClientMessage(event) => {
        let data = event.data.as_data32();
        if event.type_ == self.atoms.get(AtomName::WL_SURFACE_SERIAL) {
          let Some(tracked) = self.windows.get_mut(&event.window) else {
            return Ok(());
          };
          // A repeated serial for the same surface is ignored, as in
          // wlroots' xwm_handle_surface_serial_message.
          if tracked.serial.is_some() {
            return Ok(());
          }
          let serial = u64::from(data[0]) | (u64::from(data[1]) << 32);
          tracked.serial = Some(serial);
          events.push(XwmEvent::Serial {
            window: event.window,
            serial,
          });
        } else if self.atoms.name_of(event.type_).is_none()
          && !self.warned_surface_id
        {
          // Possibly WL_SURFACE_ID from an Xwayland before 23.1, which
          // Shingle does not support; other messages come in XW-4.
          self.warned_surface_id = true;
          events.push(XwmEvent::Notice(format!(
            "ignoring X client message type {}; if it is WL_SURFACE_ID, \
             this Xwayland is older than 23.1, which Shingle needs",
            event.type_
          )));
        }
      }
      _ => {}
    }
    Ok(())
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
    if self.client_list.len() != before {
      self.write_client_lists()?;
    }
    Ok(())
  }

  /// Writes `_NET_CLIENT_LIST` in map order and, until XW-4 restacks X
  /// windows, the same list as `_NET_CLIENT_LIST_STACKING`.
  fn write_client_lists(&mut self) -> Result<(), XwmError> {
    for list in [
      AtomName::NET_CLIENT_LIST,
      AtomName::NET_CLIENT_LIST_STACKING,
    ] {
      let atom = self.atoms.get(list);
      self.connection.change_property32(
        self.root,
        atom,
        WINDOW_TYPE,
        &self.client_list,
      )?;
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

  /// Writes `_NET_WM_STATE` for Shingle's state of a managed window, when
  /// it changed. `_NET_WM_STATE_MODAL` is kept as the client set it.
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
    let state = NetWmState {
      modal: tracked.info.state.modal,
      ..state
    };
    if tracked.state == Some(state) {
      return Ok(());
    }
    tracked.state = Some(state);
    tracked.info.state = state;
    let property = self.atoms.get(AtomName::NET_WM_STATE);
    let value = state.encode(&self.atoms);
    self
      .connection
      .change_property32(window, property, ATOM_TYPE, &value)
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

  /// What the window manager knows about `window`.
  pub fn window(&self, window: Window) -> Option<&WindowInfo> {
    self.windows.get(&window).map(|tracked| &tracked.info)
  }

  /// `_NET_CLIENT_LIST`: the mapped managed windows in map order.
  pub fn client_list(&self) -> &[Window] {
    &self.client_list
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

#[cfg(test)]
mod tests;
