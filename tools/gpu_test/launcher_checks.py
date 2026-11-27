"""Real-client checks of shingle-launcher, run inside run.py's namespace.

docs/design/launcher.md, section 11. Offscreen Shingle on the render node
runs with distinctive launcher colours; `shingle-launcher --trace --timing`
(the release build by default, `run.py --launcher PATH`) runs as its client
with a private XDG_DATA_HOME, XDG_STATE_HOME and XDG_CACHE_HOME and an
XDG_DATA_DIRS holding only three `.desktop` files that start `wl_probe`
windows, and with `wl_probe` on PATH. STIPC keys and pointer drive it, its
`--trace` lines (one JSON object per frame, launch and close) say what it
shows and where, and `launcher.yaml` what it saved. The group `launcher`
checks type-to-launch, favouriting, renaming, a shortcut from the typed
text, moving to a category by keyboard and by drag, the context menu,
persistence across restarts, the toggle, closing by a click outside, and
the startup time; then type-to-launch again under the host's `wayfire`,
headless in the namespace (wayfire_reference.py), since the launcher is
meant to work there too.

Signals go through `run.signal_checked`, via checks.py's Shingle and Client
helpers, and the launched probes through `signal_checked` with their comm.
"""

import json
import os
import signal
import statistics
import time

import checks
import run
import wayfire_reference

xw = checks.xw
GROUPS = ('launcher',)
LAUNCHER_VARIABLE = 'SHINGLE_LAUNCHER_BINARY'
# The comm the kernel keeps of `shingle-launcher` (15 characters).
LAUNCHER_COMM = 'shingle-launcher'
# The first-frame target of the design, in milliseconds.
STARTUP_TARGET_MS = 100.0
# Warm starts measured.
STARTS = 7
# Headless Wayfire with IPC and STIPC for keys, and layer-shell, which is
# Wayfire's own.
WAYFIRE_INI = """[core]
plugins = ipc ipc-rules stipc
"""
# Colours the checks can tell apart in captures.
PANEL = [1.0, 0.0, 1.0, 1.0]
BACKDROP = [0.0, 0.0, 0.0, 0.5]
PROBES = (('alpha', 'Alpha Probe', 'Utility;'),
          ('beta', 'Beta Probe', 'Graphics;'),
          ('gamma', 'Gamma Probe', 'Office;'))
KEYS = {'enter': 'KEY_ENTER', 'esc': 'KEY_ESC', 'tab': 'KEY_TAB',
        'up': 'KEY_UP', 'down': 'KEY_DOWN', 'f2': 'KEY_F2',
        'backspace': 'KEY_BACKSPACE', 'delete': 'KEY_DELETE',
        'ctrl': 'KEY_LEFTCTRL', 'shift': 'KEY_LEFTSHIFT'}

def desktop_entry(name, command, categories):
  """The text of a desktop entry file."""
  return ('[Desktop Entry]\nType=Application\n'
          f'Name={name}\nExec={command}\nCategories={categories}\n')

def trace_events(text):
  """The JSON objects of a `--trace` log, in order."""
  events = []
  for line in text.splitlines():
    if not line.startswith('{'):
      continue
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict):
      events.append(event)
  return events

def last_frame(events):
  """The last `frame` event, or None."""
  frames = [event for event in events if event.get('event') == 'frame']
  return frames[-1] if frames else None

def row(frame, name):
  """The row named `name` of `frame`, or None."""
  return next((entry for entry in (frame or {}).get('rows', [])
               if entry.get('name') == name), None)

def category(frame, title):
  """The category titled `title` of `frame`, or None."""
  return next((entry for entry in (frame or {}).get('categories', [])
               if entry.get('title') == title), None)

def menu_item(frame, label):
  """The context menu item `label` of `frame`, or None."""
  return next((entry for entry in (frame or {}).get('menu') or []
               if entry.get('label') == label), None)

def centre(rect):
  """The integer centre of a trace rectangle."""
  return (int(rect['x'] + rect['width'] / 2),
          int(rect['y'] + rect['height'] / 2))

def first_frame_ms(text):
  """The launcher's own first-frame time from its `--timing` line."""
  for line in text.splitlines():
    if 'first frame after' in line:
      try:
        return float(line.split('first frame after', 1)[1].split()[0])
      except (IndexError, ValueError):
        return None
  return None

