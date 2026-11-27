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
round_trip = _load('round_trip.py')

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

  def test_shares_are_read_from_version_2_sets(self):
    root = {'direction': 'vertical', 'children': [
        {'weight': 1, 'node': leaf(1)}, {'weight': 1, 'node': leaf(2)}]}
    saved = {'version': 2, 'wsets': [{'wset': 1, 'workspaces': [
        {'root': root}]}]}
    self.assertEqual(run.saved_shares(saved, 1, 2), (0.5, 0.5))

  def test_a_floating_window_has_no_share(self):
    root = {'direction': 'vertical', 'children': [
        {'weight': 1, 'node': leaf(1)}]}
    self.assertIsNone(run.saved_shares(session(root), 1, 2))
    self.assertIsNone(run.saved_shares({}, 1, 2))

class RoundTripTest(unittest.TestCase):
  """The pure helpers of round_trip.py."""

  def test_geometries_are_compared_in_their_own_workspace(self):
    tile = {'x': 2560, 'y': 0, 'width': 1280, 'height': 720}
    self.assertEqual(round_trip.workspace_column(tile, 1280), 2)
    self.assertEqual(round_trip.shifted(tile, -2560)['x'], 0)
    snapshot = {'views': [{'id': 5, 'geometry': {
        'x': 2660, 'y': 80, 'width': 400, 'height': 300}}],
                'layouts': [{'layout': {'geometry': tile,
                                        'vertical-split': []}}]}
    own = round_trip.own_workspace(snapshot, 1280)
    self.assertEqual(own['views'][0]['geometry']['x'], 100)
    self.assertEqual(own['layouts'][0]['layout']['geometry']['x'], 0)
    # Already in their own workspace, as master reported them.
    self.assertEqual(round_trip.own_workspace(own, 1280), own)
    self.assertEqual(snapshot['views'][0]['geometry']['x'], 2660)

  def test_the_tile_layout_nests_bravo_over_charlie(self):
    views = dict(zip(round_trip.APPS, range(1, 6)))
    layout = round_trip.tile_layout(views)['layout']['vertical-split']
    self.assertEqual([child['weight'] for child in layout], [3, 2, 1])
    self.assertEqual(layout[1]['horizontal-split'],
                     [{'view-id': 2, 'weight': 1},
                      {'view-id': 3, 'weight': 2}])

  def test_view_state_keeps_only_session_fields_by_id(self):
    views = [{'id': 2, 'pid': 7, 'activated': True, 'app-id': 'b'},
             {'id': 1, 'pid': 8, 'last-focus-timestamp': 5}]
    state = round_trip.view_state(views)
    self.assertEqual([view['id'] for view in state], [1, 2])
    self.assertNotIn('pid', state[0])
    self.assertNotIn('activated', state[1])
    self.assertEqual(state[1]['app-id'], 'b')

  def test_trees_and_floating_are_read_from_either_version(self):
    floating = {'x': 1, 'y': 2, 'width': 3, 'height': 4}
    root = {'direction': 'vertical', 'children': [
        {'weight': 1, 'node': {'kind': 'split', 'children': [
            {'weight': 1, 'node': leaf(1)},
            {'weight': 1, 'node': {'kind': 'leaf', 'window': 2,
                                   'state': {'floating': floating}}}]}}]}
    workspaces = [{'root': root}]
    for saved in ({'outputs': [{'workspaces': workspaces}]},
                  {'wsets': [{'workspaces': workspaces}]}):
      self.assertEqual(round_trip.session_trees(saved), [workspaces])
      self.assertEqual(round_trip.session_floating(saved, 2), floating)
      self.assertIsNone(round_trip.session_floating(saved, 1))
    self.assertEqual(round_trip.session_trees({}), [])
    saved = {'wsets': [{'workspaces': [
        {'workspace': {'x': 0, 'y': 0}, 'root': root, 'focused': 1,
         'stack': [1, 2]}]}]}
    self.assertEqual(round_trip.session_roots(saved),
                     [[({'x': 0, 'y': 0}, root)]])

if __name__ == '__main__':
  unittest.main()
