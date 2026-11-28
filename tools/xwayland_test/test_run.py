"""Unit tests for the XWayland harness's pure helpers."""

import os
import shutil
import signal
import struct
import subprocess
import tempfile
import unittest

import run

class FramingTest(unittest.TestCase):
  """Wayfire IPC framing."""

  def test_round_trip(self):
    data = run.encode_message({'method': 'list-methods'})
    data += run.encode_message({'b': 2})
    messages, rest = run.decode_messages(data + data[:3])
    self.assertEqual(messages, [{'method': 'list-methods'}, {'b': 2}])
    self.assertEqual(rest, data[:3])

  def test_incomplete_frame_waits(self):
    data = run.encode_message({'a': 1})
    self.assertEqual(run.decode_messages(data[:-1]), ([], data[:-1]))

class PathPointsTest(unittest.TestCase):
  """The pointer path of a drag."""

  def test_points_end_at_the_target(self):
    points = run.path_points((0, 0), (100, 50), 4)
    self.assertEqual(points, [(25, 12), (50, 25), (75, 38), (100, 50)])

  def test_one_step_jumps(self):
    self.assertEqual(run.path_points((3, 4), (5, 6), 1), [(5, 6)])

class XpropTest(unittest.TestCase):
  """Parsing xprop output."""

  def test_atoms_and_windows(self):
    text = ('_NET_SUPPORTED(ATOM) = _NET_WM_STATE, _NET_ACTIVE_WINDOW\n'
            '_NET_SUPPORTING_WM_CHECK(WINDOW): window id # 0x200000\n')
    self.assertEqual(run.parse_xprop(text), {
        '_NET_SUPPORTED': '_NET_WM_STATE, _NET_ACTIVE_WINDOW',
        '_NET_SUPPORTING_WM_CHECK': 'window id # 0x200000',
    })

class SessionTest(unittest.TestCase):
  """The restored-client session."""

  def test_one_window_with_the_command(self):
    session = run.session_json(['sh', '-c', 'true'], '/w')
    self.assertEqual(session['version'], 1)
    window = session['windows'][0]
    self.assertEqual(window['identity']['command'], ['sh', '-c', 'true'])
    workspace = session['outputs'][0]['workspaces'][0]
    self.assertEqual(workspace['stack'], [window['id']])

class WaitTest(unittest.TestCase):
  """Where a slow exit waits, from /proc."""

  def test_names_the_kernel_function_and_system_call(self):
    self.assertEqual(
        run.describe_wait('folio_wait_bit_common', '74 0x5 0x0 0x0\n'),
        'folio_wait_bit_common, system call 74')
    self.assertEqual(run.describe_wait('0', 'running\n'),
                     'nothing (running)')
    self.assertEqual(run.describe_wait('', ''), '?, system call ?')

  def test_reads_a_live_and_a_missing_process(self):
    with subprocess.Popen(['sleep', '30']) as child:
      try:
        deadline = run.time.monotonic() + 5
        while (run.blocked_in(child.pid) or '').startswith('nothing'):
          self.assertLess(run.time.monotonic(), deadline)
          run.time.sleep(0.01)
        self.assertIn('system call', run.blocked_in(child.pid))
      finally:
        child.kill()
    self.assertIsNone(run.blocked_in(child.pid))

class KillTest(unittest.TestCase):
  """kill_xwayland refuses anything but an Xwayland process."""

  def test_refuses_special_and_foreign_pids(self):
    for pid in (-1, 0, 1, None, '7', True):
      self.assertFalse(run.kill_xwayland(pid), pid)
    # This process is alive and not Xwayland, so it survives.
    self.assertFalse(run.kill_xwayland(os.getpid()))

