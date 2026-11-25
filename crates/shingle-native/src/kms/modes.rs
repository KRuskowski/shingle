//! Mode selection, following Wayfire.
//!
//! [`select_default_mode`] is Wayfire's `select_default_mode` and
//! [`find_matching_mode`] its `find_matching_mode`
//! (wayfire/src/core/output-layout.cpp). [`candidates`] puts the chosen
//! mode first and the fallbacks for a rejected `TEST_ONLY` commit after
//! it: the preferred mode, every mode by area, and the mode the CRTC
//! already has (docs/design/native-backend.md, section 6).

use super::Mode;

/// How the output's mode is chosen, as Wayfire's `[output:NAME] mode`
/// option says. NB-7 always uses [`ModeChoice::Auto`]; NB-9 reads it from
/// the configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModeChoice {
  /// The preferred mode (`auto`, or no configuration).
  #[default]
  Auto,
  /// The largest mode (`highres`).
  HighRes,
  /// The largest mode at the highest refresh rate (`highrr`).
  HighRr,
  /// `WxH@R`: this size at this refresh rate, or the closest refresh at
  /// that size. A zero refresh takes the preferred or fastest mode.
  Explicit {
    /// Width in pixels.
    width: u32,
    /// Height in pixels.
    height: u32,
    /// Refresh in millihertz, or zero for any.
    refresh_mhz: u32,
  },
}

fn area(mode: &Mode) -> u64 {
  let (width, height) = mode.size();
  u64::from(width) * u64::from(height)
}

/// The first mode with the largest area, as Wayfire's loop finds it.
fn biggest<'a>(pool: impl Iterator<Item = &'a Mode>) -> Option<&'a Mode> {
  let mut best: Option<&Mode> = None;
  for mode in pool {
    if area(mode) > best.map_or(0, area) {
      best = Some(mode);
    }
  }
  best
}

/// The mode Wayfire picks without an explicit one: the preferred mode for
/// `auto`, the largest for `highres`, the largest of the fastest for
/// `highrr`. Without such a mode, the last one in the list.
pub fn select_default_mode(
  modes: &[Mode],
  choice: ModeChoice,
) -> Option<&Mode> {
  let chosen = match choice {
    ModeChoice::Auto | ModeChoice::Explicit { .. } => {
      modes.iter().find(|mode| mode.preferred())
    }
    ModeChoice::HighRes => biggest(modes.iter()),
    ModeChoice::HighRr => {
      let fastest = modes.iter().map(Mode::refresh_mhz).max().unwrap_or(0);
      biggest(modes.iter().filter(|mode| mode.refresh_mhz() == fastest))
    }
  };
  chosen.or_else(|| modes.last())
}

/// Wayfire's `find_matching_mode`: a mode of `width` by `height` at
/// `refresh_mhz`, or, with a zero refresh, the preferred mode of that
/// size. Otherwise, unless `exact`, the mode of that size whose refresh is
/// closest (the fastest for a zero refresh).
pub fn find_matching_mode(
  modes: &[Mode],
  width: u32,
  height: u32,
  refresh_mhz: u32,
  exact: bool,
) -> Option<&Mode> {
  let target = if refresh_mhz == 0 {
    i64::MAX
  } else {
    i64::from(refresh_mhz)
  };
  let mut best: Option<&Mode> = None;
  for mode in modes.iter().filter(|mode| mode.size() == (width, height)) {
    if mode.refresh_mhz() == refresh_mhz {
      return Some(mode);
    }
    if refresh_mhz == 0 && mode.preferred() {
      return Some(mode);
    }
    if exact {
      continue;
    }
    let distance = |mode: &Mode| (i64::from(mode.refresh_mhz()) - target).abs();
    if best.is_none_or(|best| distance(best) > distance(mode)) {
      best = Some(mode);
    }
  }
  best
}

