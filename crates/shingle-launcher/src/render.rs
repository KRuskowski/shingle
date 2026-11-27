//! Drawing the panel with Gloomy into an offscreen texture and reading it
//! back, and choosing the Vulkan driver it runs on (docs/design/launcher.md,
//! section 4).

use std::{
  collections::HashMap,
  ffi::OsString,
  fs,
  path::{Path, PathBuf},
  sync::Arc,
};

use gloomy_core::{
  GloomyRenderer, InteractionState, Vec2, Vec4, compute_layout,
  image_renderer::ImageRenderer,
  render_ui,
  texture::Texture,
  ui::{RenderContext, render_widget},
};

use crate::{
  icon_cache::{IconPixels, IconStore},
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
  spare_textures: HashMap<String, Texture>,
  icons: HashMap<String, (Arc<IconPixels>, Texture)>,
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
      icons: HashMap::new(),
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

  /// Draws `frame` over `clear`, with the icons of `icons`, and hands each
  /// row of pixels, `ARGB8888` with alpha forced opaque, to `row` with its
  /// index.
  pub fn draw(
    &mut self,
    frame: &mut Frame,
    clear: Color,
    icons: &IconStore,
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
    // Gloomy's image pass runs after its boxes and before its text, but
    // places images in logical pixels on a target of device pixels, so
    // the icons are drawn here at device pixels, one texel per pixel.
    let scale = self.scale;
    for slot in &frame.icons {
      let Some(pixels) = icons.get(&slot.icon).cloned() else {
        continue;
      };
      let size = Vec2::new(pixels.width as f32, pixels.height as f32);
      let corner =
        Vec2::new((slot.rect.x * scale).round(), (slot.rect.y * scale).round());
      // Uploaded once per decoded icon.
      let current = self
        .icons
        .get(&slot.icon)
        .is_some_and(|(uploaded, _)| Arc::ptr_eq(uploaded, &pixels));
      if !current {
        let texture = icon_texture(&self.device, &self.queue, &pixels);
        self
          .icons
          .insert(slot.icon.clone(), (pixels.clone(), texture));
      }
      let texture = &self.icons[&slot.icon].1;
      self.gloomy.images().draw(
        &self.device,
        texture,
        corner + size * 0.5,
        size,
        Vec4::ONE,
      );
    }
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

/// A texture of `pixels` that is sampled as the bytes it holds: not an
/// sRGB format, so that an icon shows as its file's bytes, as the panel's
/// colours do.
fn icon_texture(
  device: &wgpu::Device,
  queue: &wgpu::Queue,
  pixels: &IconPixels,
) -> Texture {
  let size = wgpu::Extent3d {
    width: pixels.width.max(1),
    height: pixels.height.max(1),
    depth_or_array_layers: 1,
  };
  let texture = device.create_texture(&wgpu::TextureDescriptor {
    label: Some("icon"),
    size,
    mip_level_count: 1,
    sample_count: 1,
    dimension: wgpu::TextureDimension::D2,
    format: wgpu::TextureFormat::Rgba8Unorm,
    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    view_formats: &[],
  });
  // Gloomy's image shader maps the texture's last row to the top of the
  // quad, so the rows go in bottom first.
  let stride = pixels.width as usize * 4;
  let flipped = pixels
    .rgba
    .chunks_exact(stride.max(1))
    .rev()
    .flatten()
    .copied()
    .collect::<Vec<_>>();
  queue.write_texture(
    wgpu::ImageCopyTexture {
      texture: &texture,
      mip_level: 0,
      origin: wgpu::Origin3d::ZERO,
      aspect: wgpu::TextureAspect::All,
    },
    &flipped,
    wgpu::ImageDataLayout {
      offset: 0,
      bytes_per_row: Some(4 * pixels.width),
      rows_per_image: Some(pixels.height),
    },
    size,
  );
  let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
  // Drawn one texel per pixel, so nearest sampling changes nothing but
  // keeps half-pixel rounding from blurring the icon.
  let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
    label: Some("icon"),
    mag_filter: wgpu::FilterMode::Nearest,
    min_filter: wgpu::FilterMode::Nearest,
    ..Default::default()
  });
  Texture {
    texture,
    view,
    sampler,
    width: pixels.width,
    height: pixels.height,
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

  /// Draws a panel with one icon slot at `scale` and returns the pixels,
  /// BGRA, and the width; `None` without a Vulkan adapter.
  fn draw_icon(scale: f32) -> Option<(Vec<u8>, usize)> {
    let mut renderer = match Renderer::new(RendererChoice::Auto) {
      Ok(renderer) => renderer,
      Err(error) => {
        eprintln!("skipped, no renderer: {error}");
        return None;
      }
    };
    let side = (20.0 * scale).round() as u32;
    let mut store = IconStore::new(side);
    let mut rgba = Vec::new();
    for y in 0..side {
      for _ in 0..side {
        // Top half opaque red, bottom half half transparent white.
        rgba.extend_from_slice(if y < side / 2 {
          &[255, 0, 0, 255]
        } else {
          &[255, 255, 255, 128]
        });
      }
    }
    store.icons.insert(
      "test".into(),
      crate::icon_cache::CachedIcon {
        source: PathBuf::from("/test.png"),
        modified: 0,
        pixels: Some(Arc::new(IconPixels {
          width: side,
          height: side,
          rgba,
        })),
      },
    );
    let width = (100.0 * scale).round() as u32;
    let height = (60.0 * scale).round() as u32;
    renderer.resize(width, height, scale);
    let mut frame = Frame {
      main: gloomy_core::Widget::container(),
      overlay: gloomy_core::Widget::container(),
      icons: vec![crate::view::IconSlot {
        rect: crate::view::Rect::new(40.0, 20.0, 20.0, 20.0),
        icon: "test".into(),
        row: 0,
      }],
      regions: Vec::new(),
    };
    let mut pixels = Vec::new();
    renderer
      .draw(&mut frame, (0.0, 0.0, 0.0, 1.0), &store, |_, bytes| {
        pixels.extend_from_slice(bytes)
      })
      .unwrap();
    Some((pixels, width as usize))
  }

  #[test]
  fn icons_are_drawn_at_device_pixels_as_their_bytes() {
    for scale in [1.0, 1.5, 2.0] {
      let Some((pixels, width)) = draw_icon(scale) else {
        return;
      };
      let at = |x: f32, y: f32| {
        let index = ((y * scale) as usize * width + (x * scale) as usize) * 4;
        pixels[index..index + 4].to_vec()
      };
      // BGRA: red on top, white at half alpha over black below, black
      // around it.
      assert_eq!(at(41.0, 21.0), [0, 0, 255, 255], "scale {scale}");
      assert_eq!(at(59.5, 29.5), [0, 0, 255, 255], "scale {scale}");
      let below = at(50.0, 35.0);
      assert!(
        below[..3].iter().all(|value| (127..=129).contains(value)),
        "scale {scale}: {below:?}"
      );
      assert_eq!(at(39.0, 30.0), [0, 0, 0, 255], "scale {scale}");
      assert_eq!(at(61.0, 30.0), [0, 0, 0, 255], "scale {scale}");
      assert_eq!(at(50.0, 41.0), [0, 0, 0, 255], "scale {scale}");
    }
  }
}
