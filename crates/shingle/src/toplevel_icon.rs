use std::collections::HashMap;

use wayland_protocols::xdg::toplevel_icon::v1::server::{
  xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1},
  xdg_toplevel_icon_v1::{self, XdgToplevelIconV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{Event, ToplevelIcon, ToplevelIconBuffer, state::CompositorState};

struct IconBuffer {
  source: ObjectId,
  data: ToplevelIconBuffer,
}

struct Icon {
  resource: XdgToplevelIconV1,
  name: Option<String>,
  buffers: Vec<IconBuffer>,
  immutable: bool,
}

#[derive(Default)]
pub(crate) struct Icons {
  objects: HashMap<ObjectId, Icon>,
  pending: HashMap<ObjectId, Option<ToplevelIcon>>,
  assigned: HashMap<ObjectId, ToplevelIcon>,
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, XdgToplevelIconManagerV1, ()>(1, ());
}

/// Applies an icon assignment when its toplevel surface commits.
pub(crate) fn commit(state: &mut CompositorState, object: &ObjectId) {
  let Some(surface) = state.surfaces.get(object) else {
    return;
  };
  let Some(toplevel) = surface.role.toplevel() else {
    return;
  };
  // xdg-toplevel-icon names the xdg_toplevel.
  let Some(resource) = toplevel.handle.xdg().map(Resource::id) else {
    return;
  };
  let Some(icon) = state.icons.pending.remove(&resource) else {
    return;
  };
  if let Some(icon) = icon {
    state.icons.assigned.insert(resource, icon);
  } else {
    state.icons.assigned.remove(&resource);
  }
  if let Some(window) = toplevel.window() {
    state.events.push(Event::IconChanged { window });
  }
}

pub(crate) fn for_surface(
  state: &CompositorState,
  object: &ObjectId,
) -> Option<ToplevelIcon> {
  let surface = state.surfaces.get(object)?;
  let toplevel = surface.role.toplevel()?.handle.xdg()?;
  state.icons.assigned.get(&Resource::id(toplevel)).cloned()
}

pub(crate) fn toplevel_destroyed(
  state: &mut CompositorState,
  toplevel: &ObjectId,
) {
  state.icons.pending.remove(toplevel);
  state.icons.assigned.remove(toplevel);
}

/// A retained icon rejects premature destruction of any contributing buffer.
pub(crate) fn buffer_destroyed(state: &mut CompositorState, buffer: &ObjectId) {
  for icon in state.icons.objects.values() {
    if icon.buffers.iter().any(|item| item.source == *buffer) {
      icon.resource.post_error(
        xdg_toplevel_icon_v1::Error::NoBuffer,
        "an icon buffer was destroyed before its icon",
      );
    }
  }
}

impl GlobalDispatch<XdgToplevelIconManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("xdg_toplevel_icon_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<XdgToplevelIconManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let resource = data_init.init(resource, ());
    resource.icon_size(64);
    resource.done();
  }
}

impl Dispatch<XdgToplevelIconManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &XdgToplevelIconManagerV1,
    request: xdg_toplevel_icon_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      xdg_toplevel_icon_manager_v1::Request::CreateIcon { id } => {
        let icon = data_init.init(id, ());
        state.icons.objects.insert(
          Resource::id(&icon),
          Icon {
            resource: icon,
            name: None,
            buffers: Vec::new(),
            immutable: false,
          },
        );
      }
      xdg_toplevel_icon_manager_v1::Request::SetIcon { toplevel, icon } => {
        let toplevel = Resource::id(&toplevel);
        let mut assigned = None;
        if let Some(icon) = icon {
          let id = Resource::id(&icon);
          if let Some(icon) = state.icons.objects.get_mut(&id) {
            if icon.name.is_some() || !icon.buffers.is_empty() {
              assigned = Some(ToplevelIcon {
                name: icon.name.clone(),
                buffers: icon
                  .buffers
                  .iter()
                  .map(|buffer| buffer.data.clone())
                  .collect(),
              });
            }
            icon.immutable = true;
          }
        }
        state.icons.pending.insert(toplevel, assigned);
      }
      xdg_toplevel_icon_manager_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<XdgToplevelIconV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &XdgToplevelIconV1,
    request: xdg_toplevel_icon_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let id = Resource::id(resource);
    match request {
      xdg_toplevel_icon_v1::Request::SetName { icon_name } => {
        let Some(icon) = state.icons.objects.get_mut(&id) else {
          return;
        };
        if icon.immutable {
          resource.post_error(
            xdg_toplevel_icon_v1::Error::Immutable,
            "an assigned icon cannot be renamed",
          );
          return;
        }
        icon.name = Some(icon_name);
      }
      xdg_toplevel_icon_v1::Request::AddBuffer { buffer, scale } => {
        let Some(icon) = state.icons.objects.get_mut(&id) else {
          return;
        };
        if icon.immutable {
          resource.post_error(
            xdg_toplevel_icon_v1::Error::Immutable,
            "an assigned icon cannot receive more buffers",
          );
          return;
        }
        let source = Resource::id(&buffer);
        let Some(attachment) = state.buffers.get(&source) else {
          resource.post_error(
            xdg_toplevel_icon_v1::Error::NoBuffer,
            "the icon buffer was already destroyed",
          );
          return;
        };
        if !attachment.is_shm()
          || attachment.width != attachment.height
          || scale <= 0
        {
          resource.post_error(
            xdg_toplevel_icon_v1::Error::InvalidBuffer,
            "icon buffers must be positive-scale SHM squares",
          );
          return;
        }
        // Icons keep their buffers, which are never released.
        let snapshot = match attachment.snapshot(Vec::new(), 0, 0, None) {
          Ok(snapshot) => snapshot,
          Err(message) => {
            resource
              .post_error(xdg_toplevel_icon_v1::Error::InvalidBuffer, message);
            return;
          }
        };
        let Some(rgba) = snapshot.rgba() else {
          return;
        };
        let data = ToplevelIconBuffer {
          width: snapshot.width,
          scale,
          rgba,
        };
        if let Some(existing) = icon.buffers.iter_mut().find(|item| {
          item.data.width == snapshot.width && item.data.scale == scale
        }) {
          *existing = IconBuffer { source, data };
        } else {
          icon.buffers.push(IconBuffer { source, data });
        }
      }
      xdg_toplevel_icon_v1::Request::Destroy => {}
      _ => {}
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &XdgToplevelIconV1,
    _data: &(),
  ) {
    state.icons.objects.remove(&Resource::id(resource));
  }
}
