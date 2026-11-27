//! Drawing the panel with Gloomy into an offscreen texture and reading it
//! back, and choosing the Vulkan driver it runs on (docs/design/launcher.md,
//! section 4).

use std::{
  collections::HashMap,
  ffi::OsString,
  fs,
  path::{Path, PathBuf},
};

use gloomy_core::{
  GloomyRenderer, InteractionState, Vec2, compute_layout,
  image_renderer::ImageRenderer,
  render_ui,
  ui::{RenderContext, render_widget},
};

use crate::{
  theme::Color,
  view::{FIELD_ID, Frame},
};

/// The format drawn in. Its bytes are `wl_shm`'s little-endian
/// `ARGB8888`, and it is not an sRGB format, so a configured colour is the
/// bytes it shows as, as with every colour of Shingle's config.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// The Vulkan loader's variables the software choice sets.
const LOADER_VARIABLES: [&str; 3] = [
  "VK_DRIVER_FILES",
  "VK_ICD_FILENAMES",
  "VK_LOADER_LAYERS_DISABLE",
];

/// Which Vulkan driver draws the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RendererChoice {
  /// lavapipe when it is installed, else the loader's default.
  #[default]
  Auto,
  /// lavapipe, or fail.
  Software,
  /// The loader's default, normally the hardware GPU.
  Hardware,
}

impl From<shingle_config::LauncherRenderer> for RendererChoice {
  fn from(renderer: shingle_config::LauncherRenderer) -> Self {
    match renderer {
      shingle_config::LauncherRenderer::Auto => Self::Auto,
      shingle_config::LauncherRenderer::Software => Self::Software,
      shingle_config::LauncherRenderer::Hardware => Self::Hardware,
    }
  }
}

/// The directories the Vulkan loader reads ICD manifests from, for an
/// environment looked up through `var`.
pub fn icd_directories(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
  let mut directories = vec![PathBuf::from("/etc/vulkan/icd.d")];
  let data_dirs = var("XDG_DATA_DIRS")
    .filter(|value| !value.is_empty())
    .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
    .unwrap_or_default();
  // The loader's defaults always count: a session may set XDG_DATA_DIRS
  // without the system directories.
  for directory in data_dirs.into_iter().chain([
    PathBuf::from("/usr/local/share"),
    PathBuf::from("/usr/share"),
  ]) {
    let icd = directory.join("vulkan/icd.d");
    if !directories.contains(&icd) {
      directories.push(icd);
    }
  }
  directories
}

/// The first lavapipe manifest (`lvp_icd*.json`) in `directories`.
pub fn lavapipe_manifest(directories: &[PathBuf]) -> Option<PathBuf> {
  directories.iter().find_map(|directory| {
    let mut found = fs::read_dir(directory)
      .ok()?
      .flatten()
      .map(|item| item.path())
      .filter(|path| {
        path
          .file_name()
          .and_then(|name| name.to_str())
          .is_some_and(|name| {
            name.starts_with("lvp_icd") && name.ends_with(".json")
          })
      })
      .collect::<Vec<_>>();
    found.sort();
    found.into_iter().next()
  })
}

/// The loader variables to set for `choice`, or `None` to leave the
/// loader alone. Fails for [`RendererChoice::Software`] without lavapipe.
pub fn loader_overrides(
  choice: RendererChoice,
  lavapipe: Option<&Path>,
) -> Result<Option<Vec<(&'static str, OsString)>>, String> {
  let manifest = match (choice, lavapipe) {
    (RendererChoice::Hardware, _) | (RendererChoice::Auto, None) => {
      return Ok(None);
    }
    (RendererChoice::Software, None) => {
      return Err("renderer: software needs lavapipe (lvp_icd*.json)".into());
    }
    (_, Some(manifest)) => manifest.as_os_str().to_owned(),
  };
  Ok(Some(vec![
    (LOADER_VARIABLES[0], manifest.clone()),
    (LOADER_VARIABLES[1], manifest),
    (LOADER_VARIABLES[2], "~implicit~".into()),
  ]))
}

