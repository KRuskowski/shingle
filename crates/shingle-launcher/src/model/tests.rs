use super::*;
use crate::index::EntryKind;

fn entry(id: &str, kind: EntryKind, name: &str, category: &str) -> Entry {
  Entry {
    id: id.to_owned(),
    kind,
    name: name.to_owned(),
    generic_name: String::new(),
    comment: String::new(),
    keywords: Vec::new(),
    program: name.to_lowercase(),
    command: format!("exec {}", name.to_lowercase()),
    terminal: false,
    category: category.to_owned(),
    icon: String::new(),
    actions: Vec::new(),
  }
}

fn index() -> Index {
  Index {
    version: 1,
    entries: vec![
      entry("ff", EntryKind::Application, "Firefox", "Internet"),
      entry("chat", EntryKind::Application, "Chat", "Internet"),
      entry("calc", EntryKind::Application, "Calc", "Office"),
      entry("write", EntryKind::Application, "Writer", "Office"),
      entry("doom", EntryKind::Application, "Doom", "Games"),
      entry("path:htop", EntryKind::Command, "htop", ""),
    ],
  }
}

struct Fixture {
  _directory: tempfile::TempDir,
  model: Model,
  store: LayoutStore,
  history: PathBuf,
}

fn fixture() -> Fixture {
  let directory = tempfile::tempdir().unwrap();
  let store = LayoutStore::new(directory.path().join("launcher.yaml"));
  let history = directory.path().join("history.json");
  let model = Model::new(
    index(),
    Layout::default(),
    History::default(),
    Storage {
      layout: Some(store.clone()),
      history: Some(history.clone()),
    },
    1_000_000,
  );
  Fixture {
    _directory: directory,
    model,
    store,
    history,
  }
}

fn typed(model: &mut Model, text: &str) {
  for character in text.chars() {
    model.key(Key::Text(character.to_string()), Modifiers::NONE);
  }
}

fn ctrl(model: &mut Model, character: char) -> Effect {
  model.key(Key::Char(character), Modifiers::CTRL)
}

fn press(model: &mut Model, key: Key) -> Effect {
  model.key(key, Modifiers::NONE)
}

fn row_names(model: &Model) -> Vec<String> {
  model
    .rows()
    .iter()
    .map(|row| match row {
      Row::Entry(position) => model.catalog.names[*position].clone(),
      Row::Action(position, action) => format!(
        "{} > {}",
        model.catalog.names[*position],
        model.catalog.entries[*position].actions[*action].name
      ),
      Row::Run => "<run>".to_owned(),
    })
    .collect()
}

fn select_category(model: &mut Model, key: CategoryKey) {
  let index = model.catalog.category_index(&key).unwrap();
  model.select_category(index);
}

fn named(name: &str) -> CategoryKey {
  CategoryKey::Named(name.to_owned())
}

#[test]
fn opens_on_all_and_typing_launches_the_best_match() {
  let Fixture {
    mut model, history, ..
  } = fixture();
  assert_eq!(model.selected_key(), CategoryKey::All);
  assert_eq!(
    row_names(&model),
    ["Calc", "Chat", "Doom", "Firefox", "Writer"]
  );
  typed(&mut model, "fir");
  assert_eq!(row_names(&model), ["Firefox", "<run>"]);
  let effect = press(&mut model, Key::Enter);
  assert_eq!(
    effect,
    Effect::Launch(Launch {
      id: Some("ff".into()),
      command: "exec firefox".into(),
      terminal: false,
    })
  );
  assert_eq!(History::load(&history).entries["ff"].count, 1);
}

#[test]
fn the_run_row_and_ctrl_enter_run_the_typed_text() {
  let Fixture { mut model, .. } = fixture();
  typed(&mut model, "echo hi");
  assert_eq!(row_names(&model), ["<run>"]);
  let run = Effect::Launch(Launch {
    id: None,
    command: "echo hi".into(),
    terminal: false,
  });
  assert_eq!(press(&mut model, Key::Enter), run);
  typed(&mut model, "");
  assert_eq!(model.key(Key::Enter, Modifiers::CTRL), run);
}

