//! `wp_linux_drm_syncobj_manager_v1` version 1: explicit synchronisation
//! of client DMA-BUFs through DRM timeline syncobjs
//! (docs/design/dmabuf.md, section 3).
//!
//! The global exists only alongside `zwp_linux_dmabuf_v1`, once a
//! frontend calls [`crate::Compositor::enable_syncobj`] and the render
//! node of the DMA-BUF feedback passes wlroots' check: timeline syncobjs
//! and `DRM_IOCTL_SYNCOBJ_EVENTFD`. Clients import timelines from fds and
//! set an acquire and a release point with every buffer they commit, by
//! the rules wlroots enforces (types/wlr_linux_drm_syncobj_v1.c).
//!
//! Acquire: a commit whose acquire point has not signalled waits in the
//! surface's commit queue on an eventfd the kernel signals with the
//! point, as smithay's `DrmSyncPoint::generate_blocker` does; implicit
//! fences are not waited for then. Release: the point is kept with the
//! buffer's lock and signalled when the buffer is released, that is,
//! when it was replaced and the GPU has finished reading it
//! (crate::buffer_lock). `wl_buffer.release` is sent as well.

mod timeline;

use std::{collections::HashMap, os::fd::AsFd, sync::Arc};

use calloop::LoopHandle;
use wayland_protocols::wp::linux_drm_syncobj::v1::server::{
  wp_linux_drm_syncobj_manager_v1::{self, WpLinuxDrmSyncobjManagerV1},
  wp_linux_drm_syncobj_surface_v1::{self, WpLinuxDrmSyncobjSurfaceV1},
  wp_linux_drm_syncobj_timeline_v1::{self, WpLinuxDrmSyncobjTimelineV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  backend::{GlobalId, ObjectId},
};

#[cfg(test)]
pub(crate) use timeline::Timeline;
pub(crate) use timeline::{RenderNode, SyncPoint, SyncobjDevice};
use timeline::{dev_major, dev_minor};

use crate::{
  buffer::BufferContent,
  commit_queue::{Blocker, BlockerSource, FdWait},
  state::{CompositorState, PendingBuffer, PendingSurface},
};

/// The version Shingle offers, the protocol's only one.
pub(crate) const VERSION: u32 = 1;

/// The points a surface's syncobj object set since its last commit.
struct SurfaceSync {
  resource: WpLinuxDrmSyncobjSurfaceV1,
  acquire: Option<SyncPoint>,
  release: Option<SyncPoint>,
}

/// The compositor's explicit-sync state, once a frontend enabled it.
pub(crate) struct SyncobjState {
  device: Box<dyn SyncobjDevice>,
  /// The syncobj objects, by their `wl_surface`.
  surfaces: HashMap<ObjectId, SurfaceSync>,
  _global: GlobalId,
}

/// A timeline object's data: the imported timeline, or None if the
/// import failed and the client was ended.
pub(crate) struct TimelineData(Option<Arc<dyn timeline::Timeline>>);

/// A surface object's data: its `wl_surface`.
pub(crate) struct SurfaceData(ObjectId);

/// Opens the render node `main_device` and checks that it supports
/// explicit sync, for [`enable`].
pub(crate) fn open_render_node(
  main_device: u64,
) -> Result<Arc<RenderNode>, String> {
  let node = RenderNode::open(main_device)?;
  node
    .check_syncobj()
    .map_err(|error| format!("{}: {error}", node.path().display()))?;
  Ok(Arc::new(node))
}

/// The log line of an offered global.
pub(crate) fn offered(main_device: u64) -> String {
  format!(
    "linux-drm-syncobj version {VERSION} on render node {}:{}",
    dev_major(main_device),
    dev_minor(main_device)
  )
}

/// Creates the global with `device`, or replaces the device for later
/// imports when it exists, as after a GPU loss. Timelines imported
/// earlier keep the device they came from.
pub(crate) fn enable(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  loop_handle: &LoopHandle<'static, CompositorState>,
  device: Box<dyn SyncobjDevice>,
) -> Result<(), String> {
  if state.dmabuf.is_none() {
    return Err("linux-dmabuf is not offered".into());
  }
  if let Some(existing) = &mut state.syncobj {
    existing.device = device;
    return Ok(());
  }
  let global = handle
    .create_global::<CompositorState, WpLinuxDrmSyncobjManagerV1, ()>(
      VERSION,
      (),
    );
  state.commit_queue.add_source(Box::new(ExplicitSync {
    handle: loop_handle.clone(),
  }));
  state.syncobj = Some(SyncobjState {
    device,
    surfaces: HashMap::new(),
    _global: global,
  });
  Ok(())
}

/// Forgets the syncobj object of a destroyed surface; its later requests
/// are `no_surface`.
pub(crate) fn surface_destroyed(
  state: &mut CompositorState,
  object: &ObjectId,
) {
  if let Some(syncobj) = &mut state.syncobj {
    syncobj.surfaces.remove(object);
  }
}

/// Signals a release point, when its buffer was released.
pub(crate) fn signal_release(point: &SyncPoint) {
  crate::trace::buffer(format_args!("syncobj release point={}", point.point));
  if let Err(error) = point.timeline.signal(point.point) {
    eprintln!("shingle: cannot signal a syncobj release point: {error}");
  }
}

/// What a commit has, for the commit rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitBuffer {
  /// No buffer attached in this commit, or a null one.
  None,
  /// A DMA-BUF.
  Dmabuf,
  /// Any other buffer.
  Other,
}

