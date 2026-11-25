"""Unit tests for the XWayland harness's pure helpers."""

import os
import signal
import subprocess
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

if __name__ == '__main__':
  unittest.main()
