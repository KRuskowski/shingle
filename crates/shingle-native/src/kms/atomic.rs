//! Atomic requests: the property sets Shingle commits.
//!
//! An [`AtomicRequest`] is a list of `(object, property, value)` triples.
//! The builders here produce every request the backend sends, so their
//! exact contents are unit-tested: a full modeset of one output
//! ([`modeset`]) or of several at once ([`modeset_all`]), a page flip
//! ([`flip`], or [`page_flip`] with a render fence and the cursor plane)
//! and DPMS ([`set_active`]). Every output's cursor plane, where it has
//! one, is set in each modeset, shown or off, and moved or changed in
//! flips.

use super::{
  BlobId, FramebufferId, Object, PropertyId,
  props::{
    CONTENT_TYPE_GRAPHICS, ConnectorProps, CrtcProps, LINK_STATUS_GOOD,
    PlaneProps, ROTATE_0,
  },
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

/// The properties of the connector, CRTC, primary plane and cursor plane
/// Shingle drives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputProps {
  /// The connector's properties.
  pub connector: ConnectorProps,
  /// The CRTC's properties.
  pub crtc: CrtcProps,
  /// The primary plane's properties.
  pub plane: PlaneProps,
  /// The CRTC's cursor plane, if it has one that scans out ARGB8888.
  pub cursor: Option<PlaneProps>,
}

/// What the cursor plane shows: a framebuffer of `size` pixels with its
/// top-left corner at (`x`, `y`) of the CRTC, which may lie outside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorState {
  /// The cursor buffer's framebuffer.
  pub framebuffer: FramebufferId,
  /// The buffer's size, which the plane shows unscaled.
  pub size: (u32, u32),
  /// The left edge in the CRTC's pixels.
  pub x: i32,
  /// The top edge in the CRTC's pixels.
  pub y: i32,
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
  /// What the cursor plane shows, or `None` to turn it off. Applied only
  /// where the output has a cursor plane.
  pub cursor: Option<CursorState>,
}

/// The full state of the output for an `ALLOW_MODESET` commit: connector
/// on the CRTC, mode, `ACTIVE`, the primary plane showing `framebuffer`
/// over the whole mode, `VRR_ENABLED` where it exists, the cursor plane
/// where it exists, and everything in `teardown` turned off.
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
  // The outputs set their own cursor planes, shown or off.
  let cursors: Vec<_> = outputs
    .iter()
    .filter_map(|(output, _)| output.cursor.map(|cursor| cursor.id))
    .collect();
  for other in &teardown.planes {
    if cursors.contains(&other.id) {
      continue;
    }
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
  // A link the kernel marked bad works again only after a modeset that
  // sets it good, as wlroots' `atomic_connector_add` does.
  if let Some(link_status) = output.connector.link_status
    && state.active
  {
    request.set(connector, link_status, LINK_STATUS_GOOD);
  }
  // What another DRM master may have left: a lower or higher `max bpc`,
  // an HDR colorimetry and metadata, gamma and colour tables (Shingle
  // applies gamma when it draws) and a rotated plane. wlroots sets
  // `max bpc`, `Colorspace`, `HDR_OUTPUT_METADATA`, `content type` and
  // `GAMMA_LUT` this way; Mutter and KWin also reset `DEGAMMA_LUT`, `CTM`
  // and `rotation`.
  if state.active {
    if let Some((max_bpc, value)) = output.connector.max_bpc {
      request.set(connector, max_bpc, value);
    }
    for property in output.connector.color.into_iter().flatten() {
      request.set(connector, property, 0);
    }
    if let Some(content_type) = output.connector.content_type {
      request.set(connector, content_type, CONTENT_TYPE_GRAPHICS);
    }
    for property in output.crtc.color.into_iter().flatten() {
      request.set(Object::Crtc(output.crtc.id), property, 0);
    }
  }
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
  if let Some(rotation) = p.rotation {
    request.set(plane, rotation, ROTATE_0);
  }
  if let Some(cursor) = &output.cursor {
    set_cursor(request, cursor, output.crtc.id, state.cursor);
    if let Some(rotation) = cursor.rotation {
      request.set(Object::Plane(cursor.id), rotation, ROTATE_0);
    }
  }
}

