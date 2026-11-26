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

class ShmTraceTest(unittest.TestCase):
  """Upload sizes from Shingle's buffer trace, and CPU time."""

  LOG = '\n'.join([
      'shingle: buffer: shm upload 3042000 bytes surface=3',
      'shingle: buffer: release wl_buffer@20',
      'shingle: buffer: shm upload 4000000 bytes surface=3',
      'shingle: buffer: shm upload 16 bytes surface=7',
      'shingle: buffer: dmabuf cached dmabuf=1 surface=3',
      'shingle: buffer: shm upload 400 bytes surface=3',
      'something else: upload 5 bytes surface=3',
  ])

  def test_uploads_are_read_in_order(self):
    self.assertEqual(checks.upload_sizes(self.LOG),
                     [('3', 3042000), ('3', 4000000), ('7', 16),
                      ('3', 400)])
    self.assertEqual(checks.upload_sizes(self.LOG, '7'), [('7', 16)])

  def test_the_damage_upload_follows_the_full_one(self):
    self.assertEqual(checks.damage_uploads(self.LOG), (4000000, 400))
    self.assertEqual(checks.damage_uploads(''), (None, None))
    self.assertEqual(
        checks.damage_uploads('shm upload 4000000 bytes surface=1'),
        (4000000, None))

  def test_cpu_seconds_skip_a_command_with_spaces(self):
    stat = ('42 (shingle (x) y) S 1 42 42 0 -1 4194560 100 0 0 0 '
            '250 50 0 0 20 0 3 0 100 0 0')
    self.assertEqual(checks.cpu_seconds(stat, 100), 3.0)

  def test_buffer_points_scale_to_the_view(self):
    geometry = {'x': 60, 'y': 30, 'width': 1170, 'height': 650}
    self.assertEqual(checks.buffer_point(geometry, (505, 505)), (650, 358))
    self.assertEqual(checks.buffer_point(geometry, (0, 0)), (60, 30))

  def test_probe_events_by_name(self):
    output = '{"event": "damage", "step": "full"}\nnot json\n[1]\n'
    self.assertEqual(checks.probe_events(output, 'damage'),
                     [{'event': 'damage', 'step': 'full'}])
    self.assertEqual(checks.probe_events(output, 'release'), [])

class SamePixelsTest(unittest.TestCase):
  """Captures compare by size and pixels."""

  def test_same_and_different(self):
    capture = checks.xw.Capture
    self.assertTrue(checks.same_pixels(capture(1, 1, b'abc'),
                                       capture(1, 1, b'abc')))
    self.assertFalse(checks.same_pixels(capture(1, 1, b'abc'),
                                        capture(1, 1, b'abd')))
    self.assertFalse(checks.same_pixels(None, capture(1, 1, b'abc')))

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
    # Between the thresholds: neither saturated nor grey.
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
    shot = FakeShot(4, 4, lambda x, y: (26, 27, 24) if x else (0, 0, 0))
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
    import decoration_checks
    import dmabuf_checks
    import drag_icon_checks
    import colour_checks
    import focus_checks
    import nested_output_checks
    import output_checks
    import switcher_checks
    import vswipe_checks
    import resize_checks
    import wayfire_checks
    self.assertEqual(set(checks.GROUPS), set(checks.CHECKS))
    self.assertEqual(set(dmabuf_checks.GROUPS), set(dmabuf_checks.CHECKS))
    self.assertEqual(set(output_checks.GROUPS), set(output_checks.CHECKS))
    self.assertEqual(set(nested_output_checks.GROUPS),
                     set(nested_output_checks.CHECKS))
    self.assertEqual(set(wayfire_checks.GROUPS), set(wayfire_checks.CHECKS))
    self.assertEqual(set(vswipe_checks.GROUPS), set(vswipe_checks.CHECKS))
    self.assertEqual(set(decoration_checks.GROUPS),
                     set(decoration_checks.CHECKS))
    self.assertEqual(set(switcher_checks.GROUPS), set(switcher_checks.CHECKS))
    self.assertEqual(set(colour_checks.GROUPS), set(colour_checks.CHECKS))
    self.assertEqual(set(focus_checks.GROUPS), set(focus_checks.CHECKS))
    self.assertEqual(set(drag_icon_checks.GROUPS),
                     set(drag_icon_checks.CHECKS))
    self.assertEqual(set(resize_checks.GROUPS), set(resize_checks.CHECKS))
    self.assertEqual(
        checks.GROUPS + dmabuf_checks.GROUPS + output_checks.GROUPS
        + nested_output_checks.GROUPS + wayfire_checks.GROUPS
        + vswipe_checks.GROUPS + decoration_checks.GROUPS
        + switcher_checks.GROUPS + colour_checks.GROUPS
        + focus_checks.GROUPS + drag_icon_checks.GROUPS
        + resize_checks.GROUPS,
        checks.run.CHECK_GROUPS)
    # Every DB-4 and DB-5 group runs NVIDIA Vulkan clients but `syncobj`.
    self.assertEqual(tuple(group for group in dmabuf_checks.GROUPS
                           if group != 'syncobj'),
                     checks.run.MODESET_GROUPS)
    self.assertEqual(set(checks.all_checks()), set(checks.run.CHECK_GROUPS))

  def test_tail(self):
    self.assertEqual(checks.tail('a\nb\nc\n', 2), 'b | c')
    self.assertEqual(checks.tail(''), '')

if __name__ == '__main__':
  unittest.main()
