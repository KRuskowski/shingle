#!/usr/bin/env python3
"""Benchmark of shm commits in offscreen Shingle and in nested Shingle
inside offscreen Shingle, from `wl_surface.commit` to presentation, per
buffer size and damage size.

For each mode (`offscreen`, `nested`) and each upload variant
(`staging`: Shingle's staging buffers in system memory; `write_texture`:
wgpu's `Queue::write_texture`, with `SHINGLE_SHM_STAGING=0`, as before
the staging buffers; and, when asked for, `staging_device_local`:
Shingle's staging buffers in the device-local, host-visible memory of
the PCIe BAR, with `SHINGLE_SHM_STAGING=device-local`), one
Shingle (two for nested) starts with `SHINGLE_TRACE_BUFFERS=1`, and for
each case a `wl_probe --animate FRAMES --presentation --size WxH
[--damage-square SIDE]` draws FRAMES frames, one per frame callback.
A case's row has the probe's commit-to-presentation latency
(`wp_presentation`), the time each upload took in Shingle (its buffer
trace's `us=`), the bytes each uploaded, the measured Shingle's CPU time
per frame (and the outer one's in nested mode) and the frame rate.
`staging_probe` (crates/shingle-render/examples) times CPU writes into
each host-visible memory type before and after the cases, and this host
process samples `nvidia-smi` (performance state, graphics clock, PCIe
generation, BAR1 use) meanwhile, so that each row shows the GPU's state
while it ran (docs/status.md#shm-upload-speed).

Results go to results.json and results.md in the output directory
(`target/shm_bench/TIMESTAMP` by default), and the table is printed.

Everything runs inside tools/gpu_test/run.py's namespace (user, mount,
network and PID namespaces, a private /dev with only the render node and
NVIDIA's nodes, a private runtime directory with the host's hidden, the
host's display variables removed). Shingle always gets `--backend
offscreen` or `--backend nested` (checks.safe_backend), and every signal
goes through run.signal_checked to a child inside the PID namespace.

Usage, from the repository root and outside any sandbox that forbids
user namespaces, after `cargo build -p shingle-bin`, `cargo build -p
shingle --example wl_probe` and `cargo build -p shingle-render --example
staging_probe`:

  nice -n 19 tools/shm_bench/run.py [--modes offscreen,nested]
      [--variants staging,write_texture,staging_device_local]
      [--frames 120]
      [--cases 1280x720:full,1280x720:64 ...]

It lowers its own priority to 19 either way. The pure helpers are tested
by `python3 -m unittest` in this directory (test_bench.py).
"""

import argparse
import json
import os
import subprocess
import sys
import threading
import time

import bench

HERE = os.path.dirname(os.path.abspath(__file__))
GPU_TEST = os.path.join(HERE, '..', 'gpu_test')
INNER_FLAG = '--inner'
NICENESS = 19
NVIDIA_INTERVAL = 2.0
# The probe's own refusal outside a namespace.
PROBE_VARIABLES = {'SHINGLE_GPU_TEST_NAMESPACE': '1'}
# Debug builds load the Vulkan validation layer through wgpu, which
# costs CPU on every call; the soak harness leaves it out the same way.
VALIDATION_DISABLED = {'VK_LOADER_LAYERS_DISABLE': '*validation*'}

def lower_priority(niceness=NICENESS):
  """Raises this process's niceness to `niceness`; returns it."""
  current = os.nice(0)
  if current < niceness:
    current = os.nice(niceness - current)
  return current

def split_list(text, known=None):
  """The comma-separated items of `text`; ValueError for one not in
  `known`, if given."""
  items = [item for item in text.split(',') if item]
  if known is not None:
    unknown = sorted(set(items) - set(known))
    if unknown:
      raise ValueError(f'unknown {unknown}; known: {", ".join(known)}')
  return items