/// The modes to try, in order: the one `choice` selects, then the
/// preferred mode, then every mode from the largest area down (faster
/// first at equal size), then `current`, the mode the CRTC already shows.
/// Each mode appears once.
pub fn candidates(
  modes: &[Mode],
  choice: ModeChoice,
  current: Option<&Mode>,
) -> Vec<Mode> {
  let chosen = match choice {
    ModeChoice::Explicit {
      width,
      height,
      refresh_mhz,
    } => find_matching_mode(modes, width, height, refresh_mhz, false)
      .or_else(|| select_default_mode(modes, ModeChoice::Auto)),
    _ => select_default_mode(modes, choice),
  };
  let mut by_area: Vec<&Mode> = modes.iter().collect();
  by_area.sort_by(|a, b| {
    area(b)
      .cmp(&area(a))
      .then(b.refresh_mhz().cmp(&a.refresh_mhz()))
  });
  let mut list: Vec<Mode> = Vec::new();
  let ordered = chosen
    .into_iter()
    .chain(modes.iter().filter(|mode| mode.preferred()))
    .chain(by_area)
    .chain(current);
  for mode in ordered {
    if !list.contains(mode) {
      list.push(mode.clone());
    }
  }
  list
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::MODE_TYPE_PREFERRED;

  /// A mode of `width` by `height` whose timings give `hz` exactly.
  fn mode(width: u16, height: u16, hz: u32, preferred: bool) -> Mode {
    let htotal = width + 100;
    let vtotal = height + 20;
    Mode {
      clock: u32::from(htotal) * u32::from(vtotal) * hz / 1000,
      hdisplay: width,
      htotal,
      vdisplay: height,
      vtotal,
      vrefresh: hz,
      kind: if preferred { MODE_TYPE_PREFERRED } else { 0 },
      name: format!("{width}x{height}"),
      ..Mode::default()
    }
  }

  fn sizes(modes: &[Mode]) -> Vec<(u32, u32, u32)> {
    modes
      .iter()
      .map(|mode| {
        let (width, height) = mode.size();
        (width, height, (mode.refresh_mhz() + 500) / 1000)
      })
      .collect()
  }

  fn monitor() -> Vec<Mode> {
    vec![
      mode(2560, 1440, 60, true),
      mode(2560, 1440, 144, false),
      mode(3840, 2160, 30, false),
      mode(1920, 1080, 60, false),
      mode(1920, 1080, 144, false),
    ]
  }

  #[test]
  fn auto_takes_the_preferred_mode() {
    let modes = monitor();
    let chosen = select_default_mode(&modes, ModeChoice::Auto).unwrap();
    assert_eq!(chosen.size(), (2560, 1440));
    assert_eq!(chosen.refresh_mhz(), 60_000);
  }

  #[test]
  fn highres_and_highrr_follow_wayfire() {
    let modes = monitor();
    let highres = select_default_mode(&modes, ModeChoice::HighRes).unwrap();
    assert_eq!(highres.size(), (3840, 2160));
    let highrr = select_default_mode(&modes, ModeChoice::HighRr).unwrap();
    assert_eq!(highrr.size(), (2560, 1440));
    assert_eq!(highrr.refresh_mhz(), 144_000);
  }

  #[test]
  fn without_a_preferred_mode_the_last_one_is_taken() {
    let modes = vec![mode(1024, 768, 60, false), mode(800, 600, 60, false)];
    let chosen = select_default_mode(&modes, ModeChoice::Auto).unwrap();
    assert_eq!(chosen.size(), (800, 600));
    assert_eq!(select_default_mode(&[], ModeChoice::Auto), None);
  }

  #[test]
  fn explicit_modes_match_exactly_or_by_closest_refresh() {
    let modes = monitor();
    let exact = find_matching_mode(&modes, 1920, 1080, 144_000, true);
    assert_eq!(exact.map(Mode::refresh_mhz), Some(144_000));
    assert_eq!(find_matching_mode(&modes, 1920, 1080, 100_000, true), None);
    let closest = find_matching_mode(&modes, 1920, 1080, 110_000, false);
    assert_eq!(closest.map(Mode::refresh_mhz), Some(144_000));
    let closest = find_matching_mode(&modes, 1920, 1080, 75_000, false);
    assert_eq!(closest.map(Mode::refresh_mhz), Some(60_000));
    let fastest = find_matching_mode(&modes, 1920, 1080, 0, false);
    assert_eq!(fastest.map(Mode::refresh_mhz), Some(144_000));
    let preferred = find_matching_mode(&modes, 2560, 1440, 0, false);
    assert_eq!(preferred.map(Mode::refresh_mhz), Some(60_000));
    assert_eq!(find_matching_mode(&modes, 640, 480, 0, false), None);
  }

  #[test]
  fn fallbacks_go_preferred_then_by_area_then_current() {
    let modes = monitor();
    let current = mode(1024, 768, 60, false);
    let list = candidates(&modes, ModeChoice::Auto, Some(&current));
    assert_eq!(
      sizes(&list),
      vec![
        (2560, 1440, 60),
        (3840, 2160, 30),
        (2560, 1440, 144),
        (1920, 1080, 144),
        (1920, 1080, 60),
        (1024, 768, 60),
      ]
    );
  }

  #[test]
  fn an_explicit_choice_comes_before_the_preferred_mode() {
    let modes = monitor();
    let choice = ModeChoice::Explicit {
      width: 1920,
      height: 1080,
      refresh_mhz: 144_000,
    };
    let list = candidates(&modes, choice, None);
    assert_eq!(sizes(&list)[..2], [(1920, 1080, 144), (2560, 1440, 60)]);
    assert_eq!(list.len(), modes.len());
    let missing = ModeChoice::Explicit {
      width: 640,
      height: 480,
      refresh_mhz: 0,
    };
    assert_eq!(
      sizes(&candidates(&modes, missing, None))[0],
      (2560, 1440, 60)
    );
  }

  #[test]
  fn the_current_mode_is_listed_once() {
    let modes = monitor();
    let list = candidates(&modes, ModeChoice::Auto, Some(&modes[3]));
    assert_eq!(list.len(), modes.len());
    assert!(candidates(&[], ModeChoice::Auto, None).is_empty());
    let only_current = candidates(&[], ModeChoice::Auto, Some(&modes[0]));
    assert_eq!(only_current, vec![modes[0].clone()]);
  }
}
