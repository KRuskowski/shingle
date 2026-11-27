"""Raw-protocol clients of ext-image-copy-capture and ext-data-control.

They extend tools/output_test/wayland_wire.py, the Python client that
speaks the Wayland wire protocol itself, with the requests and events of
ext-image-capture-source-v1, ext-image-copy-capture-v1,
ext-foreign-toplevel-list-v1 and ext-data-control-v1, so that the checks
in screencast_checks.py see exactly what Shingle sends; no packaged tool
here speaks these protocols yet (grim 1.4 and wl-clipboard 2.2 use the
wlr ones). The client keeps the descriptors that events carry, which
ext_data_control_source_v1.send needs.

Only screencast_checks.py imports this, inside run.py's namespace, against
a Shingle started there.
"""

import array
import importlib.util
import mmap
import os
import socket

def _load_wayland_wire():
  """tools/output_test/wayland_wire.py, which runs nothing on import."""
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..',
                      'output_test', 'wayland_wire.py')
  spec = importlib.util.spec_from_file_location('wayland_wire', path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

wire = _load_wayland_wire()
NewId = wire.NewId

# Event signatures, as in wayland_wire.EVENTS; fds take no bytes, so `send`
# lists only its string.
EVENTS = {
    'wl_pointer': {0: ('enter', 'uoii'), 1: ('leave', 'uo')},
    'ext_image_copy_capture_session_v1': {
        0: ('buffer_size', 'uu'), 1: ('shm_format', 'u'),
        2: ('dmabuf_device', 'a'), 3: ('dmabuf_format', 'ua'), 4: ('done', ''),
        5: ('stopped', '')},
    'ext_image_copy_capture_frame_v1': {
        0: ('transform', 'u'), 1: ('damage', 'iiii'),
        2: ('presentation_time', 'uuu'), 3: ('ready', ''), 4: ('failed', 'u')},
    'ext_image_copy_capture_cursor_session_v1': {
        0: ('enter', ''), 1: ('leave', ''), 2: ('position', 'ii'),
        3: ('hotspot', 'ii')},
    'ext_foreign_toplevel_list_v1': {0: ('toplevel', 'n'), 1: ('finished', '')},
    'ext_foreign_toplevel_handle_v1': {
        0: ('closed', ''), 1: ('done', ''), 2: ('title', 's'),
        3: ('app_id', 's'), 4: ('identifier', 's')},
    'ext_data_control_device_v1': {
        0: ('data_offer', 'n'), 1: ('selection', 'o'), 2: ('finished', ''),
        3: ('primary_selection', 'o')},
    'ext_data_control_source_v1': {0: ('send', 's'), 1: ('cancelled', '')},
    'ext_data_control_offer_v1': {0: ('offer', 's')},
}

NEW_OBJECTS = {
    ('ext_foreign_toplevel_list_v1',
     'toplevel'): 'ext_foreign_toplevel_handle_v1',
    ('ext_data_control_device_v1', 'data_offer'): 'ext_data_control_offer_v1',
}

REQUESTS = {
    'wl_seat': {'get_pointer': 0},
    'ext_output_image_capture_source_manager_v1': {
        'create_source': 0, 'destroy': 1},
    'ext_foreign_toplevel_image_capture_source_manager_v1': {
        'create_source': 0, 'destroy': 1},
    'ext_image_capture_source_v1': {'destroy': 0},
    'ext_image_copy_capture_manager_v1': {
        'create_session': 0, 'create_pointer_cursor_session': 1,
        'destroy': 2},
    'ext_image_copy_capture_session_v1': {'create_frame': 0, 'destroy': 1},
    'ext_image_copy_capture_frame_v1': {
        'destroy': 0, 'attach_buffer': 1, 'damage_buffer': 2, 'capture': 3},
    'ext_image_copy_capture_cursor_session_v1': {
        'destroy': 0, 'get_capture_session': 1},
    'ext_foreign_toplevel_list_v1': {'stop': 0, 'destroy': 1},
    'ext_data_control_manager_v1': {
        'create_data_source': 0, 'get_data_device': 1, 'destroy': 2},
    'ext_data_control_device_v1': {
        'set_selection': 0, 'destroy': 1, 'set_primary_selection': 2},
    'ext_data_control_source_v1': {'offer': 0, 'destroy': 1},
    'ext_data_control_offer_v1': {'receive': 0, 'destroy': 1},
}

for interface, events in EVENTS.items():
  wire.EVENTS.setdefault(interface, {}).update(events)
wire.NEW_OBJECTS.update(NEW_OBJECTS)
for interface, requests in REQUESTS.items():
  wire.REQUESTS.setdefault(interface, {}).update(requests)

# `ext_image_copy_capture_manager_v1.options` paint_cursors.
PAINT_CURSORS = 1
# `wl_shm.format` ARGB8888.
ARGB8888 = 0

def shm_buffer(client, width, height, fill=b'\0\0\0\0',
               format_code=ARGB8888):
  """A `width` by `height` shm buffer filled with `fill` bytes per pixel;
  returns its id and its memory, an mmap the caller reads and closes."""
  shm = client.bind('wl_shm', 1)
  size = width * height * 4
  fd = os.memfd_create('capture-wire-buffer', os.MFD_CLOEXEC)
  os.ftruncate(fd, size)
  memory = mmap.mmap(fd, size)
  memory.write(fill * (width * height))
  pool = client.request(shm, 'create_pool', NewId('wl_shm_pool'), size,
                        fds=[fd])
  os.close(fd)
  buffer = client.request(pool, 'create_buffer', NewId('wl_buffer'), 0,
                          width, height, width * 4, format_code)
  client.request(pool, 'destroy')
  return buffer, memory

class Client(wire.Client):
  """wayland_wire.Client that keeps the descriptors events carry, in
  arrival order, in `fds`."""

  def __init__(self, path):
    self.fds = []
    super().__init__(path)

  def _read(self, timeout):
    import select
    ready, _, _ = select.select([self.socket], [], [], timeout)
    if not ready:
      return False
    data, ancillary, _, _ = self.socket.recvmsg(
        65536, socket.CMSG_SPACE(16 * 4))
    for level, kind, payload in ancillary:
      if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
        fds = array.array('i')
        fds.frombytes(payload[:len(payload) - len(payload) % 4])
        self.fds.extend(fds)
    if not data:
      raise ConnectionError('the compositor closed the connection')
    self.buffer += data
    while len(self.buffer) >= 8:
      sender, opcode, size = wire.decode_header(self.buffer)
      if len(self.buffer) < size:
        break
      body = self.buffer[8:size]
      self.buffer = self.buffer[size:]
      self._event(sender, opcode, body)
    return True

  def close(self):
    """Closes the connection and the descriptors still kept."""
    for fd in self.fds:
      os.close(fd)
    self.fds = []
    super().close()

  def first_output(self):
    """Binds the first `wl_output`; returns its id."""
    return self.bind('wl_output', 4)

  def toplevels(self):
    """Binds ext-foreign-toplevel-list; returns {app_id: handle id}."""
    self.bind('ext_foreign_toplevel_list_v1', 1)
    self.roundtrip()
    self.roundtrip()
    return {arguments[0]: sender for sender, interface, name, arguments
            in self.events
            if interface == 'ext_foreign_toplevel_handle_v1'
            and name == 'app_id'}

  def session(self, source, options=0):
    """Creates a capture session of `source`; returns it after the first
    batch of constraints, or with `stopped`."""
    manager = self.bind('ext_image_copy_capture_manager_v1', 1)
    session = self.request(manager, 'create_session',
                           NewId('ext_image_copy_capture_session_v1'),
                           source, options)
    self.roundtrip()
    return session

  def output_source(self, output):
    """An image capture source of `output`."""
    manager = self.bind('ext_output_image_capture_source_manager_v1', 1)
    return self.request(manager, 'create_source',
                        NewId('ext_image_capture_source_v1'), output)

  def toplevel_source(self, handle):
    """An image capture source of the toplevel `handle`."""
    manager = self.bind(
        'ext_foreign_toplevel_image_capture_source_manager_v1', 1)
    return self.request(manager, 'create_source',
                        NewId('ext_image_capture_source_v1'), handle)

  def buffer_size(self, session):
    """The last `buffer_size` of `session`, or None."""
    sizes = [tuple(arguments) for name, arguments in self.events_of(session)
             if name == 'buffer_size']
    return sizes[-1] if sizes else None

  def start_capture(self, session, size):
    """Asks for one frame of `session` in a new `size` ARGB8888 buffer;
    returns (frame, buffer, memory)."""
    buffer, memory = shm_buffer(self, *size, fill=b'\x09\x09\x09\x09')
    frame = self.request(session, 'create_frame',
                         NewId('ext_image_copy_capture_frame_v1'))
    self.request(frame, 'attach_buffer', buffer)
    self.request(frame, 'damage_buffer', ('i', 0), ('i', 0), ('i', size[0]),
                 ('i', size[1]))
    self.request(frame, 'capture')
    return frame, buffer, memory

  def finish_capture(self, started, wait=10.0):
    """Waits up to `wait` seconds for a frame `start_capture` asked for;
    returns its event names and, once it is ready or failed, the buffer's
    bytes, destroying the frame and the buffer. Until then the bytes are
    None and both are kept, so that it can be called again."""
    frame, buffer, memory = started
    finished = (self.wait_for(frame, 'ready', timeout=wait)
                or 'failed' in self.names_of(frame))
    if not finished:
      return self.names_of(frame), None
    data = bytes(memory)
    memory.close()
    self.request(frame, 'destroy')
    self.request(buffer, 'destroy')
    return self.names_of(frame), data

  def capture(self, session, size, wait=10.0):
    """Captures one frame of `session` into a new `size` ARGB8888 buffer;
    returns (frame, the frame's event names, the buffer's bytes), the
    bytes None if it was neither ready nor failed within `wait`."""
    started = self.start_capture(session, size)
    names, data = self.finish_capture(started, wait)
    return started[0], names, data

def dmabuf_formats(client, session):
  """[(fourcc, [modifier])] of `session`'s `dmabuf_format` events."""
  import struct
  formats = []
  for name, arguments in client.events_of(session):
    if name != 'dmabuf_format':
      continue
    fourcc, modifiers = arguments
    count = len(modifiers) // 8
    formats.append((fourcc, list(struct.unpack(f'={count}Q', modifiers))))
  return formats

def fourcc_name(fourcc):
  """The four characters of a DRM fourcc."""
  return fourcc.to_bytes(4, 'little').decode(errors='replace')

def pixel(data, width, x, y):
  """(r, g, b) of pixel (`x`, `y`) of ARGB8888 bytes `data`."""
  offset = (y * width + x) * 4
  blue, green, red = data[offset:offset + 3]
  return red, green, blue

# zwp_linux_dmabuf_v1 and its params, for capture buffers made with GBM.
wire.REQUESTS.setdefault('zwp_linux_dmabuf_v1', {}).update(
    {'destroy': 0, 'create_params': 1})
wire.REQUESTS.setdefault('zwp_linux_buffer_params_v1', {}).update(
    {'destroy': 0, 'add': 1, 'create': 2, 'create_immed': 3})
wire.REQUESTS.setdefault('wl_buffer', {}).update({'destroy': 0})
wire.REQUESTS.setdefault('wl_seat', {}).update({'get_pointer': 0})

GBM_BO_USE_RENDERING = 1 << 2
GBM_BO_TRANSFER_READ = 1 << 0

class Gbm:
  """libgbm through ctypes, on the render node: allocates buffers with a
  modifier, as xdg-desktop-portal-wlr does, and maps them for reading."""

  def __init__(self, node='/dev/dri/renderD128'):
    import ctypes
    self.ctypes = ctypes
    library = ctypes.CDLL('libgbm.so.1')
    self.library = library
    library.gbm_create_device.restype = ctypes.c_void_p
    library.gbm_create_device.argtypes = [ctypes.c_int]
    library.gbm_device_destroy.argtypes = [ctypes.c_void_p]
    library.gbm_bo_create_with_modifiers2.restype = ctypes.c_void_p
    library.gbm_bo_create_with_modifiers2.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32,
        ctypes.POINTER(ctypes.c_uint64), ctypes.c_uint, ctypes.c_uint32]
    for name, restype in (('gbm_bo_get_fd', ctypes.c_int),
                          ('gbm_bo_get_stride', ctypes.c_uint32),
                          ('gbm_bo_get_offset', ctypes.c_uint32),
                          ('gbm_bo_get_modifier', ctypes.c_uint64)):
      getattr(library, name).restype = restype
    library.gbm_bo_get_fd.argtypes = [ctypes.c_void_p]
    library.gbm_bo_get_stride.argtypes = [ctypes.c_void_p]
    library.gbm_bo_get_modifier.argtypes = [ctypes.c_void_p]
    library.gbm_bo_get_offset.argtypes = [ctypes.c_void_p, ctypes.c_int]
    library.gbm_bo_map.restype = ctypes.c_void_p
    library.gbm_bo_map.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32,
        ctypes.c_uint32, ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32),
        ctypes.POINTER(ctypes.c_void_p)]
    library.gbm_bo_unmap.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    library.gbm_bo_destroy.argtypes = [ctypes.c_void_p]
    self.fd = os.open(node, os.O_RDWR | os.O_CLOEXEC)
    self.device = library.gbm_create_device(self.fd)
    if not self.device:
      os.close(self.fd)
      raise RuntimeError('gbm_create_device failed')

  def create(self, width, height, fourcc, modifiers):
    """A buffer object with one of `modifiers`, or None."""
    array = (self.ctypes.c_uint64 * len(modifiers))(*modifiers)
    return self.library.gbm_bo_create_with_modifiers2(
        self.device, width, height, fourcc, array, len(modifiers),
        GBM_BO_USE_RENDERING) or None

  def plane(self, bo):
    """(fd, offset, stride, modifier) of `bo`'s plane; the fd is new."""
    return (self.library.gbm_bo_get_fd(bo),
            self.library.gbm_bo_get_offset(bo, 0),
            self.library.gbm_bo_get_stride(bo),
            self.library.gbm_bo_get_modifier(bo))

  def read(self, bo, width, height):
    """(bytes, stride) of `bo` mapped for reading, or None."""
    stride = self.ctypes.c_uint32()
    data = self.ctypes.c_void_p()
    pointer = self.library.gbm_bo_map(bo, 0, 0, width, height,
                                      GBM_BO_TRANSFER_READ,
                                      self.ctypes.byref(stride),
                                      self.ctypes.byref(data))
    if not pointer:
      return None
    try:
      return self.ctypes.string_at(pointer, stride.value * height), stride.value
    finally:
      self.library.gbm_bo_unmap(bo, data)

  def destroy(self, bo):
    """Frees `bo`."""
    self.library.gbm_bo_destroy(bo)

  def close(self):
    """Frees the device."""
    self.library.gbm_device_destroy(self.device)
    os.close(self.fd)

def dmabuf_buffer(client, plane, width, height, fourcc):
  """A `wl_buffer` of one DMA-BUF `plane` (fd, offset, stride, modifier)
  through zwp_linux_dmabuf_v1.create_immed."""
  fd, offset, stride, modifier = plane
  dmabuf = client.bind('zwp_linux_dmabuf_v1', 4)
  params = client.request(dmabuf, 'create_params',
                          NewId('zwp_linux_buffer_params_v1'))
  client.request(params, 'add', 0, offset, stride, modifier >> 32,
                 modifier & 0xffffffff, fds=[fd])
  buffer = client.request(params, 'create_immed', NewId('wl_buffer'),
                          ('i', width), ('i', height), fourcc, 0)
  client.request(params, 'destroy')
  return buffer
