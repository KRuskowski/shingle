"""Tests for the pure helpers of tools/gpu_test/nested_clipboard_checks.py."""

import unittest

import nested_clipboard_checks as clipboard

class HelperTest(unittest.TestCase):
  """Options and nested's log line."""

  def test_wl_clipboard_options_name_the_selection(self):
    self.assertEqual(clipboard.wl_options('clipboard'), [])
    self.assertEqual(clipboard.wl_options('primary'), ['--primary'])
    with self.assertRaises(ValueError):
      clipboard.wl_options('secondary')

  def test_the_sharing_line_names_the_selections_and_protocol(self):
    log = ('WAYLAND_DISPLAY=wayland-1\n'
           'shingle: sharing [Clipboard, Primary] with the host through '
           'zwlr_data_control_manager_v1\n')
    self.assertEqual(clipboard.shared_line(log),
                     (('clipboard', 'primary'),
                      'zwlr_data_control_manager_v1'))
    self.assertEqual(
        clipboard.shared_line('shingle: sharing [Clipboard] with the host '
                              'through ext_data_control_manager_v1'),
        (('clipboard',), 'ext_data_control_manager_v1'))
    self.assertIsNone(clipboard.shared_line('shingle: the host offers '
                                            'neither\n'))

if __name__ == '__main__':
  unittest.main()
