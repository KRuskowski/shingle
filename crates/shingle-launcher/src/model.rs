//! The launcher's state and what keys and pointer events do to it, without
//! Wayland or a GPU (docs/design/launcher.md, section 10).

use std::path::PathBuf;

use crate::{
  catalog::{Catalog, CategoryKey},
  history::History,
  index::{Entry, EntryKind, Index},
  layout::{Layout, LayoutStore},
};

/// The category the launcher opens on, unless the user has favourites.
const FALLBACK_CATEGORY: CategoryKey = CategoryKey::All;
/// How far the pointer moves with a button held before a press is a drag.
pub const DRAG_THRESHOLD: f32 = 6.0;

/// A key, after the keymap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
  /// Text typed without Ctrl or Alt.
  Text(String),
  /// A letter, digit or other character pressed with Ctrl or Alt, in
  /// lower case.
  Char(char),
  /// Return or keypad Enter.
  Enter,
  /// Escape.
  Escape,
  /// Backspace.
  Backspace,
  /// Delete.
  Delete,
  /// The arrow keys.
  Up,
  /// Down arrow.
  Down,
  /// Left arrow.
  Left,
  /// Right arrow.
  Right,
  /// Page Up.
  PageUp,
  /// Page Down.
  PageDown,
  /// Home.
  Home,
  /// End.
  End,
  /// Tab; Shift+Tab when [`Modifiers::shift`] is set.
  Tab,
  /// F2.
  F2,
}

/// The modifiers held with a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
  /// Control.
  pub ctrl: bool,
  /// Shift.
  pub shift: bool,
  /// Alt.
  pub alt: bool,
}

impl Modifiers {
  /// Control alone.
  pub const CTRL: Self = Self {
    ctrl: true,
    shift: false,
    alt: false,
  };
  /// None.
  pub const NONE: Self = Self {
    ctrl: false,
    shift: false,
    alt: false,
  };
}

/// What the caller must do after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
  /// Nothing changed.
  None,
  /// Draw a new frame.
  Redraw,
  /// Run this and exit.
  Launch(Launch),
  /// Exit without launching.
  Close,
}

/// A command to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
  /// The entry launched, for the history; `None` for typed text.
  pub id: Option<String>,
  /// The `/bin/sh` command line.
  pub command: String,
  /// Whether it runs in a terminal.
  pub terminal: bool,
}

/// What a prompt asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptKind {
  /// A new name for the entry.
  Rename(String),
  /// A new name for the named category.
  RenameCategory(String),
  /// The command of a new shortcut.
  NewCommand,
  /// The name of a new shortcut running the command.
  NewName(String),
  /// The command of the shortcut.
  EditCommand(String),
  /// The category to move the entry to.
  MoveTo(String),
}

impl PromptKind {
  /// The label shown before the field.
  pub fn label(&self) -> &'static str {
    match self {
      Self::Rename(_) => "Rename to:",
      Self::RenameCategory(_) => "Rename category to:",
      Self::NewCommand => "Shortcut command:",
      Self::NewName(_) => "Shortcut name:",
      Self::EditCommand(_) => "Command:",
      Self::MoveTo(_) => "Move to category:",
    }
  }
}

/// An open prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
  /// What it asks for.
  pub kind: PromptKind,
  /// What was typed so far.
  pub text: String,
}

/// What a context menu item does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
  /// Adds the entry to the favourites or removes it.
  ToggleFavourite,
  /// Opens the rename prompt.
  Rename,
  /// Opens the move prompt.
  MoveTo,
  /// Hides the entry or shows it again.
  ToggleHidden,
  /// Opens the command prompt of a shortcut.
  EditCommand,
  /// Removes a shortcut.
  Remove,
  /// Opens the category rename prompt.
  RenameCategory,
  /// Removes a listed category.
  RemoveCategory,
  /// Moves the category up.
  CategoryUp,
  /// Moves the category down.
  CategoryDown,
}

/// What a context menu is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuTarget {
  /// The entry with this ID.
  Entry(String),
  /// The named category.
  Category(String),
}

