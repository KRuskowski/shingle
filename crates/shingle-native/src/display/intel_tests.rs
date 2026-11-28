//! The display on fake cards shaped like the first test laptops' GPUs,
//! from public drm_info dumps (crates/shingle-native/src/kms/fake/
//! drm_info.rs): i915 on a UHD 620 (Whiskey Lake, the Latitude 5400) and
//! on Meteor Lake's Arc graphics (the Latitude 5450), with the driver's
//! own refusals of async flips and cursor buffers.

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use super::*;
use crate::{
  kms::{
    ConnectorId, KmsDevice, MODIFIER_LINEAR, Object, PlaneId,
    fake::{FakeKms, drm_info},
    props::{LINK_STATUS_BAD, LINK_STATUS_GOOD},
  },
  scanout::tests::FakeImporter,
};

type Card = Rc<RefCell<FakeKms>>;

const X_TILED: u64 = 0x0100_0000_0000_0001;
const Y_TILED: u64 = 0x0100_0000_0000_0002;
const Y_TILED_CCS: u64 = 0x0100_0000_0000_0004;
const YF_TILED_CCS: u64 = 0x0100_0000_0000_0005;
const TILED_4: u64 = 0x0100_0000_0000_0009;
const TILED_4_MTL_RC_CCS: u64 = 0x0100_0000_0000_000d;
const TILED_4_MTL_MC_CCS: u64 = 0x0100_0000_0000_000e;
const TILED_4_MTL_RC_CCS_CC: u64 = 0x0100_0000_0000_000f;

/// A started display on `fake` with `policy`.
fn start(fake: FakeKms, policy: OutputPolicy) -> (Card, Display) {
  let card = fake.shared();
  let mut display = Display::prepare(Box::new(Rc::clone(&card)), policy)
    .expect("the display starts");
  display.start().expect("the modeset is accepted");
  (card, display)
}

/// Each lit output's name, CRTC, primary plane and mode size.
fn lit(display: &Display) -> Vec<(String, u32, u32, (u32, u32))> {
  display
    .lit
    .iter()
    .map(|lit| {
      let name = lit.name().to_owned();
      (name, lit.crtc().0, lit.plane().0, lit.mode.size())
    })
    .collect()
}

/// The value `request` sets for property `name` of `object`.
fn set_in(
  card: &Card,
  request: &AtomicRequest,
  object: Object,
  name: &str,
) -> Option<u64> {
  let property = card.borrow_mut().property_id(name);
  request.get(object, property)
}

/// What ANV renders XRGB8888 to on display version 9, with
/// `drmFormatModifierPlaneCount` 2 for CCS, and the plane counts GBM
/// (iris) gives those modifiers.
fn gen9_renderer(card: &Card) -> Rc<RefCell<FakeImporter>> {
  let gbm = Rc::clone(&card.borrow().gbm);
  let mut gbm = gbm.borrow_mut();
  gbm.plane_counts.insert(Y_TILED_CCS, Some(2));
  gbm.plane_counts.insert(YF_TILED_CCS, Some(2));
  // The renderer lists only single-plane modifiers (shingle-render's
  // `usable_modifiers`), so Y_TILED_CCS is not among them.
  FakeImporter::rendering(&[MODIFIER_LINEAR, X_TILED, Y_TILED])
}

/// The same for Meteor Lake: 4-tiled, and its CCS modifiers with two and
/// three planes.
fn mtl_renderer(card: &Card) -> Rc<RefCell<FakeImporter>> {
  let gbm = Rc::clone(&card.borrow().gbm);
  let mut gbm = gbm.borrow_mut();
  gbm.plane_counts.insert(TILED_4_MTL_RC_CCS, Some(2));
  gbm.plane_counts.insert(TILED_4_MTL_MC_CCS, Some(2));
  gbm.plane_counts.insert(TILED_4_MTL_RC_CCS_CC, Some(3));
  FakeImporter::rendering(&[MODIFIER_LINEAR, X_TILED, TILED_4])
}

#[test]
fn whiskey_lake_lights_each_output_where_the_console_left_it() {
  let (card, display) =
    start(drm_info::whiskey_lake(), OutputPolicy::default());
  // The panel first, then the monitors in the card's order, each on the
  // CRTC and primary plane the console used, so i915 can keep the
  // panel's mode without a full modeset.
  assert_eq!(
    lit(&display),
    vec![
      ("eDP-1".to_owned(), 51, 31, (1920, 1080)),
      ("DP-1".to_owned(), 72, 52, (3840, 2160)),
      // No preferred mode: Wayfire's `select_default_mode` takes the
      // last mode, which the kernel's order makes the smallest.
      ("DP-2".to_owned(), 93, 73, (720, 400)),
    ]
  );
  let fake = card.borrow();
  assert!(fake.commits.iter().all(|(_, _, accepted)| *accepted));
  // The panel lists its fixed mode alone, as i915 lists eDP modes.
  assert_eq!(display.lit[0].mode.refresh_mhz() / 1000, 60);
}