def summary(values):
  """`median, min-max` of `values` in ms, for INFO lines."""
  if not values:
    return 'none'
  return (f'median {statistics.median(values):.1f} ms, '
          f'{min(values):.1f}-{max(values):.1f} ms')

class Fixture:
  """The private data, state and cache directories, three desktop files
  and a PATH directory holding `wl_probe`."""

  def __init__(self, shingle, wl_probe):
    self.directory = os.path.join(shingle.directory, 'launcher')
    self.data_home = os.path.join(self.directory, 'data')
    self.system = os.path.join(self.directory, 'system')
    self.bin = os.path.join(self.directory, 'bin')
    applications = os.path.join(self.system, 'applications')
    for path in (self.data_home, applications, self.bin):
      os.makedirs(path)
    os.symlink(wl_probe, os.path.join(self.bin, 'wl_probe'))
    for app_id, name, categories in PROBES:
      with open(os.path.join(applications, f'{app_id}.desktop'), 'w') as file:
        file.write(desktop_entry(name, f'{wl_probe} {app_id}', categories))
    self.layout_path = os.path.join(self.data_home, 'shingle',
                                    'launcher.yaml')

  def environment(self):
    """What the launcher runs with besides the client environment."""
    return {
        'XDG_DATA_HOME': self.data_home,
        'XDG_DATA_DIRS': self.system,
        'XDG_STATE_HOME': os.path.join(self.directory, 'state'),
        'XDG_CACHE_HOME': os.path.join(self.directory, 'cache'),
        'PATH': f'{self.bin}:{os.environ.get("PATH", "/usr/bin:/bin")}',
    }

  def layout_text(self):
    """launcher.yaml as written, or ''."""
    try:
      with open(self.layout_path) as file:
        return file.read()
    except FileNotFoundError:
      return ''

class Launcher:
  """One run of the launcher against `shingle`."""

  count = 0

  def __init__(self, shingle, binary, fixture, config_path):
    Launcher.count += 1
    self.shingle = shingle
    self.started = time.monotonic()
    self.client = checks.Client(
        shingle,
        [binary, '--config', config_path, '--trace', '--timing'],
        f'launcher-{Launcher.count}', extra_environment=fixture.environment(),
        program=LAUNCHER_COMM)

  def events(self):
    """Its trace so far."""
    return trace_events(self.client.output())

  def frame(self):
    """Its last frame."""
    return last_frame(self.events())

  def wait_frame(self, predicate, timeout=10):
    """Waits for a frame `predicate` accepts; returns it or None."""
    def match():
      frame = self.frame()
      return frame if frame and predicate(frame) else None
    return xw.wait_for(match, timeout=timeout, interval=0.01)

  def key(self, *names):
    """Presses the keys `names` (KEYS names or evdev names) together."""
    keys = [KEYS.get(name, name) for name in names]
    for key in keys:
      self.shingle.call('stipc/feed_key', {'key': key, 'state': True})
    for key in reversed(keys):
      self.shingle.call('stipc/feed_key', {'key': key, 'state': False})

  def type(self, text):
    """Types `text` through STIPC keys."""
    for key, shifted in checks_app().key_sequence(text):
      if shifted:
        self.key('shift', key)
      else:
        self.key(key)

  def move(self, point):
    """Moves the STIPC pointer to `point`."""
    self.shingle.call('stipc/move_cursor', {'x': point[0], 'y': point[1]})

  def button(self, mode, button='BTN_LEFT'):
    """Presses, releases or clicks (`full`) a button."""
    self.shingle.call('stipc/feed_button', {'combo': button, 'mode': mode})

  def exited(self, timeout=10):
    """Its exit status within `timeout`, or None."""
    return self.client.exited(timeout)

  def stop(self):
    """Ends it if it still runs."""
    return self.client.stop()

def checks_app():
  """app_checks, for its key tables; imported late, as run.py does."""
  import app_checks
  return app_checks

def end_launched(events):
  """Ends the `wl_probe` clients the launcher started, by the PIDs of its
  launch events, through `signal_checked` with their comm."""
  for event in events:
    pid = event.get('pid') if event.get('event') == 'launch' else None
    if isinstance(pid, int):
      run.signal_checked(pid, signal.SIGTERM, 'wl_probe')

def launcher_config():
  """BASE_CONFIG with the distinctive launcher colours."""
  config = dict(checks.BASE_CONFIG)
  config['launcher'] = {'colors': {'panel': PANEL, 'backdrop': BACKDROP}}
  return config