class SignalCheckedTest(unittest.TestCase):
  """signal_checked signals only a live process with the expected comm."""

  def test_refuses_special_pids_and_other_programs(self):
    for pid in (-1, 0, 1, None, '7', True):
      self.assertFalse(run.signal_checked(pid, signal.SIGTERM, 'sleep'), pid)
    self.assertFalse(
        run.signal_checked(os.getpid(), signal.SIGTERM, 'xterm'))

  def test_signals_its_own_child(self):
    child = subprocess.Popen(['sleep', '30'])
    try:
      self.assertFalse(run.signal_checked(child.pid, signal.SIGTERM, 'xterm'))
      self.assertTrue(run.signal_checked(child.pid, signal.SIGTERM, 'sleep'))
      self.assertEqual(child.wait(5), -signal.SIGTERM)
    finally:
      if child.poll() is None:
        child.kill()
        child.wait()

  def test_long_names_compare_truncated(self):
    child = subprocess.Popen(['sleep', '30'])
    try:
      self.assertFalse(
          run.signal_checked(child.pid, 0, 'sleep-but-longer-than-15'))
    finally:
      child.kill()
      child.wait()

class XParsingTest(unittest.TestCase):
  """Parsing xwininfo and xprop window lists."""

  def test_xwininfo_geometry(self):
    text = (
        'xwininfo: Window id: 0x400022 "user@host: ~"\n\n'
        '  Absolute upper-left X:  640\n'
        '  Absolute upper-left Y:  0\n'
        '  Relative upper-left X:  640\n'
        '  Width: 640\n'
        '  Height: 720\n')
    self.assertEqual(run.parse_xwininfo(text),
                     {'x': 640, 'y': 0, 'width': 640, 'height': 720})
    self.assertIsNone(run.parse_xwininfo('X Error of failed request'))

  def test_window_lists_and_classes(self):
    self.assertEqual(
        run.parse_window_list('window id # 0x400022, 0x600008'),
        [0x400022, 0x600008])
    self.assertEqual(run.parse_window_list(''), [])
    self.assertEqual(run.parse_wm_class('"xterm", "XTerm"'), 'XTerm')
    self.assertEqual(run.parse_wm_class('not set'), '')

class TilesTest(unittest.TestCase):
  """tiles_fill and same_geometry."""

  AREA = {'x': 0, 'y': 0, 'width': 100, 'height': 50}

  def test_exact_cover(self):
    left = {'x': 0, 'y': 0, 'width': 60, 'height': 50}
    right = {'x': 60, 'y': 0, 'width': 40, 'height': 50}
    self.assertTrue(run.tiles_fill([left, right], self.AREA))

  def test_gaps_overlaps_and_overflow_fail(self):
    left = {'x': 0, 'y': 0, 'width': 60, 'height': 50}
    self.assertFalse(run.tiles_fill([left], self.AREA))
    overlap = {'x': 50, 'y': 0, 'width': 50, 'height': 50}
    gap = {'x': 0, 'y': 0, 'width': 10, 'height': 50}
    self.assertFalse(run.tiles_fill([left, overlap, gap], self.AREA))
    outside = {'x': 60, 'y': 10, 'width': 40, 'height': 50}
    filler = {'x': 60, 'y': 0, 'width': 40, 'height': 0}
    self.assertFalse(run.tiles_fill([left, outside, filler], self.AREA))

  def test_same_geometry_ignores_other_keys(self):
    self.assertTrue(run.same_geometry(
        {'x': 1, 'y': 2, 'width': 3, 'height': 4, 'synthetic': True},
        {'x': 1, 'y': 2, 'width': 3, 'height': 4}))
    self.assertFalse(run.same_geometry({'x': 1}, {'x': 2}))

  def test_the_client_is_inside_wayfires_default_frame(self):
    self.assertEqual(
        run.inside_frame({'x': 200, 'y': 150, 'width': 500, 'height': 400}),
        {'x': 204, 'y': 184, 'width': 492, 'height': 362})

