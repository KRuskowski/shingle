//! Fake cards shaped like real ones: a [`FakeKms`] built from a
//! `drm_info -j` dump, trimmed by tools/native_test/drm_info_fixture.py
//! into crates/shingle-native/testdata/drm_info.
//!
//! The card gets the dump's connectors with their names, modes and
//! properties, its CRTCs and planes in the kernel's order with what each
//! of them showed, every plane's formats and `IN_FORMATS`, the cursor
//! size and async-flip caps, and every range property's range. What the
//! firmware or console left lit stays lit, so Shingle's first modeset
//! meets what it meets on the real machine.

use std::collections::HashMap;

use serde_json::Value;

use super::{DriverRules, FakeKms, connector_info, default_modes};
use crate::kms::{
  Connection, ConnectorId, ConnectorInfo, CrtcId, Mode, PlaneId, Subpixel,
  in_formats::tests::blob,
};

/// The Latitude 5400's GPU: i915 on an Intel UHD 620 (Whiskey Lake,
/// display version 9), from drmdb snapshot 25f142a0dd2b with the eDP
/// panel and two DP monitors lit.
pub(crate) fn whiskey_lake() -> FakeKms {
  load(
    include_str!("../../../testdata/drm_info/whiskey_lake_uhd620.json"),
    DriverRules::I915 { display_version: 9 },
  )
}

/// The Latitude 5450's GPU: i915 on Meteor Lake's Arc graphics (display
/// version 14), from drmdb snapshot 9ca19e791f9d: the eDP panel connected
/// but dark, and three monitors lit through a DP-MST dock.
pub(crate) fn meteor_lake() -> FakeKms {
  load(
    include_str!("../../../testdata/drm_info/meteor_lake_arc.json"),
    DriverRules::I915 {
      display_version: 14,
    },
  )
}

