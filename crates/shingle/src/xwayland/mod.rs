//! XWayland glue: binds the X display, starts and supervises Xwayland
//! through `shingle-xwayland`, adds its Wayland connection as a client and
//! connects the X window manager when it is ready.
//!
//! The descriptors to watch (display sockets in lazy mode, the pidfd, the
//! readiness pipe and the window manager connection) are calloop sources
//! whose callbacks only set flags in [`Status`]; [`Xwayland::poll`] acts on
//! them once per [`crate::Compositor::poll`]. Each source is removed before
//! its descriptor closes, so a reused descriptor number is never watched
//! by mistake. See docs/design/xwayland.md, sections 1 and 2.
//!
//! [`shell`] is the `xwayland_shell_v1` protocol and [`windows`] turns
//! managed X windows into Shingle toplevels (sections 3 and 4).
//! [`selection`] bridges the clipboard and primary selection, and [`dnd`]
//! drag and drop (section 5).

#[cfg(test)]
mod behaviour_tests;
pub(crate) mod dnd;
#[cfg(test)]
pub(crate) mod fixture;
pub(crate) mod focus;
pub(crate) mod requests;
pub(crate) mod selection;
pub(crate) mod shell;
pub(crate) mod unmanaged;
pub(crate) mod windows;

use std::{
  collections::HashMap,
  ffi::OsString,
  os::fd::{AsRawFd, BorrowedFd, OwnedFd},
  path::PathBuf,
  sync::Arc,
  time::Duration,
};

use calloop::{
  Interest, LoopHandle, Mode, PostAction, RegistrationToken,
  generic::{FdWrapper, Generic},
};
use serde_json::json;
use shingle_config::XwaylandMode;
use shingle_xwayland::{
  DisplaySockets, Pairing, Readiness, Restart, Selection, StartMode, Window,
  X11Connection, XwaylandServer, Xwm, server,
};
use wayland_server::DisplayHandle;

use crate::state::{ClientDataImpl, CompositorState, SurfaceId};
use windows::{XRequest, XWindowState};

/// How long Xwayland gets to exit after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(1);

/// XWayland settings of [`crate::Config`].
#[derive(Clone, Debug)]
pub struct XwaylandSettings {
  /// When Xwayland starts, or [`XwaylandMode::Disabled`] for no X display
  /// at all. Read only at startup.
  pub mode: XwaylandMode,
  /// Shell command run in the client environment each time Xwayland is
  /// ready; empty runs nothing.
  pub startup_script: String,
  /// Sends the startup script's stdout and stderr to `/dev/null`.
  pub discard_script_output: bool,
  /// The directory holding the X lock files and `.X11-unix`. It is `/tmp`,
  /// where X clients look; tests use a private directory. Read only at
  /// startup.
  pub socket_base: PathBuf,
  /// The Xwayland binary. `None` uses `SHINGLE_XWAYLAND` when set, else
  /// `Xwayland` from `PATH`. Read only at startup.
  pub program: Option<OsString>,
}

impl Default for XwaylandSettings {
  /// Disabled: a compositor built from [`crate::Config::default`], as in
  /// tests and embedders, binds no X display unless asked to. The
  /// configuration file's default, `lazy`, comes in through
  /// `From<ShingleConfig>`.
  fn default() -> Self {
    Self {
      mode: XwaylandMode::Disabled,
      startup_script: String::new(),
      discard_script_output: true,
      socket_base: PathBuf::from("/tmp"),
      program: None,
    }
  }
}

