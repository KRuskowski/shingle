"""WG-6's real-client checks of server-side frames, run inside run.py's
namespace.

docs/design/wayfire-gaps.md, WG-6. Offscreen Shingle reads a synthetic
wayfire.ini that loads `decoration`, with an opaque magenta frame for
the focused window and cyan for the others, so that frames show in
captures, and with new views floating. Two `wl_probe --pattern` clients
map, one asking for server-side decorations through xdg-decoration, one
for client-side, and IPC places them side by side. Input comes through
STIPC:

- `decoration`: the server-side probe is told `server` and the other
  `client`; IPC's `geometry` of the first is its frame, its
  `base-geometry` the frame less Wayfire's default margins, and its
  configure is that size, while the second's two are the same; `grim`
  shows the first's pattern inside a cyan frame and the second's with
  no frame around it; a click on the title focuses the first, whose
  frame turns magenta; a drag of the title bar moves it with the
  pointer from where the drag passed Wayfire's five-pixel threshold; the
  maximize button maximizes it to the workarea and restores it; the
  minimize button minimizes it; and the close button asks the probe to
  close, which then exits.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import time

import checks

xw = checks.xw
GROUPS = ('decoration',)
INI = """[core]
plugins = decoration simple-tile move place
xwayland = false
vwidth = 1
vheight = 1

[workarounds]
auto_reload_config = false

[simple-tile]
animation_duration = 0
tile_by_default = none

[decoration]
active_color = 1.0 0.0 1.0 1.0
inactive_color = 0.0 1.0 1.0 1.0

[autostart]
autostart_wf_shell = false
"""
# Wayfire's default frame: a 30-pixel title bar and 4-pixel borders,
# 21-pixel buttons with 4 pixels of padding on each side.
BORDER = 4
TITLE = 30
BUTTON = 21
PADDING = 4
# The buttons from the right, in Wayfire's default order.
CLOSE, MAXIMIZE, MINIMIZE = 0, 1, 2
# Where IPC puts the two probes' windows.
SSD_GEOMETRY = {'x': 100, 'y': 100, 'width': 400, 'height': 300}
CSD_GEOMETRY = {'x': 700, 'y': 100, 'width': 400, 'height': 300}
# A first motion past the frame's five-pixel threshold.
MOVE_START = 6
# Longer than the frame's 300 ms double click.
CLICK_GAP_SECONDS = 0.4

def margins_inside(geometry):
  """`geometry` less Wayfire's default frame."""
  return {
      'x': geometry['x'] + BORDER,
      'y': geometry['y'] + TITLE + BORDER,
      'width': geometry['width'] - 2 * BORDER,
      'height': geometry['height'] - TITLE - 2 * BORDER,
  }

def button_centre(geometry, index):
  """The centre of the button `index` places from the right of a frame
  at `geometry`, as Wayfire's `create_buttons` lays them out."""
  per_button = 2 * PADDING + BUTTON
  left = (geometry['x'] + geometry['width'] - BORDER + PADDING
          - (index + 1) * per_button)
  top = geometry['y'] + BORDER + (TITLE - BUTTON) // 2
  return left + BUTTON // 2, top + BUTTON // 2

def frame_colour(shot, geometry):
  """The colour of the frame's top border and title bar left of the
  title text, or the reason it is not one colour."""
  points = [(geometry['x'] + 1, geometry['y'] + 1),
            (geometry['x'] + geometry['width'] - 2, geometry['y'] + 1),
            (geometry['x'] + 1, geometry['y'] + geometry['height'] - 2),
            (geometry['x'] + geometry['width'] // 2,
             geometry['y'] + geometry['height'] - 2)]
  colours = {checks.classify(*shot.pixel(x, y)) for x, y in points}
  if len(colours) != 1:
    return f'frame pixels {[shot.pixel(x, y) for x, y in points]}'
  return colours.pop()

