//! Shingle's X window manager: the connection, atoms and bring-up.
//!
//! This is the start of the port of the wlroots XWM (`xwayland/xwm.c`)
//! that Wayfire relies on. It connects over Xwayland's `-wm` socket,
//! interns its atoms, redirects the root's children, takes the window
//! manager selections and advertises what it supports. Managing windows
//! follows in XW-3.

mod atoms;
mod connection;

pub use atoms::{Atoms, NAMES as ATOM_NAMES, Name as AtomName, SUPPORTED};
pub use connection::{
  ATOM_TYPE, COMPOSITE, WINDOW_TYPE, X11Connection, XConnection, XwmError,
};

use x11rb::protocol::{
  Event,
  xproto::{EventMask, Window},
};

/// The name the window manager gives itself in `_NET_WM_NAME` on its check
/// window.
pub const WM_NAME: &str = "Shingle";

/// The geometry of the check window, as wlroots creates it.
const CHECK_WINDOW_GEOMETRY: (i16, i16, u16, u16) = (0, 0, 10, 10);

/// Something the window manager saw that the compositor should know about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum XwmEvent {
  /// The server reported an error for one of the window manager's
  /// requests. It is only logged.
  RequestFailed(String),
}

/// The window manager on one Xwayland connection.
pub struct Xwm<C: XConnection> {
  connection: C,
  atoms: Atoms,
  root: Window,
  check_window: Window,
}

impl<C: XConnection> Xwm<C> {
  /// Brings the window manager up on `connection`:
  ///
  /// 1. interns [`ATOM_NAMES`] in one batch;
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
    })
  }

  /// Reads every event that has arrived, without waiting. Window events
  /// are not handled before XW-3, so only request errors are reported.
  ///
  /// # Errors
  ///
  /// When the connection failed or closed; the window manager is then
  /// unusable.
  pub fn dispatch(&mut self) -> Result<Vec<XwmEvent>, XwmError> {
    let mut events = Vec::new();
    while let Some(event) = self.connection.poll_for_event()? {
      if let Event::Error(error) = event {
        events.push(XwmEvent::RequestFailed(format!("{error:?}")));
      }
    }
    Ok(events)
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

  /// The connection.
  pub fn connection(&self) -> &C {
    &self.connection
  }
}

#[cfg(test)]
mod tests;
