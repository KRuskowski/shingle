"""WG-5's real-client checks of touchpad workspace swipes, run inside
run.py's namespace.

docs/design/wayfire-gaps.md, WG-5. Offscreen Shingle reads a synthetic
wayfire.ini that loads `vswipe` on a 3x1 grid, with a 100-pixel magenta
gap so that the wall shows in captures. Two `wl_probe --pattern`
clients map, one on each of the first two workspaces, and swipes come
through Shingle's own `stipc/feed_gesture` (decision 7), with the STIPC
pointer on the output:

- `vswipe`: a short four-finger swipe sends no `wset-workspace-changed`;
  a swipe held half way shows, in a `grim` capture, the first probe's
  right half left of the magenta gap and the second probe's left half
  right of it; lifting the fingers there switches to the second
  workspace, with one `wset-workspace-changed`; a long swipe back
  switches to the first, which a capture then shows unshifted; and a
  long three-finger swipe, the clients', switches nothing.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import time

import checks

xw = checks.xw
GROUPS = ('vswipe',)
INI = """[core]
plugins = vswipe vswitch simple-tile
xwayland = false
vwidth = 3
vheight = 1

[workarounds]
auto_reload_config = false

[simple-tile]
animation_duration = 0

[vswitch]
duration = 0

[vswipe]
gap = 100
background = 1.0 0.0 1.0 1.0

[autostart]
autostart_wf_shell = false
"""
# 12.8 pixels are 0.05 of a workspace at vswipe's default speed factor,
# the most one update moves; the first update only decides the direction.
STEP = 12.8
LONG = 20
SHORT = 5
# Updates that take the wall about half way: 0.05 each, slowed a little.
HALF = 12
# How long a check waits for an event that must not come.
QUIET_SECONDS = 1.0
# Longer than vswipe's default 180 ms slide, during which Wayfire, and
# Shingle, leave a new swipe to the clients.
SLIDE_SECONDS = 0.5

def gesture(shingle, phase, **data):
  """One `stipc/feed_gesture` step of a swipe."""
  reply = shingle.call('stipc/feed_gesture',
                       {'type': 'swipe', 'phase': phase, **data})
  if reply.get('result') != 'ok':
    raise RuntimeError(f'stipc/feed_gesture: {json.dumps(reply)}')

def swipe_updates(shingle, fingers, steps, dx):
  """Begins a swipe of `fingers` and moves it `steps` times by `dx`."""
  gesture(shingle, 'begin', fingers=fingers)
  for _ in range(steps):
    gesture(shingle, 'update', dx=dx, dy=0)

def lift(shingle, dx):
  """Two slow updates, so that the swipe is no flick, and the end."""
  for _ in range(2):
    gesture(shingle, 'update', dx=1.0 if dx > 0 else -1.0, dy=0)
  gesture(shingle, 'end', cancelled=False)

def swipe(shingle, fingers, steps, dx):
  """A whole swipe of `fingers`, `steps` updates of `dx`."""
  swipe_updates(shingle, fingers, steps, dx)
  lift(shingle, dx)

def next_change(watcher, timeout):
  """The next `wset-workspace-changed` event, or None."""
  try:
    event = watcher.receive(timeout)
  except TimeoutError:
    return None
  return event if event.get('event') == 'wset-workspace-changed' else None

def current_workspace(shingle):
  """The (x, y) of the focused output's current workspace."""
  workspace = shingle.call('window-rules/get-focused-output')['info'][
      'workspace']
  return workspace['x'], workspace['y']

def colour_runs(shot, y):
  """The runs of one classified colour along row `y` of `shot`, as
  [(colour or None, first x, last x)]."""
  runs = []
  for x in range(shot.width):
    colour = checks.classify(*shot.pixel(x, y))
    if runs and runs[-1][0] == colour:
      runs[-1] = (colour, runs[-1][1], x)
    else:
      runs.append((colour, x, x))
  return runs

def half_way_wall(shot, y_top, y_bottom, width):
  """Whether `shot` shows the wall about half way between two probes: a
  magenta gap in the middle third of rows `y_top` (the probes' top
  quadrants) and `y_bottom` (their bottom quadrants), with the first
  probe's right quadrants (green, white) left of it and the second's
  left quadrants (red, blue) right of it. Returns a description of what
  was found when it does not."""
  for y, left, right in ((y_top, 'green', 'red'),
                         (y_bottom, 'white', 'blue')):
    runs = [run for run in colour_runs(shot, y) if run[2] - run[1] >= 10]
    gaps = [index for index, run in enumerate(runs) if run[0] == 'magenta']
    if len(gaps) != 1:
      return f'row {y}: {runs}'
    index = gaps[0]
    gap = runs[index]
    if not width / 3 <= (gap[1] + gap[2]) / 2 <= 2 * width / 3:
      return f'row {y}: gap at {gap[1]}-{gap[2]}'
    before = runs[index - 1][0] if index > 0 else None
    after = runs[index + 1][0] if index + 1 < len(runs) else None
    if (before, after) != (left, right):
      return f'row {y}: {before} | gap | {after}, runs {runs}'
  return None

