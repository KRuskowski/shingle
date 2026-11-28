//! The X resource database on the root window, `RESOURCE_MANAGER`, which
//! `xrdb` writes and Xlib, Xft, GTK and Qt read when a client starts. The
//! window manager sets only the entries it is given and keeps every other
//! line, so resources merged by the startup script or the user survive.

/// `resources` with each of `changes` applied: an entry `(name, Some(v))`
/// replaces every line for `name` with `name:\tv` (appended if there was
/// none), and `(name, None)` removes them. Lines are `name: value`, as
/// `xrdb -query` prints them; other lines keep their order.
pub fn merge(resources: &str, changes: &[(&str, Option<String>)]) -> String {
  let names_line = |line: &str, name: &str| {
    line
      .split_once(':')
      .is_some_and(|(key, _)| key.trim() == name)
  };
  let mut lines = resources
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
  for (name, value) in changes {
    let at = lines.iter().position(|line| names_line(line, name));
    lines.retain(|line| !names_line(line, name));
    if let Some(value) = value {
      let line = format!("{name}:\t{value}");
      match at {
        Some(index) => lines.insert(index.min(lines.len()), line),
        None => lines.push(line),
      }
    }
  }
  let mut merged = lines.join("\n");
  if !merged.is_empty() {
    merged.push('\n');
  }
  merged
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_empty_database_gets_the_entry() {
    assert_eq!(
      merge("", &[("Xft.dpi", Some("144".into()))]),
      "Xft.dpi:\t144\n"
    );
  }

  #[test]
  fn other_lines_stay_and_the_entry_is_replaced_in_place() {
    let before = "XTerm*faceName:\tMonospace\nXft.dpi:\t96\nXft.hinting:\t1\n";
    assert_eq!(
      merge(before, &[("Xft.dpi", Some("192".into()))]),
      "XTerm*faceName:\tMonospace\nXft.dpi:\t192\nXft.hinting:\t1\n"
    );
  }

  #[test]
  fn none_removes_the_entry() {
    let before = "Xft.dpi: 144\nXft.antialias:\t1\n";
    assert_eq!(merge(before, &[("Xft.dpi", None)]), "Xft.antialias:\t1\n");
    assert_eq!(merge("Xft.dpi:\t144\n", &[("Xft.dpi", None)]), "");
  }

  #[test]
  fn names_match_whole_keys_only() {
    let before = "Xft.dpiX:\t1\n";
    assert_eq!(
      merge(before, &[("Xft.dpi", Some("120".into()))]),
      "Xft.dpiX:\t1\nXft.dpi:\t120\n"
    );
  }
}
