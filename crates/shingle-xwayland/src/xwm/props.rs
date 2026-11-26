//! Parsers for the ICCCM and EWMH window properties the window manager
//! reads, on the raw bytes of a `GetProperty` reply.
//!
//! Format-32 values arrive in the client's byte order, which x11rb's
//! `RustConnection` asks for as native, so 32-bit items are read with
//! `u32::from_ne_bytes`.

/// `AtomEnum::STRING`, the type of Latin-1 text properties.
pub const STRING_TYPE: u32 = 31;

/// The raw value of one window property.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Property {
  /// The property's type atom.
  pub kind: u32,
  /// 8, 16 or 32.
  pub format: u8,
  /// The value bytes.
  pub value: Vec<u8>,
}

impl Property {
  /// A format-8 property.
  pub fn bytes(kind: u32, value: &[u8]) -> Self {
    Self {
      kind,
      format: 8,
      value: value.to_vec(),
    }
  }

  /// A format-32 property.
  pub fn words(kind: u32, values: &[u32]) -> Self {
    Self {
      kind,
      format: 32,
      value: values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect(),
    }
  }

  /// The 32-bit items, or none if the format is not 32.
  pub fn values32(&self) -> Vec<u32> {
    if self.format != 32 {
      return Vec::new();
    }
    self
      .value
      .chunks_exact(4)
      .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
      .collect()
  }

  /// The first 32-bit item.
  pub fn first32(&self) -> Option<u32> {
    self.values32().first().copied()
  }
}

/// `WM_CLASS`: `(instance, class)`, two NUL-terminated strings. A missing
/// class yields an empty one, a missing value `None`.
pub fn wm_class(value: &[u8]) -> Option<(String, String)> {
  if value.is_empty() {
    return None;
  }
  let mut parts = value.split(|byte| *byte == 0);
  let instance = latin1(parts.next().unwrap_or_default());
  let class = latin1(parts.next().unwrap_or_default());
  Some((instance, class))
}

/// A text property: Latin-1 when its type is `STRING`, and lossy UTF-8
/// for anything else: `UTF8_STRING`, and `COMPOUND_TEXT`, whose ASCII
/// subset is what clients usually send.
pub fn text(property: &Property) -> String {
  let value = property.value.strip_suffix(&[0]).unwrap_or(&property.value);
  if property.kind == STRING_TYPE {
    latin1(value)
  } else {
    String::from_utf8_lossy(value).into_owned()
  }
}

/// Decodes Latin-1, where every byte is the code point of the same value.
fn latin1(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| char::from(*byte)).collect()
}

/// `WM_SIZE_HINTS` flags (ICCCM 4.1.2.3).
mod size_flags {
  pub const US_POSITION: u32 = 1;
  pub const P_POSITION: u32 = 4;
  pub const P_MIN_SIZE: u32 = 16;
  pub const P_MAX_SIZE: u32 = 32;
  pub const P_BASE_SIZE: u32 = 256;
}

/// The parts of `WM_NORMAL_HINTS` Shingle uses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SizeHints {
  /// The minimum size, or the base size when no minimum is given, as
  /// ICCCM 4.1.2.3 allows.
  pub min: Option<(i32, i32)>,
  /// The maximum size.
  pub max: Option<(i32, i32)>,
  /// The user or program asked for its position (`USPosition` or
  /// `PPosition`).
  pub position: bool,
}

impl SizeHints {
  /// Parses the 18 items of `WM_SIZE_HINTS`; a short value keeps only what
  /// it covers.
  pub fn parse(values: &[u32]) -> Self {
    let Some(&flags) = values.first() else {
      return Self::default();
    };
    let pair = |index: usize| -> Option<(i32, i32)> {
      let width = *values.get(index)? as i32;
      let height = *values.get(index + 1)? as i32;
      (width > 0 || height > 0).then_some((width.max(0), height.max(0)))
    };
    let min = if flags & size_flags::P_MIN_SIZE != 0 {
      pair(5)
    } else if flags & size_flags::P_BASE_SIZE != 0 {
      pair(15)
    } else {
      None
    };
    let max = (flags & size_flags::P_MAX_SIZE != 0)
      .then(|| pair(7))
      .flatten();
    Self {
      min,
      max,
      position: flags & (size_flags::US_POSITION | size_flags::P_POSITION) != 0,
    }
  }

  /// Whether the minimum and maximum are equal and positive, which
  /// Wayfire's tile plugin treats as a fixed-size window.
  pub fn fixed(&self) -> bool {
    matches!((self.min, self.max), (Some(min), Some(max))
      if min == max && min.0 > 0 && min.1 > 0)
  }
}

/// The parts of `WM_HINTS` Shingle uses (ICCCM 4.1.2.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WmHints {
  /// The client wants keyboard input from the window manager. Without the
  /// `InputHint` flag this is `true`, as wlroots assumes.
  pub input: bool,
  /// `XUrgencyHint`.
  pub urgent: bool,
}

impl Default for WmHints {
  fn default() -> Self {
    Self {
      input: true,
      urgent: false,
    }
  }
}

impl WmHints {
  /// Parses `WM_HINTS`.
  pub fn parse(values: &[u32]) -> Self {
    const INPUT_HINT: u32 = 1;
    const URGENCY_HINT: u32 = 256;
    let Some(&flags) = values.first() else {
      return Self::default();
    };
    let input = if flags & INPUT_HINT != 0 {
      values.get(1).is_none_or(|input| *input != 0)
    } else {
      true
    };
    Self {
      input,
      urgent: flags & URGENCY_HINT != 0,
    }
  }
}

