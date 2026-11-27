//! A minimal in-process Wayland client for protocol tests.
//!
//! The client and the compositor run on the test thread. Requests are
//! flushed, the compositor dispatches them, and the client reads its
//! replies without blocking, until a `wl_display.sync` round trip ends.

use std::{
  os::{fd::OwnedFd, unix::net::UnixStream},
  path::Path,
};

use wayland_client::{
  Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
  backend::ObjectId,
  protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device,
    wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard,
    wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface, wl_touch,
  },
};
use wayland_protocols::{
  ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
  },
  ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
  },
  ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1, ext_image_copy_capture_frame_v1,
    ext_image_copy_capture_session_v1,
  },
  ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
  },
  ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
  },
  wp::{
    commit_timing::v1::client::{
      wp_commit_timer_v1, wp_commit_timing_manager_v1,
    },
    fifo::v1::client::{wp_fifo_manager_v1, wp_fifo_v1},
    fractional_scale::v1::client::{
      wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
      wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    input_method::zv1::client::{
      zwp_input_method_context_v1::{self, ZwpInputMethodContextV1},
      zwp_input_method_v1::{self, ZwpInputMethodV1},
    },
    linux_dmabuf::zv1::client::{
      zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
      zwp_linux_dmabuf_v1,
    },
    linux_drm_syncobj::v1::client::{
      wp_linux_drm_syncobj_manager_v1, wp_linux_drm_syncobj_surface_v1,
      wp_linux_drm_syncobj_timeline_v1,
    },
    presentation_time::client::{wp_presentation, wp_presentation_feedback},
    single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1,
    text_input::zv1::client::{
      zwp_text_input_manager_v1::ZwpTextInputManagerV1,
      zwp_text_input_v1::{self, ZwpTextInputV1},
    },
    viewporter::client::{wp_viewport, wp_viewporter},
  },
  xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
  },
  xdg::{
    shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
    xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1},
  },
  xwayland::shell::v1::client::{xwayland_shell_v1, xwayland_surface_v1},
};
use wayland_protocols_misc::{
  server_decoration::client::{
    org_kde_kwin_server_decoration::{self, OrgKdeKwinServerDecoration},
    org_kde_kwin_server_decoration_manager::{
      self, OrgKdeKwinServerDecorationManager,
    },
  },
  zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2,
    zwp_input_method_v2,
  },
  zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
  },
};
use wayland_protocols_wlr::{
  data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
  },
  foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
  },
  gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
  },
  layer_shell::v1::client::{
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
  },
  output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
  },
  output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
  },
  screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
  },
};

use crate::{
  Compositor,
  wayfire_shell::protocol::client::{
    zwf_hotspot_v2::{self, ZwfHotspotV2},
    zwf_output_v2::{self, ZwfOutputV2},
    zwf_shell_manager_v2::ZwfShellManagerV2,
    zwf_surface_v2::ZwfSurfaceV2,
  },
};

