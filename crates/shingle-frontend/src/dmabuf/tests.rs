//! Unit tests of the linux-dmabuf decision and the texture cache, with a
//! fake importer that hands out one uploaded texture.

use std::{
  cell::{Cell, RefCell},
  ffi::OsStr,
  rc::Rc,
  sync::Arc,
};

use shingle::{DmabufBuffer, DmabufFeedback, DmabufFormat, DmabufPlane};
use shingle_render::{
  DmabufCaps, DmabufUnavailable, GpuTexture, TextureFilter,
  dmabuf::{FormatCaps, ModifierCaps},
};

use super::{DmabufTextures, Import, capture_formats, feedback};

const XR24: u32 = u32::from_le_bytes(*b"XR24");
const AR24: u32 = u32::from_le_bytes(*b"AR24");

/// A completed-work callback.
type WorkDone = Box<dyn FnOnce() + Send>;

/// Hands out clones of one texture, counts imports, and keeps the
/// completed-work callbacks for the test to run.
#[derive(Clone)]
pub(crate) struct FakeImport {
  texture: GpuTexture,
  pub(crate) imports: Rc<Cell<usize>>,
  pub(crate) filters: Rc<RefCell<Vec<TextureFilter>>>,
  pub(crate) fail: Rc<Cell<bool>>,
  callbacks: Rc<RefCell<Vec<WorkDone>>>,
}

impl FakeImport {
  pub(crate) fn new(texture: GpuTexture) -> Self {
    Self {
      texture,
      imports: Rc::default(),
      filters: Rc::default(),
      fail: Rc::default(),
      callbacks: Rc::default(),
    }
  }

  /// Lets the GPU "finish" everything submitted so far.
  pub(crate) fn finish_work(&self) {
    for callback in self.callbacks.borrow_mut().drain(..) {
      callback();
    }
  }
}

impl Import for FakeImport {
  fn import(
    &self,
    _buffer: &DmabufBuffer,
    filter: TextureFilter,
  ) -> Result<GpuTexture, String> {
    if self.fail.get() {
      return Err("the fake importer refuses".into());
    }
    self.imports.set(self.imports.get() + 1);
    self.filters.borrow_mut().push(filter);
    Ok(self.texture.clone())
  }

  fn on_submitted_work_done(&self, done: Box<dyn FnOnce() + Send>) {
    self.callbacks.borrow_mut().push(done);
  }

  fn poll(&self) {}
}

/// A fake importer with a 1x1 texture on the test binary's one GPU
/// context (crates/shingle-frontend/src/test_gpu.rs), or `None` without a
/// GPU adapter.
pub(crate) fn fake_import() -> Option<FakeImport> {
  crate::test_gpu::white_texture().map(FakeImport::new)
}

/// The feedback the fake importer's tests advertise.
pub(crate) fn test_feedback() -> DmabufFeedback {
  DmabufFeedback {
    main_device: (226 << 8) | 128,
    formats: vec![DmabufFormat {
      format: XR24,
      modifier: 0,
      max_width: 4096,
      max_height: 4096,
    }],
  }
}

fn buffer(serial: u64) -> Arc<DmabufBuffer> {
  let fd = rustix::fs::memfd_create(
    "shingle-frontend-test-dmabuf",
    rustix::fs::MemfdFlags::CLOEXEC,
  )
  .expect("memfd_create succeeds");
  Arc::new(DmabufBuffer {
    serial,
    width: 4,
    height: 4,
    format: XR24,
    modifier: 0,
    planes: vec![DmabufPlane {
      fd,
      offset: 0,
      stride: 16,
    }],
  })
}

fn caps(render_node: Option<(u32, u32)>) -> DmabufCaps {
  let modifier = |modifier| ModifierCaps {
    modifier,
    max_width: 8192,
    max_height: 4096,
    srgb: true,
  };
  DmabufCaps {
    render_node,
    formats: vec![
      FormatCaps {
        fourcc: AR24,
        sample: vec![modifier(0), modifier(0x0300_0000_0060_6015)],
        render: Vec::new(),
      },
      FormatCaps {
        fourcc: XR24,
        sample: vec![modifier(0)],
        render: Vec::new(),
      },
    ],
    external_semaphore_fd: true,
  }
}

#[test]
fn the_feedback_lists_every_sampling_pair_on_the_render_node() {
  let caps = caps(Some((226, 128)));
  let offered = feedback(None, Ok(&caps)).expect("offered");
  assert_eq!(offered.main_device, (226 << 8) | 128);
  let pairs = offered
    .formats
    .iter()
    .map(|format| (format.format, format.modifier, format.max_width))
    .collect::<Vec<_>>();
  assert_eq!(
    pairs,
    [
      (AR24, 0, 8192),
      (AR24, 0x0300_0000_0060_6015, 8192),
      (XR24, 0, 8192)
    ]
  );
  assert!(feedback(Some(OsStr::new("1")), Ok(&caps)).is_ok());
}

