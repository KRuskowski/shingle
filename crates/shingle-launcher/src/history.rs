//! How often and how recently each entry was launched, and the frecency
//! ranked on.

use std::{collections::BTreeMap, fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::files::write_atomically;

/// The version of the history file.
pub const HISTORY_VERSION: u32 = 1;
/// The most frecency can add to a match score.
pub const MAX_FRECENCY: f64 = 3.0;
const SECONDS_PER_DAY: f64 = 86_400.0;

/// One entry's launches.
#[derive(
  Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct Use {
  /// How often it was launched.
  pub count: u32,
  /// When it was last launched, in seconds since the Unix epoch.
  pub last: u64,
}

/// Launches by entry ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
  /// [`HISTORY_VERSION`].
  pub version: u32,
  /// Launches by entry ID.
  pub entries: BTreeMap<String, Use>,
}

impl History {
  /// The history at `path`; empty when it is missing or unreadable.
  pub fn load(path: &Path) -> Self {
    fs::read(path)
      .ok()
      .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
      .filter(|history| history.version == HISTORY_VERSION)
      .unwrap_or_default()
  }

  /// Writes the history to `path`.
  pub fn save(&self, path: &Path) -> io::Result<()> {
    let mut history = self.clone();
    history.version = HISTORY_VERSION;
    let bytes =
      serde_json::to_vec_pretty(&history).map_err(io::Error::other)?;
    write_atomically(path, &bytes)
  }

  /// Counts a launch of `id` at `now` (Unix seconds).
  pub fn record(&mut self, id: &str, now: u64) {
    let entry = self.entries.entry(id.to_owned()).or_default();
    entry.count = entry.count.saturating_add(1);
    entry.last = entry.last.max(now);
  }

  /// `2 ln(1 + count) / (1 + age_days / 7)`, at most [`MAX_FRECENCY`]; 0
  /// for an entry never launched.
  pub fn frecency(&self, id: &str, now: u64) -> f64 {
    let Some(entry) = self.entries.get(id) else {
      return 0.0;
    };
    let age_days = now.saturating_sub(entry.last) as f64 / SECONDS_PER_DAY;
    let value = 2.0 * f64::from(entry.count).ln_1p() / (1.0 + age_days / 7.0);
    value.min(MAX_FRECENCY)
  }

  /// The IDs launched, most recent first.
  pub fn recent(&self) -> Vec<&str> {
    let mut ids = self.entries.iter().collect::<Vec<_>>();
    ids.sort_by(|(a_id, a), (b_id, b)| {
      b.last.cmp(&a.last).then_with(|| a_id.cmp(b_id))
    });
    ids.into_iter().map(|(id, _)| id.as_str()).collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const DAY: u64 = 86_400;

  #[test]
  fn frecency_grows_with_use_and_fades_with_age() {
    let mut history = History::default();
    let now = 100 * DAY;
    assert_eq!(history.frecency("a", now), 0.0);
    history.record("a", now);
    let once = history.frecency("a", now);
    history.record("a", now);
    history.record("a", now);
    let thrice = history.frecency("a", now);
    assert!(thrice > once && once > 0.0);
    assert!(history.frecency("a", now + 14 * DAY) < thrice / 2.0);
    for _ in 0..1000 {
      history.record("a", now);
    }
    assert_eq!(history.frecency("a", now), MAX_FRECENCY);
  }

  #[test]
  fn recent_is_newest_first_and_the_file_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state/history.json");
    assert_eq!(History::load(&path), History::default());
    let mut history = History::default();
    history.record("old", DAY);
    history.record("new", 3 * DAY);
    history.record("mid", 2 * DAY);
    assert_eq!(history.recent(), ["new", "mid", "old"]);
    history.save(&path).unwrap();
    let loaded = History::load(&path);
    assert_eq!(loaded.entries, history.entries);
    fs::write(&path, b"garbage").unwrap();
    assert_eq!(History::load(&path), History::default());
  }
}
