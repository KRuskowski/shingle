//! xdg-foreign, versions 1 and 2: a client exports its toplevel under a
//! handle, and another client imports the handle to make the exported
//! toplevel the parent of its own, as a portal's file chooser does. Wayfire
//! offers both versions (wlroots' `wlr_xdg_foreign_v1` and `_v2` on one
//! registry), and handles of either version import with the other, as in
//! wlroots. GTK 3 and xdg-desktop-portal-gtk use version 1.

use std::collections::HashMap;

use wayland_protocols::xdg::foreign::zv1::server::{
  zxdg_exported_v1::{self, ZxdgExportedV1},
  zxdg_exporter_v1::{self, ZxdgExporterV1},
  zxdg_imported_v1::{self, ZxdgImportedV1},
  zxdg_importer_v1::{self, ZxdgImporterV1},
};
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

use crate::{shell, state::CompositorState};

struct Export {
  surface: ObjectId,
  handle: String,
}

/// An imported handle of either version.
enum Imported {
  V1(ZxdgImportedV1),
  V2(ZxdgImportedV2),
}

impl Imported {
  /// Tells the client that the import is no longer valid.
  fn destroyed(&self) {
    match self {
      Imported::V1(resource) => resource.destroyed(),
      Imported::V2(resource) => resource.destroyed(),
    }
  }
}

struct Import {
  resource: Imported,
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
  handle.create_global::<CompositorState, ZxdgExporterV1, ()>(1, ());
  handle.create_global::<CompositorState, ZxdgImporterV1, ()>(1, ());
  handle.create_global::<CompositorState, ZxdgExporterV2, ()>(1, ());
  handle.create_global::<CompositorState, ZxdgImporterV2, ()>(1, ());
}

/// Records export `id` of toplevel surface `surface` and returns its new
/// handle, or None when no handle could be made.
fn export(
  state: &mut CompositorState,
  id: ObjectId,
  surface: ObjectId,
) -> Option<String> {
  let handle = handle(&state.foreign)?;
  state.foreign.handles.insert(handle.clone(), id.clone());
  state.foreign.exports.insert(
    id,
    Export {
      surface,
      handle: handle.clone(),
    },
  );
  Some(handle)
}

/// Records an import of `handle`; an unknown handle is invalid at once.
fn import(
  state: &mut CompositorState,
  resource: Imported,
  id: ObjectId,
  handle: &str,
) {
  let export = state.foreign.handles.get(handle).cloned();
  if export.is_none() {
    resource.destroyed();
  }
  state
    .foreign
    .imports
    .insert(id, Import { resource, export });
}

/// Makes import `id`'s exported toplevel the parent of `child`, if the
/// import is still valid.
fn set_parent_of(state: &mut CompositorState, id: ObjectId, child: ObjectId) {
  let Some(export) = state
    .foreign
    .imports
    .get(&id)
    .and_then(|import| import.export.clone())
  else {
    return;
  };
  let parent = state
    .foreign
    .exports
    .get(&export)
    .and_then(|export| state.surfaces.get(&export.surface))
    .and_then(|surface| {
      surface
        .role
        .toplevel()
        .and_then(|toplevel| toplevel.window())
    });
  shell::set_toplevel_parent(state, &child, parent);
  state.foreign.imported_parents.insert(child, id);
}

/// Forgets import `id` and the parents it gave.
fn import_destroyed(state: &mut CompositorState, id: ObjectId) {
  state.foreign.imports.remove(&id);
  revoke_imports(state, vec![id]);
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
  state
    .surfaces
    .get(object)
    .is_some_and(|surface| surface.role.toplevel().is_some())
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
        if let Some(handle) = export(state, Resource::id(&exported), surface) {
          exported.handle(handle);
        }
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
        let id = Resource::id(&imported);
        import(state, Imported::V2(imported), id, &handle);
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
      let child = Resource::id(&surface);
      if !toplevel(state, &child) {
        resource.post_error(
          zxdg_imported_v2::Error::InvalidSurface,
          "only xdg_toplevel surfaces can receive an imported parent",
        );
        return;
      }
      set_parent_of(state, Resource::id(resource), child);
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZxdgImportedV2,
    _data: &(),
  ) {
    import_destroyed(state, Resource::id(resource));
  }
}

impl GlobalDispatch<ZxdgExporterV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zxdg_exporter_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZxdgExporterV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZxdgExporterV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZxdgExporterV1,
    request: zxdg_exporter_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zxdg_exporter_v1::Request::Export { id, surface } = request {
      let exported = data_init.init(id, ());
      let surface = Resource::id(&surface);
      // Version 1 has no error for this; wlroots ignores the request too
      // and the export never gets a handle.
      if !toplevel(state, &surface) {
        return;
      }
      if let Some(handle) = export(state, Resource::id(&exported), surface) {
        exported.handle(handle);
      }
    }
  }
}

