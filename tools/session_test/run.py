#!/usr/bin/env python3
"""Real-client checks that a slow session write never stalls Shingle.

Runs a headless Shingle and, as its client, a nested Shingle on lavapipe
inside an unprivileged user, mount, network and PID namespace
(`unshare -U -r -m -n -p -f --mount-proc`), as tools/output_test/run.py
does: a private directory is bind-mounted over `/tmp`, `/dev/dri` and
`/dev/input` are covered with empty tmpfs mounts, the runtime, config,
state and home directories are private, and the host's
`WAYLAND_DISPLAY`, `WAYLAND_SOCKET`, `DISPLAY` and `WAYFIRE_SOCKET` are
removed from every environment. Xwayland is off.

The nested Shingle's `XDG_STATE_HOME` is a FUSE mount
(tools/session_test/slowfs.py) that holds every fsync for `--delay`
seconds, as this host's busy `/home` did for 8 to 17 s
(docs/status.md#sessions). Two `wl_probe` clients map in it, and while
a session write is stuck in fsync the harness changes the layout with
`simple-tile/set-layout` every 100 ms and times each reply. It checks
that every reply came within 50 ms, that the bursts were coalesced into
few writes, that SIGTERM exits with status 0, and that the session on
disk afterwards holds the last layout.

Usage, from the repository root and outside any sandbox that forbids
user namespaces (needs python3-fuse and a readable `/dev/fuse`):

  cargo build -p shingle-bin
  cargo build -p shingle --example wl_probe
  tools/session_test/run.py [--binary target/debug/shingle]
      [--wl-probe target/debug/examples/wl_probe] [--delay 1.5] [--keep]

`--checks round-trip` instead runs the session round trips of
tools/session_test/round_trip.py, which need no FUSE: five clients'
layout survives SIGKILL and SIGTERM with a restart after each, and with
`--master-binary` (a `shingle` built from `master`) a session saved by
it, version 1 or 2, restores unchanged. `--checks slow round-trip` runs both.

It prints one line per check and exits 0 when all pass. Signals go only
to processes started here, through `signal_checked`. The pure helpers
are tested by `python3 -m unittest` in this directory (test_run.py).
"""

import argparse
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import time

def _load_output_harness():
  """tools/output_test/run.py, for its Shingle runner, which brings the
  XWayland harness's IPC client, results and checked signal helper."""
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..',
                      'output_test', 'run.py')
  spec = importlib.util.spec_from_file_location('output_harness', path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

_OUTPUT = _load_output_harness()
Headless = _OUTPUT.Headless
Results = _OUTPUT.Results
signal_checked = _OUTPUT.signal_checked
wait_for = _OUTPUT.wait_for
HOST_VARIABLES = _OUTPUT.HOST_VARIABLES
hide_host_runtime = _OUTPUT.hide_host_runtime
host_uid_environment = _OUTPUT.host_uid_environment
LAVAPIPE = _OUTPUT._HARNESS.LAVAPIPE

INNER_FLAG = '--inner'
NAMESPACE_VARIABLE = 'SHINGLE_SESSION_TEST_NAMESPACE'
SLOWFS = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                      'slowfs.py')
# The longest an IPC reply may take while a session write is stuck.
REPLY_LIMIT = 0.05
# How long the layout keeps changing, and how often.
CHANGE_PERIOD = 3.0
CHANGE_INTERVAL = 0.1

def parse_events(text):
  """The (event, time, path) tuples in a slowfs log."""
  events = []
  for line in text.splitlines():
    event, stamp, path = line.split(' ', 2)
    events.append((event, float(stamp), path))
  return events

def fsync_intervals(events):
  """(begin, end) of each fsync in a slowfs log, in order; `end` is None
  for one still in progress."""
  intervals = []
  for event, stamp, _ in events:
    if event == 'begin':
      intervals.append([stamp, None])
    elif event == 'end' and intervals and intervals[-1][1] is None:
      intervals[-1][1] = stamp
  return [tuple(interval) for interval in intervals]

