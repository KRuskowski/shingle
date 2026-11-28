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
- Whole views (docs/design/wayfire-gaps.md, "Whole views as built"): a
  `wl_probe --pattern --subsurface --popup --decoration server` probe
  has a yellow subsurface at its bottom left, a magenta popup at its top
  left and a black server-side frame. Each effect transforms the
  subsurface and the frame with the view and leaves the popup, a view
  of its own, as it is: zoom draws the subsurface zoomed, faint, and
  not at its place, with the popup at its own place in full; fade
  draws the subsurface faded; zap draws no subsurface while the view is
  a line; a spinning close draws the frame and the subsurface turning
  with it; fire draws the frame's top above its line and the
  subsurface not yet; squeezimize draws the frame with the view and
  the subsurface warped away from its place. Without whole views the
  subsurface stayed at its place, in full, or went at once, and the
  frame was not drawn while the view turned, burnt or was squeezed.

- `animate-cost`: what whole views cost. Offscreen Shingle on one
  3840x2160 output with `SHINGLE_TRACE_TURNS=0`, which logs every turn,
  tiles eight `wl_probe --pattern` clients and closes them all at once
  with zoom, spin and fire, once as views of one surface each, which
  draw from their surfaces' textures, and once with a subsurface and a
  server-side frame each, which are drawn into textures of their own
  first; and with no close animation. Then it opens eight at once in
  the same ways, where a view of several pieces is drawn into its
  texture every frame, and last lets eight windows of each kind draw
  frames all the time without any animation. It reports the render
  turns' lengths while the animations run or the windows draw, which on
  the offscreen backend include waiting for the GPU, and checks that
  each run drew frames.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import re
import signal
import statistics
import time

import checks

xw = checks.xw
GROUPS = ('animate', 'animate-cost')
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
plugins = animate decoration
xwayland = false
vwidth = 1
vheight = 1
background_color = \\#808080FF

[workarounds]
auto_reload_config = false

[animate]
enabled_for = app_id is "zap" | app_id is "pieces-zap"
open_animation = zap
close_animation = zap
zap_duration = {ZAP_MS}ms linear
startup_duration = {STARTUP_MS}ms linear
fade_enabled_for = app_id is "fade" | app_id is "pieces-fade"
fade_duration = {FADE_MS}ms linear
zoom_enabled_for = app_id is "zoom" | app_id is "pieces-zoom"
zoom_duration = {ZOOM_MS}ms linear
fire_enabled_for = app_id is "fire" | app_id is "pieces-fire"
fire_duration = {FIRE_MS}ms linear

[decoration]
active_color = \\#000000FF
inactive_color = \\#000000FF
border_size = 12

[autostart]
autostart_wf_shell = false
"""
GREY = (128, 128, 128)
# wl_probe's --subsurface.
SUBSURFACE_SIZE = (80, 60)
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

def yellow(r, g, b):
  """The subsurface's yellow, in full."""
  return r > 200 and g > 200 and b < 60

def yellowish(r, g, b):
  """The subsurface's yellow over the grey, faded or in full."""
  return r > b + 40 and g > b + 40 and abs(r - g) < 30

def magenta(r, g, b):
  """The popup's magenta, in full."""
  return r > 200 and b > 200 and g < 60

def dark(r, g, b):
  """The frame's black over the grey at half its opacity or more, which
  Shingle blends in linear light: a dark grey, which no colour of the
  pattern faded over the grey is."""
  return max(r, g, b) < 100 and max(r, g, b) - min(r, g, b) < 8

def title_bar(view):
  """The rectangle of `view`'s frame above its content."""
  geometry, content = view['geometry'], view['base-geometry']
  return {'x': geometry['x'], 'y': geometry['y'],
          'width': geometry['width'],
          'height': max(0, content['y'] - geometry['y'])}

def around(view, margin=60):
  """The rectangle `margin` pixels around `view`'s geometry, frame and
  subsurface included."""
  geometry = view['geometry']
  return {'x': geometry['x'] - margin, 'y': geometry['y'] - margin,
          'width': geometry['width'] + 2 * margin,
          'height': geometry['height'] + 2 * margin}