#[test]
fn syncobj_is_tried_unless_forced_off() {
  assert!(super::syncobj_allowed(None).is_ok());
  assert!(super::syncobj_allowed(Some(OsStr::new("1"))).is_ok());
  let forced = super::syncobj_allowed(Some(OsStr::new("0")));
  assert!(forced.is_err_and(|reason| reason.contains("SHINGLE_SYNCOBJ=0")));
}

#[test]
fn no_feedback_when_forced_off_unable_or_without_a_render_node() {
  let caps_without_node = caps(None);
  let forced = feedback(Some(OsStr::new("0")), Ok(&caps(Some((226, 128)))));
  assert!(forced.is_err_and(|reason| reason.contains("SHINGLE_DMABUF=0")));
  let missing = feedback(None, Err(&DmabufUnavailable::NotVulkan));
  assert!(missing.is_err_and(|reason| reason.contains("not a Vulkan")));
  let lavapipe = feedback(None, Ok(&caps_without_node));
  assert!(lavapipe.is_err_and(|reason| reason.contains("render node")));
  let mut empty = caps(Some((226, 128)));
  for format in &mut empty.formats {
    format.sample.clear();
  }
  assert!(feedback(None, Ok(&empty)).is_err());
}

#[test]
fn a_buffer_is_imported_once_and_then_taken_from_the_cache() {
  let Some(fake) = fake_import() else {
    return;
  };
  let mut cache = DmabufTextures::new(Box::new(fake.clone()));
  let first = buffer(1);
  cache.import(&first).expect("imported");
  assert!(cache.texture(&first).is_some());
  assert!(cache.texture(&first).is_some());
  assert_eq!(fake.imports.get(), 1);
  let second = buffer(2);
  assert!(cache.texture(&second).is_some(), "imported on first use");
  assert_eq!(fake.imports.get(), 2);
  assert_eq!(cache.cached(), 2);
  drop(first);
  cache.prune();
  assert_eq!(cache.cached(), 1, "a buffer that is gone is dropped");
}

#[test]
fn a_filter_change_imports_again_with_the_new_filter() {
  let Some(fake) = fake_import() else {
    return;
  };
  let mut cache = DmabufTextures::new(Box::new(fake.clone()));
  let shown = buffer(1);
  cache.texture(&shown);
  cache.set_filter(TextureFilter::Linear);
  cache.texture(&shown);
  cache.set_filter(TextureFilter::Nearest);
  cache.texture(&shown);
  assert_eq!(
    *fake.filters.borrow(),
    [TextureFilter::Linear, TextureFilter::Nearest]
  );
}

#[test]
fn a_failed_import_is_tried_once_per_buffer() {
  let Some(fake) = fake_import() else {
    return;
  };
  let mut cache = DmabufTextures::new(Box::new(fake.clone()));
  fake.fail.set(true);
  let broken = buffer(1);
  assert!(cache.import(&broken).is_err());
  assert!(cache.texture(&broken).is_none());
  fake.fail.set(false);
  assert!(cache.texture(&broken).is_none(), "not retried every frame");
  cache.clear();
  assert!(cache.texture(&broken).is_some(), "retried after a GPU loss");
}

#[test]
fn frames_hold_nothing_without_locks_and_retire_once_done() {
  let Some(fake) = fake_import() else {
    return;
  };
  let mut cache = DmabufTextures::new(Box::new(fake.clone()));
  cache.frame_submitted(Vec::new());
  assert_eq!(cache.frames_in_flight(), 0);
  fake.finish_work();
  cache.retire();
  assert_eq!(cache.frames_in_flight(), 0);
}

#[test]
fn captures_go_into_the_formats_and_modifiers_the_device_renders_to() {
  let mut caps = caps(Some((226, 128)));
  assert_eq!(capture_formats(&caps), None, "nothing renders yet");
  caps.formats[0].render = caps.formats[0].sample.clone();
  caps.formats[1].render = caps.formats[1].sample.clone();
  let capture = capture_formats(&caps).expect("both formats render");
  assert_eq!(capture.device, (226 << 8) | 128);
  // XR24 first, as wlroots announces it.
  assert_eq!(
    capture.formats,
    [(XR24, vec![0]), (AR24, vec![0, 0x0300_0000_0060_6015])]
  );
  caps.render_node = None;
  assert_eq!(capture_formats(&caps), None, "no device to name");
}
