//! A Wayland client for the native backend's presentation checks
//! (docs/native-testing.md, NB-7): one `xdg_toplevel` that draws a new
//! frame whenever its frame callback fires, asks for `wp_presentation`
//! feedback for each, and prints what it gets as JSON lines on stdout:
//!
//! - `{"event":"commit","n":N,"monotonic_ns":T}` when frame N is committed;
//! - `{"event":"frame","n":N,"time_ms":T}` when frame N's callback fires;
//! - `{"event":"presented","n":N,"sec":S,"nsec":NS,"refresh":R,"seq":Q,
//!   "flags":F}` or `{"event":"discarded","n":N}` for frame N's feedback.
//!
//! tools/native_test/present_check.py compares these with the page flips
//! Shingle logs with `SHINGLE_TRACE_FLIPS=1`. It is an ordinary client and
//! safe in any compositor.
//!
//! ```text
//! present_probe [FRAMES]
//! ```

use std::{
  io::Write,
  os::fd::{AsFd, OwnedFd},
};

use wayland_client::{
  Connection, Dispatch, QueueHandle, WEnum,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
  },
};
use wayland_protocols::{
  wp::presentation_time::client::{wp_presentation, wp_presentation_feedback},
  xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};

/// The globals the probe needs.
#[derive(Default)]
struct Globals {
  compositor: Option<wl_compositor::WlCompositor>,
  shm: Option<wl_shm::WlShm>,
  wm_base: Option<xdg_wm_base::XdgWmBase>,
  presentation: Option<wp_presentation::WpPresentation>,
}

/// The probe's state.
struct Probe {
  globals: Globals,
  surface: Option<wl_surface::WlSurface>,
  /// The size of the last configure, once acknowledged.
  size: Option<(i32, i32)>,
  /// The current buffer and its size.
  buffer: Option<(wl_buffer::WlBuffer, (i32, i32), OwnedFd)>,
  /// Frames committed so far and how many to commit.
  committed: u32,
  frames: u32,
  /// Feedback still outstanding.
  pending_feedback: u32,
  /// Whether a frame callback is outstanding.
  waiting: bool,
  closed: bool,
}

fn print(line: String) {
  let mut stdout = std::io::stdout().lock();
  let _ = writeln!(stdout, "{line}");
  let _ = stdout.flush();
}

fn monotonic_ns() -> u128 {
  let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
  now.tv_sec as u128 * 1_000_000_000 + now.tv_nsec as u128
}

impl Probe {
  /// Fills a buffer of the configured size with a colour that changes
  /// every frame, so each commit is new content.
  fn ensure_buffer(&mut self, handle: &QueueHandle<Self>) -> Option<()> {
    let size = self.size?;
    if self
      .buffer
      .as_ref()
      .is_some_and(|(_, held, _)| *held == size)
    {
      return Some(());
    }
    let shm = self.globals.shm.as_ref()?;
    let bytes = size.0 * size.1 * 4;
    let fd = rustix::fs::memfd_create(
      "present-probe",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .ok()?;
    rustix::fs::ftruncate(&fd, bytes as u64).ok()?;
    let pool = shm.create_pool(fd.as_fd(), bytes, handle, ());
    let buffer = pool.create_buffer(
      0,
      size.0,
      size.1,
      size.0 * 4,
      wl_shm::Format::Xrgb8888,
      handle,
      (),
    );
    pool.destroy();
    self.buffer = Some((buffer, size, fd));
    Some(())
  }

  /// Draws and commits the next frame with a frame callback and
  /// presentation feedback.
  fn draw(&mut self, handle: &QueueHandle<Self>) {
    if self.committed >= self.frames || self.ensure_buffer(handle).is_none() {
      return;
    }
    let (Some(surface), Some((buffer, (width, height), fd))) =
      (self.surface.as_ref(), self.buffer.as_ref())
    else {
      return;
    };
    let shade = (self.committed % 2) as u8 * 0x80 + 0x40;
    let pixels = [shade, shade, shade, 0xff].repeat((width * height) as usize);
    let _ = rustix::io::pwrite(fd, &pixels, 0);
    let n = self.committed;
    surface.frame(handle, n);
    if let Some(presentation) = &self.globals.presentation {
      presentation.feedback(surface, handle, n);
      self.pending_feedback += 1;
    }
    surface.attach(Some(buffer), 0, 0);
    surface.damage_buffer(0, 0, *width, *height);
    surface.commit();
    self.committed += 1;
    self.waiting = true;
    print(format!(
      r#"{{"event":"commit","n":{n},"monotonic_ns":{}}}"#,
      monotonic_ns()
    ));
  }

  fn finished(&self) -> bool {
    self.closed
      || (self.committed >= self.frames
        && !self.waiting
        && self.pending_feedback == 0)
  }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
  fn event(
    probe: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    let wl_registry::Event::Global {
      name, interface, ..
    } = event
    else {
      return;
    };
    let globals = &mut probe.globals;
    match interface.as_str() {
      "wl_compositor" => {
        globals.compositor = Some(registry.bind(name, 4, handle, ()));
      }
      "wl_shm" => globals.shm = Some(registry.bind(name, 1, handle, ())),
      "xdg_wm_base" => {
        globals.wm_base = Some(registry.bind(name, 1, handle, ()));
      }
      "wp_presentation" => {
        globals.presentation = Some(registry.bind(name, 1, handle, ()));
      }
      _ => {}
    }
  }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
  fn event(
    _: &mut Self,
    wm_base: &xdg_wm_base::XdgWmBase,
    event: xdg_wm_base::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_wm_base::Event::Ping { serial } = event {
      wm_base.pong(serial);
    }
  }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
  fn event(
    probe: &mut Self,
    xdg_surface: &xdg_surface::XdgSurface,
    event: xdg_surface::Event,
    _: &(),
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    if let xdg_surface::Event::Configure { serial } = event {
      xdg_surface.ack_configure(serial);
      if !probe.waiting {
        probe.draw(handle);
      }
    }
  }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
  fn event(
    probe: &mut Self,
    _: &xdg_toplevel::XdgToplevel,
    event: xdg_toplevel::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      xdg_toplevel::Event::Configure { width, height, .. } => {
        probe.size = Some(if width > 0 && height > 0 {
          (width, height)
        } else {
          (640, 480)
        });
      }
      xdg_toplevel::Event::Close => probe.closed = true,
      _ => {}
    }
  }
}

