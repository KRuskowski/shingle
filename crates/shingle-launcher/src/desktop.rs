//! Desktop entry files: the `[Desktop Entry]` group, localised values, and
//! the `Exec` key, as the freedesktop Desktop Entry Specification 1.5
//! describes them.

use std::collections::HashMap;

/// The keys of a file's `[Desktop Entry]` group, as written, values with
/// the specification's string escapes still in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopEntry {
  keys: HashMap<String, String>,
}

impl DesktopEntry {
  /// Parses `text`, keeping only the `[Desktop Entry]` group. Comments,
  /// blank lines, other groups (such as desktop actions) and malformed lines
  /// are skipped; a key given twice keeps its first value.
  pub fn parse(text: &str) -> Self {
    let mut keys = HashMap::new();
    let mut in_entry = false;
    for line in text.lines() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      if line.starts_with('[') {
        in_entry = line == "[Desktop Entry]";
        continue;
      }
      if !in_entry {
        continue;
      }
      if let Some((key, value)) = line.split_once('=') {
        keys
          .entry(key.trim().to_owned())
          .or_insert_with(|| value.trim().to_owned());
      }
    }
    Self { keys }
  }

  /// The raw value of `key`, without locale.
  pub fn raw(&self, key: &str) -> Option<&str> {
    self.keys.get(key).map(String::as_str)
  }

  /// The string value of `key`, unescaped.
  pub fn string(&self, key: &str) -> Option<String> {
    self.raw(key).map(unescape)
  }

  /// The best localised value of `key` for `locales` (most specific first,
  /// as [`locale_candidates`] gives them), unescaped; the unlocalised value
  /// if none matches.
  pub fn localized(&self, key: &str, locales: &[String]) -> Option<String> {
    locales
      .iter()
      .find_map(|locale| self.raw(&format!("{key}[{locale}]")))
      .or_else(|| self.raw(key))
      .map(unescape)
  }

  /// A `;`-separated list value, localised as [`Self::localized`], with
  /// empty items dropped.
  pub fn list(&self, key: &str, locales: &[String]) -> Vec<String> {
    let raw = locales
      .iter()
      .find_map(|locale| self.raw(&format!("{key}[{locale}]")))
      .or_else(|| self.raw(key));
    raw.map(split_list).unwrap_or_default()
  }

  /// A boolean value: `true` is true, anything else (or none) false.
  pub fn boolean(&self, key: &str) -> bool {
    self.raw(key) == Some("true")
  }
}

/// Replaces the specification's string escapes `\s`, `\n`, `\t`, `\r` and
/// `\\`; any other backslash is kept.
pub fn unescape(value: &str) -> String {
  let mut result = String::with_capacity(value.len());
  let mut characters = value.chars();
  while let Some(character) = characters.next() {
    if character != '\\' {
      result.push(character);
      continue;
    }
    match characters.next() {
      Some('s') => result.push(' '),
      Some('n') => result.push('\n'),
      Some('t') => result.push('\t'),
      Some('r') => result.push('\r'),
      Some('\\') => result.push('\\'),
      Some(other) => {
        result.push('\\');
        result.push(other);
      }
      None => result.push('\\'),
    }
  }
  result
}

/// Splits a list value at unescaped `;`, unescaping each item.
fn split_list(value: &str) -> Vec<String> {
  let mut items = Vec::new();
  let mut current = String::new();
  let mut characters = value.chars();
  while let Some(character) = characters.next() {
    match character {
      '\\' => match characters.next() {
        Some(';') => current.push(';'),
        Some(other) => {
          current.push('\\');
          current.push(other);
        }
        None => current.push('\\'),
      },
      ';' => items.push(std::mem::take(&mut current)),
      other => current.push(other),
    }
  }
  items.push(current);
  items
    .into_iter()
    .map(|item| unescape(&item))
    .filter(|item| !item.is_empty())
    .collect()
}

