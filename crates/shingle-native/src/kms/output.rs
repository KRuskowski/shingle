//! Probing a card and choosing what to light: the connector, its CRTC and
//! the CRTC's primary plane.
//!
//! [`probe`] reads every connector, CRTC and plane with their properties.
//! [`Card::choose_connector`] picks the output the way the design says
//! (docs/design/native-backend.md, section 5): a connector named by the
//! configuration, else the first connected internal panel, else the first
//! connected connector. [`Card::output`] finds its CRTC and primary plane
//! and collects what another session left on, for the modeset to turn
//! off.

use std::{fmt, io};

use super::{
  ConnectorId, ConnectorInfo, CrtcId, CrtcInfo, FORMAT_XRGB8888, KmsDevice,
  Mode, Object, PlaneId, PlaneInfo,
  atomic::{OutputProps, Teardown},
  edid::{self, Edid},
  in_formats::{self, InFormats},
  props::{
    ConnectorProps, CrtcProps, MissingProperty, PLANE_TYPE_PRIMARY, PlaneProps,
    PropertyTable,
  },
};
use crate::{crtcs, kms::Connection};

/// Everything read from a card in one probe.
#[derive(Clone, Debug, Default)]
pub struct Card {
  /// Every connector, in the kernel's order.
  pub connectors: Vec<(ConnectorInfo, PropertyTable)>,
  /// Every CRTC, in the kernel's index order.
  pub crtcs: Vec<(CrtcInfo, PropertyTable)>,
  /// Every plane.
  pub planes: Vec<(PlaneInfo, PropertyTable)>,
}

/// Reads every connector, CRTC and plane of `device`, probing connectors
/// for displays.
pub fn probe(device: &mut dyn KmsDevice) -> io::Result<Card> {
  let resources = device.resources()?;
  let mut card = Card::default();
  for id in resources.connectors {
    let mut info = device.connector(id)?;
    let table = PropertyTable::load(device, Object::Connector(id))?;
    if info.connection == Connection::Connected {
      info.edid = table
        .value("EDID")
        .filter(|blob| *blob != 0)
        .and_then(|blob| device.blob(blob).ok())
        .and_then(|blob| Edid::parse(&blob, &edid::system_manufacturer));
    }
    card.connectors.push((info, table));
  }
  for id in resources.crtcs {
    let info = device.crtc(id)?;
    let table = PropertyTable::load(device, Object::Crtc(id))?;
    card.crtcs.push((info, table));
  }
  for id in resources.planes {
    let info = device.plane(id)?;
    let table = PropertyTable::load(device, Object::Plane(id))?;
    card.planes.push((info, table));
  }
  Ok(card)
}

/// Why a connector cannot be lit.
#[derive(Debug)]
pub enum OutputError {
  /// None of its encoders can drive a CRTC that has a primary plane.
  NoCrtc(String),
  /// Every CRTC that can drive it drives another output.
  Busy(String),
  /// Its CRTC's primary plane cannot scan out XRGB8888.
  NoXrgb8888(String),
  /// An object lacks a property atomic modesetting needs.
  Missing(MissingProperty),
}

impl fmt::Display for OutputError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoCrtc(name) => {
        write!(formatter, "{name} has no CRTC with a primary plane")
      }
      Self::Busy(name) => {
        write!(formatter, "every CRTC that can drive {name} is in use")
      }
      Self::NoXrgb8888(name) => write!(
        formatter,
        "the primary plane for {name} cannot scan out XRGB8888"
      ),
      Self::Missing(missing) => missing.fmt(formatter),
    }
  }
}

impl std::error::Error for OutputError {}

impl From<MissingProperty> for OutputError {
  fn from(missing: MissingProperty) -> Self {
    Self::Missing(missing)
  }
}

/// A connector with the CRTC and primary plane that drive it.
#[derive(Clone, Debug)]
pub struct Output {
  /// The connector as probed.
  pub connector: ConnectorInfo,
  /// The property ids of connector, CRTC and plane.
  pub props: OutputProps,
  /// What other outputs and planes the modeset turns off.
  pub teardown: Teardown,
  /// The CRTC's mode before Shingle set one, the last fallback.
  pub current_mode: Option<Mode>,
  /// The primary plane's `IN_FORMATS`, if it has a readable one.
  pub in_formats: Option<InFormats>,
}

