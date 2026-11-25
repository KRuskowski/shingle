use std::{
  collections::HashSet,
  os::fd::OwnedFd,
  os::unix::net::UnixListener,
  sync::{Arc, Mutex},
};

use wayland_protocols::wp::security_context::v1::server::{
  wp_security_context_manager_v1::{self, WpSecurityContextManagerV1},
  wp_security_context_v1::{self, WpSecurityContextV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::state::{ClientDataImpl, CompositorState};

#[derive(Clone, Debug, Default)]
pub(crate) struct SandboxMetadata {
  pub(crate) engine: Option<String>,
  pub(crate) app_id: Option<String>,
  pub(crate) instance_id: Option<String>,
}

#[derive(Default)]
pub(crate) struct ContextState {
  pub(crate) metadata: SandboxMetadata,
  pub(crate) committed: bool,
}

pub(crate) struct PendingListener {
  pub(crate) id: u64,
  pub(crate) listen_fd: OwnedFd,
  pub(crate) close_fd: OwnedFd,
  pub(crate) context: Arc<Mutex<ContextState>>,
}

pub(crate) struct SecurityListener {
  pub(crate) socket: UnixListener,
  pub(crate) _close_fd: OwnedFd,
  pub(crate) context: Arc<Mutex<ContextState>>,
  pub(crate) listen_source: Option<calloop::RegistrationToken>,
  pub(crate) close_source: Option<calloop::RegistrationToken>,
}

pub(crate) struct SecurityContexts {
  pub(crate) privileged_protocols: Arc<HashSet<String>>,
  pub(crate) pending: Vec<PendingListener>,
  pub(crate) ready: HashSet<u64>,
  pub(crate) closed: HashSet<u64>,
  next_listener: u64,
}

impl SecurityContexts {
  pub(crate) fn new(protocols: HashSet<String>) -> Self {
    Self {
      privileged_protocols: Arc::new(protocols),
      pending: Vec::new(),
      ready: HashSet::new(),
      closed: HashSet::new(),
      next_listener: 1,
    }
  }
}

pub(crate) fn parse_privileged(value: &str) -> HashSet<String> {
  value
    .split(',')
    .map(str::trim)
    .filter(|name| !name.is_empty())
    .map(str::to_owned)
    .collect()
}

/// Applies the configured policy only to clients carrying sandbox metadata.
pub(crate) fn can_view(client: Client, interface: &str) -> bool {
  let Some(data) = client.get_data::<ClientDataImpl>() else {
    return true;
  };
  if data
    .sandbox
    .as_ref()
    .is_none_or(|metadata| metadata.engine.is_none())
  {
    return true;
  }
  !data.privileged_protocols.contains(interface)
}

/// Implements global visibility for one protocol interface.
macro_rules! protocol_can_view {
  ($interface:literal) => {
    fn can_view(client: wayland_server::Client, _: &()) -> bool {
      crate::security_context::can_view(client, $interface)
    }
  };
}
pub(crate) use protocol_can_view;

/// Sandboxed clients must not create nested contexts.
pub(crate) fn can_view_manager(client: Client) -> bool {
  client
    .get_data::<ClientDataImpl>()
    .is_none_or(|data| data.sandbox.is_none())
}

pub(crate) fn create_global(handle: &DisplayHandle) {
  handle
    .create_global::<CompositorState, WpSecurityContextManagerV1, ()>(1, ());
}

impl GlobalDispatch<WpSecurityContextManagerV1, ()> for CompositorState {
  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpSecurityContextManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }

  fn can_view(client: Client, _: &()) -> bool {
    can_view_manager(client)
  }
}

impl Dispatch<WpSecurityContextManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    resource: &WpSecurityContextManagerV1,
    request: wp_security_context_manager_v1::Request,
    _data: &(),
    _handle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let wp_security_context_manager_v1::Request::CreateListener {
      id,
      listen_fd,
      close_fd,
    } = request
    else {
      return;
    };
    if !can_view_manager(client.clone()) {
      resource.post_error(
        wp_security_context_manager_v1::Error::Nested,
        "nested security contexts are forbidden",
      );
      return;
    }
    if rustix::net::getsockname(&listen_fd).is_err()
      || rustix::fs::fcntl_getfl(&close_fd).is_err()
    {
      resource.post_error(
        wp_security_context_manager_v1::Error::InvalidListenFd,
        "invalid security-context listener descriptor",
      );
      return;
    }
    let context = Arc::new(Mutex::new(ContextState::default()));
    data_init.init(id, context.clone());
    let id = state.security_context.next_listener;
    state.security_context.next_listener += 1;
    state.security_context.pending.push(PendingListener {
      id,
      listen_fd,
      close_fd,
      context,
    });
  }
}

impl Dispatch<WpSecurityContextV1, Arc<Mutex<ContextState>>>
  for CompositorState
{
  fn request(
    _state: &mut Self,
    _client: &Client,
    resource: &WpSecurityContextV1,
    request: wp_security_context_v1::Request,
    context: &Arc<Mutex<ContextState>>,
    _handle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    let Ok(mut context) = context.lock() else {
      return;
    };
    if context.committed
      && !matches!(request, wp_security_context_v1::Request::Destroy)
    {
      resource.post_error(
        wp_security_context_v1::Error::AlreadyUsed,
        "security context was already committed",
      );
      return;
    }
    match request {
      wp_security_context_v1::Request::SetSandboxEngine { name } => {
        set_metadata(resource, &mut context.metadata.engine, name);
      }
      wp_security_context_v1::Request::SetAppId { app_id } => {
        set_metadata(resource, &mut context.metadata.app_id, app_id);
      }
      wp_security_context_v1::Request::SetInstanceId { instance_id } => {
        set_metadata(resource, &mut context.metadata.instance_id, instance_id);
      }
      wp_security_context_v1::Request::Commit => {
        let metadata = &context.metadata;
        if [
          metadata.engine.as_deref(),
          metadata.app_id.as_deref(),
          metadata.instance_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(str::is_empty)
        {
          resource.post_error(
            wp_security_context_v1::Error::InvalidMetadata,
            "security-context metadata cannot be empty",
          );
          return;
        }
        context.committed = true;
      }
      _ => {}
    }
  }
}

fn set_metadata(
  resource: &WpSecurityContextV1,
  destination: &mut Option<String>,
  value: String,
) {
  if destination.is_some() {
    resource.post_error(
      wp_security_context_v1::Error::AlreadySet,
      "security-context metadata was already set",
    );
    return;
  }
  *destination = Some(value);
}