/// Everything the test client has received, in order per object type.
#[derive(Default)]
pub(crate) struct Events {
  pub(crate) globals: Vec<(u32, String, u32)>,
  /// Names of globals the compositor removed, which are also gone from
  /// `globals`.
  pub(crate) globals_removed: Vec<u32>,
  pub(crate) output: Vec<wl_output::Event>,
  pub(crate) xdg_output: Vec<zxdg_output_v1::Event>,
  pub(crate) heads: Vec<zwlr_output_head_v1::Event>,
  pub(crate) modes: Vec<zwlr_output_mode_v1::Event>,
  /// Output-management heads in announcement order, with whether each was
  /// finished.
  pub(crate) head_objects: Vec<(ZwlrOutputHeadV1, bool)>,
  /// Output-management modes in announcement order, with their size,
  /// refresh, preferred flag and whether each was finished.
  pub(crate) mode_objects: Vec<(ZwlrOutputModeV1, ModeSeen)>,
  /// The mode each head last named as current.
  pub(crate) current_modes: Vec<ObjectId>,
  /// Each head's name, and whether it last said it is enabled.
  pub(crate) head_names: std::collections::HashMap<ObjectId, String>,
  /// Each head's last `enabled` state.
  pub(crate) head_enabled: std::collections::HashMap<ObjectId, bool>,
  /// The serial of the last output-manager `done`.
  pub(crate) output_serial: u32,
  /// Outcomes of output configurations: `succeeded`, `failed` or
  /// `cancelled`.
  pub(crate) configuration_results: Vec<&'static str>,
  pub(crate) frame_done: Vec<u32>,
  pub(crate) feedback: Vec<wp_presentation_feedback::Event>,
  /// Keymaps received by `wl_keyboard` objects, as text.
  pub(crate) keymaps: Vec<String>,
  /// Keymaps received by input-method keyboard grabs, as text.
  pub(crate) grab_keymaps: Vec<String>,
  /// `xdg_toplevel.configure` sizes, with the toplevel they were sent to.
  pub(crate) configures: Vec<(ObjectId, i32, i32)>,
  /// Every `xdg_toplevel.configure`, in order, with its state list.
  pub(crate) configure_states: Vec<(ObjectId, i32, i32, Vec<u32>)>,
  /// Toplevels that received `xdg_toplevel.close`, in order.
  pub(crate) closes: Vec<ObjectId>,
  /// Every `wl_pointer` event, in order.
  pub(crate) pointer: Vec<wl_pointer::Event>,
  /// Every `wl_touch` event, in order.
  pub(crate) touch: Vec<wl_touch::Event>,
  /// wlr-data-control `selection` events, in order.
  pub(crate) control_selections: Vec<Option<ZwlrDataControlOfferV1>>,
  /// wlr-data-control `primary_selection` events, in order.
  pub(crate) control_primary: Vec<Option<ZwlrDataControlOfferV1>>,
  /// The MIME types each wlr-data-control or ext-data-control offer
  /// announced.
  pub(crate) offer_types: std::collections::HashMap<ObjectId, Vec<String>>,
  /// ext-data-control `selection` events, in order.
  pub(crate) ext_control_selections: Vec<Option<ExtDataControlOfferV1>>,
  /// ext-data-control `primary_selection` events, in order.
  pub(crate) ext_control_primary: Vec<Option<ExtDataControlOfferV1>>,
  /// wlr-data-control and ext-data-control sources' `send` events: MIME
  /// type and descriptor.
  pub(crate) control_sends: Vec<(String, OwnedFd)>,
  /// wlr-data-control and ext-data-control sources that were cancelled.
  pub(crate) control_cancelled: Vec<ObjectId>,
  /// Every wlr-screencopy frame event, in order.
  pub(crate) screencopy: Vec<zwlr_screencopy_frame_v1::Event>,
  /// Every ext-image-copy-capture session event, with its session.
  pub(crate) capture_sessions:
    Vec<(ObjectId, ext_image_copy_capture_session_v1::Event)>,
  /// Every ext-image-copy-capture frame event, with its frame.
  pub(crate) capture_frames:
    Vec<(ObjectId, ext_image_copy_capture_frame_v1::Event)>,
  /// Every ext-image-copy-capture cursor session event, in order.
  pub(crate) cursor_sessions:
    Vec<ext_image_copy_capture_cursor_session_v1::Event>,
  /// ext-foreign-toplevel-list handles with the app IDs they announced.
  pub(crate) ext_toplevels: Vec<(ExtForeignToplevelHandleV1, Option<String>)>,
  /// The `wl_buffer`s that received `release`, in order.
  pub(crate) buffer_releases: Vec<ObjectId>,
  /// `zwp_linux_dmabuf_v1` `format` and `modifier` events, as (format,
  /// modifier), with `None` for `format`.
  pub(crate) dmabuf_formats: Vec<(u32, Option<u64>)>,
  /// Every `zwp_linux_dmabuf_feedback_v1` event, with its object.
  pub(crate) dmabuf_feedback:
    Vec<(ObjectId, zwp_linux_dmabuf_feedback_v1::Event)>,
  /// `zwp_linux_buffer_params_v1` answers: the params and the created
  /// buffer, or `None` for `failed`.
  pub(crate) dmabuf_created: Vec<(ObjectId, Option<wl_buffer::WlBuffer>)>,
  /// Every `wl_surface` event, with its surface.
  pub(crate) surface: Vec<(ObjectId, wl_surface::Event)>,
  /// Layer-surface configures: surface, serial, width and height.
  pub(crate) layer_configures: Vec<(ObjectId, u32, u32, u32)>,
  /// Layer surfaces that received `closed`.
  pub(crate) layer_closed: Vec<ObjectId>,
  /// `ext_session_lock_v1` events, as `locked` and `finished`.
  pub(crate) lock: Vec<&'static str>,
  /// Lock-surface configures: lock surface, serial, width and height.
  pub(crate) lock_configures: Vec<(ObjectId, u32, u32, u32)>,
  /// `wp_fractional_scale_v1.preferred_scale`, with its object.
  pub(crate) preferred_scales: Vec<(ObjectId, u32)>,
  /// wlr-output-power events: the control and `on`, `off` or `failed`.
  pub(crate) power: Vec<(ObjectId, &'static str)>,
  /// wlr-gamma-control events: the control and `size` or `failed`.
  pub(crate) gamma: Vec<(ObjectId, &'static str)>,
  /// The `wl_output`s each wlr foreign-toplevel handle is on, by handle.
  pub(crate) toplevel_outputs:
    std::collections::HashMap<ObjectId, Vec<ObjectId>>,
  /// ext-workspace groups as announced, by object.
  pub(crate) workspace_groups: std::collections::HashMap<ObjectId, GroupSeen>,
  /// ext-workspace workspaces as announced, by object.
  pub(crate) workspaces:
    std::collections::HashMap<ObjectId, (ExtWorkspaceHandleV1, WorkspaceSeen)>,
  /// wayfire-shell events, in order, with their object: `enter_fullscreen`,
  /// `leave_fullscreen` and `toggle_menu` of outputs, `enter`, `leave`
  /// and `proximity N` of hotspots.
  pub(crate) wayfire_shell: Vec<(ObjectId, String)>,
  /// `zxdg_toplevel_decoration_v1.configure` modes, with their object.
  pub(crate) decoration_modes: Vec<(ObjectId, u32)>,
  /// `org_kde_kwin_server_decoration_manager.default_mode` and
  /// `org_kde_kwin_server_decoration.mode` events, in order: the manager's
  /// as `default N`, a decoration's as `mode N`.
  pub(crate) kde_decoration: Vec<String>,
  /// `wl_data_device` drag events, in order: `enter`, `leave`, `motion`
  /// and `drop`.
  pub(crate) drag: Vec<&'static str>,
  /// The offer of each `wl_data_device.enter`, in order.
  pub(crate) drag_offers: Vec<Option<wl_data_offer::WlDataOffer>>,
  /// `wl_data_source` events, in order: `target MIME` (or `target -`),
  /// `action BITS`, `send MIME`, `dnd_drop_performed`, `dnd_finished` and
  /// `cancelled`.
  pub(crate) data_source: Vec<String>,
  /// `wl_data_source.send` descriptors, in order.
  pub(crate) data_sends: Vec<OwnedFd>,
  /// Every `zwp_text_input_v1` event, with its object.
  pub(crate) text_input_v1: Vec<(ObjectId, zwp_text_input_v1::Event)>,
  /// Every `zwp_input_method_v1` event, in order.
  pub(crate) input_method_v1: Vec<zwp_input_method_v1::Event>,
  /// Every `zwp_input_method_context_v1` event, with its object.
  pub(crate) input_method_context:
    Vec<(ObjectId, zwp_input_method_context_v1::Event)>,
  /// Every `zwp_input_method_v2` event, in order.
  pub(crate) input_method_v2: Vec<zwp_input_method_v2::Event>,
  synced: bool,
}

impl Events {
  /// Returns the last size configured for `toplevel`.
  pub(crate) fn last_configure(
    &self,
    toplevel: &xdg_toplevel::XdgToplevel,
  ) -> Option<(i32, i32)> {
    self
      .configures
      .iter()
      .rev()
      .find(|(id, ..)| *id == toplevel.id())
      .map(|(_, width, height)| (*width, *height))
  }
}

/// What an ext-workspace group said about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GroupSeen {
  /// The `wl_output`s it is on now.
  pub(crate) outputs: Vec<ObjectId>,
  /// The workspaces in it now.
  pub(crate) workspaces: Vec<ObjectId>,
  pub(crate) removed: bool,
}

/// What an ext-workspace workspace said about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WorkspaceSeen {
  pub(crate) id: String,
  pub(crate) active: bool,
  pub(crate) removed: bool,
}

