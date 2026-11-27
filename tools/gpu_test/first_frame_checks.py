"""Time to first frame of new tiled windows, run inside run.py's
namespace.

`run.py --checks first-frame` starts offscreen Shingle with a tiled
`wl_probe`, then starts kitty, foot and Firefox beside it, each several
times, every one with `WAYLAND_DEBUG=1`. From each client's log, with
libwayland's millisecond timestamps (resize_checks.py), it reports how
long after the client was started it got its first configure, committed
its first buffer, and committed its first buffer after acknowledging the
configure of its tile's size, the frame that fills its tile. Shingle's
first configure leaves the size to the client, as Wayfire's does, and the
tile's size comes when the window maps, so the time between the first
buffer and the tile's is what the extra round trip costs; before
`shingle-initial-size` the first configure was the tile's size and the
two were the same buffer.

foot is taken from `SHINGLE_FOOT` (a path), else from PATH, and skipped
with an INFO line when neither has it. The first Firefox start creates
its profile and is reported apart from the later ones.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import os
import re
import shutil
import statistics
import time

import checks
import dmabuf_checks
import resize_checks

xw = checks.xw
GROUPS = ('first-frame',)
FOOT_VARIABLE = 'SHINGLE_FOOT'
# How often each client starts, and how long one start may take.
ROUNDS = {'kitty': 5, 'foot': 5, 'firefox': 4}
TIMEOUT = {'kitty': 20, 'foot': 20, 'firefox': 90}
# How long after mapping a client's last configure counts as its final
# size.
SETTLE = 2.0
TOPLEVEL = re.compile(r'new id xdg_toplevel[#@](\d+)')

def toplevel_id(messages):
  """The id of the client's first xdg_toplevel, or None."""
  for message in messages:
    if message['request'] and message['message'] == 'get_toplevel':
      match = TOPLEVEL.search(message['arguments'])
      if match:
        return int(match.group(1))
  return None

def configures(messages, toplevel):
  """The configures of `toplevel` as (ms, size, serial): the size of each
  `xdg_toplevel.configure` and the serial of the `xdg_surface.configure`
  that follows it."""
  found = []
  size = None
  for message in messages:
    if message['request']:
      continue
    if (message['interface'] == 'xdg_toplevel' and message['id'] == toplevel
        and message['message'] == 'configure'):
      parts = [part.strip() for part in message['arguments'].split(',')]
      size = (int(parts[0]), int(parts[1]))
    elif (message['interface'] == 'xdg_surface'
          and message['message'] == 'configure' and size is not None):
      found.append((message['ms'], size, int(message['arguments'])))
      size = None
  return found

def acks(messages):
  """{serial: ms} of the client's `xdg_surface.ack_configure` requests."""
  found = {}
  for message in messages:
    if (message['request'] and message['interface'] == 'xdg_surface'
        and message['message'] == 'ack_configure'):
      found.setdefault(int(message['arguments']), message['ms'])
  return found

def first_frames(messages, start_ms):
  """From a client's messages, the milliseconds after `start_ms` of its
  first `configure`, its first buffer (`first_buffer`), and its first
  buffer after acknowledging the configure of its final size
  (`tile_frame`), with that `size`, the `first_size` configured and the
  number of `configures` up to the tile's; None for what has not
  happened."""
  toplevel = toplevel_id(messages)
  result = {'configure': None, 'first_size': None, 'first_buffer': None,
            'tile_frame': None, 'size': None, 'configures': 0}
  if toplevel is None:
    return result
  received = configures(messages, toplevel)
  commits = resize_checks.main_commits(messages)
  if commits:
    result['first_buffer'] = commits[0][0] - start_ms
  if not received:
    return result
  result['configure'] = received[0][0] - start_ms
  result['first_size'] = received[0][1]
  final = received[-1][1]
  if final == (0, 0):
    return result
  index, (_, size, serial) = next(
      (index, item) for index, item in enumerate(received)
      if item[1] == final)
  result['size'] = size
  result['configures'] = index + 1
  acked = acks(messages).get(serial)
  if acked is None:
    return result
  for ms, _ in commits:
    if ms >= acked:
      result['tile_frame'] = ms - start_ms
      break
  return result

def summary(values):
  """'median (min to max) ms' of `values`, or 'none'."""
  values = [value for value in values if value is not None]
  if not values:
    return 'none'
  return (f'{statistics.median(values):.1f} ms ({min(values):.1f} to '
          f'{max(values):.1f})')

def foot_command():
  """The foot binary, or None."""
  path = os.environ.get(FOOT_VARIABLE)
  if path and os.access(path, os.X_OK):
    return path
  return shutil.which('foot')

