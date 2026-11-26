//! The bindings of Wayfire's `oswitch` and `wsets` plugins, which the
//! compositor matches itself so that every frontend and headless Shingle's
//! STIPC keyboard run them alike.

use shingle_config::{Binding, Modifiers};
use shingle_core::WsetId;

use super::focus::{FocusSettings, OutputSwitch};
use crate::state::CompositorState;

/// What an `oswitch` or `wsets` binding does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputAction {
  /// Focus another output, with the focused window if `with_window`.
  Switch {
    /// Which output.
    switch: OutputSwitch,
    /// Whether the focused window goes along.
    with_window: bool,
  },
  /// Show workspace set N on the active output (`wsets_bindings`).
  ShowWset(u32),
  /// Send the focused window to workspace set N (`send_window_bindings`).
  SendToWset(u32),
}

/// The action whose binding matches `key` with `modifiers`, if any.
pub(crate) fn matching(
  settings: &FocusSettings,
  modifiers: Modifiers,
  key: &str,
) -> Option<OutputAction> {
  let oswitch = &settings.oswitch;
  let switches: [(&Binding, OutputSwitch, bool); 8] = [
    (&oswitch.next_output, OutputSwitch::Next, false),
    (&oswitch.next_output_with_win, OutputSwitch::Next, true),
    (&oswitch.prev_output, OutputSwitch::Previous, false),
    (&oswitch.prev_output_with_win, OutputSwitch::Previous, true),
    (&oswitch.left_output, OutputSwitch::Left, false),
    (&oswitch.right_output, OutputSwitch::Right, false),
    (&oswitch.above_output, OutputSwitch::Above, false),
    (&oswitch.below_output, OutputSwitch::Below, false),
  ];
  for (binding, switch, with_window) in switches {
    if binding.matches(modifiers, key) {
      return Some(OutputAction::Switch {
        switch,
        with_window,
      });
    }
  }
  let wsets = &settings.wsets;
  if let Some(binding) = wsets
    .wsets_bindings
    .iter()
    .find(|binding| binding.binding.matches(modifiers, key))
  {
    return Some(OutputAction::ShowWset(binding.wset));
  }
  wsets
    .send_window_bindings
    .iter()
    .find(|binding| binding.binding.matches(modifiers, key))
    .map(|binding| OutputAction::SendToWset(binding.wset))
}

/// Runs `action`. Returns whether it changed anything.
pub(crate) fn run(state: &mut CompositorState, action: OutputAction) -> bool {
  match action {
    OutputAction::Switch {
      switch,
      with_window,
    } => {
      let before = state.active_output.clone();
      super::focus::oswitch(state, switch, with_window) != before
    }
    OutputAction::ShowWset(index) => {
      let output = state.active_output.clone();
      super::wsets::select_wset(state, &output, WsetId(index))
        .map_err(|error| eprintln!("shingle: wsets failed: {error}"))
        .is_ok()
    }
    OutputAction::SendToWset(index) => {
      let Some(window) = crate::input::focused_window(state) else {
        return false;
      };
      super::wsets::send_to_wset(state, window, WsetId(index))
        .map_err(|error| eprintln!("shingle: wsets failed: {error}"))
        .is_ok()
    }
  }
}

#[cfg(test)]
mod tests {
  use shingle_config::{Modifier, OswitchConfig, WsetBinding, WsetsConfig};

  use super::*;

  fn settings() -> FocusSettings {
    FocusSettings {
      oswitch: OswitchConfig {
        left_output: Binding::new("left", vec![Modifier::Super]),
        ..OswitchConfig::default()
      },
      wsets: WsetsConfig {
        wsets_bindings: vec![WsetBinding {
          wset: 2,
          binding: Binding::new("2", vec![Modifier::Alt]),
        }],
        send_window_bindings: vec![WsetBinding {
          wset: 3,
          binding: Binding::new("3", vec![Modifier::Alt, Modifier::Shift]),
        }],
        ..WsetsConfig::default()
      },
      device_outputs: Default::default(),
      focus_timeout: std::time::Duration::from_secs(10),
    }
  }

  fn held(super_key: bool, alt: bool, shift: bool) -> Modifiers {
    Modifiers {
      super_key,
      control: false,
      alt,
      shift,
    }
  }

  #[test]
  fn oswitch_and_wsets_bindings_match_their_actions() {
    let settings = settings();
    assert_eq!(
      matching(&settings, held(true, false, false), "o"),
      Some(OutputAction::Switch {
        switch: OutputSwitch::Next,
        with_window: false,
      })
    );
    assert_eq!(
      matching(&settings, held(true, false, true), "o"),
      Some(OutputAction::Switch {
        switch: OutputSwitch::Next,
        with_window: true,
      })
    );
    assert_eq!(
      matching(&settings, held(true, false, false), "left"),
      Some(OutputAction::Switch {
        switch: OutputSwitch::Left,
        with_window: false,
      })
    );
    assert_eq!(
      matching(&settings, held(false, true, false), "2"),
      Some(OutputAction::ShowWset(2))
    );
    assert_eq!(
      matching(&settings, held(false, true, true), "3"),
      Some(OutputAction::SendToWset(3))
    );
    assert_eq!(matching(&settings, held(false, false, false), "o"), None);
    assert_eq!(matching(&settings, held(true, false, false), "right"), None);
  }
}
