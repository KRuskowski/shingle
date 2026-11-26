//! The frontend's side of shared-memory uploads (docs/design/dmabuf.md,
//! section 5): which rectangles of a committed `wl_shm` buffer to write,
//! and the write itself, straight from the client's pool.
//!
//! [`crate::Frontend::attach_gpu`] turns on
//! [`shingle::Compositor::set_shm_upload`], so committed shm buffers stay
//! with the compositor, locked, until they are uploaded here. The
//! frontend uploads each one right after the dispatch that committed it,
//! drawn or not, and releases it once `write_texture` has returned,
//! because wgpu has then copied the bytes into its staging memory.

use shingle::{BufferSnapshot, ShmContent};
use shingle_core::Rect;
use shingle_render::{
  GpuTexture, ShmImage, TextureFilter, TextureUploader, UploadRect,
};

/// More damage rectangles than this are uploaded as their bounding box,
/// so that a commit with many small rectangles costs a few calls.
const MAX_RECTS: usize = 16;

/// The rectangles of a `width` by `height` buffer to write: all of it
/// when `full` (a new texture), otherwise each damage rectangle, clipped
/// to the buffer. Many rectangles become their bounding box, and
/// rectangles that together cover at least the buffer's area become the
/// whole buffer, which is one write.
pub(crate) fn upload_rects(
  width: u32,
  height: u32,
  damage: &[Rect],
  full: bool,
) -> Vec<UploadRect> {
  let whole = [0, 0, width, height];
  if full {
    return vec![whole];
  }
  let mut rects = damage
    .iter()
    .filter_map(|rect| clip(rect, width, height))
    .collect::<Vec<_>>();
  let area = rects
    .iter()
    .map(|rect| u64::from(rect[2]) * u64::from(rect[3]))
    .sum::<u64>();
  if area >= u64::from(width) * u64::from(height) {
    return vec![whole];
  }
  if rects.len() > MAX_RECTS {
    rects = vec![bounding_box(&rects)];
  }
  rects
}

/// `rect` clipped to a `width` by `height` buffer, or `None` if nothing
/// of it is inside.
fn clip(rect: &Rect, width: u32, height: u32) -> Option<UploadRect> {
  let left = i64::from(rect.x).clamp(0, i64::from(width));
  let top = i64::from(rect.y).clamp(0, i64::from(height));
  let right =
    (i64::from(rect.x) + i64::from(rect.width)).clamp(0, i64::from(width));
  let bottom =
    (i64::from(rect.y) + i64::from(rect.height)).clamp(0, i64::from(height));
  (right > left && bottom > top).then_some([
    left as u32,
    top as u32,
    (right - left) as u32,
    (bottom - top) as u32,
  ])
}

/// The smallest rectangle holding all of `rects`, which is not empty.
fn bounding_box(rects: &[UploadRect]) -> UploadRect {
  let left = rects.iter().map(|rect| rect[0]).min().unwrap_or(0);
  let top = rects.iter().map(|rect| rect[1]).min().unwrap_or(0);
  let right = rects
    .iter()
    .map(|rect| rect[0] + rect[2])
    .max()
    .unwrap_or(0);
  let bottom = rects
    .iter()
    .map(|rect| rect[1] + rect[3])
    .max()
    .unwrap_or(0);
  [left, top, right - left, bottom - top]
}

/// What one upload did.
pub(crate) struct Uploaded {
  /// The texture now showing the buffer.
  pub(crate) texture: GpuTexture,
  /// Bytes copied from the client's pool.
  pub(crate) bytes: usize,
}

/// Uploads `content`, the shm buffer `buffer` shows, into `existing` if
/// it fits, writing only the damage, or into a new texture written in
/// full. `damage_known` is false when commits may have been missed, as
/// after a lost device, so that only a full write is right. The buffer
/// is not released here.
pub(crate) fn upload(
  uploader: &TextureUploader,
  existing: Option<&GpuTexture>,
  buffer: &BufferSnapshot,
  content: &ShmContent,
  filter: TextureFilter,
  damage_known: bool,
) -> Result<Uploaded, String> {
  content
    .read(|bytes| {
      let image = ShmImage {
        width: buffer.width,
        height: buffer.height,
        stride: content.stride(),
        fourcc: content.fourcc(),
        bytes,
      };
      let reuse = existing.filter(|texture| uploader.fits(texture, &image));
      let texture = match reuse {
        Some(texture) => texture.clone(),
        None => uploader.create(&image, filter)?,
      };
      let rects = upload_rects(
        buffer.width,
        buffer.height,
        &buffer.damage,
        reuse.is_none() || !damage_known,
      );
      let bytes = uploader.write(&texture, &image, &rects)?;
      Ok(Uploaded { texture, bytes })
    })?
    .map_err(|error: shingle_render::UploadError| error.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_new_texture_is_written_in_full() {
    assert_eq!(
      upload_rects(1000, 1000, &[Rect::new(5, 5, 10, 10)], true),
      [[0, 0, 1000, 1000]]
    );
  }

  #[test]
  fn damage_is_written_rectangle_by_rectangle_and_clipped() {
    assert_eq!(
      upload_rects(1000, 1000, &[Rect::new(500, 500, 10, 10)], false),
      [[500, 500, 10, 10]]
    );
    assert_eq!(
      upload_rects(
        100,
        50,
        &[Rect::new(-5, 40, 20, 20), Rect::new(90, 0, 30, 1)],
        false
      ),
      [[0, 40, 15, 10], [90, 0, 10, 1]]
    );
    assert!(
      upload_rects(100, 50, &[Rect::new(200, 0, 5, 5)], false).is_empty()
    );
    assert!(upload_rects(100, 50, &[], false).is_empty());
  }

  #[test]
  fn damage_covering_the_area_is_one_full_write() {
    assert_eq!(
      upload_rects(
        10,
        10,
        &[Rect::new(0, 0, 10, 6), Rect::new(0, 4, 10, 6)],
        false
      ),
      [[0, 0, 10, 10]]
    );
  }

  #[test]
  fn many_rectangles_become_their_bounding_box() {
    let damage = (0..20)
      .map(|index| Rect::new(index * 10, index, 2, 2))
      .collect::<Vec<_>>();
    assert_eq!(upload_rects(1000, 1000, &damage, false), [[0, 0, 192, 21]]);
    let few = &damage[..MAX_RECTS];
    assert_eq!(upload_rects(1000, 1000, few, false).len(), MAX_RECTS);
  }
}
