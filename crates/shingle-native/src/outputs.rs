//! Which output to light and how: the configuration's per-connector modes,
//! the choice among connected connectors, the diff between two probes and
//! the modes clients may ask for.
//!
//! [`OutputPolicy`] holds the `mode` of every `[output:NAME]` entry
//! (docs/design/native-backend.md, section 5). [`choose`] picks the
//! connector: the one already lit if it is still connected, else a
//! connector the configuration names, else an internal panel, else the
//! first connected one, never one whose mode is `off`. [`snapshot`] and
//! [`diff`] tell what a hotplug changed. [`requested_mode`] finds the mode a
//! wlr-output-management client asked for, and [`mode_list`] describes the
//! connector's modes to clients.

use shingle::{OutputModeInfo, RequestedMode};
use shingle_config::{OutputMode, ShingleConfig};

use crate::kms::{
  Connection, ConnectorInfo, Mode,
  modes::{self, ModeChoice},
  output::Card,
};

/// The configured mode of each output, by connector name.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutputPolicy {
  /// The mode of outputs without their own entry.
  pub default: OutputMode,
  /// The named entries in configuration order.
  pub named: Vec<(String, OutputMode)>,
}

impl OutputPolicy {
  /// The modes of `config`'s default output and named outputs.
  pub fn from_config(config: &ShingleConfig) -> Self {
    Self {
      default: config.output.mode,
      named: config
        .outputs
        .iter()
        .filter_map(|output| Some((output.name.clone()?, output.mode)))
        .collect(),
    }
  }

  /// The configured mode of the output `name`.
  pub fn mode(&self, name: &str) -> OutputMode {
    self
      .named
      .iter()
      .find(|(named, _)| named == name)
      .map_or(self.default, |(_, mode)| *mode)
  }

  /// How to choose the mode of output `name`, or `None` if it is off.
  pub fn choice(&self, name: &str) -> Option<ModeChoice> {
    mode_choice(self.mode(name))
  }

  /// The mode `choice` would light first on `connector`.
  pub fn preferred_mode(&self, connector: &ConnectorInfo) -> Option<Mode> {
    let choice = self.choice(&connector.name)?;
    first_mode(connector, choice)
  }
}

/// How to choose a mode configured as `mode`, or `None` for `off`.
pub fn mode_choice(mode: OutputMode) -> Option<ModeChoice> {
  match mode {
    OutputMode::Off => None,
    OutputMode::Auto => Some(ModeChoice::Auto),
    OutputMode::HighRes => Some(ModeChoice::HighRes),
    OutputMode::HighRr => Some(ModeChoice::HighRr),
    OutputMode::Explicit {
      width,
      height,
      refresh_mhz,
    } => Some(ModeChoice::Explicit {
      width,
      height,
      refresh_mhz,
    }),
  }
}

/// The mode `choice` lights first on `connector`.
pub fn first_mode(
  connector: &ConnectorInfo,
  choice: ModeChoice,
) -> Option<Mode> {
  modes::candidates(&connector.modes, choice, None)
    .into_iter()
    .next()
}

/// The connected connectors to light, in priority order, as multi-output
/// lights them (docs/design/multi-output.md, section 7, "Decision:
/// native"): those in `lit` (in its order), then those the configuration
/// names (in its order), then internal panels, then the rest in the
/// card's order, skipping every connector for which `off` is true. The
/// caller lights as many as it has CRTCs for, from the front.
pub fn rank<'a>(
  card: &'a Card,
  policy: &OutputPolicy,
  lit: &[&str],
  off: &dyn Fn(&str) -> bool,
) -> Vec<&'a ConnectorInfo> {
  let usable: Vec<&ConnectorInfo> = card
    .connectors
    .iter()
    .map(|(info, _)| info)
    .filter(|info| info.connection == Connection::Connected)
    .filter(|info| !off(&info.name))
    .collect();
  let named =
    |name: &str| usable.iter().copied().find(|info| info.name == name);
  let mut ranked: Vec<&ConnectorInfo> = Vec::new();
  let mut push = |info: &'a ConnectorInfo| {
    if !ranked.iter().any(|known| known.id == info.id) {
      ranked.push(info);
    }
  };
  lit
    .iter()
    .filter_map(|name| named(name))
    .for_each(&mut push);
  policy
    .named
    .iter()
    .filter_map(|(name, _)| named(name))
    .for_each(&mut push);
  usable
    .iter()
    .copied()
    .filter(|info| info.internal)
    .for_each(&mut push);
  usable.iter().copied().for_each(&mut push);
  ranked
}

/// The connector to light: `keep` if it is still connected and not off,
/// else the first connected connector the configuration names (in its
/// order), else the first connected internal panel, else the first
/// connected connector, skipping those configured `off`.
pub fn choose<'a>(
  card: &'a Card,
  policy: &OutputPolicy,
  keep: Option<&str>,
) -> Option<&'a ConnectorInfo> {
  let keep: Vec<&str> = keep.into_iter().collect();
  let off = |name: &str| policy.choice(name).is_none();
  rank(card, policy, &keep, &off).into_iter().next()
}