/// The locale keys to try for a `LC_MESSAGES` value such as
/// `de_DE.UTF-8@euro`, most specific first: `lang_COUNTRY@MODIFIER`,
/// `lang_COUNTRY`, `lang@MODIFIER`, `lang`. `C` and `POSIX` give none.
pub fn locale_candidates(value: &str) -> Vec<String> {
  let (rest, modifier) = match value.split_once('@') {
    Some((rest, modifier)) => (rest, Some(modifier)),
    None => (value, None),
  };
  let rest = rest.split('.').next().unwrap_or_default();
  if rest.is_empty() || rest == "C" || rest == "POSIX" {
    return Vec::new();
  }
  let (language, country) = match rest.split_once('_') {
    Some((language, country)) => (language, Some(country)),
    None => (rest, None),
  };
  let mut candidates = Vec::new();
  if let (Some(country), Some(modifier)) = (country, modifier) {
    candidates.push(format!("{language}_{country}@{modifier}"));
  }
  if let Some(country) = country {
    candidates.push(format!("{language}_{country}"));
  }
  if let Some(modifier) = modifier {
    candidates.push(format!("{language}@{modifier}"));
  }
  candidates.push(language.to_owned());
  candidates
}

/// What the `%c`, `%k` and `%i` field codes of an `Exec` value expand to.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecContext<'a> {
  /// The entry's translated name, for `%c`.
  pub name: &'a str,
  /// The desktop file's path, for `%k`.
  pub path: &'a str,
  /// The `Icon` value, for `%i`; `%i` expands to nothing without one.
  pub icon: Option<&'a str>,
}

/// The arguments of an `Exec` value whose string escapes were already
/// replaced ([`DesktopEntry::string`]): split at unquoted spaces, double
/// quotes and their backslash escapes removed, and field codes expanded.
/// The launcher opens no files, so `%f`, `%F`, `%u`, `%U` (and the
/// deprecated `%d`, `%D`, `%n`, `%N`, `%v`, `%m`) expand to nothing.
///
/// Returns `None` for an unterminated quote or an empty command.
pub fn parse_exec(
  exec: &str,
  context: &ExecContext<'_>,
) -> Option<Vec<String>> {
  let mut arguments = Vec::new();
  let mut current = String::new();
  let mut has_current = false;
  let mut quoted = false;
  let mut characters = exec.chars().peekable();
  while let Some(character) = characters.next() {
    if quoted {
      match character {
        '"' => quoted = false,
        '\\' => match characters.next() {
          Some(escaped) => current.push(escaped),
          None => return None,
        },
        other => current.push(other),
      }
      continue;
    }
    match character {
      '"' => {
        quoted = true;
        has_current = true;
      }
      ' ' | '\t' => {
        if has_current {
          arguments.push(std::mem::take(&mut current));
          has_current = false;
        }
      }
      '%' => {
        let Some(code) = characters.next() else {
          break;
        };
        match code {
          '%' => {
            current.push('%');
            has_current = true;
          }
          'c' => {
            current.push_str(context.name);
            has_current = true;
          }
          'k' => {
            current.push_str(context.path);
            has_current = true;
          }
          'i' => {
            if let Some(icon) = context.icon.filter(|icon| !icon.is_empty()) {
              if has_current {
                arguments.push(std::mem::take(&mut current));
                has_current = false;
              }
              arguments.push("--icon".to_owned());
              arguments.push(icon.to_owned());
            }
          }
          _ => {}
        }
      }
      other => {
        current.push(other);
        has_current = true;
      }
    }
  }
  if quoted {
    return None;
  }
  if has_current {
    arguments.push(current);
  }
  (!arguments.is_empty()).then_some(arguments)
}

/// `argument` quoted for `/bin/sh`: unchanged when it holds only characters
/// the shell never interprets, else in single quotes.
pub fn shell_quote(argument: &str) -> String {
  let plain = !argument.is_empty()
    && argument.chars().all(|character| {
      character.is_ascii_alphanumeric() || "_@%+=:,./-".contains(character)
    });
  if plain {
    argument.to_owned()
  } else {
    format!("'{}'", argument.replace('\'', r"'\''"))
  }
}

