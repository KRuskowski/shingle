//! The display driven through a fake card.

use std::{cell::RefCell, rc::Rc, time::Duration};

use super::*;
use crate::{
  kms::{
    AtomicRequest, Connection, Object, PropertyId,
    fake::{FakeKms, mode},
  },
  pacing::FLIP_TIMEOUT,
  scanout::Frame,
};

type Card = Rc<RefCell<FakeKms>>;

fn prepare(card: &Card) -> Result<Display, StartError> {
  Display::prepare(Box::new(Rc::clone(card)), OutputPolicy::default())
}

/// A display that passed its test and made its first modeset.
fn started() -> (Card, Display) {
  let card = FakeKms::single_output().shared();
  let mut display = prepare(&card).expect("the display starts");
  display.start().expect("the modeset is accepted");
  (card, display)
}

fn frame(width: u32, height: u32) -> Drawn {
  Drawn::Pixels(Frame {
    width,
    height,
    rgba: [10, 20, 30, 255].repeat((width * height) as usize),
  })
}

fn property(card: &Card, name: &str) -> PropertyId {
  card.borrow_mut().property_id(name)
}

/// The value `request` sets for `name` on the fake's CRTC.
fn crtc_value(card: &Card, request: &AtomicRequest, name: &str) -> Option<u64> {
  let crtc = Object::Crtc(card.borrow().crtc_id);
  request.get(crtc, property(card, name))
}

fn plane_fb(card: &Card, request: &AtomicRequest) -> Option<u64> {
  let plane = Object::Plane(card.borrow().plane_id);
  request.get(plane, property(card, "FB_ID"))
}

/// Draws and flips one frame at `now`.
fn flip_frame(display: &mut Display, now: Instant) {
  assert!(display.begin_frame(), "the display is ready for a frame");
  display.present(&frame(1280, 720), now);
}

#[test]
fn startup_tests_the_preferred_mode_without_committing() {
  let card = FakeKms::single_output().shared();
  let display = prepare(&card).unwrap();
  let fake = card.borrow();
  assert_eq!(fake.commits.len(), 1, "one test, nothing else");
  let (request, flags, accepted) = fake.commits[0].clone();
  let request = &request;
  assert!(accepted);
  assert_eq!(flags, CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET);
  assert_eq!(display.size(), Some((1280, 720)));
  // The full property set of a modeset: connector, CRTC and 10 plane
  // properties, and no VRR on a CRTC without it.
  assert_eq!(request.entries().len(), 13);
  drop(fake);
  assert_eq!(crtc_value(&card, request, "ACTIVE"), Some(1));
  let blob = crtc_value(&card, request, "MODE_ID").unwrap() as u32;
  assert_eq!(card.borrow().blobs[&blob].size(), (1280, 720));
  assert!(!display.ready(), "nothing is shown before the real modeset");
}

#[test]
fn a_rejected_mode_falls_back_to_the_next() {
  let card = FakeKms::single_output().shared();
  card.borrow_mut().reject_width(1280);
  let display = prepare(&card).unwrap();
  assert_eq!(display.size(), Some((1920, 1080)), "next by area");
  let fake = card.borrow();
  let tests: Vec<bool> = fake.commits.iter().map(|(_, _, ok)| *ok).collect();
  assert_eq!(tests, vec![false, true]);
  assert_eq!(fake.blobs.len(), 1, "the rejected mode's blob is gone");
  assert_eq!(fake.buffers.len(), 2, "and so are its buffers");
}

#[test]
fn the_current_crtc_mode_is_the_last_fallback() {
  let card = FakeKms::single_output().shared();
  card.borrow_mut().crtc_mode = Some(mode(800, 600, 60, false));
  card.borrow_mut().reject_widths(vec![1280, 1920, 1024]);
  let display = prepare(&card).unwrap();
  assert_eq!(display.size(), Some((800, 600)));
}

#[test]
fn every_mode_rejected_fails_before_the_socket() {
  let card = FakeKms::single_output().shared();
  card.borrow_mut().reject_all(22);
  let Err(error) = prepare(&card) else {
    panic!("no mode should pass");
  };
  let message = error.to_string();
  assert!(message.starts_with("HDMI-A-1: every mode was rejected: "));
  let order: Vec<_> = ["1280x720@60", "1920x1080@60", "1024x768@60"]
    .iter()
    .map(|mode| message.find(mode).expect("each mode is named"))
    .collect();
  assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{message}");
  assert!(card.borrow().real_commits().is_empty());
  assert!(card.borrow().buffers.is_empty());
}

#[test]
fn another_master_fails_at_once() {
  let card = FakeKms::single_output().shared();
  card.borrow_mut().reject_all(13);
  let error = prepare(&card).err().expect("EACCES is not a mode problem");
  assert!(matches!(error, StartError::NoMaster(_)), "{error}");
  assert_eq!(card.borrow().commits.len(), 1, "no other mode is tried");
  card.borrow_mut().reject_all(16);
  assert!(matches!(prepare(&card), Err(StartError::NoMaster(_))));
}

#[test]
fn drivers_without_atomic_are_refused() {
  let card = FakeKms::single_output().shared();
  card.borrow_mut().no_atomic = true;
  let error = prepare(&card).err().expect("atomic is required");
  assert!(
    error.to_string().starts_with("atomic KMS required"),
    "{error}"
  );
  assert!(card.borrow().commits.is_empty());
}

