"""Tests for the pure helpers of the shm upload benchmark (bench.py) and
the parts of run.py that need no namespace."""

import json
import unittest

import bench
import run

NVIDIA = '''GPU 00000000:01:00.0
    Performance State                     : P2
    Clocks Event Reasons
        Idle                              : Not Active
    BAR1 Memory Usage
        Total                             : 256 MiB
        Used                              : 46 MiB
        Free                              : 210 MiB
    PCI
        GPU Link Info
            PCIe Generation
                Max                       : 4
                Current                   : 1
                Device Current            : 1
            Link Width
                Max                       : 16x
                Current                   : 16x
    Clocks
        Graphics                          : 210 MHz
        SM                                : 210 MHz
    Max Clocks
        Graphics                          : 2160 MHz
'''

class CaseTest(unittest.TestCase):
  """Cases and the probe options they become."""

  def test_cases_parse_and_print_again(self):
    case = bench.parse_case('1280x720:64')
    self.assertEqual(case, {'width': 1280, 'height': 720, 'damage': 64})
    self.assertEqual(bench.case_label(case), '1280x720:64')
    full = bench.parse_case('256x256:full')
    self.assertIsNone(full['damage'])
    self.assertEqual(bench.case_label(full), '256x256:full')
    self.assertIsNone(bench.parse_case('10x10')['damage'])
    for bad in ('1280', '0x10:full', '10x10:0', 'axb:full', '10x10:x'):
      with self.assertRaises(ValueError, msg=bad):
        bench.parse_case(bad)
    for case in bench.DEFAULT_CASES:
      self.assertEqual(bench.case_label(bench.parse_case(case)), case)

  def test_probe_arguments(self):
    self.assertEqual(
        bench.probe_arguments(bench.parse_case('1920x1080:full'), 60),
        ['--animate', '60', '--presentation', '--size', '1920x1080'])
    self.assertEqual(
        bench.probe_arguments(bench.parse_case('100x50:8'), 5),
        ['--animate', '5', '--presentation', '--size', '100x50',
         '--damage-square', '8'])

  def test_damage_bytes(self):
    self.assertEqual(bench.damage_bytes(bench.parse_case('10x20:full')), 800)
    self.assertEqual(bench.damage_bytes(bench.parse_case('100x50:8')), 256)
    # The probe clips the square to the buffer.
    self.assertEqual(bench.damage_bytes(bench.parse_case('30x20:64')), 1600)

class StatisticsTest(unittest.TestCase):
  """Percentiles and the summary row."""

  def test_percentile_by_nearest_rank(self):
    values = [5, 1, 4, 2, 3]
    self.assertEqual(bench.percentile(values, 0.5), 3)
    self.assertEqual(bench.percentile(values, 0.9), 5)
    self.assertEqual(bench.percentile(values, 0.0), 1)
    self.assertEqual(bench.percentile([7], 0.9), 7)
    self.assertIsNone(bench.percentile([], 0.5))

  def test_summary(self):
    row = bench.summarize([10.0, 20.0, 30.0, 40.0],
                          [(400, 5), (400, 7), (400, 9)], 0.2, None, 2.0)
    self.assertEqual(row['frames'], 4)
    self.assertEqual(row['latency_p50_ms'], 20.0)
    self.assertEqual(row['latency_p90_ms'], 40.0)
    self.assertEqual(row['latency_max_ms'], 40.0)
    self.assertEqual(row['upload_p50_us'], 7)
    self.assertEqual(row['upload_p90_us'], 9)
    self.assertEqual(row['upload_bytes'], 400)
    self.assertAlmostEqual(row['cpu_ms_per_frame'], 50.0)
    self.assertIsNone(row['outer_cpu_ms_per_frame'])
    self.assertEqual(row['fps'], 2.0)
    empty = bench.summarize([], [], 1.0, 1.0, 0.0)
    self.assertEqual(empty['frames'], 0)
    self.assertIsNone(empty['cpu_ms_per_frame'])
    self.assertIsNone(empty['fps'])

