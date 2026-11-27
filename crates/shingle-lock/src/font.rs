//! The lock screen's font: swaylock's `font` option names a family,
//! which fontconfig resolves (`fc-match`), as Pango does for swaylock.
//! Without fontconfig, or for a font it cannot read, the Inter that
//! Shingle bundles is used.

use std::{path::Path, process::Command};

use ab_glyph::FontArc;

/// Inter Regular, the font Shingle's frontend bundles.
const BUNDLED: &[u8] =
  include_bytes!("../../shingle-frontend/fonts/Inter-Regular.ttf");

/// The bundled Inter.
pub fn bundled() -> FontArc {
  FontArc::try_from_slice(BUNDLED).expect("the bundled font parses")
}

/// The file and face index `fc-match` printed as `FILE:INDEX`.
fn parse_match(output: &str) -> Option<(&str, u32)> {
  let line = output.lines().next()?.trim();
  let (file, index) = line.rsplit_once(':')?;
  if file.is_empty() {
    return None;
  }
  Some((file, index.parse().ok()?))
}

/// The font for `family`, as fontconfig matches it, or the bundled one.
/// Reports what it fell back for on standard error.
pub fn load(family: &str) -> FontArc {
  let matched = Command::new("fc-match")
    .args(["-f", "%{file}:%{index}\n", family])
    .output()
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| {
      let text = String::from_utf8_lossy(&output.stdout).into_owned();
      parse_match(&text).map(|(file, index)| (file.to_owned(), index))
    });
  let Some((file, index)) = matched else {
    eprintln!("shingle-lock: fontconfig did not match {family:?}; using Inter");
    return bundled();
  };
  match read(Path::new(&file), index) {
    Some(font) => font,
    None => {
      eprintln!("shingle-lock: cannot read the font {file}; using Inter");
      bundled()
    }
  }
}

fn read(path: &Path, index: u32) -> Option<FontArc> {
  let bytes = std::fs::read(path).ok()?;
  ab_glyph::FontVec::try_from_vec_and_index(bytes, index)
    .ok()
    .map(FontArc::new)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fc_match_output_names_a_file_and_face() {
    assert_eq!(
      parse_match("/usr/share/fonts/invented.ttc:2\n"),
      Some(("/usr/share/fonts/invented.ttc", 2))
    );
    assert_eq!(parse_match(""), None);
    assert_eq!(parse_match(":0"), None);
    assert_eq!(parse_match("/no/index"), None);
  }

  #[test]
  fn the_bundled_font_has_glyphs() {
    use ab_glyph::Font;
    let font = bundled();
    assert_ne!(font.glyph_id('8').0, 0);
    assert!(read(Path::new("/nonexistent/invented.ttf"), 0).is_none());
  }
}