def box_centre(box):
  """The centre of a (left, top, right, bottom) box."""
  left, top, right, bottom = box
  return ((left + right) // 2, (top + bottom) // 2)

def pieces_probe(shingle, wl_probe, app_id):
  """A probe of several pieces: subsurface, popup and frame; its view
  once mapped and its popup drawn."""
  client = checks.Client(
      shingle, [wl_probe, app_id, '--pattern', '--subsurface', '--popup',
                '--decoration', 'server', '--size', f'{SIZE[0]}x{SIZE[1]}'],
      app_id, program='wl_probe')
  view = xw.wait_for(lambda: shingle.view(app_id), timeout=15)
  xw.wait_for(lambda: '"popup"' in client.output(), timeout=15)
  return client, view

def pieces_settled(shingle, captures, view, timeout):
  """The capture once `view` is fully drawn with its pieces, and the
  boxes of its subsurface and popup in it, or Nones."""
  area = around(view)
  full = SUBSURFACE_SIZE[0] * SUBSURFACE_SIZE[1] * 9 // 10
  # The subsurface may cover a quadrant's centre, so the pieces' colours
  # in full, unchanged between two captures, tell that it settled.
  shot = captures.settled(
      shingle, lambda shot: shot.matching(yellow, area)[0] >= full
      and shot.matching(magenta, area)[0] >= full, timeout=timeout,
      stable=True)
  if shot is None:
    return None, None, None
  return shot, shot.matching(yellow, area)[1], shot.matching(magenta, area)[1]

def pieces_checks(results, shingle, wl_probe, captures, clients):
  """Every effect on a view of several pieces."""
  # zoom: the subsurface zooms with the view, the popup stays.
  client, view = pieces_probe(shingle, wl_probe, 'pieces-zoom')
  clients.append(client)
  area = around(view)
  time.sleep(ZOOM_MS / 3000)
  zooming = captures.take(shingle)
  settled, sub, popup = pieces_settled(shingle, captures, view,
                                       ZOOM_MS / 1000 + 5)
  results.check('animate: a view of several pieces settles with its '
                'subsurface and popup', sub is not None and popup is not None,
                describe(settled, view))
  if zooming is None or sub is None or popup is None:
    return
  faint = zooming.matching(yellowish, area)[0]
  results.check('animate: zoom draws the subsurface zoomed with its view, '
                'not at its place',
                faint > 100 and not yellow(*zooming.pixel(*box_centre(sub))),
                f'{faint} yellowish pixels, '
                f'{zooming.pixel(*box_centre(sub))} at {box_centre(sub)}')
  results.check('animate: the popup, a view of its own, is drawn at its '
                'place in full', magenta(*zooming.pixel(*box_centre(popup))),
                f'{zooming.pixel(*box_centre(popup))} at {box_centre(popup)}')

  # spin: the closing view turns with its frame and subsurface.
  for name, value in (('close-animation-type', 'spin'),
                      ('close-animation-duration', f'{SPIN_MS}ms linear')):
    shingle.call('window-rules/set-view-property',
                 {'id': view['id'], 'property': name, 'value': value})
  client.stop()
  clients.remove(client)

  def spinning(shot):
    return (turned(shot, view) and shot.matching(dark, area)[0] > 100
            and shot.matching(yellowish, area)[0] > 100)
  shot = captures.settled(shingle, spinning, timeout=SPIN_MS / 2000)
  frame = shot.matching(dark, area)[0] if shot else 0
  turning = shot.matching(yellowish, area)[0] if shot else 0
  results.check('animate: a spinning close draws the frame and the '
                'subsurface turning with the view',
                shot is not None and spinning(shot),
                f'{frame} frame and {turning} subsurface pixels')
  shot = captures.settled(shingle, lambda shot: view_gone(shot, view),
                          timeout=SPIN_MS / 1000 + 5, stable=True)
  results.check('animate: the view of several pieces is gone after its '
                'close', shot is not None and view_gone(shot, view)
                and shot.matching(yellowish, area)[0] == 0,
                describe(shot, view))

  # fade: the subsurface fades with the view.
  client, view = pieces_probe(shingle, wl_probe, 'pieces-fade')
  clients.append(client)
  time.sleep(FADE_MS / 3000)
  fading = captures.take(shingle)
  settled, sub, popup = pieces_settled(shingle, captures, view,
                                       FADE_MS / 1000 + 5)
  if fading is not None and sub is not None:
    pixel = fading.pixel(*box_centre(sub))
    results.check('animate: fade fades the subsurface with its view',
                  yellowish(*pixel) and not yellow(*pixel), str(pixel))
  else:
    results.check('animate: fade fades the subsurface with its view', False,
                  describe(settled, view))
  client.stop()
  clients.remove(client)
  captures.settled(shingle, lambda shot: view_gone(shot, view),
                   timeout=FADE_MS / 1000 + 5, stable=True)

  # zap: a line through the middle, without the subsurface below it.
  client, view = pieces_probe(shingle, wl_probe, 'pieces-zap')
  clients.append(client)
  area = around(view)
  time.sleep(ZAP_MS / 2000)
  line = captures.take(shingle)
  settled, sub, popup = pieces_settled(shingle, captures, view,
                                       ZAP_MS / 1000 + 5)
  pixel = line.pixel(*box_centre(sub)) if line and sub else None
  results.check('animate: zap draws no subsurface at its place while the '
                'view is a line', pixel is not None and not yellowish(*pixel),
                f'{pixel} at the subsurface')
  client.stop()
  clients.remove(client)
  captures.settled(shingle, lambda shot: view_gone(shot, view),
                   timeout=ZAP_MS / 1000 + 5, stable=True)

  # fire: the frame's top above the line, the subsurface below it not
  # yet; then squeezimize warps the view with both.
  client, view = pieces_probe(shingle, wl_probe, 'pieces-fire')
  clients.append(client)
  area = around(view)
  time.sleep(FIRE_MS / 4000)
  burning_shot = captures.take(shingle)
  settled, sub, popup = pieces_settled(shingle, captures, view,
                                       FIRE_MS / 1000 * 3 + 5)
  if burning_shot is not None and sub is not None:
    # The title bar's black where the particles' glow leaves it; the
    # particles alone are never a neutral dark grey.
    bar = title_bar(view)
    frame = burning_shot.matching(dark, bar)[0]
    pixel = burning_shot.pixel(*box_centre(sub))
    results.check('animate: fire draws the frame\'s top above its line and '
                  'the subsurface below it not yet',
                  frame * 20 > bar['width'] * bar['height']
                  and not yellowish(*pixel),
                  f'{frame} frame pixels in {bar}, {pixel} at the subsurface')
  else:
    results.check('animate: fire draws the frame\'s top above its line and '
                  'the subsurface below it not yet', False,
                  describe(settled, view))
  for name, value in (('minimize-animation-type', 'squeezimize'),
                      ('minimize-animation-duration',
                       f'{SQUEEZE_MS}ms linear')):
    shingle.call('window-rules/set-view-property',
                 {'id': view['id'], 'property': name, 'value': value})
  shingle.call('wm-actions/set-minimized',
               {'view_id': view['id'], 'state': True})
  time.sleep(SQUEEZE_MS / 3000)
  shot = captures.take(shingle)
  if shot is not None and settled is not None and sub is not None:
    frame = shot.matching(dark, area)[0]
    count, box = shot.matching(yellow, area)
    full = settled.matching(yellow, area)[0]
    results.check('animate: squeezimize warps the frame and the subsurface '
                  'with the view', frame > 100 and box != sub
                  and count != full,
                  f'{frame} frame pixels, subsurface {count} pixels in '
                  f'{box}, settled {full} in {sub}')
  else:
    results.check('animate: squeezimize warps the frame and the subsurface '
                  'with the view', False, describe(shot, view))
  shot = captures.settled(shingle, lambda shot: view_gone(shot, view)
                          and shot.matching(yellowish, area)[0] == 0,
                          timeout=SQUEEZE_MS / 1000 + 5, stable=True)
  results.check('animate: the squeezed view is gone with its pieces',
                shot is not None and view_gone(shot, view),
                describe(shot, view))

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

    pieces_checks(results, shingle, wl_probe, captures, clients)
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    results.check('animate: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

# The cost runs: how many windows close at once, on what output, over
# how long.
COST_WINDOWS = 8
COST_OUTPUT = {'name': 'HEADLESS-1', 'mode': '3840x2160@60'}
COST_MS = 1500
# A render turn as SHINGLE_TRACE_TURNS logs it: its length and start.
TURN = re.compile(r'turn render ([0-9.]+) ms at ([0-9.]+)')

def render_turns(log, start, end):
  """The lengths in milliseconds of the render turns `log` shows begun
  between the monotonic times `start` and `end`."""
  return [float(length) for length, begun in TURN.findall(log)
          if start <= float(begun) <= end]

def summary(lengths):
  """The count, median, 95th percentile and maximum of `lengths`."""
  if not lengths:
    return 'no frames'
  ordered = sorted(lengths)
  p95 = ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))]
  return (f'{len(ordered)} frames, median '
          f'{statistics.median(ordered):.2f} ms, p95 {p95:.2f} ms, max '
          f'{ordered[-1]:.2f} ms')