def unframed(shot, geometry):
  """Whether the pixels just outside `geometry` are the background, not
  a frame."""
  points = [(geometry['x'] - 2, geometry['y'] - 2),
            (geometry['x'] + geometry['width'] // 2, geometry['y'] - 10),
            (geometry['x'] + geometry['width'] + 1,
             geometry['y'] + geometry['height'] // 2)]
  return all(checks.classify(*shot.pixel(x, y)) not in ('magenta', 'cyan')
             for x, y in points)

def click(shingle, x, y):
  """A left click at (x, y) through STIPC."""
  shingle.call('stipc/move_cursor', {'x': x, 'y': y})
  shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
  shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'release'})
  time.sleep(CLICK_GAP_SECONDS)

def drag(shingle, start, end, steps=10):
  """A left drag from `start` to `end` through STIPC. The first motion
  passes the frame's five-pixel threshold and starts the move, which
  follows the pointer from there, as Wayfire's move plugin grabs the
  view where the pointer is when the move starts."""
  shingle.call('stipc/move_cursor', {'x': start[0], 'y': start[1]})
  shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
  shingle.call('stipc/move_cursor',
               {'x': start[0] + MOVE_START, 'y': start[1]})
  time.sleep(0.03)
  start = (start[0] + MOVE_START, start[1])
  for step in range(1, steps + 1):
    x = start[0] + (end[0] - start[0]) * step // steps
    y = start[1] + (end[1] - start[1]) * step // steps
    shingle.call('stipc/move_cursor', {'x': x, 'y': y})
    time.sleep(0.03)
  shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'release'})
  time.sleep(CLICK_GAP_SECONDS)

def probe(shingle, wl_probe, app_id, mode):
  """A `wl_probe --pattern --decoration MODE` client, and its view."""
  client = checks.Client(
      shingle, [wl_probe, app_id, '--pattern', '--decoration', mode], app_id,
      program='wl_probe')
  view = xw.wait_for(lambda: shingle.view(app_id), timeout=15)
  return client, view

def place(shingle, app_id, geometry):
  """Floats `app_id`'s view at `geometry` and returns it once there."""
  view = shingle.view(app_id)
  shingle.call('window-rules/configure-view',
               {'id': view['id'], 'geometry': geometry})
  return xw.wait_for(
      lambda: (lambda view: view if view and view['geometry'] == geometry
               else None)(shingle.view(app_id)), timeout=10)

def last_configure(client):
  """The size of the last configure a probe reported."""
  events = checks.probe_events(client.output(), 'configure')
  return (events[-1]['width'], events[-1]['height']) if events else None

