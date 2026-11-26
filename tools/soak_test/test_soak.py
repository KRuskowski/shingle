"""Tests for the pure helpers of the soak harness (soak.py) and the
parts of run.py that need no namespace."""

import csv
import io
import os
import unittest

import run
import soak

STATUS = '''Name:\tshingle
State:\tS (sleeping)
PPid:\t4
NSpid:\t81234\t7
VmPeak:\t  912340 kB
VmRSS:\t  123456 kB
Threads:\t17
'''

STAT = ('42 (shingle (main) x) S 1 42 42 0 -1 4194560 100 0 0 0 '
        '250 150 30 20 20 0 17 0 12345 1000 50')

NVIDIA = '''GPU 00000000:01:00.0
    Processes
        GPU instance ID                   : N/A
        Process ID                        : 100
            Type                          : G
            Name                          :
            Used GPU Memory               : 72 MiB
        Process ID                        : 200
            Type                          : C+G
            Used GPU Memory               : 197 MiB
        Process ID                        : 300
            Type                          : G
            Used GPU Memory               : N/A
'''

def row(minute, process='shingle', **values):
  """A sample row at `minute` with `values`."""
  base = {'minute': minute, 'elapsed_s': minute * 60.0, 'process': process,
          'pid': 7, 'cpu_s': minute * 2.0, 'namespace_cpu_s': minute * 30.0}
  base.update(values)
  return base

class ProcTest(unittest.TestCase):
  """Reading /proc."""

  def test_status(self):
    self.assertEqual(soak.parse_status(STATUS),
                     {'rss_kib': 123456, 'threads': 17})
    self.assertEqual(soak.parse_nspid(STATUS), [81234, 7])
    self.assertEqual(soak.parse_ppid(STATUS), 4)
    self.assertEqual(soak.parse_nspid('Name:\tx\n'), [])
    self.assertIsNone(soak.parse_ppid('Name:\tx\n'))

  def test_stat_cpu_survives_parentheses_in_the_name(self):
    self.assertEqual(soak.parse_stat_cpu(STAT, 100), 4.0)
    self.assertEqual(soak.parse_stat_cpu(STAT, 100, children=True), 4.5)

  def test_loadavg_and_pause(self):
    self.assertEqual(soak.parse_loadavg('12.50 3.1 2.0 1/100 5\n'), 12.5)
    self.assertTrue(soak.should_pause(12.01))
    self.assertFalse(soak.should_pause(12.0))
    self.assertTrue(soak.should_pause(5.0, limit=4.0))

class GpuTest(unittest.TestCase):
  """GPU memory by host PID."""

  def test_nvidia_smi_processes(self):
    self.assertEqual(soak.parse_nvidia_pids(NVIDIA), {100: 72, 200: 197})

  def test_host_pid_only_below_the_root(self):
    processes = {
        10: (1, [10]),
        11: (10, [11, 1]),
        12: (11, [12, 7]),
        # Another namespace's PID 7, not below the root.
        20: (1, [20]),
        21: (20, [21, 7]),
    }
    self.assertEqual(soak.host_pid(7, processes, 10), 12)
    self.assertIsNone(soak.host_pid(7, processes, 99))
    self.assertIsNone(soak.host_pid(8, processes, 10))

class LatencyTest(unittest.TestCase):
  """Latency statistics."""

  def test_percentiles(self):
    values = list(range(1, 101))
    self.assertEqual(soak.percentile(values, 0.5), 50)
    self.assertEqual(soak.percentile(values, 0.99), 99)
    self.assertEqual(soak.latency_summary(values), (50, 99, 100))
    self.assertEqual(soak.latency_summary([]), (None, None, None))
    self.assertIsNone(soak.percentile([], 0.5))

class TrendTest(unittest.TestCase):
  """The trend test."""

  def test_flat_noise_is_not_flagged(self):
    values = [100000 + (37 * index) % 5000 for index in range(40)]
    result = soak.trend(values, (8192, 0.05))
    self.assertTrue(result['judged'])
    self.assertFalse(result['flagged'])

  def test_steady_growth_is_flagged(self):
    values = [100000 + 1000 * index for index in range(40)]
    result = soak.trend(values, (8192, 0.05))
    self.assertTrue(result['flagged'])
    self.assertGreater(result['growth'], 8192)
    self.assertAlmostEqual(result['slope_per_hour'], 60000.0)

  def test_warmup_growth_is_left_out(self):
    values = [10, 50, 90, 100, 100] + [100] * 30
    self.assertFalse(soak.trend(values, (3, 0.0))['flagged'])

  def test_one_late_spike_is_not_flagged(self):
    values = [100] * 30 + [400]
    self.assertFalse(soak.trend(values, (3, 0.0))['flagged'])

  def test_a_leaked_object_each_cycle_is_flagged(self):
    values = [5 + index for index in range(20)]
    self.assertTrue(soak.trend(values, soak.COUNT_TOLERANCE)['flagged'])

  def test_short_series_are_not_judged(self):
    result = soak.trend([1, 2, 3, 4, 5, 6, 7, 8], (0, 0.0))
    self.assertFalse(result['judged'])
    self.assertFalse(result['flagged'])

  def test_missing_values_are_skipped(self):
    values = [None] * 10 + [100] * 10
    result = soak.trend(values, (3, 0.0))
    self.assertEqual(result['samples'], 10)
    self.assertFalse(result['flagged'])

  def test_relative_tolerance(self):
    # 4 % growth of 1 GiB is below the 5 % tolerance, though above 8 MiB.
    values = [1 << 20] * 10 + [int((1 << 20) * 1.04)] * 10
    self.assertFalse(soak.trend(values, (8192, 0.05), warmup=0)['flagged'])

  def test_slope(self):
    self.assertAlmostEqual(soak.least_squares_slope([1, 3, 5, 7]), 2.0)
    self.assertEqual(soak.least_squares_slope([4]), 0.0)

  def test_tolerances(self):
    self.assertEqual(soak.tolerance_for('rss_kib'), (8192, 0.05))
    self.assertEqual(soak.tolerance_for('count_surfaces'),
                     soak.COUNT_TOLERANCE)
    self.assertIsNone(soak.tolerance_for('count_ipc_clients'))
    self.assertIsNone(soak.tolerance_for('cpu_s'))

  def test_trends_by_process_and_flagged(self):
    rows = []
    for minute in range(20):
      rows.append(row(minute, rss_kib=50000, fds=40 + minute,
                      count_surfaces=1))
      rows.append(row(minute, 'outer', rss_kib=60000, fds=30,
                      count_surfaces=1))
    judged = soak.trends(rows)
    self.assertEqual(set(judged), {'shingle', 'outer'})
    self.assertNotIn('gpu_mib', judged['shingle'])
    self.assertEqual([(process, column) for process, column, _
                      in soak.flagged(judged)], [('shingle', 'fds')])