/// `_MOTIF_WM_HINTS`: `Some(false)` when the client leaves out the title
/// or the border, `Some(true)` when it asks for both or for all
/// decorations, `None` when it says nothing about them. This is wlroots'
/// reading (`read_surface_motif_hints`), which ignores hints of fewer than
/// five words and takes `MWM_DECOR_ALL` as everything, and Wayfire draws
/// a frame only for `Some(true)` or `None`
/// (wayfire:src/view/xwayland/xwayland-toplevel-view.hpp,
/// `update_decorated`).
pub fn motif_decorations(values: &[u32]) -> Option<bool> {
  const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
  const MWM_DECOR_ALL: u32 = 1 << 0;
  const MWM_DECOR_BORDER: u32 = 1 << 1;
  const MWM_DECOR_TITLE: u32 = 1 << 3;
  if values.len() < 5 || values[0] & MWM_HINTS_DECORATIONS == 0 {
    return None;
  }
  let decorations = values[2];
  Some(
    decorations & MWM_DECOR_ALL != 0
      || decorations & (MWM_DECOR_BORDER | MWM_DECOR_TITLE)
        == MWM_DECOR_BORDER | MWM_DECOR_TITLE,
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn wm_class_splits_instance_and_class() {
    assert_eq!(
      wm_class(b"xterm\0XTerm\0"),
      Some(("xterm".into(), "XTerm".into()))
    );
    assert_eq!(wm_class(b"only"), Some(("only".into(), String::new())));
    assert_eq!(wm_class(b""), None);
    // Latin-1, not UTF-8.
    assert_eq!(wm_class(b"\xe9\0C\0").unwrap().0, "\u{e9}");
  }

  #[test]
  fn text_follows_the_property_type() {
    let title = Property::bytes(500, "Grüße".as_bytes());
    assert_eq!(text(&title), "Grüße");
    let latin = Property::bytes(STRING_TYPE, b"Gr\xfc\xdfe");
    assert_eq!(text(&latin), "Grüße");
    let terminated = Property::bytes(STRING_TYPE, b"xterm\0");
    assert_eq!(text(&terminated), "xterm");
  }

  #[test]
  fn words_round_trip_in_native_order() {
    let property = Property::words(4, &[1, 0xdead_beef]);
    assert_eq!(property.values32(), [1, 0xdead_beef]);
    assert_eq!(property.first32(), Some(1));
    assert!(Property::bytes(4, b"abcd").values32().is_empty());
  }

  /// A `WM_SIZE_HINTS` value with `flags` and the given sizes.
  fn size_hints(
    flags: u32,
    min: (u32, u32),
    max: (u32, u32),
    base: (u32, u32),
  ) -> Vec<u32> {
    let mut values = vec![0; 18];
    values[0] = flags;
    values[5] = min.0;
    values[6] = min.1;
    values[7] = max.0;
    values[8] = max.1;
    values[15] = base.0;
    values[16] = base.1;
    values
  }

  #[test]
  fn size_hints_read_min_max_and_base() {
    let hints =
      SizeHints::parse(&size_hints(16 | 32, (100, 50), (400, 300), (0, 0)));
    assert_eq!(hints.min, Some((100, 50)));
    assert_eq!(hints.max, Some((400, 300)));
    assert!(!hints.position);
    assert!(!hints.fixed());
    let base = SizeHints::parse(&size_hints(256 | 4, (1, 1), (0, 0), (20, 10)));
    assert_eq!(base.min, Some((20, 10)));
    assert_eq!(base.max, None);
    assert!(base.position);
    let fixed =
      SizeHints::parse(&size_hints(48, (300, 200), (300, 200), (0, 0)));
    assert!(fixed.fixed());
  }

  #[test]
  fn short_or_empty_size_hints_are_tolerated() {
    assert_eq!(SizeHints::parse(&[]), SizeHints::default());
    assert_eq!(SizeHints::parse(&[16, 0, 0, 0, 0, 5]).min, None);
    assert!(SizeHints::parse(&[1]).position);
  }

  #[test]
  fn wm_hints_default_input_to_true() {
    assert_eq!(WmHints::parse(&[]), WmHints::default());
    assert!(WmHints::default().input);
    // No InputHint flag: input is assumed.
    assert!(WmHints::parse(&[0, 0]).input);
    assert!(!WmHints::parse(&[1, 0]).input);
    assert!(WmHints::parse(&[1, 1]).input);
    assert!(WmHints::parse(&[256 | 1, 1]).urgent);
  }

  #[test]
  fn motif_hints_report_decorations() {
    assert_eq!(motif_decorations(&[]), None);
    assert_eq!(motif_decorations(&[1, 1, 0, 0, 0]), None);
    assert_eq!(motif_decorations(&[2, 0, 0, 0, 0]), Some(false));
    assert_eq!(motif_decorations(&[2, 0, 1, 0, 0]), Some(true));
    // Too short for wlroots.
    assert_eq!(motif_decorations(&[2, 0, 0]), None);
    // A border without a title, or a title without a border, is the
    // client's own frame; both are a server-side one.
    assert_eq!(motif_decorations(&[2, 0, 2, 0, 0]), Some(false));
    assert_eq!(motif_decorations(&[2, 0, 8, 0, 0]), Some(false));
    assert_eq!(motif_decorations(&[2, 0, 10, 0, 0]), Some(true));
  }
}
