//! A fake DRM card for unit tests: it keeps objects and properties in
//! memory, applies commits to them, records every commit and lets tests
//! reject commits and deliver page flips.

use std::{
  cell::RefCell,
  collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
  io,
  os::fd::BorrowedFd,
  rc::Rc,
  time::Duration,
};

use super::{
  AtomicRequest, BlobId, BufferId, CommitFlags, Connection, ConnectorId,
  ConnectorInfo, CrtcId, CrtcInfo, DumbFramebuffer, FORMAT_ARGB8888,
  FORMAT_XRGB8888, FramebufferId, KmsDevice, MODE_TYPE_PREFERRED, Mode, Object,
  PageFlip, PlanarFramebuffer, PlaneId, PlaneInfo, Property, PropertyId,
  Resources, Subpixel,
};
use crate::scanout::allocator::{AllocatedBuffer, BufferAllocator};

pub(crate) mod drm_info;

/// The fake card's GBM: it hands out memfds as buffers, with the first
/// modifier it is offered, and counts the buffers alive.
#[derive(Debug, Default)]
pub(crate) struct FakeGbm {
  /// Plane counts by modifier; modifiers not listed have one plane.
  pub(crate) plane_counts: HashMap<u64, Option<u32>>,
  /// Makes every allocation fail.
  pub(crate) fail: bool,
  /// The plane count of every buffer, whatever the modifier says.
  pub(crate) forced_planes: Option<u32>,
  /// The modifier list of every allocation.
  pub(crate) requests: Vec<Vec<u64>>,
  /// How many buffers are alive.
  pub(crate) live: usize,
  next_handle: u32,
}

/// Counts a fake buffer as alive until it is dropped.
struct Alive(Rc<RefCell<FakeGbm>>);

impl Drop for Alive {
  fn drop(&mut self) {
    self.0.borrow_mut().live -= 1;
  }
}

/// The fake card's [`BufferAllocator`].
pub(crate) struct FakeAllocator(pub(crate) Rc<RefCell<FakeGbm>>);

impl BufferAllocator for FakeAllocator {
  fn plane_count(&self, _fourcc: u32, modifier: u64) -> Option<u32> {
    self
      .0
      .borrow()
      .plane_counts
      .get(&modifier)
      .copied()
      .unwrap_or(Some(1))
  }

  fn allocate(
    &mut self,
    width: u32,
    height: u32,
    fourcc: u32,
    modifiers: &[u64],
  ) -> io::Result<AllocatedBuffer> {
    let mut gbm = self.0.borrow_mut();
    gbm.requests.push(modifiers.to_vec());
    if gbm.fail {
      return Err(io::ErrorKind::OutOfMemory.into());
    }
    let modifier = *modifiers.first().ok_or(io::ErrorKind::InvalidInput)?;
    let planes = gbm.forced_planes.unwrap_or(1);
    gbm.next_handle += 1;
    gbm.live += 1;
    let handle = BufferId(500 + gbm.next_handle);
    drop(gbm);
    let fd = rustix::fs::memfd_create(
      "shingle-fake-gbm",
      rustix::fs::MemfdFlags::CLOEXEC,
    )?;
    Ok(AllocatedBuffer {
      handle,
      fd,
      width,
      height,
      fourcc,
      modifier,
      stride: (width * 4).div_ceil(256) * 256,
      offset: 0,
      planes,
      keep: Box::new(Alive(Rc::clone(&self.0))),
    })
  }
}

/// A mode of `width` by `height` at `hz`, with timings that give exactly
/// that refresh rate.
pub(crate) fn mode(width: u16, height: u16, hz: u32, preferred: bool) -> Mode {
  // Totals that are multiples of 100 and 10 make the pixels per frame a
  // multiple of 1000, so the clock in kHz is exact.
  let htotal = (width + 200).div_ceil(100) * 100;
  let vtotal = (height + 50).div_ceil(10) * 10;
  let pixels = u32::from(htotal) * u32::from(vtotal);
  Mode {
    clock: pixels / 1000 * hz,
    hdisplay: width,
    hsync_start: width + 50,
    hsync_end: width + 100,
    htotal,
    vdisplay: height,
    vsync_start: height + 5,
    vsync_end: height + 10,
    vtotal,
    vrefresh: hz,
    kind: if preferred { MODE_TYPE_PREFERRED } else { 0 },
    name: format!("{width}x{height}"),
    ..Mode::default()
  }
}

/// A connected connector named `name` on no CRTC, with the default modes.
pub(crate) fn connector_info(id: u32, name: &str) -> ConnectorInfo {
  ConnectorInfo {
    id: ConnectorId(id),
    name: name.to_owned(),
    internal: false,
    connection: Connection::Connected,
    modes: default_modes(),
    physical_size_mm: (600, 340),
    subpixel: Subpixel::HorizontalRgb,
    current_crtc: None,
    possible_crtcs: Vec::new(),
    edid: None,
  }
}

