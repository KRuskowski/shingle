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
the startup time; icons from a configured theme, hicolor and pixmaps in
captures and from the icon cache in a later first frame; a desktop
action found by search and by the Menu key and a right click; and the
panel at fractional scales 1.25, 1.5 and 2, which must be the output's
device pixels and match the launcher's own `--screenshot --scale`
drawing pixel for pixel. Then it checks type-to-launch again under the
host's `wayfire`, headless in the namespace (wayfire_reference.py),
since the launcher is meant to work there too.

Signals go through `run.signal_checked`, via checks.py's Shingle and Client
helpers, and the launched probes through `signal_checked` with their comm.
"""

import json
import math
import os
import signal
import statistics
import struct
import subprocess
import time
import zlib

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
# The icon theme the launcher is configured with; it inherits hicolor.
ICON_THEME = 'ProbeTheme'
# Each probe's icon name and the colour the fixture draws it: alpha's is
# an SVG in ProbeTheme that hides hicolor's red PNG, beta's an SVG in
# hicolor and gamma's a PNG in pixmaps.
ICONS = {'alpha': ('alpha-icon', 'yellow'), 'beta': ('beta-icon', 'green'),
         'gamma': ('gamma-icon', 'blue')}
# Alpha's desktop action: its ID, name, the probe it starts and its icon,
# a cyan PNG in hicolor.
ACTION = {'id': 'second', 'name': 'Second Window', 'app_id': 'alpha-second',
          'icon': 'action-icon', 'colour': 'cyan'}
RGBA = {'red': (255, 0, 0, 255), 'green': (0, 255, 0, 255),
        'blue': (0, 0, 255, 255), 'yellow': (255, 255, 0, 255),
        'cyan': (0, 255, 255, 255)}
# The icon's side at the default font size, in logical pixels.
ICON_SIZE = 28
# The fractional scales checked, on an output of this many device pixels.
SCALES = (1.25, 1.5, 2.0)
SCALED_OUTPUT = (1920, 1080)
HICOLOR_INDEX = """[Icon Theme]
Name=Hicolor
Directories=48x48/apps,scalable/apps

[48x48/apps]
Size=48
Type=Threshold

[scalable/apps]
Size=48
MinSize=8
MaxSize=512
Type=Scalable
"""
PROBE_THEME_INDEX = """[Icon Theme]
Name=Probe Theme
Inherits=hicolor
Directories=scalable/apps

[scalable/apps]
Size=48
MinSize=8
MaxSize=512
Type=Scalable
"""
KEYS = {'enter': 'KEY_ENTER', 'esc': 'KEY_ESC', 'tab': 'KEY_TAB',
        'up': 'KEY_UP', 'down': 'KEY_DOWN', 'f2': 'KEY_F2',
        'backspace': 'KEY_BACKSPACE', 'delete': 'KEY_DELETE',
        'ctrl': 'KEY_LEFTCTRL', 'shift': 'KEY_LEFTSHIFT', 'menu': 'KEY_COMPOSE'}

def desktop_entry(name, command, categories, icon=None, actions=()):
  """The text of a desktop entry file with `actions`, (id, name, command,
  icon) tuples."""
  text = ('[Desktop Entry]\nType=Application\n'
          f'Name={name}\nExec={command}\nCategories={categories}\n')
  if icon:
    text += f'Icon={icon}\n'
  if actions:
    text += 'Actions=' + ''.join(f'{action[0]};' for action in actions) + '\n'
  for action_id, action_name, action_command, action_icon in actions:
    text += (f'\n[Desktop Action {action_id}]\nName={action_name}\n'
             f'Exec={action_command}\n')
    if action_icon:
      text += f'Icon={action_icon}\n'
  return text

def png(width, height, rgba):
  """A PNG of `width` × `height` pixels of the colour `rgba`."""
  line = b'\x00' + bytes(rgba) * width

  def chunk(kind, data):
    return (struct.pack('>I', len(data)) + kind + data
            + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff))
  return (b'\x89PNG\r\n\x1a\n'
          + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 6, 0, 0,
                                       0))
          + chunk(b'IDAT', zlib.compress(line * height))
          + chunk(b'IEND', b''))

def svg(rgba):
  """An SVG square of the colour `rgba`."""
  red, green, blue, _ = rgba
  return ('<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48">'
          f'<rect width="48" height="48" fill="#{red:02x}{green:02x}'
          f'{blue:02x}"/></svg>')

def round_half_up(value):
  """`value` rounded as Rust's `f64::round` rounds positive values."""
  return int(math.floor(value + 0.5))