#[test]
fn no_connected_output_starts_without_scanning_out() {
  let card = FakeKms::single_output().shared();
  card.borrow_mut().disconnect();
  let mut display = prepare(&card).expect("no output is not an error");
  assert_eq!(display.size(), None);
  assert_eq!(display.output_info(), None);
  display.start().unwrap();
  assert!(!display.ready());
  assert!(!display.begin_frame());
  assert!(card.borrow().commits.is_empty());
}

#[test]
fn the_first_modeset_is_tested_then_committed_blocking() {
  let (card, display) = started();
  let fake = card.borrow();
  let flags: Vec<_> = fake.commits.iter().map(|(_, flags, _)| *flags).collect();
  assert_eq!(
    flags,
    vec![
      CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      CommitFlags::ALLOW_MODESET,
    ]
  );
  assert_eq!(fake.commits[1].0, fake.commits[2].0);
  drop(fake);
  assert!(display.ready());
  assert_eq!(display.state(), State::Idle);
}

#[test]
fn frames_flip_to_the_back_buffer_and_wait_for_the_event() {
  let (card, mut display) = started();
  let modeset = card.borrow().commits[2].0.clone();
  let shown = plane_fb(&card, &modeset).unwrap();
  let now = Instant::now();
  flip_frame(&mut display, now);
  let (request, flags) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(flags, CommitFlags::NONBLOCK | CommitFlags::PAGE_FLIP_EVENT);
  assert_eq!(request.entries().len(), 1, "a flip sets FB_ID only");
  let back = plane_fb(&card, &request).unwrap();
  assert_ne!(back, shown);
  let pixels = card.borrow().buffers[&(back as u32)].1[..4].to_vec();
  assert_eq!(pixels, [30, 20, 10, 255], "RGBA became XRGB8888");
  assert_eq!(display.state(), State::FlipPending { since: now });
  assert!(!display.begin_frame(), "no frame before the flip completes");
  assert!(display.dispatch().is_empty(), "no event yet");
  let time = Duration::from_micros(5_000_123);
  card.borrow_mut().complete_flip(time, 41);
  let presented = display.dispatch();
  assert_eq!(
    presented,
    vec![PresentInfo {
      time,
      refresh_ns: 16_666_666,
      seq: 41,
      flags: PresentationFlags::Vsync
        | PresentationFlags::HwClock
        | PresentationFlags::HwCompletion,
    }]
  );
  assert!(display.ready());
  flip_frame(&mut display, now);
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(plane_fb(&card, &request), Some(shown), "buffers alternate");
}

#[test]
fn sequences_keep_counting_past_32_bits() {
  let (card, mut display) = started();
  let mut seqs = Vec::new();
  for sequence in [u32::MAX - 1, u32::MAX, 0, 1] {
    flip_frame(&mut display, Instant::now());
    card
      .borrow_mut()
      .complete_flip(Duration::from_secs(1), sequence);
    seqs.extend(display.dispatch().into_iter().map(|info| info.seq));
  }
  let wrap = 1u64 << 32;
  assert_eq!(seqs, vec![wrap - 2, wrap - 1, wrap, wrap + 1]);
}

#[test]
fn a_lost_flip_times_out_and_is_recovered_with_a_modeset() {
  let (card, mut display) = started();
  let start = Instant::now();
  for round in 1..3u32 {
    let committed = start + FLIP_TIMEOUT * 3 * round;
    flip_frame(&mut display, committed);
    card.borrow_mut().lose_flip();
    let before = card.borrow().commits.len();
    let early = committed + FLIP_TIMEOUT - Duration::from_millis(1);
    assert_eq!(display.check_timeout(early), Ok(()));
    assert_eq!(card.borrow().commits.len(), before, "not yet");
    assert_eq!(display.deadline(), Some(committed + FLIP_TIMEOUT));
    assert_eq!(display.check_timeout(committed + FLIP_TIMEOUT), Ok(()));
    let fake = card.borrow();
    let recovery: Vec<_> = fake.commits[before..]
      .iter()
      .map(|(_, flags, _)| *flags)
      .collect();
    assert_eq!(
      recovery,
      vec![
        CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
        CommitFlags::ALLOW_MODESET,
      ],
      "a blocking modeset after timeout {round}"
    );
    drop(fake);
    assert!(display.ready());
  }
  let committed = start + FLIP_TIMEOUT * 20;
  flip_frame(&mut display, committed);
  card.borrow_mut().lose_flip();
  let fatal = display.check_timeout(committed + FLIP_TIMEOUT).unwrap_err();
  assert_eq!(fatal.0, "3 page flips in a row did not complete");
}

#[test]
fn a_completed_flip_resets_the_timeout_count() {
  let (card, mut display) = started();
  let start = Instant::now();
  for round in 0..5u32 {
    let committed = start + FLIP_TIMEOUT * 3 * round;
    flip_frame(&mut display, committed);
    card.borrow_mut().lose_flip();
    assert_eq!(display.check_timeout(committed + FLIP_TIMEOUT), Ok(()));
    flip_frame(&mut display, committed + FLIP_TIMEOUT);
    card
      .borrow_mut()
      .complete_flip(Duration::from_secs(1), round);
    assert_eq!(display.dispatch().len(), 1);
  }
}

#[test]
fn a_flip_refused_for_lack_of_master_pauses() {
  let (card, mut display) = started();
  card.borrow_mut().reject_all(13);
  flip_frame(&mut display, Instant::now());
  assert_eq!(display.state(), State::Paused);
  assert!(!display.ready());
  card.borrow_mut().accept_all();
  display.pause();
  display.resume();
  assert!(display.ready(), "the seat's resume brings it back");
}

