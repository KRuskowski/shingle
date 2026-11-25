//! The atoms the window manager interns at bring-up.

/// Declares [`Name`] and [`NAMES`] from one list, so they cannot drift.
macro_rules! atoms {
  ($($variant:ident = $name:literal,)*) => {
    /// An atom the window manager uses, by its role.
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    #[allow(non_camel_case_types)]
    pub enum Name {
      $(
        #[doc = concat!("`", $name, "`.")]
        $variant,
      )*
    }

    /// Every atom name, in the order of [`Name`].
    pub const NAMES: &[&str] = &[$($name,)*];
  };
}

// wlroots' list (xwayland/xwm.c, `atom_map`), without `WL_SURFACE_ID`,
// which Shingle does not support, and the Xdnd atoms, which come with drag
// and drop (XW-6).
atoms! {
  WM_PROTOCOLS = "WM_PROTOCOLS",
  WM_HINTS = "WM_HINTS",
  WM_NORMAL_HINTS = "WM_NORMAL_HINTS",
  WM_SIZE_HINTS = "WM_SIZE_HINTS",
  WM_WINDOW_ROLE = "WM_WINDOW_ROLE",
  MOTIF_WM_HINTS = "_MOTIF_WM_HINTS",
  UTF8_STRING = "UTF8_STRING",
  WM_S0 = "WM_S0",
  NET_SUPPORTED = "_NET_SUPPORTED",
  NET_WM_CM_S0 = "_NET_WM_CM_S0",
  NET_WM_PID = "_NET_WM_PID",
  NET_WM_NAME = "_NET_WM_NAME",
  NET_WM_STATE = "_NET_WM_STATE",
  NET_WM_STRUT_PARTIAL = "_NET_WM_STRUT_PARTIAL",
  NET_WM_WINDOW_TYPE = "_NET_WM_WINDOW_TYPE",
  WM_TAKE_FOCUS = "WM_TAKE_FOCUS",
  WINDOW = "WINDOW",
  NET_ACTIVE_WINDOW = "_NET_ACTIVE_WINDOW",
  NET_CLOSE_WINDOW = "_NET_CLOSE_WINDOW",
  NET_WM_MOVERESIZE = "_NET_WM_MOVERESIZE",
  NET_SUPPORTING_WM_CHECK = "_NET_SUPPORTING_WM_CHECK",
  NET_WM_STATE_FOCUSED = "_NET_WM_STATE_FOCUSED",
  NET_WM_STATE_MODAL = "_NET_WM_STATE_MODAL",
  NET_WM_STATE_FULLSCREEN = "_NET_WM_STATE_FULLSCREEN",
  NET_WM_STATE_MAXIMIZED_VERT = "_NET_WM_STATE_MAXIMIZED_VERT",
  NET_WM_STATE_MAXIMIZED_HORZ = "_NET_WM_STATE_MAXIMIZED_HORZ",
  NET_WM_STATE_HIDDEN = "_NET_WM_STATE_HIDDEN",
  NET_WM_STATE_STICKY = "_NET_WM_STATE_STICKY",
  NET_WM_STATE_SHADED = "_NET_WM_STATE_SHADED",
  NET_WM_STATE_SKIP_TASKBAR = "_NET_WM_STATE_SKIP_TASKBAR",
  NET_WM_STATE_SKIP_PAGER = "_NET_WM_STATE_SKIP_PAGER",
  NET_WM_STATE_ABOVE = "_NET_WM_STATE_ABOVE",
  NET_WM_STATE_BELOW = "_NET_WM_STATE_BELOW",
  NET_WM_STATE_DEMANDS_ATTENTION = "_NET_WM_STATE_DEMANDS_ATTENTION",
  NET_WM_PING = "_NET_WM_PING",
  WM_CHANGE_STATE = "WM_CHANGE_STATE",
  WM_STATE = "WM_STATE",
  WM_DELETE_WINDOW = "WM_DELETE_WINDOW",
  CLIPBOARD = "CLIPBOARD",
  PRIMARY = "PRIMARY",
  WL_SELECTION = "_WL_SELECTION",
  TARGETS = "TARGETS",
  CLIPBOARD_MANAGER = "CLIPBOARD_MANAGER",
  INCR = "INCR",
  TEXT = "TEXT",
  TIMESTAMP = "TIMESTAMP",
  DELETE = "DELETE",
  NET_STARTUP_ID = "_NET_STARTUP_ID",
  NET_STARTUP_INFO = "_NET_STARTUP_INFO",
  NET_STARTUP_INFO_BEGIN = "_NET_STARTUP_INFO_BEGIN",
  NET_WM_WINDOW_TYPE_NORMAL = "_NET_WM_WINDOW_TYPE_NORMAL",
  NET_WM_WINDOW_TYPE_UTILITY = "_NET_WM_WINDOW_TYPE_UTILITY",
  NET_WM_WINDOW_TYPE_TOOLTIP = "_NET_WM_WINDOW_TYPE_TOOLTIP",
  NET_WM_WINDOW_TYPE_DND = "_NET_WM_WINDOW_TYPE_DND",
  NET_WM_WINDOW_TYPE_DROPDOWN_MENU = "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
  NET_WM_WINDOW_TYPE_POPUP_MENU = "_NET_WM_WINDOW_TYPE_POPUP_MENU",
  NET_WM_WINDOW_TYPE_COMBO = "_NET_WM_WINDOW_TYPE_COMBO",
  NET_WM_WINDOW_TYPE_MENU = "_NET_WM_WINDOW_TYPE_MENU",
  NET_WM_WINDOW_TYPE_NOTIFICATION = "_NET_WM_WINDOW_TYPE_NOTIFICATION",
  NET_WM_WINDOW_TYPE_SPLASH = "_NET_WM_WINDOW_TYPE_SPLASH",
  NET_WM_WINDOW_TYPE_DESKTOP = "_NET_WM_WINDOW_TYPE_DESKTOP",
  NET_WM_WINDOW_TYPE_DOCK = "_NET_WM_WINDOW_TYPE_DOCK",
  NET_WM_WINDOW_TYPE_TOOLBAR = "_NET_WM_WINDOW_TYPE_TOOLBAR",
  NET_WM_WINDOW_TYPE_DIALOG = "_NET_WM_WINDOW_TYPE_DIALOG",
  NET_CLIENT_LIST = "_NET_CLIENT_LIST",
  NET_CLIENT_LIST_STACKING = "_NET_CLIENT_LIST_STACKING",
  WL_SURFACE_SERIAL = "WL_SURFACE_SERIAL",
}