def device_rect(rect, scale):
  """A trace rectangle in logical pixels as the device pixels the
  launcher draws it at: (x, y, width, height) from its rounded edges."""
  left = round_half_up(rect['x'] * scale)
  top = round_half_up(rect['y'] * scale)
  return (left, top,
          round_half_up((rect['x'] + rect['width']) * scale) - left,
          round_half_up((rect['y'] + rect['height']) * scale) - top)

def mismatches(shot, image, rect, tolerance=2):
  """How many pixels of `rect` (x, y, width, height) of the capture
  `shot` differ from `image`, a capture of that rectangle alone, by more
  than `tolerance` in any channel."""
  x0, y0, width, height = rect
  if (image.width, image.height) != (width, height):
    return width * height
  count = 0
  for y in range(height):
    for x in range(width):
      shown = shot.pixel(x0 + x, y0 + y)
      drawn = image.pixel(x, y)
      if any(abs(a - b) > tolerance for a, b in zip(shown, drawn)):
        count += 1
  return count

def colour_box(shot, rect, colour):
  """The (width, height) of the box around the pixels of `colour` in
  `rect` (x, y, width, height) of `shot`, or None."""
  x0, y0, width, height = rect
  found = [(x, y) for y in range(y0, y0 + height) for x in range(x0, x0 + width)
           if checks.classify(*shot.pixel(x, y)) == colour]
  if not found:
    return None
  xs = [x for x, _ in found]
  ys = [y for _, y in found]
  return (max(xs) - min(xs) + 1, max(ys) - min(ys) + 1)

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

def write_file(path, data):
  """Writes `data`, text or bytes, to `path`, creating its directory."""
  os.makedirs(os.path.dirname(path), exist_ok=True)
  with open(path, 'wb' if isinstance(data, bytes) else 'w') as file:
    file.write(data)

class Fixture:
  """The private data, state and cache directories, three desktop files
  with icons and one action, their icon theme, and a PATH directory
  holding `wl_probe`."""

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
      actions = ()
      if app_id == 'alpha':
        actions = ((ACTION['id'], ACTION['name'],
                    f'{wl_probe} {ACTION["app_id"]}', ACTION['icon']),)
      with open(os.path.join(applications, f'{app_id}.desktop'), 'w') as file:
        file.write(desktop_entry(name, f'{wl_probe} {app_id}', categories,
                                 ICONS[app_id][0], actions))
    icons = os.path.join(self.system, 'icons')
    write_file(os.path.join(icons, 'hicolor', 'index.theme'), HICOLOR_INDEX)
    write_file(os.path.join(icons, ICON_THEME, 'index.theme'),
               PROBE_THEME_INDEX)
    write_file(os.path.join(icons, 'hicolor', '48x48', 'apps',
                            'alpha-icon.png'), png(48, 48, RGBA['red']))
    write_file(os.path.join(icons, ICON_THEME, 'scalable', 'apps',
                            'alpha-icon.svg'), svg(RGBA['yellow']))
    write_file(os.path.join(icons, 'hicolor', 'scalable', 'apps',
                            'beta-icon.svg'), svg(RGBA['green']))
    write_file(os.path.join(self.system, 'pixmaps', 'gamma-icon.png'),
               png(64, 64, RGBA['blue']))
    write_file(os.path.join(icons, 'hicolor', '48x48', 'apps',
                            f'{ACTION["icon"]}.png'),
               png(48, 48, RGBA[ACTION['colour']]))
    self.layout_path = os.path.join(self.data_home, 'shingle',
                                    'launcher.yaml')
    self.icon_cache = os.path.join(self.directory, 'cache', 'shingle',
                                   'launcher-icons')

  def icon_cache_file(self, pixels):
    """The icon cache of the fixture's theme at `pixels` device pixels."""
    return os.path.join(self.icon_cache,
                        f'{ICON_THEME}-{ICON_SIZE}-{pixels}.bin')

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
  """BASE_CONFIG with the distinctive launcher colours and the fixture's
  icon theme."""
  config = dict(checks.BASE_CONFIG)
  config['launcher'] = {'colors': {'panel': PANEL, 'backdrop': BACKDROP},
                        'icon_theme': ICON_THEME}
  return config

