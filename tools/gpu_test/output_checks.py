"""MO-5's real-client checks of offscreen Shingle with two outputs, run
inside run.py's namespace.

docs/design/multi-output.md, MO-5. `shingle --backend offscreen
--outputs 2` runs with `HEADLESS-1` at 1280x720 and 60 Hz and
`HEADLESS-2` at 1024x768 and 30 Hz beside it, and the group `outputs`
checks:

- both outputs are listed over IPC at their automatic positions;
- two `wl_probe --animate --presentation` clients, one on each output,
  get as many presented frames over 5 s as their output's refresh rate
  gives, within 2 %, so that each output is paced on its own, and every
  presentation feedback names the output its window is on as
  `sync_output`;
- `grim -o` of each output shows that output's own windows, and a
  floating window that crosses the boundary is cut there: its part
  beyond its own output is not drawn on the other one;
- a drag with `stipc/feed_button` and `stipc/move_cursor` that ends on
  the other output moves the window there.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import subprocess
import time

import checks

xw = checks.xw
GROUPS = ('outputs',)
# The two outputs, as `[output:NAME]` entries.
OUTPUTS = (
    {'name': 'HEADLESS-1', 'mode': '1280x720@60'},
    {'name': 'HEADLESS-2', 'mode': '1024x768@30'},
)
# Where they are, by name: (x, y, width, height), and their rates.
LAYOUT = {'HEADLESS-1': (0, 0, 1280, 720), 'HEADLESS-2': (1280, 0, 1024, 768)}
RATES = {'HEADLESS-1': 60, 'HEADLESS-2': 30}
# How long the frames are counted, and the tolerance asked for.
COUNT_SECONDS = 5
TOLERANCE = 0.02
# Frames each probe draws at most; they are stopped before.
ANIMATE_FRAMES = 100000
# A floating window across the boundary between the outputs.
STRADDLING = {'x': 1180, 'y': 100, 'width': 200, 'height': 200}
# A floating window on HEADLESS-2, in layout coordinates; IPC takes and
# reports it relative to that output, as Wayfire does.
ON_SECOND = {'x': 1880, 'y': 400, 'width': 300, 'height': 200}

def config():
  """The YAML config, as JSON: checks.py's base with the two outputs, no
  open animation and no gaps."""
  result = json.loads(json.dumps(checks.BASE_CONFIG))
  result['outputs'] = [dict(output) for output in OUTPUTS]
  result['animate'] = {'open_animation': 'none'}
  result['tiling'] = {'animation_duration': 0,
                      'gaps': {'top': 0, 'right': 0, 'bottom': 0,
                               'left': 0, 'inner': 0}}
  return result

def probe_events(text, name):
  """The JSON events called `name` that a probe printed."""
  events = []
  for line in text.splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get('event') == name:
      events.append(event)
  return events

def frames_in_window(presented, output, skip_ms=1000,
                     length_ms=COUNT_SECONDS * 1000):
  """How many of the `presented` events on `output` fall into the
  `length_ms` window that starts `skip_ms` after the first of them, or
  None if they do not last that long."""
  times = [event['msec'] for event in presented
           if event.get('output') == output]
  if not times:
    return None
  start = times[0] + skip_ms
  end = start + length_ms
  if times[-1] < end:
    return None
  return sum(1 for time_ms in times if start <= time_ms < end)

def rate_matches(count, rate, seconds=COUNT_SECONDS, tolerance=TOLERANCE):
  """Whether `count` frames in `seconds` are `rate` Hz within
  `tolerance`."""
  expected = rate * seconds
  return count is not None and abs(count - expected) <= tolerance * expected

def outputs_by_name(shingle):
  """`window-rules/list-outputs` by name: (id, geometry tuple)."""
  found = {}
  for output in shingle.call('window-rules/list-outputs'):
    geometry = output['geometry']
    found[output['name']] = (output['id'], (
        geometry['x'], geometry['y'], geometry['width'], geometry['height']))
  return found

def grim(shingle, output):
  """A capture of `output` alone, or None."""
  try:
    done = subprocess.run(['grim', '-o', output, '-t', 'ppm', '-'],
                          env=shingle.client_environment(),
                          capture_output=True, timeout=20,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  parsed = xw.parse_ppm(done.stdout) if done.returncode == 0 else None
  return xw.Capture(*parsed) if parsed else None

def local(geometry, output):
  """`geometry` relative to `output`'s top-left corner."""
  x, y, _, _ = LAYOUT[output]
  return dict(geometry, x=geometry['x'] - x, y=geometry['y'] - y)

