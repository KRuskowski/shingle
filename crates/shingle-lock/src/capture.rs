//! Screenshots of every output, taken with wlr-screencopy before the lock
//! is asked for, as swaylock-effects' `--screenshots` does: once the
//! lock is pending, the compositor shows no client any more
//! (docs/design/lock-screen.md).

use std::{
  collections::HashMap,
  os::fd::{AsFd, OwnedFd},
  time::{Duration, Instant},
};

use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, WEnum,
  globals::GlobalList,
  protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
  zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
  zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// One output's pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screenshot {
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// Tightly packed B, G, R, A rows from the top, alpha 255.
  pub bgra: Vec<u8>,
}

/// A frame's buffer as the compositor described it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
  format: wl_shm::Format,
  width: u32,
  height: u32,
  stride: u32,
}

/// One output's capture in flight.
struct Pending {
  frame: ZwlrScreencopyFrameV1,
  version: u32,
  shape: Option<Shape>,
  memory: Option<(OwnedFd, wl_buffer::WlBuffer)>,
  y_invert: bool,
  done: Option<Option<Screenshot>>,
}

/// The state of the capture queue.
struct Capturer {
  shm: wl_shm::WlShm,
  pending: HashMap<u32, Pending>,
}

/// Converts `stride`-separated rows of `format` to [`Screenshot::bgra`],
/// flipping them with `y_invert`; `None` for a format it does not read.
fn convert(data: &[u8], shape: Shape, y_invert: bool) -> Option<Screenshot> {
  let swap = match shape.format {
    wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => false,
    wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => true,
    _ => return None,
  };
  let row = shape.width as usize * 4;
  let stride = shape.stride as usize;
  if stride < row || data.len() < stride * shape.height as usize {
    return None;
  }
  let mut bgra = Vec::with_capacity(row * shape.height as usize);
  for y in 0..shape.height as usize {
    let source = if y_invert {
      shape.height as usize - 1 - y
    } else {
      y
    };
    for pixel in data[source * stride..source * stride + row].chunks_exact(4) {
      if swap {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xff]);
      } else {
        bgra.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
      }
    }
  }
  Some(Screenshot {
    width: shape.width,
    height: shape.height,
    bgra,
  })
}

/// Captures every `wl_output` in `globals` without the cursor, waiting at
/// most `timeout` for all of them; returns the screenshots by the
/// output's global name. Outputs that fail or run out of time are left
/// out, and so is everything when the compositor has no screencopy.
pub fn capture_outputs(
  connection: &Connection,
  globals: &GlobalList,
  timeout: Duration,
) -> HashMap<u32, Screenshot> {
  let mut queue = connection.new_event_queue::<Capturer>();
  let qh = queue.handle();
  let manager =
    match globals.bind::<ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ()) {
      Ok(manager) => manager,
      Err(error) => {
        eprintln!("shingle-lock: no screenshot, no wlr-screencopy: {error}");
        return HashMap::new();
      }
    };
  let Ok(shm) = globals.bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ()) else {
    return HashMap::new();
  };
  let mut capturer = Capturer {
    shm,
    pending: HashMap::new(),
  };
  let mut outputs = Vec::new();
  for global in globals.contents().clone_list() {
    if global.interface != "wl_output" {
      continue;
    }
    let output: wl_output::WlOutput =
      globals
        .registry()
        .bind(global.name, global.version.min(4), &qh, ());
    let frame = manager.capture_output(0, &output, &qh, global.name);
    capturer.pending.insert(
      global.name,
      Pending {
        frame,
        version: manager.version(),
        shape: None,
        memory: None,
        y_invert: false,
        done: None,
      },
    );
    outputs.push(output);
  }
  let deadline = Instant::now() + timeout;
  loop {
    if queue.dispatch_pending(&mut capturer).is_err() {
      break;
    }
    if capturer
      .pending
      .values()
      .all(|pending| pending.done.is_some())
    {
      break;
    }
    let now = Instant::now();
    if now >= deadline || queue.flush().is_err() {
      eprintln!("shingle-lock: not every output was captured in time");
      break;
    }
    let Some(guard) = queue.prepare_read() else {
      continue;
    };
    let left = deadline - now;
    let fd = guard.connection_fd();
    let mut fds = [rustix::event::PollFd::new(
      &fd,
      rustix::event::PollFlags::IN,
    )];
    let wait = rustix::event::Timespec {
      tv_sec: left.as_secs() as _,
      tv_nsec: left.subsec_nanos() as _,
    };
    let ready = rustix::event::poll(&mut fds, Some(&wait));
    if matches!(ready, Ok(count) if count > 0) {
      let _ = guard.read();
    }
  }
  let shots = capturer
    .pending
    .drain()
    .filter_map(|(name, pending)| {
      pending.frame.destroy();
      if let Some((_, buffer)) = &pending.memory {
        buffer.destroy();
      }
      Some((name, pending.done.flatten()?))
    })
    .collect();
  for output in outputs {
    if output.version() >= 3 {
      output.release();
    }
  }
  manager.destroy();
  let _ = connection.flush();
  shots
}

