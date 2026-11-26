//! NB-10's display features through the fake card: the cursor plane,
//! `VRR_ENABLED`, async page flips and render fences, each with its
//! fallbacks and overrides.

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

use super::*;
use crate::{
  kms::{
    FORMAT_XRGB8888, MODIFIER_LINEAR, Object, PlaneId, PropertyId,
    fake::{FakeKms, libc_einval},
    in_formats::tests::blob,
  },
  scanout::{Frame, tests::FakeImporter},
};

type Card = Rc<RefCell<FakeKms>>;

const OUTPUT: &str = "HDMI-A-1";
const FLIP: CommitFlags =
  CommitFlags(CommitFlags::NONBLOCK.0 | CommitFlags::PAGE_FLIP_EVENT.0);
const ASYNC_FLIP: CommitFlags =
  CommitFlags(FLIP.0 | CommitFlags::PAGE_FLIP_ASYNC.0);

/// A started display on `fake` with `policy`.
fn start(fake: FakeKms, policy: OutputPolicy) -> (Card, Display) {
  let card = fake.shared();
  let mut display = Display::prepare(Box::new(Rc::clone(&card)), policy)
    .expect("the display starts");
  display.start().expect("the modeset is accepted");
  (card, display)
}

/// A started display whose CRTC has a cursor plane, which it returns.
fn with_cursor_plane() -> (Card, Display, PlaneId) {
  let mut fake = FakeKms::single_output();
  let crtc = fake.crtc_id;
  let cursor = fake.add_cursor_plane(crtc);
  let (card, display) = start(fake, OutputPolicy::default());
  (card, display, cursor)
}

/// A 2x2 cursor image at (`x`, `y`).
fn arrow(x: i32, y: i32) -> CursorImage {
  CursorImage {
    width: 2,
    height: 2,
    rgba: Arc::from([255_u8; 16].as_slice()),
    position: (x, y),
  }
}

/// `image` moved to (`x`, `y`): the same pixels.
fn moved(image: &CursorImage, x: i32, y: i32) -> CursorImage {
  CursorImage {
    position: (x, y),
    ..image.clone()
  }
}

fn property(card: &Card, name: &str) -> PropertyId {
  card.borrow_mut().property_id(name)
}

/// The value `request` sets for `name` on `plane`.
fn plane_value(
  card: &Card,
  request: &AtomicRequest,
  plane: PlaneId,
  name: &str,
) -> Option<u64> {
  request.get(Object::Plane(plane), property(card, name))
}

/// The commits made since the `since`th, with flags and acceptance.
fn commits_since(
  card: &Card,
  since: usize,
) -> Vec<(AtomicRequest, CommitFlags, bool)> {
  card.borrow().commits[since..].to_vec()
}

fn frame() -> Drawn {
  Drawn::Pixels(Frame {
    width: 1280,
    height: 720,
    rgba: [10, 20, 30, 255].repeat(1280 * 720),
  })
}

/// Draws a copy frame and flips it.
fn flip_frame(display: &mut Display) {
  assert!(display.begin_frame(), "the display is ready for a frame");
  display.present(&frame(), Instant::now());
}

/// Completes the pending flip with `sequence` and dispatches it.
fn complete(
  card: &Card,
  display: &mut Display,
  sequence: u32,
) -> Vec<PresentInfo> {
  card
    .borrow_mut()
    .complete_flip(Duration::from_millis(u64::from(sequence)), sequence);
  display.dispatch()
}

#[test]
fn the_cursor_plane_is_found_and_the_modeset_turns_it_off() {
  let (card, display, cursor) = with_cursor_plane();
  assert_eq!(display.cursor_plane_size(OUTPUT), Ok((64, 64)));
  let (modeset, flags) = card.borrow().real_commits()[0].clone();
  assert_eq!(flags, CommitFlags::ALLOW_MODESET);
  assert_eq!(plane_value(&card, &modeset, cursor, "FB_ID"), Some(0));
  assert_eq!(plane_value(&card, &modeset, cursor, "CRTC_ID"), Some(0));
  assert!(!display.cursor_pending(OUTPUT), "nothing to show yet");
}

