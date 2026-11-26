//! Zero-copy scanout of the display, through the fake card, its fake GBM
//! and a fake renderer's GPU side: modifier negotiation, the switch from
//! copy buffers, the fallbacks and `SHINGLE_SCANOUT=copy`.

use std::{cell::RefCell, rc::Rc, time::Duration};

use super::*;
use crate::{
  kms::{
    FORMAT_XRGB8888, MODIFIER_LINEAR, Object, fake::FakeKms,
    in_formats::tests::blob,
  },
  scanout::{Frame, tests::FakeImporter},
};

type Card = Rc<RefCell<FakeKms>>;

const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6015;
const X_TILED: u64 = 0x0100_0000_0000_0001;
const OUTPUT: &str = "HDMI-A-1";

/// A started display whose primary plane scans out XRGB8888 with
/// `plane`, set to try zero-copy with `choice`.
fn started(plane: &[u64], choice: ScanoutChoice) -> (Card, Display) {
  let mut fake = FakeKms::single_output();
  let entries: Vec<(u64, u32, u64)> =
    plane.iter().map(|modifier| (1, 0, *modifier)).collect();
  fake.in_formats = Some(blob(&[FORMAT_XRGB8888], &entries));
  let card = fake.shared();
  let mut display =
    Display::prepare(Box::new(Rc::clone(&card)), OutputPolicy::default())
      .expect("the display starts");
  display.start().expect("the modeset is accepted");
  display.set_scanout_choice(choice);
  (card, display)
}

/// The commits made since the `since`th one, with their flags.
fn commits_since(card: &Card, since: usize) -> Vec<(AtomicRequest, u32)> {
  card.borrow().commits[since..]
    .iter()
    .map(|(request, flags, _)| (request.clone(), flags.0))
    .collect()
}

fn plane_fb(card: &Card, request: &AtomicRequest) -> Option<u64> {
  let plane = Object::Plane(card.borrow().plane_id);
  let fb_id = card.borrow_mut().property_id("FB_ID");
  request.get(plane, fb_id)
}

fn target_serial(display: &Display) -> Option<u64> {
  match display.draw_target(OUTPUT) {
    DrawTarget::Dmabuf(buffer) => Some(buffer.serial),
    DrawTarget::Readback => None,
  }
}

/// Draws a frame in place and completes its flip; returns the flags it
/// was presented with.
fn present_in_place(
  card: &Card,
  display: &mut Display,
  sequence: u32,
) -> PresentationFlags {
  assert!(display.begin_frame(), "the output is ready");
  display.present(&Drawn::InPlace, Instant::now());
  card
    .borrow_mut()
    .complete_flip(Duration::from_millis(u64::from(sequence)), sequence);
  let presented = display.dispatch();
  assert_eq!(presented.len(), 1);
  presented[0].flags
}

#[test]
fn zero_copy_replaces_the_copy_buffers_after_a_tested_blocking_flip() {
  let (card, mut display) =
    started(&[MODIFIER_LINEAR, X_TILED], ScanoutChoice::Auto);
  assert_eq!(display.scanout_status(OUTPUT), Some(ScanoutStatus::Untried));
  assert!(matches!(display.draw_target(OUTPUT), DrawTarget::Readback));
  let importer = FakeImporter::rendering(&[X_TILED, BLOCK_LINEAR]);
  let before = card.borrow().commits.len();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::ZeroCopy(X_TILED))
  );
  assert_eq!(importer.borrow().asked_size, Some((1280, 720)));
  assert_eq!(
    importer.borrow().prepared,
    vec![(OUTPUT.to_owned(), vec![1, 2, 3])],
    "the renderer imports and clears the buffers before they are shown"
  );
  assert_eq!(card.borrow().gbm.borrow().requests, vec![vec![X_TILED]; 3]);
  let commits = commits_since(&card, before);
  assert_eq!(commits.len(), 2, "a test and the flip");
  assert_eq!(commits[0].1, CommitFlags::TEST_ONLY.0);
  assert_eq!(commits[1].1, 0, "a blocking flip without an event");
  assert_eq!(commits[0].0, commits[1].0);
  assert_eq!(commits[1].0.entries().len(), 1, "only the plane's FB_ID");
  let shown = plane_fb(&card, &commits[1].0).expect("the plane is flipped");
  let fake = card.borrow();
  let framebuffer = fake.framebuffers[&(shown as u32)];
  assert_eq!(framebuffer.modifier, X_TILED);
  assert_eq!((framebuffer.width, framebuffer.height), (1280, 720));
  assert_eq!(fake.framebuffers.len(), 3);
  assert!(fake.buffers.is_empty(), "the dumb buffers are freed");
  drop(fake);
  assert_eq!(target_serial(&display), Some(2));
  let flags = present_in_place(&card, &mut display, 1);
  assert!(flags.contains(PresentationFlags::ZeroCopy));
  assert!(flags.contains(PresentationFlags::Vsync));
  assert_eq!(target_serial(&display), Some(3), "the next back buffer");
  // Trying again changes nothing.
  let commits = card.borrow().commits.len();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(card.borrow().commits.len(), commits);
  assert_eq!(importer.borrow().prepared.len(), 1);
}

