//! The user's layout: favourites, categories, renames, hidden entries and
//! shortcuts, stored as one YAML file (docs/design/launcher.md, section 7).
//!
//! The file holds only what the user changed. Entry IDs the index no
//! longer has are kept, so that reinstalling an application brings its
//! place back.

use std::{
  collections::BTreeMap,
  fs, io,
  path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::files::write_atomically;

/// The version of the layout file.
pub const LAYOUT_VERSION: u32 = 1;
/// The prefix of shortcut IDs.
pub const SHORTCUT_PREFIX: &str = "shortcut:";
const HEADER: &str =
  "# shingle-launcher layout; edited by the launcher, safe to edit by hand.\n";

/// The layout file's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
  /// [`LAYOUT_VERSION`].
  pub version: u32,
  /// Favourite entry IDs, in the order shown.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub favourites: Vec<String>,
  /// Categories in the order shown, each with the entries placed in it.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub categories: Vec<CategoryLayout>,
  /// Shown names by entry ID.
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  pub renamed: BTreeMap<String, String>,
  /// Entries shown only in the Hidden category.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub hidden: Vec<String>,
  /// The user's own entries.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub shortcuts: Vec<Shortcut>,
}

/// One category of the layout.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryLayout {
  /// Its shown name.
  pub name: String,
  /// Entries placed in it, in the order shown, ahead of its default
  /// members.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub entries: Vec<String>,
}

/// A shortcut the user added.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shortcut {
  /// `shortcut:SLUG`.
  pub id: String,
  /// Its name.
  pub name: String,
  /// The `/bin/sh` command it runs.
  pub command: String,
  /// Whether it runs in a terminal.
  #[serde(default, skip_serializing_if = "is_false")]
  pub terminal: bool,
}

fn is_false(value: &bool) -> bool {
  !value
}

impl Layout {
  /// Whether `id` is a favourite.
  pub fn is_favourite(&self, id: &str) -> bool {
    self.favourites.iter().any(|favourite| favourite == id)
  }

  /// Adds `id` to the end of the favourites or removes it; returns whether
  /// it is a favourite now.
  pub fn toggle_favourite(&mut self, id: &str) -> bool {
    if self.is_favourite(id) {
      self.favourites.retain(|favourite| favourite != id);
      false
    } else {
      self.favourites.push(id.to_owned());
      true
    }
  }

  /// Makes `id` a favourite at `index` (clamped) of the favourites.
  pub fn place_favourite(&mut self, id: &str, index: usize) {
    self.favourites.retain(|favourite| favourite != id);
    let index = index.min(self.favourites.len());
    self.favourites.insert(index, id.to_owned());
  }

  /// Whether `id` is hidden.
  pub fn is_hidden(&self, id: &str) -> bool {
    self.hidden.iter().any(|hidden| hidden == id)
  }

  /// Hides `id` or shows it again.
  pub fn set_hidden(&mut self, id: &str, hidden: bool) {
    self.hidden.retain(|entry| entry != id);
    if hidden {
      self.hidden.push(id.to_owned());
    }
  }

  /// Shows `id` as `name`. An empty name, or `original` (its name before
  /// renames), removes the rename. A shortcut's own name changes instead.
  pub fn rename(&mut self, id: &str, name: &str, original: &str) {
    let name = name.trim();
    if let Some(shortcut) = self.shortcut_mut(id) {
      if !name.is_empty() {
        shortcut.name = name.to_owned();
      }
      return;
    }
    if name.is_empty() || name == original {
      self.renamed.remove(id);
    } else {
      self.renamed.insert(id.to_owned(), name.to_owned());
    }
  }

  /// The shortcut `id`.
  pub fn shortcut(&self, id: &str) -> Option<&Shortcut> {
    self.shortcuts.iter().find(|shortcut| shortcut.id == id)
  }

  fn shortcut_mut(&mut self, id: &str) -> Option<&mut Shortcut> {
    self.shortcuts.iter_mut().find(|shortcut| shortcut.id == id)
  }

  /// Adds a shortcut named `name` (the command, when empty) running
  /// `command`; returns its new, unique ID.
  pub fn add_shortcut(&mut self, name: &str, command: &str) -> String {
    let name = if name.trim().is_empty() {
      command.trim()
    } else {
      name.trim()
    };
    let slug = slug(name);
    let mut id = format!("{SHORTCUT_PREFIX}{slug}");
    let mut number = 2;
    while self.shortcut(&id).is_some() {
      id = format!("{SHORTCUT_PREFIX}{slug}-{number}");
      number += 1;
    }
    self.shortcuts.push(Shortcut {
      id: id.clone(),
      name: name.to_owned(),
      command: command.trim().to_owned(),
      terminal: false,
    });
    id
  }

