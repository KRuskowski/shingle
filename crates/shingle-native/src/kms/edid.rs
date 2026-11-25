//! Make, model and serial number from a connector's EDID.
//!
//! wlroots, and so Wayfire, describes an output with libdisplay-info's
//! `di_info_get_make`, `di_info_get_model` and `di_info_get_serial`. This
//! module reads the same fields from the EDID base block:
//!
//! - **make**: the manufacturer's PNP id (bytes 8 and 9) looked up in the
//!   PNP id list (hwdata's `pnp.ids`), else the three-letter id itself;
//! - **model**: the display product name descriptor (tag `0xFC`), else the
//!   product code as `0x%04X`;
//! - **serial**: the display serial number descriptor (tag `0xFF`), else a
//!   non-zero serial number field as `0x%08X`.
//!
//! Without a readable EDID, wlroots reports `Unknown` make and model and no
//! serial, and so does [`Edid::unknown`].

use std::{collections::HashMap, path::Path, sync::OnceLock};

/// The EDID header.
const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// Where hwdata installs the PNP id list.
const PNP_IDS: &str = "/usr/share/hwdata/pnp.ids";

/// What an EDID says about the display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Edid {
  /// Manufacturer name, or its PNP id.
  pub make: String,
  /// Product name, or product code.
  pub model: String,
  /// Serial number, if the EDID has one.
  pub serial: Option<String>,
}

impl Edid {
  /// What wlroots reports without an EDID.
  pub fn unknown() -> Self {
    Self {
      make: "Unknown".into(),
      model: "Unknown".into(),
      serial: None,
    }
  }

  /// Parses an EDID blob, naming the manufacturer with `manufacturer`,
  /// which maps a PNP id such as `DEL` to a name. Returns `None` for a
  /// blob shorter than the 128-byte base block or without the EDID header.
  pub fn parse(
    blob: &[u8],
    manufacturer: &dyn Fn(&str) -> Option<String>,
  ) -> Option<Self> {
    let base = blob.get(..128)?;
    if base[..8] != HEADER {
      return None;
    }
    let id = u16::from_be_bytes([base[8], base[9]]);
    let letter = |shift: u16| {
      let code = ((id >> shift) & 0x1f) as u8;
      (b'A' - 1 + code) as char
    };
    let pnp: String = [letter(10), letter(5), letter(0)].into_iter().collect();
    let product = u16::from_le_bytes([base[10], base[11]]);
    let serial_number =
      u32::from_le_bytes([base[12], base[13], base[14], base[15]]);
    let mut name = None;
    let mut serial = None;
    for descriptor in base[54..126].chunks_exact(18) {
      // Display descriptors start with a zero pixel clock.
      if descriptor[0] != 0 || descriptor[1] != 0 || descriptor[2] != 0 {
        continue;
      }
      match descriptor[3] {
        0xfc => name = name.or_else(|| descriptor_text(&descriptor[5..])),
        0xff => serial = serial.or_else(|| descriptor_text(&descriptor[5..])),
        _ => {}
      }
    }
    Some(Self {
      make: manufacturer(&pnp).unwrap_or(pnp),
      model: name.unwrap_or_else(|| format!("0x{product:04X}")),
      serial: serial.or_else(|| {
        (serial_number != 0).then(|| format!("0x{serial_number:08X}"))
      }),
    })
  }
}

/// The text of a display descriptor: up to 13 bytes, ended by a newline
/// and padded with spaces. Empty text counts as none.
fn descriptor_text(bytes: &[u8]) -> Option<String> {
  let end = bytes
    .iter()
    .position(|byte| *byte == b'\n' || *byte == 0)
    .unwrap_or(bytes.len());
  let text: String = bytes[..end]
    .iter()
    .map(|byte| {
      if byte.is_ascii_graphic() || *byte == b' ' {
        *byte as char
      } else {
        '?'
      }
    })
    .collect();
  let text = text.trim_end();
  (!text.is_empty()).then(|| text.to_owned())
}