def parse_options(arguments):
  """The command line."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--modes', default=','.join(bench.MODES))
  parser.add_argument('--variants',
                      default=','.join(bench.DEFAULT_VARIANTS),
                      help=f'of {", ".join(bench.VARIANTS)}')
  parser.add_argument('--cases', default=','.join(bench.DEFAULT_CASES),
                      help='WIDTHxHEIGHT:DAMAGE, DAMAGE full or a side')
  parser.add_argument('--frames', type=int, default=120)
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--wl-probe', default='target/debug/examples/wl_probe')
  parser.add_argument('--staging-probe',
                      default='target/debug/examples/staging_probe')
  parser.add_argument('--output', help='directory for the results')
  parser.add_argument(INNER_FLAG, action='store_true',
                      help=argparse.SUPPRESS)
  options = parser.parse_args(arguments)
  try:
    options.modes = split_list(options.modes, bench.MODES)
    options.variants = split_list(options.variants, bench.VARIANTS)
    options.cases = [bench.parse_case(case)
                     for case in split_list(options.cases)]
  except ValueError as error:
    parser.error(str(error))
  if options.frames <= 0:
    parser.error('--frames must be positive')
  return options

def nvidia_state():
  """`bench.parse_nvidia_state` of `nvidia-smi` now, or {} without it."""
  try:
    done = subprocess.run(
        ['nvidia-smi', '-q'],
        capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL)
  except (OSError, subprocess.TimeoutExpired):
    return {}
  return bench.parse_nvidia_state(done.stdout)

class NvidiaSampler:
  """Appends `nvidia_state()` to nvidia.jsonl every NVIDIA_INTERVAL."""

  def __init__(self, path):
    self.path = path
    self.stop_event = threading.Event()
    self.thread = threading.Thread(target=self._run, daemon=True)

  def start(self):
    """Starts sampling."""
    self.thread.start()

  def stop(self):
    """Stops sampling."""
    self.stop_event.set()
    self.thread.join()

  def _run(self):
    while True:
      state = nvidia_state()
      state['time'] = time.time()
      with open(self.path, 'a') as file:
        file.write(json.dumps(state) + '\n')
      if self.stop_event.wait(NVIDIA_INTERVAL):
        break

def staging_probe(path, icd):
  """The JSON lines `staging_probe` prints on the Vulkan driver `icd`, or
  an error entry."""
  environment = dict(os.environ, VK_ICD_FILENAMES=icd, **PROBE_VARIABLES)
  try:
    done = subprocess.run([path], capture_output=True, text=True,
                          timeout=120, stdin=subprocess.DEVNULL,
                          env=environment)
  except (OSError, subprocess.TimeoutExpired) as error:
    return [{'error': str(error)}]
  lines = []
  for line in done.stdout.splitlines():
    try:
      lines.append(json.loads(line))
    except ValueError:
      continue
  if done.returncode != 0:
    lines.append({'error': done.stderr.strip()[-300:]})
  return lines

class Bench:
  """One mode and variant: its Shingle (and outer Shingle) and cases."""

  def __init__(self, checks, mode, variant, binary):
    self.checks = checks
    nvidia = {'VK_ICD_FILENAMES': checks.NVIDIA_ICD}
    measured = dict(nvidia, SHINGLE_TRACE_BUFFERS='1',
                    SHINGLE_SHM_STAGING=bench.VARIANTS[variant])
    self.outer = None
    name = f'bench-{mode}-{variant}'
    if mode == 'offscreen':
      self.shingle = checks.Shingle(binary, name, checks.BASE_CONFIG,
                                    extra_environment=measured)
    else:
      # The outer Shingle uploads nothing from shm: nested presents
      # through DMA-BUFs.
      self.outer = checks.Shingle(binary, f'{name}-outer', checks.BASE_CONFIG,
                                  extra_environment=nvidia)
      measured['WAYLAND_DISPLAY'] = self.outer.wayland_display()
      self.shingle = checks.Shingle(binary, name, checks.BASE_CONFIG,
                                    backend=('--backend', 'nested'),
                                    extra_environment=measured)

  def case(self, wl_probe, case, frames, index):
    """Runs one case; returns its summary row, with 'error' if the probe
    did not map or finish."""
    xw = self.checks.xw
    app_id = f'bench-{index}'
    log_start = len(self.shingle.log_text())
    probe = self.checks.Client(
        self.shingle, [wl_probe, app_id, *bench.probe_arguments(case, frames)],
        app_id)
    error = None
    try:
      view = xw.wait_for(lambda: self.shingle.view(app_id), timeout=30,
                         interval=0.1)
      if view is None:
        error = 'the probe did not map'
      before = self._cpu()
      started = time.monotonic()
      done = xw.wait_for(lambda: bench.events(probe.output(), 'animated'),
                         timeout=60 + frames * 2, interval=0.1)
      elapsed = time.monotonic() - started
      after = self._cpu()
      if not done and error is None:
        error = 'the probe did not finish its frames'
    finally:
      probe.stop()
    log = self.shingle.log_text()[log_start:]
    row = bench.summarize(
        bench.latencies_ms(probe.output()), bench.uploads(log),
        after[0] - before[0],
        None if self.outer is None else after[1] - before[1], elapsed)
    row['elapsed_s'] = elapsed
    if error:
      row['error'] = error
    return row

  def _cpu(self):
    """CPU seconds of the measured and the outer Shingle (None)."""
    process_cpu = self.checks.process_cpu_seconds
    outer = (None if self.outer is None
             else process_cpu(self.outer.process.pid))
    return process_cpu(self.shingle.process.pid), outer

  def stop(self):
    """Stops both Shingles; returns their exit statuses."""
    statuses = [self.shingle.stop()]
    if self.outer is not None:
      statuses.append(self.outer.stop())
    return statuses

def inner(options):
  """Runs the benchmark; called inside gpu_test's namespace."""
  sys.path.insert(0, os.path.abspath(GPU_TEST))
  import checks
  os.environ[checks.NAMESPACE_VARIABLE] = '1'
  binary = os.path.abspath(options.binary)
  wl_probe = os.path.abspath(options.wl_probe)
  results = {'started': time.strftime('%Y-%m-%d %H:%M'),
             'frames': options.frames,
             'staging_probe_before': staging_probe(options.staging_probe,
                                                   checks.NVIDIA_ICD),
             'rows': []}
  index = 0
  failed = False
  for mode in options.modes:
    for variant in options.variants:
      run = Bench(checks, mode, variant, binary)
      try:
        for case in options.cases:
          index += 1
          row = run.case(wl_probe, case, options.frames, index)
          row.update(mode=mode, variant=variant, case=bench.case_label(case),
                     time=time.time())
          failed = failed or 'error' in row
          results['rows'].append(row)
          print(bench.markdown([row]).splitlines()[-1], flush=True)
      finally:
        statuses = run.stop()
      if any(status != 0 for status in statuses):
        print(f'shm_bench: {mode} {variant} exit statuses {statuses}',
              flush=True)
        failed = True
  results['staging_probe_after'] = staging_probe(options.staging_probe,
                                                 checks.NVIDIA_ICD)
  with open(os.path.join(options.output, 'results.json'), 'w') as file:
    json.dump(results, file, indent=1)
  table = bench.markdown(results['rows'])
  with open(os.path.join(options.output, 'results.md'), 'w') as file:
    file.write(table + '\n')
  print(table, flush=True)
  for label in ('staging_probe_before', 'staging_probe_after'):
    for line in results[label]:
      print(f'{label}: {json.dumps(line)}', flush=True)
  return 1 if failed else 0

