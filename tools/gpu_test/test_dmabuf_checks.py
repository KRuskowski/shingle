"""Tests for the pure helpers of tools/gpu_test/dmabuf_checks.py."""

import os
import unittest
from unittest import mock

import dmabuf_checks

WAYLAND_INFO = """\
interface: 'wl_shm',                                     version:  2, name:  9
\tformats (fourcc):
\t         1 = 'XR24'
interface: 'zwp_linux_dmabuf_v1',                        version:  5, name: 50
\tmain device: 0xE280
\ttranche
\t\ttarget device: 0xE280
\t\tflags: none
\t\tformats (fourcc) and modifiers (names):
\t\t0x34325241 = 'AR24'; 0x0300000000606015 = NVIDIA_BLOCK_LINEAR_2D,HEIGHT=5
\t\t0x34325258 = 'XR24'; 0x0000000000000000 = LINEAR
interface: 'wp_linux_drm_syncobj_manager_v1',            version:  1, name: 51
interface: 'xdg_wm_base',                                version:  6, name: 45
"""

class WaylandInfoTest(unittest.TestCase):
  """What wayland-info says of linux-dmabuf."""

  def test_version_device_and_pairs(self):
    version, device, pairs = dmabuf_checks.dmabuf_global(WAYLAND_INFO)
    self.assertEqual((version, device), (5, 0xE280))
    self.assertEqual(pairs, [('AR24', 0x0300000000606015), ('XR24', 0)])

  def test_the_syncobj_global(self):
    self.assertEqual(dmabuf_checks.syncobj_global(WAYLAND_INFO), 1)
    self.assertIsNone(dmabuf_checks.syncobj_global("interface: 'wl_shm', "
                                                   "version: 2, name: 9\n"))

  def test_no_global(self):
    self.assertEqual(dmabuf_checks.dmabuf_global("interface: 'wl_shm', "
                                                 "version: 2, name: 9\n"),
                     (None, None, []))

  def test_the_expected_pairs_are_four_formats_by_seven_modifiers(self):
    pairs = dmabuf_checks.expected_pairs()
    self.assertEqual(len(pairs), 28)
    self.assertIn(('XB24', 0), pairs)
    self.assertIn(('AR24', 0x0300000000606010), pairs)

class TraceTest(unittest.TestCase):
  """The buffer trace in Shingle's log."""

  LOG = '\n'.join([
      'shingle: rendering offscreen on NVIDIA GeForce RTX 3060',
      'shingle: buffer: dmabuf import dmabuf=1 64x32 XR24 modifier=0x0',
      'shingle: buffer: dmabuf import dmabuf=2 64x32 XR24 modifier=0x0',
      'shingle: buffer: dmabuf cached dmabuf=1 surface=3',
      'shingle: buffer: dmabuf cached dmabuf=1 surface=3',
      'shingle: buffer: shm upload 4096 bytes surface=4',
      'shingle: buffer: blocked implicit 250 µs dmabuf=2',
      'shingle: buffer: release wl_buffer@12',
      'shingle: buffer: blocked syncobj 900 µs point=4 dmabuf=1',
      'shingle: buffer: blocked syncobj 100 µs point=5 dmabuf=2',
      'shingle: buffer: blocked syncobj 300 µs point=6 dmabuf=1',
      'shingle: buffer: syncobj acquire ready point=7 dmabuf=2',
      'shingle: buffer: syncobj release point=3',
      'shingle: buffer: something new',
  ])

  def test_events_and_fields(self):
    events = dmabuf_checks.trace(self.LOG)
    self.assertEqual([kind for kind, _ in events],
                     ['import', 'import', 'cached', 'cached', 'shm',
                      'blocked', 'release', 'syncobj_wait', 'syncobj_wait',
                      'syncobj_wait', 'syncobj_ready', 'syncobj_release'])
    self.assertEqual(events[4][1]['amount'], 4096)
    self.assertEqual(events[5][1], {'dmabuf': '2', 'amount': 250})
    self.assertEqual(events[7][1], {'point': '4', 'dmabuf': '1',
                                    'amount': 900})

  def test_summary(self):
    summary = dmabuf_checks.summarise(dmabuf_checks.trace(self.LOG))
    self.assertEqual(summary['imports'], {'1': 1, '2': 1})
    self.assertEqual(summary['cached'], {'1'})
    self.assertEqual((summary['shm'], summary['blocked'],
                      summary['releases']), (1, 1, 1))
    self.assertIn('2 buffers imported', dmabuf_checks.describe(summary))
    self.assertEqual((summary['syncobj_waits'], summary['syncobj_ready'],
                      summary['syncobj_releases']), (3, 1, 1))
    self.assertEqual(dmabuf_checks.syncobj_acquires(summary), 4)
    self.assertIn('3 acquire waits (300 µs median)',
                  dmabuf_checks.describe(summary))