/// An open context menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
  /// What it is for.
  pub target: MenuTarget,
  /// Its items, with their labels.
  pub items: Vec<(MenuAction, String)>,
  /// Where it opened, in panel coordinates.
  pub at: (f32, f32),
  /// The highlighted item.
  pub selected: usize,
}

/// A row of the entry list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
  /// The entry at this position of [`Catalog::entries`].
  Entry(usize),
  /// "Run" the typed text.
  Run,
}

/// What a point of the panel is, as the view lays it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
  /// Outside the panel.
  Backdrop,
  /// Inside the panel, on nothing else.
  Panel,
  /// The search field.
  Field,
  /// The category at this index of [`Catalog::categories`].
  Category(usize),
  /// The row at this index of [`Model::rows`].
  Row(usize),
  /// The context menu item at this index.
  MenuItem(usize),
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
  /// The primary button.
  Left,
  /// The secondary button, for the context menu.
  Right,
  /// Anything else.
  Other,
}

/// An entry being dragged.
#[derive(Clone, Debug, PartialEq)]
pub struct Drag {
  /// The dragged entry's ID.
  pub id: String,
  /// Its shown name, drawn under the pointer.
  pub name: String,
  /// Where the pointer is.
  pub at: (f32, f32),
  /// What it is over.
  pub over: Target,
}

/// A press that may become a click or a drag.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Press {
  at: (f32, f32),
  target: Target,
}

/// Where edits and launches are written; `None` writes nothing.
#[derive(Clone, Debug, Default)]
pub struct Storage {
  /// The layout file.
  pub layout: Option<LayoutStore>,
  /// The history file.
  pub history: Option<PathBuf>,
}

/// The launcher's state.
pub struct Model {
  index: Index,
  layout: Layout,
  history: History,
  storage: Storage,
  now: u64,
  /// What is shown, rebuilt after every edit.
  pub catalog: Catalog,
  /// The search text.
  pub query: String,
  /// The selected category, an index of [`Catalog::categories`].
  pub category: usize,
  /// The selected row, an index of [`Self::rows`].
  pub selected: usize,
  /// The first row shown.
  pub scroll: usize,
  /// How many rows fit; set by the view.
  pub page: usize,
  /// The open prompt.
  pub prompt: Option<Prompt>,
  /// The open context menu.
  pub menu: Option<Menu>,
  /// The entry being dragged.
  pub drag: Option<Drag>,
  /// What the pointer is over.
  pub hover: Target,
  /// A message for the status line, such as a failed write.
  pub status: String,
  results: Vec<usize>,
  press: Option<Press>,
}

impl Model {
  /// The launcher over `index`, `layout` and `history` at `now` (Unix
  /// seconds), writing to `storage`.
  pub fn new(
    index: Index,
    layout: Layout,
    history: History,
    storage: Storage,
    now: u64,
  ) -> Self {
    let catalog = Catalog::build(&index, &layout, &history);
    let favourites = &catalog.categories[0];
    let start = if favourites.entries.is_empty() {
      FALLBACK_CATEGORY
    } else {
      CategoryKey::Favourites
    };
    let category = catalog.category_index(&start).unwrap_or(0);
    Self {
      index,
      layout,
      history,
      storage,
      now,
      catalog,
      query: String::new(),
      category,
      selected: 0,
      scroll: 0,
      page: 10,
      prompt: None,
      menu: None,
      drag: None,
      hover: Target::Panel,
      status: String::new(),
      results: Vec::new(),
      press: None,
    }
  }

  /// The layout as edited so far.
  pub fn layout(&self) -> &Layout {
    &self.layout
  }

  /// The history, with this run's launch once there was one.
  pub fn history(&self) -> &History {
    &self.history
  }

  /// Whether the list shows search results.
  pub fn searching(&self) -> bool {
    !self.query.is_empty()
  }

