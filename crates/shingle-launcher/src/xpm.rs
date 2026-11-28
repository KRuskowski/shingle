//! A small decoder of XPM icons, the X PixMap format older applications
//! still install into the pixmaps directories: XPM 3, the C source form
//! `xpm` writes, with colours as hexadecimal values, `None` and X11
//! colour names (libXpm's `XpmReadFileToImage`, read for the format).

use std::{collections::HashMap, fs, sync::OnceLock};

/// The largest side decoded, far above any icon, so that a broken header
/// cannot ask for gigabytes.
const MAX_SIDE: u32 = 4096;
/// The most characters per pixel accepted; libXpm's files use 1 to 3.
const MAX_CHARS_PER_PIXEL: usize = 8;
/// Where X11's colour names are.
const RGB_FILES: [&str; 2] = ["/usr/share/X11/rgb.txt", "/etc/X11/rgb.txt"];

/// An XPM's pixels: straight RGBA, row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
  /// Its width in pixels.
  pub width: u32,
  /// Its height in pixels.
  pub height: u32,
  /// `width * height * 4` bytes.
  pub rgba: Vec<u8>,
}

/// Decodes the XPM 3 file `data`, looking colour names up in X11's
/// `rgb.txt` (read once).
pub fn decode(data: &[u8]) -> Result<Image, String> {
  decode_with(data, &|name| system_colours().get(&name_key(name)).copied())
}

/// Decodes the XPM 3 file `data`, looking colour names up with `named`,
/// which gets a name as the file writes it.
pub fn decode_with(
  data: &[u8],
  named: &dyn Fn(&str) -> Option<[u8; 3]>,
) -> Result<Image, String> {
  let text = String::from_utf8_lossy(data);
  if !text.trim_start().starts_with("/* XPM */") {
    return Err("not an XPM 3 file".into());
  }
  let strings = c_strings(&text);
  let mut strings = strings.iter();
  let header = strings.next().ok_or("no XPM header")?;
  let values = header
    .split_whitespace()
    .take(4)
    .map(|value| value.parse::<u32>().map_err(|_| "a broken XPM header"))
    .collect::<Result<Vec<_>, _>>()?;
  let [width, height, colours, chars] = values[..] else {
    return Err("a short XPM header".into());
  };
  let chars = chars as usize;
  if width == 0
    || height == 0
    || width > MAX_SIDE
    || height > MAX_SIDE
    || chars == 0
    || chars > MAX_CHARS_PER_PIXEL
  {
    return Err(format!("an XPM of {width}x{height}, {chars} per pixel"));
  }
  let mut table = HashMap::new();
  for _ in 0..colours {
    let line = strings.next().ok_or("missing XPM colours")?;
    let key = line.get(..chars).ok_or("a short XPM colour")?;
    let colour = colour_of(&line[chars..], named);
    table.insert(key.to_owned(), colour);
  }
  let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
  for _ in 0..height {
    let row = strings.next().ok_or("missing XPM rows")?;
    let row = row.as_bytes();
    for x in 0..width as usize {
      let key = row
        .get(x * chars..(x + 1) * chars)
        .and_then(|key| std::str::from_utf8(key).ok())
        .ok_or("a short XPM row")?;
      let colour = table.get(key).ok_or("an XPM pixel of no colour")?;
      rgba.extend_from_slice(colour);
    }
  }
  Ok(Image {
    width,
    height,
    rgba,
  })
}

/// The C string literals of `text` in order, outside comments, with
/// backslash escapes taken literally but for `\"` and `\\`.
fn c_strings(text: &str) -> Vec<String> {
  let mut strings = Vec::new();
  let mut characters = text.chars().peekable();
  while let Some(character) = characters.next() {
    match character {
      '/' if characters.peek() == Some(&'*') => {
        characters.next();
        let mut previous = '\0';
        for character in characters.by_ref() {
          if previous == '*' && character == '/' {
            break;
          }
          previous = character;
        }
      }
      '"' => {
        let mut string = String::new();
        while let Some(character) = characters.next() {
          match character {
            '"' => break,
            '\\' => {
              if let Some(escaped) = characters.next() {
                string.push(escaped);
              }
            }
            _ => string.push(character),
          }
        }
        strings.push(string);
      }
      _ => {}
    }
  }
  strings
}