/// The fake monitor's modes: 1280x720 preferred, then 1920x1080 and
/// 1024x768, all at exactly 60 Hz.
pub(crate) fn default_modes() -> Vec<Mode> {
  vec![
    mode(1280, 720, 60, true),
    mode(1920, 1080, 60, false),
    mode(1024, 768, 60, false),
  ]
}

/// Decides whether a commit fails, and with which errno.
pub(crate) type Reject =
  Box<dyn FnMut(&AtomicRequest, CommitFlags) -> Option<i32>>;

/// Rules of a real driver that the fake card enforces on top of the
/// kernel's own, as a fixture of that driver's card sets them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriverRules {
  /// i915 (and xe, which shares its display code) of a display version:
  /// 9 for Skylake to Comet Lake (UHD 620), 14 for Meteor Lake.
  I915 { display_version: u32 },
}

/// The fake card.
pub(crate) struct FakeKms {
  pub(crate) connector_id: ConnectorId,
  /// What the first connector is, apart from its id, connection, modes
  /// and CRTCs: its name, whether it is internal, its size.
  pub(crate) first_info: ConnectorInfo,
  pub(crate) crtc_id: CrtcId,
  pub(crate) plane_id: PlaneId,
  /// Connectors beyond the first, in order.
  extra_connectors: Vec<ConnectorInfo>,
  extra_crtcs: Vec<CrtcId>,
  extra_planes: Vec<(PlaneId, u64, Vec<CrtcId>)>,
  /// The first connector's modes and connection.
  pub(crate) modes: Vec<Mode>,
  pub(crate) connection: Connection,
  /// The CRTCs the first connector can use.
  pub(crate) connector_crtcs: Vec<CrtcId>,
  /// The CRTC's mode before Shingle sets one.
  pub(crate) crtc_mode: Option<Mode>,
  /// The primary plane's formats and `IN_FORMATS` blob.
  pub(crate) plane_formats: Vec<u32>,
  pub(crate) in_formats: Option<Vec<u8>>,
  /// Whether `enable_atomic` fails.
  pub(crate) no_atomic: bool,
  /// Whether the CRTC has `VRR_ENABLED`.
  pub(crate) vrr: bool,
  /// Every connector's `vrr_capable`.
  pub(crate) vrr_capable: bool,
  /// What `DRM_CAP_CURSOR_WIDTH` and `_HEIGHT` say.
  pub(crate) cursor_size: (u32, u32),
  /// Whether the driver takes `DRM_MODE_PAGE_FLIP_ASYNC`; commits with it
  /// fail with `EINVAL` otherwise.
  pub(crate) async_flips: bool,
  /// The framebuffers of ARGB8888 dumb buffers, for cursor planes.
  pub(crate) argb: BTreeSet<u32>,
  /// Property values by object id and name.
  values: BTreeMap<u32, BTreeMap<String, u64>>,
  property_ids: HashMap<String, PropertyId>,
  /// Every commit, with its flags and whether it was accepted.
  pub(crate) commits: Vec<(AtomicRequest, CommitFlags, bool)>,
  pub(crate) reject: Reject,
  /// Mode blobs that exist.
  pub(crate) blobs: BTreeMap<u32, Mode>,
  next_id: u32,
  /// Dumb buffers that exist, with their contents.
  pub(crate) buffers: BTreeMap<u32, (DumbFramebuffer, Vec<u8>)>,
  /// The CRTCs whose commit with `PAGE_FLIP_EVENT` awaits its event.
  pub(crate) pending: BTreeSet<CrtcId>,
  /// Events waiting to be read.
  pub(crate) events: VecDeque<PageFlip>,
  /// Whether dumb buffer allocation fails.
  pub(crate) no_buffers: bool,
  /// EDID blobs by connector.
  pub(crate) edids: HashMap<ConnectorId, Vec<u8>>,
  /// The card's GBM.
  pub(crate) gbm: Rc<RefCell<FakeGbm>>,
  /// Whether the card has no GBM device.
  pub(crate) no_gbm: bool,
  /// Framebuffers of GBM buffers that exist.
  pub(crate) framebuffers: BTreeMap<u32, PlanarFramebuffer>,
  /// Whether making a framebuffer of a GBM buffer fails.
  pub(crate) reject_framebuffers: bool,
  /// Formats of planes other than the primary plane, where they differ
  /// from the defaults.
  pub(crate) plane_format_lists: HashMap<PlaneId, Vec<u32>>,
  /// `IN_FORMATS` blobs of planes other than the primary plane.
  pub(crate) plane_in_formats: HashMap<PlaneId, Vec<u8>>,
  /// Property names of objects that have other properties than the
  /// defaults, as a real card's objects do.
  pub(crate) property_lists: HashMap<u32, Vec<String>>,
  /// The range of a range property, by object and name; values outside
  /// are refused as the kernel refuses them.
  pub(crate) ranges: HashMap<(u32, String), (u64, u64)>,
  /// The real driver's rules, if this is one's card.
  pub(crate) rules: Option<DriverRules>,
}

