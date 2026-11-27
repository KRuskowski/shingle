use shingle_core::Rect;
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, Resource,
  backend::{ClientId, ObjectId},
  protocol::wl_region::{self, WlRegion},
};

use crate::state::{CompositorState, Surface};

pub(crate) struct RegionData;

/// `wl_surface.set_input_region`: the region's rectangles are copied now,
/// as the protocol requires, so that the client may destroy the region
/// before it commits, as GTK and Qt do.
pub(crate) fn set_input(
  state: &mut CompositorState,
  surface: &ObjectId,
  region: Option<WlRegion>,
) {
  let rectangles = region
    .map(|region| state.regions.get(&region.id()).cloned().unwrap_or_default());
  if let Some(surface) = state.surfaces.get_mut(surface) {
    surface.pending.input_region = Some(rectangles);
  }
}

/// Applies the input region set since the last commit, if any.
pub(crate) fn commit_input(state: &mut CompositorState, surface: &ObjectId) {
  if let Some(surface) = state.surfaces.get_mut(surface)
    && let Some(rectangles) = surface.pending.input_region.take()
  {
    surface.input_region = rectangles;
  }
}

pub(crate) fn contains(
  _state: &CompositorState,
  surface: &Surface,
  x: f64,
  y: f64,
) -> bool {
  let Some(region) = &surface.input_region else {
    return true;
  };
  region.iter().any(|rectangle| {
    x >= f64::from(rectangle.x)
      && y >= f64::from(rectangle.y)
      && x < f64::from(rectangle.x) + f64::from(rectangle.width)
      && y < f64::from(rectangle.y) + f64::from(rectangle.height)
  })
}

impl Dispatch<WlRegion, RegionData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WlRegion,
    request: wl_region::Request,
    _data: &RegionData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let rectangles = state.regions.entry(resource.id()).or_default();
    match request {
      wl_region::Request::Add {
        x,
        y,
        width,
        height,
      } => {
        if width > 0 && height > 0 {
          rectangles.push(Rect::new(x, y, width as u32, height as u32));
        }
      }
      wl_region::Request::Subtract {
        x,
        y,
        width,
        height,
      } => {
        if width > 0 && height > 0 {
          subtract(rectangles, Rect::new(x, y, width as u32, height as u32));
        }
      }
      wl_region::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &WlRegion,
    _data: &RegionData,
  ) {
    state.regions.remove(&resource.id());
  }
}

fn subtract(rectangles: &mut Vec<Rect>, cut: Rect) {
  let mut result = Vec::new();
  for rectangle in rectangles.drain(..) {
    let right = rectangle.x.saturating_add(rectangle.width as i32);
    let bottom = rectangle.y.saturating_add(rectangle.height as i32);
    let cut_right = cut.x.saturating_add(cut.width as i32);
    let cut_bottom = cut.y.saturating_add(cut.height as i32);
    if cut_right <= rectangle.x
      || cut.x >= right
      || cut_bottom <= rectangle.y
      || cut.y >= bottom
    {
      result.push(rectangle);
      continue;
    }
    push_rect(
      &mut result,
      rectangle.x,
      rectangle.y,
      right,
      cut.y.min(bottom),
    );
    push_rect(
      &mut result,
      rectangle.x,
      cut_bottom.max(rectangle.y),
      right,
      bottom,
    );
    push_rect(
      &mut result,
      rectangle.x,
      cut.y.max(rectangle.y),
      cut.x.min(right),
      cut_bottom.min(bottom),
    );
    push_rect(
      &mut result,
      cut_right.max(rectangle.x),
      cut.y.max(rectangle.y),
      right,
      cut_bottom.min(bottom),
    );
  }
  *rectangles = result;
}

fn push_rect(rectangles: &mut Vec<Rect>, x0: i32, y0: i32, x1: i32, y1: i32) {
  if x0 < x1 && y0 < y1 {
    rectangles.push(Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32));
  }
}

#[cfg(test)]
mod tests {
  use shingle_core::Rect;
  use wayland_client::protocol::wl_compositor::WlCompositor;

  use crate::test_client::TestClient;

  #[test]
  fn an_input_region_destroyed_before_the_commit_still_applies() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let view = client.toplevel(&mut compositor, "region");
    let wl_compositor = client.bind::<WlCompositor>(4);
    let region = wl_compositor.create_region(&client.handle, ());
    region.add(10, 10, 50, 40);
    view.surface.set_input_region(Some(&region));
    // GTK and Qt destroy the region at once; the surface keeps a copy.
    region.destroy();
    client.roundtrip(&mut compositor);
    client.attach_buffer(&mut compositor, &view.surface, 100, 80);
    let regions = compositor
      .state
      .surfaces
      .values()
      .filter(|surface| surface.role.toplevel().is_some())
      .map(|surface| surface.input_region.clone())
      .collect::<Vec<_>>();
    assert_eq!(regions, [Some(vec![Rect::new(10, 10, 50, 40)])]);
    // Unset, the region is infinite again.
    view.surface.set_input_region(None);
    view.surface.commit();
    client.roundtrip(&mut compositor);
    assert!(
      compositor
        .state
        .surfaces
        .values()
        .all(|surface| surface.input_region.is_none())
    );
    let _ = std::fs::remove_file(path);
  }
}
