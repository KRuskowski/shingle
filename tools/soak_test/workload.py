"""The soak workload and its sampler, run inside tools/gpu_test/run.py's
namespace by run.py's inner half.

Each cycle maps, resizes, rearranges and closes `wl_probe` windows over
IPC (tiling, floating, maximize, fullscreen, minimize, always on top,
workspaces), adds and removes an output, runs `vkcube` on the GPU
(DMA-BUF), measures frame latency with `wl_probe --animate
--presentation`, maps, retitles and closes `xw_probe` X windows through
Xwayland, copies text between `wl-copy`, `wl-paste` and `xclip` in both
directions, drags text between windows with STIPC's pointer, takes a
`grim` screencopy, and toggles Scale and Expo. What runs depends on the
mode: headless Shingle has no frontend, so no GPU client, latency,
Scale or Expo. A cycle ends when only the long-lived anchor window is
left, and the sampler samples there, once a minute, so that object
counts compare between samples.

Every random choice comes from a generator seeded with the run's seed
and the cycle number, so a run repeats. Shingle always gets an explicit
`--headless` or `--backend offscreen|nested` (checks.safe_backend) and
every signal goes through run.signal_checked to a child of this process
inside the PID namespace.
"""

import collections
import json
import os
import random
import signal
import subprocess
import threading
import time

import checks
import run as gpu_run
import soak

xw = checks.xw

# Both probes refuse to run without one of these.
NAMESPACE_VARIABLES = {
    'SHINGLE_GPU_TEST_NAMESPACE': '1',
    'SHINGLE_XWAYLAND_TEST_NAMESPACE': '1',
}
# The steps of a cycle, in order; a run can leave some out to find which
# one makes something grow.
STEPS = ('x', 'clipboard', 'dnd', 'windows', 'outputs', 'vkcube',
         'latency', 'capture', 'overview')
MODES = ('headless', 'offscreen', 'nested', 'attach')
# The measured Shingle's configuration: lazy Xwayland, four workspaces,
# no config reload.
CONFIG = {
    'xwayland': {'mode': 'lazy'},
    'workspaces': {'width': 2, 'height': 2},
    'behavior': {'auto_reload': False},
}
# The outer offscreen Shingle of the nested mode, which only hosts it.
OUTER_CONFIG = {
    'xwayland': {'mode': False},
    'workspaces': {'width': 1, 'height': 1},
    'behavior': {'auto_reload': False},
}
SAMPLE_INTERVAL = 60.0
# The shortest time from the start of one cycle to the start of the
# next, which keeps headless Shingle's quick cycles to the same churn
# per minute as the others' and the client mix well under two cores.
CYCLE_PERIOD = 15.0
# How long a paused workload waits before it looks at the load again.
PAUSE_STEP = 10.0
# Lines a client keeps of its output.
MAX_LINES = 5000
# The frames of each latency measurement and of each vkcube run.
LATENCY_FRAMES = 120
VKCUBE_FRAMES = 240
ANCHOR = 'soak-anchor'
# How long a Shingle gets to exit after SIGTERM, as in the output
# harness: the exit syncs the session file, slow on the shared disk.
STOP_TIMEOUT = 60.0
# Shingle log lines that mean it crashed or hit a bug.
LOG_ALARMS = ('panicked', 'RUST_BACKTRACE', 'protocol error')

