"""Tests for the pure helpers of tools/gpu_test/checks.py."""

import unittest

import checks

class SafeBackendTest(unittest.TestCase):
  """Shingle is only ever started on a backend that cannot reach the
  host's display, card node or input."""

  def test_explicit_safe_backends(self):
    for arguments in (['--headless'], ['--backend', 'offscreen'],
                      ['--backend=nested'], ['--config', 'x', '--headless']):
      self.assertTrue(checks.safe_backend(arguments), arguments)

  def test_native_or_automatic_selection_is_refused(self):
    for arguments in ([], ['--backend', 'native'], ['--backend=native'],
                      ['--backend'], ['--config', 'offscreen'],
                      ['--backend', 'drm']):
      self.assertFalse(checks.safe_backend(arguments), arguments)

  def test_shingle_refuses_an_unsafe_backend_before_starting(self):
    with self.assertRaises(ValueError):
      checks.Shingle('/nonexistent/shingle', 'x', {}, backend=())

class ColourTest(unittest.TestCase):
  """Pixel colours by name."""

  def test_saturated_and_grey(self):
    self.assertEqual(checks.classify(255, 10, 0), 'red')
    self.assertEqual(checks.classify(0, 250, 255), 'cyan')
    self.assertEqual(checks.classify(255, 255, 255), 'white')
    self.assertEqual(checks.classify(0, 0, 0), 'black')
    self.assertEqual(checks.classify(250, 250, 0), 'yellow')

  def test_ambiguous_colours_have_no_name(self):
    self.assertIsNone(checks.classify(128, 0, 0))
    # The default background, 0.1 linear, is 89 in sRGB.
    self.assertIsNone(checks.classify(89, 89, 89))

  def test_inversion_of_the_pattern(self):
    for row in checks.PATTERN:
      for colour in row:
        self.assertIn(colour, checks.INVERTED)

class FakeShot:
  """A capture whose pixels come from a function."""

  def __init__(self, width, height, pixel):
    self.width = width
    self.height = height
    self._pixel = pixel

  def pixel(self, x, y):
    """The (r, g, b) at (x, y)."""
    return self._pixel(x, y)

def pattern_at(geometry, inverted=False):
  """A pixel function that draws the probe's pattern over `geometry`."""
  colours = {'red': (255, 0, 0), 'green': (0, 255, 0), 'blue': (0, 0, 255),
             'white': (255, 255, 255), 'cyan': (0, 255, 255),
             'magenta': (255, 0, 255), 'yellow': (255, 255, 0),
             'black': (0, 0, 0)}

  def pixel(x, y):
    column = int(x - geometry['x'] >= geometry['width'] // 2)
    row = int(y - geometry['y'] >= geometry['height'] // 2)
    name = checks.PATTERN[row][column]
    return colours[checks.INVERTED[name] if inverted else name]
  return pixel

class PatternTest(unittest.TestCase):
  """Where the quadrants are looked for and how mismatches read."""

  geometry = {'x': 100, 'y': 50, 'width': 400, 'height': 200}

  def test_points_are_the_quadrant_centres(self):
    self.assertEqual(
        checks.quadrant_points(self.geometry),
        [((200, 100), (0, 0)), ((400, 100), (1, 0)),
         ((200, 200), (0, 1)), ((400, 200), (1, 1))])
    self.assertEqual(
        checks.quadrant_points(self.geometry, (10, 20))[0], ((210, 120),
                                                             (0, 0)))

  def test_a_drawn_pattern_matches(self):
    shot = FakeShot(1280, 720, pattern_at(self.geometry))
    self.assertEqual(checks.pattern_mismatches(shot, self.geometry), [])
    inverted = FakeShot(1280, 720, pattern_at(self.geometry, True))
    self.assertEqual(
        checks.pattern_mismatches(inverted, self.geometry, inverted=True), [])
    self.assertEqual(len(checks.pattern_mismatches(inverted, self.geometry)),
                     4)

  def test_a_flipped_or_shifted_pattern_does_not(self):
    flipped = {'x': 100, 'y': 50, 'width': 400, 'height': 200}

    def upside_down(x, y):
      return pattern_at(flipped)(x, 299 - y)
    shot = FakeShot(1280, 720, upside_down)
    wrong = checks.pattern_mismatches(shot, self.geometry)
    self.assertEqual([expected for _, expected, _ in wrong],
                     ['red', 'green', 'blue', 'white'])
    small = FakeShot(300, 150, pattern_at(self.geometry))
    self.assertIn(((400, 200), 'white', 'outside'),
                  checks.pattern_mismatches(small, self.geometry))

  def test_background(self):
    shot = FakeShot(4, 4, lambda x, y: (89, 90, 87) if x else (0, 0, 0))
    self.assertTrue(checks.background_at(shot, 1, 1))
    self.assertFalse(checks.background_at(shot, 0, 1))

  def test_gaps_extend_the_base_config(self):
    config = checks.with_gaps(checks.OUTER_GAPS)
    self.assertEqual(config['xwayland'], {'mode': False})
    self.assertEqual(config['tiling']['gaps']['left'], 60)
    self.assertNotIn('tiling', checks.BASE_CONFIG)

  def test_summary_lines_keep_the_driver_and_the_end(self):
    text = ('EGL_VERSION = 1.5\nnoise\nGL_RENDERER = llvmpipe\nmore\n'
            '300 frames\n')
    self.assertEqual(checks.summary_lines(text),
                     'EGL_VERSION = 1.5 | GL_RENDERER = llvmpipe | more | '
                     '300 frames')
    self.assertEqual(checks.summary_lines(''), '')

  def test_released_buffers_reads_the_probe_s_release_lines(self):
    output = ('{"event": "configure", "width": 1}\nnot json\n'
              '{"event": "release", "buffer": "a"}\n[1]\n'
              '{"event": "release", "buffer": "c"}\n')
    self.assertEqual(checks.released_buffers(output), {'a', 'c'})
    self.assertEqual(checks.released_buffers(''), set())

  def test_every_group_has_a_check(self):
    self.assertEqual(set(checks.GROUPS), set(checks.CHECKS))
    self.assertEqual(checks.GROUPS, checks.run.CHECK_GROUPS)

  def test_tail(self):
    self.assertEqual(checks.tail('a\nb\nc\n', 2), 'b | c')
    self.assertEqual(checks.tail(''), '')

if __name__ == '__main__':
  unittest.main()
