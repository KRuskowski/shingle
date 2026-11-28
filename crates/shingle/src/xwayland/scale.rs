//! Zero scaling (XW-7, docs/design/xwayland.md, section 6): with
//! `xwayland.zero_scaling`, X clients see the layout in physical pixels
//! and Shingle scales their windows down, as KWin's
//! `XwaylandClientsScale` does.
//!
//! There is one X scale for the whole X screen, the largest output scale
//! and never below 1 ([`target`]), because X has one coordinate space and
//! every X window lives in it. An X coordinate is a logical coordinate
//! times the X scale. Xwayland learns the scaled layout from the
//! `wl_output` and xdg-output values its own connection gets
//! ([`crate::output`] and [`crate::xdg_output`]); its surfaces' buffers
//! are X pixels, so their size in the layout is their buffer size over the
//! X scale (`crate::viewport::dimensions`, through [`of_surface`]); points
//! sent to its surfaces, such as pointer, touch and drag positions, are
//! multiplied by it ([`point_for`]), and input regions compared in its
//! units; and every X geometry the window manager reports or is asked to
//! configure passes through [`rect_to_x`] and [`rect_from_x`]. `Xft.dpi`
//! tells X toolkits to draw that much larger ([`resources`]). IPC,
//! layouts and sessions keep logical coordinates. Without the option, or
//! with no output above scale 1, the X scale is 1 and every conversion is
//! the identity, so nothing changes.

use shingle_core::Rect;
use shingle_xwayland::Geometry;
use wayland_server::Client;

use crate::{
  shell::Role,
  state::{ClientDataImpl, CompositorState, Surface},
};

/// The X scale: X pixels per logical pixel. It is 1 unless zero scaling
/// is on and an output's scale is above 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct XScale(f64);

impl Default for XScale {
  /// 1: X coordinates are logical coordinates, as in Wayfire.
  fn default() -> Self {
    Self(1.0)
  }
}

impl XScale {
  /// The scale `value`, which must be finite and at least 1.
  pub(crate) fn new(value: f64) -> Self {
    debug_assert!(value.is_finite() && value >= 1.0);
    Self(value)
  }

  /// X pixels per logical pixel.
  pub(crate) fn get(self) -> f64 {
    self.0
  }

  /// Whether X coordinates are logical coordinates.
  pub(crate) fn is_identity(self) -> bool {
    self.0 == 1.0
  }
}

/// The X scale for `zero_scaling` and the scales of the outputs: the
/// largest of them, as KWin's `Workspace::updateXwaylandScale` chooses,
/// and 1 without zero scaling or when no output is above 1. Below 1, X
/// clients would draw fewer pixels than they are shown with, so zero
/// scaling never lowers the X scale.
pub(crate) fn target(
  zero_scaling: bool,
  scales: impl IntoIterator<Item = f64>,
) -> XScale {
  if !zero_scaling {
    return XScale::default();
  }
  let largest = scales
    .into_iter()
    .filter(|scale| scale.is_finite())
    .fold(1.0, f64::max);
  XScale::new(largest)
}