  /// The rows of the entry list: search results and Run, or the selected
  /// category's entries.
  pub fn rows(&self) -> Vec<Row> {
    if self.searching() {
      let mut rows = self
        .results
        .iter()
        .map(|position| Row::Entry(*position))
        .collect::<Vec<_>>();
      rows.push(Row::Run);
      rows
    } else {
      self.catalog.categories[self.category]
        .entries
        .iter()
        .map(|position| Row::Entry(*position))
        .collect()
    }
  }

  /// The entry of row `row`, if it is one.
  pub fn row_entry(&self, row: usize) -> Option<&Entry> {
    match self.rows().get(row) {
      Some(Row::Entry(position)) => Some(&self.catalog.entries[*position]),
      _ => None,
    }
  }

  fn selected_position(&self) -> Option<usize> {
    match self.rows().get(self.selected) {
      Some(Row::Entry(position)) => Some(*position),
      _ => None,
    }
  }

  /// The selected entry.
  pub fn selected_entry(&self) -> Option<&Entry> {
    self.row_entry(self.selected)
  }

  fn selected_key(&self) -> CategoryKey {
    self.catalog.categories[self.category].key.clone()
  }

  /// Replaces the index, keeping the selection where it can; for the
  /// background rescan.
  pub fn set_index(&mut self, index: Index) {
    self.index = index;
    self.rebuild(None);
  }

  /// Rebuilds the catalog after a change, keeping the selected category by
  /// key and selecting `select` (an entry ID) or the entry selected before.
  fn rebuild(&mut self, select: Option<&str>) {
    let key = self.selected_key();
    let selected = select
      .map(str::to_owned)
      .or_else(|| self.selected_entry().map(|entry| entry.id.clone()));
    self.catalog = Catalog::build(&self.index, &self.layout, &self.history);
    self.category = self
      .catalog
      .category_index(&key)
      .unwrap_or_else(|| self.category.min(self.catalog.categories.len() - 1));
    self.refresh_results();
    if let Some(id) = selected
      && let Some(row) = self.rows().iter().position(|row| match row {
        Row::Entry(position) => self.catalog.entries[*position].id == id,
        Row::Run => false,
      })
    {
      self.selected = row;
    }
    self.clamp_selection();
  }

  fn refresh_results(&mut self) {
    self.results = if self.searching() {
      self.catalog.search(&self.query, &self.history, self.now)
    } else {
      Vec::new()
    };
  }

  fn clamp_selection(&mut self) {
    let count = self.rows().len();
    self.selected = self.selected.min(count.saturating_sub(1));
    let page = self.page.max(1);
    if self.selected < self.scroll {
      self.scroll = self.selected;
    } else if self.selected >= self.scroll + page {
      self.scroll = self.selected + 1 - page;
    }
    self.scroll = self.scroll.min(count.saturating_sub(page));
  }

  /// Saves the layout and rebuilds; a failed write is shown in the status
  /// line.
  fn edited(&mut self, select: Option<&str>) -> Effect {
    if let Some(store) = &self.storage.layout
      && let Err(error) = store.save(&self.layout)
    {
      self.status = format!("cannot save {}: {error}", store.path().display());
    }
    self.rebuild(select);
    Effect::Redraw
  }

  fn set_query(&mut self, query: String) -> Effect {
    self.query = query;
    self.selected = 0;
    self.scroll = 0;
    self.refresh_results();
    Effect::Redraw
  }

  fn select_category(&mut self, index: usize) -> Effect {
    self.category = index.min(self.catalog.categories.len() - 1);
    self.query.clear();
    self.results.clear();
    self.selected = 0;
    self.scroll = 0;
    Effect::Redraw
  }

  fn step_category(&mut self, forward: bool) -> Effect {
    let count = self.catalog.categories.len();
    let next = if forward {
      (self.category + 1) % count
    } else {
      (self.category + count - 1) % count
    };
    self.select_category(next)
  }

  fn move_selection(&mut self, delta: isize) -> Effect {
    let count = self.rows().len();
    if count == 0 {
      return Effect::None;
    }
    self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
    self.clamp_selection();
    Effect::Redraw
  }

