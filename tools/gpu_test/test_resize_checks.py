"""Tests for resize_checks.py's helpers; nothing here starts a process."""

import unittest

import resize_checks

# A WAYLAND_DEBUG excerpt as kitty writes it: the main surface, two
# configures during a drag, the buffers drawn for them, and the final
# configure acknowledged and drawn after the release at 1300 ms.
LOG = '\n'.join([
    '[1000.000] {Default Queue}  -> xdg_wm_base#9.get_xdg_surface('
    'new id xdg_surface#34, wl_surface#30)',
    '[1000.100] {Default Queue}  -> wl_surface#58.attach(wl_buffer#41, 0, 0)',
    '[1000.200] {Default Queue}  -> wl_surface#58.commit()',
    '[1100.000] {Default Queue} xdg_toplevel#35.configure(700, 682, '
    'array[16])',
    '[1100.001] {Default Queue} xdg_surface#34.configure(12)',
    '[1100.002] {Default Queue}  -> xdg_surface#34.ack_configure(12)',
    '[1100.500] {mesa egl surface queue}  -> wl_shm_pool#81.create_buffer('
    'new id wl_buffer#83, 0, 700, 682, 2800, 0)',
    '[1101.000] {mesa egl surface queue}  -> wl_surface#30.attach('
    'wl_buffer#83, 0, 0)',
    '[1101.100] {mesa egl surface queue}  -> wl_surface#30.commit()',
    '[1200.000] {Default Queue} xdg_toplevel#35.configure(800, 682, '
    'array[16])',
    '[1200.001] {Default Queue} xdg_surface#34.configure(13)',
    '[1200.500] {mesa egl surface queue}  -> wl_shm_pool#81.create_buffer('
    'new id wl_buffer#84, 0, 800, 682, 3200, 0)',
    '[1201.000] {mesa egl surface queue}  -> wl_surface#30.attach('
    'wl_buffer#84, 0, 0)',
    '[1201.100] {mesa egl surface queue}  -> wl_surface#30.commit()',
    '[1250.000]  -> wl_surface#30.commit()',
    '[1290.000] {Default Queue} xdg_toplevel#35.configure(900, 682, '
    'array[16])',
    '[1290.001] {Default Queue} xdg_surface#34.configure(14)',
    '[1340.000] {mesa egl surface queue}  -> wl_shm_pool#81.create_buffer('
    'new id wl_buffer#83, 0, 900, 682, 3600, 0)',
    '[1340.500] {mesa egl surface queue}  -> wl_surface#30.attach('
    'wl_buffer#83, 0, 0)',
    '[1341.000] {mesa egl surface queue}  -> wl_surface#30.commit()',
    'not a debug line',
])

class HelperTest(unittest.TestCase):
  """Reading configures and buffers from WAYLAND_DEBUG output."""

  def test_lines_parse_with_and_without_a_queue_name(self):
    messages = resize_checks.parse_debug(LOG)
    self.assertEqual(len(messages), 20)
    self.assertEqual(messages[14]['message'], 'commit')
    first = messages[0]
    self.assertTrue(first['request'])
    self.assertEqual((first['interface'], first['id'], first['message']),
                     ('xdg_wm_base', 9, 'get_xdg_surface'))
    configure = messages[3]
    self.assertFalse(configure['request'])
    self.assertEqual(configure['ms'], 1100.0)

  def test_only_the_xdg_surfaces_buffers_count_with_their_sizes(self):
    commits = resize_checks.main_commits(resize_checks.parse_debug(LOG))
    self.assertEqual(commits, [(1101.1, (700, 682)), (1201.1, (800, 682)),
                               (1341.0, (900, 682))])

  def test_metrics_cover_the_drag_and_the_settling_after_it(self):
    metrics = resize_checks.resize_metrics(
        resize_checks.parse_debug(LOG), 1050.0, 1300.0)
    self.assertEqual(metrics, {'configures': 3, 'buffers': 2, 'sizes': 2,
                               'final': (900, 682), 'settle_ms': 41.0})

  def test_a_final_size_never_drawn_has_no_settle_time(self):
    lines = [line for line in LOG.splitlines()
             if not line.startswith('[134')]
    metrics = resize_checks.resize_metrics(
        resize_checks.parse_debug('\n'.join(lines)), 1050.0, 1300.0)
    self.assertIsNone(metrics['settle_ms'])

  def test_wayland_time_is_realtime_microseconds_cut_to_32_bits(self):
    self.assertEqual(resize_checks.wayland_ms(1.5), 1500.0)
    self.assertEqual(resize_checks.wayland_ms((1 << 32) / 1e6 + 0.002), 2.0)

if __name__ == '__main__':
  unittest.main()
