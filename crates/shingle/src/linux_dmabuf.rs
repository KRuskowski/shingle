//! `zwp_linux_dmabuf_v1` version 5: client DMA-BUFs as `wl_buffer`s
//! (docs/design/dmabuf.md, sections 2 and 3).
//!
//! The global exists only once a frontend that can import DMA-BUFs calls
//! [`crate::Compositor::enable_dmabuf`] with the formats its renderer
//! samples and a [`DmabufImporter`]. Every client then gets the same
//! feedback, one tranche on the renderer's render node, through a sealed
//! format table. `create` and `create_immed` check the parameters as the
//! protocol and wlroots do and then import the buffer for real through the
//! importer, so `failed` means the renderer cannot sample it. Buffers keep
//! their fds for their lifetime, so a frontend can import them again, for
//! example after a GPU loss.
//!
//! Implicit synchronisation: a commit whose DMA-BUF still has pending
//! write fences (its fd does not poll readable) waits in the surface's
//! commit queue until it does, as smithay's `DmabufBlocker` does it.

use std::{
  collections::HashSet,
  fmt,
  os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
  sync::{Arc, Mutex, PoisonError},
};

use calloop::LoopHandle;
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
  zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
  zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
  zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_server::{
  Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
  WEnum,
  backend::{ClientId, GlobalId, ObjectId},
  protocol::wl_buffer::WlBuffer,
};

use crate::{
  buffer::{Attachment, BufferContent},
  commit_queue::{Blocker, BlockerSource, FdWait},
  state::{BufferData, CompositorState, PendingBuffer, PendingSurface},
};

/// The version Shingle offers: feedback (4) and one modifier for all
/// planes (5), as wlroots and so Wayfire implement it.
pub(crate) const VERSION: u32 = 5;
/// The most planes a buffer may have, by protocol.
const MAX_PLANES: usize = 4;
/// `DRM_FORMAT_MOD_INVALID`.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// The `zwp_linux_buffer_params_v1` flags Shingle knows.
const KNOWN_FLAGS: u32 = 0b111;

/// One (format, modifier) pair the renderer can sample, with the largest
/// buffer it accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmabufFormat {
  /// The DRM fourcc.
  pub format: u32,
  /// The DRM modifier.
  pub modifier: u64,
  /// The largest width in pixels.
  pub max_width: u32,
  /// The largest height in pixels.
  pub max_height: u32,
}

/// What every client is told: the device to allocate on and the pairs to
/// allocate with. Shingle sends it as the default feedback and as every
/// surface's feedback, in a single tranche.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmabufFeedback {
  /// The render node's `dev_t`: the main device and the tranche's target.
  pub main_device: u64,
  /// The pairs, in the order of the format table.
  pub formats: Vec<DmabufFormat>,
}

/// One plane of a [`DmabufBuffer`].
pub struct DmabufPlane {
  /// The plane's DMA-BUF, owned for the buffer's lifetime.
  pub fd: OwnedFd,
  /// Byte offset of the plane.
  pub offset: u32,
  /// Bytes per row.
  pub stride: u32,
}

/// A client DMA-BUF that passed the protocol checks: what a frontend
/// imports. Every `wl_buffer` made from it, every snapshot showing it and
/// the importer's cache share it through an `Arc`.
pub struct DmabufBuffer {
  /// Unique for the compositor's lifetime; textures are cached by it.
  pub serial: u64,
  /// Width in pixels.
  pub width: u32,
  /// Height in pixels.
  pub height: u32,
  /// The DRM fourcc.
  pub format: u32,
  /// The DRM modifier, the same for every plane.
  pub modifier: u64,
  /// The planes, in plane order. Shingle accepts only single-plane
  /// formats, so there is one.
  pub planes: Vec<DmabufPlane>,
}

impl fmt::Debug for DmabufBuffer {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("DmabufBuffer")
      .field("serial", &self.serial)
      .field("width", &self.width)
      .field("height", &self.height)
      .field("format", &fourcc_name(self.format))
      .field("modifier", &format_args!("{:#018x}", self.modifier))
      .field("planes", &self.planes.len())
      .finish()
  }
}