  /// Scrolls the list by `rows`.
  pub fn scroll_by(&mut self, rows: isize) -> Effect {
    let count = self.rows().len();
    let page = self.page.max(1);
    let scroll = self
      .scroll
      .saturating_add_signed(rows)
      .min(count.saturating_sub(page));
    if scroll == self.scroll {
      return Effect::None;
    }
    self.scroll = scroll;
    self.selected = self.selected.clamp(scroll, scroll + page - 1);
    self.clamp_selection();
    Effect::Redraw
  }

  /// What activating row `row` does: launch its entry or run the query.
  fn activate(&mut self, row: usize) -> Effect {
    match self.rows().get(row) {
      Some(Row::Entry(position)) => {
        let entry = &self.catalog.entries[*position];
        let launch = Launch {
          id: Some(entry.id.clone()),
          command: entry.command.clone(),
          terminal: entry.terminal,
        };
        self.record(&entry.id.clone());
        Effect::Launch(launch)
      }
      Some(Row::Run) => self.run_query(),
      None => Effect::None,
    }
  }

  fn run_query(&mut self) -> Effect {
    let command = self.query.trim();
    if command.is_empty() {
      return Effect::None;
    }
    Effect::Launch(Launch {
      id: None,
      command: command.to_owned(),
      terminal: false,
    })
  }

  fn record(&mut self, id: &str) {
    self.history.record(id, self.now);
    if let Some(path) = &self.storage.history
      && let Err(error) = self.history.save(path)
    {
      eprintln!("shingle-launcher: cannot save {}: {error}", path.display());
    }
  }

  /// Handles a key.
  pub fn key(&mut self, key: Key, modifiers: Modifiers) -> Effect {
    if self.drag.is_some() {
      if key == Key::Escape {
        self.drag = None;
        self.press = None;
        return Effect::Redraw;
      }
      return Effect::None;
    }
    if self.menu.is_some() {
      return self.menu_key(key);
    }
    if self.prompt.is_some() {
      return self.prompt_key(key, modifiers);
    }
    self.status.clear();
    match (key, modifiers.ctrl) {
      (Key::Text(text), false) => {
        let query = format!("{}{text}", self.query);
        self.set_query(query)
      }
      (Key::Backspace, false) => {
        if self.query.is_empty() {
          return Effect::None;
        }
        let mut query = self.query.clone();
        query.pop();
        self.set_query(query)
      }
      (Key::Backspace, true) => {
        let query = without_last_word(&self.query);
        self.set_query(query)
      }
      (Key::Char('u'), true) => self.set_query(String::new()),
      (Key::Enter, false) => self.activate(self.selected),
      (Key::Enter, true) => self.run_query(),
      (Key::Escape, _) => {
        if self.searching() {
          self.set_query(String::new())
        } else {
          Effect::Close
        }
      }
      (Key::Up, false) => self.move_selection(-1),
      (Key::Down, false) => self.move_selection(1),
      (Key::PageUp, _) => self.move_selection(-(self.page.max(1) as isize)),
      (Key::PageDown, _) => self.move_selection(self.page.max(1) as isize),
      (Key::Home, _) => self.move_selection(isize::MIN / 2),
      (Key::End, _) => self.move_selection(isize::MAX / 2),
      (Key::Tab, _) => self.step_category(!modifiers.shift),
      (Key::Left, false) if !self.searching() => self.step_category(false),
      (Key::Right, false) if !self.searching() => self.step_category(true),
      (Key::Up, true) if modifiers.shift => self.move_category(-1),
      (Key::Down, true) if modifiers.shift => self.move_category(1),
      (Key::Up, true) => self.reorder(-1),
      (Key::Down, true) => self.reorder(1),
      (Key::Char('d'), true) => self.toggle_favourite(),
      (Key::Char('h'), true) => self.toggle_hidden(),
      (Key::Char('n'), true) => self.new_shortcut(),
      (Key::Char('e'), true) => self.open_command_prompt(),
      (Key::Char('m'), true) => self.open_move_prompt(),
      (Key::F2, _) if modifiers.shift => self.open_category_rename(),
      (Key::F2, _) => self.open_rename(),
      (Key::Delete, _) => self.remove_shortcut(),
      _ => Effect::None,
    }
  }