fn number(value: &Value, key: &str) -> u64 {
  value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn id(value: &Value) -> u32 {
  number(value, "id") as u32
}

fn list(value: &Value, key: &str) -> Vec<Value> {
  value
    .get(key)
    .and_then(Value::as_array)
    .cloned()
    .unwrap_or_default()
}

/// A mode of the fixture's layout.
fn mode(value: &Value) -> Mode {
  let short = |key| number(value, key) as u16;
  Mode {
    clock: number(value, "clock") as u32,
    hdisplay: short("hdisplay"),
    hsync_start: short("hsync_start"),
    hsync_end: short("hsync_end"),
    htotal: short("htotal"),
    hskew: short("hskew"),
    vdisplay: short("vdisplay"),
    vsync_start: short("vsync_start"),
    vsync_end: short("vsync_end"),
    vtotal: short("vtotal"),
    vscan: short("vscan"),
    vrefresh: number(value, "vrefresh") as u32,
    flags: number(value, "flags") as u32,
    kind: number(value, "type") as u32,
    name: value
      .get("name")
      .and_then(Value::as_str)
      .unwrap_or_default()
      .to_owned(),
  }
}

/// The CRTCs a `possible_crtcs` mask names, by index in `crtcs`.
fn crtcs_of(mask: u64, crtcs: &[CrtcId]) -> Vec<CrtcId> {
  crtcs
    .iter()
    .enumerate()
    .filter(|(index, _)| mask & (1 << index) != 0)
    .map(|(_, crtc)| *crtc)
    .collect()
}

/// drm_info's subpixel numbers (`DRM_MODE_SUBPIXEL_*`).
fn subpixel(value: u64) -> Subpixel {
  match value {
    2 => Subpixel::HorizontalRgb,
    3 => Subpixel::HorizontalBgr,
    4 => Subpixel::VerticalRgb,
    5 => Subpixel::VerticalBgr,
    6 => Subpixel::None,
    _ => Subpixel::Unknown,
  }
}

/// Records `object`'s properties in `fake`: their names, current values
/// (blob ids are left to the fake) and ranges.
fn properties(fake: &mut FakeKms, object: u32, value: &Value) {
  let Some(properties) = value.get("properties").and_then(Value::as_object)
  else {
    return;
  };
  let mut names = Vec::new();
  for (name, property) in properties {
    names.push(name.clone());
    if !matches!(name.as_str(), "IN_FORMATS" | "EDID" | "type") {
      fake.value_mut(object, name, number(property, "value"));
    }
    if let Some([min, max]) = property
      .get("range")
      .and_then(Value::as_array)
      .map(Vec::as_slice)
    {
      let bound = |value: &Value| value.as_u64().unwrap_or(0);
      fake
        .ranges
        .insert((object, name.clone()), (bound(min), bound(max)));
    }
  }
  fake.property_lists.insert(object, names);
}

/// A fake card from the fixture `json`, enforcing `rules`.
pub(crate) fn load(json: &str, rules: DriverRules) -> FakeKms {
  let card: Value = serde_json::from_str(json).expect("a fixture");
  let mut fake = FakeKms::single_output();
  fake.rules = Some(rules);
  // Ids the fake allocates must not meet the dump's object ids.
  fake.next_id = 100_000;
  let caps = card.get("caps").cloned().unwrap_or(Value::Null);
  fake.cursor_size = (
    number(&caps, "CURSOR_WIDTH") as u32,
    number(&caps, "CURSOR_HEIGHT") as u32,
  );
  // Kernels before 6.8 have no DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP; from then
  // i915 reports it as it reports the legacy cap.
  fake.async_flips = caps
    .get("ATOMIC_ASYNC_PAGE_FLIP")
    .and_then(Value::as_u64)
    .unwrap_or_else(|| number(&caps, "ASYNC_PAGE_FLIP"))
    == 1;
  fake.vrr = false;
  fake.vrr_capable = false;

  let crtcs: Vec<CrtcId> =
    list(&card, "crtcs").iter().map(|c| CrtcId(id(c))).collect();
  let encoders: HashMap<u64, u64> = list(&card, "encoders")
    .iter()
    .map(|e| (number(e, "id"), number(e, "possible_crtcs")))
    .collect();

  for (index, value) in list(&card, "crtcs").iter().enumerate() {
    let crtc = crtcs[index];
    properties(&mut fake, crtc.0, value);
    if let Some(current) = value.get("mode").filter(|m| !m.is_null()) {
      let blob = fake.alloc();
      fake.blobs.insert(blob, mode(current));
      fake.value_mut(crtc.0, "MODE_ID", u64::from(blob));
    }
    if index == 0 {
      fake.crtc_id = crtc;
    } else {
      fake.extra_crtcs.push(crtc);
    }
  }

  for (index, value) in list(&card, "connectors").iter().enumerate() {
    let connector = ConnectorId(id(value));
    properties(&mut fake, connector.0, value);
    let possible = list(value, "encoders")
      .iter()
      .filter_map(Value::as_u64)
      .map(|encoder| encoders.get(&encoder).copied().unwrap_or(0))
      .fold(0, |mask, crtcs| mask | crtcs);
    let name = value.get("name").and_then(Value::as_str).unwrap_or("?");
    let mut info: ConnectorInfo = connector_info(connector.0, name);
    info.internal = matches!(number(value, "type"), 7 | 14 | 16);
    info.connection = match number(value, "status") {
      1 => Connection::Connected,
      2 => Connection::Disconnected,
      _ => Connection::Unknown,
    };
    info.modes = list(value, "modes").iter().map(mode).collect();
    let size = list(value, "phy_size");
    info.physical_size_mm = (
      size.first().and_then(Value::as_u64).unwrap_or(0) as u32,
      size.get(1).and_then(Value::as_u64).unwrap_or(0) as u32,
    );
    info.subpixel = subpixel(number(value, "subpixel"));
    info.possible_crtcs = crtcs_of(possible, &crtcs);
    if index == 0 {
      fake.connector_id = connector;
      fake.connection = info.connection;
      fake.modes = info.modes.clone();
      fake.connector_crtcs = info.possible_crtcs.clone();
      fake.first_info = info;
    } else {
      fake.extra_connectors.push(info);
    }
  }

  for (index, value) in list(&card, "planes").iter().enumerate() {
    let plane = PlaneId(id(value));
    let kind = value
      .pointer("/properties/type/value")
      .and_then(Value::as_u64)
      .unwrap_or(0);
    properties(&mut fake, plane.0, value);
    fake.value_mut(plane.0, "CRTC_ID", number(value, "crtc_id"));
    fake.value_mut(plane.0, "FB_ID", number(value, "fb_id"));
    let formats: Vec<u32> = list(value, "formats")
      .iter()
      .filter_map(Value::as_u64)
      .map(|format| format as u32)
      .collect();
    let in_formats = in_formats_blob(value, &formats);
    if index == 0 {
      assert_eq!(kind, 1, "the first plane is CRTC 0's primary plane");
      fake.plane_id = plane;
      fake.plane_formats = formats;
      fake.in_formats = in_formats;
    } else {
      let possible = crtcs_of(number(value, "possible_crtcs"), &crtcs);
      fake.extra_planes.push((plane, kind, possible));
      fake.plane_format_lists.insert(plane, formats);
      if let Some(in_formats) = in_formats {
        fake.plane_in_formats.insert(plane, in_formats);
      }
    }
  }
  fake
}

/// The kernel-layout `IN_FORMATS` blob of a fixture plane, whose
/// `in_formats` lists `[modifier, [fourcc]]` over `formats`.
fn in_formats_blob(plane: &Value, formats: &[u32]) -> Option<Vec<u8>> {
  let entries = list(plane, "in_formats");
  if entries.is_empty() {
    return None;
  }
  let modifiers: Vec<(u64, u32, u64)> = entries
    .iter()
    .filter_map(|entry| {
      let modifier = entry.get(0)?.as_u64()?;
      let mut mask = 0_u64;
      for format in entry.get(1)?.as_array()? {
        let format = format.as_u64()? as u32;
        if let Some(index) = formats.iter().position(|f| *f == format) {
          mask |= 1 << index;
        }
      }
      Some((mask, 0, modifier))
    })
    .collect();
  Some(blob(formats, &modifiers))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::{FORMAT_XRGB8888, KmsDevice, Object, in_formats};

  #[test]
  fn the_whiskey_lake_card_has_the_dumps_objects() {
    let mut fake = whiskey_lake();
    let resources = fake.resources().unwrap();
    assert_eq!(resources.connectors.len(), 4);
    assert_eq!(resources.crtcs, vec![CrtcId(51), CrtcId(72), CrtcId(93)]);
    assert_eq!(resources.planes.len(), 9);
    let panel = fake.connector(ConnectorId(95)).unwrap();
    assert_eq!(panel.name, "eDP-1");
    assert!(panel.internal);
    assert_eq!(panel.current_crtc, Some(CrtcId(51)));
    assert_eq!(panel.modes.len(), 1);
    assert!(panel.modes[0].preferred());
    assert_eq!(panel.modes[0].refresh_mhz() / 1000, 60);
    let hdmi = fake.connector(ConnectorId(115)).unwrap();
    assert_eq!(hdmi.name, "HDMI-A-1");
    assert_eq!(hdmi.connection, Connection::Disconnected);
    assert_eq!(fake.cursor_size(), (256, 256));
    assert!(fake.async_page_flips());
    let props = fake.properties(Object::Connector(ConnectorId(95))).unwrap();
    let bpc = props.iter().find(|p| p.name == "max bpc").unwrap();
    assert_eq!((bpc.value, bpc.range), (12, Some((6, 12))));
    assert!(props.iter().all(|p| p.name != "vrr_capable"));
  }

  #[test]
  fn planes_keep_their_in_formats() {
    let mut fake = meteor_lake();
    let props = fake.properties(Object::Plane(PlaneId(90))).unwrap();
    let blob_id = props
      .iter()
      .find(|p| p.name == "IN_FORMATS")
      .map(|p| p.value)
      .unwrap();
    let parsed = in_formats::parse(&fake.blob(blob_id).unwrap()).unwrap();
    assert_eq!(
      parsed.modifiers(FORMAT_XRGB8888),
      vec![
        0x0100_0000_0000_000e,
        0x0100_0000_0000_000d,
        0x0100_0000_0000_000f,
        0x0100_0000_0000_0009,
        0x0100_0000_0000_0001,
        0,
      ]
    );
    let cursor = fake.plane(PlaneId(83)).unwrap();
    assert_eq!(cursor.possible_crtcs, vec![CrtcId(88)]);
  }

  #[test]
  fn the_single_output_card_keeps_its_defaults() {
    let fake = FakeKms::single_output();
    assert_eq!(fake.first_info.name, "HDMI-A-1");
    assert_eq!(fake.modes, default_modes());
  }
}