class ChildrenTest(unittest.TestCase):
  """Parsing `xwininfo -root -children`."""

  def test_children_come_bottom_to_top(self):
    text = ('xwininfo: Window id: 0x3ef (the root window) (has no name)\n'
            '\n  Root window id: 0x3ef (the root window) (has no name)\n'
            '  Parent window id: 0x0 (none)\n'
            '     3 children:\n'
            '     0x40002c (has no name): ()  218x446+104+270  +104+270\n'
            '     0x40000c "sh": ("xterm" "XTerm")  426x720+0+0  +0+0\n'
            '     0x200001 (has no name): ()  10x10+0+0  +0+0\n')
    self.assertEqual(run.parse_children(text),
                     [0x200001, 0x40000c, 0x40002c])

class XcursorTest(unittest.TestCase):
  """Reading Xcursor files."""

  def write(self, images):
    """An Xcursor file with (nominal, width, height, xhot, yhot) images."""
    header = struct.pack('<4sIII', b'Xcur', 16, 0x10000, len(images))
    position = 16 + 12 * len(images)
    toc = b''
    chunks = b''
    for nominal, width, height, xhot, yhot in images:
      toc += struct.pack('<III', 0xfffd0002, nominal, position)
      chunk = struct.pack('<IIIIIIIII', 36, 0xfffd0002, nominal, 1, width,
                          height, xhot, yhot, 0)
      chunk += b'\0' * (width * height * 4)
      chunks += chunk
      position += len(chunk)
    path = os.path.join(self.directory, 'cursor')
    with open(path, 'wb') as file:
      file.write(header + toc + chunks)
    return path

  def setUp(self):
    self.directory = tempfile.mkdtemp()

  def tearDown(self):
    shutil.rmtree(self.directory)

  def test_the_nearest_size_wins(self):
    path = self.write([(16, 16, 16, 1, 1), (24, 24, 24, 3, 1),
                       (32, 32, 32, 4, 2)])
    self.assertEqual(run.xcursor_image(path, 24), (24, 24, 3, 1))
    self.assertEqual(run.xcursor_image(path, 30), (32, 32, 4, 2))
    # The first of two equally near sizes, as the xcursor crate picks.
    self.assertEqual(run.xcursor_image(path, 20), (16, 16, 1, 1))

  def test_bad_files_give_none(self):
    path = os.path.join(self.directory, 'bad')
    with open(path, 'wb') as file:
      file.write(b'nope')
    self.assertIsNone(run.xcursor_image(path, 24))
    self.assertIsNone(run.xcursor_image(path + '-missing', 24))

class CaptureTest(unittest.TestCase):
  """PPM screenshots."""

  def test_ppm_parses_and_pixels_match(self):
    pixels = bytes([255, 0, 0, 0, 0, 255, 255, 0, 0,
                    10, 10, 10, 255, 0, 0, 0, 0, 255])
    parsed = run.parse_ppm(b'P6\n# grim\n3 2\n255\n' + pixels)
    self.assertEqual(parsed, (3, 2, pixels))
    shot = run.Capture(*parsed)
    self.assertEqual(shot.pixel(1, 0), (0, 0, 255))
    self.assertEqual(shot.matching(run.red), (3, (0, 0, 2, 1)))
    self.assertEqual(shot.matching(run.blue), (2, (1, 0, 2, 1)))
    rect = {'x': 0, 'y': 1, 'width': 2, 'height': 1}
    self.assertEqual(shot.matching(run.red, rect), (1, (1, 1, 1, 1)))
    self.assertEqual(shot.matching(run.green), (0, None))

  def test_bad_ppm_gives_none(self):
    self.assertIsNone(run.parse_ppm(b'P5\n1 1\n255\n\0'))
    self.assertIsNone(run.parse_ppm(b'P6\n2 2\n255\n\0\0\0'))

  def test_saved_captures_round_trip_in_order(self):
    directory = tempfile.mkdtemp()
    try:
      os.environ[run.CAPTURES_VARIABLE] = directory
      run._saved_captures[0] = 0
      first = run.Capture(2, 1, bytes([1, 2, 3, 4, 5, 6]))
      run.save_capture(first)
      run.save_capture(None)
      run.save_capture(run.Capture(1, 1, bytes([7, 8, 9])))
      self.assertEqual(sorted(os.listdir(directory)),
                       ['000.ppm', '001.ppm'])
      with open(os.path.join(directory, '000.ppm'), 'rb') as file:
        self.assertEqual(run.parse_ppm(file.read()),
                         (2, 1, first.pixels))
    finally:
      del os.environ[run.CAPTURES_VARIABLE]
      shutil.rmtree(directory)