/// Imports client DMA-BUFs for a frontend's renderer. Shingle calls it
/// from `create` and `create_immed`, before the client may use the
/// buffer, so that a buffer the renderer cannot sample gets `failed`
/// rather than a broken surface. The compositor crate does not depend on
/// a renderer; the frontend implements this.
pub trait DmabufImporter {
  /// Imports `buffer` and keeps it ready to draw, or says why not.
  fn import(&mut self, buffer: &Arc<DmabufBuffer>) -> Result<(), String>;
}

/// The readable name of a fourcc, such as `XR24`.
pub fn fourcc_name(format: u32) -> String {
  format
    .to_le_bytes()
    .iter()
    .map(|&byte| {
      if byte.is_ascii_graphic() {
        char::from(byte)
      } else {
        '?'
      }
    })
    .collect()
}

/// The format table: a sealed memfd of 16-byte entries, format, padding
/// and modifier, in [`DmabufFeedback::formats`] order.
struct FormatTable {
  fd: OwnedFd,
  size: u32,
}

impl FormatTable {
  fn new(formats: &[DmabufFormat]) -> Result<Self, String> {
    let bytes = table_bytes(formats);
    let fd = rustix::fs::memfd_create(
      "shingle-dmabuf-formats",
      rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|error| format!("memfd_create: {error}"))?;
    let mut written = 0;
    while written < bytes.len() {
      written += rustix::io::write(&fd, &bytes[written..])
        .map_err(|error| format!("writing the format table: {error}"))?;
    }
    rustix::fs::fcntl_add_seals(
      &fd,
      rustix::fs::SealFlags::SHRINK
        | rustix::fs::SealFlags::GROW
        | rustix::fs::SealFlags::WRITE
        | rustix::fs::SealFlags::SEAL,
    )
    .map_err(|error| format!("sealing the format table: {error}"))?;
    let size = u32::try_from(bytes.len())
      .map_err(|_| "the format table is too large".to_string())?;
    Ok(Self { fd, size })
  }

  /// A fresh read-only descriptor of the table for one client, as the
  /// protocol asks; a duplicate if `/proc` cannot reopen it.
  fn client_fd(&self) -> Option<OwnedFd> {
    let path = format!("/proc/self/fd/{}", self.fd.as_raw_fd());
    rustix::fs::open(
      path.as_str(),
      rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
      rustix::fs::Mode::empty(),
    )
    .ok()
    .or_else(|| self.fd.try_clone().ok())
  }
}

/// The table's bytes: per pair a native-endian `u32` format, 4 bytes of
/// padding and a `u64` modifier.
fn table_bytes(formats: &[DmabufFormat]) -> Vec<u8> {
  formats
    .iter()
    .flat_map(|format| {
      let mut entry = [0; 16];
      entry[..4].copy_from_slice(&format.format.to_ne_bytes());
      entry[8..].copy_from_slice(&format.modifier.to_ne_bytes());
      entry
    })
    .collect()
}

/// The compositor's DMA-BUF state, once a frontend enabled it.
pub(crate) struct DmabufState {
  feedback: DmabufFeedback,
  table: FormatTable,
  importer: Box<dyn DmabufImporter>,
  next_serial: u64,
  /// Clients whose failed import was logged already.
  logged: HashSet<ClientId>,
  _global: GlobalId,
}

impl DmabufState {
  /// The modifier limits of an advertised pair.
  fn advertised(&self, format: u32, modifier: u64) -> Option<&DmabufFormat> {
    self
      .feedback
      .formats
      .iter()
      .find(|entry| entry.format == format && entry.modifier == modifier)
  }
}

/// Creates the global, or replaces the feedback and importer when it
/// exists, as after a GPU loss.
pub(crate) fn enable(
  state: &mut CompositorState,
  handle: &DisplayHandle,
  loop_handle: &LoopHandle<'static, CompositorState>,
  feedback: DmabufFeedback,
  importer: Box<dyn DmabufImporter>,
) -> Result<(), String> {
  if feedback.formats.is_empty() {
    return Err("no format to advertise".into());
  }
  if feedback.formats.len() > usize::from(u16::MAX) {
    return Err("too many formats for the feedback's indices".into());
  }
  let table = FormatTable::new(&feedback.formats)?;
  if let Some(existing) = &mut state.dmabuf {
    existing.feedback = feedback;
    existing.table = table;
    existing.importer = importer;
    return Ok(());
  }
  let global =
    handle.create_global::<CompositorState, ZwpLinuxDmabufV1, ()>(VERSION, ());
  state.commit_queue.add_source(Box::new(ImplicitSync {
    handle: loop_handle.clone(),
  }));
  state.dmabuf = Some(DmabufState {
    feedback,
    table,
    importer,
    next_serial: 1,
    logged: HashSet::new(),
    _global: global,
  });
  Ok(())
}