#[test]
fn shingle_scanout_copy_keeps_copy_scanout_with_its_reason() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Copy);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  let before = card.borrow().commits.len();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::Copy(
      "SHINGLE_SCANOUT=copy forces copy scanout".into()
    ))
  );
  assert_eq!(card.borrow().commits.len(), before, "nothing is committed");
  assert!(card.borrow().gbm.borrow().requests.is_empty());
  assert!(importer.borrow().prepared.is_empty());
  assert!(matches!(display.draw_target(OUTPUT), DrawTarget::Readback));
  flip_copy_frame(&card, &mut display);
}

/// Flips a copy frame and checks it is not presented as zero-copy.
fn flip_copy_frame(card: &Card, display: &mut Display) {
  assert!(display.begin_frame());
  let frame = Frame {
    width: 1280,
    height: 720,
    rgba: vec![0; 1280 * 720 * 4],
  };
  display.present(&Drawn::Pixels(frame), Instant::now());
  card.borrow_mut().complete_flip(Duration::from_millis(5), 5);
  let flags = display.dispatch()[0].flags;
  assert!(!flags.contains(PresentationFlags::ZeroCopy));
}

#[test]
fn without_gbm_a_renderer_or_a_shared_modifier_copy_scanout_stays() {
  let reason = |card: &Card, display: &mut Display, render: Option<&[u64]>| {
    let importer = Rc::new(RefCell::new(FakeImporter {
      modifiers: render.map(<[u64]>::to_vec),
      ..FakeImporter::default()
    }));
    display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
    assert_eq!(card.borrow().gbm.borrow().live, 0);
    assert!(card.borrow().framebuffers.is_empty());
    assert_eq!(card.borrow().buffers.len(), 2, "the copy buffers stay");
    flip_copy_frame(card, display);
    match display.scanout_status(OUTPUT) {
      Some(ScanoutStatus::Copy(reason)) => reason,
      status => panic!("copy scanout expected, got {status:?}"),
    }
  };

  let mut fake = FakeKms::single_output();
  fake.no_gbm = true;
  let card = fake.shared();
  let mut display =
    Display::prepare(Box::new(Rc::clone(&card)), OutputPolicy::default())
      .unwrap();
  display.start().unwrap();
  display.set_scanout_choice(ScanoutChoice::Auto);
  let why = reason(&card, &mut display, Some(&[MODIFIER_LINEAR]));
  assert!(why.starts_with("the card has no GBM device: "), "{why}");

  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let why = reason(&card, &mut display, None);
  assert_eq!(why, "the fake renderer has no GPU");

  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let why = reason(&card, &mut display, Some(&[BLOCK_LINEAR]));
  assert!(why.starts_with("no XRGB8888 modifier is both"), "{why}");

  // IN_FORMATS without an XRGB8888 modifier.
  let (card, mut display) = started(&[], ScanoutChoice::Auto);
  let why = reason(&card, &mut display, Some(&[MODIFIER_LINEAR]));
  assert!(why.contains("(none)"), "{why}");
}

#[test]
fn a_plane_without_in_formats_keeps_copy_scanout() {
  let card = FakeKms::single_output().shared();
  let mut display =
    Display::prepare(Box::new(Rc::clone(&card)), OutputPolicy::default())
      .unwrap();
  display.start().unwrap();
  display.set_scanout_choice(ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::Copy(
      "the primary plane has no IN_FORMATS property".into()
    ))
  );
}

#[test]
fn a_refused_test_flip_frees_the_gbm_buffers_and_keeps_copy() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  card.borrow_mut().reject =
    Box::new(|_, flags| (flags == CommitFlags::TEST_ONLY).then_some(22));
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  let Some(ScanoutStatus::Copy(reason)) = display.scanout_status(OUTPUT) else {
    panic!("copy scanout expected");
  };
  assert!(
    reason.starts_with("the card refuses a flip to a GBM buffer: "),
    "{reason}"
  );
  let fake = card.borrow();
  assert!(fake.framebuffers.is_empty());
  assert_eq!(fake.gbm.borrow().live, 0);
  assert_eq!(fake.buffers.len(), 2);
  assert!(
    fake.real_commits().len() == 1,
    "nothing but the first modeset was committed"
  );
}

#[test]
fn a_test_refused_for_lack_of_master_is_tried_again() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  card.borrow_mut().reject_all(13);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(display.scanout_status(OUTPUT), Some(ScanoutStatus::Untried));
  assert_eq!(card.borrow().gbm.borrow().live, 0);
  card.borrow_mut().accept_all();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::ZeroCopy(MODIFIER_LINEAR))
  );
}

