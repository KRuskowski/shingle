//! DRM timeline syncobjs on the render node (docs/design/dmabuf.md,
//! section 3): the [`Timeline`] and [`SyncobjDevice`] seams the protocol
//! code uses, and their implementation on a render node the compositor
//! opens itself. A render node gives no display control; the card node
//! is never opened.

use std::{
  io,
  os::fd::{AsFd, BorrowedFd, OwnedFd},
  path::PathBuf,
  sync::Arc,
};

use drm::control::{Device as _, syncobj};

/// A client's DRM timeline, imported once and shared by every point set
/// on it.
pub(crate) trait Timeline: Send + Sync {
  /// Whether `point` has signalled.
  fn signalled(&self, point: u64) -> io::Result<bool>;

  /// An eventfd that becomes readable once `point` has signalled, even if
  /// no fence for it has been submitted yet.
  fn eventfd(&self, point: u64) -> io::Result<OwnedFd>;

  /// Signals `point`, telling the client that the compositor is done.
  fn signal(&self, point: u64) -> io::Result<()>;
}

/// Turns clients' timeline fds into [`Timeline`]s.
pub(crate) trait SyncobjDevice {
  /// Imports the syncobj `fd`, which stays the client's.
  fn import(&self, fd: BorrowedFd<'_>) -> io::Result<Arc<dyn Timeline>>;
}

/// One point on a timeline: an acquire point to wait for or a release
/// point to signal.
#[derive(Clone)]
pub(crate) struct SyncPoint {
  /// The timeline.
  pub(crate) timeline: Arc<dyn Timeline>,
  /// The point's value.
  pub(crate) point: u64,
}

impl SyncPoint {
  /// Whether both points are on the same imported timeline object.
  pub(crate) fn same_timeline(&self, other: &SyncPoint) -> bool {
    Arc::ptr_eq(&self.timeline, &other.timeline)
  }
}

impl std::fmt::Debug for SyncPoint {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(formatter, "SyncPoint({})", self.point)
  }
}

/// The major number of a `dev_t`, in glibc's encoding.
pub(crate) fn dev_major(device: u64) -> u64 {
  ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff)
}

/// The minor number of a `dev_t`, in glibc's encoding.
pub(crate) fn dev_minor(device: u64) -> u64 {
  (device & 0xff) | ((device >> 12) & !0xff)
}

/// The `/dev` path of a render node from its sysfs `uevent`, which names
/// it as `DEVNAME=dri/renderDN`. Anything else, a card node in
/// particular, is refused.
fn render_node_path(uevent: &str) -> Result<PathBuf, String> {
  let name = uevent
    .lines()
    .find_map(|line| line.strip_prefix("DEVNAME="))
    .ok_or("the device's uevent has no DEVNAME")?;
  let number = name
    .strip_prefix("dri/renderD")
    .ok_or_else(|| format!("{name} is not a render node"))?;
  if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
    return Err(format!("{name} is not a render node"));
  }
  Ok(PathBuf::from("/dev").join(name))
}

/// The render node the compositor opened for syncobjs.
pub(crate) struct RenderNode {
  fd: OwnedFd,
  path: PathBuf,
}

impl AsFd for RenderNode {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.fd.as_fd()
  }
}

impl drm::Device for RenderNode {}

impl drm::control::Device for RenderNode {}

impl RenderNode {
  /// Opens the render node `device` (a `dev_t`), found through
  /// `/sys/dev/char/MAJOR:MINOR/uevent`, read-write and close-on-exec,
  /// and checks that the opened file is that device.
  pub(crate) fn open(device: u64) -> Result<Self, String> {
    let (major, minor) = (dev_major(device), dev_minor(device));
    let uevent = format!("/sys/dev/char/{major}:{minor}/uevent");
    let text = std::fs::read_to_string(&uevent)
      .map_err(|error| format!("reading {uevent}: {error}"))?;
    let path = render_node_path(&text)?;
    let fd = rustix::fs::open(
      &path,
      rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
      rustix::fs::Mode::empty(),
    )
    .map_err(|error| format!("opening {}: {error}", path.display()))?;
    let stat = rustix::fs::fstat(&fd)
      .map_err(|error| format!("fstat {}: {error}", path.display()))?;
    if stat.st_rdev != device {
      return Err(format!("{} is not device {major}:{minor}", path.display()));
    }
    Ok(Self { fd, path })
  }

