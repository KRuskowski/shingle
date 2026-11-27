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

  def test_desktop_entries_carry_icons_and_actions(self):
    text = launcher_checks.desktop_entry(
        'A', 'a', 'Utility;', 'a-icon',
        (('two', 'Second', 'a --two', 'two-icon'), ('three', 'Third', 'b',
                                                    None)))
    self.assertIn('Icon=a-icon\nActions=two;three;\n', text)
    self.assertIn('\n[Desktop Action two]\nName=Second\nExec=a --two\n'
                  'Icon=two-icon\n', text)
    self.assertTrue(text.endswith('[Desktop Action three]\nName=Third\n'
                                  'Exec=b\n'))

  def test_png_and_svg_fixtures(self):
    data = launcher_checks.png(3, 2, (1, 2, 3, 4))
    self.assertTrue(data.startswith(b'\x89PNG\r\n\x1a\n'))
    width, height = launcher_checks.struct.unpack('>II', data[16:24])
    self.assertEqual((width, height), (3, 2))
    self.assertIn('fill="#ffff00"', launcher_checks.svg((255, 255, 0, 255)))

  def test_device_rectangles_round_their_edges(self):
    rect = {'x': 260, 'y': 120, 'width': 760, 'height': 480}
    self.assertEqual(launcher_checks.device_rect(rect, 1.5),
                     (390, 180, 1140, 720))
    odd = {'x': 0.5, 'y': 1, 'width': 1, 'height': 1}
    self.assertEqual(launcher_checks.device_rect(odd, 1.0), (1, 1, 1, 1))
    self.assertEqual(launcher_checks.round_half_up(2.5), 3)

  def test_captures_are_compared_and_measured(self):
    Capture = launcher_checks.xw.Capture
    shot = Capture(3, 2, bytes([0, 0, 0, 255, 255, 0, 255, 255, 0,
                                0, 0, 0, 255, 255, 0, 10, 10, 10]))
    image = Capture(2, 1, bytes([255, 255, 0, 255, 255, 0]))
    self.assertEqual(launcher_checks.mismatches(shot, image, (1, 0, 2, 1)),
                     (0, 0))
    self.assertEqual(launcher_checks.mismatches(shot, image, (0, 0, 2, 1)),
                     (1, 1))
    self.assertEqual(launcher_checks.mismatches(shot, image, (0, 0, 3, 1)),
                     (3, 3))
    near = Capture(1, 1, bytes([255, 255, 20]))
    self.assertEqual(launcher_checks.mismatches(near, Capture(
        1, 1, bytes([255, 255, 0])), (0, 0, 1, 1)), (1, 0))
    self.assertTrue(launcher_checks.same_drawing((2, 0), 1000))
    self.assertFalse(launcher_checks.same_drawing((3, 0), 1000))
    self.assertFalse(launcher_checks.same_drawing((1, 1), 1000))
    self.assertEqual(launcher_checks.colour_box(shot, (0, 0, 3, 2), 'yellow'),
                     (2, 2))
    self.assertIsNone(launcher_checks.colour_box(shot, (0, 0, 3, 2), 'red'))
    frame = {'rows': [{'name': 'A', 'icon': {
        'rect': {'x': 1, 'y': 0, 'width': 2, 'height': 1}}},
                      {'name': 'B', 'icon': None}]}
    self.assertEqual(launcher_checks.icon_colours(shot, frame, ['A', 'B']),
                     {'A': 'yellow', 'B': None})

if __name__ == '__main__':
  unittest.main()
