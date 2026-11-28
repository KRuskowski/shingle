"""Unit tests for the pure helpers of the native test tools.

They never create a device or open a socket, so they are safe on any host.
"""

import os
import tempfile
import unittest

import ipc
import present_check
import uinput

class FramingTest(unittest.TestCase):
  """Wayfire IPC framing."""

  def test_round_trip(self):
    data = ipc.encode_message({'method': 'input/list-devices', 'data': {}})
    reply, rest = ipc.decode_message(data + b'xy')
    self.assertEqual(reply, {'method': 'input/list-devices', 'data': {}})
    self.assertEqual(rest, b'xy')

  def test_incomplete_frames_wait(self):
    data = ipc.encode_message({'a': 1})
    self.assertEqual(ipc.decode_message(data[:-1]), (None, data[:-1]))
    self.assertEqual(ipc.decode_message(data[:2]), (None, data[:2]))

  def test_usage_without_arguments(self):
    self.assertEqual(ipc.main([]), 2)

class GuardTest(unittest.TestCase):
  """The test-machine guard of uinput.py."""

  def test_marker_decides(self):
    with tempfile.TemporaryDirectory() as directory:
      marker = os.path.join(directory, 'marker')
      self.assertFalse(uinput.on_test_machine(marker))
      with open(marker, 'w', encoding='utf-8'):
        pass
      self.assertTrue(uinput.on_test_machine(marker))

  def test_no_device_without_the_marker(self):
    if uinput.on_test_machine():
      self.skipTest('this is the test machine')
    self.assertEqual(uinput.main(['key', 'KEY_A']), 1)

class ArgumentsTest(unittest.TestCase):
  """uinput.py's command line."""

  def test_commands_and_their_devices(self):
    chord = uinput.parse_arguments(
        ['key', 'KEY_LEFTCTRL', 'KEY_LEFTALT', 'KEY_F2'])
    self.assertEqual(chord.keys, ['KEY_LEFTCTRL', 'KEY_LEFTALT', 'KEY_F2'])
    self.assertEqual(uinput.device_kind(chord.command), 'keyboard')
    held = uinput.parse_arguments(['--settle', '2', 'key', 'KEY_A',
                                   '--hold', '15'])
    self.assertEqual((held.settle, held.hold), (2.0, 15.0))
    self.assertEqual(uinput.device_kind('wheel'), 'mouse')
    self.assertEqual(uinput.device_kind('touch'), 'touch')
    device = uinput.parse_arguments(['device', 'touch', '--seconds', '3'])
    self.assertEqual((device.kind, device.seconds), ('touch', 3.0))

  def test_touch_positions(self):
    self.assertEqual(uinput.touch_position(0.0), 0)
    self.assertEqual(uinput.touch_position(1.0), uinput.TOUCH_MAX)
    self.assertEqual(uinput.touch_position(0.5), 2048)
    with self.assertRaises(ValueError):
      uinput.touch_position(1.5)

