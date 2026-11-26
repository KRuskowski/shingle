//! Wayfire's options, with their types and defaults, and the values a
//! Wayfire file gave them.
//!
//! The table is data/wayfire-options.tsv, which
//! tools/wayfire_options/generate.py writes from wayfire:metadata/*.xml at
//! the commit docs/design/wayfire-gaps.md pins. A Wayfire import starts from
//! its defaults, and [`WayfireValues`] answers `wayfire/get-config-option`
//! and `list-config-options` for every option in it.

use std::{
  collections::{BTreeMap, BTreeSet},
  sync::LazyLock,
};

/// The table as checked in.
const TABLE: &str = include_str!("../data/wayfire-options.tsv");

/// Sections that are Wayfire objects: each `[output:NAME]` or
/// `[input-device:NAME]` section of a file holds their options.
pub(crate) const OBJECTS: &[&str] = &["output", "input-device"];

/// One option of Wayfire's metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WayfireOption {
  /// The plugin or object, such as `core`, `simple-tile` or `output`.
  pub section: &'static str,
  /// The option name.
  pub name: &'static str,
  /// Wayfire's type: `int`, `double`, `bool`, `string`, `color`, `key`,
  /// `button`, `activator`, `animation` or `dynamic-list`.
  pub kind: &'static str,
  /// The default as Wayfire's metadata writes it; empty for a dynamic
  /// list.
  pub default: &'static str,
  /// For a dynamic list, the prefixes of each entry's options, such as
  /// `command_` and `binding_` for `[command] bindings`; empty otherwise.
  pub entries: Vec<&'static str>,
}

impl WayfireOption {
  /// Whether this is a dynamic list, whose value is a list of entries
  /// rather than one text.
  pub fn is_list(&self) -> bool {
    self.kind == "dynamic-list"
  }
}

static OPTIONS: LazyLock<Vec<WayfireOption>> = LazyLock::new(|| {
  TABLE
    .lines()
    .filter(|line| !line.is_empty() && !line.starts_with('#'))
    .map(|line| {
      let columns = line.split('\t').collect::<Vec<_>>();
      let [section, name, kind, default, entries] = columns[..] else {
        panic!("wayfire-options.tsv: bad line {line:?}");
      };
      WayfireOption {
        section,
        name,
        kind,
        default,
        entries: entries
          .split_whitespace()
          .map(|entry| entry.strip_suffix('*').unwrap_or(entry))
          .collect(),
      }
    })
    .collect()
});

/// Every Wayfire option, sorted by section and then by name.
pub fn wayfire_options() -> &'static [WayfireOption] {
  &OPTIONS
}

/// The option `name` of `section`, where `section` may name an object
/// instance such as `output:DP-1`.
pub fn wayfire_option(
  section: &str,
  name: &str,
) -> Option<&'static WayfireOption> {
  let section = table_section(section);
  wayfire_options()
    .iter()
    .find(|option| option.section == section && option.name == name)
}

/// The table section of a file section: `output` for `output:DP-1`, the
/// section itself otherwise.
fn table_section(section: &str) -> &str {
  match section.split_once(':') {
    Some((object, _)) if OBJECTS.contains(&object) => object,
    _ => section,
  }
}

/// The value of one option, as `wayfire/get-config-option` reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionValue {
  /// An option with one value: the text the file gave it, or the
  /// default.
  Text {
    /// The value's text.
    value: String,
    /// Wayfire's default.
    default: &'static str,
  },
  /// A dynamic list: one tuple per entry, the entry's name followed by
  /// the value of each of its options, as Wayfire's IPC reports compound
  /// options.
  List(Vec<Vec<String>>),
}

/// What a Wayfire file set, by section, for the options of the table.
///
/// Sections are lower case, except that object instances keep the case
/// of their name (`output:DP-1`). Options are kept as the file wrote them,
/// after wf-config's line rules; an option whose value Shingle could not
/// use is left out, so that it reports Wayfire's default as wf-config
/// does. A configuration that is not a Wayfire import has none, and every
/// option reports its default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WayfireValues {
  /// Each section's options in file order. A later line for the same
  /// option overrides an earlier one.
  sections: BTreeMap<String, Vec<(String, String)>>,
  /// Options whose value Shingle rejected, which keep their default.
  rejected: BTreeSet<(String, String)>,
}

impl WayfireValues {
  /// Records that `section` set `option` to `value`.
  pub(crate) fn set(&mut self, section: &str, option: &str, value: &str) {
    let entries = self.sections.entry(section.to_owned()).or_default();
    entries.retain(|(name, _)| name != option);
    entries.push((option.to_owned(), value.to_owned()));
    self
      .rejected
      .remove(&(section.to_owned(), option.to_owned()));
  }

  /// Records that the value of `option` in `section` could not be used.
  pub(crate) fn reject(&mut self, section: &str, option: &str) {
    self
      .rejected
      .insert((section.to_owned(), option.to_owned()));
  }

  /// The text `section` gave `option`, unless it was rejected.
  fn text(&self, section: &str, option: &str) -> Option<&str> {
    if self
      .rejected
      .contains(&(section.to_owned(), option.to_owned()))
    {
      return None;
    }
    self
      .sections
      .get(section)?
      .iter()
      .find(|(name, _)| name == option)
      .map(|(_, value)| value.as_str())
  }

