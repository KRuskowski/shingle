"""Tests for the pure helpers of tools/session_test/run.py."""

import importlib.util
import os
import unittest

def _load(name):
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)), name)
  spec = importlib.util.spec_from_file_location(name[:-3], path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

run = _load('run.py')

def leaf(window):
  """A saved leaf node for `window`."""
  return {'kind': 'leaf', 'window': window, 'state': {}}

def session(root):
  """A saved session with `root` as its only workspace's root split."""
  return {'outputs': [{'workspaces': [{'root': root}]}]}

class FsyncLogTest(unittest.TestCase):
  """Reading slowfs's log."""

  def test_events_are_parsed_with_paths_containing_spaces(self):
    events = run.parse_events('begin 1.5 /a b\nend 2.25 /a b\n')
    self.assertEqual(events, [('begin', 1.5, '/a b'), ('end', 2.25, '/a b')])

  def test_intervals_pair_begin_and_end_and_leave_the_last_open(self):
    events = [('begin', 1.0, '/x'), ('end', 2.0, '/x'), ('begin', 3.0, '/x')]
    self.assertEqual(run.fsync_intervals(events), [(1.0, 2.0), (3.0, None)])

  def test_overlapping_counts_open_intervals_and_touching_ends(self):
    intervals = [(0.0, 1.0), (1.5, 2.0), (2.5, None), (5.0, 6.0)]
    self.assertEqual(run.overlapping(intervals, 1.0, 3.0),
                     [(0.0, 1.0), (1.5, 2.0), (2.5, None)])
    self.assertEqual(run.overlapping(intervals, 7.0, 8.0), [(2.5, None)])

class LayoutTest(unittest.TestCase):
  """Building set-layout requests and reading the saved layout."""

  def test_request_splits_two_views_by_percent(self):
    request = run.layout_request('vertical-split', 4, 9, 30)
    self.assertEqual(request['layout'], {'vertical-split': [
        {'view-id': 4, 'weight': 30}, {'view-id': 9, 'weight': 70}]})
    self.assertEqual(request['workspace'], {'x': 0, 'y': 0})

  def test_request_without_percent_tiles_the_first_view_alone(self):
    request = run.layout_request('horizontal-split', 4, 9, None)
    self.assertEqual(request['layout'],
                     {'horizontal-split': [{'view-id': 4, 'weight': 1}]})

  def test_root_direction_follows_the_reply(self):
    self.assertEqual(run.root_direction(
        {'layout': {'horizontal-split': []}}), 'horizontal-split')
    self.assertEqual(run.root_direction({'layout': None}), 'vertical-split')

  def test_steps_alternate_floating_and_end_at_23_percent(self):
    steps = run.layout_steps(30)
    self.assertEqual(len(steps), 30)
    self.assertEqual(steps[-1], 23)
    self.assertTrue(all(step is None for step in steps[1:-1:2]))
    self.assertTrue(all(0 < step < 100 for step in steps[::2]))

  def test_shares_of_the_two_windows_in_a_root_split(self):
    root = {'direction': 'vertical', 'children': [
        {'weight': 23000000, 'node': leaf(1)},
        {'weight': 77000000, 'node': leaf(2)}]}
    self.assertEqual(run.saved_shares(session(root), 1, 2), (0.23, 0.77))
    self.assertIsNone(run.saved_shares(session(root), 2, 1))

  def test_shares_are_found_in_a_nested_split(self):
    inner = {'kind': 'split', 'direction': 'horizontal', 'children': [
        {'weight': 1, 'node': leaf(1)}, {'weight': 3, 'node': leaf(2)}]}
    root = {'direction': 'vertical', 'children': [
        {'weight': 1, 'node': inner}]}
    self.assertEqual(run.saved_shares(session(root), 1, 2), (0.25, 0.75))

  def test_a_floating_window_has_no_share(self):
    root = {'direction': 'vertical', 'children': [
        {'weight': 1, 'node': leaf(1)}]}
    self.assertIsNone(run.saved_shares(session(root), 1, 2))
    self.assertIsNone(run.saved_shares({}, 1, 2))

if __name__ == '__main__':
  unittest.main()
