//! The one GPU context every test of this crate draws with.
//!
//! Tests that opened a device of their own crashed the test binary now
//! and then (SIGSEGV): with Vulkan's validation layer loaded, as wgpu
//! loads it in debug builds where it is installed, the loader's
//! `vkSetDebugUtilsObjectNameEXT` trampoline crashed inside the layer
//! while another test created or destroyed its instance and device in
//! parallel. One context, created once and never dropped, and taken by
//! one test at a time, avoids both: no instance is created or destroyed
//! while another draws, and the context's gamma and inversion state, which
//! a frame sets on the device, cannot leak between tests running at once.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use shingle_render::{
  GpuContext, GpuOptions, GpuTexture, RenderError, TextureFilter,
};

/// The shared context and a white 1x1 texture on it.
struct SharedGpu {
  gpu: Mutex<GpuContext>,
  white: GpuTexture,
}

/// The shared context, created on first use, or `None` without a GPU
/// adapter.
fn shared() -> Option<&'static SharedGpu> {
  static GPU: OnceLock<Option<SharedGpu>> = OnceLock::new();
  GPU
    .get_or_init(|| {
      let instance = GpuContext::create_instance();
      let gpu = match pollster::block_on(GpuContext::new(
        instance,
        GpuOptions::default(),
      )) {
        Ok(gpu) => gpu,
        Err(RenderError::RequestAdapter(error)) => {
          eprintln!("skipping GPU test, no GPU adapter: {error}");
          return None;
        }
        Err(error) => panic!("the test GPU context failed: {error}"),
      };
      let white = gpu
        .create_texture(1, 1, &[255; 4], TextureFilter::Linear)
        .expect("a 1x1 texture uploads");
      Some(SharedGpu {
        gpu: Mutex::new(gpu),
        white,
      })
    })
    .as_ref()
}

/// Takes the shared context for one test, waiting while another test
/// holds it, with inversion off and the identity gamma ramp. `None`
/// without a GPU adapter. A test that panicked while holding it leaves it
/// usable.
pub(crate) fn lock() -> Option<MutexGuard<'static, GpuContext>> {
  let mut gpu = shared()?.gpu.lock().unwrap_or_else(PoisonError::into_inner);
  gpu.set_inversion(false, false);
  gpu
    .set_gamma_ramp(&identity_ramp())
    .expect("the identity ramp has 768 values");
  Some(gpu)
}

/// A white 1x1 texture on the shared context, without taking it, or
/// `None` without a GPU adapter.
pub(crate) fn white_texture() -> Option<GpuTexture> {
  shared().map(|shared| shared.white.clone())
}

/// The identity gamma ramp: 256 values each for red, green and blue.
fn identity_ramp() -> Vec<u16> {
  (0..3)
    .flat_map(|_| (0..256u16).map(|value| value * 257))
    .collect()
}

mod tests {
  use super::*;

  #[test]
  fn the_identity_ramp_spans_every_channel() {
    let ramp = identity_ramp();
    assert_eq!(ramp.len(), 768);
    assert_eq!((ramp[0], ramp[255], ramp[256]), (0, u16::MAX, 0));
  }
}