impl GlobalDispatch<ZwpLinuxDmabufV1, ()> for CompositorState {
  crate::security_context::protocol_can_view!("zwp_linux_dmabuf_v1");

  fn bind(
    state: &mut Self,
    _handle: &DisplayHandle,
    _client: &Client,
    resource: New<ZwpLinuxDmabufV1>,
    _global_data: &(),
    data_init: &mut DataInit<'_, Self>,
  ) {
    let resource = data_init.init(resource, ());
    let Some(dmabuf) = &state.dmabuf else {
      return;
    };
    // Version 4 replaced these events with feedback.
    if resource.version() >= 4 {
      return;
    }
    let mut formats = Vec::new();
    for entry in &dmabuf.feedback.formats {
      if !formats.contains(&entry.format) {
        formats.push(entry.format);
        resource.format(entry.format);
      }
      if resource.version() >= 3 {
        resource.modifier(
          entry.format,
          (entry.modifier >> 32) as u32,
          entry.modifier as u32,
        );
      }
    }
  }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for CompositorState {
  fn request(
    state: &mut Self,
    _client: &Client,
    _resource: &ZwpLinuxDmabufV1,
    request: zwp_linux_dmabuf_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    match request {
      zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
        data_init.init(params_id, Mutex::new(Params::default()));
      }
      zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id } => {
        let feedback = data_init.init(id, ());
        send_feedback(state, &feedback);
      }
      zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, surface: _ } => {
        // Until DB-7 adds a scanout tranche, every surface gets the default
        // feedback and it is never re-sent.
        let feedback = data_init.init(id, ());
        send_feedback(state, &feedback);
      }
      zwp_linux_dmabuf_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for CompositorState {
  fn request(
    _state: &mut Self,
    _client: &Client,
    _resource: &ZwpLinuxDmabufFeedbackV1,
    _request: zwp_linux_dmabuf_feedback_v1::Request,
    _data: &(),
    _dhandle: &DisplayHandle,
    _data_init: &mut DataInit<'_, Self>,
  ) {
  }
}

/// Sends the one-tranche feedback: the table, the main device, the
/// tranche on the same device with every pair and no flags, and `done`.
fn send_feedback(state: &CompositorState, feedback: &ZwpLinuxDmabufFeedbackV1) {
  let Some(dmabuf) = &state.dmabuf else {
    return;
  };
  let Some(fd) = dmabuf.table.client_fd() else {
    eprintln!("shingle: no descriptor for the DMA-BUF format table");
    return;
  };
  feedback.format_table(fd.as_fd(), dmabuf.table.size);
  let device = dmabuf.feedback.main_device.to_ne_bytes().to_vec();
  feedback.main_device(device.clone());
  feedback.tranche_target_device(device);
  feedback.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty());
  feedback.tranche_formats(tranche_indices(dmabuf.feedback.formats.len()));
  feedback.tranche_done();
  feedback.done();
}

/// The `u16` indices `0..count` of the format table, native-endian.
fn tranche_indices(count: usize) -> Vec<u8> {
  (0..count)
    .flat_map(|index| (index as u16).to_ne_bytes())
    .collect()
}

/// A plane given with `add`.
struct ParamsPlane {
  fd: OwnedFd,
  offset: u32,
  stride: u32,
  modifier: u64,
}

/// The state of one `zwp_linux_buffer_params_v1`.
#[derive(Default)]
pub(crate) struct Params {
  planes: [Option<ParamsPlane>; MAX_PLANES],
  used: bool,
}

/// What `create` decided before importing.
#[derive(Debug, Eq, PartialEq)]
enum Refusal {
  /// A protocol error that ends the client.
  Fatal(zwp_linux_buffer_params_v1::Error, String),
  /// `failed`, or `invalid_wl_buffer` for `create_immed`.
  Failed(String),
}