def overlapping(intervals, start, end):
  """The intervals that overlap [start, end]; an open one never ends."""
  return [(begin, finish) for begin, finish in intervals
          if begin <= end and (finish is None or finish >= start)]

def layout_request(direction, first, second, percent):
  """`simple-tile/set-layout` data for the first workspace: a `direction`
  split (`horizontal-split` or `vertical-split`) of views `first` and
  `second`, `first` taking `percent` of it, or with `percent` None of
  `first` alone, so that `second` floats."""
  if percent is None:
    children = [{'view-id': first, 'weight': 1}]
  else:
    children = [{'view-id': first, 'weight': percent},
                {'view-id': second, 'weight': 100 - percent}]
  return {
      'wset-index': 1,
      'workspace': {'x': 0, 'y': 0},
      'layout': {direction: children},
  }

def root_direction(reply):
  """The split key of a `simple-tile/get-layout` reply's root."""
  layout = reply.get('layout') or {}
  return next((key for key in ('horizontal-split', 'vertical-split')
               if key in layout), 'vertical-split')

def saved_shares(session, first, second):
  """The width shares of windows `first` and `second` in a saved
  session's first workspace, as (share, share) summing to 1, or None if
  they are not the two children of one split there. Version 2 sessions
  keep workspaces per set, version 1 per output."""
  for record in session.get('wsets', session.get('outputs', [])):
    for workspace in record.get('workspaces', []):
      split = workspace.get('root', {})
      shares = _split_shares(split, first, second)
      if shares:
        return shares
  return None

def _split_shares(split, first, second):
  children = split.get('children', [])
  nodes = [child.get('node', {}) for child in children]
  windows = [node.get('window') if node.get('kind') == 'leaf' else None
             for node in nodes]
  if windows == [first, second]:
    total = sum(child['weight'] for child in children)
    return tuple(child['weight'] / total for child in children)
  for node in nodes:
    if node.get('kind') == 'split':
      shares = _split_shares(node, first, second)
      if shares:
        return shares
  return None

def layout_steps(count):
  """`count` layout changes, as percentages for `layout_request`: every
  other one floats the second view, so that each changes the tiling and
  is saved, and the last tiles both at 23 and 77 percent."""
  steps = [None if index % 2 else 30 + (index * 7) % 41
           for index in range(count - 1)]
  return steps + [23]