#[test]
fn a_pause_forgets_the_pending_flip_and_resume_modesets_again() {
  let (card, mut display) = started();
  flip_frame(&mut display, Instant::now());
  display.pause();
  assert!(!display.ready());
  // The flip completes while another session owns the display.
  card.borrow_mut().complete_flip(Duration::from_secs(2), 7);
  assert!(
    display.dispatch().is_empty(),
    "a stale flip presents nothing"
  );
  let before = card.borrow().commits.len();
  let resumed = display.resume();
  assert_eq!(resumed, Resumed::default());
  let fake = card.borrow();
  let flags: Vec<_> = fake.commits[before..]
    .iter()
    .map(|(_, flags, _)| *flags)
    .collect();
  assert_eq!(
    flags,
    vec![
      CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      CommitFlags::ALLOW_MODESET,
    ]
  );
  let request = &fake.commits.last().unwrap().0;
  assert_eq!(request.entries().len(), 13, "a full modeset");
  drop(fake);
  assert!(display.ready());
}

#[test]
fn resume_turns_off_what_the_other_session_lit() {
  let (card, mut display) = started();
  display.pause();
  let (connector, crtc, _) = card.borrow_mut().add_lit_neighbours();
  display.resume();
  let request = card.borrow().commits.last().unwrap().0.clone();
  let connector_crtc = property(&card, "CRTC_ID");
  assert_eq!(
    request.get(Object::Connector(connector), connector_crtc),
    Some(0)
  );
  assert_eq!(
    request.get(Object::Crtc(crtc), property(&card, "ACTIVE")),
    Some(0)
  );
}

#[test]
fn an_output_unplugged_while_away_is_gone_after_resume() {
  let (card, mut display) = started();
  display.pause();
  card.borrow_mut().disconnect();
  let resumed = display.resume();
  assert!(resumed.output_changed);
  assert_eq!(display.size(), None);
  assert!(!display.ready());
  assert!(card.borrow().buffers.is_empty(), "its buffers are freed");
  assert!(card.borrow().blobs.is_empty(), "and its mode blob");
  display.pause();
  card.borrow_mut().connection = Connection::Connected;
  let resumed = display.resume();
  assert!(resumed.output_changed);
  assert_eq!(resumed.resized, Some((1280, 720)));
  assert!(display.ready());
}

#[test]
fn a_new_mode_list_after_resume_picks_a_mode_again() {
  let (card, mut display) = started();
  display.pause();
  card.borrow_mut().modes = vec![mode(1920, 1080, 60, true)];
  let resumed = display.resume();
  assert_eq!(resumed.resized, Some((1920, 1080)));
  assert!(!resumed.output_changed);
  assert_eq!(display.output_info().unwrap().refresh_mhz, 60_000);
  assert!(display.ready());
}

#[test]
fn dpms_off_clears_active_and_on_modesets_again() {
  let (card, mut display) = started();
  let before = card.borrow().commits.len();
  display.set_dpms(false);
  assert!(!display.dpms_on());
  assert!(!display.ready());
  let fake = card.borrow();
  let off: Vec<_> = fake.commits[before..].to_vec();
  drop(fake);
  assert_eq!(off.len(), 2);
  assert_eq!(
    off[0].1,
    CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET
  );
  assert_eq!(off[1].1, CommitFlags::ALLOW_MODESET);
  let crtc = Object::Crtc(card.borrow().crtc_id);
  assert_eq!(
    off[1].0.entries(),
    &[(crtc, property(&card, "ACTIVE"), 0)],
    "DPMS off changes ACTIVE only"
  );
  assert_eq!(card.borrow().value(crtc.id(), "ACTIVE"), 0);
  display.set_dpms(true);
  assert!(display.dpms_on());
  assert!(display.ready());
  let (request, flags) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(flags, CommitFlags::ALLOW_MODESET);
  assert_eq!(crtc_value(&card, &request, "ACTIVE"), Some(1));
  assert_eq!(request.entries().len(), 13, "DPMS on is a full modeset");
}

#[test]
fn dpms_waits_for_the_pending_flip() {
  let (card, mut display) = started();
  flip_frame(&mut display, Instant::now());
  let before = card.borrow().commits.len();
  display.set_dpms(false);
  assert_eq!(
    card.borrow().commits.len(),
    before,
    "no commit while pending"
  );
  assert!(display.dpms_on());
  card.borrow_mut().complete_flip(Duration::from_secs(1), 1);
  assert_eq!(display.dispatch().len(), 1, "the flip still presents");
  assert!(!display.dpms_on(), "then the display goes off");
}

#[test]
fn dpms_changed_while_paused_is_applied_by_the_resume() {
  let (card, mut display) = started();
  display.pause();
  display.set_dpms(false);
  assert_eq!(card.borrow().commits.len(), 3, "nothing while paused");
  display.resume();
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(crtc_value(&card, &request, "ACTIVE"), Some(0));
  assert!(!display.dpms_on());
  assert!(!display.ready());
}

#[test]
fn the_output_is_described_from_the_connector_and_mode() {
  let (_, display) = started();
  let info = display.output_info().unwrap();
  assert_eq!(info.name, "HDMI-A-1");
  assert_eq!(info.physical_size_mm, (600, 340));
  assert_eq!(info.subpixel, wl_output::Subpixel::HorizontalRgb);
  assert_eq!(info.refresh_mhz, 60_000);
}

