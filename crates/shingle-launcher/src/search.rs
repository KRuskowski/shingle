//! Ranking entries for a query: the fuzzy match of their fields, a bonus
//! for desktop entries and shortcuts, and frecency.

use crate::{
  history::History,
  index::{Entry, EntryKind},
  matching::score,
};

/// How much a match in each field counts, relative to the shown name.
const GENERIC_NAME_WEIGHT: f64 = 0.8;
const KEYWORD_WEIGHT: f64 = 0.7;
const PROGRAM_WEIGHT: f64 = 0.9;
/// Added for desktop entries and shortcuts over plain commands.
const ENTRY_BONUS: f64 = 0.3;

/// An entry as the user sees it.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
  /// The entry.
  pub entry: &'a Entry,
  /// Its shown name, after the user's renames.
  pub name: &'a str,
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
        score: matched + bonus + history.frecency(&candidate.entry.id, now),
      })
    })
    .collect::<Vec<_>>();
  hits.sort_by(|a, b| {
    b.score.total_cmp(&a.score).then_with(|| {
      let name = |hit: &Hit| candidates[hit.index].name.to_lowercase();
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
    }
  }

  fn names(entries: &[Entry], query: &str, history: &History) -> Vec<String> {
    let candidates = entries
      .iter()
      .map(|entry| Candidate {
        entry,
        name: &entry.name,
      })
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
      Candidate {
        entry: &entries[0],
        name: "Renamed",
      },
      Candidate {
        entry: &entries[1],
        name: "Alpha",
      },
    ];
    let hits = rank("ren", &candidates, &History::default(), 0, 10);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].index, 0);
    let both = [
      Candidate {
        entry: &entries[0],
        name: "Same",
      },
      Candidate {
        entry: &entries[1],
        name: "same",
      },
    ];
    let hits = rank("sa", &both, &History::default(), 0, 1);
    assert_eq!(hits.len(), 1);
  }
}