  fn open_prompt(&mut self, kind: PromptKind, text: String) -> Effect {
    self.prompt = Some(Prompt { kind, text });
    Effect::Redraw
  }

  fn open_rename(&mut self) -> Effect {
    let Some(position) = self.selected_position() else {
      return Effect::None;
    };
    let id = self.catalog.entries[position].id.clone();
    let name = self.catalog.names[position].clone();
    self.open_prompt(PromptKind::Rename(id), name)
  }

  fn open_category_rename(&mut self) -> Effect {
    match self.selected_key() {
      CategoryKey::Named(name) => {
        self.open_prompt(PromptKind::RenameCategory(name.clone()), name)
      }
      _ => self.note("Only named categories can be renamed"),
    }
  }

  fn open_command_prompt(&mut self) -> Effect {
    match self.selected_entry() {
      Some(entry) if entry.kind == EntryKind::Shortcut => {
        let (id, command) = (entry.id.clone(), entry.command.clone());
        self.open_prompt(PromptKind::EditCommand(id), command)
      }
      _ => self.note("Only shortcuts have a command to edit"),
    }
  }

  fn open_move_prompt(&mut self) -> Effect {
    let Some(position) = self.selected_position() else {
      return Effect::None;
    };
    let id = self.catalog.entries[position].id.clone();
    let current = self.catalog.category_of(position).unwrap_or("").to_owned();
    self.open_prompt(PromptKind::MoveTo(id), current)
  }

  fn new_shortcut(&mut self) -> Effect {
    let command = self.query.trim().to_owned();
    if command.is_empty() {
      return self.open_prompt(PromptKind::NewCommand, String::new());
    }
    let name = command
      .split_whitespace()
      .next()
      .unwrap_or_default()
      .to_owned();
    self.open_prompt(PromptKind::NewName(command), name)
  }

  fn note(&mut self, message: &str) -> Effect {
    self.status = message.to_owned();
    Effect::Redraw
  }

  fn prompt_key(&mut self, key: Key, modifiers: Modifiers) -> Effect {
    let Some(prompt) = &mut self.prompt else {
      return Effect::None;
    };
    match (key, modifiers.ctrl) {
      (Key::Text(text), false) => prompt.text.push_str(&text),
      (Key::Backspace, false) => {
        prompt.text.pop();
      }
      (Key::Backspace, true) => prompt.text = without_last_word(&prompt.text),
      (Key::Char('u'), true) => prompt.text.clear(),
      (Key::Escape, _) => self.prompt = None,
      (Key::Enter, _) => {
        let prompt = self.prompt.take().expect("a prompt is open");
        return self.accept(prompt);
      }
      _ => return Effect::None,
    }
    Effect::Redraw
  }

  /// Applies an accepted prompt.
  fn accept(&mut self, prompt: Prompt) -> Effect {
    let text = prompt.text.trim().to_owned();
    match prompt.kind {
      PromptKind::Rename(id) => {
        let original = self.original_name(&id);
        self.layout.rename(&id, &text, &original);
        self.edited(Some(&id))
      }
      PromptKind::RenameCategory(old) => {
        let key = CategoryKey::Named(old.clone());
        let members = self.catalog.member_ids(&key);
        let order = self.catalog.named_order();
        if !self.layout.rename_category(&old, &text, &members, &order) {
          return Effect::Redraw;
        }
        let effect = self.edited(None);
        if let Some(index) =
          self.catalog.category_index(&CategoryKey::Named(text))
        {
          self.category = index;
          self.clamp_selection();
        }
        effect
      }
      PromptKind::NewCommand => {
        if text.is_empty() {
          return Effect::Redraw;
        }
        let name = text.split_whitespace().next().unwrap_or("").to_owned();
        self.open_prompt(PromptKind::NewName(text), name)
      }
      PromptKind::NewName(command) => self.add_shortcut(&text, &command),
      PromptKind::EditCommand(id) => {
        if text.is_empty() {
          return Effect::Redraw;
        }
        self.layout.set_shortcut_command(&id, &text);
        self.edited(Some(&id))
      }
      PromptKind::MoveTo(id) => self.move_to(&id, &text, None),
    }
  }

