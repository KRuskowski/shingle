//! Icons decoded at the size they are drawn, in memory and in one cache
//! file per theme and size, so that a warm start reads pixels instead of
//! looking icons up and decoding them (docs/design/launcher.md, Icons).

use std::{
  collections::HashMap,
  ffi::OsString,
  fs,
  os::unix::ffi::{OsStrExt, OsStringExt},
  path::{Path, PathBuf},
  sync::{Arc, Mutex, OnceLock},
  time::UNIX_EPOCH,
};

use resvg::usvg::{self, TreeParsing, TreeTextToPath, fontdb};

use crate::{files::write_atomically, icons::IconLookup};

/// The first bytes of a cache file, with its format's version. Version 2
/// decodes XPM files and draws the text of SVG files, which version 1's
/// pixels lack.
const MAGIC: &[u8; 8] = b"SLICON02";

/// An icon's pixels: straight (not premultiplied) RGBA, row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconPixels {
  /// Its width in pixels.
  pub width: u32,
  /// Its height in pixels.
  pub height: u32,
  /// `width * height * 4` bytes.
  pub rgba: Vec<u8>,
}

/// What is known of one icon name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedIcon {
  /// The file it was decoded from; empty when none was found.
  pub source: PathBuf,
  /// The file's modification time in nanoseconds, to notice a change.
  pub modified: u64,
  /// The pixels, or `None` when nothing was found or decoding failed.
  pub pixels: Option<Arc<IconPixels>>,
}

/// The icons of one size, by the `Icon` value that names them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IconStore {
  /// The side in device pixels every icon is decoded at.
  pub pixels: u32,
  /// The icons by name.
  pub icons: HashMap<String, CachedIcon>,
}

impl IconStore {
  /// An empty store for icons of `pixels` device pixels.
  pub fn new(pixels: u32) -> Self {
    Self {
      pixels,
      icons: HashMap::new(),
    }
  }

  /// The pixels of `icon`, when they are loaded.
  pub fn get(&self, icon: &str) -> Option<&Arc<IconPixels>> {
    self.icons.get(icon)?.pixels.as_ref()
  }

  /// Whether `icon` was looked up, found or not.
  pub fn knows(&self, icon: &str) -> bool {
    self.icons.contains_key(icon)
  }

  /// The store as the bytes of a cache file.
  pub fn encode(&self) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    let mut names = self.icons.keys().collect::<Vec<_>>();
    names.sort();
    put(&mut bytes, self.pixels);
    put(&mut bytes, names.len() as u32);
    for name in names {
      let icon = &self.icons[name];
      put_bytes(&mut bytes, name.as_bytes());
      put_bytes(&mut bytes, icon.source.as_os_str().as_bytes());
      bytes.extend_from_slice(&icon.modified.to_le_bytes());
      match &icon.pixels {
        Some(pixels) => {
          put(&mut bytes, pixels.width);
          put(&mut bytes, pixels.height);
          bytes.extend_from_slice(&pixels.rgba);
        }
        None => {
          put(&mut bytes, 0);
          put(&mut bytes, 0);
        }
      }
    }
    bytes
  }

  /// The store a cache file's `bytes` hold, or `None` when they are not
  /// one, of another version or cut short.
  pub fn decode(bytes: &[u8]) -> Option<Self> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(MAGIC.len())? != MAGIC {
      return None;
    }
    let pixels = reader.u32()?;
    let count = reader.u32()?;
    let mut icons = HashMap::new();
    for _ in 0..count {
      let name = String::from_utf8(reader.chunk()?.to_vec()).ok()?;
      let source = PathBuf::from(OsString::from_vec(reader.chunk()?.to_vec()));
      let modified = u64::from_le_bytes(reader.take(8)?.try_into().ok()?);
      let width = reader.u32()?;
      let height = reader.u32()?;
      let length = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
      let pixels = (length > 0)
        .then(|| {
          Some(Arc::new(IconPixels {
            width,
            height,
            rgba: reader.take(length)?.to_vec(),
          }))
        })
        .flatten();
      if length > 0 && pixels.is_none() {
        return None;
      }
      icons.insert(
        name,
        CachedIcon {
          source,
          modified,
          pixels,
        },
      );
    }
    Some(Self { pixels, icons })
  }

  /// The cache file at `path`, or an empty store of `pixels` when it is
  /// missing, broken or of another size.
  pub fn load(path: &Path, pixels: u32) -> Self {
    fs::read(path)
      .ok()
      .and_then(|bytes| Self::decode(&bytes))
      .filter(|store| store.pixels == pixels)
      .unwrap_or_else(|| Self::new(pixels))
  }

  /// Writes the store to the cache file at `path`.
  pub fn save(&self, path: &Path) -> std::io::Result<()> {
    write_atomically(path, &self.encode())
  }

  /// Writes `changed` into the cache file at `path` over what it holds
  /// now, so that passes running one after another, or at once in
  /// threads of their own, each add their icons and none drops
  /// another's. A file of another size or version starts empty.
  pub fn save_merged(
    path: &Path,
    pixels: u32,
    changed: impl IntoIterator<Item = (String, CachedIcon)>,
  ) -> std::io::Result<()> {
    static SAVING: Mutex<()> = Mutex::new(());
    let _saving = SAVING.lock().unwrap_or_else(|error| error.into_inner());
    let mut store = Self::load(path, pixels);
    store.icons.extend(changed);
    store.save(path)
  }
}

