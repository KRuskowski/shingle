//! The lock screen's fonts: a text face and a light face for the clock.
//!
//! swaylock's `font` option names a family, which fontconfig resolves
//! (`fc-match`), as Pango does for swaylock. Without it the first of
//! [`PREFERRED`] that is installed is used, so that the lock screen is
//! set in a clean sans rather than whatever `sans-serif` names. The
//! clock takes the light face of the same family, else the light face
//! of another preferred family, else the text face. Without fontconfig,
//! or for a font it cannot read, the Inter that Shingle bundles is used.

use std::{path::Path, process::Command};

use ab_glyph::FontArc;

/// Inter Regular, the font Shingle's frontend bundles.
const BUNDLED: &[u8] =
  include_bytes!("../../shingle-frontend/fonts/Inter-Regular.ttf");

/// The families used without a `font` option, best first.
pub const PREFERRED: &[&str] =
  &["Inter", "Cantarell", "Noto Sans", "DejaVu Sans"];

/// fontconfig's weight of a light face (`FC_WEIGHT_LIGHT`); thin and
/// extra-light faces weigh less.
const LIGHT: u32 = 50;

/// The `font` value that means no family was asked for.
const UNSET: &str = "sans-serif";

/// The bundled Inter.
pub fn bundled() -> FontArc {
  FontArc::try_from_slice(BUNDLED).expect("the bundled font parses")
}

/// A face fontconfig matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
  /// Its family name.
  pub family: String,
  /// Its fontconfig weight.
  pub weight: u32,
  /// The font file.
  pub file: String,
  /// The face's index in the file.
  pub index: u32,
}

/// The lock screen's faces.
#[derive(Clone)]
pub struct Fonts {
  /// Dots aside, every text but the time.
  pub text: FontArc,
  /// The time, light if there is a light face.
  pub clock: FontArc,
  /// Which faces these are, for the log.
  pub description: String,
}

/// `FAMILY|WEIGHT|FILE:INDEX` as [`fc_match`] asks fontconfig to print.
fn parse_match(output: &str) -> Option<Match> {
  let line = output.lines().next()?.trim();
  let mut parts = line.splitn(3, '|');
  let family = parts.next()?.trim();
  let weight = parts.next()?.trim().parse().ok()?;
  let (file, index) = parts.next()?.rsplit_once(':')?;
  if family.is_empty() || file.is_empty() {
    return None;
  }
  Some(Match {
    family: family.to_owned(),
    weight,
    file: file.to_owned(),
    index: index.parse().ok()?,
  })
}

/// What fontconfig matches for `pattern`, such as `Inter:light`.
fn fc_match(pattern: &str) -> Option<Match> {
  let output = Command::new("fc-match")
    .args(["-f", "%{family[0]}|%{weight}|%{file}:%{index}\n", pattern])
    .output()
    .ok()
    .filter(|output| output.status.success())?;
  parse_match(&String::from_utf8_lossy(&output.stdout))
}

/// The text and clock faces for `requested` (the `font` option, `None`
/// when unset), with `matcher` standing for fontconfig: see the module
/// comment. `None` for the text face means the bundled Inter.
pub fn choose(
  requested: Option<&str>,
  matcher: &dyn Fn(&str) -> Option<Match>,
) -> (Option<Match>, Option<Match>) {
  let installed =
    |family: &str, found: &Match| found.family.eq_ignore_ascii_case(family);
  let text = match requested {
    Some(family) => matcher(family),
    None => PREFERRED.iter().find_map(|family| {
      matcher(family).filter(|found| installed(family, found))
    }),
  };
  let light = |family: &str| {
    matcher(&format!("{family}:light"))
      .filter(|found| installed(family, found) && found.weight <= LIGHT)
  };
  let own = text.as_ref().and_then(|text| light(&text.family));
  let clock = match (own, requested) {
    (Some(own), _) => Some(own),
    (None, Some(_)) => text.clone(),
    (None, None) => PREFERRED
      .iter()
      .find_map(|family| light(family))
      .or_else(|| text.clone()),
  };
  (text, clock)
}