/// The colour of one colour definition after its key characters:
/// `c` (colour visual) first, then `g`, `g4` and `m` (grey and mono
/// visuals), each value running to the next key, as libXpm reads them.
/// `None` is transparent; a value that cannot be read is black.
fn colour_of(
  definition: &str,
  named: &dyn Fn(&str) -> Option<[u8; 3]>,
) -> [u8; 4] {
  const KEYS: [&str; 5] = ["c", "g", "g4", "m", "s"];
  let mut values: Vec<(&str, Vec<&str>)> = Vec::new();
  for token in definition.split_whitespace() {
    let starts_key = KEYS.contains(&token)
      && values.last().is_none_or(|(_, value)| !value.is_empty());
    if starts_key {
      values.push((token, Vec::new()));
    } else if let Some((_, value)) = values.last_mut() {
      value.push(token);
    }
  }
  let value = ["c", "g", "g4", "m"].iter().find_map(|wanted| {
    values
      .iter()
      .find(|(key, value)| key == wanted && !value.is_empty())
      .map(|(_, value)| value.join(" "))
  });
  let Some(value) = value else {
    return [0, 0, 0, 255];
  };
  if value.eq_ignore_ascii_case("none") {
    return [0, 0, 0, 0];
  }
  let rgb = match value.strip_prefix('#') {
    Some(hex) => hex_colour(hex),
    None => named(&value),
  };
  let [red, green, blue] = rgb.unwrap_or([0, 0, 0]);
  [red, green, blue, 255]
}

/// `#RGB`, `#RRGGBB`, `#RRRGGGBBB` or `#RRRRGGGGBBBB`, each channel's
/// most significant byte.
fn hex_colour(hex: &str) -> Option<[u8; 3]> {
  if hex.is_empty()
    || !hex.len().is_multiple_of(3)
    || hex.len() > 12
    || !hex.chars().all(|digit| digit.is_ascii_hexdigit())
  {
    return None;
  }
  let digits = hex.len() / 3;
  let channel = |index: usize| {
    let part = &hex[index * digits..(index + 1) * digits];
    let value = u32::from_str_radix(part, 16).ok()?;
    // Scaled to 8 bits: one digit repeats, more keep the top byte.
    Some(match digits {
      1 => (value * 17) as u8,
      _ => (value >> (4 * (digits - 2))) as u8,
    })
  };
  Some([channel(0)?, channel(1)?, channel(2)?])
}

/// A colour name as X11 compares it: lower case, without spaces.
fn name_key(name: &str) -> String {
  name
    .chars()
    .filter(|character| !character.is_whitespace())
    .flat_map(char::to_lowercase)
    .collect()
}

/// The colours of an `rgb.txt` text, by [`name_key`].
fn parse_rgb_txt(text: &str) -> HashMap<String, [u8; 3]> {
  let mut colours = HashMap::new();
  for line in text.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('!') || line.starts_with('#') {
      continue;
    }
    let mut parts = line.split_whitespace();
    let mut channel = || parts.next()?.parse::<u8>().ok();
    let (Some(red), Some(green), Some(blue)) =
      (channel(), channel(), channel())
    else {
      continue;
    };
    let name = parts.collect::<Vec<_>>().join(" ");
    if !name.is_empty() {
      colours.entry(name_key(&name)).or_insert([red, green, blue]);
    }
  }
  colours
}

