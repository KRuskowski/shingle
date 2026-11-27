//! Frames the compositor shows without a copy: each output draws into
//! three GBM buffers on the compositor's render node, imported into the
//! lock's GPU device as render targets and handed to the compositor as
//! `zwp_linux_dmabuf_v1` buffers. Nothing is read back, so a background
//! that moves at the output's refresh rate costs the GPU's drawing and
//! little CPU (docs/design/lock-screen.md).
//!
//! The compositor's default feedback names its main device and the
//! (format, modifier) pairs it imports ([`query`]); the buffers use
//! `XRGB8888` with the modifiers it imports that the lock's device can
//! render to ([`modifiers`]). Whatever fails is reported once, and the
//! lock draws through `wl_shm` instead.

use std::{
  collections::BTreeSet,
  fs::File,
  os::{
    fd::{AsFd, OwnedFd},
    unix::fs::MetadataExt,
  },
  path::PathBuf,
  time::Duration,
};

use shingle_render::dmabuf::{DmabufDescriptor, DmabufUse, ImportedTexture};
use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, globals::GlobalList,
  protocol::wl_buffer::WlBuffer,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
  zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
  zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
  zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::gpu::Gpu;

/// DRM's `XRGB8888`.
pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// How many buffers each output alternates.
pub const BUFFERS: usize = 3;

/// What the compositor's default linux-dmabuf feedback offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
  /// Its main device, a `dev_t`.
  pub main_device: Option<u64>,
  /// Every (fourcc, modifier) pair of its tranches.
  pub pairs: Vec<(u32, u64)>,
}

/// The format table's entries: a fourcc, 4 bytes of padding and a
/// modifier, 16 bytes each in the host's byte order.
pub fn parse_table(bytes: &[u8]) -> Vec<(u32, u64)> {
  bytes
    .chunks_exact(16)
    .map(|entry| {
      let fourcc = u32::from_ne_bytes(entry[..4].try_into().expect("4"));
      let modifier = u64::from_ne_bytes(entry[8..].try_into().expect("8"));
      (fourcc, modifier)
    })
    .collect()
}

/// A `dev_t` array's device.
pub fn parse_device(bytes: &[u8]) -> Option<u64> {
  Some(u64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?))
}

/// The modifiers to allocate `fourcc` with: the ones `feedback` offers
/// for it that `renders` says the lock's device can render to, in the
/// feedback's order.
pub fn modifiers(
  feedback: &Feedback,
  fourcc: u32,
  renders: &dyn Fn(u64) -> bool,
) -> Vec<u64> {
  let mut found = Vec::new();
  for (format, modifier) in &feedback.pairs {
    if *format == fourcc && renders(*modifier) && !found.contains(modifier) {
      found.push(*modifier);
    }
  }
  found
}

/// Collects the default feedback on a queue of its own.
#[derive(Default)]
struct Collector {
  table: Vec<(u32, u64)>,
  main_device: Option<u64>,
  indices: BTreeSet<u16>,
  done: bool,
}

/// Asks the compositor for its default feedback, waiting at most
/// `timeout`.
///
/// # Errors
///
/// Without `zwp_linux_dmabuf_v1` version 4, or when the feedback does
/// not come.
pub fn query(
  connection: &Connection,
  globals: &GlobalList,
  timeout: Duration,
) -> Result<Feedback, String> {
  let mut queue = connection.new_event_queue::<Collector>();
  let qh = queue.handle();
  let dmabuf = globals
    .bind::<ZwpLinuxDmabufV1, _, _>(&qh, 4..=4, ())
    .map_err(|error| format!("no zwp_linux_dmabuf_v1 version 4: {error}"))?;
  let feedback = dmabuf.get_default_feedback(&qh, ());
  let mut collector = Collector::default();
  let deadline = std::time::Instant::now() + timeout;
  while !collector.done && std::time::Instant::now() < deadline {
    queue
      .roundtrip(&mut collector)
      .map_err(|error| format!("the feedback: {error}"))?;
  }
  feedback.destroy();
  dmabuf.destroy();
  if !collector.done {
    return Err("the compositor sent no linux-dmabuf feedback".into());
  }
  Ok(Feedback {
    main_device: collector.main_device,
    pairs: collector
      .indices
      .iter()
      .filter_map(|index| collector.table.get(usize::from(*index)).copied())
      .collect(),
  })
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Collector {
  fn event(
    _state: &mut Self,
    _proxy: &ZwpLinuxDmabufV1,
    _event: <ZwpLinuxDmabufV1 as Proxy>::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for Collector {
  fn event(
    state: &mut Self,
    _proxy: &ZwpLinuxDmabufFeedbackV1,
    event: zwp_linux_dmabuf_feedback_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    match event {
      zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, size } => {
        state.table = read_table(fd, size as usize).unwrap_or_default();
      }
      zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
        state.main_device = parse_device(&device);
      }
      zwp_linux_dmabuf_feedback_v1::Event::TrancheFormats { indices } => {
        state.indices.extend(
          indices
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]])),
        );
      }
      zwp_linux_dmabuf_feedback_v1::Event::Done => state.done = true,
      _ => {}
    }
  }
}

