//! `SHINGLE_TRACE_BUFFERS=1`: one log line per step of a client buffer's
//! path, so that a test harness can tell which path a client took
//! (docs/design/dmabuf.md, section 10): `shm upload N bytes`,
//! `dmabuf import`, `dmabuf cached`, `blocked implicit N µs` and
//! `release`.

use std::sync::OnceLock;

/// Whether `SHINGLE_TRACE_BUFFERS` is `1`, read once.
pub fn buffers_enabled() -> bool {
  static ENABLED: OnceLock<bool> = OnceLock::new();
  *ENABLED.get_or_init(|| {
    std::env::var_os("SHINGLE_TRACE_BUFFERS").is_some_and(|value| value == "1")
  })
}

/// Logs one buffer trace line, `shingle: buffer: MESSAGE`, if tracing is
/// on.
pub fn buffer(message: std::fmt::Arguments<'_>) {
  if buffers_enabled() {
    eprintln!("shingle: buffer: {message}");
  }
}
