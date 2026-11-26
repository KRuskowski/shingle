//! The line syntax of Wayfire's INI files, as wf-config reads it
//! (wf-config:src/file.cpp, `load_configuration_options_from_string`).
//!
//! The steps run in wf-config's order: every unescaped `#` starts a
//! comment and `\#` stands for a literal `#`, trailing whitespace goes, a
//! line ending in an unescaped `\` continues on the next one, and empty
//! lines are dropped. What is left is a section header or an option line.

/// One logical line: a source line after comment removal, with any
/// continuation lines appended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Line {
  /// The 1-based number of the first source line it came from.
  pub(crate) number: usize,
  /// The text, without comments and trailing whitespace.
  pub(crate) text: String,
}

/// What a logical line holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Entry<'a> {
  /// `[NAME]`, with the name exactly as written between the brackets.
  Section(&'a str),
  /// `NAME = VALUE`, both trimmed as wf-config trims them.
  Option {
    /// The option name.
    name: &'a str,
    /// The value, possibly empty.
    value: &'a str,
  },
  /// A line that is neither, which wf-config reports and skips.
  Malformed,
}

/// Splits `source` into wf-config's logical lines.
pub(crate) fn lines(source: &str) -> Vec<Line> {
  let stripped = source.split('\n').enumerate().map(|(index, line)| Line {
    number: index + 1,
    text: trim_end(&remove_comment(line)).to_owned(),
  });
  join_lines(stripped)
    .into_iter()
    .filter(|line| !line.text.is_empty())
    .collect()
}

/// Classifies a logical line from [`lines`].
pub(crate) fn entry(line: &str) -> Entry<'_> {
  let trimmed = trim(line);
  if trimmed.len() >= 2 && trimmed.starts_with('[') && trimmed.ends_with(']') {
    return Entry::Section(&trimmed[1..trimmed.len() - 1]);
  }
  match line.split_once('=') {
    Some((name, value)) => Entry::Option {
      name: trim(name),
      value: trim(value),
    },
    None => Entry::Malformed,
  }
}

/// C's `isspace` in the "C" locale, which wf-config trims with.
fn is_space(character: char) -> bool {
  matches!(character, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

fn trim_end(text: &str) -> &str {
  text.trim_end_matches(is_space)
}

/// wf-config's `ignore_leading_trailing_whitespace`: it never removes the
/// last remaining character, so text of only whitespace keeps one.
fn trim(text: &str) -> &str {
  if text.is_empty() {
    return text;
  }
  let bytes = text.as_bytes();
  let mut start = 0;
  let mut end = bytes.len() - 1;
  while start < end && is_space(char::from(bytes[start])) {
    start += 1;
  }
  while start < end && is_space(char::from(bytes[end])) {
    end -= 1;
  }
  &text[start..=end]
}

/// Cuts `line` at its first `#` not preceded by `\`, then turns every
/// `\#` left into `#` (`remove_comments` and `remove_escaped_sharps`).
fn remove_comment(line: &str) -> String {
  let bytes = line.as_bytes();
  let end = (0..bytes.len())
    .find(|&index| {
      bytes[index] == b'#' && (index == 0 || bytes[index - 1] != b'\\')
    })
    .unwrap_or(bytes.len());
  let mut result = String::with_capacity(end);
  let mut had_escape = false;
  for character in line[..end].chars() {
    if character == '#' && had_escape {
      result.pop();
    }
    result.push(character);
    had_escape = character == '\\';
  }
  result
}

/// Appends each line ending in `\` to the next one, without the `\`
/// (`join_lines`). A line ending in `\\` loses one backslash and does not
/// continue; an empty line ends a continuation.
fn join_lines(lines: impl Iterator<Item = Line>) -> Vec<Line> {
  let mut result: Vec<Line> = Vec::new();
  let mut continuing = false;
  for line in lines {
    match result.last_mut() {
      Some(last) if continuing => last.text.push_str(&line.text),
      _ => result.push(line),
    }
    let last = result.last_mut().expect("a line was just pushed");
    continuing = last.text.ends_with('\\');
    if continuing {
      last.text.pop();
      continuing = !last.text.ends_with('\\');
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  fn texts(source: &str) -> Vec<(usize, String)> {
    lines(source)
      .into_iter()
      .map(|line| (line.number, line.text))
      .collect()
  }

  #[test]
  fn an_unescaped_sharp_starts_a_comment_anywhere() {
    assert_eq!(
      texts("# heading\na = 1 # note\nb = #10203040\nc=x#y\n"),
      vec![(2, "a = 1".into()), (3, "b =".into()), (4, "c=x".into()),]
    );
  }

  #[test]
  fn an_escaped_sharp_is_a_literal_sharp() {
    assert_eq!(
      texts("color = \\#101010FF\nboth = \\#1 # gone\n"),
      vec![(1, "color = #101010FF".into()), (2, "both = #1".into())]
    );
    // The backslash before the sharp goes; any other backslash stays.
    assert_eq!(texts("a = x\\y\\#z"), vec![(1, "a = x\\y#z".into())]);
  }

  #[test]
  fn a_trailing_backslash_joins_the_next_line() {
    let source = "plugins = \\\n  alpha \\\n  animate\nnext = 1\n";
    assert_eq!(
      texts(source),
      vec![
        (1, "plugins =   alpha   animate".into()),
        (4, "next = 1".into()),
      ]
    );
  }

  #[test]
  fn an_escaped_trailing_backslash_does_not_join() {
    // WCM writes a value ending in a backslash with a second one.
    assert_eq!(
      texts("a = x\\\\\nb = 2\n"),
      vec![(1, "a = x\\".into()), (2, "b = 2".into())]
    );
  }

  #[test]
  fn an_empty_or_comment_line_ends_a_continuation() {
    assert_eq!(
      texts("a = 1 \\\n# comment\nb = 2\n"),
      vec![(1, "a = 1 ".into()), (3, "b = 2".into())]
    );
    // Trailing whitespace after the backslash is removed first.
    assert_eq!(texts("a = 1\\  \n2\n"), vec![(1, "a = 12".into())]);
  }

  #[test]
  fn whitespace_is_trimmed_as_wf_config_trims_it() {
    assert_eq!(texts("a = 1\r\n\t\x0b\n"), vec![(1, "a = 1".into())]);
    assert_eq!(
      entry("  key\t=  some value"),
      Entry::Option {
        name: "key",
        value: "some value"
      }
    );
    assert_eq!(
      entry("key ="),
      Entry::Option {
        name: "key",
        value: ""
      }
    );
    // Only the first `=` separates name and value.
    assert_eq!(
      entry("rule = on created if a = b"),
      Entry::Option {
        name: "rule",
        value: "on created if a = b"
      }
    );
    assert_eq!(trim("  "), " ");
  }

  #[test]
  fn sections_and_malformed_lines_are_recognised() {
    assert_eq!(entry("  [core]"), Entry::Section("core"));
    assert_eq!(entry("[output:DP-1]"), Entry::Section("output:DP-1"));
    assert_eq!(entry("[ spaced ]"), Entry::Section(" spaced "));
    assert_eq!(entry("no equals sign"), Entry::Malformed);
    assert_eq!(entry("[core"), Entry::Malformed);
  }
}
