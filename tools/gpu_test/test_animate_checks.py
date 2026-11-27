"""Tests for animate_checks.py's helpers; nothing here starts a process."""

import unittest

import animate_checks

GREY = (128, 128, 128)
COLOURS = {
    'red': (255, 0, 0), 'green': (0, 255, 0), 'blue': (0, 0, 255),
    'white': (255, 255, 255),
}
VIEW = {'id': 1, 'geometry': {'x': 20, 'y': 20, 'width': 80, 'height': 80}}

class Shot:
  """A 120 by 120 capture of the grey with `paint(x, y)` over it, which
  returns a pixel or None for the grey."""

  def __init__(self, paint):
    self.width = 120
    self.height = 120
    self.paint = paint

  def pixel(self, x, y):
    return self.paint(x, y) or GREY

def pattern(x, y, top=20, left=20, size=80):
  """The probe's quadrants over (left, top, size, size)."""
  if not (left <= x < left + size and top <= y < top + size):
    return None
  column = (x - left) * 2 // size
  row = (y - top) * 2 // size
  return COLOURS[(('red', 'green'), ('blue', 'white'))[row][column]]

class PredicateTest(unittest.TestCase):
  """Each animation's look against what it is not."""

  def test_the_settled_pattern_and_the_empty_view(self):
    self.assertTrue(animate_checks.pattern_shown(Shot(pattern), VIEW))
    self.assertFalse(animate_checks.view_gone(Shot(pattern), VIEW))
    self.assertTrue(animate_checks.view_gone(Shot(lambda x, y: None), VIEW))
    # A view zoomed small about its centre is not gone.
    small = Shot(lambda x, y: pattern(x, y, 47, 47, 26))
    self.assertFalse(animate_checks.view_gone(small, VIEW))
    # Nor is a nearly opaque one fully drawn.
    dim = Shot(lambda x, y: tuple(value * 9 // 10 + 13 for value in
                                  pattern(x, y) or GREY))
    self.assertFalse(animate_checks.pattern_shown(dim, VIEW))

  def test_a_fade_is_between_the_colour_and_the_grey(self):
    half = Shot(lambda x, y: tuple((value + 128) // 2 for value in
                                   pattern(x, y) or GREY))
    self.assertTrue(animate_checks.faded(half, VIEW))
    self.assertFalse(animate_checks.faded(Shot(pattern), VIEW))
    self.assertFalse(animate_checks.faded(Shot(lambda x, y: None), VIEW))

  def test_a_zoom_draws_only_around_the_centre(self):
    small = Shot(lambda x, y: pattern(x, y, 47, 47, 26))
    self.assertTrue(animate_checks.zoomed(small, VIEW))
    self.assertFalse(animate_checks.zoomed(Shot(pattern), VIEW))

  def test_a_zap_is_a_line_through_the_middle(self):
    line = Shot(lambda x, y: (255, 0, 0) if 20 <= x < 80 and y == 60
                else None)
    self.assertTrue(animate_checks.zap_line(line, VIEW))
    self.assertFalse(animate_checks.zap_line(Shot(pattern), VIEW))

  def test_fire_burns_below_the_drawn_top(self):
    def fire(x, y):
      if y < 60:
        return pattern(x, y)
      if 20 <= x < 100 and 60 <= y < 70:
        return (230, 140, 60)
      return None
    self.assertTrue(animate_checks.burning(Shot(fire), VIEW))
    self.assertFalse(animate_checks.burning(Shot(pattern), VIEW))
    cut = Shot(lambda x, y: pattern(x, y) if y < 60 else None)
    self.assertFalse(animate_checks.burning(cut, VIEW), 'no fire')

  def test_a_turned_view_has_a_slanted_top_edge(self):
    # Rises one row per two columns from left to right.
    slanted = Shot(lambda x, y: (255, 0, 0) if 20 <= x < 100
                   and y >= 40 - (x - 20) // 2 else None)
    self.assertTrue(animate_checks.turned(slanted, VIEW))
    self.assertFalse(animate_checks.turned(Shot(pattern), VIEW))

  def test_a_squeeze_empties_the_top_first(self):
    low = Shot(lambda x, y: (0, 0, 255) if 40 <= x < 80 and y >= 85
               else None)
    self.assertTrue(animate_checks.squeezed(low, VIEW))
    self.assertFalse(animate_checks.squeezed(Shot(pattern), VIEW))

  def test_the_start_is_darker_than_the_grey(self):
    self.assertTrue(animate_checks.startup_dark(Shot(lambda x, y: (20,) * 3)))
    self.assertFalse(animate_checks.startup_dark(Shot(lambda x, y: None)))

if __name__ == '__main__':
  unittest.main()