def launcher_checks(results, binary, wl_probe, captures):
  """Type-to-launch, the in-place edits, persistence and startup time."""
  launcher_binary = os.environ.get(LAUNCHER_VARIABLE)
  if not launcher_binary or not os.access(launcher_binary, os.X_OK):
    results.check('launcher: the shingle-launcher binary exists', False,
                  f'{LAUNCHER_VARIABLE}={launcher_binary}')
    return
  shingle = checks.Shingle(binary, 'launcher', launcher_config())
  config_path = os.path.join(shingle.directory, 'shingle.yaml')
  fixture = Fixture(shingle, wl_probe)
  launched = []
  try:
    _run_scenario(results, shingle, launcher_binary, fixture, config_path,
                  captures, launched)
  finally:
    end_launched(launched)
    status = shingle.stop()
    results.check('launcher: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')
  _under_wayfire(results, launcher_binary, fixture, config_path)

def _under_wayfire(results, binary, fixture, config_path):
  """Type-to-launch under headless Wayfire; INFO when it cannot run."""
  try:
    wayfire = wayfire_reference.Wayfire('launcher-wayfire', WAYFIRE_INI)
  except RuntimeError as error:
    print(f'INFO launcher: no headless Wayfire to run under: {error}',
          flush=True)
    return
  launched = []
  try:
    launcher, frame = _start(wayfire, binary, fixture, config_path, results,
                             'under Wayfire')
    if frame is None:
      return
    launcher.type('beta')
    launcher.wait_frame(lambda frame: frame['rows']
                        and frame['rows'][0]['name'] == 'Beta Probe')
    launcher.key('enter')
    status = launcher.exited()
    _finish(launcher, launched)
    mapped = xw.wait_for(lambda: wayfire.view('beta'), timeout=15)
    results.check('launcher: under headless Wayfire it shows, takes typed '
                  'text and launches', status == 0 and bool(mapped),
                  f'status {status}, {checks.tail(launcher.client.output())}')
  finally:
    end_launched(launched)
    wayfire.stop()

def _start(shingle, binary, fixture, config_path, results, step):
  """Starts the launcher and waits for its first frame."""
  launcher = Launcher(shingle, binary, fixture, config_path)
  frame = launcher.wait_frame(lambda frame: True, timeout=20)
  if frame is None:
    results.check(f'launcher: {step}: the launcher shows a frame', False,
                  checks.tail(launcher.client.output()))
  return launcher, frame

def _finish(launcher, launched):
  """Collects a finished launcher's launches and ends it."""
  launched.extend(launcher.events())
  launcher.stop()