class ParsingTest(unittest.TestCase):
  """Probe events, the buffer trace and nvidia-smi."""

  def test_latencies_from_presented_events(self):
    text = '\n'.join([
        json.dumps({'event': 'presented', 'latency_us': 16500}),
        'not json',
        json.dumps({'event': 'discarded'}),
        json.dumps({'event': 'presented', 'latency_us': 'x'}),
        json.dumps({'event': 'presented', 'latency_us': 4000}),
    ])
    self.assertEqual(bench.latencies_ms(text), [16.5, 4.0])
    self.assertEqual(len(bench.events(text, 'discarded')), 1)

  def test_uploads_from_the_buffer_trace(self):
    log = '\n'.join([
        'shingle: buffer: shm upload 3686400 bytes surface=7 us=1512',
        'shingle: buffer: shm upload 16384 bytes surface=7 us=40',
        # Before upload times were traced: skipped.
        'shingle: buffer: shm upload 400 bytes surface=3',
        'shingle: buffer: dmabuf cached dmabuf=2 surface=4',
        'shingle: buffer: release buffer=9',
        'upload 5 bytes us=1',
    ])
    self.assertEqual(bench.uploads(log), [(3686400, 1512), (16384, 40)])

  def test_nvidia_state(self):
    self.assertEqual(bench.parse_nvidia_state(NVIDIA),
                     {'pstate': 'P2', 'graphics_mhz': 210, 'pcie_gen': 1,
                      'bar1_used_mib': 46})
    self.assertEqual(bench.parse_nvidia_state('No devices were found'), {})

  def test_state_during_a_case(self):
    samples = [
        {'time': 1.0, 'pstate': 'P8', 'pcie_gen': 1, 'graphics_mhz': 210},
        {'time': 3.0, 'pstate': 'P2', 'pcie_gen': 4, 'graphics_mhz': 1800},
        {'time': 5.0, 'pstate': 'P2', 'pcie_gen': 4, 'graphics_mhz': 1900},
    ]
    self.assertEqual(bench.state_during(samples, 2.0, 6.0),
                     {'pstate': 'P2', 'pcie_gen': '4',
                      'graphics_mhz': '1800-1900'})
    self.assertEqual(bench.state_during(samples, 0.5, 4.0),
                     {'pstate': 'P2/P8', 'pcie_gen': '1-4',
                      'graphics_mhz': '210-1800'})
    # No sample inside: the last one before counts.
    self.assertEqual(bench.state_during(samples, 1.5, 2.5)['pcie_gen'], '1')
    self.assertEqual(bench.state_during([], 0, 1), {})

  def test_markdown(self):
    row = {'mode': 'nested', 'variant': 'staging', 'case': '10x10:full',
           'frames': 3, 'latency_p50_ms': 16.25}
    lines = bench.markdown([row]).splitlines()
    self.assertEqual(len(lines), 3)
    self.assertTrue(lines[0].startswith('| mode | variant | case | frames'))
    self.assertIn('| nested | staging | 10x10:full | 3 | 16.2 |', lines[2])
    self.assertTrue(lines[2].endswith('| - |'))
    wide = bench.markdown([row], bench.COLUMNS + bench.GPU_COLUMNS)
    self.assertIn('pcie_gen', wide.splitlines()[0])

class OptionsTest(unittest.TestCase):
  """run.py's command line."""

  def test_defaults(self):
    options = run.parse_options([])
    self.assertEqual(options.modes, list(bench.MODES))
    self.assertEqual(options.variants, list(bench.DEFAULT_VARIANTS))
    self.assertTrue(set(bench.DEFAULT_VARIANTS) <= set(bench.VARIANTS))
    self.assertEqual(len(options.cases), len(bench.DEFAULT_CASES))
    self.assertEqual(options.frames, 120)
    self.assertFalse(options.inner)

  def test_choices(self):
    options = run.parse_options(['--modes', 'nested', '--variants',
                                 'staging', '--cases', '64x64:8',
                                 '--frames', '10'])
    self.assertEqual(options.modes, ['nested'])
    self.assertEqual(options.variants, ['staging'])
    self.assertEqual(options.cases,
                     [{'width': 64, 'height': 64, 'damage': 8}])
    for bad in (['--modes', 'native'], ['--variants', 'gl'],
                ['--cases', '64'], ['--frames', '0']):
      with self.assertRaises(SystemExit, msg=bad):
        run.parse_options(bad)

  def test_split_list(self):
    self.assertEqual(run.split_list('a,,b'), ['a', 'b'])
    self.assertEqual(run.split_list('a', ('a', 'b')), ['a'])
    with self.assertRaises(ValueError):
      run.split_list('c', ('a', 'b'))

if __name__ == '__main__':
  unittest.main()