/// What an output-management mode said about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ModeSeen {
  pub(crate) size: (i32, i32),
  pub(crate) refresh: i32,
  pub(crate) preferred: bool,
  pub(crate) finished: bool,
}

/// An `xdg_toplevel` with its surface and `xdg_surface`.
pub(crate) struct Toplevel {
  pub(crate) surface: wl_surface::WlSurface,
  pub(crate) _xdg_surface: xdg_surface::XdgSurface,
  pub(crate) toplevel: xdg_toplevel::XdgToplevel,
}

/// Marks the `wl_callback` of a round trip.
pub(crate) struct Sync;

/// Marks a `wl_surface.frame` callback.
pub(crate) struct Frame;

/// A connected test client with its event queue.
pub(crate) struct TestClient {
  connection: Connection,
  queue: EventQueue<Events>,
  pub(crate) handle: QueueHandle<Events>,
  registry: wl_registry::WlRegistry,
  pub(crate) events: Events,
}

impl TestClient {
  /// Connects to the compositor's socket and reads its globals.
  pub(crate) fn connect(compositor: &mut Compositor, socket: &Path) -> Self {
    let stream =
      UnixStream::connect(socket).expect("the test socket accepts clients");
    Self::from_stream(compositor, stream)
  }

  /// Connects as the test Xwayland
  /// ([`Compositor::connect_test_xwayland`]) and reads its globals.
  pub(crate) fn connect_xwayland(compositor: &mut Compositor) -> Self {
    let stream = compositor.connect_test_xwayland();
    Self::from_stream(compositor, stream)
  }

  /// Uses an already connected `stream` and reads the globals.
  fn from_stream(compositor: &mut Compositor, stream: UnixStream) -> Self {
    let connection =
      Connection::from_socket(stream).expect("the client connects");
    let queue = connection.new_event_queue();
    let handle = queue.handle();
    let registry = connection.display().get_registry(&handle, ());
    let mut client = Self {
      connection,
      queue,
      handle,
      registry,
      events: Events::default(),
    };
    client.roundtrip(compositor);
    client
  }

  /// Exchanges messages until the compositor has handled every request
  /// sent so far and the client has read the replies.
  pub(crate) fn roundtrip(&mut self, compositor: &mut Compositor) {
    self.events.synced = false;
    self.connection.display().sync(&self.handle, Sync);
    for _ in 0..100 {
      self.queue.flush().expect("the client flushes");
      compositor
        .dispatch_pending()
        .expect("the compositor dispatches");
      if let Some(guard) = self.queue.prepare_read() {
        // WouldBlock only means that nothing has arrived yet.
        let _ = guard.read();
      }
      self
        .queue
        .dispatch_pending(&mut self.events)
        .expect("the client dispatches");
      if self.events.synced {
        return;
      }
    }
    panic!("the round trip did not finish");
  }

  /// Flushes, lets the compositor dispatch and reads, without requiring
  /// the round trip to finish, and returns the protocol error that ended
  /// the connection, if any.
  pub(crate) fn protocol_error(
    &mut self,
    compositor: &mut Compositor,
  ) -> Option<wayland_client::backend::protocol::ProtocolError> {
    for _ in 0..10 {
      let _ = self.queue.flush();
      let _ = compositor.dispatch_pending();
      if let Some(guard) = self.queue.prepare_read() {
        let _ = guard.read();
      }
      let _ = self.queue.dispatch_pending(&mut self.events);
      if let Some(error) = self.connection.protocol_error() {
        return Some(error);
      }
    }
    None
  }

  /// The errno that ended the connection, if it ended without a protocol
  /// error, as libwayland ends it on a `wl_display` error.
  pub(crate) fn connection_errno(&self) -> Option<rustix::io::Errno> {
    match self.connection.flush() {
      Err(wayland_client::backend::WaylandError::Io(error)) => error
        .raw_os_error()
        .map(rustix::io::Errno::from_raw_os_error),
      _ => None,
    }
  }

  /// Binds the first global of interface `I` at `version`.
  pub(crate) fn bind<I>(&self, version: u32) -> I
  where
    I: Proxy + 'static,
    Events: Dispatch<I, ()>,
  {
    let (name, _, _) = self
      .events
      .globals
      .iter()
      .find(|(_, interface, _)| interface == I::interface().name)
      .unwrap_or_else(|| panic!("no {} global", I::interface().name));
    self.registry.bind(*name, version, &self.handle, ())
  }

