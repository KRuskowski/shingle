//! The clock's text: swaylock-effects' `timestr` and `datestr`, which
//! are `strftime` formats in the local time zone.

use std::{ffi::CString, time::SystemTime};

/// `format` applied to `time` in the local time zone, as `strftime`
/// writes it; empty for an empty format, and for one that cannot be
/// passed to C or writes more than 255 bytes.
pub fn format(format: &str, time: SystemTime) -> String {
  if format.is_empty() {
    return String::new();
  }
  let Ok(c_format) = CString::new(format) else {
    return String::new();
  };
  let seconds = time
    .duration_since(SystemTime::UNIX_EPOCH)
    .map_or(0, |duration| duration.as_secs()) as libc::time_t;
  // SAFETY: `tm` is plain data that localtime_r fills; both pointers are
  // valid for the call.
  let mut tm: libc::tm = unsafe { std::mem::zeroed() };
  // SAFETY: As above.
  if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
    return String::new();
  }
  let mut buffer = [0_u8; 256];
  // SAFETY: The buffer is valid for its length, the format is a
  // NUL-terminated string and `tm` was filled by localtime_r.
  let written = unsafe {
    libc::strftime(
      buffer.as_mut_ptr().cast(),
      buffer.len(),
      c_format.as_ptr(),
      &tm,
    )
  };
  String::from_utf8_lossy(&buffer[..written]).into_owned()
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;

  #[test]
  fn formats_follow_strftime() {
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 * 365);
    assert_eq!(format("", time), "");
    assert_eq!(format("%Y", time).len(), 4);
    assert_eq!(format("literal", time), "literal");
    assert_eq!(format("%H:%M:%S", time).len(), 8);
    assert_eq!(format("a\0b", time), "");
  }
}
