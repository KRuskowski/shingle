//! What the window manager knows about one X window, and how it classifies
//! it, as a port of Wayfire's `determine_type`, `is_unmanaged` and
//! `is_dialog` (src/view/xwayland.cpp).

use x11rb::protocol::xproto::Window;

use super::{
  atoms::{Atoms, Name},
  props::{SizeHints, WmHints},
};

/// A rectangle in X root coordinates, which are Shingle's logical
/// coordinates (docs/design/xwayland.md, section 6).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Geometry {
  /// Left edge.
  pub x: i32,
  /// Top edge.
  pub y: i32,
  /// Width; never 0 in a request, which "would freeze xwayland"
  /// (Wayfire, src/view/xwayland/xwayland-toplevel.cpp).
  pub width: u32,
  /// Height; never 0 in a request.
  pub height: u32,
}

impl Geometry {
  /// A rectangle.
  pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
    Self {
      x,
      y,
      width,
      height,
    }
  }

  /// The same rectangle with width and height at least 1.
  pub fn nonzero(self) -> Self {
    Self {
      width: self.width.max(1),
      height: self.height.max(1),
      ..self
    }
  }
}

/// An `_NET_WM_WINDOW_TYPE` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowType {
  /// `_NET_WM_WINDOW_TYPE_NORMAL`.
  Normal,
  /// `_NET_WM_WINDOW_TYPE_DIALOG`.
  Dialog,
  /// `_NET_WM_WINDOW_TYPE_UTILITY`.
  Utility,
  /// `_NET_WM_WINDOW_TYPE_DND`.
  Dnd,
  /// `_NET_WM_WINDOW_TYPE_SPLASH`.
  Splash,
  /// `_NET_WM_WINDOW_TYPE_TOOLTIP`.
  Tooltip,
  /// `_NET_WM_WINDOW_TYPE_MENU`, `_DROPDOWN_MENU` or `_POPUP_MENU`.
  Menu,
  /// `_NET_WM_WINDOW_TYPE_COMBO`.
  Combo,
  /// `_NET_WM_WINDOW_TYPE_NOTIFICATION`.
  Notification,
  /// `_NET_WM_WINDOW_TYPE_DOCK`.
  Dock,
  /// `_NET_WM_WINDOW_TYPE_TOOLBAR`.
  Toolbar,
  /// `_NET_WM_WINDOW_TYPE_DESKTOP`.
  Desktop,
  /// Any other atom.
  Other(u32),
}

impl WindowType {
  /// The type an atom names.
  pub fn from_atom(atoms: &Atoms, atom: u32) -> Self {
    let table = [
      (Name::NET_WM_WINDOW_TYPE_NORMAL, Self::Normal),
      (Name::NET_WM_WINDOW_TYPE_DIALOG, Self::Dialog),
      (Name::NET_WM_WINDOW_TYPE_UTILITY, Self::Utility),
      (Name::NET_WM_WINDOW_TYPE_DND, Self::Dnd),
      (Name::NET_WM_WINDOW_TYPE_SPLASH, Self::Splash),
      (Name::NET_WM_WINDOW_TYPE_TOOLTIP, Self::Tooltip),
      (Name::NET_WM_WINDOW_TYPE_MENU, Self::Menu),
      (Name::NET_WM_WINDOW_TYPE_DROPDOWN_MENU, Self::Menu),
      (Name::NET_WM_WINDOW_TYPE_POPUP_MENU, Self::Menu),
      (Name::NET_WM_WINDOW_TYPE_COMBO, Self::Combo),
      (Name::NET_WM_WINDOW_TYPE_NOTIFICATION, Self::Notification),
      (Name::NET_WM_WINDOW_TYPE_DOCK, Self::Dock),
      (Name::NET_WM_WINDOW_TYPE_TOOLBAR, Self::Toolbar),
      (Name::NET_WM_WINDOW_TYPE_DESKTOP, Self::Desktop),
    ];
    table
      .into_iter()
      .find(|(name, _)| atoms.get(*name) == atom)
      .map_or(Self::Other(atom), |(_, kind)| kind)
  }
}

/// The `_NET_WM_STATE` atoms Shingle reads and writes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetWmState {
  /// `_NET_WM_STATE_FULLSCREEN`.
  pub fullscreen: bool,
  /// `_NET_WM_STATE_MAXIMIZED_VERT`.
  pub maximized_vert: bool,
  /// `_NET_WM_STATE_MAXIMIZED_HORZ`.
  pub maximized_horz: bool,
  /// `_NET_WM_STATE_HIDDEN`, which Shingle uses for minimized.
  pub hidden: bool,
  /// `_NET_WM_STATE_FOCUSED`.
  pub focused: bool,
  /// `_NET_WM_STATE_MODAL`.
  pub modal: bool,
}