/// The format table in `fd`, `size` bytes.
fn read_table(fd: OwnedFd, size: usize) -> Option<Vec<(u32, u64)>> {
  if size == 0 {
    return Some(Vec::new());
  }
  // SAFETY: a private read-only mapping of the table the compositor
  // sent, unmapped before returning; nothing else maps it here.
  unsafe {
    let mapping = rustix::mm::mmap(
      std::ptr::null_mut(),
      size,
      rustix::mm::ProtFlags::READ,
      rustix::mm::MapFlags::PRIVATE,
      fd.as_fd(),
      0,
    )
    .ok()?;
    let bytes = std::slice::from_raw_parts(mapping.cast::<u8>(), size);
    let table = parse_table(bytes);
    let _ = rustix::mm::munmap(mapping, size);
    Some(table)
  }
}

/// The render node whose device number is `device`.
fn render_node(device: u64) -> Option<PathBuf> {
  std::fs::read_dir("/dev/dri")
    .ok()?
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .filter(|path| {
      path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("renderD"))
    })
    .find(|path| {
      std::fs::metadata(path).is_ok_and(|metadata| metadata.rdev() == device)
    })
}

/// Allocates the lock's DMA-BUFs.
pub struct Allocator {
  device: gbm::Device<File>,
  modifiers: Vec<u64>,
}

impl Allocator {
  /// An allocator for `gpu` alone, with every modifier it renders
  /// `XRGB8888` to: what the tests and the frame-cost measurement use
  /// without a compositor.
  ///
  /// # Errors
  ///
  /// When the GPU imports no DMA-BUFs or has no render node.
  #[cfg(test)]
  pub fn for_gpu(gpu: &Gpu) -> Result<Self, String> {
    let caps = gpu
      .context()
      .dmabuf()
      .map_err(|reason| format!("the GPU imports no DMA-BUFs: {reason}"))?;
    let pairs = caps
      .format(XRGB8888)
      .map(|format| {
        format
          .render
          .iter()
          .map(|modifier| (XRGB8888, modifier.modifier))
          .collect()
      })
      .unwrap_or_default();
    Self::new(
      gpu,
      &Feedback {
        main_device: caps.render_node_dev(),
        pairs,
      },
    )
  }

  /// An allocator on the compositor's main device, if the lock's GPU is
  /// that device and can render to a modifier the compositor imports.
  ///
  /// # Errors
  ///
  /// The reason the lock draws through `wl_shm` instead.
  pub fn new(gpu: &Gpu, feedback: &Feedback) -> Result<Self, String> {
    let caps = gpu
      .context()
      .dmabuf()
      .map_err(|reason| format!("the GPU imports no DMA-BUFs: {reason}"))?;
    let main = feedback
      .main_device
      .ok_or("the feedback names no main device")?;
    if caps.render_node_dev() != Some(main) {
      return Err(
        "the lock's GPU is not the compositor's main device".to_owned(),
      );
    }
    let modifiers = modifiers(feedback, XRGB8888, &|modifier| {
      caps.rendering(XRGB8888, modifier).is_some()
    });
    if modifiers.is_empty() {
      return Err(
        "no XRGB8888 modifier both imports in the compositor and renders \
         here"
          .to_owned(),
      );
    }
    let path = render_node(main).ok_or("no render node for the device")?;
    let file = File::options()
      .read(true)
      .write(true)
      .open(&path)
      .map_err(|error| format!("{}: {error}", path.display()))?;
    let device =
      gbm::Device::new(file).map_err(|error| format!("GBM: {error}"))?;
    Ok(Self { device, modifiers })
  }

