//! What the launcher shows: the index and the user's shortcuts with the
//! layout applied, as categories of entries.

use std::collections::{HashMap, HashSet};

use crate::{
  history::History,
  index::{Entry, EntryKind, Index},
  layout::Layout,
  search::{Candidate, rank},
};

/// The default category of a shortcut not placed anywhere.
pub const SHORTCUTS: &str = "Shortcuts";
/// How many entries the Recent category shows.
pub const RECENT_LIMIT: usize = 20;
/// How many results a search shows.
pub const RESULT_LIMIT: usize = 100;

/// Which category a [`Category`] is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CategoryKey {
  /// The favourites, in the user's order.
  Favourites,
  /// The most recently launched entries.
  Recent,
  /// A category by name: a listed one or a default one.
  Named(String),
  /// Every shown entry except commands on `$PATH`.
  All,
  /// The hidden entries.
  Hidden,
}

/// A category and its entries, as shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
  /// Which it is.
  pub key: CategoryKey,
  /// Its shown title.
  pub title: String,
  /// Positions in [`Catalog::entries`], in the order shown.
  pub entries: Vec<usize>,
}

/// The entries and categories the launcher shows.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
  /// The index's entries, then the shortcuts.
  pub entries: Vec<Entry>,
  /// The shown name of each entry, after renames.
  pub names: Vec<String>,
  /// The categories, in the order shown.
  pub categories: Vec<Category>,
  hidden: Vec<bool>,
  favourite: Vec<bool>,
  positions: HashMap<String, usize>,
}

impl Catalog {
  /// The catalog of `index` with `layout` applied and Recent from
  /// `history`.
  pub fn build(index: &Index, layout: &Layout, history: &History) -> Self {
    let mut entries = index.entries.clone();
    entries.extend(layout.shortcuts.iter().map(|shortcut| Entry {
      id: shortcut.id.clone(),
      kind: EntryKind::Shortcut,
      name: shortcut.name.clone(),
      generic_name: String::new(),
      comment: shortcut.command.clone(),
      keywords: Vec::new(),
      program: String::new(),
      command: shortcut.command.clone(),
      terminal: shortcut.terminal,
      category: SHORTCUTS.to_owned(),
    }));
    let positions = entries
      .iter()
      .enumerate()
      .map(|(position, entry)| (entry.id.clone(), position))
      .collect::<HashMap<_, _>>();
    let names = entries
      .iter()
      .map(|entry| {
        layout
          .renamed
          .get(&entry.id)
          .cloned()
          .unwrap_or_else(|| entry.name.clone())
      })
      .collect::<Vec<_>>();
    let hidden = entries
      .iter()
      .map(|entry| layout.is_hidden(&entry.id))
      .collect::<Vec<_>>();
    let favourite = entries
      .iter()
      .map(|entry| layout.is_favourite(&entry.id))
      .collect::<Vec<_>>();
    let mut catalog = Self {
      entries,
      names,
      categories: Vec::new(),
      hidden,
      favourite,
      positions,
    };
    catalog.categories = catalog.categorize(layout, history);
    catalog
  }

