//! Render targets: a presentable window surface and an offscreen texture.

use crate::{GpuContext, RenderError, RenderState, RenderTile, draw};

/// A `wgpu::Surface` that frames are drawn into and presented on.
///
/// The surface may come from any window handle, so this crate does not
/// depend on a windowing library.
pub struct WindowTarget {
  surface: wgpu::Surface<'static>,
  config: wgpu::SurfaceConfiguration,
  tearing_present_mode: Option<wgpu::PresentMode>,
}

impl WindowTarget {
  /// Creates a surface for `window` on the context's instance.
  pub fn create_surface(
    instance: &wgpu::Instance,
    window: impl Into<wgpu::SurfaceTarget<'static>>,
  ) -> Result<wgpu::Surface<'static>, RenderError> {
    Ok(instance.create_surface(window)?)
  }

  /// Configures `surface` for `gpu` at `size` physical pixels, preferring
  /// an sRGB format and FIFO presentation.
  pub fn new(
    gpu: &GpuContext,
    surface: wgpu::Surface<'static>,
    size: (u32, u32),
  ) -> Result<Self, RenderError> {
    let capabilities = surface.get_capabilities(gpu.adapter());
    let format = preferred_format(&capabilities.formats)
      .ok_or(RenderError::NoSurfaceFormat)?;
    let config = wgpu::SurfaceConfiguration {
      usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
      format,
      color_space: wgpu::SurfaceColorSpace::Auto,
      width: size.0.max(1),
      height: size.1.max(1),
      desired_maximum_frame_latency: 2,
      present_mode: wgpu::PresentMode::Fifo,
      alpha_mode: wgpu::CompositeAlphaMode::Auto,
      view_formats: vec![],
    };
    surface.configure(gpu.device(), &config);
    Ok(Self {
      surface,
      config,
      tearing_present_mode: capabilities
        .present_modes
        .contains(&wgpu::PresentMode::Immediate)
        .then_some(wgpu::PresentMode::Immediate),
    })
  }

  /// The configured size in physical pixels.
  pub fn size(&self) -> (u32, u32) {
    (self.config.width, self.config.height)
  }

  /// The surface's texture format.
  pub fn format(&self) -> wgpu::TextureFormat {
    self.config.format
  }

  /// Reconfigures the surface for a new size. Zero sizes are ignored.
  pub fn resize(&mut self, gpu: &GpuContext, size: (u32, u32)) {
    if size.0 == 0 || size.1 == 0 {
      return;
    }
    self.config.width = size.0;
    self.config.height = size.1;
    self.surface.configure(gpu.device(), &self.config);
  }

  /// Uses immediate presentation when requested and supported by the host.
  pub fn set_tearing(&mut self, gpu: &GpuContext, tearing: bool) {
    let mode = presentation_mode(self.tearing_present_mode, tearing);
    if self.config.present_mode != mode {
      self.config.present_mode = mode;
      self.surface.configure(gpu.device(), &self.config);
    }
  }

  /// Draws and presents one frame. Returns false when no frame could be
  /// acquired this time (timeout, occlusion or an outdated surface).
  pub fn render(
    &mut self,
    gpu: &GpuContext,
    state: &RenderState,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> Result<bool, RenderError> {
    let frame = match self.surface.get_current_texture() {
      wgpu::CurrentSurfaceTexture::Success(frame)
      | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
      wgpu::CurrentSurfaceTexture::Timeout
      | wgpu::CurrentSurfaceTexture::Occluded => {
        return Ok(false);
      }
      wgpu::CurrentSurfaceTexture::Outdated => {
        self.surface.configure(gpu.device(), &self.config);
        return Ok(false);
      }
      wgpu::CurrentSurfaceTexture::Lost => {
        return Err(RenderError::SurfaceLost);
      }
      wgpu::CurrentSurfaceTexture::Validation => {
        return Err(RenderError::SurfaceValidation);
      }
    };
    let view = frame.texture.create_view(&Default::default());
    draw(gpu, state, &view, self.size(), tiles, clear);
    gpu.queue().present(frame);
    Ok(true)
  }
}