/// The checks of `add`, as the protocol and wlroots make them: index,
/// duplicate, used params and, since version 5, one modifier for all.
fn check_add(
  params: &Params,
  version: u32,
  plane_idx: u32,
  modifier: u64,
) -> Result<(), (zwp_linux_buffer_params_v1::Error, String)> {
  use zwp_linux_buffer_params_v1::Error;
  if params.used {
    return Err((Error::AlreadyUsed, "the params were already used".into()));
  }
  let Some(slot) = params.planes.get(plane_idx as usize) else {
    return Err((
      Error::PlaneIdx,
      format!("plane index {plane_idx} is not below {MAX_PLANES}"),
    ));
  };
  if slot.is_some() {
    return Err((Error::PlaneSet, format!("plane {plane_idx} is set")));
  }
  if version >= 5
    && params
      .planes
      .iter()
      .flatten()
      .any(|plane| plane.modifier != modifier)
  {
    return Err((
      Error::InvalidFormat,
      "every plane must use the same modifier".into(),
    ));
  }
  Ok(())
}

/// The checks of `create` and `create_immed` before the import: complete
/// planes, flags, size, the advertised pair, the plane count and bounds,
/// in wlroots' order. The bounds are checked against the size
/// `lseek(SEEK_END)` gives, and skipped where that fails.
fn check_create(
  planes: &[ParamsPlane],
  version: u32,
  width: i32,
  height: i32,
  format: u32,
  flags: u32,
  advertised: Option<&DmabufFormat>,
) -> Result<(), Refusal> {
  use zwp_linux_buffer_params_v1::Error;
  if planes.is_empty() {
    return Err(Refusal::Fatal(
      Error::Incomplete,
      "no planes were added".into(),
    ));
  }
  if flags & !KNOWN_FLAGS != 0 {
    return Err(Refusal::Fatal(
      Error::InvalidFormat,
      format!("unknown flags {flags:#x}"),
    ));
  }
  if width <= 0 || height <= 0 {
    return Err(Refusal::Fatal(
      Error::InvalidDimensions,
      format!("{width}x{height} is not a valid size"),
    ));
  }
  let modifier = planes[0].modifier;
  let known = crate::linux_dmabuf::single_plane(format);
  if advertised.is_none() && version >= 4 {
    return Err(Refusal::Fatal(
      Error::InvalidFormat,
      format!(
        "format {} with modifier {modifier:#x} was not advertised",
        fourcc_name(format)
      ),
    ));
  }
  if known && planes.len() != 1 {
    return Err(Refusal::Fatal(
      Error::Incomplete,
      format!(
        "format {} has one plane, not {}",
        fourcc_name(format),
        planes.len()
      ),
    ));
  }
  let (width, height) = (width as u64, height as u64);
  // Only the first plane's height is known without the format's
  // subsampling; Shingle samples single-plane formats only.
  if let Some(size) = fd_size(planes[0].fd.as_fd()) {
    let plane = &planes[0];
    let end = u64::from(plane.stride)
      .checked_mul(height)
      .and_then(|bytes| bytes.checked_add(u64::from(plane.offset)));
    if u64::from(plane.offset) >= size || end.is_none_or(|end| end > size) {
      return Err(Refusal::Fatal(
        Error::OutOfBounds,
        format!("plane 0 extends past its {size}-byte DMA-BUF"),
      ));
    }
  }
  if flags != 0 {
    return Err(Refusal::Failed(format!(
      "flags {flags:#x} (y-invert or interlaced) are not drawn"
    )));
  }
  let Some(limits) = advertised.filter(|_| known) else {
    return Err(Refusal::Failed(format!(
      "format {} with modifier {modifier:#x} cannot be imported",
      fourcc_name(format)
    )));
  };
  if width > u64::from(limits.max_width)
    || height > u64::from(limits.max_height)
  {
    return Err(Refusal::Failed(format!(
      "{width}x{height} exceeds the modifier's {}x{}",
      limits.max_width, limits.max_height
    )));
  }
  Ok(())
}

/// Whether Shingle knows `format` as a single-plane format it can sample.
pub(crate) fn single_plane(format: u32) -> bool {
  const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
  }
  [
    fourcc(b"AR24"),
    fourcc(b"XR24"),
    fourcc(b"AB24"),
    fourcc(b"XB24"),
  ]
  .contains(&format)
}

/// A plane fd's size, as wlroots finds it.
fn fd_size(fd: BorrowedFd<'_>) -> Option<u64> {
  rustix::fs::seek(fd, rustix::fs::SeekFrom::End(0)).ok()
}