class Client:
  """A client process whose output is read as lines in the background,
  stopped through `signal_checked`."""

  def __init__(self, arguments, environment, program, stdin=False):
    self.program = program
    self.arguments = arguments
    self.process = subprocess.Popen(
        arguments, env=environment,
        stdin=subprocess.PIPE if stdin else subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        errors='replace')
    self.lines = collections.deque(maxlen=MAX_LINES)
    self.lock = threading.Lock()
    self.reader = threading.Thread(target=self._read, daemon=True)
    self.reader.start()

  def _read(self):
    for line in self.process.stdout:
      with self.lock:
        self.lines.append(line.rstrip('\n'))

  def text(self):
    """What the client printed so far."""
    with self.lock:
      return '\n'.join(self.lines)

  def events(self, name=None):
    """The JSON events the client printed, those called `name` if
    given."""
    found = []
    with self.lock:
      lines = list(self.lines)
    for line in lines:
      try:
        event = json.loads(line)
      except ValueError:
        continue
      if isinstance(event, dict) and name in (None, event.get('event')):
        found.append(event)
    return found

  def wait_event(self, name, timeout=10.0):
    """The first `name` event within `timeout` seconds, or None."""
    return xw.wait_for(lambda: next(iter(self.events(name)), None),
                       timeout=timeout, interval=0.05)

  def send(self, line):
    """Writes one command line to the client's stdin."""
    try:
      self.process.stdin.write(line + '\n')
      self.process.stdin.flush()
    except (BrokenPipeError, ValueError, AttributeError):
      pass

  def exited(self, timeout):
    """The exit status within `timeout` seconds, or None."""
    try:
      return self.process.wait(timeout)
    except subprocess.TimeoutExpired:
      return None

  def stop(self):
    """SIGTERM, then SIGKILL, each through `signal_checked`; returns the
    exit status, or None when the process would not go."""
    if self.process.poll() is None:
      gpu_run.signal_checked(self.process.pid, signal.SIGTERM, self.program)
      if self.exited(5) is None:
        gpu_run.signal_checked(self.process.pid, signal.SIGKILL,
                               self.program)
        self.exited(5)
    self.reader.join(timeout=2)
    return self.process.poll()

def client_errors(client):
  """The protocol and client errors in `client`'s output: the probes'
  `error` events and libwayland's protocol error lines."""
  errors = [event.get('text', '') for event in client.events('error')]
  errors += [line for line in client.text().splitlines()
             if 'protocol error' in line.lower()
             or line.startswith('wl_display@')]
  return errors

class LogWatcher:
  """Reads a Shingle log from where it last stopped and finds alarms."""

  def __init__(self, path):
    self.path = path
    self.offset = 0
    self.size = 0

  def new_alarms(self):
    """The log lines since the last call that contain an alarm."""
    try:
      with open(self.path, errors='replace') as file:
        file.seek(self.offset)
        text = file.read()
        self.offset = file.tell()
    except OSError:
      return []
    self.size = self.offset
    return [line for line in text.splitlines()
            if any(alarm in line for alarm in LOG_ALARMS)]

class AttachedProcess:
  """The PID of a Shingle this harness did not start."""

  def __init__(self, pid):
    self.pid = pid
    self.returncode = None

class Attached:
  """A Shingle that runs already, such as native Shingle on the test
  machine, reached through its IPC socket: the same interface as
  checks.Shingle for what the workload uses, but never signalled or
  stopped by the harness."""

  def __init__(self, ipc_path, pid, runtime, log_path, session_path):
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 1:
      raise ValueError(f'refusing PID {pid!r}')
    self.process = AttachedProcess(pid)
    self.ipc = xw.Ipc(ipc_path)
    self.runtime = runtime
    self.log_path = log_path
    self.session_path = session_path
    self.display = self.ipc.call('stipc/get_display')

  def call(self, method, data=None):
    """One IPC request."""
    return self.ipc.call(method, data)

  def alive(self):
    """Whether the process still runs."""
    return os.path.exists(f'/proc/{self.process.pid}')

  def log_text(self):
    """The log, or what it ends with, if one was named."""
    if not self.log_path:
      return ''
    try:
      with open(self.log_path, errors='replace') as file:
        file.seek(max(0, os.path.getsize(self.log_path) - 65536))
        return file.read()
    except OSError:
      return ''

  def client_environment(self, extra=None):
    """The environment of a client of this Shingle."""
    environment = {key: value for key, value in os.environ.items()
                   if key not in gpu_run.STRIPPED_VARIABLES}
    environment.update({
        'XDG_RUNTIME_DIR': self.runtime,
        'WAYLAND_DISPLAY': os.path.join(self.runtime,
                                        self.display.get('wayland', '')),
    })
    environment.update(extra or {})
    return environment

