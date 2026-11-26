//! Render fences: a sync_file that signals when a frame's submission has
//! finished on the GPU, for a KMS plane's `IN_FENCE_FD`
//! (docs/design/native-backend.md, NB-10).
//!
//! wgpu cannot export a fence, but wgpu-hal lets the next queue submission
//! signal a semaphore of our own (`Queue::add_signal_semaphore`). A
//! [`FenceExporter`] creates a binary semaphore exportable as a sync_file
//! (`VK_KHR_external_semaphore_fd` with `SYNC_FD`), arms the queue with it
//! before the frame is submitted ([`FenceExporter::arm`]) and exports it
//! afterwards ([`FenceExporter::export`]), as wlroots' Vulkan renderer
//! exports its render-done semaphore. Exporting a sync_file resets the
//! semaphore, but Vulkan still forbids destroying it while the submission
//! runs, so each one is kept until wgpu reports the submitted work done.

use std::{
  os::fd::{FromRawFd, OwnedFd},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use ash::vk;
use wgpu::hal::api::Vulkan;

use crate::GpuContext;

/// A semaphore the queue's next submission signals.
#[derive(Debug)]
#[must_use = "an armed fence is exported or disarmed"]
pub struct ArmedFence {
  semaphore: vk::Semaphore,
}

/// Exports sync_files of queue submissions; see the module documentation.
///
/// It holds the Vulkan device's function table, so it must be dropped
/// before the [`GpuContext`] it was made for.
pub struct FenceExporter {
  device: ash::Device,
  semaphore_fd: ash::khr::external_semaphore_fd::Device,
  /// The semaphores of submissions that may still run, with whether wgpu
  /// reported their work done.
  running: Vec<(vk::Semaphore, Arc<AtomicBool>)>,
}

const SYNC_FD: vk::ExternalSemaphoreHandleTypeFlags =
  vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD;

impl FenceExporter {
  /// An exporter for `gpu`, or why it cannot export sync_files: the
  /// device was not opened for DMA-BUFs, lacks
  /// `VK_KHR_external_semaphore_fd`, or cannot export binary semaphores
  /// as sync_files.
  pub fn new(gpu: &GpuContext) -> Result<Self, String> {
    let caps = gpu.dmabuf().map_err(|reason| reason.to_string())?;
    if !caps.external_semaphore_fd {
      return Err("the device lacks VK_KHR_external_semaphore_fd".into());
    }
    // SAFETY: the guard is only used within this function, and the
    // device handle cloned from it lives as long as the wgpu device,
    // which the caller keeps longer than the exporter.
    let hal = unsafe { gpu.device().as_hal::<Vulkan>() }
      .ok_or("the device is not a Vulkan device")?;
    let instance = hal.shared_instance().raw_instance();
    let info =
      vk::PhysicalDeviceExternalSemaphoreInfo::default().handle_type(SYNC_FD);
    let mut properties = vk::ExternalSemaphoreProperties::default();
    // SAFETY: a query on the device's own physical device.
    unsafe {
      instance.get_physical_device_external_semaphore_properties(
        hal.raw_physical_device(),
        &info,
        &mut properties,
      );
    }
    if !properties
      .external_semaphore_features
      .contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE)
    {
      return Err("the device cannot export semaphores as sync_files".into());
    }
    let device = hal.raw_device().clone();
    let semaphore_fd =
      ash::khr::external_semaphore_fd::Device::new(instance, &device);
    Ok(Self {
      device,
      semaphore_fd,
      running: Vec::new(),
    })
  }

  /// Makes the next submission on `gpu`'s queue signal a new semaphore,
  /// which [`FenceExporter::export`] turns into a sync_file once the
  /// frame was submitted. Semaphores of finished submissions are freed
  /// first.
  pub fn arm(&mut self, gpu: &GpuContext) -> Result<ArmedFence, String> {
    self.retire();
    let mut export =
      vk::ExportSemaphoreCreateInfo::default().handle_types(SYNC_FD);
    let info = vk::SemaphoreCreateInfo::default().push_next(&mut export);
    // SAFETY: a plain binary semaphore on the device.
    let semaphore = unsafe { self.device.create_semaphore(&info, None) }
      .map_err(|error| format!("creating a semaphore failed: {error}"))?;
    // SAFETY: the semaphore stays alive until the submission that signals
    // it has finished (`running`), as wgpu-hal requires.
    match unsafe { gpu.queue().as_hal::<Vulkan>() } {
      Some(queue) => queue.add_signal_semaphore(semaphore, None),
      None => {
        // SAFETY: never handed to the queue.
        unsafe { self.device.destroy_semaphore(semaphore, None) };
        return Err("the queue is not a Vulkan queue".into());
      }
    }
    Ok(ArmedFence { semaphore })
  }

  /// The sync_file of the submission `armed` was armed for, which must
  /// have been made on `gpu`'s queue since: it signals when that
  /// submission has finished. `None` means it has finished already.
  pub fn export(
    &mut self,
    gpu: &GpuContext,
    armed: ArmedFence,
  ) -> Result<Option<OwnedFd>, String> {
    self.keep_until_done(gpu, armed.semaphore);
    let info = vk::SemaphoreGetFdInfoKHR::default()
      .semaphore(armed.semaphore)
      .handle_type(SYNC_FD);
    // SAFETY: the semaphore was created exportable as a sync_file and
    // has a signal operation pending since the submission.
    let fd = unsafe { self.semaphore_fd.get_semaphore_fd(&info) }
      .map_err(|error| format!("exporting the render fence failed: {error}"))?;
    if fd < 0 {
      // Vulkan returns -1 for a semaphore that has signalled already.
      return Ok(None);
    }
    // SAFETY: vkGetSemaphoreFdKHR hands over a new descriptor.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
  }

  /// Gives up `armed` when no submission followed: it is taken off the
  /// queue, or kept until the submission that took it has finished.
  pub fn disarm(&mut self, gpu: &GpuContext, armed: ArmedFence) {
    // SAFETY: the guard is dropped at once.
    let removed = unsafe { gpu.queue().as_hal::<Vulkan>() }
      .is_some_and(|queue| queue.remove_signal_semaphore(armed.semaphore));
    if removed {
      // SAFETY: no submission refers to it.
      unsafe { self.device.destroy_semaphore(armed.semaphore, None) };
    } else {
      self.keep_until_done(gpu, armed.semaphore);
    }
  }

  /// How many semaphores wait for their submissions, for tests.
  pub fn running(&self) -> usize {
    self.running.len()
  }

  /// Keeps `semaphore` until the work submitted to `gpu`'s queue so far
  /// has finished.
  fn keep_until_done(&mut self, gpu: &GpuContext, semaphore: vk::Semaphore) {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    gpu
      .queue()
      .on_submitted_work_done(move || flag.store(true, Ordering::SeqCst));
    self.running.push((semaphore, done));
  }

  /// Frees the semaphores whose submissions have finished.
  fn retire(&mut self) {
    let device = &self.device;
    self.running.retain(|(semaphore, done)| {
      let finished = done.load(Ordering::SeqCst);
      if finished {
        // SAFETY: the submission that signalled it has finished.
        unsafe { device.destroy_semaphore(*semaphore, None) };
      }
      !finished
    });
  }
}

