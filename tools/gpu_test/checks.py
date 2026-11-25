"""Real-client checks of offscreen Shingle, run inside run.py's namespace.

These are DB-2's checks (docs/design/dmabuf.md, section 11):

- `offscreen`: offscreen Shingle starts on the GPU; a `wl_probe --pattern`
  shm client maps and `grim` shows its four quadrants at the view
  geometry; after `invert/toggle` the capture shows them inverted.
- `nested`: nested Shingle on lavapipe inside offscreen Shingle shows its
  own `wl_probe --pattern` client in captures of the outer output.
- `baseline`: what `vkcube`, `es2gears_wayland` and nested Shingle on the
  GPU do against a compositor without linux-dmabuf. These are recorded
  (`INFO` lines), not judged; DB-4 compares against them.

Only run.py imports this, after it has built the namespace, so every
process started here sees only the private /dev, /tmp and runtime
directory. Shingle always gets an explicit `--backend offscreen` or
`--backend nested` (`safe_backend`), and every signal goes through
`run.signal_checked` to a child of this process inside the PID namespace.
"""

import importlib.util
import json
import os
import signal
import subprocess
import time

import run

LAVAPIPE = '/usr/share/vulkan/icd.d/lvp_icd.json'
NVIDIA_ICD = '/etc/vulkan/icd.d/nvidia_icd.json'
NAMESPACE_VARIABLE = 'SHINGLE_GPU_TEST_NAMESPACE'
GROUPS = ('offscreen', 'nested', 'baseline')
# What every Shingle here runs with: no X display, one workspace, no
# session restore or config reload.
BASE_CONFIG = {
    'xwayland': {'mode': False},
    'workspaces': {'width': 1, 'height': 1},
    'behavior': {'auto_reload': False},
}
# Outer gaps, so that views are not at the output's origin and the
# background shows around them.
OUTER_GAPS = {'tiling': {'gaps': {'top': 30, 'right': 50, 'bottom': 40,
                                  'left': 60, 'inner': 0}}}
INNER_GAPS = {'tiling': {'gaps': {'top': 20, 'right': 10, 'bottom': 15,
                                  'left': 25, 'inner': 0}}}
OUTPUT = (1280, 720)
# The quadrants of `wl_probe --pattern` and what inversion makes of them.
PATTERN = (('red', 'green'), ('blue', 'white'))
INVERTED = {'red': 'cyan', 'green': 'magenta', 'blue': 'yellow',
            'white': 'black'}