fn put(bytes: &mut Vec<u8>, value: u32) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
  put(bytes, value.len() as u32);
  bytes.extend_from_slice(value);
}

struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, count: usize) -> Option<&'a [u8]> {
    let end = self.at.checked_add(count)?;
    let taken = self.bytes.get(self.at..end)?;
    self.at = end;
    Some(taken)
  }

  fn u32(&mut self) -> Option<u32> {
    Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
  }

  fn chunk(&mut self) -> Option<&'a [u8]> {
    let length = self.u32()? as usize;
    self.take(length)
  }
}

/// The cache file of icons of `theme` drawn `size` logical pixels large
/// as `pixels` device pixels, in `directory`.
pub fn cache_path(
  directory: &Path,
  theme: &str,
  size: u32,
  pixels: u32,
) -> PathBuf {
  let theme = theme
    .chars()
    .map(|character| {
      if character.is_alphanumeric() || "-_.".contains(character) {
        character
      } else {
        '_'
      }
    })
    .collect::<String>();
  directory.join(format!("{theme}-{size}-{pixels}.bin"))
}

/// The modification time of `path` in nanoseconds since the epoch, or 0.
pub fn modified(path: &Path) -> u64 {
  fs::metadata(path)
    .and_then(|metadata| metadata.modified())
    .ok()
    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    .map(|duration| duration.as_nanos() as u64)
    .unwrap_or_default()
}

/// The icon file at `path` decoded into a square of `side` pixels: an SVG
/// drawn at that size, its text with the system's fonts, a PNG or an XPM
/// scaled to it, each centred with its aspect ratio kept.
pub fn decode_file(path: &Path, side: u32) -> Result<IconPixels, String> {
  let data = fs::read(path).map_err(|error| error.to_string())?;
  let extension = path
    .extension()
    .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
  match extension.as_deref() {
    Some("svg") => decode_svg_with(&data, side, system_fonts),
    Some("xpm") => decode_xpm(&data, side),
    _ => decode_png(&data, side),
  }
}

/// The system's fonts, found through fontconfig's configuration as
/// resvg's own tools find them, loaded once, the first time an SVG icon
/// with text is decoded: only on the icon thread, after the first frame.
fn system_fonts() -> &'static fontdb::Database {
  static FONTS: OnceLock<fontdb::Database> = OnceLock::new();
  FONTS.get_or_init(|| {
    let mut fonts = fontdb::Database::new();
    fonts.load_system_fonts();
    fonts
  })
}

/// An SVG drawn into a square of `side` pixels, without its text.
pub fn decode_svg(data: &[u8], side: u32) -> Result<IconPixels, String> {
  static NO_FONTS: OnceLock<fontdb::Database> = OnceLock::new();
  decode_svg_with(data, side, || NO_FONTS.get_or_init(fontdb::Database::new))
}