class SyncobjCheckTest(unittest.TestCase):
  """Judging syncobj use, the modes and probe output."""

  class Recorder:
    """Records checks."""

    def __init__(self):
      self.checks = []

    def check(self, name, passed, detail=''):
      self.checks.append((name, bool(passed)))
      return passed

  def summary(self, waits, ready, releases):
    summary = dmabuf_checks.summarise([])
    summary.update(syncobj_waits=waits, syncobj_ready=ready,
                   syncobj_releases=releases)
    return summary

  def test_on_needs_acquires_and_releases_and_off_needs_none(self):
    on, off = dmabuf_checks.Mode(), dmabuf_checks.Mode(syncobj=False)
    recorder = self.Recorder()
    for mode, summary in ((on, self.summary(3, 1, 2)),
                          (on, self.summary(0, 4, 2)),
                          (on, self.summary(0, 0, 0)),
                          (on, self.summary(2, 0, 0)),
                          (off, self.summary(0, 0, 0)),
                          (off, self.summary(0, 1, 0))):
      dmabuf_checks.syncobj_check(recorder, mode, 'x', summary)
    self.assertEqual([passed for _, passed in recorder.checks],
                     [True, True, False, False, True, False])

  def test_modes(self):
    off = dmabuf_checks.Mode(syncobj=False)
    self.assertEqual(off.name('dmabuf'), 'dmabuf-nosyncobj')
    self.assertEqual(off.environment({'A': '1'}),
                     {'SHINGLE_TRACE_BUFFERS': '1', 'SHINGLE_SYNCOBJ': '0',
                      'A': '1'})
    self.assertEqual(dmabuf_checks.Mode().name('dmabuf'), 'dmabuf')
    self.assertNotIn('SHINGLE_SYNCOBJ', dmabuf_checks.Mode().environment())

  def test_prefixed_results(self):
    recorder = self.Recorder()
    dmabuf_checks.Prefixed(recorder, 'p: ').check('name', True)
    self.assertEqual(recorder.checks, [('p: name', True)])

  def test_probe_events(self):
    output = ('noise\n{"event": "mapped"}\n'
              '{"event": "live", "frames": 90, "released": []}\n')
    self.assertEqual(dmabuf_checks.probe_event(output, 'live')['frames'], 90)
    self.assertIsNone(dmabuf_checks.probe_event(output, 'destroyed'))

class FakeShot:
  """A capture whose pixels come from a function."""

  def __init__(self, width, height, pixel):
    self.width = width
    self.height = height
    self.pixel = pixel

class PixelTest(unittest.TestCase):
  """Drawn, changed and coloured shares of a view."""

  GEOMETRY = {'x': 0, 'y': 0, 'width': 64, 'height': 64}

  def test_drawn_counts_what_is_not_background(self):
    half = FakeShot(64, 64, lambda x, y: (255, 0, 0) if x < 32 else
                    (26, 26, 26))
    self.assertAlmostEqual(dmabuf_checks.drawn(half, self.GEOMETRY), 0.5)
    self.assertEqual(dmabuf_checks.drawn(None, self.GEOMETRY), 0.0)
    self.assertEqual(dmabuf_checks.drawn(half, None), 0.0)

  def test_differs_and_colour_share(self):
    red = FakeShot(64, 64, lambda x, y: (255, 0, 0))
    green = FakeShot(64, 64, lambda x, y: (0, 255, 0))
    self.assertEqual(dmabuf_checks.differs(red, red, self.GEOMETRY), 0)
    self.assertEqual(dmabuf_checks.differs(red, green, self.GEOMETRY), 256)
    self.assertEqual(
        dmabuf_checks.colour_share(green, self.GEOMETRY, 'green'), 1.0)
    self.assertEqual(
        dmabuf_checks.colour_share(red, self.GEOMETRY, 'green'), 0.0)

class ProfileTest(unittest.TestCase):
  """Browser profiles are throwaway and private."""

  def test_user_js(self):
    text = dmabuf_checks.firefox_user_js(('a.b', 'false', 'c', '"x"'))
    self.assertEqual(text, 'user_pref("a.b", false);\n'
                     'user_pref("c", "x");\n')

  def test_a_profile_outside_the_private_home_is_refused(self):
    with mock.patch.dict(os.environ, {'HOME': '/home/someone'}):
      with self.assertRaises(RuntimeError):
        dmabuf_checks.private_profile('firefox')

if __name__ == '__main__':
  unittest.main()