impl Dispatch<ZxdgExportedV1, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZxdgExportedV1,
    _request: zxdg_exported_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZxdgExportedV1,
    _data: &(),
  ) {
    revoke_export(state, &Resource::id(resource));
  }
}

impl GlobalDispatch<ZxdgImporterV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zxdg_importer_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZxdgImporterV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZxdgImporterV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZxdgImporterV1,
    request: zxdg_importer_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zxdg_importer_v1::Request::Import { id, handle } = request {
      let imported = data_init.init(id, ());
      let id = Resource::id(&imported);
      import(state, Imported::V1(imported), id, &handle);
    }
  }
}

impl Dispatch<ZxdgImportedV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &ZxdgImportedV1,
    request: zxdg_imported_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zxdg_imported_v1::Request::SetParentOf { surface } = request {
      let child = Resource::id(&surface);
      if toplevel(state, &child) {
        set_parent_of(state, Resource::id(resource), child);
      }
    }
  }

  fn destroyed(
    state: &mut Self,
    _client: ClientId,
    resource: &ZxdgImportedV1,
    _data: &(),
  ) {
    import_destroyed(state, Resource::id(resource));
  }
}

#[cfg(test)]
mod tests {
  use wayland_protocols::xdg::foreign::{
    zv1::client::{
      zxdg_exporter_v1::ZxdgExporterV1, zxdg_importer_v1::ZxdgImporterV1,
    },
    zv2::client::{
      zxdg_exporter_v2::ZxdgExporterV2, zxdg_importer_v2::ZxdgImporterV2,
    },
  };

  use crate::test_client::TestClient;

  /// The parent window of the toplevel with `app_id`.
  fn parent_of(
    compositor: &crate::Compositor,
    app_id: &str,
  ) -> Option<shingle_core::WindowId> {
    compositor
      .state
      .surfaces
      .values()
      .filter_map(|surface| surface.role.toplevel())
      .find(|toplevel| toplevel.app_id == app_id)
      .and_then(|toplevel| toplevel.parent)
  }

  fn window_of(
    compositor: &crate::Compositor,
    app_id: &str,
  ) -> Option<shingle_core::WindowId> {
    compositor
      .state
      .surfaces
      .values()
      .filter_map(|surface| surface.role.toplevel())
      .find(|toplevel| toplevel.app_id == app_id)
      .and_then(|toplevel| toplevel.window())
  }

  #[test]
  fn a_version_1_import_parents_a_toplevel_of_another_client() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut owner = TestClient::connect(&mut compositor, &path);
    let mut portal = TestClient::connect(&mut compositor, &path);
    let main = owner.toplevel(&mut compositor, "main");
    owner.attach_buffer(&mut compositor, &main.surface, 200, 100);
    let exporter = owner.bind::<ZxdgExporterV1>(1);
    let exported = exporter.export(&main.surface, &owner.handle, ());
    owner.roundtrip(&mut compositor);
    let handle = owner.events.foreign[0]
      .strip_prefix("handle ")
      .expect("a handle")
      .to_owned();
    let chooser = portal.toplevel(&mut compositor, "chooser");
    let importer = portal.bind::<ZxdgImporterV1>(1);
    let imported = importer.import(handle, &portal.handle, ());
    imported.set_parent_of(&chooser.surface);
    portal.roundtrip(&mut compositor);
    assert_eq!(
      parent_of(&compositor, "chooser"),
      window_of(&compositor, "main")
    );
    // Revoking the export ends the import and the parent it gave.
    exported.destroy();
    owner.roundtrip(&mut compositor);
    portal.roundtrip(&mut compositor);
    assert_eq!(portal.events.foreign, ["destroyed"]);
    assert_eq!(parent_of(&compositor, "chooser"), None);
    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn handles_import_across_versions_and_unknown_ones_are_destroyed() {
    let (mut compositor, path) = crate::tests::test_compositor();
    let mut client = TestClient::connect(&mut compositor, &path);
    let main = client.toplevel(&mut compositor, "main");
    client.attach_buffer(&mut compositor, &main.surface, 200, 100);
    let exporter = client.bind::<ZxdgExporterV2>(1);
    let _exported = exporter.export_toplevel(&main.surface, &client.handle, ());
    client.roundtrip(&mut compositor);
    let handle = client.events.foreign[0]
      .strip_prefix("handle ")
      .expect("a handle")
      .to_owned();
    let dialog = client.toplevel(&mut compositor, "dialog");
    let importer = client.bind::<ZxdgImporterV1>(1);
    importer
      .import(handle, &client.handle, ())
      .set_parent_of(&dialog.surface);
    client.roundtrip(&mut compositor);
    assert_eq!(
      parent_of(&compositor, "dialog"),
      window_of(&compositor, "main")
    );
    let importer = client.bind::<ZxdgImporterV2>(1);
    importer.import_toplevel("unknown".into(), &client.handle, ());
    client.roundtrip(&mut compositor);
    assert_eq!(client.events.foreign.last().unwrap(), "destroyed");
    let _ = std::fs::remove_file(path);
  }
}
