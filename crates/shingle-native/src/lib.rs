//! The native backend: Shingle on a seat, started from a VT.
//!
//! It runs [`shingle_frontend::Frontend`] with libseat for the session and
//! VT switching ([`session`]), libinput for input devices ([`input`]) with
//! hotplug through libinput's udev backend ([`udev`]), and the event loop
//! in [`host`]. There is no display yet: frames are drawn offscreen at a
//! fixed size, and KMS follows in NB-7 (docs/design/native-backend.md).
//! The `shingle` binary (crates/shingle-bin) checks [`preflight`] when it
//! selects this backend and then calls [`run`].
//!
//! Every device interaction sits behind a trait ([`session::Session`],
//! [`input::InputSource`]) whose real implementation is only ever run on
//! a machine whose seat Shingle may take; the unit tests use fakes.

#![warn(missing_docs)]

mod diagnostics;
pub mod host;
pub mod input;
pub mod session;
#[cfg(test)]
mod test_client;
pub mod udev;

use std::{
  cell::RefCell,
  error::Error,
  ffi::OsString,
  path::{Path, PathBuf},
  rc::Rc,
  time::Instant,
};

use shingle_config::ShingleConfig;
use shingle_frontend::Frontend;
use shingle_render::OffscreenRenderer;

pub use session::{NoSeat, preflight};

use crate::{
  host::Backend,
  input::LibinputSource,
  session::{
    ENABLE_TIMEOUT, LibSeatSession, Session, check_backend, wait_for_enable,
  },
};

/// The size of the offscreen output until KMS describes a real one.
pub const OUTPUT_SIZE: (u32, u32) = (1280, 720);

/// Options of the native backend from the command line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
  /// `--allow-builtin-seat`: run on a libseat backend that cannot switch
  /// VT.
  pub allow_builtin_seat: bool,
}

/// Runs Shingle on the seat of the current VT until it quits or a SIGINT
/// or SIGTERM arrives, then saves the session and closes the seat.
///
/// The seat, the renderer and libinput are set up before
/// [`Frontend::new`] creates the Wayland socket and starts clients, so a
/// start that fails leaves no clients behind. Without a seat this returns
/// [`NoSeat`]. The arguments are those of `shingle_nested::run`.
pub fn run(
  config: ShingleConfig,
  config_path: Option<PathBuf>,
  ipc_path: Option<&Path>,
  spawn: Vec<OsString>,
  options: Options,
) -> Result<(), Box<dyn Error>> {
  if let Some(path) = diagnostics::log_path(|name| std::env::var_os(name))
    && let Err(error) = diagnostics::open_log(&path)
  {
    eprintln!("shingle: cannot write {}: {error}", path.display());
  }
  let mut session = LibSeatSession::open()?;
  wait_for_enable(&mut session, ENABLE_TIMEOUT, &Instant::now)?;
  let backend = session.backend();
  check_backend(backend.as_deref(), options.allow_builtin_seat)?;
  diagnostics::log(&format!(
    "seat {} enabled ({} backend)",
    session.seat_name(),
    backend.as_deref().unwrap_or("unknown")
  ));
  let session = Rc::new(RefCell::new(session));
  let renderer =
    pollster::block_on(OffscreenRenderer::new(OUTPUT_SIZE.0, OUTPUT_SIZE.1))?;
  let input = LibinputSource::new(Rc::clone(&session))?;
  let input_config = config.input.clone();
  let frontend = Frontend::new(config, spawn, config_path, ipc_path)?;
  let backend = Backend::new(
    frontend,
    session,
    input,
    input_config,
    OUTPUT_SIZE,
    Some(Box::new(renderer)),
  )?;
  backend.run()
}

#[cfg(test)]
mod tests {
  use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
  };

  use shingle::{Compositor, Config, InputDeviceId};
  use shingle_config::{ShingleConfig, XwaylandMode};

  /// A configuration that never reads the user's files or binds an X
  /// display.
  pub(crate) fn test_config() -> ShingleConfig {
    let mut config = ShingleConfig::default();
    config.behavior.auto_reload = false;
    config.session.restore = false;
    config.xwayland.mode = XwaylandMode::Disabled;
    config
  }

  /// A fresh base path in the temporary directory for one compositor's
  /// sockets and session file.
  pub(crate) fn base_path() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
      "shingle-native-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ))
  }

  /// A compositor on a private Wayland socket with IPC next to it.
  pub(crate) fn compositor() -> Compositor {
    let base = base_path();
    let mut compositor =
      Compositor::with_socket_path(Config::from(test_config()), &base)
        .expect("the temporary directory accepts a Wayland socket");
    compositor
      .enable_ipc(&base.with_extension("ipc"))
      .expect("the temporary directory accepts an IPC socket");
    compositor
  }

  /// Sends one IPC request to the compositor's socket at `path` and
  /// returns the reply, running `poll` until it arrives.
  pub(crate) fn ipc_request(
    path: &std::path::Path,
    request: &str,
    mut poll: impl FnMut(),
  ) -> String {
    let mut stream = UnixStream::connect(path).expect("the IPC socket accepts");
    let length = u32::try_from(request.len()).expect("a short request");
    stream
      .write_all(&length.to_ne_bytes())
      .and_then(|()| stream.write_all(request.as_bytes()))
      .expect("the request is sent");
    stream.set_nonblocking(true).expect("nonblocking");
    let mut reply = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
      poll();
      let mut buffer = [0; 4096];
      match stream.read(&mut buffer) {
        Ok(read) => reply.extend_from_slice(&buffer[..read]),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("reading the reply failed: {error}"),
      }
      if reply.len() >= 4 {
        let length = u32::from_ne_bytes(reply[..4].try_into().unwrap());
        if reply.len() >= 4 + length as usize {
          return String::from_utf8(reply[4..].to_vec()).expect("UTF-8");
        }
      }
    }
    panic!("no IPC reply to {request}");
  }

  /// The `input/configure-device` request for `id`.
  pub(crate) fn configure_request(id: InputDeviceId, enabled: bool) -> String {
    format!(
      concat!(
        r#"{{"method":"input/configure-device","#,
        r#""data":{{"id":{},"enabled":{}}}}}"#
      ),
      id.get(),
      enabled
    )
  }

  /// Disables `id` over IPC.
  pub(crate) fn disable(compositor: &mut Compositor, id: InputDeviceId) {
    let path = compositor_ipc_path(compositor);
    let reply = ipc_request(&path, &configure_request(id, false), || {
      compositor
        .poll(Some(Duration::from_millis(5)))
        .expect("dispatch succeeds");
    });
    assert_eq!(reply, r#"{"result":"ok"}"#);
  }

  /// The IPC socket of a compositor made by [`compositor`].
  pub(crate) fn compositor_ipc_path(compositor: &Compositor) -> PathBuf {
    PathBuf::from(compositor.socket_name()).with_extension("ipc")
  }
}
