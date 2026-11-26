#!/usr/bin/env python3
"""Soak test of headless, offscreen and nested Shingle in a private
namespace, with a repeatable churn of real clients.

The run happens inside tools/gpu_test/run.py's namespace: an
unprivileged user, mount, network and PID namespace with a private /tmp,
HOME, XDG directories and runtime directory (the host's /run/user/<uid>
hidden), WAYLAND_DISPLAY, WAYLAND_SOCKET, DISPLAY and WAYFIRE_SOCKET
removed, and a private /dev with only the basic nodes, the render node,
NVIDIA's nodes (with nvidia-modeset, for vkcube's swapchain) and
udmabuf: never a card node, /dev/input or a TTY. gpu_test fails the run
if any process opens another device.

Modes:

- `headless`: `shingle --headless`.
- `offscreen`: `shingle --backend offscreen` on NVIDIA.
- `nested`: `shingle --backend nested` on NVIDIA in a window of an
  offscreen Shingle; both are measured, the nested one gets the clients.
- `attach`: a Shingle that already runs, such as native Shingle on the
  test machine, reached by `--ipc` and `--pid`, without a namespace. It
  refuses to run unless /etc/shingle-native-test-machine exists, and it
  never signals that Shingle (docs/native-testing.md, step 53).

The workload (workload.py) repeats a cycle of client churn: wl_probe
windows mapping, resizing, floating, maximizing, going fullscreen,
minimizing and moving between workspaces and closing; an output added
and removed; vkcube (DMA-BUF); a `wl_probe --animate --presentation` for
frame latency; X windows through Xwayland; the clipboard between
wl-copy, wl-paste and xclip both ways; drag and drop from Wayland and X
sources; a grim screencopy; Scale and Expo. Offscreen and nested Shingle
save their session as the layout changes. Headless runs no GPU client,
no latency client and no overview, since it has no frontend.

Once a minute, at the first moment only the long-lived anchor window is
mapped, it samples each measured Shingle's RSS, threads, open fds, CPU
time and `stipc/object_counts` (clients, surfaces, buffers, pools,
callbacks, textures and the rest), the CPU time of the whole namespace,
the load, and frame latency since the last sample; this host process
reads each Shingle's GPU memory from `nvidia-smi` by its host PID. The
samples go to samples.csv and a short summary.md in the output directory
(`target/soak/MODE-TIMESTAMP` by default), with a trend test for growth
of every leak-prone series (soak.py). The run fails on growth, a crash,
a log alarm (a panic), a protocol error or a failed check, such as a
window that did not map or a paste that did not match.

Load: run it under `nice -n 19` (it lowers its own priority to 19
either way). Before each cycle it reads /proc/loadavg and waits while
the 1-minute load is above `--load-limit` (12 by default, for a 20-core
host shared with builds), sampling on schedule meanwhile.

Usage, from the repository root and outside any sandbox that forbids
user namespaces, after `cargo build -p shingle-bin`, `cargo build -p
shingle --example wl_probe` and `cargo build -p shingle-xwayland
--example xw_probe`:

  nice -n 19 tools/soak_test/run.py --mode offscreen --minutes 120

Signals go only to processes inside the PID namespace, through
`signal_checked`; this host process signals nothing itself. The pure
helpers are tested by `python3 -m unittest` in this directory
(test_soak.py).
"""

import argparse
import json
import os
import subprocess
import sys
import threading
import time

import soak

HERE = os.path.dirname(os.path.abspath(__file__))
GPU_TEST = os.path.join(HERE, '..', 'gpu_test')
INNER_FLAG = '--inner'
NICENESS = 19
GPU_INTERVAL = 60.0
# workload.STEPS, kept here so that `--help` works outside the namespace.
STEPS = ('x', 'clipboard', 'dnd', 'windows', 'outputs', 'vkcube',
         'latency', 'capture', 'overview')
