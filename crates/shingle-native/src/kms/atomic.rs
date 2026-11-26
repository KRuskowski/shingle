//! Atomic requests: the property sets Shingle commits.
//!
//! An [`AtomicRequest`] is a list of `(object, property, value)` triples.
//! The builders here produce every request the backend sends, so their
//! exact contents are unit-tested: a full modeset of one output
//! ([`modeset`]) or of several at once ([`modeset_all`]), a page flip
//! ([`flip`]) and DPMS ([`set_active`]).

use super::{
  BlobId, FramebufferId, Object, PropertyId,
  props::{ConnectorProps, CrtcProps, PlaneProps},
};

/// Properties to set in one atomic commit, in the order they were added.
/// Setting a property twice keeps the last value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AtomicRequest {
  entries: Vec<(Object, PropertyId, u64)>,
}

impl AtomicRequest {
  /// An empty request.
  pub fn new() -> Self {
    Self::default()
  }

  /// Sets `property` of `object` to `value`.
  pub fn set(&mut self, object: Object, property: PropertyId, value: u64) {
    match self
      .entries
      .iter_mut()
      .find(|(o, p, _)| *o == object && *p == property)
    {
      Some(entry) => entry.2 = value,
      None => self.entries.push((object, property, value)),
    }
  }

  /// The value `property` of `object` is set to, if it is.
  pub fn get(&self, object: Object, property: PropertyId) -> Option<u64> {
    self
      .entries
      .iter()
      .find(|(o, p, _)| *o == object && *p == property)
      .map(|(_, _, value)| *value)
  }

  /// Every triple, in the order they were first set.
  pub fn entries(&self) -> &[(Object, PropertyId, u64)] {
    &self.entries
  }

  /// Whether nothing is set.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }
}

/// The properties of the connector, CRTC and primary plane Shingle drives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputProps {
  /// The connector's properties.
  pub connector: ConnectorProps,
  /// The CRTC's properties.
  pub crtc: CrtcProps,
  /// The primary plane's properties.
  pub plane: PlaneProps,
}

/// Objects left on by another session that a modeset turns off, so only
/// the enabled output is lit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Teardown {
  /// Other connectors attached to a CRTC: set `CRTC_ID` to 0.
  pub connectors: Vec<ConnectorProps>,
  /// Other active CRTCs: set `ACTIVE` and `MODE_ID` to 0.
  pub crtcs: Vec<CrtcProps>,
  /// Other planes attached to a CRTC, including our CRTC's cursor and
  /// overlay planes: set `FB_ID` and `CRTC_ID` to 0.
  pub planes: Vec<PlaneProps>,
}

/// What a full modeset sets the output to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModesetState {
  /// The mode blob for `MODE_ID`.
  pub mode: BlobId,
  /// The framebuffer to show.
  pub framebuffer: FramebufferId,
  /// The mode's size, which the framebuffer has too.
  pub size: (u32, u32),
  /// Whether the CRTC is on; false keeps the mode with the display off.
  pub active: bool,
  /// Whether variable refresh is on. Applied only where the CRTC has
  /// `VRR_ENABLED`.
  pub vrr: bool,
}

/// The full state of the output for an `ALLOW_MODESET` commit: connector
/// on the CRTC, mode, `ACTIVE`, the primary plane showing `framebuffer`
/// over the whole mode, `VRR_ENABLED` where it exists, and everything in
/// `teardown` turned off.
pub fn modeset(
  output: &OutputProps,
  state: &ModesetState,
  teardown: &Teardown,
) -> AtomicRequest {
  modeset_all(&[(output, *state)], teardown)
}

/// One `ALLOW_MODESET` commit for several outputs at once: the full state
/// of each, in order, as [`modeset`] sets it for one, then everything in
/// `teardown` turned off. The kernel tests and applies the configuration
/// as a whole, so a limit shared by the outputs, such as memory bandwidth,
/// shows in its `TEST_ONLY`.
pub fn modeset_all(
  outputs: &[(&OutputProps, ModesetState)],
  teardown: &Teardown,
) -> AtomicRequest {
  let mut request = AtomicRequest::new();
  for (output, state) in outputs {
    set_output(&mut request, output, state);
  }
  for other in &teardown.connectors {
    request.set(Object::Connector(other.id), other.crtc_id, 0);
  }
  for other in &teardown.crtcs {
    let object = Object::Crtc(other.id);
    request.set(object, other.active, 0);
    request.set(object, other.mode_id, 0);
  }
  for other in &teardown.planes {
    let object = Object::Plane(other.id);
    request.set(object, other.fb_id, 0);
    request.set(object, other.crtc_id, 0);
  }
  request
}