class KeysTest(unittest.TestCase):
  """Typing through STIPC."""

  def test_text_becomes_evdev_names(self):
    sent = []
    shingle = run.Shingle.__new__(run.Shingle)
    shingle.press = lambda key, pressed: sent.append((key, pressed))
    shingle.type_text('a b\n')
    self.assertEqual(sent, [
        ('KEY_A', True), ('KEY_A', False), ('KEY_SPACE', True),
        ('KEY_SPACE', False), ('KEY_B', True), ('KEY_B', False),
        ('KEY_ENTER', True), ('KEY_ENTER', False)])

class SelectionTest(unittest.TestCase):
  """The selection group's payloads and options."""

  def test_payloads_have_their_size_and_differ_by_seed(self):
    for size in (run.SMALL_PAYLOAD, run.LARGE_PAYLOAD, 1, 64, 65):
      self.assertEqual(len(run.payload(size, 3)), size)
    self.assertNotEqual(run.payload(10, 1), run.payload(10, 2))
    self.assertEqual(run.payload(10, 1), run.payload(10, 1))
    large = run.payload(run.LARGE_PAYLOAD, 5)
    self.assertTrue(all(32 <= byte < 127 or byte == 10 for byte in large))
    self.assertIn(b'\n', large)

  def test_labels(self):
    self.assertEqual(run.payload_label(10), '10 bytes')
    self.assertEqual(run.payload_label(1 << 20), '1 MiB')
    self.assertEqual(run.payload_label(8 << 20), '8 MiB')
    self.assertEqual(run.payload_label((1 << 20) + 1), '1048577 bytes')

  def test_selection_options(self):
    self.assertEqual(run.selection_options('clipboard'),
                     (['-selection', 'clipboard'], []))
    self.assertEqual(run.selection_options('primary'),
                     (['-selection', 'primary'], ['--primary']))
    with self.assertRaises(ValueError):
      run.selection_options('secondary')

  def test_compare_names_the_first_difference(self):
    self.assertEqual(run.compare(b'abc', b'abc'), '3 bytes')
    self.assertEqual(run.compare(None, b'abc'), 'nothing')
    self.assertEqual(run.compare(b'abd', b'abc'),
                     '3 bytes, expected 3, first difference at 2')
    self.assertEqual(run.compare(b'ab', b'abc'),
                     '2 bytes, expected 3, first difference at 2')

  def test_helpers_are_stopped_through_signal_checked(self):
    helper = run.Helper(['sleep', '30'], dict(os.environ), 'sleep')
    helper.stop()
    self.assertEqual(helper.process.returncode, -signal.SIGTERM)
    # One whose program name does not match is not signalled.
    other = run.Helper(['sleep', '30'], dict(os.environ), 'xclip')
    self.assertFalse(
        run.signal_checked(other.pid, signal.SIGTERM, other.program))
    self.assertIsNone(other.process.poll())
    other.process.kill()
    other.process.wait()

  def test_helpers_get_their_data_on_stdin(self):
    with tempfile.TemporaryFile() as out:
      helper = run.Helper(['cat'], dict(os.environ), 'cat', b'data',
                          stdout=out)
      self.assertEqual(helper.wait(10), 0)
      out.seek(0)
      self.assertEqual(out.read(), b'data')
    failing = run.Helper(['sh', '-c', 'echo oops >&2'], dict(os.environ),
                         'sh')
    failing.wait(10)
    self.assertEqual(failing.errors(), 'oops')

