//! Overrides that turn NB-10's display features off: the cursor plane,
//! async page flips and render fences (docs/design/native-backend.md,
//! NB-10), so that each fallback can be forced on the test machine
//! (docs/native-testing.md).
//!
//! - `SHINGLE_CURSOR_PLANE=0`, or wlroots' `WLR_NO_HARDWARE_CURSORS=1`
//!   which Wayfire users know: the frontend draws the cursor into every
//!   frame.
//! - `SHINGLE_ASYNC_FLIPS=0`: frames of clients that ask for tearing wait
//!   for the vblank like every other frame.
//! - `SHINGLE_IN_FENCE=0`: the renderer waits for the GPU before each
//!   zero-copy flip, as before NB-10, instead of handing the plane a
//!   fence.
//!
//! Unset, empty or `1` leaves a feature on; any other value too, with a
//! warning for the log.

use std::ffi::OsString;

/// Which features an override turned off, each with the reason the log
/// gives.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaneFeatures {
  /// Why cursor planes are not used.
  pub no_cursor_plane: Option<String>,
  /// Why async page flips are not used.
  pub no_async_flips: Option<String>,
  /// Why render fences are not used.
  pub no_in_fence: Option<String>,
}

/// What a variable that turns a feature off says: off (with the reason),
/// on, or on with a warning.
fn off_switch(
  name: &str,
  value: Option<OsString>,
  off: &str,
  warnings: &mut Vec<String>,
) -> Option<String> {
  let value = value.filter(|value| !value.is_empty())?;
  match value.to_str() {
    Some("0") => Some(format!("{name}=0 {off}")),
    Some("1") => None,
    _ => {
      warnings.push(format!(
        "{name}={} is neither 0 nor 1; leaving it on",
        value.to_string_lossy()
      ));
      None
    }
  }
}

impl PlaneFeatures {
  /// Reads the overrides through `var`, which looks up an environment
  /// variable. Returns the features and the warnings for the log.
  pub fn from_env(
    var: &dyn Fn(&str) -> Option<OsString>,
  ) -> (Self, Vec<String>) {
    let mut warnings = Vec::new();
    let wlroots = var("WLR_NO_HARDWARE_CURSORS")
      .filter(|value| value == "1")
      .map(|_| "WLR_NO_HARDWARE_CURSORS=1 turns cursor planes off".to_owned());
    let cursor = off_switch(
      "SHINGLE_CURSOR_PLANE",
      var("SHINGLE_CURSOR_PLANE"),
      "turns cursor planes off",
      &mut warnings,
    );
    let async_flips = off_switch(
      "SHINGLE_ASYNC_FLIPS",
      var("SHINGLE_ASYNC_FLIPS"),
      "keeps every page flip synchronized",
      &mut warnings,
    );
    let fence = off_switch(
      "SHINGLE_IN_FENCE",
      var("SHINGLE_IN_FENCE"),
      "makes the renderer wait for the GPU before each flip",
      &mut warnings,
    );
    let features = Self {
      no_cursor_plane: cursor.or(wlroots),
      no_async_flips: async_flips,
      no_in_fence: fence,
    };
    (features, warnings)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn read(vars: &[(&str, &str)]) -> (PlaneFeatures, Vec<String>) {
    let vars: Vec<(String, OsString)> = vars
      .iter()
      .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
      .collect();
    PlaneFeatures::from_env(&|name| {
      vars
        .iter()
        .find(|(known, _)| known == name)
        .map(|(_, value)| value.clone())
    })
  }

  #[test]
  fn everything_is_on_by_default() {
    assert_eq!(read(&[]), (PlaneFeatures::default(), Vec::new()));
    let (features, warnings) = read(&[
      ("SHINGLE_CURSOR_PLANE", "1"),
      ("SHINGLE_ASYNC_FLIPS", ""),
      ("WLR_NO_HARDWARE_CURSORS", "0"),
    ]);
    assert_eq!(features, PlaneFeatures::default());
    assert!(warnings.is_empty());
  }

  #[test]
  fn zero_turns_each_off_with_its_reason() {
    let (features, warnings) = read(&[
      ("SHINGLE_CURSOR_PLANE", "0"),
      ("SHINGLE_ASYNC_FLIPS", "0"),
      ("SHINGLE_IN_FENCE", "0"),
    ]);
    assert!(warnings.is_empty());
    assert_eq!(
      features,
      PlaneFeatures {
        no_cursor_plane: Some(
          "SHINGLE_CURSOR_PLANE=0 turns cursor planes off".into()
        ),
        no_async_flips: Some(
          "SHINGLE_ASYNC_FLIPS=0 keeps every page flip synchronized".into()
        ),
        no_in_fence: Some(
          "SHINGLE_IN_FENCE=0 makes the renderer wait for the GPU before \
           each flip"
            .into()
        ),
      }
    );
  }

  #[test]
  fn wlroots_variable_turns_the_cursor_plane_off() {
    let (features, _) = read(&[("WLR_NO_HARDWARE_CURSORS", "1")]);
    assert_eq!(
      features.no_cursor_plane.as_deref(),
      Some("WLR_NO_HARDWARE_CURSORS=1 turns cursor planes off")
    );
  }

  #[test]
  fn other_values_leave_it_on_with_a_warning() {
    let (features, warnings) = read(&[("SHINGLE_IN_FENCE", "no")]);
    assert_eq!(features, PlaneFeatures::default());
    assert_eq!(
      warnings,
      ["SHINGLE_IN_FENCE=no is neither 0 nor 1; leaving it on"]
    );
  }
}