impl Card {
  /// The connector to light: the connected one named `wanted`, else the
  /// first connected internal panel, else the first connected connector.
  pub fn choose_connector(
    &self,
    wanted: Option<&str>,
  ) -> Option<&ConnectorInfo> {
    let connected = || {
      self
        .connectors
        .iter()
        .map(|(info, _)| info)
        .filter(|info| info.connection == Connection::Connected)
    };
    wanted
      .and_then(|name| connected().find(|info| info.name == name))
      .or_else(|| connected().find(|info| info.internal))
      .or_else(|| connected().next())
  }

  /// The connector with `id`, if the card has it.
  pub fn connector(&self, id: ConnectorId) -> Option<&ConnectorInfo> {
    self
      .connectors
      .iter()
      .map(|(info, _)| info)
      .find(|info| info.id == id)
  }

  fn primary_plane(&self, crtc: CrtcId) -> Option<&(PlaneInfo, PropertyTable)> {
    let mut primaries = self.planes.iter().filter(|(info, table)| {
      table.value("type") == Some(PLANE_TYPE_PRIMARY)
        && info.possible_crtcs.contains(&crtc)
    });
    let first = primaries.clone().next();
    primaries
      .find(|(info, _)| info.current_crtc == Some(crtc))
      .or(first)
  }

  /// Finds the CRTC and primary plane for `connector`: the CRTC it is on
  /// now if that one can drive it, else the first possible CRTC with a
  /// primary plane, and that CRTC's primary plane, preferring the one
  /// attached to it already.
  pub fn output(
    &self,
    connector: &ConnectorInfo,
    reading: &mut dyn FnMut(u64) -> Option<Vec<u8>>,
  ) -> Result<Output, OutputError> {
    let usable = |crtc: &CrtcId| {
      connector.possible_crtcs.contains(crtc)
        && self.primary_plane(*crtc).is_some()
    };
    let crtc = connector
      .current_crtc
      .filter(usable)
      .or_else(|| {
        self
          .crtcs
          .iter()
          .map(|(info, _)| info.id)
          .find(|crtc| usable(crtc))
      })
      .ok_or_else(|| OutputError::NoCrtc(connector.name.clone()))?;
    let Some((plane, _)) = self.primary_plane(crtc) else {
      return Err(OutputError::NoCrtc(connector.name.clone()));
    };
    self.output_on(connector, crtc, plane.id, reading)
  }

