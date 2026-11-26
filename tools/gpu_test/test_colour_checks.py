"""Tests for colour_checks.py's helpers; nothing here starts a process."""

import os
import tempfile
import unittest

import colour_checks
import wayfire_reference

class Shot:
  """A capture of `outside` with a rectangle of `inside`."""

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
    return self.inside if inside else self.outside

class HelperTest(unittest.TestCase):
  """The sampled points, the Wayland socket lookup and Wayfire's uid."""

  def test_the_corners_show_the_background(self):
    shot = Shot(100, 80, (1, 2, 3), colour_checks.BACKGROUND,
                {'x': 10, 'y': 10, 'width': 50, 'height': 50})
    self.assertEqual(colour_checks.background_pixels(shot),
                     {colour_checks.BACKGROUND})
    covered = Shot(100, 80, (1, 2, 3), colour_checks.BACKGROUND,
                   {'x': 0, 'y': 0, 'width': 5, 'height': 5})
    self.assertEqual(colour_checks.background_pixels(covered),
                     {colour_checks.BACKGROUND, (1, 2, 3)})
    self.assertIsNone(colour_checks.background_pixels(None))

  def test_frame_points_are_on_the_borders(self):
    geometry = {'x': 20, 'y': 10, 'width': 60, 'height': 40}
    inner = {'x': 24, 'y': 14, 'width': 52, 'height': 32}
    shot = Shot(100, 80, (0, 0, 0), colour_checks.FRAME, inner)
    self.assertEqual(colour_checks.frame_pixels(shot, geometry),
                     {colour_checks.FRAME})
    self.assertIsNone(colour_checks.frame_pixels(None, geometry))

  def test_the_wayland_socket_is_not_its_lock(self):
    with tempfile.TemporaryDirectory() as runtime:
      self.assertIsNone(wayfire_reference.wayland_socket(runtime))
      open(os.path.join(runtime, 'wayland-1.lock'), 'w').close()
      self.assertIsNone(wayfire_reference.wayland_socket(runtime))
      open(os.path.join(runtime, 'wayland-1'), 'w').close()
      self.assertEqual(wayfire_reference.wayland_socket(runtime),
                       os.path.join(runtime, 'wayland-1'))

  def test_wayfire_never_runs_as_root(self):
    saved = os.environ.get('SHINGLE_GPU_TEST_UID')
    try:
      for value, expected in (('1234', '1234'), ('0', '1000'),
                              ('-1', '1000'), ('x', '1000')):
        os.environ['SHINGLE_GPU_TEST_UID'] = value
        self.assertEqual(wayfire_reference.unprivileged_uid(), expected)
      del os.environ['SHINGLE_GPU_TEST_UID']
      self.assertEqual(wayfire_reference.unprivileged_uid(), '1000')
    finally:
      if saved is not None:
        os.environ['SHINGLE_GPU_TEST_UID'] = saved

if __name__ == '__main__':
  unittest.main()
