"""Tests of the pure helpers of app_checks.py."""

import unittest
import zlib

import app_checks
import checks

xw = checks.xw

def solid(width, height, colour):
  """A capture of one colour."""
  return xw.Capture(width, height, bytes(colour) * (width * height))

def painted(width, height, background, rect, colour):
  """A capture of `background` with `rect` filled with `colour`."""
  pixels = bytearray(bytes(background) * (width * height))
  for y in range(rect['y'], rect['y'] + rect['height']):
    for x in range(rect['x'], rect['x'] + rect['width']):
      offset = (y * width + x) * 3
      pixels[offset:offset + 3] = bytes(colour)
  return xw.Capture(width, height, bytes(pixels))

class KeysTest(unittest.TestCase):
  """Typing on a US layout."""

  def test_letters_digits_and_symbols(self):
    self.assertEqual(app_checks.key_sequence('aB1:'), [
        ('KEY_A', False), ('KEY_B', True), ('KEY_1', False),
        ('KEY_SEMICOLON', True)])
    self.assertEqual(app_checks.key_sequence('<a b="c">\n')[-1],
                     ('KEY_ENTER', False))

  def test_unknown_characters_are_refused(self):
    with self.assertRaises(ValueError):
      app_checks.key_sequence('é')

  def test_chords(self):
    self.assertEqual(app_checks.chord_keys('ctrl+shift+comma'),
                     ['KEY_LEFTCTRL', 'KEY_LEFTSHIFT', 'KEY_COMMA'])
    self.assertEqual(app_checks.chord_keys('ctrl+o'),
                     ['KEY_LEFTCTRL', 'KEY_O'])
    self.assertEqual(app_checks.chord_keys('esc'), ['KEY_ESC'])
    self.assertEqual(app_checks.chord_keys('f'), ['KEY_F'])
    self.assertEqual(app_checks.chord_keys('alt+f4'),
                     ['KEY_LEFTALT', 'KEY_F4'])
    with self.assertRaises(ValueError):
      app_checks.chord_keys('ctrl+nope')

class DebugLogTest(unittest.TestCase):
  """Reading WAYLAND_DEBUG logs."""

  LOG = '\n'.join([
      '[1.000] {Default Queue}  -> xdg_surface#30.get_popup(new id '
      'xdg_popup#40, xdg_surface#20, xdg_positioner#39)',
      '[2.000] {Default Queue} xdg_popup#40.popup_done()',
      '[3.000]  -> xdg_popup#40.destroy()',
      '[4.000] wl_display#1.error(xdg_wm_base#7, 4, "bad popup")',
  ])

  def test_requests_and_events(self):
    self.assertEqual(
        app_checks.debug_requests(self.LOG, 'xdg_surface', 'get_popup'), 1)
    self.assertEqual(
        app_checks.debug_requests(self.LOG, 'xdg_popup', 'destroy'), 1)
    self.assertEqual(
        app_checks.debug_events(self.LOG, 'xdg_popup', 'popup_done'), 1)
    self.assertEqual(
        app_checks.debug_events(self.LOG, 'xdg_popup', 'destroy'), 0)

  def test_protocol_errors(self):
    self.assertEqual(app_checks.protocol_errors(self.LOG),
                     ['xdg_wm_base#7, 4, "bad popup"'])
    self.assertEqual(app_checks.protocol_errors('fine\n'), [])

  def test_crash_lines(self):
    self.assertEqual(app_checks.crash_lines('a\nSegmentation fault\nb'),
                     ['Segmentation fault'])
    self.assertEqual(app_checks.crash_lines('all good'), [])