#[test]
fn without_a_cursor_plane_or_with_an_override_the_cursor_is_drawn() {
  let (_, mut display) =
    start(FakeKms::single_output(), OutputPolicy::default());
  assert_eq!(
    display.cursor_plane_size(OUTPUT),
    Err("HDMI-A-1's CRTC has no cursor plane".into())
  );
  display.set_cursor(OUTPUT, Some(arrow(1, 1)));
  assert!(!display.cursor_pending(OUTPUT), "there is no plane to set");
  let (_, mut display, _) = with_cursor_plane();
  display.set_features(PlaneFeatures {
    no_cursor_plane: Some(
      "SHINGLE_CURSOR_PLANE=0 turns cursor planes off".into(),
    ),
    ..PlaneFeatures::default()
  });
  assert_eq!(
    display.cursor_plane_size(OUTPUT),
    Err("SHINGLE_CURSOR_PLANE=0 turns cursor planes off".into())
  );
  display.set_cursor(OUTPUT, Some(arrow(1, 1)));
  assert!(!display.cursor_pending(OUTPUT));
  assert!(!display.commit_cursor(OUTPUT, Instant::now()));
}

#[test]
fn the_cursor_moves_with_flips_of_the_cursor_plane_alone() {
  let (card, mut display, cursor) = with_cursor_plane();
  let image = arrow(10, -20);
  display.set_cursor(OUTPUT, Some(image.clone()));
  assert!(display.cursor_pending(OUTPUT));
  let before = card.borrow().commits.len();
  assert!(display.commit_cursor(OUTPUT, Instant::now()));
  let commits = commits_since(&card, before);
  assert_eq!(commits.len(), 2, "a test the first time, then the flip");
  assert_eq!(commits[0].1, CommitFlags::TEST_ONLY);
  assert_eq!(commits[1].1, FLIP);
  let request = &commits[1].0;
  assert_eq!(request, &commits[0].0);
  assert!(
    request
      .entries()
      .iter()
      .all(|(object, ..)| *object == Object::Plane(cursor)),
    "only the cursor plane: {request:?}"
  );
  let framebuffer = plane_value(&card, request, cursor, "FB_ID").unwrap();
  assert!(
    card.borrow().argb.contains(&(framebuffer as u32)),
    "ARGB8888"
  );
  assert_eq!(
    plane_value(&card, request, cursor, "CRTC_ID"),
    Some(u64::from(card.borrow().crtc_id.0))
  );
  assert_eq!(plane_value(&card, request, cursor, "CRTC_W"), Some(64));
  assert_eq!(plane_value(&card, request, cursor, "CRTC_X"), Some(10));
  assert_eq!(
    plane_value(&card, request, cursor, "CRTC_Y"),
    Some(-20_i64 as u64)
  );
  let pixels = card.borrow().buffers[&(framebuffer as u32)].1.clone();
  assert_eq!(pixels[..8], [255; 8], "the image is in the buffer");
  // A move while the flip is pending waits; its event presents nothing.
  display.set_cursor(OUTPUT, Some(moved(&image, 30, 40)));
  assert!(!display.commit_cursor(OUTPUT, Instant::now()));
  assert!(
    complete(&card, &mut display, 1).is_empty(),
    "no frame shown"
  );
  let before = card.borrow().commits.len();
  assert!(display.commit_cursor(OUTPUT, Instant::now()));
  let commits = commits_since(&card, before);
  assert_eq!(commits.len(), 1, "no test after the first");
  let request = &commits[0].0;
  assert_eq!(
    plane_value(&card, request, cursor, "FB_ID"),
    Some(framebuffer),
    "the same image needs no new buffer"
  );
  assert_eq!(plane_value(&card, request, cursor, "CRTC_X"), Some(30));
  assert!(complete(&card, &mut display, 2).is_empty());
  assert!(!display.cursor_pending(OUTPUT));
  assert!(
    !display.commit_cursor(OUTPUT, Instant::now()),
    "nothing to do"
  );
  // Another image goes into the other buffer; hiding turns the plane off.
  display.set_cursor(OUTPUT, Some(arrow(30, 40)));
  assert!(display.commit_cursor(OUTPUT, Instant::now()));
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  let other = plane_value(&card, &request, cursor, "FB_ID").unwrap();
  assert_ne!(other, framebuffer);
  complete(&card, &mut display, 3);
  display.set_cursor(OUTPUT, None);
  assert!(display.commit_cursor(OUTPUT, Instant::now()));
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(plane_value(&card, &request, cursor, "FB_ID"), Some(0));
  assert_eq!(plane_value(&card, &request, cursor, "CRTC_ID"), Some(0));
}

