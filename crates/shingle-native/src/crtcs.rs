//! Which CRTC drives which connector when several are lit.
//!
//! [`match_crtcs`] is the matching of wlroots' `match_connectors_with_crtcs`
//! (backend/drm/util.c), which `realloc_crtcs` (backend/drm/drm.c) runs for
//! every enabled connector at once: it lights as many connectors as the
//! card's CRTCs allow, prefers the CRTC each connector is on already, and
//! never moves the CRTC of an output that is lit, since that would need a
//! modeset of a running output (wlroots refuses the configuration then).
//! Shingle keeps lit outputs on their CRTCs and matches the others among
//! the CRTCs left, so an output that is lit never goes dark because another
//! was plugged in. Where not every connector can be lit, the earlier ones
//! in the list, which the caller orders by priority
//! ([`crate::outputs::rank`]), win.
//! (docs/design/multi-output.md, section 7, "Decision: native".)

/// A connector that wants a CRTC, with CRTCs named by their index in the
/// card's CRTC list.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Want {
  /// The CRTCs that can drive it.
  pub possible: Vec<usize>,
  /// The CRTC it is on now, as the console or another session left it,
  /// which it gets if it can.
  pub current: Option<usize>,
  /// The CRTC Shingle drives it with, which it keeps.
  pub lit: Option<usize>,
}

/// The best matching found so far, and the one being built.
struct Search<'a> {
  wants: &'a [Want],
  taken: Vec<bool>,
  chosen: Vec<Option<usize>>,
  best: Vec<Option<usize>>,
  best_score: Option<Score>,
}

/// How good a matching is: more connectors lit first, then fewer moved
/// from their current CRTC, then the earlier connectors lit.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Score {
  lit: usize,
  kept: usize,
  /// Whether each connector is lit, in priority order.
  order: Vec<bool>,
}

impl Search<'_> {
  fn score(&self) -> Score {
    let order: Vec<bool> = self.chosen.iter().map(Option::is_some).collect();
    Score {
      lit: order.iter().filter(|lit| **lit).count(),
      kept: self
        .chosen
        .iter()
        .zip(self.wants)
        .filter(|(chosen, want)| chosen.is_some() && **chosen == want.current)
        .count(),
      order,
    }
  }

  fn run(&mut self, index: usize, lit: usize) {
    if index == self.wants.len() {
      let score = self.score();
      if self.best_score.as_ref().is_none_or(|best| score > *best) {
        self.best_score = Some(score);
        self.best = self.chosen.clone();
      }
      return;
    }
    // Even lighting every remaining connector cannot beat the best.
    let remaining = self.wants.len() - index;
    if self
      .best_score
      .as_ref()
      .is_some_and(|best| lit + remaining < best.lit)
    {
      return;
    }
    let want = &self.wants[index];
    if want.lit.is_some() {
      // Placed before the search; it keeps its CRTC.
      self.run(index + 1, lit + 1);
      return;
    }
    let current = want.current.filter(|crtc| want.possible.contains(crtc));
    let mut candidates: Vec<usize> = current.into_iter().collect();
    candidates.extend(
      want
        .possible
        .iter()
        .copied()
        .filter(|crtc| Some(*crtc) != current),
    );
    for crtc in candidates {
      if self.taken.get(crtc).is_none_or(|taken| *taken) {
        continue;
      }
      self.taken[crtc] = true;
      self.chosen[index] = Some(crtc);
      self.run(index + 1, lit + 1);
      self.chosen[index] = None;
      self.taken[crtc] = false;
    }
    self.run(index + 1, lit);
  }
}

