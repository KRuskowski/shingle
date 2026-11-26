"""Tests for drag_icon_checks.py's helpers; nothing here starts a
process."""

import unittest

import drag_icon_checks

class Shot:
  """A grey capture with a magenta square of `size` at `corner`."""

  def __init__(self, corner=None, size=32, width=200, height=100):
    self.width = width
    self.height = height
    self.corner = corner
    self.size = size

  def pixel(self, x, y):
    if self.corner is not None:
      left, top = self.corner
      if left <= x < left + self.size and top <= y < top + self.size:
        return (255, 0, 255)
    return (40, 40, 40)

class HelperTest(unittest.TestCase):
  """Finding the icon in a capture."""

  def test_the_icon_is_found_centred_on_its_point(self):
    shot = Shot(corner=(84, 34))
    self.assertTrue(drag_icon_checks.icon_at(shot, (100, 50)))
    # Ten pixels off, its far side is not magenta.
    self.assertFalse(drag_icon_checks.icon_at(shot, (110, 50)))
    self.assertFalse(drag_icon_checks.icon_at(Shot(), (100, 50)))

  def test_magenta_points_find_any_icon(self):
    self.assertEqual(drag_icon_checks.magenta_points(Shot()), [])
    points = drag_icon_checks.magenta_points(Shot(corner=(84, 34)))
    self.assertIn((88, 40), points)
    self.assertTrue(all(84 <= x < 116 and 34 <= y < 66
                        for x, y in points))

if __name__ == '__main__':
  unittest.main()
