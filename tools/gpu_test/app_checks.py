"""The application compatibility sweep, inside run.py's namespace.

Each installed application runs, one at a time, against its own offscreen
Shingle with two outputs and two workspaces, a private HOME with fresh
profile and config directories, a private D-Bus session bus (with the
host's system bus covered) and no network, and is driven through a
scripted scenario over IPC and STIPC input:

1. it maps, is tiled and draws (not blank, nothing outside its view);
2. it has keyboard focus and takes typed text, checked by its effect;
3. a popup (right-click menu) opens and closes;
4. a dialog (a file chooser or settings window) opens as a child of the
   main window and closes;
5. text copied in it is read by `wl-paste`, and text from `wl-copy` is
   pasted into it;
6. the tiler resizes it beside a `wl_probe`, and a Super+right drag of
   the boundary resizes it again; each time it must redraw at its new
   size, and the probe must stay visible;
7. it goes fullscreen and back;
8. it moves to the second workspace and back, and to the second output
   and back, drawn each time;
9. it closes.

Captures at each step are judged for blank content (one colour), stale
content (Shingle's distinctive background inside the view after a
resize) and misplaced content (anything but the background in the outer
gaps). The client runs with `WAYLAND_DEBUG=1`; its log and Shingle's are
searched for protocol errors and crashes. rofi (an X11 client through
Xwayland) must appear on top of a tiled window with keyboard focus and
launch a command, and dunst must show a `notify-send` notification in
its layer surface and take it away again.

Groups: `apps` runs every application, and `app-NAME` one of them. Each
step is one PASS or FAIL line, or an INFO line for what does not apply
to an application; the end prints the per-app table that
docs/status.md records. Before each application the 1-minute load must
be under LOAD_LIMIT, and the sweep lowers its own priority to niceness
19.

Every process here is a child of this one inside the PID namespace.
Applications fork helpers (D-Bus activated services, browser content
processes, LibreOffice's oosplash), so after each application every
process that appeared since it started is ended through
`run.signal_checked`, with its own `/proc/PID/comm` checked; no PID is
ever taken from a reply, and none at or below 1 is ever signalled.
"""

import json
import os
import re
import signal
import struct
import subprocess
import time
import zlib

import checks
import run
import screencast_checks

xw = checks.xw
LOAD_LIMIT = 12.0
# Shingle's background in every sweep: (102, 0, 102), a colour no
# application draws, so that the background showing inside a view means
# the client did not cover it.
BACKGROUND_COLOR = [0.4, 0.0, 0.4, 1.0]
BACKGROUND = (102, 0, 102)
GAPS = {'top': 24, 'right': 24, 'bottom': 24, 'left': 24, 'inner': 0}
OUTPUTS = (
    {'name': 'HEADLESS-1', 'mode': '1280x720@60'},
    {'name': 'HEADLESS-2', 'mode': '1024x768@60'},
)
# How long an application gets to map its first window.
MAP_TIMEOUT = 90
# The probe tiled beside the application for the resize steps.
PROBE_APP_ID = 'sweep-probe'
TOKEN_TYPED = 'shingle typed 42'
# Seconds a browser gets after mapping before the location bar is used.
BROWSER_SETTLE = 8
TOKEN_PASTED = 'shingle pasted 7'
STEPS = ('maps', 'tiles', 'draws', 'focus', 'typing', 'popup', 'dialog',
         'copy', 'paste', 'resize-tiler', 'resize-drag', 'fullscreen',
         'workspace', 'output', 'close', 'protocol', 'crash')
# US layout: characters typed with Shift, and the key of each.
SHIFTED = {'_': 'KEY_MINUS', '+': 'KEY_EQUAL', ':': 'KEY_SEMICOLON',
           '"': 'KEY_APOSTROPHE', '<': 'KEY_COMMA', '>': 'KEY_DOT',
           '?': 'KEY_SLASH', '(': 'KEY_9', ')': 'KEY_0', '#': 'KEY_3',
           '!': 'KEY_1', '@': 'KEY_2', '%': 'KEY_5', '&': 'KEY_7',
           '*': 'KEY_8', '{': 'KEY_LEFTBRACE', '}': 'KEY_RIGHTBRACE',
           '|': 'KEY_BACKSLASH', '~': 'KEY_GRAVE'}
PLAIN = {' ': 'KEY_SPACE', '\n': 'KEY_ENTER', '-': 'KEY_MINUS',
         '=': 'KEY_EQUAL', ';': 'KEY_SEMICOLON', "'": 'KEY_APOSTROPHE',
         ',': 'KEY_COMMA', '.': 'KEY_DOT', '/': 'KEY_SLASH',
         '[': 'KEY_LEFTBRACE', ']': 'KEY_RIGHTBRACE', '\\': 'KEY_BACKSLASH',
         '`': 'KEY_GRAVE', '\t': 'KEY_TAB'}
MODIFIERS = {'ctrl': 'KEY_LEFTCTRL', 'shift': 'KEY_LEFTSHIFT',
             'alt': 'KEY_LEFTALT', 'super': 'KEY_LEFTMETA'}
NAMED_KEYS = {'esc': 'KEY_ESC', 'enter': 'KEY_ENTER', 'tab': 'KEY_TAB',
              'comma': 'KEY_COMMA', 'space': 'KEY_SPACE',
              'backspace': 'KEY_BACKSPACE'}
# libwayland's WAYLAND_DEBUG lines for a protocol error the client got.
PROTOCOL_ERROR = re.compile(r'wl_display[#@]1\.error\((.*)\)')
CRASH_WORDS = ('Segmentation fault', 'core dumped', 'SIGSEGV', 'SIGABRT',
               'Aborted (core', 'panicked at')

def key_sequence(text):
  """The (evdev key, shifted) presses that type `text` on a US layout."""
  presses = []
  for character in text:
    if character in SHIFTED:
      presses.append((SHIFTED[character], True))
    elif character in PLAIN:
      presses.append((PLAIN[character], False))
    elif character.isascii() and character.isalpha():
      presses.append((f'KEY_{character.upper()}', character.isupper()))
    elif character.isascii() and character.isdigit():
      presses.append((f'KEY_{character}', False))
    else:
      raise ValueError(f'cannot type {character!r}')
  return presses

def chord_keys(chord):
  """The evdev keys of a chord such as `ctrl+shift+v`, modifiers first."""
  keys = []
  for part in chord.lower().split('+'):
    if part in MODIFIERS:
      keys.append(MODIFIERS[part])
    elif part in NAMED_KEYS:
      keys.append(NAMED_KEYS[part])
    elif len(part) == 1:
      keys.append(key_sequence(part)[0][0])
    elif part.startswith('f') and part[1:].isdigit():
      keys.append(f'KEY_{part.upper()}')
    else:
      raise ValueError(f'unknown key {part!r} in {chord!r}')
  return keys

def debug_requests(text, interface, message):
  """How many `interface.message` requests a WAYLAND_DEBUG log has."""
  pattern = re.compile(r'->\s*' + re.escape(interface) + r'[#@]\d+\.'
                       + re.escape(message) + r'\(')
  return sum(1 for line in text.splitlines() if pattern.search(line))

def debug_events(text, interface, message):
  """How many `interface.message` events a WAYLAND_DEBUG log has."""
  pattern = re.compile(r'\]\s*(?:\{[^}]*\}\s*)?' + re.escape(interface)
                       + r'[#@]\d+\.' + re.escape(message) + r'\(')
  return sum(1 for line in text.splitlines()
             if '->' not in line and pattern.search(line))

def protocol_errors(text):
  """The arguments of every `wl_display.error` in a WAYLAND_DEBUG log, and
  libwayland's own protocol error messages."""
  found = [match.group(1) for match in PROTOCOL_ERROR.finditer(text)]
  found += [line.strip() for line in text.splitlines()
            if 'protocol error' in line.lower()
            and 'wl_display' not in line][:5]
  return found

def crash_lines(text):
  """The lines of a log that report a crash."""
  return [line.strip() for line in text.splitlines()
          if any(word in line for word in CRASH_WORDS)][:5]

def sample_points(rect, step=16, inset=0):
  """Grid points inside `rect` ({'x', 'y', 'width', 'height'})."""
  points = []
  for y in range(rect['y'] + inset, rect['y'] + rect['height'] - inset, step):
    for x in range(rect['x'] + inset, rect['x'] + rect['width'] - inset,
                   step):
      points.append((x, y))
  return points

