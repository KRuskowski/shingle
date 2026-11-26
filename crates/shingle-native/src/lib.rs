//! The native backend: Shingle on a seat, started from a VT.
//!
//! It runs [`shingle_frontend::Frontend`] with libseat for the session and
//! VT switching ([`session`]), libinput for input devices ([`input`]) with
//! hotplug through libinput's udev backend ([`udev`]), atomic KMS for the
//! display ([`kms`], [`display`]) with zero-copy or copy scanout
//! ([`scanout`]) and
//! page-flip pacing ([`pacing`]), DRM hotplug ([`hotplug`]) and the
//! configured choice of output and mode ([`outputs`]), a Vulkan renderer
//! on the card's GPU ([`gpu`]), and the event loop in [`host`]
//! (docs/design/native-backend.md). The `shingle` binary
//! (crates/shingle-bin) checks [`preflight`] when it selects this backend
//! and then calls [`run`].
//!
//! Every device interaction sits behind a trait ([`session::Session`],
//! [`input::InputSource`], [`kms::KmsDevice`],
//! [`hotplug::DeviceMonitor`]) whose real implementation
//! is only ever run on a machine whose seat Shingle may take; the unit
//! tests use fakes.

#![warn(missing_docs)]

pub mod crtcs;
mod diagnostics;
pub mod display;
pub mod gpu;
pub mod host;
pub mod hotplug;
pub mod input;
pub mod kms;
pub mod outputs;
pub mod pacing;
pub mod scanout;
pub mod session;
#[cfg(test)]
mod test_client;
pub mod udev;

use std::{
  cell::RefCell,
  error::Error,
  ffi::OsString,
  io,
  os::fd::OwnedFd,
  path::{Path, PathBuf},
  rc::Rc,
  time::Instant,
};

use shingle_config::ShingleConfig;
use shingle_frontend::Frontend;

pub use display::{Fatal, StartError};
pub use session::{NoSeat, preflight};

use crate::{
  gpu::{Card, FrameRenderer, GpuRenderer, RendererFactory},
  host::{Backend, Rendering},
  hotplug::UdevMonitor,
  input::LibinputSource,
  kms::{KmsDevice, drm::DrmDevice},
  outputs::OutputPolicy,
  scanout::ScanoutChoice,
  session::{
    ENABLE_TIMEOUT, LibSeatSession, Session, check_backend, wait_for_enable,
  },
};

/// The output size while no output is lit, as when nothing is connected
/// at startup.
pub const OUTPUT_SIZE: (u32, u32) = (1280, 720);

/// Where sysfs lists DRM cards.
const SYS_CLASS_DRM: &str = "/sys/class/drm";

/// Options of the native backend from the command line.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
  /// `--allow-builtin-seat`: run on a libseat backend that cannot switch
  /// VT.
  pub allow_builtin_seat: bool,
  /// `--outputs N`: light at most this many monitors.
  pub max_outputs: Option<u32>,
}

/// A prepared display, its card's PCI slot and its device node.
pub type Prepared = (display::Display, Option<String>, PathBuf);

/// Opens the first card in `cards` that the seat lets Shingle open, as
/// `open` makes a [`KmsDevice`] of its descriptor, and prepares its
/// display ([`display::Display::prepare_limited`]) with the configured
/// modes of `policy`, lighting at most `max_outputs` monitors. Returns the
/// display, the card's PCI slot, for the renderer, and its device node,
/// for the hotplug monitor. Cards that cannot be opened are skipped; if
/// none can, the error names each one tried.
pub fn open_display(
  session: &mut dyn Session,
  cards: &[Card],
  policy: &OutputPolicy,
  max_outputs: Option<usize>,
  open: &mut dyn FnMut(OwnedFd) -> io::Result<Box<dyn KmsDevice>>,
) -> Result<Prepared, StartError> {
  let mut tried = Vec::new();
  for card in cards {
    let device = match session.open_device(&card.devnode).and_then(&mut *open) {
      Ok(device) => device,
      Err(error) => {
        tried.push((card.devnode.clone(), error.to_string()));
        continue;
      }
    };
    diagnostics::log(&format!("using {}", card.devnode.display()));
    let display =
      display::Display::prepare_limited(device, policy.clone(), max_outputs)?;
    return Ok((display, card.pci_slot.clone(), card.devnode.clone()));
  }
  Err(StartError::NoDrmDevice(tried))
}

/// Sets up everything that can fail before the Wayland socket exists, in
/// the order of docs/design/native-backend.md, section 6: the display,
/// with a `TEST_ONLY` modeset passed, then the renderer. Only then does
/// `frontend` create the socket and start clients, so a failed start
/// leaves none behind.
pub fn startup<T>(
  display: impl FnOnce() -> Result<Prepared, StartError>,
  rendering: impl FnOnce(
    (u32, u32),
    Option<String>,
  ) -> Result<Rendering, StartError>,
  frontend: impl FnOnce() -> Result<T, Box<dyn Error>>,
) -> Result<(display::Display, Rendering, T, PathBuf), Box<dyn Error>> {
  let (display, pci, devnode) = display()?;
  let rendering = rendering(display.size().unwrap_or(OUTPUT_SIZE), pci)?;
  let frontend = frontend()?;
  Ok((display, rendering, frontend, devnode))
}