# `--mode attach` runs clients against a Shingle outside any namespace,
# so it refuses to run anywhere but the native test machine
# (docs/native-testing.md), which tools/native_test/uinput.py marks the
# same way.
# Debug builds of Shingle turn on wgpu's validation, which loads the
# Khronos validation layer; its own bookkeeping grows the heap by about
# 1 MiB a minute under the workload while Shingle's does not
# (docs/status.md#soak-tests). The loader leaves it out with this, unless
# `--validation` asks for it.
VALIDATION_VARIABLE = 'VK_LOADER_LAYERS_DISABLE'
VALIDATION_LAYERS = '*validation*'

def without_validation(environment, validation):
  """`environment` with the Vulkan validation layer disabled for every
  process of the run, unless `validation`."""
  environment = dict(environment)
  if not validation:
    environment[VALIDATION_VARIABLE] = VALIDATION_LAYERS
  return environment

TEST_MACHINE_MARKER = '/etc/shingle-native-test-machine'

def lower_priority(niceness=NICENESS):
  """Raises this process's niceness to `niceness`, which its children
  inherit; returns the niceness it has afterwards."""
  current = os.nice(0)
  if current < niceness:
    current = os.nice(niceness - current)
  return current

def parse_options(arguments):
  """The command line."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--mode', required=True,
                      choices=('headless', 'offscreen', 'nested', 'attach'))
  parser.add_argument('--minutes', type=float, default=30.0)
  parser.add_argument('--seed', type=int, default=1)
  parser.add_argument('--load-limit', type=float, default=12.0)
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--wl-probe', default='target/debug/examples/wl_probe')
  parser.add_argument('--xw-probe', default='target/debug/examples/xw_probe')
  parser.add_argument('--output', help='directory for samples.csv and '
                      'summary.md')
  parser.add_argument('--steps', default=','.join(STEPS),
                      help='the cycle steps to run, comma-separated '
                      f'(default {",".join(STEPS)})')
  parser.add_argument('--validation', action='store_true',
                      help='keep the Vulkan validation layer that debug '
                      'builds load')
  parser.add_argument('--ipc', help='attach: the running Shingle\'s IPC '
                      'socket')
  parser.add_argument('--pid', type=int, help='attach: its PID')
  parser.add_argument('--log', help='attach: its log, scanned for panics')
  parser.add_argument('--session', help='attach: its session file')
  parser.add_argument(INNER_FLAG, action='store_true',
                      help=argparse.SUPPRESS)
  options = parser.parse_args(arguments)
  options.steps = [step for step in options.steps.split(',') if step]
  unknown = sorted(set(options.steps) - set(STEPS))
  if unknown:
    parser.error(f'unknown steps {unknown}; known: {", ".join(STEPS)}')
  if options.mode == 'attach' and (not options.ipc or not options.pid):
    parser.error('--mode attach needs --ipc and --pid')
  return options

def host_processes():
  """{host PID: (ppid, nspid)} of every process this host can see."""
  processes = {}
  for entry in os.listdir('/proc'):
    if not entry.isdigit():
      continue
    try:
      with open(f'/proc/{entry}/status') as file:
        text = file.read()
    except OSError:
      continue
    processes[int(entry)] = (soak.parse_ppid(text), soak.parse_nspid(text))
  return processes

def sample_gpu(output, root):
  """Appends one line to gpu.jsonl: the GPU memory nvidia-smi reports
  for each process named in pids.json, found by its host PID below the
  host process `root`, or as it is when `root` is None (no namespace)."""
  try:
    with open(os.path.join(output, 'pids.json')) as file:
      pids = json.load(file)
  except (OSError, ValueError):
    return
  try:
    done = subprocess.run(['nvidia-smi', '-q', '-d', 'PIDS'],
                          capture_output=True, text=True, timeout=30,
                          stdin=subprocess.DEVNULL)
  except (OSError, subprocess.TimeoutExpired):
    return
  used = soak.parse_nvidia_pids(done.stdout)
  processes = host_processes()
  found = {}
  for label, pid in pids.items():
    host = pid if root is None else soak.host_pid(pid, processes, root)
    if host is not None and host in used:
      found[label] = used[host]
  with open(os.path.join(output, 'gpu.jsonl'), 'a') as file:
    file.write(json.dumps({'time': time.time(), 'used': found}) + '\n')

class GpuSampler:
  """Samples GPU memory on the host every GPU_INTERVAL seconds."""

  def __init__(self, output, root):
    self.output = output
    self.root = root
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
    while not self.stop_event.wait(5.0):
      sample_gpu(self.output, self.root)
      if self.stop_event.wait(GPU_INTERVAL - 5.0):
        break

def inner(options):
  """Runs the soak; called inside gpu_test's namespace."""
  # gpu_test's checks.py imports gpu_test's run.py as `run`.
  sys.path.insert(0, os.path.abspath(GPU_TEST))
  sys.path.insert(1, HERE)
  import workload
  return workload.run_soak(
      options.mode, options.minutes * 60, os.path.abspath(options.binary),
      os.path.abspath(options.wl_probe), os.path.abspath(options.xw_probe),
      options.seed, options.output, options.load_limit,
      steps=options.steps)