#[test]
fn start_errors_name_their_cause() {
  let none = StartError::NoDrmDevice(Vec::new());
  assert_eq!(
    none.to_string(),
    "no DRM device: no card found under /sys/class/drm"
  );
  let tried = StartError::NoDrmDevice(vec![
    (PathBuf::from("/dev/dri/card1"), "Permission denied".into()),
    (PathBuf::from("/dev/dri/card0"), "No such device".into()),
  ]);
  assert_eq!(
    tried.to_string(),
    "no DRM device: tried /dev/dri/card1 (Permission denied), \
     /dev/dri/card0 (No such device)"
  );
}

fn policy(named: &[(&str, &str)]) -> OutputPolicy {
  OutputPolicy {
    default: shingle_config::OutputMode::Auto,
    named: named
      .iter()
      .map(|(name, mode)| ((*name).to_owned(), mode.parse().unwrap()))
      .collect(),
    ..OutputPolicy::default()
  }
}

/// A started display on HDMI-A-1 with a second connector, DP-2, that is
/// unplugged until a test plugs it in.
fn with_second_connector() -> (Card, Display) {
  let mut fake = FakeKms::single_output();
  let mut dp = crate::kms::fake::connector_info(9, "DP-2");
  dp.connection = Connection::Disconnected;
  dp.modes = vec![mode(2560, 1440, 60, true), mode(1920, 1080, 60, false)];
  fake.add_connector(dp);
  let card = fake.shared();
  let mut display = prepare(&card).expect("the display starts");
  display.start().expect("the modeset is accepted");
  (card, display)
}

/// The connector the last real modeset lit.
fn lit_connector(card: &Card) -> Option<u64> {
  let crtc_id = property(card, "CRTC_ID");
  let commits = card.borrow().real_commits();
  commits.iter().rev().find_map(|(request, _)| {
    request
      .entries()
      .iter()
      .find_map(|(object, property, value)| {
        (matches!(object, Object::Connector(_))
          && *property == crtc_id
          && *value != 0)
          .then(|| u64::from(object.id()))
      })
  })
}

#[test]
fn the_configured_mode_is_lit_and_off_outputs_are_skipped() {
  let card = FakeKms::single_output().shared();
  let display = Display::prepare(
    Box::new(Rc::clone(&card)),
    policy(&[("HDMI-A-1", "1920x1080@60")]),
  )
  .unwrap();
  assert_eq!(display.size(), Some((1920, 1080)));
  let off = Display::prepare(
    Box::new(FakeKms::single_output()),
    policy(&[("HDMI-A-1", "off")]),
  )
  .unwrap();
  assert_eq!(off.size(), None, "an output configured off stays dark");
  let highres = Display::prepare(
    Box::new(FakeKms::single_output()),
    policy(&[("HDMI-A-1", "highres")]),
  )
  .unwrap();
  assert_eq!(highres.size(), Some((1920, 1080)));
}

#[test]
fn a_hotplug_that_leaves_the_lit_output_alone_changes_nothing() {
  let (card, mut display) = with_second_connector();
  let before = card.borrow().commits.len();
  card
    .borrow_mut()
    .set_connection("DP-2", Connection::Connected);
  let change = display.hotplug();
  assert_eq!(change, Resumed::default());
  assert_eq!(card.borrow().commits.len(), before, "no modeset");
  assert_eq!(display.output_name(), Some("HDMI-A-1"));
  assert!(display.ready());
}

#[test]
fn unplugging_the_lit_output_lights_the_next_one() {
  let (card, mut display) = with_second_connector();
  card
    .borrow_mut()
    .set_connection("DP-2", Connection::Connected);
  display.hotplug();
  card.borrow_mut().disconnect();
  let change = display.hotplug();
  assert!(change.output_changed);
  assert_eq!(change.resized, Some((2560, 1440)));
  assert_eq!(display.output_name(), Some("DP-2"));
  let dp = card.borrow().connector_named("DP-2");
  assert_eq!(lit_connector(&card), Some(u64::from(dp.0)));
  assert!(display.ready());
  // Plugging HDMI-A-1 back in keeps DP-2 lit.
  card
    .borrow_mut()
    .set_connection("HDMI-A-1", Connection::Connected);
  assert_eq!(display.hotplug(), Resumed::default());
  assert_eq!(display.output_name(), Some("DP-2"));
}

#[test]
fn with_every_output_unplugged_nothing_is_drawn_until_one_returns() {
  let (card, mut display) = started();
  flip_frame(&mut display, Instant::now());
  card.borrow_mut().disconnect();
  let change = display.hotplug();
  assert!(change.output_changed);
  assert_eq!(display.size(), None);
  assert!(!display.ready());
  assert!(!display.begin_frame());
  assert!(card.borrow().buffers.is_empty(), "its buffers are freed");
  // The flip of the unplugged output completes late and is ignored.
  card.borrow_mut().complete_flip(Duration::from_secs(1), 1);
  assert!(display.dispatch().is_empty());
  card.borrow_mut().connection = Connection::Connected;
  let change = display.hotplug();
  assert!(change.output_changed);
  assert_eq!(change.resized, Some((1280, 720)));
  assert!(display.ready());
}

#[test]
fn a_replugged_monitor_with_other_modes_gets_a_new_mode() {
  let (card, mut display) = started();
  card.borrow_mut().disconnect();
  display.hotplug();
  {
    let mut fake = card.borrow_mut();
    fake.connection = Connection::Connected;
    fake.modes = vec![mode(3840, 2160, 60, true), mode(1920, 1080, 60, false)];
  }
  let change = display.hotplug();
  assert_eq!(change.resized, Some((3840, 2160)));
  assert_eq!(display.output_info().unwrap().modes.len(), 2);
}