impl Dispatch<wl_callback::WlCallback, u32> for Probe {
  fn event(
    probe: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    n: &u32,
    _: &Connection,
    handle: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { callback_data } = event {
      print(format!(
        r#"{{"event":"frame","n":{n},"time_ms":{callback_data}}}"#
      ));
      probe.waiting = false;
      probe.draw(handle);
    }
  }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u32> for Probe {
  fn event(
    probe: &mut Self,
    _: &wp_presentation_feedback::WpPresentationFeedback,
    event: wp_presentation_feedback::Event,
    n: &u32,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wp_presentation_feedback::Event::Presented {
        tv_sec_hi,
        tv_sec_lo,
        tv_nsec,
        refresh,
        seq_hi,
        seq_lo,
        flags,
      } => {
        let sec = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
        let seq = (u64::from(seq_hi) << 32) | u64::from(seq_lo);
        let flags = match flags {
          WEnum::Value(flags) => flags.bits(),
          WEnum::Unknown(bits) => bits,
        };
        print(format!(
          concat!(
            r#"{{"event":"presented","n":{},"sec":{},"nsec":{},"#,
            r#""refresh":{},"seq":{},"flags":{}}}"#
          ),
          n, sec, tv_nsec, refresh, seq, flags
        ));
        probe.pending_feedback -= 1;
      }
      wp_presentation_feedback::Event::Discarded => {
        print(format!(r#"{{"event":"discarded","n":{n}}}"#));
        probe.pending_feedback -= 1;
      }
      _ => {}
    }
  }
}

macro_rules! ignore_events {
  ($($interface:ty),* $(,)?) => {
    $(
      impl Dispatch<$interface, ()> for Probe {
        fn event(
          _: &mut Self,
          _: &$interface,
          _: <$interface as wayland_client::Proxy>::Event,
          _: &(),
          _: &Connection,
          _: &QueueHandle<Self>,
        ) {
        }
      }
    )*
  };
}

ignore_events!(
  wl_compositor::WlCompositor,
  wl_surface::WlSurface,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_buffer::WlBuffer,
  wp_presentation::WpPresentation,
);

/// The frame count from the arguments, 300 by default.
fn frames(args: &[String]) -> Result<u32, String> {
  match args {
    [] => Ok(300),
    [count] => count
      .parse()
      .map_err(|_| format!("not a frame count: {count}")),
    _ => Err("usage: present_probe [FRAMES]".into()),
  }
}

fn main() -> std::process::ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let frames = match frames(&args) {
    Ok(frames) => frames,
    Err(message) => {
      eprintln!("{message}");
      return std::process::ExitCode::from(2);
    }
  };
  let connection = match Connection::connect_to_env() {
    Ok(connection) => connection,
    Err(error) => {
      eprintln!("present_probe: no Wayland display: {error}");
      return std::process::ExitCode::FAILURE;
    }
  };
  let mut queue = connection.new_event_queue();
  let handle = queue.handle();
  connection.display().get_registry(&handle, ());
  let mut probe = Probe {
    globals: Globals::default(),
    surface: None,
    size: None,
    buffer: None,
    committed: 0,
    frames,
    pending_feedback: 0,
    waiting: false,
    closed: false,
  };
  if queue.roundtrip(&mut probe).is_err() {
    return std::process::ExitCode::FAILURE;
  }
  let (Some(compositor), Some(wm_base)) =
    (&probe.globals.compositor, &probe.globals.wm_base)
  else {
    eprintln!("present_probe: no wl_compositor or xdg_wm_base");
    return std::process::ExitCode::FAILURE;
  };
  if probe.globals.presentation.is_none() {
    eprintln!("present_probe: no wp_presentation");
  }
  let surface = compositor.create_surface(&handle, ());
  let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, ());
  let toplevel = xdg_surface.get_toplevel(&handle, ());
  toplevel.set_app_id("present_probe".into());
  toplevel.set_title("present_probe".into());
  surface.commit();
  probe.surface = Some(surface);
  while !probe.finished() {
    if queue.blocking_dispatch(&mut probe).is_err() {
      return std::process::ExitCode::FAILURE;
    }
  }
  std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_frame_count_is_the_only_argument() {
    assert_eq!(frames(&[]), Ok(300));
    assert_eq!(frames(&["5".into()]), Ok(5));
    assert!(frames(&["x".into()]).is_err());
    assert!(frames(&["1".into(), "2".into()]).is_err());
  }
}
