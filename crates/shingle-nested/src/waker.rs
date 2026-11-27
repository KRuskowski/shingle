//! Wakes winit's loop when the compositor has work.
//!
//! winit cannot wait on descriptors of its own, so without this nested
//! served Wayland clients and IPC only when a host event or its 16 ms
//! poll woke it, adding up to 16 ms to every request
//! (docs/status.md#ipc-latency). A [`Waker`] thread polls a duplicate
//! of the compositor's event-loop descriptor ([`Compositor`]'s `AsFd`)
//! and, when it becomes readable, calls its wake function, which sends
//! winit a user event. The descriptor stays readable until the next turn
//! dispatches the compositor, so the thread then waits until
//! [`Waker::arm`] says that turn has run before it polls again.
//!
//! [`Compositor`]: shingle::Compositor

use std::{
  io,
  os::fd::{BorrowedFd, OwnedFd},
  sync::mpsc::{self, Receiver, Sender},
  thread::JoinHandle,
};

use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};

/// The thread waking the loop; see the module documentation. Dropping it
/// stops the thread.
pub(crate) struct Waker {
  armed: Option<Sender<()>>,
  stop: OwnedFd,
  thread: Option<JoinHandle<()>>,
}

impl Waker {
  /// Starts watching `fd`, calling `wake` each time it becomes readable
  /// after [`Waker::arm`]. The thread ends when `wake` returns false, as
  /// when the event loop is gone. The first watch starts at once.
  pub(crate) fn start(
    fd: BorrowedFd<'_>,
    wake: impl Fn() -> bool + Send + 'static,
  ) -> io::Result<Self> {
    let watched = fd.try_clone_to_owned()?;
    let stop = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
    let stop_watched = stop.try_clone()?;
    let (armed, arms) = mpsc::channel();
    armed.send(()).map_err(io::Error::other)?;
    let thread = std::thread::Builder::new()
      .name("nested-waker".into())
      .spawn(move || watch(&watched, &stop_watched, &arms, &wake))?;
    Ok(Self {
      armed: Some(armed),
      stop,
      thread: Some(thread),
    })
  }

  /// Lets the thread watch again: the turn after its last wake-up has
  /// dispatched the compositor.
  pub(crate) fn arm(&self) {
    if let Some(armed) = &self.armed {
      let _ = armed.send(());
    }
  }
}

impl Drop for Waker {
  fn drop(&mut self) {
    // Closing the channel ends a thread waiting to be armed, and the
    // eventfd one in poll.
    self.armed = None;
    let _ = rustix::io::write(&self.stop, &1_u64.to_ne_bytes());
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

/// The thread: waits to be armed, then for `fd` or `stop`, and wakes.
fn watch(
  fd: &OwnedFd,
  stop: &OwnedFd,
  arms: &Receiver<()>,
  wake: &dyn Fn() -> bool,
) {
  while arms.recv().is_ok() {
    // Arms sent while the loop was awake all mean the same turn.
    while arms.try_recv().is_ok() {}
    let mut fds = [
      PollFd::new(fd, PollFlags::IN),
      PollFd::new(stop, PollFlags::IN),
    ];
    loop {
      match poll(&mut fds, None) {
        Ok(_) => break,
        Err(rustix::io::Errno::INTR) => {}
        Err(_) => return,
      }
    }
    if !fds[1].revents().is_empty() || !wake() {
      return;
    }
  }
}

#[cfg(test)]
mod tests {
  use std::{
    sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
  };

  use std::os::fd::AsFd;

  use super::*;

  /// An eventfd to stand for the compositor's descriptor, and a wake
  /// function counting its calls.
  fn fixture() -> (OwnedFd, Arc<AtomicUsize>, impl Fn() -> bool + Send) {
    let fd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
      .expect("eventfd");
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let wake = move || {
      counter.fetch_add(1, Ordering::SeqCst);
      true
    };
    (fd, wakes, wake)
  }

  fn wait_for(wakes: &AtomicUsize, count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
      if wakes.load(Ordering::SeqCst) >= count {
        return true;
      }
      std::thread::sleep(Duration::from_millis(1));
    }
    false
  }

  /// Whether `fd` polls readable now.
  fn readable(fd: &OwnedFd) -> bool {
    let timeout = rustix::event::Timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    poll(&mut fds, Some(&timeout)).unwrap_or(0) > 0
  }

  fn signal(fd: &OwnedFd) {
    rustix::io::write(fd, &1_u64.to_ne_bytes()).expect("eventfd write");
  }

  fn drain(fd: &OwnedFd) {
    let mut value = [0; 8];
    let _ = rustix::io::read(fd, &mut value);
  }

  #[test]
  fn it_wakes_once_per_turn_while_the_descriptor_stays_readable() {
    let (fd, wakes, wake) = fixture();
    let waker = Waker::start(fd.as_fd(), wake).expect("starts");
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(wakes.load(Ordering::SeqCst), 0, "nothing to do yet");
    signal(&fd);
    assert!(wait_for(&wakes, 1), "readable wakes the loop");
    // Still readable, but the turn has not run: no second wake-up.
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    // The turn dispatched it; armed again, the thread waits for more.
    drain(&fd);
    waker.arm();
    waker.arm();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    signal(&fd);
    assert!(wait_for(&wakes, 2), "new work wakes it again");
    assert!(readable(&fd), "the waker never reads the descriptor");
  }

  #[test]
  fn dropping_it_stops_a_thread_waiting_in_poll_or_for_an_arm() {
    let (fd, _, wake) = fixture();
    let waker = Waker::start(fd.as_fd(), wake).expect("starts");
    std::thread::sleep(Duration::from_millis(10));
    let started = Instant::now();
    drop(waker);
    assert!(started.elapsed() < Duration::from_secs(1));
    let (fd, wakes, wake) = fixture();
    let waker = Waker::start(fd.as_fd(), wake).expect("starts");
    signal(&fd);
    assert!(wait_for(&wakes, 1));
    drop(waker);
  }

  #[test]
  fn it_stops_when_the_loop_is_gone() {
    let (fd, _, _) = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let waker = Waker::start(fd.as_fd(), move || {
      counter.fetch_add(1, Ordering::SeqCst);
      false
    })
    .expect("starts");
    signal(&fd);
    assert!(wait_for(&calls, 1));
    waker.arm();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no wake after the end");
    drop(waker);
  }
}