class CsvTest(unittest.TestCase):
  """The samples CSV."""

  def test_header_values_and_counts(self):
    rows = [row(0, rss_kib=10, count_surfaces=1, latency_p50_ms=1.25),
            row(1, rss_kib=11, count_buffers=2)]
    parsed = list(csv.DictReader(io.StringIO(soak.csv_text(rows))))
    self.assertEqual(len(parsed), 2)
    self.assertEqual(parsed[0]['rss_kib'], '10')
    self.assertEqual(parsed[0]['latency_p50_ms'], '1.250')
    self.assertEqual(parsed[0]['count_buffers'], '')
    self.assertEqual(parsed[1]['count_buffers'], '2')
    header = soak.csv_text(rows).splitlines()[0].split(',')
    self.assertEqual(header[:len(soak.BASE_COLUMNS)],
                     list(soak.BASE_COLUMNS))
    self.assertEqual(header[len(soak.BASE_COLUMNS):],
                     ['count_buffers', 'count_surfaces'])

  def test_flatten_counts(self):
    reply = {'result': 'ok', 'clients': 2, 'surfaces': 3, 'flag': True,
             'frontend': {'textures': 4, 'name': 'x'}}
    self.assertEqual(soak.flatten_counts(reply),
                     {'count_clients': 2, 'count_surfaces': 3,
                      'count_frontend_textures': 4})
    self.assertEqual(soak.flatten_counts(None), {})

class SummaryTest(unittest.TestCase):
  """The Markdown summary."""

  def rows(self, leak=False):
    return [row(minute, rss_kib=50000 + (minute * 2000 if leak else 0),
                fds=40, latency_p50_ms=8.0, latency_p99_ms=16.0 + minute)
            for minute in range(20)]

  def test_a_clean_run_passes(self):
    rows = self.rows()
    text = soak.summary_markdown('offscreen', {'duration': '20 min'}, rows,
                                 soak.trends(rows), [], {'cycles': 30})
    self.assertIn('# Soak run: offscreen', text)
    self.assertIn('Result: **PASS**', text)
    self.assertIn('- cycles: 30', text)
    self.assertIn('mean CPU of everything in the namespace: 0.50 cores',
                  text)
    self.assertIn('| rss_kib | 50000 | 50000 |', text)
    self.assertIn('highest per-minute p99 35.0 ms', text)
    self.assertIn('median 2.00 s a minute', text)
    self.assertNotIn('## Growth', text)

  def test_growth_and_failures_fail(self):
    rows = self.rows(leak=True)
    text = soak.summary_markdown('headless', {}, rows, soak.trends(rows),
                                 ['cycle 3: wl_probe error: x'], {})
    self.assertIn('Result: **FAIL**', text)
    self.assertIn('## Growth', text)
    self.assertIn('- shingle rss_kib:', text)
    self.assertIn('## Failures', text)
    self.assertIn('- cycle 3: wl_probe error: x', text)

  def test_mean_cores_needs_two_rows(self):
    self.assertIsNone(soak.mean_cores([row(0)]))
    self.assertAlmostEqual(soak.mean_cores([row(0), row(2)]), 0.5)

class RunTest(unittest.TestCase):
  """run.py's options and priority."""

  def test_options(self):
    options = run.parse_options(['--mode', 'nested', '--minutes', '2'])
    self.assertEqual((options.mode, options.minutes, options.load_limit),
                     ('nested', 2.0, 12.0))
    self.assertFalse(options.inner)
    self.assertEqual(options.steps, list(run.STEPS))
    with self.assertRaises(SystemExit):
      run.parse_options(['--mode', 'native'])
    options = run.parse_options(['--mode', 'offscreen', '--steps',
                                 'windows,vkcube'])
    self.assertEqual(options.steps, ['windows', 'vkcube'])
    with self.assertRaises(SystemExit):
      run.parse_options(['--mode', 'offscreen', '--steps', 'windows,dance'])
    with self.assertRaises(SystemExit):
      run.parse_options(['--mode', 'attach', '--ipc', '/tmp/ipc'])

  def test_validation_is_off_unless_asked(self):
    self.assertEqual(run.without_validation({'A': '1'}, False),
                     {'A': '1', 'VK_LOADER_LAYERS_DISABLE': '*validation*'})
    self.assertEqual(run.without_validation({'A': '1'}, True), {'A': '1'})

  def test_lower_priority_never_raises_it(self):
    before = os.nice(0)
    self.assertGreaterEqual(run.lower_priority(before), before)

if __name__ == '__main__':
  unittest.main()