class Context:
  """One soak run: the measured Shingle (or two in nested mode), the
  binaries, the failures, the latency samples and the counters."""

  def __init__(self, mode, binary, wl_probe, xw_probe, seed, output,
               attached=None):
    if mode not in MODES:
      raise ValueError(f'unknown mode {mode}')
    self.mode = mode
    self.binary = binary
    self.wl_probe = wl_probe
    self.xw_probe = xw_probe
    self.seed = seed
    self.output = output
    self.failures = []
    self.failure_counts = collections.Counter()
    self.latencies = []
    self.frames = 0
    self.discarded = 0
    self.cycles = 0
    self.paused = 0.0
    self.counters = collections.Counter()
    self.steps = STEPS
    self.outer = None
    nvidia = {'VK_ICD_FILENAMES': checks.NVIDIA_ICD}
    # vkcube's Vulkan driver: NVIDIA in the GPU namespace, the system's
    # choice on the test machine.
    self.vulkan = {} if mode == 'attach' else nvidia
    if mode == 'attach':
      self.shingle = attached
    elif mode == 'headless':
      self.shingle = checks.Shingle(binary, 'soak', CONFIG,
                                    backend=('--headless',))
    elif mode == 'offscreen':
      self.shingle = checks.Shingle(binary, 'soak', CONFIG,
                                    extra_environment=nvidia)
    else:
      self.outer = checks.Shingle(binary, 'soak-outer', OUTER_CONFIG,
                                  extra_environment=nvidia)
      environment = dict(nvidia,
                         WAYLAND_DISPLAY=self.outer.wayland_display())
      self.shingle = checks.Shingle(binary, 'soak', CONFIG,
                                    backend=('--backend', 'nested'),
                                    extra_environment=environment)
    self.frontend = mode != 'headless'
    if mode == 'attach':
      self.display = attached.display.get('xwayland') or None
    else:
      self.display = xw.wait_for(lambda: next(
          (line.split('=', 1)[1] for line in self.shingle.log_text()
           .splitlines() if line.startswith('DISPLAY=')), None), timeout=30)
    self.wayland = self.shingle.client_environment(NAMESPACE_VARIABLES)
    self.x = dict(self.wayland, DISPLAY=self.display or '')
    self.logs = {}
    if self.shingle.log_path:
      self.logs['shingle'] = LogWatcher(self.shingle.log_path)
    if self.outer is not None:
      self.logs['outer'] = LogWatcher(self.outer.log_path)
    if mode == 'attach':
      self.session_path = attached.session_path
      # Skip what the log said before the run.
      for watcher in self.logs.values():
        watcher.new_alarms()
    else:
      self.session_path = os.path.join(self.shingle.directory, 'state',
                                       'session.json')
    self.session_mtime = None
    self.anchor = None

  def processes(self):
    """{label: Shingle} of the measured processes."""
    measured = {'shingle': self.shingle}
    if self.outer is not None:
      measured['outer'] = self.outer
    return measured

  def fail(self, message):
    """Records a failure; repeats are counted, not listed again."""
    self.failure_counts[message] += 1
    if self.failure_counts[message] == 1:
      self.failures.append(f'cycle {self.cycles}: {message}')
      print(f'soak: FAIL cycle {self.cycles}: {message}', flush=True)

  def call(self, method, data=None, expect_ok=True):
    """One IPC request to the measured Shingle; a reply that is not ok
    is a failure when `expect_ok`."""
    try:
      reply = self.shingle.call(method, data)
    except (OSError, TimeoutError, ConnectionError, ValueError) as error:
      self.fail(f'IPC {method} failed: {error}')
      return {}
    if expect_ok and isinstance(reply, dict) and reply.get('result') not in (
        None, 'ok'):
      self.fail(f'IPC {method} replied {json.dumps(reply)[:200]}')
    return reply

  def views(self):
    """The mapped views, by `window-rules/list-views`."""
    views = self.call('window-rules/list-views', expect_ok=False)
    if not isinstance(views, list):
      return []
    return [view for view in views if view.get('mapped', True)]

  def view(self, app_id):
    """The one mapped view with `app_id`, or None."""
    found = [view for view in self.views() if view.get('app-id') == app_id]
    return found[0] if len(found) == 1 else None

  def wl_client(self, app_id, *options):
    """A `wl_probe` with `app_id`."""
    return Client([self.wl_probe, app_id, *options], self.wayland,
                  'wl_probe')

  def alive(self):
    """Whether every measured Shingle still runs."""
    return all(shingle.alive() for shingle in self.processes().values())

def wait_mapped(context, app_id, client, timeout=15.0):
  """The view of `app_id` once mapped, or None, which is a failure."""
  view = xw.wait_for(lambda: context.view(app_id), timeout=timeout,
                     interval=0.1)
  if view is None:
    context.fail(f'{app_id} did not map: {checks.tail(client.text(), 3)}')
  return view