class PresentCheckTest(unittest.TestCase):
  """present_check.py's comparison of feedback with page flips."""

  LOG = [
      'shingle: output HDMI-A-1: 1920x1080 at 60.000 Hz',
      'shingle: page flip seq=10 at 100.016000000 s',
      'shingle: page flip seq=11 at 100.032666000 s',
  ]

  def events(self, presented_nsec=16_000_400, frame_ms=100_016, seq=10,
             flags=7):
    return [
        {'event': 'commit', 'n': 0, 'monotonic_ns': 100_010_000_000},
        {'event': 'frame', 'n': 0, 'time_ms': frame_ms},
        {'event': 'presented', 'n': 0, 'sec': 100, 'nsec': presented_nsec,
         'refresh': 16_666_666, 'seq': seq, 'flags': flags},
    ]

  def test_flips_are_parsed_from_the_log(self):
    self.assertEqual(present_check.parse_flips(self.LOG),
                     {10: 100_016_000_000, 11: 100_032_666_000})

  def test_flips_of_one_output_are_told_apart(self):
    log = [
        'shingle: HDMI-A-1: page flip seq=10 at 100.016000000 s',
        'shingle: DP-1: page flip seq=10 at 100.020000000 s',
        'shingle: DP-1: page flip seq=11 at 100.027000000 s',
    ]
    self.assertEqual(present_check.parse_flips(log, 'DP-1'),
                     {10: 100_020_000_000, 11: 100_027_000_000})
    self.assertEqual(present_check.parse_flips(log, 'HDMI-A-1'),
                     {10: 100_016_000_000})
    self.assertEqual(present_check.parse_flips(self.LOG, 'DP-1'), {})

  def test_matching_feedback_passes(self):
    flips = present_check.parse_flips(self.LOG)
    problems, statistics = present_check.check(flips, self.events())
    self.assertEqual(problems, [])
    self.assertEqual(statistics['max_difference_ns'], 400)
    self.assertEqual(statistics['presented'], 1)
    self.assertIsNone(statistics['median_frame_interval_ms'])

  def test_frame_intervals_are_summarised(self):
    events = [{'event': 'frame', 'n': n, 'time_ms': time}
              for n, time in enumerate([100_016, 100_032, 100_049, 100_066])]
    flips = {n: time * 1_000_000 for n, time in enumerate(
        [100_016, 100_032, 100_049, 100_066])}
    _, statistics = present_check.check(flips, events)
    self.assertEqual(statistics['median_frame_interval_ms'], 17)

  def test_feedback_off_by_more_than_a_millisecond_fails(self):
    flips = present_check.parse_flips(self.LOG)
    problems, _ = present_check.check(
        flips, self.events(presented_nsec=17_100_000))
    self.assertEqual(len(problems), 1)
    self.assertIn('from its flip', problems[0])

  def test_callbacks_must_come_from_flips_after_the_commit(self):
    flips = present_check.parse_flips(self.LOG)
    problems, _ = present_check.check(flips, self.events(frame_ms=100_005))
    self.assertIn('frame 0: callback at 100005 ms is no flip time', problems)
    self.assertIn('frame 0: callback before its commit', problems)

  def test_unknown_sequences_and_missing_flags_fail(self):
    flips = present_check.parse_flips(self.LOG)
    problems, _ = present_check.check(flips, self.events(seq=12, flags=1))
    self.assertEqual(len(problems), 2)

  def test_zero_copy_is_counted_and_can_be_required(self):
    flips = present_check.parse_flips(self.LOG)
    events = self.events(flags=0xf)
    problems, statistics = present_check.check(flips, events)
    self.assertEqual((problems, statistics['zero_copy']), ([], 1))
    problems, _ = present_check.check(flips, events, 'zero-copy')
    self.assertEqual(problems, [])
    problems, _ = present_check.check(flips, events, 'copy')
    self.assertEqual(problems, ['frame 0: presented as zero-copy'])
    problems, statistics = present_check.check(
        flips, self.events(), 'zero-copy')
    self.assertEqual(problems, ['frame 0: not presented as zero-copy'])
    self.assertEqual(statistics['zero_copy'], 0)

  def test_async_flips_can_be_required(self):
    log = self.LOG + ['shingle: page flip seq=12 at 100.040000000 s async']
    flips = present_check.parse_flips(log)
    self.assertEqual(flips[12], 100_040_000_000, 'an async line is a flip')
    torn = self.events(flags=0x6)
    problems, statistics = present_check.check(flips, torn)
    self.assertEqual(problems, ['frame 0: flags 0x6'])
    self.assertEqual(statistics['tearing'], 1)
    problems, _ = present_check.check(flips, torn, tearing=True)
    self.assertEqual(problems, [])
    # A synchronized flip before the first async one is counted, one
    # after it fails.
    problems, statistics = present_check.check(
        flips, self.events(), tearing=True)
    self.assertEqual((problems, statistics['sync_before_async']), ([], 1))
    synced_after = torn + [dict(event, n=1) for event in self.events()
                           if event['event'] == 'presented']
    problems, _ = present_check.check(flips, synced_after, tearing=True)
    self.assertEqual(problems, ['frame 1: flags 0x7, not an async flip'])

  def test_async_flips_may_share_a_vblank(self):
    log = self.LOG + ['shingle: page flip seq=12 at 100.040000000 s async']
    flips = present_check.parse_flips(log)
    first = self.events(flags=0x6, seq=12)
    second = [dict(event, n=1) for event in first]
    events = first + second
    problems, _ = present_check.check(flips, events)
    self.assertIn('frame 1: seq 12 after 12', problems)
    problems, _ = present_check.check(flips, events, tearing=True)
    self.assertNotIn('frame 1: seq 12 after 12', problems)
    # The callback of an async flip carries the vblank's time, which can
    # be before the commit.
    early = self.events(flags=0x6, frame_ms=100_005)
    problems, _ = present_check.check(flips, early, tearing=True)
    self.assertNotIn('frame 0: callback before its commit', problems)

  def test_variable_refresh_can_be_required(self):
    flips = present_check.parse_flips(self.LOG)
    events = self.events()
    problems, statistics = present_check.check(flips, events, vrr=True)
    self.assertEqual(
        problems, ['frame 0: refresh 16666666, not 0 for variable refresh'])
    self.assertEqual(statistics['variable_refresh'], 0)
    events[-1]['refresh'] = 0
    problems, statistics = present_check.check(flips, events, vrr=True)
    self.assertEqual((problems, statistics['variable_refresh']), ([], 1))

  def test_options_are_parsed(self):
    args = present_check.parse_args(
        ['--output', 'DP-1', '--scanout', 'copy', 'log', 'probe'])
    self.assertEqual((args.output, args.scanout, args.native_log,
                      args.probe_output), ('DP-1', 'copy', 'log', 'probe'))
    self.assertIsNone(present_check.parse_args(['--scanout', 'gbm', 'a', 'b']))
    args = present_check.parse_args(['--tearing', '--vrr', 'log', 'probe'])
    self.assertEqual((args.tearing, args.vrr), (True, True))
    self.assertIsNone(present_check.parse_args(['only-one']))

  def test_no_feedback_fails(self):
    problems, _ = present_check.check({}, [])
    self.assertEqual(problems, ['no presented feedback'])
    self.assertEqual(present_check.main([]), 2)

if __name__ == '__main__':
  unittest.main()