/// `arguments` as one `/bin/sh` command line.
pub fn shell_join(arguments: &[String]) -> String {
  arguments
    .iter()
    .map(|argument| shell_quote(argument))
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
  use super::*;

  const FIREFOX: &str = "\
# A comment
[Desktop Entry]
Type=Application
Name=Firefox
Name[de]=Firefox Webbrowser
GenericName=Web Browser
Keywords=web;browser;internet\\;net;
Keywords[de]=Netz;
Exec=firefox %u
Icon=firefox
Terminal=false
Categories=Network;WebBrowser;

[Desktop Action new-window]
Name=New Window
Exec=firefox --new-window %u
";

  #[test]
  fn parses_the_desktop_entry_group_only() {
    let entry = DesktopEntry::parse(FIREFOX);
    assert_eq!(entry.raw("Type"), Some("Application"));
    assert_eq!(entry.raw("Exec"), Some("firefox %u"));
    assert!(!entry.boolean("Terminal"));
    assert_eq!(entry.list("Categories", &[]), ["Network", "WebBrowser"]);
  }

  #[test]
  fn picks_the_most_specific_locale() {
    let entry = DesktopEntry::parse(FIREFOX);
    let german = locale_candidates("de_AT.UTF-8");
    assert_eq!(german, ["de_AT", "de"]);
    assert_eq!(
      entry.localized("Name", &german).as_deref(),
      Some("Firefox Webbrowser")
    );
    assert_eq!(entry.localized("Name", &[]).as_deref(), Some("Firefox"));
    assert_eq!(entry.list("Keywords", &german), ["Netz"]);
    assert_eq!(
      entry.list("Keywords", &[]),
      ["web", "browser", "internet;net"]
    );
  }

  #[test]
  fn locale_candidates_follow_the_specification() {
    assert_eq!(
      locale_candidates("sr_RS@latin"),
      ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
    );
    assert_eq!(locale_candidates("fr"), ["fr"]);
    assert!(locale_candidates("C.UTF-8").is_empty());
    assert!(locale_candidates("POSIX").is_empty());
  }

  #[test]
  fn unescapes_strings() {
    assert_eq!(unescape(r"a\sb\tc\\d\x"), "a b\tc\\d\\x");
  }

  #[test]
  fn exec_drops_file_codes_and_expands_the_others() {
    let context = ExecContext {
      name: "Kate Editor",
      path: "/usr/share/applications/kate.desktop",
      icon: Some("kate"),
    };
    assert_eq!(parse_exec("kate -b %U", &context).unwrap(), ["kate", "-b"]);
    assert_eq!(
      parse_exec("app %i --name=%c %k 100%%", &context).unwrap(),
      [
        "app",
        "--icon",
        "kate",
        "--name=Kate Editor",
        "/usr/share/applications/kate.desktop",
        "100%"
      ]
    );
    let no_icon = ExecContext::default();
    assert_eq!(parse_exec("app %i %f", &no_icon).unwrap(), ["app"]);
  }

  #[test]
  fn exec_unquotes_arguments() {
    let context = ExecContext::default();
    assert_eq!(
      parse_exec(r#""/opt/My App/run" --say "a \"b\" \$c""#, &context).unwrap(),
      ["/opt/My App/run", "--say", "a \"b\" $c"]
    );
    assert_eq!(parse_exec(r#"sh -c "x"#, &context), None);
    assert_eq!(parse_exec("  %f ", &context), None);
    assert_eq!(parse_exec(r#"app """#, &context).unwrap(), ["app", ""]);
  }

  #[test]
  fn shell_quoting_survives_the_shell() {
    assert_eq!(shell_quote("firefox"), "firefox");
    assert_eq!(shell_quote("--x=1,2"), "--x=1,2");
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
    assert_eq!(shell_quote(""), "''");
    let arguments =
      vec!["printf".to_owned(), "%s|".to_owned(), "a 'b' $c".into()];
    let output = std::process::Command::new("/bin/sh")
      .arg("-c")
      .arg(shell_join(&arguments))
      .output()
      .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "a 'b' $c|");
  }
}