#[test]
fn escape_clears_then_closes_and_backspace_edits() {
  let Fixture { mut model, .. } = fixture();
  typed(&mut model, "doom x");
  press(&mut model, Key::Backspace);
  assert_eq!(model.query, "doom ");
  ctrl(&mut model, 'u');
  assert_eq!(model.query, "");
  typed(&mut model, "one two");
  model.key(Key::Backspace, Modifiers::CTRL);
  assert_eq!(model.query, "one ");
  assert_eq!(press(&mut model, Key::Escape), Effect::Redraw);
  assert_eq!(model.query, "");
  assert_eq!(press(&mut model, Key::Escape), Effect::Close);
}

#[test]
fn arrows_tab_and_pages_move_the_selection_and_category() {
  let Fixture { mut model, .. } = fixture();
  model.page = 2;
  press(&mut model, Key::Down);
  press(&mut model, Key::Down);
  assert_eq!(model.selected, 2);
  assert_eq!(model.scroll, 1);
  press(&mut model, Key::End);
  assert_eq!(model.selected, 4);
  press(&mut model, Key::Home);
  assert_eq!((model.selected, model.scroll), (0, 0));
  press(&mut model, Key::Tab);
  assert_eq!(model.catalog.categories[model.category].title, "Favourites");
  model.key(
    Key::Tab,
    Modifiers {
      shift: true,
      ..Modifiers::NONE
    },
  );
  assert_eq!(model.selected_key(), CategoryKey::All);
  press(&mut model, Key::Left);
  assert_eq!(model.selected_key(), named("Office"));
  assert_eq!(row_names(&model), ["Calc", "Writer"]);
  assert_eq!(model.scroll_by(1), Effect::None);
}

#[test]
fn ctrl_d_favourites_and_the_launcher_then_opens_on_them() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  typed(&mut model, "doom");
  assert_eq!(ctrl(&mut model, 'd'), Effect::Redraw);
  assert_eq!(store.load().0.favourites, ["doom"]);
  assert_eq!(model.selected_entry().unwrap().id, "doom");
  let reopened = Model::new(
    index(),
    store.load().0,
    History::default(),
    Storage::default(),
    0,
  );
  assert_eq!(reopened.selected_key(), CategoryKey::Favourites);
  assert_eq!(row_names(&reopened), ["Doom"]);
  ctrl(&mut model, 'd');
  assert!(store.load().0.favourites.is_empty());
}

#[test]
fn f2_renames_in_a_prompt_and_escape_cancels() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  typed(&mut model, "firefox");
  press(&mut model, Key::F2);
  let prompt = model.prompt.clone().unwrap();
  assert_eq!(prompt.kind, PromptKind::Rename("ff".into()));
  assert_eq!(prompt.text, "Firefox");
  ctrl(&mut model, 'u');
  typed(&mut model, "Web");
  assert_eq!(model.query, "firefox", "typing goes to the prompt");
  press(&mut model, Key::Enter);
  assert!(model.prompt.is_none());
  assert_eq!(store.load().0.renamed["ff"], "Web");
  press(&mut model, Key::Escape);
  typed(&mut model, "web");
  assert_eq!(row_names(&model)[0], "Web");
  press(&mut model, Key::F2);
  typed(&mut model, "ignored");
  press(&mut model, Key::Escape);
  assert!(model.prompt.is_none());
  assert_eq!(store.load().0.renamed["ff"], "Web");
}

#[test]
fn ctrl_n_adds_a_shortcut_from_the_typed_text_into_the_category() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  typed(&mut model, "rsync -a ~ /mnt");
  ctrl(&mut model, 'n');
  let prompt = model.prompt.clone().unwrap();
  assert_eq!(prompt.kind, PromptKind::NewName("rsync -a ~ /mnt".into()));
  assert_eq!(prompt.text, "rsync");
  ctrl(&mut model, 'u');
  typed(&mut model, "Backup");
  press(&mut model, Key::Enter);
  let layout = store.load().0;
  assert_eq!(layout.shortcuts[0].name, "Backup");
  assert_eq!(layout.shortcuts[0].command, "rsync -a ~ /mnt");
  assert_eq!(model.query, "");
  assert_eq!(model.selected_key(), named("Shortcuts"));
  assert_eq!(model.selected_entry().unwrap().name, "Backup");
  // In a named category the shortcut goes there; with an empty query the
  // command is asked for first.
  select_category(&mut model, named("Office"));
  ctrl(&mut model, 'n');
  assert_eq!(model.prompt.as_ref().unwrap().kind, PromptKind::NewCommand);
  typed(&mut model, "libreoffice --calc");
  press(&mut model, Key::Enter);
  assert_eq!(model.prompt.as_ref().unwrap().text, "libreoffice");
  press(&mut model, Key::Enter);
  assert_eq!(model.selected_key(), named("Office"));
  assert_eq!(row_names(&model), ["Calc", "Writer", "libreoffice"]);
  assert_eq!(
    store
      .load()
      .0
      .category("Office")
      .unwrap()
      .entries
      .last()
      .unwrap(),
    "shortcut:libreoffice"
  );
  // The shortcut launches its command; Ctrl+E edits it and Delete goes.
  press(&mut model, Key::End);
  ctrl(&mut model, 'e');
  ctrl(&mut model, 'u');
  typed(&mut model, "true");
  press(&mut model, Key::Enter);
  assert_eq!(
    press(&mut model, Key::Enter),
    Effect::Launch(Launch {
      id: Some("shortcut:libreoffice".into()),
      command: "true".into(),
      terminal: false
    })
  );
  press(&mut model, Key::Delete);
  assert_eq!(row_names(&model), ["Calc", "Writer"]);
  assert_eq!(store.load().0.shortcuts.len(), 1);
}