#[test]
fn hotplugs_while_paused_wait_for_the_resume() {
  let (card, mut display) = started();
  display.pause();
  card.borrow_mut().disconnect();
  let before = card.borrow().commits.len();
  assert_eq!(display.hotplug(), Resumed::default());
  assert_eq!(card.borrow().commits.len(), before);
  assert!(display.resume().output_changed);
}

#[test]
fn a_reloaded_policy_relights_with_the_new_mode() {
  let (card, mut display) = started();
  let change = display.set_policy(policy(&[("HDMI-A-1", "1920x1080")]));
  assert_eq!(change.resized, Some((1920, 1080)));
  let before = card.borrow().commits.len();
  assert_eq!(
    display.set_policy(policy(&[("HDMI-A-1", "1920x1080@60")])),
    Resumed::default(),
    "the same mode is not set again"
  );
  assert_eq!(card.borrow().commits.len(), before);
  let change = display.set_policy(policy(&[("HDMI-A-1", "off")]));
  assert!(change.output_changed);
  assert_eq!(display.size(), None);
}

#[test]
fn mode_requests_are_tested_before_they_are_made() {
  let (card, mut display) = started();
  let before = card.borrow().commits.len();
  display.test_mode(RequestedMode::Listed(1)).unwrap();
  {
    let fake = card.borrow();
    let tests: Vec<_> = fake.commits[before..].iter().map(|c| c.1).collect();
    assert_eq!(
      tests,
      [CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET],
      "a test commits nothing"
    );
    assert_eq!(fake.buffers.len(), 2, "its buffers are freed again");
  }
  assert_eq!(display.size(), Some((1280, 720)));
  let change = display.set_mode(RequestedMode::Listed(1)).unwrap();
  assert_eq!(change.resized, Some((1920, 1080)));
  let fake = card.borrow();
  let flags: Vec<_> = fake.commits[before + 1..].iter().map(|c| c.1).collect();
  assert_eq!(
    flags,
    [
      CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET,
      CommitFlags::ALLOW_MODESET,
    ]
  );
  assert_eq!(fake.buffers.len(), 2, "the old buffers are gone");
  assert_eq!(fake.blobs.len(), 1);
  drop(fake);
  assert_eq!(display.output_info().unwrap().current_mode, Some(1));
}

#[test]
fn a_rejected_mode_request_keeps_the_old_mode() {
  let (card, mut display) = started();
  card.borrow_mut().reject_width(1920);
  let error = display.set_mode(RequestedMode::Listed(1)).unwrap_err();
  assert!(error.starts_with("1920x1080@60"), "{error}");
  assert!(display.test_mode(RequestedMode::Listed(1)).is_err());
  let missing = RequestedMode::Custom {
    width: 800,
    height: 600,
    refresh_mhz: 0,
  };
  assert_eq!(
    display.set_mode(missing).unwrap_err(),
    "the output does not offer that mode"
  );
  assert_eq!(display.size(), Some((1280, 720)));
  assert_eq!(card.borrow().buffers.len(), 2);
  assert_eq!(card.borrow().blobs.len(), 1);
  display.pause();
  assert_eq!(
    display.test_mode(RequestedMode::Listed(0)).unwrap_err(),
    "the seat is not active"
  );
}

#[test]
fn the_edid_names_make_model_and_serial_and_modes_are_listed() {
  let mut fake = FakeKms::single_output();
  let id = fake.connector_id;
  fake.edids.insert(
    id,
    crate::kms::edid::tests::blob(
      "DEL",
      0x1234,
      0,
      &[(0xfc, "Test Monitor"), (0xff, "SN-1")],
    ),
  );
  let card = fake.shared();
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  let info = display.output_info().unwrap();
  // The make is hwdata's name for DEL where hwdata is installed.
  let make = crate::kms::edid::system_manufacturer("DEL")
    .unwrap_or_else(|| "DEL".into());
  assert_eq!(
    (
      info.make.as_str(),
      info.model.as_str(),
      info.serial.as_deref()
    ),
    (make.as_str(), "Test Monitor", Some("SN-1"))
  );
  assert_eq!(
    info.description(),
    format!("{make} Test Monitor SN-1 (HDMI-A-1)")
  );
  let sizes: Vec<_> = info
    .modes
    .iter()
    .map(|mode| (mode.width, mode.height, mode.preferred))
    .collect();
  assert_eq!(
    sizes,
    [(1280, 720, true), (1920, 1080, false), (1024, 768, false)]
  );
  assert_eq!(info.current_mode, Some(0));
  let (_, plain) = started();
  let info = plain.output_info().unwrap();
  assert_eq!((info.make.as_str(), info.serial), ("Unknown", None));
}

/// A card with HDMI-A-1 on CRTC 2 and `extra` more connectors, each on a
/// CRTC and primary plane of its own, all connected, with the CRTC of
/// each extra connector.
fn pipes(extra: &[(&str, Vec<Mode>)]) -> (Card, Vec<CrtcId>) {
  let mut fake = FakeKms::single_output();
  let mut crtcs = Vec::new();
  for (index, (name, modes)) in extra.iter().enumerate() {
    let mut info = crate::kms::fake::connector_info(20 + index as u32, name);
    info.modes = modes.clone();
    crtcs.push(fake.add_pipe(info));
  }
  (fake.shared(), crtcs)
}