/// Which request asked for the buffer.
enum Create {
  /// `create`: answer with `created` or `failed`.
  Event,
  /// `create_immed`, with the client's new `wl_buffer`.
  Immediate(New<WlBuffer>),
}

impl Dispatch<ZwpLinuxBufferParamsV1, Mutex<Params>> for CompositorState {
  fn request(
    state: &mut Self,
    client: &Client,
    resource: &ZwpLinuxBufferParamsV1,
    request: zwp_linux_buffer_params_v1::Request,
    data: &Mutex<Params>,
    dhandle: &DisplayHandle,
    data_init: &mut DataInit<'_, Self>,
  ) {
    let mut params = data.lock().unwrap_or_else(PoisonError::into_inner);
    match request {
      zwp_linux_buffer_params_v1::Request::Add {
        fd,
        plane_idx,
        offset,
        stride,
        modifier_hi,
        modifier_lo,
      } => {
        let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
        match check_add(&params, resource.version(), plane_idx, modifier) {
          Ok(()) => {
            params.planes[plane_idx as usize] = Some(ParamsPlane {
              fd,
              offset,
              stride,
              modifier,
            });
          }
          Err((error, message)) => resource.post_error(error, message),
        }
      }
      zwp_linux_buffer_params_v1::Request::Create {
        width,
        height,
        format,
        flags,
      } => create(
        state,
        client,
        resource,
        &mut params,
        dhandle,
        data_init,
        (width, height, format, flag_bits(flags)),
        Create::Event,
      ),
      zwp_linux_buffer_params_v1::Request::CreateImmed {
        buffer_id,
        width,
        height,
        format,
        flags,
      } => create(
        state,
        client,
        resource,
        &mut params,
        dhandle,
        data_init,
        (width, height, format, flag_bits(flags)),
        Create::Immediate(buffer_id),
      ),
      zwp_linux_buffer_params_v1::Request::Destroy => {}
      _ => {}
    }
  }
}

fn flag_bits(flags: WEnum<zwp_linux_buffer_params_v1::Flags>) -> u32 {
  match flags {
    WEnum::Value(flags) => flags.bits(),
    WEnum::Unknown(bits) => bits,
  }
}

/// `create` and `create_immed`: checks, imports and answers.
#[allow(clippy::too_many_arguments)]
fn create(
  state: &mut CompositorState,
  client: &Client,
  resource: &ZwpLinuxBufferParamsV1,
  params: &mut Params,
  dhandle: &DisplayHandle,
  data_init: &mut DataInit<'_, CompositorState>,
  (width, height, format, flags): (i32, i32, u32, u32),
  request: Create,
) {
  use zwp_linux_buffer_params_v1::Error;
  if params.used {
    resource.post_error(Error::AlreadyUsed, "the params were already used");
    if let Create::Immediate(id) = request {
      data_init.init(id, BufferData);
    }
    return;
  }
  params.used = true;
  let count = params
    .planes
    .iter()
    .take_while(|plane| plane.is_some())
    .count();
  if params.planes[count..].iter().any(Option::is_some) {
    resource.post_error(Error::Incomplete, "the planes have a gap");
    if let Create::Immediate(id) = request {
      data_init.init(id, BufferData);
    }
    return;
  }
  let planes = params
    .planes
    .iter_mut()
    .take(count)
    .filter_map(Option::take)
    .collect::<Vec<_>>();
  let Some(dmabuf) = &mut state.dmabuf else {
    // The global exists only with DMA-BUF state, so this is not reached.
    match request {
      Create::Event => resource.failed(),
      Create::Immediate(id) => {
        data_init.init(id, BufferData);
        resource.post_error(Error::InvalidWlBuffer, "DMA-BUFs are off");
      }
    }
    return;
  };
  let modifier = planes.first().map_or(MOD_INVALID, |plane| plane.modifier);
  let checked = check_create(
    &planes,
    resource.version(),
    width,
    height,
    format,
    flags,
    dmabuf.advertised(format, modifier),
  );
  let outcome = checked.and_then(|()| {
    let serial = dmabuf.next_serial;
    dmabuf.next_serial += 1;
    let buffer = Arc::new(DmabufBuffer {
      serial,
      width: width as u32,
      height: height as u32,
      format,
      modifier,
      planes: planes
        .into_iter()
        .map(|plane| DmabufPlane {
          fd: plane.fd,
          offset: plane.offset,
          stride: plane.stride,
        })
        .collect(),
    });
    dmabuf
      .importer
      .import(&buffer)
      .map(|()| buffer)
      .map_err(Refusal::Failed)
  });
  match (outcome, request) {
    (Ok(buffer), Create::Event) => {
      let Ok(wl_buffer) = client
        .create_resource::<WlBuffer, BufferData, CompositorState>(
          dhandle, 1, BufferData,
        )
      else {
        resource.failed();
        return;
      };
      add_buffer(state, &wl_buffer, buffer);
      resource.created(&wl_buffer);
    }
    (Ok(buffer), Create::Immediate(id)) => {
      let wl_buffer = data_init.init(id, BufferData);
      add_buffer(state, &wl_buffer, buffer);
    }
    (Err(Refusal::Fatal(error, message)), request) => {
      if let Create::Immediate(id) = request {
        data_init.init(id, BufferData);
      }
      resource.post_error(error, message);
    }
    (Err(Refusal::Failed(message)), request) => {
      if let Some(dmabuf) = &mut state.dmabuf
        && dmabuf.logged.insert(client.id())
      {
        eprintln!("shingle: DMA-BUF import failed: {message}");
      }
      match request {
        Create::Event => resource.failed(),
        Create::Immediate(id) => {
          data_init.init(id, BufferData);
          resource.post_error(Error::InvalidWlBuffer, message);
        }
      }
    }
  }
}