/// Picks the first sRGB format, or else the first format offered.
fn preferred_format(
  formats: &[wgpu::TextureFormat],
) -> Option<wgpu::TextureFormat> {
  formats
    .iter()
    .copied()
    .find(wgpu::TextureFormat::is_srgb)
    .or_else(|| formats.first().copied())
}

fn presentation_mode(
  immediate: Option<wgpu::PresentMode>,
  tearing: bool,
) -> wgpu::PresentMode {
  if tearing {
    immediate.unwrap_or(wgpu::PresentMode::Fifo)
  } else {
    wgpu::PresentMode::Fifo
  }
}

/// A texture that frames are drawn into and read back from, used by
/// `shingle-lock`, by tests and by the KMS copy path.
pub struct OffscreenTarget {
  texture: wgpu::Texture,
  view: wgpu::TextureView,
  width: u32,
  height: u32,
}

impl OffscreenTarget {
  /// The target's texture format. Readback returns RGBA8 in sRGB encoding.
  pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

  /// Allocates a `width` by `height` target.
  pub fn new(
    gpu: &GpuContext,
    width: u32,
    height: u32,
  ) -> Result<Self, RenderError> {
    if width == 0 || height == 0 {
      return Err(RenderError::EmptyTexture);
    }
    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
      label: Some("shingle offscreen target"),
      size: wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: Self::FORMAT,
      usage: wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::COPY_SRC,
      view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Ok(Self {
      texture,
      view,
      width,
      height,
    })
  }

  /// The target size in pixels.
  pub fn size(&self) -> (u32, u32) {
    (self.width, self.height)
  }

  /// The target texture.
  pub fn texture(&self) -> &wgpu::Texture {
    &self.texture
  }

  /// A view of the whole target, for [`draw`].
  pub fn view(&self) -> &wgpu::TextureView {
    &self.view
  }

  /// Draws one frame into the target.
  pub fn render(
    &self,
    gpu: &GpuContext,
    state: &RenderState,
    tiles: &[RenderTile<'_>],
    clear: [f64; 4],
  ) -> wgpu::SubmissionIndex {
    draw(gpu, state, &self.view, self.size(), tiles, clear)
  }

  /// Copies the target to the CPU as tightly packed RGBA8 rows, waiting
  /// for all submitted work first.
  pub fn read_rgba(&self, gpu: &GpuContext) -> Result<Vec<u8>, RenderError> {
    self.read_rgba_within(gpu, None)
  }

  /// [`OffscreenTarget::read_rgba`], waiting at most `timeout` for the
  /// GPU, or without limit for `None`. Running out of time is
  /// [`RenderError::Timeout`]; the KMS copy path treats repeated timeouts
  /// as a hung GPU.
  pub fn read_rgba_within(
    &self,
    gpu: &GpuContext,
    timeout: Option<std::time::Duration>,
  ) -> Result<Vec<u8>, RenderError> {
    let row_bytes = self.width as usize * 4;
    let padded_row_bytes = padded_row_bytes(self.width);
    let readback = gpu.device().create_buffer(&wgpu::BufferDescriptor {
      label: Some("shingle offscreen readback"),
      size: (padded_row_bytes * self.height as usize) as u64,
      usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
      mapped_at_creation: false,
    });
    let mut encoder =
      gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
          label: Some("shingle offscreen readback encoder"),
        });
    encoder.copy_texture_to_buffer(
      wgpu::TexelCopyTextureInfo {
        texture: &self.texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
      },
      wgpu::TexelCopyBufferInfo {
        buffer: &readback,
        layout: wgpu::TexelCopyBufferLayout {
          offset: 0,
          bytes_per_row: Some(padded_row_bytes as u32),
          rows_per_image: Some(self.height),
        },
      },
      wgpu::Extent3d {
        width: self.width,
        height: self.height,
        depth_or_array_layers: 1,
      },
    );
    gpu.queue().submit([encoder.finish()]);
    let (sender, receiver) = std::sync::mpsc::channel();
    readback
      .clone()
      .map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = sender.send(result);
      });
    let wait = wgpu::PollType::Wait {
      submission_index: None,
      timeout,
    };
    gpu
      .device()
      .poll(wait)
      .map_err(|error| match (error, timeout) {
        (wgpu::PollError::Timeout, Some(timeout)) => {
          RenderError::Timeout(timeout)
        }
        (error, _) => RenderError::Readback(error.to_string()),
      })?;
    receiver
      .recv()
      .map_err(|error| RenderError::Readback(error.to_string()))?
      .map_err(|error| RenderError::Readback(error.to_string()))?;
    let mapped = readback
      .get_mapped_range(..)
      .map_err(|error| RenderError::Readback(error.to_string()))?;
    let rgba = unpad_rows(&mapped, row_bytes, padded_row_bytes, self.height);
    drop(mapped);
    readback.unmap();
    Ok(rgba)
  }
}

