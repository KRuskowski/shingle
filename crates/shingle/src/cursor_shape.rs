use std::{
  collections::HashMap,
  sync::{Arc, Mutex, OnceLock},
};

use wayland_protocols::wp::cursor_shape::v1::server::{
  wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
  wp_cursor_shape_manager_v1::{self, WpCursorShapeManagerV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, WEnum,
};

use crate::{
  BufferSnapshot, SurfaceRole, SurfaceSnapshot,
  state::{CompositorState, SurfaceId},
};

pub(crate) use wp_cursor_shape_device_v1::Shape;

struct CursorImage {
  width: u32,
  height: u32,
  hotspot_x: i32,
  hotspot_y: i32,
  rgba: Arc<[u8]>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, WpCursorShapeManagerV1, ()>(2, ());
}

/// Returns the cursor a client chose with `wp_cursor_shape_device_v1`.
pub(crate) fn snapshot(state: &CompositorState) -> Option<SurfaceSnapshot> {
  Some(shape_snapshot(state, state.input.cursor_shape?))
}

/// Returns the theme's default arrow (`default`, or `left_ptr` in older
/// themes) when the compositor draws a default cursor and the client under
/// the pointer, if any, has not set one. On the border of a server-side
/// frame it is the border's resize cursor instead, as Wayfire's
/// `update_cursor` sets it.
pub(crate) fn default_snapshot(
  state: &CompositorState,
) -> Option<SurfaceSnapshot> {
  (state.default_cursor && !state.input.cursor_set).then(|| {
    let shape = state.frame_pointer.cursor.unwrap_or(Shape::Default);
    shape_snapshot(state, shape)
  })
}

fn shape_snapshot(state: &CompositorState, shape: Shape) -> SurfaceSnapshot {
  let image = image(shape, &state.cursor_theme, state.cursor_size);
  SurfaceSnapshot {
    id: SurfaceId(u64::MAX - shape as u64),
    window: None,
    parent: None,
    role: SurfaceRole::Cursor,
    app_id: None,
    title: None,
    icon: None,
    geometry: shingle_core::Rect::new(
      state.input.pointer_x as i32 - image.hotspot_x,
      state.input.pointer_y as i32 - image.hotspot_y,
      image.width,
      image.height,
    ),
    opacity: 1.0,
    buffer: Some(BufferSnapshot {
      width: image.width,
      height: image.height,
      source: None,
      scale: 1,
      transform: wayland_server::protocol::wl_output::Transform::Normal,
      content: crate::SnapshotContent::Pixels(image.rgba.clone()),
      damage: Vec::new(),
      offset_x: 0,
      offset_y: 0,
    }),
    layer: None,
  }
}

impl GlobalDispatch<WpCursorShapeManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("wp_cursor_shape_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpCursorShapeManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WpCursorShapeManagerV1,
    request: wp_cursor_shape_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      wp_cursor_shape_manager_v1::Request::GetPointer {
        cursor_shape_device,
        ..
      } => {
        data_init.init(cursor_shape_device, ());
      }
      wp_cursor_shape_manager_v1::Request::GetTabletToolV2 { .. } => {}
      wp_cursor_shape_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    _resource: &WpCursorShapeDeviceV1,
    request: wp_cursor_shape_device_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let wp_cursor_shape_device_v1::Request::SetShape { serial, shape } =
      request
    {
      let WEnum::Value(shape) = shape else {
        return;
      };
      if serial != state.input.pointer_serial {
        return;
      }
      let focused = state.input.pointer_focus.as_ref().is_some_and(|object| {
        state
          .surfaces
          .get(object)
          .is_some_and(|surface| surface.client == client.id())
      });
      if focused {
        state.input.cursor_shape = Some(shape);
        state.input.cursor_surface = None;
        state.input.cursor_set = true;
      }
    }
  }
}

type CursorCache = Mutex<HashMap<(u32, String, u32), Arc<CursorImage>>>;

/// The theme's default arrow (`default`, else `left_ptr`) for the X root
/// window, in the X server's format; the built-in arrow when the theme
/// has none.
pub(crate) fn default_x_cursor(
  state: &CompositorState,
) -> Option<shingle_xwayland::CursorImage> {
  let image = image(Shape::Default, &state.cursor_theme, state.cursor_size);
  shingle_xwayland::CursorImage::from_rgba(
    image.width,
    image.height,
    (image.hotspot_x, image.hotspot_y),
    &image.rgba,
  )
}

fn image(
  shape: Shape,
  configured_theme: &str,
  configured_size: u32,
) -> Arc<CursorImage> {
  let theme = if configured_theme == "default" {
    std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into())
  } else {
    configured_theme.to_owned()
  };
  let size = if configured_size == 24 {
    std::env::var("XCURSOR_SIZE")
      .ok()
      .and_then(|size| size.parse().ok())
      .unwrap_or(24)
  } else {
    configured_size
  }
  .clamp(1, 256);
  let key = (shape as u32, theme.clone(), size);
  static CACHE: OnceLock<CursorCache> = OnceLock::new();
  let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
  if let Some(image) =
    cache.lock().ok().and_then(|cache| cache.get(&key).cloned())
  {
    return image;
  }
  let image = Arc::new(
    load_image(shape, &theme, size).unwrap_or_else(|| fallback_image(size)),
  );
  if let Ok(mut cache) = cache.lock() {
    cache.insert(key, image.clone());
  }
  image
}

