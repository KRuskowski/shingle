"""The real-client checks of a view straddling two workspaces, run inside
run.py's namespace.

docs/design/wayfire-gaps.md, straddling views. Offscreen Shingle reads a
synthetic wayfire.ini with a 2x1 grid, new views floating, a 6 s vswitch
slide with a 100-pixel magenta gap, and Expo without a zoom, its
background magenta and its cells 40 pixels apart. A `wl_probe --pattern`
client, `over`, is placed by `configure-view` 200 pixels into the first
workspace's right edge and 100 past it, its centre on the first
workspace, so its green top-right quadrant is 50 pixels on the first
workspace and 100 on the second. A second probe, `here`, maps on the
second workspace. Input comes through STIPC:

- `straddle`: `grim` shows `over` to the first workspace's right edge;
  during the slide to the second workspace a capture shows the green
  quadrant cut at the first workspace's edge, the magenta gap, and the
  rest of it, 100 pixels, at the second workspace's left edge; after
  the slide the second workspace shows that part at its left edge; a
  click on it focuses `over` without switching workspace, and IPC keeps
  `over` on the first workspace; and Expo shows the part of the green
  quadrant in each workspace's cell with the magenta gap between them.
  Headless Wayfire (wayfire_reference.py) must show the same captures
  outside the slide and Expo, and the same focus after the click.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py and `Wayfire.stop`, to children of this process
inside the PID namespace.
"""

import json

import checks
import wayfire_reference

xw = checks.xw
GROUPS = ('straddle',)
COMMON = """xwayland = false
vwidth = 2
vheight = 1

[autostart]
autostart_wf_shell = false
"""
INI = """[core]
plugins = vswitch expo simple-tile move place
""" + COMMON + """
[workarounds]
auto_reload_config = false

[simple-tile]
animation_duration = 0
tile_by_default = none

[vswitch]
duration = 6000
gap = 100
background = 1.0 0.0 1.0 1.0

[expo]
duration = 0
offset = 40
background = 1.0 0.0 1.0 1.0
inactive_brightness = 1.0
"""
WAYFIRE_INI = """[core]
plugins = ipc ipc-rules stipc vswitch move place
""" + COMMON
# The probe's rectangle relative to the first workspace's right edge, and
# its height: 200 pixels on the first workspace, 100 on the second.
INSIDE = 200
WIDTH = 300
HEIGHT = 200
TOP = 100
# Where the probe's right column, green on its top row, falls on each
# side of the edge.
GREEN_HOME = WIDTH // 2 - (WIDTH - INSIDE)
GREEN_AWAY = WIDTH - INSIDE
# Runs shorter than this are edges and borders.
SHORT_RUN = 3

def colour_runs(shot, y):
  """The runs of one classified colour along row `y` of `shot`, as
  [(colour or None, first x, last x)], without runs shorter than
  SHORT_RUN."""
  runs = []
  for x in range(shot.width):
    colour = checks.classify(*shot.pixel(x, y))
    if runs and runs[-1][0] == colour:
      runs[-1] = (colour, runs[-1][1], x)
    else:
      runs.append((colour, x, x))
  return [run for run in runs if run[2] - run[1] + 1 >= SHORT_RUN]

def split_by_gap(shot, y, before, after):
  """Whether row `y` of `shot` shows the green quadrant cut by a magenta
  gap: a green run of `before` pixels, the gap, and a green run of
  `after` pixels, each within a tenth and two pixels. Returns None when
  it does, else what the row shows."""
  runs = colour_runs(shot, y)
  for index, run in enumerate(runs):
    if run[0] != 'magenta' or index == 0 or index + 1 == len(runs):
      continue
    left, right = runs[index - 1], runs[index + 1]
    lengths = (left[2] - left[1] + 1, right[2] - right[1] + 1)
    close = all(abs(length - wanted) <= wanted / 10 + 2
                for length, wanted in zip(lengths, (before, after)))
    if left[0] == right[0] == 'green' and close:
      return None
  return f'row {y}: {runs}'