/// What IPC and the client environment see of XWayland, and the flags the
/// event sources set.
#[derive(Debug, Default)]
pub(crate) struct Status {
  /// The `DISPLAY` value while the display is bound.
  pub(crate) display: Option<String>,
  /// The server's PID while it runs.
  pub(crate) pid: Option<u32>,
  /// A display socket became readable while the server waits for one.
  pub(crate) connection_pending: bool,
  /// The readiness pipe became readable.
  pub(crate) readiness_pending: bool,
  /// The pidfd became readable: the server exited.
  pub(crate) exit_pending: bool,
  /// The window manager connection became readable.
  pub(crate) xwm_pending: bool,
  /// X windows and surfaces waiting for their partner.
  pub(crate) pairing: Pairing<SurfaceId>,
  /// Every X window the window manager reported, mapped or not.
  pub(crate) windows: HashMap<Window, XWindowState>,
  /// Requests for the window manager's next flush.
  pub(crate) requests: Vec<XRequest>,
  /// The last map sequence number given to an unmanaged window.
  pub(crate) sequence: u64,
  /// Wayland clients' requests for an X client's selection, as MIME type
  /// and descriptor, for the window manager's next flush.
  pub(crate) selection_receives: Vec<(Selection, String, OwnedFd)>,
}

impl Status {
  /// Wayfire's `stipc/get_xwayland_pid` value: the PID, or -1.
  pub(crate) fn pid_or_minus_one(&self) -> i64 {
    self.pid.map_or(-1, i64::from)
  }
}

/// The source behind a flag in [`Status`].
#[derive(Clone, Copy)]
enum Source {
  Connection,
  Readiness,
  Exit,
  Xwm,
}

/// The bound display, the server and the window manager.
pub(crate) struct Xwayland {
  // Field order matters on drop, but `shut_down` runs first anyway.
  xwm: Option<Xwm<X11Connection>>,
  server: XwaylandServer,
  startup_script: String,
  discard_script_output: bool,
  listen_sources: Vec<RegistrationToken>,
  exit_source: Option<RegistrationToken>,
  readiness_source: Option<RegistrationToken>,
  xwm_source: Option<RegistrationToken>,
  /// The selection bridge's glue for the current window manager.
  bridge: selection::Bridge,
  /// The drag and drop bridge's glue for the current window manager.
  drag: dnd::DragBridge,
}

/// The start mode of an enabled [`XwaylandMode`].
fn start_mode(mode: XwaylandMode) -> Option<StartMode> {
  match mode {
    XwaylandMode::Eager => Some(StartMode::Eager),
    XwaylandMode::Lazy => Some(StartMode::Lazy),
    XwaylandMode::Disabled => None,
  }
}

impl Xwayland {
  /// Binds the X display for `settings`, unless XWayland is disabled. A
  /// display that cannot be bound is logged, and Shingle runs without X.
  pub(crate) fn bind(settings: &XwaylandSettings) -> Option<Self> {
    let mode = start_mode(settings.mode)?;
    let sockets = match DisplaySockets::bind_in(&settings.socket_base) {
      Ok(sockets) => sockets,
      Err(error) => {
        eprintln!("shingle: cannot bind an X display, no XWayland: {error}");
        return None;
      }
    };
    let program = server::program(
      settings
        .program
        .clone()
        .or_else(|| std::env::var_os(server::PROGRAM_VARIABLE)),
    );
    Some(Self {
      xwm: None,
      server: XwaylandServer::new(sockets, mode, program),
      startup_script: settings.startup_script.clone(),
      discard_script_output: settings.discard_script_output,
      listen_sources: Vec::new(),
      exit_source: None,
      readiness_source: None,
      xwm_source: None,
      bridge: selection::Bridge::default(),
      drag: dnd::DragBridge::default(),
    })
  }

  /// Publishes `DISPLAY`, then starts the server in eager mode or watches
  /// the display sockets in lazy mode.
  pub(crate) fn start_up(
    &mut self,
    state: &mut CompositorState,
    display: &DisplayHandle,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    state.xwayland.display = Some(self.server.display_name());
    eprintln!(
      "shingle: X display {} ({})",
      self.server.display_name(),
      match self.server.mode() {
        StartMode::Eager => "Xwayland starts now",
        StartMode::Lazy => "Xwayland starts on the first X11 connection",
      }
    );
    if self.server.waiting_for_connection() {
      self.watch_sockets(handle);
    } else {
      self.start(state, display, handle);
    }
  }