impl FakeKms {
  /// One connected `HDMI-A-1` (connector 1) that CRTC 2 with primary
  /// plane 3 can drive; nothing is lit yet.
  pub(crate) fn single_output() -> Self {
    Self {
      connector_id: ConnectorId(1),
      first_info: connector_info(1, "HDMI-A-1"),
      crtc_id: CrtcId(2),
      plane_id: PlaneId(3),
      extra_connectors: Vec::new(),
      extra_crtcs: Vec::new(),
      extra_planes: Vec::new(),
      modes: default_modes(),
      connection: Connection::Connected,
      connector_crtcs: vec![CrtcId(2)],
      crtc_mode: None,
      plane_formats: vec![FORMAT_XRGB8888],
      in_formats: None,
      no_atomic: false,
      vrr: false,
      vrr_capable: false,
      cursor_size: (64, 64),
      async_flips: false,
      argb: BTreeSet::new(),
      values: BTreeMap::new(),
      property_ids: HashMap::new(),
      commits: Vec::new(),
      reject: Box::new(|_, _| None),
      blobs: BTreeMap::new(),
      next_id: 100,
      buffers: BTreeMap::new(),
      pending: BTreeSet::new(),
      events: VecDeque::new(),
      no_buffers: false,
      edids: HashMap::new(),
      gbm: Rc::default(),
      no_gbm: false,
      framebuffers: BTreeMap::new(),
      reject_framebuffers: false,
      plane_format_lists: HashMap::new(),
      plane_in_formats: HashMap::new(),
      property_lists: HashMap::new(),
      ranges: HashMap::new(),
      rules: None,
    }
  }

  /// Shares the card, so a test can look at it while a display owns it.
  pub(crate) fn shared(self) -> Rc<RefCell<Self>> {
    Rc::new(RefCell::new(self))
  }

  pub(crate) fn add_connector(&mut self, mut info: ConnectorInfo) {
    if info.possible_crtcs.is_empty() {
      info.possible_crtcs = vec![self.crtc_id];
    }
    self.extra_connectors.push(info);
  }

  /// Unplugs the first connector.
  pub(crate) fn disconnect(&mut self) {
    self.connection = Connection::Disconnected;
  }

  /// Plugs a display into or out of the connector `name`.
  pub(crate) fn set_connection(&mut self, name: &str, connection: Connection) {
    if name == self.first_info.name {
      self.connection = connection;
      return;
    }
    let info = self
      .extra_connectors
      .iter_mut()
      .find(|info| info.name == name)
      .expect("a known connector");
    info.connection = connection;
  }

  /// The id of the connector `name`.
  pub(crate) fn connector_named(&self, name: &str) -> ConnectorId {
    if name == self.first_info.name {
      return self.connector_id;
    }
    self
      .extra_connectors
      .iter()
      .find(|info| info.name == name)
      .map(|info| info.id)
      .expect("a known connector")
  }

  /// Lights a second connector `DP-1` on a second CRTC, as the console
  /// may, and attaches a cursor plane to our CRTC. Returns their ids.
  pub(crate) fn add_lit_neighbours(
    &mut self,
  ) -> (ConnectorId, CrtcId, PlaneId) {
    let connector = ConnectorId(4);
    let crtc = CrtcId(5);
    let cursor = PlaneId(6);
    let mut info = connector_info(connector.0, "DP-1");
    info.possible_crtcs = vec![crtc];
    info.current_crtc = Some(crtc);
    self.extra_connectors.push(info);
    self.extra_crtcs.push(crtc);
    // A cursor plane (type 2) on our CRTC.
    self.extra_planes.push((cursor, 2, vec![self.crtc_id]));
    self.value_mut(connector.0, "CRTC_ID", u64::from(crtc.0));
    self.value_mut(crtc.0, "ACTIVE", 1);
    self.value_mut(cursor.0, "CRTC_ID", u64::from(self.crtc_id.0));
    (connector, crtc, cursor)
  }

  /// Removes connector `id` from the card, as the kernel removes a DP-MST
  /// connector when its dock is unplugged, and returns what it was. Its
  /// properties stay known, so that a commit may still turn it off, as
  /// the kernel allows for an unregistered connector that is still in
  /// use (drm_atomic_helper.c, `update_connector_routing`).
  pub(crate) fn unplug_connector(&mut self, id: ConnectorId) -> ConnectorInfo {
    let index = self
      .extra_connectors
      .iter()
      .position(|info| info.id == id)
      .expect("a connector beyond the first");
    self.extra_connectors.remove(index)
  }

  /// Adds `info` as a new connector shaped like connector `like`: its
  /// property names and ranges, on no CRTC, as the kernel creates a new
  /// DP-MST connector, with a new id, when a dock is plugged in again.
  pub(crate) fn plug_connector(&mut self, info: ConnectorInfo, like: u32) {
    if let Some(names) = self.property_lists.get(&like).cloned() {
      self.property_lists.insert(info.id.0, names);
    }
    let ranges: Vec<_> = self
      .ranges
      .iter()
      .filter(|((object, _), _)| *object == like)
      .map(|((_, name), range)| ((info.id.0, name.clone()), *range))
      .collect();
    self.ranges.extend(ranges);
    self.value_mut(info.id.0, "CRTC_ID", 0);
    self.extra_connectors.push(info);
  }

