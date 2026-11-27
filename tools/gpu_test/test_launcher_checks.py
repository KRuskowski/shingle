"""Tests for launcher_checks.py's helpers; nothing here starts a process."""

import unittest

import launcher_checks

FRAME = {
    'event': 'frame', 'query': 'al', 'category': 'All',
    'categories': [{'title': 'Favourites', 'count': 0,
                    'rect': {'x': 10, 'y': 20, 'width': 100, 'height': 30}}],
    'rows': [{'id': 'alpha.desktop', 'name': 'Alpha Probe',
              'favourite': False, 'selected': True,
              'rect': {'x': 200, 'y': 20, 'width': 300, 'height': 40}}],
    'menu': [{'label': 'Hide',
              'rect': {'x': 5, 'y': 6, 'width': 10, 'height': 4}}],
}

class LauncherChecksTest(unittest.TestCase):

  def test_trace_lines_are_read_in_order(self):
    text = ('shingle-launcher: first frame after 41.5 ms on llvmpipe\n'
            '{"event": "frame", "query": ""}\nnot json {\n[1, 2]\n'
            '{"event": "launch", "pid": 7}\n{"event": "frame", "query": "a"}\n')
    events = launcher_checks.trace_events(text)
    self.assertEqual([event['event'] for event in events],
                     ['frame', 'launch', 'frame'])
    self.assertEqual(launcher_checks.last_frame(events)['query'], 'a')
    self.assertIsNone(launcher_checks.last_frame([]))
    self.assertEqual(launcher_checks.first_frame_ms(text), 41.5)
    self.assertIsNone(launcher_checks.first_frame_ms('nothing'))

  def test_rows_categories_and_menu_items_are_found_by_name(self):
    self.assertEqual(launcher_checks.row(FRAME, 'Alpha Probe')['id'],
                     'alpha.desktop')
    self.assertIsNone(launcher_checks.row(FRAME, 'Beta'))
    self.assertIsNone(launcher_checks.row(None, 'Beta'))
    self.assertEqual(launcher_checks.category(FRAME, 'Favourites')['count'],
                     0)
    self.assertIsNotNone(launcher_checks.menu_item(FRAME, 'Hide'))
    self.assertIsNone(launcher_checks.menu_item({'menu': None}, 'Hide'))
    self.assertEqual(
        launcher_checks.centre(FRAME['rows'][0]['rect']), (350, 40))

  def test_desktop_entries_and_summaries(self):
    text = launcher_checks.desktop_entry('A', '/x/wl_probe a', 'Utility;')
    self.assertIn('Type=Application\n', text)
    self.assertIn('Exec=/x/wl_probe a\n', text)
    self.assertEqual(launcher_checks.summary([]), 'none')
    self.assertEqual(launcher_checks.summary([30.0, 10.0, 20.0]),
                     'median 20.0 ms, 10.0-30.0 ms')

if __name__ == '__main__':
  unittest.main()
