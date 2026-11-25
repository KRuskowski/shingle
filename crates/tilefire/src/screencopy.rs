use std::collections::HashMap;

use tilefire_core::Rect;
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::ObjectId,
    protocol::{wl_output, wl_shm},
};

use crate::state::CompositorState;

struct Frame {
    region: Rect,
    used: bool,
}

#[derive(Default)]
pub(crate) struct Screencopy {
    frames: HashMap<ObjectId, Frame>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
    handle.create_global::<CompositorState, ZwlrScreencopyManagerV1, ()>(3, ());
}

fn create_frame(
    state: &mut CompositorState,
    frame: New<ZwlrScreencopyFrameV1>,
    data_init: &mut DataInit<'_, CompositorState>,
    region: Rect,
) {
    let frame = data_init.init(frame, ());
    if region.width == 0 || region.height == 0 {
        frame.failed();
        return;
    }
    frame.buffer(
        wl_shm::Format::Argb8888,
        region.width,
        region.height,
        region.width.saturating_mul(4),
    );
    if frame.version() >= 3 {
        frame.buffer_done();
    }
    state.screencopy.frames.insert(
        frame.id(),
        Frame {
            region,
            used: false,
        },
    );
}

fn copy_frame(
    state: &mut CompositorState,
    resource: &ZwlrScreencopyFrameV1,
    buffer: &wayland_server::protocol::wl_buffer::WlBuffer,
) {
    let Some(frame) = state.screencopy.frames.get_mut(&resource.id()) else {
        return;
    };
    if frame.used {
        resource.post_error(
            zwlr_screencopy_frame_v1::Error::AlreadyUsed,
            "frame has already been copied",
        );
        return;
    }
    frame.used = true;
    let Some(attachment) = state.buffers.get(&buffer.id()).cloned() else {
        resource.failed();
        return;
    };
    if attachment.width != frame.region.width
        || attachment.height != frame.region.height
    {
        resource.post_error(
            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
            "frame buffer dimensions do not match the capture",
        );
        return;
    }
    let region = frame.region;
    let rgba = composite(state, region);
    match attachment.write_rgba(&rgba) {
        Ok(()) => {
            let now = state.now();
            let seconds = now.as_secs();
            resource.flags(zwlr_screencopy_frame_v1::Flags::empty());
            resource.ready(
                (seconds >> 32) as u32,
                seconds as u32,
                now.subsec_nanos(),
            );
        }
        Err(_) => resource.failed(),
    }
}

fn composite(state: &CompositorState, region: Rect) -> Vec<u8> {
    let background = [
        srgb_channel(state.background_color[0]),
        srgb_channel(state.background_color[1]),
        srgb_channel(state.background_color[2]),
        255,
    ];
    let mut rgba = vec![0; region.width as usize * region.height as usize * 4];
    for pixel in rgba.as_chunks_mut::<4>().0 {
        *pixel = background;
    }
    for surface in state.visible_surfaces() {
        let Some(buffer) = surface.buffer else {
            continue;
        };
        let opacity = (surface.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
        if opacity == 0 {
            continue;
        }
        let intersection = intersection(region, surface.geometry);
        let Some(intersection) = intersection else {
            continue;
        };
        for y in 0..intersection.height {
            for x in 0..intersection.width {
                let Some(source_index) = sample_index(
                    &buffer,
                    surface.geometry,
                    f64::from(intersection.x) + f64::from(x),
                    f64::from(intersection.y) + f64::from(y),
                ) else {
                    continue;
                };
                let source = &buffer.rgba[source_index..source_index + 4];
                let destination_x = intersection.x - region.x + x as i32;
                let destination_y = intersection.y - region.y + y as i32;
                let destination_index = (destination_y as usize
                    * region.width as usize
                    + destination_x as usize)
                    * 4;
                let destination =
                    &mut rgba[destination_index..destination_index + 4];
                let alpha = (u32::from(source[3]) * opacity + 127) / 255;
                if alpha == 255 && opacity == 255 {
                    destination.copy_from_slice(source);
                } else if alpha > 0 {
                    for channel in 0..3 {
                        destination[channel] = ((u32::from(source[channel])
                            * opacity
                            + u32::from(destination[channel]) * (255 - alpha)
                            + 127)
                            / 255)
                            .min(255)
                            as u8;
                    }
                    destination[3] = 255;
                }
            }
        }
    }
    rgba
}

fn sample_index(
    buffer: &crate::BufferSnapshot,
    geometry: Rect,
    x: f64,
    y: f64,
) -> Option<usize> {
    if geometry.width == 0
        || geometry.height == 0
        || buffer.width == 0
        || buffer.height == 0
    {
        return None;
    }
    let u = (x - f64::from(geometry.x) + 0.5) / f64::from(geometry.width);
    let v = (y - f64::from(geometry.y) + 0.5) / f64::from(geometry.height);
    if !u.is_finite()
        || !v.is_finite()
        || !(0.0..1.0).contains(&u)
        || !(0.0..1.0).contains(&v)
    {
        return None;
    }
    let (u, v) = match buffer.transform {
        wl_output::Transform::Normal => (u, v),
        wl_output::Transform::Flipped => (1.0 - u, v),
        wl_output::Transform::_90 => (v, 1.0 - u),
        wl_output::Transform::_180 => (1.0 - u, 1.0 - v),
        wl_output::Transform::_270 => (1.0 - v, u),
        wl_output::Transform::Flipped90 => (v, u),
        wl_output::Transform::Flipped180 => (u, 1.0 - v),
        wl_output::Transform::Flipped270 => (1.0 - v, 1.0 - u),
        _ => (u, v),
    };
    let source = buffer.source.unwrap_or([
        0.0,
        0.0,
        f64::from(buffer.width),
        f64::from(buffer.height),
    ]);
    let source_x = (source[0] + u * source[2]).floor();
    let source_y = (source[1] + v * source[3]).floor();
    if !source_x.is_finite()
        || !source_y.is_finite()
        || source_x < 0.0
        || source_y < 0.0
        || source_x >= f64::from(buffer.width)
        || source_y >= f64::from(buffer.height)
    {
        return None;
    }
    Some((source_y as usize * buffer.width as usize + source_x as usize) * 4)
}

fn srgb_channel(value: f64) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round() as u8
}

fn intersection(first: Rect, second: Rect) -> Option<Rect> {
    let left = first.x.max(second.x);
    let top = first.y.max(second.y);
    let right = (first.x.saturating_add(first.width as i32))
        .min(second.x.saturating_add(second.width as i32));
    let bottom = (first.y.saturating_add(first.height as i32))
        .min(second.y.saturating_add(second.height as i32));
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect::new(
        left,
        top,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for CompositorState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, _: &()) -> bool {
        crate::security_context::can_view(client, "zwlr_screencopy_manager_v1")
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                ..
            } => {
                create_frame(state, frame, data_init, state.output);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                x,
                y,
                width,
                height,
                ..
            } => {
                let region =
                    Rect::new(x, y, width.max(0) as u32, height.max(0) as u32);
                let region = intersection(state.output, region)
                    .unwrap_or(Rect::new(x, y, 0, 0));
                create_frame(state, frame, data_init, region);
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer }
            | zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                copy_frame(state, resource, &buffer);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &ZwlrScreencopyFrameV1,
        _data: &(),
    ) {
        state.screencopy.frames.remove(&resource.id());
    }
}
