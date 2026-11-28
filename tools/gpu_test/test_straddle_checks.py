"""Tests for straddle_checks.py's helpers; nothing here starts a process."""

import unittest

import straddle_checks

COLOURS = {
    'red': (255, 0, 0), 'green': (0, 255, 0), 'black': (26, 26, 26),
    'magenta': (255, 0, 255), 'grey': (128, 128, 128),
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
  """Colour runs, the cut quadrant and Expo's cell."""

  def test_short_runs_are_left_out(self):
    shot = Shot([row(('red', 5), ('grey', 2), ('green', 4))])
    self.assertEqual(straddle_checks.colour_runs(shot, 0),
                     [('red', 0, 4), ('green', 7, 10)])

  def test_the_quadrant_is_cut_by_the_gap(self):
    good = Shot([row(('red', 20), ('green', 50), ('magenta', 100),
                     ('green', 100), ('black', 30))])
    self.assertIsNone(straddle_checks.split_by_gap(good, 0, 50, 100))
    # Border pixels at the cut do not matter.
    bordered = Shot([row(('green', 51), ('grey', 2), ('magenta', 40),
                         ('grey', 2), ('green', 98))])
    self.assertIsNone(straddle_checks.split_by_gap(bordered, 0, 50, 100))
    # Drawn whole over the gap, as before straddling views were drawn.
    whole = Shot([row(('red', 20), ('green', 150), ('magenta', 100))])
    self.assertIn('row 0', straddle_checks.split_by_gap(whole, 0, 50, 100))
    # The second part missing or of the wrong length.
    missing = Shot([row(('green', 50), ('magenta', 100), ('black', 100))])
    self.assertIsNotNone(straddle_checks.split_by_gap(missing, 0, 50, 100))
    short = Shot([row(('green', 50), ('magenta', 100), ('green', 60))])
    self.assertIsNotNone(straddle_checks.split_by_gap(short, 0, 50, 100))

  def test_expo_cells_follow_shingles_layout(self):
    class Compositor:
      def call(self, method):
        assert method == 'window-rules/get-focused-output'
        return {'info': {'geometry': {'width': 1280, 'height': 720}}}
    cell = straddle_checks.expo_scale(Compositor())
    self.assertEqual(cell['y'], 40)
    self.assertAlmostEqual(cell['scale_x'], 580 / 1280)
    self.assertAlmostEqual(cell['scale_y'], 640 / 720)

if __name__ == '__main__':
  unittest.main()
