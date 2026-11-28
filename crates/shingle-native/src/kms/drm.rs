//! The real [`KmsDevice`]: a DRM card through the `drm` crate.
//!
//! Compiled and checked on every host but only ever run on a machine
//! whose seat Shingle may take (docs/native-testing.md); unit tests never
//! construct it. The descriptor comes from the seat
//! ([`crate::session::Session::open_device`]), which makes Shingle the
//! card's DRM master while the seat is enabled.

use std::{
  collections::HashMap,
  io,
  num::NonZeroU32,
  os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use ::drm::{
  ClientCapability, Device as _, DriverCapability,
  buffer::{Buffer as _, DrmFourcc},
  control::{
    self, AtomicCommitFlags, Device as _, Event, atomic::AtomicModeReq,
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, plane, property,
  },
};

use super::{
  AtomicRequest, BlobId, BufferId, CommitFlags, Connection, ConnectorId,
  ConnectorInfo, CrtcId, CrtcInfo, DumbFramebuffer, FramebufferId, KmsDevice,
  MODIFIER_INVALID, Mode, Object, PageFlip, PlanarFramebuffer, PlaneId,
  PlaneInfo, Property, PropertyId, Resources, Subpixel,
};
use crate::scanout::allocator::{BufferAllocator, GbmAllocator};

/// A DRM card opened through the seat.
pub struct DrmDevice {
  fd: OwnedFd,
  /// The dumb buffer behind each framebuffer handed out.
  buffers: HashMap<u32, DumbBuffer>,
  /// Property names, and ranges of range properties, by id, read once.
  names: HashMap<u32, (String, Option<(u64, u64)>)>,
}

impl AsFd for DrmDevice {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.fd.as_fd()
  }
}

impl ::drm::Device for DrmDevice {}
impl control::Device for DrmDevice {}

fn invalid(what: &str) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {what} id"))
}

fn handle<T: From<NonZeroU32>>(id: u32, what: &str) -> io::Result<T> {
  NonZeroU32::new(id)
    .map(T::from)
    .ok_or_else(|| invalid(what))
}

fn from_drm_mode(mode: &control::Mode) -> Mode {
  let raw: drm_ffi::drm_mode_modeinfo = (*mode).into();
  let name = mode.name().to_string_lossy().into_owned();
  Mode {
    clock: raw.clock,
    hdisplay: raw.hdisplay,
    hsync_start: raw.hsync_start,
    hsync_end: raw.hsync_end,
    htotal: raw.htotal,
    hskew: raw.hskew,
    vdisplay: raw.vdisplay,
    vsync_start: raw.vsync_start,
    vsync_end: raw.vsync_end,
    vtotal: raw.vtotal,
    vscan: raw.vscan,
    vrefresh: raw.vrefresh,
    flags: raw.flags,
    kind: raw.type_,
    name,
  }
}

fn to_drm_mode(mode: &Mode) -> control::Mode {
  let mut name = [0; 32];
  for (slot, byte) in name.iter_mut().zip(mode.name.bytes().take(31)) {
    *slot = byte as std::ffi::c_char;
  }
  control::Mode::from(drm_ffi::drm_mode_modeinfo {
    clock: mode.clock,
    hdisplay: mode.hdisplay,
    hsync_start: mode.hsync_start,
    hsync_end: mode.hsync_end,
    htotal: mode.htotal,
    hskew: mode.hskew,
    vdisplay: mode.vdisplay,
    vsync_start: mode.vsync_start,
    vsync_end: mode.vsync_end,
    vtotal: mode.vtotal,
    vscan: mode.vscan,
    vrefresh: mode.vrefresh,
    flags: mode.flags,
    type_: mode.kind,
    name,
  })
}

fn subpixel(value: connector::SubPixel) -> Subpixel {
  match value {
    connector::SubPixel::HorizontalRgb => Subpixel::HorizontalRgb,
    connector::SubPixel::HorizontalBgr => Subpixel::HorizontalBgr,
    connector::SubPixel::VerticalRgb => Subpixel::VerticalRgb,
    connector::SubPixel::VerticalBgr => Subpixel::VerticalBgr,
    connector::SubPixel::None => Subpixel::None,
    _ => Subpixel::Unknown,
  }
}