/// A connector as one probe saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorState {
  /// Its name, such as `DP-1`.
  pub name: String,
  /// Whether a display is attached.
  pub connected: bool,
  /// The display's modes.
  pub modes: Vec<Mode>,
}

/// What a probe saw of each connector.
pub fn snapshot(card: &Card) -> Vec<ConnectorState> {
  card
    .connectors
    .iter()
    .map(|(info, _)| ConnectorState {
      name: info.name.clone(),
      connected: info.connection == Connection::Connected,
      modes: info.modes.clone(),
    })
    .collect()
}

/// A change between two probes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectorChange {
  /// A display was plugged into the connector.
  Connected(String),
  /// The connector's display was unplugged, or the connector went away.
  Disconnected(String),
  /// The connector's display stayed but offers other modes, as when a
  /// monitor is swapped while away.
  ModesChanged(String),
}

/// The changes from probe `before` to probe `after`, in `after`'s order
/// and then for connectors that went away.
pub fn diff(
  before: &[ConnectorState],
  after: &[ConnectorState],
) -> Vec<ConnectorChange> {
  let mut changes = Vec::new();
  for now in after {
    let then = before.iter().find(|then| then.name == now.name);
    let was_connected = then.is_some_and(|then| then.connected);
    match (was_connected, now.connected) {
      (false, true) => {
        changes.push(ConnectorChange::Connected(now.name.clone()))
      }
      (true, false) => {
        changes.push(ConnectorChange::Disconnected(now.name.clone()))
      }
      (true, true) if then.is_some_and(|then| then.modes != now.modes) => {
        changes.push(ConnectorChange::ModesChanged(now.name.clone()))
      }
      _ => {}
    }
  }
  for then in before {
    if then.connected && !after.iter().any(|now| now.name == then.name) {
      changes.push(ConnectorChange::Disconnected(then.name.clone()));
    }
  }
  changes
}

/// The mode a client asked for among `modes`: a listed one by index, or
/// for a custom mode the one of that size and refresh rate. A custom mode
/// without a refresh rate takes the preferred, else the fastest, mode of
/// that size. KMS cannot drive a mode the display does not list, so
/// anything else is `None`.
pub fn requested_mode(modes: &[Mode], request: RequestedMode) -> Option<Mode> {
  match request {
    RequestedMode::Listed(index) => modes.get(index).cloned(),
    RequestedMode::Custom {
      width,
      height,
      refresh_mhz,
    } => modes::find_matching_mode(modes, width, height, refresh_mhz, true)
      .or_else(|| {
        (refresh_mhz == 0)
          .then(|| modes::find_matching_mode(modes, width, height, 0, false))
          .flatten()
      })
      .cloned(),
  }
}

