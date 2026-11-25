//! Launch identities: how a client was started, read from `/proc`, so a
//! restored session can relaunch it and match its windows to their saved
//! tiles.

use std::{fs, os::unix::net::UnixStream, path::Path};

use rustix::net::sockopt::socket_peercred;
use shingle_core::LaunchIdentity;

const PROC_LIMIT: usize = 1024 * 1024;

/// The launch identity of the process at the other end of a Wayland
/// connection, found through its peer credentials. Without credentials the
/// identity is empty.
pub(crate) fn capture(stream: &UnixStream) -> LaunchIdentity {
  let Ok(credentials) = socket_peercred(stream) else {
    return LaunchIdentity::default();
  };
  capture_pid(credentials.pid.as_raw_nonzero().get())
}

/// The launch identity of process `pid`: its command line, working
/// directory and `SHINGLE_LAUNCH_ID`, with the program name as the app ID
/// and `pid-<pid>` as the launch group. XWayland windows, which all share
/// Xwayland's connection, get theirs from the PID of their X client.
pub(crate) fn capture_pid(pid: i32) -> LaunchIdentity {
  capture_pid_in(Path::new("/proc"), pid)
}

/// [`capture_pid`] with `proc` in place of `/proc`, for tests.
fn capture_pid_in(proc: &Path, pid: i32) -> LaunchIdentity {
  let root = proc.join(pid.to_string());
  let command = fs::read(root.join("cmdline"))
    .ok()
    .map(|bytes| parse_strings(&bytes))
    .unwrap_or_default();
  let working_directory = fs::read_link(root.join("cwd"))
    .ok()
    .map(|path| path.to_string_lossy().into_owned());
  let launch_id = fs::read(root.join("environ")).ok().and_then(|bytes| {
    parse_strings(&bytes).into_iter().find_map(|entry| {
      // Clients restored by a pre-rename build carry TILEFIRE_LAUNCH_ID.
      entry
        .strip_prefix("SHINGLE_LAUNCH_ID=")
        .or_else(|| entry.strip_prefix("TILEFIRE_LAUNCH_ID="))
        .map(str::to_owned)
    })
  });
  let app_id = command
    .first()
    .and_then(|program| {
      Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    })
    .unwrap_or_default();
  LaunchIdentity {
    app_id,
    launch_id,
    command,
    working_directory,
    title: None,
    launch_group: Some(format!("pid-{pid}")),
  }
}

fn parse_strings(bytes: &[u8]) -> Vec<String> {
  bytes
    .split(|byte| *byte == 0)
    .take(PROC_LIMIT)
    .filter(|part| !part.is_empty())
    .map(|part| String::from_utf8_lossy(part).into_owned())
    .collect()
}

#[cfg(test)]
mod tests {
  use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
  };

  use super::*;

  /// A fake `/proc/<pid>` under the temporary directory.
  fn fake_proc(
    pid: i32,
    cmdline: &[u8],
    environ: &[u8],
    cwd: Option<&str>,
  ) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let proc = std::env::temp_dir().join(format!(
      "shingle-launch-test-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = proc.join(pid.to_string());
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("cmdline"), cmdline).unwrap();
    fs::write(root.join("environ"), environ).unwrap();
    if let Some(cwd) = cwd {
      std::os::unix::fs::symlink(cwd, root.join("cwd")).unwrap();
    }
    proc
  }

  #[test]
  fn capture_pid_reads_command_directory_and_launch_id() {
    let proc = fake_proc(
      42,
      b"/usr/bin/foot\0--server\0",
      b"HOME=/home/x\0SHINGLE_LAUNCH_ID=shingle-restore-3\0",
      Some("/work"),
    );
    let identity = capture_pid_in(&proc, 42);
    assert_eq!(identity.app_id, "foot");
    assert_eq!(identity.command, ["/usr/bin/foot", "--server"]);
    assert_eq!(identity.working_directory.as_deref(), Some("/work"));
    assert_eq!(identity.launch_id.as_deref(), Some("shingle-restore-3"));
    assert_eq!(identity.launch_group.as_deref(), Some("pid-42"));
    assert_eq!(identity.title, None);
    fs::remove_dir_all(proc).unwrap();
  }

  #[test]
  fn capture_pid_falls_back_to_the_tilefire_launch_id() {
    let proc = fake_proc(7, b"xterm\0", b"TILEFIRE_LAUNCH_ID=old\0", None);
    let identity = capture_pid_in(&proc, 7);
    assert_eq!(identity.app_id, "xterm");
    assert_eq!(identity.launch_id.as_deref(), Some("old"));
    assert_eq!(identity.working_directory, None);
    fs::remove_dir_all(proc).unwrap();
  }

  #[test]
  fn capture_pid_of_a_missing_process_keeps_only_the_group() {
    let proc = fake_proc(1, b"", b"", None);
    let identity = capture_pid_in(&proc, 99);
    assert_eq!(identity.app_id, "");
    assert!(identity.command.is_empty());
    assert_eq!(identity.launch_id, None);
    assert_eq!(identity.launch_group.as_deref(), Some("pid-99"));
    fs::remove_dir_all(proc).unwrap();
  }

  #[test]
  fn capture_uses_the_peer_pid() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let pid = std::process::id() as i32;
    let by_stream = capture(&ours);
    assert_eq!(by_stream, capture_pid(pid));
    assert_eq!(by_stream.launch_group, Some(format!("pid-{pid}")));
    assert!(!by_stream.command.is_empty());
    drop(theirs);
  }
}