  /// Changes the command of the shortcut `id`; returns whether it exists.
  pub fn set_shortcut_command(&mut self, id: &str, command: &str) -> bool {
    match self.shortcut_mut(id) {
      Some(shortcut) => {
        shortcut.command = command.trim().to_owned();
        true
      }
      None => false,
    }
  }

  /// Removes the shortcut `id` and every mention of it; returns whether it
  /// existed.
  pub fn remove_shortcut(&mut self, id: &str) -> bool {
    let before = self.shortcuts.len();
    self.shortcuts.retain(|shortcut| shortcut.id != id);
    if self.shortcuts.len() == before {
      return false;
    }
    self.favourites.retain(|favourite| favourite != id);
    self.hidden.retain(|hidden| hidden != id);
    self.renamed.remove(id);
    for category in &mut self.categories {
      category.entries.retain(|entry| entry != id);
    }
    true
  }

  /// The listed category `name`.
  pub fn category(&self, name: &str) -> Option<&CategoryLayout> {
    self
      .categories
      .iter()
      .find(|category| category.name == name)
  }

  /// The category `id` is placed in, if any.
  pub fn placed_in(&self, id: &str) -> Option<&str> {
    self
      .categories
      .iter()
      .find(|category| category.entries.iter().any(|entry| entry == id))
      .map(|category| category.name.as_str())
  }

  /// Lists every category of `shown_order` (the categories as shown now)
  /// that is not listed yet, in that order, so that listing one more keeps
  /// the order the user sees.
  fn list_categories(&mut self, shown_order: &[String]) {
    let mut listed = Vec::new();
    for name in shown_order {
      let category = match self.categories.iter().position(|c| &c.name == name)
      {
        Some(index) => self.categories.remove(index),
        None => CategoryLayout {
          name: name.clone(),
          entries: Vec::new(),
        },
      };
      listed.push(category);
    }
    // Listed categories that are not shown (empty, or only of missing
    // entries) keep their place after the shown ones.
    listed.append(&mut self.categories);
    self.categories = listed;
  }

  /// Places `id` in `category` at `index` (clamped; the end when `None`).
  /// `members` are the category's entries as shown now, and `shown_order`
  /// the categories as shown now; the category's full order is written, and
  /// `id` leaves any other category. A new name creates the category after
  /// the shown ones.
  pub fn place(
    &mut self,
    id: &str,
    category: &str,
    index: Option<usize>,
    members: &[String],
    shown_order: &[String],
  ) {
    let name = category.trim();
    if name.is_empty() {
      return;
    }
    self.list_categories(shown_order);
    for listed in &mut self.categories {
      listed.entries.retain(|entry| entry != id);
    }
    let mut entries = members
      .iter()
      .filter(|member| member.as_str() != id)
      .cloned()
      .collect::<Vec<_>>();
    let index = index.unwrap_or(entries.len()).min(entries.len());
    entries.insert(index, id.to_owned());
    match self.categories.iter_mut().find(|c| c.name == name) {
      Some(listed) => listed.entries = entries,
      None => self.categories.push(CategoryLayout {
        name: name.to_owned(),
        entries,
      }),
    }
  }

  /// Renames the category `old` to `new`, keeping its place and writing
  /// its `members` (as shown now) under the new name; a category already
  /// called `new` takes them at its end. Returns whether anything changed.
  pub fn rename_category(
    &mut self,
    old: &str,
    new: &str,
    members: &[String],
    shown_order: &[String],
  ) -> bool {
    let new = new.trim();
    if new.is_empty() || new == old {
      return false;
    }
    self.list_categories(shown_order);
    let Some(index) = self.categories.iter().position(|c| c.name == old) else {
      return false;
    };
    let mut moved = self.categories.remove(index);
    moved.entries = members.to_vec();
    match self.categories.iter_mut().find(|c| c.name == new) {
      Some(existing) => {
        existing
          .entries
          .retain(|entry| !moved.entries.contains(entry));
        existing.entries.append(&mut moved.entries);
      }
      None => {
        moved.name = new.to_owned();
        self.categories.insert(index, moved);
      }
    }
    true
  }

  /// Removes the listed category `name`; its entries go back to their
  /// default categories. Returns whether it was listed.
  pub fn remove_category(&mut self, name: &str) -> bool {
    let before = self.categories.len();
    self.categories.retain(|category| category.name != name);
    self.categories.len() != before
  }

  /// Moves the category `name` by `delta` places in `shown_order`.
  pub fn move_category(
    &mut self,
    name: &str,
    delta: isize,
    shown_order: &[String],
  ) {
    self.list_categories(shown_order);
    let Some(index) = self.categories.iter().position(|c| c.name == name)
    else {
      return;
    };
    let shown = shown_order.len().min(self.categories.len());
    let target = index.saturating_add_signed(delta);
    if index < shown && target < shown && target != index {
      let category = self.categories.remove(index);
      self.categories.insert(target, category);
    }
  }
}

