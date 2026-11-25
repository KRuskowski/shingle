"""Unit tests for the XWayland harness's pure helpers."""

import os
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
    for pid in (-1, 0, 1, None, '7'):
      self.assertFalse(run.kill_xwayland(pid), pid)
    # This process is alive and not Xwayland, so it survives.
    self.assertFalse(run.kill_xwayland(os.getpid()))


if __name__ == '__main__':
  unittest.main()