/// Adds the full state of `output` to `request`.
fn set_output(
  request: &mut AtomicRequest,
  output: &OutputProps,
  state: &ModesetState,
) {
  let connector = Object::Connector(output.connector.id);
  let crtc = Object::Crtc(output.crtc.id);
  let plane = Object::Plane(output.plane.id);
  let (width, height) = state.size;
  request.set(
    connector,
    output.connector.crtc_id,
    u64::from(output.crtc.id.0),
  );
  request.set(crtc, output.crtc.mode_id, u64::from(state.mode.0));
  request.set(crtc, output.crtc.active, u64::from(state.active));
  if let Some(vrr) = output.crtc.vrr_enabled {
    request.set(crtc, vrr, u64::from(state.vrr));
  }
  let p = &output.plane;
  request.set(plane, p.fb_id, u64::from(state.framebuffer.0));
  request.set(plane, p.crtc_id, u64::from(output.crtc.id.0));
  // Source coordinates are 16.16 fixed point.
  request.set(plane, p.src_x, 0);
  request.set(plane, p.src_y, 0);
  request.set(plane, p.src_w, u64::from(width) << 16);
  request.set(plane, p.src_h, u64::from(height) << 16);
  request.set(plane, p.crtc_x, 0);
  request.set(plane, p.crtc_y, 0);
  request.set(plane, p.crtc_w, u64::from(width));
  request.set(plane, p.crtc_h, u64::from(height));
}

/// A page flip: the primary plane shows `framebuffer`. Everything else
/// keeps its state.
pub fn flip(output: &OutputProps, framebuffer: FramebufferId) -> AtomicRequest {
  let mut request = AtomicRequest::new();
  request.set(
    Object::Plane(output.plane.id),
    output.plane.fb_id,
    u64::from(framebuffer.0),
  );
  request
}

