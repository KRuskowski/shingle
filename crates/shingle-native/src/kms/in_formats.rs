//! The `IN_FORMATS` blob: which format and modifier pairs a plane scans
//! out.
//!
//! The blob is `struct drm_format_modifier_blob` (include/uapi/drm/
//! drm_mode.h): a header, an array of fourcc formats, and an array of
//! `struct drm_format_modifier`, each naming a modifier and, as a 64-bit
//! mask starting at format index `offset`, the formats it applies to.

use std::fmt;

/// The header's size: six `u32`s.
const HEADER: usize = 24;
/// The size of one `struct drm_format_modifier`.
const MODIFIER: usize = 24;

/// Why a blob is not a valid `IN_FORMATS` blob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InFormatsError {
  /// The blob is shorter than its header.
  Truncated,
  /// The header's version is not 1.
  Version(u32),
  /// An array lies outside the blob.
  OutOfBounds,
}

impl fmt::Display for InFormatsError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Truncated => formatter.write_str("IN_FORMATS blob is truncated"),
      Self::Version(version) => {
        write!(formatter, "IN_FORMATS blob has version {version}, not 1")
      }
      Self::OutOfBounds => {
        formatter.write_str("IN_FORMATS blob points past its end")
      }
    }
  }
}

impl std::error::Error for InFormatsError {}

/// A plane's format and modifier pairs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InFormats {
  pairs: Vec<(u32, u64)>,
}

impl InFormats {
  /// Every `(fourcc, modifier)` pair, ordered by modifier entry and then
  /// by format index, as the blob lists them.
  pub fn pairs(&self) -> &[(u32, u64)] {
    &self.pairs
  }

  /// The modifiers the plane supports for `format`.
  pub fn modifiers(&self, format: u32) -> Vec<u64> {
    self
      .pairs
      .iter()
      .filter(|(fourcc, _)| *fourcc == format)
      .map(|(_, modifier)| *modifier)
      .collect()
  }

  /// Whether the plane scans out `format` with `modifier`.
  pub fn supports(&self, format: u32, modifier: u64) -> bool {
    self.pairs.contains(&(format, modifier))
  }
}

fn u32_at(blob: &[u8], offset: usize) -> Result<u32, InFormatsError> {
  blob
    .get(offset..offset + 4)
    .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap_or_default()))
    .ok_or(InFormatsError::OutOfBounds)
}

fn u64_at(blob: &[u8], offset: usize) -> Result<u64, InFormatsError> {
  blob
    .get(offset..offset + 8)
    .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap_or_default()))
    .ok_or(InFormatsError::OutOfBounds)
}