/// Shows `state` on the cursor plane `cursor` of `crtc`, or turns the
/// plane off.
fn set_cursor(
  request: &mut AtomicRequest,
  cursor: &PlaneProps,
  crtc: super::CrtcId,
  state: Option<CursorState>,
) {
  let plane = Object::Plane(cursor.id);
  let Some(state) = state else {
    request.set(plane, cursor.fb_id, 0);
    request.set(plane, cursor.crtc_id, 0);
    return;
  };
  let (width, height) = state.size;
  request.set(plane, cursor.fb_id, u64::from(state.framebuffer.0));
  request.set(plane, cursor.crtc_id, u64::from(crtc.0));
  request.set(plane, cursor.src_x, 0);
  request.set(plane, cursor.src_y, 0);
  request.set(plane, cursor.src_w, u64::from(width) << 16);
  request.set(plane, cursor.src_h, u64::from(height) << 16);
  // CRTC_X and CRTC_Y are signed: the kernel reads the value as an i64.
  request.set(plane, cursor.crtc_x, i64::from(state.x) as u64);
  request.set(plane, cursor.crtc_y, i64::from(state.y) as u64);
  request.set(plane, cursor.crtc_w, u64::from(width));
  request.set(plane, cursor.crtc_h, u64::from(height));
}

/// What one page flip changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Flip {
  /// The primary plane's new framebuffer, if a frame was drawn.
  pub framebuffer: Option<FramebufferId>,
  /// A sync_file the primary plane waits for before it shows the new
  /// framebuffer (`IN_FENCE_FD`), if the frame's rendering may still run.
  /// Ignored without a framebuffer or where the plane lacks the property.
  pub fence: Option<i32>,
  /// The cursor plane's new state, `Some(None)` to turn it off, if it
  /// changes. Ignored where the output has no cursor plane.
  pub cursor: Option<Option<CursorState>>,
}

/// A page flip: the primary plane shows `framebuffer`. Everything else
/// keeps its state.
pub fn flip(output: &OutputProps, framebuffer: FramebufferId) -> AtomicRequest {
  page_flip(
    output,
    &Flip {
      framebuffer: Some(framebuffer),
      ..Flip::default()
    },
  )
}

