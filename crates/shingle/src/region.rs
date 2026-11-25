use shingle_core::Rect;
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, Resource,
    backend::{ClientId, ObjectId},
    protocol::wl_region::{self, WlRegion},
};

use crate::state::{CompositorState, Surface};

pub(crate) struct RegionData;

pub(crate) fn set_input(
    state: &mut CompositorState,
    surface: &ObjectId,
    region: Option<WlRegion>,
) {
    if let Some(surface) = state.surfaces.get_mut(surface) {
        surface.pending.input_region = Some(region);
    }
}

pub(crate) fn commit_input(state: &mut CompositorState, surface: &ObjectId) {
    let update = state
        .surfaces
        .get_mut(surface)
        .and_then(|surface| surface.pending.input_region.take());
    let Some(update) = update else {
        return;
    };
    let rectangles = update.map(|region| {
        state.regions.get(&region.id()).cloned().unwrap_or_default()
    });
    if let Some(surface) = state.surfaces.get_mut(surface) {
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
                    rectangles.push(Rect::new(
                        x,
                        y,
                        width as u32,
                        height as u32,
                    ));
                }
            }
            wl_region::Request::Subtract {
                x,
                y,
                width,
                height,
            } => {
                if width > 0 && height > 0 {
                    subtract(
                        rectangles,
                        Rect::new(x, y, width as u32, height as u32),
                    );
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
