//! DRM hotplug: udev events that say the card's connectors changed.
//!
//! [`DeviceMonitor`] is the seam. [`UdevMonitor`] listens on a udev monitor
//! for subsystem `drm`, as wlroots' session does
//! (backend/session/session.c, `handle_udev_event`): a `change` event for
//! the card with `HOTPLUG=1` means connectors changed (with `CONNECTOR=ID`
//! when the kernel names one), and a `remove` event means the card went
//! away. [`classify`] is that decision as a pure function.
//!
//! [`UdevMonitor`] is compiled and checked everywhere but only ever created
//! on a machine whose seat Shingle may take (docs/native-testing.md); unit
//! tests use a fake monitor.

use std::{
  ffi::OsString,
  io,
  os::fd::{AsFd, BorrowedFd},
  path::Path,
};

/// What a udev event means for the card.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmEvent {
  /// Connectors changed; the card must be probed again. Holds the
  /// connector id the kernel named, if it named one.
  Hotplug(Option<u32>),
  /// The card was removed.
  Removed,
}

/// Where hotplug events come from.
pub trait DeviceMonitor {
  /// Reads every pending event without waiting and returns those about
  /// the card.
  fn dispatch(&mut self) -> Vec<DrmEvent>;

  /// The descriptor that becomes readable when events are pending.
  fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}

/// Decides what a udev event of subsystem `drm` means for the card named
/// `card` (such as `card0`): its action, the event device's name and its
/// `HOTPLUG` and `CONNECTOR` properties. Events for other devices, such as
/// connector devices (`card0-DP-1`) or another card, and changes that are
/// not hotplugs (leases) mean nothing.
pub fn classify(
  action: Option<&str>,
  sysname: &str,
  card: &str,
  hotplug: Option<&str>,
  connector: Option<&str>,
) -> Option<DrmEvent> {
  if sysname != card {
    return None;
  }
  match action? {
    "change" if hotplug == Some("1") => Some(DrmEvent::Hotplug(
      connector.and_then(|connector| connector.parse().ok()),
    )),
    "remove" => Some(DrmEvent::Removed),
    _ => None,
  }
}

/// The real monitor, on a udev netlink socket.
pub struct UdevMonitor {
  socket: ::udev::MonitorSocket,
  card: OsString,
}

impl UdevMonitor {
  /// Listens for DRM events about the card at `devnode`, such as
  /// `/dev/dri/card0`.
  pub fn new(devnode: &Path) -> io::Result<Self> {
    let card = devnode
      .file_name()
      .ok_or_else(|| io::Error::other("the card has no device name"))?
      .to_owned();
    let socket = ::udev::MonitorBuilder::new()?
      .match_subsystem("drm")?
      .listen()?;
    Ok(Self { socket, card })
  }
}

impl DeviceMonitor for UdevMonitor {
  fn dispatch(&mut self) -> Vec<DrmEvent> {
    let card = self.card.to_string_lossy();
    self
      .socket
      .iter()
      .filter_map(|event| {
        let text = |name: &str| {
          event
            .property_value(name)
            .map(|value| value.to_string_lossy().into_owned())
        };
        classify(
          event.action().and_then(|action| action.to_str()),
          &event.sysname().to_string_lossy(),
          &card,
          text("HOTPLUG").as_deref(),
          text("CONNECTOR").as_deref(),
        )
      })
      .collect()
  }

  fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
    Some(self.socket.as_fd())
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use std::{cell::RefCell, rc::Rc};

  use super::*;

  /// A monitor whose events the test queues.
  #[derive(Clone, Default)]
  pub(crate) struct FakeMonitor {
    pub(crate) events: Rc<RefCell<Vec<DrmEvent>>>,
  }

  impl DeviceMonitor for FakeMonitor {
    fn dispatch(&mut self) -> Vec<DrmEvent> {
      std::mem::take(&mut *self.events.borrow_mut())
    }

    fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
      None
    }
  }

  #[test]
  fn card_hotplugs_are_recognised_as_wlroots_does() {
    let event = |action, sysname, hotplug, connector| {
      classify(Some(action), sysname, "card1", hotplug, connector)
    };
    assert_eq!(
      event("change", "card1", Some("1"), None),
      Some(DrmEvent::Hotplug(None))
    );
    assert_eq!(
      event("change", "card1", Some("1"), Some("77")),
      Some(DrmEvent::Hotplug(Some(77)))
    );
    assert_eq!(
      event("remove", "card1", None, None),
      Some(DrmEvent::Removed)
    );
    // Leases, other cards and connector devices are not ours.
    assert_eq!(event("change", "card1", None, None), None);
    assert_eq!(event("change", "card0", Some("1"), None), None);
    assert_eq!(event("change", "card1-DP-1", Some("1"), None), None);
    assert_eq!(event("add", "card1", None, None), None);
    assert_eq!(classify(None, "card1", "card1", Some("1"), None), None);
  }
}