impl NetWmState {
  /// The state an atom list describes; unknown atoms are ignored.
  pub fn parse(atoms: &Atoms, values: &[u32]) -> Self {
    let has = |name| values.contains(&atoms.get(name));
    Self {
      fullscreen: has(Name::NET_WM_STATE_FULLSCREEN),
      maximized_vert: has(Name::NET_WM_STATE_MAXIMIZED_VERT),
      maximized_horz: has(Name::NET_WM_STATE_MAXIMIZED_HORZ),
      hidden: has(Name::NET_WM_STATE_HIDDEN),
      focused: has(Name::NET_WM_STATE_FOCUSED),
      modal: has(Name::NET_WM_STATE_MODAL),
    }
  }

  /// The atom list for the property, in a fixed order.
  pub fn encode(&self, atoms: &Atoms) -> Vec<u32> {
    [
      (self.modal, Name::NET_WM_STATE_MODAL),
      (self.fullscreen, Name::NET_WM_STATE_FULLSCREEN),
      (self.maximized_vert, Name::NET_WM_STATE_MAXIMIZED_VERT),
      (self.maximized_horz, Name::NET_WM_STATE_MAXIMIZED_HORZ),
      (self.hidden, Name::NET_WM_STATE_HIDDEN),
      (self.focused, Name::NET_WM_STATE_FOCUSED),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .map(|(_, name)| atoms.get(name))
    .collect()
  }
}

/// The `WM_PROTOCOLS` Shingle acts on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Protocols {
  /// `WM_DELETE_WINDOW`: close asks instead of killing the client.
  pub delete_window: bool,
  /// `WM_TAKE_FOCUS`.
  pub take_focus: bool,
  /// `_NET_WM_PING`.
  pub ping: bool,
}

impl Protocols {
  /// The protocols an atom list names.
  pub fn parse(atoms: &Atoms, values: &[u32]) -> Self {
    let has = |name| values.contains(&atoms.get(name));
    Self {
      delete_window: has(Name::WM_DELETE_WINDOW),
      take_focus: has(Name::WM_TAKE_FOCUS),
      ping: has(Name::NET_WM_PING),
    }
  }
}

/// How Shingle treats a window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
  /// A toplevel Shingle manages: tiled, floated, listed in `list-views`.
  Managed {
    /// Wayfire's `is_dialog`: type `DIALOG`, or a parent and no type.
    dialog: bool,
  },
  /// Drawn where the client put it, never managed: override-redirect
  /// windows and Wayfire's parented popups.
  Unmanaged,
  /// A drag-and-drop icon (XW-6).
  DragIcon,
}

/// Everything the window manager has read about a window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowInfo {
  /// The geometry the server last reported.
  pub geometry: Geometry,
  /// The window bypasses the window manager.
  pub override_redirect: bool,
  /// The instance part of `WM_CLASS`.
  pub instance: String,
  /// The class part of `WM_CLASS`, which is Shingle's app ID.
  pub class: String,
  /// `_NET_WM_NAME`, else `WM_NAME`.
  pub title: String,
  /// `WM_TRANSIENT_FOR`, when it names a window the manager knows.
  pub transient_for: Option<Window>,
  /// `_NET_WM_WINDOW_TYPE`, in the client's order of preference.
  pub types: Vec<WindowType>,
  /// `WM_NORMAL_HINTS`.
  pub size_hints: SizeHints,
  /// `WM_HINTS`.
  pub hints: WmHints,
  /// `WM_PROTOCOLS`.
  pub protocols: Protocols,
  /// `_NET_WM_STATE` as the client set it.
  pub state: NetWmState,
  /// The client's PID, from XRes or else `_NET_WM_PID`.
  pub pid: Option<u32>,
  /// `_MOTIF_WM_HINTS` decorations.
  pub decorations: Option<bool>,
  /// `_NET_STARTUP_ID`.
  pub startup_id: Option<String>,
}

impl WindowInfo {
  /// Whether `_NET_WM_WINDOW_TYPE` lists `kind`.
  pub fn has_type(&self, kind: WindowType) -> bool {
    self.types.contains(&kind)
  }

  /// Wayfire's `is_dialog`.
  pub fn is_dialog(&self) -> bool {
    self.has_type(WindowType::Dialog)
      || (self.transient_for.is_some() && self.types.is_empty())
  }