/// An SVG drawn into a square of `side` pixels, its text with the fonts
/// `fonts` gives, which is only called for an SVG with text.
pub fn decode_svg_with<'a>(
  data: &[u8],
  side: u32,
  fonts: impl FnOnce() -> &'a fontdb::Database,
) -> Result<IconPixels, String> {
  let mut tree = usvg::Tree::from_data(data, &usvg::Options::default())
    .map_err(|error| error.to_string())?;
  // Text nodes are drawn only once they are paths. A file without a
  // `<text` element has none, and loading fonts for it would be wasted.
  if data.windows(5).any(|window| window == b"<text") {
    tree.convert_text(fonts());
  }
  let mut pixmap =
    tiny_skia::Pixmap::new(side, side).ok_or("an empty icon size")?;
  let size = tree.size;
  let scale = (side as f32 / size.width()).min(side as f32 / size.height());
  let transform = tiny_skia::Transform::from_translate(
    (side as f32 - size.width() * scale) / 2.0,
    (side as f32 - size.height() * scale) / 2.0,
  )
  .pre_scale(scale, scale);
  resvg::Tree::from_usvg(&tree).render(transform, &mut pixmap.as_mut());
  let rgba = pixmap
    .pixels()
    .iter()
    .flat_map(|pixel| {
      let color = pixel.demultiply();
      [color.red(), color.green(), color.blue(), color.alpha()]
    })
    .collect();
  Ok(IconPixels {
    width: side,
    height: side,
    rgba,
  })
}

/// A PNG scaled into a square of `side` pixels, centred, filtered with
/// its alpha premultiplied so that transparent pixels do not darken the
/// edges.
pub fn decode_png(data: &[u8], side: u32) -> Result<IconPixels, String> {
  let image =
    image::load_from_memory_with_format(data, image::ImageFormat::Png)
      .map_err(|error| error.to_string())?
      .to_rgba8();
  fit(image, side)
}

/// An XPM ([`crate::xpm`]) scaled into a square of `side` pixels as
/// [`decode_png`] scales a PNG.
pub fn decode_xpm(data: &[u8], side: u32) -> Result<IconPixels, String> {
  let decoded = crate::xpm::decode(data)?;
  let image =
    image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba)
      .ok_or("an XPM of the wrong length")?;
  fit(image, side)
}