  /// Binds the global called `name`, which may already be removed, as
  /// interface `I` at `version`.
  pub(crate) fn bind_name<I>(&self, name: u32, version: u32) -> I
  where
    I: Proxy + 'static,
    Events: Dispatch<I, ()>,
  {
    self.registry.bind(name, version, &self.handle, ())
  }

  /// The name of the first global of interface `I`.
  pub(crate) fn global_name<I: Proxy>(&self) -> u32 {
    self
      .events
      .globals
      .iter()
      .find(|(_, interface, _)| interface == I::interface().name)
      .map(|(name, ..)| *name)
      .unwrap_or_else(|| panic!("no {} global", I::interface().name))
  }

  /// Creates a surface with an attached and committed 1x1 shm buffer and
  /// makes it the pointer's cursor, which keeps it visible without a shell.
  pub(crate) fn cursor_surface(
    &mut self,
    compositor: &mut Compositor,
  ) -> wl_surface::WlSurface {
    let wl_compositor = self.bind::<wl_compositor::WlCompositor>(4);
    let shm = self.bind::<wl_shm::WlShm>(1);
    let seat = self.bind::<wl_seat::WlSeat>(5);
    let pointer = seat.get_pointer(&self.handle, ());
    let fd = rustix::fs::memfd_create(
      "shingle-test-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, 4).expect("ftruncate succeeds");
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), 4, &self.handle, ());
    let buffer = pool.create_buffer(
      0,
      1,
      1,
      4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    );
    let surface = wl_compositor.create_surface(&self.handle, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    pointer.set_cursor(0, Some(&surface), 0, 0);
    self.roundtrip(compositor);
    surface
  }

  /// Attaches a new 1x1 ARGB8888 shm buffer to `surface` and commits it.
  pub(crate) fn commit_surface_again(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
  ) {
    let shm = self.bind::<wl_shm::WlShm>(1);
    let fd = rustix::fs::memfd_create(
      "shingle-test-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, 4).expect("ftruncate succeeds");
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), 4, &self.handle, ());
    let buffer = pool.create_buffer(
      0,
      1,
      1,
      4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    self.roundtrip(compositor);
  }

  /// Creates an `xdg_toplevel` with `app_id` and commits it without a
  /// buffer. The initial configure is acknowledged during the round trip,
  /// so a buffer can be attached next.
  pub(crate) fn toplevel(
    &mut self,
    compositor: &mut Compositor,
    app_id: &str,
  ) -> Toplevel {
    let wl_compositor = self.bind::<wl_compositor::WlCompositor>(4);
    let wm_base = self.bind::<xdg_wm_base::XdgWmBase>(1);
    let surface = wl_compositor.create_surface(&self.handle, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &self.handle, ());
    let toplevel = xdg_surface.get_toplevel(&self.handle, ());
    toplevel.set_app_id(app_id.into());
    surface.commit();
    self.roundtrip(compositor);
    Toplevel {
      surface,
      _xdg_surface: xdg_surface,
      toplevel,
    }
  }

  /// Creates a `width` by `height` ARGB8888 shm buffer in a pool of its
  /// own, without attaching it.
  pub(crate) fn shm_buffer(
    &self,
    width: i32,
    height: i32,
  ) -> wl_buffer::WlBuffer {
    let shm = self.bind::<wl_shm::WlShm>(1);
    let size = width * height * 4;
    let fd = rustix::fs::memfd_create(
      "shingle-test-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, size as u64).expect("ftruncate succeeds");
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), size, &self.handle, ());
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Argb8888,
      &self.handle,
      (),
    );
    pool.destroy();
    buffer
  }

  /// Creates a `width` by `height` XRGB8888 shm buffer, which is opaque,
  /// in a pool of its own, without attaching it.
  pub(crate) fn opaque_buffer(
    &self,
    width: i32,
    height: i32,
  ) -> wl_buffer::WlBuffer {
    let shm = self.bind::<wl_shm::WlShm>(1);
    let size = width * height * 4;
    let fd = rustix::fs::memfd_create(
      "shingle-test-buffer",
      rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create succeeds");
    rustix::fs::ftruncate(&fd, size as u64).expect("ftruncate succeeds");
    let pool =
      shm.create_pool(rustix::fd::AsFd::as_fd(&fd), size, &self.handle, ());
    let buffer = pool.create_buffer(
      0,
      width,
      height,
      width * 4,
      wl_shm::Format::Xrgb8888,
      &self.handle,
      (),
    );
    pool.destroy();
    buffer
  }

  /// The `wl_surface` events `surface` received, in order.
  pub(crate) fn surface_events(
    &self,
    surface: &wl_surface::WlSurface,
  ) -> Vec<&wl_surface::Event> {
    self
      .events
      .surface
      .iter()
      .filter(|(id, _)| *id == surface.id())
      .map(|(_, event)| event)
      .collect()
  }

  /// Attaches and commits a `width` by `height` shm buffer on `surface`.
  pub(crate) fn attach_buffer(
    &mut self,
    compositor: &mut Compositor,
    surface: &wl_surface::WlSurface,
    width: i32,
    height: i32,
  ) {
    let buffer = self.shm_buffer(width, height);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    self.roundtrip(compositor);
  }
}

