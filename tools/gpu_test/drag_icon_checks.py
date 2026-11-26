"""WG-10's real-client check of Wayland drag icons, run inside run.py's
namespace.

docs/design/wayfire-gaps.md, WG-10. Offscreen Shingle on the render node
shows two floating `wl_probe` windows: `dragsrc` with `--drag TEXT
--drag-icon` and `droptgt` with `--drop`. Through STIPC's pointer the
group `drag-icon` presses the button on the source, which starts a drag
with a magenta 32x32 icon centred on the pointer, and moves it in steps
over the background and onto the target. `grim` captures must show the
icon centred on the pointer at each stop and not where it was before;
after the release the target reports the dropped text and a capture
shows no magenta anywhere.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import time

import checks

xw = checks.xw
GROUPS = ('drag-icon',)
TEXT = 'dragged with an icon'
ICON = 32
SOURCE_GEOMETRY = {'x': 100, 'y': 100, 'width': 400, 'height': 300}
TARGET_GEOMETRY = {'x': 750, 'y': 100, 'width': 400, 'height': 300}
# Where the drag starts, on the source, and its stops: over the
# background between the windows and below them, and on the target.
START = (300, 250)
STOPS = ((620, 250), (620, 550), (950, 250))

def config():
  """BASE_CONFIG with floating windows and no animations."""
  result = dict(checks.BASE_CONFIG)
  result['tiling'] = {'tile_by_default': 'none'}
  result['animate'] = {'open_animation': 'none', 'close_animation': 'none'}
  return result

def icon_at(shot, centre):
  """Whether the 32x32 icon centred on `centre` shows in `shot`: every
  point a few pixels inside its edges is magenta."""
  x, y = centre
  inset = ICON // 2 - 3
  points = [(x + dx, y + dy) for dx in (-inset, 0, inset)
            for dy in (-inset, 0, inset)]
  return all(checks.classify(*shot.pixel(px, py)) == 'magenta'
             for px, py in points)

def magenta_points(shot, step=8):
  """The points of a grid over `shot` that are magenta."""
  return [(x, y) for y in range(0, shot.height, step)
          for x in range(0, shot.width, step)
          if checks.classify(*shot.pixel(x, y)) == 'magenta']

def move(shingle, start, end, steps=8):
  """Moves the STIPC pointer from `start` to `end` in `steps`."""
  for step in range(1, steps + 1):
    shingle.call('stipc/move_cursor', {
        'x': start[0] + (end[0] - start[0]) * step // steps,
        'y': start[1] + (end[1] - start[1]) * step // steps,
    })
    time.sleep(0.02)

def place(shingle, app_id, geometry):
  """Floats `app_id`'s view at `geometry`; returns whether it got there."""
  view = shingle.view(app_id)
  shingle.call('window-rules/configure-view',
               {'id': view['id'], 'geometry': geometry})
  return xw.wait_for(
      lambda: (shingle.view(app_id) or {}).get('geometry') == geometry,
      timeout=10)

def drag_icon_checks(results, binary, wl_probe, captures):
  """A drag icon follows the pointer and goes with the drop."""
  shingle = checks.Shingle(binary, 'drag-icon', config())
  clients = []
  try:
    source = checks.Client(
        shingle, [wl_probe, 'dragsrc', '--drag', TEXT, '--drag-icon'],
        'dragsrc', program='wl_probe')
    clients.append(source)
    target = checks.Client(shingle, [wl_probe, 'droptgt', '--drop'],
                           'droptgt', program='wl_probe')
    clients.append(target)
    mapped = all(xw.wait_for(lambda app_id=app_id: shingle.view(app_id),
                             timeout=15) for app_id in ('dragsrc', 'droptgt'))
    placed = mapped and place(shingle, 'dragsrc', SOURCE_GEOMETRY) and place(
        shingle, 'droptgt', TARGET_GEOMETRY)
    results.check('drag-icon: the source and target probes map where IPC '
                  'places them', bool(placed),
                  f'{shingle.views()}')
    if not placed:
      return
    idle = captures.settled(shingle, lambda shot: not magenta_points(shot),
                            stable=True)
    results.check('drag-icon: no icon shows before the drag',
                  idle is not None and not magenta_points(idle),
                  str(magenta_points(idle)[:5]) if idle else 'no capture')
    shingle.call('stipc/move_cursor', {'x': START[0], 'y': START[1]})
    time.sleep(0.1)
    shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
    started = xw.wait_for(
        lambda: checks.probe_events(source.output(), 'drag_icon'), timeout=10)
    results.check('drag-icon: the press starts a drag with an icon',
                  bool(started), checks.tail(source.output()))
    previous = START
    for stop in STOPS:
      move(shingle, previous, stop)
      shot = captures.settled(
          shingle,
          lambda shot, stop=stop, before=previous: icon_at(shot, stop)
          and not icon_at(shot, before))
      results.check(
          f'drag-icon: grim shows the icon centred on the pointer at '
          f'{stop} and no longer at {previous}',
          shot is not None and icon_at(shot, stop)
          and not icon_at(shot, previous),
          f'at stop {shot.pixel(*stop) if shot else None}, '
          f'magenta {magenta_points(shot)[:6] if shot else None}')
      previous = stop
    entered = checks.probe_events(target.output(), 'drop_enter')
    results.check('drag-icon: the drag enters the target under the icon',
                  bool(entered), checks.tail(target.output()))
    shingle.call('stipc/feed_button',
                 {'combo': 'BTN_LEFT', 'mode': 'release'})
    dropped = xw.wait_for(
        lambda: [event for event in checks.probe_events(
            target.output(), 'dropped') if event.get('text') == TEXT],
        timeout=10)
    results.check('drag-icon: the target gets the dropped text',
                  bool(dropped), checks.tail(target.output()))
    shot = captures.settled(shingle, lambda shot: not magenta_points(shot),
                            stable=True)
    results.check(
        'drag-icon: after the drop grim shows no icon anywhere, and the '
        'target where it was',
        shot is not None and not magenta_points(shot)
        and checks.classify(*shot.pixel(*STOPS[-1])) == 'white',
        f'magenta {magenta_points(shot)[:6]}, at the drop '
        f'{shot.pixel(*STOPS[-1])}' if shot else 'no capture')
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    results.check('drag-icon: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {
    'drag-icon': drag_icon_checks,
}
