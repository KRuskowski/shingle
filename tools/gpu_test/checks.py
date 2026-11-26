"""Real-client checks of offscreen Shingle, run inside run.py's namespace.

These are DB-2's and DB-3's checks (docs/design/dmabuf.md, section 11):

- `offscreen`: offscreen Shingle starts on the GPU; a `wl_probe --pattern`
  shm client maps and `grim` shows its four quadrants at the view
  geometry; after `invert/toggle` the capture shows them inverted.
- `nested`: nested Shingle on lavapipe inside offscreen Shingle shows its
  own `wl_probe --pattern` client in captures of the outer output.
- `baseline`: what `vkcube`, `es2gears_wayland` and nested Shingle on the
  GPU do against a compositor without linux-dmabuf (`SHINGLE_DMABUF=0`).
  These are recorded (`INFO` lines), not judged; DB-4 compares against
  them, and its own checks are in dmabuf_checks.py.
- `buffers` (DB-3): a `wl_probe --fifo` client commits buffer `a` with a
  FIFO barrier and then `b` and `c` behind it in one flush; `b` is
  replaced before it is shown, and both `b` and `c` must be released.
- `shm` (DB-6): a `wl_probe --damage` client commits a 1000x1000 buffer
  from a sealed pool, then damages 10x10 pixels of it; Shingle's buffer
  trace must show the full upload and then 400 bytes, not 4 MB, and the
  probe must get the buffer back both times. A `wl_probe --animate`
  client then draws full-output frames, and the CPU time offscreen
  Shingle spent on them is recorded (`INFO`), not judged.

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
GROUPS = ('offscreen', 'nested', 'baseline', 'buffers', 'shm')
# `wl_probe --damage`'s buffer and square (crates/shingle/examples).
DAMAGE_FULL_BYTES = 1000 * 1000 * 4
DAMAGE_SQUARE_BYTES = 10 * 10 * 4
# Frames `wl_probe --animate` draws for the CPU measurement.
ANIMATE_FRAMES = 600
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
# The default `background_color`, 0.1 0.1 0.1, as the bytes Wayfire
# writes: 0.1 * 255 = 25.5, which the GPU may round either way.
BACKGROUND = (26, 26, 26)
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
  """Whether (x, y) of `shot` shows the default background, BACKGROUND,
  rather than client content."""
  return all(abs(value - expected) <= 3
             for value, expected in zip(shot.pixel(x, y), BACKGROUND))

def summary_lines(text):
  """The lines of `text` that name a driver (`*_RENDERER`, `*_VENDOR`,
  `*_VERSION`, `Selected GPU`) and its last two lines, joined by ' | '."""
  lines = text.strip().splitlines()
  named = [line.strip() for line in lines
           if any(key in line for key in ('RENDERER', 'VENDOR', 'VERSION',
                                          'Selected GPU'))]
  last = [line.strip() for line in lines[-2:] if line.strip() not in named]
  return ' | '.join(named + last)

def released_buffers(output):
  """The labels of the buffers a `wl_probe` reported `release` for, in
  `output`, its JSON lines; other lines are skipped."""
  released = set()
  for line in output.splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get('event') == 'release':
      released.add(event.get('buffer'))
  return released

def upload_sizes(log, surface=None):
  """[(surface, bytes)] of every `shm upload N bytes surface=S` line of
  Shingle's buffer trace in `log`, in order, for `surface` only if
  given."""
  uploads = []
  for line in log.splitlines():
    words = line.split()
    if 'upload' not in words or 'bytes' not in words:
      continue
    index = words.index('upload')
    if index == 0 or words[index - 1] != 'shm':
      continue
    try:
      size = int(words[index + 1])
      found = next(word.split('=', 1)[1] for word in words
                   if word.startswith('surface='))
    except (IndexError, ValueError, StopIteration):
      continue
    if surface is None or found == surface:
      uploads.append((found, size))
  return uploads

def damage_uploads(log):
  """The bytes of the upload after the first `DAMAGE_FULL_BYTES` one of
  the same surface in `log`: (full, next), with None for what is
  missing."""
  uploads = upload_sizes(log)
  for index, (surface, size) in enumerate(uploads):
    if size != DAMAGE_FULL_BYTES:
      continue
    later = [bytes_ for found, bytes_ in uploads[index + 1:]
             if found == surface]
    return size, (later[0] if later else None)
  return None, None

def buffer_point(geometry, point, size=1000):
  """Where pixel `point` of a `size`-pixel square buffer drawn at the
  view `geometry` lands on the output."""
  return (geometry['x'] + point[0] * geometry['width'] // size,
          geometry['y'] + point[1] * geometry['height'] // size)

def cpu_seconds(stat, ticks_per_second):
  """User plus system CPU seconds from the text of /proc/PID/stat."""
  # The command may contain spaces; the fields after it are fixed.
  fields = stat.rsplit(')', 1)[1].split()
  # utime and stime are fields 14 and 15, the 12th and 13th after ')'.
  return (int(fields[11]) + int(fields[12])) / ticks_per_second

def process_cpu_seconds(pid):
  """`cpu_seconds` of process `pid`, inside this PID namespace."""
  with open(f'/proc/{pid}/stat') as file:
    return cpu_seconds(file.read(), os.sysconf('SC_CLK_TCK'))

def probe_events(output, name):
  """The JSON events called `name` that a `wl_probe` printed."""
  events = []
  for line in output.splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get('event') == name:
      events.append(event)
  return events

def same_pixels(first, second):
  """Whether two captures, either of which may be None, are the same."""
  return (first is not None and second is not None
          and (first.width, first.height, first.pixels)
          == (second.width, second.height, second.pixels))

def tail(text, lines=6):
  """The last `lines` lines of `text`, joined by ' | '."""
  return ' | '.join(text.strip().splitlines()[-lines:])

class Shingle:
  """One Shingle in /tmp/NAME, with its own runtime, config, state and
  home directories, config file and IPC socket."""

  def __init__(self, binary, name, config, backend=('--backend', 'offscreen'),
               extra_environment=None, ini=None):
    """With `ini`, the text of a wayfire.ini, Shingle reads that instead
    of the YAML `config`, with a `[session]` section added."""
    if not safe_backend(list(backend)):
      raise ValueError(f'refusing to start shingle with {backend}')
    self.directory = os.path.join('/tmp', name)
    self.runtime = os.path.join(self.directory, 'runtime')
    for sub in ('runtime', 'config', 'state', 'home'):
      os.makedirs(os.path.join(self.directory, sub), mode=0o700)
    self.ipc_path = os.path.join(self.runtime, 'ipc')
    session_path = os.path.join(self.directory, 'state', 'session.json')
    if ini is not None:
      config_path = os.path.join(self.directory, 'wayfire.ini')
      with open(config_path, 'w') as file:
        file.write(ini)
        file.write(f'\n[session]\nrestore = false\npath = {session_path}\n')
    else:
      config = dict(config)
      config['session'] = {'restore': False, 'path': session_path}
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

  def settled(self, shingle, predicate, timeout=15.0, stable=False):
    """Captures until `predicate(shot)` holds or time runs out; returns
    the last capture and saves it when a directory was given. With
    `stable`, also until two captures in a row are the same, so that an
    open animation has ended: saved captures of static content are then
    comparable pixel for pixel between runs (DB-6)."""
    last = [None]
    previous = [None]

    def attempt():
      previous[0], last[0] = last[0], self.take(shingle)
      if last[0] is None or not predicate(last[0]):
        return False
      return not stable or same_pixels(previous[0], last[0])
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
        outer, lambda shot: not pattern_mismatches(shot, geometry),
        stable=True)
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
        lambda shot: not pattern_mismatches(shot, geometry, inverted=True),
        stable=True)
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
  """Where the one view on `outer`, nested's window, draws its surface,
  inside a server-side frame if it has one (WG-6), or None."""
  views = [view for view in outer.views() if view.get('mapped', True)]
  return views[0]['base-geometry'] if len(views) == 1 else None

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
      shot = captures.settled(outer, lambda shot: not wrong(shot),
                              stable=True)
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
  # Since DB-4 offscreen Shingle offers linux-dmabuf; the baseline is
  # without it.
  outer = Shingle(binary, 'baseline', BASE_CONFIG,
                  extra_environment={'SHINGLE_DMABUF': '0'})
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

def buffer_checks(results, binary, wl_probe, captures):
  """Buffers replaced behind a FIFO barrier are released (DB-3)."""
  outer = Shingle(binary, 'buffers', with_gaps(OUTER_GAPS))
  probe = Client(outer, [wl_probe, 'fifo', '--pattern', '--fifo'],
                 'wl_probe')
  try:
    view = xw.wait_for(lambda: outer.view('fifo'), timeout=15)
    results.check('buffers: the wl_probe --fifo client maps',
                  view is not None, tail(probe.output()))
    sent = xw.wait_for(lambda: '"event": "fifo"' in probe.output(),
                       timeout=15)
    results.check('buffers: the probe sent its FIFO commits', sent,
                  tail(probe.output()))
    # Waits the whole time when a release is missing, so that a late one
    # still counts.
    xw.wait_for(lambda: {'b', 'c'} <= released_buffers(probe.output()),
                timeout=10)
    released = released_buffers(probe.output())
    print(f'INFO buffers: released {sorted(released)}', flush=True)
    results.check(
        'buffers: both buffers committed behind the FIFO barrier are '
        'released',
        {'b', 'c'} <= released,
        f'released {sorted(released)}, {tail(probe.output())}')
    if view is not None:
      geometry = view['geometry']
      shot = captures.settled(
          outer, lambda shot: not pattern_mismatches(shot, geometry),
          stable=True)
      results.check(
          'buffers: the capture shows the last buffer\'s quadrants',
          shot is not None and not pattern_mismatches(shot, geometry),
          f'wrong {pattern_mismatches(shot, geometry) if shot else None}')
  finally:
    probe.stop()
  status = outer.stop()
  results.check('buffers: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}, {tail(outer.log_text())}')

def shm_checks(results, binary, wl_probe, captures):
  """Damage-limited shm uploads and the CPU cost of shm frames (DB-6)."""
  outer = Shingle(binary, 'shm', with_gaps(OUTER_GAPS),
                  extra_environment={'SHINGLE_TRACE_BUFFERS': '1'})
  probe = Client(outer, [wl_probe, 'damage', '--damage'], 'wl_probe')
  try:
    view = xw.wait_for(lambda: outer.view('damage'), timeout=15)
    results.check('shm: the wl_probe --damage client maps',
                  view is not None, tail(probe.output()))

    def done():
      steps = [event.get('step')
               for event in probe_events(probe.output(), 'damage')]
      releases = [event for event in probe_events(probe.output(), 'release')
                  if event.get('buffer') == 'damage']
      return 'square' in steps and len(releases) >= 2
    finished = xw.wait_for(done, timeout=15)
    full, square = damage_uploads(outer.log_text())
    print(f'INFO shm: the 1000x1000 buffer uploaded {full} bytes, then '
          f'{square} bytes for its 10x10 damage', flush=True)
    results.check('shm: the probe got its buffer back after each commit',
                  finished, tail(probe.output()))
    results.check('shm: the first commit uploads the whole buffer',
                  full == DAMAGE_FULL_BYTES, f'{full} bytes')
    results.check('shm: a 10x10 damage uploads 400 bytes, not 4 MB',
                  square == DAMAGE_SQUARE_BYTES, f'{square} bytes')
    if view is not None:
      # The buffer is drawn at the view's size: the square's centre and a
      # point of the white rest, in buffer pixels, scaled to the view.
      points = [(buffer_point(view['geometry'], point), colour)
                for point, colour in (((505, 505), 'red'),
                                      ((250, 250), 'white'))]

      def shows(shot):
        return all(classify(*shot.pixel(*point)) == colour
                   for point, colour in points)
      shot = captures.settled(outer, shows, stable=True)
      results.check(
          'shm: the capture shows the damaged square red on white',
          shot is not None and shows(shot),
          f'{[(point, shot.pixel(*point)) for point, _ in points]}'
          if shot else 'no capture')
  finally:
    probe.stop()
  status = outer.stop()
  results.check('shm: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}, {tail(outer.log_text())}')
  # The CPU measurement, without the trace and without gaps, so that the
  # frames cover the whole output.
  outer = Shingle(binary, 'shm-cpu', BASE_CONFIG)
  probe = Client(outer, [wl_probe, 'animate', '--animate',
                         str(ANIMATE_FRAMES)], 'wl_probe')
  try:
    view = xw.wait_for(lambda: outer.view('animate'), timeout=15)
    before = process_cpu_seconds(outer.process.pid)
    started = time.monotonic()
    animated = xw.wait_for(
        lambda: probe_events(probe.output(), 'animated'), timeout=120)
    elapsed = time.monotonic() - started
    after = process_cpu_seconds(outer.process.pid)
    geometry = view['geometry'] if view else {}
    used = after - before
    print(f'INFO shm: {ANIMATE_FRAMES} frames of '
          f'{geometry.get("width")}x{geometry.get("height")} in '
          f'{elapsed:.2f} s; offscreen Shingle used {used:.2f} s CPU, '
          f'{used * 1000 / ANIMATE_FRAMES:.2f} ms a frame', flush=True)
    results.check('shm: the wl_probe --animate client drew its frames',
                  bool(animated), tail(probe.output()))
  finally:
    probe.stop()
  status = outer.stop()
  results.check('shm: SIGTERM exits the measured Shingle with status 0',
                status == 0, f'status {status}, {tail(outer.log_text())}')

CHECKS = {
    'offscreen': offscreen_checks,
    'nested': nested_checks,
    'baseline': baseline_checks,
    'buffers': buffer_checks,
    'shm': shm_checks,
}

def all_checks():
  """DB-2's and DB-3's groups here, DB-4's and DB-5's
  (dmabuf_checks.py), MO-5's (output_checks.py), MO-6's
  (nested_output_checks.py), WG-2's (wayfire_checks.py), WG-5's
  (vswipe_checks.py), WG-6's (decoration_checks.py), WG-7's
  (switcher_checks.py), and the Wayfire comparisons of configured
  colours (colour_checks.py) and of focus after a minimize
  (focus_checks.py)."""
  import colour_checks
  import decoration_checks
  import dmabuf_checks
  import focus_checks
  import nested_output_checks
  import output_checks
  import switcher_checks
  import vswipe_checks
  import wayfire_checks
  return {**CHECKS, **dmabuf_checks.CHECKS, **output_checks.CHECKS,
          **nested_output_checks.CHECKS, **wayfire_checks.CHECKS,
          **vswipe_checks.CHECKS, **decoration_checks.CHECKS,
          **switcher_checks.CHECKS, **colour_checks.CHECKS,
          **focus_checks.CHECKS}

def run_checks(groups, binary, wl_probe, captures_directory=None):
  """Runs the check `groups` in order; returns 0 when all passed."""
  os.environ[NAMESPACE_VARIABLE] = '1'
  groups_by_name = all_checks()
  results = xw.Results()
  captures = Captures(captures_directory)
  for group in groups:
    try:
      groups_by_name[group](results, binary, wl_probe, captures)
    except (OSError, RuntimeError, TimeoutError, ConnectionError) as error:
      results.check(f'{group}: the checks ran to the end', False, str(error))
  passed = sum(1 for _, ok, _ in results.checks if ok)
  print(f'gpu_test: {passed} of {len(results.checks)} checks passed',
        flush=True)
  return 0 if results.all_passed() else 1
