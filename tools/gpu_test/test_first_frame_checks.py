"""Tests for first_frame_checks.py's helpers; nothing here starts a
process."""

import os
import tempfile
import unittest

import first_frame_checks
import resize_checks

# A client started at 1000 ms that gets a 0x0 initial configure, draws
# its own 320x240, then gets its tile's 600x682 when it maps, which it
# acknowledges and draws.
LOG = '\n'.join([
    '[1010.000]  -> xdg_wm_base#9.get_xdg_surface('
    'new id xdg_surface#34, wl_surface#30)',
    '[1010.100]  -> xdg_surface#34.get_toplevel(new id xdg_toplevel#35)',
    '[1010.200]  -> wl_surface#30.commit()',
    '[1020.000] xdg_toplevel#35.configure(0, 0, array[0])',
    '[1020.001] xdg_surface#34.configure(5)',
    '[1020.100]  -> xdg_surface#34.ack_configure(5)',
    '[1030.000]  -> wl_shm_pool#81.create_buffer('
    'new id wl_buffer#83, 0, 320, 240, 1280, 0)',
    '[1030.100]  -> wl_surface#30.attach(wl_buffer#83, 0, 0)',
    '[1030.200]  -> wl_surface#30.commit()',
    '[1040.000] xdg_toplevel#35.configure(600, 682, array[20])',
    '[1040.001] xdg_surface#34.configure(6)',
    '[1045.000]  -> wl_surface#30.commit()',
    '[1050.000]  -> xdg_surface#34.ack_configure(6)',
    '[1052.000]  -> wl_surface#30.attach(wl_buffer#90, 0, 0)',
    '[1052.500]  -> wl_surface#30.commit()',
    '[1060.000] xdg_toplevel#35.configure(600, 682, array[24])',
    '[1060.001] xdg_surface#34.configure(7)',
])

class HelperTest(unittest.TestCase):
  """Reading the first frames from WAYLAND_DEBUG output."""

  def setUp(self):
    self.messages = resize_checks.parse_debug(LOG)

  def test_the_toplevel_and_its_configures_with_serials(self):
    self.assertEqual(first_frame_checks.toplevel_id(self.messages), 35)
    self.assertEqual(first_frame_checks.configures(self.messages, 35),
                     [(1020.001, (0, 0), 5), (1040.001, (600, 682), 6),
                      (1060.001, (600, 682), 7)])
    self.assertEqual(first_frame_checks.configures(self.messages, 99), [])
    self.assertEqual(first_frame_checks.acks(self.messages),
                     {5: 1020.1, 6: 1050.0})

  def test_the_tile_frame_follows_the_ack_of_the_tiles_configure(self):
    frames = first_frame_checks.first_frames(self.messages, 1000.0)
    self.assertAlmostEqual(frames['configure'], 20.001)
    self.assertEqual(frames['first_size'], (0, 0))
    self.assertAlmostEqual(frames['first_buffer'], 30.2)
    # Not the commit before the ack, which still showed 320x240.
    self.assertAlmostEqual(frames['tile_frame'], 52.5)
    self.assertEqual(frames['size'], (600, 682))
    self.assertEqual(frames['configures'], 2)

  def test_a_first_configure_of_the_tiles_size_is_its_first_frame(self):
    log = '\n'.join(line.replace('configure(0, 0,', 'configure(600, 682,')
                    for line in LOG.splitlines())
    frames = first_frame_checks.first_frames(
        resize_checks.parse_debug(log), 1000.0)
    self.assertEqual(frames['configures'], 1)
    self.assertAlmostEqual(frames['tile_frame'], 30.2)
    self.assertEqual(frames['tile_frame'], frames['first_buffer'])

  def test_what_has_not_happened_is_none(self):
    frames = first_frame_checks.first_frames(self.messages[:3], 1000.0)
    self.assertIsNone(frames['configure'])
    self.assertIsNone(frames['tile_frame'])
    self.assertIsNone(first_frame_checks.first_frames([], 0)['first_buffer'])
    only_zero = first_frame_checks.first_frames(self.messages[:9], 1000.0)
    self.assertIsNone(only_zero['tile_frame'])
    self.assertIsNone(only_zero['size'])

  def test_summary(self):
    self.assertEqual(first_frame_checks.summary([3.0, None, 1.0, 2.0]),
                     '2.0 ms (1.0 to 3.0)')
    self.assertEqual(first_frame_checks.summary([None]), 'none')

  def test_foot_from_the_variable_with_its_unpacked_libraries(self):
    with tempfile.TemporaryDirectory() as root:
      binary = os.path.join(root, 'usr', 'bin', 'foot')
      libraries = os.path.join(root, 'usr', 'lib', 'x86_64-linux-gnu')
      os.makedirs(os.path.dirname(binary))
      os.makedirs(libraries)
      with open(binary, 'w') as file:
        file.write('#!/bin/sh\n')
      os.chmod(binary, 0o755)
      saved = os.environ.get(first_frame_checks.FOOT_VARIABLE)
      os.environ[first_frame_checks.FOOT_VARIABLE] = binary
      try:
        self.assertEqual(first_frame_checks.foot_command(), binary)
      finally:
        if saved is None:
          del os.environ[first_frame_checks.FOOT_VARIABLE]
        else:
          os.environ[first_frame_checks.FOOT_VARIABLE] = saved
      self.assertEqual(first_frame_checks.foot_environment(binary),
                       {'LD_LIBRARY_PATH': libraries})
    self.assertEqual(first_frame_checks.foot_environment('/usr/bin/foot'),
                     {})

if __name__ == '__main__':
  unittest.main()