/// Reads a keymap that a compositor shared as a file descriptor. `pread`
/// leaves the descriptor's offset, which the compositor shares, alone.
fn read_keymap(fd: &OwnedFd, size: u32) -> String {
  let mut bytes = vec![0; size as usize];
  let mut read = 0;
  while read < bytes.len() {
    let count = rustix::io::pread(fd, &mut bytes[read..], read as u64)
      .expect("the keymap is readable");
    if count == 0 {
      break;
    }
    read += count;
  }
  bytes.truncate(read);
  // The compositor may count a terminating null byte in `size`.
  while bytes.last() == Some(&0) {
    bytes.pop();
  }
  String::from_utf8(bytes).expect("the keymap is UTF-8")
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Events {
  fn event(
    events: &mut Self,
    buffer: &wl_buffer::WlBuffer,
    event: wl_buffer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_buffer::Event::Release = event {
      events.buffer_releases.push(buffer.id());
    }
  }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    event: zwp_linux_dmabuf_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwp_linux_dmabuf_v1::Event::Format { format } => {
        events.dmabuf_formats.push((format, None));
      }
      zwp_linux_dmabuf_v1::Event::Modifier {
        format,
        modifier_hi,
        modifier_lo,
      } => events.dmabuf_formats.push((
        format,
        Some((u64::from(modifier_hi) << 32) | u64::from(modifier_lo)),
      )),
      _ => {}
    }
  }
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()>
  for Events
{
  fn event(
    events: &mut Self,
    feedback: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
    event: zwp_linux_dmabuf_feedback_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.dmabuf_feedback.push((feedback.id(), event));
  }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()>
  for Events
{
  fn event(
    events: &mut Self,
    params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    event: zwp_linux_buffer_params_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwp_linux_buffer_params_v1::Event::Created { buffer } => {
        events.dmabuf_created.push((params.id(), Some(buffer)));
      }
      zwp_linux_buffer_params_v1::Event::Failed => {
        events.dmabuf_created.push((params.id(), None));
      }
      _ => {}
    }
  }

  wayland_client::event_created_child!(
    Events,
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    [
      zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE
        => (wl_buffer::WlBuffer, ()),
    ]
  );
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_keyboard::WlKeyboard,
    event: wl_keyboard::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_keyboard::Event::Keymap { fd, size, .. } = event {
      events.keymaps.push(read_keymap(&fd, size));
    }
  }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_pointer::WlPointer,
    event: wl_pointer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.pointer.push(event);
  }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for Events {
  fn event(
    _: &mut Self,
    _: &wl_data_device_manager::WlDataDeviceManager,
    _: wl_data_device_manager::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_data_device::WlDataDevice,
    event: wl_data_device::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wl_data_device::Event::Enter { id, .. } => {
        events.drag.push("enter");
        events.drag_offers.push(id);
      }
      wl_data_device::Event::Leave => events.drag.push("leave"),
      wl_data_device::Event::Motion { .. } => events.drag.push("motion"),
      wl_data_device::Event::Drop => events.drag.push("drop"),
      _ => {}
    }
  }

  wayland_client::event_created_child!(Events, wl_data_device::WlDataDevice, [
    wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
  ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for Events {
  fn event(
    events: &mut Self,
    offer: &wl_data_offer::WlDataOffer,
    event: wl_data_offer::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_data_offer::Event::Offer { mime_type } = event {
      events
        .offer_types
        .entry(offer.id())
        .or_default()
        .push(mime_type);
    }
  }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_data_source::WlDataSource,
    event: wl_data_source::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let line = match event {
      wl_data_source::Event::Target { mime_type } => {
        format!("target {}", mime_type.as_deref().unwrap_or("-"))
      }
      wl_data_source::Event::Action { dnd_action } => {
        let bits = match dnd_action {
          WEnum::Value(action) => action.bits(),
          WEnum::Unknown(bits) => bits,
        };
        format!("action {bits}")
      }
      wl_data_source::Event::Send { mime_type, fd } => {
        events.data_sends.push(fd);
        format!("send {mime_type}")
      }
      wl_data_source::Event::DndDropPerformed => "dnd_drop_performed".into(),
      wl_data_source::Event::DndFinished => "dnd_finished".into(),
      wl_data_source::Event::Cancelled => "cancelled".into(),
      _ => return,
    };
    events.data_source.push(line);
  }
}

impl Dispatch<wl_touch::WlTouch, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_touch::WlTouch,
    event: wl_touch::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.touch.push(event);
  }
}

impl
  Dispatch<zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2, ()>
  for Events
{
  fn event(
    events: &mut Self,
    _: &zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
    event: zwp_input_method_keyboard_grab_v2::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zwp_input_method_keyboard_grab_v2::Event::Keymap {
      fd, size, ..
    } = event
    {
      events.grab_keymaps.push(read_keymap(&fd, size));
    }
  }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Events {
  fn event(
    _: &mut Self,
    wm_base: &xdg_wm_base::XdgWmBase,
    event: xdg_wm_base::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_wm_base::Event::Ping { serial } = event {
      wm_base.pong(serial);
    }
  }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Events {
  fn event(
    _: &mut Self,
    xdg_surface: &xdg_surface::XdgSurface,
    event: xdg_surface::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let xdg_surface::Event::Configure { serial } = event {
      xdg_surface.ack_configure(serial);
    }
  }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Events {
  fn event(
    events: &mut Self,
    toplevel: &xdg_toplevel::XdgToplevel,
    event: xdg_toplevel::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      xdg_toplevel::Event::Configure {
        width,
        height,
        states,
      } => {
        events.configures.push((toplevel.id(), width, height));
        let states = states
          .as_chunks::<4>()
          .0
          .iter()
          .map(|bytes| u32::from_ne_bytes(*bytes))
          .collect();
        events
          .configure_states
          .push((toplevel.id(), width, height, states));
      }
      xdg_toplevel::Event::Close => events.closes.push(toplevel.id()),
      _ => {}
    }
  }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      wl_registry::Event::Global {
        name,
        interface,
        version,
      } => events.globals.push((name, interface, version)),
      wl_registry::Event::GlobalRemove { name } => {
        events.globals.retain(|(global, ..)| *global != name);
        events.globals_removed.push(name);
      }
      _ => {}
    }
  }
}