  /// Sets property `name` of `object` as the kernel would, such as a
  /// connector's `link-status` after a failed link training.
  pub(crate) fn set_value(&mut self, object: u32, name: &str, value: u64) {
    self.value_mut(object, name, value);
  }

  /// The connector `id` beyond the first, to change what it offers.
  pub(crate) fn connector_mut(
    &mut self,
    id: ConnectorId,
  ) -> &mut ConnectorInfo {
    self
      .extra_connectors
      .iter_mut()
      .find(|info| info.id == id)
      .expect("a connector beyond the first")
  }

  /// Rejects every commit whose primary plane is `width` pixels wide with
  /// `EINVAL`, as a driver refuses a mode it cannot drive.
  pub(crate) fn reject_width(&mut self, width: u32) {
    self.reject_widths(vec![width]);
  }

  /// Rejects commits for every listed plane width.
  pub(crate) fn reject_widths(&mut self, widths: Vec<u32>) {
    let crtc_w = self.property_id("CRTC_W");
    let plane = Object::Plane(self.plane_id);
    self.reject = Box::new(move |request, _| {
      request
        .get(plane, crtc_w)
        .filter(|width| widths.contains(&(*width as u32)))
        .map(|_| libc_einval())
    });
  }

  /// Rejects every commit with `errno`.
  pub(crate) fn reject_all(&mut self, errno: i32) {
    self.reject = Box::new(move |_, _| Some(errno));
  }

  /// Accepts every commit again.
  pub(crate) fn accept_all(&mut self) {
    self.reject = Box::new(|_, _| None);
  }

  /// Whether any CRTC has a flip pending.
  pub(crate) fn flip_pending(&self) -> bool {
    !self.pending.is_empty()
  }

  /// Completes the one pending flip at `time` with vblank `sequence`.
  pub(crate) fn complete_flip(&mut self, time: Duration, sequence: u32) {
    assert_eq!(self.pending.len(), 1, "one flip is pending");
    let crtc = *self.pending.first().expect("a pending flip");
    self.complete_flip_on(crtc, time, sequence);
  }

  /// Completes the flip pending on `crtc`.
  pub(crate) fn complete_flip_on(
    &mut self,
    crtc: CrtcId,
    time: Duration,
    sequence: u32,
  ) {
    assert!(self.pending.remove(&crtc), "no flip is pending on {crtc:?}");
    self.events.push_back(PageFlip {
      crtc,
      sequence,
      time,
    });
  }

  /// Loses every pending flip's event, as a hung driver does.
  pub(crate) fn lose_flip(&mut self) {
    self.pending.clear();
  }

  /// Adds a cursor plane that only `crtc` can use, and returns it.
  pub(crate) fn add_cursor_plane(&mut self, crtc: CrtcId) -> PlaneId {
    self.add_shared_cursor_plane(vec![crtc])
  }

  /// Adds a cursor plane that each of `crtcs` can use, and returns it.
  pub(crate) fn add_shared_cursor_plane(
    &mut self,
    crtcs: Vec<CrtcId>,
  ) -> PlaneId {
    let plane = PlaneId(self.alloc());
    self.extra_planes.push((plane, 2, crtcs));
    plane
  }

  /// Adds a CRTC with a primary plane of its own, and returns both ids.
  pub(crate) fn add_crtc(&mut self) -> (CrtcId, PlaneId) {
    let crtc = CrtcId(self.alloc());
    let plane = PlaneId(self.alloc());
    self.extra_crtcs.push(crtc);
    self.extra_planes.push((plane, 1, vec![crtc]));
    (crtc, plane)
  }

  /// Adds connector `info` with a CRTC and primary plane of its own that
  /// only it can use, as a card with one CRTC per port has. Returns the
  /// CRTC.
  pub(crate) fn add_pipe(&mut self, mut info: ConnectorInfo) -> CrtcId {
    let (crtc, _) = self.add_crtc();
    info.possible_crtcs = vec![crtc];
    self.extra_connectors.push(info);
    crtc
  }

  /// The CRTCs a request touches: those it sets, and those of the planes
  /// and connectors it sets, as they are or will be attached.
  fn touched(
    &self,
    request: &AtomicRequest,
    names: &HashMap<PropertyId, String>,
  ) -> BTreeSet<CrtcId> {
    let mut crtcs = BTreeSet::new();
    for (object, property, value) in request.entries() {
      let name = names.get(property).map(String::as_str);
      match (object, name) {
        (Object::Crtc(crtc), _) => {
          crtcs.insert(*crtc);
        }
        (Object::Plane(_) | Object::Connector(_), Some("CRTC_ID")) => {
          if *value != 0 {
            crtcs.insert(CrtcId(*value as u32));
          }
          let now = self.value(object.id(), "CRTC_ID");
          if now != 0 {
            crtcs.insert(CrtcId(now as u32));
          }
        }
        (Object::Plane(plane), _) => {
          let now = self.value(plane.0, "CRTC_ID");
          if now != 0 {
            crtcs.insert(CrtcId(now as u32));
          }
        }
        _ => {}
      }
    }
    crtcs
  }