def finish(context, clients):
  """Stops `clients`, records their protocol errors, and waits until
  only the anchor is mapped."""
  for client in clients:
    client.stop()
    for error in client_errors(client):
      context.fail(f'{client.program} error: {error[:200]}')
  clients.clear()

  def settled():
    return [view.get('app-id') for view in context.views()] == [ANCHOR]
  if not xw.wait_for(settled, timeout=20, interval=0.2):
    context.fail('views left after the cycle: '
                 f'{[view.get("app-id") for view in context.views()]}')

def windows_step(context, rng, clients):
  """Maps three probes, then tiles, floats, resizes, maximizes, makes
  fullscreen, minimizes, stacks and moves them between workspaces."""
  names = [f'soak-{context.cycles}-{index}' for index in range(3)]
  for index, name in enumerate(names):
    options = ['--pattern'] if index == 0 else []
    if index == 2 and context.frontend:
      options += ['--decoration', rng.choice(['server', 'client'])]
    clients.append(context.wl_client(name, *options))
  views = [wait_mapped(context, name, client)
           for name, client in zip(names, clients)]
  views = [view for view in views if view]
  context.counters['windows'] += len(views)
  for view in views:
    view_id = view['id']
    action = rng.choice(['float', 'maximize', 'fullscreen', 'minimize',
                         'top', 'workspace'])
    context.counters[f'action {action}'] += 1
    if action == 'float':
      geometry = {'x': rng.randrange(0, 400), 'y': rng.randrange(0, 300),
                  'width': rng.randrange(200, 700),
                  'height': rng.randrange(150, 500)}
      context.call('window-rules/configure-view',
                   {'id': view_id, 'geometry': geometry})
    elif action == 'maximize':
      context.call('wm-actions/toggle_maximize', {'view_id': view_id})
    elif action == 'fullscreen':
      context.call('wm-actions/set-fullscreen',
                   {'view_id': view_id, 'state': True})
      time.sleep(0.2)
      context.call('wm-actions/set-fullscreen',
                   {'view_id': view_id, 'state': False})
    elif action == 'minimize':
      context.call('wm-actions/set-minimized',
                   {'view_id': view_id, 'state': True})
      time.sleep(0.2)
      context.call('wm-actions/set-minimized',
                   {'view_id': view_id, 'state': False})
    elif action == 'top':
      context.call('wm-actions/toggle_always_on_top', {'view_id': view_id})
    else:
      context.call('vswitch/send-view',
                   {'view-id': view_id, 'x': rng.randrange(2),
                    'y': rng.randrange(2)})
    context.call('window-rules/focus-view', {'id': view_id},
                 expect_ok=False)
    time.sleep(0.1)
  for _ in range(3):
    context.call('vswitch/set-workspace',
                 {'x': rng.randrange(2), 'y': rng.randrange(2)})
    time.sleep(0.3)
  context.call('vswitch/set-workspace', {'x': 0, 'y': 0})
  # Close one through the compositor, as a user would; the others are
  # stopped by signal at the end of the cycle.
  if views:
    context.call('window-rules/close-view', {'id': views[0]['id']})
    if clients[0].exited(5) is None:
      context.fail('a wl_probe did not exit after close-view')

def output_step(context):
  """Adds an output, moves the anchor onto it and back, and removes
  it."""
  if context.mode == 'nested':
    reply = context.call('stipc/create_wayland_output')
  else:
    reply = context.call('wayfire/create-headless-output',
                         {'width': 800, 'height': 600})
  output = (reply or {}).get('output') or {}
  name = output.get('name')
  if not name:
    context.fail(f'no output was created: {json.dumps(reply)[:200]}')
    return
  context.counters['outputs'] += 1
  anchor = context.view(ANCHOR)
  if anchor and isinstance(output.get('id'), int):
    context.call('window-rules/configure-view',
                 {'id': anchor['id'], 'output_id': output['id']})
    time.sleep(0.5)
  if context.mode == 'nested':
    context.call('stipc/destroy_wayland_output', {'output': name})
  else:
    context.call('wayfire/destroy-headless-output', {'output': name})
  time.sleep(0.5)
  # Unplugged under `return`, the anchor joined the active output.
  if context.view(ANCHOR) is None:
    context.fail('the anchor is gone after its output was removed')