impl Dispatch<wl_callback::WlCallback, Sync> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    _: wl_callback::Event,
    _: &Sync,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.synced = true;
  }
}

impl Dispatch<wl_callback::WlCallback, Frame> for Events {
  fn event(
    events: &mut Self,
    _: &wl_callback::WlCallback,
    event: wl_callback::Event,
    _: &Frame,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_callback::Event::Done { callback_data } = event {
      events.frame_done.push(callback_data);
    }
  }
}

impl Dispatch<wl_output::WlOutput, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wl_output::WlOutput,
    event: wl_output::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.output.push(event);
  }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &zxdg_output_v1::ZxdgOutputV1,
    event: zxdg_output_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.xdg_output.push(event);
  }
}

impl Dispatch<ZwlrOutputHeadV1, ()> for Events {
  fn event(
    events: &mut Self,
    head: &ZwlrOutputHeadV1,
    event: zwlr_output_head_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match &event {
      zwlr_output_head_v1::Event::Mode { mode } => {
        events
          .mode_objects
          .push((mode.clone(), ModeSeen::default()));
      }
      zwlr_output_head_v1::Event::CurrentMode { mode } => {
        events.current_modes.push(mode.id());
      }
      zwlr_output_head_v1::Event::Name { name } => {
        events.head_names.insert(head.id(), name.clone());
      }
      zwlr_output_head_v1::Event::Enabled { enabled } => {
        events.head_enabled.insert(head.id(), *enabled != 0);
      }
      zwlr_output_head_v1::Event::Finished => {
        for (known, finished) in &mut events.head_objects {
          if known.id() == head.id() {
            *finished = true;
          }
        }
      }
      _ => {}
    }
    events.heads.push(event);
  }

  wayland_client::event_created_child!(Events, ZwlrOutputHeadV1, [
    zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
  ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for Events {
  fn event(
    events: &mut Self,
    mode: &ZwlrOutputModeV1,
    event: zwlr_output_mode_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let Some((_, seen)) = events
      .mode_objects
      .iter_mut()
      .find(|(known, _)| known.id() == mode.id())
    {
      match &event {
        zwlr_output_mode_v1::Event::Size { width, height } => {
          seen.size = (*width, *height);
        }
        zwlr_output_mode_v1::Event::Refresh { refresh } => {
          seen.refresh = *refresh;
        }
        zwlr_output_mode_v1::Event::Preferred => seen.preferred = true,
        zwlr_output_mode_v1::Event::Finished => seen.finished = true,
        _ => {}
      }
    }
    events.modes.push(event);
  }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrOutputManagerV1,
    event: zwlr_output_manager_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_output_manager_v1::Event::Head { head } => {
        events.head_objects.push((head, false));
      }
      zwlr_output_manager_v1::Event::Done { serial } => {
        events.output_serial = serial;
      }
      _ => {}
    }
  }

  wayland_client::event_created_child!(Events, ZwlrOutputManagerV1, [
    zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
  ]);
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrOutputConfigurationV1,
    event: zwlr_output_configuration_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.configuration_results.push(match event {
      zwlr_output_configuration_v1::Event::Succeeded => "succeeded",
      zwlr_output_configuration_v1::Event::Failed => "failed",
      _ => "cancelled",
    });
  }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for Events {
  fn event(
    _: &mut Self,
    _: &ZwlrOutputConfigurationHeadV1,
    _: <ZwlrOutputConfigurationHeadV1 as Proxy>::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for Events {
  fn event(
    events: &mut Self,
    _: &wp_presentation_feedback::WpPresentationFeedback,
    event: wp_presentation_feedback::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.feedback.push(event);
  }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrDataControlDeviceV1,
    event: zwlr_data_control_device_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_device_v1::Event::Selection { id } => {
        events.control_selections.push(id);
      }
      zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
        events.control_primary.push(id);
      }
      _ => {}
    }
  }