  /// What the kernel, and the driver's rules if any, refuse in
  /// `request`: a property the object does not have or a value outside
  /// its range (`drm_atomic_set_property`), and in an async flip any
  /// change but the primary plane's `FB_ID`, `IN_FENCE_FD` and
  /// `FB_DAMAGE_CLIPS` (drm_atomic_uapi.c, `drm_atomic_set_property`).
  fn refuse(
    &self,
    request: &AtomicRequest,
    flags: CommitFlags,
    names: &HashMap<PropertyId, String>,
  ) -> Option<i32> {
    let einval = Some(libc_einval());
    let changes_allowed = |object: &Object, name: &str| {
      matches!(object, Object::Plane(plane) if self.plane_type(*plane) == 1)
        && matches!(name, "FB_ID" | "IN_FENCE_FD" | "FB_DAMAGE_CLIPS")
    };
    for (object, property, value) in request.entries() {
      let Some(name) = names.get(property) else {
        return einval;
      };
      if !self.property_names(*object).contains(name) {
        return einval;
      }
      if let Some((min, max)) = self.ranges.get(&(object.id(), name.clone()))
        && !(*min..=*max).contains(value)
      {
        return einval;
      }
      if flags.contains(CommitFlags::PAGE_FLIP_ASYNC)
        && !changes_allowed(object, name)
        && self.value(object.id(), name) != *value
      {
        return einval;
      }
    }
    match self.rules {
      Some(DriverRules::I915 { display_version }) => {
        self.refuse_i915(request, flags, names, display_version)
      }
      None => None,
    }
  }

  /// What i915 refuses beyond the kernel's rules: async flips of buffers
  /// it cannot flip async (intel_display.c, `intel_async_flip_check_hw`:
  /// `LINEAR` before display version 12, and every CCS modifier of these
  /// generations), and a cursor plane of another size than 64, 128 or 256
  /// pixels wide and 8 to that many lines high (as many from display
  /// version 14 on, which has no `HAS_CUR_FBC`), scaled, or with a stride
  /// other than its width in pixels (intel_cursor.c, `i9xx_check_cursor`).
  fn refuse_i915(
    &self,
    request: &AtomicRequest,
    flags: CommitFlags,
    names: &HashMap<PropertyId, String>,
    display_version: u32,
  ) -> Option<i32> {
    let einval = Some(libc_einval());
    let set = |object: Object, name: &str| {
      request
        .entries()
        .iter()
        .find(|(o, p, _)| {
          *o == object && names.get(p).is_some_and(|n| n == name)
        })
        .map(|(_, _, value)| *value)
    };
    let after = |object: Object, name: &str| {
      set(object, name).unwrap_or_else(|| self.value(object.id(), name))
    };
    let planes: BTreeSet<PlaneId> = request
      .entries()
      .iter()
      .filter_map(|(object, ..)| match object {
        Object::Plane(plane) => Some(*plane),
        _ => None,
      })
      .collect();
    for plane in planes {
      let object = Object::Plane(plane);
      let framebuffer = after(object, "FB_ID") as u32;
      if framebuffer == 0 {
        continue;
      }
      let (modifier, stride) = match self.framebuffers.get(&framebuffer) {
        Some(buffer) => (buffer.modifier, buffer.stride),
        None => match self.buffers.get(&framebuffer) {
          Some((buffer, _)) => (super::MODIFIER_LINEAR, buffer.stride),
          None => continue,
        },
      };
      if flags.contains(CommitFlags::PAGE_FLIP_ASYNC)
        && set(object, "FB_ID").is_some()
      {
        let flippable = match modifier {
          super::MODIFIER_LINEAR => display_version >= 12,
          // X, Y, Yf and 4 tiling.
          0x0100_0000_0000_0001..=0x0100_0000_0000_0003
          | 0x0100_0000_0000_0009 => true,
          _ => false,
        };
        if !flippable {
          return einval;
        }
      }
      if self.plane_type(plane) == 2 {
        let width = after(object, "CRTC_W");
        let height = after(object, "CRTC_H");
        let scaled = after(object, "SRC_W") != width << 16
          || after(object, "SRC_H") != height << 16;
        let lowest = if display_version >= 14 { width } else { 8 };
        if ![64, 128, 256].contains(&width)
          || !(lowest..=width).contains(&height)
          || scaled
          || u64::from(stride) != width * 4
          || !self.argb.contains(&framebuffer)
        {
          return einval;
        }
      }
    }
    None
  }

  /// The commits that were not `TEST_ONLY`.
  pub(crate) fn real_commits(&self) -> Vec<(AtomicRequest, CommitFlags)> {
    self
      .commits
      .iter()
      .filter(|(_, flags, ok)| *ok && !flags.contains(CommitFlags::TEST_ONLY))
      .map(|(request, flags, _)| (request.clone(), *flags))
      .collect()
  }

  /// The current value of `name` on `object`.
  pub(crate) fn value(&self, object: u32, name: &str) -> u64 {
    self
      .values
      .get(&object)
      .and_then(|values| values.get(name))
      .copied()
      .unwrap_or(0)
  }

