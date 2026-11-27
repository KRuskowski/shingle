//! Shared-memory uploads without CPU copies (docs/design/dmabuf.md,
//! section 5).
//!
//! A client's `wl_shm` buffer goes to the GPU straight from its pool: the
//! texture has the buffer's byte order (`Bgra8UnormSrgb` for ARGB8888
//! and XRGB8888, `Rgba8UnormSrgb` for ABGR8888 and XBGR8888, from the
//! DRM format table in [`crate::dmabuf`]), X formats get the opaque tile
//! flag instead of a CPU pass that sets alpha, and `Queue::write_texture`
//! reads the rows with the client's stride, which wgpu allows without
//! alignment. Only damaged rectangles are written into an existing
//! texture. The rows are copied into a staging buffer in system memory
//! ([`crate::staging`]) and from there into the texture on the GPU, or,
//! without one, by `Queue::write_texture`, whose own staging memory may
//! be video memory the CPU writes slowly. Either way the bytes are copied
//! before the write returns, so the client may have its buffer back then.

use crate::{
  GpuTexture, TextureFilter,
  dmabuf::{drm_format, tile_flags},
  gpu::bind_texture,
};

/// A client's shared-memory buffer, as its pool holds it.
#[derive(Clone, Copy, Debug)]
pub struct ShmImage<'a> {
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// Bytes from the start of one row to the start of the next.
  pub stride: u32,
  /// The DRM fourcc, one of [`crate::dmabuf::DRM_FORMATS`].
  pub fourcc: u32,
  /// The buffer from its first pixel to the end of its last row's
  /// pixels: at least `stride * (height - 1) + width * 4` bytes.
  pub bytes: &'a [u8],
}

/// A rectangle of a texture to write, `[x, y, width, height]` in texels.
pub type UploadRect = [u32; 4];

/// Why an upload was refused.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum UploadError {
  /// The format is not in [`crate::dmabuf::DRM_FORMATS`].
  #[error("DRM format {0:#010x} cannot be uploaded")]
  UnknownFormat(u32),
  /// The texture does not match the image's size and format.
  #[error("the texture does not match the image")]
  Mismatch,
  /// The image is empty or its bytes are too short for its size and
  /// stride.
  #[error("the image does not fit its bytes")]
  Truncated,
  /// A rectangle reaches outside the image.
  #[error("rectangle {0:?} is outside the image")]
  OutOfBounds(UploadRect),
}

/// Checks `image` and `rects` and returns the bytes each rectangle
/// copies, `width * 4` per row, as they would be written.
pub fn upload_bytes(
  image: &ShmImage<'_>,
  rects: &[UploadRect],
) -> Result<usize, UploadError> {
  drm_format(image.fourcc).ok_or(UploadError::UnknownFormat(image.fourcc))?;
  let row = u64::from(image.width) * 4;
  let required = u64::from(image.height)
    .checked_sub(1)
    .map(|rows| rows * u64::from(image.stride) + row)
    .ok_or(UploadError::Truncated)?;
  if image.width == 0
    || u64::from(image.stride) < row
    || (image.bytes.len() as u64) < required
  {
    return Err(UploadError::Truncated);
  }
  let mut total = 0;
  for rect in rects {
    let [x, y, width, height] = *rect;
    if u64::from(x) + u64::from(width) > u64::from(image.width)
      || u64::from(y) + u64::from(height) > u64::from(image.height)
    {
      return Err(UploadError::OutOfBounds(*rect));
    }
    total += width as usize * height as usize * 4;
  }
  Ok(total)
}

/// Uploads shared-memory buffers into a [`crate::GpuContext`]'s device
/// without the context: clones of its device, queue, texture layout and
/// samplers ([`crate::GpuContext::texture_uploader`]). The frontend
/// holds one, so that it can upload right after a commit, before it has
/// a frame to draw.
#[derive(Clone)]
pub struct TextureUploader {
  pub(crate) device: wgpu::Device,
  pub(crate) queue: wgpu::Queue,
  pub(crate) layout: wgpu::BindGroupLayout,
  pub(crate) linear_sampler: wgpu::Sampler,
  pub(crate) nearest_sampler: wgpu::Sampler,
  pub(crate) staging: Option<crate::staging::SharedStaging>,
}

impl TextureUploader {
  fn sampler(&self, filter: TextureFilter) -> &wgpu::Sampler {
    match filter {
      TextureFilter::Linear => &self.linear_sampler,
      TextureFilter::Nearest => &self.nearest_sampler,
    }
  }

