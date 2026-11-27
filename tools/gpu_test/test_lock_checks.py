"""Tests of lock_checks.py's frame helpers; run with `python3 -m unittest`
in this directory."""

import unittest

import lock_checks

def frame(width, height, colour_at, lock='locked', lock_surface='1'):
  """A recorded frame whose pixel (x, y) is `colour_at(x, y)`."""
  pixels = bytes(value for y in range(height) for x in range(width)
                 for value in colour_at(x, y))
  header = (f'P6\n# shingle-frame output=HEADLESS-1 frame=7 lock={lock} '
            f'lock_surface={lock_surface}\n{width} {height}\n255\n')
  return header.encode() + pixels

RED, GREEN, BLUE, WHITE = (255, 0, 0), (0, 255, 0), (0, 0, 255), (255,) * 3

def pattern(x, y):
  """The probe's quadrants over a 64x48 frame."""
  if y < 24:
    return RED if x < 32 else GREEN
  return BLUE if x < 32 else WHITE

def ramp(position, edge, width=16.0):
  """How far `position` is across a blurred `edge`, from 0 to 1."""
  return min(1.0, max(0.0, (position - edge) / (2 * width) + 0.5))

def blurred(x, y):
  """The pattern blurred: each boundary a 32 pixel ramp."""
  across, down = ramp(x, 31.5), ramp(y, 23.5)
  top = [r * (1 - across) + g * across for r, g in zip(RED, GREEN)]
  bottom = [b * (1 - across) + w * across for b, w in zip(BLUE, WHITE)]
  return tuple(int(t * (1 - down) + b * down) for t, b in zip(top, bottom))

class FrameTest(unittest.TestCase):
  """The parser and the per-frame verdicts."""

  def test_the_header_names_the_lock_state(self):
    parsed = lock_checks.parse_frame(frame(4, 2, lambda x, y: RED))
    info, width, height, pixels = parsed
    self.assertEqual((info['output'], info['frame'], info['lock'],
                      info['lock_surface']), ('HEADLESS-1', '7', 'locked', '1'))
    self.assertEqual((width, height, len(pixels)), (4, 2, 24))
    self.assertIsNone(lock_checks.parse_frame(b'P6\n4 2\n255\n'))
    self.assertIsNone(lock_checks.parse_frame(frame(4, 2, lambda x, y: RED)
                                              [:-1]))

  def test_a_sharp_pattern_is_found_and_a_blurred_one_is_not(self):
    sharp = lock_checks.parse_frame(frame(64, 48, pattern))
    self.assertTrue(lock_checks.sharp_pattern(sharp))
    soft = lock_checks.parse_frame(frame(64, 48, blurred))
    self.assertFalse(lock_checks.sharp_pattern(soft))
    cyan = lock_checks.parse_frame(frame(64, 48, lambda x, y: (0, 255, 255)))
    self.assertFalse(lock_checks.sharp_pattern(cyan))
    self.assertGreater(lock_checks.colour_count(cyan, 'cyan'), 100)

  def test_black_and_differences(self):
    black = lock_checks.parse_frame(frame(40, 24, lambda x, y: (0, 0, 0)))
    self.assertTrue(lock_checks.all_black(black))
    dot = lock_checks.parse_frame(frame(
        40, 24, lambda x, y: (200, 0, 0) if (x, y) == (1, 1) else (0, 0, 0)))
    self.assertFalse(lock_checks.all_black(dot))
    self.assertEqual(lock_checks.difference(black, black), 0)
    self.assertEqual(lock_checks.difference(black, dot, step=1), 200)
    self.assertTrue(lock_checks.reddish((180, 60, 70)))
    self.assertFalse(lock_checks.reddish((128, 128, 128)))

  def test_the_caps_lock_and_layout_colours_have_their_own_classes(self):
    self.assertEqual(lock_checks.classify(128, 0, 255), lock_checks.VIOLET)
    self.assertEqual(lock_checks.classify(0, 255, 128), lock_checks.MINT)
    self.assertEqual(lock_checks.classify(0, 255, 255), 'cyan')
    self.assertIsNone(lock_checks.classify(128, 128, 128))
    violet = lock_checks.parse_frame(frame(16, 8, lambda x, y: (128, 0, 255)))
    self.assertEqual(lock_checks.Frame(violet[0], violet, False).violet, 16)

  def test_a_covering_capture_is_scaled_about_the_centre(self):
    # 640x400 on 800x400: scaled by 1.25, 50 rows cut at top and bottom.
    self.assertEqual(lock_checks.covered_point((320, 200), (640, 400),
                                               (800, 400)), (400, 200))
    self.assertEqual(lock_checks.covered_point((160, 100), (640, 400),
                                               (800, 400)), (200, 75))

  def test_letters_become_evdev_keys(self):
    self.assertEqual(lock_checks.key_names('ab'), ['KEY_A', 'KEY_B'])

  def test_white_over_a_dimmed_blur_is_not_the_pattern(self):
    # The minimal style's white dots beside a blurred quadrant dimmed to
    # 201: both classify as pattern colours, but the blue is not pure.
    def dots(x, y):
      return WHITE if x % 16 < 4 else (1, 0, 201)
    frame_ = lock_checks.parse_frame(frame(64, 48, dots))
    self.assertFalse(lock_checks.sharp_pattern(frame_))
    self.assertTrue(lock_checks.unblurred(frame_, (0, 0)))
    self.assertFalse(lock_checks.unblurred(frame_, (0, 0), (8, 0)))

  def test_the_minimal_styles_white_is_counted_by_rows(self):
    # A white bar in the middle rows of a 32x200 frame, a grey above.
    def bar(x, y):
      if 95 <= y <= 105:
        return WHITE
      return (200, 200, 200) if y < 80 else (0, 0, 0)
    parsed = lock_checks.parse_frame(frame(32, 200, bar))
    self.assertEqual(lock_checks.bright(parsed, range(90, 110)), 11 * 32)
    verdicts = lock_checks.Frame(parsed[0], parsed, False)
    self.assertEqual(verdicts.bright_band, 11 * 32)
    self.assertEqual(verdicts.bright_above, 0)

