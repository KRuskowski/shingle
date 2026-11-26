"""Tests for the pure helpers of tools/gpu_test/run.py."""

import os
import signal
import subprocess
import unittest

import run

class AllowedDevicesTest(unittest.TestCase):
  """Which nodes the private /dev holds."""

  def test_no_card_input_or_tty(self):
    for modeset in (False, True):
      devices = run.allowed_devices(modeset)
      self.assertIn('dri/renderD128', devices)
      self.assertIn('udmabuf', devices)
      for device in devices:
        self.assertFalse(device.startswith(('dri/card', 'input', 'tty')))

  def test_modeset_only_when_asked(self):
    self.assertNotIn('nvidia-modeset', run.allowed_devices(False))
    self.assertIn('nvidia-modeset', run.allowed_devices(True))

  def test_missing_nodes_are_skipped(self):
    present = {'/dev/null', '/dev/dri/renderD128'}
    self.assertEqual(
        run.bindable(('null', 'dri/renderD128', 'nvidia0'),
                     lambda path: path in present),
        ['null', 'dri/renderD128'])

class AllowedPathTest(unittest.TestCase):
  """Which open files the device check accepts."""

  devices = ('null', 'dri/renderD128', 'nvidia0')

  def test_bound_and_private_nodes(self):
    for path in ('/dev/null', '/dev/dri/renderD128', '/dev/nvidia0',
                 '/dev/pts/0', '/dev/ptmx', '/dev/shm/x', '/home/a',
                 'pipe:[12]', 'anon_inode:[eventfd]', '/dev/dri'):
      self.assertTrue(run.is_allowed_path(path, self.devices), path)

  def test_card_input_tty_and_unbound(self):
    for path in ('/dev/dri/card0', '/dev/input/event3', '/dev/tty1',
                 '/dev/nvidia-modeset', '/dev/udmabuf', '/dev/input'):
      self.assertFalse(run.is_allowed_path(path, self.devices), path)

  def test_disallowed_pairs(self):
    opened = [('vkcube', '/dev/nvidia0'), ('vkcube', '/dev/dri/card0'),
              ('vkcube', '/dev/dri/card0'), ('a', '/dev/tty0')]
    self.assertEqual(run.disallowed(opened, self.devices),
                     [('a', '/dev/tty0'), ('vkcube', '/dev/dri/card0')])

  def test_unexpected_dev_entries(self):
    entries = ['null', 'dri', 'pts', 'ptmx', 'shm', 'fd', 'stdin', 'input']
    self.assertEqual(run.unexpected_dev_entries(entries, self.devices),
                     ['input'])

class EnvironmentTest(unittest.TestCase):
  """The command's environment."""

  def test_host_display_variables_are_removed(self):
    environment = {'WAYLAND_DISPLAY': 'wayland-1', 'DISPLAY': ':0',
                   'WAYLAND_SOCKET': '3', 'WAYFIRE_SOCKET': '/run/w',
                   'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/run/bus',
                   'PATH': '/usr/bin', 'HOME': '/home/karl'}
    clean = run.clean_environment(environment, '/w', '/run/user/1000')
    for name in run.STRIPPED_VARIABLES:
      self.assertNotIn(name, clean)
    self.assertEqual(clean['PATH'], '/usr/bin')
    self.assertEqual(clean['HOME'], '/w/home')
    self.assertEqual(clean['XDG_RUNTIME_DIR'], '/run/user/1000')
    self.assertEqual(clean['XDG_CACHE_HOME'], '/w/cache')

class SignalCheckedTest(unittest.TestCase):
  """Only a live process of the expected name is signalled."""

  def test_refuses_dangerous_pids(self):
    for pid in (-1, 0, 1, True, '123', None):
      self.assertFalse(run.signal_checked(pid, signal.SIGTERM, 'sleep'))

  def test_refuses_another_command(self):
    child = subprocess.Popen(['sleep', '30'])
    try:
      self.assertFalse(run.signal_checked(child.pid, signal.SIGTERM, 'cat'))
      self.assertIsNone(child.poll())
      self.assertTrue(run.signal_checked(child.pid, signal.SIGTERM,
                                         'sleep'))
      self.assertEqual(child.wait(5), -signal.SIGTERM)
    finally:
      if child.poll() is None:
        child.kill()
        child.wait()

  def test_command_name_of_a_missing_pid(self):
    self.assertIsNone(run.command_name(2 ** 22 + 12345))
    self.assertEqual(run.command_name(os.getpid())[:6], 'python')

if __name__ == '__main__':
  unittest.main()
