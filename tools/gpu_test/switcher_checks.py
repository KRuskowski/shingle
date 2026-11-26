"""WG-7's real-client checks of the Alt+Tab switcher's thumbnails, run
inside run.py's namespace.

docs/design/wayfire-gaps.md, WG-7. Offscreen Shingle reads a synthetic
wayfire.ini that loads `switcher` and `simple-tile`, and three
`wl_probe --pattern` clients tile side by side, mapped in the order
first, second, third, so that third is focused and first the least
recently. Keys come through STIPC:

- `switcher`: holding `alt` and tapping `tab` activates the `switcher`
  plugin (`plugin-activation-state-changed`) and, in a `grim` capture
  once the thumbnails settle, shows Wayfire's arrangement over black:
  the selected view's pattern in the middle of the output, scaled to fit
  45 % of it; the views to either side, a third of the output away,
  smaller, dimmer and turned, so that the edge nearer the middle is the
  shorter one; and the corners black. A second `tab` moves the selection
  on; releasing `alt` focuses the view it selected, first, and once the
  thumbnails are back the capture shows every probe in its tile and the
  plugin deactivates.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json

import checks

xw = checks.xw
GROUPS = ('switcher',)
INI = """[core]
plugins = switcher simple-tile
xwayland = false
vwidth = 1
vheight = 1

[workarounds]
auto_reload_config = false

[simple-tile]
animation_duration = 0

