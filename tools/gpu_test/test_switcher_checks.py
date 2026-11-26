"""Tests for switcher_checks.py's helpers; nothing here starts a process."""

import unittest

import switcher_checks

class Shot:
  """A capture from a function of (x, y) to a pixel."""

  def __init__(self, width, height, pixel):
    self.width = width
    self.height = height
    self.pixel = pixel

def rectangle(shot_width, shot_height, left, right, top_of, bottom_of):
  """A shot that is white where left <= x <= right and top_of(x) <= y <=
  bottom_of(x), black elsewhere."""
  def pixel(x, y):
    inside = left <= x <= right and top_of(x) <= y <= bottom_of(x)
    return (255, 255, 255) if inside else (0, 0, 0)
  return Shot(shot_width, shot_height, pixel)

class HelperTest(unittest.TestCase):
  """Thumbnail rectangles, column heights and turned sides."""

  def test_a_thumbnail_fits_45_percent_and_is_never_enlarged(self):
    output = {'width': 1280, 'height': 720}
    tall = {'geometry': {'x': 0, 'y': 0, 'width': 426, 'height': 720}}
    self.assertEqual(
        switcher_checks.thumbnail_rect(tall, output, (640, 360)),
        {'x': 544, 'y': 198, 'width': 192, 'height': 324})
    small = {'geometry': {'x': 0, 'y': 0, 'width': 100, 'height': 50}}
    self.assertEqual(
        switcher_checks.thumbnail_rect(small, output, (100, 100), 0.5),
        {'x': 75, 'y': 88, 'width': 50, 'height': 25})

  def test_column_extent_counts_the_non_black_run(self):
    shot = rectangle(20, 20, 5, 10, lambda x: 4, lambda x: 12)
    self.assertEqual(switcher_checks.column_extent(shot, 7, 8), 9)
    self.assertEqual(switcher_checks.column_extent(shot, 2, 8), 0)

  def test_a_turned_side_is_shorter_towards_the_middle(self):
    # Shorter on the right: its right edge is nearer the middle.
    shot = rectangle(100, 100, 10, 50, lambda x: 10 + x // 10,
                     lambda x: 90 - x // 10)
    rect = {'x': 10, 'y': 10, 'width': 41, 'height': 80}
    found = switcher_checks.side_problem(shot, rect, 'right')
    # The white shot has no pattern, so only the pattern is wrong.
    self.assertIn('pattern', found)
    original = switcher_checks.checks.pattern_mismatches
    switcher_checks.checks.pattern_mismatches = lambda *_: []
    try:
      self.assertIsNone(switcher_checks.side_problem(shot, rect, 'right'))
      self.assertIn('heights',
                    switcher_checks.side_problem(shot, rect, 'left'))
    finally:
      switcher_checks.checks.pattern_mismatches = original

  def test_shrunk_keeps_the_centre(self):
    self.assertEqual(
        switcher_checks.shrunk({'x': 0, 'y': 5, 'width': 100, 'height': 10}),
        {'x': 20, 'y': 5, 'width': 60, 'height': 10})

if __name__ == '__main__':
  unittest.main()