/// The fonts for `family`, the `font` option; reports what it fell back
/// for on standard error.
pub fn load(family: &str) -> Fonts {
  let requested = (family != UNSET).then_some(family);
  let (text, clock) = choose(requested, &fc_match);
  let open = |found: &Option<Match>| {
    let found = found.as_ref()?;
    let font = read(Path::new(&found.file), found.index);
    if font.is_none() {
      eprintln!(
        "shingle-lock: cannot read the font {}; using Inter",
        found.file
      );
    }
    font
  };
  if text.is_none() {
    eprintln!("shingle-lock: fontconfig did not match {family:?}; using Inter");
  }
  let name = |found: &Option<Match>| {
    found
      .as_ref()
      .map_or("the bundled Inter".to_owned(), |found| {
        format!("{} (weight {})", found.family, found.weight)
      })
  };
  let description = format!("text {}, clock {}", name(&text), name(&clock));
  let text_font = open(&text).unwrap_or_else(bundled);
  let clock_font = open(&clock).unwrap_or_else(|| text_font.clone());
  Fonts {
    text: text_font,
    clock: clock_font,
    description,
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
  fn fc_match_output_names_a_family_weight_file_and_face() {
    assert_eq!(
      parse_match("Invented Sans|50|/usr/share/fonts/invented.ttc:2\n"),
      Some(Match {
        family: "Invented Sans".into(),
        weight: 50,
        file: "/usr/share/fonts/invented.ttc".into(),
        index: 2,
      })
    );
    assert_eq!(parse_match(""), None);
    assert_eq!(parse_match("Sans|80|:0"), None);
    assert_eq!(parse_match("Sans|80|/no/index"), None);
    assert_eq!(parse_match("Sans|heavy|/a.ttf:0"), None);
  }

  fn face(family: &str, weight: u32) -> Match {
    Match {
      family: family.into(),
      weight,
      file: format!("/invented/{family}-{weight}.ttf"),
      index: 0,
    }
  }

  /// A fontconfig with `faces` installed: a pattern gets the face of its
  /// family closest to its weight, an unknown family the first face.
  fn matcher(faces: Vec<Match>) -> impl Fn(&str) -> Option<Match> {
    move |pattern: &str| {
      let (family, light) = match pattern.strip_suffix(":light") {
        Some(family) => (family, true),
        None => (pattern, false),
      };
      let wanted = if light { LIGHT } else { 80 };
      faces
        .iter()
        .filter(|face| face.family == family)
        .min_by_key(|face| face.weight.abs_diff(wanted))
        .or(faces.first())
        .cloned()
    }
  }

  #[test]
  fn the_preferred_families_and_a_light_clock() {
    // Inter is not installed: Cantarell, which has no light face, so
    // the clock takes another preferred family's.
    let host = matcher(vec![
      face("Noto Sans", 80),
      face("Cantarell", 80),
      face("DejaVu Sans", 80),
      face("DejaVu Sans", 40),
    ]);
    let (text, clock) = choose(None, &host);
    assert_eq!(text.unwrap().family, "Cantarell");
    assert_eq!(clock.unwrap(), face("DejaVu Sans", 40));
    // A family with its own light face keeps it.
    let inter = matcher(vec![face("Inter", 80), face("Inter", 50)]);
    let (text, clock) = choose(None, &inter);
    assert_eq!(text.unwrap(), face("Inter", 80));
    assert_eq!(clock.unwrap(), face("Inter", 50));
    // `font` wins and keeps its family for the clock without a light
    // face.
    let (text, clock) = choose(Some("Noto Sans"), &host);
    assert_eq!(text.unwrap(), face("Noto Sans", 80));
    assert_eq!(clock.unwrap(), face("Noto Sans", 80));
    // Nothing installed: the bundled Inter.
    assert_eq!(choose(None, &|_| None), (None, None));
  }

  #[test]
  fn the_bundled_font_has_glyphs() {
    use ab_glyph::Font;
    let font = bundled();
    assert_ne!(font.glyph_id('8').0, 0);
    assert!(read(Path::new("/nonexistent/invented.ttf"), 0).is_none());
  }
}