def decoration_checks(results, binary, wl_probe, captures):
  """Frames of a server-side and a client-side probe, and the buttons."""
  shingle = checks.Shingle(binary, 'decoration', None, ini=INI)
  probes = []
  try:
    ssd_client, ssd = probe(shingle, wl_probe, 'ssd', 'server')
    probes.append(ssd_client)
    csd_client, csd = probe(shingle, wl_probe, 'csd', 'client')
    probes.append(csd_client)
    modes = [[event.get('mode') for event in
              checks.probe_events(client.output(), 'decoration')]
             for client in probes]
    results.check('decoration: the probes are told server and client',
                  ssd is not None and csd is not None
                  and modes[0][-1:] == ['server']
                  and modes[1][-1:] == ['client'],
                  f'modes {modes} | '
                  + ' | '.join(checks.tail(probe.output()) for probe in probes))
    if ssd is None or csd is None:
      return
    ssd = place(shingle, 'ssd', SSD_GEOMETRY)
    csd = place(shingle, 'csd', CSD_GEOMETRY)
    content = margins_inside(SSD_GEOMETRY)
    configured = xw.wait_for(
        lambda: last_configure(ssd_client) == (content['width'],
                                               content['height']),
        timeout=10)
    results.check(
        'decoration: IPC gives the frame as geometry and the client inside '
        'it as base-geometry, and the client is configured to that size',
        ssd is not None and ssd['base-geometry'] == content and configured,
        f'{json.dumps(ssd)}, configured {last_configure(ssd_client)}')
    results.check(
        'decoration: a client-side window\'s geometry is its client',
        csd is not None and csd['base-geometry'] == CSD_GEOMETRY
        and last_configure(csd_client) == (400, 300),
        f'{json.dumps(csd)}, configured {last_configure(csd_client)}')
    if ssd is None or csd is None:
      return

    def framed(colour):
      return lambda shot: (
          not checks.pattern_mismatches(shot, content)
          and not checks.pattern_mismatches(shot, CSD_GEOMETRY)
          and frame_colour(shot, SSD_GEOMETRY) == colour
          and unframed(shot, CSD_GEOMETRY))
    shot = captures.settled(shingle, framed('cyan'), stable=True)
    results.check(
        'decoration: grim shows the server-side probe inside a cyan frame '
        'and the client-side probe without one',
        shot is not None and framed('cyan')(shot),
        f'frame {frame_colour(shot, SSD_GEOMETRY)}, ssd wrong '
        f'{checks.pattern_mismatches(shot, content)}, csd wrong '
        f'{checks.pattern_mismatches(shot, CSD_GEOMETRY)}, csd unframed '
        f'{unframed(shot, CSD_GEOMETRY)}' if shot else 'no capture')

    title = (SSD_GEOMETRY['x'] + 150, SSD_GEOMETRY['y'] + 15)
    click(shingle, *title)
    shot = captures.settled(shingle, framed('magenta'))
    results.check('decoration: a click on the title focuses the window and '
                  'its frame turns magenta',
                  shot is not None and framed('magenta')(shot)
                  and shingle.view('ssd').get('activated'),
                  f'frame {frame_colour(shot, SSD_GEOMETRY)}, '
                  f'{json.dumps(shingle.view("ssd"))}' if shot
                  else 'no capture')

    drag(shingle, title, (title[0] + 100, title[1] + 50))
    moved = dict(SSD_GEOMETRY, x=SSD_GEOMETRY['x'] + 100 - MOVE_START,
                 y=SSD_GEOMETRY['y'] + 50)
    view = xw.wait_for(
        lambda: (lambda view: view if view['geometry'] == moved
                 else None)(shingle.view('ssd')), timeout=10)
    results.check('decoration: dragging the title bar moves the window '
                  'with the pointer once the drag passed five pixels',
                  view is not None,
                  json.dumps(shingle.view('ssd')))
    geometry = shingle.view('ssd')['geometry']

    click(shingle, *button_centre(geometry, MAXIMIZE))
    workarea = shingle.call('window-rules/get-focused-output')['info'][
        'workarea']
    maximized = xw.wait_for(
        lambda: shingle.view('ssd')['geometry'] == workarea, timeout=10)
    results.check('decoration: the maximize button maximizes the window '
                  'to the workarea', maximized,
                  f'{json.dumps(shingle.view("ssd"))}, workarea {workarea}')
    click(shingle, *button_centre(workarea, MAXIMIZE))
    restored = xw.wait_for(
        lambda: shingle.view('ssd')['geometry'] == geometry, timeout=10)
    results.check('decoration: clicking it again restores the window',
                  restored, json.dumps(shingle.view('ssd')))

    click(shingle, *button_centre(geometry, MINIMIZE))
    minimized = xw.wait_for(lambda: shingle.view('ssd')['minimized'],
                            timeout=10)
    results.check('decoration: the minimize button minimizes the window',
                  minimized, json.dumps(shingle.view('ssd')))
    shingle.call('wm-actions/set-minimized',
                 {'view_id': shingle.view('ssd')['id'], 'state': False})
    xw.wait_for(lambda: not shingle.view('ssd')['minimized'], timeout=10)

    click(shingle, *button_centre(geometry, CLOSE))
    status = ssd_client.exited(10)
    results.check('decoration: the close button asks the client to close, '
                  'and it exits',
                  status == 0
                  and checks.probe_events(ssd_client.output(), 'close'),
                  f'status {status}, {checks.tail(ssd_client.output())}')
  finally:
    for client in probes:
      client.stop()
    status = shingle.stop()
    results.check('decoration: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {
    'decoration': decoration_checks,
}