def vkcube_step(context, clients):
  """Runs vkcube, a Vulkan DMA-BUF client, for VKCUBE_FRAMES frames."""
  environment = dict(context.wayland, **context.vulkan)
  client = Client(['vkcube', '--wsi', 'wayland', '--c', str(VKCUBE_FRAMES)],
                  environment, 'vkcube')
  clients.append(client)
  mapped = xw.wait_for(lambda: any(view.get('app-id') != ANCHOR
                                   for view in context.views()),
                       timeout=20, interval=0.2)
  if not mapped:
    context.fail(f'vkcube did not map: {checks.tail(client.text(), 3)}')
  status = client.exited(40)
  if status is None:
    context.fail('vkcube did not finish its frames')
  elif status != 0:
    context.fail(f'vkcube exited with {status}: '
                 f'{checks.tail(client.text(), 3)}')
  else:
    context.counters['vkcube runs'] += 1

def latency_step(context, clients):
  """Frame latency from presentation feedback: LATENCY_FRAMES frames of
  a `wl_probe --animate --presentation`."""
  name = f'soak-latency-{context.cycles}'
  client = context.wl_client(name, '--animate', str(LATENCY_FRAMES),
                             '--presentation')
  clients.append(client)
  if wait_mapped(context, name, client) is None:
    return
  if client.wait_event('animated', timeout=60) is None:
    context.fail(f'{name} did not finish its frames')
  presented = client.events('presented')
  context.latencies += [event['latency_us'] / 1000 for event in presented
                        if isinstance(event.get('latency_us'), int)]
  context.frames += len(presented)
  context.discarded += len(client.events('discarded'))

def x_step(context, rng, clients):
  """Maps two X windows through Xwayland, retitles, moves and closes
  them."""
  for index in range(2):
    name = f'SoakX{context.cycles}x{index}'
    client = Client([context.xw_probe, '--class', f'{name.lower()},{name}',
                     '--title', name, '--size',
                     f'{rng.randrange(200, 500)}x{rng.randrange(150, 400)}'],
                    context.x, 'xw_probe', stdin=True)
    clients.append(client)
    view = wait_mapped(context, name, client, timeout=30)
    if view is None:
      continue
    context.counters['x windows'] += 1
    client.send(f'title {name} renamed')
    client.send(f'configure {rng.randrange(0, 200)} {rng.randrange(0, 200)} '
                f'{rng.randrange(200, 600)} {rng.randrange(150, 450)}')
    time.sleep(0.3)
    if index == 0:
      context.call('window-rules/close-view', {'id': view['id']})
      if client.exited(10) is None:
        context.fail('an xw_probe did not exit after close-view')

def clipboard_step(context, rng):
  """Copies a payload between wl-copy, wl-paste and xclip, both ways."""
  helpers = []
  try:
    data = xw.payload(rng.choice([64, 4096, 300000]), seed=context.cycles)
    copy = xw.Helper(['wl-copy', '--foreground'], context.wayland,
                     'wl-copy', data)
    helpers.append(copy)
    pasted = xw.wait_paste(lambda: _paste(['wl-paste', '--no-newline'],
                                          context.wayland), data)
    if pasted != data:
      context.fail(f'wl-paste got {xw.compare(pasted, data)}')
    if context.display:
      pasted = xw.wait_paste(
          lambda: _paste(['xclip', '-selection', 'clipboard', '-o'],
                         context.x), data)
      if pasted != data:
        context.fail(f'xclip -o got {xw.compare(pasted, data)} from '
                     'wl-copy')
      data = xw.payload(rng.choice([64, 4096, 300000]),
                        seed=context.cycles + 1)
      helpers.append(xw.Helper(['xclip', '-selection', 'clipboard', '-i',
                                '-quiet'], context.x, 'xclip', data))
      pasted = xw.wait_paste(lambda: _paste(['wl-paste', '--no-newline'],
                                            context.wayland), data)
      if pasted != data:
        context.fail(f'wl-paste got {xw.compare(pasted, data)} from '
                     'xclip')
    context.counters['clipboard rounds'] += 1
  finally:
    for helper in helpers:
      helper.stop()