impl Capturer {
  /// Gives frame `name` a buffer of its shape and asks for the copy.
  fn copy(&mut self, name: u32, qh: &QueueHandle<Self>) {
    let shm = self.shm.clone();
    let Some(pending) = self.pending.get_mut(&name) else {
      return;
    };
    let Some(shape) = pending.shape.filter(|_| pending.memory.is_none()) else {
      return;
    };
    let size = shape.stride as usize * shape.height as usize;
    let memory = rustix::fs::memfd_create(
      "shingle-lock-capture",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .and_then(|fd| rustix::fs::ftruncate(&fd, size as u64).map(|()| fd));
    let Ok(fd) = memory else {
      pending.done = Some(None);
      return;
    };
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
      0,
      shape.width as i32,
      shape.height as i32,
      shape.stride as i32,
      shape.format,
      qh,
      (),
    );
    pool.destroy();
    pending.frame.copy(&buffer);
    pending.memory = Some((fd, buffer));
  }

  /// Reads frame `name`'s pixels once it is ready.
  fn ready(&mut self, name: u32) {
    let Some(pending) = self.pending.get_mut(&name) else {
      return;
    };
    let shot = pending.shape.zip(pending.memory.as_ref()).and_then(
      |(shape, (fd, _))| {
        let mut data = vec![0; shape.stride as usize * shape.height as usize];
        rustix::io::pread(fd, &mut data, 0).ok()?;
        convert(&data, shape, pending.y_invert)
      },
    );
    pending.done = Some(shot);
  }
}

impl Dispatch<ZwlrScreencopyFrameV1, u32> for Capturer {
  fn event(
    state: &mut Self,
    _: &ZwlrScreencopyFrameV1,
    event: zwlr_screencopy_frame_v1::Event,
    name: &u32,
    _: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_screencopy_frame_v1::Event::Buffer {
        format: WEnum::Value(format),
        width,
        height,
        stride,
      } => {
        if let Some(pending) = state.pending.get_mut(name) {
          pending.shape = Some(Shape {
            format,
            width,
            height,
            stride,
          });
          // Before version 3 no buffer_done follows.
          if pending.version < 3 {
            state.copy(*name, qh);
          }
        }
      }
      zwlr_screencopy_frame_v1::Event::BufferDone => state.copy(*name, qh),
      zwlr_screencopy_frame_v1::Event::Flags {
        flags: WEnum::Value(flags),
      } => {
        if let Some(pending) = state.pending.get_mut(name) {
          pending.y_invert =
            flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
        }
      }
      zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready(*name),
      zwlr_screencopy_frame_v1::Event::Failed => {
        if let Some(pending) = state.pending.get_mut(name) {
          pending.done = Some(None);
        }
      }
      _ => {}
    }
  }
}

macro_rules! ignore {
  ($($interface:ty),+ $(,)?) => {
    $(
      impl Dispatch<$interface, ()> for Capturer {
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
    )+
  };
}

ignore!(
  ZwlrScreencopyManagerV1,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_buffer::WlBuffer,
  wl_output::WlOutput,
);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn captures_become_top_down_bgra_rows() {
    let shape = Shape {
      format: wl_shm::Format::Xrgb8888,
      width: 1,
      height: 2,
      stride: 8,
    };
    let data = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9];
    let shot = convert(&data, shape, false).unwrap();
    assert_eq!(shot.bgra, [1, 2, 3, 255, 4, 5, 6, 255]);
    let flipped = convert(&data, shape, true).unwrap();
    assert_eq!(flipped.bgra, [4, 5, 6, 255, 1, 2, 3, 255]);
    let swapped = convert(
      &data,
      Shape {
        format: wl_shm::Format::Xbgr8888,
        ..shape
      },
      false,
    )
    .unwrap();
    assert_eq!(swapped.bgra, [3, 2, 1, 255, 6, 5, 4, 255]);
    let other = Shape {
      format: wl_shm::Format::Rgb565,
      ..shape
    };
    assert_eq!(convert(&data, other, false), None);
    assert_eq!(convert(&data[..8], shape, false), None);
  }
}
