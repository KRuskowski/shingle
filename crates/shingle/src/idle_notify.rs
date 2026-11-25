use std::{collections::HashMap, time::Duration};

use wayland_protocols::{
  ext::idle_notify::v1::server::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
  },
  wp::idle_inhibit::zv1::server::{
    zwp_idle_inhibit_manager_v1::{self, ZwpIdleInhibitManagerV1},
    zwp_idle_inhibitor_v1::{self, ZwpIdleInhibitorV1},
  },
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::ObjectId,
};

use crate::state::CompositorState;

struct Notification {
  resource: ExtIdleNotificationV1,
  timeout: Duration,
  created: Duration,
  ignore_inhibitors: bool,
  idle: bool,
}

#[derive(Default)]
pub(crate) struct IdleNotifications {
  notifications: HashMap<ObjectId, Notification>,
  inhibitors: HashMap<ObjectId, ObjectId>,
}

pub(crate) fn create_globals(handle: &DisplayHandle) {
  handle.create_global::<CompositorState, ExtIdleNotifierV1, ()>(2, ());
  handle.create_global::<CompositorState, ZwpIdleInhibitManagerV1, ()>(1, ());
}

pub(crate) fn sync(state: &mut CompositorState) {
  let inhibited = state.idle.is_idle_inhibited();
  let now = state.now();
  let last_activity = state.idle.last_activity();
  for notification in &mut state.idle_notifications.notifications.values_mut() {
    if notification.ignore_inhibitors || !inhibited {
      let start = notification.created.max(last_activity);
      let due = now >= start.saturating_add(notification.timeout);
      if due && !notification.idle {
        notification.resource.idled();
        notification.idle = true;
      } else if !due && notification.idle {
        notification.resource.resumed();
        notification.idle = false;
      }
    }
  }
}

pub(crate) fn sync_inhibitors(state: &mut CompositorState) {
  state.idle_notifications.inhibitors.retain(|_, surface| {
    state
      .surfaces
      .get(surface)
      .is_some_and(|surface| surface.buffer.is_some())
  });
  let inhibited = !state.idle_notifications.inhibitors.is_empty();
  if state.idle.is_idle_inhibited() != inhibited {
    let commands = state.idle.set_idle_inhibited(inhibited, state.now());
    state.process_idle_commands(commands);
  }
}

impl GlobalDispatch<ExtIdleNotifierV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("ext_idle_notifier_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ExtIdleNotifierV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ExtIdleNotifierV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ExtIdleNotifierV1,
    request: ext_idle_notifier_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let (id, timeout, ignore_inhibitors) = match request {
      ext_idle_notifier_v1::Request::GetIdleNotification {
        id,
        timeout,
        ..
      } => (id, timeout, false),
      ext_idle_notifier_v1::Request::GetInputIdleNotification {
        id,
        timeout,
        ..
      } => (id, timeout, true),
      ext_idle_notifier_v1::Request::Destroy => return,
      _ => return,
    };
    let notification = data_init.init(id, ());
    state.idle_notifications.notifications.insert(
      notification.id(),
      Notification {
        resource: notification,
        timeout: Duration::from_millis(timeout.into()),
        created: state.now(),
        ignore_inhibitors,
        idle: false,
      },
    );
  }
}

impl Dispatch<ExtIdleNotificationV1, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ExtIdleNotificationV1,
    request: ext_idle_notification_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let ext_idle_notification_v1::Request::Destroy = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ExtIdleNotificationV1,
    _data: &(),
  ) {
    state
      .idle_notifications
      .notifications
      .remove(&resource.id());
  }
}

impl GlobalDispatch<ZwpIdleInhibitManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_idle_inhibit_manager_v1");

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpIdleInhibitManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpIdleInhibitManagerV1,
    request: zwp_idle_inhibit_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_idle_inhibit_manager_v1::Request::CreateInhibitor {
      id,
      surface,
    } = request
    {
      let inhibitor = data_init.init(id, ());
      state
        .idle_notifications
        .inhibitors
        .insert(inhibitor.id(), surface.id());
    }
  }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZwpIdleInhibitorV1,
    request: zwp_idle_inhibitor_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    if let zwp_idle_inhibitor_v1::Request::Destroy = request {}
  }

  fn destroyed(
    state: &mut Self,
    _client: wayland_server::backend::ClientId,
    resource: &ZwpIdleInhibitorV1,
    _data: &(),
  ) {
    state.idle_notifications.inhibitors.remove(&resource.id());
  }
}