  /// Replaces the startup script, which takes effect the next time the
  /// server becomes ready.
  pub(crate) fn set_startup_script(&mut self, script: String, discard: bool) {
    self.startup_script = script;
    self.discard_script_output = discard;
  }

  /// The start mode this display runs in.
  pub(crate) fn mode(&self) -> StartMode {
    self.server.mode()
  }

  /// Acts on whatever the event sources flagged during this turn.
  pub(crate) fn poll(
    &mut self,
    state: &mut CompositorState,
    display: &DisplayHandle,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    if std::mem::take(&mut state.xwayland.connection_pending)
      && self.server.waiting_for_connection()
    {
      self.unwatch_sockets(handle);
      self.start(state, display, handle);
    }
    if std::mem::take(&mut state.xwayland.readiness_pending) {
      self.read_readiness(state, handle);
    }
    // x11rb may have buffered events while waiting for a reply, which
    // leaves the socket unreadable, so read them every turn.
    state.xwayland.xwm_pending = false;
    self.dispatch_xwm(state, handle);
    self.pump_selections(state, handle);
    if std::mem::take(&mut state.xwayland.exit_pending) {
      self.reap(state, display, handle);
    }
  }

  /// Hands Shingle's queued requests to the window manager and sends
  /// them.
  pub(crate) fn flush(
    &mut self,
    state: &mut CompositorState,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    let requests = std::mem::take(&mut state.xwayland.requests);
    let Some(xwm) = &mut self.xwm else {
      return;
    };
    let focus = focus::target(state);
    let stacking = focus::stacking(state);
    let mut result = Ok(());
    for request in requests {
      result = match request {
        XRequest::Configure {
          window,
          geometry,
          force,
        } => xwm.configure(window, geometry, force),
        XRequest::State { window, state } => xwm.set_state(window, state),
        XRequest::Close(window) => xwm.close(window),
        XRequest::OfferFocus(window) => xwm.offer_focus(window),
      };
      if result.is_err() {
        break;
      }
    }
    let result = result
      .and_then(|()| self.drag.sync(state, xwm))
      .and_then(|()| self.bridge.sync(state, xwm))
      .and_then(|()| match focus {
        focus::Target::Window(window) => xwm.set_focus(window),
        focus::Target::Keep => Ok(()),
      })
      .and_then(|()| xwm.restack(&stacking));
    if let Err(error) = result.and_then(|()| xwm.flush()) {
      eprintln!("shingle: X window manager lost its connection: {error}");
      self.drop_xwm(state, handle);
      return;
    }
    self.bridge.watch(xwm, handle);
  }

  /// Passes X clients' requests for Wayland selections to their sources
  /// and moves data on every selection transfer.
  fn pump_selections(
    &mut self,
    state: &mut CompositorState,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    let Some(xwm) = &mut self.xwm else {
      return;
    };
    for send in xwm.take_selection_sends() {
      selection::send(state, send);
    }
    if let Err(error) = xwm.pump_transfers().and_then(|()| xwm.flush()) {
      eprintln!("shingle: X window manager lost its connection: {error}");
      self.drop_xwm(state, handle);
    }
  }

  /// Stops the window manager and the server and removes every source.
  /// The display sockets and lock are removed when `self` is dropped.
  pub(crate) fn shut_down(
    &mut self,
    state: &mut CompositorState,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    self.drop_xwm(state, handle);
    self.unwatch_sockets(handle);
    remove(handle, &mut self.readiness_source);
    remove(handle, &mut self.exit_source);
    self.server.stop(STOP_GRACE);
    state.xwayland.pid = None;
  }