/// `modes` as clients see them.
pub fn mode_list(modes: &[Mode]) -> Vec<OutputModeInfo> {
  modes
    .iter()
    .map(|mode| {
      let (width, height) = mode.size();
      OutputModeInfo {
        width,
        height,
        refresh_mhz: mode.refresh_mhz(),
        preferred: mode.preferred(),
      }
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::{
    fake::{FakeKms, connector_info, mode},
    output::probe,
  };

  fn policy(named: &[(&str, &str)]) -> OutputPolicy {
    OutputPolicy {
      default: OutputMode::Auto,
      named: named
        .iter()
        .map(|(name, mode)| ((*name).to_owned(), mode.parse().unwrap()))
        .collect(),
    }
  }

  /// HDMI-A-1, a DP-1 monitor and an internal eDP-1 panel, all connected.
  fn card() -> Card {
    let mut device = FakeKms::single_output();
    device.add_connector(connector_info(10, "DP-1"));
    let mut panel = connector_info(11, "eDP-1");
    panel.internal = true;
    device.add_connector(panel);
    probe(&mut device).unwrap()
  }

  fn chosen(card: &Card, policy: &OutputPolicy, keep: Option<&str>) -> String {
    choose(card, policy, keep)
      .map_or_else(String::new, |info| info.name.clone())
  }

  #[test]
  fn the_lit_output_stays_then_named_then_internal_then_first() {
    let card = card();
    let none = policy(&[]);
    assert_eq!(chosen(&card, &none, None), "eDP-1");
    assert_eq!(chosen(&card, &none, Some("DP-1")), "DP-1");
    let named = policy(&[("DP-9", "auto"), ("DP-1", "highres")]);
    assert_eq!(chosen(&card, &named, None), "DP-1");
    assert_eq!(chosen(&card, &named, Some("HDMI-A-1")), "HDMI-A-1");
  }

  #[test]
  fn connectors_rank_lit_then_named_then_internal_then_card_order() {
    let mut device = FakeKms::single_output();
    device.add_connector(connector_info(10, "DP-1"));
    let mut panel = connector_info(11, "eDP-1");
    panel.internal = true;
    device.add_connector(panel);
    device.add_connector(connector_info(12, "DP-2"));
    let mut gone = connector_info(13, "DP-3");
    gone.connection = Connection::Disconnected;
    device.add_connector(gone);
    let card = probe(&mut device).unwrap();
    let names = |lit: &[&str], policy: &OutputPolicy, off: &[&str]| {
      let off = |name: &str| off.contains(&name);
      rank(&card, policy, lit, &off)
        .into_iter()
        .map(|info| info.name.clone())
        .collect::<Vec<_>>()
    };
    let none = policy(&[]);
    assert_eq!(
      names(&[], &none, &[]),
      ["eDP-1", "HDMI-A-1", "DP-1", "DP-2"]
    );
    let named = policy(&[("DP-2", "auto"), ("DP-9", "auto")]);
    assert_eq!(
      names(&["DP-1"], &named, &[]),
      ["DP-1", "DP-2", "eDP-1", "HDMI-A-1"]
    );
    assert_eq!(
      names(&["DP-3"], &named, &["eDP-1", "DP-2"]),
      ["HDMI-A-1", "DP-1"],
      "disconnected and off connectors are left out"
    );
  }

  #[test]
  fn outputs_configured_off_are_never_lit() {
    let card = card();
    let off = policy(&[("eDP-1", "off"), ("DP-1", "off")]);
    assert_eq!(chosen(&card, &off, Some("DP-1")), "HDMI-A-1");
    let all_off = OutputPolicy {
      default: OutputMode::Off,
      named: Vec::new(),
    };
    assert_eq!(chosen(&card, &all_off, None), "");
  }

  #[test]
  fn the_policy_maps_wayfire_modes_to_choices() {
    let policy = policy(&[("DP-1", "2560x1440@144"), ("DP-2", "off")]);
    assert_eq!(
      policy.choice("DP-1"),
      Some(ModeChoice::Explicit {
        width: 2560,
        height: 1440,
        refresh_mhz: 144_000
      })
    );
    assert_eq!(policy.choice("DP-2"), None);
    assert_eq!(policy.choice("HDMI-A-1"), Some(ModeChoice::Auto));
    let mut config = ShingleConfig::default();
    config.output.mode = OutputMode::HighRr;
    config.outputs.push(shingle_config::OutputConfig {
      mode: OutputMode::Off,
      ..shingle_config::OutputConfig::named("eDP-1")
    });
    let from_config = OutputPolicy::from_config(&config);
    assert_eq!(from_config.mode("eDP-1"), OutputMode::Off);
    assert_eq!(from_config.mode("DP-3"), OutputMode::HighRr);
  }

  fn state(name: &str, connected: bool, modes: Vec<Mode>) -> ConnectorState {
    ConnectorState {
      name: name.into(),
      connected,
      modes,
    }
  }

  #[test]
  fn diffing_finds_plugs_unplugs_and_new_modes() {
    let monitor = vec![mode(1920, 1080, 60, true)];
    let other = vec![mode(2560, 1440, 60, true)];
    let before = [
      state("HDMI-A-1", true, monitor.clone()),
      state("DP-1", false, Vec::new()),
      state("DP-2", true, monitor.clone()),
      state("DP-3", true, monitor.clone()),
    ];
    let after = [
      state("HDMI-A-1", false, Vec::new()),
      state("DP-1", true, other.clone()),
      state("DP-2", true, other),
    ];
    assert_eq!(
      diff(&before, &after),
      [
        ConnectorChange::Disconnected("HDMI-A-1".into()),
        ConnectorChange::Connected("DP-1".into()),
        ConnectorChange::ModesChanged("DP-2".into()),
        ConnectorChange::Disconnected("DP-3".into()),
      ]
    );
    assert!(diff(&after, &after).is_empty());
  }

  #[test]
  fn requested_modes_must_be_listed() {
    let modes = vec![
      mode(1920, 1080, 60, true),
      mode(1920, 1080, 144, false),
      mode(1280, 720, 60, false),
    ];
    let custom = |width, height, hz: u32| RequestedMode::Custom {
      width,
      height,
      refresh_mhz: hz * 1000,
    };
    assert_eq!(
      requested_mode(&modes, RequestedMode::Listed(1)),
      Some(modes[1].clone())
    );
    assert_eq!(requested_mode(&modes, RequestedMode::Listed(3)), None);
    assert_eq!(
      requested_mode(&modes, custom(1920, 1080, 144)),
      Some(modes[1].clone())
    );
    assert_eq!(requested_mode(&modes, custom(1920, 1080, 75)), None);
    assert_eq!(
      requested_mode(&modes, custom(1920, 1080, 0)),
      Some(modes[0].clone()),
      "without a refresh, the preferred mode of the size"
    );
    assert_eq!(
      requested_mode(&modes, custom(1280, 720, 0)),
      Some(modes[2].clone())
    );
    assert_eq!(requested_mode(&modes, custom(800, 600, 0)), None);
  }

  #[test]
  fn the_mode_list_carries_size_refresh_and_preference() {
    let list =
      mode_list(&[mode(1920, 1080, 60, true), mode(1280, 720, 50, false)]);
    assert_eq!(
      list,
      [
        OutputModeInfo {
          width: 1920,
          height: 1080,
          refresh_mhz: 60_000,
          preferred: true
        },
        OutputModeInfo {
          width: 1280,
          height: 720,
          refresh_mhz: 50_000,
          preferred: false
        },
      ]
    );
  }
}