def configure(shingle, view, **data):
  """`window-rules/configure-view` for `view`; returns whether it
  answered ok."""
  reply = shingle.call('window-rules/configure-view',
                       dict(data, id=view['id']))
  return reply.get('result') == 'ok'

def mapped_view(shingle, app_id):
  """The mapped view of `app_id`, within 15 s, or None."""
  return xw.wait_for(lambda: shingle.view(app_id), timeout=15)

def pacing_checks(results, shingle, wl_probe):
  """Two animating probes, one per output, and their presentations."""
  probes = []
  try:
    for name in RATES:
      app_id = f'paced-{name}'
      probe = checks.Client(
          shingle, [wl_probe, app_id, '--animate', str(ANIMATE_FRAMES),
                    '--presentation'], app_id, program='wl_probe')
      probes.append((name, probe))
      view = mapped_view(shingle, app_id)
      if view is None:
        results.check(f'outputs: {app_id} maps', False,
                      checks.tail(probe.output()))
        return
      ids = outputs_by_name(shingle)
      configure(shingle, view, output_id=ids[name][0])
    time.sleep(COUNT_SECONDS + 2.5)
    for name, probe in probes:
      presented = probe_events(probe.output(), 'presented')
      count = frames_in_window(presented, name)
      results.check(
          f'outputs: {name} presents {RATES[name]} frames a second over '
          f'{COUNT_SECONDS} s, within 2 %',
          rate_matches(count, RATES[name]),
          f'{count} frames, {len(presented)} presented in all')
      # Once on its output, every feedback names it.
      outputs = [event['output'] for event in presented]
      first = outputs.index(name) if name in outputs else len(outputs)
      wrong = [output for output in outputs[first:] if output != name]
      results.check(
          f'outputs: presentation feedback of the window on {name} names '
          f'{name}',
          first < len(outputs) and not wrong,
          f'{len(outputs) - first} on {name}, others {sorted(set(wrong))}')
  finally:
    for _, probe in probes:
      probe.stop()

def settled(shingle, output, predicate, timeout=15):
  """Captures `output` until `predicate(shot)` holds or time runs out;
  returns the last capture."""
  last = [None]

  def attempt():
    last[0] = grim(shingle, output)
    return last[0] is not None and predicate(last[0])
  xw.wait_for(attempt, timeout=timeout, interval=0.3)
  return last[0]

def colour_at(shot, x, y):
  """The name of the colour at (x, y) of `shot`, or None."""
  return checks.classify(*shot.pixel(x, y)) if shot else None

