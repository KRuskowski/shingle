//! `shingle`: the Shingle compositor.
//!
//! One binary for every backend. It parses the command line
//! ([`cli::parse`]), selects the backend from it and the environment
//! ([`backend::select`]) and runs that backend: nested in a window
//! (`shingle-nested`), headless ([`headless`]), offscreen on the GPU
//! ([`offscreen`]) or native on libseat and libinput (`shingle-native`).

mod backend;
mod cli;
mod headless;
mod offscreen;

use std::{error::Error, ffi::OsString, process::ExitCode};

use backend::{Backend, Reason, Selection};
use cli::{Command, Options};
use shingle_config::ShingleConfig;

/// Exit status for a command line or environment that cannot be used.
const USAGE_STATUS: u8 = 2;

/// What `shingle` does for a command line and environment, decided before
/// anything is started.
#[derive(Debug, Eq, PartialEq)]
enum Plan {
  /// Print this to standard output and exit successfully.
  Print(String),
  /// Print this to standard error and exit with this status.
  Fail(String, u8),
  /// Run this backend with these options.
  Start(Selection, Options),
}

/// Decides what to do for `arguments` (without the program name) and the
/// environment that `env` looks up by name.
fn plan(
  arguments: impl IntoIterator<Item = OsString>,
  env: impl Fn(&str) -> Option<OsString>,
) -> Plan {
  let options = match cli::parse(arguments) {
    Ok(Command::Run(options)) => options,
    Ok(Command::Help) => return Plan::Print(cli::HELP.to_owned()),
    Ok(Command::Version) => {
      return Plan::Print(format!("shingle {}\n", env!("CARGO_PKG_VERSION")));
    }
    Err(error) => {
      return Plan::Fail(
        format!(
          "shingle: {error}\nTry 'shingle --help' for more information.\n"
        ),
        USAGE_STATUS,
      );
    }
  };
  match backend::select(options.backend, &env) {
    Ok(selection) if selection.backend == Backend::Native => {
      // Refuse an SSH login before libseat is even opened.
      match shingle_native::preflight(&env) {
        Ok(()) => Plan::Start(selection, options),
        Err(error) => Plan::Fail(no_seat(&error, &selection.reason), 1),
      }
    }
    // Offscreen and nested get several outputs with MO-5 and MO-6.
    Ok(selection)
      if options.outputs.is_some()
        && selection.backend != Backend::Headless =>
    {
      Plan::Fail(
        format!(
          "shingle: --outputs needs the headless backend, not {}\n",
          selection.backend
        ),
        USAGE_STATUS,
      )
    }
    Ok(selection) => Plan::Start(selection, options),
    Err(error) => Plan::Fail(format!("shingle: {error}\n"), USAGE_STATUS),
  }
}

/// The message for a native backend without a seat: the cause, why native
/// was selected and what to use instead.
fn no_seat(error: &shingle_native::NoSeat, reason: &Reason) -> String {
  let why = match reason {
    Reason::Argument => reason.to_string(),
    _ => format!("selected because {reason}"),
  };
  format!(
    "shingle: {error}\n\
     shingle: the native backend was {why}; inside a Wayland or X11 \
     session shingle runs nested, and --headless needs no seat.\n"
  )
}

/// Blocks SIGINT and SIGTERM in the calling thread, and so in every
/// thread it starts afterwards.
///
/// The compositor reads both through a signalfd, which only works while
/// every thread has them blocked; calloop blocks them only in the thread
/// that creates the compositor. The offscreen and native backends open
/// the GPU first, and a thread the driver starts then would take a
/// SIGTERM with the default action and end Shingle without saving the
/// session (NVIDIA's driver does). A signal that arrives before the
/// compositor exists stays pending and ends it as soon as it does.
/// Children start with an empty mask, which Rust's `Command` sets.
fn block_shutdown_signals() -> std::io::Result<()> {
  // SAFETY: `set` is initialised by sigemptyset before use, and
  // pthread_sigmask only changes this thread's mask.
  unsafe {
    let mut set = std::mem::zeroed::<libc::sigset_t>();
    libc::sigemptyset(&mut set);
    libc::sigaddset(&mut set, libc::SIGINT);
    libc::sigaddset(&mut set, libc::SIGTERM);
    match libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) {
      0 => Ok(()),
      error => Err(std::io::Error::from_raw_os_error(error)),
    }
  }
}