impl DrmDevice {
  /// Wraps a card descriptor from the seat and makes it non-blocking, so
  /// reading events never waits.
  pub fn new(fd: OwnedFd) -> io::Result<Self> {
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(Self {
      fd,
      buffers: HashMap::new(),
      names: HashMap::new(),
    })
  }

  /// The name of property `id` and, for a range property, its range.
  fn property_name(
    &mut self,
    id: property::Handle,
  ) -> io::Result<(String, Option<(u64, u64)>)> {
    let raw = u32::from(id);
    if let Some(known) = self.names.get(&raw) {
      return Ok(known.clone());
    }
    let info = self.get_property(id)?;
    let name = info.name().to_string_lossy().into_owned();
    let range = match info.value_type() {
      property::ValueType::UnsignedRange(min, max) => Some((*min, *max)),
      _ => None,
    };
    self.names.insert(raw, (name.clone(), range));
    Ok((name, range))
  }

  fn crtc_list(
    &self,
    filter: control::CrtcListFilter,
  ) -> io::Result<Vec<CrtcId>> {
    let resources = self.resource_handles()?;
    Ok(
      resources
        .filter_crtcs(filter)
        .into_iter()
        .map(|crtc| CrtcId(crtc.into()))
        .collect(),
    )
  }
}

impl KmsDevice for DrmDevice {
  fn enable_atomic(&mut self) -> io::Result<()> {
    self.set_client_capability(ClientCapability::UniversalPlanes, true)?;
    self.set_client_capability(ClientCapability::Atomic, true)
  }

  fn monotonic_timestamps(&mut self) -> bool {
    self
      .get_driver_capability(DriverCapability::MonotonicTimestamp)
      .is_ok_and(|value| value == 1)
  }

  fn resources(&mut self) -> io::Result<Resources> {
    let handles = self.resource_handles()?;
    let planes = self.plane_handles()?;
    Ok(Resources {
      connectors: handles
        .connectors()
        .iter()
        .map(|id| ConnectorId((*id).into()))
        .collect(),
      crtcs: handles
        .crtcs()
        .iter()
        .map(|id| CrtcId((*id).into()))
        .collect(),
      planes: planes.into_iter().map(|id| PlaneId(id.into())).collect(),
    })
  }

  fn connector(&mut self, id: ConnectorId) -> io::Result<ConnectorInfo> {
    let handle: connector::Handle = handle(id.0, "connector")?;
    // A forced probe, as at startup and after hotplug.
    let info = self.get_connector(handle, true)?;
    let mut possible_crtcs = Vec::new();
    let mut current_crtc = None;
    for encoder in info.encoders() {
      let encoder = self.get_encoder(*encoder)?;
      for crtc in self.crtc_list(encoder.possible_crtcs())? {
        if !possible_crtcs.contains(&crtc) {
          possible_crtcs.push(crtc);
        }
      }
      if Some(encoder.handle()) == info.current_encoder() {
        current_crtc = encoder.crtc().map(|crtc| CrtcId(crtc.into()));
      }
    }
    let interface = info.interface();
    Ok(ConnectorInfo {
      id,
      name: format!("{}-{}", interface.as_str(), info.interface_id()),
      internal: matches!(
        interface,
        connector::Interface::EmbeddedDisplayPort
          | connector::Interface::LVDS
          | connector::Interface::DSI
      ),
      connection: match info.state() {
        connector::State::Connected => Connection::Connected,
        connector::State::Disconnected => Connection::Disconnected,
        connector::State::Unknown => Connection::Unknown,
      },
      modes: info.modes().iter().map(from_drm_mode).collect(),
      physical_size_mm: info.size().unwrap_or((0, 0)),
      subpixel: subpixel(info.subpixel()),
      current_crtc,
      possible_crtcs,
      edid: None,
    })
  }

  fn crtc(&mut self, id: CrtcId) -> io::Result<CrtcInfo> {
    let handle: crtc::Handle = handle(id.0, "CRTC")?;
    let info = self.get_crtc(handle)?;
    Ok(CrtcInfo {
      id,
      mode: info.mode().as_ref().map(from_drm_mode),
    })
  }