class ListMonitorsTest(unittest.TestCase):
  """Parsing `xrandr --listmonitors`."""

  def test_monitors_and_positions(self):
    text = ('Monitors: 2\n'
            ' 0: +XWAYLAND0 1280/310x720/175+0+0  XWAYLAND0\n'
            ' 1: +XWAYLAND1 1024/310x768/175-1024+200  XWAYLAND1\n')
    self.assertEqual(run.parse_listmonitors(text), [
        ('XWAYLAND0', (0, 0, 1280, 720)),
        ('XWAYLAND1', (-1024, 200, 1024, 768)),
    ])
    self.assertEqual(run.parse_listmonitors('Monitors: 0\n'), [])

class HostRuntimeTest(unittest.TestCase):
  """Passing the host uid into the namespace."""

  def test_the_host_uid_is_added_to_the_environment(self):
    environment = run.host_uid_environment({'PATH': '/bin'})
    self.assertEqual(environment, {
        'PATH': '/bin',
        run.HOST_UID_VARIABLE: str(os.getuid()),
    })

  def test_nothing_is_mounted_without_a_numeric_uid(self):
    saved = os.environ.pop(run.HOST_UID_VARIABLE, None)
    try:
      self.assertIsNone(run.hide_host_runtime())
      os.environ[run.HOST_UID_VARIABLE] = '1000; rm -rf /'
      self.assertIsNone(run.hide_host_runtime())
    finally:
      os.environ.pop(run.HOST_UID_VARIABLE, None)
      if saved is not None:
        os.environ[run.HOST_UID_VARIABLE] = saved

class HidpiTest(unittest.TestCase):
  """The `hidpi` group's conversions and checkerboard count."""

  def test_coordinates_round_half_away_from_zero_like_rust(self):
    self.assertEqual(run.to_x(201, 1.5), 302)
    self.assertEqual(run.to_x(-201, 1.5), -302)
    self.assertEqual(run.to_x(10, 2), 20)
    self.assertEqual(run.from_x(301, 1.5), 201)
    self.assertEqual(run.from_x(301, 2), 151)

  def test_rectangles_convert_by_their_edges(self):
    rect = {'x': 640, 'y': 0, 'width': 641, 'height': 720}
    self.assertEqual(run.rect_to_x(rect, 1.5),
                     {'x': 960, 'y': 0, 'width': 962, 'height': 1080})
    self.assertEqual(
        run.rect_from_x({'x': 301, 'y': 211, 'width': 120, 'height': 90},
                        1.5),
        {'x': 201, 'y': 141, 'width': 80, 'height': 60})
    self.assertEqual(run.box_of({'x': 1, 'y': 2, 'width': 3, 'height': 4}),
                     (1, 2, 3, 5))

  def test_checkerboard_fraction(self):
    width, height = 16, 12
    pixels = bytearray()
    for y in range(height):
      for x in range(width):
        pixels += bytes([0, 0, 0] if (x + y) % 2 == 0 else [255] * 3)
    shot = run.Capture(width, height, bytes(pixels))
    rect = {'x': 0, 'y': 0, 'width': width, 'height': height}
    self.assertEqual(run.checker_fraction(shot, rect, inset=1, step=1), 1.0)
    shifted = dict(rect, x=1)
    self.assertEqual(
        run.checker_fraction(shot, shifted, inset=1, step=1), 0.0)
    grey = run.Capture(width, height, bytes([128] * width * height * 3))
    self.assertEqual(run.checker_fraction(grey, rect, inset=1, step=1), 0.0)

if __name__ == '__main__':
  unittest.main()
