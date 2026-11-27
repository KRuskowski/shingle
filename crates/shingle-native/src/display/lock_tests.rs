//! Modesets while the session lock hides every client: black instead of
//! a frame drawn before the lock (docs/design/lock-screen.md).

use std::{cell::RefCell, rc::Rc, time::Duration};

use super::*;
use crate::{
  kms::{AtomicRequest, Object, fake::FakeKms},
  scanout::Frame,
};

type Card = Rc<RefCell<FakeKms>>;

fn started() -> (Card, Display) {
  let card = FakeKms::single_output().shared();
  let mut display =
    Display::prepare(Box::new(Rc::clone(&card)), OutputPolicy::default())
      .expect("the display starts");
  display.start().expect("the modeset is accepted");
  (card, display)
}

fn plane_fb(card: &Card, request: &AtomicRequest) -> Option<u64> {
  let plane = Object::Plane(card.borrow().plane_id);
  let property = card.borrow_mut().property_id("FB_ID");
  request.get(plane, property)
}

/// Presents one frame and completes its flip; returns its buffer.
fn present(card: &Card, display: &mut Display) -> u64 {
  assert!(display.begin_frame(), "the display is ready for a frame");
  let frame = Drawn::Pixels(Frame {
    width: 1280,
    height: 720,
    rgba: [200, 0, 0, 255].repeat(1280 * 720),
  });
  display.present(&frame, Instant::now());
  let (request, _) = card.borrow().real_commits().pop().unwrap();
  card.borrow_mut().complete_flip(Duration::from_secs(1), 1);
  display.dispatch();
  plane_fb(card, &request).expect("the flip sets a buffer")
}

/// The buffer the last real commit, a modeset, shows.
fn modeset_fb(card: &Card) -> u64 {
  let (request, flags) = card.borrow().real_commits().pop().unwrap();
  assert!(flags.contains(CommitFlags::ALLOW_MODESET), "{flags:?}");
  plane_fb(card, &request).expect("the modeset sets a buffer")
}

#[test]
fn dpms_on_under_the_lock_shows_black_until_a_frame_drawn_under_it() {
  let (card, mut display) = started();
  let desktop = present(&card, &mut display);
  display.set_dpms(false);
  let buffers = card.borrow().buffers.len();
  display.set_lock_hides_clients(true);
  assert_eq!(card.borrow().buffers.len(), buffers + 1, "a black buffer");
  display.set_dpms(true);
  let shown = modeset_fb(&card);
  assert_ne!(
    shown, desktop,
    "the frame from before the lock is not shown"
  );
  assert!(
    card.borrow().buffers[&(shown as u32)]
      .1
      .iter()
      .all(|byte| *byte == 0),
    "the buffer shown is black"
  );
  // A frame drawn under the lock is shown by the next modeset.
  let locked = present(&card, &mut display);
  display.set_dpms(false);
  display.set_dpms(true);
  assert_eq!(modeset_fb(&card), locked);
  // The black buffer goes with the lock.
  display.set_lock_hides_clients(false);
  assert_eq!(card.borrow().buffers.len(), buffers);
}

#[test]
fn the_resume_after_a_vt_switch_under_the_lock_shows_black() {
  let (card, mut display) = started();
  let desktop = present(&card, &mut display);
  display.pause();
  display.set_lock_hides_clients(true);
  display.resume();
  let shown = modeset_fb(&card);
  assert_ne!(shown, desktop);
  // Without the lock the resume shows the last frame, as before.
  display.set_lock_hides_clients(false);
  let (card, mut display) = started();
  let desktop = present(&card, &mut display);
  display.pause();
  display.resume();
  assert_eq!(modeset_fb(&card), desktop);
}

#[test]
fn a_frame_drawn_before_the_lock_was_presented_is_never_reshown() {
  let (card, mut display) = started();
  // Locked from the start: fresh buffers are black, so nothing is made.
  let buffers = card.borrow().buffers.len();
  display.set_lock_hides_clients(true);
  assert_eq!(card.borrow().buffers.len(), buffers);
  display.set_dpms(false);
  display.set_dpms(true);
  let shown = modeset_fb(&card);
  assert!(
    card.borrow().buffers[&(shown as u32)]
      .1
      .iter()
      .all(|byte| *byte == 0)
  );
  display.set_lock_hides_clients(false);
  // A desktop frame, then the lock while the output is on: the next
  // modeset needs a black buffer, made then.
  let desktop = present(&card, &mut display);
  display.set_lock_hides_clients(true);
  display.set_dpms(false);
  display.set_dpms(true);
  assert_ne!(modeset_fb(&card), desktop);
}
