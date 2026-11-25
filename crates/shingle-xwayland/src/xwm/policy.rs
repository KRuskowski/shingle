//! What an X client gets when it asks to configure its own window
//! (`ConfigureRequest`), as Wayfire's `handle_client_configure` and
//! `configure_request` decide it
//! (src/view/xwayland/xwayland-toplevel-view.hpp).

use super::window::Geometry;

/// Where Shingle has put the window, as far as a configure request is
/// concerned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placement {
  /// Not a Shingle window yet: the request is granted as asked, and its
  /// position becomes the start position.
  Unmapped,
  /// Tiled, maximized or fullscreen: the client gets this geometry back.
  Fixed(Geometry),
  /// Floating: the request, clamped to this workarea.
  Floating {
    /// The workarea of the window's output.
    workarea: Geometry,
  },
}

/// The geometry to configure in answer to `request`. Never empty.
pub fn configure_reply(request: Geometry, placement: Placement) -> Geometry {
  match placement {
    Placement::Unmapped => request.nonzero(),
    Placement::Fixed(geometry) => geometry.nonzero(),
    Placement::Floating { workarea } => clamp(request, workarea).nonzero(),
  }
}

/// `wf::clamp`: shrinks `geometry` to fit `bounds`, then moves it inside.
pub fn clamp(geometry: Geometry, bounds: Geometry) -> Geometry {
  let width = geometry.width.min(bounds.width);
  let height = geometry.height.min(bounds.height);
  let max_x = bounds.x.saturating_add_unsigned(bounds.width - width);
  let max_y = bounds.y.saturating_add_unsigned(bounds.height - height);
  Geometry {
    x: geometry.x.clamp(bounds.x, max_x),
    y: geometry.y.clamp(bounds.y, max_y),
    width,
    height,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const WORKAREA: Geometry = Geometry::new(0, 30, 1280, 690);

  #[test]
  fn unmapped_windows_get_what_they_ask() {
    let request = Geometry::new(-50, 2000, 300, 200);
    assert_eq!(configure_reply(request, Placement::Unmapped), request);
    assert_eq!(
      configure_reply(Geometry::new(5, 5, 0, 0), Placement::Unmapped),
      Geometry::new(5, 5, 1, 1)
    );
  }

  #[test]
  fn tiled_and_fullscreen_windows_keep_their_geometry() {
    let tile = Geometry::new(640, 30, 640, 690);
    let request = Geometry::new(0, 0, 100, 100);
    assert_eq!(configure_reply(request, Placement::Fixed(tile)), tile);
  }

  #[test]
  fn floating_requests_are_clamped_to_the_workarea() {
    let floating = Placement::Floating { workarea: WORKAREA };
    let inside = Geometry::new(100, 100, 300, 200);
    assert_eq!(configure_reply(inside, floating), inside);
    // Moved back inside.
    assert_eq!(
      configure_reply(Geometry::new(1200, 0, 300, 200), floating),
      Geometry::new(980, 30, 300, 200)
    );
    assert_eq!(
      configure_reply(Geometry::new(-40, 900, 300, 200), floating),
      Geometry::new(0, 520, 300, 200)
    );
    // Shrunk to fit.
    assert_eq!(
      configure_reply(Geometry::new(10, 10, 5000, 5000), floating),
      WORKAREA
    );
  }
}