#[test]
fn whiskey_lake_scans_out_y_tiled_without_ccs() {
  let (card, mut display) =
    start(drm_info::whiskey_lake(), OutputPolicy::default());
  let renderer = gen9_renderer(&card);
  display.set_scanout_choice(ScanoutChoice::Auto);
  display.upgrade_scanout("eDP-1", &mut Rc::clone(&renderer));
  assert_eq!(
    display.scanout_status("eDP-1"),
    Some(ScanoutStatus::ZeroCopy(Y_TILED))
  );
  // The plane lists Yf_CCS, Y_CCS, Yf, Y, X and LINEAR; the renderer
  // takes neither Yf nor a CCS modifier, which has a second plane.
  let gbm = Rc::clone(&card.borrow().gbm);
  assert_eq!(gbm.borrow().requests[0], vec![Y_TILED, X_TILED, 0]);
}

#[test]
fn meteor_lake_scans_out_4_tiled_without_ccs() {
  let (card, mut display) =
    start(drm_info::meteor_lake(), OutputPolicy::default());
  let renderer = mtl_renderer(&card);
  display.set_scanout_choice(ScanoutChoice::Auto);
  display.upgrade_scanout("eDP-1", &mut Rc::clone(&renderer));
  assert_eq!(
    display.scanout_status("eDP-1"),
    Some(ScanoutStatus::ZeroCopy(TILED_4))
  );
  let gbm = Rc::clone(&card.borrow().gbm);
  assert_eq!(gbm.borrow().requests[0], vec![TILED_4, X_TILED, 0]);
}

#[test]
fn meteor_lake_lights_the_dark_panel_beside_a_dp_mst_dock() {
  let (_, display) = start(drm_info::meteor_lake(), OutputPolicy::default());
  // Four pipes: the panel (connected, but dark in the dump, as with the
  // lid closed) takes the free one, the dock's monitors keep theirs.
  assert_eq!(
    lit(&display),
    vec![
      ("eDP-1".to_owned(), 88, 33, (1920, 1200)),
      ("DP-6".to_owned(), 145, 90, (3840, 2160)),
      ("DP-7".to_owned(), 202, 147, (1920, 1080)),
      ("DP-8".to_owned(), 259, 204, (1920, 1080)),
    ]
  );
}

#[test]
fn unplugging_a_dp_mst_dock_turns_its_pipes_off() {
  let (card, mut display) =
    start(drm_info::meteor_lake(), OutputPolicy::default());
  let dock = [ConnectorId(336), ConnectorId(338), ConnectorId(345)];
  let mut removed = Vec::new();
  for connector in dock {
    removed.push(card.borrow_mut().unplug_connector(connector));
  }
  let before = card.borrow().commits.len();
  display.hotplug();
  assert_eq!(display.outputs(), vec!["eDP-1"]);
  let commits = card.borrow().real_commits();
  let (modeset, flags) = commits.last().expect("a modeset").clone();
  assert!(flags.contains(CommitFlags::ALLOW_MODESET));
  assert!(card.borrow().commits.len() > before);
  for crtc in [145, 202, 259] {
    let object = Object::Crtc(crate::kms::CrtcId(crtc));
    assert_eq!(set_in(&card, &modeset, object, "ACTIVE"), Some(0));
  }
  // The kernel lets a commit turn an unregistered connector off.
  for connector in dock {
    let object = Object::Connector(connector);
    assert_eq!(set_in(&card, &modeset, object, "CRTC_ID"), Some(0));
  }
  // Plugged in again, the kernel makes new connectors with new ids.
  for (index, mut info) in removed.into_iter().enumerate() {
    let like = info.id.0;
    info.id = ConnectorId(400 + index as u32);
    info.current_crtc = None;
    card.borrow_mut().plug_connector(info, like);
  }
  display.hotplug();
  assert_eq!(display.outputs(), vec!["eDP-1", "DP-6", "DP-7", "DP-8"]);
}

