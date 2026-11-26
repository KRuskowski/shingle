"""Tests for wayfire_checks.py's helpers; nothing here starts a process."""

import os
import subprocess
import tempfile
import unittest

import wayfire_checks

class HelperTest(unittest.TestCase):
  """The INI, layout and stand-in helpers."""

  def test_ini_names_the_plugins_and_replaces_autostart(self):
    text = wayfire_checks.ini('scale vswitch')
    self.assertIn('plugins = scale vswitch\n', text)
    self.assertIn('autostart_wf_shell = false\n', text)
    text = wayfire_checks.ini('autostart', 'panel = wf-panel\n')
    self.assertNotIn('autostart_wf_shell', text)
    self.assertTrue(text.endswith('[autostart]\npanel = wf-panel\n'))

  def test_layout_views_finds_every_view(self):
    layout = {'vertical-split': [
        {'view-id': 3, 'geometry': {}},
        {'horizontal-split': [{'view-id': 4}, {'view-id': 5}]},
    ]}
    self.assertEqual(wayfire_checks.layout_views(layout), [3, 4, 5])
    self.assertEqual(wayfire_checks.layout_views({'vertical-split': []}), [])
    self.assertEqual(wayfire_checks.layout_views(None), [])

  def test_stand_ins_only_record_that_they_started(self):
    with tempfile.TemporaryDirectory() as directory:
      bin_directory = wayfire_checks.write_stand_ins(directory)
      self.assertEqual(sorted(os.listdir(bin_directory)),
                       ['wf-background', 'wf-panel'])
      self.assertEqual(wayfire_checks.started(directory, 'wf-panel'), 0)
      for _ in range(2):
        subprocess.run([os.path.join(bin_directory, 'wf-panel')], check=True,
                       timeout=5, stdin=subprocess.DEVNULL)
      self.assertEqual(wayfire_checks.started(directory, 'wf-panel'), 2)
      self.assertEqual(wayfire_checks.started(directory, 'wf-background'), 0)

if __name__ == '__main__':
  unittest.main()