#[test]
fn a_frame_carries_the_cursor_and_modesets_keep_it() {
  let (card, mut display, cursor) = with_cursor_plane();
  display.set_cursor(OUTPUT, Some(arrow(5, 6)));
  flip_frame(&mut display);
  let (request, flags) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(flags, FLIP);
  let primary = card.borrow().plane_id;
  assert!(plane_value(&card, &request, primary, "FB_ID").is_some());
  let shown = plane_value(&card, &request, cursor, "FB_ID").unwrap();
  assert_ne!(shown, 0);
  let presented = complete(&card, &mut display, 1);
  assert_eq!(presented.len(), 1, "a frame was presented");
  assert!(!display.cursor_pending(OUTPUT));
  // DPMS off and on: the modeset that turns it on shows the cursor.
  display.set_dpms(false);
  display.set_dpms(true);
  let (request, flags) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(flags, CommitFlags::ALLOW_MODESET);
  assert_eq!(plane_value(&card, &request, cursor, "FB_ID"), Some(shown));
  assert_eq!(plane_value(&card, &request, cursor, "CRTC_X"), Some(5));
  // A resume's modeset too.
  display.pause();
  display.resume();
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(plane_value(&card, &request, cursor, "FB_ID"), Some(shown));
}

#[test]
fn a_refused_cursor_plane_leaves_the_cursor_to_the_frame() {
  let (card, mut display, cursor) = with_cursor_plane();
  let fb_id = property(&card, "FB_ID");
  card.borrow_mut().reject = Box::new(move |request, _| {
    request
      .get(Object::Plane(cursor), fb_id)
      .filter(|framebuffer| *framebuffer != 0)
      .map(|_| libc_einval())
  });
  display.set_cursor(OUTPUT, Some(arrow(5, 6)));
  let before = card.borrow().commits.len();
  assert!(!display.commit_cursor(OUTPUT, Instant::now()));
  assert_eq!(commits_since(&card, before).len(), 1, "the test only");
  assert_eq!(
    display.cursor_plane_size(OUTPUT),
    Err(
      "the cursor plane refused the cursor: Invalid argument (os error 22)"
        .into()
    )
  );
  assert!(!display.cursor_pending(OUTPUT));
  assert!(display.ready(), "the output can draw its frame");
  // A frame refused for its cursor goes out without it.
  let (card, mut display, cursor) = with_cursor_plane();
  let fb_id = property(&card, "FB_ID");
  card.borrow_mut().reject = Box::new(move |request, flags| {
    let shows = request
      .get(Object::Plane(cursor), fb_id)
      .is_some_and(|framebuffer| framebuffer != 0);
    (shows && !flags.contains(CommitFlags::TEST_ONLY)).then(libc_einval)
  });
  display.set_cursor(OUTPUT, Some(arrow(5, 6)));
  flip_frame(&mut display);
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(plane_value(&card, &request, cursor, "FB_ID"), None);
  assert_eq!(
    complete(&card, &mut display, 1).len(),
    1,
    "the frame is shown"
  );
  assert!(display.cursor_plane_size(OUTPUT).is_err());
}

#[test]
fn crtcs_that_share_a_cursor_plane_give_it_to_the_first() {
  let mut fake = FakeKms::single_output();
  let first = fake.crtc_id;
  let second = fake.add_pipe(crate::kms::fake::connector_info(20, "DP-1"));
  fake.add_shared_cursor_plane(vec![first, second]);
  let (_, display) = start(fake, OutputPolicy::default());
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  assert!(display.cursor_plane_size("HDMI-A-1").is_ok());
  assert_eq!(
    display.cursor_plane_size("DP-1"),
    Err("DP-1's CRTC has no cursor plane".into())
  );
}

/// A policy with `vrr` for every output.
fn vrr_policy(vrr: bool) -> OutputPolicy {
  OutputPolicy {
    default_vrr: vrr,
    ..OutputPolicy::default()
  }
}

/// A card whose CRTC has `VRR_ENABLED` and whose monitor is
/// `capable`.
fn vrr_card(capable: bool) -> FakeKms {
  let mut fake = FakeKms::single_output();
  fake.vrr = true;
  fake.vrr_capable = capable;
  fake
}

fn vrr_enabled(card: &Card, request: &AtomicRequest) -> Option<u64> {
  let crtc = Object::Crtc(card.borrow().crtc_id);
  request.get(crtc, property(card, "VRR_ENABLED"))
}

#[test]
fn vrr_is_on_where_configured_and_the_monitor_is_capable() {
  let (card, mut display) = start(vrr_card(true), vrr_policy(true));
  let commits = card.borrow().commits.clone();
  assert!(
    commits
      .iter()
      .all(|(request, ..)| vrr_enabled(&card, request) == Some(1)),
    "the start's test and modeset"
  );
  assert_eq!(
    card.borrow().value(card.borrow().crtc_id.0, "VRR_ENABLED"),
    1
  );
  assert!(display.output_info().unwrap().adaptive_sync, "heads say so");
  flip_frame(&mut display);
  let presented = complete(&card, &mut display, 1);
  assert_eq!(presented[0].refresh_ns, 0, "no constant refresh rate");
  assert!(presented[0].flags.contains(PresentationFlags::Vsync));
}