#[test]
fn ctrl_m_moves_to_a_category_and_ctrl_arrows_reorder() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  typed(&mut model, "firefox");
  ctrl(&mut model, 'm');
  assert_eq!(model.prompt.as_ref().unwrap().text, "Internet");
  ctrl(&mut model, 'u');
  typed(&mut model, "Office");
  press(&mut model, Key::Enter);
  press(&mut model, Key::Escape);
  select_category(&mut model, named("Office"));
  assert_eq!(row_names(&model), ["Calc", "Writer", "Firefox"]);
  press(&mut model, Key::End);
  model.key(Key::Up, Modifiers::CTRL);
  model.key(Key::Up, Modifiers::CTRL);
  assert_eq!(row_names(&model), ["Firefox", "Calc", "Writer"]);
  assert_eq!(model.selected, 0);
  assert_eq!(
    store.load().0.category("Office").unwrap().entries,
    ["ff", "calc", "write"]
  );
  assert_eq!(model.key(Key::Up, Modifiers::CTRL), Effect::None);
  // A new category name creates it.
  ctrl(&mut model, 'm');
  ctrl(&mut model, 'u');
  typed(&mut model, "Web");
  press(&mut model, Key::Enter);
  assert_eq!(model.selected_key(), named("Web"));
  assert_eq!(row_names(&model), ["Firefox"]);
}

#[test]
fn ctrl_h_hides_into_hidden_and_shows_again() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  typed(&mut model, "chat");
  ctrl(&mut model, 'h');
  assert_eq!(row_names(&model), ["<run>"]);
  press(&mut model, Key::Escape);
  select_category(&mut model, CategoryKey::Hidden);
  assert_eq!(row_names(&model), ["Chat"]);
  ctrl(&mut model, 'h');
  assert!(store.load().0.hidden.is_empty());
  assert!(model.catalog.category_index(&CategoryKey::Hidden).is_none());
}

#[test]
fn categories_rename_and_move_by_keyboard() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  select_category(&mut model, named("Office"));
  model.key(
    Key::F2,
    Modifiers {
      shift: true,
      ..Modifiers::NONE
    },
  );
  ctrl(&mut model, 'u');
  typed(&mut model, "Work");
  press(&mut model, Key::Enter);
  assert_eq!(model.selected_key(), named("Work"));
  assert_eq!(row_names(&model), ["Calc", "Writer"]);
  let up = Modifiers {
    ctrl: true,
    shift: true,
    alt: false,
  };
  model.key(Key::Up, up);
  assert_eq!(model.catalog.named_order(), ["Games", "Work", "Internet"]);
  assert_eq!(model.selected_key(), named("Work"));
  assert_eq!(
    store
      .load()
      .0
      .categories
      .iter()
      .map(|category| category.name.clone())
      .collect::<Vec<_>>(),
    ["Games", "Work", "Internet"]
  );
}

/// The row of the entry named `name`.
fn row_of(model: &Model, name: &str) -> usize {
  row_names(model).iter().position(|row| row == name).unwrap()
}