/// The commit rules of a syncobj surface, in wlroots' order
/// (wlr_linux_drm_syncobj_v1.c, `surface_synced_commit`): points need a
/// buffer, a buffer needs both points, points on one timeline must be in
/// order, and the buffer must be a DMA-BUF.
fn check_commit(
  buffer: CommitBuffer,
  acquire: Option<&SyncPoint>,
  release: Option<&SyncPoint>,
) -> Result<(), (wp_linux_drm_syncobj_surface_v1::Error, &'static str)> {
  use wp_linux_drm_syncobj_surface_v1::Error;
  if buffer == CommitBuffer::None {
    if acquire.is_some() {
      return Err((Error::NoBuffer, "acquire point set but no buffer"));
    }
    if release.is_some() {
      return Err((Error::NoBuffer, "release point set but no buffer"));
    }
    return Ok(());
  }
  let Some(acquire) = acquire else {
    return Err((Error::NoAcquirePoint, "buffer committed without acquire"));
  };
  let Some(release) = release else {
    return Err((Error::NoReleasePoint, "buffer committed without release"));
  };
  if acquire.same_timeline(release) && acquire.point >= release.point {
    return Err((
      Error::ConflictingPoints,
      "the acquire point is not below the release point on one timeline",
    ));
  }
  if buffer == CommitBuffer::Other {
    return Err((
      Error::UnsupportedBuffer,
      "only DMA-BUFs support explicit sync",
    ));
  }
  Ok(())
}

/// Applies the commit rules to the pending state of `object` at
/// `wl_surface.commit`, moves the acquire point into it and hands the
/// release point to the buffer's lock. Returns false when a rule was
/// broken, which ended the client.
pub(crate) fn commit(state: &mut CompositorState, object: &ObjectId) -> bool {
  let Some(syncobj) = &mut state.syncobj else {
    return true;
  };
  let Some(sync) = syncobj.surfaces.get_mut(object) else {
    return true;
  };
  let Some(surface) = state.surfaces.get_mut(object) else {
    return true;
  };
  let acquire = sync.acquire.take();
  let release = sync.release.take();
  let (buffer, lock) = match &surface.pending.buffer {
    PendingBuffer::Attach(attachment, lock) => (
      if matches!(attachment.content, BufferContent::Dmabuf(_)) {
        CommitBuffer::Dmabuf
      } else {
        CommitBuffer::Other
      },
      Some(lock),
    ),
    PendingBuffer::Unchanged | PendingBuffer::Detach => {
      (CommitBuffer::None, None)
    }
  };
  if let Err((error, message)) =
    check_commit(buffer, acquire.as_ref(), release.as_ref())
  {
    sync.resource.post_error(error, message);
    return false;
  }
  if let (Some(lock), Some(release)) = (lock, release) {
    lock.add_release_point(release);
  }
  surface.pending.acquire = acquire;
  true
}

impl GlobalDispatch<WpLinuxDrmSyncobjManagerV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!(
    "wp_linux_drm_syncobj_manager_v1"
  );

  fn bind(
    _state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<WpLinuxDrmSyncobjManagerV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    data_init.init(resource, ());
  }
}