def add_nvidia_state(output):
  """Adds to each row of results.json the GPU state `nvidia-smi` showed
  while it ran (bench.state_during), and writes results.md again."""
  try:
    with open(os.path.join(output, 'results.json')) as file:
      results = json.load(file)
    with open(os.path.join(output, 'nvidia.jsonl')) as file:
      samples = [json.loads(line) for line in file if line.strip()]
  except (OSError, ValueError):
    return
  for row in results['rows']:
    row.update(bench.state_during(samples, row['time'] - row['elapsed_s'],
                                  row['time']))
  with open(os.path.join(output, 'results.json'), 'w') as file:
    json.dump(results, file, indent=1)
  table = bench.markdown(results['rows'], bench.COLUMNS + bench.GPU_COLUMNS)
  with open(os.path.join(output, 'results.md'), 'w') as file:
    file.write(table + '\n')
  print(table, flush=True)

def main(arguments=None):
  """Lowers the priority, then runs the benchmark in gpu_test's
  namespace and samples `nvidia-smi` from outside it."""
  options = parse_options(sys.argv[1:] if arguments is None else arguments)
  niceness = lower_priority()
  if options.inner:
    return inner(options)
  if niceness < NICENESS:
    print(f'shm_bench: could not lower the priority to {NICENESS}',
          flush=True)
    return 1
  output = os.path.abspath(options.output or os.path.join(
      'target', 'shm_bench', time.strftime('%Y%m%d-%H%M%S')))
  os.makedirs(output, exist_ok=True)
  command = [
      sys.executable, os.path.join(GPU_TEST, 'run.py'), '--modeset',
      '--timeout', '3600', '--',
      sys.executable, os.path.abspath(__file__), INNER_FLAG,
      '--modes', ','.join(options.modes),
      '--variants', ','.join(options.variants),
      '--cases', ','.join(bench.case_label(case) for case in options.cases),
      '--frames', str(options.frames),
      '--binary', os.path.abspath(options.binary),
      '--wl-probe', os.path.abspath(options.wl_probe),
      '--staging-probe', os.path.abspath(options.staging_probe),
      '--output', output]
  print(f'shm_bench: results into {output}', flush=True)
  sampler = NvidiaSampler(os.path.join(output, 'nvidia.jsonl'))
  sampler.start()
  try:
    status = subprocess.call(command,
                             env=dict(os.environ, **VALIDATION_DISABLED))
  finally:
    sampler.stop()
  add_nvidia_state(output)
  print(f'shm_bench: exit status {status}; results in {output}', flush=True)
  return status

if __name__ == '__main__':
  sys.exit(main())