#[test]
fn a_click_launches_and_a_click_outside_closes() {
  let Fixture { mut model, .. } = fixture();
  let row = Target::Row(row_of(&model, "Doom"));
  assert_eq!(
    model.pointer_button(Button::Left, true, (10.0, 10.0), row),
    Effect::None
  );
  let effect = model.pointer_button(Button::Left, false, (11.0, 10.0), row);
  let Effect::Launch(launch) = effect else {
    panic!("{effect:?}");
  };
  assert_eq!(launch.id.as_deref(), Some("doom"));
  model.pointer_button(Button::Left, true, (0.0, 0.0), Target::Backdrop);
  assert_eq!(
    model.pointer_button(Button::Left, false, (0.0, 0.0), Target::Backdrop),
    Effect::Close
  );
  let office = model.catalog.category_index(&named("Office")).unwrap();
  model.pointer_button(
    Button::Left,
    true,
    (0.0, 0.0),
    Target::Category(office),
  );
  model.pointer_button(
    Button::Left,
    false,
    (0.0, 0.0),
    Target::Category(office),
  );
  assert_eq!(model.selected_key(), named("Office"));
}

#[test]
fn the_context_menu_favourites_renames_and_closes() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  let row = Target::Row(row_of(&model, "Calc"));
  model.pointer_button(Button::Right, true, (50.0, 60.0), row);
  let menu = model.menu.clone().unwrap();
  assert_eq!(menu.at, Some((50.0, 60.0)));
  assert_eq!(menu.items[0].1, "Add to Favourites");
  assert_eq!(menu.items.len(), 4);
  model.pointer_motion((55.0, 65.0), Target::MenuItem(0));
  model.pointer_button(Button::Left, false, (55.0, 65.0), Target::MenuItem(0));
  assert!(model.menu.is_none());
  assert_eq!(store.load().0.favourites, ["calc"]);
  model.pointer_button(Button::Right, true, (50.0, 60.0), row);
  assert_eq!(
    model.menu.as_ref().unwrap().items[0].1,
    "Remove from Favourites"
  );
  press(&mut model, Key::Down);
  press(&mut model, Key::Enter);
  assert_eq!(
    model.prompt.as_ref().unwrap().kind,
    PromptKind::Rename("calc".into())
  );
  press(&mut model, Key::Escape);
  model.pointer_button(Button::Right, true, (50.0, 60.0), row);
  assert_eq!(
    model.pointer_button(Button::Left, true, (0.0, 0.0), Target::Panel),
    Effect::Redraw
  );
  assert!(model.menu.is_none());
  let games = model.catalog.category_index(&named("Games")).unwrap();
  model.pointer_button(
    Button::Right,
    true,
    (5.0, 5.0),
    Target::Category(games),
  );
  let items = &model.menu.as_ref().unwrap().items;
  assert_eq!(items[0].0, MenuAction::RenameCategory);
  assert_eq!(items.len(), 3, "a default category cannot be removed");
}

#[test]
fn dragging_reorders_moves_and_favourites() {
  let Fixture {
    mut model, store, ..
  } = fixture();
  select_category(&mut model, named("Office"));
  let writer = Target::Row(row_of(&model, "Writer"));
  model.pointer_button(Button::Left, true, (100.0, 100.0), writer);
  model.pointer_motion((100.0, 103.0), writer);
  assert!(model.drag.is_none(), "below the threshold");
  model.pointer_motion((100.0, 80.0), Target::Row(0));
  assert_eq!(model.drag.as_ref().unwrap().name, "Writer");
  model.pointer_button(Button::Left, false, (100.0, 80.0), Target::Row(0));
  assert_eq!(row_names(&model), ["Writer", "Calc"]);
  assert_eq!(
    store.load().0.category("Office").unwrap().entries,
    ["write", "calc"]
  );
  // Onto another category, and onto Favourites.
  let games = model.catalog.category_index(&named("Games")).unwrap();
  model.pointer_button(Button::Left, true, (100.0, 100.0), Target::Row(1));
  model.pointer_motion((10.0, 100.0), Target::Category(games));
  model.pointer_button(
    Button::Left,
    false,
    (10.0, 100.0),
    Target::Category(games),
  );
  assert_eq!(store.load().0.placed_in("calc"), Some("Games"));
  assert_eq!(model.selected_key(), named("Games"));
  assert_eq!(row_names(&model), ["Doom", "Calc"]);
  let favourites = model
    .catalog
    .category_index(&CategoryKey::Favourites)
    .unwrap();
  model.pointer_button(Button::Left, true, (100.0, 100.0), Target::Row(0));
  model.pointer_motion((10.0, 10.0), Target::Category(favourites));
  model.pointer_button(
    Button::Left,
    false,
    (10.0, 10.0),
    Target::Category(favourites),
  );
  assert_eq!(store.load().0.favourites, ["doom"]);
  // Escape cancels a drag.
  model.pointer_button(Button::Left, true, (100.0, 100.0), Target::Row(0));
  model.pointer_motion((10.0, 10.0), Target::Category(favourites));
  press(&mut model, Key::Escape);
  assert!(model.drag.is_none());
  assert_eq!(
    model.pointer_button(
      Button::Left,
      false,
      (10.0, 10.0),
      Target::Category(favourites)
    ),
    Effect::None
  );
}

