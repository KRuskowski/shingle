"""Tests for the pure helpers of tools/gpu_test/nested_output_checks.py."""

import unittest

import nested_output_checks as nested

class WindowTest(unittest.TestCase):
  """Window titles, sizes and the mapping between the two layouts."""

  def test_windows_are_titled_after_their_outputs(self):
    self.assertEqual(nested.window_title('HEADLESS-1'), 'Shingle')
    self.assertEqual(nested.window_title('HEADLESS-3'),
                     'Shingle - HEADLESS-3')

  def test_every_output_must_be_as_big_as_its_window(self):
    windows = {'Shingle': (0, 0, 640, 720),
               'Shingle - HEADLESS-2': (640, 0, 640, 720)}
    outputs = {'HEADLESS-1': (1, (0, 0, 640, 720)),
               'HEADLESS-2': (2, (640, 0, 640, 720))}
    self.assertTrue(nested.sizes_match(windows, outputs))
    outputs['HEADLESS-2'] = (2, (640, 0, 1280, 720))
    self.assertFalse(nested.sizes_match(windows, outputs))
    self.assertFalse(nested.sizes_match({}, {'HEADLESS-1': (1, (0, 0, 1,
                                                                1))}))
    self.assertFalse(nested.sizes_match(windows, {}))

  def test_nested_points_land_in_their_window(self):
    window = (640, 0, 640, 720)
    output = (640, 0, 640, 720)
    self.assertEqual(nested.to_host((740, 200), window, output), (740, 200))
    window = (700, 50, 500, 400)
    output = (1280, 0, 500, 400)
    self.assertEqual(nested.to_host((1380, 200), window, output), (800, 250))
    self.assertEqual(nested.offset_of(window, output), (-580, 50))

  def test_wlr_randr_scale_is_read(self):
    text = ('HEADLESS-1 "Shingle Headless output (HEADLESS-1)"\n'
            '  Enabled: yes\n  Scale: 1.500000\n')
    self.assertEqual(nested.randr_scale(text), 1.5)
    self.assertIsNone(nested.randr_scale('HEADLESS-1\n'))

  def test_scaled_configs_free_super_and_the_buttons_on_the_host(self):
    outer, inner = nested.scaled_configs(2.0)
    self.assertEqual(outer['output'], {'scale': 2.0})
    self.assertEqual(outer['pointer']['move_button'], '')
    self.assertEqual(inner['pointer']['move_button'], 'super+btn_left')

if __name__ == '__main__':
  unittest.main()
