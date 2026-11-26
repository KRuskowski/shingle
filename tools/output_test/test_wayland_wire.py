"""Tests for wayland_wire.py and the pure helpers of
wayfire_shell_checks.py."""

import os
import socket
import struct
import threading
import unittest

import wayfire_shell_checks
import wayland_wire

def message(sender, opcode, body=b''):
  """An event as Shingle sends it."""
  return wayland_wire.encode_message(sender, opcode, body)

class MarshallingTest(unittest.TestCase):
  """Strings, headers and event arguments."""

  def test_strings_carry_their_nul_and_padding(self):
    self.assertEqual(wayland_wire.encode_string('abc'),
                     struct.pack('=I', 4) + b'abc\0')
    self.assertEqual(wayland_wire.encode_string('wl_shm'),
                     struct.pack('=I', 7) + b'wl_shm\0\0')
    self.assertEqual(len(wayland_wire.encode_string('')), 8)

  def test_headers_hold_size_and_opcode(self):
    data = message(3, 2, b'\1\0\0\0')
    self.assertEqual(wayland_wire.decode_header(data), (3, 2, 12))

  def test_event_arguments_decode_by_signature(self):
    body = (struct.pack('=I', 7) + wayland_wire.encode_string('wl_output')
            + struct.pack('=I', 4))
    self.assertEqual(wayland_wire.decode_arguments(body, 'usu'),
                     [7, 'wl_output', 4])
    body = struct.pack('=ii', -5, 3) + struct.pack('=I', 2) + b'\1\2\0\0'
    self.assertEqual(wayland_wire.decode_arguments(body, 'iia'),
                     [-5, 3, b'\1\2'])

class ClientTest(unittest.TestCase):
  """A client against a scripted server on a socket pair."""

  def setUp(self):
    self.directory = os.path.join(os.environ.get('TMPDIR', '/tmp'),
                                  f'wayland-wire-test-{os.getpid()}')
    os.makedirs(self.directory, exist_ok=True)
    self.path = os.path.join(self.directory, 'socket')
    if os.path.exists(self.path):
      os.unlink(self.path)
    self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    self.server.bind(self.path)
    self.server.listen(1)
    self.requests = []

  def tearDown(self):
    self.server.close()
    os.unlink(self.path)
    os.rmdir(self.directory)

  def serve(self, events):
    """Answers the registry's first sync with `events` and every sync
    with `done`, recording the requests."""

    pending = list(events)

    def run():
      connection, _ = self.server.accept()
      buffer = b''
      with connection:
        while True:
          data = connection.recv(4096)
          if not data:
            return
          buffer += data
          while len(buffer) >= 8:
            sender, opcode, size = wayland_wire.decode_header(buffer)
            if len(buffer) < size:
              break
            body, buffer = buffer[8:size], buffer[size:]
            self.requests.append((sender, opcode, body))
            if (sender, opcode) == (1, 0):
              callback = struct.unpack('=I', body)[0]
              connection.sendall(b''.join(pending)
                                 + message(callback, 0, b'\0' * 4))
              pending.clear()
    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread

  def test_globals_events_and_binds(self):
    shell = (struct.pack('=I', 9)
             + wayland_wire.encode_string('zwf_shell_manager_v2')
             + struct.pack('=I', 3))
    thread = self.serve([message(2, 0, shell)])
    client = wayland_wire.Client(self.path)
    self.assertEqual(client.has_global('zwf_shell_manager_v2'), 3)
    self.assertIsNone(client.has_global('wl_seat'))
    manager = client.bind('zwf_shell_manager_v2', 3)
    output = client.request(manager, 'get_wf_output', 5,
                            wayland_wire.NewId('zwf_output_v2'))
    hotspot = client.request(output, 'create_hotspot',
                             wayland_wire.EDGE_TOP, 10, 500,
                             wayland_wire.NewId('zwf_hotspot_v2'))
    client.roundtrip()
    # The bind names the global, the interface, the version and the id.
    bind = self.requests[2]
    self.assertEqual(bind[:2], (2, 0))
    self.assertEqual(
        wayland_wire.decode_arguments(bind[2], 'usuu'),
        [9, 'zwf_shell_manager_v2', 3, manager])
    self.assertEqual(self.requests[3],
                     (manager, 0, struct.pack('=II', 5, output)))
    self.assertEqual(self.requests[4],
                     (output, 2, struct.pack('=IIII', 1, 10, 500, hotspot)))
    client._event(hotspot, 0, b'')
    client._event(hotspot, 2, struct.pack('=I', 4))
    client._event(output, 2, b'')
    self.assertEqual(client.events_of(hotspot),
                     [('enter', []), ('proximity_changed', [4])])
    self.assertEqual(client.names_of(output), ['toggle_menu'])
    client.close()
    thread.join(5)

class HelperTest(unittest.TestCase):
  """wayfire_shell_checks' parsing."""

  def test_global_versions_are_read_from_wayland_info(self):
    info = ("interface: 'wl_compositor',                version:  6, name:  1\n"
            "interface: 'zwf_shell_manager_v2',         version:  3, "
            "name: 40\n")
    self.assertEqual(
        wayfire_shell_checks.global_version(info, 'zwf_shell_manager_v2'), 3)
    self.assertEqual(
        wayfire_shell_checks.global_version(info, 'wl_compositor'), 6)
    self.assertIsNone(wayfire_shell_checks.global_version(info, 'wl_seat'))

if __name__ == '__main__':
  unittest.main()