/// How long [`capture_frame`] waits for the GPU before it gives up.
pub const CAPTURE_TIMEOUT: std::time::Duration =
  std::time::Duration::from_secs(1);

/// Draws `tiles` over `clear` into a new `width` by `height` offscreen
/// target and reads it back as tightly packed RGBA8 rows, for screencopy.
///
/// The frame is drawn in layout logical pixels, at scale 1 and without
/// the output transform, but with `state`'s zoom and the context's gamma
/// and inversion, so it shows what the output shows in the orientation
/// clients lay out in. Waits at most [`CAPTURE_TIMEOUT`] for the GPU.
pub fn capture_frame(
  gpu: &GpuContext,
  state: &RenderState,
  (width, height): (u32, u32),
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
) -> Result<Vec<u8>, RenderError> {
  let target = OffscreenTarget::new(gpu, width, height)?;
  target.render(gpu, &capture_state(state), tiles, clear);
  target.read_rgba_within(gpu, Some(CAPTURE_TIMEOUT))
}

/// Draws `tiles` over `clear` into the DMA-BUF `buffer`, a capture
/// client's, with `state` as it is: its scale and output transform, so
/// that the buffer holds the output's physical pixels as a scanout buffer
/// would. Imports the buffer for rendering and waits at most
/// [`CAPTURE_TIMEOUT`] for the GPU, so that the client may read it as soon
/// as this returns.
pub fn capture_into_dmabuf(
  gpu: &GpuContext,
  state: &RenderState,
  buffer: &crate::DmabufDescriptor<'_>,
  tiles: &[RenderTile<'_>],
  clear: [f64; 4],
) -> Result<(), RenderError> {
  let texture = gpu.import_dmabuf(buffer, crate::DmabufUse::Render)?;
  let view = texture.texture().create_view(&Default::default());
  let mut access = crate::ForeignAccess::new();
  access.add(texture.foreign());
  let index = crate::draw_with_access(
    gpu,
    state,
    &view,
    (buffer.width, buffer.height),
    tiles,
    clear,
    access,
  );
  gpu
    .device()
    .poll(wgpu::PollType::Wait {
      submission_index: Some(index),
      timeout: Some(CAPTURE_TIMEOUT),
    })
    .map(|_| ())
    .map_err(|error| match error {
      wgpu::PollError::Timeout => RenderError::Timeout(CAPTURE_TIMEOUT),
      error => RenderError::Readback(error.to_string()),
    })
}

/// `state` with scale 1 and no output transform, keeping its zoom and
/// filter.
fn capture_state(state: &RenderState) -> RenderState {
  let mut capture = *state;
  capture.set_scale_factor(1.0);
  capture.set_output_transform(crate::TextureTransform::Normal);
  capture
}

/// Bytes per row of a readback buffer, rounded up to wgpu's copy alignment.
fn padded_row_bytes(width: u32) -> usize {
  let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
  (width as usize * 4).div_ceil(alignment) * alignment
}

/// Drops the alignment padding from each of `height` rows.
fn unpad_rows(
  padded: &[u8],
  row_bytes: usize,
  padded_row_bytes: usize,
  height: u32,
) -> Vec<u8> {
  let mut rgba = vec![0; row_bytes * height as usize];
  for (row, destination) in rgba.chunks_exact_mut(row_bytes).enumerate() {
    let start = row * padded_row_bytes;
    destination.copy_from_slice(&padded[start..start + row_bytes]);
  }
  rgba
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tearing_uses_immediate_only_when_requested_and_supported() {
    assert_eq!(
      presentation_mode(Some(wgpu::PresentMode::Immediate), true),
      wgpu::PresentMode::Immediate
    );
    assert_eq!(
      presentation_mode(Some(wgpu::PresentMode::Immediate), false),
      wgpu::PresentMode::Fifo
    );
    assert_eq!(presentation_mode(None, true), wgpu::PresentMode::Fifo);
  }

  #[test]
  fn preferred_format_picks_srgb_then_first() {
    use wgpu::TextureFormat::{Bgra8Unorm, Bgra8UnormSrgb, Rgba8Unorm};
    assert_eq!(
      preferred_format(&[Bgra8Unorm, Bgra8UnormSrgb]),
      Some(Bgra8UnormSrgb)
    );
    assert_eq!(
      preferred_format(&[Rgba8Unorm, Bgra8Unorm]),
      Some(Rgba8Unorm)
    );
    assert_eq!(preferred_format(&[]), None);
  }

  #[test]
  fn draw_builds_one_pipeline_per_target_format() {
    let instance = GpuContext::create_instance();
    let gpu = match pollster::block_on(GpuContext::new(
      instance,
      crate::GpuOptions::default(),
    )) {
      Ok(gpu) => gpu,
      Err(RenderError::RequestAdapter(error)) => {
        eprintln!("skipping, no GPU adapter: {error}");
        return;
      }
      Err(error) => panic!("GPU context failed: {error}"),
    };
    let state = RenderState::default();
    let tiles = [RenderTile::solid(
      crate::RenderRect {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
      },
      [1.0, 0.0, 0.0, 1.0],
    )];
    let rgba = OffscreenTarget::new(&gpu, 4, 4).expect("target");
    // An empty frame only clears, so it needs no pipeline.
    rgba.render(&gpu, &state, &[], [0.0; 4]);
    assert_eq!(gpu.pipeline_count(), 0);
    rgba.render(&gpu, &state, &tiles, [0.0; 4]);
    rgba.render(&gpu, &state, &tiles, [0.0; 4]);
    assert_eq!(gpu.pipeline_count(), 1);
    let bgra = gpu.device().create_texture(&wgpu::TextureDescriptor {
      label: None,
      size: wgpu::Extent3d {
        width: 4,
        height: 4,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: wgpu::TextureFormat::Bgra8UnormSrgb,
      usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
      view_formats: &[],
    });
    let view = bgra.create_view(&Default::default());
    draw(&gpu, &state, &view, (4, 4), &tiles, [0.0; 4]);
    assert_eq!(gpu.pipeline_count(), 2);
  }

  #[test]
  fn captures_ignore_scale_and_output_transform_but_keep_zoom() {
    let mut state = RenderState::default();
    state.set_scale_factor(2.0);
    state.set_output_transform(crate::TextureTransform::Rotate90);
    state.set_zoom(3.0, 10.0, 20.0);
    state.set_nearest_filter(true);
    let capture = capture_state(&state);
    assert_eq!(capture.scale_factor(), 1.0);
    assert_eq!(capture.output_transform(), crate::TextureTransform::Normal);
    assert_eq!(capture.zoom(), (3.0, 10.0, 20.0));
    assert_eq!(capture.texture_filter(), crate::TextureFilter::Nearest);
  }

  #[test]
  fn readback_rows_are_aligned_and_unpadded() {
    assert_eq!(padded_row_bytes(1), 256);
    assert_eq!(padded_row_bytes(64), 256);
    assert_eq!(padded_row_bytes(65), 512);
    let padded = (0..2)
      .flat_map(|row| {
        let mut bytes = vec![row as u8 + 1; 8];
        bytes.resize(256, 0xee);
        bytes
      })
      .collect::<Vec<_>>();
    assert_eq!(
      unpad_rows(&padded, 8, 256, 2),
      [vec![1; 8], vec![2; 8]].concat()
    );
  }
}