  /// The node's path, for log lines.
  pub(crate) fn path(&self) -> &std::path::Path {
    &self.path
  }

  /// wlroots' test for what explicit sync needs: timeline syncobjs, and
  /// `DRM_IOCTL_SYNCOBJ_EVENTFD`, which fails with `ENOENT` on handle 0
  /// where it exists (types/wlr_linux_drm_syncobj_v1.c).
  pub(crate) fn check_syncobj(&self) -> Result<(), String> {
    use drm::Device as _;
    let timeline = self
      .get_driver_capability(drm::DriverCapability::TimelineSyncObj)
      .map_err(|error| format!("no timeline syncobj capability: {error}"))?;
    if timeline != 1 {
      return Err("the driver has no timeline syncobjs".into());
    }
    let eventfd = new_eventfd()?;
    match drm_ffi::syncobj::eventfd(self.as_fd(), 0, 0, eventfd.as_fd(), false)
    {
      Err(error)
        if error.raw_os_error()
          == Some(rustix::io::Errno::NOENT.raw_os_error()) =>
      {
        Ok(())
      }
      Err(error) => Err(format!("no syncobj eventfd: {error}")),
      Ok(_) => Err("the syncobj eventfd probe did not fail as expected".into()),
    }
  }
}

/// A non-blocking, close-on-exec eventfd.
fn new_eventfd() -> Result<OwnedFd, String> {
  rustix::event::eventfd(
    0,
    rustix::event::EventfdFlags::CLOEXEC
      | rustix::event::EventfdFlags::NONBLOCK,
  )
  .map_err(|error| format!("eventfd: {error}"))
}

impl SyncobjDevice for Arc<RenderNode> {
  fn import(&self, fd: BorrowedFd<'_>) -> io::Result<Arc<dyn Timeline>> {
    let handle = self.fd_to_syncobj(fd, false)?;
    Ok(Arc::new(DrmTimeline {
      node: Arc::clone(self),
      handle,
    }))
  }
}

/// A timeline imported into the render node; the handle is destroyed
/// with the last point that uses it.
pub(crate) struct DrmTimeline {
  node: Arc<RenderNode>,
  handle: syncobj::Handle,
}

impl DrmTimeline {
  /// A new timeline syncobj of the compositor's own, for tests that act
  /// as the client.
  #[cfg(test)]
  pub(crate) fn create(node: &Arc<RenderNode>) -> io::Result<Self> {
    Ok(Self {
      node: Arc::clone(node),
      handle: node.create_syncobj(false)?,
    })
  }

  /// The syncobj as an fd a client would send.
  #[cfg(test)]
  pub(crate) fn export(&self) -> io::Result<OwnedFd> {
    self.node.syncobj_to_fd(self.handle, false)
  }
}

impl Timeline for DrmTimeline {
  fn signalled(&self, point: u64) -> io::Result<bool> {
    // An absolute timeout of 0 only polls; waiting for submission turns a
    // point without a fence yet into ETIME rather than EINVAL.
    match self.node.syncobj_timeline_wait(
      &[self.handle],
      &[point],
      0,
      true,
      true,
      false,
    ) {
      Ok(_) => Ok(true),
      Err(error)
        if error.raw_os_error()
          == Some(rustix::io::Errno::TIME.raw_os_error()) =>
      {
        Ok(false)
      }
      Err(error) => Err(error),
    }
  }

  fn eventfd(&self, point: u64) -> io::Result<OwnedFd> {
    let eventfd = new_eventfd().map_err(io::Error::other)?;
    self
      .node
      .syncobj_eventfd(self.handle, point, eventfd.as_fd(), false)?;
    Ok(eventfd)
  }

  fn signal(&self, point: u64) -> io::Result<()> {
    self.node.syncobj_timeline_signal(&[self.handle], &[point])
  }
}

impl Drop for DrmTimeline {
  fn drop(&mut self) {
    let _ = self.node.destroy_syncobj(self.handle);
  }
}

#[cfg(test)]
mod tests {
  use std::{os::fd::AsFd, sync::Arc, time::Duration};