#[test]
fn the_cursor_plane_takes_the_cards_256_pixel_buffers() {
  let (card, mut display) =
    start(drm_info::whiskey_lake(), OutputPolicy::default());
  assert_eq!(display.cursor_plane_size("eDP-1"), Ok((256, 256)));
  let image = CursorImage {
    width: 24,
    height: 24,
    rgba: Arc::from(vec![255_u8; 24 * 24 * 4].as_slice()),
    position: (100, -3),
  };
  display.set_cursor("eDP-1", Some(image));
  assert!(display.commit_cursor("eDP-1", Instant::now()));
  let commits = card.borrow().real_commits();
  let (request, _) = commits.last().expect("the cursor's flip").clone();
  // Plane 47 is pipe A's cursor plane; i915 takes it at 256 pixels with
  // a stride of 1024 bytes (`i9xx_check_cursor`).
  let cursor = Object::Plane(PlaneId(47));
  assert_eq!(set_in(&card, &request, cursor, "CRTC_W"), Some(256));
  assert_eq!(set_in(&card, &request, cursor, "CRTC_H"), Some(256));
  assert_eq!(
    set_in(&card, &request, cursor, "CRTC_Y"),
    Some(-3_i64 as u64)
  );
  card
    .borrow_mut()
    .complete_flip(Duration::from_millis(16), 1);
  assert!(
    display.dispatch().is_empty(),
    "a cursor flip presents nothing"
  );
}

#[test]
fn vrr_needs_display_version_11_and_a_capable_panel() {
  let policy = OutputPolicy {
    default_vrr: true,
    ..OutputPolicy::default()
  };
  // Display version 9 has VRR_ENABLED on its CRTCs but no connector is
  // vrr_capable (i915's `HAS_VRR` needs version 11).
  let (_, display) = start(drm_info::whiskey_lake(), policy.clone());
  assert!(display.lit.iter().all(|lit| !lit.vrr));
  // Meteor Lake's panel is vrr_capable; the dock's monitors are not.
  let (card, display) = start(drm_info::meteor_lake(), policy);
  let vrr: Vec<(&str, bool)> = display
    .lit
    .iter()
    .map(|lit| (lit.name(), lit.vrr))
    .collect();
  assert_eq!(
    vrr,
    vec![
      ("eDP-1", true),
      ("DP-6", false),
      ("DP-7", false),
      ("DP-8", false)
    ]
  );
  let commits = card.borrow().real_commits();
  let (modeset, _) = commits.last().unwrap().clone();
  let panel = Object::Crtc(crate::kms::CrtcId(88));
  assert_eq!(set_in(&card, &modeset, panel, "VRR_ENABLED"), Some(1));
}

#[test]
fn the_fixtures_refuse_what_i915_refuses() {
  let mut fake = drm_info::whiskey_lake();
  let primary = Object::Plane(PlaneId(31));
  let fb_id = fake.property_id("FB_ID");
  let buffer = fake.create_dumb_framebuffer(1920, 1080).unwrap();
  let mut flip = AtomicRequest::new();
  flip.set(primary, fb_id, u64::from(buffer.framebuffer.0));
  let async_flip = CommitFlags::PAGE_FLIP_ASYNC | CommitFlags::NONBLOCK;
  // A LINEAR buffer cannot flip async before display version 12.
  assert!(fake.commit(&flip, async_flip).is_err());
  assert!(fake.commit(&flip, CommitFlags::NONBLOCK).is_ok());
  // No property but the primary plane's FB_ID may change in one.
  let crtc_x = fake.property_id("CRTC_X");
  let mut moved = AtomicRequest::new();
  moved.set(primary, crtc_x, 5);
  assert!(fake.commit(&moved, async_flip).is_err());
  // A property the object lacks is refused, as is a value out of range.
  let vrr_capable = fake.property_id("vrr_capable");
  let mut missing = AtomicRequest::new();
  missing.set(Object::Connector(ConnectorId(95)), vrr_capable, 1);
  assert!(fake.commit(&missing, CommitFlags::TEST_ONLY).is_err());
  let max_bpc = fake.property_id("max bpc");
  let mut bpc = AtomicRequest::new();
  bpc.set(Object::Connector(ConnectorId(95)), max_bpc, 16);
  assert!(fake.commit(&bpc, CommitFlags::TEST_ONLY).is_err());
  bpc.set(Object::Connector(ConnectorId(95)), max_bpc, 8);
  assert!(fake.commit(&bpc, CommitFlags::TEST_ONLY).is_ok());
  // A cursor 200 pixels wide, which i915 cannot show.
  let cursor = Object::Plane(PlaneId(47));
  let wide = fake.create_argb_framebuffer(200, 256).unwrap();
  let mut request = AtomicRequest::new();
  for (name, value) in [
    ("FB_ID", u64::from(wide.framebuffer.0)),
    ("CRTC_ID", 51),
    ("SRC_W", 200 << 16),
    ("SRC_H", 256 << 16),
    ("CRTC_W", 200),
    ("CRTC_H", 256),
  ] {
    let property = fake.property_id(name);
    request.set(cursor, property, value);
  }
  assert!(fake.commit(&request, CommitFlags::TEST_ONLY).is_err());
}