#[test]
fn a_new_index_keeps_the_selection() {
  let Fixture { mut model, .. } = fixture();
  press(&mut model, Key::Down);
  assert_eq!(model.selected_entry().unwrap().id, "chat");
  let mut index = index();
  index
    .entries
    .insert(0, entry("a", EntryKind::Application, "Aardvark", "Games"));
  model.set_index(index);
  assert_eq!(model.selected_entry().unwrap().id, "chat");
  assert_eq!(row_names(&model)[0], "Aardvark");
}

#[test]
fn removing_the_last_word() {
  assert_eq!(without_last_word("one two  "), "one ");
  assert_eq!(without_last_word("one"), "");
}

/// The fixture with a "Private Window" action on Firefox.
fn with_action() -> Fixture {
  let mut fixture = fixture();
  let mut index = fixture.model.index().clone();
  index.entries[0].actions = vec![crate::index::EntryAction {
    id: "private".into(),
    name: "Private Window".into(),
    icon: String::new(),
    command: "exec firefox --private-window".into(),
  }];
  fixture.model.set_index(index);
  fixture
}

#[test]
fn desktop_actions_are_search_results_that_launch() {
  let Fixture {
    mut model, history, ..
  } = with_action();
  typed(&mut model, "fire");
  assert_eq!(row_names(&model), ["Firefox", "<run>"]);
  press(&mut model, Key::Escape);
  typed(&mut model, "priv");
  assert_eq!(row_names(&model), ["Firefox > Private Window", "<run>"]);
  // Edits are for entries, not their actions.
  assert_eq!(ctrl(&mut model, 'd'), Effect::None);
  assert_eq!(
    press(&mut model, Key::Enter),
    Effect::Launch(Launch {
      id: Some("ff#private".into()),
      command: "exec firefox --private-window".into(),
      terminal: false,
    })
  );
  assert_eq!(History::load(&history).entries["ff#private"].count, 1);
}

#[test]
fn the_menu_key_and_a_right_click_open_the_actions() {
  let Fixture { mut model, .. } = with_action();
  typed(&mut model, "firefox");
  assert_eq!(press(&mut model, Key::Menu), Effect::Redraw);
  let menu = model.menu.clone().unwrap();
  assert_eq!(menu.at, None);
  assert_eq!(
    menu.items[0],
    (MenuAction::Action(0), "Private Window".into())
  );
  assert_eq!(menu.items[1].1, "Add to Favourites");
  let launched = press(&mut model, Key::Enter);
  assert!(
    matches!(&launched, Effect::Launch(launch)
      if launch.id.as_deref() == Some("ff#private")),
    "{launched:?}"
  );
  // Shift+F10 opens the same menu; a right click on the row too.
  let Fixture { mut model, .. } = with_action();
  typed(&mut model, "firefox");
  let shift = Modifiers {
    shift: true,
    ..Modifiers::NONE
  };
  assert_eq!(model.key(Key::F10, Modifiers::NONE), Effect::None);
  assert_eq!(model.key(Key::F10, shift), Effect::Redraw);
  press(&mut model, Key::Escape);
  assert!(model.menu.is_none());
  model.pointer_button(Button::Right, true, (40.0, 50.0), Target::Row(0));
  let menu = model.menu.clone().unwrap();
  assert_eq!(menu.items[0].0, MenuAction::Action(0));
  // Entries without actions start with the edits.
  press(&mut model, Key::Escape);
  press(&mut model, Key::Escape);
  typed(&mut model, "calc");
  press(&mut model, Key::Menu);
  assert_eq!(model.menu.unwrap().items[0].1, "Add to Favourites");
}
