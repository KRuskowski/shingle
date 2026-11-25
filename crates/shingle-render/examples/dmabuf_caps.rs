//! Prints what every Vulkan adapter can import as DMA-BUFs: the render
//! node and, per DRM format, the modifiers that can be sampled and those
//! that can be rendered to, with their largest size. vulkaninfo does not
//! show this, because it needs the import query for each modifier.
//!
//! `cargo run -p shingle-render --example dmabuf_caps`. It opens no device
//! and no DRM node; set `VK_ICD_FILENAMES` to look at one driver only.

use shingle_render::GpuContext;
use shingle_render::dmabuf::{self, DmabufCaps, ModifierCaps};

fn main() {
  let instance = GpuContext::create_instance();
  let adapters =
    pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
  if adapters.is_empty() {
    println!("no Vulkan adapter");
  }
  for adapter in adapters {
    let info = adapter.get_info();
    println!(
      "{} ({:?}, {} {})",
      info.name, info.device_type, info.driver, info.driver_info
    );
    match DmabufCaps::for_adapter(&adapter) {
      Ok(caps) => print_caps(&caps),
      Err(reason) => println!("  no DMA-BUF import: {reason}"),
    }
  }
}

fn print_caps(caps: &DmabufCaps) {
  match caps.render_node {
    Some((major, minor)) => println!("  render node {major}:{minor}"),
    None => println!("  no render node (VK_EXT_physical_device_drm)"),
  }
  println!(
    "  VK_KHR_external_semaphore_fd: {}",
    caps.external_semaphore_fd
  );
  for format in &caps.formats {
    let name =
      dmabuf::drm_format(format.fourcc).map_or("?", |format| format.name);
    println!("  {name}");
    print_list("sample", &format.sample);
    print_list("render", &format.render);
  }
}

fn print_list(label: &str, list: &[ModifierCaps]) {
  if list.is_empty() {
    println!("    {label}: none");
  }
  for caps in list {
    let decode = if caps.srgb {
      ""
    } else {
      ", UNORM, shader decodes"
    };
    println!(
      "    {label}: {} up to {}x{}{decode}",
      dmabuf::modifier_name(caps.modifier),
      caps.max_width,
      caps.max_height
    );
  }
}