  /// Whether `texture` can take `image`'s contents: a texture this
  /// uploader made for the same size and byte order, not an import.
  pub fn fits(&self, texture: &GpuTexture, image: &ShmImage<'_>) -> bool {
    drm_format(image.fourcc).is_some_and(|format| {
      !texture.is_imported()
        && texture.width == image.width
        && texture.height == image.height
        && texture.texture.format() == format.srgb
        && texture.flags == shm_flags(format.opaque)
    })
  }

  /// A new texture for `image`'s size and format, with nothing written
  /// yet, sampled with `filter`.
  pub fn create(
    &self,
    image: &ShmImage<'_>,
    filter: TextureFilter,
  ) -> Result<GpuTexture, UploadError> {
    let format = drm_format(image.fourcc)
      .ok_or(UploadError::UnknownFormat(image.fourcc))?;
    if image.width == 0 || image.height == 0 {
      return Err(UploadError::Truncated);
    }
    let texture = self.device.create_texture(&wgpu::TextureDescriptor {
      label: Some("shingle shm texture"),
      size: wgpu::Extent3d {
        width: image.width,
        height: image.height,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: format.srgb,
      usage: wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::COPY_SRC,
      view_formats: &[],
    });
    let bind_group =
      bind_texture(&self.device, &self.layout, self.sampler(filter), &texture);
    Ok(GpuTexture {
      texture,
      bind_group,
      width: image.width,
      height: image.height,
      flags: shm_flags(format.opaque),
      foreign: None,
    })
  }

  /// Writes `rects` of `image` into `texture`, which must fit it
  /// ([`TextureUploader::fits`]), from `image.bytes` with its stride:
  /// through the context's staging buffers in system memory
  /// ([`crate::staging`]), or with `Queue::write_texture` where there are
  /// none or one could not be made. Either way the bytes have been copied
  /// out of `image` when this returns. Returns the bytes copied, as
  /// [`upload_bytes`] counts them.
  pub fn write(
    &self,
    texture: &GpuTexture,
    image: &ShmImage<'_>,
    rects: &[UploadRect],
  ) -> Result<usize, UploadError> {
    if !self.fits(texture, image) {
      return Err(UploadError::Mismatch);
    }
    let total = upload_bytes(image, rects)?;
    if let Some(staging) = &self.staging {
      let mut pool = staging.lock().unwrap_or_else(|error| error.into_inner());
      let written = pool.write(
        &self.queue,
        &texture.texture,
        image.bytes,
        image.stride,
        rects,
      );
      match written {
        Ok(()) => return Ok(total),
        Err(error) => {
          eprintln!("shingle: shm staging failed, writing directly: {error}")
        }
      }
    }
    for &[x, y, width, height] in rects {
      if width == 0 || height == 0 {
        continue;
      }
      self.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
          texture: &texture.texture,
          mip_level: 0,
          origin: wgpu::Origin3d { x, y, z: 0 },
          aspect: wgpu::TextureAspect::All,
        },
        image.bytes,
        wgpu::TexelCopyBufferLayout {
          offset: u64::from(y) * u64::from(image.stride) + u64::from(x) * 4,
          bytes_per_row: Some(image.stride),
          rows_per_image: None,
        },
        wgpu::Extent3d {
          width,
          height,
          depth_or_array_layers: 1,
        },
      );
    }
    Ok(total)
  }

  /// What the staging buffers uploads go through have done, or None
  /// when uploads use `Queue::write_texture` ([`crate::staging`]).
  pub fn staging_stats(&self) -> Option<crate::staging::StagingStats> {
    let staging = self.staging.as_ref()?;
    let pool = staging.lock().unwrap_or_else(|error| error.into_inner());
    Some(pool.stats())
  }

  /// `texture` sampled with `filter` instead: the same texels, a new
  /// bind group. Textures uploaded from client buffers that were given
  /// back cannot be uploaded again, so a filter change rebinds them.
  pub fn rebind(
    &self,
    texture: &GpuTexture,
    filter: TextureFilter,
  ) -> GpuTexture {
    GpuTexture {
      bind_group: bind_texture(
        &self.device,
        &self.layout,
        self.sampler(filter),
        &texture.texture,
      ),
      ..texture.clone()
    }
  }

  /// A copy of `texture` made on the GPU, sampled with `filter`, or
  /// `None` for a texture that cannot be copied: an imported DMA-BUF or
  /// one made without `COPY_SRC`. The scene keeps the old frame of a
  /// geometry animation this way.
  pub fn duplicate(
    &self,
    texture: &GpuTexture,
    filter: TextureFilter,
  ) -> Option<GpuTexture> {
    if texture.is_imported()
      || !texture
        .texture
        .usage()
        .contains(wgpu::TextureUsages::COPY_SRC)
    {
      return None;
    }
    let size = texture.texture.size();
    let copy = self.device.create_texture(&wgpu::TextureDescriptor {
      label: Some("shingle texture copy"),
      size,
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: texture.texture.format(),
      usage: wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::COPY_SRC,
      view_formats: &[],
    });
    let mut encoder =
      self
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
          label: Some("shingle texture copy"),
        });
    encoder.copy_texture_to_texture(
      texture.texture.as_image_copy(),
      copy.as_image_copy(),
      size,
    );
    self.queue.submit([encoder.finish()]);
    let bind_group =
      bind_texture(&self.device, &self.layout, self.sampler(filter), &copy);
    Some(GpuTexture {
      texture: copy,
      bind_group,
      width: texture.width,
      height: texture.height,
      flags: texture.flags,
      foreign: None,
    })
  }
}