#[test]
fn a_link_the_kernel_marked_bad_is_set_good_by_a_modeset() {
  let (card, mut display) =
    start(drm_info::meteor_lake(), OutputPolicy::default());
  let before = card.borrow().commits.len();
  display.hotplug();
  assert_eq!(card.borrow().commits.len(), before, "nothing changed");
  // DP link training failed on the dock's first monitor: i915 marks the
  // link bad and sends a hotplug, and the monitor stays black until a
  // modeset sets it good.
  let monitor = ConnectorId(336);
  card
    .borrow_mut()
    .set_value(monitor.0, "link-status", LINK_STATUS_BAD);
  display.hotplug();
  let commits = card.borrow().real_commits();
  let (modeset, flags) = commits.last().expect("a modeset").clone();
  assert!(flags.contains(CommitFlags::ALLOW_MODESET));
  let object = Object::Connector(monitor);
  assert_eq!(
    set_in(&card, &modeset, object, "link-status"),
    Some(LINK_STATUS_GOOD)
  );
  assert_eq!(card.borrow().value(monitor.0, "link-status"), 0);
  assert_eq!(display.outputs(), vec!["eDP-1", "DP-6", "DP-7", "DP-8"]);
  assert_eq!(display.size_of("DP-6"), Some((3840, 2160)));
  // After a fallback to a slower link the kernel offers smaller modes
  // only; the output gets one of them.
  let mut fake = card.borrow_mut();
  fake
    .connector_mut(monitor)
    .modes
    .retain(|mode| mode.hdisplay < 3840);
  fake.set_value(monitor.0, "link-status", LINK_STATUS_BAD);
  drop(fake);
  display.hotplug();
  assert!(
    display
      .size_of("DP-6")
      .is_some_and(|(width, _)| width < 3840)
  );
  assert_eq!(card.borrow().value(monitor.0, "link-status"), 0);
}

#[test]
fn the_first_modeset_clears_what_another_session_left() {
  // A session before Shingle left a night-light gamma ramp and a colour
  // matrix on the panel's CRTC, its primary plane rotated by 180 degrees
  // and the panel at 6 bits per colour.
  let mut fake = drm_info::whiskey_lake();
  fake.set_value(51, "GAMMA_LUT", 700);
  fake.set_value(51, "CTM", 701);
  fake.set_value(31, "rotation", 4);
  fake.set_value(95, "max bpc", 6);
  let (card, _display) = start(fake, OutputPolicy::default());
  let fake = card.borrow();
  assert_eq!(fake.value(51, "GAMMA_LUT"), 0);
  assert_eq!(fake.value(51, "DEGAMMA_LUT"), 0);
  assert_eq!(fake.value(51, "CTM"), 0);
  assert_eq!(fake.value(31, "rotation"), 1);
  // Pipe A's cursor plane too, though it stays off.
  assert_eq!(fake.value(47, "rotation"), 1);
  for connector in [95, 105, 121] {
    assert_eq!(fake.value(connector, "max bpc"), 8);
  }
}

#[test]
fn async_flips_come_back_with_tiled_buffers_on_display_version_9() {
  let (card, mut display) =
    start(drm_info::whiskey_lake(), OutputPolicy::default());
  display.set_tearing("eDP-1", true);
  let async_flip = CommitFlags::NONBLOCK
    | CommitFlags::PAGE_FLIP_EVENT
    | CommitFlags::PAGE_FLIP_ASYNC;
  let flags_since = |since: usize| -> Vec<(CommitFlags, bool)> {
    card.borrow().commits[since..]
      .iter()
      .map(|(_, flags, accepted)| (*flags, *accepted))
      .collect()
  };
  // Copy scanout's dumb buffers are LINEAR, which i915 does not flip
  // async before display version 12: the frame flips in sync.
  let before = card.borrow().commits.len();
  assert!(display.begin_frame_on("eDP-1"));
  let frame = crate::scanout::Frame {
    width: 1920,
    height: 1080,
    rgba: vec![0; 1920 * 1080 * 4],
  };
  display.present_on("eDP-1", &Drawn::Pixels(frame), Instant::now());
  let sync = CommitFlags::NONBLOCK | CommitFlags::PAGE_FLIP_EVENT;
  assert_eq!(flags_since(before), [(async_flip, false), (sync, true)]);
  let crtc = crate::kms::CrtcId(51);
  card
    .borrow_mut()
    .complete_flip_on(crtc, Duration::from_millis(16), 1);
  display.dispatch_flips();
  // Y-tiled GBM buffers do flip async: the refusal was the buffers'.
  display.set_scanout_choice(ScanoutChoice::Auto);
  display.upgrade_scanout("eDP-1", &mut gen9_renderer(&card));
  assert_eq!(
    display.scanout_status("eDP-1"),
    Some(ScanoutStatus::ZeroCopy(Y_TILED))
  );
  let before = card.borrow().commits.len();
  assert!(display.begin_frame_on("eDP-1"));
  display.present_on("eDP-1", &Drawn::InPlace, Instant::now());
  assert_eq!(flags_since(before), [(async_flip, true)]);
}