class Probe:
  """One `wl_probe` client of the nested Shingle."""

  def __init__(self, wl_probe, nested, app_id):
    environment = {
        'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
        'HOME': os.path.join(nested.directory, 'home'),
        'XDG_RUNTIME_DIR': nested.runtime,
        'WAYLAND_DISPLAY': nested.wayland_display(),
        NAMESPACE_VARIABLE: '1',
    }
    self.process = subprocess.Popen(
        [wl_probe, app_id], env=environment, stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

  def stop(self):
    """Ends the probe: SIGTERM, then SIGKILL, each through
    `signal_checked`."""
    if self.process.poll() is None:
      signal_checked(self.process.pid, signal.SIGTERM, 'wl_probe')
      try:
        self.process.wait(5)
      except subprocess.TimeoutExpired:
        signal_checked(self.process.pid, signal.SIGKILL, 'wl_probe')
        self.process.wait()
    self.process.stdin.close()

def thread_states(pid):
  """`name:state:wchan` of each thread of `pid` not waiting on a futex,
  to show where an exit is stuck."""
  states = []
  try:
    tasks = os.listdir(f'/proc/{pid}/task')
  except OSError:
    return states
  for task in tasks:
    try:
      with open(f'/proc/{pid}/task/{task}/comm') as file:
        name = file.read().strip()
      with open(f'/proc/{pid}/task/{task}/wchan') as file:
        wchan = file.read().strip()
      with open(f'/proc/{pid}/task/{task}/stat') as file:
        state = file.read().rsplit(')', 1)[1].split()[0]
    except OSError:
      continue
    # Idle threads say nothing about a stuck exit.
    if wchan != 'futex_wait_queue':
      states.append(f'{name}:{state}:{wchan}')
  return states

def stop_shingle(shingle, timeout):
  """SIGTERM through `signal_checked`, then waits up to `timeout`; returns
  (status, seconds, thread states sampled 1 s after the signal)."""
  shingle.ipc.close()
  started = time.monotonic()
  if not signal_checked(shingle.process.pid, signal.SIGTERM, 'shingle'):
    return 'not signalled', 0.0, []
  states = []
  try:
    try:
      shingle.process.wait(1)
    except subprocess.TimeoutExpired:
      states = thread_states(shingle.process.pid)
    status = shingle.process.wait(timeout)
  except subprocess.TimeoutExpired:
    signal_checked(shingle.process.pid, signal.SIGKILL, 'shingle')
    shingle.process.wait()
    status = 'timeout'
  finally:
    shingle.log.close()
  return status, time.monotonic() - started, states

def mount_slowfs(backing, mountpoint, delay, log_path):
  """Starts slowfs at `mountpoint`; returns its process once mounted."""
  os.makedirs(backing)
  os.makedirs(mountpoint)
  process = subprocess.Popen(
      [sys.executable, SLOWFS, backing, mountpoint, '--delay', str(delay),
       '--log', log_path], stdin=subprocess.DEVNULL)
  mounted = wait_for(lambda: is_mounted(mountpoint))
  if not mounted:
    process.kill()
    raise RuntimeError('slowfs did not mount')
  return process

def is_mounted(path):
  """Whether something is mounted at `path` in this mount namespace."""
  with open('/proc/self/mountinfo') as mounts:
    return any(line.split()[4] == path for line in mounts)

def read_log(path):
  """The slowfs log's events so far."""
  try:
    with open(path) as log:
      return parse_events(log.read())
  except FileNotFoundError:
    return []

def nested_views(nested, count):
  """The nested Shingle's mapped views by app ID, once there are `count`
  of them; None before."""
  views = {view['app-id']: view['id']
           for view in nested.ipc.call('window-rules/list-views')
           if view.get('mapped')}
  return views if len(views) == count else None

def slow_write_checks(results, binary, wl_probe, delay):
  """Layout changes stay responsive while a session write is stuck, and
  the session on disk after exit is the last one."""
  backing = '/tmp/slow-backing'
  state = '/tmp/slow-state'
  log_path = '/tmp/slowfs.log'
  slowfs = mount_slowfs(backing, state, delay, log_path)
  outer = Headless(binary, 'outer', json.dumps({
      'output': {'mode': '640x480@60'},
      'xwayland': {'mode': False},
      'session': {'restore': False},
      'workspaces': {'width': 1, 'height': 1},
  }), 'yaml')
  nested = None
  probes = []
  try:
    nested = Headless(binary, 'nested', json.dumps({
        'xwayland': {'mode': False},
        'session': {'restore': False},
        'workspaces': {'width': 1, 'height': 1},
    }), 'yaml', backend=('--backend', 'nested'), extra_environment={
        'WAYLAND_DISPLAY': outer.wayland_display(),
        'VK_ICD_FILENAMES': LAVAPIPE,
        'XDG_STATE_HOME': state,
    })
    probes = [Probe(wl_probe, nested, app_id) for app_id in ('left', 'right')]
    views = wait_for(lambda: nested_views(nested, 2), timeout=30)
    results.check('two clients map in nested Shingle', views, str(views))
    if not views:
      return nested.log_text()
    first, second = views['left'], views['right']
    stuck = wait_for(lambda: any(
        end is None for _, end in fsync_intervals(read_log(log_path))),
                     timeout=10, interval=0.005)
    results.check('a session write is stuck in fsync', stuck,
                  str(read_log(log_path)))
    latencies = []
    errors = []
    direction = root_direction(nested.ipc.call(
        'simple-tile/get-layout',
        {'wset-index': 1, 'workspace': {'x': 0, 'y': 0}}))
    sequence = layout_steps(int(CHANGE_PERIOD / CHANGE_INTERVAL))
    started = time.monotonic()
    for index, percent in enumerate(sequence):
      sent = time.monotonic()
      reply = nested.ipc.call(
          'simple-tile/set-layout',
          layout_request(direction, first, second, percent))
      latencies.append(time.monotonic() - sent)
      if reply.get('result') != 'ok':
        errors.append(reply)
      time.sleep(max(0.0, started + (index + 1) * CHANGE_INTERVAL
                     - time.monotonic()))
    finished = time.monotonic()
    worst = max(latencies)
    results.check('set-layout succeeds', not errors, str(errors[:3]))
    results.check(
        f'every IPC reply within {REPLY_LIMIT * 1000:.0f} ms during slow '
        'writes', worst < REPLY_LIMIT,
        f'{len(latencies)} replies, worst {worst * 1000:.1f} ms, median '
        f'{sorted(latencies)[len(latencies) // 2] * 1000:.1f} ms')
    during = overlapping(fsync_intervals(read_log(log_path)), started,
                         finished)
    results.check('session writes were stuck in fsync meanwhile',
                  during, str(during))
    # One write at a time, each at least `delay` long, newest first.
    limit = min(int((finished - started) / delay) + 2, len(sequence) // 3)
    results.check('the layout changes were coalesced into few writes',
                  0 < len(during) <= limit,
                  f'{len(sequence)} changes, {len(during)} writes, '
                  f'at most {limit}')
    status, exit_time, states = stop_shingle(nested, 30)
    nested_log = nested.log_text()
    nested = None
    results.check('SIGTERM exits nested with status 0', status == 0,
                  f'{status} after {exit_time:.1f} s, threads after 1 s: '
                  f'{states}')
    session_path = os.path.join(backing, 'shingle', 'session.json')
    try:
      with open(session_path) as file:
        session = json.load(file)
    except (OSError, ValueError) as error:
      session = {}
      results.check('the session file is valid JSON', False, repr(error))
    shares = saved_shares(session, first, second)
    expected = (sequence[-1] / 100, 1 - sequence[-1] / 100)
    results.check(
        'the session on disk holds the last layout',
        shares is not None
        and all(abs(a - b) < 1e-6 for a, b in zip(shares, expected)),
        f'saved {shares}, last {expected}')
    leftovers = [name for name in os.listdir(os.path.join(backing,
                                                          'shingle'))
                 if name != 'session.json']
    results.check('no temporary session files are left', not leftovers,
                  str(leftovers))
    return nested_log
  finally:
    for probe in probes:
      probe.stop()
    if nested:
      nested.stop()
    outer_status = outer.stop()
    results.check('SIGTERM exits headless with status 0', outer_status == 0,
                  str(outer_status))
    subprocess.run(['umount', state], check=False)
    try:
      slowfs.wait(10)
    except subprocess.TimeoutExpired:
      signal_checked(slowfs.pid, signal.SIGKILL, 'python3')
      slowfs.wait()

def _load_round_trip():
  """tools/session_test/round_trip.py."""
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                      'round_trip.py')
  spec = importlib.util.spec_from_file_location('round_trip', path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

def check_groups(names, binary, wl_probe, delay, master_binary):
  """The (name, function of results returning a log) of each group in
  `names`, in order."""
  round_trip = _load_round_trip()
  harness = sys.modules[__name__]
  groups = {
      'slow': lambda results: slow_write_checks(results, binary, wl_probe,
                                                delay),
      'round-trip': lambda results: round_trip.round_trip_checks(
          harness, results, binary, wl_probe),
  }
  selected = [(name, groups[name]) for name in names]
  if master_binary and 'round-trip' in names:
    selected.append(('master', lambda results: round_trip.master_checks(
        harness, results, binary, master_binary, wl_probe)))
  return selected

def inner(binary, wl_probe, delay, names=('slow',), master_binary=None):
  """Runs the checks inside the namespace; returns the exit status."""
  work = os.environ['SESSION_TEST_WORK']
  private_tmp = os.path.join(work, 'tmp')
  os.makedirs(private_tmp)
  subprocess.run(['mount', '--bind', private_tmp, '/tmp'], check=True)
  for device in ('/dev/dri', '/dev/input'):
    if os.path.isdir(device):
      subprocess.run(['mount', '-t', 'tmpfs', 'none', device], check=True)
  runtime = hide_host_runtime()
  results = Results()
  results.check('/dev/dri is empty in the namespace',
                not os.path.isdir('/dev/dri') or not os.listdir('/dev/dri'))
  results.check('the host runtime directory is hidden',
                runtime is None or os.listdir(runtime) == [])
  results.check('no host display variable is set',
                not any(name in os.environ for name in HOST_VARIABLES))
  for name, checks in check_groups(names, binary, wl_probe, delay,
                                   master_binary):
    log = ''
    try:
      log = checks(results)
    except Exception as error:  # pylint: disable=broad-except
      results.check(f'{name} checks ran', False, repr(error))
    if not results.all_passed():
      print(f'--- {name}: nested log ---\n{log}--- end ---', flush=True)
  passed = sum(1 for _, ok, _ in results.checks if ok)
  print(f'{passed}/{len(results.checks)} checks passed', flush=True)
  return 0 if results.all_passed() else 1

def main():
  """Sets up the work directory and re-runs itself in the namespace."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--wl-probe', default='target/debug/examples/wl_probe')
  parser.add_argument('--delay', type=float, default=1.5)
  parser.add_argument('--checks', nargs='+', default=['slow'],
                      choices=['slow', 'round-trip'])
  parser.add_argument('--master-binary')
  parser.add_argument('--keep', action='store_true')
  parser.add_argument(INNER_FLAG, action='store_true')
  options = parser.parse_args()
  binary = os.path.abspath(options.binary)
  wl_probe = os.path.abspath(options.wl_probe)
  master_binary = (os.path.abspath(options.master_binary)
                   if options.master_binary else None)
  if options.inner:
    return inner(binary, wl_probe, options.delay, options.checks,
                 master_binary)
  if 'slow' in options.checks:
    if importlib.util.find_spec('fuse') is None:
      print('python3-fuse is not installed', file=sys.stderr)
      return 2
    if not os.access('/dev/fuse', os.R_OK | os.W_OK):
      print('/dev/fuse is not usable', file=sys.stderr)
      return 2
  work = os.path.abspath(os.path.join('target',
                                      f'session_test-{os.getpid()}'))
  os.makedirs(work)
  environment = {key: value for key, value in os.environ.items()
                 if key not in HOST_VARIABLES}
  environment['SESSION_TEST_WORK'] = work
  try:
    # The PID namespace confines every signal to processes started in it.
    command = ['unshare', '--user', '--map-root-user', '--mount', '--net',
               '--pid', '--fork', '--mount-proc', '--',
               sys.executable, os.path.abspath(__file__),
               '--binary', binary, '--wl-probe', wl_probe,
               '--delay', str(options.delay), '--checks', *options.checks,
               *(['--master-binary', master_binary] if master_binary
                 else []), INNER_FLAG]
    return subprocess.run(command,
                          env=host_uid_environment(environment)).returncode
  finally:
    if not options.keep:
      shutil.rmtree(work, ignore_errors=True)

if __name__ == '__main__':
  sys.exit(main())