def cost_config(effect, pieces, opening=False):
  """The YAML config, as JSON, of a cost run: closes, or with `opening`
  opens, with `effect` over COST_MS, frames on every window with
  `pieces`."""
  config = json.loads(json.dumps(checks.BASE_CONFIG))
  config['outputs'] = [dict(COST_OUTPUT)]
  effect = 'none' if effect == 'steady' else effect
  config['animate'] = {
      'open_animation': effect if opening else 'none',
      'close_animation': 'none' if opening else effect,
      'duration_ms': COST_MS, 'spin_duration_ms': COST_MS,
      'startup_duration_ms': 0}
  config['tiling'] = {'animation_duration': 0}
  config['decoration'] = {'forced_views': 'all' if pieces else 'none'}
  return config

def cost_run(results, binary, wl_probe, effect, pieces, opening=False):
  """Closes COST_WINDOWS windows at once with `effect`, or with
  `opening` opens them at once, and returns the render turns' lengths
  until the animations ended."""
  kind = 'pieces' if pieces else 'plain'
  which = 'opening' if opening else 'closing'
  if effect == 'steady':
    which = 'drawing'
  name = f'animate-cost-{which}-{effect}-{kind}'
  shingle = checks.Shingle(binary, name,
                           cost_config(effect, pieces, opening),
                           extra_environment={'SHINGLE_TRACE_TURNS': '0'})
  clients = []
  # Fire lasts up to three times longer and until its particles died.
  length = COST_MS * (4 if effect == 'fire' else 1) / 1000 + 0.5
  steady = effect == 'steady'
  try:
    extra = ['--subsurface'] if pieces else []
    if steady:
      # Every client draws frames all the time.
      extra += ['--animate', '100000']
    start = time.monotonic()
    for index in range(COST_WINDOWS):
      app_id = f'cost-{index}'
      clients.append(checks.Client(
          shingle, [wl_probe, app_id, '--pattern', *extra], app_id,
          program='wl_probe'))
    mapped = xw.wait_for(
        lambda: len([view for view in shingle.views()
                     if view.get('mapped')]) == COST_WINDOWS, timeout=30)
    results.check(f'animate-cost: {COST_WINDOWS} {kind} windows map for '
                  f'{effect}', mapped)
    if opening:
      time.sleep(max(0.0, start + length - time.monotonic()))
    elif steady:
      time.sleep(2)
      start = time.monotonic()
      time.sleep(3)
    else:
      # Let the tiles settle and the pipelines build.
      time.sleep(2)
      start = time.monotonic()
      for client in clients:
        checks.run.signal_checked(client.process.pid, signal.SIGTERM,
                                  'wl_probe')
      for client in clients:
        client.stop()
      clients = []
      time.sleep(length)
    end = time.monotonic()
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
  lengths = render_turns(shingle.log_text(), start, end)
  print(f'INFO animate-cost: {which} {effect} {kind}: {summary(lengths)}',
        flush=True)
  results.check(f'animate-cost: {which} {kind} windows with {effect} draws '
                'frames, and Shingle exits with status 0',
                (len(lengths) > 3 or effect == 'none') and status == 0,
                f'{summary(lengths)}, status {status}')
  return lengths

def cost_checks(results, binary, wl_probe, captures):
  """Frame times of whole views against views of one surface."""
  del captures
  for opening in (False, True):
    for effect in ('zoom', 'spin', 'fire', 'none'):
      for pieces in (False, True):
        cost_run(results, binary, wl_probe, effect, pieces, opening)
  # Without any animation, while every window draws.
  for pieces in (False, True):
    cost_run(results, binary, wl_probe, 'steady', pieces)

CHECKS = {
    'animate': animate_checks,
    'animate-cost': cost_checks,
}