#[test]
fn vrr_stays_off_unless_configured_and_capable() {
  for (capable, configured) in [(false, true), (true, false)] {
    let (card, mut display) = start(vrr_card(capable), vrr_policy(configured));
    let (modeset, _) = card.borrow().real_commits()[0].clone();
    assert_eq!(
      vrr_enabled(&card, &modeset),
      Some(0),
      "{capable} {configured}"
    );
    assert!(!display.output_info().unwrap().adaptive_sync);
    flip_frame(&mut display);
    assert_eq!(complete(&card, &mut display, 1)[0].refresh_ns, 16_666_666);
  }
  // A CRTC without the property gets none at all.
  let (card, _) = start(FakeKms::single_output(), vrr_policy(true));
  let (modeset, _) = card.borrow().real_commits()[0].clone();
  assert_eq!(vrr_enabled(&card, &modeset), None);
}

#[test]
fn a_reloaded_vrr_makes_a_modeset_that_sets_it() {
  let (card, mut display) = start(vrr_card(true), vrr_policy(false));
  let before = card.borrow().real_commits().len();
  display.set_policy(vrr_policy(true));
  let commits = card.borrow().real_commits();
  assert_eq!(commits.len(), before + 1, "one modeset");
  assert_eq!(commits[before].1, CommitFlags::ALLOW_MODESET);
  assert_eq!(vrr_enabled(&card, &commits[before].0), Some(1));
  display.set_policy(vrr_policy(false));
  let (modeset, _) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(vrr_enabled(&card, &modeset), Some(0));
}

/// A started display whose driver takes async flips as `async_flips`
/// says, with a client asking for tearing.
fn tearing(async_flips: bool) -> (Card, Display) {
  let mut fake = FakeKms::single_output();
  fake.async_flips = async_flips;
  let (card, mut display) = start(fake, OutputPolicy::default());
  display.set_tearing(OUTPUT, true);
  (card, display)
}

fn last_flags(card: &Card) -> CommitFlags {
  card
    .borrow()
    .real_commits()
    .last()
    .map(|(_, flags)| *flags)
    .unwrap()
}

#[test]
fn tearing_frames_flip_async_and_are_not_vsynced() {
  let (card, mut display) = tearing(true);
  flip_frame(&mut display);
  assert_eq!(last_flags(&card), ASYNC_FLIP);
  let presented = complete(&card, &mut display, 1);
  assert!(!presented[0].flags.contains(PresentationFlags::Vsync));
  assert!(presented[0].flags.contains(PresentationFlags::HwCompletion));
  display.set_tearing(OUTPUT, false);
  flip_frame(&mut display);
  assert_eq!(last_flags(&card), FLIP, "back to vsync");
  let presented = complete(&card, &mut display, 2);
  assert!(presented[0].flags.contains(PresentationFlags::Vsync));
}

#[test]
fn without_async_flips_tearing_frames_wait_for_the_vblank() {
  let (card, mut display) = tearing(false);
  flip_frame(&mut display);
  assert_eq!(last_flags(&card), FLIP);
  let (card_on, mut display_off) = tearing(true);
  display_off.set_features(PlaneFeatures {
    no_async_flips: Some(
      "SHINGLE_ASYNC_FLIPS=0 keeps every page flip synchronized".into(),
    ),
    ..PlaneFeatures::default()
  });
  flip_frame(&mut display_off);
  assert_eq!(last_flags(&card_on), FLIP, "the override");
}

#[test]
fn a_refused_async_flip_is_made_again_in_sync_and_stays_so() {
  let (card, mut display) = tearing(true);
  card.borrow_mut().reject = Box::new(|_, flags| {
    flags
      .contains(CommitFlags::PAGE_FLIP_ASYNC)
      .then(libc_einval)
  });
  let before = card.borrow().commits.len();
  flip_frame(&mut display);
  let commits = commits_since(&card, before);
  let flags: Vec<_> =
    commits.iter().map(|(_, flags, ok)| (*flags, *ok)).collect();
  assert_eq!(flags, [(ASYNC_FLIP, false), (FLIP, true)]);
  complete(&card, &mut display, 1);
  let before = card.borrow().commits.len();
  flip_frame(&mut display);
  assert_eq!(commits_since(&card, before).len(), 1, "no async try again");
  assert_eq!(last_flags(&card), FLIP);
}