  fn categorize(&self, layout: &Layout, history: &History) -> Vec<Category> {
    let shown = |position: &usize| !self.hidden[*position];
    let lookup = |ids: &mut dyn Iterator<Item = &str>| {
      let mut seen = HashSet::new();
      ids
        .filter_map(|id| self.position(id))
        .filter(shown)
        .filter(|position| seen.insert(*position))
        .collect::<Vec<_>>()
    };
    let mut categories = vec![Category {
      key: CategoryKey::Favourites,
      title: "Favourites".to_owned(),
      entries: lookup(&mut layout.favourites.iter().map(String::as_str)),
    }];
    let mut recent = lookup(&mut history.recent().into_iter());
    recent.truncate(RECENT_LIMIT);
    categories.push(Category {
      key: CategoryKey::Recent,
      title: "Recent".to_owned(),
      entries: recent,
    });
    // Placed entries first, in the listed order, then the default members
    // not placed anywhere, by name.
    let mut placed = HashSet::new();
    let mut named = Vec::new();
    for listed in &layout.categories {
      let entries = lookup(
        &mut listed
          .entries
          .iter()
          .map(String::as_str)
          .filter(|id| placed.insert((*id).to_owned())),
      );
      named.push((listed.name.clone(), entries));
    }
    let mut defaults: Vec<(String, Vec<usize>)> = Vec::new();
    for position in self.sorted(0..self.entries.len()) {
      let entry = &self.entries[position];
      if self.hidden[position]
        || entry.category.is_empty()
        || placed.contains(&entry.id)
      {
        continue;
      }
      match named.iter_mut().find(|(name, _)| *name == entry.category) {
        Some((_, entries)) => entries.push(position),
        None => match defaults.iter_mut().find(|(n, _)| *n == entry.category) {
          Some((_, entries)) => entries.push(position),
          None => defaults.push((entry.category.clone(), vec![position])),
        },
      }
    }
    defaults.sort_by_key(|(name, _)| name.to_lowercase());
    categories.extend(named.into_iter().chain(defaults).map(
      |(title, entries)| Category {
        key: CategoryKey::Named(title.clone()),
        title,
        entries,
      },
    ));
    categories.push(Category {
      key: CategoryKey::All,
      title: "All".to_owned(),
      entries: self
        .sorted(0..self.entries.len())
        .into_iter()
        .filter(|position| {
          !self.hidden[*position]
            && self.entries[*position].kind != EntryKind::Command
        })
        .collect(),
    });
    let hidden =
      self.sorted((0..self.entries.len()).filter(|p| self.hidden[*p]));
    if !hidden.is_empty() {
      categories.push(Category {
        key: CategoryKey::Hidden,
        title: "Hidden".to_owned(),
        entries: hidden,
      });
    }
    categories
  }

  /// `positions` sorted by shown name, then ID.
  fn sorted(&self, positions: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut positions = positions.collect::<Vec<_>>();
    positions.sort_by_cached_key(|position| {
      (
        self.names[*position].to_lowercase(),
        self.entries[*position].id.clone(),
      )
    });
    positions
  }

  /// The position of the entry `id`.
  pub fn position(&self, id: &str) -> Option<usize> {
    self.positions.get(id).copied()
  }

  /// Whether the entry at `position` is hidden.
  pub fn is_hidden(&self, position: usize) -> bool {
    self.hidden[position]
  }

  /// Whether the entry at `position` is a favourite.
  pub fn is_favourite(&self, position: usize) -> bool {
    self.favourite[position]
  }

  /// The position in [`Self::categories`] of the category with `key`.
  pub fn category_index(&self, key: &CategoryKey) -> Option<usize> {
    self
      .categories
      .iter()
      .position(|category| &category.key == key)
  }

  /// The names of the named categories, in the order shown.
  pub fn named_order(&self) -> Vec<String> {
    self
      .categories
      .iter()
      .filter_map(|category| match &category.key {
        CategoryKey::Named(name) => Some(name.clone()),
        _ => None,
      })
      .collect()
  }

  /// The IDs of the entries of the category with `key`, as shown.
  pub fn member_ids(&self, key: &CategoryKey) -> Vec<String> {
    self
      .category_index(key)
      .map(|index| {
        self.categories[index]
          .entries
          .iter()
          .map(|position| self.entries[*position].id.clone())
          .collect()
      })
      .unwrap_or_default()
  }

  /// The named category the entry at `position` is shown in, if any.
  pub fn category_of(&self, position: usize) -> Option<&str> {
    self
      .categories
      .iter()
      .find_map(|category| match &category.key {
        CategoryKey::Named(name) if category.entries.contains(&position) => {
          Some(name.as_str())
        }
        _ => None,
      })
  }