impl Dispatch<WpLinuxDrmSyncobjManagerV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpLinuxDrmSyncobjManagerV1,
    request: wp_linux_drm_syncobj_manager_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    use wp_linux_drm_syncobj_manager_v1::{Error, Request};
    let Some(syncobj) = &mut state.syncobj else {
      return;
    };
    match request {
      Request::GetSurface { id, surface } => {
        let object = surface.id();
        let created = data_init.init(id, SurfaceData(object.clone()));
        if syncobj.surfaces.contains_key(&object) {
          resource.post_error(
            Error::SurfaceExists,
            "the surface has a syncobj object already",
          );
          return;
        }
        syncobj.surfaces.insert(
          object,
          SurfaceSync {
            resource: created,
            acquire: None,
            release: None,
          },
        );
      }
      Request::ImportTimeline { id, fd } => {
        match syncobj.device.import(fd.as_fd()) {
          Ok(timeline) => {
            data_init.init(id, TimelineData(Some(timeline)));
          }
          Err(error) => {
            data_init.init(id, TimelineData(None));
            resource.post_error(
              Error::InvalidTimeline,
              format!("the timeline cannot be imported: {error}"),
            );
          }
        }
      }
      Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<WpLinuxDrmSyncobjTimelineV1, TimelineData> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &WpLinuxDrmSyncobjTimelineV1,
    _request: wp_linux_drm_syncobj_timeline_v1::Request,
    _data: &TimelineData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    // Destroying a timeline keeps the points set on it.
  }
}

/// The point a `set_acquire_point` or `set_release_point` names.
fn point_of(
  timeline: &WpLinuxDrmSyncobjTimelineV1,
  point_hi: u32,
  point_lo: u32,
) -> Option<SyncPoint> {
  let timeline = timeline.data::<TimelineData>()?.0.clone()?;
  Some(SyncPoint {
    timeline,
    point: (u64::from(point_hi) << 32) | u64::from(point_lo),
  })
}

impl Dispatch<WpLinuxDrmSyncobjSurfaceV1, SurfaceData> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    resource: &WpLinuxDrmSyncobjSurfaceV1,
    request: wp_linux_drm_syncobj_surface_v1::Request,
    data: &SurfaceData,
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
    use wp_linux_drm_syncobj_surface_v1::{Error, Request};
    let Some(syncobj) = &mut state.syncobj else {
      return;
    };
    let sync = syncobj
      .surfaces
      .get_mut(&data.0)
      .filter(|sync| sync.resource == *resource);
    match request {
      Request::SetAcquirePoint {
        timeline,
        point_hi,
        point_lo,
      } => match sync {
        Some(sync) => sync.acquire = point_of(&timeline, point_hi, point_lo),
        None => {
          resource.post_error(Error::NoSurface, "the wl_surface is gone");
        }
      },
      Request::SetReleasePoint {
        timeline,
        point_hi,
        point_lo,
      } => match sync {
        Some(sync) => sync.release = point_of(&timeline, point_hi, point_lo),
        None => {
          resource.post_error(Error::NoSurface, "the wl_surface is gone");
        }
      },
      Request::Destroy => {
        // Points set since the last commit are discarded with the object.
        if sync.is_some() {
          syncobj.surfaces.remove(&data.0);
        }
      }
      _ => {}
    }
  }
}

/// Blocks commits whose acquire point has not signalled.
struct ExplicitSync {
  handle: LoopHandle<'static, CompositorState>,
}

impl BlockerSource for ExplicitSync {
  fn blockers(
    &self,
    surface: &ObjectId,
    pending: &PendingSurface,
  ) -> Result<Vec<Box<dyn Blocker>>, String> {
    let Some(acquire) = &pending.acquire else {
      return Ok(Vec::new());
    };
    let serial = match &pending.buffer {
      PendingBuffer::Attach(attachment, _) => match &attachment.content {
        BufferContent::Dmabuf(buffer) => buffer.serial,
        _ => 0,
      },
      _ => 0,
    };
    if acquire.timeline.signalled(acquire.point).unwrap_or(false) {
      crate::trace::buffer(format_args!(
        "syncobj acquire ready point={} dmabuf={serial}",
        acquire.point
      ));
      return Ok(Vec::new());
    }
    let eventfd = acquire
      .timeline
      .eventfd(acquire.point)
      .map_err(|error| format!("syncobj eventfd: {error}"))?;
    let check = acquire.clone();
    let point = acquire.point;
    let wait = FdWait::register(
      &self.handle,
      eventfd,
      surface,
      Box::new(move || check.timeline.signalled(check.point).unwrap_or(false)),
      Box::new(move |waited| {
        crate::trace::buffer(format_args!(
          "blocked syncobj {} µs point={point} dmabuf={serial}",
          waited.as_micros()
        ));
      }),
    )?;
    Ok(vec![Box::new(wait)])
  }
}

#[cfg(test)]
mod tests;