  /// The value of `option` in `section`, or `None` when the table has no
  /// such option. `section` may be an object instance from the file,
  /// such as `output:DP-1`.
  pub fn value(&self, section: &str, option: &str) -> Option<OptionValue> {
    let table = wayfire_option(section, option)?;
    if !table.is_list() {
      return Some(OptionValue::Text {
        value: self
          .text(section, option)
          .unwrap_or(table.default)
          .to_owned(),
        default: table.default,
      });
    }
    // wf-config's compound options: every name that each entry prefix
    // completes to an option of the section, in file order.
    let entries = self.sections.get(section).map_or(&[][..], Vec::as_slice);
    let first = table.entries.first().copied().unwrap_or_default();
    let mut tuples = Vec::new();
    for (name, _) in entries {
      let Some(suffix) = name.strip_prefix(first) else {
        continue;
      };
      let values = table
        .entries
        .iter()
        .map(|prefix| self.text(section, &format!("{prefix}{suffix}")))
        .collect::<Option<Vec<_>>>();
      if let Some(values) = values {
        let mut tuple = vec![suffix.to_owned()];
        tuple.extend(values.into_iter().map(str::to_owned));
        tuples.push(tuple);
      }
    }
    Some(OptionValue::List(tuples))
  }

  /// Every section `list-config-options` reports: each section of the
  /// table, and each object instance the file has, in order.
  pub fn sections(&self) -> Vec<String> {
    let mut sections = wayfire_options()
      .iter()
      .map(|option| option.section.to_owned())
      .collect::<BTreeSet<_>>();
    sections.extend(
      self
        .sections
        .keys()
        .filter(|section| section.as_str() != table_section(section))
        .cloned(),
    );
    sections.into_iter().collect()
  }

  /// The table's options of `section`, which may be an object instance.
  pub fn options(
    section: &str,
  ) -> impl Iterator<Item = &'static WayfireOption> + '_ {
    let table = table_section(section);
    wayfire_options()
      .iter()
      .filter(move |option| option.section == table)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_table_holds_every_option_of_the_pinned_metadata() {
    let options = wayfire_options();
    assert_eq!(options.len(), 332);
    let vwidth = wayfire_option("core", "vwidth").unwrap();
    assert_eq!((vwidth.kind, vwidth.default), ("int", "3"));
    let bindings = wayfire_option("command", "bindings").unwrap();
    assert!(bindings.is_list());
    assert_eq!(bindings.entries, ["command_", "binding_"]);
    assert_eq!(
      wayfire_option("autostart", "autostart").unwrap().entries,
      [""]
    );
    assert_eq!(
      wayfire_option("output:DP-1", "mode").unwrap().default,
      "auto"
    );
    assert!(wayfire_option("core", "nonsense").is_none());
    let mut sorted = options.to_vec();
    sorted.sort_by_key(|option| (option.section, option.name));
    assert_eq!(sorted, options);
  }

  #[test]
  fn values_report_the_file_or_the_default() {
    let mut values = WayfireValues::default();
    values.set("core", "vwidth", "4");
    values.set("core", "vheight", "x");
    values.reject("core", "vheight");
    values.set("core", "vwidth", "5");
    assert_eq!(
      values.value("core", "vwidth"),
      Some(OptionValue::Text {
        value: "5".into(),
        default: "3",
      })
    );
    assert_eq!(
      values.value("core", "vheight"),
      Some(OptionValue::Text {
        value: "3".into(),
        default: "3",
      })
    );
    assert_eq!(values.value("core", "nonsense"), None);
  }

  #[test]
  fn dynamic_lists_pair_their_entries_by_name() {
    // In the order of the first entry's options, here `command_`.
    let mut values = WayfireValues::default();
    values.set("command", "binding_b", "<super> KEY_B");
    values.set("command", "command_a", "run-a");
    values.set("command", "binding_a", "<super> KEY_A");
    values.set("command", "command_b", "run-b");
    values.set("command", "repeatable_binding_c", "KEY_C");
    values.set("command", "command_c", "run-c");
    let tuple = |items: &[&str]| {
      items
        .iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
    };
    assert_eq!(
      values.value("command", "bindings"),
      Some(OptionValue::List(vec![
        tuple(&["a", "run-a", "<super> KEY_A"]),
        tuple(&["b", "run-b", "<super> KEY_B"]),
      ]))
    );
    assert_eq!(
      values.value("command", "repeatable_bindings"),
      Some(OptionValue::List(vec![tuple(&["c", "run-c", "KEY_C"])]))
    );
    values.set("autostart", "panel", "true");
    assert_eq!(
      values.value("autostart", "autostart"),
      Some(OptionValue::List(vec![tuple(&["panel", "true"])]))
    );
  }

  #[test]
  fn object_instances_are_sections_of_their_own() {
    let mut values = WayfireValues::default();
    values.set("output:DP-1", "mode", "1920x1080");
    let sections = values.sections();
    assert!(sections.contains(&"output:DP-1".to_owned()));
    assert!(sections.contains(&"output".to_owned()));
    assert!(sections.contains(&"simple-tile".to_owned()));
    assert_eq!(
      values.value("output:DP-1", "mode"),
      Some(OptionValue::Text {
        value: "1920x1080".into(),
        default: "auto",
      })
    );
    assert_eq!(
      WayfireValues::options("output:DP-1").count(),
      WayfireValues::options("output").count()
    );
  }
}