/// Reads a `pnp.ids` list: one `ID<TAB>Name` per line.
pub fn parse_pnp_ids(text: &str) -> HashMap<String, String> {
  text
    .lines()
    .filter_map(|line| {
      let (id, name) = line.split_once('\t')?;
      (id.len() == 3).then(|| (id.to_owned(), name.trim().to_owned()))
    })
    .collect()
}

/// The manufacturer named by `pnp` in hwdata's list, read once.
pub fn system_manufacturer(pnp: &str) -> Option<String> {
  static TABLE: OnceLock<HashMap<String, String>> = OnceLock::new();
  TABLE
    .get_or_init(|| {
      std::fs::read_to_string(Path::new(PNP_IDS))
        .map(|text| parse_pnp_ids(&text))
        .unwrap_or_default()
    })
    .get(pnp)
    .cloned()
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;

  /// An EDID base block for PNP id `pnp`, product `product` and serial
  /// number `serial`, with the given display descriptors.
  pub(crate) fn blob(
    pnp: &str,
    product: u16,
    serial: u32,
    descriptors: &[(u8, &str)],
  ) -> Vec<u8> {
    let mut edid = vec![0u8; 128];
    edid[..8].copy_from_slice(&HEADER);
    let letters: Vec<u16> =
      pnp.bytes().map(|byte| u16::from(byte - b'A' + 1)).collect();
    let id = (letters[0] << 10) | (letters[1] << 5) | letters[2];
    edid[8..10].copy_from_slice(&id.to_be_bytes());
    edid[10..12].copy_from_slice(&product.to_le_bytes());
    edid[12..16].copy_from_slice(&serial.to_le_bytes());
    // The first descriptor is a detailed timing, as in real EDIDs.
    edid[54] = 0x02;
    edid[55] = 0x3a;
    for (index, (tag, text)) in descriptors.iter().enumerate() {
      let start = 72 + index * 18;
      edid[start + 3] = *tag;
      let mut payload = text.as_bytes().to_vec();
      payload.push(b'\n');
      payload.resize(13, b' ');
      edid[start + 5..start + 18].copy_from_slice(&payload);
    }
    edid
  }

  fn dell(pnp: &str) -> Option<String> {
    (pnp == "DEL").then(|| "Dell Inc.".to_owned())
  }

  #[test]
  fn descriptors_name_the_model_and_serial() {
    let edid = blob(
      "DEL",
      0xa0b1,
      0x1234,
      &[(0xff, "ABC123"), (0xfc, "DELL U2720Q")],
    );
    assert_eq!(
      Edid::parse(&edid, &dell),
      Some(Edid {
        make: "Dell Inc.".into(),
        model: "DELL U2720Q".into(),
        serial: Some("ABC123".into()),
      })
    );
  }

  #[test]
  fn without_descriptors_the_codes_are_used() {
    let edid = blob("GSM", 0x5b09, 0x0001_0203, &[]);
    assert_eq!(
      Edid::parse(&edid, &dell),
      Some(Edid {
        make: "GSM".into(),
        model: "0x5B09".into(),
        serial: Some("0x00010203".into()),
      })
    );
    let no_serial = blob("GSM", 1, 0, &[(0xfc, "  ")]);
    let parsed = Edid::parse(&no_serial, &dell).unwrap();
    assert_eq!((parsed.model.as_str(), parsed.serial), ("0x0001", None));
  }

  #[test]
  fn broken_blobs_are_refused() {
    assert_eq!(Edid::parse(&[0; 64], &dell), None);
    let mut edid = blob("DEL", 1, 1, &[]);
    edid[0] = 1;
    assert_eq!(Edid::parse(&edid, &dell), None);
    assert_eq!(Edid::unknown().make, "Unknown");
  }

  #[test]
  fn pnp_ids_are_read_from_hwdata_lines() {
    let table = parse_pnp_ids("DEL\tDell Inc.\nGSM\tLG Electronics\nbad\n");
    assert_eq!(table.get("DEL").map(String::as_str), Some("Dell Inc."));
    assert_eq!(table.len(), 2);
  }
}