fn load_image(shape: Shape, theme: &str, size: u32) -> Option<CursorImage> {
  let theme = xcursor::CursorTheme::load(theme);
  let path = theme.load_icon(shape_name(shape)).or_else(|| {
    (shape == Shape::Default)
      .then(|| theme.load_icon("left_ptr"))
      .flatten()
  })?;
  let bytes = std::fs::read(path).ok()?;
  let images = xcursor::parser::parse_xcursor(&bytes)?;
  let image = images
    .into_iter()
    .min_by_key(|image| image.size.abs_diff(size))?;
  Some(CursorImage {
    width: image.width,
    height: image.height,
    hotspot_x: image.xhot as i32,
    hotspot_y: image.yhot as i32,
    rgba: image.pixels_rgba.into(),
  })
}

fn fallback_image(size: u32) -> CursorImage {
  let size = size as usize;
  let bottom = size.saturating_sub(size / 6).max(1);
  let mut rgba = vec![0; size * size * 4];
  for y in 0..size {
    for x in 0..size {
      let edge = y / 2;
      let inside = x <= edge && y < bottom;
      if !inside {
        continue;
      }
      let border = x == 0 || y == 0 || x == edge || y + 1 == bottom;
      let color = if border {
        [0, 0, 0, 255]
      } else {
        [255, 255, 255, 255]
      };
      rgba[(y * size + x) * 4..][..4].copy_from_slice(&color);
    }
  }
  CursorImage {
    width: size as u32,
    height: size as u32,
    hotspot_x: 0,
    hotspot_y: 0,
    rgba: rgba.into(),
  }
}

fn shape_name(shape: Shape) -> &'static str {
  match shape {
    Shape::Default => "default",
    Shape::ContextMenu => "context-menu",
    Shape::Help => "help",
    Shape::Pointer => "pointer",
    Shape::Progress => "progress",
    Shape::Wait => "wait",
    Shape::Cell => "cell",
    Shape::Crosshair => "crosshair",
    Shape::Text => "text",
    Shape::VerticalText => "vertical-text",
    Shape::Alias => "alias",
    Shape::Copy => "copy",
    Shape::Move => "move",
    Shape::NoDrop => "no-drop",
    Shape::NotAllowed => "not-allowed",
    Shape::Grab => "grab",
    Shape::Grabbing => "grabbing",
    Shape::EResize => "e-resize",
    Shape::NResize => "n-resize",
    Shape::NeResize => "ne-resize",
    Shape::NwResize => "nw-resize",
    Shape::SResize => "s-resize",
    Shape::SeResize => "se-resize",
    Shape::SwResize => "sw-resize",
    Shape::WResize => "w-resize",
    Shape::EwResize => "ew-resize",
    Shape::NsResize => "ns-resize",
    Shape::NeswResize => "nesw-resize",
    Shape::NwseResize => "nwse-resize",
    Shape::ColResize => "col-resize",
    Shape::RowResize => "row-resize",
    Shape::AllScroll => "all-scroll",
    Shape::ZoomIn => "zoom-in",
    Shape::ZoomOut => "zoom-out",
    Shape::DndAsk => "dnd-ask",
    Shape::AllResize => "all-resize",
    _ => "default",
  }
}

#[cfg(test)]
mod tests {
  use wayland_server::backend::ObjectId;

  use super::*;
  use crate::input_device::tests::test_state;

  fn cursors(state: &CompositorState) -> Vec<SurfaceSnapshot> {
    state
      .visible_surfaces()
      .into_iter()
      .filter(|snapshot| snapshot.role == SurfaceRole::Cursor)
      .collect()
  }

  #[test]
  fn no_cursor_is_drawn_without_the_default_cursor() {
    let state = test_state();
    assert!(cursors(&state).is_empty());
  }

  #[test]
  fn default_cursor_follows_the_pointer() {
    let mut state = test_state();
    state.default_cursor = true;
    crate::input::pointer_motion(&mut state, 100.0, 50.0);
    let cursors = cursors(&state);
    assert_eq!(cursors.len(), 1);
    let cursor = &cursors[0];
    let buffer = cursor.buffer.as_ref().expect("the cursor has pixels");
    assert!(buffer.width > 0 && buffer.height > 0);
    assert!(cursor.geometry.x <= 100 && cursor.geometry.y <= 50);
    assert!(
      cursor.geometry.x + cursor.geometry.width as i32 > 100
        && cursor.geometry.y + cursor.geometry.height as i32 > 50
    );
  }

  #[test]
  fn a_hidden_client_cursor_is_not_replaced_by_the_default() {
    let mut state = test_state();
    state.default_cursor = true;
    // What wl_pointer.set_cursor with a null surface leaves behind.
    state.input.cursor_set = true;
    assert!(cursors(&state).is_empty());
  }

  #[test]
  fn leaving_a_client_brings_back_the_default_cursor() {
    let mut state = test_state();
    state.default_cursor = true;
    state.input.pointer_focus = Some(ObjectId::null());
    state.input.cursor_set = true;
    crate::input::pointer_motion(&mut state, 10.0, 10.0);
    assert_eq!(state.input.pointer_focus, None);
    assert_eq!(cursors(&state).len(), 1);
  }
}