  use super::{
    DrmTimeline, RenderNode, SyncobjDevice, dev_major, dev_minor,
    render_node_path,
  };

  /// `makedev(226, 128)`.
  const RENDER_NODE: u64 = (226 << 8) | 128;

  #[test]
  fn dev_t_numbers_split_as_glibc_encodes_them() {
    assert_eq!((dev_major(RENDER_NODE), dev_minor(RENDER_NODE)), (226, 128));
    // makedev(259, 70000): both numbers above their low bits.
    let large = (259 << 8) | (70000 & 0xff) | ((70000 & !0xff) << 12);
    assert_eq!((dev_major(large), dev_minor(large)), (259, 70000));
  }

  #[test]
  fn only_render_nodes_are_opened() {
    assert_eq!(
      render_node_path("MAJOR=226\nMINOR=128\nDEVNAME=dri/renderD128\n"),
      Ok("/dev/dri/renderD128".into())
    );
    for uevent in [
      "MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\n",
      "DEVNAME=dri/renderD\n",
      "DEVNAME=dri/renderD12/../card0\n",
      "MAJOR=226\n",
    ] {
      assert!(render_node_path(uevent).is_err(), "{uevent:?}");
    }
  }

  #[test]
  fn opening_a_missing_device_fails() {
    // makedev(4095, 4095) exists on no host.
    let missing = (4095 << 8) | 0xff | ((4095 & !0xff) << 12);
    assert!(RenderNode::open(missing).is_err());
  }

  /// The render node, if this host has one with explicit sync support.
  fn render_node() -> Option<Arc<RenderNode>> {
    let node = match RenderNode::open(RENDER_NODE) {
      Ok(node) => node,
      Err(error) => {
        eprintln!("skipped: no render node 226:128: {error}");
        return None;
      }
    };
    if let Err(error) = node.check_syncobj() {
      eprintln!("skipped: {}: {error}", node.path().display());
      return None;
    }
    Some(Arc::new(node))
  }

  /// Whether `fd` polls readable within `timeout`.
  fn readable_within(
    fd: std::os::fd::BorrowedFd<'_>,
    timeout: Duration,
  ) -> bool {
    let mut fds = [rustix::event::PollFd::new(
      &fd,
      rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
      tv_sec: timeout.as_secs() as i64,
      tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    let polled = rustix::event::poll(&mut fds, Some(&timeout));
    matches!(polled, Ok(count) if count > 0)
  }

  /// On the device: a client's timeline, imported from its fd, reports
  /// points as unsignalled until they are, wakes an eventfd registered
  /// before the point had a fence, and is signalled by the compositor.
  #[test]
  fn a_timeline_on_the_render_node_waits_and_signals() {
    let Some(node) = render_node() else {
      return;
    };
    // The client's side: its own syncobj, sent as an fd.
    let client = DrmTimeline::create(&node).expect("a syncobj is created");
    let fd = client.export().expect("the syncobj exports");
    let timeline = node.import(fd.as_fd()).expect("the timeline imports");
    drop(fd);
    assert!(!timeline.signalled(1).expect("point 1 can be queried"));
    let eventfd = timeline.eventfd(2).expect("an eventfd registers");
    assert!(!readable_within(eventfd.as_fd(), Duration::ZERO));
    // The client signals point 1: the eventfd waits for 2.
    use super::Timeline as _;
    client.signal(1).expect("point 1 signals");
    assert!(timeline.signalled(1).expect("point 1 can be queried"));
    assert!(!timeline.signalled(2).expect("point 2 can be queried"));
    assert!(!readable_within(eventfd.as_fd(), Duration::from_millis(50)));
    client.signal(2).expect("point 2 signals");
    assert!(readable_within(eventfd.as_fd(), Duration::from_secs(1)));
    // The compositor signals a release point the client sees.
    timeline.signal(5).expect("the compositor signals point 5");
    assert!(client.signalled(5).expect("point 5 can be queried"));
  }

  #[test]
  fn a_bad_timeline_fd_does_not_import() {
    let Some(node) = render_node() else {
      return;
    };
    let not_a_syncobj = rustix::fs::memfd_create(
      "shingle-not-a-syncobj",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    assert!(node.import(not_a_syncobj.as_fd()).is_err());
  }
}
