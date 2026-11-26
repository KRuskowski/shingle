"""Real-client check of interactive resizes, run inside run.py's
namespace.

Offscreen Shingle on the render node tiles `kitty` beside a `wl_probe`.
Through STIPC the group `resize` holds Super, presses the right button
near kitty's right edge and drags the boundary, then releases, twice:
once `paced`, one pointer motion per output frame, and once in `bursts`
of eight motions per frame, as a 500 Hz mouse moves a host's pointer.
kitty runs with `WAYLAND_DEBUG=1`, so its log has every configure it
receives and every buffer it attaches, with libwayland's millisecond
timestamps. From them the group reports, for each drag, how many
configures kitty got and how many buffers of how many sizes it
committed while the drag ran, and how long after the release it
committed a buffer of the final size; and from the probe's log how
many configures the window that kept its size got. It requires that
kitty drew new sizes during both drags and its final size promptly
after them, and that a window whose size stays the same is not
configured again, as under Wayfire.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import re
import time

import checks

xw = checks.xw
GROUPS = ('resize',)
# How far each drag moves the boundary, over how many output frames, and
# how many pointer motions one IPC write, one frame, carries.
DISTANCE = 300
FRAMES = 40
PACED = 1
BURST = 8
FRAME_SECONDS = 0.016
# What the check requires: buffers of new sizes while a drag runs, and
# the final size drawn soon after the release.
MINIMUM_DRAG_SIZES = 5
MAXIMUM_SETTLE_MS = 250

# One line of WAYLAND_DEBUG output: `[12345.678] ...` with an optional
# queue name and `->` for requests.
LINE = re.compile(r'^\[\s*(\d+)\.(\d+)\]\s*(?:\{[^}]*\}\s*)?(->\s*)?'
                  r'([a-z_0-9]+)[#@](\d+)\.([a-z_0-9]+)\((.*)\)\s*$')
NEW_BUFFER = re.compile(r'new id wl_buffer[#@](\d+), -?\d+, (\d+), (\d+)')
XDG_SURFACE = re.compile(r'new id xdg_surface[#@](\d+), wl_surface[#@](\d+)')

def wayland_ms(now=None):
  """libwayland's WAYLAND_DEBUG timestamp for `now` (time.time()):
  CLOCK_REALTIME in microseconds cut to 32 bits, printed as
  milliseconds. It wraps every 71 minutes, which a check may straddle
  only by bad luck."""
  now = time.time() if now is None else now
  return (int(now * 1_000_000) % (1 << 32)) / 1000

def parse_debug(text):
  """The messages of a WAYLAND_DEBUG log as dicts with `ms`, `request`,
  `interface`, `id`, `message` and `arguments`."""
  messages = []
  for line in text.splitlines():
    match = LINE.match(line.strip())
    if not match:
      continue
    ms, fraction, arrow, interface, ident, message, arguments = (
        match.groups())
    messages.append({
        'ms': int(ms) + int(fraction) / 1000,
        'request': bool(arrow),
        'interface': interface,
        'id': int(ident),
        'message': message,
        'arguments': arguments,
    })
  return messages

def main_commits(messages):
  """The commits of a buffer to the client's xdg surface, as (ms, size):
  wl_shm buffer sizes from `create_buffer`, None for a buffer of unknown
  size."""
  surface = None
  sizes = {}
  attached = None
  commits = []
  for message in messages:
    if not message['request']:
      continue
    created = NEW_BUFFER.match(message['arguments'])
    if created and message['message'] == 'create_buffer':
      sizes[int(created.group(1))] = (int(created.group(2)),
                                      int(created.group(3)))
    paired = XDG_SURFACE.match(message['arguments'])
    if paired and message['message'] == 'get_xdg_surface':
      surface = int(paired.group(2))
    if message['interface'] != 'wl_surface' or message['id'] != surface:
      continue
    if message['message'] == 'attach':
      buffer = re.match(r'wl_buffer[#@](\d+)', message['arguments'])
      attached = sizes.get(int(buffer.group(1))) if buffer else None
      attached = attached or 'unknown'
    elif message['message'] == 'commit' and attached is not None:
      commits.append((message['ms'], None if attached == 'unknown'
                      else attached))
      attached = None
  return commits

def resize_metrics(messages, press_ms, release_ms):
  """From a client's messages: `configures` it received and `buffers` it
  committed between `press_ms` and `release_ms`, the number of distinct
  buffer `sizes` among them, the `final` configure's size, and
  `settle_ms`, from the release to the first commit of a buffer of the
  final size after the final configure (None if it never did)."""
  configures = [m for m in messages
                if not m['request'] and m['interface'] == 'xdg_toplevel'
                and m['message'] == 'configure']
  during = [m for m in configures if press_ms <= m['ms'] <= release_ms]
  commits = main_commits(messages)
  drawn = [size for ms, size in commits if press_ms <= ms <= release_ms]
  final = None
  if configures:
    parts = [part.strip() for part in configures[-1]['arguments'].split(',')]
    final = (int(parts[0]), int(parts[1]))
  settle = None
  if final is not None:
    final_ms = configures[-1]['ms']
    for ms, size in commits:
      if ms >= final_ms and size in (final, None):
        settle = max(0.0, ms - release_ms)
        break
  return {'configures': len(during), 'buffers': len(drawn),
          'sizes': len({size for size in drawn if size}), 'final': final,
          'settle_ms': settle}

def config():
  """BASE_CONFIG without animations."""
  result = dict(checks.BASE_CONFIG)
  result['animate'] = {'open_animation': 'none', 'close_animation': 'none'}
  return result

def move_burst(shingle, points):
  """Moves the STIPC pointer through `points` in one IPC write, so that
  Shingle reads them in one turn, and waits for every reply."""
  shingle.ipc.socket.sendall(b''.join(
      xw.encode_message({'method': 'stipc/move_cursor',
                         'data': {'x': x, 'y': y}}) for x, y in points))
  for _ in points:
    shingle.ipc.receive()

def drag(shingle, start, end, burst):
  """An STIPC Super+right-button drag from `start` to `end` over FRAMES
  output frames with `burst` motions each; returns the WAYLAND_DEBUG
  times of the press and the release."""
  steps = FRAMES * burst
  shingle.call('stipc/move_cursor', {'x': start[0], 'y': start[1]})
  time.sleep(0.1)
  shingle.call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': True})
  shingle.call('stipc/feed_button', {'combo': 'BTN_RIGHT', 'mode': 'press'})
  press = wayland_ms()
  points = [(start[0] + (end[0] - start[0]) * step // steps,
             start[1] + (end[1] - start[1]) * step // steps)
            for step in range(1, steps + 1)]
  for index in range(0, steps, burst):
    move_burst(shingle, points[index:index + burst])
    time.sleep(FRAME_SECONDS)
  release = wayland_ms()
  shingle.call('stipc/feed_button',
               {'combo': 'BTN_RIGHT', 'mode': 'release'})
  shingle.call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': False})
  return press, release

def one_drag(results, shingle, kitty, probe, name, burst, distance):
  """One drag of kitty's right edge by `distance` with `burst` motions per
  frame, its numbers and its checks; returns how many configures `probe`
  got meanwhile."""
  geometry = shingle.view('kitty')['geometry']
  edge = geometry['x'] + geometry['width'] - 10
  middle = geometry['y'] + geometry['height'] // 2
  probe_before = len(checks.probe_events(probe.output(), 'configure'))
  press, release = drag(shingle, (edge, middle), (edge + distance, middle),
                        burst)
  resized = xw.wait_for(
      lambda: abs(shingle.view('kitty')['geometry']['width']
                  - geometry['width']) > abs(distance) // 2, timeout=5)
  results.check(f'resize: the {name} drag resizes kitty\'s tile',
                bool(resized),
                f'{geometry} -> {shingle.view("kitty")["geometry"]}')
  # Give kitty time to catch up before reading its log.
  time.sleep(1.5)
  metrics = resize_metrics(parse_debug(kitty.output()), press, release)
  probe_configures = (len(checks.probe_events(probe.output(), 'configure'))
                      - probe_before)
  final = shingle.view('kitty')['geometry']
  print(f'INFO resize: {name} drag, {FRAMES * burst} motions in '
        f'{release - press:.0f} ms, {burst} per frame: kitty got '
        f'{metrics["configures"]} configures and committed '
        f'{metrics["buffers"]} buffers of {metrics["sizes"]} sizes; final '
        f'configure {metrics["final"]} for a tile of {final["width"]}x'
        f'{final["height"]}, drawn {metrics["settle_ms"]} ms after the '
        f'release; the floating probe got {probe_configures} configures',
        flush=True)
  results.check(
      f'resize: kitty draws at least {MINIMUM_DRAG_SIZES} sizes while the '
      f'{name} drag runs', metrics['sizes'] >= MINIMUM_DRAG_SIZES,
      str(metrics))
  results.check(
      f'resize: kitty draws its final size within {MAXIMUM_SETTLE_MS} ms '
      f'of the {name} drag\'s release',
      metrics['settle_ms'] is not None
      and metrics['settle_ms'] <= MAXIMUM_SETTLE_MS, str(metrics))
  return probe_configures

def resize_checks(results, binary, wl_probe, captures):
  """kitty keeps drawing while a tile resize runs and catches up after."""
  del captures
  shingle = checks.Shingle(binary, 'resize', config())
  clients = []
  try:
    kitty = checks.Client(
        shingle, ['kitty', '--config', 'NONE', 'sh', '-c', 'sleep 60'],
        'kitty', extra_environment={'WAYLAND_DEBUG': '1'})
    clients.append(kitty)
    if not xw.wait_for(lambda: shingle.view('kitty'), timeout=20):
      results.check('resize: kitty maps', False,
                    checks.tail(kitty.output()))
      return
    probe = checks.Client(shingle, [wl_probe, 'other'], 'other',
                          program='wl_probe')
    clients.append(probe)
    tiled = xw.wait_for(
        lambda: shingle.view('other') and shingle.view('kitty')[
            'geometry']['width'] < checks.OUTPUT[0] - 100, timeout=15)
    results.check('resize: kitty and a probe tile side by side',
                  bool(tiled), str(shingle.views()))
    if not tiled:
      return
    # A floating probe whose size and state nothing changes.
    floater = checks.Client(shingle, [wl_probe, 'floater'], 'floater',
                            program='wl_probe')
    clients.append(floater)
    floated = xw.wait_for(lambda: shingle.view('floater'), timeout=15)
    if floated:
      shingle.call('window-rules/configure-view', {
          'id': shingle.view('floater')['id'],
          'geometry': {'x': 900, 'y': 400, 'width': 300, 'height': 200}})
      floated = xw.wait_for(
          lambda: not shingle.view('floater')['tiled-edges']
          and shingle.view('kitty')['geometry']['width'] < 1000, timeout=5)
    results.check('resize: a third probe floats beside the two tiles',
                  bool(floated), str(shingle.views()))
    if not floated:
      return
    # Focus kitty first, so that the drags change nothing about the
    # floater, which as the newest window was focused.
    kitty_geometry = shingle.view('kitty')['geometry']
    shingle.call('stipc/move_cursor', {
        'x': kitty_geometry['x'] + kitty_geometry['width'] // 2,
        'y': kitty_geometry['y'] + kitty_geometry['height'] // 2})
    for mode in ('press', 'release'):
      shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': mode})
    focused = xw.wait_for(lambda: shingle.view('kitty')['activated'],
                          timeout=5)
    results.check('resize: a click focuses kitty', bool(focused),
                  str(shingle.view('kitty')))
    # Let kitty draw at its tiled size, and the floater settle.
    time.sleep(1.0)
    unchanged = 0
    for name, burst, distance in (('paced', PACED, DISTANCE),
                                  ('burst', BURST, -DISTANCE)):
      unchanged += one_drag(results, shingle, kitty, floater, name, burst,
                            distance)
    results.check(
        'resize: the floating probe, whose state stays the same, gets no '
        'configure during the drags', unchanged == 0,
        f'{unchanged} configures')
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    results.check('resize: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {'resize': resize_checks}