#[test]
fn a_cursor_change_keeps_the_frame_synchronized() {
  let mut fake = FakeKms::single_output();
  fake.async_flips = true;
  let crtc = fake.crtc_id;
  fake.add_cursor_plane(crtc);
  let (card, mut display) = start(fake, OutputPolicy::default());
  display.set_tearing(OUTPUT, true);
  display.set_cursor(OUTPUT, Some(arrow(1, 2)));
  flip_frame(&mut display);
  assert_eq!(
    last_flags(&card),
    FLIP,
    "async flips touch the primary only"
  );
  complete(&card, &mut display, 1);
  flip_frame(&mut display);
  assert_eq!(last_flags(&card), ASYNC_FLIP, "the cursor did not change");
}

/// A started display on zero-copy scanout, whose primary plane has
/// `IN_FENCE_FD`.
fn zero_copy() -> (Card, Display) {
  let mut fake = FakeKms::single_output();
  fake.in_formats = Some(blob(&[FORMAT_XRGB8888], &[(1, 0, MODIFIER_LINEAR)]));
  let (card, mut display) = start(fake, OutputPolicy::default());
  display.set_scanout_choice(ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert!(matches!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::ZeroCopy(_))
  ));
  (card, display)
}

/// A fence that has signalled: an eventfd that is readable.
fn signalled_fence() -> RenderFence {
  let fd = rustix::event::eventfd(1, rustix::event::EventfdFlags::CLOEXEC)
    .expect("an eventfd");
  RenderFence::new(fd)
}

fn fence_value(card: &Card, request: &AtomicRequest) -> Option<u64> {
  let primary = card.borrow().plane_id;
  plane_value(card, request, primary, "IN_FENCE_FD")
}

#[test]
fn a_fenced_frame_hands_the_plane_its_fence() {
  let (card, mut display) = zero_copy();
  assert_eq!(display.fences_allowed(), Ok(()));
  let fence = signalled_fence();
  assert!(display.begin_frame());
  display.present(&Drawn::Fenced(fence.clone()), Instant::now());
  let (request, flags) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(flags, FLIP);
  assert_eq!(
    fence_value(&card, &request),
    Some(fence.fd().as_raw_fd() as u64)
  );
  let presented = complete(&card, &mut display, 1);
  assert!(presented[0].flags.contains(PresentationFlags::ZeroCopy));
  // A frame without a fence sets none.
  assert!(display.begin_frame());
  display.present(&Drawn::InPlace, Instant::now());
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(fence_value(&card, &request), None);
}

#[test]
fn a_refused_fence_is_waited_for_and_fences_stay_off() {
  let (card, mut display) = zero_copy();
  let in_fence = property(&card, "IN_FENCE_FD");
  let primary = card.borrow().plane_id;
  card.borrow_mut().reject = Box::new(move |request, _| {
    request
      .get(Object::Plane(primary), in_fence)
      .map(|_| libc_einval())
  });
  let before = card.borrow().commits.len();
  assert!(display.begin_frame());
  display.present(&Drawn::Fenced(signalled_fence()), Instant::now());
  let commits = commits_since(&card, before);
  assert_eq!(commits.len(), 2);
  assert!(
    !commits[0].2 && commits[1].2,
    "refused, then made without it"
  );
  assert_eq!(fence_value(&card, &commits[1].0), None);
  assert_eq!(
    display.fences_allowed(),
    Err("the card refused IN_FENCE_FD: Invalid argument (os error 22)".into())
  );
  complete(&card, &mut display, 1);
  let before = card.borrow().commits.len();
  assert!(display.begin_frame());
  display.present(&Drawn::Fenced(signalled_fence()), Instant::now());
  assert_eq!(
    commits_since(&card, before).len(),
    1,
    "no fence tried again"
  );
}

#[test]
fn the_fence_override_waits_on_the_cpu() {
  let (card, mut display) = zero_copy();
  display.set_features(PlaneFeatures {
    no_in_fence: Some("SHINGLE_IN_FENCE=0 makes the renderer wait".into()),
    ..PlaneFeatures::default()
  });
  assert_eq!(
    display.fences_allowed(),
    Err("SHINGLE_IN_FENCE=0 makes the renderer wait".into())
  );
  assert!(display.begin_frame());
  display.present(&Drawn::Fenced(signalled_fence()), Instant::now());
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  assert_eq!(fence_value(&card, &request), None);
}