def probe(shingle, wl_probe, app_id):
  """A `wl_probe --pattern` client, and its view once mapped."""
  client = checks.Client(shingle, [wl_probe, app_id, '--pattern'], app_id,
                         program='wl_probe')
  view = xw.wait_for(lambda: shingle.view(app_id), timeout=15)
  return client, view

def swipe_checks(results, binary, wl_probe, captures):
  """Swipes over two probes on a 3x1 grid."""
  shingle = checks.Shingle(binary, 'vswipe', None, ini=INI)
  probes = []
  watcher = None
  try:
    output = shingle.call('window-rules/get-focused-output')['info']
    client, first = probe(shingle, wl_probe, 'first')
    probes.append(client)
    shingle.call('vswitch/set-workspace',
                 {'x': 1, 'y': 0, 'output-id': output['id']})
    client, second = probe(shingle, wl_probe, 'second')
    probes.append(client)
    shingle.call('vswitch/set-workspace',
                 {'x': 0, 'y': 0, 'output-id': output['id']})
    results.check('vswipe: two wl_probes map on workspaces 1 and 2',
                  first is not None and second is not None,
                  ' | '.join(checks.tail(probe.output()) for probe in probes))
    if first is None or second is None:
      return
    geometry = first['geometry']
    shot = captures.settled(
        shingle, lambda shot: not checks.pattern_mismatches(shot, geometry),
        stable=True)
    results.check('vswipe: the first workspace shows the first probe',
                  shot is not None
                  and not checks.pattern_mismatches(shot, geometry),
                  f'wrong {checks.pattern_mismatches(shot, geometry)}'
                  if shot else 'no capture')
    width = output['geometry']['width']
    shingle.call('stipc/move_cursor',
                 {'x': width // 2, 'y': output['geometry']['height'] // 2})
    watcher = xw.Ipc(shingle.ipc_path)
    watcher.call('window-rules/events/watch',
                 {'events': ['wset-workspace-changed']})

    swipe(shingle, 4, SHORT, -STEP)
    event = next_change(watcher, QUIET_SECONDS)
    results.check('vswipe: a short swipe sends no wset-workspace-changed',
                  event is None, json.dumps(event))

    swipe_updates(shingle, 4, HALF, -STEP)
    rows = (geometry['y'] + geometry['height'] // 4,
            geometry['y'] + geometry['height'] * 3 // 4)
    shot = captures.settled(
        shingle, lambda shot: half_way_wall(shot, *rows, width) is None)
    found = half_way_wall(shot, *rows, width) if shot else 'no capture'
    results.check('vswipe: grim shows the wall half way, the magenta gap '
                  'between the two probes', found is None, str(found))
    event = next_change(watcher, 0.3)
    results.check('vswipe: nothing switches while the fingers are down',
                  event is None, json.dumps(event))
    lift(shingle, -STEP)
    event = next_change(watcher, 5)
    time.sleep(SLIDE_SECONDS)
    results.check(
        'vswipe: lifting the fingers half way sends wset-workspace-changed '
        'to the second workspace',
        event is not None and event.get('new-workspace') == {'x': 1, 'y': 0}
        and event.get('previous-workspace') == {'x': 0, 'y': 0},
        json.dumps(event))

    swipe(shingle, 4, LONG, STEP)
    event = next_change(watcher, 5)
    results.check(
        'vswipe: a long swipe back sends wset-workspace-changed to the '
        'first workspace',
        event is not None and event.get('new-workspace') == {'x': 0, 'y': 0},
        json.dumps(event))
    time.sleep(SLIDE_SECONDS)
    workspace = current_workspace(shingle)
    shot = captures.settled(
        shingle, lambda shot: not checks.pattern_mismatches(shot, geometry),
        stable=True)
    results.check('vswipe: after the slide the first workspace is current '
                  'and its probe in place, unshifted',
                  workspace == (0, 0) and shot is not None
                  and not checks.pattern_mismatches(shot, geometry),
                  f'workspace {workspace}, wrong '
                  f'{checks.pattern_mismatches(shot, geometry)}'
                  if shot else 'no capture')

    swipe(shingle, 3, LONG, -STEP)
    event = next_change(watcher, QUIET_SECONDS)
    results.check('vswipe: a three-finger swipe, the clients\', switches '
                  'nothing', event is None and current_workspace(shingle)
                  == (0, 0), json.dumps(event))
  finally:
    if watcher is not None:
      watcher.close()
    for client in probes:
      client.stop()
    status = shingle.stop()
    results.check('vswipe: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {
    'vswipe': swipe_checks,
}