  /// Starts the server and adds its Wayland connection as a client.
  fn start(
    &mut self,
    state: &mut CompositorState,
    display: &DisplayHandle,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    let wayland = match self.server.start() {
      Ok(wayland) => wayland,
      Err(error) => {
        eprintln!("shingle: cannot start Xwayland: {error}");
        return;
      }
    };
    let client = display.clone().insert_client(
      wayland,
      Arc::new(ClientDataImpl {
        disconnected: state.disconnected.clone(),
        sandbox: None,
        privileged_protocols: state
          .security_context
          .privileged_protocols
          .clone(),
        xwayland: true,
      }),
    );
    if let Err(error) = client {
      eprintln!("shingle: cannot add Xwayland as a Wayland client: {error}");
      self.server.stop(STOP_GRACE);
      return;
    }
    state.dispatch_pending = true;
    let Some(process) = self.server.process() else {
      return;
    };
    let pid = process.pid();
    state.xwayland.pid = Some(pid);
    eprintln!(
      "shingle: started Xwayland (pid {pid}) on {}",
      self.server.display_name()
    );
    self.exit_source = watch(handle, process.pidfd(), Source::Exit);
    self.readiness_source = process
      .displayfd()
      .and_then(|fd| watch(handle, fd, Source::Readiness));
  }

  /// Reads the readiness pipe and, once the server is ready, connects the
  /// window manager, runs the startup script and announces
  /// `xwayland-ready`.
  fn read_readiness(
    &mut self,
    state: &mut CompositorState,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    // The pipe may close while being read; stop watching it first.
    remove(handle, &mut self.readiness_source);
    let Some(process) = self.server.process_mut() else {
      return;
    };
    match process.read_readiness() {
      Ok(Readiness::Pending) => {
        self.readiness_source = process
          .displayfd()
          .and_then(|fd| watch(handle, fd, Source::Readiness));
      }
      Ok(Readiness::Closed) => {
        eprintln!("shingle: Xwayland closed its readiness pipe unready");
      }
      Ok(Readiness::Ready(number)) => {
        let Some(stream) = process.take_wm_stream() else {
          return;
        };
        let pid = process.pid();
        let name = self.server.display_name();
        if format!(":{number}") != name {
          eprintln!("shingle: Xwayland reports display :{number}, not {name}");
        }
        let bridged = selection::bridged(state);
        let started = X11Connection::connect(stream)
          .and_then(Xwm::start)
          .and_then(|mut xwm| {
            xwm.bridge_selections(bridged)?;
            xwm.bridge_dnd()?;
            Ok(xwm)
          });
        match started {
          Ok(mut xwm) => {
            if let Some(cursor) = crate::cursor_shape::default_x_cursor(state)
              && let Err(error) = xwm.set_cursor(&cursor)
            {
              eprintln!("shingle: cannot set the X root cursor: {error}");
            }
            self.xwm_source = watch(handle, xwm_fd(&xwm), Source::Xwm);
            self.xwm = Some(xwm);
          }
          Err(error) => {
            eprintln!("shingle: X window manager failed, stopping: {error}");
            remove(handle, &mut self.exit_source);
            self.server.stop(STOP_GRACE);
            state.xwayland.pid = None;
            return;
          }
        }
        eprintln!("shingle: Xwayland ready on {name}");
        self.run_startup_script(state);
        state.ipc_events.push(json!({
            "event": "xwayland-ready",
            "display": name,
            "pid": pid,
        }));
      }
      Err(error) => {
        eprintln!("shingle: cannot read Xwayland's readiness pipe: {error}");
      }
    }
  }

  /// Runs the startup script, if any, as a reaped child.
  fn run_startup_script(&self, state: &mut CompositorState) {
    if self.startup_script.trim().is_empty() {
      return;
    }
    let environment = state.client_environment();
    match shingle_session::launch_command(
      &self.startup_script,
      &environment,
      self.discard_script_output,
    ) {
      Ok(child) => state.ipc_children.push(child),
      Err(error) => {
        eprintln!("shingle: cannot run the XWayland startup script: {error}");
      }
    }
  }

  /// Reads the window manager's events and applies them.
  fn dispatch_xwm(
    &mut self,
    state: &mut CompositorState,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    let Some(xwm) = &mut self.xwm else {
      return;
    };
    match xwm.dispatch() {
      Ok(events) => {
        for event in events {
          windows::apply(state, event);
        }
      }
      Err(error) => {
        eprintln!("shingle: X window manager lost its connection: {error}");
        self.drop_xwm(state, handle);
      }
    }
  }