  /// The CRTC and primary plane for each of `wants`, in priority order,
  /// or `None` for those that stay dark: lit outputs keep theirs, and the
  /// others share the CRTCs left as [`crtcs::match_crtcs`] matches them,
  /// each with a primary plane that can scan out XRGB8888 and that no
  /// other output uses, preferring the one attached to its CRTC already.
  pub fn allocate(
    &self,
    wants: &[Wanted<'_>],
  ) -> Vec<Option<(CrtcId, PlaneId)>> {
    let index =
      |crtc: CrtcId| self.crtcs.iter().position(|(info, _)| info.id == crtc);
    let scanout = |crtc: CrtcId| {
      self.planes.iter().any(|(info, table)| {
        table.value("type") == Some(PLANE_TYPE_PRIMARY)
          && info.possible_crtcs.contains(&crtc)
          && info.formats.contains(&FORMAT_XRGB8888)
      })
    };
    let matching: Vec<crtcs::Want> = wants
      .iter()
      .map(|want| crtcs::Want {
        possible: want
          .connector
          .possible_crtcs
          .iter()
          .copied()
          .filter(|crtc| scanout(*crtc))
          .filter_map(index)
          .collect(),
        current: want.connector.current_crtc.and_then(index),
        lit: want.lit.and_then(|(crtc, _)| index(crtc)),
      })
      .collect();
    let matched = crtcs::match_crtcs(&matching, self.crtcs.len());
    let mut taken: Vec<PlaneId> = wants
      .iter()
      .filter_map(|want| want.lit)
      .map(|(_, p)| p)
      .collect();
    let mut allocated = Vec::with_capacity(wants.len());
    for (want, crtc) in wants.iter().zip(matched) {
      let crtc = crtc.map(|index| self.crtcs[index].0.id);
      let pair = match (want.lit, crtc) {
        (Some(lit), Some(crtc)) if lit.0 == crtc => Some(lit),
        (_, Some(crtc)) => self.free_primary(crtc, &taken).map(|plane| {
          taken.push(plane);
          (crtc, plane)
        }),
        (_, None) => None,
      };
      allocated.push(pair);
    }
    allocated
  }

  /// A primary plane of `crtc` that can scan out XRGB8888 and is not in
  /// `taken`, preferring the one attached to `crtc` already.
  fn free_primary(&self, crtc: CrtcId, taken: &[PlaneId]) -> Option<PlaneId> {
    let mut free = self.planes.iter().filter(|(info, table)| {
      table.value("type") == Some(PLANE_TYPE_PRIMARY)
        && info.possible_crtcs.contains(&crtc)
        && info.formats.contains(&FORMAT_XRGB8888)
        && !taken.contains(&info.id)
    });
    let first = free.clone().next().map(|(info, _)| info.id);
    free
      .find(|(info, _)| info.current_crtc == Some(crtc))
      .map(|(info, _)| info.id)
      .or(first)
  }

  /// `connector` driven by `crtc` through primary plane `plane`, which
  /// the caller chose ([`Card::output`], [`Card::allocate`]). Its
  /// teardown is everything else that is lit.
  pub fn output_on(
    &self,
    connector: &ConnectorInfo,
    crtc: CrtcId,
    plane: PlaneId,
    reading: &mut dyn FnMut(u64) -> Option<Vec<u8>>,
  ) -> Result<Output, OutputError> {
    let Some((plane, plane_table)) =
      self.planes.iter().find(|(info, _)| info.id == plane)
    else {
      return Err(OutputError::NoCrtc(connector.name.clone()));
    };
    if !plane.formats.contains(&FORMAT_XRGB8888) {
      return Err(OutputError::NoXrgb8888(connector.name.clone()));
    }
    let (crtc_info, crtc_table) = self
      .crtcs
      .iter()
      .find(|(info, _)| info.id == crtc)
      .ok_or_else(|| OutputError::NoCrtc(connector.name.clone()))?;
    let connector_table = self
      .connectors
      .iter()
      .find(|(info, _)| info.id == connector.id)
      .map(|(_, table)| table)
      .ok_or_else(|| OutputError::NoCrtc(connector.name.clone()))?;
    let props = OutputProps {
      connector: ConnectorProps::new(connector.id, connector_table)?,
      crtc: CrtcProps::new(crtc, crtc_table)?,
      plane: PlaneProps::new(plane.id, plane_table)?,
    };
    let in_formats = plane_table
      .value("IN_FORMATS")
      .filter(|blob| *blob != 0)
      .and_then(reading)
      .and_then(|blob| in_formats::parse(&blob).ok());
    Ok(Output {
      connector: connector.clone(),
      teardown: self.teardown(&props)?,
      props,
      current_mode: crtc_info.mode.clone(),
      in_formats,
    })
  }

  /// Everything lit by someone else: other connectors on a CRTC, other
  /// active CRTCs, and planes on a CRTC other than our primary plane.
  fn teardown(&self, ours: &OutputProps) -> Result<Teardown, MissingProperty> {
    self.teardown_except(&[ours])
  }

  /// Everything this probe saw lit that none of `ours` uses: connectors on
  /// a CRTC, active CRTCs, and planes on a CRTC, including the cursor and
  /// overlay planes of our CRTCs. A modeset of all of `ours` turns these
  /// off, so only Shingle's outputs stay lit.
  pub fn teardown_except(
    &self,
    ours: &[&OutputProps],
  ) -> Result<Teardown, MissingProperty> {
    let mut teardown = Teardown::default();
    for (info, table) in &self.connectors {
      if ours.iter().all(|ours| info.id != ours.connector.id)
        && table.value("CRTC_ID").is_some_and(|crtc| crtc != 0)
      {
        teardown
          .connectors
          .push(ConnectorProps::new(info.id, table)?);
      }
    }
    for (info, table) in &self.crtcs {
      if ours.iter().all(|ours| info.id != ours.crtc.id)
        && table.value("ACTIVE").is_some_and(|active| active != 0)
      {
        teardown.crtcs.push(CrtcProps::new(info.id, table)?);
      }
    }
    for (info, table) in &self.planes {
      if ours.iter().all(|ours| info.id != ours.plane.id)
        && table.value("CRTC_ID").is_some_and(|crtc| crtc != 0)
      {
        teardown.planes.push(PlaneProps::new(info.id, table)?);
      }
    }
    Ok(teardown)
  }
}

/// A connector [`Card::allocate`] finds a CRTC and plane for.
#[derive(Clone, Copy, Debug)]
pub struct Wanted<'a> {
  /// The connector.
  pub connector: &'a ConnectorInfo,
  /// The CRTC and primary plane that drive it, if it is lit; it keeps
  /// them.
  pub lit: Option<(CrtcId, PlaneId)>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::{
    MODIFIER_LINEAR,
    fake::{FakeKms, connector_info},
  };

