"""Tests for decoration_checks.py's helpers; nothing here starts a
process."""

import unittest

import decoration_checks

COLOURS = {
    'magenta': (255, 0, 255), 'cyan': (0, 255, 255), 'grey': (40, 40, 40),
}

class Shot:
  """A capture of one colour with a rectangle of another."""

  def __init__(self, width, height, inside, outside, rect):
    self.width = width
    self.height = height
    self.inside = inside
    self.outside = outside
    self.rect = rect

  def pixel(self, x, y):
    rect = self.rect
    inside = (rect['x'] <= x < rect['x'] + rect['width']
              and rect['y'] <= y < rect['y'] + rect['height'])
    return COLOURS[self.inside if inside else self.outside]

class HelperTest(unittest.TestCase):
  """Wayfire's default frame geometry and the frame's colours."""

  def test_the_client_is_inside_wayfires_default_margins(self):
    self.assertEqual(
        decoration_checks.margins_inside(
            {'x': 100, 'y': 100, 'width': 400, 'height': 300}),
        {'x': 104, 'y': 134, 'width': 392, 'height': 262})

  def test_buttons_are_where_wayfire_lays_them_out(self):
    geometry = {'x': 10, 'y': 20, 'width': 400, 'height': 300}
    # Frame-relative buttons at 371, 342 and 313, 8 below the top, 21
    # pixels square (crates/shingle/src/frame.rs).
    self.assertEqual(decoration_checks.button_centre(geometry, 0),
                     (10 + 371 + 10, 20 + 8 + 10))
    self.assertEqual(decoration_checks.button_centre(geometry, 1),
                     (10 + 342 + 10, 20 + 8 + 10))
    self.assertEqual(decoration_checks.button_centre(geometry, 2),
                     (10 + 313 + 10, 20 + 8 + 10))

  def test_a_frame_has_one_colour_and_nothing_frames_the_outside(self):
    rect = {'x': 10, 'y': 20, 'width': 50, 'height': 40}
    framed = Shot(100, 100, 'magenta', 'grey', rect)
    self.assertEqual(decoration_checks.frame_colour(framed, rect), 'magenta')
    self.assertTrue(decoration_checks.unframed(
        framed, {'x': 70, 'y': 70, 'width': 10, 'height': 10}))
    self.assertFalse(decoration_checks.unframed(
        framed, {'x': 12, 'y': 32, 'width': 10, 'height': 10}))
    half = Shot(100, 100, 'cyan', 'grey',
                {'x': 10, 'y': 20, 'width': 25, 'height': 40})
    self.assertIn('frame pixels', decoration_checks.frame_colour(half, rect))

if __name__ == '__main__':
  unittest.main()