/// DP-1's modes: 1920x1080 at 60 Hz preferred, then 1280x720 at 30 Hz.
fn dp_modes() -> Vec<Mode> {
  vec![mode(1920, 1080, 60, true), mode(1280, 720, 30, false)]
}

/// The value `request` sets for `name` on `object`.
fn value(
  card: &Card,
  request: &AtomicRequest,
  object: Object,
  name: &str,
) -> Option<u64> {
  request.get(object, property(card, name))
}

#[test]
fn two_monitors_are_lit_with_one_combined_modeset() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes())]);
  let mut display = prepare(&card).unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  assert_eq!(display.size_of("DP-1"), Some((1920, 1080)));
  {
    let fake = card.borrow();
    let flags: Vec<_> = fake
      .commits
      .iter()
      .map(|(_, flags, ok)| (*flags, *ok))
      .collect();
    let test = CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET;
    assert_eq!(
      flags,
      [(test, true), (test, true)],
      "one test per new output"
    );
    assert!(fake.real_commits().is_empty());
    // The second test holds both outputs.
    assert_eq!(fake.commits[1].0.entries().len(), 26);
  }
  display.start().unwrap();
  let (request, flags) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(flags, CommitFlags::ALLOW_MODESET);
  let dp = card.borrow().connector_named("DP-1");
  let hdmi = card.borrow().connector_id;
  assert_eq!(
    value(&card, &request, Object::Connector(dp), "CRTC_ID"),
    Some(u64::from(crtcs[0].0))
  );
  assert_eq!(
    value(&card, &request, Object::Connector(hdmi), "CRTC_ID"),
    Some(u64::from(card.borrow().crtc_id.0))
  );
  assert!(display.ready_on("HDMI-A-1") && display.ready_on("DP-1"));
  assert_eq!(display.output_info_of("DP-1").unwrap().refresh_mhz, 60_000);
  assert!(display.unlit().is_empty());
}

#[test]
fn the_combined_modeset_turns_off_only_what_no_output_uses() {
  let (card, _) = pipes(&[("DP-1", dp_modes())]);
  let (neighbour, neighbour_crtc, cursor) =
    card.borrow_mut().add_lit_neighbours();
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(
    value(&card, &request, Object::Connector(neighbour), "CRTC_ID"),
    Some(0)
  );
  assert_eq!(
    value(&card, &request, Object::Crtc(neighbour_crtc), "ACTIVE"),
    Some(0)
  );
  assert_eq!(
    value(&card, &request, Object::Plane(cursor), "FB_ID"),
    Some(0)
  );
  // Our two outputs are on, and nothing of theirs is turned off.
  let on = |crtc: CrtcId| value(&card, &request, Object::Crtc(crtc), "ACTIVE");
  let ours: Vec<_> = display
    .outputs()
    .iter()
    .map(|name| display.find(name).unwrap().crtc())
    .collect();
  assert_eq!(
    ours.iter().map(|crtc| on(*crtc)).collect::<Vec<_>>(),
    [Some(1), Some(1)]
  );
  assert_eq!(request.entries().len(), 26 + 5);
}

#[test]
fn a_failing_combined_test_leaves_the_lowest_priority_output_dark() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes())]);
  // The two CRTCs cannot be active together, as with too little
  // bandwidth for both.
  {
    let mut fake = card.borrow_mut();
    let active = fake.property_id("ACTIVE");
    let (first, second) = (Object::Crtc(fake.crtc_id), Object::Crtc(crtcs[0]));
    fake.reject = Box::new(move |request, _| {
      (request.get(first, active) == Some(1)
        && request.get(second, active) == Some(1))
      .then_some(28)
    });
  }
  let display = prepare(&card).unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1"]);
  let tests = card
    .borrow()
    .commits
    .iter()
    .filter(|(_, _, ok)| !ok)
    .count();
  assert_eq!(tests, 2, "every mode of DP-1 was tried");
  let unlit = display.unlit();
  assert_eq!(unlit.len(), 1);
  assert_eq!(unlit[0].0.name, "DP-1");
  assert_eq!(unlit[0].0.current_mode, None);
  assert_eq!(unlit[0].1, (1920, 1080), "its preferred mode");
}

#[test]
fn a_shared_limit_gives_the_later_output_a_smaller_mode() {
  let (card, _) = pipes(&[("DP-1", dp_modes())]);
  // Together the outputs may be at most 3000 pixels wide.
  {
    let mut fake = card.borrow_mut();
    let width = fake.property_id("CRTC_W");
    fake.reject = Box::new(move |request, _| {
      let total: u64 = request
        .entries()
        .iter()
        .filter(|(object, property, _)| {
          *property == width && matches!(object, Object::Plane(_))
        })
        .map(|(_, _, value)| *value)
        .sum();
      (total > 3000).then_some(28)
    });
  }
  let display = prepare(&card).unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  assert_eq!(display.size_of("HDMI-A-1"), Some((1280, 720)));
  assert_eq!(display.size_of("DP-1"), Some((1280, 720)), "its next mode");
  assert_eq!(display.output_info_of("DP-1").unwrap().refresh_mhz, 30_000);
}

