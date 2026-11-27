//! Every frame the frontend presents, written to a directory for the GPU
//! harness (tools/gpu_test/lock_checks.py), which checks them one by one:
//! `SHINGLE_RECORD_FRAMES=DIR`.
//!
//! Each presented frame is drawn again with the same tiles into an
//! offscreen target of the output's logical size, read back and written
//! as a binary PPM, `DIR/OUTPUT-NNNNNN.ppm`, with a comment naming the
//! output, the frame number, the session-lock state and whether a lock
//! surface was drawn. A file is written under a temporary name and
//! renamed, so a reader never sees half of one. This costs a redraw and
//! a readback per frame and is for tests only.

use std::{
  fs,
  io::{self, Write},
  path::{Path, PathBuf},
};

/// The environment variable that names the directory.
pub const RECORD_VARIABLE: &str = "SHINGLE_RECORD_FRAMES";

/// What a recorded frame was drawn under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameInfo<'a> {
  /// The output's name.
  pub output: &'a str,
  /// The session-lock state, as `shingle::LockState`'s name in lower
  /// case.
  pub lock: String,
  /// Whether a lock surface was among what was drawn.
  pub lock_surface: bool,
  /// When it was presented, on `CLOCK_MONOTONIC`.
  pub time: std::time::Duration,
}

/// Writes presented frames as PPM files.
#[derive(Debug)]
pub struct FrameRecorder {
  directory: PathBuf,
  next: u64,
}

impl FrameRecorder {
  /// A recorder into `directory`, which must exist.
  pub fn new(directory: PathBuf) -> Self {
    Self { directory, next: 0 }
  }

  /// A recorder into the directory [`RECORD_VARIABLE`] names, if it is
  /// set and not empty; the directory is created.
  pub fn from_env() -> Option<Self> {
    let directory = std::env::var_os(RECORD_VARIABLE)
      .filter(|value| !value.is_empty())
      .map(PathBuf::from)?;
    if let Err(error) = fs::create_dir_all(&directory) {
      eprintln!(
        "shingle: not recording frames into {}: {error}",
        directory.display()
      );
      return None;
    }
    eprintln!(
      "shingle: recording every frame into {}",
      directory.display()
    );
    Some(Self::new(directory))
  }

  /// Writes one frame of `size`, tightly packed RGBA8 rows, drawn under
  /// `info`. Returns the path written.
  ///
  /// # Errors
  ///
  /// When the pixels do not fit `size` or the file cannot be written.
  pub fn record(
    &mut self,
    info: &FrameInfo<'_>,
    size: (u32, u32),
    rgba: &[u8],
  ) -> io::Result<PathBuf> {
    let number = self.next;
    self.next += 1;
    let name = format!("{}-{number:06}.ppm", info.output);
    let path = self.directory.join(&name);
    let temporary = self.directory.join(format!(".{name}.tmp"));
    fs::write(&temporary, encode(info, number, size, rgba)?)?;
    fs::rename(&temporary, &path)?;
    Ok(path)
  }

  /// The directory frames go to.
  pub fn directory(&self) -> &Path {
    &self.directory
  }
}

/// A binary PPM of `rgba` with the header comment the harness reads.
fn encode(
  info: &FrameInfo<'_>,
  number: u64,
  (width, height): (u32, u32),
  rgba: &[u8],
) -> io::Result<Vec<u8>> {
  let pixels = width as usize * height as usize;
  if rgba.len() != pixels * 4 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{} bytes for a {width}x{height} frame", rgba.len()),
    ));
  }
  let mut out = Vec::with_capacity(pixels * 3 + 128);
  write!(
    out,
    "P6\n# shingle-frame output={} frame={number} lock={} lock_surface={} \
     time={}.{:09}\n{width} {height}\n255\n",
    info.output,
    info.lock,
    u8::from(info.lock_surface),
    info.time.as_secs(),
    info.time.subsec_nanos()
  )?;
  for pixel in rgba.chunks_exact(4) {
    out.extend_from_slice(&pixel[..3]);
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn frames_are_ppm_files_with_their_lock_state() {
    let directory = tempfile::tempdir().unwrap();
    let mut recorder = FrameRecorder::new(directory.path().to_owned());
    let info = FrameInfo {
      output: "HEADLESS-1",
      lock: "pending".into(),
      lock_surface: false,
      time: std::time::Duration::from_millis(1500),
    };
    let rgba = [1, 2, 3, 255, 4, 5, 6, 255];
    let first = recorder.record(&info, (2, 1), &rgba).unwrap();
    let second = recorder.record(&info, (2, 1), &rgba).unwrap();
    assert!(first.ends_with("HEADLESS-1-000000.ppm"));
    assert!(second.ends_with("HEADLESS-1-000001.ppm"));
    let bytes = fs::read(&first).unwrap();
    let header = b"P6\n# shingle-frame output=HEADLESS-1 frame=0 \
                   lock=pending lock_surface=0 time=1.500000000\n2 1\n255\n";
    assert_eq!(&bytes[..header.len()], header);
    assert_eq!(&bytes[header.len()..], [1, 2, 3, 4, 5, 6]);
    // Nothing is left under a temporary name.
    let names = fs::read_dir(directory.path())
      .unwrap()
      .map(|entry| entry.unwrap().file_name().into_string().unwrap())
      .collect::<Vec<_>>();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(recorder.record(&info, (3, 1), &rgba).is_err());
  }
}