  /// The id of property `name`, shared by every object as in DRM.
  pub(crate) fn property_id(&mut self, name: &str) -> PropertyId {
    let next = PropertyId(1000 + self.property_ids.len() as u32);
    *self.property_ids.entry(name.to_owned()).or_insert(next)
  }

  fn value_mut(&mut self, object: u32, name: &str, value: u64) {
    self
      .values
      .entry(object)
      .or_default()
      .insert(name.to_owned(), value);
  }

  fn property_names(&self, object: Object) -> Vec<String> {
    if let Some(names) = self.property_lists.get(&object.id()) {
      return names.clone();
    }
    let names: Vec<&str> = match object {
      Object::Connector(_) => vec!["CRTC_ID", "DPMS", "EDID", "vrr_capable"],
      Object::Crtc(_) => {
        let mut names = vec!["MODE_ID", "ACTIVE", "GAMMA_LUT"];
        if self.vrr {
          names.push("VRR_ENABLED");
        }
        names
      }
      Object::Plane(_) => vec![
        "type",
        "FB_ID",
        "CRTC_ID",
        "SRC_X",
        "SRC_Y",
        "SRC_W",
        "SRC_H",
        "CRTC_X",
        "CRTC_Y",
        "CRTC_W",
        "CRTC_H",
        "IN_FORMATS",
        "IN_FENCE_FD",
      ],
    };
    names.into_iter().map(str::to_owned).collect()
  }

  fn alloc(&mut self) -> u32 {
    self.next_id += 1;
    self.next_id
  }

  fn plane_type(&self, id: PlaneId) -> u64 {
    if id == self.plane_id {
      return 1;
    }
    self
      .extra_planes
      .iter()
      .find(|(plane, ..)| *plane == id)
      .map_or(0, |(_, kind, _)| *kind)
  }
}

/// `EINVAL`.
pub(crate) fn libc_einval() -> i32 {
  22
}

/// The blob ids of other planes' `IN_FORMATS` start here.
const IN_FORMATS_BLOBS: u64 = 90_000;

impl KmsDevice for FakeKms {
  fn enable_atomic(&mut self) -> io::Result<()> {
    if self.no_atomic {
      return Err(io::Error::from_raw_os_error(95));
    }
    Ok(())
  }

  fn monotonic_timestamps(&mut self) -> bool {
    true
  }

  fn resources(&mut self) -> io::Result<Resources> {
    let mut connectors = vec![self.connector_id];
    connectors.extend(self.extra_connectors.iter().map(|info| info.id));
    let mut crtcs = vec![self.crtc_id];
    crtcs.extend(&self.extra_crtcs);
    let mut planes = vec![self.plane_id];
    planes.extend(self.extra_planes.iter().map(|(id, ..)| *id));
    Ok(Resources {
      connectors,
      crtcs,
      planes,
    })
  }

  fn connector(&mut self, id: ConnectorId) -> io::Result<ConnectorInfo> {
    if id == self.connector_id {
      let crtc = self.value(id.0, "CRTC_ID");
      let mut info = self.first_info.clone();
      info.id = id;
      info.connection = self.connection;
      info.modes = if self.connection == Connection::Connected {
        self.modes.clone()
      } else {
        Vec::new()
      };
      info.possible_crtcs = self.connector_crtcs.clone();
      info.current_crtc = (crtc != 0).then_some(CrtcId(crtc as u32));
      return Ok(info);
    }
    let crtc = self.value(id.0, "CRTC_ID");
    let mut info = self
      .extra_connectors
      .iter()
      .find(|info| info.id == id)
      .cloned()
      .ok_or(io::ErrorKind::NotFound)?;
    info.current_crtc = (crtc != 0).then_some(CrtcId(crtc as u32));
    Ok(info)
  }

  fn crtc(&mut self, id: CrtcId) -> io::Result<CrtcInfo> {
    let blob = self.value(id.0, "MODE_ID") as u32;
    let mode = match self.blobs.get(&blob) {
      Some(mode) => Some(mode.clone()),
      None if id == self.crtc_id => self.crtc_mode.clone(),
      None => None,
    };
    Ok(CrtcInfo { id, mode })
  }

  fn plane(&mut self, id: PlaneId) -> io::Result<PlaneInfo> {
    let crtc = self.value(id.0, "CRTC_ID");
    let possible_crtcs = if id == self.plane_id {
      vec![self.crtc_id]
    } else {
      self
        .extra_planes
        .iter()
        .find(|(plane, ..)| *plane == id)
        .map(|(_, _, crtcs)| crtcs.clone())
        .unwrap_or_default()
    };
    // Cursor planes scan out ARGB8888.
    let formats = if let Some(formats) = self.plane_format_lists.get(&id) {
      formats.clone()
    } else if self.plane_type(id) == 2 {
      vec![FORMAT_ARGB8888]
    } else {
      self.plane_formats.clone()
    };
    Ok(PlaneInfo {
      id,
      possible_crtcs,
      current_crtc: (crtc != 0).then_some(CrtcId(crtc as u32)),
      formats,
    })
  }

