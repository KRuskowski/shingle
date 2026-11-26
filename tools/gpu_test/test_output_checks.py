"""Tests for the pure helpers of tools/gpu_test/output_checks.py."""

import json
import unittest

import output_checks

def presented(output, msec):
  """One `presented` event as the probe prints it."""
  return {'event': 'presented', 'output': output, 'seq': 1, 'msec': msec}

class FrameCountTest(unittest.TestCase):
  """Frames per output in a window, and the rate they make."""

  def test_frames_are_counted_per_output_after_the_skip(self):
    events = [presented('HEADLESS-1', 1000 + 10 * index)
              for index in range(1000)]
    events += [presented('HEADLESS-2', 1000 + 20 * index)
               for index in range(500)]
    count = output_checks.frames_in_window(events, 'HEADLESS-1', skip_ms=100,
                                           length_ms=5000)
    self.assertEqual(count, 500)
    self.assertEqual(
        output_checks.frames_in_window(events, 'HEADLESS-2', skip_ms=100,
                                       length_ms=5000), 250)

  def test_too_short_a_run_counts_nothing(self):
    events = [presented('HEADLESS-1', 10 * index) for index in range(100)]
    self.assertIsNone(output_checks.frames_in_window(events, 'HEADLESS-1'))
    self.assertIsNone(output_checks.frames_in_window(events, 'HEADLESS-9'))

  def test_rates_match_within_two_percent(self):
    self.assertTrue(output_checks.rate_matches(300, 60))
    self.assertTrue(output_checks.rate_matches(294, 60))
    self.assertFalse(output_checks.rate_matches(293, 60))
    self.assertTrue(output_checks.rate_matches(153, 30))
    self.assertFalse(output_checks.rate_matches(300, 30))
    self.assertFalse(output_checks.rate_matches(None, 30))

  def test_probe_events_skip_other_lines(self):
    text = '\n'.join([
        json.dumps(presented('HEADLESS-2', 5)),
        'not json',
        json.dumps({'event': 'configure'}),
        '[1, 2]',
    ])
    self.assertEqual(output_checks.probe_events(text, 'presented'),
                     [presented('HEADLESS-2', 5)])

class LayoutTest(unittest.TestCase):
  """The outputs and the windows placed on them."""

  def test_geometry_relative_to_an_output(self):
    self.assertEqual(
        output_checks.local(output_checks.ON_SECOND, 'HEADLESS-2'),
        {'x': 600, 'y': 400, 'width': 300, 'height': 200})

  def test_the_straddling_window_crosses_the_boundary(self):
    window = output_checks.STRADDLING
    boundary = output_checks.LAYOUT['HEADLESS-2'][0]
    self.assertLess(window['x'], boundary)
    self.assertGreater(window['x'] + window['width'], boundary)

  def test_the_config_names_both_outputs_with_their_rates(self):
    config = output_checks.config()
    self.assertEqual([output['name'] for output in config['outputs']],
                     list(output_checks.RATES))
    self.assertEqual(config['xwayland'], {'mode': False})

if __name__ == '__main__':
  unittest.main()