  #[test]
  fn connected_internal_panels_win_unless_a_name_is_wanted() {
    let mut device = FakeKms::single_output();
    let mut panel = connector_info(device.connector_id.0 + 100, "eDP-1");
    panel.internal = true;
    let mut off = connector_info(device.connector_id.0 + 101, "DP-2");
    off.connection = Connection::Disconnected;
    device.add_connector(off);
    device.add_connector(panel);
    let card = probe(&mut device).unwrap();
    assert_eq!(card.choose_connector(None).unwrap().name, "eDP-1");
    assert_eq!(
      card.choose_connector(Some("HDMI-A-1")).unwrap().name,
      "HDMI-A-1"
    );
    assert_eq!(card.choose_connector(Some("DP-2")).unwrap().name, "eDP-1");
  }

  #[test]
  fn nothing_connected_chooses_nothing() {
    let mut device = FakeKms::single_output();
    device.disconnect();
    let card = probe(&mut device).unwrap();
    assert!(card.choose_connector(None).is_none());
  }

  #[test]
  fn the_output_uses_the_primary_plane_of_a_possible_crtc() {
    let mut device = FakeKms::single_output();
    let card = probe(&mut device).unwrap();
    let connector = card.choose_connector(None).unwrap();
    let output = card.output(connector, &mut |_| None).unwrap();
    assert_eq!(output.props.crtc.id, device.crtc_id);
    assert_eq!(output.props.plane.id, device.plane_id);
    assert_eq!(output.teardown, Teardown::default());
    assert!(output.current_mode.is_none());
  }

  #[test]
  fn what_the_console_left_on_elsewhere_is_torn_down() {
    let mut device = FakeKms::single_output();
    let (other_connector, other_crtc, cursor) = device.add_lit_neighbours();
    let card = probe(&mut device).unwrap();
    let connector = card.choose_connector(Some("HDMI-A-1")).unwrap();
    let output = card.output(connector, &mut |_| None).unwrap();
    let ids = |teardown: &Teardown| {
      (
        teardown.connectors.iter().map(|c| c.id).collect::<Vec<_>>(),
        teardown.crtcs.iter().map(|c| c.id).collect::<Vec<_>>(),
        teardown.planes.iter().map(|p| p.id).collect::<Vec<_>>(),
      )
    };
    assert_eq!(
      ids(&output.teardown),
      (vec![other_connector], vec![other_crtc], vec![cursor])
    );
  }

  fn wanted(card: &Card, name: &str) -> ConnectorInfo {
    card
      .connectors
      .iter()
      .map(|(info, _)| info.clone())
      .find(|info| info.name == name)
      .expect("a known connector")
  }