/// Sets `overrides` in the process environment for the duration of `body`
/// and restores the previous values after, so that nothing launched later
/// inherits them.
fn with_environment<T>(
  overrides: &[(&'static str, OsString)],
  body: impl FnOnce() -> T,
) -> T {
  let saved = overrides
    .iter()
    .map(|(name, _)| (*name, std::env::var_os(name)))
    .collect::<Vec<_>>();
  for (name, value) in overrides {
    // SAFETY: the launcher creates its renderer on the main thread before
    // it starts any other thread, so no thread reads the environment
    // concurrently.
    unsafe { std::env::set_var(name, value) };
  }
  let result = body();
  for (name, value) in saved {
    // SAFETY: as above; still no other thread runs.
    unsafe {
      match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
      }
    }
  }
  result
}

/// Gloomy's renderer on a wgpu device, with an offscreen target and a
/// buffer to read it back through.
pub struct Renderer {
  device: wgpu::Device,
  queue: wgpu::Queue,
  gloomy: GloomyRenderer,
  spare_images: ImageRenderer,
  spare_textures: HashMap<String, gloomy_core::texture::Texture>,
  target: Option<Target>,
  scale: f32,
  adapter: String,
}

/// The offscreen texture and its readback buffer, for one size.
struct Target {
  texture: wgpu::Texture,
  buffer: wgpu::Buffer,
  width: u32,
  height: u32,
  stride: u32,
}

impl Renderer {
  /// A renderer on the driver `choice` picks.
  pub fn new(choice: RendererChoice) -> Result<Self, String> {
    let lavapipe =
      lavapipe_manifest(&icd_directories(|name| std::env::var_os(name)));
    let overrides = loader_overrides(choice, lavapipe.as_deref())?;
    let create = || {
      let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
      });
      let adapter = pollster::block_on(instance.request_adapter(
        &wgpu::RequestAdapterOptions {
          power_preference: wgpu::PowerPreference::LowPower,
          compatible_surface: None,
          force_fallback_adapter: false,
        },
      ))
      .ok_or("no Vulkan adapter")?;
      let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
          label: Some("shingle-launcher"),
          required_features: wgpu::Features::empty(),
          required_limits: wgpu::Limits::downlevel_defaults(),
        },
        None,
      ))
      .map_err(|error| error.to_string())?;
      Ok::<_, String>((adapter.get_info().name, device, queue))
    };
    let (adapter, device, queue) = match &overrides {
      Some(overrides) => with_environment(overrides, create)?,
      None => create()?,
    };
    let gloomy = GloomyRenderer::new(&device, FORMAT, 1, 1, 1.0);
    let spare_images = ImageRenderer::new(&device, FORMAT, 1, 1);
    let mut renderer = Self {
      device,
      queue,
      gloomy,
      spare_images,
      spare_textures: HashMap::new(),
      target: None,
      scale: 1.0,
      adapter,
    };
    renderer.resize(1, 1, 1.0);
    Ok(renderer)
  }

  /// The adapter's name, for the log.
  pub fn adapter(&self) -> &str {
    &self.adapter
  }

  /// Measures text: the width of a text at a size in logical pixels, for
  /// [`crate::view::build`].
  pub fn measurer(&mut self) -> impl Fn(&str, f32) -> f32 + '_ {
    let text: &gloomy_core::TextRenderer = self.gloomy.text();
    move |content, size| text.measure(content, size, None).x
  }

  /// Makes the target `width` × `height` pixels, drawn at `scale`.
  pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
    let width = width.max(1);
    let height = height.max(1);
    if self
      .target
      .as_ref()
      .is_some_and(|target| (target.width, target.height) == (width, height))
      && self.scale == scale
    {
      return;
    }
    let texture = self.device.create_texture(&wgpu::TextureDescriptor {
      label: Some("panel"),
      size: wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: FORMAT,
      usage: wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::COPY_SRC,
      view_formats: &[],
    });
    // wgpu copies rows aligned to 256 bytes.
    let stride = (width * 4).div_ceil(256) * 256;
    let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
      label: Some("panel readback"),
      size: u64::from(stride) * u64::from(height),
      usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
      mapped_at_creation: false,
    });
    self.gloomy.resize(&self.queue, width, height, scale);
    self.spare_images.resize(&self.queue, width, height);
    self.scale = scale;
    self.target = Some(Target {
      texture,
      buffer,
      width,
      height,
      stride,
    });
  }

  /// Draws `frame` over `clear` and hands each row of pixels, `ARGB8888`
  /// with alpha forced opaque, to `row` with its index.
  pub fn draw(
    &mut self,
    frame: &mut Frame,
    clear: Color,
    mut row: impl FnMut(usize, &[u8]),
  ) -> Result<(), String> {
    let target = self.target.as_ref().ok_or("no target")?;
    let logical = Vec2::new(
      target.width as f32 / self.scale,
      target.height as f32 / self.scale,
    );
    let interaction = InteractionState {
      focused_id: Some(FIELD_ID.to_owned()),
      ..InteractionState::default()
    };
    for widget in [&mut frame.main, &mut frame.overlay] {
      compute_layout(widget, 0.0, 0.0, logical.x, logical.y);
    }
    let (r, g, b, a) = clear;
    self.gloomy.set_clear_color(
      f64::from(r),
      f64::from(g),
      f64::from(b),
      f64::from(a),
    );
    render_ui(
      &frame.main,
      &mut self.gloomy,
      &self.device,
      &self.queue,
      Some(&interaction),
      None,
    );
    {
      let (primitives, text) = self.gloomy.split_overlay_mut();
      let mut context = RenderContext::new(
        primitives,
        text,
        &mut self.spare_images,
        &mut self.spare_textures,
        &self.device,
        &self.queue,
        Some(&interaction),
        logical.x as u32,
        logical.y as u32,
        self.scale,
        None,
        None,
        None,
      );
      render_widget(&frame.overlay, &mut context);
    }
    self.gloomy.prepare(&self.device, &self.queue);
    let view = target
      .texture
      .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder =
      self
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
          label: Some("panel"),
        });
    self
      .gloomy
      .render(&mut encoder, &view, &self.device, &self.queue);
    encoder.copy_texture_to_buffer(
      wgpu::ImageCopyTexture {
        texture: &target.texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
      },
      wgpu::ImageCopyBuffer {
        buffer: &target.buffer,
        layout: wgpu::ImageDataLayout {
          offset: 0,
          bytes_per_row: Some(target.stride),
          rows_per_image: Some(target.height),
        },
      },
      wgpu::Extent3d {
        width: target.width,
        height: target.height,
        depth_or_array_layers: 1,
      },
    );
    self.queue.submit([encoder.finish()]);
    let slice = target.buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
      let _ = sender.send(result);
    });
    self.device.poll(wgpu::Maintain::Wait);
    receiver
      .recv()
      .map_err(|error| error.to_string())?
      .map_err(|error| error.to_string())?;
    {
      let data = slice.get_mapped_range();
      let mut line = vec![0_u8; target.width as usize * 4];
      for index in 0..target.height as usize {
        let start = index * target.stride as usize;
        let end = start + line.len();
        line.copy_from_slice(&data[start..end]);
        for pixel in line.chunks_exact_mut(4) {
          pixel[3] = 255;
        }
        row(index, &line);
      }
    }
    target.buffer.unmap();
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn icd_directories_always_include_the_system_ones() {
    let directories = icd_directories(|name| {
      (name == "XDG_DATA_DIRS").then(|| "/fixture:/usr/share".into())
    });
    assert_eq!(
      directories,
      [
        PathBuf::from("/etc/vulkan/icd.d"),
        PathBuf::from("/fixture/vulkan/icd.d"),
        PathBuf::from("/usr/share/vulkan/icd.d"),
        PathBuf::from("/usr/local/share/vulkan/icd.d"),
      ]
    );
  }

  #[test]
  fn lavapipe_is_found_by_its_manifest_name() {
    let directory = tempfile::tempdir().unwrap();
    let icd = directory.path().join("icd.d");
    fs::create_dir_all(&icd).unwrap();
    fs::write(icd.join("nvidia_icd.json"), "{}").unwrap();
    assert_eq!(lavapipe_manifest(std::slice::from_ref(&icd)), None);
    fs::write(icd.join("lvp_icd.x86_64.json"), "{}").unwrap();
    assert_eq!(
      lavapipe_manifest(&[directory.path().join("missing"), icd.clone()]),
      Some(icd.join("lvp_icd.x86_64.json"))
    );
  }

  #[test]
  fn the_choice_decides_the_loader_variables() {
    let manifest = Path::new("/usr/share/vulkan/icd.d/lvp_icd.json");
    assert_eq!(
      loader_overrides(RendererChoice::Hardware, Some(manifest)),
      Ok(None)
    );
    assert_eq!(loader_overrides(RendererChoice::Auto, None), Ok(None));
    assert!(loader_overrides(RendererChoice::Software, None).is_err());
    let overrides = loader_overrides(RendererChoice::Auto, Some(manifest))
      .unwrap()
      .unwrap();
    assert_eq!(overrides[0], ("VK_DRIVER_FILES", manifest.into()));
    assert_eq!(
      overrides[2],
      ("VK_LOADER_LAYERS_DISABLE", "~implicit~".into())
    );
    assert_eq!(
      RendererChoice::from(shingle_config::LauncherRenderer::Software),
      RendererChoice::Software
    );
  }

  #[test]
  fn the_environment_is_restored_after_creation() {
    let name = "SHINGLE_LAUNCHER_TEST_RESTORED";
    let seen =
      with_environment(&[(name, "inside".into())], || std::env::var_os(name));
    assert_eq!(seen, Some("inside".into()));
    assert_eq!(std::env::var_os(name), None);
  }
}