  fn plane(&mut self, id: PlaneId) -> io::Result<PlaneInfo> {
    let handle: plane::Handle = handle(id.0, "plane")?;
    let info = self.get_plane(handle)?;
    Ok(PlaneInfo {
      id,
      possible_crtcs: self.crtc_list(info.possible_crtcs())?,
      current_crtc: info.crtc().map(|crtc| CrtcId(crtc.into())),
      formats: info.formats().to_vec(),
    })
  }

  fn properties(&mut self, object: Object) -> io::Result<Vec<Property>> {
    let set = match object {
      Object::Connector(ConnectorId(id)) => {
        self.get_properties(handle::<connector::Handle>(id, "connector")?)?
      }
      Object::Crtc(CrtcId(id)) => {
        self.get_properties(handle::<crtc::Handle>(id, "CRTC")?)?
      }
      Object::Plane(PlaneId(id)) => {
        self.get_properties(handle::<plane::Handle>(id, "plane")?)?
      }
    };
    let (ids, values) = set.as_props_and_values();
    let mut properties = Vec::with_capacity(ids.len());
    for (id, value) in ids.iter().zip(values) {
      let (name, range) = self.property_name(*id)?;
      properties.push(Property {
        id: PropertyId((*id).into()),
        name,
        value: *value,
        range,
      });
    }
    Ok(properties)
  }

  fn blob(&mut self, id: u64) -> io::Result<Vec<u8>> {
    self.get_property_blob(id)
  }

  fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<BlobId> {
    let mode = to_drm_mode(mode);
    match self.create_property_blob(&mode)? {
      property::Value::Blob(id) => Ok(BlobId(id as u32)),
      _ => Err(io::Error::other("the kernel returned no blob")),
    }
  }

  fn destroy_blob(&mut self, blob: BlobId) -> io::Result<()> {
    self.destroy_property_blob(u64::from(blob.0))
  }

  fn commit(
    &mut self,
    request: &AtomicRequest,
    flags: CommitFlags,
  ) -> io::Result<()> {
    let mut atomic = AtomicModeReq::new();
    for (object, property, value) in request.entries() {
      let object =
        NonZeroU32::new(object.id()).ok_or_else(|| invalid("object"))?;
      let property: property::Handle = handle(property.0, "property")?;
      atomic.add_raw_property(object, property, *value);
    }
    self.atomic_commit(AtomicCommitFlags::from_bits_truncate(flags.0), atomic)
  }