/// The CRTC for each of `wants`, of a card with `crtcs` CRTCs, or `None`
/// for a connector that stays dark: lit connectors keep their CRTC, and
/// the others get the matching that lights the most of them, then moves
/// the fewest from their current CRTC, then lights the earlier ones.
pub fn match_crtcs(wants: &[Want], crtcs: usize) -> Vec<Option<usize>> {
  let mut search = Search {
    wants,
    taken: vec![false; crtcs],
    chosen: vec![None; wants.len()],
    best: vec![None; wants.len()],
    best_score: None,
  };
  for (index, want) in wants.iter().enumerate() {
    if let Some(crtc) = want.lit.filter(|crtc| *crtc < crtcs)
      && !search.taken[crtc]
    {
      search.taken[crtc] = true;
      search.chosen[index] = Some(crtc);
    }
  }
  // A lit connector whose CRTC was taken twice, or is out of range, is
  // matched like any other.
  let fixed: Vec<Want> = wants
    .iter()
    .zip(&search.chosen)
    .map(|(want, chosen)| Want {
      lit: chosen.and(want.lit),
      ..want.clone()
    })
    .collect();
  search.wants = &fixed;
  search.run(0, 0);
  search.best
}

#[cfg(test)]
mod tests {
  use super::*;

  fn want(possible: &[usize]) -> Want {
    Want {
      possible: possible.to_vec(),
      ..Want::default()
    }
  }

  #[test]
  fn each_connector_gets_a_crtc_of_its_own() {
    let wants = [want(&[0, 1]), want(&[0, 1])];
    assert_eq!(match_crtcs(&wants, 2), [Some(0), Some(1)]);
  }

  #[test]
  fn a_constrained_connector_is_not_starved_by_an_earlier_one() {
    // wlroots' matching maximises the lit connectors: the first could
    // take CRTC 0, but then the second, which only CRTC 0 drives, would
    // stay dark.
    let wants = [want(&[0, 1]), want(&[0])];
    assert_eq!(match_crtcs(&wants, 2), [Some(1), Some(0)]);
  }

  #[test]
  fn more_connectors_than_crtcs_leave_the_last_ones_dark() {
    let wants = [want(&[0, 1]), want(&[0, 1]), want(&[0, 1])];
    assert_eq!(match_crtcs(&wants, 2), [Some(0), Some(1), None]);
  }

  #[test]
  fn connectors_keep_the_crtc_they_are_on() {
    let mut first = want(&[0, 1]);
    first.current = Some(1);
    let mut second = want(&[0, 1]);
    second.current = Some(0);
    assert_eq!(match_crtcs(&[first, second], 2), [Some(1), Some(0)]);
  }

  #[test]
  fn lit_outputs_never_move() {
    // The best matching would move the lit connector to CRTC 1 so that
    // the new one could have CRTC 0; Shingle keeps the lit output, and
    // the new one stays dark, as wlroots keeps the old configuration.
    let mut lit = want(&[0, 1]);
    lit.lit = Some(0);
    let new = want(&[0]);
    assert_eq!(match_crtcs(&[lit.clone(), new], 2), [Some(0), None]);
    // A second new connector still gets the free CRTC.
    let other = want(&[0, 1]);
    assert_eq!(match_crtcs(&[lit, other], 2), [Some(0), Some(1)]);
  }

  #[test]
  fn connectors_without_a_possible_crtc_stay_dark() {
    let wants = [want(&[]), want(&[2]), want(&[0])];
    assert_eq!(match_crtcs(&wants, 1), [None, None, Some(0)]);
    assert!(match_crtcs(&[], 3).is_empty());
  }

  #[test]
  fn a_crtc_claimed_twice_by_lit_outputs_goes_to_the_first() {
    let mut first = want(&[0, 1]);
    first.lit = Some(0);
    let mut second = want(&[0, 1]);
    second.lit = Some(0);
    assert_eq!(match_crtcs(&[first, second], 2), [Some(0), Some(1)]);
  }

  #[test]
  fn four_monitors_on_three_crtcs() {
    // Two DisplayPort connectors that every CRTC drives, an HDMI port on
    // CRTC 2 only and an internal panel on CRTC 0 or 1.
    let wants = [
      want(&[0, 1]),
      want(&[0, 1, 2]),
      want(&[0, 1, 2]),
      want(&[2]),
    ];
    // Three can be lit; the first three in priority order are.
    assert_eq!(match_crtcs(&wants, 3), [Some(0), Some(1), Some(2), None]);
    // Without the third, the HDMI port gets CRTC 2.
    let wants = [want(&[0, 1]), want(&[0, 1, 2]), want(&[2])];
    assert_eq!(match_crtcs(&wants, 3), [Some(0), Some(1), Some(2)]);
  }
}