#[test]
fn flips_are_paced_and_routed_per_crtc() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes())]);
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  let now = Instant::now();
  assert!(display.begin_frame_on("HDMI-A-1"));
  display.present_on("HDMI-A-1", &frame(1280, 720), now);
  assert!(display.begin_frame_on("DP-1"));
  display.present_on("DP-1", &frame(1920, 1080), now);
  assert!(!display.ready_on("HDMI-A-1") && !display.ready_on("DP-1"));
  let hdmi_crtc = card.borrow().crtc_id;
  card
    .borrow_mut()
    .complete_flip_on(crtcs[0], Duration::from_millis(5), 900);
  let presented = display.dispatch_flips();
  assert_eq!(presented.len(), 1);
  assert_eq!(presented[0].0, "DP-1");
  assert_eq!(presented[0].1.seq, 900);
  assert_eq!(presented[0].1.refresh_ns, 16_666_666);
  assert!(display.ready_on("DP-1"), "DP-1 may draw again");
  assert!(!display.ready_on("HDMI-A-1"), "HDMI-A-1 still waits");
  assert!(display.flip_pending());
  // DP-1 flips again while HDMI-A-1's flip is pending.
  assert!(display.begin_frame_on("DP-1"));
  display.present_on("DP-1", &frame(1920, 1080), now);
  card
    .borrow_mut()
    .complete_flip_on(hdmi_crtc, Duration::from_millis(9), 3);
  card
    .borrow_mut()
    .complete_flip_on(crtcs[0], Duration::from_millis(21), 901);
  let names: Vec<_> = display
    .dispatch_flips()
    .into_iter()
    .map(|(name, info)| (name, info.seq))
    .collect();
  assert_eq!(
    names,
    [("HDMI-A-1".to_owned(), 3), ("DP-1".to_owned(), 901)]
  );
  assert!(!display.flip_pending());
}

#[test]
fn a_lost_flip_on_one_crtc_recovers_that_output_alone() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes())]);
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  let start = Instant::now();
  assert!(display.begin_frame_on("HDMI-A-1"));
  display.present_on("HDMI-A-1", &frame(1280, 720), start);
  assert!(display.begin_frame_on("DP-1"));
  display.present_on("DP-1", &frame(1920, 1080), start + FLIP_TIMEOUT / 2);
  // HDMI-A-1's flip is lost; DP-1's is still in time.
  let first = card.borrow().crtc_id;
  card.borrow_mut().pending.remove(&first);
  let before = card.borrow().commits.len();
  assert_eq!(display.check_timeout(start + FLIP_TIMEOUT), Ok(()));
  let recovery = card.borrow().commits[before..].to_vec();
  assert_eq!(recovery.len(), 2, "a tested modeset");
  let request = &recovery[1].0;
  assert_eq!(
    value(&card, request, Object::Crtc(first), "ACTIVE"),
    Some(1)
  );
  assert_eq!(
    value(&card, request, Object::Crtc(crtcs[0]), "ACTIVE"),
    None,
    "DP-1 and its pending flip are left alone"
  );
  assert!(display.ready_on("HDMI-A-1"));
  assert!(!display.ready_on("DP-1"));
}

#[test]
fn monitors_plugged_in_later_are_added_and_unplugged_ones_removed() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes()), ("DP-2", dp_modes())]);
  card
    .borrow_mut()
    .set_connection("DP-1", Connection::Disconnected);
  card
    .borrow_mut()
    .set_connection("DP-2", Connection::Disconnected);
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1"]);
  card
    .borrow_mut()
    .set_connection("DP-1", Connection::Connected);
  let change = display.hotplug();
  assert!(change.outputs_changed);
  assert!(!change.output_changed, "HDMI-A-1 stays the first");
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  card
    .borrow_mut()
    .set_connection("DP-2", Connection::Connected);
  display.hotplug();
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1", "DP-2"]);
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  let active =
    |crtc: CrtcId| value(&card, &request, Object::Crtc(crtc), "ACTIVE");
  let first = card.borrow().crtc_id;
  assert_eq!(
    [active(first), active(crtcs[0]), active(crtcs[1])],
    [Some(1), Some(1), Some(1)],
    "one modeset of all three"
  );
  // Unplugging DP-1 turns its CRTC off and keeps the others.
  card
    .borrow_mut()
    .set_connection("DP-1", Connection::Disconnected);
  let change = display.hotplug();
  assert!(change.outputs_changed);
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-2"]);
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  let active =
    |crtc: CrtcId| value(&card, &request, Object::Crtc(crtc), "ACTIVE");
  assert_eq!(
    [active(first), active(crtcs[0]), active(crtcs[1])],
    [Some(1), Some(0), Some(1)]
  );
  assert!(display.ready_on("HDMI-A-1") && display.ready_on("DP-2"));
  // A hotplug that changes nothing commits nothing.
  let commits = card.borrow().commits.len();
  assert_eq!(display.hotplug(), Resumed::default());
  assert_eq!(card.borrow().commits.len(), commits);
}

#[test]
fn a_monitor_without_a_free_crtc_is_lit_once_one_is_free() {
  // DP-2 can only use HDMI-A-1's CRTC.
  let mut fake = FakeKms::single_output();
  let mut dp1 = crate::kms::fake::connector_info(20, "DP-1");
  dp1.modes = dp_modes();
  fake.add_pipe(dp1);
  fake.add_connector(crate::kms::fake::connector_info(21, "DP-2"));
  let card = fake.shared();
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  let unlit: Vec<_> = display
    .unlit()
    .into_iter()
    .map(|(info, _)| info.name)
    .collect();
  assert_eq!(unlit, ["DP-2"]);
  card.borrow_mut().disconnect();
  display.hotplug();
  assert_eq!(
    display.outputs(),
    ["DP-1", "DP-2"],
    "DP-2 takes the free CRTC"
  );
  assert!(display.unlit().is_empty());
}