  /// Reaps an exited server and applies the restart rule.
  fn reap(
    &mut self,
    state: &mut CompositorState,
    display: &DisplayHandle,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    // Reaping closes the pidfd and the pipe; stop watching them first.
    remove(handle, &mut self.exit_source);
    remove(handle, &mut self.readiness_source);
    let exited = match self.server.reap() {
      Ok(Some(exited)) => exited,
      Ok(None) => {
        // Not exited after all: keep watching.
        if let Some(process) = self.server.process() {
          self.exit_source = watch(handle, process.pidfd(), Source::Exit);
          self.readiness_source = process
            .displayfd()
            .and_then(|fd| watch(handle, fd, Source::Readiness));
        }
        return;
      }
      Err(error) => {
        eprintln!("shingle: cannot reap Xwayland: {error}");
        return;
      }
    };
    self.drop_xwm(state, handle);
    state.xwayland.pid = None;
    let ran = exited.ran_for.as_secs_f64();
    eprintln!(
      "shingle: Xwayland exited ({}) after {ran:.1} s",
      exited.status
    );
    match exited.restart {
      Restart::Now => {
        eprintln!("shingle: restarting Xwayland");
        self.start(state, display, handle);
      }
      Restart::OnConnection => {
        eprintln!("shingle: Xwayland restarts on the next X11 connection");
        self.watch_sockets(handle);
      }
      Restart::Never => {
        eprintln!(
          "shingle: not restarting Xwayland, which ran for {} s or less",
          server::MIN_RUN_FOR_RESTART.as_secs()
        );
      }
    }
  }

  /// Removes the window manager and its source. Every X window goes
  /// away with it, as if its surface had been destroyed.
  fn drop_xwm(
    &mut self,
    state: &mut CompositorState,
    handle: &LoopHandle<'static, CompositorState>,
  ) {
    remove(handle, &mut self.xwm_source);
    self.bridge.clear(handle);
    self.drag.clear();
    if self.xwm.take().is_some() {
      dnd::forget_x11(state);
      windows::forget_all(state);
      selection::forget_x11(state);
    }
  }

  /// Watches both display sockets for a lazy start.
  fn watch_sockets(&mut self, handle: &LoopHandle<'static, CompositorState>) {
    self.unwatch_sockets(handle);
    self.listen_sources = self
      .server
      .sockets()
      .listen_fds()
      .into_iter()
      .filter_map(|fd| watch(handle, fd, Source::Connection))
      .collect();
  }

  /// Stops watching the display sockets.
  fn unwatch_sockets(&mut self, handle: &LoopHandle<'static, CompositorState>) {
    for token in self.listen_sources.drain(..) {
      handle.remove(token);
    }
  }
}

/// The window manager connection's descriptor.
fn xwm_fd(xwm: &Xwm<X11Connection>) -> BorrowedFd<'_> {
  use std::os::fd::AsFd;
  xwm.connection().as_fd()
}

/// Registers `fd` as a level-triggered source that sets `source`'s flag.
/// The caller removes the source before the descriptor closes. A failed
/// registration is logged and yields `None`.
fn watch(
  handle: &LoopHandle<'static, CompositorState>,
  fd: BorrowedFd<'_>,
  source: Source,
) -> Option<RegistrationToken> {
  // SAFETY: every caller removes the source before the descriptor it
  // borrows here is closed, and the wrapper never closes it.
  let wrapper = unsafe { FdWrapper::new(fd.as_raw_fd()) };
  let inserted = handle.insert_source(
    Generic::new(wrapper, Interest::READ, Mode::Level),
    move |_, _, state: &mut CompositorState| {
      let status = &mut state.xwayland;
      match source {
        Source::Connection => status.connection_pending = true,
        Source::Readiness => status.readiness_pending = true,
        Source::Exit => status.exit_pending = true,
        Source::Xwm => status.xwm_pending = true,
      }
      Ok(PostAction::Continue)
    },
  );
  match inserted {
    Ok(token) => Some(token),
    Err(error) => {
      eprintln!("shingle: cannot watch an XWayland descriptor: {error}");
      None
    }
  }
}

