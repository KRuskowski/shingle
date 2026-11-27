//! Ranking entries for a query: the fuzzy match of their fields, a bonus
//! for desktop entries and shortcuts, and frecency.

use crate::{
  history::History,
  index::{Entry, EntryAction, EntryKind, action_id},
  matching::score,
};

/// How much a match in each field counts, relative to the shown name.
const GENERIC_NAME_WEIGHT: f64 = 0.8;
const KEYWORD_WEIGHT: f64 = 0.7;
const PROGRAM_WEIGHT: f64 = 0.9;
/// Added for desktop entries and shortcuts over plain commands.
const ENTRY_BONUS: f64 = 0.3;
/// Taken from an action's score, so that an action ranks below an entry
/// that matches as well.
const ACTION_PENALTY: f64 = 0.4;

/// An entry, or one of its desktop actions, as the user sees it.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
  /// The entry.
  pub entry: &'a Entry,
  /// Its shown name, after the user's renames.
  pub name: &'a str,
  /// The action, when the candidate is one of the entry's actions.
  pub action: Option<&'a EntryAction>,
}

impl<'a> Candidate<'a> {
  /// The entry itself.
  pub fn entry(entry: &'a Entry, name: &'a str) -> Self {
    Self {
      entry,
      name,
      action: None,
    }
  }

  /// The name ties are sorted by: the action's, else the entry's.
  fn sort_name(&self) -> String {
    self
      .action
      .map_or(self.name, |action| action.name.as_str())
      .to_lowercase()
  }

  /// The ID its frecency is recorded under.
  fn history_id(&self) -> String {
    match self.action {
      Some(action) => action_id(&self.entry.id, &action.id),
      None => self.entry.id.clone(),
    }
  }
}

/// The match of `query` against an action named `action` of an entry
/// shown as `name`: against the action's name, or against both names
/// together. A query the entry's name already matches must match the
/// action's own name, so that typing an application's name lists it
/// rather than all its actions.
fn action_score(query: &str, name: &str, action: &str) -> Option<f64> {
  let own = score(query, action);
  if own.is_none() && score(query, name).is_some() {
    return None;
  }
  let together = score(query, &format!("{name} {action}"));
  [own, together].into_iter().flatten().reduce(f64::max)
}

/// A ranked match.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
  /// The candidate's position in the slice given to [`rank`].
  pub index: usize,
  /// Its score; higher is better.
  pub score: f64,
}

/// The best match of `query` against `candidate`'s fields, weighted, or
/// `None` when no field matches.
pub fn match_score(query: &str, candidate: &Candidate<'_>) -> Option<f64> {
  let entry = candidate.entry;
  if let Some(action) = candidate.action {
    return action_score(query, candidate.name, &action.name)
      .map(|value| value - ACTION_PENALTY);
  }
  let weighted = |text: &str, weight: f64| {
    (!text.is_empty())
      .then(|| score(query, text))
      .flatten()
      .map(|value| value * weight)
  };
  [
    weighted(candidate.name, 1.0),
    weighted(&entry.generic_name, GENERIC_NAME_WEIGHT),
    weighted(&entry.program, PROGRAM_WEIGHT),
  ]
  .into_iter()
  .chain(
    entry
      .keywords
      .iter()
      .map(|keyword| weighted(keyword, KEYWORD_WEIGHT)),
  )
  .flatten()
  .reduce(f64::max)
}