/// `name` in lower case with every run of other characters than letters
/// and digits made one `-`.
fn slug(name: &str) -> String {
  let mut slug = String::new();
  for character in name.chars().flat_map(char::to_lowercase) {
    if character.is_alphanumeric() {
      slug.push(character);
    } else if !slug.ends_with('-') && !slug.is_empty() {
      slug.push('-');
    }
  }
  let slug = slug.trim_end_matches('-').to_owned();
  if slug.is_empty() {
    "shortcut".to_owned()
  } else {
    slug
  }
}

/// The layout file and how it is read and written.
#[derive(Clone, Debug)]
pub struct LayoutStore {
  path: PathBuf,
}

impl LayoutStore {
  /// The store at `path`.
  pub fn new(path: PathBuf) -> Self {
    Self { path }
  }

  /// Where the file is.
  pub fn path(&self) -> &Path {
    &self.path
  }

  /// The layout, and a warning when the file could not be used. A missing
  /// file is an empty layout. A file that does not parse is moved aside to
  /// `launcher.yaml.bad`, so the next write does not lose it.
  pub fn load(&self) -> (Layout, Option<String>) {
    let text = match fs::read_to_string(&self.path) {
      Ok(text) => text,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return (Layout::default(), None);
      }
      Err(error) => {
        return (
          Layout::default(),
          Some(format!("cannot read {}: {error}", self.path.display())),
        );
      }
    };
    if text.trim().is_empty() {
      return (Layout::default(), None);
    }
    match serde_yaml::from_str::<Layout>(&text) {
      Ok(layout) => (layout, None),
      Err(error) => {
        let mut aside = self.path.as_os_str().to_owned();
        aside.push(".bad");
        let moved = fs::rename(&self.path, &aside);
        let what = match moved {
          Ok(()) => format!("moved to {}", Path::new(&aside).display()),
          Err(error) => format!("could not be moved aside: {error}"),
        };
        (
          Layout::default(),
          Some(format!("{}: {error}; {what}", self.path.display())),
        )
      }
    }
  }

  /// Writes `layout`, atomically.
  pub fn save(&self, layout: &Layout) -> io::Result<()> {
    let mut layout = layout.clone();
    layout.version = LAYOUT_VERSION;
    let yaml = serde_yaml::to_string(&layout).map_err(io::Error::other)?;
    write_atomically(&self.path, format!("{HEADER}{yaml}").as_bytes())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
  }

  #[test]
  fn favourites_toggle_and_reorder() {
    let mut layout = Layout::default();
    assert!(layout.toggle_favourite("a"));
    assert!(layout.toggle_favourite("b"));
    assert!(layout.toggle_favourite("c"));
    layout.place_favourite("c", 0);
    assert_eq!(layout.favourites, ["c", "a", "b"]);
    layout.place_favourite("c", 99);
    assert_eq!(layout.favourites, ["a", "b", "c"]);
    assert!(!layout.toggle_favourite("b"));
    assert_eq!(layout.favourites, ["a", "c"]);
  }

  #[test]
  fn renames_hide_and_reset() {
    let mut layout = Layout::default();
    layout.rename("ff", "  Web  ", "Firefox");
    assert_eq!(layout.renamed["ff"], "Web");
    layout.rename("ff", "Firefox", "Firefox");
    assert!(layout.renamed.is_empty());
    layout.rename("ff", "Web", "Firefox");
    layout.rename("ff", "", "Firefox");
    assert!(layout.renamed.is_empty());
    layout.set_hidden("ff", true);
    layout.set_hidden("ff", true);
    assert_eq!(layout.hidden, ["ff"]);
    layout.set_hidden("ff", false);
    assert!(!layout.is_hidden("ff"));
  }

  #[test]
  fn shortcuts_get_unique_ids_and_leave_no_trace() {
    let mut layout = Layout::default();
    let first = layout.add_shortcut("Back up home!", "rsync -a ~ /mnt ");
    let second = layout.add_shortcut("back-up  home", "true");
    let unnamed = layout.add_shortcut(" ", "wl_probe x");
    assert_eq!(first, "shortcut:back-up-home");
    assert_eq!(second, "shortcut:back-up-home-2");
    assert_eq!(unnamed, "shortcut:wl-probe-x");
    assert_eq!(layout.shortcut(&first).unwrap().command, "rsync -a ~ /mnt");
    assert_eq!(layout.shortcut(&unnamed).unwrap().name, "wl_probe x");
    layout.rename(&first, "Backup", "Back up home!");
    assert_eq!(layout.shortcut(&first).unwrap().name, "Backup");
    assert!(layout.renamed.is_empty());
    assert!(layout.set_shortcut_command(&first, "true"));
    layout.toggle_favourite(&first);
    layout.set_hidden(&first, true);
    layout.place(&first, "Work", None, &[], &[]);
    assert!(layout.remove_shortcut(&first));
    assert!(!layout.remove_shortcut(&first));
    assert!(layout.favourites.is_empty() && layout.hidden.is_empty());
    assert!(layout.categories[0].entries.is_empty());
  }

  #[test]
  fn placing_writes_the_category_order_and_keeps_the_shown_order() {
    let mut layout = Layout::default();
    let shown = strings(&["Games", "Internet", "Office"]);
    // Moving the browser into Office at the front lists every shown
    // category in its order and writes Office's whole order.
    layout.place("ff", "Office", Some(0), &strings(&["calc", "doc"]), &shown);
    assert_eq!(
      layout
        .categories
        .iter()
        .map(|category| category.name.as_str())
        .collect::<Vec<_>>(),
      ["Games", "Internet", "Office"]
    );
    assert_eq!(
      layout.category("Office").unwrap().entries,
      ["ff", "calc", "doc"]
    );
    assert_eq!(layout.placed_in("ff"), Some("Office"));
    // Reordering inside the category.
    layout.place(
      "ff",
      "Office",
      Some(2),
      &strings(&["ff", "calc", "doc"]),
      &shown,
    );
    assert_eq!(
      layout.category("Office").unwrap().entries,
      ["calc", "doc", "ff"]
    );
    // A new category goes after the shown ones, and the entry leaves Office.
    layout.place("ff", " Work ", None, &[], &shown);
    assert_eq!(layout.categories.last().unwrap().name, "Work");
    assert_eq!(layout.category("Office").unwrap().entries, ["calc", "doc"]);
    assert_eq!(layout.placed_in("ff"), Some("Work"));
  }

  #[test]
  fn categories_rename_merge_move_and_go() {
    let mut layout = Layout::default();
    let shown = strings(&["A", "B", "C"]);
    assert!(layout.rename_category("B", "Bee", &strings(&["x", "y"]), &shown));
    assert_eq!(layout.categories[1].name, "Bee");
    assert_eq!(layout.categories[1].entries, ["x", "y"]);
    let shown = strings(&["A", "Bee", "C"]);
    assert!(layout.rename_category("C", "Bee", &strings(&["z"]), &shown));
    assert_eq!(layout.category("Bee").unwrap().entries, ["x", "y", "z"]);
    assert!(layout.category("C").is_none());
    let shown = strings(&["A", "Bee"]);
    layout.move_category("Bee", -1, &shown);
    assert_eq!(layout.categories[0].name, "Bee");
    layout.move_category("Bee", -1, &shown);
    assert_eq!(layout.categories[0].name, "Bee");
    assert!(layout.remove_category("Bee"));
    assert!(!layout.remove_category("Bee"));
    assert!(!layout.rename_category("A", " ", &[], &shown));
  }

  #[test]
  fn the_store_round_trips_and_sets_bad_files_aside() {
    let directory = tempfile::tempdir().unwrap();
    let store =
      LayoutStore::new(directory.path().join("shingle/launcher.yaml"));
    assert_eq!(store.load(), (Layout::default(), None));
    let mut layout = Layout::default();
    layout.toggle_favourite("firefox.desktop");
    layout.rename("firefox.desktop", "Web", "Firefox");
    let id = layout.add_shortcut("Backup", "rsync -a ~/ /mnt/");
    layout.place(&id, "Work", None, &[], &[]);
    store.save(&layout).unwrap();
    let text = fs::read_to_string(store.path()).unwrap();
    assert!(text.starts_with("# shingle-launcher layout"), "{text}");
    assert!(text.contains("firefox.desktop: Web"), "{text}");
    assert!(!text.contains("hidden"), "empty lists are left out: {text}");
    let (loaded, warning) = store.load();
    assert_eq!(warning, None);
    assert_eq!(loaded.favourites, layout.favourites);
    assert_eq!(loaded.shortcuts, layout.shortcuts);
    assert_eq!(loaded.version, LAYOUT_VERSION);
    fs::write(store.path(), "favourites: {broken").unwrap();
    let (loaded, warning) = store.load();
    assert_eq!(loaded, Layout::default());
    assert!(warning.unwrap().contains("moved to"));
    assert!(directory.path().join("shingle/launcher.yaml.bad").exists());
    assert!(!store.path().exists());
  }

  #[test]
  fn a_hand_written_file_reads() {
    let text = "favourites: [a]\ncategories:\n  - name: Work\nhidden: [b]\n";
    let layout = serde_yaml::from_str::<Layout>(text).unwrap();
    assert_eq!(layout.categories[0].entries, Vec::<String>::new());
    assert_eq!(layout.hidden, ["b"]);
  }
}