/// `image` scaled into a square of `side` pixels, centred, filtered with
/// its alpha premultiplied so that transparent pixels do not darken the
/// edges.
fn fit(image: image::RgbaImage, side: u32) -> Result<IconPixels, String> {
  let (width, height) = image.dimensions();
  if width == 0 || height == 0 {
    return Err("an empty image".into());
  }
  let scale = (side as f32 / width as f32).min(side as f32 / height as f32);
  let scaled_width = ((width as f32 * scale).round() as u32).clamp(1, side);
  let scaled_height = ((height as f32 * scale).round() as u32).clamp(1, side);
  let scaled = if (scaled_width, scaled_height) == (width, height) {
    image
  } else {
    let mut premultiplied = image;
    for pixel in premultiplied.pixels_mut() {
      let alpha = u16::from(pixel[3]);
      for channel in &mut pixel.0[..3] {
        *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
      }
    }
    let mut scaled = image::imageops::resize(
      &premultiplied,
      scaled_width,
      scaled_height,
      image::imageops::FilterType::CatmullRom,
    );
    for pixel in scaled.pixels_mut() {
      let alpha = u16::from(pixel[3]);
      if alpha > 0 {
        for channel in &mut pixel.0[..3] {
          *channel =
            ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
      }
    }
    scaled
  };
  let mut rgba = vec![0_u8; side as usize * side as usize * 4];
  let left = (side - scaled_width) / 2;
  let top = (side - scaled_height) / 2;
  for (y, row) in scaled.rows().enumerate() {
    for (x, pixel) in row.enumerate() {
      let at = ((top as usize + y) * side as usize + left as usize + x) * 4;
      rgba[at..at + 4].copy_from_slice(&pixel.0);
    }
  }
  Ok(IconPixels {
    width: side,
    height: side,
    rgba,
  })
}

/// What looking up `icon` found: its file, the file's time and its
/// pixels, decoded only when `known` does not already hold the same file
/// at the same time.
pub fn resolve(
  lookup: &mut IconLookup,
  icon: &str,
  size: u32,
  scale: u32,
  pixels: u32,
  known: Option<&CachedIcon>,
) -> CachedIcon {
  let Some(source) = lookup.find(icon, size, scale) else {
    return CachedIcon {
      source: PathBuf::new(),
      modified: 0,
      pixels: None,
    };
  };
  let time = modified(&source);
  if let Some(known) = known
    && known.source == source
    && known.modified == time
  {
    return known.clone();
  }
  let decoded = match decode_file(&source, pixels) {
    Ok(decoded) => Some(Arc::new(decoded)),
    Err(error) => {
      eprintln!("shingle-launcher: icon {}: {error}", source.display());
      None
    }
  };
  CachedIcon {
    source,
    modified: time,
    pixels: decoded,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::icons::IconPaths;

  /// A PNG of `width` × `height` pixels of `color`.
  fn png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba(color));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
  }

  fn pixel(icon: &IconPixels, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * icon.width + x) * 4) as usize;
    icon.rgba[at..at + 4].try_into().unwrap()
  }

  const SQUARES: &str = r##"<svg xmlns="http://www.w3.org/2000/svg"
    width="16" height="8" viewBox="0 0 16 8">
    <rect x="0" y="0" width="8" height="8" fill="#ff0000"/>
    <rect x="8" y="0" width="8" height="8" fill="#0000ff" opacity="0.5"/>
  </svg>"##;

  #[test]
  fn svgs_are_drawn_at_the_size_centred() {
    let icon = decode_svg(SQUARES.as_bytes(), 32).unwrap();
    assert_eq!((icon.width, icon.height), (32, 32));
    // 16 × 8 scales to 32 × 16, centred vertically.
    assert_eq!(pixel(&icon, 4, 4), [0, 0, 0, 0]);
    assert_eq!(pixel(&icon, 4, 16), [255, 0, 0, 255]);
    let blue = pixel(&icon, 28, 16);
    assert_eq!((blue[0], blue[2]), (0, 255));
    assert!((127..=128).contains(&blue[3]), "{blue:?}");
    assert!(decode_svg(b"not svg", 32).is_err());
  }

  /// A 16 × 16 SVG whose only content is text.
  const TEXT: &str = r##"<svg xmlns="http://www.w3.org/2000/svg"
    width="16" height="16" viewBox="0 0 16 16">
    <text x="0" y="16" font-size="16" fill="#00ff00"
      font-family="DejaVu Sans, Liberation Sans, sans-serif">MM</text>
  </svg>"##;

  #[test]
  fn svg_text_is_drawn_with_fonts_and_left_out_without() {
    let opaque = |icon: &IconPixels| {
      icon.rgba.chunks(4).filter(|pixel| pixel[3] > 0).count()
    };
    let without = decode_svg(TEXT.as_bytes(), 32).unwrap();
    assert_eq!(opaque(&without), 0);
    let fonts = system_fonts();
    if fonts.is_empty() {
      eprintln!("no system fonts; text is not drawn");
      return;
    }
    let with = decode_svg_with(TEXT.as_bytes(), 32, || fonts).unwrap();
    assert!(opaque(&with) > 100, "{} pixels drawn", opaque(&with));
    // An SVG without text never asks for fonts.
    let squares = decode_svg_with(SQUARES.as_bytes(), 32, || {
      panic!("fonts loaded for an SVG without text")
    });
    assert!(squares.is_ok());
  }

  #[test]
  fn xpms_are_scaled_like_pngs() {
    let xpm = b"/* XPM */\nstatic char *x[] = {\n\"2 1 2 1\",\n\
      \". c None\",\n\"# c #0A141E\",\n\".#\"};\n";
    let icon = decode_xpm(xpm, 2).unwrap();
    // 2 × 1 in a square of 2, at the top as the centre rounds down.
    assert_eq!((icon.width, icon.height), (2, 2));
    assert_eq!(pixel(&icon, 0, 0), [0, 0, 0, 0]);
    assert_eq!(pixel(&icon, 1, 0), [10, 20, 30, 255]);
    assert_eq!(pixel(&icon, 1, 1), [0, 0, 0, 0]);
    assert!(decode_xpm(b"/* XPM */", 2).is_err());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("old.xpm");
    fs::write(&path, xpm).unwrap();
    assert_eq!(decode_file(&path, 2), Ok(icon));
  }

  #[test]
  fn merged_saves_keep_what_other_passes_wrote() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("icons.bin");
    let icon = |source: &str| CachedIcon {
      source: source.into(),
      modified: 1,
      pixels: None,
    };
    IconStore::save_merged(&path, 8, [("a".to_owned(), icon("/a"))]).unwrap();
    IconStore::save_merged(&path, 8, [("b".to_owned(), icon("/b"))]).unwrap();
    let store = IconStore::load(&path, 8);
    assert_eq!(store.icons.len(), 2);
    assert_eq!(store.icons["a"], icon("/a"));
    // Another size starts again.
    IconStore::save_merged(&path, 9, [("c".to_owned(), icon("/c"))]).unwrap();
    assert_eq!(IconStore::load(&path, 9).icons.len(), 1);
  }

  #[test]
  fn pngs_are_scaled_without_dark_edges() {
    let big = png(64, 64, [10, 200, 30, 255]);
    let icon = decode_png(&big, 28).unwrap();
    assert_eq!((icon.width, icon.height), (28, 28));
    assert_eq!(pixel(&icon, 14, 14), [10, 200, 30, 255]);
    // A half transparent white keeps its colour when scaled.
    let faint = png(48, 24, [255, 255, 255, 64]);
    let icon = decode_png(&faint, 32).unwrap();
    assert_eq!(pixel(&icon, 16, 16), [255, 255, 255, 64]);
    assert_eq!(pixel(&icon, 16, 2), [0, 0, 0, 0]);
    let exact = decode_png(&png(16, 16, [1, 2, 3, 4]), 16).unwrap();
    assert_eq!(pixel(&exact, 0, 0), [1, 2, 3, 4]);
    assert!(decode_png(b"nope", 16).is_err());
  }

  #[test]
  fn the_store_round_trips_and_rejects_other_files() {
    let mut store = IconStore::new(2);
    store.icons.insert(
      "app".into(),
      CachedIcon {
        source: "/icons/app.png".into(),
        modified: 42,
        pixels: Some(Arc::new(IconPixels {
          width: 2,
          height: 2,
          rgba: (0..16).collect(),
        })),
      },
    );
    store.icons.insert(
      "missing".into(),
      CachedIcon {
        source: PathBuf::new(),
        modified: 0,
        pixels: None,
      },
    );
    let bytes = store.encode();
    assert_eq!(IconStore::decode(&bytes), Some(store.clone()));
    assert_eq!(IconStore::decode(&bytes[..bytes.len() - 1]), None);
    assert_eq!(IconStore::decode(b"SLICON00"), None);
    let mut old = bytes.clone();
    old[..8].copy_from_slice(b"SLICON01");
    assert_eq!(IconStore::decode(&old), None);
    assert!(store.knows("missing") && store.get("missing").is_none());
    assert_eq!(store.get("app").unwrap().rgba.len(), 16);
    let directory = tempfile::tempdir().unwrap();
    let path = cache_path(directory.path(), "Bad/Theme", 28, 42);
    assert_eq!(path, directory.path().join("Bad_Theme-28-42.bin"));
    store.save(&path).unwrap();
    assert_eq!(IconStore::load(&path, 2), store);
    assert_eq!(IconStore::load(&path, 3), IconStore::new(3));
  }

  #[test]
  fn resolving_decodes_only_changed_files() {
    let directory = tempfile::tempdir().unwrap();
    let pixmaps = directory.path().join("pixmaps");
    fs::create_dir_all(&pixmaps).unwrap();
    fs::write(pixmaps.join("app.png"), png(8, 8, [9, 9, 9, 255])).unwrap();
    fs::write(pixmaps.join("bad.png"), b"not a png").unwrap();
    let paths = IconPaths {
      base_dirs: Vec::new(),
      pixmap_dirs: vec![pixmaps.clone()],
    };
    let mut lookup = IconLookup::new(paths, "hicolor");
    let found = resolve(&mut lookup, "app", 8, 1, 8, None);
    assert_eq!(found.source, pixmaps.join("app.png"));
    assert_eq!(pixel(found.pixels.as_ref().unwrap(), 0, 0), [9, 9, 9, 255]);
    // The same file at the same time is not decoded again.
    let mut stale = found.clone();
    stale.pixels = Some(Arc::new(IconPixels {
      width: 1,
      height: 1,
      rgba: vec![1, 1, 1, 1],
    }));
    let again = resolve(&mut lookup, "app", 8, 1, 8, Some(&stale));
    assert_eq!(again, stale);
    stale.modified += 1;
    let changed = resolve(&mut lookup, "app", 8, 1, 8, Some(&stale));
    assert_eq!(changed, found);
    let bad = resolve(&mut lookup, "bad", 8, 1, 8, None);
    assert!(bad.pixels.is_none() && !bad.source.as_os_str().is_empty());
    let none = resolve(&mut lookup, "none", 8, 1, 8, None);
    assert!(none.pixels.is_none() && none.source.as_os_str().is_empty());
  }
}