def icon_colours(shot, frame, names):
  """The colour at the centre of the icon of each row in `names`, by
  name; None for a row without a drawn icon."""
  colours = {}
  for name in names:
    icon = (row(frame, name) or {}).get('icon')
    colours[name] = (checks.classify(*shot.pixel(*centre(icon['rect'])))
                     if icon else None)
  return colours

def mapped_count(shingle, app_id):
  """How many mapped views have `app_id`."""
  return len([view for view in shingle.views()
              if view.get('app-id') == app_id and view.get('mapped', True)])

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
  for scale in SCALES:
    _scaled(results, binary, launcher_binary, fixture, captures, scale)
  _under_wayfire(results, launcher_binary, fixture, config_path)

def _screenshot(shingle, binary, fixture, config_path, scale):
  """The launcher's own drawing of its panel at `scale`, or None."""
  path = os.path.join(shingle.directory, f'screenshot-{scale}.ppm')
  done = subprocess.run(
      [binary, '--config', config_path, '--screenshot', path,
       '--scale', str(scale)],
      env=shingle.client_environment(fixture.environment()),
      capture_output=True, timeout=60, stdin=subprocess.DEVNULL)
  if done.returncode != 0:
    print(f'INFO launcher: --screenshot failed: {done.stderr[-300:]!r}',
          flush=True)
    return None
  with open(path, 'rb') as file:
    parsed = xw.parse_ppm(file.read())
  return xw.Capture(*parsed) if parsed else None