def shows(shot, points):
  """Whether `shot` shows each (x, y) of `points` in its colour."""
  return all(checks.classify(*shot.pixel(x, y)) == colour
             for (x, y), colour in points)

def focused_app(compositor):
  """The app id of the focused view, or None."""
  reply = compositor.call('window-rules/get-focused-view')
  return (reply.get('info') or {}).get('app-id')

def current_workspace(compositor):
  """The (x, y) of the focused output's current workspace."""
  workspace = compositor.call('window-rules/get-focused-output')['info'][
      'workspace']
  return workspace['x'], workspace['y']

def place(compositor, app_id, x, y, width, height):
  """`configure-view` of `app_id` to the rectangle, relative to the
  current workspace; returns its view once its geometry is that, or
  None."""
  view = compositor.view(app_id)
  if view is None:
    return None
  wanted = {'x': x, 'y': y, 'width': width, 'height': height}
  reply = compositor.call('window-rules/configure-view',
                          {'id': view['id'], 'geometry': wanted})
  if reply.get('result') != 'ok':
    raise RuntimeError(f'configure-view: {json.dumps(reply)}')
  return xw.wait_for(
      lambda: (compositor.view(app_id) or {}).get('geometry') == wanted
      and compositor.view(app_id), timeout=10)

def click(compositor, x, y):
  """A left click at layout point (`x`, `y`) through STIPC."""
  compositor.call('stipc/move_cursor', {'x': x, 'y': y})
  compositor.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
  compositor.call('stipc/feed_button',
                  {'combo': 'BTN_LEFT', 'mode': 'release'})

