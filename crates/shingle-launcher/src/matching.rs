//! Fuzzy matching in the style of fzy: a case-insensitive subsequence
//! match, scored for matches at word starts and runs of consecutive
//! characters, with small penalties for the characters skipped.

/// Score of a match that is the whole candidate.
pub const EXACT: f64 = 1000.0;
const GAP_LEADING: f64 = -0.005;
const GAP_TRAILING: f64 = -0.005;
const GAP_INNER: f64 = -0.01;
const CONSECUTIVE: f64 = 1.0;
const AFTER_SLASH: f64 = 0.9;
const AFTER_WORD: f64 = 0.8;
const AFTER_CAPITAL: f64 = 0.7;
const AFTER_DOT: f64 = 0.6;
/// Added when the candidate starts with the whole query.
const PREFIX: f64 = 1.0;
/// Candidates longer than this are matched as subsequences without the
/// scoring table.
const MAX_SCORED: usize = 256;

/// The score of `query` against `candidate`, or `None` when the query's
/// characters do not all appear in order. Higher is better; an empty query
/// matches everything with 0.
pub fn score(query: &str, candidate: &str) -> Option<f64> {
  if query.is_empty() {
    return Some(0.0);
  }
  let needle = query
    .chars()
    .flat_map(char::to_lowercase)
    .collect::<Vec<_>>();
  let original = candidate.chars().collect::<Vec<_>>();
  let haystack = original
    .iter()
    .map(|character| character.to_lowercase().next().unwrap_or(*character))
    .collect::<Vec<_>>();
  if !is_subsequence(&needle, &haystack) {
    return None;
  }
  if needle == haystack {
    return Some(EXACT);
  }
  let prefix = if haystack.starts_with(&needle) {
    PREFIX
  } else {
    0.0
  };
  if haystack.len() > MAX_SCORED || needle.len() > haystack.len() {
    return Some(GAP_INNER * haystack.len() as f64 + prefix);
  }
  Some(table_score(&needle, &haystack, &bonuses(&original)) + prefix)
}

fn is_subsequence(needle: &[char], haystack: &[char]) -> bool {
  let mut characters = haystack.iter();
  needle
    .iter()
    .all(|wanted| characters.any(|character| character == wanted))
}

/// The bonus for a match at each position, from the character before it.
fn bonuses(original: &[char]) -> Vec<f64> {
  let mut previous = '/';
  original
    .iter()
    .map(|&character| {
      let bonus = match previous {
        '/' => AFTER_SLASH,
        '-' | '_' | ' ' => AFTER_WORD,
        '.' => AFTER_DOT,
        before if before.is_lowercase() && character.is_uppercase() => {
          AFTER_CAPITAL
        }
        _ => 0.0,
      };
      previous = character;
      bonus
    })
    .collect()
}

/// fzy's dynamic programme: `best[j]` is the best score of the needle so
/// far with its last character at or before `j`, `ending[j]` the best with
/// it exactly at `j`.
fn table_score(needle: &[char], haystack: &[char], bonus: &[f64]) -> f64 {
  let width = haystack.len();
  let mut best = vec![f64::NEG_INFINITY; width];
  let mut ending = vec![f64::NEG_INFINITY; width];
  for (i, wanted) in needle.iter().enumerate() {
    let gap = if i + 1 == needle.len() {
      GAP_TRAILING
    } else {
      GAP_INNER
    };
    let previous_best = std::mem::replace(&mut best, vec![0.0; width]);
    let previous_ending = std::mem::replace(&mut ending, vec![0.0; width]);
    let mut running = f64::NEG_INFINITY;
    for (j, character) in haystack.iter().enumerate() {
      if character == wanted {
        let score = if i == 0 {
          j as f64 * GAP_LEADING + bonus[j]
        } else if j > 0 {
          (previous_best[j - 1] + bonus[j])
            .max(previous_ending[j - 1] + CONSECUTIVE)
        } else {
          f64::NEG_INFINITY
        };
        ending[j] = score;
        running = score.max(running + gap);
      } else {
        ending[j] = f64::NEG_INFINITY;
        running += gap;
      }
      best[j] = running;
    }
  }
  best[width - 1]
}

#[cfg(test)]
mod tests {
  use super::*;

  fn better(query: &str, first: &str, second: &str) -> bool {
    score(query, first).unwrap() > score(query, second).unwrap()
  }

  #[test]
  fn requires_the_characters_in_order() {
    assert!(score("ffx", "Firefox").is_some());
    assert!(score("xff", "Firefox").is_none());
    assert!(score("fire fox", "Firefox").is_none());
    assert_eq!(score("", "anything"), Some(0.0));
  }

  #[test]
  fn ignores_case_and_ranks_exact_matches_first() {
    assert_eq!(score("KITTY", "kitty"), Some(EXACT));
    assert!(better("kitty", "kitty", "kitty-terminal"));
  }

  #[test]
  fn prefers_prefixes_word_starts_and_runs() {
    assert!(better("fire", "Firefox", "Bonfire"));
    assert!(better("ls", "LibreOffice Calc Spreadsheet", "Calculus"));
    assert!(better("term", "GNOME Terminal", "Thermometer"));
    assert!(better("sm", "System Monitor", "Plasma"));
    assert!(better("vc", "VideoCapture", "Voice"));
  }

  #[test]
  fn long_candidates_still_match() {
    let long = format!("{}needle", "x".repeat(400));
    assert!(score("needle", &long).is_some());
    assert!(score("nope", &long).is_none());
  }
}