impl Drop for FenceExporter {
  fn drop(&mut self) {
    // SAFETY: waits for every submission, after which no semaphore is in
    // use. A lost device returns an error, and then nothing runs either.
    unsafe {
      let _ = self.device.device_wait_idle();
      for (semaphore, _) in self.running.drain(..) {
        self.device.destroy_semaphore(semaphore, None);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;
  use crate::{GpuOptions, OffscreenTarget, RenderError, RenderState};

  /// A GPU context opened for DMA-BUFs, or `None`, saying why, without
  /// one.
  fn gpu() -> Option<GpuContext> {
    let options = GpuOptions {
      dmabuf: true,
      ..GpuOptions::default()
    };
    match pollster::block_on(GpuContext::new(
      GpuContext::create_instance(),
      options,
    )) {
      Ok(gpu) => Some(gpu),
      Err(RenderError::RequestAdapter(error)) => {
        eprintln!("skipping, no GPU adapter: {error}");
        None
      }
      Err(error) => panic!("GPU context failed: {error}"),
    }
  }

  /// Whether `fd` becomes readable, which a sync_file does when its fence
  /// signals, within `timeout`.
  fn signals(fd: &OwnedFd, timeout: Duration) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let timeout = Timespec {
      tv_sec: timeout.as_secs() as i64,
      tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    poll(&mut fds, Some(&timeout)).is_ok_and(|ready| ready > 0)
  }

  #[test]
  fn a_frame_exports_a_fence_that_signals() {
    let Some(gpu) = gpu() else { return };
    let mut exporter = match FenceExporter::new(&gpu) {
      Ok(exporter) => exporter,
      Err(reason) => {
        eprintln!("skipping, {reason}");
        return;
      }
    };
    let target = OffscreenTarget::new(&gpu, 64, 64).expect("a target");
    let mut exported = 0;
    for _ in 0..3 {
      let armed = exporter.arm(&gpu).expect("the queue is armed");
      target.render(&gpu, &RenderState::default(), &[], [0.0, 0.0, 0.0, 1.0]);
      let fence = exporter.export(&gpu, armed).expect("the fence exports");
      if let Some(fd) = fence {
        assert!(signals(&fd, Duration::from_secs(2)), "the GPU finished");
        exported += 1;
      }
      gpu
        .device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the device is idle");
    }
    eprintln!(
      "{}: {exported} of 3 fences were pending when exported",
      gpu.adapter().get_info().name
    );
    // The next arm frees the semaphores of finished frames.
    let armed = exporter.arm(&gpu).expect("the queue is armed");
    assert_eq!(exporter.running(), 0);
    exporter.disarm(&gpu, armed);
    assert_eq!(exporter.running(), 0, "taken off the queue unused");
  }
}