/// The tile flags of an uploaded shm texture: opaque for X formats. The
/// sRGB format decodes in the sampler, so no `DECODE_SRGB`.
fn shm_flags(opaque: bool) -> u32 {
  if opaque { tile_flags::OPAQUE } else { 0 }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dmabuf::{
    DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888,
  };

  fn image(bytes: &[u8], width: u32, height: u32, stride: u32) -> ShmImage<'_> {
    ShmImage {
      width,
      height,
      stride,
      fourcc: DRM_FORMAT_XRGB8888,
      bytes,
    }
  }

  #[test]
  fn rectangles_copy_their_own_pixels_whatever_the_stride() {
    let bytes = vec![0; 1000 * 4 * 999 + 4000];
    let buffer = image(&bytes, 1000, 1000, 4000);
    assert_eq!(upload_bytes(&buffer, &[[0, 0, 1000, 1000]]), Ok(4_000_000));
    assert_eq!(upload_bytes(&buffer, &[[500, 500, 10, 10]]), Ok(400));
    assert_eq!(
      upload_bytes(&buffer, &[[0, 0, 1, 1], [999, 999, 1, 1]]),
      Ok(8)
    );
    assert_eq!(upload_bytes(&buffer, &[]), Ok(0));
    // A padded stride copies the same bytes.
    let padded = vec![0; 4096 * 9 + 400];
    assert_eq!(
      upload_bytes(&image(&padded, 100, 10, 4096), &[[0, 2, 100, 3]]),
      Ok(1200)
    );
    // A stride that is not a multiple of four is fine too.
    let odd = vec![0; 7 * 2 + 4];
    assert_eq!(upload_bytes(&image(&odd, 1, 3, 7), &[[0, 0, 1, 3]]), Ok(12));
  }

  #[test]
  fn bad_images_and_rectangles_are_refused() {
    let bytes = vec![0; 64];
    assert_eq!(
      upload_bytes(&image(&bytes, 4, 5, 16), &[]),
      Err(UploadError::Truncated)
    );
    assert_eq!(
      upload_bytes(&image(&bytes, 4, 4, 12), &[]),
      Err(UploadError::Truncated)
    );
    assert_eq!(
      upload_bytes(&image(&bytes, 4, 0, 16), &[]),
      Err(UploadError::Truncated)
    );
    assert_eq!(
      upload_bytes(&image(&bytes, 4, 4, 16), &[[3, 0, 2, 1]]),
      Err(UploadError::OutOfBounds([3, 0, 2, 1]))
    );
    assert_eq!(
      upload_bytes(&image(&bytes, 4, 4, 16), &[[0, 4, 1, 1]]),
      Err(UploadError::OutOfBounds([0, 4, 1, 1]))
    );
    let mut rgb565 = image(&bytes, 4, 4, 16);
    rgb565.fourcc = crate::dmabuf::fourcc(b"RG16");
    assert_eq!(
      upload_bytes(&rgb565, &[]),
      Err(UploadError::UnknownFormat(rgb565.fourcc))
    );
  }

  #[test]
  fn x_formats_are_opaque_and_byte_order_follows_the_table() {
    assert_eq!(shm_flags(true), tile_flags::OPAQUE);
    assert_eq!(shm_flags(false), 0);
    assert_eq!(
      drm_format(DRM_FORMAT_ARGB8888).map(|format| format.srgb),
      Some(wgpu::TextureFormat::Bgra8UnormSrgb)
    );
    assert_eq!(
      drm_format(DRM_FORMAT_ABGR8888).map(|format| format.srgb),
      Some(wgpu::TextureFormat::Rgba8UnormSrgb)
    );
  }
}