class LookTest(unittest.TestCase):
  """The trace of the minimal style's frames."""

  TRACE = (
      'shingle-lock: frame {n} of output {o} at {ms} ms, read back 64x4\n'
      'shingle-lock: look of output {o}: clock {c:.2f} input {i:.2f} rise '
      '0.00 shake -1.50 outline 1.00 tint 0.20 dots 3\n')

  def looks(self):
    """Five frames of output 12 and one of output 40."""
    text = ''.join(self.TRACE.format(n=n, o=12, ms=100 + 16 * n,
                                     c=1 - n / 4, i=n / 4)
                   for n in range(5))
    text += self.TRACE.format(n=1, o=40, ms=5, c=1, i=0)
    return [lock_checks.Look(match)
            for match in lock_checks.LOOK.finditer(text)]

  def test_each_frame_is_read(self):
    looks = self.looks()
    self.assertEqual(len(looks), 6)
    first = looks[0]
    self.assertEqual((first.output, first.ms, first.read_back), (12, 100, 256))
    self.assertEqual((first.clock, first.input, first.shake, first.tint,
                      first.dots), (1.0, 0.0, -1.5, 0.2, 3))

  def test_rates_and_spans(self):
    looks = [look for look in self.looks() if look.output == 12]
    self.assertAlmostEqual(lock_checks.frame_rate(looks), 62.5)
    self.assertEqual(lock_checks.frame_rate(looks[:1]), 0.0)
    moving = lock_checks.span(looks, lambda look: look.clock < 1.0,
                              lambda look: look.input == 1.0)
    self.assertEqual([look.number for look in moving], [1, 2, 3, 4])
    self.assertEqual(lock_checks.span(looks, lambda look: False,
                                      lambda look: True), [])
    self.assertIn('4 frames', lock_checks.describe_looks(moving))

if __name__ == '__main__':
  unittest.main()
