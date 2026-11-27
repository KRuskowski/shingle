//! Times wgpu 0.20's Vulkan instance, adapter, device and a Gloomy renderer
//! for the launcher's panel, the measurement behind choosing lavapipe
//! (docs/design/launcher.md, section 4). Run it inside tools/gpu_test/
//! run.py's namespace, with `VK_DRIVER_FILES` to pick one driver.

use std::time::Instant;

fn main() {
  let start = Instant::now();
  let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
    backends: wgpu::Backends::VULKAN,
    ..Default::default()
  });
  let t_instance = start.elapsed();
  let adapter = pollster::block_on(
    instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
  )
  .unwrap();
  let t_adapter = start.elapsed();
  let (device, queue) = pollster::block_on(adapter.request_device(
    &wgpu::DeviceDescriptor {
      label: None,
      required_features: wgpu::Features::empty(),
      required_limits: wgpu::Limits::downlevel_defaults(),
    },
    None,
  ))
  .unwrap();
  let t_device = start.elapsed();
  let format = wgpu::TextureFormat::Bgra8UnormSrgb;
  let mut renderer =
    gloomy_core::GloomyRenderer::new(&device, format, 720, 480, 1.0);
  renderer.resize(&queue, 720, 480, 1.0);
  let t_renderer = start.elapsed();
  println!(
    "{:?} instance {:?} adapter {:?} device {:?} renderer {:?}",
    adapter.get_info().name,
    t_instance,
    t_adapter,
    t_device,
    t_renderer
  );
}