/// The `limit` best matches of a non-empty `query` among `candidates`,
/// best first, ties by name. Frecency from `history` at `now` adds to each
/// score.
pub fn rank(
  query: &str,
  candidates: &[Candidate<'_>],
  history: &History,
  now: u64,
  limit: usize,
) -> Vec<Hit> {
  let mut hits = candidates
    .iter()
    .enumerate()
    .filter_map(|(index, candidate)| {
      let matched = match_score(query, candidate)?;
      let bonus = if candidate.entry.kind == EntryKind::Command {
        0.0
      } else {
        ENTRY_BONUS
      };
      Some(Hit {
        index,
        score: matched + bonus + history.frecency(&candidate.history_id(), now),
      })
    })
    .collect::<Vec<_>>();
  hits.sort_by(|a, b| {
    b.score.total_cmp(&a.score).then_with(|| {
      let name = |hit: &Hit| candidates[hit.index].sort_name();
      name(a).cmp(&name(b))
    })
  });
  hits.truncate(limit);
  hits
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(id: &str, kind: EntryKind, name: &str) -> Entry {
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
      category: String::new(),
      icon: String::new(),
      actions: Vec::new(),
    }
  }

  fn names(entries: &[Entry], query: &str, history: &History) -> Vec<String> {
    let candidates = entries
      .iter()
      .map(|entry| Candidate::entry(entry, &entry.name))
      .collect::<Vec<_>>();
    rank(query, &candidates, history, 1_000_000, 10)
      .into_iter()
      .map(|hit| entries[hit.index].name.clone())
      .collect()
  }

  #[test]
  fn desktop_entries_beat_commands_and_better_matches_beat_use() {
    let entries = [
      entry("path:firecfg", EntryKind::Command, "firecfg"),
      entry("firefox.desktop", EntryKind::Application, "Firefox"),
      entry("bonfire.desktop", EntryKind::Application, "Bonfire"),
    ];
    let mut history = History::default();
    assert_eq!(
      names(&entries, "fire", &history),
      ["Firefox", "firecfg", "Bonfire"]
    );
    history.record("bonfire.desktop", 1_000_000);
    history.record("bonfire.desktop", 1_000_000);
    assert_eq!(names(&entries, "fire", &history)[0], "Bonfire");
    assert_eq!(names(&entries, "firefox", &history)[0], "Firefox");
  }

  #[test]
  fn generic_names_keywords_and_programs_match_too() {
    let mut browser = entry("a.desktop", EntryKind::Application, "Zeta");
    browser.generic_name = "Web Browser".into();
    let mut keyworded = entry("b.desktop", EntryKind::Application, "Omega");
    keyworded.keywords = vec!["internet".into()];
    let mut program = entry("c.desktop", EntryKind::Application, "Files");
    program.program = "nautilus".into();
    let entries = [browser, keyworded, program];
    let history = History::default();
    assert_eq!(names(&entries, "web", &history), ["Zeta"]);
    assert_eq!(names(&entries, "inter", &history), ["Omega"]);
    assert_eq!(names(&entries, "naut", &history), ["Files"]);
  }

  #[test]
  fn the_shown_name_is_matched_and_ties_sort_by_name() {
    let entries = [
      entry("b.desktop", EntryKind::Application, "Beta"),
      entry("a.desktop", EntryKind::Application, "Alpha"),
    ];
    let candidates = [
      Candidate::entry(&entries[0], "Renamed"),
      Candidate::entry(&entries[1], "Alpha"),
    ];
    let hits = rank("ren", &candidates, &History::default(), 0, 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].index, 0);
    let both = [
      Candidate::entry(&entries[0], "Same"),
      Candidate::entry(&entries[1], "same"),
    ];
    let hits = rank("sa", &both, &History::default(), 0, 1);
    assert_eq!(hits.len(), 1);
  }

  #[test]
  fn actions_match_their_own_name_or_both_names() {
    let mut browser = entry("ff.desktop", EntryKind::Application, "Firefox");
    browser.actions = vec![EntryAction {
      id: "private".into(),
      name: "New Private Window".into(),
      icon: String::new(),
      command: "firefox --private-window".into(),
    }];
    let candidates = [
      Candidate::entry(&browser, "Firefox"),
      Candidate {
        entry: &browser,
        name: "Firefox",
        action: Some(&browser.actions[0]),
      },
    ];
    let indices = |query: &str, history: &History| {
      rank(query, &candidates, history, 1_000_000, 10)
        .into_iter()
        .map(|hit| hit.index)
        .collect::<Vec<_>>()
    };
    let history = History::default();
    // The entry's name alone does not list its actions.
    assert_eq!(indices("firefox", &history), [0]);
    assert_eq!(indices("private", &history), [1]);
    assert_eq!(indices("fire priv", &history), [1]);
    assert_eq!(indices("w", &history), [1]);
    // An action used often climbs by its own history.
    let mut used = History::default();
    for _ in 0..5 {
      used.record("ff.desktop#private", 1_000_000);
    }
    assert!(
      rank("private", &candidates, &used, 1_000_000, 10)[0].score
        > rank("private", &candidates, &history, 1_000_000, 10)[0].score
    );
  }
}
