"""Real-client checks of Wayfire's animate effects, run inside run.py's
namespace.

docs/design/wayfire-gaps.md, "animate as built". Offscreen Shingle reads
a synthetic wayfire.ini that loads `animate` alone, on a grey background,
with every animation slowed to seconds so that `grim` captures land in
the middle of them. One `wl_probe --pattern` client at a time maps,
named after what it checks, and the matchers and view properties pick
its animation:

- `animate`: the output starts black and fades in to the grey
  background (`startup_duration`); `fade_enabled_for` fades a probe in,
  with its quadrants between their colour and the grey, and out when it
  closes; `zoom_enabled_for` zooms one from a third of its size, the
  centre drawn and the quadrant centres still grey, and closing it while
  it opens turns the zoom around, so that it is gone in as long as it
  had run; `enabled_for` with `open_animation = zap` shows a probe first
  as a thin line through its middle; `fire_enabled_for` burns one in
  from the top, its top quadrants drawn, its bottom ones not, and fire
  coloured particles below them; the `close-animation-type` view
  property set over IPC to `spin` turns a closing probe, so that its top
  edge is no longer level; and `minimize-animation-type` set to
  `squeezimize` squeezes a minimized probe down towards the bottom of
  the output, its top quadrants already grey while its bottom centre is
  still drawn. Each check also waits for its animation to end with the
  view fully drawn or gone, before the next probe maps.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import time

import checks

xw = checks.xw
GROUPS = ('animate',)
# Lengths in milliseconds; long enough for a capture, which takes a few
# hundred, to land inside the phase a check looks for.
STARTUP_MS = 3000
FADE_MS = 6000
ZOOM_MS = 12000
ZAP_MS = 6000
FIRE_MS = 6000
SPIN_MS = 8000
SQUEEZE_MS = 6000
INI = f"""[core]
plugins = animate
xwayland = false
vwidth = 1
vheight = 1
background_color = \\#808080FF

[workarounds]
auto_reload_config = false

[animate]
enabled_for = app_id is "zap"
open_animation = zap
close_animation = zap
zap_duration = {ZAP_MS}ms linear
startup_duration = {STARTUP_MS}ms linear
fade_enabled_for = app_id is "fade"
fade_duration = {FADE_MS}ms linear
zoom_enabled_for = app_id is "zoom"
zoom_duration = {ZOOM_MS}ms linear
fire_enabled_for = app_id is "fire"
fire_duration = {FIRE_MS}ms linear

