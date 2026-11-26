//! The property cache: the ids of the properties Shingle sets, read once
//! per object, and their values at the time.

use std::{fmt, io};

use super::{ConnectorId, CrtcId, KmsDevice, Object, PlaneId, PropertyId};

/// An object's properties by name, with the values read at load time.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PropertyTable {
  object: Option<Object>,
  properties: Vec<super::Property>,
}

/// A property Shingle needs that an object does not have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingProperty {
  /// The object.
  pub object: Object,
  /// The property's name.
  pub name: &'static str,
}

impl fmt::Display for MissingProperty {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let (kind, id) = match self.object {
      Object::Connector(ConnectorId(id)) => ("connector", id),
      Object::Crtc(CrtcId(id)) => ("CRTC", id),
      Object::Plane(PlaneId(id)) => ("plane", id),
    };
    write!(formatter, "{kind} {id} has no {} property", self.name)
  }
}

impl std::error::Error for MissingProperty {}

impl From<MissingProperty> for io::Error {
  fn from(missing: MissingProperty) -> Self {
    io::Error::new(io::ErrorKind::Unsupported, missing)
  }
}

impl PropertyTable {
  /// Reads `object`'s properties from `device`.
  pub fn load(device: &mut dyn KmsDevice, object: Object) -> io::Result<Self> {
    Ok(Self {
      object: Some(object),
      properties: device.properties(object)?,
    })
  }

  /// The id of property `name`, if the object has it.
  pub fn id(&self, name: &str) -> Option<PropertyId> {
    self.find(name).map(|property| property.id)
  }

  /// The value property `name` had when the table was loaded.
  pub fn value(&self, name: &str) -> Option<u64> {
    self.find(name).map(|property| property.value)
  }

  /// The id of property `name`, which the object must have.
  pub fn require(
    &self,
    name: &'static str,
  ) -> Result<PropertyId, MissingProperty> {
    self.id(name).ok_or(MissingProperty {
      object: self.object.unwrap_or(Object::Connector(ConnectorId(0))),
      name,
    })
  }

  fn find(&self, name: &str) -> Option<&super::Property> {
    self
      .properties
      .iter()
      .find(|property| property.name == name)
  }
}

/// The `type` property's value for a primary plane.
pub const PLANE_TYPE_PRIMARY: u64 = 1;

/// The `type` property's value for a cursor plane.
pub const PLANE_TYPE_CURSOR: u64 = 2;

/// A connector's property ids.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorProps {
  /// The connector.
  pub id: ConnectorId,
  /// `CRTC_ID`.
  pub crtc_id: PropertyId,
}

impl ConnectorProps {
  /// Takes the ids from a loaded table.
  pub fn new(
    id: ConnectorId,
    table: &PropertyTable,
  ) -> Result<Self, MissingProperty> {
    Ok(Self {
      id,
      crtc_id: table.require("CRTC_ID")?,
    })
  }
}

/// A CRTC's property ids.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrtcProps {
  /// The CRTC.
  pub id: CrtcId,
  /// `MODE_ID`.
  pub mode_id: PropertyId,
  /// `ACTIVE`.
  pub active: PropertyId,
  /// `VRR_ENABLED`, where the driver has it.
  pub vrr_enabled: Option<PropertyId>,
}

impl CrtcProps {
  /// Takes the ids from a loaded table.
  pub fn new(
    id: CrtcId,
    table: &PropertyTable,
  ) -> Result<Self, MissingProperty> {
    Ok(Self {
      id,
      mode_id: table.require("MODE_ID")?,
      active: table.require("ACTIVE")?,
      vrr_enabled: table.id("VRR_ENABLED"),
    })
  }
}

/// A plane's property ids.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneProps {
  /// The plane.
  pub id: PlaneId,
  /// `FB_ID`.
  pub fb_id: PropertyId,
  /// `CRTC_ID`.
  pub crtc_id: PropertyId,
  /// `SRC_X`, 16.16 fixed point like the other `SRC_*`.
  pub src_x: PropertyId,
  /// `SRC_Y`.
  pub src_y: PropertyId,
  /// `SRC_W`.
  pub src_w: PropertyId,
  /// `SRC_H`.
  pub src_h: PropertyId,
  /// `CRTC_X`.
  pub crtc_x: PropertyId,
  /// `CRTC_Y`.
  pub crtc_y: PropertyId,
  /// `CRTC_W`.
  pub crtc_w: PropertyId,
  /// `CRTC_H`.
  pub crtc_h: PropertyId,
  /// `IN_FORMATS`, where the driver has it.
  pub in_formats: Option<PropertyId>,
  /// `IN_FENCE_FD`, where the driver has it: a sync_file the plane waits
  /// for before it scans out a new framebuffer.
  pub in_fence_fd: Option<PropertyId>,
}

impl PlaneProps {
  /// Takes the ids from a loaded table.
  pub fn new(
    id: PlaneId,
    table: &PropertyTable,
  ) -> Result<Self, MissingProperty> {
    Ok(Self {
      id,
      fb_id: table.require("FB_ID")?,
      crtc_id: table.require("CRTC_ID")?,
      src_x: table.require("SRC_X")?,
      src_y: table.require("SRC_Y")?,
      src_w: table.require("SRC_W")?,
      src_h: table.require("SRC_H")?,
      crtc_x: table.require("CRTC_X")?,
      crtc_y: table.require("CRTC_Y")?,
      crtc_w: table.require("CRTC_W")?,
      crtc_h: table.require("CRTC_H")?,
      in_formats: table.id("IN_FORMATS"),
      in_fence_fd: table.id("IN_FENCE_FD"),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::fake::FakeKms;

  #[test]
  fn ids_and_values_come_from_the_device() {
    let mut device = FakeKms::single_output();
    let connector = device.connector_id;
    let table =
      PropertyTable::load(&mut device, Object::Connector(connector)).unwrap();
    let props = ConnectorProps::new(connector, &table).unwrap();
    assert_eq!(table.id("CRTC_ID"), Some(props.crtc_id));
    assert_eq!(table.value("CRTC_ID"), Some(0));
    assert_eq!(table.id("NOPE"), None);
  }

  #[test]
  fn missing_properties_name_the_object() {
    let table = PropertyTable {
      object: Some(Object::Crtc(CrtcId(7))),
      properties: Vec::new(),
    };
    let error = CrtcProps::new(CrtcId(7), &table).unwrap_err();
    assert_eq!(error.to_string(), "CRTC 7 has no MODE_ID property");
  }

  #[test]
  fn optional_properties_may_be_missing() {
    let mut device = FakeKms::single_output();
    let crtc = device.crtc_id;
    let table = PropertyTable::load(&mut device, Object::Crtc(crtc)).unwrap();
    let props = CrtcProps::new(crtc, &table).unwrap();
    assert_eq!(props.vrr_enabled, None);
  }
}