/// The interned values of every [`Name`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Atoms {
  values: Vec<u32>,
}

impl Atoms {
  /// Wraps the values the server returned for [`NAMES`], in that order.
  /// Returns `None` when the count does not match.
  pub fn from_values(values: Vec<u32>) -> Option<Self> {
    (values.len() == NAMES.len()).then_some(Self { values })
  }

  /// The value of `name`.
  pub fn get(&self, name: Name) -> u32 {
    self.values[name as usize]
  }

  /// The value of the atom called `name`, if it is one of [`NAMES`].
  pub fn by_name(&self, name: &str) -> Option<u32> {
    NAMES
      .iter()
      .position(|candidate| *candidate == name)
      .map(|index| self.values[index])
  }

  /// The name of an interned value, if it is one of [`NAMES`].
  pub fn name_of(&self, value: u32) -> Option<&'static str> {
    self
      .values
      .iter()
      .position(|candidate| *candidate == value)
      .map(|index| NAMES[index])
  }
}

/// `_NET_SUPPORTED`, wlroots' list (xwm.c, `xwm_create`).
pub const SUPPORTED: &[Name] = &[
  Name::NET_WM_STATE,
  Name::NET_ACTIVE_WINDOW,
  Name::NET_WM_MOVERESIZE,
  Name::NET_WM_STATE_FOCUSED,
  Name::NET_WM_STATE_MODAL,
  Name::NET_WM_STATE_FULLSCREEN,
  Name::NET_WM_STATE_MAXIMIZED_VERT,
  Name::NET_WM_STATE_MAXIMIZED_HORZ,
  Name::NET_WM_STATE_HIDDEN,
  Name::NET_CLIENT_LIST,
  Name::NET_CLIENT_LIST_STACKING,
];

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use super::*;

  #[test]
  fn names_are_unique_and_follow_the_enum() {
    let unique = NAMES.iter().collect::<HashSet<_>>();
    assert_eq!(unique.len(), NAMES.len());
    assert_eq!(NAMES[Name::WM_PROTOCOLS as usize], "WM_PROTOCOLS");
    assert_eq!(NAMES[Name::WL_SURFACE_SERIAL as usize], "WL_SURFACE_SERIAL");
    assert_eq!(NAMES[Name::NET_WM_NAME as usize], "_NET_WM_NAME");
  }

  #[test]
  fn wl_surface_id_and_xdnd_are_left_out() {
    assert!(!NAMES.contains(&"WL_SURFACE_ID"));
    assert!(!NAMES.iter().any(|name| name.starts_with("Xdnd")));
  }

  #[test]
  fn atoms_map_names_to_values_and_back() {
    let values = (100..100 + NAMES.len() as u32).collect::<Vec<_>>();
    let atoms = Atoms::from_values(values).unwrap();
    assert_eq!(atoms.get(Name::WM_PROTOCOLS), 100);
    assert_eq!(atoms.name_of(101), Some("WM_HINTS"));
    assert_eq!(atoms.name_of(5), None);
    assert_eq!(atoms.by_name("WM_HINTS"), Some(101));
    assert_eq!(atoms.by_name("text/plain"), None);
    assert!(Atoms::from_values(vec![1, 2]).is_none());
  }
}