/// Parses an `IN_FORMATS` blob.
pub fn parse(blob: &[u8]) -> Result<InFormats, InFormatsError> {
  if blob.len() < HEADER {
    return Err(InFormatsError::Truncated);
  }
  let version = u32_at(blob, 0)?;
  if version != 1 {
    return Err(InFormatsError::Version(version));
  }
  let count_formats = u32_at(blob, 8)? as usize;
  let formats_offset = u32_at(blob, 12)? as usize;
  let count_modifiers = u32_at(blob, 16)? as usize;
  let modifiers_offset = u32_at(blob, 20)? as usize;
  let formats = (0..count_formats)
    .map(|index| u32_at(blob, formats_offset + index * 4))
    .collect::<Result<Vec<_>, _>>()?;
  let mut pairs = Vec::new();
  for index in 0..count_modifiers {
    let entry = modifiers_offset + index * MODIFIER;
    let mask = u64_at(blob, entry)?;
    let offset = u32_at(blob, entry + 8)? as usize;
    let modifier = u64_at(blob, entry + 16)?;
    for bit in 0..64 {
      if mask & (1 << bit) != 0
        && let Some(format) = formats.get(offset + bit)
      {
        pairs.push((*format, modifier));
      }
    }
  }
  Ok(InFormats { pairs })
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use crate::kms::{FORMAT_XRGB8888, MODIFIER_LINEAR};

  const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
  const NV12: u32 = u32::from_le_bytes(*b"NV12");
  const X_TILED: u64 = 0x0100_0000_0000_0001;

  /// Builds a blob as the kernel lays it out: header, formats, then
  /// modifiers at an 8-byte aligned offset.
  pub(crate) fn blob(
    formats: &[u32],
    modifiers: &[(u64, u32, u64)],
  ) -> Vec<u8> {
    let formats_offset = HEADER;
    let modifiers_offset = (formats_offset + formats.len() * 4).div_ceil(8) * 8;
    let mut blob = Vec::new();
    for value in [
      1,
      0,
      formats.len() as u32,
      formats_offset as u32,
      modifiers.len() as u32,
      modifiers_offset as u32,
    ] {
      blob.extend_from_slice(&value.to_ne_bytes());
    }
    for format in formats {
      blob.extend_from_slice(&format.to_ne_bytes());
    }
    blob.resize(modifiers_offset, 0);
    for (mask, offset, modifier) in modifiers {
      blob.extend_from_slice(&mask.to_ne_bytes());
      blob.extend_from_slice(&offset.to_ne_bytes());
      blob.extend_from_slice(&0u32.to_ne_bytes());
      blob.extend_from_slice(&modifier.to_ne_bytes());
    }
    blob
  }

  #[test]
  fn pairs_follow_the_modifier_masks() {
    let parsed = parse(&blob(
      &[FORMAT_XRGB8888, ARGB8888, NV12],
      &[(0b111, 0, MODIFIER_LINEAR), (0b011, 0, X_TILED)],
    ))
    .unwrap();
    assert_eq!(
      parsed.pairs(),
      &[
        (FORMAT_XRGB8888, MODIFIER_LINEAR),
        (ARGB8888, MODIFIER_LINEAR),
        (NV12, MODIFIER_LINEAR),
        (FORMAT_XRGB8888, X_TILED),
        (ARGB8888, X_TILED),
      ]
    );
    assert_eq!(
      parsed.modifiers(FORMAT_XRGB8888),
      vec![MODIFIER_LINEAR, X_TILED]
    );
    assert!(parsed.supports(NV12, MODIFIER_LINEAR));
    assert!(!parsed.supports(NV12, X_TILED));
  }

  #[test]
  fn masks_start_at_their_offset() {
    let parsed = parse(&blob(
      &[FORMAT_XRGB8888, ARGB8888, NV12],
      &[(0b10, 1, X_TILED)],
    ))
    .unwrap();
    assert_eq!(parsed.pairs(), &[(NV12, X_TILED)]);
  }

  #[test]
  fn bad_blobs_are_rejected() {
    assert_eq!(parse(&[0; 8]), Err(InFormatsError::Truncated));
    let mut wrong = blob(&[FORMAT_XRGB8888], &[]);
    wrong[0] = 2;
    assert_eq!(parse(&wrong), Err(InFormatsError::Version(2)));
    let mut short = blob(&[FORMAT_XRGB8888], &[(1, 0, MODIFIER_LINEAR)]);
    short.truncate(short.len() - 4);
    assert_eq!(parse(&short), Err(InFormatsError::OutOfBounds));
    let mut past = blob(&[FORMAT_XRGB8888], &[]);
    // A format count larger than the blob holds.
    past[8] = 50;
    assert_eq!(parse(&past), Err(InFormatsError::OutOfBounds));
  }

  #[test]
  fn masks_past_the_format_list_are_ignored() {
    let parsed =
      parse(&blob(&[FORMAT_XRGB8888], &[(u64::MAX, 0, MODIFIER_LINEAR)]))
        .unwrap();
    assert_eq!(parsed.pairs(), &[(FORMAT_XRGB8888, MODIFIER_LINEAR)]);
  }
}
