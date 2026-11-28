use shingle_core::Rect;
use wayland_protocols::wp::input_method::zv1::server::{
  zwp_input_panel_surface_v1::{self, ZwpInputPanelSurfaceV1},
  zwp_input_panel_v1::{self, ZwpInputPanelV1},
};
use wayland_protocols_misc::zwp_input_method_v2::server::{
  zwp_input_method_v2::ZwpInputMethodV2,
  zwp_input_popup_surface_v2::{self, ZwpInputPopupSurfaceV2},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{
  shell::{self, Role},
  state::CompositorState,
  text_input::TextInputSnapshot,
};

#[derive(Clone, Copy)]
enum PanelKind {
  Bottom,
  Overlay,
}

#[derive(Clone)]
enum PanelResource {
  V1(ZwpInputPanelSurfaceV1),
  V2(ZwpInputPopupSurfaceV2),
}

#[derive(Clone)]
pub(crate) struct InputPanelRole {
  resource: PanelResource,
  owner: Option<ObjectId>,
  kind: PanelKind,
  cursor_rectangle: Option<Rect>,
  pub(crate) visible: bool,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwpInputPanelV1, ()>(1, ());
}

/// Creates a v2 popup and gives its surface the shared input-panel role.
pub(crate) fn create_v2(
  state: &mut CompositorState,
  method: &ZwpInputMethodV2,
  popup: ZwpInputPopupSurfaceV2,
  surface: &wayland_server::protocol::wl_surface::WlSurface,
) {
  let object = Resource::id(surface);
  let Some(surface) = state.surfaces.get_mut(&object) else {
    return;
  };
  if !matches!(surface.role, Role::None) {
    method.post_error(0u32, "input popup surface already has a role");
    return;
  }
  surface.role = Role::InputPanel(Box::new(InputPanelRole {
    resource: PanelResource::V2(popup),
    owner: Some(Resource::id(method)),
    kind: PanelKind::Overlay,
    cursor_rectangle: None,
    visible: false,
  }));
}

/// Makes popup children inert when their v2 input method is destroyed.
pub(crate) fn method_destroyed(state: &mut CompositorState, method: &ObjectId) {
  for surface in state.surfaces.values_mut() {
    if matches!(
        &surface.role,
        Role::InputPanel(panel) if panel.owner.as_ref() == Some(method)
    ) {
      surface.role = Role::Dead;
    }
  }
}

/// Shows panels only for active text input and updates v2 cursor hints.
pub(crate) fn sync(state: &mut CompositorState) {
  let active = state
    .text_inputs
    .active()
    .and_then(|input| state.text_inputs.snapshot(&input));
  let active_methods = crate::input_method_v2::active_methods(state);
  let rectangle = active.as_ref().and_then(|active| active.cursor_rectangle);
  for surface in state.surfaces.values_mut() {
    if let Role::InputPanel(panel) = &mut surface.role {
      panel.visible = active.is_some()
        && match &panel.resource {
          PanelResource::V1(_) => true,
          PanelResource::V2(_) => panel
            .owner
            .as_ref()
            .is_some_and(|owner| active_methods.contains(owner)),
        };
      if let PanelResource::V2(popup) = &panel.resource
        && panel.visible
        && let Some(rectangle) = rectangle
        && panel.cursor_rectangle != Some(rectangle)
      {
        popup.text_input_rectangle(
          rectangle.x,
          rectangle.y,
          rectangle.width as i32,
          rectangle.height as i32,
        );
        panel.cursor_rectangle = Some(rectangle);
      }
    }
  }
}

pub(crate) fn geometry(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  let surface = state.surfaces.get(object)?;
  let Role::InputPanel(panel) = &surface.role else {
    return None;
  };
  let (width, height) = crate::viewport::dimensions(state, object)
    .unwrap_or_else(|| shell::logical_buffer_dimensions(surface));
  let output = state.primary().rect;
  if matches!(panel.kind, PanelKind::Bottom) {
    return Some(Rect::new(
      centered(output.x, output.width, width),
      bottom(output.y, output.height, height),
      width,
      height,
    ));
  }

  let active = state
    .text_inputs
    .active()
    .and_then(|input| state.text_inputs.snapshot(&input));
  let Some(geometry) = active
    .as_ref()
    .and_then(|active| overlay_geometry(state, active, output, width, height))
  else {
    return Some(Rect::new(
      centered(output.x, output.width, width),
      bottom(output.y, output.height, height),
      width,
      height,
    ));
  };
  Some(geometry)
}

fn overlay_geometry(
  state: &CompositorState,
  active: &TextInputSnapshot,
  output: Rect,
  width: u32,
  height: u32,
) -> Option<Rect> {
  let cursor = active.cursor_rectangle?;
  let parent = crate::straddle::drawn_rect(state, &active.surface)?;
  let cursor_x =
    i64::from(parent.x) + i64::from(cursor.x) + i64::from(cursor.width) / 2;
  let cursor_top = i64::from(parent.y) + i64::from(cursor.y);
  let below = cursor_top + i64::from(cursor.height);
  let output_right = i64::from(output.x) + i64::from(output.width);
  let output_bottom = i64::from(output.y) + i64::from(output.height);
  let x = (cursor_x - i64::from(width) / 2).clamp(
    i64::from(output.x),
    (output_right - i64::from(width)).max(i64::from(output.x)),
  );
  let requested_y = if below + i64::from(height) <= output_bottom {
    below
  } else {
    cursor_top - i64::from(height)
  };
  let y = requested_y.clamp(
    i64::from(output.y),
    (output_bottom - i64::from(height)).max(i64::from(output.y)),
  );
  Some(Rect::new(x as i32, y as i32, width, height))
}

fn centered(origin: i32, available: u32, actual: u32) -> i32 {
  (i64::from(origin) + (i64::from(available) - i64::from(actual)) / 2) as i32
}

fn bottom(origin: i32, available: u32, actual: u32) -> i32 {
  (i64::from(origin) + i64::from(available) - i64::from(actual)) as i32
}

impl GlobalDispatch<ZwpInputPanelV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_input_panel_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpInputPanelV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpInputPanelV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpInputPanelV1,
    request: zwp_input_panel_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_input_panel_v1::Request::GetInputPanelSurface { id, surface } =
      request
    {
      let object = Resource::id(&surface);
      let panel = data_init.init(id, ());
      if let Some(surface) = state.surfaces.get_mut(&object)
        && matches!(surface.role, Role::None)
      {
        surface.role = Role::InputPanel(Box::new(InputPanelRole {
          resource: PanelResource::V1(panel),
          owner: None,
          kind: PanelKind::Bottom,
          cursor_rectangle: None,
          visible: false,
        }));
      }
    }
  }
}

impl Dispatch<ZwpInputPanelSurfaceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwpInputPanelSurfaceV1,
    request: zwp_input_panel_surface_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let kind = match request {
      zwp_input_panel_surface_v1::Request::SetToplevel { .. } => {
        Some(PanelKind::Bottom)
      }
      zwp_input_panel_surface_v1::Request::SetOverlayPanel => {
        Some(PanelKind::Overlay)
      }
      _ => None,
    };
    if let Some(kind) = kind {
      for surface in state.surfaces.values_mut() {
        if let Role::InputPanel(panel) = &mut surface.role
          && matches!(
              &panel.resource,
              PanelResource::V1(panel) if panel == resource
          )
        {
          panel.kind = kind;
        }
      }
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpInputPanelSurfaceV1,
    _data: &(),
  ) {
    destroy(state, PanelResource::V1(resource.clone()));
  }
}

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZwpInputPopupSurfaceV2,
    request: zwp_input_popup_surface_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_input_popup_surface_v2::Request::Destroy = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZwpInputPopupSurfaceV2,
    _data: &(),
  ) {
    destroy(state, PanelResource::V2(resource.clone()));
  }
}

fn destroy(state: &mut CompositorState, resource: PanelResource) {
  for surface in state.surfaces.values_mut() {
    let matches = match (&surface.role, &resource) {
      (Role::InputPanel(panel), PanelResource::V1(resource)) => matches!(
          &panel.resource,
          PanelResource::V1(panel) if panel == resource
      ),
      (Role::InputPanel(panel), PanelResource::V2(resource)) => matches!(
          &panel.resource,
          PanelResource::V2(panel) if panel == resource
      ),
      _ => false,
    };
    if matches {
      surface.role = Role::Dead;
    }
  }
}