def _run_scenario(results, shingle, binary, fixture, config_path, captures,
                  launched):
  """The steps of the group, in order; each starts its own launcher."""
  # 1. The first start scans and shows the panel over a backdrop.
  launcher, frame = _start(shingle, binary, fixture, config_path,
                           results, 'start')
  if frame is None:
    return
  cold = first_frame_ms(launcher.client.output())
  print(f'INFO launcher: first start without a cache: {cold} ms', flush=True)
  panel = frame['panel']
  inside = (int(panel['x']) + 6, int(panel['y'] + panel['height']) - 6)
  outside = (int(panel['x']) - 20, int(panel['y']) + 20)

  def shows_panel(shot):
    return (checks.classify(*shot.pixel(*inside)) == 'magenta'
            and checks.classify(*shot.pixel(*outside)) == 'black')
  shot = captures.settled(shingle, shows_panel)
  results.check(
      'launcher: grim shows the panel in the middle over a darkened output',
      shot is not None and shows_panel(shot),
      f'inside {shot.pixel(*inside)}, outside {shot.pixel(*outside)}'
      if shot else 'no capture')
  results.check(
      'launcher: the categories and the fixture entries are shown',
      category(frame, 'Favourites') and category(frame, 'Accessories')
      and row(frame, 'Alpha Probe') and row(frame, 'Gamma Probe'),
      json.dumps(frame)[:300])

  # 2. Type-to-launch.
  launcher.type('alpha')
  typed = launcher.wait_frame(
      lambda frame: frame['query'] == 'alpha' and frame['rows']
      and frame['rows'][0]['name'] == 'Alpha Probe')
  results.check('launcher: typing filters to the best match',
                typed is not None, json.dumps(launcher.frame())[:300])
  launcher.key('enter')
  status = launcher.exited()
  _finish(launcher, launched)
  mapped = xw.wait_for(lambda: shingle.view('alpha'), timeout=15)
  results.check('launcher: Enter launches it, the launcher exits 0 and the '
                'window maps', status == 0 and bool(mapped),
                f'status {status}, views {shingle.views()}')

  # 3. Ctrl+D favourites.
  launcher, frame = _start(shingle, binary, fixture, config_path,
                           results, 'favourite')
  if frame is None:
    return
  launcher.type('beta')
  launcher.wait_frame(lambda frame: frame['query'] == 'beta')
  launcher.key('ctrl', 'KEY_D')
  favoured = launcher.wait_frame(
      lambda frame: (row(frame, 'Beta Probe') or {}).get('favourite'))
  results.check('launcher: Ctrl+D marks the entry a favourite and saves it',
                favoured is not None
                and 'favourites:\n- beta.desktop' in fixture.layout_text(),
                fixture.layout_text())

  # 4. F2 renames.
  launcher.key('ctrl', 'KEY_U')
  launcher.type('gamma')
  launcher.wait_frame(lambda frame: frame['query'] == 'gamma')
  launcher.key('f2')
  launcher.wait_frame(lambda frame: frame['prompt'])
  launcher.key('ctrl', 'KEY_U')
  launcher.type('Delta Ray')
  launcher.key('enter')
  launcher.wait_frame(lambda frame: not frame['prompt'])
  launcher.key('esc')
  launcher.type('delta')
  renamed = launcher.wait_frame(
      lambda frame: frame['rows'] and frame['rows'][0]['name'] == 'Delta Ray')
  results.check('launcher: F2 renames in place, the new name is searched '
                'and saved', renamed is not None
                and 'gamma.desktop: Delta Ray' in fixture.layout_text(),
                fixture.layout_text())

  # 5. Ctrl+N adds a shortcut from the typed text; 6. Ctrl+M moves it.
  launcher.key('esc')
  launcher.type('wl_probe epsilon')
  launcher.wait_frame(lambda frame: frame['query'] == 'wl_probe epsilon')
  launcher.key('ctrl', 'KEY_N')
  launcher.wait_frame(lambda frame: frame['prompt'])
  launcher.key('ctrl', 'KEY_U')
  launcher.type('Epsilon')
  launcher.key('enter')
  added = launcher.wait_frame(
      lambda frame: not frame['prompt'] and row(frame, 'Epsilon'))
  results.check('launcher: Ctrl+N adds a shortcut running the typed text',
                added is not None
                and 'command: wl_probe epsilon' in fixture.layout_text(),
                fixture.layout_text())
  launcher.key('ctrl', 'KEY_M')
  launcher.wait_frame(lambda frame: frame['prompt'])
  launcher.key('ctrl', 'KEY_U')
  launcher.type('Probes')
  launcher.key('enter')
  moved = launcher.wait_frame(
      lambda frame: frame['category'] == 'Probes' and row(frame, 'Epsilon'))
  results.check('launcher: Ctrl+M moves it to a new category',
                moved is not None
                and '- name: Probes\n  entries:\n  - shortcut:epsilon'
                in fixture.layout_text(), fixture.layout_text())

  # 7. A drag from Accessories onto Probes moves Alpha Probe there.
  frame = launcher.frame()
  accessories = category(frame, 'Accessories')
  if accessories:
    launcher.move(centre(accessories['rect']))
    launcher.button('full')
  shown = launcher.wait_frame(
      lambda frame: frame['category'] == 'Accessories'
      and row(frame, 'Alpha Probe'))
  probes = category(shown, 'Probes')
  dragged = None
  if shown and probes:
    start = centre(row(shown, 'Alpha Probe')['rect'])
    end = centre(probes['rect'])
    launcher.move(start)
    launcher.button('press')
    for step in range(1, 9):
      launcher.move((start[0] + (end[0] - start[0]) * step // 8,
                     start[1] + (end[1] - start[1]) * step // 8))
      time.sleep(0.02)
    launcher.wait_frame(lambda frame: frame['drag'] == 'Alpha Probe')
    launcher.button('release')
    dragged = launcher.wait_frame(
        lambda frame: frame['category'] == 'Probes'
        and row(frame, 'Alpha Probe') and not frame['drag'])
  results.check('launcher: dragging an entry onto a category moves it there',
                dragged is not None and 'alpha.desktop'
                in fixture.layout_text().split('- name: Probes', 1)[-1],
                fixture.layout_text())

  # 8. The context menu favourites Delta Ray.
  launcher.key('ctrl', 'KEY_U')
  launcher.type('delta')
  frame = launcher.wait_frame(lambda frame: row(frame, 'Delta Ray'))
  menu = None
  if frame:
    launcher.move(centre(row(frame, 'Delta Ray')['rect']))
    launcher.button('full', 'BTN_RIGHT')
    menu = launcher.wait_frame(
        lambda frame: menu_item(frame, 'Add to Favourites'))
  if menu:
    launcher.move(centre(menu_item(menu, 'Add to Favourites')['rect']))
    launcher.button('full')
  results.check(
      'launcher: the right-click menu adds the entry to the favourites',
      menu is not None and launcher.wait_frame(
          lambda frame: not frame['menu']
          and (row(frame, 'Delta Ray') or {}).get('favourite')) is not None
      and '- gamma.desktop' in fixture.layout_text(),
      fixture.layout_text())
  launcher.key('esc')
  launcher.key('esc')
  status = launcher.exited()
  _finish(launcher, launched)
  results.check('launcher: Escape clears the query and then closes it',
                status == 0, f'status {status}')

  # 9. Persistence: a new launcher opens on the favourites, keeps the
  # category and launches the shortcut.
  launcher, frame = _start(shingle, binary, fixture, config_path,
                           results, 'restart')
  if frame is None:
    return
  names = [entry['name'] for entry in frame['rows']]
  probes = category(frame, 'Probes')
  results.check(
      'launcher: after a restart it opens on the favourites with the '
      'renamed entry and the new category',
      frame['category'] == 'Favourites'
      and names == ['Beta Probe', 'Delta Ray']
      and probes is not None and probes['count'] == 2,
      json.dumps(frame)[:400])
  launcher.type('eps')
  launcher.wait_frame(
      lambda frame: frame['rows'] and frame['rows'][0]['name'] == 'Epsilon')
  launcher.key('enter')
  status = launcher.exited()
  _finish(launcher, launched)
  mapped = xw.wait_for(lambda: shingle.view('epsilon'), timeout=15)
  results.check('launcher: the saved shortcut launches its command',
                status == 0 and bool(mapped),
                f'status {status}, views {shingle.views()}')

  # 10. A second invocation closes the first; 11. a click outside closes.
  first, frame = _start(shingle, binary, fixture, config_path,
                        results, 'toggle')
  if frame is None:
    return
  second = Launcher(shingle, binary, fixture, config_path)
  second_status = second.exited()
  first_status = first.exited()
  results.check(
      'launcher: running it again while it is open closes both',
      second_status == 0 and first_status == 0
      and 'closed the open launcher' in second.client.output(),
      f'first {first_status}, second {second_status}')
  _finish(first, launched)
  _finish(second, launched)
  launcher, frame = _start(shingle, binary, fixture, config_path,
                           results, 'click outside')
  if frame is None:
    return
  launcher.move(outside)
  launcher.button('full')
  status = launcher.exited()
  _finish(launcher, launched)
  results.check('launcher: a click outside the panel closes it',
                status == 0, f'status {status}')

  # 12. Startup time, warm: the index is cached.
  own, observed = [], []
  for _ in range(STARTS):
    checks_app().wait_for_load()
    launcher = Launcher(shingle, binary, fixture, config_path)
    seen = xw.wait_for(lambda: first_frame_ms(launcher.client.output()),
                       timeout=20, interval=0.002)
    if seen is not None:
      own.append(seen)
      observed.append((time.monotonic() - launcher.started) * 1000)
    launcher.key('esc')
    launcher.exited()
    _finish(launcher, launched)
  print(f'INFO launcher: first frame by the launcher\'s clock, '
        f'{STARTS} warm starts: {summary(own)}', flush=True)
  print(f'INFO launcher: first frame from exec as the harness sees it: '
        f'{summary(observed)}', flush=True)
  results.check(
      f'launcher: the first frame comes within {STARTUP_TARGET_MS:.0f} ms '
      '(median of warm starts)',
      own and statistics.median(own) < STARTUP_TARGET_MS, summary(own))

CHECKS = {
    'launcher': launcher_checks,
}