class CaptureTest(unittest.TestCase):
  """Judging captures."""

  VIEW = {'x': 10, 'y': 10, 'width': 40, 'height': 30}

  def test_blank(self):
    self.assertTrue(app_checks.blank(solid(64, 48, (0, 0, 0)), self.VIEW))
    dot = {'x': 20, 'y': 20, 'width': 3, 'height': 3}
    shot = painted(64, 48, (0, 0, 0), dot, (255, 255, 255))
    self.assertFalse(app_checks.blank(shot, self.VIEW))

  def test_stale_edges(self):
    # The client drew only the left half of its view.
    drawn = dict(self.VIEW, width=20)
    shot = painted(64, 48, app_checks.BACKGROUND, drawn, (200, 200, 200))
    self.assertEqual(app_checks.stale_edges(shot, self.VIEW), ['right'])
    full = painted(64, 48, app_checks.BACKGROUND, self.VIEW, (200, 200, 200))
    self.assertEqual(app_checks.stale_edges(full, self.VIEW), [])

  def test_misplaced_skips_views(self):
    output = [(0, 0, 64, 48)]
    shot = painted(64, 48, app_checks.BACKGROUND,
                   {'x': 0, 'y': 0, 'width': 64, 'height': 48}, (1, 200, 3))
    gaps = {'top': 4, 'right': 4, 'bottom': 4, 'left': 4}
    self.assertEqual(len(app_checks.misplaced(shot, output, gaps=gaps)), 4)
    whole = {'x': 0, 'y': 0, 'width': 64, 'height': 48}
    self.assertEqual(app_checks.misplaced(shot, output, [whole], gaps), [])
    clean = solid(64, 48, app_checks.BACKGROUND)
    self.assertEqual(app_checks.misplaced(clean, output, gaps=gaps), [])

  def test_changed_share(self):
    before = solid(32, 32, (0, 0, 0))
    half = {'x': 0, 'y': 0, 'width': 16, 'height': 32}
    after = painted(32, 32, (0, 0, 0), half, (9, 9, 9))
    whole = {'x': 0, 'y': 0, 'width': 32, 'height': 32}
    self.assertAlmostEqual(app_checks.changed_share(before, after, whole),
                           0.5)
    self.assertEqual(app_checks.changed_share(None, after, whole), 0.0)

  def test_views_are_placed_in_the_layout_by_their_output(self):
    outputs = {'HEADLESS-1': {'id': 1, 'rect': (0, 0, 1280, 720)},
               'HEADLESS-2': {'id': 2, 'rect': (1280, 0, 1024, 768)}}
    geometry = {'x': 10, 'y': 20, 'width': 30, 'height': 40}
    self.assertEqual(app_checks.in_layout(geometry, 2, outputs),
                     dict(geometry, x=1290))
    self.assertEqual(app_checks.in_layout(geometry, 1, outputs), geometry)
    # A parked view's output id is -1; it stays where IPC put it.
    self.assertEqual(app_checks.in_layout(geometry, -1, outputs), geometry)

  def test_centred_on(self):
    area = {'x': 0, 'y': 0, 'width': 1000, 'height': 800}
    parent = {'x': 100, 'y': 100, 'width': 600, 'height': 400}
    child = {'x': 300, 'y': 250, 'width': 200, 'height': 100}
    self.assertTrue(app_checks.centred_on(child, parent, area))
    self.assertFalse(app_checks.centred_on(dict(child, x=0 + 50), parent,
                                           area))
    # Pushed against the output's edge.
    self.assertTrue(app_checks.centred_on(dict(child, x=0), parent, area))

class AppsTest(unittest.TestCase):
  """The swept applications' settings."""

  def test_only_vlc_has_an_orphan_dialog(self):
    orphans = [app.name for app in app_checks.APPS if app.orphan_dialog]
    self.assertEqual(orphans, ['vlc'])

class LoadTest(unittest.TestCase):
  """Waiting for the load."""

  def test_waits_until_under_the_limit(self):
    loads = iter(['13.5 1 1 1/1 1', '12.0 1 1 1/1 1', '3.2 1 1 1/1 1'])
    slept = []
    load = app_checks.wait_for_load(12, sleep=slept.append,
                                    read=lambda: next(loads))
    self.assertEqual(load, 3.2)
    self.assertEqual(len(slept), 2)