def edge_points(rect, inset=6, step=12):
  """Points `inset` pixels inside each edge of `rect`, by edge."""
  left, top = rect['x'] + inset, rect['y'] + inset
  right = rect['x'] + rect['width'] - 1 - inset
  bottom = rect['y'] + rect['height'] - 1 - inset
  return {
      'top': [(x, top) for x in range(left, right + 1, step)],
      'bottom': [(x, bottom) for x in range(left, right + 1, step)],
      'left': [(left, y) for y in range(top, bottom + 1, step)],
      'right': [(right, y) for y in range(top, bottom + 1, step)],
  }

def is_background(colour, tolerance=4):
  """Whether `colour` is Shingle's sweep background."""
  return all(abs(a - b) <= tolerance for a, b in zip(colour, BACKGROUND))

def inside(shot, point):
  """Whether `point` lies in `shot`."""
  return 0 <= point[0] < shot.width and 0 <= point[1] < shot.height

def colours_in(shot, rect, step=16):
  """{colour: count} of a grid of samples of `rect`."""
  counts = {}
  for point in sample_points(rect, step):
    if inside(shot, point):
      colour = shot.pixel(*point)
      counts[colour] = counts.get(colour, 0) + 1
  return counts

def blank(shot, rect):
  """Whether `rect` of `shot` is one colour (or missing): nothing drawn.
  A fine grid, so that a terminal's prompt counts."""
  return len(colours_in(shot, rect, step=3)) <= 1

def background_share(shot, points):
  """The share of `points` of `shot` that show the background."""
  points = [point for point in points if inside(shot, point)]
  if not points:
    return 1.0
  return sum(is_background(shot.pixel(*point)) for point in points) / len(
      points)

def stale_edges(shot, rect, limit=0.5):
  """The edges of `rect` along which more than `limit` of the samples
  show the background: the client's buffer does not reach them."""
  return sorted(edge for edge, points in edge_points(rect).items()
                if background_share(shot, points) > limit)

def within(point, rect):
  """Whether `point` lies in `rect` ({'x', 'y', 'width', 'height'})."""
  return (rect['x'] <= point[0] < rect['x'] + rect['width']
          and rect['y'] <= point[1] < rect['y'] + rect['height'])

def is_shadow(colour, tolerance=6):
  """Whether `colour` is the background darkened by a translucent
  client-side shadow, which GTK draws around its windows."""
  red, green, blue = colour
  return (abs(red - blue) <= tolerance and green <= BACKGROUND[1] + tolerance
          and red <= BACKGROUND[0] + tolerance)