  fn properties(&mut self, object: Object) -> io::Result<Vec<Property>> {
    let id = object.id();
    let mut properties = Vec::new();
    for name in self.property_names(object) {
      let value = match (object, name.as_str()) {
        (Object::Plane(plane), "type") => self.plane_type(plane),
        (Object::Plane(plane), "IN_FORMATS") if plane == self.plane_id => {
          u64::from(self.in_formats.is_some()) * 77
        }
        (Object::Plane(plane), "IN_FORMATS")
          if self.plane_in_formats.contains_key(&plane) =>
        {
          IN_FORMATS_BLOBS + u64::from(plane.0)
        }
        (Object::Connector(connector), "EDID")
          if self.edids.contains_key(&connector) =>
        {
          5000 + u64::from(connector.0)
        }
        (Object::Connector(_), "vrr_capable")
          if !self.property_lists.contains_key(&id) =>
        {
          u64::from(self.vrr_capable)
        }
        _ => self.value(id, &name),
      };
      properties.push(Property {
        id: self.property_id(&name),
        range: self.ranges.get(&(id, name.clone())).copied(),
        name,
        value,
      });
    }
    Ok(properties)
  }

  fn blob(&mut self, id: u64) -> io::Result<Vec<u8>> {
    if let Some(edid) = id
      .checked_sub(5000)
      .and_then(|connector| u32::try_from(connector).ok())
      .and_then(|connector| self.edids.get(&ConnectorId(connector)))
    {
      return Ok(edid.clone());
    }
    if let Some(blob) = id
      .checked_sub(IN_FORMATS_BLOBS)
      .and_then(|plane| u32::try_from(plane).ok())
      .and_then(|plane| self.plane_in_formats.get(&PlaneId(plane)))
    {
      return Ok(blob.clone());
    }
    match (id, &self.in_formats) {
      (77, Some(blob)) => Ok(blob.clone()),
      _ => Err(io::ErrorKind::NotFound.into()),
    }
  }

  fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<BlobId> {
    let id = self.alloc();
    self.blobs.insert(id, mode.clone());
    Ok(BlobId(id))
  }

  fn destroy_blob(&mut self, blob: BlobId) -> io::Result<()> {
    self
      .blobs
      .remove(&blob.0)
      .map(|_| ())
      .ok_or_else(|| io::ErrorKind::NotFound.into())
  }

  fn commit(
    &mut self,
    request: &AtomicRequest,
    flags: CommitFlags,
  ) -> io::Result<()> {
    let names: HashMap<PropertyId, String> = self
      .property_ids
      .iter()
      .map(|(name, id)| (*id, name.clone()))
      .collect();
    let mut error = (self.reject)(request, flags);
    if error.is_none() {
      error = self.refuse(request, flags, &names);
    }
    let touched = self.touched(request, &names);
    if error.is_none()
      && !touched.is_disjoint(&self.pending)
      && !flags.contains(CommitFlags::TEST_ONLY)
    {
      // The kernel allows one pending commit per CRTC.
      error = Some(16);
    }
    // Modes and routing need ALLOW_MODESET; turning planes on and off
    // does not.
    let modeset = request.entries().iter().any(|(object, property, _)| {
      matches!(
        (object, names.get(property).map(String::as_str)),
        (Object::Crtc(_), Some("MODE_ID" | "ACTIVE"))
          | (Object::Connector(_), Some("CRTC_ID"))
      )
    });
    if error.is_none()
      && flags.contains(CommitFlags::PAGE_FLIP_ASYNC)
      && !self.async_flips
    {
      error = Some(libc_einval());
    }
    if error.is_none() && modeset && !flags.contains(CommitFlags::ALLOW_MODESET)
    {
      error = Some(libc_einval());
    }
    self.commits.push((request.clone(), flags, error.is_none()));
    if let Some(errno) = error {
      return Err(io::Error::from_raw_os_error(errno));
    }
    if flags.contains(CommitFlags::TEST_ONLY) {
      return Ok(());
    }
    for (object, property, value) in request.entries() {
      let name = names.get(property).cloned().unwrap_or_default();
      self.value_mut(object.id(), &name, *value);
    }
    if flags.contains(CommitFlags::PAGE_FLIP_EVENT) {
      self.pending.extend(touched);
    }
    Ok(())
  }

  fn create_dumb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer> {
    if self.no_buffers {
      return Err(io::ErrorKind::OutOfMemory.into());
    }
    let buffer = BufferId(self.alloc());
    let framebuffer = FramebufferId(self.alloc());
    // A stride with padding, as drivers align rows.
    let stride = (width * 4).div_ceil(64) * 64;
    let fb = DumbFramebuffer {
      buffer,
      framebuffer,
      width,
      height,
      stride,
    };
    self
      .buffers
      .insert(framebuffer.0, (fb, vec![0; (stride * height) as usize]));
    Ok(fb)
  }

