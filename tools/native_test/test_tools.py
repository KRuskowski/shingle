"""Unit tests for the pure helpers of the native test tools.

They never create a device or open a socket, so they are safe on any host.
"""

import os
import tempfile
import unittest

import ipc
import uinput

class FramingTest(unittest.TestCase):
  """Wayfire IPC framing."""

  def test_round_trip(self):
    data = ipc.encode_message({'method': 'input/list-devices', 'data': {}})
    reply, rest = ipc.decode_message(data + b'xy')
    self.assertEqual(reply, {'method': 'input/list-devices', 'data': {}})
    self.assertEqual(rest, b'xy')

  def test_incomplete_frames_wait(self):
    data = ipc.encode_message({'a': 1})
    self.assertEqual(ipc.decode_message(data[:-1]), (None, data[:-1]))
    self.assertEqual(ipc.decode_message(data[:2]), (None, data[:2]))

  def test_usage_without_arguments(self):
    self.assertEqual(ipc.main([]), 2)

class GuardTest(unittest.TestCase):
  """The test-machine guard of uinput.py."""

  def test_marker_decides(self):
    with tempfile.TemporaryDirectory() as directory:
      marker = os.path.join(directory, 'marker')
      self.assertFalse(uinput.on_test_machine(marker))
      with open(marker, 'w', encoding='utf-8'):
        pass
      self.assertTrue(uinput.on_test_machine(marker))

  def test_no_device_without_the_marker(self):
    if uinput.on_test_machine():
      self.skipTest('this is the test machine')
    self.assertEqual(uinput.main(['key', 'KEY_A']), 1)

class ArgumentsTest(unittest.TestCase):
  """uinput.py's command line."""

  def test_commands_and_their_devices(self):
    chord = uinput.parse_arguments(
        ['key', 'KEY_LEFTCTRL', 'KEY_LEFTALT', 'KEY_F2'])
    self.assertEqual(chord.keys, ['KEY_LEFTCTRL', 'KEY_LEFTALT', 'KEY_F2'])
    self.assertEqual(uinput.device_kind(chord.command), 'keyboard')
    held = uinput.parse_arguments(['--settle', '2', 'key', 'KEY_A',
                                   '--hold', '15'])
    self.assertEqual((held.settle, held.hold), (2.0, 15.0))
    self.assertEqual(uinput.device_kind('wheel'), 'mouse')
    self.assertEqual(uinput.device_kind('touch'), 'touch')
    device = uinput.parse_arguments(['device', 'touch', '--seconds', '3'])
    self.assertEqual((device.kind, device.seconds), ('touch', 3.0))

  def test_touch_positions(self):
    self.assertEqual(uinput.touch_position(0.0), 0)
    self.assertEqual(uinput.touch_position(1.0), uinput.TOUCH_MAX)
    self.assertEqual(uinput.touch_position(0.5), 2048)
    with self.assertRaises(ValueError):
      uinput.touch_position(1.5)

if __name__ == '__main__':
  unittest.main()