/// Loads the configuration and runs the selected backend until it ends.
fn start(
  selection: &Selection,
  options: Options,
) -> Result<(), Box<dyn Error>> {
  block_shutdown_signals()?;
  // Clients find IPC as WAYFIRE_SOCKET through the compositor's client
  // environment, not through Shingle's own environment.
  let config = ShingleConfig::load(options.config.as_deref())?;
  match selection.backend {
    Backend::Nested => shingle_nested::run(
      config,
      options.config,
      options.ipc.as_deref(),
      options.spawn,
    ),
    Backend::Headless => {
      let outputs = headless::output_count(
        options.outputs,
        std::env::var("WLR_HEADLESS_OUTPUTS").ok().as_deref(),
      )?;
      headless::run(config, options.ipc.as_deref(), options.spawn, outputs)
    }
    Backend::Offscreen => offscreen::run(
      config,
      options.config,
      options.ipc.as_deref(),
      options.spawn,
    ),
    Backend::Native => shingle_native::run(
      config,
      options.config,
      options.ipc.as_deref(),
      options.spawn,
      shingle_native::Options {
        allow_builtin_seat: options.allow_builtin_seat,
      },
    ),
  }
}

/// The exit status for a backend that ended with `error`: 2 for a native
/// backend that gave up after startup (docs/design/native-backend.md,
/// section 6), 1 for everything else, such as a start that failed.
fn failure_status(error: &(dyn Error + 'static)) -> u8 {
  if error.downcast_ref::<shingle_native::Fatal>().is_some() {
    2
  } else {
    1
  }
}

fn main() -> ExitCode {
  match plan(std::env::args_os().skip(1), |name| std::env::var_os(name)) {
    Plan::Print(text) => {
      print!("{text}");
      ExitCode::SUCCESS
    }
    Plan::Fail(text, status) => {
      eprint!("{text}");
      ExitCode::from(status)
    }
    Plan::Start(selection, options) => {
      eprintln!(
        "shingle: using the {} backend ({})",
        selection.backend, selection.reason
      );
      match start(&selection, options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
          eprintln!("shingle: {error}");
          ExitCode::from(failure_status(error.as_ref()))
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_native_backend_that_gives_up_exits_with_status_2() {
    let fatal = shingle_native::Fatal("3 page flips in a row".into());
    assert_eq!(failure_status(&fatal), 2);
    let start =
      shingle_native::StartError::NoAdapter("no Vulkan driver".into());
    assert_eq!(failure_status(&start), 1);
    let io = std::io::Error::other("anything else");
    assert_eq!(failure_status(&io), 1);
  }

  fn planned(arguments: &[&str], pairs: &[(&str, &str)]) -> Plan {
    let pairs: Vec<(String, OsString)> = pairs
      .iter()
      .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
      .collect();
    plan(arguments.iter().map(OsString::from), move |name| {
      pairs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
    })
  }

  fn started(plan: Plan) -> (Backend, Options) {
    match plan {
      Plan::Start(selection, options) => (selection.backend, options),
      other => panic!("expected a start, got {other:?}"),
    }
  }

  /// Whether `signal` is blocked in the calling thread.
  fn blocked(signal: libc::c_int) -> bool {
    // SAFETY: a null new set only reads the mask into `current`.
    unsafe {
      let mut current = std::mem::zeroed::<libc::sigset_t>();
      libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut current);
      libc::sigismember(&current, signal) == 1
    }
  }

  #[test]
  fn shutdown_signals_are_blocked_for_threads_started_later() {
    // A thread of its own, so the test runner's mask is left alone.
    std::thread::spawn(|| {
      assert!(!blocked(libc::SIGTERM));
      block_shutdown_signals().expect("the mask can be changed");
      assert!(blocked(libc::SIGTERM) && blocked(libc::SIGINT));
      let inherited =
        std::thread::spawn(|| (blocked(libc::SIGTERM), blocked(libc::SIGINT)));
      assert_eq!(inherited.join().unwrap(), (true, true));
      assert!(!blocked(libc::SIGHUP));
    })
    .join()
    .expect("the checks pass");
  }

  #[test]
  fn wayland_session_runs_nested() {
    let (backend, options) = started(planned(
      &["--config", "data/shingle.yaml", "--spawn", "foot"],
      &[("WAYLAND_DISPLAY", "wayland-1")],
    ));
    assert_eq!(backend, Backend::Nested);
    assert_eq!(options.config, Some("data/shingle.yaml".into()));
    assert_eq!(options.spawn, [OsString::from("foot")]);
  }

  #[test]
  fn offscreen_runs_offscreen_inside_a_session() {
    let (backend, _) = started(planned(
      &["--backend", "offscreen"],
      &[("WAYLAND_DISPLAY", "wayland-1"), ("SSH_CONNECTION", "x")],
    ));
    assert_eq!(backend, Backend::Offscreen);
  }

  #[test]
  fn headless_runs_headless_inside_a_session() {
    let (backend, _) = started(planned(
      &["--headless"],
      &[("WAYLAND_DISPLAY", "wayland-1")],
    ));
    assert_eq!(backend, Backend::Headless);
  }

  // These tests only plan: no seat, device or VT is touched.

  #[test]
  fn outputs_need_the_headless_backend() {
    let (backend, options) =
      started(planned(&["--headless", "--outputs", "2"], &[]));
    assert_eq!((backend, options.outputs), (Backend::Headless, Some(2)));
    let session = [("WAYLAND_DISPLAY", "wayland-1")];
    for arguments in [
      &["--outputs", "2"][..],
      &["--backend", "offscreen", "--outputs", "2"],
    ] {
      assert!(matches!(
        planned(arguments, &session),
        Plan::Fail(text, USAGE_STATUS) if text.contains("--outputs")
      ));
    }
  }

  #[test]
  fn tty_login_selects_native() {
    let (backend, options) = started(planned(
      &["--allow-builtin-seat"],
      &[("TERM", "linux"), ("XDG_SEAT", "seat0")],
    ));
    assert_eq!(backend, Backend::Native);
    assert!(options.allow_builtin_seat);
    let (backend, _) = started(planned(&["--backend", "native"], &[]));
    assert_eq!(backend, Backend::Native);
  }

  #[test]
  fn ssh_without_a_seat_is_refused_before_anything_starts() {
    let Plan::Fail(message, status) =
      planned(&[], &[("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.2 22")])
    else {
      panic!("native must not start over SSH");
    };
    assert_eq!(status, 1);
    assert!(
      message.starts_with("shingle: no seat: started over SSH"),
      "{message}"
    );
    assert!(message.contains("run from a VT login or start seatd"));
    assert!(
      message.contains(
        "selected because none of WAYLAND_DISPLAY, WAYLAND_SOCKET or \
         DISPLAY is set"
      ),
      "{message}"
    );
    assert!(message.contains("--headless"), "{message}");
    let Plan::Fail(message, 1) =
      planned(&["--backend", "native"], &[("SSH_TTY", "/dev/pts/1")])
    else {
      panic!("native must not start over SSH");
    };
    assert!(
      message.contains("requested on the command line"),
      "{message}"
    );
  }

  #[test]
  fn ssh_with_a_host_session_still_nests() {
    let (backend, _) = started(planned(
      &[],
      &[("SSH_CONNECTION", "x"), ("WAYLAND_DISPLAY", "wayland-1")],
    ));
    assert_eq!(backend, Backend::Nested);
  }

  #[test]
  fn bad_options_are_usage_errors() {
    for arguments in [
      &["--backend", "drm"][..],
      &["--bogus"][..],
      &["--config"][..],
      &["--headless", "--backend=nested"][..],
    ] {
      let Plan::Fail(message, status) =
        planned(arguments, &[("WAYLAND_DISPLAY", "wayland-1")])
      else {
        panic!("{arguments:?} must fail");
      };
      assert_eq!(status, USAGE_STATUS, "{arguments:?}");
      assert!(message.starts_with("shingle: "), "{message}");
      assert!(
        message.ends_with("Try 'shingle --help' for more information.\n")
      );
    }
  }

  #[test]
  fn unknown_backend_variable_is_a_usage_error() {
    assert_eq!(
      planned(&[], &[("SHINGLE_BACKEND", "x11")]),
      Plan::Fail(
        "shingle: SHINGLE_BACKEND: unknown backend 'x11' \
         (expected nested, native, headless or offscreen)\n"
          .into(),
        USAGE_STATUS
      )
    );
  }

  #[test]
  fn help_and_version_print() {
    assert_eq!(planned(&["--help"], &[]), Plan::Print(cli::HELP.into()));
    let Plan::Print(version) = planned(&["--version"], &[]) else {
      panic!("--version prints");
    };
    assert_eq!(version, format!("shingle {}\n", env!("CARGO_PKG_VERSION")));
  }
}