def attach(options):
  """Soaks a running Shingle on the native test machine, without a
  namespace; returns the exit status."""
  if not os.path.exists(TEST_MACHINE_MARKER):
    print(f'soak: {TEST_MACHINE_MARKER} does not exist; --mode attach '
          'runs only on the native test machine', flush=True)
    return 1
  sys.path.insert(0, os.path.abspath(GPU_TEST))
  sys.path.insert(1, HERE)
  import workload
  output = os.path.abspath(options.output or os.path.join(
      'target', 'soak', f'attach-{time.strftime("%Y%m%d-%H%M%S")}'))
  os.makedirs(output, exist_ok=True)
  runtime = os.environ.get('XDG_RUNTIME_DIR', f'/run/user/{os.getuid()}')
  attached = workload.Attached(options.ipc, options.pid, runtime,
                               options.log, options.session)
  sampler = GpuSampler(output, None)
  sampler.start()
  try:
    return workload.run_soak(
        'attach', options.minutes * 60, None,
        os.path.abspath(options.wl_probe), os.path.abspath(options.xw_probe),
        options.seed, output, options.load_limit, attached, options.steps)
  finally:
    sampler.stop()

def main(arguments=None):
  """Lowers the priority, then runs the soak in gpu_test's namespace and
  samples GPU memory from outside it."""
  options = parse_options(sys.argv[1:] if arguments is None else arguments)
  niceness = lower_priority()
  if options.inner:
    return inner(options)
  if niceness < NICENESS:
    print(f'soak: could not lower the priority to {NICENESS}', flush=True)
    return 1
  if options.mode == 'attach':
    return attach(options)
  output = options.output or os.path.join(
      'target', 'soak', f'{options.mode}-{time.strftime("%Y%m%d-%H%M%S")}')
  output = os.path.abspath(output)
  os.makedirs(output, exist_ok=True)
  command = [
      sys.executable, os.path.join(GPU_TEST, 'run.py'), '--modeset',
      '--timeout', str(options.minutes * 60 + 900), '--',
      sys.executable, os.path.abspath(__file__), INNER_FLAG,
      '--mode', options.mode, '--minutes', str(options.minutes),
      '--seed', str(options.seed), '--load-limit', str(options.load_limit),
      '--steps', ','.join(options.steps),
      '--binary', os.path.abspath(options.binary),
      '--wl-probe', os.path.abspath(options.wl_probe),
      '--xw-probe', os.path.abspath(options.xw_probe),
      '--output', output]
  print(f'soak: {options.mode} for {options.minutes} min into {output}',
        flush=True)
  process = subprocess.Popen(
      command, env=without_validation(os.environ, options.validation))
  sampler = GpuSampler(output, process.pid)
  sampler.start()
  try:
    status = process.wait()
  finally:
    sampler.stop()
  try:
    os.remove(os.path.join(output, 'pids.json'))
  except OSError:
    pass
  print(f'soak: exit status {status}; results in {output}', flush=True)
  return status

if __name__ == '__main__':
  sys.exit(main())