def _scaled(results, binary, launcher_binary, fixture, captures, scale):
  """The panel on an output at the fractional `scale`: a buffer of the
  output's device pixels, the panel as the launcher draws it at that
  scale pixel for pixel, and icons at their device size."""
  name = f'launcher: at scale {scale}'
  config = launcher_config()
  config['output'] = {'width': SCALED_OUTPUT[0], 'height': SCALED_OUTPUT[1],
                      'scale': scale}
  shingle = checks.Shingle(binary, f'launcher-scale-{scale}', config)
  config_path = os.path.join(shingle.directory, 'shingle.yaml')
  launched = []
  try:
    # Keep the pointer off the panel, so that no row is hovered.
    shingle.call('stipc/move_cursor', {'x': 0, 'y': 0})
    launcher = Launcher(shingle, launcher_binary, fixture, config_path)

    def ready(frame):
      applications = [entry for entry in frame['rows']
                      if entry['id'] and entry['id'].endswith('.desktop')]
      return (frame.get('scale') == scale and frame.get('fractional')
              and applications
              and all(entry.get('icon') for entry in applications))
    frame = launcher.wait_frame(ready, timeout=20)
    image = _screenshot(shingle, launcher_binary, fixture, config_path, scale)
    if frame is None or image is None:
      results.check(f'{name} the launcher draws a fractional frame and a '
                    'screenshot', False,
                    json.dumps(launcher.frame())[:400])
      return
    buffer = frame['buffer']
    rect = device_rect(frame['panel'], scale)
    counted = [None]

    def matches(shot):
      counted[0] = mismatches(shot, image, rect)
      return counted[0] == 0
    shot = captures.settled(shingle, matches, stable=True)
    results.check(
        f'{name} the buffer is the output\'s {SCALED_OUTPUT[0]}x'
        f'{SCALED_OUTPUT[1]} device pixels and the capture shows the '
        'panel exactly as the launcher draws it at that scale',
        (buffer['width'], buffer['height']) == SCALED_OUTPUT
        and shot is not None and counted[0] == 0,
        f'buffer {buffer}, panel {rect}, {counted[0]} pixels differ')
    pixels = round_half_up(ICON_SIZE * scale)
    sizes = {}
    for entry in frame['rows']:
      icon = entry.get('icon')
      app_id = (entry['id'] or '').split('.')[0]
      colour = ICONS.get(app_id, (None, None))[1]
      if icon and colour and shot is not None:
        sizes[entry['name']] = colour_box(
            shot, device_rect(icon['rect'], scale), colour)
    results.check(
        f'{name} icons fill {pixels}x{pixels} device pixels',
        sizes and all(size == (pixels, pixels) for size in sizes.values()),
        json.dumps(sizes))
    launcher.key('esc')
    launcher.exited()
    _finish(launcher, launched)
  finally:
    end_launched(launched)
    shingle.stop()

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

  # 1b. Icons: alpha's from the configured theme over hicolor's, beta's
  # from hicolor, gamma's from pixmaps; then the cache is written.
  names = {f'{name}': ICONS[app_id][1] for app_id, name, _ in PROBES}
  drawn = sum(1 for entry in frame['rows'] if entry.get('icon'))
  print(f'INFO launcher: the first frame without an icon cache draws '
        f'{drawn} icons', flush=True)
  iconed = launcher.wait_frame(
      lambda frame: all((row(frame, name) or {}).get('icon')
                        for name in names), timeout=15)
  shown = [None]

  def shows_icons(shot):
    shown[0] = icon_colours(shot, iconed, names)
    return shown[0] == names
  shot = captures.settled(shingle, shows_icons) if iconed else None
  results.check(
      'launcher: icons from the configured theme, hicolor and pixmaps are '
      'drawn as they load', iconed is not None and shown[0] == names,
      f'{shown[0]}, {json.dumps(launcher.frame())[:300]}')
  cache_file = fixture.icon_cache_file(ICON_SIZE)
  results.check('launcher: the decoded icons are cached',
                xw.wait_for(lambda: os.path.exists(cache_file), timeout=10),
                cache_file)

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

  # 2b. The desktop action: found by its name, with its own icon, and
  # launched.
  launcher, frame = _start(shingle, binary, fixture, config_path,
                           results, 'actions')
  if frame is None:
    return
  first_icons = [entry['name'] for entry in frame['rows'] if entry.get('icon')]
  results.check('launcher: a later start draws the cached icons in its '
                'first frame', len(first_icons) >= 3,
                json.dumps(frame['rows'])[:300])
  launcher.type('second')
  found = launcher.wait_frame(
      lambda frame: frame['rows'] and frame['rows'][0].get('action')
      == ACTION['id'] and frame['rows'][0].get('icon'))
  colour = [None]

  def shows_action_icon(shot):
    colour[0] = checks.classify(
        *shot.pixel(*centre(found['rows'][0]['icon']['rect'])))
    return colour[0] == ACTION['colour']
  shot = captures.settled(shingle, shows_action_icon) if found else None
  results.check(
      'launcher: typing an action\'s name lists it under its entry with its '
      'own icon', found is not None
      and found['rows'][0]['parent'] == 'Alpha Probe'
      and colour[0] == ACTION['colour'],
      f'{colour[0]}, {json.dumps(launcher.frame())[:300]}')
  launcher.key('enter')
  status = launcher.exited()
  _finish(launcher, launched)
  mapped = xw.wait_for(lambda: mapped_count(shingle, ACTION['app_id']) == 1,
                       timeout=15)
  results.check('launcher: Enter on the action runs its Exec',
                status == 0 and bool(mapped),
                f'status {status}, views {shingle.views()}')
  launcher, frame = _start(shingle, binary, fixture, config_path,
                           results, 'action menu')
  if frame is None:
    return
  launcher.type('alpha')
  launcher.wait_frame(lambda frame: frame['rows']
                      and frame['rows'][0]['name'] == 'Alpha Probe')
  launcher.key('menu')
  by_key = launcher.wait_frame(
      lambda frame: (frame['menu'] or [{}])[0].get('label') == ACTION['name'])
  launcher.key('esc')
  launcher.wait_frame(lambda frame: not frame['menu'])
  launcher.key('shift', 'KEY_F10')
  by_f10 = launcher.wait_frame(
      lambda frame: (frame['menu'] or [{}])[0].get('label') == ACTION['name'])
  launcher.key('esc')
  frame = launcher.wait_frame(lambda frame: not frame['menu'])
  by_click = None
  if frame and row(frame, 'Alpha Probe'):
    launcher.move(centre(row(frame, 'Alpha Probe')['rect']))
    launcher.button('full', 'BTN_RIGHT')
    by_click = launcher.wait_frame(
        lambda frame: menu_item(frame, ACTION['name']))
  if by_click:
    launcher.move(centre(menu_item(by_click, ACTION['name'])['rect']))
    launcher.button('full')
  status = launcher.exited()
  _finish(launcher, launched)
  mapped = xw.wait_for(lambda: mapped_count(shingle, ACTION['app_id']) == 2,
                       timeout=15)
  results.check(
      'launcher: the Menu key, Shift+F10 and a right click open the '
      'entry\'s menu with its action first, and choosing it launches it',
      by_key is not None and by_f10 is not None and by_click is not None
      and status == 0 and bool(mapped),
      f'key {by_key is not None}, F10 {by_f10 is not None}, click '
      f'{by_click is not None}, status {status}, views {shingle.views()}')

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