  fn original_name(&self, id: &str) -> String {
    self
      .catalog
      .position(id)
      .map(|position| self.catalog.entries[position].name.clone())
      .unwrap_or_default()
  }

  fn add_shortcut(&mut self, name: &str, command: &str) -> Effect {
    let id = self.layout.add_shortcut(name, command);
    if !self.searching()
      && let CategoryKey::Named(category) = self.selected_key()
    {
      let members = self
        .catalog
        .member_ids(&CategoryKey::Named(category.clone()));
      let order = self.catalog.named_order();
      self.layout.place(&id, &category, None, &members, &order);
    }
    self.query.clear();
    let effect = self.edited(Some(&id));
    // Show the new shortcut in its category.
    if let Some(position) = self.catalog.position(&id)
      && let Some(category) = self.catalog.category_of(position)
      && let Some(index) = self
        .catalog
        .category_index(&CategoryKey::Named(category.to_owned()))
    {
      self.category = index;
      self.rebuild(Some(&id));
    }
    effect
  }

  /// Moves the entry `id` to the named category `name` at `index` (the
  /// end when `None`).
  fn move_to(&mut self, id: &str, name: &str, index: Option<usize>) -> Effect {
    let name = name.trim();
    if name.is_empty() {
      return Effect::Redraw;
    }
    let key = CategoryKey::Named(name.to_owned());
    let members = self.catalog.member_ids(&key);
    let order = self.catalog.named_order();
    self.layout.place(id, name, index, &members, &order);
    let searching = self.searching();
    let effect = self.edited(Some(id));
    if !searching && let Some(category) = self.catalog.category_index(&key) {
      self.category = category;
      self.rebuild(Some(id));
    }
    effect
  }

  fn toggle_favourite(&mut self) -> Effect {
    let Some(entry) = self.selected_entry() else {
      return Effect::None;
    };
    let id = entry.id.clone();
    self.layout.toggle_favourite(&id);
    self.edited(Some(&id))
  }

  fn toggle_hidden(&mut self) -> Effect {
    let Some(position) = self.selected_position() else {
      return Effect::None;
    };
    let id = self.catalog.entries[position].id.clone();
    let hidden = self.catalog.is_hidden(position);
    self.layout.set_hidden(&id, !hidden);
    self.edited(None)
  }

  fn remove_shortcut(&mut self) -> Effect {
    match self.selected_entry() {
      Some(entry) if entry.kind == EntryKind::Shortcut => {
        let id = entry.id.clone();
        self.layout.remove_shortcut(&id);
        self.edited(None)
      }
      _ => self.note("Only shortcuts can be removed; Ctrl+H hides"),
    }
  }

  /// Moves the selected entry by `delta` in its category or the
  /// favourites.
  fn reorder(&mut self, delta: isize) -> Effect {
    if self.searching() {
      return self.note("Reorder in a category, not in search results");
    }
    let Some(entry) = self.selected_entry() else {
      return Effect::None;
    };
    let id = entry.id.clone();
    let target = self.selected.saturating_add_signed(delta);
    if target >= self.rows().len() || target == self.selected {
      return Effect::None;
    }
    self.place_at(&id, target)
  }

  /// Puts the entry `id` at `index` of the selected category.
  fn place_at(&mut self, id: &str, index: usize) -> Effect {
    match self.selected_key() {
      CategoryKey::Favourites => {
        self.layout.place_favourite(id, index);
        self.edited(Some(id))
      }
      CategoryKey::Named(name) => {
        let key = CategoryKey::Named(name.clone());
        let members = self.catalog.member_ids(&key);
        let order = self.catalog.named_order();
        self.layout.place(id, &name, Some(index), &members, &order);
        self.edited(Some(id))
      }
      _ => self.note("This category is sorted by itself"),
    }
  }