def misplaced(shot, outputs, views=(), gaps=GAPS):
  """Points in the outer gaps of `outputs` [(x, y, w, h)], outside every
  rectangle of `views`, that do not show the background: content drawn
  where no view is."""
  wrong = []
  for x, y, width, height in outputs:
    middle_x, middle_y = x + width // 2, y + height // 2
    for point in ((x + gaps['left'] // 2, middle_y),
                  (x + width - 1 - gaps['right'] // 2, middle_y),
                  (middle_x, y + gaps['top'] // 2),
                  (middle_x, y + height - 1 - gaps['bottom'] // 2)):
      if any(within(point, view) for view in views):
        continue
      if inside(shot, point) and not is_shadow(shot.pixel(*point)):
        wrong.append(point)
  return wrong

def changed_share(first, second, rect, step=8):
  """The share of samples of `rect` that differ between two captures."""
  if first is None or second is None:
    return 0.0
  points = [point for point in sample_points(rect, step)
            if inside(first, point) and inside(second, point)]
  if not points:
    return 0.0
  return sum(first.pixel(*point) != second.pixel(*point)
             for point in points) / len(points)

def centred_on(child, parent, area, slack=2):
  """Whether `child` is centred on `parent`, or pushed inside `area` (an
  output) where centring would leave it, as Wayfire places dialogs."""
  def centre(rect):
    return (rect['x'] + rect['width'] / 2, rect['y'] + rect['height'] / 2)
  (cx, cy), (px, py) = centre(child), centre(parent)
  at_edge_x = (child['x'] <= area['x'] + slack or child['x'] + child['width']
               >= area['x'] + area['width'] - slack)
  at_edge_y = (child['y'] <= area['y'] + slack or child['y'] + child['height']
               >= area['y'] + area['height'] - slack)
  return ((abs(cx - px) <= slack or at_edge_x)
          and (abs(cy - py) <= slack or at_edge_y))

def load_average(text):
  """The 1-minute load from the text of /proc/loadavg."""
  return float(text.split()[0])

def wait_for_load(limit=LOAD_LIMIT, sleep=time.sleep, read=None):
  """Waits until the 1-minute load is under `limit`; returns the load."""
  read = read or (lambda: open('/proc/loadavg').read())
  while True:
    load = load_average(read())
    if load < limit:
      return load
    print(f'INFO apps: load {load:.1f} >= {limit}, waiting', flush=True)
    sleep(15)

def minimal_pdf():
  """A one-page PDF with a filled blue rectangle and some text."""
  stream = (b'0 0 1 rg 72 400 300 200 re f BT /F1 24 Tf 72 300 Td '
            b'(Shingle sweep) Tj ET')
  objects = [
      b'<< /Type /Catalog /Pages 2 0 R >>',
      b'<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
      b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] '
      b'/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
      b'<< /Length %d >>\nstream\n' % len(stream) + stream
      + b'\nendstream',
      b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ]
  out = bytearray(b'%PDF-1.4\n')
  offsets = []
  for number, body in enumerate(objects, 1):
    offsets.append(len(out))
    out += b'%d 0 obj\n' % number + body + b'\nendobj\n'
  xref = len(out)
  out += b'xref\n0 %d\n0000000000 65535 f \n' % (len(objects) + 1)
  for offset in offsets:
    out += b'%010d 00000 n \n' % offset
  out += (b'trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n'
          % (len(objects) + 1, xref))
  return bytes(out)

def minimal_png(width=64, height=48, colour=(0, 160, 0)):
  """An RGB PNG of one colour."""
  def chunk(kind, data):
    body = kind + data
    return (struct.pack('>I', len(data)) + body
            + struct.pack('>I', zlib.crc32(body) & 0xffffffff))
  row = b'\x00' + bytes(colour) * width
  return (b'\x89PNG\r\n\x1a\n'
          + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0,
                                       0))
          + chunk(b'IDAT', zlib.compress(row * height))
          + chunk(b'IEND', b''))

SVG = ('<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300">'
       '<rect x="20" y="20" width="200" height="120" fill="#00a000"/>'
       '</svg>\n')

def unprivileged_uid(environment=None):
  """The caller's uid on the host, as run.py passes it, never 0."""
  environment = os.environ if environment is None else environment
  uid = environment.get('SHINGLE_GPU_TEST_UID', '1000')
  return uid if uid.isdigit() and uid != '0' else '1000'

def as_user(arguments, uid=None):
  """`arguments` run in a nested user namespace that maps the namespace's
  root back to the caller's uid, as wayfire_reference.py runs Wayfire:
  applications run as the user they run as on the desktop, and some, such
  as VLC, refuse to run as root. `unshare` execs the command, so the PID
  is the application's."""
  uid = uid or unprivileged_uid()
  return ['unshare', '--user', f'--map-user={uid}', f'--map-group={uid}',
          '--', *arguments]

def main_view(views):
  """The largest of `views` without a parent, else the largest."""
  def area(view):
    return view['geometry']['width'] * view['geometry']['height']
  parentless = [view for view in views if view.get('parent', -1) == -1]
  return max(parentless or views, key=area)

def shifted(geometry, dx=0, dy=0):
  """`geometry` moved by `dx` and `dy`."""
  return dict(geometry, x=geometry['x'] + dx, y=geometry['y'] + dy)

def workspace_column(geometry, area):
  """How many workspaces right of the current one IPC `geometry` is, by
  its centre: Wayfire reports a view on another workspace one output
  width (`area`'s) per workspace from the current one."""
  centre = geometry['x'] + geometry['width'] / 2
  return int((centre - area['x']) // area['width'])

def new_pids(before, current, keep):
  """PIDs in `current` but not `before`, without `keep`, 1 or below."""
  return sorted(pid for pid in set(current) - set(before) - set(keep)
                if isinstance(pid, int) and pid > 1)

def namespace_pids():
  """Every PID in this PID namespace."""
  return [int(entry) for entry in os.listdir('/proc') if entry.isdigit()]

def descendants(pid, parents=None):
  """`pid` and every process below it, from {pid: ppid}."""
  if parents is None:
    parents = {}
    for other in namespace_pids():
      try:
        with open(f'/proc/{other}/stat') as file:
          parents[other] = int(file.read().rsplit(')', 1)[1].split()[1])
      except (OSError, IndexError, ValueError):
        continue
  found = {pid}
  grew = True
  while grew:
    grew = False
    for child, parent in parents.items():
      if parent in found and child not in found:
        found.add(child)
        grew = True
  return found

def end_processes(pids, timeout=5.0):
  """SIGTERM, then SIGKILL, every live process of `pids`, each through
  `signal_checked` with its own comm; returns those that would not go."""
  for sig, wait in ((signal.SIGTERM, timeout), (signal.SIGKILL, 2.0)):
    live = [pid for pid in pids if run.command_name(pid) is not None
            and not _zombie(pid)]
    for pid in live:
      name = run.command_name(pid)
      if name:
        run.signal_checked(pid, sig, name)
    xw.wait_for(lambda: all(run.command_name(pid) is None or _zombie(pid)
                            for pid in pids), timeout=wait)
  _reap()
  return [pid for pid in pids
          if run.command_name(pid) is not None and not _zombie(pid)]

def _zombie(pid):
  """Whether `pid` is a zombie."""
  try:
    with open(f'/proc/{pid}/stat') as file:
      return file.read().rsplit(')', 1)[1].split()[0] == 'Z'
  except (OSError, IndexError):
    return False

def _reap():
  """Reaps orphans reparented to this process, PID 1 of the namespace.
  Called only once no `Popen` of ours is waited for any more."""
  while True:
    try:
      pid, _ = os.waitpid(-1, os.WNOHANG)
    except ChildProcessError:
      return
    if pid == 0:
      return

def lower_priority():
  """Raises this process's niceness to 19, which its children inherit."""
  current = os.nice(0)
  if current < 19:
    os.nice(19 - current)

def table(matrix, steps=STEPS):
  """The per-app results as a Markdown table: one row per app, one
  column per step, `ok`, `FAIL`, `n/a` or empty."""
  lines = ['| App | ' + ' | '.join(steps) + ' |',
           '|' + '---|' * (len(steps) + 1)]
  for app, results in matrix.items():
    cells = [results.get(step, '') for step in steps]
    lines.append(f'| {app} | ' + ' | '.join(cells) + ' |')
  return '\n'.join(lines)

class App:
  """What the sweep runs and how it drives one application."""

  def __init__(self, name, command, text=None, popup='right-click',
               dialog=None, fullscreen_key=None, environment=None,
               heavy=False, prepare=None, kind='wayland', map_timeout=None,
               copy_paste=True, dismiss=None, orphan_dialog=None):
    """`command` may contain `{dir}` (the app's files) and `{home}`.

    `text` is how typed text is checked: 'shell' (typed commands write a
    file), 'editor' (select all and copy, read by wl-paste), 'browser' (a
    data: URL typed into the location bar paints the page) or None.
    `popup` is 'right-click' or None, `dialog` the chord that opens one,
    `prepare(directory)` writes the files the command opens, and a first
    window titled with the prefix `dismiss`, such as a welcome dialog, is
    closed before the scenario. `orphan_dialog`, when set, says why the
    app's dialog has no parent, so that it is tiled as Wayfire tiles it:
    the dialog step then checks only that it is drawn, focused and
    closed."""
    self.name = name
    self.command = command
    self.text = text
    self.popup = popup
    self.dialog = dialog
    self.fullscreen_key = fullscreen_key
    self.environment = environment or {}
    self.heavy = heavy
    self.prepare = prepare
    self.kind = kind
    self.map_timeout = map_timeout or MAP_TIMEOUT
    self.copy_paste = copy_paste
    self.dismiss = dismiss
    self.orphan_dialog = orphan_dialog

def _write(path, data):
  mode = 'wb' if isinstance(data, bytes) else 'w'
  with open(path, mode) as file:
    file.write(data)

def _prepare_pdf(directory):
  _write(os.path.join(directory, 'sweep.pdf'), minimal_pdf())

def _prepare_gimp(directory):
  _write(os.path.join(directory, 'sweep.png'), minimal_png())
  # GIMP 3 shows a welcome dialog on its first start.
  config = os.path.join(directory, 'config', 'GIMP', '3.0')
  os.makedirs(config, exist_ok=True)
  _write(os.path.join(config, 'gimprc'), '(show-welcome-dialog no)\n')

def _prepare_svg(directory):
  _write(os.path.join(directory, 'sweep.svg'), SVG)

def _prepare_files(directory):
  for name in ('alpha.txt', 'beta.txt'):
    _write(os.path.join(directory, 'files', name), 'sweep\n')

def _prepare_firefox(directory):
  _write(os.path.join(directory, 'profile', 'user.js'),
         ''.join(f'user_pref("{name}", {value});\n' for name, value in
                 zip(FIREFOX_PREFERENCES[::2], FIREFOX_PREFERENCES[1::2])))

FIREFOX_PREFERENCES = (
    'browser.shell.checkDefaultBrowser', 'false',
    'browser.aboutwelcome.enabled', 'false',
    'browser.startup.homepage_override.mstone', '"ignore"',
    'browser.startup.page', '0',
    'datareporting.policy.dataSubmissionEnabled', 'false',
    'toolkit.telemetry.reportingpolicy.firstRun', 'false',
    'app.update.enabled', 'false',
    'browser.tabs.warnOnClose', 'false',
    'browser.warnOnQuit', 'false',
    'browser.sessionstore.resume_from_crash', 'false',
    'widget.use-xdg-desktop-portal.file-picker', '0',
)
CHROMIUM_FLAGS = ('--user-data-dir={dir}/profile', '--no-first-run',
                  '--no-default-browser-check', '--no-sandbox',
                  '--password-store=basic', '--disable-sync',
                  '--ozone-platform-hint=auto', '--disable-component-update',
                  'about:blank')

APPS = (
    App('kitty', ['kitty', '--config', 'NONE', '-o',
                  'confirm_os_window_close=0'], text='shell', popup=None,
        environment={'SHELL': '/bin/sh'}),
    App('konsole', ['konsole', '--separate', '--workdir', '{dir}'],
        text='shell', dialog='ctrl+shift+comma',
        environment={'SHELL': '/bin/sh'}),
    App('kate', ['kate', '--block', '--new', '{dir}/note.txt'], text='editor',
        dialog='ctrl+o'),
    App('dolphin', ['dolphin', '--new-window', '{dir}/files'],
        dialog='ctrl+shift+comma', prepare=_prepare_files,
        copy_paste=False),
    App('okular', ['okular', '{dir}/sweep.pdf'], dialog='ctrl+o',
        prepare=_prepare_pdf, copy_paste=False),
    App('evince', ['evince', '{dir}/sweep.pdf'], dialog='ctrl+o',
        prepare=_prepare_pdf, copy_paste=False),
    App('mpv', ['mpv', '--no-config', '--force-window=yes', '--idle=yes',
                '--loop=inf', 'av://lavfi:testsrc=size=640x480:rate=15'],
        popup=None, fullscreen_key='f', copy_paste=False),
    App('vlc', ['vlc', '--no-qt-privacy-ask', '--no-qt-recentplay',
                '--no-qt-error-dialogs'], dialog='ctrl+o', copy_paste=False,
        orphan_dialog='WM_TRANSIENT_FOR names Qt\'s hidden leader window'),
    App('gimp', ['gimp', '--no-splash', '--no-data', '--no-fonts',
                 '{dir}/sweep.png'], dialog='ctrl+n', prepare=_prepare_gimp,
        copy_paste=False, heavy=True, map_timeout=180,
        dismiss='Welcome to GIMP'),
    App('inkscape', ['inkscape', '--with-gui', '{dir}/sweep.svg'],
        dialog='ctrl+o', prepare=_prepare_svg, copy_paste=False, heavy=True,
        map_timeout=180),
    App('libreoffice', ['libreoffice', '--writer', '--norestore',
                        '--nologo', '--nofirststartwizard',
                        '-env:UserInstallation=file://{dir}/lo'],
        text='editor', dialog='ctrl+o', heavy=True, map_timeout=180),
    App('firefox', ['firefox', '--no-remote', '--new-instance', '--profile',
                    '{dir}/profile', 'about:blank'],
        text='browser', dialog='ctrl+o', heavy=True, prepare=_prepare_firefox),
    App('chromium', ['chromium', *CHROMIUM_FLAGS], text='browser',
        dialog='ctrl+o', heavy=True),
    App('google-chrome', ['google-chrome', *CHROMIUM_FLAGS], text='browser',
        dialog='ctrl+o', heavy=True),
)
SPECIAL = ('rofi', 'dunst')
GROUPS = ('apps',) + tuple(f'app-{app.name}' for app in APPS) + tuple(
    f'app-{name}' for name in SPECIAL)

def config():
  """The YAML config of each sweep's Shingle, as JSON."""
  result = json.loads(json.dumps(checks.BASE_CONFIG))
  result['xwayland'] = {'mode': 'lazy'}
  result['workspaces'] = {'width': 2, 'height': 1}
  result['outputs'] = [dict(output) for output in OUTPUTS]
  result['background_color'] = BACKGROUND_COLOR
  result['animate'] = {'open_animation': 'none'}
  result['tiling'] = {'animation_duration': 0, 'gaps': dict(GAPS)}
  return result

class Bus:
  """A private D-Bus session bus that activates nothing."""

  def __init__(self, directory):
    self.directory = directory
    services = os.path.join(directory, 'services')
    os.makedirs(services, exist_ok=True)
    path = os.path.join(directory, 'bus.conf')
    socket_path = os.path.join(directory, 'bus')
    _write(path, screencast_checks.BUS_CONFIG.format(socket=socket_path,
                                                     services=services))
    self.address = f'unix:path={socket_path}'
    environment = {key: value for key, value in os.environ.items()
                   if key not in run.STRIPPED_VARIABLES}
    self.service = screencast_checks.Service(directory, 'dbus', [
        'dbus-daemon', '--nofork', '--nosyslog', f'--config-file={path}'],
        environment)
    if not xw.wait_for(lambda: os.path.exists(socket_path), timeout=10):
      raise RuntimeError(f'no private bus: {self.service.text()}')

  def stop(self):
    """Ends the bus."""
    self.service.stop()

class Sweep:
  """One application's Shingle, bus, home and helpers."""

  def __init__(self, binary, name, captures_directory):
    self.name = name
    self.before = namespace_pids()
    self.shingle = checks.Shingle(
        binary, f'app-{name}', config(),
        backend=('--backend', 'offscreen', '--outputs', str(len(OUTPUTS))))
    self.directory = os.path.join(self.shingle.directory, 'app')
    self.home = os.path.join(self.directory, 'home')
    for sub in ('home', 'config', 'data', 'cache', 'runtime-x', 'files',
                'profile'):
      os.makedirs(os.path.join(self.directory, sub), exist_ok=True)
    self.bus = Bus(os.path.join(self.directory, 'dbus'))
    self.display = xw.wait_for(lambda: next(
        (line.split('=', 1)[1] for line in self.shingle.log_text().splitlines()
         if line.startswith('DISPLAY=')), None), timeout=30)
    self.captures_directory = captures_directory
    self.capture_index = 0
    self.outputs = self._outputs()

  def _outputs(self):
    found = {}
    for output in self.shingle.call('window-rules/list-outputs'):
      geometry = output['geometry']
      found[output['name']] = {
          'id': output['id'], 'rect': (geometry['x'], geometry['y'],
                                       geometry['width'], geometry['height'])}
    return found

  def environment(self, extra=None):
    """The environment of an application of this sweep."""
    environment = {
        'HOME': self.home,
        'XDG_CONFIG_HOME': os.path.join(self.directory, 'config'),
        'XDG_DATA_HOME': os.path.join(self.directory, 'data'),
        'XDG_CACHE_HOME': os.path.join(self.directory, 'cache'),
        'XDG_CURRENT_DESKTOP': 'wayfire',
        'DBUS_SESSION_BUS_ADDRESS': self.bus.address,
        'DBUS_SYSTEM_BUS_ADDRESS': 'unix:path=/nonexistent/system_bus',
        'NO_AT_BRIDGE': '1',
        'GTK_A11Y': 'none',
        'WAYLAND_DEBUG': '1',
    }
    if self.display:
      environment['DISPLAY'] = self.display
    environment.update(extra or {})
    return environment

  def start(self, arguments, name, extra=None):
    """A client of this sweep's Shingle, run as the unprivileged user."""
    return checks.Client(self.shingle, as_user(arguments), name,
                         self.environment(extra),
                         program=os.path.basename(arguments[0]))

  def views(self):
    """The mapped views."""
    return [view for view in self.shingle.views() if view.get('mapped')]

  def app_views(self):
    """The mapped views but the probe."""
    return [view for view in self.views()
            if view.get('app-id') != PROBE_APP_ID]

  def view(self, view_id):
    """The listed view with `view_id`, or None."""
    return next((view for view in self.shingle.views()
                 if view['id'] == view_id), None)

  def focused_id(self):
    """The focused view's id, or None."""
    info = self.shingle.call('window-rules/get-focused-view').get('info')
    return info.get('id') if info else None

  def layout(self):
    """(app-id, geometry, tiled-edges) of every mapped view."""
    return [(view['app-id'], view['geometry'], view['tiled-edges'])
            for view in self.views()]

  def rects(self):
    """The geometry of every mapped view."""
    return [view['geometry'] for view in self.views()]

  def output_rects(self):
    """Every output's rectangle."""
    return [output['rect'] for output in self.outputs.values()]

  def capture(self, step):
    """A capture of every output, saved as `NAME-NN-step.ppm` when the
    sweep keeps captures."""
    shot = checks.Captures().take(self.shingle)
    if shot is not None and self.captures_directory:
      os.makedirs(self.captures_directory, exist_ok=True)
      path = os.path.join(self.captures_directory,
                          f'{self.name}-{self.capture_index:02}-{step}.ppm')
      self.capture_index += 1
      _write(path, xw.encode_ppm(shot))
    return shot

  def settled(self, step, predicate, timeout=15.0):
    """Captures until `predicate(shot)` holds; saves and returns the
    last."""
    last = [None]

    def attempt():
      last[0] = checks.Captures().take(self.shingle)
      return last[0] is not None and predicate(last[0])
    xw.wait_for(attempt, timeout=timeout, interval=0.4)
    if last[0] is not None and self.captures_directory:
      os.makedirs(self.captures_directory, exist_ok=True)
      path = os.path.join(self.captures_directory,
                          f'{self.name}-{self.capture_index:02}-{step}.ppm')
      self.capture_index += 1
      _write(path, xw.encode_ppm(last[0]))
    return last[0]

  def key(self, key, pressed):
    """One STIPC key."""
    self.shingle.call('stipc/feed_key', {'key': key, 'state': pressed})

  def type_text(self, text, delay=0.0):
    """Types `text` through STIPC keys."""
    for key, shifted in key_sequence(text):
      if shifted:
        self.key('KEY_LEFTSHIFT', True)
      self.key(key, True)
      self.key(key, False)
      if shifted:
        self.key('KEY_LEFTSHIFT', False)
      if delay:
        time.sleep(delay)

  def chord(self, chord):
    """Presses and releases a chord such as `ctrl+o`."""
    keys = chord_keys(chord)
    for key in keys:
      self.key(key, True)
    for key in reversed(keys):
      self.key(key, False)

  def click(self, point, button='BTN_LEFT'):
    """Moves the STIPC pointer to `point` and clicks."""
    self.shingle.call('stipc/move_cursor', {'x': point[0], 'y': point[1]})
    self.shingle.call('stipc/feed_button', {'combo': button, 'mode': 'full'})

  def x_properties(self, title):
    """WM_TRANSIENT_FOR, WM_CLIENT_LEADER and _NET_WM_WINDOW_TYPE of the
    X window named `title`, as xprop prints them, for the record."""
    environment = self.environment({'WAYLAND_DEBUG': '0'})
    try:
      tree = subprocess.run(['xwininfo', '-root', '-tree'], env=environment,
                            capture_output=True, text=True, timeout=10)
      window = next((line.split()[0] for line in tree.stdout.splitlines()
                     if f'"{title}"' in line), None)
      if window is None:
        return 'no window of that name'
      done = subprocess.run(
          ['xprop', '-id', window, 'WM_TRANSIENT_FOR', 'WM_CLIENT_LEADER',
           '_NET_WM_WINDOW_TYPE'], env=environment, capture_output=True,
          text=True, timeout=10)
      return f'{window}: ' + ' | '.join(done.stdout.split('\n')).strip()
    except subprocess.TimeoutExpired:
      return 'timed out'

  def wl_copy(self, text):
    """Starts `wl-copy --foreground` with `text`; returns the client."""
    path = os.path.join(self.directory, 'copy.txt')
    _write(path, text)
    return checks.Client(self.shingle, ['sh', '-c',
                                        f'exec wl-copy --foreground < {path}'],
                         'wl-copy', self.environment({'WAYLAND_DEBUG': '0'}),
                         program='wl-copy')

  def wl_paste(self):
    """What `wl-paste` reads from the clipboard, or None."""
    try:
      done = subprocess.run(
          ['wl-paste', '--no-newline'], capture_output=True, timeout=10,
          env=self.shingle.client_environment(), stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
      return None
    if done.returncode != 0:
      return None
    return done.stdout.decode(errors='replace')

  def stop(self, results):
    """Ends every helper, the bus and Shingle; checks Shingle's exit."""
    keep = descendants(self.shingle.process.pid) | {os.getpid()}
    leftovers = new_pids(self.before, namespace_pids(), keep)
    self.bus.stop()
    stuck = end_processes(leftovers)
    if stuck:
      print(f'INFO {self.name}: processes would not end: {stuck}', flush=True)
    log = self.shingle.log_text()
    alive = self.shingle.alive()
    status = self.shingle.stop()
    results.check(f'{self.name}: Shingle ran throughout and SIGTERM exits '
                  'it with status 0', alive and status == 0,
                  f'status {status}, {checks.tail(log)}')
    panics = [line for line in log.splitlines() if 'panicked' in line]
    end_processes(new_pids(self.before, namespace_pids(), {os.getpid()}))
    return panics

class Scenario:
  """Runs one App's steps and records each in `results` and `row`."""

  def __init__(self, results, sweep, app, wl_probe):
    self.results = results
    self.sweep = sweep
    self.app = app
    self.wl_probe = wl_probe
    self.row = {}
    self.client = None
    self.main = None
    self.x11 = False

  def record(self, step, passed, detail=''):
    """One step's PASS or FAIL."""
    self.row[step] = 'ok' if passed else 'FAIL'
    self.results.check(f'{self.app.name}: {step}', passed, detail)
    return passed

  def skip(self, step, reason):
    """A step that does not apply."""
    self.row[step] = 'n/a'
    print(f'INFO {self.app.name}: {step}: n/a, {reason}', flush=True)

  def log(self):
    """The application's output so far."""
    return self.client.output() if self.client else ''

  def main_view(self):
    """The main view, refreshed."""
    return self.sweep.view(self.main['id']) if self.main else None

  def output_rect(self, name='HEADLESS-1'):
    """An output's rectangle as a geometry dict."""
    x, y, width, height = self.sweep.outputs[name]['rect']
    return {'x': x, 'y': y, 'width': width, 'height': height}

  def run(self):
    """Every step in order; later steps need the window to have
    mapped."""
    directory = self.sweep.directory
    if self.app.prepare:
      self.app.prepare(directory)
    arguments = [part.format(dir=directory, home=self.sweep.home)
                 for part in self.app.command]
    self.client = self.sweep.start(arguments, self.app.name,
                                   self.app.environment)
    try:
      if not self.step_maps():
        return
      for step in (self.step_tiles, self.step_draws, self.step_focus,
                   self.step_typing, self.step_popup, self.step_dialog,
                   self.step_copy_paste, self.step_resize,
                   self.step_fullscreen, self.step_workspace,
                   self.step_output, self.step_close):
        if step != self.step_close and self.main_view() is None:
          break
        try:
          step()
        except (OSError, RuntimeError, TimeoutError, ConnectionError,
                KeyError, TypeError) as error:
          self.record(step.__name__[5:].replace('_', '-'), False,
                      f'{type(error).__name__}: {error}')
    finally:
      self.finish()

  def step_maps(self):
    """It maps a window."""
    views = xw.wait_for(lambda: self.sweep.app_views(),
                        timeout=self.app.map_timeout)
    if not views:
      return self.record('maps', False, checks.tail(self.log(), 10))
    # Some applications map a splash, a welcome dialog or a second window
    # first; the largest view without a parent is the main one.
    time.sleep(2)
    views = self.sweep.app_views() or views
    if self.app.dismiss:
      for view in views:
        if view['title'].startswith(self.app.dismiss):
          print(f'INFO {self.app.name}: closing {view["title"]!r} first',
                flush=True)
          self.sweep.shingle.call('window-rules/close-view',
                                  {'id': view['id']})
      views = xw.wait_for(lambda: [
          view for view in self.sweep.app_views()
          if not view['title'].startswith(self.app.dismiss)],
          timeout=self.app.map_timeout)
      if not views:
        return self.record('maps', False, 'only its first window mapped')
      time.sleep(2)
    self.main = main_view(views)
    x11 = 'xdg_toplevel' not in self.log()
    self.x11 = x11
    print(f'INFO {self.app.name}: maps as {"X11" if x11 else "Wayland"}, '
          f'app-id {self.main["app-id"]!r}, title {self.main["title"]!r}, '
          f'{len(views)} view(s)', flush=True)
    return self.record('maps', True, f'{len(views)} view(s)')

  def step_tiles(self):
    """It fills the work area of the first output, inside the gaps."""
    view = self.main_view()
    area = self.output_rect()
    expected = (area['x'] + GAPS['left'], area['y'] + GAPS['top'],
                area['width'] - GAPS['left'] - GAPS['right'],
                area['height'] - GAPS['top'] - GAPS['bottom'])
    geometry = view['geometry']
    actual = (geometry['x'], geometry['y'], geometry['width'],
              geometry['height'])
    self.record('tiles', actual == expected and view['tiled-edges'] != 0,
                f'geometry {actual}, expected {expected}, tiled-edges '
                f'{view["tiled-edges"]}')

  def drawn_ok(self, shot, rect):
    """(passed, detail): `rect` is drawn, to its edges, and nothing is
    outside the views."""
    if shot is None:
      return False, 'no capture'
    problems = []
    if blank(shot, rect):
      problems.append('blank')
    stale = stale_edges(shot, rect)
    if stale:
      problems.append(f'background along {stale}')
    wrong = misplaced(shot, self.sweep.output_rects(), self.sweep.rects())
    if wrong:
      problems.append(f'content in the gaps at {wrong}')
    return not problems, ', '.join(problems) or 'drawn'

  def wait_drawn(self, step, rect_of, timeout=20.0):
    """Captures until `rect_of()` is drawn; returns (passed, detail)."""
    outcome = [False, 'no capture']

    def good(shot):
      rect = rect_of()
      if rect is None:
        outcome[:] = [False, 'no view']
        return False
      outcome[:] = self.drawn_ok(shot, rect)
      return outcome[0]
    self.sweep.settled(step, good, timeout=timeout)
    return tuple(outcome)

  def base(self):
    """The main view's client rectangle, or None."""
    view = self.main_view()
    return view['base-geometry'] if view else None

  def step_draws(self):
    """It draws its whole view and nothing outside it."""
    passed, detail = self.wait_drawn('draws', self.base, timeout=30)
    self.record('draws', passed, detail)

  def step_focus(self):
    """The new window has keyboard focus without being asked."""
    focused = xw.wait_for(lambda: self.sweep.focused_id() == self.main['id'],
                          timeout=10)
    if not focused:
      self.sweep.shingle.call('window-rules/focus-view',
                              {'id': self.main['id']})
    self.record('focus', focused,
                f'focused {self.sweep.focused_id()}, main {self.main["id"]}')

  def click_into(self):
    """Clicks into the middle of the main view, as a user would before
    typing."""
    base = self.base()
    self.sweep.click((base['x'] + base['width'] // 2,
                      base['y'] + base['height'] // 2))
    time.sleep(0.5)

  def shell_file(self, name):
    return os.path.join(self.sweep.directory, name)

  def step_typing(self):
    """Typed text reaches it, checked by what it does with the text."""
    kind = self.app.text
    if kind == 'shell':
      time.sleep(2)
      path = self.shell_file('typed.txt')
      self.sweep.type_text(f'echo {TOKEN_TYPED} > {path}\n')
      done = xw.wait_for(lambda: _read(path).strip() == TOKEN_TYPED,
                         timeout=10)
      self.record('typing', done, f'{path}: {_read(path)!r}')
    elif kind == 'editor':
      self.click_into()
      self.sweep.type_text(TOKEN_TYPED, delay=0.01)
      time.sleep(0.5)
      self.sweep.chord('ctrl+a')
      self.sweep.chord('ctrl+c')
      # LibreOffice's autocorrect capitalizes the sentence.
      text = xw.wait_for(lambda: (self.sweep.wl_paste() or '').lower()
                         == TOKEN_TYPED, timeout=10)
      self.record('typing', bool(text),
                  f'wl-paste read {self.sweep.wl_paste()!r}')
      self.record('copy', bool(text), 'the typed text, copied in the app')
    elif kind == 'browser':
      # Chrome opens a tab of its own shortly after it starts, which
      # takes the location bar.
      time.sleep(BROWSER_SETTLE)
      ok = self.browse('red', 'typing', typed=True)
      self.record('typing', ok, 'a typed data: URL paints the page red')
    elif self.app.fullscreen_key:
      self.sweep.chord(self.app.fullscreen_key)
      went = xw.wait_for(lambda: self.main_view()['fullscreen'], timeout=10)
      self.sweep.chord(self.app.fullscreen_key)
      back = xw.wait_for(lambda: not self.main_view()['fullscreen'],
                         timeout=10)
      self.record('typing', went and back,
                  f'its fullscreen key: fullscreen {went}, back {back}')
    elif self.app.dialog:
      self.skip('typing', 'no text field; the dialog step checks a key')
    else:
      self.skip('typing', 'nothing to type into')

  def browse(self, colour, step, typed):
    """Types, or pastes, a data: URL that paints the page `colour` into
    the location bar; returns whether the page shows it."""
    url = f'data:text/html,<body style="background:{colour}">'
    self.sweep.chord('ctrl+l')
    time.sleep(0.5)
    if typed:
      self.sweep.type_text(url, delay=0.01)
    else:
      self.sweep.chord('ctrl+v')
    time.sleep(0.5)
    self.sweep.chord('enter')
    base = self.base()
    page = {'x': base['x'] + base['width'] // 4,
            'y': base['y'] + base['height'] * 2 // 3,
            'width': base['width'] // 2, 'height': base['height'] // 4}

    def painted(shot):
      return sum(checks.classify(*shot.pixel(*point)) == colour
                 for point in sample_points(page)) > 0.8 * len(
                     sample_points(page))
    shot = self.sweep.settled(step, painted, timeout=20)
    return shot is not None and painted(shot)

  def menus(self):
    """How many menus the client opened and closed: xdg popups, and the
    subsurfaces GTK 3 uses for popovers inside its window."""
    log = self.log()
    opened = (debug_requests(log, 'xdg_surface', 'get_popup')
              + debug_requests(log, 'wl_subcompositor', 'get_subsurface'))
    closed = (debug_requests(log, 'xdg_popup', 'destroy')
              + debug_requests(log, 'wl_subsurface', 'destroy')
              + debug_events(log, 'xdg_popup', 'popup_done'))
    return opened, closed

  def step_popup(self):
    """A right-click opens a popup menu, and Escape closes it. A Wayland
    client's menu is seen in its log; an X client's, an override-redirect
    window, in captures."""
    if not self.app.popup:
      return self.skip('popup', 'no menu on a click')
    base = self.base()
    point = (base['x'] + base['width'] // 2, base['y'] + base['height'] // 2)
    if self.x11:
      before = self.sweep.settled('popup-before', lambda shot: True)
      self.sweep.click(point, 'BTN_RIGHT')
      shot = self.sweep.settled('popup', lambda shot: changed_share(
          before, shot, base) > 0.01, timeout=10)
      opened = changed_share(before, shot, base) > 0.01
      self.sweep.chord('esc')
      after = self.sweep.settled('popup-closed', lambda shot: changed_share(
          before, shot, base) < 0.005, timeout=10)
      closed = changed_share(before, after, base) < 0.005
      return self.record('popup', opened and closed,
                         f'X menu: {changed_share(before, shot, base):.3f} '
                         f'of the window changed, closed {closed}')
    opened_before, closed_before = self.menus()
    self.sweep.click(point, 'BTN_RIGHT')
    opened = xw.wait_for(lambda: self.menus()[0] > opened_before, timeout=10)
    shot = self.sweep.capture('popup')
    detail = f'{self.menus()[0] - opened_before} menu surface(s)'
    if opened and shot is not None:
      wrong = misplaced(shot, self.sweep.output_rects(), self.sweep.rects())
      detail += f', gaps {"clean" if not wrong else wrong}'
    self.sweep.chord('esc')
    closed = xw.wait_for(lambda: self.menus()[1] > closed_before, timeout=10)
    self.record('popup', opened and closed,
                f'{detail}, closed {bool(closed)}')

  def step_dialog(self):
    """Its dialog chord opens a child window that is drawn, focused and
    closed by Escape."""
    if not self.app.dialog:
      return self.skip('dialog', 'no dialog on a key')
    known = {view['id'] for view in self.sweep.app_views()}
    self.sweep.chord(self.app.dialog)
    dialog = xw.wait_for(lambda: next(
        (view for view in self.sweep.app_views() if view['id'] not in known),
        None), timeout=20)
    if dialog is None:
      return self.record('dialog', False, f'no new view after '
                         f'{self.app.dialog}')
    time.sleep(1)
    dialog = self.sweep.view(dialog['id']) or dialog
    if self.x11:
      print(f'INFO {self.app.name}: dialog X properties: '
            f'{self.sweep.x_properties(dialog["title"])}', flush=True)
    problems = []
    if self.app.orphan_dialog:
      print(f'INFO {self.app.name}: dialog parent and tiling not checked: '
            f'{self.app.orphan_dialog}', flush=True)
    elif dialog['parent'] != self.main['id']:
      problems.append(f'parent {dialog["parent"]}, not {self.main["id"]}')
    if self.app.orphan_dialog:
      pass
    elif dialog['tiled-edges'] != 0:
      problems.append('tiled')
    elif not centred_on(dialog['geometry'], self.main_view()['geometry'],
                        self.output_rect()):
      problems.append(f'not centred on {self.main_view()["geometry"]}')
    drawn = self.wait_drawn('dialog', lambda: (
        self.sweep.view(dialog['id']) or {}).get('base-geometry'))
    if not drawn[0]:
      problems.append(f'dialog {drawn[1]}')
    if self.sweep.focused_id() != dialog['id']:
      problems.append(f'focus on {self.sweep.focused_id()}')
    self.sweep.chord('esc')
    gone = xw.wait_for(lambda: dialog['id'] not in
                       {view['id'] for view in self.sweep.app_views()},
                       timeout=10)
    if not gone:
      problems.append('Escape left it open')
      self.sweep.shingle.call('window-rules/close-view', {'id': dialog['id']})
      xw.wait_for(lambda: dialog['id'] not in
                  {view['id'] for view in self.sweep.app_views()}, timeout=10)
    back = xw.wait_for(lambda: self.sweep.focused_id() == self.main['id'],
                       timeout=5)
    if not back:
      problems.append(f'focus went to {self.sweep.focused_id()} after')
    self.record('dialog', not problems,
                f'{dialog["title"]!r} {dialog["geometry"]}'
                + (f': {", ".join(problems)}' if problems else ''))

  def step_copy_paste(self):
    """Text copied in it reaches wl-paste and text from wl-copy is pasted
    into it."""
    kind = self.app.text
    if not self.app.copy_paste or kind is None:
      self.skip('copy', 'no text to copy')
      self.skip('paste', 'no text field')
      return
    copier = self.sweep.wl_copy(TOKEN_PASTED if kind != 'browser' else
                                'data:text/html,<body style="background:'
                                'blue">')
    try:
      time.sleep(1)
      if kind == 'shell':
        self.skip('copy', 'a terminal copies a mouse selection only')
        path = self.shell_file('pasted.txt')
        self.sweep.type_text('echo ')
        self.sweep.chord('ctrl+shift+v')
        time.sleep(0.5)
        self.sweep.type_text(f' > {path}\n')
        done = xw.wait_for(lambda: _read(path).strip() == TOKEN_PASTED,
                           timeout=10)
        self.record('paste', done, f'{path}: {_read(path)!r}')
      elif kind == 'editor':
        self.sweep.chord('ctrl+a')
        self.sweep.chord('ctrl+v')
        time.sleep(1)
        copier.stop()
        self.sweep.chord('ctrl+a')
        self.sweep.chord('ctrl+c')
        text = xw.wait_for(lambda: self.sweep.wl_paste() == TOKEN_PASTED,
                           timeout=10)
        self.record('paste', text, f'wl-paste read {self.sweep.wl_paste()!r}')
      elif kind == 'browser':
        ok = self.browse('blue', 'paste', typed=False)
        self.record('paste', ok, 'a pasted data: URL paints the page blue')
        copier.stop()
        self.sweep.chord('ctrl+l')
        self.sweep.chord('ctrl+c')
        text = xw.wait_for(lambda: (self.sweep.wl_paste() or '').startswith(
            'data:text/html'), timeout=10)
        self.record('copy', text,
                    f'wl-paste read {(self.sweep.wl_paste() or "")[:60]!r}')
        self.sweep.chord('esc')
    finally:
      copier.stop()

  def redrawn(self, step):
    """(passed, detail): the main view is drawn to its edges."""
    return self.wait_drawn(step, self.base)

  def probe_shown(self, shot):
    """Whether the probe's quadrants show at its view."""
    probe = next((view for view in self.sweep.views()
                  if view.get('app-id') == PROBE_APP_ID), None)
    if probe is None or shot is None:
      return False
    return not checks.pattern_mismatches(shot, probe['base-geometry'])

  def step_resize(self):
    """The tiler halves it beside a probe, then a Super+right drag on the
    boundary widens it; it must redraw each time."""
    probe = checks.Client(self.sweep.shingle,
                          [self.wl_probe, PROBE_APP_ID, '--pattern'],
                          'probe', program='wl_probe')
    try:
      halved = xw.wait_for(lambda: self.main_view()['geometry']['width']
                           < self.output_rect()['width'] * 0.7, timeout=15)
      drawn = self.redrawn('resize-tiler')
      # A dialog's closing animation may still cover it for a moment.
      shot = self.sweep.settled('resize-tiler-probe', self.probe_shown,
                                timeout=10)
      shown = self.probe_shown(shot)
      self.record('resize-tiler', halved and drawn[0] and shown,
                  f'geometry {self.main_view()["geometry"]}, {drawn[1]}, '
                  f'views {self.sweep.layout()}, '
                  f'probe {"shown" if shown else "covered or missing"}')
      before = self.main_view()['geometry']
      # The boundary is between the two views: the left one's right edge.
      probe_view = next(view for view in self.sweep.views()
                        if view.get('app-id') == PROBE_APP_ID)
      left = min((before, probe_view['geometry']), key=lambda g: g['x'])
      start = (left['x'] + left['width'] - 20, left['y'] + left['height'] // 2)
      drag(self.sweep.shingle, start, (start[0] + 160, start[1]))
      changed = xw.wait_for(lambda: self.main_view()['geometry']['width']
                            != before['width'], timeout=10)
      drawn = self.redrawn('resize-drag')
      # A dialog's closing animation may still cover it for a moment.
      shot = self.sweep.settled('resize-drag-probe', self.probe_shown,
                                timeout=10)
      shown = self.probe_shown(shot)
      self.record('resize-drag', changed and drawn[0] and shown,
                  f'{before["width"]} -> '
                  f'{self.main_view()["geometry"]["width"]}, {drawn[1]}, '
                  f'probe {"shown" if shown else "covered or missing"}')
    finally:
      probe.stop()
    xw.wait_for(lambda: self.main_view()['geometry']['width']
                > self.output_rect()['width'] * 0.8, timeout=10)

  def step_fullscreen(self):
    """IPC fullscreen covers the output; leaving it restores the tile."""
    tiled = self.main_view()['geometry']
    self.sweep.shingle.call('wm-actions/set-fullscreen',
                            {'view_id': self.main['id'], 'state': True})
    area = self.output_rect()
    went = xw.wait_for(lambda: self.main_view()['fullscreen'] and (
        self.main_view()['geometry']['width'], self.main_view()['geometry'][
            'height']) == (area['width'], area['height']), timeout=10)
    drawn = self.redrawn('fullscreen')
    self.sweep.shingle.call('wm-actions/set-fullscreen',
                            {'view_id': self.main['id'], 'state': False})
    back = xw.wait_for(lambda: not self.main_view()['fullscreen']
                       and self.main_view()['geometry'] == tiled, timeout=10)
    redrawn = self.redrawn('unfullscreen')
    self.record('fullscreen', went and drawn[0] and back and redrawn[0],
                f'fullscreen {bool(went)}: {drawn[1]}; back {bool(back)}: '
                f'{redrawn[1]}')

  def step_workspace(self):
    """Sent to the second workspace it leaves the first; switching there
    shows it drawn; it comes back with the switch. Its IPC geometry is
    relative to the current workspace, as in Wayfire: one output width
    to the right while it is away, the same rectangle less that width
    once its workspace is current, and on the first again back home."""
    output = self.sweep.outputs['HEADLESS-1']['id']
    area = self.output_rect()
    width = area['width']
    self.sweep.shingle.call('vswitch/send-view',
                            {'view-id': self.main['id'], 'x': 1, 'y': 0})

    def one_right():
      geometry = self.main_view()['geometry']
      return geometry if workspace_column(geometry, area) == 1 else None
    away = xw.wait_for(one_right, timeout=10)
    empty = self.sweep.settled('workspace-empty', lambda shot: background_share(
        shot, sample_points(area, 32)) > 0.98, timeout=10)
    cleared = empty is not None and background_share(
        empty, sample_points(area, 32)) > 0.98
    self.sweep.shingle.call('vswitch/set-workspace',
                            {'output-id': output, 'x': 1, 'y': 0})
    current = away is not None and xw.wait_for(
        lambda: self.main_view()['geometry'] == shifted(away, dx=-width),
        timeout=10)
    there = self.redrawn('workspace-2')
    self.sweep.shingle.call('vswitch/set-workspace', {
        'output-id': output, 'x': 0, 'y': 0, 'view-id': self.main['id']})
    home = self.redrawn('workspace-1')
    back = workspace_column(self.main_view()['geometry'], area) == 0
    self.record('workspace', bool(cleared and there[0] and home[0] and away
                                  and current and back),
                f'first workspace cleared {cleared}, '
                f'second: {there[1]}, back: {home[1]}; IPC geometry away '
                f'{json.dumps(away)}, on its workspace {bool(current)}, '
                f'home {back}')

  def step_output(self):
    """Moved to the second output it is drawn there at that output's
    size; moved back it is drawn on the first."""
    second = self.sweep.outputs['HEADLESS-2']
    first = self.sweep.outputs['HEADLESS-1']
    self.sweep.shingle.call('window-rules/configure-view',
                            {'id': self.main['id'], 'output_id': second['id']})
    moved = xw.wait_for(lambda: self.main_view()['output-id'] == second['id'],
                        timeout=10)
    there = self.redrawn('output-2')
    self.sweep.shingle.call('window-rules/configure-view',
                            {'id': self.main['id'], 'output_id': first['id']})
    back = xw.wait_for(lambda: self.main_view()['output-id'] == first['id'],
                       timeout=10)
    home = self.redrawn('output-1')
    self.record('output', moved and there[0] and back and home[0],
                f'moved {bool(moved)} {self.main_view()["geometry"]}: '
                f'{there[1]}; back {bool(back)}: {home[1]}')

  def step_close(self):
    """close-view ends the application, or it asks first."""
    known = {view['id'] for view in self.sweep.app_views()}
    self.sweep.shingle.call('window-rules/close-view',
                            {'id': self.main['id']})
    status = self.client.exited(20)
    self.sweep.capture('close')
    asked = None
    if status is None:
      asked = next((view for view in self.sweep.app_views()
                    if view['id'] not in known), None)
    gone = not any(view['id'] == self.main['id']
                   for view in self.sweep.app_views())
    if status is not None or gone:
      self.record('close', True, f'exit {status}, window gone {gone}')
    elif asked is not None:
      self.record('close', True, f'asks first: {asked["title"]!r}')
    else:
      self.record('close', False, 'still running with its window')

  def finish(self):
    """Stops the client and judges its log."""
    status = self.client.process.poll()
    if status is None:
      end_processes(sorted(descendants(self.client.process.pid)))
      status = self.client.process.poll()
    self.client.log.close()
    log = self.log()
    errors = protocol_errors(log)
    self.record('protocol', not errors, '; '.join(errors[:3]) or 'none')
    crashed = crash_lines(log)
    signalled = isinstance(status, int) and status < 0 and status not in (
        -signal.SIGTERM, -signal.SIGKILL)
    self.record('crash', not crashed and not signalled,
                f'exit {status}' + (f', {crashed}' if crashed else ''))

def _read(path):
  try:
    with open(path, errors='replace') as file:
      return file.read()
  except OSError:
    return ''

def drag(shingle, start, end, steps=20):
  """A Super+right-button drag from `start` to `end` through STIPC."""
  shingle.call('stipc/move_cursor', {'x': start[0], 'y': start[1]})
  time.sleep(0.1)
  shingle.call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': True})
  shingle.call('stipc/feed_button', {'combo': 'BTN_RIGHT', 'mode': 'press'})
  for index in range(1, steps + 1):
    shingle.call('stipc/move_cursor', {
        'x': start[0] + (end[0] - start[0]) * index // steps,
        'y': start[1] + (end[1] - start[1]) * index // steps})
    time.sleep(0.02)
  shingle.call('stipc/feed_button', {'combo': 'BTN_RIGHT', 'mode': 'release'})
  shingle.call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': False})

def app_checks(results, binary, wl_probe, captures, app, matrix):
  """Runs one App's scenario in its own sweep."""
  if app.heavy:
    load = wait_for_load()
    print(f'INFO {app.name}: load {load:.1f}', flush=True)
  else:
    wait_for_load()
  sweep = Sweep(binary, app.name, captures.directory)
  scenario = Scenario(results, sweep, app, wl_probe)
  try:
    scenario.run()
  finally:
    panics = sweep.stop(results)
    if panics:
      scenario.record('crash', False, f'Shingle: {panics[:2]}')
    matrix[app.name] = scenario.row

def rofi_checks(results, binary, wl_probe, captures, matrix):
  """rofi through Xwayland appears over a tiled window with keyboard
  focus and runs the typed command."""
  wait_for_load()
  sweep = Sweep(binary, 'rofi', captures.directory)
  row = {}
  matrix['rofi'] = row

  def record(step, passed, detail=''):
    row[step] = 'ok' if passed else 'FAIL'
    return results.check(f'rofi: {step}', passed, detail)
  probe = checks.Client(sweep.shingle, [wl_probe, PROBE_APP_ID, '--pattern'],
                        'probe', program='wl_probe')
  rofi = None
  try:
    view = xw.wait_for(lambda: sweep.views(), timeout=15)
    before = sweep.settled('before', lambda shot: True)
    if not view or not sweep.display:
      record('maps', False, f'probe {bool(view)}, DISPLAY {sweep.display}')
      return
    rofi = sweep.start(['rofi', '-show', 'run', '-no-lazy-grab'], 'rofi',
                       {'WAYLAND_DEBUG': '0'})
    area = {'x': 320, 'y': 180, 'width': 640, 'height': 360}
    shot = sweep.settled('shown', lambda shot: changed_share(
        before, shot, area) > 0.3, timeout=30)
    share = changed_share(before, shot, area)
    record('maps', share > 0.3,
           f'{share:.2f} of the middle changed; {checks.tail(rofi.output())}')
    record('draws', shot is not None and not blank(shot, area),
           'the middle of the output')
    record('tiles', not sweep.app_views()[1:],
           'rofi is not a managed view: '
           f'{[view["app-id"] for view in sweep.app_views()]}')
    time.sleep(1)
    path = os.path.join(sweep.directory, 'rofi-ran')
    sweep.type_text(f'touch {path}', delay=0.02)
    time.sleep(0.5)
    sweep.chord('ctrl+enter')
    ran = xw.wait_for(lambda: os.path.exists(path), timeout=15)
    record('typing', ran, f'{path} exists: {ran}')
    record('focus', ran, 'its typed command ran')
    gone = rofi.exited(10)
    after = sweep.settled('after', lambda shot: changed_share(
        before, shot, area) < 0.05, timeout=15)
    cleared = changed_share(before, after, area) < 0.05
    record('close', gone is not None and cleared,
           f'exit {gone}, the probe shows again: {cleared}')
    record('crash', gone == 0, f'exit {gone}')
  finally:
    if rofi is not None:
      rofi.stop()
    probe.stop()
    sweep.stop(results)

def dunst_checks(results, binary, wl_probe, captures, matrix):
  """dunst shows a notify-send notification in a layer surface on the
  private bus and takes it away."""
  del wl_probe
  wait_for_load()
  sweep = Sweep(binary, 'dunst', captures.directory)
  row = {}
  matrix['dunst'] = row

  def record(step, passed, detail=''):
    row[step] = 'ok' if passed else 'FAIL'
    return results.check(f'dunst: {step}', passed, detail)
  dunst = sweep.start(['dunst'], 'dunst')
  try:
    owned = xw.wait_for(lambda: 'org.freedesktop.Notifications' in _bus_names(
        sweep), timeout=20)
    record('maps', owned, 'dunst owns org.freedesktop.Notifications')
    before = sweep.settled('before', lambda shot: True)
    whole = {'x': 0, 'y': 0, 'width': before.width if before else 1,
             'height': before.height if before else 1}
    sent = subprocess.run(
        ['notify-send', '--expire-time=3000', 'Shingle sweep', 'body text'],
        env=sweep.environment({'WAYLAND_DEBUG': '0'}), capture_output=True,
        text=True, timeout=20, stdin=subprocess.DEVNULL)
    layer = xw.wait_for(lambda: debug_requests(
        dunst.output(), 'zwlr_layer_shell_v1', 'get_layer_surface') > 0,
        timeout=10)
    shot = sweep.settled('shown', lambda shot: changed_share(
        before, shot, whole, step=4) > 0.002, timeout=10)
    share = changed_share(before, shot, whole, step=4)
    record('draws', sent.returncode == 0 and layer and share > 0.002,
           f'notify-send {sent.returncode} {sent.stderr.strip()!r}, '
           f'layer surface {bool(layer)}, {share:.4f} of the outputs changed')
    after = sweep.settled('expired', lambda shot: changed_share(
        before, shot, whole, step=4) == 0, timeout=15)
    record('close', changed_share(before, after, whole, step=4) == 0,
           'the notification went away after its 3 s')
  finally:
    status = dunst.process.poll()
    dunst.stop()
    log = dunst.output()
    errors = protocol_errors(log)
    record('protocol', not errors, '; '.join(errors[:3]) or 'none')
    record('crash', status is None and not crash_lines(log),
           f'running until stopped: {status is None}')
    sweep.stop(results)

def _bus_names(sweep):
  done = subprocess.run(
      ['dbus-send', f'--bus={sweep.bus.address}', '--print-reply',
       '--dest=org.freedesktop.DBus', '/org/freedesktop/DBus',
       'org.freedesktop.DBus.ListNames'], capture_output=True, text=True,
      timeout=10)
  return done.stdout

MATRIX = {}

def _group(app):
  def check(results, binary, wl_probe, captures):
    _prepare_namespace()
    app_checks(results, binary, wl_probe, captures, app, MATRIX)
    _print_table()
  return check

def _special(function):
  def check(results, binary, wl_probe, captures):
    _prepare_namespace()
    function(results, binary, wl_probe, captures, MATRIX)
    _print_table()
  return check

def _prepare_namespace():
  lower_priority()
  if not screencast_checks.mount_private_dbus_dir():
    raise RuntimeError('cannot cover /run/dbus')

def _print_table():
  print('INFO apps: per-app results so far:', flush=True)
  for line in table(MATRIX).splitlines():
    print(f'INFO {line}', flush=True)

def all_apps(results, binary, wl_probe, captures):
  """Every application, one after the other."""
  for app in APPS:
    try:
      _group(app)(results, binary, wl_probe, captures)
    except (OSError, RuntimeError, TimeoutError, ConnectionError) as error:
      results.check(f'{app.name}: the scenario ran to the end', False,
                    str(error))
  for name, function in (('rofi', rofi_checks), ('dunst', dunst_checks)):
    try:
      _special(function)(results, binary, wl_probe, captures)
    except (OSError, RuntimeError, TimeoutError, ConnectionError) as error:
      results.check(f'{name}: the scenario ran to the end', False, str(error))

CHECKS = {'apps': all_apps}
CHECKS.update({f'app-{app.name}': _group(app) for app in APPS})
CHECKS['app-rofi'] = _special(rofi_checks)
CHECKS['app-dunst'] = _special(dunst_checks)