/// A page flip of what `flip` changes: the primary plane's framebuffer
/// with its fence, and the cursor plane. Everything else keeps its state.
pub fn page_flip(output: &OutputProps, flip: &Flip) -> AtomicRequest {
  let mut request = AtomicRequest::new();
  let plane = Object::Plane(output.plane.id);
  if let Some(framebuffer) = flip.framebuffer {
    request.set(plane, output.plane.fb_id, u64::from(framebuffer.0));
    if let (Some(fence), Some(property)) =
      (flip.fence, output.plane.in_fence_fd)
    {
      // A descriptor, which the kernel reads as a signed value.
      request.set(plane, property, i64::from(fence) as u64);
    }
  }
  if let (Some(state), Some(cursor)) = (flip.cursor, &output.cursor) {
    set_cursor(&mut request, cursor, output.crtc.id, state);
  }
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
        link_status: None,
        max_bpc: None,
        color: [None; 2],
        content_type: None,
      },
      crtc: CrtcProps {
        id: CrtcId(2),
        mode_id: PropertyId(20),
        active: PropertyId(21),
        vrr_enabled: vrr.then_some(PropertyId(22)),
        color: [None; 3],
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
        in_fence_fd: None,
        rotation: None,
      },
      cursor: None,
    }
  }

  /// [`output_props`] with a cursor plane 4 (properties 70..) and the
  /// primary plane's `IN_FENCE_FD` (property 40).
  pub(crate) fn props_with_cursor() -> OutputProps {
    let mut props = output_props(false);
    props.plane.in_fence_fd = Some(PropertyId(40));
    props.cursor = Some(PlaneProps {
      id: PlaneId(4),
      fb_id: PropertyId(70),
      crtc_id: PropertyId(71),
      src_x: PropertyId(72),
      src_y: PropertyId(73),
      src_w: PropertyId(74),
      src_h: PropertyId(75),
      crtc_x: PropertyId(76),
      crtc_y: PropertyId(77),
      crtc_w: PropertyId(78),
      crtc_h: PropertyId(79),
      in_formats: None,
      in_fence_fd: None,
      rotation: None,
    });
    props
  }

  fn cursor_state(x: i32, y: i32) -> CursorState {
    CursorState {
      framebuffer: FramebufferId(80),
      size: (64, 64),
      x,
      y,
    }
  }

  /// The cursor plane's properties as `(property, value)` pairs.
  fn cursor_entries(request: &AtomicRequest) -> Vec<(u32, u64)> {
    request
      .entries()
      .iter()
      .filter(|(object, ..)| *object == Object::Plane(PlaneId(4)))
      .map(|(_, property, value)| (property.0, *value))
      .collect()
  }

  fn state() -> ModesetState {
    ModesetState {
      mode: BlobId(50),
      framebuffer: FramebufferId(60),
      size: (1920, 1080),
      active: true,
      vrr: false,
      cursor: None,
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
  fn a_modeset_sets_a_bad_link_good_while_the_output_is_on() {
    let mut props = output_props(false);
    props.connector.link_status = Some(PropertyId(11));
    let connector = Object::Connector(ConnectorId(1));
    let on = modeset(&props, &state(), &Teardown::default());
    assert_eq!(on.get(connector, PropertyId(11)), Some(LINK_STATUS_GOOD));
    let off = ModesetState {
      active: false,
      ..state()
    };
    let off = modeset(&props, &off, &Teardown::default());
    assert_eq!(off.get(connector, PropertyId(11)), None);
    assert!(flip(&props, FramebufferId(61)).entries().len() == 1);
  }

  #[test]
  fn a_modeset_resets_what_another_master_left() {
    let mut props = props_with_cursor();
    props.connector.max_bpc = Some((PropertyId(12), 8));
    props.connector.color = [Some(PropertyId(13)), Some(PropertyId(14))];
    props.connector.content_type = Some(PropertyId(15));
    props.crtc.color = [Some(PropertyId(23)), None, Some(PropertyId(25))];
    props.plane.rotation = Some(PropertyId(90));
    if let Some(cursor) = &mut props.cursor {
      cursor.rotation = Some(PropertyId(91));
    }
    let request = modeset(&props, &state(), &Teardown::default());
    let crtc = Object::Crtc(CrtcId(2));
    assert_eq!(
      request.get(Object::Connector(ConnectorId(1)), PropertyId(12)),
      Some(8)
    );
    let connector = Object::Connector(ConnectorId(1));
    assert_eq!(request.get(connector, PropertyId(13)), Some(0));
    assert_eq!(request.get(connector, PropertyId(14)), Some(0));
    assert_eq!(
      request.get(connector, PropertyId(15)),
      Some(CONTENT_TYPE_GRAPHICS)
    );
    assert_eq!(request.get(crtc, PropertyId(23)), Some(0));
    assert_eq!(request.get(crtc, PropertyId(25)), Some(0));
    assert_eq!(
      request.get(Object::Plane(PlaneId(3)), PropertyId(90)),
      Some(ROTATE_0)
    );
    assert_eq!(
      request.get(Object::Plane(PlaneId(4)), PropertyId(91)),
      Some(ROTATE_0),
      "the cursor plane, off or not"
    );
    // A flip changes none of them.
    let flipped = flip(&props, FramebufferId(61));
    assert_eq!(flipped.entries().len(), 1);
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
        link_status: None,
        max_bpc: None,
        color: [None; 2],
        content_type: None,
      }],
      crtcs: vec![CrtcProps {
        id: CrtcId(5),
        mode_id: PropertyId(41),
        active: PropertyId(42),
        vrr_enabled: None,
        color: [None; 3],
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
        link_status: None,
        max_bpc: None,
        color: [None; 2],
        content_type: None,
      },
      crtc: CrtcProps {
        id: CrtcId(8),
        ..first.crtc
      },
      plane: PlaneProps {
        id: PlaneId(9),
        ..first.plane
      },
      cursor: None,
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
        color: [None; 3],
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
  fn a_modeset_shows_the_cursor_plane_or_turns_it_off() {
    let props = props_with_cursor();
    let request = modeset(&props, &state(), &Teardown::default());
    assert_eq!(request.entries().len(), 13 + 2);
    assert_eq!(cursor_entries(&request), [(70, 0), (71, 0)], "off");
    let shown = ModesetState {
      cursor: Some(cursor_state(100, 200)),
      ..state()
    };
    let request = modeset(&props, &shown, &Teardown::default());
    assert_eq!(
      cursor_entries(&request),
      [
        (70, 80),
        (71, 2),
        (72, 0),
        (73, 0),
        (74, 64 << 16),
        (75, 64 << 16),
        (76, 100),
        (77, 200),
        (78, 64),
        (79, 64),
      ]
    );
  }

  #[test]
  fn the_teardown_spares_the_cursor_planes_of_the_outputs() {
    let props = props_with_cursor();
    let cursor = props.cursor.expect("a cursor plane");
    let other = PlaneProps {
      id: PlaneId(6),
      ..cursor
    };
    let teardown = Teardown {
      planes: vec![cursor, other],
      ..Teardown::default()
    };
    let shown = ModesetState {
      cursor: Some(cursor_state(1, 2)),
      ..state()
    };
    let request = modeset(&props, &shown, &teardown);
    assert_eq!(
      request.get(Object::Plane(PlaneId(4)), PropertyId(70)),
      Some(80),
      "the console's cursor plane is now ours, and shown"
    );
    assert_eq!(
      request.get(Object::Plane(PlaneId(6)), PropertyId(70)),
      Some(0),
      "another CRTC's is turned off"
    );
  }

  #[test]
  fn a_flip_moves_the_cursor_and_hands_the_plane_its_fence() {
    let props = props_with_cursor();
    let request = page_flip(
      &props,
      &Flip {
        framebuffer: Some(FramebufferId(61)),
        fence: Some(17),
        cursor: Some(Some(cursor_state(-10, -3))),
      },
    );
    let plane = Object::Plane(PlaneId(3));
    assert_eq!(
      request.entries()[..2],
      [(plane, PropertyId(30), 61), (plane, PropertyId(40), 17)]
    );
    let cursor = cursor_entries(&request);
    assert_eq!(cursor.len(), 10);
    assert_eq!(cursor[6], (76, -10_i64 as u64), "signed, sign-extended");
    assert_eq!(cursor[7], (77, -3_i64 as u64));
    // A cursor move alone touches only the cursor plane.
    let request = page_flip(
      &props,
      &Flip {
        cursor: Some(None),
        ..Flip::default()
      },
    );
    assert_eq!(
      request.entries(),
      [
        (Object::Plane(PlaneId(4)), PropertyId(70), 0),
        (Object::Plane(PlaneId(4)), PropertyId(71), 0),
      ]
    );
  }

  #[test]
  fn fences_and_cursors_need_their_properties() {
    let props = output_props(false);
    let request = page_flip(
      &props,
      &Flip {
        framebuffer: Some(FramebufferId(61)),
        fence: Some(17),
        cursor: Some(Some(cursor_state(0, 0))),
      },
    );
    assert_eq!(request, flip(&props, FramebufferId(61)));
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