  /// The positions of the best matches of `query` among the shown
  /// entries, best first.
  pub fn search(&self, query: &str, history: &History, now: u64) -> Vec<usize> {
    let shown = (0..self.entries.len())
      .filter(|position| !self.hidden[*position])
      .collect::<Vec<_>>();
    let candidates = shown
      .iter()
      .map(|position| Candidate {
        entry: &self.entries[*position],
        name: &self.names[*position],
      })
      .collect::<Vec<_>>();
    rank(query, &candidates, history, now, RESULT_LIMIT)
      .into_iter()
      .map(|hit| shown[hit.index])
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(id: &str, kind: EntryKind, name: &str, category: &str) -> Entry {
    Entry {
      id: id.to_owned(),
      kind,
      name: name.to_owned(),
      generic_name: String::new(),
      comment: String::new(),
      keywords: Vec::new(),
      program: name.to_lowercase(),
      command: name.to_lowercase(),
      terminal: false,
      category: category.to_owned(),
    }
  }

  fn index() -> Index {
    Index {
      version: 1,
      entries: vec![
        entry("ff", EntryKind::Application, "Firefox", "Internet"),
        entry("chat", EntryKind::Application, "Chat", "Internet"),
        entry("calc", EntryKind::Application, "Calc", "Office"),
        entry("doom", EntryKind::Application, "Doom", "Games"),
        entry("path:htop", EntryKind::Command, "htop", ""),
      ],
    }
  }

  fn shown(catalog: &Catalog) -> Vec<(String, Vec<String>)> {
    catalog
      .categories
      .iter()
      .map(|category| {
        (
          category.title.clone(),
          category
            .entries
            .iter()
            .map(|position| catalog.names[*position].clone())
            .collect(),
        )
      })
      .collect()
  }

  fn row(title: &str, names: &[&str]) -> (String, Vec<String>) {
    (
      title.to_owned(),
      names.iter().map(|name| (*name).to_owned()).collect(),
    )
  }

  #[test]
  fn without_a_layout_categories_are_the_defaults_by_name() {
    let catalog =
      Catalog::build(&index(), &Layout::default(), &History::default());
    assert_eq!(
      shown(&catalog),
      [
        row("Favourites", &[]),
        row("Recent", &[]),
        row("Games", &["Doom"]),
        row("Internet", &["Chat", "Firefox"]),
        row("Office", &["Calc"]),
        row("All", &["Calc", "Chat", "Doom", "Firefox"]),
      ]
    );
    assert_eq!(
      catalog.category_of(catalog.position("ff").unwrap()),
      Some("Internet")
    );
    assert_eq!(catalog.named_order(), ["Games", "Internet", "Office"]);
  }

  #[test]
  fn the_layout_places_renames_hides_and_adds_shortcuts() {
    let mut layout = Layout::default();
    let backup = layout.add_shortcut("Backup", "rsync");
    layout.toggle_favourite("doom");
    layout.toggle_favourite("missing.desktop");
    layout.toggle_favourite(&backup);
    layout.rename("ff", "Web", "Firefox");
    layout.set_hidden("chat", true);
    let order = ["Games", "Internet", "Office"].map(str::to_owned);
    layout.place("ff", "Office", Some(0), &["calc".to_owned()], &order);
    let mut history = History::default();
    history.record("path:htop", 5);
    history.record("calc", 9);
    let catalog = Catalog::build(&index(), &layout, &history);
    assert_eq!(
      shown(&catalog),
      [
        row("Favourites", &["Doom", "Backup"]),
        row("Recent", &["Calc", "htop"]),
        row("Games", &["Doom"]),
        row("Internet", &[]),
        row("Office", &["Web", "Calc"]),
        row("Shortcuts", &["Backup"]),
        row("All", &["Backup", "Calc", "Doom", "Web"]),
        row("Hidden", &["Chat"]),
      ]
    );
    let ff = catalog.position("ff").unwrap();
    assert!(!catalog.is_favourite(ff));
    assert!(catalog.is_hidden(catalog.position("chat").unwrap()));
    assert_eq!(
      catalog.member_ids(&CategoryKey::Named("Office".into())),
      ["ff", "calc"]
    );
  }

  #[test]
  fn search_covers_commands_and_renames_but_not_hidden_entries() {
    let mut layout = Layout::default();
    layout.rename("calc", "Spreadsheet", "Calc");
    layout.set_hidden("doom", true);
    let catalog = Catalog::build(&index(), &layout, &History::default());
    let names = |query: &str| {
      catalog
        .search(query, &History::default(), 0)
        .into_iter()
        .map(|position| catalog.names[position].clone())
        .collect::<Vec<_>>()
    };
    assert_eq!(names("htop"), ["htop"]);
    assert_eq!(names("sheet"), ["Spreadsheet"]);
    assert!(names("doom").is_empty());
  }
}
