"""Tests for vswipe_checks.py's helpers; nothing here starts a process."""

import unittest

import vswipe_checks

COLOURS = {
    'red': (255, 0, 0), 'green': (0, 255, 0), 'blue': (0, 0, 255),
    'white': (255, 255, 255), 'magenta': (255, 0, 255),
}

class Shot:
  """A capture whose rows are lists of colour names."""

  def __init__(self, rows):
    self.rows = rows
    self.width = len(rows[0])
    self.height = len(rows)

  def pixel(self, x, y):
    return COLOURS[self.rows[y][x]]

def row(*runs):
  """A row of (colour, length) runs."""
  return [colour for colour, length in runs for _ in range(length)]

class HelperTest(unittest.TestCase):
  """Colour runs and the half-way wall."""

  def test_colour_runs_merge_equal_pixels(self):
    shot = Shot([row(('red', 3), ('magenta', 2), ('blue', 1))])
    self.assertEqual(vswipe_checks.colour_runs(shot, 0),
                     [('red', 0, 2), ('magenta', 3, 4), ('blue', 5, 5)])

  def test_the_half_way_wall_has_the_gap_between_the_probes(self):
    good = Shot([
        row(('green', 40), ('magenta', 20), ('red', 40)),
        row(('white', 40), ('magenta', 20), ('blue', 40)),
    ])
    self.assertIsNone(vswipe_checks.half_way_wall(good, 0, 1, 100))
    # The gap at the edge is not half way.
    edge = Shot([
        row(('green', 80), ('magenta', 20)),
        row(('white', 80), ('magenta', 20)),
    ])
    self.assertIn('gap at', vswipe_checks.half_way_wall(edge, 0, 1, 100))
    # The wrong probe halves around it.
    wrong = Shot([
        row(('red', 40), ('magenta', 20), ('green', 40)),
        row(('white', 40), ('magenta', 20), ('blue', 40)),
    ])
    self.assertIn('red | gap | green',
                  vswipe_checks.half_way_wall(wrong, 0, 1, 100))
    # No gap at all.
    none = Shot([row(('green', 100)), row(('white', 100))])
    self.assertIsNotNone(vswipe_checks.half_way_wall(none, 0, 1, 100))

if __name__ == '__main__':
  unittest.main()