class FilesTest(unittest.TestCase):
  """The documents the applications open."""

  def test_pdf_has_a_valid_cross_reference(self):
    pdf = app_checks.minimal_pdf()
    self.assertTrue(pdf.startswith(b'%PDF-1.4'))
    start = int(pdf.rsplit(b'startxref\n', 1)[1].split(b'\n')[0])
    self.assertTrue(pdf[start:].startswith(b'xref'))
    for number in range(1, 6):
      offset = pdf.index(b'%d 0 obj' % number)
      self.assertIn(b'%010d 00000 n' % offset, pdf)

  def test_png_decodes(self):
    png = app_checks.minimal_png(4, 2, (1, 2, 3))
    self.assertTrue(png.startswith(b'\x89PNG'))
    start = png.index(b'IDAT') + 4
    length = int.from_bytes(png[start - 8:start - 4], 'big')
    rows = zlib.decompress(png[start:start + length])
    self.assertEqual(rows, (b'\x00' + bytes((1, 2, 3)) * 4) * 2)

class ProcessTest(unittest.TestCase):
  """Choosing which processes to end."""

  def test_new_pids_never_include_low_or_kept_pids(self):
    self.assertEqual(app_checks.new_pids([1, 5], [-1, 0, 1, 5, 7, 9, 12],
                                         {9}), [7, 12])

  def test_descendants(self):
    parents = {10: 1, 11: 10, 12: 11, 13: 1, 14: 12}
    self.assertEqual(app_checks.descendants(10, parents), {10, 11, 12, 14})

class UserTest(unittest.TestCase):
  """Running applications as the caller."""

  def test_as_user_maps_back_to_the_callers_uid(self):
    self.assertEqual(app_checks.as_user(['vlc', '-q'], '1234'), [
        'unshare', '--user', '--map-user=1234', '--map-group=1234', '--',
        'vlc', '-q'])

  def test_root_is_never_the_uid(self):
    self.assertEqual(app_checks.unprivileged_uid(
        {'SHINGLE_GPU_TEST_UID': '0'}), '1000')
    self.assertEqual(app_checks.unprivileged_uid(
        {'SHINGLE_GPU_TEST_UID': '1001'}), '1001')

class MainViewTest(unittest.TestCase):
  """Choosing an application's main window."""

  def test_the_largest_view_without_a_parent(self):
    def view(view_id, width, parent=-1):
      return {'id': view_id, 'parent': parent,
              'geometry': {'width': width, 'height': 100}}
    views = [view(1, 300), view(2, 900, parent=1), view(3, 500)]
    self.assertEqual(app_checks.main_view(views)['id'], 3)
    self.assertEqual(app_checks.main_view([view(2, 9, parent=1)])['id'], 2)

class WorkspaceTest(unittest.TestCase):

  def test_shifted_moves_only_the_corner(self):
    geometry = {'x': 24, 'y': 24, 'width': 600, 'height': 400}
    self.assertEqual(app_checks.shifted(geometry, dx=1280),
                     {'x': 1304, 'y': 24, 'width': 600, 'height': 400})
    self.assertEqual(geometry['x'], 24)

  def test_workspace_column_follows_the_centre(self):
    area = {'x': 0, 'y': 0, 'width': 1280, 'height': 720}
    view = {'x': 24, 'y': 24, 'width': 600, 'height': 400}
    self.assertEqual(app_checks.workspace_column(view, area), 0)
    self.assertEqual(app_checks.workspace_column(
        app_checks.shifted(view, dx=1280), area), 1)
    self.assertEqual(app_checks.workspace_column(
        app_checks.shifted(view, dx=-1280), area), -1)
    # A centre just past the edge is the next workspace's.
    self.assertEqual(app_checks.workspace_column(
        {'x': 1000, 'y': 0, 'width': 600, 'height': 10}, area), 1)

class TableTest(unittest.TestCase):
  """The per-app table."""

  def test_rows_follow_the_steps(self):
    text = app_checks.table({'kitty': {'maps': 'ok', 'popup': 'n/a'}},
                            steps=('maps', 'popup', 'dialog'))
    self.assertEqual(text.splitlines(), [
        '| App | maps | popup | dialog |', '|---|---|---|---|',
        '| kitty | ok | n/a |  |'])

class GroupsTest(unittest.TestCase):
  """The groups run.py knows."""

  def test_run_lists_every_group(self):
    import run
    self.assertEqual(set(app_checks.GROUPS), set(run.APP_GROUPS))
    self.assertEqual(set(app_checks.GROUPS), set(app_checks.CHECKS))

if __name__ == '__main__':
  unittest.main()