def clipping_checks(results, shingle, wl_probe):
  """`grim -o` per output, a window cut at the boundary, and a drag onto
  the other output."""
  probes = []
  try:
    for app_id in ('straddling', 'second'):
      probe = checks.Client(shingle, [wl_probe, app_id, '--pattern'], app_id,
                            program='wl_probe')
      probes.append(probe)
      if mapped_view(shingle, app_id) is None:
        results.check(f'outputs: {app_id} maps', False,
                      checks.tail(probe.output()))
        return
    ids = outputs_by_name(shingle)
    own = local(ON_SECOND, 'HEADLESS-2')
    configure(shingle, shingle.view('straddling'),
              output_id=ids['HEADLESS-1'][0], geometry=STRADDLING)
    configure(shingle, shingle.view('second'),
              output_id=ids['HEADLESS-2'][0], geometry=own)

    def placed():
      straddling = shingle.view('straddling') or {}
      second = shingle.view('second') or {}
      return (straddling.get('geometry') == STRADDLING
              and second.get('geometry') == own)
    results.check('outputs: the windows float where configure-view put them',
                  xw.wait_for(placed, timeout=10),
                  json.dumps([shingle.view('straddling'),
                              shingle.view('second')]))
    # On HEADLESS-1 the straddling window's left quadrants, red over blue,
    # 50 pixels left of the boundary.
    first = settled(shingle, 'HEADLESS-1',
                    lambda shot: colour_at(shot, 1230, 150) == 'red'
                    and colour_at(shot, 1230, 250) == 'blue')
    results.check(
        'outputs: grim -o HEADLESS-1 shows the straddling window up to the '
        'boundary',
        first is not None and (first.width, first.height) == (1280, 720)
        and colour_at(first, 1230, 150) == 'red'
        and colour_at(first, 1230, 250) == 'blue',
        f'size {(first.width, first.height) if first else None}, pixels '
        f'{first.pixel(1230, 150) if first else None} and '
        f'{first.pixel(1230, 250) if first else None}')
    second = settled(shingle, 'HEADLESS-2',
                     lambda shot: not checks.pattern_mismatches(shot, own))
    results.check(
        'outputs: grim -o HEADLESS-2 shows its own window at its geometry',
        second is not None and (second.width, second.height) == (1024, 768)
        and not checks.pattern_mismatches(second, own),
        f'size {(second.width, second.height) if second else None}, wrong '
        f'{checks.pattern_mismatches(second, own) if second else None}')
    # Layout x 1330 would be the straddling window's green top right.
    overflow = (50, 150)
    results.check(
        'outputs: the straddling window is cut at the boundary, not drawn '
        'on HEADLESS-2',
        second is not None and checks.background_at(second, *overflow),
        f'pixel {second.pixel(*overflow) if second else None} at {overflow}')
    drag_checks(results, shingle)
  finally:
    for probe in probes:
      probe.stop()

def drag_checks(results, shingle):
  """Drags the straddling window onto HEADLESS-2 with STIPC."""
  start = (1200, 150)
  shingle.call('stipc/move_cursor', {'x': start[0], 'y': start[1]})
  shingle.call('stipc/feed_button', {'combo': 'S-BTN_LEFT', 'mode': 'press'})
  for x in range(1250, 1501, 50):
    shingle.call('stipc/move_cursor', {'x': x, 'y': start[1]})
    time.sleep(0.05)
  shingle.call('stipc/feed_button',
               {'combo': 'S-BTN_LEFT', 'mode': 'release'})
  ids = outputs_by_name(shingle)
  view = xw.wait_for(
      lambda: (lambda view: view if view and view.get('output-id')
               == ids['HEADLESS-2'][0] else None)(shingle.view('straddling')),
      timeout=10)
  # 300 pixels right in the layout, reported relative to HEADLESS-2.
  dropped = local(dict(STRADDLING, x=STRADDLING['x'] + 300), 'HEADLESS-2')
  results.check(
      'outputs: a drag across the boundary moves the window to HEADLESS-2',
      view is not None and view['geometry']['x'] == dropped['x'],
      json.dumps(shingle.view('straddling')))

def output_checks(results, binary, wl_probe, captures):
  """Offscreen Shingle with two outputs at different refresh rates."""
  del captures
  shingle = checks.Shingle(
      binary, 'outputs', config(),
      backend=('--backend', 'offscreen', '--outputs', '2'))
  try:
    found = xw.wait_for(lambda: len(outputs_by_name(shingle)) == 2,
                        timeout=15)
    listed = {name: geometry
              for name, (_, geometry) in outputs_by_name(shingle).items()}
    results.check('outputs: offscreen Shingle starts with both outputs',
                  found and listed == LAYOUT, json.dumps(listed))
    pacing_checks(results, shingle, wl_probe)
    clipping_checks(results, shingle, wl_probe)
  finally:
    status = shingle.stop()
  results.check('outputs: SIGTERM exits offscreen Shingle with status 0',
                status == 0,
                f'status {status}, {checks.tail(shingle.log_text())}')
  return status

CHECKS = {
    'outputs': output_checks,
}
