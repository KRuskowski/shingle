"""A raw Wayland wire-protocol client, just enough for WG-4's checks of
wayfire-shell-unstable-v2 (docs/design/wayfire-gaps.md).

It speaks the protocol on the socket itself, without libwayland, so that
the checks see exactly the messages Shingle sends: it binds globals,
creates objects by number, marshals requests and parses the events the
checks need. Events it has no signature for are kept with their raw
body. It can map an `xdg_toplevel` with a `wl_shm` buffer, answering
pings and configures, for `zwf_surface_v2.interactive_move`.

It runs only inside tools/output_test/run.py's namespace, against a
Shingle started there, and never against the host's session.
"""

import array
import mmap
import os
import select
import socket
import struct
import time

# Event signatures by interface and opcode: (name, argument types), with
# `u` uint, `i` int, `s` string, `o` object, `a` array.
EVENTS = {
    'wl_display': {0: ('error', 'ous'), 1: ('delete_id', 'u')},
    'wl_registry': {0: ('global', 'usu'), 1: ('global_remove', 'u')},
    'wl_callback': {0: ('done', 'u')},
    'wl_output': {4: ('name', 's')},
    'xdg_wm_base': {0: ('ping', 'u')},
    'xdg_surface': {0: ('configure', 'u')},
    'xdg_toplevel': {0: ('configure', 'iia'), 1: ('close', '')},
    'wl_buffer': {0: ('release', '')},
    'zwf_output_v2': {0: ('enter_fullscreen', ''), 1: ('leave_fullscreen', ''),
                      2: ('toggle_menu', '')},
    'zwf_hotspot_v2': {0: ('enter', ''), 1: ('leave', ''),
                       2: ('proximity_changed', 'u')},
}

# Request opcodes by interface and name.
REQUESTS = {
    'wl_display': {'sync': 0, 'get_registry': 1},
    'wl_registry': {'bind': 0},
    'wl_compositor': {'create_surface': 0},
    'wl_surface': {'attach': 1, 'damage': 2, 'commit': 6},
    'wl_shm': {'create_pool': 0},
    'wl_shm_pool': {'create_buffer': 0, 'destroy': 1},
    'xdg_wm_base': {'get_xdg_surface': 2, 'pong': 3},
    'xdg_surface': {'get_toplevel': 1, 'ack_configure': 4},
    'xdg_toplevel': {'set_title': 2, 'set_app_id': 3},
    'zwf_shell_manager_v2': {'get_wf_output': 0, 'get_wf_surface': 1},
    'zwf_output_v2': {'inhibit_output': 0, 'inhibit_output_done': 1,
                      'create_hotspot': 2, 'create_custom_hotspot': 3},
    'zwf_surface_v2': {'interactive_move': 0},
}

# `zwf_output_v2.hotspot_edge`.
EDGE_TOP, EDGE_BOTTOM, EDGE_LEFT, EDGE_RIGHT = 1, 2, 4, 8
# `wl_shm.format` XRGB8888.
XRGB8888 = 1

class NewId:
  """A `new_id` argument: the object a request creates, of `interface`."""

  def __init__(self, interface):
    self.interface = interface
    self.id = None

def pad(length):
  """`length` rounded up to a multiple of 4."""
  return (length + 3) & ~3

def encode_string(text):
  """A wire string: its length with the NUL, the bytes, padding."""
  data = text.encode() + b'\0'
  return struct.pack('=I', len(data)) + data + b'\0' * (pad(len(data))
                                                        - len(data))

def encode_message(sender, opcode, body):
  """A message of `sender` with `opcode` and the marshalled `body`."""
  size = 8 + len(body)
  return struct.pack('=II', sender, (size << 16) | opcode) + body

def decode_header(data):
  """(sender, opcode, size) of the message at the start of `data`."""
  sender, word = struct.unpack_from('=II', data)
  return sender, word & 0xffff, word >> 16