  fn move_category(&mut self, delta: isize) -> Effect {
    let CategoryKey::Named(name) = self.selected_key() else {
      return self.note("Only named categories can be moved");
    };
    let order = self.catalog.named_order();
    self.layout.move_category(&name, delta, &order);
    let effect = self.edited(None);
    if let Some(index) = self.catalog.category_index(&CategoryKey::Named(name))
    {
      self.category = index;
    }
    effect
  }

  fn menu_key(&mut self, key: Key) -> Effect {
    let Some(menu) = &mut self.menu else {
      return Effect::None;
    };
    match key {
      Key::Escape => self.menu = None,
      Key::Up => menu.selected = menu.selected.saturating_sub(1),
      Key::Down => {
        menu.selected = (menu.selected + 1).min(menu.items.len() - 1)
      }
      Key::Enter => {
        let selected = menu.selected;
        return self.choose(selected);
      }
      _ => return Effect::None,
    }
    Effect::Redraw
  }

  /// Opens the context menu of `target` at `at`.
  fn open_menu(&mut self, target: Target, at: (f32, f32)) -> Effect {
    let menu = match target {
      Target::Row(row) => {
        let Some(Row::Entry(position)) = self.rows().get(row).copied() else {
          return Effect::None;
        };
        self.selected = row;
        let entry = &self.catalog.entries[position];
        let mut items = vec![
          (
            MenuAction::ToggleFavourite,
            if self.catalog.is_favourite(position) {
              "Remove from Favourites"
            } else {
              "Add to Favourites"
            },
          ),
          (MenuAction::Rename, "Rename…"),
          (MenuAction::MoveTo, "Move to category…"),
          (
            MenuAction::ToggleHidden,
            if self.catalog.is_hidden(position) {
              "Show"
            } else {
              "Hide"
            },
          ),
        ];
        if entry.kind == EntryKind::Shortcut {
          items.push((MenuAction::EditCommand, "Edit command…"));
          items.push((MenuAction::Remove, "Remove shortcut"));
        }
        Menu {
          target: MenuTarget::Entry(entry.id.clone()),
          items: own(items),
          at,
          selected: 0,
        }
      }
      Target::Category(index) => {
        let CategoryKey::Named(name) =
          self.catalog.categories[index].key.clone()
        else {
          return Effect::None;
        };
        self.select_category(index);
        let mut items = vec![
          (MenuAction::RenameCategory, "Rename…"),
          (MenuAction::CategoryUp, "Move up"),
          (MenuAction::CategoryDown, "Move down"),
        ];
        if self.layout.category(&name).is_some() {
          items.push((MenuAction::RemoveCategory, "Remove category"));
        }
        Menu {
          target: MenuTarget::Category(name),
          items: own(items),
          at,
          selected: 0,
        }
      }
      _ => return Effect::None,
    };
    self.menu = Some(menu);
    Effect::Redraw
  }

  /// Runs item `item` of the open menu.
  fn choose(&mut self, item: usize) -> Effect {
    let Some(menu) = self.menu.take() else {
      return Effect::None;
    };
    let Some((action, _)) = menu.items.get(item).cloned() else {
      return Effect::Redraw;
    };
    if let MenuTarget::Entry(id) = &menu.target
      && let Some(row) = self.rows().iter().position(|row| match row {
        Row::Entry(position) => &self.catalog.entries[*position].id == id,
        Row::Run => false,
      })
    {
      self.selected = row;
    }
    match action {
      MenuAction::ToggleFavourite => self.toggle_favourite(),
      MenuAction::Rename => self.open_rename(),
      MenuAction::MoveTo => self.open_move_prompt(),
      MenuAction::ToggleHidden => self.toggle_hidden(),
      MenuAction::EditCommand => self.open_command_prompt(),
      MenuAction::Remove => self.remove_shortcut(),
      MenuAction::RenameCategory => self.open_category_rename(),
      MenuAction::RemoveCategory => {
        if let MenuTarget::Category(name) = &menu.target {
          self.layout.remove_category(name);
        }
        self.edited(None)
      }
      MenuAction::CategoryUp => self.move_category(-1),
      MenuAction::CategoryDown => self.move_category(1),
    }
  }