def _paste(arguments, environment):
  """A paste command's output, or None when it failed."""
  try:
    done = subprocess.run(arguments, env=environment, capture_output=True,
                          timeout=10, stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  return done.stdout if done.returncode == 0 else None

def dnd_step(context, clients, from_x):
  """Drags text with STIPC's pointer into a `wl_probe --drop`, from a
  `wl_probe --drag` or, with `from_x`, an `xw_probe --xdnd-source`."""
  text = f'soak drag {context.cycles}'
  target_name = f'soak-drop-{context.cycles}'
  if from_x:
    source_name = f'SoakDrag{context.cycles}'
    source = Client([context.xw_probe, '--class',
                     f'{source_name.lower()},{source_name}', '--title',
                     source_name, '--xdnd-source', text, '--drag-icon'],
                    context.x, 'xw_probe', stdin=True)
  else:
    source_name = f'soak-drag-{context.cycles}'
    source = context.wl_client(source_name, '--drag', text)
  target = context.wl_client(target_name, '--drop')
  clients += [source, target]
  views = [wait_mapped(context, name, client, timeout=30)
           for name, client in ((source_name, source),
                                (target_name, target))]
  if None in views:
    return
  start, end = (xw.centre(view['geometry']) for view in views)
  try:
    _drag(context, start, end)
  finally:
    context.call('stipc/feed_button',
                 {'combo': 'BTN_LEFT', 'mode': 'release'})
  dropped = target.wait_event('dropped', timeout=10) or {}
  if dropped.get('text') != text:
    context.fail(f'the {"X" if from_x else "Wayland"} drag dropped '
                 f'{json.dumps(dropped)[:120]}')
  else:
    context.counters['drags from X' if from_x else 'drags'] += 1
  context.call('stipc/move_cursor', {'x': 5, 'y': 5})

def _drag(context, start, end, steps=10):
  """Presses at `start`, moves to `end` and leaves the button pressed."""
  context.call('stipc/move_cursor', {'x': start[0], 'y': start[1]})
  context.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
  for dx in (2, 5, 8):
    context.call('stipc/move_cursor', {'x': start[0] + dx, 'y': start[1]})
    time.sleep(0.05)
  for x, y in xw.path_points((start[0] + 8, start[1]), end, steps):
    context.call('stipc/move_cursor', {'x': x, 'y': y})
    time.sleep(0.05)
  time.sleep(0.2)

def capture_step(context):
  """One `grim` screencopy of the measured Shingle."""
  try:
    done = subprocess.run(['grim', '-t', 'ppm', '-'], env=context.wayland,
                          capture_output=True, timeout=20,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    context.fail('grim timed out')
    return
  if done.returncode != 0 or not done.stdout.startswith(b'P6'):
    context.fail(f'grim failed: {done.stderr[-200:]!r}')
  else:
    context.counters['captures'] += 1

def overview_step(context):
  """Opens and closes Scale and Expo."""
  for method in ('scale/toggle', 'expo/toggle'):
    context.call(method)
    time.sleep(0.6)
    context.call(method)
    time.sleep(0.6)
  context.counters['overviews'] += 1

def run_cycle(context):
  """One cycle of the workload; returns when only the anchor is left."""
  rng = random.Random(f'{context.seed}-{context.cycles}')
  clients = []
  steps = context.steps
  try:
    # X first, so that lazy Xwayland has exited again, idle, by the end
    # of the cycle, where the sample is taken.
    if context.display and 'x' in steps:
      x_step(context, rng, clients)
      finish(context, clients)
    if 'clipboard' in steps:
      clipboard_step(context, rng)
    if 'dnd' in steps:
      dnd_step(context, clients,
               from_x=bool(context.display) and context.cycles % 2 == 1)
      finish(context, clients)
    if 'windows' in steps:
      windows_step(context, rng, clients)
      finish(context, clients)
    if 'outputs' in steps:
      output_step(context)
    if context.frontend and 'vkcube' in steps:
      vkcube_step(context, clients)
      finish(context, clients)
    if context.frontend and 'latency' in steps:
      latency_step(context, clients)
      finish(context, clients)
    if 'capture' in steps:
      capture_step(context)
    if context.frontend and 'overview' in steps:
      overview_step(context)
  finally:
    finish(context, clients)
  context.cycles += 1

def namespace_cpu_seconds():
  """CPU seconds of every process in this PID namespace, with those of
  the children each has reaped, so that every process that ever ran in
  it counts once."""
  ticks = os.sysconf('SC_CLK_TCK')
  total = 0.0
  for entry in os.listdir('/proc'):
    if not entry.isdigit():
      continue
    try:
      with open(f'/proc/{entry}/stat') as file:
        total += soak.parse_stat_cpu(file.read(), ticks, children=True)
    except (OSError, IndexError, ValueError):
      continue
  return total

def process_sample(pid):
  """{'rss_kib', 'threads', 'fds', 'cpu_s'} of `pid`, or {} when it is
  gone."""
  try:
    with open(f'/proc/{pid}/status') as file:
      values = soak.parse_status(file.read())
    with open(f'/proc/{pid}/stat') as file:
      values['cpu_s'] = soak.parse_stat_cpu(file.read(),
                                            os.sysconf('SC_CLK_TCK'))
    values['fds'] = len(os.listdir(f'/proc/{pid}/fd'))
  except OSError:
    return {}
  return values

def gpu_memory(output, label, now):
  """The GPU memory of `label` in MiB from the host sampler's
  gpu.jsonl in `output`, if a line of the last two minutes has it."""
  try:
    with open(os.path.join(output, 'gpu.jsonl')) as file:
      lines = file.read().splitlines()
  except OSError:
    return None
  for line in reversed(lines):
    try:
      entry = json.loads(line)
    except ValueError:
      continue
    if now - entry.get('time', 0) > 120:
      return None
    if label in entry.get('used', {}):
      return entry['used'][label]
  return None

def write_pids(context):
  """Writes {label: PID inside the namespace} for the host's GPU
  sampler."""
  pids = {label: shingle.process.pid
          for label, shingle in context.processes().items()}
  path = os.path.join(context.output, 'pids.json')
  with open(path + '.new', 'w') as file:
    json.dump(pids, file)
  os.replace(path + '.new', path)

def session_writes(context):
  """Counts a changed session file and fails on one that is not JSON."""
  try:
    mtime = os.stat(context.session_path).st_mtime_ns
  except OSError:
    return
  if mtime != context.session_mtime:
    context.session_mtime = mtime
    context.counters['session writes'] += 1
    try:
      with open(context.session_path) as file:
        json.load(file)
    except (OSError, ValueError) as error:
      context.fail(f'the session file is not JSON: {error}')

def sample(context, started, rows):
  """Takes one sample of every measured process and appends its rows."""
  elapsed = time.monotonic() - started
  now = time.time()
  with open('/proc/loadavg') as file:
    load1 = soak.parse_loadavg(file.read())
  session_writes(context)
  p50, p99, worst = soak.latency_summary(context.latencies)
  shared = {
      'minute': round(elapsed / 60, 2),
      'elapsed_s': round(elapsed, 1),
      'namespace_cpu_s': namespace_cpu_seconds(),
      'load1': load1,
      'paused_s': round(context.paused, 1),
      'cycles': context.cycles,
      'session_writes': context.counters['session writes'],
      'frames': context.frames,
      'latency_p50_ms': p50,
      'latency_p99_ms': p99,
      'latency_max_ms': worst,
  }
  context.latencies = []
  for label, shingle in context.processes().items():
    row = dict(shared, process=label, pid=shingle.process.pid)
    row.update(process_sample(shingle.process.pid))
    row['gpu_mib'] = gpu_memory(context.output, label, now)
    try:
      reply = shingle.call('stipc/object_counts')
    except (OSError, TimeoutError, ConnectionError, ValueError) as error:
      context.fail(f'{label}: stipc/object_counts failed: {error}')
      reply = {}
    row.update(soak.flatten_counts(reply))
    rows.append(row)
  print(f'soak: sample at {shared["minute"]} min, cycles {context.cycles}, '
        f'load {load1}, ' + ', '.join(
            f'{row["process"]} rss {row.get("rss_kib")} KiB '
            f'fds {row.get("fds")}' for row in rows[-len(
                context.processes()):]), flush=True)

def check_alive(context):
  """Records crashes and log alarms; returns whether to go on."""
  for label, watcher in context.logs.items():
    for line in watcher.new_alarms():
      context.fail(f'{label} log: {line[:200]}')
  for label, shingle in context.processes().items():
    if not shingle.alive():
      context.fail(f'{label} exited with {shingle.process.returncode}: '
                   f'{checks.tail(shingle.log_text(), 4)}')
      return False
  return True

def soak_loop(context, duration, rows, load_limit=soak.LOAD_LIMIT):
  """Runs cycles for `duration` seconds, sampling at the first quiet
  point after each minute and pausing while the load is too high."""
  started = time.monotonic()
  next_sample = started
  last_sample = None
  while True:
    now = time.monotonic()
    if now >= next_sample:
      sample(context, started, rows)
      last_sample = now
      next_sample += SAMPLE_INTERVAL
      while next_sample <= now:
        next_sample += SAMPLE_INTERVAL
    if now - started >= duration or not check_alive(context):
      break
    with open('/proc/loadavg') as file:
      load1 = soak.parse_loadavg(file.read())
    if soak.should_pause(load1, load_limit):
      print(f'soak: load {load1} above {load_limit}, pausing', flush=True)
      wait = min(PAUSE_STEP, max(0.0, next_sample - time.monotonic()) + 0.1)
      time.sleep(wait)
      context.paused += wait
      continue
    cycle_started = time.monotonic()
    run_cycle(context)
    rest = CYCLE_PERIOD - (time.monotonic() - cycle_started)
    if rest > 0:
      time.sleep(rest)
  if context.alive() and (last_sample is None
                          or time.monotonic() - last_sample > 5):
    sample(context, started, rows)

def stop_shingle(shingle):
  """SIGTERM through `signal_checked`, and SIGKILL after STOP_TIMEOUT
  seconds, since the exit writes the session; returns the exit status
  or 'timeout'."""
  shingle.ipc.close()
  try:
    return checks._stop(shingle.process, 'shingle', STOP_TIMEOUT)
  finally:
    shingle.log.close()

def stop_all(context):
  """Stops the anchor and every Shingle; a bad exit status is a
  failure."""
  if context.anchor is not None:
    context.anchor.stop()
    for error in client_errors(context.anchor):
      context.fail(f'anchor error: {error[:200]}')
  for label in ('shingle', 'outer'):
    shingle = context.processes().get(label)
    if shingle is None or isinstance(shingle, Attached):
      continue
    alive = shingle.alive()
    status = stop_shingle(shingle)
    if alive and status != 0:
      context.fail(f'{label} SIGTERM exit status {status}')

def run_soak(mode, duration, binary, wl_probe, xw_probe, seed, output,
             load_limit=soak.LOAD_LIMIT, attached=None, steps=STEPS):
  """Runs the soak and writes samples.csv and summary.md to `output`;
  returns 0 when nothing failed and nothing grew. In mode `attach`,
  `attached` is the running Shingle to soak."""
  os.environ.update(NAMESPACE_VARIABLES)
  started = time.time()
  context = Context(mode, binary, wl_probe, xw_probe, seed, output,
                    attached)
  context.steps = tuple(steps)
  rows = []
  try:
    write_pids(context)
    context.anchor = context.wl_client(ANCHOR, '--pattern')
    if wait_mapped(context, ANCHOR, context.anchor) is not None:
      soak_loop(context, duration, rows, load_limit)
  finally:
    check_alive(context)
    stop_all(context)
  for label, watcher in context.logs.items():
    context.counters[f'{label} log bytes'] = watcher.size
  judged = soak.trends(rows)
  options = {'mode': mode, 'duration': f'{duration / 60:.0f} min',
             'seed': seed, 'binary': binary,
             'steps': ' '.join(context.steps),
             'started': time.strftime('%Y-%m-%d %H:%M',
                                      time.localtime(started))}
  info = {'cycles': context.cycles,
          'paused for load': f'{context.paused:.0f} s',
          'frames with presentation feedback': context.frames,
          'discarded frames': context.discarded}
  info.update(sorted(context.counters.items()))
  if context.failure_counts:
    info['failures (with repeats)'] = sum(context.failure_counts.values())
  with open(os.path.join(output, 'samples.csv'), 'w') as file:
    file.write(soak.csv_text(rows))
  summary = soak.summary_markdown(mode, options, rows, judged,
                                  context.failures, info)
  with open(os.path.join(output, 'summary.md'), 'w') as file:
    file.write(summary + '\n')
  for label, shingle in context.processes().items():
    with open(os.path.join(output, f'{label}.log.tail'), 'w') as file:
      file.write(checks.tail(shingle.log_text(), 40).replace(' | ', '\n'))
  print(summary, flush=True)
  return 1 if context.failures or soak.flagged(judged) else 0