[autostart]
autostart_wf_shell = false
"""
GREY = (128, 128, 128)
# How far a channel may be from the grey and still be the background.
GREY_TOLERANCE = 12
SIZE = (480, 360)

def grey(pixel):
  """Whether `pixel` is the grey background."""
  return all(abs(value - 128) <= GREY_TOLERANCE for value in pixel)

def at(shot, x, y):
  """The pixel at (x, y), or the grey outside the capture."""
  if 0 <= x < shot.width and 0 <= y < shot.height:
    return shot.pixel(x, y)
  return GREY

def quadrants(shot, view):
  """The four quadrant centres of `view` in `shot`: [(colour or None,
  pixel)] in PATTERN order, top left first."""
  return [(checks.classify(*at(shot, x, y)), at(shot, x, y))
          for (x, y), _ in checks.quadrant_points(view['geometry'])]

# How far a channel of a settled view may be from its pattern colour.
PATTERN_TOLERANCE = 8
COLOURS = {'red': (255, 0, 0), 'green': (0, 255, 0), 'blue': (0, 0, 255),
           'white': (255, 255, 255)}

def pattern_shown(shot, view):
  """Whether every quadrant shows its colour, fully opaque."""
  expected = [colour for row in checks.PATTERN for colour in row]
  return all(
      all(abs(value - want) <= PATTERN_TOLERANCE
          for value, want in zip(pixel, COLOURS[colour]))
      for colour, (_, pixel) in zip(expected, quadrants(shot, view)))

def view_gone(shot, view):
  """Whether nothing of `view` is left: a 9 by 9 grid over its geometry,
  its centre included, shows the grey."""
  geometry = view['geometry']
  return all(
      grey(at(shot, geometry['x'] + geometry['width'] * column // 8,
              geometry['y'] + geometry['height'] * row // 8))
      for column in range(1, 8) for row in range(1, 8))

def faded(shot, view):
  """Whether the red quadrant is between red and the grey: partly
  transparent."""
  (_, red), = quadrants(shot, view)[:1]
  return (checks.classify(*red) is None and not grey(red)
          and red[0] > red[1] + 20 and red[0] > red[2] + 20)

def centre(view):
  """The centre of `view`'s geometry."""
  geometry = view['geometry']
  return (geometry['x'] + geometry['width'] // 2,
          geometry['y'] + geometry['height'] // 2)

def zoomed(shot, view):
  """Whether `view` is drawn small about its centre: its quadrant
  centres grey, a pixel near its centre not."""
  x, y = centre(view)
  near = [at(shot, x + dx, y + dy) for dx in (-8, 8) for dy in (-8, 8)]
  return (all(grey(pixel) for _, pixel in quadrants(shot, view))
          and sum(1 for pixel in near if not grey(pixel)) >= 3)

def zap_line(shot, view):
  """Whether `view` is a thin line through its middle: more than a third
  of its width drawn on the centre row, and nothing 20 rows above and
  below."""
  geometry = view['geometry']
  x0, width = geometry['x'], geometry['width']
  _, y = centre(view)
  row = [at(shot, x, y) for x in range(x0, x0 + width, 4)]
  drawn = sum(1 for pixel in row if not grey(pixel))
  clear = all(grey(at(shot, x, y + dy))
              for x in range(x0, x0 + width, 8) for dy in (-20, 20))
  return drawn * 4 > width / 3 and clear

def fire_pixel(pixel):
  """Whether `pixel` has the fire's orange over the grey."""
  r, g, b = pixel
  return r > b + 40 and r > g + 15

def burning(shot, view):
  """Whether `view` is drawn down to a line between its top and bottom
  quadrants with fire below it: top quadrants shown, bottom ones not,
  fire coloured pixels in its bottom half."""
  shown = quadrants(shot, view)
  top = [colour for colour, _ in shown[:2]] == list(checks.PATTERN[0])
  bottom = all(colour not in ('blue', 'white') for colour, _ in shown[2:])
  geometry = view['geometry']
  fire = any(
      fire_pixel(at(shot, x, y))
      for x in range(geometry['x'], geometry['x'] + geometry['width'], 6)
      for y in range(geometry['y'] + geometry['height'] // 2,
                     geometry['y'] + geometry['height'], 6))
  return top and bottom and fire

def top_edge(shot, view, x):
  """The first row from above `view` down to its middle that is not grey
  at column `x`, or None."""
  geometry = view['geometry']
  for y in range(max(0, geometry['y'] - geometry['height'] // 2),
                 geometry['y'] + geometry['height'] // 2):
    if not grey(at(shot, x, y)):
      return y
  return None

def turned(shot, view):
  """Whether `view`'s top edge is no longer level: it starts more than
  8 rows apart at a quarter and three quarters of its width."""
  geometry = view['geometry']
  left = top_edge(shot, view, geometry['x'] + geometry['width'] // 4)
  right = top_edge(shot, view, geometry['x'] + geometry['width'] * 3 // 4)
  return left is not None and right is not None and abs(left - right) > 8

def squeezed(shot, view):
  """Whether `view` is going down: its top quadrants grey while
  something is still drawn at its bottom centre."""
  shown = quadrants(shot, view)
  x, _ = centre(view)
  geometry = view['geometry']
  bottom = geometry['y'] + geometry['height']
  below = any(not grey(at(shot, x + dx, y))
              for dx in (-20, 0, 20)
              for y in range(bottom - geometry['height'] // 6, bottom, 4))
  return all(grey(pixel) for _, pixel in shown[:2]) and below

def startup_dark(shot):
  """Whether the output's middle is darker than the grey: the start's
  black, fading."""
  r, g, b = shot.pixel(shot.width // 2, shot.height // 2)
  return max(r, g, b) < 128 - GREY_TOLERANCE

def probe(shingle, wl_probe, app_id):
  """A `wl_probe --pattern` client of SIZE, and its view once mapped."""
  client = checks.Client(
      shingle, [wl_probe, app_id, '--pattern', '--size',
                f'{SIZE[0]}x{SIZE[1]}'], app_id, program='wl_probe')
  view = xw.wait_for(lambda: shingle.view(app_id), timeout=15)
  return client, view

def describe(shot, view):
  """What a check saw, for its message."""
  if shot is None:
    return 'no capture'
  return f'quadrants {[pixel for _, pixel in quadrants(shot, view)]}'

def animate_checks(results, binary, wl_probe, captures):
  """Every animation, one probe at a time."""
  shingle = checks.Shingle(binary, 'animate', None, ini=INI)
  clients = []
  try:
    # The output waits for a background that never comes, then fades
    # in from black over STARTUP_MS.
    shot = captures.take(shingle)
    results.check('animate: the output starts dark and fades in',
                  shot is not None and startup_dark(shot),
                  str(shot.pixel(shot.width // 2, shot.height // 2))
                  if shot else 'no capture')
    time.sleep(STARTUP_MS / 1000)
    shot = captures.settled(
        shingle, lambda shot: grey(shot.pixel(shot.width // 2,
                                              shot.height // 2)),
        stable=True)
    results.check('animate: after startup_duration the grey background '
                  'shows', shot is not None
                  and grey(shot.pixel(shot.width // 2, shot.height // 2)),
                  str(shot.pixel(shot.width // 2, shot.height // 2))
                  if shot else 'no capture')

    # fade: in, then out.
    client, view = probe(shingle, wl_probe, 'fade')
    clients.append(client)
    shot = captures.settled(shingle, lambda shot: faded(shot, view),
                            timeout=FADE_MS / 2000)
    results.check('animate: fade_enabled_for fades a view in',
                  shot is not None and faded(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: pattern_shown(shot, view),
                            timeout=FADE_MS / 1000 + 5, stable=True)
    results.check('animate: the faded view ends fully drawn',
                  shot is not None and pattern_shown(shot, view),
                  describe(shot, view))
    client.stop()
    clients.remove(client)
    shot = captures.settled(shingle, lambda shot: faded(shot, view),
                            timeout=FADE_MS / 2000)
    results.check('animate: and fades it out when it closes',
                  shot is not None and faded(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: view_gone(shot, view),
                            timeout=FADE_MS / 1000 + 5, stable=True)
    results.check('animate: until it is gone',
                  shot is not None and view_gone(shot, view),
                  describe(shot, view))

    # zoom: from a third of the size; closed half way it goes back.
    client, view = probe(shingle, wl_probe, 'zoom')
    clients.append(client)
    opened = time.monotonic()
    shot = captures.settled(shingle, lambda shot: zoomed(shot, view),
                            timeout=ZOOM_MS / 4000)
    results.check('animate: zoom_enabled_for zooms a view in from a third '
                  'of its size', shot is not None and zoomed(shot, view),
                  describe(shot, view))
    client.stop()
    clients.remove(client)
    ran = time.monotonic() - opened
    shot = captures.take(shingle)
    results.check('animate: closing it while it opens zooms it back out '
                  'from where it was',
                  shot is not None and zoomed(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: view_gone(shot, view),
                            timeout=ran + 3)
    gone_after = time.monotonic() - opened - ran
    # Gone in as long as it ran, not in the rest of ZOOM_MS.
    results.check('animate: and is gone in about as long as it ran',
                  shot is not None and view_gone(shot, view)
                  and gone_after < ran + 3,
                  f'{gone_after:.1f} s after {ran:.1f} s, '
                  f'{describe(shot, view)}')

    # zap: a line first, from open_animation.
    client, view = probe(shingle, wl_probe, 'zap')
    clients.append(client)
    shot = captures.settled(shingle, lambda shot: zap_line(shot, view),
                            timeout=ZAP_MS / 1000)
    results.check('animate: open_animation zap shows the view as a line '
                  'through its middle first',
                  shot is not None and zap_line(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: pattern_shown(shot, view),
                            timeout=ZAP_MS / 1000 + 5, stable=True)
    results.check('animate: the zapped view ends fully drawn',
                  shot is not None and pattern_shown(shot, view),
                  describe(shot, view))

    # spin: chosen for its close by a view property over IPC.
    view_id = view['id']
    for name, value in (('close-animation-type', 'spin'),
                        ('close-animation-duration', f'{SPIN_MS}ms linear')):
      reply = shingle.call('window-rules/set-view-property',
                           {'id': view_id, 'property': name,
                            'value': value})
      results.check(f'animate: set-view-property {name} is accepted',
                    reply.get('result') == 'ok', str(reply))
    client.stop()
    clients.remove(client)
    shot = captures.settled(shingle, lambda shot: turned(shot, view),
                            timeout=SPIN_MS / 4000)
    results.check('animate: close-animation-type spin turns the closing '
                  'view', shot is not None and turned(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: view_gone(shot, view),
                            timeout=SPIN_MS / 1000 + 5, stable=True)
    results.check('animate: until it is gone',
                  shot is not None and view_gone(shot, view),
                  describe(shot, view))

    # fire: burns in from the top.
    client, view = probe(shingle, wl_probe, 'fire')
    clients.append(client)
    shot = captures.settled(shingle, lambda shot: burning(shot, view),
                            timeout=FIRE_MS / 1000)
    results.check('animate: fire_enabled_for draws the view down to a '
                  'line with fire below it',
                  shot is not None and burning(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: pattern_shown(shot, view),
                            timeout=FIRE_MS / 1000 * 3 + 5, stable=True)
    results.check('animate: the burnt-in view ends fully drawn and the '
                  'particles gone', shot is not None
                  and pattern_shown(shot, view), describe(shot, view))

    # squeezimize: chosen for the minimize by view properties.
    for name, value in (('minimize-animation-type', 'squeezimize'),
                        ('minimize-animation-duration',
                         f'{SQUEEZE_MS}ms linear')):
      shingle.call('window-rules/set-view-property',
                   {'id': view['id'], 'property': name, 'value': value})
    reply = shingle.call('wm-actions/set-minimized',
                         {'view_id': view['id'], 'state': True})
    results.check('animate: wm-actions/set-minimized minimizes the view',
                  reply.get('result') == 'ok', str(reply))
    shot = captures.settled(shingle, lambda shot: squeezed(shot, view),
                            timeout=SQUEEZE_MS / 1000)
    results.check('animate: minimize-animation-type squeezimize squeezes '
                  'the view down towards the bottom of the output',
                  shot is not None and squeezed(shot, view),
                  describe(shot, view))
    shot = captures.settled(shingle, lambda shot: view_gone(shot, view),
                            timeout=SQUEEZE_MS / 1000 + 5, stable=True)
    results.check('animate: until it is gone',
                  shot is not None and view_gone(shot, view),
                  describe(shot, view))
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    results.check('animate: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {
    'animate': animate_checks,
}