/// `value` rounded into `i32`.
fn to_i32(value: f64) -> i32 {
  value
    .round()
    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// The X coordinate of logical coordinate `value`.
pub(crate) fn to_x(value: i32, scale: XScale) -> i32 {
  if scale.is_identity() {
    return value;
  }
  to_i32(f64::from(value) * scale.get())
}

/// The logical coordinate of X coordinate `value`.
pub(crate) fn from_x(value: i32, scale: XScale) -> i32 {
  if scale.is_identity() {
    return value;
  }
  to_i32(f64::from(value) / scale.get())
}

/// The X geometry of logical rectangle `rect`. Its edges are converted,
/// not its size, so rectangles that touch in the layout, such as tiles
/// and outputs, touch in X too.
pub(crate) fn rect_to_x(rect: Rect, scale: XScale) -> Geometry {
  let (x, y) = (to_x(rect.x, scale), to_x(rect.y, scale));
  let right = to_x(rect.x.saturating_add_unsigned(rect.width), scale);
  let bottom = to_x(rect.y.saturating_add_unsigned(rect.height), scale);
  Geometry::new(x, y, right.abs_diff(x), bottom.abs_diff(y))
}

/// The logical rectangle of X geometry `geometry`, by its edges.
pub(crate) fn rect_from_x(geometry: Geometry, scale: XScale) -> Rect {
  let (x, y) = (from_x(geometry.x, scale), from_x(geometry.y, scale));
  let right = from_x(geometry.x.saturating_add_unsigned(geometry.width), scale);
  let bottom =
    from_x(geometry.y.saturating_add_unsigned(geometry.height), scale);
  Rect::new(x, y, right.abs_diff(x), bottom.abs_diff(y))
}

/// The logical size of an X size such as a size hint; 0 or less, which
/// means unset, stays as it is, and any other size is at least 1.
pub(crate) fn size_from_x(size: (i32, i32), scale: XScale) -> (i32, i32) {
  if scale.is_identity() {
    return size;
  }
  let convert = |value: i32| {
    if value <= 0 {
      return value;
    }
    to_i32(f64::from(value) / scale.get()).max(1)
  };
  (convert(size.0), convert(size.1))
}

/// A size in the units of a client whose coordinates are `scale` times
/// logical ones, such as a buffer's, in logical pixels: over the scale,
/// rounded, and at least 1 unless it is 0.
pub(crate) fn size_over(size: (u32, u32), scale: f64) -> (u32, u32) {
  if scale == 1.0 {
    return size;
  }
  let convert = |value: u32| {
    if value == 0 {
      return 0;
    }
    let scaled = (f64::from(value) / scale).round();
    scaled.clamp(1.0, f64::from(u32::MAX)) as u32
  };
  (convert(size.0), convert(size.1))
}

/// The point (`x`, `y`) of a surface, in logical pixels from its top left
/// corner, as a client whose coordinates are `scale` times logical ones
/// sees it.
pub(crate) fn point_to_client(x: f64, y: f64, scale: f64) -> (f64, f64) {
  (x * scale, y * scale)
}

/// The point (`x`, `y`) of `object`'s surface, in logical pixels from
/// its top left corner, in its client's units: X pixels for an X window.
pub(crate) fn point_for(
  state: &CompositorState,
  object: &wayland_server::backend::ObjectId,
  (x, y): (f64, f64),
) -> (f64, f64) {
  point_to_client(x, y, of_object(state, object))
}

/// The point (`x`, `y`) of `object`'s surface in its client's units, in
/// logical pixels from its top left corner.
pub(crate) fn point_from(
  state: &CompositorState,
  object: &wayland_server::backend::ObjectId,
  (x, y): (f64, f64),
) -> (f64, f64) {
  let scale = of_object(state, object);
  (x / scale, y / scale)
}

/// [`of_surface`] of the surface `object`, 1 for one that is gone.
pub(crate) fn of_object(
  state: &CompositorState,
  object: &wayland_server::backend::ObjectId,
) -> f64 {
  state
    .surfaces
    .get(object)
    .map_or(1.0, |surface| of_surface(state, surface))
}

/// A rectangle of `object`'s surface in logical pixels from its top left
/// corner, such as its size, in its client's units: X pixels for an X
/// window. Its edges are converted, rounded, as [`rect_to_x`] converts.
pub(crate) fn rect_for(
  state: &CompositorState,
  object: &wayland_server::backend::ObjectId,
  rect: Rect,
) -> Rect {
  let scale = of_object(state, object);
  if scale == 1.0 {
    return rect;
  }
  let geometry = rect_to_x(rect, XScale::new(scale));
  Rect::new(geometry.x, geometry.y, geometry.width, geometry.height)
}

/// Whether `client` is Shingle's Xwayland.
pub(crate) fn is_xwayland(client: Option<&Client>) -> bool {
  client
    .and_then(|client| client.get_data::<ClientDataImpl>())
    .is_some_and(|data| data.xwayland)
}

/// How many of its own units a logical pixel is to `surface`'s client:
/// the X scale for an X window's surface, 1 for every other surface.
/// Xwayland's cursor images are not X windows and keep their size, as
/// without zero scaling: their size comes from `XCURSOR_SIZE`, which the
/// client environment sets for Wayland and X clients alike.
pub(crate) fn of_surface(state: &CompositorState, surface: &Surface) -> f64 {
  if matches!(surface.role, Role::Xwayland(_)) {
    state.xwayland.scale.get()
  } else {
    1.0
  }
}

/// The DPI X clients are told for X scale 1, the X default.
const BASE_DPI: f64 = 96.0;

/// The X resources Shingle sets for X scale `scale`, as changes for
/// `RESOURCE_MANAGER`: `Xft.dpi`, 96 times the scale, which Xft, GTK and
/// Qt read to draw text and widgets that much larger. At scale 1 it is
/// removed again, leaving the X default or the user's own resources.
pub(crate) fn resources(scale: XScale) -> Vec<(&'static str, Option<String>)> {
  let dpi = (!scale.is_identity())
    .then(|| format!("{}", (BASE_DPI * scale.get()).round() as u32));
  vec![("Xft.dpi", dpi)]
}

/// Tells Xwayland about a new X scale: its `wl_output`s and xdg-outputs
/// get the layout in the new X pixels, every X toplevel is configured to
/// its geometry in them, and every X surface is redrawn at its new size
/// in the layout.
pub(crate) fn changed(state: &mut CompositorState) {
  let x_scale = state.xwayland.scale;
  for output in state.outputs.iter() {
    output.send_xwayland(x_scale);
  }
  crate::xdg_output::update_xwayland(state);
  crate::shell::reconfigure_all(state);
  let objects = state
    .surfaces
    .iter()
    .filter(|(_, surface)| {
      matches!(surface.role, Role::Xwayland(_)) && surface.buffer.is_some()
    })
    .map(|(object, _)| object.clone())
    .collect::<Vec<_>>();
  for object in objects {
    if let Some(snapshot) = state.snapshot(&object) {
      state
        .events
        .push(crate::Event::SurfaceCommitted(Box::new(snapshot)));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_x_scale_is_the_largest_output_scale_with_zero_scaling() {
    assert_eq!(target(false, [1.0, 2.0]), XScale::default());
    assert_eq!(target(true, [1.0, 1.5, 1.0]).get(), 1.5);
    assert_eq!(target(true, [2.0, 1.5]).get(), 2.0);
    // Never below 1, and nothing without outputs.
    assert_eq!(target(true, [0.5]).get(), 1.0);
    assert_eq!(target(true, []).get(), 1.0);
    assert_eq!(target(true, [f64::NAN, 1.25]).get(), 1.25);
  }

  #[test]
  fn scale_one_is_the_identity() {
    let one = XScale::default();
    let rect = Rect::new(-7, 13, 301, 199);
    assert_eq!(rect_to_x(rect, one), Geometry::new(-7, 13, 301, 199));
    assert_eq!(rect_from_x(Geometry::new(-7, 13, 301, 199), one), rect);
    assert_eq!(size_from_x((301, 0), one), (301, 0));
    assert_eq!(size_from_x((-1, 7), one), (-1, 7));
    assert_eq!(size_over((301, 199), 1.0), (301, 199));
    assert_eq!(point_to_client(1.25, 2.5, 1.0), (1.25, 2.5));
  }

  #[test]
  fn integer_scales_multiply_and_divide() {
    let two = XScale::new(2.0);
    assert_eq!(
      rect_to_x(Rect::new(10, 20, 640, 360), two),
      Geometry::new(20, 40, 1280, 720)
    );
    assert_eq!(
      rect_from_x(Geometry::new(20, 40, 1280, 720), two),
      Rect::new(10, 20, 640, 360)
    );
    assert_eq!(size_from_x((200, 0), two), (100, 0));
    assert_eq!(point_to_client(10.25, 3.0, 2.0), (20.5, 6.0));
  }

  #[test]
  fn fractional_scales_convert_edges_so_neighbours_still_touch() {
    let scale = XScale::new(1.5);
    // Two tiles of a 1281-pixel-wide area, split at 640.
    let left = rect_to_x(Rect::new(0, 0, 640, 720), scale);
    let right = rect_to_x(Rect::new(640, 0, 641, 720), scale);
    assert_eq!(left, Geometry::new(0, 0, 960, 1080));
    assert_eq!(right.x, 960);
    assert_eq!(right.x + right.width as i32, 1922);
    // Odd positions round to the nearest X pixel.
    assert_eq!(
      rect_to_x(Rect::new(1, 1, 3, 3), scale),
      Geometry::new(2, 2, 4, 4)
    );
    assert_eq!(
      rect_from_x(Geometry::new(2, 2, 4, 4), scale),
      Rect::new(1, 1, 3, 3)
    );
    // The operator's layout: 1680 and 2560 wide at scale 1, then a 4K
    // output at 1.5 (2560 logical pixels wide).
    let outputs = [
      Rect::new(0, 0, 1680, 1050),
      Rect::new(1680, 0, 2560, 1440),
      Rect::new(4240, 0, 2560, 1440),
    ]
    .map(|rect| rect_to_x(rect, scale));
    assert_eq!(outputs[0], Geometry::new(0, 0, 2520, 1575));
    assert_eq!(outputs[1], Geometry::new(2520, 0, 3840, 2160));
    assert_eq!(outputs[2], Geometry::new(6360, 0, 3840, 2160));
  }

  #[test]
  fn sizes_over_a_scale_stay_positive() {
    assert_eq!(size_over((3, 1), 2.0), (2, 1));
    assert_eq!(size_over((0, 5), 1.5), (0, 3));
    assert_eq!(size_from_x((1, 1), XScale::new(2.0)), (1, 1));
    assert_eq!(size_from_x((0, -1), XScale::new(2.0)), (0, -1));
  }
}