  fn create_dumb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer> {
    let mut buffer =
      self.create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)?;
    let framebuffer =
      match control::Device::add_framebuffer(self, &buffer, 24, 32) {
        Ok(framebuffer) => framebuffer,
        Err(error) => {
          let _ = self.destroy_dumb_buffer(buffer);
          return Err(error);
        }
      };
    // Dumb buffers are zeroed by the kernel, but make sure they start
    // black whatever the driver does.
    if let Ok(mut mapping) = self.map_dumb_buffer(&mut buffer) {
      mapping.fill(0);
    }
    let id = u32::from(framebuffer);
    let fb = DumbFramebuffer {
      buffer: BufferId(buffer.handle().into()),
      framebuffer: FramebufferId(id),
      width,
      height,
      stride: buffer.pitch(),
    };
    self.buffers.insert(id, buffer);
    Ok(fb)
  }

  fn create_argb_framebuffer(
    &mut self,
    width: u32,
    height: u32,
  ) -> io::Result<DumbFramebuffer> {
    let mut buffer =
      self.create_dumb_buffer((width, height), DrmFourcc::Argb8888, 32)?;
    // Depth 32 at 32 bits per pixel is ARGB8888 for the legacy call.
    let framebuffer =
      match control::Device::add_framebuffer(self, &buffer, 32, 32) {
        Ok(framebuffer) => framebuffer,
        Err(error) => {
          let _ = self.destroy_dumb_buffer(buffer);
          return Err(error);
        }
      };
    if let Ok(mut mapping) = self.map_dumb_buffer(&mut buffer) {
      mapping.fill(0);
    }
    let id = u32::from(framebuffer);
    let fb = DumbFramebuffer {
      buffer: BufferId(buffer.handle().into()),
      framebuffer: FramebufferId(id),
      width,
      height,
      stride: buffer.pitch(),
    };
    self.buffers.insert(id, buffer);
    Ok(fb)
  }

  fn cursor_size(&mut self) -> (u32, u32) {
    let size = |capability| {
      self
        .get_driver_capability(capability)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(64)
    };
    (
      size(DriverCapability::CursorWidth),
      size(DriverCapability::CursorHeight),
    )
  }

  fn async_page_flips(&mut self) -> bool {
    self
      .get_driver_capability(DriverCapability::AtomicASyncPageFlip)
      .is_ok_and(|value| value == 1)
  }

  fn write_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
    write: &mut dyn FnMut(&mut [u8]),
  ) -> io::Result<()> {
    let mut dumb = *self
      .buffers
      .get(&buffer.framebuffer.0)
      .ok_or(io::ErrorKind::NotFound)?;
    let mut mapping = self.map_dumb_buffer(&mut dumb)?;
    write(&mut mapping);
    Ok(())
  }

  fn destroy_dumb_framebuffer(
    &mut self,
    buffer: &DumbFramebuffer,
  ) -> io::Result<()> {
    let dumb = self
      .buffers
      .remove(&buffer.framebuffer.0)
      .ok_or(io::ErrorKind::NotFound)?;
    let framebuffer: framebuffer::Handle =
      handle(buffer.framebuffer.0, "framebuffer")?;
    let removed = self.destroy_framebuffer(framebuffer);
    self.destroy_dumb_buffer(dumb)?;
    removed
  }

  fn add_framebuffer(
    &mut self,
    buffer: &PlanarFramebuffer,
  ) -> io::Result<FramebufferId> {
    // drm's add_planar_framebuffer asserts on an invalid modifier; refuse
    // it here instead.
    if buffer.modifier == MODIFIER_INVALID {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "a framebuffer needs an explicit modifier",
      ));
    }
    let info = drm_ffi::mode::add_fb2(
      self.fd.as_fd(),
      buffer.width,
      buffer.height,
      buffer.fourcc,
      &[buffer.buffer.0, 0, 0, 0],
      &[buffer.stride, 0, 0, 0],
      &[buffer.offset, 0, 0, 0],
      &[buffer.modifier, 0, 0, 0],
      drm_ffi::DRM_MODE_FB_MODIFIERS,
    )?;
    Ok(FramebufferId(info.fb_id))
  }

  fn remove_framebuffer(
    &mut self,
    framebuffer: FramebufferId,
  ) -> io::Result<()> {
    drm_ffi::mode::rm_fb(self.fd.as_fd(), framebuffer.0)
  }

  fn buffer_allocator(&mut self) -> io::Result<Box<dyn BufferAllocator>> {
    Ok(Box::new(GbmAllocator::new(self.fd.as_fd())?))
  }

  fn read_events(&mut self) -> io::Result<Vec<PageFlip>> {
    let events = match self.receive_events() {
      Ok(events) => events,
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
        return Ok(Vec::new());
      }
      Err(error) => return Err(error),
    };
    Ok(
      events
        .filter_map(|event| match event {
          Event::PageFlip(flip) => Some(PageFlip {
            crtc: CrtcId(flip.crtc.into()),
            sequence: flip.frame,
            time: flip.duration,
          }),
          _ => None,
        })
        .collect(),
    )
  }

  fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
    Some(self.fd.as_fd())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::kms::fake::mode;

  #[test]
  fn modes_survive_the_round_trip_through_the_kernel_struct() {
    let original = Mode {
      flags: 5,
      hskew: 3,
      vscan: 1,
      ..mode(1920, 1080, 60, true)
    };
    assert_eq!(from_drm_mode(&to_drm_mode(&original)), original);
    let long = Mode {
      name: "x".repeat(40),
      ..original
    };
    assert_eq!(from_drm_mode(&to_drm_mode(&long)).name, "x".repeat(31));
  }
}
