//! wlr-layer-shell: panels, backgrounds and overlays, each on one output
//! (docs/design/multi-output.md, section 8). A surface goes on the output
//! it names, else on the active output; its anchors, exclusive zone and
//! the workarea it takes are its output's, and it is sent `closed` when
//! that output goes.

use std::collections::BTreeSet;

use shingle_core::{OutputId, Rect};
use wayland_protocols_wlr::layer_shell::v1::server::{
  zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
  zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum, backend::ObjectId,
};

use crate::{
  shell::{self, Role},
  state::{CompositorState, PendingBuffer, Surface},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayerState {
  pub(crate) layer: zwlr_layer_shell_v1::Layer,
  pub(crate) width: u32,
  pub(crate) height: u32,
  pub(crate) anchor: zwlr_layer_surface_v1::Anchor,
  pub(crate) exclusive_zone: i32,
  pub(crate) margin: (i32, i32, i32, i32),
  pub(crate) keyboard: zwlr_layer_surface_v1::KeyboardInteractivity,
  pub(crate) exclusive_edge: Option<zwlr_layer_surface_v1::Anchor>,
}

impl LayerState {
  fn new(layer: zwlr_layer_shell_v1::Layer) -> Self {
    Self {
      layer,
      width: 0,
      height: 0,
      anchor: zwlr_layer_surface_v1::Anchor::empty(),
      exclusive_zone: 0,
      margin: (0, 0, 0, 0),
      keyboard: zwlr_layer_surface_v1::KeyboardInteractivity::None,
      exclusive_edge: None,
    }
  }
}

/// The public name of `role`'s current layer.
pub(crate) fn shell_layer(role: &LayerRole) -> crate::ShellLayer {
  match role.current.layer {
    zwlr_layer_shell_v1::Layer::Background => crate::ShellLayer::Background,
    zwlr_layer_shell_v1::Layer::Bottom => crate::ShellLayer::Bottom,
    zwlr_layer_shell_v1::Layer::Top => crate::ShellLayer::Top,
    _ => crate::ShellLayer::Overlay,
  }
}

#[derive(Clone)]
pub(crate) struct LayerRole {
  pub(crate) resource: ZwlrLayerSurfaceV1,
  /// The output the surface is on.
  pub(crate) output: OutputId,
  pub(crate) current: LayerState,
  pending: LayerState,
  serials: BTreeSet<u32>,
  configured: (u32, u32),
  initial_configured: bool,
  acked: Option<u32>,
  mapped: bool,
}

impl LayerRole {
  pub(crate) fn accepts_keyboard(&self) -> bool {
    self.current.keyboard != zwlr_layer_surface_v1::KeyboardInteractivity::None
  }

  pub(crate) fn exclusive_keyboard(&self) -> bool {
    self.current.keyboard
      == zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
      && matches!(
        self.current.layer,
        zwlr_layer_shell_v1::Layer::Top | zwlr_layer_shell_v1::Layer::Overlay
      )
  }
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZwlrLayerShellV1, ()>(5, ());
}

pub(crate) fn validate_commit(surface: &Surface) -> Result<(), ()> {
  let Role::Layer(layer) = &surface.role else {
    return Ok(());
  };
  if matches!(surface.pending.buffer, PendingBuffer::Attach(..))
    && (!layer.initial_configured || layer.acked.is_none())
  {
    layer.resource.post_error(
      zwlr_layer_surface_v1::Error::InvalidSurfaceState,
      "ack the initial layer-surface configure before attaching a buffer",
    );
    return Err(());
  }
  Ok(())
}

pub(crate) fn committed(state: &mut CompositorState, object: &ObjectId) {
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::Layer(layer) = &mut surface.role else {
    return;
  };
  let mapped = surface.buffer.is_some();
  let map_changed = mapped != layer.mapped;
  layer.mapped = mapped;
  if !mapped && layer.initial_configured {
    layer.initial_configured = false;
    layer.acked = None;
  }
  let changed = layer.pending != layer.current;
  layer.current = layer.pending;
  let configure = !layer.initial_configured || changed;
  if configure {
    configure_surface(state, object);
  }
  if configure || map_changed {
    update_workarea(state);
  }
  crate::input::sync_focus(state);
  shell::reconfigure_all(state);
}

pub(crate) fn geometry(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<Rect> {
  let surface = state.surfaces.get(object)?;
  let Role::Layer(layer) = &surface.role else {
    return None;
  };
  let output = state.outputs.get(&layer.output)?.rect;
  let area = if layer.current.exclusive_zone == 0 {
    usable_output(state, &layer.output, output)
  } else {
    output
  };
  // A buffer covers its viewport's destination, else its size divided by
  // its buffer scale, as for every other role.
  let (width, height) = match &surface.buffer {
    Some(_) => crate::viewport::dimensions(state, object)
      .unwrap_or_else(|| crate::shell::logical_buffer_dimensions(surface)),
    None => layer.configured,
  };
  let anchor = layer.current.anchor;
  let (margin_top, margin_right, margin_bottom, margin_left) =
    layer.current.margin;
  let x = if anchor.contains(zwlr_layer_surface_v1::Anchor::Left) {
    area.x.saturating_add(margin_left)
  } else if anchor.contains(zwlr_layer_surface_v1::Anchor::Right) {
    area
      .x
      .saturating_add(area.width as i32)
      .saturating_sub(width as i32)
      .saturating_sub(margin_right)
  } else {
    area
      .x
      .saturating_add((area.width as i32).saturating_sub(width as i32) / 2)
  };
  let y = if anchor.contains(zwlr_layer_surface_v1::Anchor::Top) {
    area.y.saturating_add(margin_top)
  } else if anchor.contains(zwlr_layer_surface_v1::Anchor::Bottom) {
    area
      .y
      .saturating_add(area.height as i32)
      .saturating_sub(height as i32)
      .saturating_sub(margin_bottom)
  } else {
    area
      .y
      .saturating_add((area.height as i32).saturating_sub(height as i32) / 2)
  };
  Some(Rect::new(x, y, width, height))
}

/// Lays out every workspace set in its output's workarea: the output's
/// rectangle minus the exclusive zones of the layer surfaces on it.
pub(crate) fn update_workarea(state: &mut CompositorState) {
  let workareas = state
    .outputs
    .iter()
    .map(|output| {
      (
        output.id.clone(),
        usable_output(state, &output.id, output.rect),
      )
    })
    .collect::<Vec<_>>();
  for (id, workarea) in workareas {
    let _ = state.core.set_output_workarea(&id, workarea);
  }
  shell::reconfigure_all(state);
  reconfigure(state);
}

/// Sends `closed` to every layer surface on output `id`, which is going,
/// and forgets them, as Wayfire closes them; panels create new ones on
/// the outputs that remain.
pub(crate) fn output_removed(state: &mut CompositorState, id: &OutputId) {
  let mut closed = false;
  for surface in state.surfaces.values_mut() {
    let Role::Layer(layer) = &surface.role else {
      continue;
    };
    if layer.output != *id {
      continue;
    }
    layer.resource.closed();
    surface.role = Role::Dead;
    surface.buffer = None;
    closed = true;
  }
  if closed {
    crate::input::sync_focus(state);
  }
}

/// Moves the layer surfaces of output `old` to `new`, its new name.
pub(crate) fn output_renamed(
  state: &mut CompositorState,
  old: &OutputId,
  new: &OutputId,
) {
  for surface in state.surfaces.values_mut() {
    if let Role::Layer(layer) = &mut surface.role
      && layer.output == *old
    {
      layer.output = new.clone();
    }
  }
}

/// The output a layer surface is on.
pub(crate) fn output_of(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<OutputId> {
  match &state.surfaces.get(object)?.role {
    Role::Layer(layer) => Some(layer.output.clone()),
    _ => None,
  }
}

pub(crate) fn reconfigure(state: &mut CompositorState) {
  let mut objects = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      matches!(surface.role, Role::Layer(_)).then_some(object.clone())
    })
    .collect::<Vec<_>>();
  objects.sort_by_key(|object| state.surfaces[object].id);
  for object in objects {
    configure_surface(state, &object);
  }
}

pub(crate) fn sync_exclusive_focus(state: &mut CompositorState) -> bool {
  let target = state
    .surfaces
    .iter()
    .filter_map(|(object, surface)| {
      (surface.buffer.is_some()
        && matches!(
            &surface.role,
            Role::Layer(layer) if layer.exclusive_keyboard()
        ))
      .then_some((surface.id, object.clone()))
    })
    .max_by_key(|(id, _)| *id)
    .map(|(_, object)| object);
  if let Some(target) = target {
    if state.input.keyboard_focus.as_ref() != Some(&target) {
      state.input.keyboard_focus = Some(target);
    }
    return true;
  }
  false
}

pub(crate) fn assign_popup(
  state: &mut CompositorState,
  popup: &wayland_protocols::xdg::shell::server::xdg_popup::XdgPopup,
  parent: &ObjectId,
) {
  shell::assign_pending_popup(state, popup, parent);
}

fn configure_surface(state: &mut CompositorState, object: &ObjectId) {
  let Some(output) = output_of(state, object)
    .and_then(|id| state.outputs.get(&id))
    .map(|output| output.rect)
  else {
    return;
  };
  let serial = state.serial();
  let Some(surface) = state.surfaces.get_mut(object) else {
    return;
  };
  let Role::Layer(layer) = &mut surface.role else {
    return;
  };
  let (width, height) = configured_size(output, &layer.pending);
  layer.resource.configure(serial, width, height);
  layer.serials.insert(serial);
  layer.configured = (width, height);
  layer.initial_configured = true;
}

fn configured_size(output: Rect, layer: &LayerState) -> (u32, u32) {
  let width = if layer.width != 0 {
    layer.width
  } else if layer.anchor.contains(
    zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right,
  ) {
    output.width.saturating_sub(
      layer.margin.1.max(0) as u32 + layer.margin.3.max(0) as u32,
    )
  } else {
    0
  };
  let height = if layer.height != 0 {
    layer.height
  } else if layer.anchor.contains(
    zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Bottom,
  ) {
    output.height.saturating_sub(
      layer.margin.0.max(0) as u32 + layer.margin.2.max(0) as u32,
    )
  } else {
    0
  };
  (width, height)
}

/// `output`, the rectangle of output `id`, minus the exclusive zones of
/// the mapped layer surfaces on it.
fn usable_output(state: &CompositorState, id: &OutputId, output: Rect) -> Rect {
  let mut output = output;
  let mut surfaces = state
    .surfaces
    .values()
    .filter(|surface| surface.buffer.is_some())
    .filter_map(|surface| match &surface.role {
      Role::Layer(layer) if layer.output == *id => Some(layer),
      _ => None,
    })
    .collect::<Vec<_>>();
  surfaces.sort_by_key(|layer| layer.current.layer as u32);
  for layer in surfaces {
    let zone = layer.current.exclusive_zone.max(0) as u32;
    if zone == 0 {
      continue;
    }
    let Some(edge) = exclusive_edge(&layer.current) else {
      continue;
    };
    match edge {
      zwlr_layer_surface_v1::Anchor::Top => {
        let amount = zone.min(output.height);
        output.y = output.y.saturating_add(amount as i32);
        output.height -= amount;
      }
      zwlr_layer_surface_v1::Anchor::Bottom => {
        output.height = output.height.saturating_sub(zone);
      }
      zwlr_layer_surface_v1::Anchor::Left => {
        let amount = zone.min(output.width);
        output.x = output.x.saturating_add(amount as i32);
        output.width -= amount;
      }
      zwlr_layer_surface_v1::Anchor::Right => {
        output.width = output.width.saturating_sub(zone);
      }
      _ => {}
    }
  }
  output
}

fn exclusive_edge(layer: &LayerState) -> Option<zwlr_layer_surface_v1::Anchor> {
  if let Some(edge) = layer.exclusive_edge {
    return Some(edge);
  }
  let anchor = layer.anchor;
  for (edge, horizontal) in [
    (zwlr_layer_surface_v1::Anchor::Top, true),
    (zwlr_layer_surface_v1::Anchor::Bottom, true),
    (zwlr_layer_surface_v1::Anchor::Left, false),
    (zwlr_layer_surface_v1::Anchor::Right, false),
  ] {
    if !anchor.contains(edge) {
      continue;
    }
    let perpendicular = if horizontal {
      zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right
    } else {
      zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Bottom
    };
    if anchor.contains(perpendicular) || anchor.bits().count_ones() == 1 {
      return Some(edge);
    }
  }
  None
}

fn destroy_resource(
  state: &mut CompositorState,
  resource: &ZwlrLayerSurfaceV1,
) {
  let object = state.surfaces.iter().find_map(|(object, surface)| {
    matches!(&surface.role, Role::Layer(layer) if layer.resource == *resource)
      .then_some(object.clone())
  });
  if let Some(object) = object
    && let Some(surface) = state.surfaces.get_mut(&object)
  {
    surface.role = Role::Dead;
    surface.buffer = None;
    update_workarea(state);
    crate::input::sync_focus(state);
    shell::reconfigure_all(state);
  }
}

impl GlobalDispatch<ZwlrLayerShellV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwlr_layer_shell_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwlrLayerShellV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwlrLayerShellV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrLayerShellV1,
    request: zwlr_layer_shell_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwlr_layer_shell_v1::Request::GetLayerSurface {
        id,
        surface,
        output,
        layer: WEnum::Value(layer),
        ..
      } => {
        let can_assign =
          state.surfaces.get(&surface.id()).is_some_and(|surface| {
            matches!(surface.role, Role::None) && !surface.committed_once
          });
        if !can_assign {
          resource.post_error(
            zwlr_layer_shell_v1::Error::Role,
            "layer surfaces require an unused wl_surface",
          );
          return;
        }
        let layer_resource = data_init.init(id, ());
        // The requested output, else the active one, as Wayfire chooses
        // (src/view/layer-shell/layer-shell.cpp); an output that is gone
        // counts as none.
        let output = output
          .and_then(|output| state.outputs.of_resource(&output))
          .map_or_else(
            || state.active().id.clone(),
            |output| output.id.clone(),
          );
        if let Some(surface) = state.surfaces.get_mut(&surface.id()) {
          surface.role = Role::Layer(Box::new(LayerRole {
            resource: layer_resource,
            output,
            current: LayerState::new(layer),
            pending: LayerState::new(layer),
            serials: BTreeSet::new(),
            configured: (0, 0),
            initial_configured: false,
            acked: None,
            mapped: false,
          }));
        }
      }
      zwlr_layer_shell_v1::Request::GetLayerSurface { .. } => {
        resource.post_error(
          zwlr_layer_shell_v1::Error::InvalidLayer,
          "unknown layer",
        );
      }
      zwlr_layer_shell_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZwlrLayerSurfaceV1,
    request: zwlr_layer_surface_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let object = state.surfaces.iter().find_map(|(object, surface)| {
      matches!(&surface.role, Role::Layer(layer) if layer.resource == *resource)
        .then_some(object.clone())
    });
    let Some(object) = object else {
      return;
    };
    let Some(surface) = state.surfaces.get_mut(&object) else {
      return;
    };
    let Role::Layer(layer) = &mut surface.role else {
      return;
    };
    match request {
      zwlr_layer_surface_v1::Request::SetSize { width, height } => {
        layer.pending.width = width;
        layer.pending.height = height;
      }
      zwlr_layer_surface_v1::Request::SetAnchor {
        anchor: WEnum::Value(anchor),
      } => layer.pending.anchor = anchor,
      zwlr_layer_surface_v1::Request::SetExclusiveZone { zone } => {
        layer.pending.exclusive_zone = zone;
      }
      zwlr_layer_surface_v1::Request::SetMargin {
        top,
        right,
        bottom,
        left,
      } => layer.pending.margin = (top, right, bottom, left),
      zwlr_layer_surface_v1::Request::SetKeyboardInteractivity {
        keyboard_interactivity: WEnum::Value(keyboard),
      } => layer.pending.keyboard = keyboard,
      zwlr_layer_surface_v1::Request::GetPopup { popup } => {
        let parent = object.clone();
        assign_popup(state, &popup, &parent);
      }
      zwlr_layer_surface_v1::Request::AckConfigure { serial } => {
        if layer.serials.contains(&serial) {
          layer.acked = Some(serial);
        }
      }
      zwlr_layer_surface_v1::Request::SetLayer {
        layer: WEnum::Value(new_layer),
      } => layer.pending.layer = new_layer,
      zwlr_layer_surface_v1::Request::SetExclusiveEdge {
        edge: WEnum::Value(edge),
      } => {
        if layer.pending.anchor.contains(edge) {
          layer.pending.exclusive_edge = Some(edge);
        } else {
          resource.post_error(
            zwlr_layer_surface_v1::Error::InvalidExclusiveEdge,
            "exclusive edge must be anchored",
          );
        }
      }
      zwlr_layer_surface_v1::Request::Destroy => {
        destroy_resource(state, resource)
      }
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZwlrLayerSurfaceV1,
    _data: &(),
  ) {
    destroy_resource(state, resource);
  }
}