/// The real renderer on the card's GPU, and the factory that recreates it
/// after the GPU was lost.
fn gpu_rendering(
  size: (u32, u32),
  pci: Option<String>,
) -> Result<Rendering, StartError> {
  let renderer =
    GpuRenderer::new(size, pci.as_deref()).map_err(StartError::NoAdapter)?;
  diagnostics::log(&format!("rendering on {}", renderer.adapter_name()));
  diagnostics::log(&renderer.dmabuf_summary());
  let factory: RendererFactory = Box::new(move |size| {
    GpuRenderer::new(size, pci.as_deref())
      .map(|renderer| Box::new(renderer) as Box<dyn FrameRenderer>)
  });
  Ok(Rendering {
    renderer: Box::new(renderer),
    factory,
  })
}

/// Runs Shingle on the seat of the current VT until it quits or a SIGINT
/// or SIGTERM arrives, then saves the session and closes the seat.
///
/// The seat, the display (with a passed `TEST_ONLY` modeset), the
/// renderer and libinput are set up before [`Frontend::new`] creates the
/// Wayland socket and starts clients, so a start that fails leaves no
/// clients behind. Without a seat this returns [`NoSeat`], and when the
/// display cannot work, [`StartError`]; both mean exit status 1. A
/// [`Fatal`] failure after startup means exit status 2. The arguments are
/// those of `shingle_nested::run`.
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
  let cards = gpu::enumerate_cards(Path::new(SYS_CLASS_DRM))
    .map_err(|error| StartError::Probe(format!("{SYS_CLASS_DRM}: {error}")))?;
  let forced = std::env::var_os("SHINGLE_DRM_DEVICE");
  let cards = gpu::card_order(&cards, forced.as_deref());
  let input_config = config.input.clone();
  let policy = OutputPolicy::from_config(&config);
  let (display, rendering, (frontend, input), devnode) = startup(
    || {
      open_display(
        &mut *session.borrow_mut(),
        &cards,
        &policy,
        options.max_outputs.map(|max| max as usize),
        &mut |fd| Ok(Box::new(DrmDevice::new(fd)?)),
      )
    },
    gpu_rendering,
    || {
      let input = LibinputSource::new(Rc::clone(&session))?;
      let frontend = Frontend::new(config, spawn, config_path, ipc_path)?;
      Ok((frontend, input))
    },
  )?;
  let (choice, warning) =
    ScanoutChoice::from_env(std::env::var_os("SHINGLE_SCANOUT").as_deref());
  if let Some(warning) = warning {
    diagnostics::log(&warning);
  }
  let mut display = display;
  display.set_scanout_choice(choice);
  let mut backend =
    Backend::new(frontend, session, input, input_config, display, rendering)?;
  // Clients start once linux-dmabuf is offered, which Backend::new does.
  backend.frontend_mut().start_clients()?;
  backend.set_trace_flips(
    std::env::var_os("SHINGLE_TRACE_FLIPS").is_some_and(|value| value == "1"),
  );
  // Without a monitor, outputs plugged in later are only seen after a VT
  // switch, as in NB-7; that is worth a warning, not a refusal.
  match UdevMonitor::new(&devnode) {
    Ok(monitor) => backend.set_device_monitor(Box::new(monitor)),
    Err(error) => diagnostics::log(&format!(
      "no DRM hotplug monitor ({error}); outputs are probed again only \
       after a VT switch"
    )),
  }
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

  use std::{cell::RefCell, collections::VecDeque, rc::Rc};

  use super::{OUTPUT_SIZE, StartError, open_display, startup};
  use crate::outputs::OutputPolicy;
  use crate::{
    gpu::Card,
    kms::{CommitFlags, KmsDevice, fake::FakeKms},
    session::tests::FakeSession,
  };

  fn card(n: u32, slot: &str) -> Card {
    Card {
      devnode: PathBuf::from(format!("/dev/dri/card{n}")),
      boot_vga: n == 1,
      pci_slot: Some(slot.to_owned()),
    }
  }

  #[test]
  fn cards_the_seat_refuses_are_skipped() {
    let mut session = FakeSession {
      refuse_open: vec!["/dev/dri/card1".into()],
      ..FakeSession::default()
    };
    let cards = [card(1, "0000:01:00.0"), card(0, "0000:00:02.0")];
    let fake = FakeKms::single_output().shared();
    let policy = OutputPolicy::default();
    let (display, pci, devnode) =
      open_display(&mut session, &cards, &policy, None, &mut |_fd| {
        Ok(Box::new(Rc::clone(&fake)) as Box<dyn KmsDevice>)
      })
      .expect("the second card works");
    assert_eq!(session.opened, ["/dev/dri/card1", "/dev/dri/card0"]);
    assert_eq!(pci.as_deref(), Some("0000:00:02.0"));
    assert_eq!(devnode, PathBuf::from("/dev/dri/card0"));
    assert_eq!(display.size(), Some((1280, 720)));
  }

  #[test]
  fn no_card_that_opens_names_every_card_tried() {
    let mut session = FakeSession {
      refuse_open: vec!["/dev/dri/card1".into(), "/dev/dri/card0".into()],
      ..FakeSession::default()
    };
    let cards = [card(1, "a"), card(0, "b")];
    let Err(error) = open_display(
      &mut session,
      &cards,
      &OutputPolicy::default(),
      None,
      &mut |_fd| Ok(Box::new(FakeKms::single_output()) as Box<dyn KmsDevice>),
    ) else {
      panic!("no card opens");
    };
    let message = error.to_string();
    assert!(message.starts_with("no DRM device: tried /dev/dri/card1 ("));
    assert!(message.contains(", /dev/dri/card0 ("), "{message}");
    let Err(error) = open_display(
      &mut session,
      &[],
      &OutputPolicy::default(),
      None,
      &mut |_fd| Ok(Box::new(FakeKms::single_output()) as Box<dyn KmsDevice>),
    ) else {
      panic!("there is no card");
    };
    assert!(
      matches!(error, StartError::NoDrmDevice(tried) if tried.is_empty())
    );
  }

  #[test]
  fn the_test_commit_passes_before_the_socket_exists() {
    let fake = FakeKms::single_output().shared();
    let order = Rc::new(RefCell::new(VecDeque::new()));
    let (display, _, _, _) = startup(
      || {
        let device = Box::new(Rc::clone(&fake));
        let display =
          crate::display::Display::prepare(device, OutputPolicy::default())?;
        order.borrow_mut().push_back("display");
        Ok((display, None, PathBuf::from("/dev/dri/card0")))
      },
      |size, _| {
        assert_eq!(size, (1280, 720));
        order.borrow_mut().push_back("renderer");
        let counters = (
          Rc::new(RefCell::new(VecDeque::new())),
          Rc::new(RefCell::new(0)),
          Rc::new(RefCell::new(0)),
        );
        Ok(crate::host::tests::fake_rendering(
          size,
          &counters.0,
          &counters.1,
          &counters.2,
        ))
      },
      || {
        let card = fake.borrow();
        let flags: Vec<_> = card
          .commits
          .iter()
          .map(|(_, flags, ok)| (*flags, *ok))
          .collect();
        assert_eq!(
          flags,
          [(CommitFlags::TEST_ONLY | CommitFlags::ALLOW_MODESET, true)],
          "a passed test and no real commit before the socket"
        );
        order.borrow_mut().push_back("socket");
        Ok(())
      },
    )
    .expect("startup succeeds");
    assert_eq!(
      order.borrow().iter().copied().collect::<Vec<_>>(),
      ["display", "renderer", "socket"]
    );
    assert_eq!(display.size(), Some((1280, 720)));
  }

  #[test]
  fn a_display_that_cannot_work_starts_no_clients() {
    let fake = FakeKms::single_output().shared();
    fake.borrow_mut().reject_all(22);
    let result = startup(
      || {
        let device = Box::new(Rc::clone(&fake));
        let display =
          crate::display::Display::prepare(device, OutputPolicy::default())?;
        Ok((display, None, PathBuf::from("/dev/dri/card0")))
      },
      |_, _| -> Result<crate::host::Rendering, StartError> {
        panic!("no renderer without a display")
      },
      || -> Result<(), Box<dyn std::error::Error>> {
        panic!("no socket and no clients")
      },
    );
    let error = result.err().expect("startup fails");
    assert!(
      error.to_string().contains("every mode was rejected"),
      "{error}"
    );
    let renderer = startup(
      || Ok((no_output_display(), None, PathBuf::from("/dev/dri/card0"))),
      |size, _| {
        assert_eq!(size, OUTPUT_SIZE, "no output renders at the default size");
        Err(StartError::NoAdapter("none".into()))
      },
      || -> Result<(), Box<dyn std::error::Error>> {
        panic!("no socket without a renderer")
      },
    );
    let error = renderer.err().expect("no adapter fails");
    assert_eq!(error.to_string(), "no Vulkan adapter: none");
  }

  fn no_output_display() -> crate::display::Display {
    let mut fake = FakeKms::single_output();
    fake.disconnect();
    crate::display::Display::prepare(Box::new(fake), OutputPolicy::default())
      .expect("no output is no error")
  }
}