  /// One buffer of `size`, imported into `gpu` to render to.
  ///
  /// # Errors
  ///
  /// When GBM, the import or the buffer's single plane fails.
  pub fn allocate(
    &self,
    gpu: &Gpu,
    size: (u32, u32),
  ) -> Result<Allocation, String> {
    let bo = self
      .device
      .create_buffer_object_with_modifiers2::<()>(
        size.0,
        size.1,
        gbm::Format::Xrgb8888,
        self
          .modifiers
          .iter()
          .map(|modifier| gbm::Modifier::from(*modifier)),
        gbm::BufferObjectFlags::RENDERING,
      )
      .map_err(|error| format!("GBM allocation: {error}"))?;
    if bo.plane_count() != 1 {
      return Err(format!("GBM gave {} planes", bo.plane_count()));
    }
    let fd = bo
      .fd()
      .map_err(|_| "GBM gave no DMA-BUF for the buffer".to_owned())?;
    let modifier = u64::from(bo.modifier());
    let (stride, offset) = (bo.stride_for_plane(0), bo.offset(0));
    let texture = gpu
      .context()
      .import_dmabuf(
        &DmabufDescriptor {
          width: size.0,
          height: size.1,
          fourcc: XRGB8888,
          modifier,
          fd: fd.as_fd(),
          offset,
          stride,
        },
        DmabufUse::Render,
      )
      .map_err(|error| error.to_string())?;
    Ok(Allocation {
      bo,
      fd,
      texture,
      modifier,
      stride,
      offset,
    })
  }

  /// One buffer of `size`, imported into `gpu` to render to and made a
  /// `wl_buffer` of `dmabuf` with `data`.
  ///
  /// # Errors
  ///
  /// When GBM, the import or the buffer's single plane fails.
  pub fn buffer<State, Data>(
    &self,
    gpu: &Gpu,
    dmabuf: &ZwpLinuxDmabufV1,
    qh: &QueueHandle<State>,
    size: (u32, u32),
    data: Data,
  ) -> Result<Slot, String>
  where
    State:
      Dispatch<WlBuffer, Data> + Dispatch<ZwpLinuxBufferParamsV1, ()> + 'static,
    Data: Send + Sync + 'static,
  {
    let Allocation {
      bo,
      fd,
      texture,
      modifier,
      stride,
      offset,
    } = self.allocate(gpu, size)?;
    let params = dmabuf.create_params(qh, ());
    params.add(
      fd.as_fd(),
      0,
      offset,
      stride,
      (modifier >> 32) as u32,
      modifier as u32,
    );
    let buffer = params.create_immed(
      size.0 as i32,
      size.1 as i32,
      XRGB8888,
      zwp_linux_buffer_params_v1::Flags::empty(),
      qh,
      data,
    );
    params.destroy();
    Ok(Slot {
      buffer,
      texture,
      busy: false,
      holds: None,
      _bo: bo,
      _fd: fd,
    })
  }
}

/// A GBM buffer imported to render to.
pub struct Allocation {
  /// The buffer object, which owns the memory.
  pub bo: gbm::BufferObject<()>,
  /// Its DMA-BUF.
  pub fd: OwnedFd,
  /// The same memory in the GPU device.
  pub texture: ImportedTexture,
  /// Its modifier.
  pub modifier: u64,
  /// Bytes per row.
  pub stride: u32,
  /// Where the plane starts.
  pub offset: u32,
}

/// One DMA-BUF of an output.
pub struct Slot {
  /// What the compositor shows.
  pub buffer: WlBuffer,
  /// The same memory, to render to.
  pub texture: ImportedTexture,
  /// The compositor holds it.
  pub busy: bool,
  /// The frame it holds, by its target's frame number.
  pub holds: Option<u64>,
  _bo: gbm::BufferObject<()>,
  _fd: OwnedFd,
}