def _load_xwayland_harness():
  """The XWayland harness module, for its IPC, capture and result
  helpers. Importing it runs nothing."""
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..',
                      'xwayland_test', 'run.py')
  spec = importlib.util.spec_from_file_location('xwayland_run', path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

xw = _load_xwayland_harness()

def safe_backend(arguments):
  """Whether `arguments` name a backend that cannot touch the host's
  display, GPU card node or input: `--headless`, or `--backend` (or
  `--backend=`) `nested` or `offscreen`. Without one, `shingle` could
  select native."""
  for index, argument in enumerate(arguments):
    if argument == '--headless':
      return True
    if argument.startswith('--backend='):
      return argument.split('=', 1)[1] in ('nested', 'offscreen')
    if argument == '--backend' and index + 1 < len(arguments):
      return arguments[index + 1] in ('nested', 'offscreen')
  return False

def classify(r, g, b):
  """The name of a clearly saturated or grey colour, or None."""
  high = [value > 200 for value in (r, g, b)]
  low = [value < 60 for value in (r, g, b)]
  if not all(h or l for h, l in zip(high, low)):
    return None
  names = {
      (True, False, False): 'red',
      (False, True, False): 'green',
      (False, False, True): 'blue',
      (True, True, True): 'white',
      (False, True, True): 'cyan',
      (True, False, True): 'magenta',
      (True, True, False): 'yellow',
      (False, False, False): 'black',
  }
  return names[tuple(high)]

def quadrant_points(geometry, offset=(0, 0)):
  """[((x, y), (column, row))] at the centre of each quadrant of the view
  `geometry` ({'x', 'y', 'width', 'height'}), shifted by `offset`."""
  points = []
  for row in (0, 1):
    for column in (0, 1):
      x = geometry['x'] + offset[0] + geometry['width'] * (1 + 2 * column) // 4
      y = geometry['y'] + offset[1] + geometry['height'] * (1 + 2 * row) // 4
      points.append(((x, y), (column, row)))
  return points

def pattern_mismatches(shot, geometry, inverted=False, offset=(0, 0)):
  """The quadrant points of `geometry` where `shot` does not show the
  probe's pattern (inverted if asked), as (point, expected, actual)."""
  wrong = []
  for (x, y), (column, row) in quadrant_points(geometry, offset):
    expected = PATTERN[row][column]
    if inverted:
      expected = INVERTED[expected]
    if not (0 <= x < shot.width and 0 <= y < shot.height):
      wrong.append(((x, y), expected, 'outside'))
      continue
    actual = classify(*shot.pixel(x, y))
    if actual != expected:
      wrong.append(((x, y), expected, actual or str(shot.pixel(x, y))))
  return wrong

def with_gaps(gaps):
  """BASE_CONFIG with `gaps` ({'tiling': ...})."""
  config = dict(BASE_CONFIG)
  config.update(gaps)
  return config

def background_at(shot, x, y):
  """Whether (x, y) of `shot` shows the default background, 0.1 linear
  or 89 in sRGB, rather than client content."""
  return all(abs(value - 89) <= 3 for value in shot.pixel(x, y))

def summary_lines(text):
  """The lines of `text` that name a driver (`*_RENDERER`, `*_VENDOR`,
  `*_VERSION`, `Selected GPU`) and its last two lines, joined by ' | '."""
  lines = text.strip().splitlines()
  named = [line.strip() for line in lines
           if any(key in line for key in ('RENDERER', 'VENDOR', 'VERSION',
                                          'Selected GPU'))]
  last = [line.strip() for line in lines[-2:] if line.strip() not in named]
  return ' | '.join(named + last)

def tail(text, lines=6):
  """The last `lines` lines of `text`, joined by ' | '."""
  return ' | '.join(text.strip().splitlines()[-lines:])

class Shingle:
  """One Shingle in /tmp/NAME, with its own runtime, config, state and
  home directories, config file and IPC socket."""

  def __init__(self, binary, name, config, backend=('--backend', 'offscreen'),
               extra_environment=None):
    if not safe_backend(list(backend)):
      raise ValueError(f'refusing to start shingle with {backend}')
    self.directory = os.path.join('/tmp', name)
    self.runtime = os.path.join(self.directory, 'runtime')
    for sub in ('runtime', 'config', 'state', 'home'):
      os.makedirs(os.path.join(self.directory, sub), mode=0o700)
    self.ipc_path = os.path.join(self.runtime, 'ipc')
    config = dict(config)
    config['session'] = {
        'restore': False,
        'path': os.path.join(self.directory, 'state', 'session.json'),
    }
    config_path = os.path.join(self.directory, 'shingle.yaml')
    with open(config_path, 'w') as file:
      # YAML accepts JSON.
      json.dump(config, file)
    environment = {key: value for key, value in os.environ.items()
                   if key not in run.STRIPPED_VARIABLES}
    environment.update({
        'XDG_RUNTIME_DIR': self.runtime,
        'XDG_CONFIG_HOME': os.path.join(self.directory, 'config'),
        'XDG_STATE_HOME': os.path.join(self.directory, 'state'),
        'HOME': os.path.join(self.directory, 'home'),
    })
    environment.update(extra_environment or {})
    arguments = [binary, *backend, '--config', config_path,
                 '--ipc', self.ipc_path]
    self.log_path = os.path.join(self.directory, 'shingle.log')
    self.log = open(self.log_path, 'w')
    self.process = subprocess.Popen(
        arguments, env=environment, stdout=self.log, stderr=self.log,
        stdin=subprocess.DEVNULL)
    self.ipc = self._connect()

  def _connect(self):
    """Waits for the IPC socket and connects."""
    deadline = time.monotonic() + 30
    while True:
      if self.process.poll() is not None:
        raise RuntimeError(f'shingle exited: {tail(self.log_text())}')
      try:
        return xw.Ipc(self.ipc_path)
      except (FileNotFoundError, ConnectionRefusedError):
        if time.monotonic() > deadline:
          raise
        time.sleep(0.05)

  def call(self, method, data=None):
    """One IPC request."""
    return self.ipc.call(method, data)

  def views(self):
    """`window-rules/list-views`."""
    return self.call('window-rules/list-views')

  def view(self, app_id):
    """The one mapped view with `app_id`, or None."""
    views = [view for view in self.views()
             if view.get('app-id') == app_id and view.get('mapped', True)]
    return views[0] if len(views) == 1 else None

  def wayland_display(self):
    """The absolute path of the socket Shingle printed, or None."""
    name = xw.wait_for(lambda: next(
        (line.split('=', 1)[1] for line in self.log_text().splitlines()
         if line.startswith('WAYLAND_DISPLAY=')), None), timeout=30)
    return os.path.join(self.runtime, name) if name else None

  def client_environment(self, extra=None):
    """The environment of a Wayland client of this Shingle."""
    environment = {key: value for key, value in os.environ.items()
                   if key not in run.STRIPPED_VARIABLES}
    environment.update({
        'XDG_RUNTIME_DIR': self.runtime,
        'WAYLAND_DISPLAY': self.wayland_display(),
        NAMESPACE_VARIABLE: '1',
    })
    environment.update(extra or {})
    return environment

  def log_text(self):
    """Shingle's output so far."""
    if not self.log.closed:
      self.log.flush()
    with open(self.log_path, errors='replace') as file:
      return file.read()

  def alive(self):
    """Whether Shingle still runs."""
    return self.process.poll() is None

  def stop(self):
    """SIGTERM through `signal_checked`, then SIGKILL after 10 s; returns
    the exit status or 'timeout'."""
    self.ipc.close()
    try:
      return _stop(self.process, 'shingle', 10)
    finally:
      self.log.close()

def _stop(process, name, timeout):
  """Ends `process`, whose command is `name`, as `Shingle.stop` does. A
  process that `signal_checked` refuses to signal is left to the end of
  the PID namespace, which kills it."""
  if process.poll() is not None:
    return process.returncode
  run.signal_checked(process.pid, signal.SIGTERM, name)
  try:
    return process.wait(timeout)
  except subprocess.TimeoutExpired:
    run.signal_checked(process.pid, signal.SIGKILL, name)
  try:
    process.wait(timeout)
    return 'timeout'
  except subprocess.TimeoutExpired:
    return 'left running'

class Client:
  """A Wayland client of a Shingle, with its output in a log file."""

  def __init__(self, shingle, arguments, name, extra_environment=None,
               program=None):
    self.name = name
    # The command name signals are checked against.
    self.program = program or os.path.basename(arguments[0])
    self.log_path = os.path.join(shingle.directory, f'{name}.log')
    self.log = open(self.log_path, 'w')
    self.process = subprocess.Popen(
        arguments, env=shingle.client_environment(extra_environment),
        stdout=self.log, stderr=self.log, stdin=subprocess.DEVNULL)

  def exited(self, timeout):
    """The exit status within `timeout` seconds, or None."""
    try:
      return self.process.wait(timeout)
    except subprocess.TimeoutExpired:
      return None

  def output(self):
    """What the client printed."""
    if not self.log.closed:
      self.log.flush()
    with open(self.log_path, errors='replace') as file:
      return file.read()

  def stop(self):
    """Ends the client through `signal_checked`; returns its status."""
    try:
      return _stop(self.process, self.program, 5)
    finally:
      self.log.close()

class Captures:
  """Takes `grim` captures and optionally saves the settled ones."""

  def __init__(self, directory=None):
    self.directory = directory
    self.saved = 0

  def take(self, shingle):
    """A capture of `shingle`'s output, or None."""
    try:
      done = subprocess.run(['grim', '-t', 'ppm', '-'],
                            env=shingle.client_environment(),
                            capture_output=True, timeout=20,
                            stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
      return None
    parsed = xw.parse_ppm(done.stdout) if done.returncode == 0 else None
    return xw.Capture(*parsed) if parsed else None

  def settled(self, shingle, predicate, timeout=15.0):
    """Captures until `predicate(shot)` holds or time runs out; returns
    the last capture and saves it when a directory was given."""
    last = [None]

    def attempt():
      last[0] = self.take(shingle)
      return last[0] is not None and predicate(last[0])
    xw.wait_for(attempt, timeout=timeout, interval=0.3)
    if self.directory and last[0] is not None:
      os.makedirs(self.directory, exist_ok=True)
      path = os.path.join(self.directory, f'{self.saved:03}.ppm')
      self.saved += 1
      with open(path, 'wb') as file:
        file.write(xw.encode_ppm(last[0]))
    return last[0]

def offscreen_checks(results, binary, wl_probe, captures):
  """Offscreen Shingle on the GPU with an shm client, and inversion."""
  outer = Shingle(binary, 'offscreen', with_gaps(OUTER_GAPS))
  started = xw.wait_for(lambda: 'rendering offscreen on' in outer.log_text(),
                        timeout=30)
  adapter = next((line for line in outer.log_text().splitlines()
                  if 'rendering offscreen on' in line), '')
  results.check('offscreen: Shingle starts and renders on NVIDIA',
                started and 'NVIDIA' in adapter and outer.alive(),
                adapter or tail(outer.log_text()))
  dmabuf = next((line for line in outer.log_text().splitlines()
                 if 'DMA-BUF' in line or 'dmabuf' in line.lower()), '')
  print(f'INFO offscreen: {dmabuf}', flush=True)
  probe = Client(outer, [wl_probe, 'pattern', '--pattern'], 'wl_probe')
  try:
    view = xw.wait_for(lambda: outer.view('pattern'), timeout=15)
    results.check('offscreen: the wl_probe shm client maps',
                  view is not None, tail(probe.output()))
    if view is None:
      return outer.stop()
    geometry = view['geometry']
    shot = captures.settled(
        outer, lambda shot: not pattern_mismatches(shot, geometry))
    results.check(
        'offscreen: grim shows the probe\'s quadrants at its view geometry',
        shot is not None and (shot.width, shot.height) == OUTPUT
        and not pattern_mismatches(shot, geometry),
        f'geometry {json.dumps(geometry)}, '
        f'size {(shot.width, shot.height) if shot else None}, '
        f'wrong {pattern_mismatches(shot, geometry) if shot else None}')
    gap = (geometry['x'] // 2, geometry['y'] // 2)
    results.check(
        'offscreen: the gaps around the view show the background',
        shot is not None and min(gap) > 0 and background_at(shot, *gap),
        f'pixel {shot.pixel(*gap) if shot else None} at {gap}')
    outer.call('invert/toggle')
    shot = captures.settled(
        outer,
        lambda shot: not pattern_mismatches(shot, geometry, inverted=True))
    results.check(
        'offscreen: after invert/toggle the capture shows the quadrants '
        'inverted',
        shot is not None
        and not pattern_mismatches(shot, geometry, inverted=True),
        f'wrong {pattern_mismatches(shot, geometry, True) if shot else None}')
    outer.call('invert/toggle')
  finally:
    probe.stop()
  status = outer.stop()
  results.check('offscreen: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}, {tail(outer.log_text())}')
  return status

def _nested(binary, outer, name, extra_environment=None, config=None):
  """Nested Shingle in a window on `outer`'s output."""
  environment = {'WAYLAND_DISPLAY': outer.wayland_display()}
  environment.update(extra_environment or {})
  return Shingle(binary, name, config or BASE_CONFIG,
                 backend=('--backend', 'nested'),
                 extra_environment=environment)

def _window_geometry(outer):
  """The geometry of the one view on `outer`, nested's window, or None."""
  views = [view for view in outer.views() if view.get('mapped', True)]
  return views[0]['geometry'] if len(views) == 1 else None

def nested_checks(results, binary, wl_probe, captures):
  """Nested Shingle on lavapipe inside offscreen Shingle."""
  outer = Shingle(binary, 'outer', with_gaps(OUTER_GAPS))
  inner = _nested(binary, outer, 'inner', {'VK_ICD_FILENAMES': LAVAPIPE},
                  config=with_gaps(INNER_GAPS))
  window = xw.wait_for(lambda: _window_geometry(outer), timeout=30)
  results.check('nested: nested Shingle on lavapipe maps a window in '
                'offscreen Shingle',
                window is not None and inner.alive(),
                tail(inner.log_text()))
  probe = Client(inner, [wl_probe, 'nested-pattern', '--pattern'],
                 'wl_probe')
  try:
    view = xw.wait_for(lambda: inner.view('nested-pattern'), timeout=15)
    results.check('nested: its wl_probe client maps', view is not None,
                  tail(probe.output()))
    if view is not None and window is not None:
      geometry = view['geometry']
      offset = (window['x'], window['y'])

      def wrong(shot):
        return pattern_mismatches(shot, geometry, offset=offset)
      shot = captures.settled(outer, lambda shot: not wrong(shot))
      results.check(
          'nested: captures of the outer output show the nested client\'s '
          'quadrants',
          shot is not None and not wrong(shot),
          f'window {json.dumps(window)}, view {json.dumps(geometry)}, '
          f'wrong {wrong(shot) if shot else None}')
  finally:
    probe.stop()
  status = inner.stop()
  results.check('nested: SIGTERM exits nested Shingle with status 0',
                status == 0, f'status {status}')
  status = outer.stop()
  results.check('nested: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def _record_client(outer, captures, name, arguments, run_for,
                   program=None):
  """Runs a GPU client against `outer` for up to `run_for` seconds and
  describes what happened: whether it mapped a view, the pixel at its
  centre, its exit status and output."""
  client = Client(outer, arguments, name.replace(' ', '-'), program=program)
  mapped = xw.wait_for(lambda: bool(outer.views()), timeout=run_for)
  drawn = None
  if mapped:
    background = None
    shot = captures.take(outer)
    if shot is not None:
      background = shot.pixel(0, 0)
      view = outer.views()[0]['geometry']
      centre = (view['x'] + view['width'] // 2,
                view['y'] + view['height'] // 2)
      drawn = shot.pixel(*centre)
      drawn = f'{drawn} at {centre} (background {background})'
  status = client.exited(run_for)
  if status is None:
    status = f'still running, stopped: {client.stop()}'
  else:
    client.stop()
  return (f'mapped {bool(mapped)}, centre pixel {drawn}, exit {status}, '
          f'output: {summary_lines(client.output()) or "none"}')

def baseline_checks(results, binary, wl_probe, captures):
  """Records what GPU clients do without linux-dmabuf."""
  del wl_probe
  outer = Shingle(binary, 'baseline', BASE_CONFIG)
  vkcube = ['vkcube', '--wsi', 'wayland', '--c', '300']
  # stdbuf, so that -info's lines survive the SIGTERM that ends it.
  gears = ['stdbuf', '-oL', 'es2gears_wayland', '-info']
  for name, arguments, run_for in (
      ('vkcube', vkcube, 20),
      ('vkcube on GPU 0', vkcube + ['--gpu_number', '0'], 20),
      ('es2gears_wayland', gears, 8)):
    summary = _record_client(outer, captures, name, arguments, run_for,
                             program=os.path.basename(arguments[0])
                             if arguments[0] != 'stdbuf' else arguments[2])
    print(f'INFO baseline: {name}: {summary}', flush=True)
    results.check(f'baseline: {name} was run and recorded',
                  outer.alive(), summary)
    xw.wait_for(lambda: not outer.views(), timeout=10)
  for name, environment in (
      ('nested Shingle', {}),
      ('nested Shingle with only the NVIDIA ICD',
       {'VK_ICD_FILENAMES': NVIDIA_ICD})):
    inner = None
    try:
      inner = _nested(binary, outer, name.replace(' ', '-'), environment)
    except RuntimeError as error:
      print(f'INFO baseline: {name}: did not start: {error}', flush=True)
    if inner is not None:
      window = xw.wait_for(lambda: _window_geometry(outer), timeout=20)
      print(f'INFO baseline: {name}: window {json.dumps(window)}, running '
            f'{inner.alive()}, log: {tail(inner.log_text(), 8)}',
            flush=True)
      inner.stop()
    results.check(f'baseline: {name} was run and recorded', outer.alive())
    xw.wait_for(lambda: not outer.views(), timeout=10)
  status = outer.stop()
  results.check('baseline: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

CHECKS = {
    'offscreen': offscreen_checks,
    'nested': nested_checks,
    'baseline': baseline_checks,
}

def run_checks(groups, binary, wl_probe, captures_directory=None):
  """Runs the check `groups` in order; returns 0 when all passed."""
  os.environ[NAMESPACE_VARIABLE] = '1'
  results = xw.Results()
  captures = Captures(captures_directory)
  for group in groups:
    try:
      CHECKS[group](results, binary, wl_probe, captures)
    except (OSError, RuntimeError, TimeoutError, ConnectionError) as error:
      results.check(f'{group}: the checks ran to the end', False, str(error))
  passed = sum(1 for _, ok, _ in results.checks if ok)
  print(f'gpu_test: {passed} of {len(results.checks)} checks passed',
        flush=True)
  return 0 if results.all_passed() else 1