  fn create_argb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer> {
    let buffer = self.create_dumb_framebuffer(width, height)?;
    self.argb.insert(buffer.framebuffer.0);
    Ok(buffer)
  }

  fn cursor_size(&mut self) -> (u32, u32) {
    self.cursor_size
  }

  fn async_page_flips(&mut self) -> bool {
    self.async_flips
  }

  fn write_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
    write: &mut dyn FnMut(&mut [u8]),
  ) -> io::Result<()> {
    let (_, pixels) = self
      .buffers
      .get_mut(&buffer.framebuffer.0)
      .ok_or(io::ErrorKind::NotFound)?;
    write(pixels);
    Ok(())
  }

  fn destroy_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
  ) -> io::Result<()> {
    self.argb.remove(&buffer.framebuffer.0);
    self
      .buffers
      .remove(&buffer.framebuffer.0)
      .map(|_| ())
      .ok_or_else(|| io::ErrorKind::NotFound.into())
  }

  fn add_framebuffer(
    &mut self,
    buffer: &PlanarFramebuffer,
  ) -> io::Result<FramebufferId> {
    if self.reject_framebuffers {
      return Err(io::Error::from_raw_os_error(libc_einval()));
    }
    let framebuffer = FramebufferId(self.alloc());
    self.framebuffers.insert(framebuffer.0, *buffer);
    Ok(framebuffer)
  }

  fn remove_framebuffer(
    &mut self,
    framebuffer: FramebufferId,
  ) -> io::Result<()> {
    self
      .framebuffers
      .remove(&framebuffer.0)
      .map(|_| ())
      .ok_or_else(|| io::ErrorKind::NotFound.into())
  }

  fn buffer_allocator(&mut self) -> io::Result<Box<dyn BufferAllocator>> {
    if self.no_gbm {
      return Err(io::Error::from_raw_os_error(19));
    }
    Ok(Box::new(FakeAllocator(Rc::clone(&self.gbm))))
  }

  fn read_events(&mut self) -> io::Result<Vec<PageFlip>> {
    Ok(self.events.drain(..).collect())
  }

  fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
    None
  }
}

/// A shared fake, so the test keeps a handle while a display owns it.
impl KmsDevice for Rc<RefCell<FakeKms>> {
  fn enable_atomic(&mut self) -> io::Result<()> {
    self.borrow_mut().enable_atomic()
  }

  fn monotonic_timestamps(&mut self) -> bool {
    self.borrow_mut().monotonic_timestamps()
  }

  fn resources(&mut self) -> io::Result<Resources> {
    self.borrow_mut().resources()
  }

  fn connector(&mut self, id: ConnectorId) -> io::Result<ConnectorInfo> {
    self.borrow_mut().connector(id)
  }

  fn crtc(&mut self, id: CrtcId) -> io::Result<CrtcInfo> {
    self.borrow_mut().crtc(id)
  }

  fn plane(&mut self, id: PlaneId) -> io::Result<PlaneInfo> {
    self.borrow_mut().plane(id)
  }

  fn properties(&mut self, object: Object) -> io::Result<Vec<Property>> {
    self.borrow_mut().properties(object)
  }

  fn blob(&mut self, id: u64) -> io::Result<Vec<u8>> {
    self.borrow_mut().blob(id)
  }

  fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<BlobId> {
    self.borrow_mut().create_mode_blob(mode)
  }

  fn destroy_blob(&mut self, blob: BlobId) -> io::Result<()> {
    self.borrow_mut().destroy_blob(blob)
  }

  fn commit(
    &mut self,
    request: &AtomicRequest,
    flags: CommitFlags,
  ) -> io::Result<()> {
    self.borrow_mut().commit(request, flags)
  }

  fn create_dumb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer> {
    self.borrow_mut().create_dumb_framebuffer(width, height)
  }

  fn create_argb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer> {
    self.borrow_mut().create_argb_framebuffer(width, height)
  }

  fn cursor_size(&mut self) -> (u32, u32) {
    self.borrow_mut().cursor_size()
  }

  fn async_page_flips(&mut self) -> bool {
    self.borrow_mut().async_page_flips()
  }

  fn write_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
    write: &mut dyn FnMut(&mut [u8]),
  ) -> io::Result<()> {
    self.borrow_mut().write_dumb_framebuffer(buffer, write)
  }

  fn destroy_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
  ) -> io::Result<()> {
    self.borrow_mut().destroy_dumb_framebuffer(buffer)
  }

  fn add_framebuffer(
    &mut self,
    buffer: &PlanarFramebuffer,
  ) -> io::Result<FramebufferId> {
    self.borrow_mut().add_framebuffer(buffer)
  }

  fn remove_framebuffer(
    &mut self,
    framebuffer: FramebufferId,
  ) -> io::Result<()> {
    self.borrow_mut().remove_framebuffer(framebuffer)
  }

  fn buffer_allocator(&mut self) -> io::Result<Box<dyn BufferAllocator>> {
    self.borrow_mut().buffer_allocator()
  }

  fn read_events(&mut self) -> io::Result<Vec<PageFlip>> {
    self.borrow_mut().read_events()
  }

  fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
    None
  }
}
