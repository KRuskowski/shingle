//! MIME types and X selection targets, as wlroots maps them
//! (xwayland/selection/selection.c, `xwm_mime_type_to_atom` and
//! `xwm_mime_type_from_atom`; incoming.c, `source_get_targets`).

/// The MIME type of UTF-8 text, `UTF8_STRING` in X.
pub const UTF8_TEXT: &str = "text/plain;charset=utf-8";

/// The MIME type of plain text, `TEXT` and `STRING` in X.
pub const PLAIN_TEXT: &str = "text/plain";

/// Targets that describe the selection rather than hold its data; they are
/// never offered to Wayland as MIME types.
const META_TARGETS: &[&str] =
  &["TARGETS", "TIMESTAMP", "MULTIPLE", "DELETE", "SAVE_TARGETS"];

/// The target names an X client sees in `TARGETS` for a Wayland MIME type:
/// UTF-8 text is `UTF8_STRING`, plain text is `TEXT` and `STRING`, and any
/// other type is the atom of the same name.
pub fn targets_for_mime(mime_type: &str) -> Vec<&str> {
  match mime_type {
    UTF8_TEXT => vec!["UTF8_STRING"],
    PLAIN_TEXT => vec!["TEXT", "STRING"],
    other => vec![other],
  }
}

/// The target names for every MIME type of a Wayland selection, after
/// `TIMESTAMP` and `TARGETS`, each once, in the order of `mime_types`.
pub fn targets_for_offer(mime_types: &[String]) -> Vec<String> {
  let mut targets = vec!["TIMESTAMP".to_owned(), "TARGETS".to_owned()];
  for mime_type in mime_types {
    for target in targets_for_mime(mime_type) {
      if !targets.iter().any(|known| known == target) {
        targets.push(target.to_owned());
      }
    }
  }
  targets
}

/// The MIME type a target an X owner offers becomes for Wayland clients:
/// `UTF8_STRING` is UTF-8 text, `TEXT` and `STRING` are plain text, a name
/// containing `/` is itself, and anything else is not offered, as in
/// wlroots' `source_get_targets`.
pub fn mime_for_target(name: &str) -> Option<String> {
  match name {
    "UTF8_STRING" => Some(UTF8_TEXT.to_owned()),
    "TEXT" | "STRING" => Some(PLAIN_TEXT.to_owned()),
    name if META_TARGETS.contains(&name) => None,
    name if name.contains('/') => Some(name.to_owned()),
    _ => None,
  }
}

/// The MIME types for the targets an X owner lists, each paired with the
/// target to convert for it, without duplicates; the first target for a
/// MIME type wins.
pub fn offer_from_targets(targets: &[(u32, String)]) -> Vec<(String, u32)> {
  let mut offer: Vec<(String, u32)> = Vec::new();
  for (atom, name) in targets {
    let Some(mime_type) = mime_for_target(name) else {
      continue;
    };
    if !offer.iter().any(|(known, _)| *known == mime_type) {
      offer.push((mime_type, *atom));
    }
  }
  offer
}

/// Which of a Wayland selection's `offered` MIME types answers an X
/// client's request for target `name`: the exact match first, then the
/// other text type for the text targets, and the type of the same name
/// for anything else.
pub fn mime_for_request(name: &str, offered: &[String]) -> Option<String> {
  let candidates: Vec<&str> = match name {
    "UTF8_STRING" => vec![UTF8_TEXT, PLAIN_TEXT, "UTF8_STRING"],
    "TEXT" => vec![PLAIN_TEXT, UTF8_TEXT, "TEXT"],
    "STRING" => vec![PLAIN_TEXT, UTF8_TEXT, "STRING"],
    name if META_TARGETS.contains(&name) => vec![],
    name => vec![name],
  };
  candidates
    .into_iter()
    .find(|candidate| offered.iter().any(|offer| offer == candidate))
    .map(str::to_owned)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
  }

  #[test]
  fn wayland_types_become_targets() {
    assert_eq!(targets_for_mime(UTF8_TEXT), ["UTF8_STRING"]);
    assert_eq!(targets_for_mime(PLAIN_TEXT), ["TEXT", "STRING"]);
    assert_eq!(targets_for_mime("image/png"), ["image/png"]);
  }

  #[test]
  fn an_offer_lists_meta_targets_first_and_each_target_once() {
    // wl-copy offers the X names as MIME types too.
    let offer = strings(&[
      PLAIN_TEXT,
      UTF8_TEXT,
      "TEXT",
      "STRING",
      "UTF8_STRING",
      "image/png",
    ]);
    assert_eq!(
      targets_for_offer(&offer),
      strings(&[
        "TIMESTAMP",
        "TARGETS",
        "TEXT",
        "STRING",
        "UTF8_STRING",
        "image/png"
      ])
    );
    assert_eq!(targets_for_offer(&[]), strings(&["TIMESTAMP", "TARGETS"]));
  }

  #[test]
  fn x_targets_become_wayland_types() {
    assert_eq!(mime_for_target("UTF8_STRING").as_deref(), Some(UTF8_TEXT));
    assert_eq!(mime_for_target("TEXT").as_deref(), Some(PLAIN_TEXT));
    assert_eq!(mime_for_target("STRING").as_deref(), Some(PLAIN_TEXT));
    assert_eq!(mime_for_target("image/png").as_deref(), Some("image/png"));
    for skipped in ["TARGETS", "TIMESTAMP", "MULTIPLE", "COMPOUND_TEXT"] {
      assert_eq!(mime_for_target(skipped), None, "{skipped}");
    }
  }

  #[test]
  fn the_first_target_for_a_type_wins() {
    let targets = [
      (1, "TARGETS".to_owned()),
      (2, "UTF8_STRING".to_owned()),
      (3, "STRING".to_owned()),
      (4, "TEXT".to_owned()),
      (5, "text/html".to_owned()),
      (6, "COMPOUND_TEXT".to_owned()),
    ];
    assert_eq!(
      offer_from_targets(&targets),
      [
        (UTF8_TEXT.to_owned(), 2),
        (PLAIN_TEXT.to_owned(), 3),
        ("text/html".to_owned(), 5),
      ]
    );
  }

  #[test]
  fn requests_prefer_the_exact_type_then_the_other_text_type() {
    let both = strings(&[PLAIN_TEXT, UTF8_TEXT]);
    assert_eq!(
      mime_for_request("UTF8_STRING", &both).as_deref(),
      Some(UTF8_TEXT)
    );
    assert_eq!(
      mime_for_request("STRING", &both).as_deref(),
      Some(PLAIN_TEXT)
    );
    let utf8 = strings(&[UTF8_TEXT]);
    assert_eq!(mime_for_request("TEXT", &utf8).as_deref(), Some(UTF8_TEXT));
    let plain = strings(&[PLAIN_TEXT]);
    assert_eq!(
      mime_for_request("UTF8_STRING", &plain).as_deref(),
      Some(PLAIN_TEXT)
    );
    let png = strings(&["image/png"]);
    assert_eq!(
      mime_for_request("image/png", &png).as_deref(),
      Some("image/png")
    );
    assert_eq!(mime_for_request("image/jpeg", &png), None);
    assert_eq!(mime_for_request("UTF8_STRING", &png), None);
    assert_eq!(mime_for_request("TARGETS", &strings(&["TARGETS"])), None);
  }
}