  /// The pointer moved to `at` (panel coordinates), over `target`.
  pub fn pointer_motion(&mut self, at: (f32, f32), target: Target) -> Effect {
    let hover_changed = self.hover != target;
    self.hover = target;
    if let Some(drag) = &mut self.drag {
      drag.at = at;
      drag.over = target;
      return Effect::Redraw;
    }
    if let Some(menu) = &mut self.menu
      && let Target::MenuItem(item) = target
      && menu.selected != item
    {
      menu.selected = item;
      return Effect::Redraw;
    }
    if let Some(press) = self.press
      && let Target::Row(row) = press.target
      && distance(press.at, at) > DRAG_THRESHOLD
      && self.menu.is_none()
      && self.prompt.is_none()
      && let Some(Row::Entry(position)) = self.rows().get(row).copied()
    {
      self.selected = row;
      self.drag = Some(Drag {
        id: self.catalog.entries[position].id.clone(),
        name: self.catalog.names[position].clone(),
        at,
        over: target,
      });
      return Effect::Redraw;
    }
    if hover_changed {
      Effect::Redraw
    } else {
      Effect::None
    }
  }

  /// A button went down (`pressed`) or up at `at`, over `target`.
  pub fn pointer_button(
    &mut self,
    button: Button,
    pressed: bool,
    at: (f32, f32),
    target: Target,
  ) -> Effect {
    if pressed {
      if self.menu.is_some() {
        if let Target::MenuItem(_) = target {
          return Effect::None;
        }
        self.menu = None;
        if target == Target::Backdrop {
          return Effect::Close;
        }
        return Effect::Redraw;
      }
      return match button {
        Button::Left => {
          self.press = Some(Press { at, target });
          Effect::None
        }
        Button::Right if self.prompt.is_none() => self.open_menu(target, at),
        _ => Effect::None,
      };
    }
    if button != Button::Left {
      return Effect::None;
    }
    let press = self.press.take();
    if let Some(drag) = self.drag.take() {
      return self.drop(&drag.id, target);
    }
    if self.menu.is_some()
      && let Target::MenuItem(item) = target
    {
      return self.choose(item);
    }
    // A click is a press and release on the same thing.
    if press.map(|press| press.target) != Some(target) {
      return Effect::None;
    }
    match target {
      Target::Backdrop => Effect::Close,
      Target::Category(index) => {
        self.prompt = None;
        self.select_category(index)
      }
      Target::Row(row) if self.prompt.is_none() => self.activate(row),
      _ => Effect::None,
    }
  }

  /// Drops the dragged entry `id` on `target`.
  fn drop(&mut self, id: &str, target: Target) -> Effect {
    match target {
      Target::Row(row) => {
        if self.searching() {
          return self.note("Drop on a category to move it there");
        }
        let row = row.min(self.rows().len().saturating_sub(1));
        self.place_at(id, row)
      }
      Target::Category(index) => {
        match self.catalog.categories[index].key.clone() {
          CategoryKey::Favourites => {
            let count = self.catalog.categories[index].entries.len();
            self.layout.place_favourite(id, count);
            self.edited(Some(id))
          }
          CategoryKey::Named(name) => self.move_to(id, &name, None),
          CategoryKey::Hidden => {
            self.layout.set_hidden(id, true);
            self.edited(None)
          }
          _ => Effect::Redraw,
        }
      }
      _ => Effect::Redraw,
    }
  }
}

fn own(items: Vec<(MenuAction, &str)>) -> Vec<(MenuAction, String)> {
  items
    .into_iter()
    .map(|(action, label)| (action, label.to_owned()))
    .collect()
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
  ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// `text` without its last word and the spaces after it.
fn without_last_word(text: &str) -> String {
  let trimmed = text.trim_end();
  match trimmed.rfind(char::is_whitespace) {
    Some(index) => trimmed[..=index].to_owned(),
    None => String::new(),
  }
}

#[cfg(test)]
mod tests;