impl Drop for Slot {
  fn drop(&mut self) {
    self.buffer.destroy();
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;

  #[test]
  fn the_format_table_and_device_are_read() {
    let mut table = Vec::new();
    for (fourcc, modifier) in [(XRGB8888, 0_u64), (0x1234, 1 << 56 | 7)] {
      table.extend_from_slice(&fourcc.to_ne_bytes());
      table.extend_from_slice(&[0; 4]);
      table.extend_from_slice(&modifier.to_ne_bytes());
    }
    assert_eq!(
      parse_table(&table),
      vec![(XRGB8888, 0), (0x1234, 1 << 56 | 7)]
    );
    assert_eq!(parse_table(&table[..20]).len(), 1);
    assert_eq!(
      parse_device(&(226_u64 << 8 | 128).to_ne_bytes()),
      Some(57_984)
    );
    assert_eq!(parse_device(&[1, 2]), None);
  }

  /// A frame of the lock drawn into a DMA-BUF of `size`, as the frames
  /// the compositor shows are, with a white square at (8, 8).
  pub(crate) fn dmabuf_frame(
    gpu: &Gpu,
    allocator: &Allocator,
    size: (u32, u32),
  ) -> (Allocation, crate::gpu::Target, crate::gpu::Frame) {
    use crate::gpu::{Drift, Frame, Placement, Target};
    let allocation = allocator.allocate(gpu, size).expect("a buffer");
    let mut target = Target::new(gpu, size, None, crate::gpu::FORMAT);
    target.set_input(
      gpu,
      Some(&crate::ui::Raster {
        width: 4,
        height: 4,
        rgba: vec![255; 64],
      }),
    );
    let frame = Frame {
      drift: Drift::STILL,
      vignette: None,
      color: [0x20, 0x40, 0x80, 0xff],
      dim: 1.0,
      seed: 0,
      clock: None,
      input: Some(Placement::at((8, 8), 1.0)),
    };
    (allocation, target, frame)
  }

  /// The time this thread has run on a CPU.
  fn thread_cpu() -> Duration {
    let mut time = libc::timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
  }

  /// The cost of a moving frame drawn into a DMA-BUF at 1080p and 4K:
  /// the wall time until the GPU finished it and the CPU time of the
  /// thread that drew it. `cargo test --release -p shingle-lock --
  /// --ignored dmabuf_frame_cost --nocapture`.
  #[test]
  #[ignore = "a measurement, run by hand"]
  fn dmabuf_frame_cost() {
    use crate::gpu::Drift;
    let Ok(gpu) = Gpu::new() else {
      eprintln!("no GPU; skipped");
      return;
    };
    let Ok(allocator) = Allocator::for_gpu(&gpu) else {
      eprintln!("no DMA-BUFs; skipped");
      return;
    };
    for size in [(1920_u32, 1080_u32), (3840, 2160)] {
      let shot = crate::capture::Screenshot {
        name: None,
        width: size.0,
        height: size.1,
        bgra: (0..size.0 * size.1 * 4).map(|i| (i % 251) as u8).collect(),
      };
      let image = gpu.image(&shot, Some((24, 3)));
      let (allocation, _, frame) = dmabuf_frame(&gpu, &allocator, size);
      let mut target =
        crate::gpu::Target::new(&gpu, size, Some(&image), crate::gpu::FORMAT);
      let config = shingle_config::DriftConfig::default();
      let (mut walls, mut cpus) = (Vec::new(), Vec::new());
      for index in 0..130_u32 {
        let moving = crate::gpu::Frame {
          drift: Drift::at(f64::from(index) / 60.0, &config, size),
          seed: index,
          dim: 0.78,
          vignette: Some((0.5, 0.5)),
          ..frame
        };
        let (wall, cpu) = (std::time::Instant::now(), thread_cpu());
        target
          .draw_to(&gpu, &moving, &allocation.texture)
          .expect("drawn");
        walls.push(wall.elapsed().as_secs_f64() * 1000.0);
        cpus.push((thread_cpu() - cpu).as_secs_f64() * 1000.0);
      }
      for (what, times) in [("wall", &mut walls), ("CPU", &mut cpus)] {
        times.drain(..10);
        times.sort_by(f64::total_cmp);
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        eprintln!(
          "DMA-BUF frame {}x{} {what}: mean {mean:.2} ms, median {:.2} ms, \
           p95 {:.2} ms, max {:.2} ms",
          size.0,
          size.1,
          times[times.len() / 2],
          times[times.len() * 95 / 100],
          times[times.len() - 1],
        );
      }
    }
  }

  #[test]
  fn modifiers_are_offered_and_renderable_in_the_feedbacks_order() {
    let feedback = Feedback {
      main_device: Some(1),
      pairs: vec![
        (XRGB8888, 5),
        (0x1234, 1),
        (XRGB8888, 0),
        (XRGB8888, 9),
        (XRGB8888, 5),
      ],
    };
    let renders = |modifier| modifier != 9;
    assert_eq!(modifiers(&feedback, XRGB8888, &renders), vec![5, 0]);
    assert!(modifiers(&feedback, 0x4242, &renders).is_empty());
    assert_eq!(XRGB8888, 0x3432_5258);
  }
}