  #[test]
  fn each_monitor_gets_a_crtc_and_primary_plane_of_its_own() {
    let mut device = FakeKms::single_output();
    let dp_crtc = device.add_pipe(connector_info(20, "DP-1"));
    // A third connector that only the first CRTC can drive.
    device.add_connector(connector_info(21, "DP-2"));
    let card = probe(&mut device).unwrap();
    let (hdmi, dp1, dp2) = (
      wanted(&card, "HDMI-A-1"),
      wanted(&card, "DP-1"),
      wanted(&card, "DP-2"),
    );
    let want = |connector| Wanted {
      connector,
      lit: None,
    };
    let allocated = card.allocate(&[want(&hdmi), want(&dp1), want(&dp2)]);
    assert_eq!(allocated[0], Some((device.crtc_id, device.plane_id)));
    assert_eq!(allocated[1].map(|(crtc, _)| crtc), Some(dp_crtc));
    assert_ne!(allocated[1].map(|(_, plane)| plane), Some(device.plane_id));
    assert_eq!(allocated[2], None, "its only CRTC is taken");
    // Lit outputs keep theirs: DP-2, lit on the first CRTC, keeps it.
    let lit = Wanted {
      connector: &dp2,
      lit: Some((device.crtc_id, device.plane_id)),
    };
    let allocated = card.allocate(&[want(&hdmi), lit]);
    assert_eq!(allocated, [None, Some((device.crtc_id, device.plane_id))]);
  }

  #[test]
  fn the_teardown_of_several_outputs_spares_all_of_them() {
    let mut device = FakeKms::single_output();
    let (other_connector, other_crtc, cursor) = device.add_lit_neighbours();
    let card = probe(&mut device).unwrap();
    let hdmi = card
      .output(&wanted(&card, "HDMI-A-1"), &mut |_| None)
      .unwrap();
    let teardown = card.teardown_except(&[&hdmi.props]).unwrap();
    assert_eq!(teardown, hdmi.teardown);
    assert_eq!(teardown.connectors[0].id, other_connector);
    // With DP-1's connector and CRTC in use too, only the cursor plane
    // is left to turn off.
    let mut dp = hdmi.props.clone();
    dp.connector.id = other_connector;
    dp.crtc.id = other_crtc;
    let teardown = card.teardown_except(&[&hdmi.props, &dp]).unwrap();
    assert!(teardown.connectors.is_empty() && teardown.crtcs.is_empty());
    assert_eq!(
      teardown.planes.iter().map(|p| p.id).collect::<Vec<_>>(),
      [cursor]
    );
  }

  #[test]
  fn a_plane_without_xrgb8888_is_refused() {
    let mut device = FakeKms::single_output();
    device.plane_formats = vec![u32::from_le_bytes(*b"NV12")];
    let card = probe(&mut device).unwrap();
    let connector = card.choose_connector(None).unwrap();
    let error = card.output(connector, &mut |_| None).unwrap_err();
    assert!(matches!(error, OutputError::NoXrgb8888(_)), "{error}");
  }

  #[test]
  fn a_connector_without_a_usable_crtc_is_refused() {
    let mut device = FakeKms::single_output();
    device.connector_crtcs = Vec::new();
    let card = probe(&mut device).unwrap();
    let connector = card.choose_connector(None).unwrap();
    let error = card.output(connector, &mut |_| None).unwrap_err();
    assert_eq!(
      error.to_string(),
      "HDMI-A-1 has no CRTC with a primary plane"
    );
  }

  #[test]
  fn in_formats_is_read_from_its_blob() {
    let mut device = FakeKms::single_output();
    device.in_formats = Some(crate::kms::in_formats::tests::blob(
      &[FORMAT_XRGB8888],
      &[(1, 0, MODIFIER_LINEAR)],
    ));
    let card = probe(&mut device).unwrap();
    let connector = card.choose_connector(None).unwrap().clone();
    let output = card
      .output(&connector, &mut |blob| device.blob(blob).ok())
      .unwrap();
    let formats = output.in_formats.expect("IN_FORMATS was parsed");
    assert!(formats.supports(FORMAT_XRGB8888, MODIFIER_LINEAR));
  }
}