#[test]
fn outputs_turn_off_and_on_one_at_a_time() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes())]);
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  // HDMI-A-1 has a flip pending; DP-1 turns off anyway.
  assert!(display.begin_frame_on("HDMI-A-1"));
  display.present_on("HDMI-A-1", &frame(1280, 720), Instant::now());
  display.set_output_dpms("DP-1", false);
  assert!(!display.output_on("DP-1"));
  assert!(display.output_on("HDMI-A-1"));
  assert!(!display.dpms_on());
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(
    request.entries(),
    &[(Object::Crtc(crtcs[0]), property(&card, "ACTIVE"), 0)]
  );
  // HDMI-A-1 waits for its flip.
  display.set_output_dpms("HDMI-A-1", false);
  assert!(display.output_on("HDMI-A-1"));
  card.borrow_mut().complete_flip(Duration::from_secs(1), 1);
  assert_eq!(display.dispatch_flips().len(), 1);
  assert!(!display.output_on("HDMI-A-1"));
  display.set_output_dpms("DP-1", true);
  assert!(display.output_on("DP-1") && display.ready_on("DP-1"));
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(request.entries().len(), 13, "a modeset of DP-1 alone");
}

#[test]
fn a_configuration_across_heads_is_tested_and_applied_at_once() {
  let (card, crtcs) = pipes(&[("DP-1", dp_modes())]);
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  let heads = [
    (
      "HDMI-A-1".to_owned(),
      HeadState::On(Some(RequestedMode::Listed(1))),
    ),
    ("DP-1".to_owned(), HeadState::Off),
  ];
  let before = card.borrow().commits.len();
  display.test_configuration(&heads).unwrap();
  assert_eq!(card.borrow().commits.len(), before + 1, "one test");
  assert_eq!(
    display.outputs(),
    ["HDMI-A-1", "DP-1"],
    "a test changes nothing"
  );
  let change = display.apply_configuration(&heads).unwrap();
  assert_eq!(change.resized, Some((1920, 1080)));
  assert!(change.outputs_changed);
  assert_eq!(display.outputs(), ["HDMI-A-1"]);
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  assert_eq!(
    value(&card, &request, Object::Crtc(crtcs[0]), "ACTIVE"),
    Some(0),
    "DP-1's CRTC goes off in the same modeset"
  );
  assert_eq!(card.borrow().blobs.len(), 1, "DP-1's blob is freed");
  // Output management keeps DP-1 off across hotplugs and reloads.
  display.hotplug();
  display.set_policy(policy(&[("DP-1", "auto")]));
  assert_eq!(display.outputs(), ["HDMI-A-1"]);
  assert_eq!(display.size(), Some((1920, 1080)), "and HDMI-A-1's mode");
  // Turning DP-1 on lights it on its free CRTC.
  let on = [("DP-1".to_owned(), HeadState::On(None))];
  display.apply_configuration(&on).unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  assert_eq!(display.size_of("DP-1"), Some((1920, 1080)));
}

#[test]
fn a_refused_configuration_keeps_every_output_as_it_was() {
  let (card, _) = pipes(&[("DP-1", dp_modes())]);
  let mut display = prepare(&card).unwrap();
  display.start().unwrap();
  let buffers = card.borrow().buffers.len();
  let blobs = card.borrow().blobs.len();
  // The real modeset of DP-1 at 1280 pixels fails after its test passed.
  let plane = Object::Plane(display.find("DP-1").unwrap().plane());
  {
    let mut fake = card.borrow_mut();
    let width = fake.property_id("CRTC_W");
    fake.reject = Box::new(move |request, flags| {
      (request.get(plane, width) == Some(1280)
        && !flags.contains(CommitFlags::TEST_ONLY))
      .then_some(22)
    });
  }
  let heads = [(
    "DP-1".to_owned(),
    HeadState::On(Some(RequestedMode::Listed(1))),
  )];
  let error = display.apply_configuration(&heads).unwrap_err();
  assert!(error.starts_with("1280x720@30"), "{error}");
  assert_eq!(display.size_of("DP-1"), Some((1920, 1080)));
  assert_eq!(display.outputs(), ["HDMI-A-1", "DP-1"]);
  assert!(
    display.ready_on("HDMI-A-1") && display.ready_on("DP-1"),
    "the old configuration was set again"
  );
  card.borrow_mut().accept_all();
  assert_eq!(card.borrow().buffers.len(), buffers);
  assert_eq!(card.borrow().blobs.len(), blobs);
  let missing = [("DP-9".to_owned(), HeadState::On(None))];
  assert_eq!(
    display.test_configuration(&missing).unwrap_err(),
    "DP-9 is not connected"
  );
}

#[test]
fn outputs_beyond_the_limit_stay_dark() {
  let (card, _) = pipes(&[("DP-1", dp_modes())]);
  let display = Display::prepare_limited(
    Box::new(Rc::clone(&card)),
    OutputPolicy::default(),
    Some(1),
  )
  .unwrap();
  assert_eq!(display.outputs(), ["HDMI-A-1"]);
  assert_eq!(display.unlit().len(), 1);
}

#[test]
fn a_named_output_comes_before_the_others() {
  let (card, _) = pipes(&[("DP-1", dp_modes())]);
  let display =
    Display::prepare(Box::new(Rc::clone(&card)), policy(&[("DP-1", "auto")]))
      .unwrap();
  assert_eq!(display.outputs(), ["DP-1", "HDMI-A-1"]);
  assert_eq!(display.output_name(), Some("DP-1"));
}