fn add_buffer(
  state: &mut CompositorState,
  wl_buffer: &WlBuffer,
  buffer: Arc<DmabufBuffer>,
) {
  let attachment =
    Attachment::dmabuf(wl_buffer.clone(), buffer, &state.buffer_releases);
  state.buffers.insert(wl_buffer.id(), attachment);
}

/// Whether `fd` polls readable now: for a DMA-BUF, whether its write
/// fences have signalled.
fn readable(fd: BorrowedFd<'_>) -> bool {
  let mut fds = [rustix::event::PollFd::new(
    &fd,
    rustix::event::PollFlags::IN,
  )];
  let zero = rustix::event::Timespec {
    tv_sec: 0,
    tv_nsec: 0,
  };
  matches!(rustix::event::poll(&mut fds, Some(&zero)), Ok(count) if count > 0)
    && fds[0].revents().contains(rustix::event::PollFlags::IN)
}

/// Blocks commits of DMA-BUFs whose implicit write fences are pending.
struct ImplicitSync {
  handle: LoopHandle<'static, CompositorState>,
}

impl BlockerSource for ImplicitSync {
  fn blockers(
    &self,
    surface: &ObjectId,
    pending: &PendingSurface,
  ) -> Result<Vec<Box<dyn Blocker>>, String> {
    let PendingBuffer::Attach(attachment, _) = &pending.buffer else {
      return Ok(Vec::new());
    };
    let BufferContent::Dmabuf(buffer) = &attachment.content else {
      return Ok(Vec::new());
    };
    Ok(
      buffer
        .planes
        .iter()
        .filter(|plane| !readable(plane.fd.as_fd()))
        .filter_map(|plane| {
          fence_wait(&self.handle, plane.fd.as_fd(), surface, buffer.serial)
        })
        .map(|wait| Box::new(wait) as Box<dyn Blocker>)
        .collect(),
    )
  }
}

/// One plane's pending fence: a wait on a duplicate of its fd, or None,
/// so that the commit applies without waiting rather than never, if the
/// wait cannot be set up.
fn fence_wait(
  handle: &LoopHandle<'static, CompositorState>,
  fd: BorrowedFd<'_>,
  surface: &ObjectId,
  serial: u64,
) -> Option<FdWait> {
  let waited = fd
    .try_clone_to_owned()
    .and_then(|own| Ok((own, fd.try_clone_to_owned()?)))
    .map_err(|error| error.to_string())
    .and_then(|(own, source)| {
      FdWait::register(
        handle,
        source,
        surface,
        Box::new(move || readable(own.as_fd())),
        Box::new(move |waited| {
          crate::trace::buffer(format_args!(
            "blocked implicit {} µs dmabuf={serial}",
            waited.as_micros()
          ));
        }),
      )
    });
  match waited {
    Ok(wait) => Some(wait),
    Err(error) => {
      eprintln!("shingle: cannot wait for a DMA-BUF fence: {error}");
      None
    }
  }
}

#[cfg(test)]
mod tests;