  /// Wayfire's `determine_type`: drag icons by type, then unmanaged
  /// windows (override-redirect, or a parented non-dialog that is neither
  /// `NORMAL` nor `UTILITY`, such as Android Studio's popups), then
  /// managed ones.
  pub fn kind(&self) -> Kind {
    if self.has_type(WindowType::Dnd) {
      return Kind::DragIcon;
    }
    if self.override_redirect
      || (self.transient_for.is_some()
        && !self.is_dialog()
        && !self.has_type(WindowType::Normal)
        && !self.has_type(WindowType::Utility))
    {
      return Kind::Unmanaged;
    }
    Kind::Managed {
      dialog: self.is_dialog(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::xwm::atoms::NAMES;

  fn atoms() -> Atoms {
    Atoms::from_values((1000..1000 + NAMES.len() as u32).collect()).unwrap()
  }

  #[test]
  fn window_types_map_from_atoms() {
    let atoms = atoms();
    let atom = |name| atoms.get(name);
    assert_eq!(
      WindowType::from_atom(&atoms, atom(Name::NET_WM_WINDOW_TYPE_DIALOG)),
      WindowType::Dialog
    );
    assert_eq!(
      WindowType::from_atom(&atoms, atom(Name::NET_WM_WINDOW_TYPE_POPUP_MENU)),
      WindowType::Menu
    );
    assert_eq!(WindowType::from_atom(&atoms, 7), WindowType::Other(7));
  }

  #[test]
  fn net_wm_state_round_trips() {
    let atoms = atoms();
    let state = NetWmState {
      fullscreen: true,
      hidden: true,
      focused: true,
      ..NetWmState::default()
    };
    let encoded = state.encode(&atoms);
    assert_eq!(
      encoded,
      [
        atoms.get(Name::NET_WM_STATE_FULLSCREEN),
        atoms.get(Name::NET_WM_STATE_HIDDEN),
        atoms.get(Name::NET_WM_STATE_FOCUSED),
      ]
    );
    assert_eq!(NetWmState::parse(&atoms, &encoded), state);
    let mut with_unknown = encoded.clone();
    with_unknown.push(3);
    assert_eq!(NetWmState::parse(&atoms, &with_unknown), state);
    assert!(NetWmState::default().encode(&atoms).is_empty());
  }

  #[test]
  fn protocols_parse_from_atoms() {
    let atoms = atoms();
    let protocols = Protocols::parse(
      &atoms,
      &[
        atoms.get(Name::WM_DELETE_WINDOW),
        atoms.get(Name::NET_WM_PING),
      ],
    );
    assert!(protocols.delete_window && protocols.ping);
    assert!(!protocols.take_focus);
  }

  /// A window with the given types and parent.
  fn info(types: &[WindowType], parent: bool) -> WindowInfo {
    WindowInfo {
      types: types.to_vec(),
      transient_for: parent.then_some(0x400001),
      ..WindowInfo::default()
    }
  }

  #[test]
  fn classification_follows_wayfire() {
    use WindowType::*;
    // A plain window is a managed toplevel.
    assert_eq!(info(&[], false).kind(), Kind::Managed { dialog: false });
    assert_eq!(
      info(&[Normal], false).kind(),
      Kind::Managed { dialog: false }
    );
    // Dialogs: by type, or a parent and no type.
    assert_eq!(
      info(&[Dialog], false).kind(),
      Kind::Managed { dialog: true }
    );
    assert_eq!(info(&[], true).kind(), Kind::Managed { dialog: true });
    // A parented window that is normal or a utility stays managed.
    assert_eq!(
      info(&[Normal], true).kind(),
      Kind::Managed { dialog: false }
    );
    assert_eq!(
      info(&[Utility], true).kind(),
      Kind::Managed { dialog: false }
    );
    // A parented menu or splash is unmanaged, like Android Studio's
    // popups; without a parent it is managed.
    assert_eq!(info(&[Menu], true).kind(), Kind::Unmanaged);
    assert_eq!(info(&[Splash], true).kind(), Kind::Unmanaged);
    assert_eq!(
      info(&[Splash], false).kind(),
      Kind::Managed { dialog: false }
    );
    // Override-redirect is always unmanaged; DND type wins over all.
    let mut menu = info(&[], false);
    menu.override_redirect = true;
    assert_eq!(menu.kind(), Kind::Unmanaged);
    let mut icon = info(&[Dnd], false);
    icon.override_redirect = true;
    assert_eq!(icon.kind(), Kind::DragIcon);
  }

  #[test]
  fn geometry_is_never_empty_in_requests() {
    assert_eq!(
      Geometry::new(1, 2, 0, 0).nonzero(),
      Geometry::new(1, 2, 1, 1)
    );
    assert_eq!(
      Geometry::new(1, 2, 30, 40).nonzero(),
      Geometry::new(1, 2, 30, 40)
    );
  }
}
