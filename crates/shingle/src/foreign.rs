use std::collections::HashMap;

use wayland_protocols::xdg::foreign::zv2::server::{
  zxdg_exported_v2::{self, ZxdgExportedV2},
  zxdg_exporter_v2::{self, ZxdgExporterV2},
  zxdg_imported_v2::{self, ZxdgImportedV2},
  zxdg_importer_v2::{self, ZxdgImporterV2},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{ClientId, ObjectId},
};

use crate::{
  shell::{self, Role, XdgKind},
  state::CompositorState,
};

struct Export {
  surface: ObjectId,
  handle: String,
}

struct Import {
  resource: ZxdgImportedV2,
  export: Option<ObjectId>,
}

#[derive(Default)]
pub(crate) struct ForeignHandles {
  exports: HashMap<ObjectId, Export>,
  handles: HashMap<String, ObjectId>,
  imports: HashMap<ObjectId, Import>,
  imported_parents: HashMap<ObjectId, ObjectId>,
}

pub(crate) fn create_globals(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ZxdgExporterV2, ()>(1, ());
  handle.create_global::<CompositorState, ZxdgImporterV2, ()>(1, ());
}

fn handle(state: &ForeignHandles) -> Option<String> {
  loop {
    let mut bytes = [0_u8; 16];
    rustix::rand::getrandom(&mut bytes, rustix::rand::GetRandomFlags::empty())
      .ok()?;
    let handle = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    if !state.handles.contains_key(&handle) {
      return Some(handle);
    }
  }
}

fn toplevel(state: &CompositorState, object: &ObjectId) -> bool {
  state.surfaces.get(object).is_some_and(|surface| {
    matches!(
      surface.role,
      Role::Xdg {
        kind: Some(XdgKind::Toplevel(_)),
        ..
      }
    )
  })
}

/// Stops tracking a relationship replaced by ordinary xdg_toplevel parenting.
pub(crate) fn clear_import_tracking(
  state: &mut CompositorState,
  child: &ObjectId,
) {
  state.foreign.imported_parents.remove(child);
}

/// Revokes exports and invalidates only relationships created through them.
pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  surface: &ObjectId,
) {
  let exports = state
    .foreign
    .exports
    .iter()
    .filter(|(_, export)| &export.surface == surface)
    .map(|(id, _)| id.clone())
    .collect::<Vec<_>>();
  for export in exports {
    revoke_export(state, &export);
  }
  state.foreign.imported_parents.remove(surface);
}

fn revoke_export(state: &mut CompositorState, export: &ObjectId) {
  let export_id = export.clone();
  let Some(export) = state.foreign.exports.remove(export) else {
    return;
  };
  state.foreign.handles.remove(&export.handle);
  let imports = state
    .foreign
    .imports
    .iter()
    .filter(|(_, import)| import.export.as_ref() == Some(&export_id))
    .map(|(id, _)| id.clone())
    .collect::<Vec<_>>();
  revoke_imports(state, imports);
}

fn revoke_imports(state: &mut CompositorState, imports: Vec<ObjectId>) {
  for import in imports {
    if let Some(object) = state.foreign.imports.get_mut(&import) {
      object.export = None;
      object.resource.destroyed();
    }
    let children = state
      .foreign
      .imported_parents
      .iter()
      .filter(|(_, parent)| *parent == &import)
      .map(|(child, _)| child.clone())
      .collect::<Vec<_>>();
    for child in children {
      state.foreign.imported_parents.remove(&child);
      shell::set_toplevel_parent(state, &child, None);
    }
  }
}

impl GlobalDispatch<ZxdgExporterV2, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zxdg_exporter_v2");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZxdgExporterV2>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZxdgExporterV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZxdgExporterV2,
    request: zxdg_exporter_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zxdg_exporter_v2::Request::ExportToplevel { id, surface } => {
        let exported = data_init.init(id, ());
        let surface = Resource::id(&surface);
        if !toplevel(state, &surface) {
          resource.post_error(
            zxdg_exporter_v2::Error::InvalidSurface,
            "only xdg_toplevel surfaces can be exported",
          );
          return;
        }
        let Some(handle) = handle(&state.foreign) else {
          return;
        };
        exported.handle(handle.clone());
        let id = Resource::id(&exported);
        state.foreign.handles.insert(handle.clone(), id.clone());
        state.foreign.exports.insert(id, Export { surface, handle });
      }
      zxdg_exporter_v2::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZxdgExportedV2, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZxdgExportedV2,
    _request: zxdg_exported_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZxdgExportedV2,
    _data: &(),
  ) {
    revoke_export(state, &Resource::id(resource));
  }
}

impl GlobalDispatch<ZxdgImporterV2, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zxdg_importer_v2");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZxdgImporterV2>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZxdgImporterV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZxdgImporterV2,
    request: zxdg_importer_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zxdg_importer_v2::Request::ImportToplevel { id, handle } => {
        let imported = data_init.init(id, ());
        let export = state.foreign.handles.get(&handle).cloned();
        if export.is_none() {
          imported.destroyed();
        }
        state.foreign.imports.insert(
          Resource::id(&imported),
          Import {
            resource: imported,
            export,
          },
        );
      }
      zxdg_importer_v2::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZxdgImportedV2, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZxdgImportedV2,
    request: zxdg_imported_v2::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zxdg_imported_v2::Request::SetParentOf { surface } = request {
      let id = Resource::id(resource);
      let Some(export) = state
        .foreign
        .imports
        .get(&id)
        .and_then(|import| import.export.clone())
      else {
        return;
      };
      let child = Resource::id(&surface);
      if !toplevel(state, &child) {
        resource.post_error(
          zxdg_imported_v2::Error::InvalidSurface,
          "only xdg_toplevel surfaces can receive an imported parent",
        );
        return;
      }
      let parent = state
        .foreign
        .exports
        .get(&export)
        .and_then(|export| state.surfaces.get(&export.surface))
        .and_then(|surface| match &surface.role {
          Role::Xdg {
            kind: Some(XdgKind::Toplevel(toplevel)),
            ..
          } => toplevel.window(),
          _ => None,
        });
      shell::set_toplevel_parent(state, &child, parent);
      state.foreign.imported_parents.insert(child, id);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZxdgImportedV2,
    _data: &(),
  ) {
    let id = Resource::id(resource);
    state.foreign.imports.remove(&id);
    revoke_imports(state, vec![id]);
  }
}