def foot_environment(path):
  """foot's environment: the libraries of an unpacked package beside its
  binary (usr/bin/foot next to usr/lib/x86_64-linux-gnu), if any."""
  libraries = os.path.join(os.path.dirname(os.path.dirname(path)), 'lib',
                           'x86_64-linux-gnu')
  if os.path.isdir(libraries) and not path.startswith('/usr/'):
    return {'LD_LIBRARY_PATH': libraries}
  return {}

def clients(shingle):
  """(name, arguments, app ID, program, environment) of each client."""
  found = [('kitty', ['kitty', '--config', 'NONE', 'sh', '-c', 'sleep 60'],
            'kitty', 'kitty', {})]
  foot = foot_command()
  if foot:
    found.append(('foot', [foot, '--config', '/dev/null', 'sh', '-c',
                           'sleep 60'], 'foot', 'foot',
                  foot_environment(foot)))
  profile = os.path.join(shingle.directory, 'home', 'firefox-profile')
  os.makedirs(profile, exist_ok=True)
  with open(os.path.join(profile, 'user.js'), 'w') as file:
    file.write(dmabuf_checks.firefox_user_js())
  found.append(('firefox', ['firefox-esr', '--no-remote', '--new-instance',
                            '--profile', profile, 'about:blank'],
                'firefox-esr', 'firefox-esr', {'MOZ_ENABLE_WAYLAND': '1'}))
  return found

def one_start(shingle, name, arguments, app_id, program, environment, round_):
  """Starts one client, waits for it to map and settle, stops it;
  returns its `first_frames`."""
  start_ms = resize_checks.wayland_ms()
  client = checks.Client(shingle, arguments, f'{name}-{round_}',
                         extra_environment=dict(environment,
                                                WAYLAND_DEBUG='1'),
                         program=program)
  try:
    xw.wait_for(lambda: shingle.view(app_id), timeout=TIMEOUT[name])
    # Until its size settles: a decoration mode or a later configure may
    # still change it after the first tile-sized frame.
    time.sleep(SETTLE)
    return first_frames(resize_checks.parse_debug(client.output()),
                        start_ms)
  finally:
    client.stop()
    xw.wait_for(lambda: shingle.view(app_id) is None, timeout=30)

def first_frame_checks(results, binary, wl_probe, captures):
  """kitty, foot and Firefox, started beside a tiled probe, each get the
  tile's size and fill it; their times to first frame are reported."""
  del captures
  shingle = checks.Shingle(binary, 'first-frame', resize_checks.config())
  probe = None
  try:
    probe = checks.Client(shingle, [wl_probe, 'first-frame-probe'],
                          'first-frame-probe', program='wl_probe')
    mapped = xw.wait_for(lambda: shingle.view('first-frame-probe'),
                         timeout=15)
    results.check('first-frame: a probe maps', bool(mapped),
                  checks.tail(probe.output()))
    if not mapped:
      return
    for name, arguments, app_id, program, environment in clients(shingle):
      starts = [one_start(shingle, name, arguments, app_id, program,
                          environment, round_)
                for round_ in range(ROUNDS[name])]
      # Beside the probe, narrower than the output.
      narrower = checks.OUTPUT[0] - 200
      filled = [frames for frames in starts if frames['tile_frame']
                is not None and frames['size'][0] < narrower]
      results.check(f'first-frame: {name} fills its tile beside the probe '
                    'every time', len(filled) == len(starts),
                    f'{[frames["size"] for frames in starts]}')
      measured = starts[1:] if name == 'firefox' else starts
      if name == 'firefox':
        print(f'INFO first-frame: firefox\'s first start, with a new '
              f'profile: configure {summary([starts[0]["configure"]])}, '
              f'first buffer {summary([starts[0]["first_buffer"]])}, tile '
              f'{summary([starts[0]["tile_frame"]])}', flush=True)
      print(f'INFO first-frame: {name} over {len(measured)} starts: first '
            f'configure {summary([f["configure"] for f in measured])}, '
            f'sized {sorted({str(f["first_size"]) for f in measured})}; '
            f'first buffer {summary([f["first_buffer"] for f in measured])}'
            f'; tile frame {summary([f["tile_frame"] for f in measured])}, '
            f'after {sorted({f["configures"] for f in measured})} '
            'configures', flush=True)
    if not foot_command():
      print('INFO first-frame: foot is not installed; set SHINGLE_FOOT to '
            'an unpacked foot binary', flush=True)
  finally:
    if probe:
      probe.stop()
    status = shingle.stop()
    results.check('first-frame: SIGTERM exits offscreen Shingle with '
                  'status 0', status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

CHECKS = {'first-frame': first_frame_checks}