/// Removes the source in `token`, if any.
fn remove(
  handle: &LoopHandle<'static, CompositorState>,
  token: &mut Option<RegistrationToken>,
) {
  if let Some(token) = token.take() {
    handle.remove(token);
  }
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    os::unix::net::UnixStream,
    path::Path,
    sync::atomic::{AtomicU32, Ordering},
    time::Instant,
  };

  use serde_json::{Value, json};

  use super::*;
  use crate::{Compositor, Config};

  /// A private directory for one test's sockets, display and fake server.
  fn temp_dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
      "shingle-xwayland-glue-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
  }

  /// Finds the descriptors in a fake server's arguments: the first
  /// `-listenfd` as `$listen`, `-displayfd` as `$ready` and `-wm` as `$wm`.
  const PARSE_ARGUMENTS: &str = concat!(
    "while [ $# -gt 0 ]; do case $1 in ",
    "-listenfd) listen=${listen:-$2};; -displayfd) ready=$2;; ",
    "-wm) wm=$2;; esac; shift; done",
  );

  /// A compositor in `directory` whose Xwayland is a bash script running
  /// `body` after [`PARSE_ARGUMENTS`].
  fn compositor(
    directory: &Path,
    mode: XwaylandMode,
    body: &str,
  ) -> Compositor {
    let program = directory.join("fake-xwayland");
    let script = format!("#!/bin/bash\n{PARSE_ARGUMENTS}\n{body}\n");
    server::write_script(&program, &script).unwrap();
    let config = Config {
      xwayland: XwaylandSettings {
        mode,
        socket_base: directory.to_owned(),
        program: Some(program.into_os_string()),
        ..XwaylandSettings::default()
      },
      ..Config::default()
    };
    Compositor::with_socket_path(config, &directory.join("wayland"))
      .expect("a compositor")
  }

  /// Calls the IPC method `method` without data.
  fn call(compositor: &mut Compositor, method: &str) -> Value {
    crate::ipc::respond(&mut compositor.state, &json!({"method": method}))
  }

  /// Polls until `done` holds, for at most ten seconds.
  fn poll_until(
    compositor: &mut Compositor,
    mut done: impl FnMut(&mut Compositor) -> bool,
  ) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(compositor) {
      assert!(Instant::now() < deadline, "timed out");
      compositor.poll(Some(Duration::from_millis(20))).unwrap();
    }
  }

  #[test]
  fn the_default_config_binds_no_display() {
    let directory = temp_dir();
    let mut compositor =
      Compositor::with_socket_path(Config::default(), &directory.join("w"))
        .unwrap();
    assert_eq!(compositor.x_display(), None);
    assert!(!directory.join(".X11-unix").exists());
    assert_eq!(compositor.client_environment().value("DISPLAY"), None);
    let reply = call(&mut compositor, "stipc/get_display");
    assert_eq!(reply["xwayland"], "");
    let reply = call(&mut compositor, "stipc/get_xwayland_display");
    assert_eq!(reply, json!({"result": "ok", "display": ""}));
    let reply = call(&mut compositor, "stipc/get_xwayland_pid");
    assert_eq!(reply, json!({"result": "ok", "pid": -1}));
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn the_config_file_default_is_lazy() {
    let config = Config::from(shingle_config::ShingleConfig::default());
    assert_eq!(config.xwayland.mode, XwaylandMode::Lazy);
    assert_eq!(config.xwayland.socket_base, Path::new("/tmp"));
    assert!(config.xwayland.program.is_none());
    assert!(config.xwayland.discard_script_output);
  }

  #[test]
  fn lazy_mode_binds_display_and_starts_on_the_first_connection() {
    let directory = temp_dir();
    let started = directory.join("started");
    let body = format!("touch {}\nexec sleep 60", started.display());
    let mut compositor = compositor(&directory, XwaylandMode::Lazy, &body);
    assert_eq!(compositor.x_display(), Some(":0"));
    assert!(directory.join(".X0-lock").exists());
    assert!(directory.join(".X11-unix/X0").exists());
    let environment = compositor.client_environment();
    assert_eq!(environment.value("DISPLAY"), Some(":0".as_ref()));
    assert_eq!(call(&mut compositor, "stipc/get_display")["xwayland"], ":0");
    assert_eq!(
      call(&mut compositor, "stipc/get_xwayland_display"),
      json!({"result": "ok", "display": ":0"})
    );
    // Nothing runs before an X client connects.
    for _ in 0..5 {
      compositor.poll(Some(Duration::from_millis(10))).unwrap();
    }
    assert_eq!(call(&mut compositor, "stipc/get_xwayland_pid")["pid"], -1);
    assert!(!started.exists());
    let _client = UnixStream::connect(directory.join(".X11-unix/X0")).unwrap();
    poll_until(&mut compositor, |compositor| {
      compositor.state.xwayland.pid.is_some()
    });
    let pid = call(&mut compositor, "stipc/get_xwayland_pid")["pid"]
      .as_i64()
      .unwrap();
    assert!(pid > 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
      assert!(Instant::now() < deadline, "the fake server did not run");
      std::thread::sleep(Duration::from_millis(10));
    }
    // Dropping the compositor stops the server and frees the display.
    drop(compositor);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert!(!directory.join(".X0-lock").exists());
    assert!(!directory.join(".X11-unix/X0").exists());
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn eager_mode_starts_with_the_compositor() {
    let directory = temp_dir();
    let mut compositor =
      compositor(&directory, XwaylandMode::Eager, "exec sleep 60");
    let pid = compositor.state.xwayland.pid.expect("started at once");
    assert_eq!(
      call(&mut compositor, "stipc/get_xwayland_pid"),
      json!({"result": "ok", "pid": pid})
    );
    drop(compositor);
    assert!(!directory.join(".X0-lock").exists());
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn stop_xwayland_frees_the_display_before_the_drop() {
    let directory = temp_dir();
    let mut compositor =
      compositor(&directory, XwaylandMode::Eager, "exec sleep 60");
    let pid = compositor.state.xwayland.pid.expect("started at once");
    compositor.stop_xwayland();
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert!(!directory.join(".X0-lock").exists());
    assert!(!directory.join(".X11-unix/X0").exists());
    assert_eq!(compositor.x_display(), None);
    assert_eq!(compositor.client_environment().value("DISPLAY"), None);
    assert_eq!(call(&mut compositor, "stipc/get_xwayland_pid")["pid"], -1);
    // A second call, and the drop after it, have nothing left to do.
    compositor.stop_xwayland();
    compositor.poll(Some(Duration::from_millis(10))).unwrap();
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_server_that_exits_at_once_is_not_restarted() {
    let directory = temp_dir();
    let runs = directory.join("runs");
    let body = format!("echo run >> {}\nexit 1", runs.display());
    let mut compositor = compositor(&directory, XwaylandMode::Eager, &body);
    assert!(compositor.state.xwayland.pid.is_some());
    poll_until(&mut compositor, |compositor| {
      compositor.state.xwayland.pid.is_none()
    });
    for _ in 0..10 {
      compositor.poll(Some(Duration::from_millis(10))).unwrap();
    }
    assert_eq!(fs::read_to_string(&runs).unwrap(), "run\n");
    assert_eq!(call(&mut compositor, "stipc/get_xwayland_pid")["pid"], -1);
    // The display stays bound, as in wlroots.
    assert_eq!(compositor.x_display(), Some(":0"));
    assert!(directory.join(".X0-lock").exists());
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }

  /// The lines of `path`, or none while it does not exist.
  fn lines(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |text| text.lines().count())
  }

  #[test]
  fn an_eager_server_that_ran_long_enough_restarts_at_once() {
    let directory = temp_dir();
    let runs = directory.join("runs");
    // Each run outlives the restart threshold, then dies.
    let body = format!("echo run >> {}\nsleep 5.5\nexit 1", runs.display());
    let mut compositor = compositor(&directory, XwaylandMode::Eager, &body);
    let first = compositor.state.xwayland.pid.expect("started at once");
    poll_until(&mut compositor, |compositor| {
      compositor
        .state
        .xwayland
        .pid
        .is_some_and(|pid| pid != first)
    });
    // Restarted without any connection, and running again.
    let deadline = Instant::now() + Duration::from_secs(10);
    while lines(&runs) < 2 {
      assert!(Instant::now() < deadline, "the restart did not run");
      std::thread::sleep(Duration::from_millis(10));
    }
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_lazy_server_that_ran_long_enough_restarts_on_connection() {
    let directory = temp_dir();
    let runs = directory.join("runs");
    // Accepts the waiting connection on its first display socket, as
    // Xwayland would, so that it does not stay readable after the exit.
    let accept = concat!(
      "python3 -c 'import socket, sys; ",
      "socket.socket(fileno=int(sys.argv[1])).accept()' \"$listen\"",
    );
    let body = format!(
      "echo run >> {}\n{accept}\nsleep 5.5\nexit 1",
      runs.display()
    );
    let mut compositor = compositor(&directory, XwaylandMode::Lazy, &body);
    let socket = directory.join(".X11-unix/X0");
    let _first = UnixStream::connect(&socket).unwrap();
    poll_until(&mut compositor, |compositor| {
      compositor.state.xwayland.pid.is_some()
    });
    poll_until(&mut compositor, |compositor| {
      compositor.state.xwayland.pid.is_none()
    });
    // No restart until the next connection.
    for _ in 0..10 {
      compositor.poll(Some(Duration::from_millis(10))).unwrap();
    }
    assert!(compositor.state.xwayland.pid.is_none());
    assert_eq!(lines(&runs), 1);
    let _second = UnixStream::connect(&socket).unwrap();
    poll_until(&mut compositor, |compositor| {
      compositor.state.xwayland.pid.is_some()
    });
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_failed_window_manager_stops_the_server() {
    let directory = temp_dir();
    // Reports display 0 ready and closes the window manager connection
    // without speaking X, so the connection setup fails.
    let body = "echo 0 >&$ready\neval \"exec $wm<&-\"\nexec sleep 60";
    let mut compositor = compositor(&directory, XwaylandMode::Eager, body);
    assert!(compositor.state.xwayland.pid.is_some());
    poll_until(&mut compositor, |compositor| {
      compositor.state.xwayland.pid.is_none()
    });
    let events = std::mem::take(&mut compositor.state.ipc_events);
    assert!(
      events
        .iter()
        .all(|event| event["event"] != "xwayland-ready")
    );
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_second_compositor_takes_the_next_display() {
    let directory = temp_dir();
    let first = compositor(&directory, XwaylandMode::Lazy, "exit 0");
    let second = Compositor::with_socket_path(
      Config {
        xwayland: XwaylandSettings {
          mode: XwaylandMode::Lazy,
          socket_base: directory.clone(),
          ..XwaylandSettings::default()
        },
        ..Config::default()
      },
      &directory.join("wayland-2"),
    )
    .unwrap();
    assert_eq!(first.x_display(), Some(":0"));
    assert_eq!(second.x_display(), Some(":1"));
    drop((first, second));
    fs::remove_dir_all(directory).unwrap();
  }

  #[test]
  fn a_changed_mode_keeps_the_running_display() {
    let directory = temp_dir();
    let mut compositor = compositor(&directory, XwaylandMode::Lazy, "exit 0");
    let mut config = Config::default();
    config.xwayland.startup_script = "true".into();
    compositor.apply_config(config).unwrap();
    assert_eq!(compositor.x_display(), Some(":0"));
    let xwayland = compositor.xwayland.as_ref().unwrap();
    assert_eq!(xwayland.mode(), StartMode::Lazy);
    assert_eq!(xwayland.startup_script, "true");
    drop(compositor);
    fs::remove_dir_all(directory).unwrap();
  }
}