/// X11's colour names from the first `rgb.txt` found, read once; a few
/// common names when there is none.
fn system_colours() -> &'static HashMap<String, [u8; 3]> {
  static COLOURS: OnceLock<HashMap<String, [u8; 3]>> = OnceLock::new();
  COLOURS.get_or_init(|| {
    RGB_FILES
      .iter()
      .find_map(|path| fs::read_to_string(path).ok())
      .map(|text| parse_rgb_txt(&text))
      .unwrap_or_else(|| {
        parse_rgb_txt(
          "0 0 0 black\n255 255 255 white\n255 0 0 red\n0 255 0 green\n\
           0 0 255 blue\n255 255 0 yellow\n190 190 190 gray\n\
           190 190 190 grey\n",
        )
      })
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  const ICON: &str = r#"/* XPM */
static char * icon_xpm[] = {
/* columns rows colors chars-per-pixel */
"3 2 4 2 0 0",
"  c None",
".. c #FF0000 s red",
"xX c light  Gray m white",
"o# g4 #0f0 c #00F",
/* pixels */
"  ..xX",
"o#  .."
};
"#;

  fn named(name: &str) -> Option<[u8; 3]> {
    (name_key(name) == "lightgray").then_some([211, 211, 211])
  }

  #[test]
  fn an_icon_decodes_with_its_colours() {
    let image = decode_with(ICON.as_bytes(), &named).unwrap();
    assert_eq!((image.width, image.height), (3, 2));
    let pixel = |x: usize, y: usize| {
      let at = (y * 3 + x) * 4;
      <[u8; 4]>::try_from(&image.rgba[at..at + 4]).unwrap()
    };
    assert_eq!(pixel(0, 0), [0, 0, 0, 0]);
    assert_eq!(pixel(1, 0), [255, 0, 0, 255]);
    // A name with a space, from the colour visual, not the mono one.
    assert_eq!(pixel(2, 0), [211, 211, 211, 255]);
    // `c` wins over `g4` wherever it is.
    assert_eq!(pixel(0, 1), [0, 0, 255, 255]);
    assert_eq!(pixel(2, 1), [255, 0, 0, 255]);
  }

  #[test]
  fn hex_colours_keep_their_top_byte() {
    assert_eq!(hex_colour("fff"), Some([255, 255, 255]));
    assert_eq!(hex_colour("123456"), Some([0x12, 0x34, 0x56]));
    assert_eq!(hex_colour("123456789"), Some([0x12, 0x45, 0x78]));
    assert_eq!(hex_colour("FFFF80800000"), Some([255, 128, 0]));
    assert_eq!(hex_colour("12345"), None);
    assert_eq!(hex_colour("ggg"), None);
  }

  #[test]
  fn strings_skip_comments_and_keep_escaped_quotes() {
    assert_eq!(
      c_strings("/* \"no\" */ \"a\\\"b\", /* x */ \"c\""),
      vec!["a\"b".to_owned(), "c".to_owned()]
    );
  }

  #[test]
  fn broken_files_are_errors() {
    let named = |_: &str| None;
    assert!(decode_with(b"not an xpm", &named).is_err());
    assert!(decode_with(b"/* XPM */ {\"1 1 1\"};", &named).is_err());
    assert!(decode_with(b"/* XPM */ {\"99999 1 1 1\"};", &named).is_err());
    // A pixel of a colour never defined.
    let unknown = b"/* XPM */ {\"1 1 1 1\", \"a c #000\", \"b\"};";
    assert!(decode_with(unknown, &named).is_err());
    // Rows cut short.
    let short = b"/* XPM */ {\"2 2 1 1\", \"a c #000\", \"aa\"};";
    assert!(decode_with(short, &named).is_err());
  }

  #[test]
  fn rgb_txt_names_ignore_case_and_spaces() {
    let colours = parse_rgb_txt(
      "! comment\n255 250 250\t\tsnow\n211 211 211\t\tlight gray\n",
    );
    assert_eq!(colours.get("snow"), Some(&[255, 250, 250]));
    assert_eq!(colours.get(&name_key("Light Gray")), Some(&[211, 211, 211]));
  }
}