def decode_arguments(body, signature):
  """The arguments of an event body, by its signature."""
  values = []
  offset = 0
  for kind in signature:
    if kind in 'uo':
      values.append(struct.unpack_from('=I', body, offset)[0])
      offset += 4
    elif kind == 'i':
      values.append(struct.unpack_from('=i', body, offset)[0])
      offset += 4
    elif kind in 'sa':
      length = struct.unpack_from('=I', body, offset)[0]
      offset += 4
      raw = body[offset:offset + length]
      values.append(raw[:-1].decode(errors='replace') if kind == 's'
                    else raw)
      offset += pad(length)
    else:
      raise ValueError(f'unknown argument type {kind}')
  return values

class Client:
  """One connection to a Wayland socket, by path."""

  def __init__(self, path):
    self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    self.socket.connect(path)
    self.next_id = 2
    self.interfaces = {1: 'wl_display'}
    self.events = []
    self.buffer = b''
    self.globals = {}
    self.error = None
    self.done_callbacks = set()
    self.registry = self.create('wl_display', 1, 'get_registry',
                                NewId('wl_registry'))
    self.roundtrip()

  def close(self):
    """Closes the connection; Shingle then destroys every object."""
    self.socket.close()

  def create(self, interface, sender, request, *arguments, fds=()):
    """Sends `request` of object `sender`; returns the id of the object
    its `NewId` argument creates, if any."""
    created = None
    body = b''
    for argument in arguments:
      if isinstance(argument, NewId):
        argument.id = self.next_id
        self.next_id += 1
        self.interfaces[argument.id] = argument.interface
        created = argument.id
        body += struct.pack('=I', argument.id)
      elif isinstance(argument, str):
        body += encode_string(argument)
      elif isinstance(argument, tuple):
        # (`i`, value) for a signed argument.
        body += struct.pack('=i', argument[1])
      else:
        body += struct.pack('=I', argument)
    opcode = REQUESTS[interface][request]
    message = encode_message(sender, opcode, body)
    ancillary = []
    if fds:
      ancillary = [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                    array.array('i', fds))]
    self.socket.sendmsg([message], ancillary)
    return created

  def request(self, sender, request, *arguments, fds=()):
    """Sends `request` of object `sender`, of the interface it was created
    with."""
    return self.create(self.interfaces[sender], sender, request, *arguments,
                       fds=fds)

  def bind(self, interface, version):
    """Binds the first global of `interface` at `version`; returns its id."""
    name = next(name for name, (found, _) in self.globals.items()
                if found == interface)
    return self.bind_name(name, interface, version)

  def bind_name(self, name, interface, version):
    """Binds global `name` as `interface` at `version`; returns its id."""
    new = NewId(interface)
    self.request(self.registry, 'bind', name, interface, version, new)
    return new.id

  def has_global(self, interface):
    """Whether a global of `interface` is announced, and its version."""
    return next((version for found, version in self.globals.values()
                 if found == interface), None)

  def _read(self, timeout):
    """Reads what arrives within `timeout` seconds and parses it."""
    ready, _, _ = select.select([self.socket], [], [], timeout)
    if not ready:
      return False
    data, ancillary, _, _ = self.socket.recvmsg(
        65536, socket.CMSG_SPACE(16 * 4))
    for level, kind, payload in ancillary:
      if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
        fds = array.array('i')
        fds.frombytes(payload[:len(payload) - len(payload) % 4])
        for fd in fds:
          os.close(fd)
    if not data:
      raise ConnectionError('the compositor closed the connection')
    self.buffer += data
    while len(self.buffer) >= 8:
      sender, opcode, size = decode_header(self.buffer)
      if len(self.buffer) < size:
        break
      body = self.buffer[8:size]
      self.buffer = self.buffer[size:]
      self._event(sender, opcode, body)
    return True

  def _event(self, sender, opcode, body):
    interface = self.interfaces.get(sender, '?')
    name, signature = EVENTS.get(interface, {}).get(opcode, (None, None))
    arguments = (decode_arguments(body, signature) if signature is not None
                 else [body])
    if interface == 'wl_display' and name == 'error':
      self.error = tuple(arguments)
    elif interface == 'wl_registry' and name == 'global':
      self.globals[arguments[0]] = (arguments[1], arguments[2])
    elif interface == 'wl_callback' and name == 'done':
      self.done_callbacks.add(sender)
    elif interface == 'xdg_wm_base' and name == 'ping':
      self.request(sender, 'pong', arguments[0])
    elif interface == 'xdg_surface' and name == 'configure':
      self.request(sender, 'ack_configure', arguments[0])
    self.events.append((sender, interface, name or f'#{opcode}', arguments))

  def roundtrip(self, timeout=10.0):
    """Waits until Shingle answered every request sent so far."""
    callback = self.request(1, 'sync', NewId('wl_callback'))
    deadline = time.monotonic() + timeout
    while callback not in self.done_callbacks:
      if self.error:
        raise RuntimeError(f'protocol error {self.error}')
      left = deadline - time.monotonic()
      if left <= 0:
        raise TimeoutError('no reply to wl_display.sync')
      self._read(left)

  def pump(self, seconds):
    """Reads events for `seconds`."""
    deadline = time.monotonic() + seconds
    while (left := deadline - time.monotonic()) > 0:
      self._read(left)

  def events_of(self, object_id):
    """The names and arguments of the events `object_id` received."""
    return [(name, arguments) for sender, _, name, arguments in self.events
            if sender == object_id]

  def names_of(self, object_id):
    """The names of the events `object_id` received, in order."""
    return [name for name, _ in self.events_of(object_id)]

  def wait_for(self, object_id, name, timeout=10.0, after=0):
    """Waits for event `name` of `object_id`, beyond the first `after`
    events it received; returns whether it came."""
    deadline = time.monotonic() + timeout
    while name not in self.names_of(object_id)[after:]:
      left = deadline - time.monotonic()
      if left <= 0:
        return False
      self._read(min(left, 0.1))
    return True

  def outputs(self):
    """Binds every `wl_output` at version 4; returns their ids by name."""
    bound = {self.bind_name(name, 'wl_output', 4)
             for name, (interface, _) in list(self.globals.items())
             if interface == 'wl_output'}
    self.roundtrip()
    return {arguments[0]: sender for sender, interface, name, arguments
            in self.events if interface == 'wl_output' and name == 'name'
            and sender in bound}

  def toplevel(self, app_id, width, height):
    """Maps an `xdg_toplevel` with a white `width` by `height` buffer;
    returns its `wl_surface`."""
    compositor = self.bind('wl_compositor', 4)
    wm_base = self.bind('xdg_wm_base', 1)
    shm = self.bind('wl_shm', 1)
    surface = self.request(compositor, 'create_surface',
                           NewId('wl_surface'))
    xdg_surface = self.request(wm_base, 'get_xdg_surface',
                               NewId('xdg_surface'), surface)
    toplevel = self.request(xdg_surface, 'get_toplevel',
                            NewId('xdg_toplevel'))
    self.request(toplevel, 'set_app_id', app_id)
    self.request(toplevel, 'set_title', app_id)
    self.request(surface, 'commit')
    self.roundtrip()
    size = width * height * 4
    fd = os.memfd_create('wayland-wire-buffer', os.MFD_CLOEXEC)
    os.ftruncate(fd, size)
    with mmap.mmap(fd, size) as memory:
      memory.write(b'\xff' * size)
    pool = self.request(shm, 'create_pool', NewId('wl_shm_pool'), size,
                        fds=[fd])
    os.close(fd)
    buffer = self.request(pool, 'create_buffer', NewId('wl_buffer'), 0,
                          width, height, width * 4, XRGB8888)
    self.request(pool, 'destroy')
    self.request(surface, 'attach', buffer, ('i', 0), ('i', 0))
    self.request(surface, 'damage', ('i', 0), ('i', 0), ('i', width),
                 ('i', height))
    self.request(surface, 'commit')
    self.roundtrip()
    return surface