def observe(compositor, wl_probe, captures, during_switch=None):
  """Places `over` across the first workspace's right edge, switches to
  the second, maps `here` there and clicks the part of `over` shown;
  returns what was seen, keyed by step, and the clients. With
  `during_switch`, calls it with the compositor just after the switch
  was asked for."""
  seen = {}
  clients = []
  output = compositor.call('window-rules/get-focused-output')['info']
  width = output['geometry']['width']
  row = TOP + HEIGHT // 4
  clients.append(checks.Client(compositor, [wl_probe, 'over', '--pattern'],
                               'over', program='wl_probe'))
  if xw.wait_for(lambda: compositor.view('over'), timeout=15) is None:
    return seen, clients
  over = place(compositor, 'over', width - INSIDE, TOP, WIDTH, HEIGHT)
  seen['placed'] = over is not None
  home = [((width - INSIDE + WIDTH // 4, row), 'red'),
          ((width - GREEN_HOME // 2, row), 'green')]
  shot = captures.settled(compositor, lambda shot: shows(shot, home))
  seen['home'] = shot is not None and shows(shot, home)
  compositor.call('vswitch/set-workspace',
                  {'x': 1, 'y': 0, 'output-id': output['id']})
  if during_switch is not None:
    during_switch(compositor)
  away = [((GREEN_AWAY // 2, row), 'green'),
          ((GREEN_AWAY + 50, row), 'black')]
  shot = captures.settled(compositor, lambda shot: shows(shot, away))
  seen['away'] = shot is not None and shows(shot, away)
  clients.append(checks.Client(compositor, [wl_probe, 'here', '--pattern'],
                               'here', program='wl_probe'))
  xw.wait_for(lambda: compositor.view('here')
              and focused_app(compositor) == 'here', timeout=15)
  place(compositor, 'here', width // 2, TOP, WIDTH, HEIGHT)
  seen['before-click'] = focused_app(compositor)
  click(compositor, GREEN_AWAY // 2, row)
  xw.wait_for(lambda: focused_app(compositor) == 'over', timeout=5)
  seen['after-click'] = (focused_app(compositor),
                         current_workspace(compositor))
  return seen, clients

# What Wayfire 0.9 shows, and what Shingle must show.
EXPECTED = {
    'placed': True,
    'home': True,
    'away': True,
    'before-click': 'here',
    'after-click': ('over', (1, 0)),
}

def straddle_checks(results, binary, wl_probe, captures):
  """A probe across two workspaces: in place, in the slide, on the
  neighbour, under a click and in Expo; then the same in Wayfire."""
  shingle = checks.Shingle(binary, 'straddle', None, ini=INI)
  clients = []
  seen = {}
  slide = {}

  def during_switch(compositor):
    row = TOP + HEIGHT // 4
    shot = captures.settled(
        compositor,
        lambda shot: split_by_gap(shot, row, GREEN_HOME, GREEN_AWAY) is None,
        timeout=5)
    slide['found'] = (split_by_gap(shot, row, GREEN_HOME, GREEN_AWAY)
                      if shot else 'no capture')
  try:
    seen, clients = observe(shingle, wl_probe, captures, during_switch)
    labels = {
        'placed': 'configure-view puts over across the first workspace\'s '
                  'right edge',
        'home': 'grim shows over to the first workspace\'s right edge',
        'away': 'after the slide the second workspace shows the rest of '
                'over at its left edge',
        'before-click': 'here maps focused on the second workspace',
        'after-click': 'a click on the part shown focuses over without '
                       'switching',
    }
    for step, label in labels.items():
      results.check(f'straddle: {label}', seen.get(step) == EXPECTED[step],
                    f'{seen.get(step)}, expected {EXPECTED[step]}')
    results.check('straddle: during the slide grim shows the green quadrant '
                  'cut at the first workspace\'s edge, the magenta gap and '
                  'its other 100 pixels on the second workspace',
                  slide.get('found') is None, str(slide.get('found')))
    over = shingle.view('over')
    workspace = None
    if over is not None:
      workspace = shingle.call('window-rules/view-info',
                               {'id': over['id']})['info'].get('geometry')
    results.check('straddle: IPC reports over where it is drawn, from the '
                  'second workspace',
                  workspace is not None
                  and workspace.get('x') == -INSIDE,
                  json.dumps(workspace))
    shingle.call('expo/toggle')

    def expo_split(shot):
      # Expo scales each workspace into its cell: the row of the green
      # quadrant and the lengths scale with it.
      cell = expo_scale(shingle)
      if cell is None:
        return 'no output'
      y = round(cell['y'] + (TOP + HEIGHT // 4) * cell['scale_y'])
      return split_by_gap(shot, y, GREEN_HOME * cell['scale_x'],
                          GREEN_AWAY * cell['scale_x'])
    shot = captures.settled(shingle, lambda shot: expo_split(shot) is None)
    row = expo_split(shot) if shot else 'no capture'
    results.check('straddle: Expo shows the green quadrant in both '
                  'workspaces\' cells, the magenta gap between them',
                  row is None, str(row))
    shingle.call('expo/toggle')
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    results.check('straddle: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')
  try:
    wayfire = wayfire_reference.Wayfire('straddle-wayfire', WAYFIRE_INI)
  except RuntimeError as error:
    print(f'INFO straddle: no Wayfire reference: {error}', flush=True)
    return
  clients = []
  try:
    reference, clients = observe(wayfire, wl_probe, captures)
  finally:
    for client in clients:
      client.stop()
    wayfire.stop()
  results.check('straddle: headless Wayfire shows over on both workspaces '
                'and focuses it on the click as Shingle does',
                reference == seen, f'Wayfire {reference}, Shingle {seen}')

def expo_scale(compositor):
  """Expo's first cell of the focused output, as Shingle lays it out
  (crates/shingle-frontend/src/expo.rs, `expo_cells`, offset 40 on a
  2x1 grid): its top and the scale of the workspace in it."""
  output = compositor.call('window-rules/get-focused-output')['info']
  geometry = output.get('geometry') or {}
  width, height = geometry.get('width'), geometry.get('height')
  if not width or not height:
    return None
  offset = 40
  cell_width = (width - 3 * offset) // 2
  cell_height = height - 2 * offset
  return {'y': offset, 'scale_x': cell_width / width,
          'scale_y': cell_height / height}

CHECKS = {
    'straddle': straddle_checks,
}