/// DPMS: the CRTC's `ACTIVE`, keeping its mode and planes, as
/// wlr-output-power-management turns a display off and on.
pub fn set_active(output: &OutputProps, active: bool) -> AtomicRequest {
  let mut request = AtomicRequest::new();
  request.set(
    Object::Crtc(output.crtc.id),
    output.crtc.active,
    u64::from(active),
  );
  request
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use crate::kms::{ConnectorId, CrtcId, PlaneId};

  /// Property ids: connector 10.., CRTC 20.., plane 30...
  pub(crate) fn output_props(vrr: bool) -> OutputProps {
    OutputProps {
      connector: ConnectorProps {
        id: ConnectorId(1),
        crtc_id: PropertyId(10),
      },
      crtc: CrtcProps {
        id: CrtcId(2),
        mode_id: PropertyId(20),
        active: PropertyId(21),
        vrr_enabled: vrr.then_some(PropertyId(22)),
      },
      plane: PlaneProps {
        id: PlaneId(3),
        fb_id: PropertyId(30),
        crtc_id: PropertyId(31),
        src_x: PropertyId(32),
        src_y: PropertyId(33),
        src_w: PropertyId(34),
        src_h: PropertyId(35),
        crtc_x: PropertyId(36),
        crtc_y: PropertyId(37),
        crtc_w: PropertyId(38),
        crtc_h: PropertyId(39),
        in_formats: None,
      },
    }
  }

  fn state() -> ModesetState {
    ModesetState {
      mode: BlobId(50),
      framebuffer: FramebufferId(60),
      size: (1920, 1080),
      active: true,
      vrr: false,
    }
  }

  #[test]
  fn a_modeset_sets_the_whole_output() {
    let request = modeset(&output_props(false), &state(), &Teardown::default());
    let connector = Object::Connector(ConnectorId(1));
    let crtc = Object::Crtc(CrtcId(2));
    let plane = Object::Plane(PlaneId(3));
    let expected = vec![
      (connector, PropertyId(10), 2),
      (crtc, PropertyId(20), 50),
      (crtc, PropertyId(21), 1),
      (plane, PropertyId(30), 60),
      (plane, PropertyId(31), 2),
      (plane, PropertyId(32), 0),
      (plane, PropertyId(33), 0),
      (plane, PropertyId(34), 1920 << 16),
      (plane, PropertyId(35), 1080 << 16),
      (plane, PropertyId(36), 0),
      (plane, PropertyId(37), 0),
      (plane, PropertyId(38), 1920),
      (plane, PropertyId(39), 1080),
    ];
    assert_eq!(request.entries(), expected.as_slice());
  }

  #[test]
  fn a_modeset_turns_vrr_off_where_the_crtc_has_it() {
    let crtc = Object::Crtc(CrtcId(2));
    let request = modeset(&output_props(true), &state(), &Teardown::default());
    assert_eq!(request.get(crtc, PropertyId(22)), Some(0));
    assert_eq!(request.entries().len(), 14);
    let on = ModesetState {
      vrr: true,
      ..state()
    };
    let request = modeset(&output_props(true), &on, &Teardown::default());
    assert_eq!(request.get(crtc, PropertyId(22)), Some(1));
  }

  #[test]
  fn a_modeset_turns_off_what_another_session_left_on() {
    let teardown = Teardown {
      connectors: vec![ConnectorProps {
        id: ConnectorId(4),
        crtc_id: PropertyId(40),
      }],
      crtcs: vec![CrtcProps {
        id: CrtcId(5),
        mode_id: PropertyId(41),
        active: PropertyId(42),
        vrr_enabled: None,
      }],
      planes: vec![PlaneProps {
        id: PlaneId(6),
        fb_id: PropertyId(43),
        crtc_id: PropertyId(44),
        ..output_props(false).plane
      }],
    };
    let request = modeset(&output_props(false), &state(), &teardown);
    let tail: Vec<_> = request.entries()[13..].to_vec();
    assert_eq!(
      tail,
      vec![
        (Object::Connector(ConnectorId(4)), PropertyId(40), 0),
        (Object::Crtc(CrtcId(5)), PropertyId(42), 0),
        (Object::Crtc(CrtcId(5)), PropertyId(41), 0),
        (Object::Plane(PlaneId(6)), PropertyId(43), 0),
        (Object::Plane(PlaneId(6)), PropertyId(44), 0),
      ]
    );
  }

  #[test]
  fn a_modeset_can_keep_the_display_off() {
    let off = ModesetState {
      active: false,
      ..state()
    };
    let request = modeset(&output_props(false), &off, &Teardown::default());
    assert_eq!(
      request.get(Object::Crtc(CrtcId(2)), PropertyId(21)),
      Some(0)
    );
  }

  #[test]
  fn one_modeset_sets_every_output_then_turns_off_the_rest() {
    let first = output_props(false);
    let second = OutputProps {
      connector: ConnectorProps {
        id: ConnectorId(7),
        crtc_id: PropertyId(10),
      },
      crtc: CrtcProps {
        id: CrtcId(8),
        ..first.crtc
      },
      plane: PlaneProps {
        id: PlaneId(9),
        ..first.plane
      },
    };
    let small = ModesetState {
      mode: BlobId(51),
      framebuffer: FramebufferId(61),
      size: (1280, 720),
      ..state()
    };
    let teardown = Teardown {
      crtcs: vec![CrtcProps {
        id: CrtcId(5),
        mode_id: PropertyId(41),
        active: PropertyId(42),
        vrr_enabled: None,
      }],
      ..Teardown::default()
    };
    let request =
      modeset_all(&[(&first, state()), (&second, small)], &teardown);
    let one = modeset(&first, &state(), &Teardown::default());
    assert_eq!(&request.entries()[..13], one.entries(), "the first output");
    let crtc = Object::Crtc(CrtcId(8));
    assert_eq!(request.get(crtc, PropertyId(20)), Some(51));
    assert_eq!(
      request.get(Object::Connector(ConnectorId(7)), PropertyId(10)),
      Some(8)
    );
    assert_eq!(
      request.get(Object::Plane(PlaneId(9)), PropertyId(38)),
      Some(1280)
    );
    assert_eq!(request.entries().len(), 13 * 2 + 2);
    assert_eq!(
      request.entries()[26..],
      [
        (Object::Crtc(CrtcId(5)), PropertyId(42), 0),
        (Object::Crtc(CrtcId(5)), PropertyId(41), 0),
      ]
    );
  }

  #[test]
  fn a_flip_only_changes_the_framebuffer() {
    let request = flip(&output_props(true), FramebufferId(61));
    assert_eq!(
      request.entries(),
      &[(Object::Plane(PlaneId(3)), PropertyId(30), 61)]
    );
  }

  #[test]
  fn dpms_only_changes_active() {
    let props = output_props(true);
    assert_eq!(
      set_active(&props, false).entries(),
      &[(Object::Crtc(CrtcId(2)), PropertyId(21), 0)]
    );
    assert_eq!(
      set_active(&props, true).entries(),
      &[(Object::Crtc(CrtcId(2)), PropertyId(21), 1)]
    );
  }

  #[test]
  fn setting_a_property_twice_keeps_the_last_value() {
    let mut request = AtomicRequest::new();
    assert!(request.is_empty());
    let object = Object::Crtc(CrtcId(2));
    request.set(object, PropertyId(1), 5);
    request.set(object, PropertyId(1), 6);
    assert_eq!(request.entries(), &[(object, PropertyId(1), 6)]);
  }
}
