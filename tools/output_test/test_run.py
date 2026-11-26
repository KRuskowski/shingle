"""Tests for the pure helpers of tools/output_test/run.py."""

import importlib.util
import os
import unittest

_SPEC = importlib.util.spec_from_file_location(
    'output_run', os.path.join(os.path.dirname(__file__), 'run.py'))
run = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(run)

RANDR = '''HEADLESS-1 "Shingle Headless output (HEADLESS-1)"
  Make: Shingle
  Model: Headless output
  Physical size: 310x175 mm
  Enabled: yes
  Modes:
    1920x1080 px, 60.000000 Hz (preferred)
    1600x900 px, 50.000000 Hz (current)
    1280x720 px, 59.940000 Hz
  Position: 100,20
  Transform: 90
  Scale: 2.000000
  Adaptive Sync: disabled
DP-1 "Dell Inc. DELL U2720Q (DP-1)"
  Enabled: no
  Modes:
'''

class ParseWlrRandrTest(unittest.TestCase):
  """parse_wlr_randr and current_mode."""

  def test_heads_modes_and_properties(self):
    heads = run.parse_wlr_randr(RANDR)
    self.assertEqual(list(heads), ['HEADLESS-1', 'DP-1'])
    head = heads['HEADLESS-1']
    self.assertEqual(head['description'],
                     'Shingle Headless output (HEADLESS-1)')
    self.assertEqual(head['modes'], [
        (1920, 1080, 60.0, ('preferred',)),
        (1600, 900, 50.0, ('current',)),
        (1280, 720, 59.94, ()),
    ])
    self.assertEqual((head['make'], head['model']),
                     ('Shingle', 'Headless output'))
    self.assertEqual(head['position'], (100, 20))
    self.assertEqual(head['transform'], '90')
    self.assertEqual(head['scale'], 2.0)
    self.assertTrue(head['enabled'])
    self.assertFalse(heads['DP-1']['enabled'])
    self.assertEqual(heads['DP-1']['modes'], [])

  def test_current_mode(self):
    heads = run.parse_wlr_randr(RANDR)
    self.assertEqual(run.current_mode(heads['HEADLESS-1']), (1600, 900, 50.0))
    self.assertIsNone(run.current_mode(heads['DP-1']))

  def test_garbage_gives_no_heads(self):
    self.assertEqual(run.parse_wlr_randr(''), {})
    self.assertEqual(run.parse_wlr_randr('  Modes:\n'), {})

XDG = """interface: 'zxdg_output_manager_v1',          version:  3, name: 10
\txdg_output_v1
\t\toutput: 9
\t\tname: 'HEADLESS-1'
\t\tdescription: 'Shingle Headless output (HEADLESS-1)'
\t\tlogical_x: 0, logical_y: 0
\t\tlogical_width: 1280, logical_height: 720
\txdg_output_v1
\t\toutput: 11
\t\tname: 'HEADLESS-2'
\t\tdescription: 'Shingle Headless output (HEADLESS-2)'
\t\tlogical_x: -1024, logical_y: 200
\t\tlogical_width: 1024, logical_height: 768
"""

class MultiOutputHelpersTest(unittest.TestCase):
  """parse_xdg_outputs and swap_outputs."""

  def test_xdg_outputs_by_name(self):
    self.assertEqual(run.parse_xdg_outputs(XDG), {
        'HEADLESS-1': (0, 0, 1280, 720),
        'HEADLESS-2': (-1024, 200, 1024, 768),
    })
    self.assertEqual(run.parse_xdg_outputs(''), {})

  def test_a_size_without_a_position_is_ignored(self):
    text = "\t\tname: 'X'\n\t\tlogical_width: 1, logical_height: 1\n"
    self.assertEqual(run.parse_xdg_outputs(text), {})

  def test_swapping_outputs_in_a_session(self):
    session = {'wsets': [
        {'output': 'HEADLESS-1', 'home': 'HEADLESS-1'},
        {'output': None, 'home': 'DP-1'},
        {'output': 'HEADLESS-3', 'home': 'HEADLESS-3'},
    ]}
    swapped = run.swap_outputs(session, 'HEADLESS-1', 'HEADLESS-3')
    self.assertEqual(swapped['wsets'], [
        {'output': 'HEADLESS-3', 'home': 'HEADLESS-3'},
        {'output': None, 'home': 'DP-1'},
        {'output': 'HEADLESS-1', 'home': 'HEADLESS-1'},
    ])

class PrivateEnvironmentTest(unittest.TestCase):
  """private_environment."""

  def test_host_display_variables_are_removed(self):
    saved = dict(os.environ)
    try:
      os.environ.update({'WAYLAND_DISPLAY': 'wayland-1', 'DISPLAY': ':0',
                         'WAYFIRE_SOCKET': '/run/x', 'WAYLAND_SOCKET': '3'})
      environment = run.private_environment('/tmp/x')
    finally:
      os.environ.clear()
      os.environ.update(saved)
    for name in run.HOST_VARIABLES:
      self.assertNotIn(name, environment)
    self.assertEqual(environment['XDG_RUNTIME_DIR'], '/tmp/x/runtime')
    self.assertEqual(environment['HOME'], '/tmp/x/home')

if __name__ == '__main__':
  unittest.main()