  wayland_client::event_created_child!(Events, ZwlrDataControlDeviceV1, [
    zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE =>
      (ZwlrDataControlOfferV1, ()),
  ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for Events {
  fn event(
    events: &mut Self,
    offer: &ZwlrDataControlOfferV1,
    event: zwlr_data_control_offer_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
      events
        .offer_types
        .entry(offer.id())
        .or_default()
        .push(mime_type);
    }
  }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for Events {
  fn event(
    events: &mut Self,
    source: &ZwlrDataControlSourceV1,
    event: zwlr_data_control_source_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
        events.control_sends.push((mime_type, fd));
      }
      zwlr_data_control_source_v1::Event::Cancelled => {
        events.control_cancelled.push(source.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ExtDataControlDeviceV1,
    event: ext_data_control_device_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      ext_data_control_device_v1::Event::Selection { id } => {
        events.ext_control_selections.push(id);
      }
      ext_data_control_device_v1::Event::PrimarySelection { id } => {
        events.ext_control_primary.push(id);
      }
      _ => {}
    }
  }

  wayland_client::event_created_child!(Events, ExtDataControlDeviceV1, [
    ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE =>
      (ExtDataControlOfferV1, ()),
  ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for Events {
  fn event(
    events: &mut Self,
    offer: &ExtDataControlOfferV1,
    event: ext_data_control_offer_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
      events
        .offer_types
        .entry(offer.id())
        .or_default()
        .push(mime_type);
    }
  }
}

impl Dispatch<ExtDataControlSourceV1, ()> for Events {
  fn event(
    events: &mut Self,
    source: &ExtDataControlSourceV1,
    event: ext_data_control_source_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      ext_data_control_source_v1::Event::Send { mime_type, fd } => {
        events.control_sends.push((mime_type, fd));
      }
      ext_data_control_source_v1::Event::Cancelled => {
        events.control_cancelled.push(source.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ExtForeignToplevelListV1,
    event: ext_foreign_toplevel_list_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
      events.ext_toplevels.push((toplevel, None));
    }
  }

  wayland_client::event_created_child!(Events, ExtForeignToplevelListV1, [
    ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE =>
      (ExtForeignToplevelHandleV1, ()),
  ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Events {
  fn event(
    events: &mut Self,
    handle: &ExtForeignToplevelHandleV1,
    event: ext_foreign_toplevel_handle_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let ext_foreign_toplevel_handle_v1::Event::AppId { app_id } = event
      && let Some((_, known)) = events
        .ext_toplevels
        .iter_mut()
        .find(|(known, _)| known == handle)
    {
      *known = Some(app_id);
    }
  }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrScreencopyFrameV1,
    event: zwlr_screencopy_frame_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.screencopy.push(event);
  }
}

impl Dispatch<ZwfOutputV2, ()> for Events {
  fn event(
    events: &mut Self,
    output: &ZwfOutputV2,
    event: zwf_output_v2::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let name = match event {
      zwf_output_v2::Event::EnterFullscreen => "enter_fullscreen",
      zwf_output_v2::Event::LeaveFullscreen => "leave_fullscreen",
      zwf_output_v2::Event::ToggleMenu => "toggle_menu",
    };
    events.wayfire_shell.push((output.id(), name.to_owned()));
  }
}

impl Dispatch<ZwfHotspotV2, ()> for Events {
  fn event(
    events: &mut Self,
    hotspot: &ZwfHotspotV2,
    event: zwf_hotspot_v2::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let name = match event {
      zwf_hotspot_v2::Event::Enter => "enter".to_owned(),
      zwf_hotspot_v2::Event::Leave => "leave".to_owned(),
      zwf_hotspot_v2::Event::ProximityChanged { proximity } => {
        format!("proximity {proximity}")
      }
    };
    events.wayfire_shell.push((hotspot.id(), name));
  }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for Events {
  fn event(
    events: &mut Self,
    decoration: &ZxdgToplevelDecorationV1,
    event: zxdg_toplevel_decoration_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
      events.decoration_modes.push((decoration.id(), mode.into()));
    }
  }
}

impl Dispatch<OrgKdeKwinServerDecorationManager, ()> for Events {
  fn event(
    events: &mut Self,
    _: &OrgKdeKwinServerDecorationManager,
    event: org_kde_kwin_server_decoration_manager::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let org_kde_kwin_server_decoration_manager::Event::DefaultMode { mode } =
      event
    {
      events
        .kde_decoration
        .push(format!("default {}", u32::from(mode)));
    }
  }
}

impl Dispatch<OrgKdeKwinServerDecoration, ()> for Events {
  fn event(
    events: &mut Self,
    _: &OrgKdeKwinServerDecoration,
    event: org_kde_kwin_server_decoration::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let org_kde_kwin_server_decoration::Event::Mode { mode } = event {
      events
        .kde_decoration
        .push(format!("mode {}", u32::from(mode)));
    }
  }
}

/// Implements `Dispatch` for objects whose events the tests ignore.
macro_rules! ignore_events {
  ($($interface:ty),* $(,)?) => {
    $(
      impl Dispatch<$interface, ()> for Events {
        fn event(
          _: &mut Self,
          _: &$interface,
          _: <$interface as Proxy>::Event,
          _: &(),
          _: &Connection,
          _: &QueueHandle<Self>,
        ) {
        }
      }
    )*
  };
}

impl Dispatch<wl_surface::WlSurface, ()> for Events {
  fn event(
    events: &mut Self,
    surface: &wl_surface::WlSurface,
    event: wl_surface::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.surface.push((surface.id(), event));
  }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Events {
  fn event(
    events: &mut Self,
    layer: &ZwlrLayerSurfaceV1,
    event: zwlr_layer_surface_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_layer_surface_v1::Event::Configure {
        serial,
        width,
        height,
      } => events
        .layer_configures
        .push((layer.id(), serial, width, height)),
      zwlr_layer_surface_v1::Event::Closed => {
        events.layer_closed.push(layer.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtSessionLockV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ExtSessionLockV1,
    event: ext_session_lock_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      ext_session_lock_v1::Event::Locked => events.lock.push("locked"),
      ext_session_lock_v1::Event::Finished => events.lock.push("finished"),
      _ => {}
    }
  }
}

impl Dispatch<ExtSessionLockSurfaceV1, ()> for Events {
  fn event(
    events: &mut Self,
    lock_surface: &ExtSessionLockSurfaceV1,
    event: ext_session_lock_surface_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let ext_session_lock_surface_v1::Event::Configure {
      serial,
      width,
      height,
    } = event
    {
      events
        .lock_configures
        .push((lock_surface.id(), serial, width, height));
    }
  }
}

impl Dispatch<WpFractionalScaleV1, ()> for Events {
  fn event(
    events: &mut Self,
    scale: &WpFractionalScaleV1,
    event: wp_fractional_scale_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wp_fractional_scale_v1::Event::PreferredScale { scale: value } =
      event
    {
      events.preferred_scales.push((scale.id(), value));
    }
  }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for Events {
  fn event(
    events: &mut Self,
    control: &ZwlrOutputPowerV1,
    event: zwlr_output_power_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let seen = match event {
      zwlr_output_power_v1::Event::Mode {
        mode: WEnum::Value(zwlr_output_power_v1::Mode::On),
      } => "on",
      zwlr_output_power_v1::Event::Mode { .. } => "off",
      zwlr_output_power_v1::Event::Failed => "failed",
      _ => return,
    };
    events.power.push((control.id(), seen));
  }
}

impl Dispatch<ZwlrGammaControlV1, ()> for Events {
  fn event(
    events: &mut Self,
    control: &ZwlrGammaControlV1,
    event: zwlr_gamma_control_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let seen = match event {
      zwlr_gamma_control_v1::Event::GammaSize { .. } => "size",
      zwlr_gamma_control_v1::Event::Failed => "failed",
      _ => return,
    };
    events.gamma.push((control.id(), seen));
  }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwlrForeignToplevelManagerV1,
    event: zwlr_foreign_toplevel_manager_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } =
      event
    {
      events.toplevel_outputs.insert(toplevel.id(), Vec::new());
    }
  }

  wayland_client::event_created_child!(Events, ZwlrForeignToplevelManagerV1, [
    zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE
      => (ZwlrForeignToplevelHandleV1, ()),
  ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Events {
  fn event(
    events: &mut Self,
    handle: &ZwlrForeignToplevelHandleV1,
    event: zwlr_foreign_toplevel_handle_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let Some(outputs) = events.toplevel_outputs.get_mut(&handle.id()) else {
      return;
    };
    match event {
      zwlr_foreign_toplevel_handle_v1::Event::OutputEnter { output } => {
        outputs.push(output.id());
      }
      zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output } => {
        outputs.retain(|id| *id != output.id());
      }
      _ => {}
    }
  }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ExtWorkspaceManagerV1,
    event: ext_workspace_manager_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
        events
          .workspace_groups
          .insert(workspace_group.id(), GroupSeen::default());
      }
      ext_workspace_manager_v1::Event::Workspace { workspace } => {
        events
          .workspaces
          .insert(workspace.id(), (workspace, WorkspaceSeen::default()));
      }
      _ => {}
    }
  }

  wayland_client::event_created_child!(Events, ExtWorkspaceManagerV1, [
    ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE
      => (ExtWorkspaceGroupHandleV1, ()),
    ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE
      => (ExtWorkspaceHandleV1, ()),
  ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Events {
  fn event(
    events: &mut Self,
    group: &ExtWorkspaceGroupHandleV1,
    event: ext_workspace_group_handle_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let Some(seen) = events.workspace_groups.get_mut(&group.id()) else {
      return;
    };
    match event {
      ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
        seen.outputs.push(output.id());
      }
      ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
        seen.outputs.retain(|id| *id != output.id());
      }
      ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
        seen.workspaces.push(workspace.id());
      }
      ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
        seen.workspaces.retain(|id| *id != workspace.id());
      }
      ext_workspace_group_handle_v1::Event::Removed => seen.removed = true,
      _ => {}
    }
  }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for Events {
  fn event(
    events: &mut Self,
    workspace: &ExtWorkspaceHandleV1,
    event: ext_workspace_handle_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let Some((_, seen)) = events.workspaces.get_mut(&workspace.id()) else {
      return;
    };
    match event {
      ext_workspace_handle_v1::Event::Id { id } => seen.id = id,
      ext_workspace_handle_v1::Event::State {
        state: WEnum::Value(state),
      } => {
        seen.active = state.contains(ext_workspace_handle_v1::State::Active);
      }
      ext_workspace_handle_v1::Event::Removed => seen.removed = true,
      _ => {}
    }
  }
}

ignore_events!(
  ExtDataControlManagerV1,
  ZxdgDecorationManagerV1,
  ZwfShellManagerV2,
  ZwfSurfaceV2,
  ZwlrOutputPowerManagerV1,
  ZwlrGammaControlManagerV1,
  wl_compositor::WlCompositor,
  wl_region::WlRegion,
  ZwlrLayerShellV1,
  ExtSessionLockManagerV1,
  WpFractionalScaleManagerV1,
  wl_shm::WlShm,
  wl_shm_pool::WlShmPool,
  wl_seat::WlSeat,
  wl_subcompositor::WlSubcompositor,
  wl_subsurface::WlSubsurface,
  zxdg_output_manager_v1::ZxdgOutputManagerV1,
  wp_presentation::WpPresentation,
  wp_viewporter::WpViewporter,
  wp_viewport::WpViewport,
  zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
  zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
  zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
  ZwpTextInputManagerV1,
  wp_fifo_manager_v1::WpFifoManagerV1,
  wp_fifo_v1::WpFifoV1,
  wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
  wp_commit_timing_manager_v1::WpCommitTimingManagerV1,
  wp_commit_timer_v1::WpCommitTimerV1,
  wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
  wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
  wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
  xwayland_shell_v1::XwaylandShellV1,
  xwayland_surface_v1::XwaylandSurfaceV1,
  ZwlrDataControlManagerV1,
  ZwlrScreencopyManagerV1,
);

impl Dispatch<ZwpTextInputV1, ()> for Events {
  fn event(
    events: &mut Self,
    input: &ZwpTextInputV1,
    event: zwp_text_input_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.text_input_v1.push((input.id(), event));
  }
}

impl Dispatch<ZwpInputMethodV1, ()> for Events {
  fn event(
    events: &mut Self,
    _: &ZwpInputMethodV1,
    event: zwp_input_method_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.input_method_v1.push(event);
  }

  wayland_client::event_created_child!(Events, ZwpInputMethodV1, [
    zwp_input_method_v1::EVT_ACTIVATE_OPCODE
      => (ZwpInputMethodContextV1, ()),
  ]);
}

impl Dispatch<ZwpInputMethodContextV1, ()> for Events {
  fn event(
    events: &mut Self,
    context: &ZwpInputMethodContextV1,
    event: zwp_input_method_context_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.input_method_context.push((context.id(), event));
  }
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for Events {
  fn event(
    events: &mut Self,
    _: &zwp_input_method_v2::ZwpInputMethodV2,
    event: zwp_input_method_v2::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    events.input_method_v2.push(event);
  }
}