[autostart]
autostart_wf_shell = false
"""
# Wayfire's switcher.cpp: thumbnails fit 45 % of the output, the sides
# are a third of it away at 66 % of the size.
SCREEN_PERCENTAGE = 0.45
BACK_SCALE = 0.66
# A pixel at or below this in every channel is the black background.
BLACK = 30
# How long a check waits for an event that must not come, or for the
# plugin to deactivate after the 500 ms exit animation.
EVENT_SECONDS = 3.0

def thumbnail_rect(view, output, centre, scale=1.0):
  """The rectangle a thumbnail of `view`'s geometry takes around
  `centre`, at `scale` times the size that fits it into 45 % of the
  `output` ({'width', 'height'}), never enlarged."""
  geometry = view['geometry']
  fit = min(output['width'] * SCREEN_PERCENTAGE / geometry['width'],
            output['height'] * SCREEN_PERCENTAGE / geometry['height'], 1.0)
  width = round(geometry['width'] * fit * scale)
  height = round(geometry['height'] * fit * scale)
  return {'x': round(centre[0] - width / 2), 'y': round(centre[1] - height / 2),
          'width': width, 'height': height}

def dark(pixel):
  """Whether `pixel` is the black background."""
  return all(value <= BLACK for value in pixel)

def column_extent(shot, x, y):
  """How many pixels of column `x` around row `y` are not black."""
  if dark(shot.pixel(x, y)):
    return 0
  top = y
  while top > 0 and not dark(shot.pixel(x, top - 1)):
    top -= 1
  bottom = y
  while bottom < shot.height - 1 and not dark(shot.pixel(x, bottom + 1)):
    bottom += 1
  return bottom - top + 1

def side_problem(shot, rect, nearer):
  """Why the thumbnail at `rect` is not a side thumbnail turned with
  its `nearer` edge ('left' or 'right'), the one towards the middle,
  shorter; None when it is."""
  wrong = checks.pattern_mismatches(shot, shrunk(rect))
  if wrong:
    return f'pattern {wrong}'
  centre_y = rect['y'] + rect['height'] // 2
  quarter = rect['width'] // 4
  left = column_extent(shot, rect['x'] + quarter, centre_y)
  right = column_extent(shot, rect['x'] + rect['width'] - quarter, centre_y)
  shorter, longer = (left, right) if nearer == 'left' else (right, left)
  if not 0 < shorter < longer:
    return f'heights left {left}, right {right}'
  return None

def shrunk(rect):
  """`rect` at 60 % of its width around its centre, so that the
  quadrant points of a turned thumbnail fall inside it."""
  width = rect['width'] * 3 // 5
  return {'x': rect['x'] + (rect['width'] - width) // 2, 'y': rect['y'],
          'width': width, 'height': rect['height']}

def red_level(shot, rect):
  """The red channel at the centre of `rect`'s top left quadrant, which
  the pattern draws red."""
  (x, y), _ = checks.quadrant_points(rect)[0]
  return shot.pixel(x, y)[0]

def arrangement_problem(shot, views, output, centre_app, left_app, right_app):
  """Why `shot` does not show Wayfire's arrangement with `centre_app` in
  the middle, `left_app` and `right_app` beside it; None when it does."""
  width, height = output['width'], output['height']
  middle = (width / 2, height / 2)
  centre = thumbnail_rect(views[centre_app], output, middle)
  wrong = checks.pattern_mismatches(shot, centre)
  if wrong:
    return f'middle {centre}: {wrong}'
  corners = ((2, 2), (width - 3, 2), (2, height - 3),
             (width - 3, height - 3))
  for x, y in corners:
    if not dark(shot.pixel(x, y)):
      return f'corner ({x}, {y}) is {shot.pixel(x, y)}, not black'
  for app, sign, nearer in ((left_app, -1, 'right'), (right_app, 1, 'left')):
    rect = thumbnail_rect(views[app], output,
                          (middle[0] + sign * width / 3, middle[1]),
                          BACK_SCALE)
    found = side_problem(shot, rect, nearer)
    if found:
      return f'{app} at {rect}: {found}'
    if not red_level(shot, rect) < red_level(shot, centre):
      return f'{app} is not dimmer than the middle'
  return None

def probe(shingle, wl_probe, app_id):
  """A `wl_probe --pattern` client, and its view once mapped."""
  client = checks.Client(shingle, [wl_probe, app_id, '--pattern'], app_id,
                         program='wl_probe')
  view = xw.wait_for(lambda: shingle.view(app_id), timeout=15)
  return client, view

def key(shingle, name, pressed):
  """One `stipc/feed_key`."""
  reply = shingle.call('stipc/feed_key', {'key': name, 'state': pressed})
  if reply.get('result') != 'ok':
    raise RuntimeError(f'stipc/feed_key: {json.dumps(reply)}')

def tab(shingle):
  """A tap of `tab`."""
  key(shingle, 'KEY_TAB', True)
  key(shingle, 'KEY_TAB', False)

def activation(watcher, timeout):
  """The next `plugin-activation-state-changed` event, or None."""
  try:
    return watcher.receive(timeout)
  except TimeoutError:
    return None

def focused_app(shingle):
  """The app id of the focused view."""
  return shingle.call('window-rules/get-focused-view').get(
      'info', {}).get('app-id')

def switcher_checks(results, binary, wl_probe, captures):
  """Alt+Tab over three probes."""
  shingle = checks.Shingle(binary, 'switcher', None, ini=INI)
  probes = []
  watcher = None
  try:
    for app_id in ('first', 'second', 'third'):
      client, _ = probe(shingle, wl_probe, app_id)
      probes.append(client)
    views = {app: shingle.view(app) for app in ('first', 'second', 'third')}
    results.check('switcher: three wl_probes map and third is focused',
                  all(views.values()) and focused_app(shingle) == 'third',
                  ' | '.join(checks.tail(probe.output()) for probe in probes))
    if not all(views.values()):
      return
    output = shingle.call('window-rules/get-focused-output')['info'][
        'geometry']
    # The cursor out of the way, at the bottom edge between the slots.
    shingle.call('stipc/move_cursor',
                 {'x': output['width'] // 2, 'y': output['height'] - 1})

    def tiles(shot):
      return not any(checks.pattern_mismatches(shot, view['geometry'])
                     for view in views.values())
    shot = captures.settled(shingle, tiles, stable=True)
    results.check('switcher: grim shows the three probes tiled',
                  shot is not None and tiles(shot), 'no capture'
                  if shot is None else 'pattern mismatch')
    watcher = xw.Ipc(shingle.ipc_path)
    watcher.call('window-rules/events/watch',
                 {'events': ['plugin-activation-state-changed']})

    key(shingle, 'KEY_LEFTALT', True)
    tab(shingle)
    event = activation(watcher, EVENT_SECONDS)
    results.check('switcher: alt+tab activates the switcher plugin',
                  event is not None and event.get('plugin') == 'switcher'
                  and event.get('state') is True, json.dumps(event))

    def arranged(shot):
      return arrangement_problem(shot, views, output, 'second', 'third',
                                 'first') is None
    shot = captures.settled(shingle, arranged, stable=True)
    found = (arrangement_problem(shot, views, output, 'second', 'third',
                                 'first') if shot else 'no capture')
    results.check(
        'switcher: grim shows second in the middle over black, third to '
        'the left and first to the right, smaller, dimmer and turned '
        'towards it', found is None, str(found))
    results.check('switcher: the focus waits for alt to come up',
                  focused_app(shingle) == 'third', focused_app(shingle))

    tab(shingle)
    middle = (output['width'] / 2, output['height'] / 2)

    def centred(shot):
      return not checks.pattern_mismatches(
          shot, thumbnail_rect(views['first'], output, middle))
    shot = captures.settled(shingle, centred, stable=True)
    results.check('switcher: a second tab brings first to the middle',
                  shot is not None and centred(shot),
                  'no capture' if shot is None else 'pattern mismatch')

    key(shingle, 'KEY_LEFTALT', False)
    focused = xw.wait_for(lambda: focused_app(shingle) == 'first', timeout=5)
    results.check('switcher: releasing alt focuses the selected view, first',
                  focused, focused_app(shingle))
    event = activation(watcher, EVENT_SECONDS)
    results.check('switcher: once the thumbnails are back the plugin '
                  'deactivates', event is not None
                  and event.get('plugin') == 'switcher'
                  and event.get('state') is False, json.dumps(event))
    shot = captures.settled(shingle, tiles, stable=True)
    results.check('switcher: grim then shows every probe in its tile again',
                  shot is not None and tiles(shot),
                  'no capture' if shot is None else 'pattern mismatch')
  finally:
    if watcher is not None:
      watcher.close()
    for client in probes:
      client.stop()
    status = shingle.stop()
    results.check('switcher: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {
    'switcher': switcher_checks,
}