#[test]
fn zero_copy_waits_until_the_output_is_ready() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  // A copy frame's flip is pending.
  assert!(display.begin_frame());
  let frame = Frame {
    width: 1280,
    height: 720,
    rgba: vec![0; 1280 * 720 * 4],
  };
  display.present(&Drawn::Pixels(frame), Instant::now());
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(display.scanout_status(OUTPUT), Some(ScanoutStatus::Untried));
  card.borrow_mut().complete_flip(Duration::from_millis(1), 1);
  display.dispatch();
  // Paused, and off.
  display.pause();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(display.scanout_status(OUTPUT), Some(ScanoutStatus::Untried));
  display.resume();
  display.set_dpms(false);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(display.scanout_status(OUTPUT), Some(ScanoutStatus::Untried));
  assert!(importer.borrow().prepared.is_empty());
  display.set_dpms(true);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::ZeroCopy(MODIFIER_LINEAR))
  );
  // The modeset of a later DPMS on or resume shows a GBM buffer.
  display.pause();
  display.resume();
  let (request, _) = card.borrow().real_commits().last().cloned().unwrap();
  let shown = plane_fb(&card, &request).unwrap() as u32;
  assert!(card.borrow().framebuffers.contains_key(&shown));
}

#[test]
fn a_zero_copy_output_the_renderer_cannot_draw_falls_back_to_copy() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  present_in_place(&card, &mut display, 1);
  display.scanout_failed(OUTPUT, "the new device cannot import it");
  // Nothing changes until the next upgrade.
  assert!(target_serial(&display).is_some());
  let before = card.borrow().commits.len();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::Copy(
      "the new device cannot import it".into()
    ))
  );
  let commits = commits_since(&card, before);
  assert_eq!(
    commits.iter().map(|(_, flags)| *flags).collect::<Vec<_>>(),
    vec![CommitFlags::TEST_ONLY.0, 0]
  );
  let shown = plane_fb(&card, &commits[1].0).unwrap() as u32;
  let fake = card.borrow();
  assert!(fake.buffers.contains_key(&shown), "a dumb buffer is shown");
  assert!(fake.framebuffers.is_empty());
  assert_eq!(fake.gbm.borrow().live, 0);
  drop(fake);
  assert!(matches!(display.draw_target(OUTPUT), DrawTarget::Readback));
  flip_copy_frame(&card, &mut display);
  // A failure on a copy output means nothing, and zero-copy is not tried
  // again for this mode.
  display.scanout_failed(OUTPUT, "ignored");
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert!(matches!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::Copy(reason)) if reason.starts_with("the new device")
  ));
  assert_eq!(importer.borrow().prepared.len(), 1);
}

#[test]
fn a_refused_gbm_flip_falls_back_to_copy() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  card.borrow_mut().reject_all(22);
  assert!(display.begin_frame());
  display.present(&Drawn::InPlace, Instant::now());
  card.borrow_mut().accept_all();
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  let Some(ScanoutStatus::Copy(reason)) = display.scanout_status(OUTPUT) else {
    panic!("copy scanout expected");
  };
  assert!(
    reason.starts_with("the page flip to a GBM buffer failed: "),
    "{reason}"
  );
}

#[test]
fn a_new_mode_tries_zero_copy_again_with_buffers_of_its_size() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  display
    .set_mode(RequestedMode::Listed(1))
    .expect("1920x1080 is set");
  assert_eq!(display.scanout_status(OUTPUT), Some(ScanoutStatus::Untried));
  assert!(card.borrow().framebuffers.is_empty(), "the old buffers go");
  assert_eq!(card.borrow().gbm.borrow().live, 0);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::ZeroCopy(MODIFIER_LINEAR))
  );
  assert_eq!(importer.borrow().asked_size, Some((1920, 1080)));
  assert_eq!(importer.borrow().prepared[1].1, vec![4, 5, 6]);
  let fake = card.borrow();
  assert!(
    fake
      .framebuffers
      .values()
      .all(|fb| (fb.width, fb.height) == (1920, 1080))
  );
}

#[test]
fn a_refused_mode_change_keeps_zero_copy() {
  let (card, mut display) = started(&[MODIFIER_LINEAR], ScanoutChoice::Auto);
  let importer = FakeImporter::rendering(&[MODIFIER_LINEAR]);
  display.upgrade_scanout(OUTPUT, &mut Rc::clone(&importer));
  // The test passes, the modeset fails.
  card.borrow_mut().reject =
    Box::new(|_, flags| (flags == CommitFlags::ALLOW_MODESET).then_some(22));
  assert!(display.set_mode(RequestedMode::Listed(1)).is_err());
  card.borrow_mut().accept_all();
  assert_eq!(
    display.scanout_status(OUTPUT),
    Some(ScanoutStatus::ZeroCopy(MODIFIER_LINEAR))
  );
  assert_eq!(card.borrow().framebuffers.len(), 3);
  assert_eq!(target_serial(&display), Some(2));
}
