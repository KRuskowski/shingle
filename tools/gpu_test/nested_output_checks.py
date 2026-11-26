"""MO-6's real-client checks of nested Shingle with a window per output,
run inside run.py's namespace.

docs/design/multi-output.md, MO-6. Nested Shingle runs on lavapipe as a
client of a headless Shingle, never of the operator's desktop, and the
group `nested-outputs` checks, with `shingle --backend nested --outputs
2`:

- the headless Shingle gets two toplevels, titled after their outputs,
  and nested lists `HEADLESS-1` and `HEADLESS-2` side by side, each the
  size the headless Shingle tiled its window to;
- each window shows its own output: a `wl_probe --pattern` moved to
  `HEADLESS-2` appears in the second window and another in the first,
  in `grim` captures of the headless Shingle;
- pointer input fed through the headless Shingle's `stipc/move_cursor`
  into a window reaches nested at that window's output, offset by its
  origin, and `stipc/feed_button` there makes that output active, with
  `output-gain-focus`;
- `stipc/create_wayland_output` opens a third window, `HEADLESS-3`,
  which the headless Shingle maps; closing the second window through
  the headless Shingle's `window-rules/close-view` moves its probe to
  another output under `output_layout.unplug: return`, and the next
  `stipc/create_wayland_output` opens `HEADLESS-2` again, which takes
  the probe back; `stipc/destroy_wayland_output` closes `HEADLESS-3`;
- Wayfire's errors for an unknown output and from a Shingle that is not
  nested, and `list-methods` with 87 methods;
- closing the last window quits nested Shingle with status 0.

The group `nested-smoke` repeats the nested smoke test of
docs/status.md with one window, inside the namespace instead of on the
host session: `stipc/get_display` names the socket nested printed,
`list-methods` returns its methods sorted, `wayland-info` lists the
globals clients rely on, and one client maps, is focused and fills its
tile.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import subprocess
import time

import checks

xw = checks.xw
GROUPS = ('nested-outputs', 'nested-smoke')
# What both Shingles run with: checks.py's base, no open animation, no
# gaps and no tiling animation, so windows sit where they are tiled.
STILL = {
    'animate': {'open_animation': 'none', 'close_animation': 'none'},
    'tiling': {'animation_duration': 0,
               'gaps': {'top': 0, 'right': 0, 'bottom': 0, 'left': 0,
                        'inner': 0}},
}
METHODS = 87
# Globals the smoke test requires nested to offer.
REQUIRED_GLOBALS = ('wl_compositor', 'wl_shm', 'wl_seat', 'wl_output',
                    'xdg_wm_base', 'zwlr_layer_shell_v1')

def config():
  """The YAML config, as JSON: checks.py's base without animations."""
  result = json.loads(json.dumps(checks.BASE_CONFIG))
  result.update(json.loads(json.dumps(STILL)))
  return result

def window_title(output):
  """The title nested gives the window of `output`."""
  return 'Shingle' if output == 'HEADLESS-1' else f'Shingle - {output}'

def host_windows(outer):
  """The mapped views of the headless Shingle by title: where each draws
  its surface, inside a server-side frame if it has one (WG-6), as
  geometry tuples."""
  found = {}
  for view in outer.views():
    if view.get('mapped', True):
      geometry = view['base-geometry']
      found[view.get('title')] = (geometry['x'], geometry['y'],
                                  geometry['width'], geometry['height'])
  return found

def host_view(outer, output):
  """The headless Shingle's view of `output`'s window, or None."""
  title = window_title(output)
  return next((view for view in outer.views()
               if view.get('title') == title), None)

def nested_outputs(nested):
  """Nested's outputs by name: (id, geometry tuple)."""
  found = {}
  for output in nested.call('window-rules/list-outputs'):
    geometry = output['geometry']
    found[output['name']] = (output['id'], (
        geometry['x'], geometry['y'], geometry['width'], geometry['height']))
  return found

def sizes_match(windows, outputs):
  """Whether every output is as big as its window, which is mapped."""
  for name, (_, geometry) in outputs.items():
    window = windows.get(window_title(name))
    if window is None or window[2:] != geometry[2:]:
      return False
  return bool(outputs)

def to_host(point, window, output):
  """`point` in nested's layout as a point of the headless Shingle: the
  window's origin plus the offset from `output`'s origin."""
  return (window[0] + point[0] - output[0], window[1] + point[1] - output[1])

def offset_of(window, output):
  """How far nested's layout is moved in the headless Shingle where
  `window` shows `output`."""
  return (window[0] - output[0], window[1] - output[1])

def view_output(nested, app_id):
  """The output name of nested's view `app_id`, or None."""
  view = nested.view(app_id)
  return view.get('output-name') if view else None

def wait_output(nested, app_id, name, timeout=15):
  """Whether nested's view `app_id` is on `name` within `timeout`."""
  return xw.wait_for(lambda: view_output(nested, app_id) == name,
                     timeout=timeout)

def next_event(watcher, name, timeout=10):
  """The next IPC event called `name` within `timeout`, or None."""
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    try:
      event = watcher.receive(timeout=max(0.1, deadline - time.monotonic()))
    except (TimeoutError, ConnectionError):
      return None
    if event.get('event') == name:
      return event
  return None

def watch(shingle, events):
  """A second IPC connection of `shingle` watching `events`."""
  watcher = xw.Ipc(shingle.ipc_path)
  watcher.call('window-rules/events/watch', {'events': events})
  return watcher

def capture(outer):
  """A `grim` capture of the headless Shingle, or None."""
  try:
    done = subprocess.run(['grim', '-t', 'ppm', '-'],
                          env=outer.client_environment(),
                          capture_output=True, timeout=20,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  parsed = xw.parse_ppm(done.stdout) if done.returncode == 0 else None
  return xw.Capture(*parsed) if parsed else None

def start(binary, name, outputs=None):
  """A headless Shingle and nested Shingle on lavapipe in it, in
  directories called after `name`, with `outputs` windows if given."""
  outer = checks.Shingle(binary, f'{name}-host', config(),
                         backend=('--headless',))
  backend = ('--backend', 'nested')
  if outputs:
    backend += ('--outputs', str(outputs))
  try:
    nested = checks.Shingle(
        binary, f'{name}-nested', config(), backend=backend,
        extra_environment={'WAYLAND_DISPLAY': outer.wayland_display(),
                           'VK_ICD_FILENAMES': checks.LAVAPIPE})
  except Exception:
    outer.stop()
    raise
  return outer, nested

def window_checks(results, outer, nested):
  """Two windows, sized as their outputs, each showing its own probe.
  Returns the outputs and windows found, or None."""
  windows = xw.wait_for(lambda: (lambda found: found if len(found) == 2
                                 else None)(host_windows(outer)),
                        timeout=30) or host_windows(outer)
  results.check('nested-outputs: the headless Shingle maps two toplevels, '
                'titled after their outputs',
                set(windows) == {window_title('HEADLESS-1'),
                                 window_title('HEADLESS-2')},
                json.dumps(windows))
  settled = xw.wait_for(
      lambda: sizes_match(host_windows(outer), nested_outputs(nested)),
      timeout=15)
  outputs = nested_outputs(nested)
  windows = host_windows(outer)
  first = outputs.get('HEADLESS-1', (None, (0, 0, 0, 0)))[1]
  second = outputs.get('HEADLESS-2', (None, (0, 0, 0, 0)))[1]
  results.check(
      'nested-outputs: nested lists HEADLESS-1 and HEADLESS-2 side by side, '
      'each as big as its window',
      settled and set(outputs) == {'HEADLESS-1', 'HEADLESS-2'}
      and first[:2] == (0, 0) and second[:2] == (first[2], 0),
      f'outputs {json.dumps(outputs)}, windows {json.dumps(windows)}')
  if set(outputs) != {'HEADLESS-1', 'HEADLESS-2'} or len(windows) != 2:
    return None
  return outputs, windows

def content_checks(results, outer, nested, wl_probe, layout):
  """A probe on each output, shown in that output's window. Returns the
  probes."""
  outputs, windows = layout
  probes = []
  for app_id, output in (('left', 'HEADLESS-1'), ('right', 'HEADLESS-2')):
    probe = checks.Client(nested, [wl_probe, app_id, '--pattern'], app_id,
                          program='wl_probe')
    probes.append(probe)
    view = xw.wait_for(lambda app_id=app_id: nested.view(app_id),
                       timeout=15)
    if view is None:
      results.check(f'nested-outputs: probe {app_id} maps', False,
                    checks.tail(probe.output()))
      return probes
    nested.call('window-rules/configure-view',
                {'id': view['id'], 'output_id': outputs[output][0]})
    results.check(f'nested-outputs: probe {app_id} is on {output}',
                  wait_output(nested, app_id, output),
                  json.dumps(nested.view(app_id)))
  for app_id, output in (('left', 'HEADLESS-1'), ('right', 'HEADLESS-2')):
    offset = offset_of(windows[window_title(output)], outputs[output][1])

    def wrong(shot, app_id=app_id, offset=offset):
      view = nested.view(app_id)
      if view is None or shot is None:
        return ['no view or capture']
      return checks.pattern_mismatches(shot, view['geometry'], offset=offset)
    last = [None]

    def shown(wrong=wrong):
      last[0] = capture(outer)
      return not wrong(last[0])
    xw.wait_for(shown, timeout=20, interval=0.3)
    results.check(
        f'nested-outputs: the {output} window shows its own probe {app_id}',
        not wrong(last[0]),
        f'offset {offset}, wrong {wrong(last[0])}')
  return probes

def input_checks(results, outer, nested, layout):
  """Pointer motion and a click through the headless Shingle land on the
  output of the window they are in."""
  outputs, windows = layout
  watcher = watch(nested, ['output-gain-focus'])
  try:
    for output, point in (('HEADLESS-2', (100, 200)),
                          ('HEADLESS-1', (300, 100))):
      origin = outputs[output][1]
      window = windows[window_title(output)]
      target = (origin[0] + point[0], origin[1] + point[1])
      host = to_host(target, window, origin)
      outer.call('stipc/move_cursor', {'x': host[0], 'y': host[1]})

      def arrived(target=target):
        reply = nested.call('window-rules/get-cursor-position')
        cursor = reply.get('pos', {})
        return (abs(cursor.get('x', -1e9) - target[0]) <= 1
                and abs(cursor.get('y', -1e9) - target[1]) <= 1)
      results.check(
          f'nested-outputs: motion into the {output} window reaches nested '
          f'at {target} on {output}',
          xw.wait_for(arrived, timeout=10),
          f'host point {host}, cursor '
          f'{nested.call("window-rules/get-cursor-position")}')
      outer.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
      outer.call('stipc/feed_button',
                 {'combo': 'BTN_LEFT', 'mode': 'release'})

      def focused(output=output):
        reply = nested.call('window-rules/get-focused-output')
        return reply.get('info', {}).get('name') == output
      results.check(
          f'nested-outputs: a click in the {output} window makes {output} '
          'active',
          xw.wait_for(focused, timeout=10),
          json.dumps(nested.call('window-rules/get-focused-output')))
      if output == 'HEADLESS-2':
        event = next_event(watcher, 'output-gain-focus')
        results.check(
            'nested-outputs: watchers get output-gain-focus for HEADLESS-2',
            event is not None
            and event.get('output', {}).get('name') == 'HEADLESS-2',
            json.dumps(event))
  finally:
    watcher.close()

def create_and_close_checks(results, outer, nested):
  """A third window by STIPC, the second closed and opened again with
  its probe back, and the third destroyed."""
  reply = nested.call('stipc/create_wayland_output')
  results.check(
      'nested-outputs: stipc/create_wayland_output replies ok with '
      'HEADLESS-3',
      reply.get('result') == 'ok'
      and reply.get('output', {}).get('name') == 'HEADLESS-3',
      json.dumps(reply))
  three = xw.wait_for(lambda: window_title('HEADLESS-3') in host_windows(
      outer) and sizes_match(host_windows(outer), nested_outputs(nested)),
                      timeout=30)
  results.check(
      'nested-outputs: the headless Shingle maps a third window, which '
      'sizes HEADLESS-3',
      three and len(host_windows(outer)) == 3,
      f'windows {json.dumps(host_windows(outer))}, outputs '
      f'{json.dumps(nested_outputs(nested))}')
  watcher = watch(nested, ['view-set-output', 'output-removed'])
  try:
    second = host_view(outer, 'HEADLESS-2')
    if second is not None:
      outer.call('window-rules/close-view', {'id': second['id']})
    gone = xw.wait_for(
        lambda: 'HEADLESS-2' not in nested_outputs(nested), timeout=15)
    moved = xw.wait_for(
        lambda: view_output(nested, 'right') not in (None, 'HEADLESS-2'),
        timeout=15)
    results.check(
        'nested-outputs: closing the HEADLESS-2 window removes the output '
        'and moves its probe to another output',
        second is not None and gone and moved
        and window_title('HEADLESS-2') not in host_windows(outer),
        f'outputs {sorted(nested_outputs(nested))}, probe on '
        f'{view_output(nested, "right")}, windows '
        f'{sorted(host_windows(outer))}')
    event = next_event(watcher, 'output-removed')
    results.check('nested-outputs: watchers get output-removed for HEADLESS-2',
                  event is not None
                  and event.get('output', {}).get('name') == 'HEADLESS-2',
                  json.dumps(event))
  finally:
    watcher.close()
  reply = nested.call('stipc/create_wayland_output')
  results.check(
      'nested-outputs: the next stipc/create_wayland_output opens '
      'HEADLESS-2 again',
      reply.get('output', {}).get('name') == 'HEADLESS-2', json.dumps(reply))
  results.check(
      'nested-outputs: under return the probe goes back to HEADLESS-2',
      wait_output(nested, 'right', 'HEADLESS-2')
      and xw.wait_for(lambda: window_title('HEADLESS-2') in host_windows(
          outer), timeout=30),
      f'probe on {view_output(nested, "right")}, windows '
      f'{sorted(host_windows(outer))}')
  reply = nested.call('stipc/destroy_wayland_output', {'output': 'HEADLESS-3'})
  closed = xw.wait_for(
      lambda: window_title('HEADLESS-3') not in host_windows(outer),
      timeout=15)
  results.check(
      'nested-outputs: stipc/destroy_wayland_output closes the HEADLESS-3 '
      'window',
      reply.get('result') == 'ok' and closed
      and 'HEADLESS-3' not in nested_outputs(nested),
      f'{json.dumps(reply)}, windows {sorted(host_windows(outer))}')

def error_checks(results, outer, nested):
  """Wayfire's errors, and the method count."""
  reply = nested.call('stipc/destroy_wayland_output', {'output': 'DP-9'})
  results.check('nested-outputs: destroying an unknown output is refused '
                'with Wayfire\'s text',
                reply == {'error': 'Could not find output: "DP-9"'},
                json.dumps(reply))
  reply = outer.call('stipc/create_wayland_output')
  results.check('nested-outputs: headless Shingle refuses '
                'stipc/create_wayland_output as Wayfire does',
                reply == {'error':
                          'Wayfire is not running in nested wayland mode!'},
                json.dumps(reply))
  methods = nested.call('list-methods').get('methods', [])
  results.check(f'nested-outputs: list-methods returns {METHODS} methods '
                'with the two nested ones',
                len(methods) == METHODS
                and 'stipc/create_wayland_output' in methods
                and 'stipc/destroy_wayland_output' in methods,
                str(len(methods)))

def quit_checks(results, outer, nested):
  """Closing every window, the last one last, quits nested."""
  for output in ('HEADLESS-2', 'HEADLESS-1'):
    view = host_view(outer, output)
    if view is not None:
      outer.call('window-rules/close-view', {'id': view['id']})
    if output == 'HEADLESS-2':
      xw.wait_for(lambda: 'HEADLESS-2' not in nested_outputs(nested),
                  timeout=15)
  try:
    status = nested.process.wait(15)
  except subprocess.TimeoutExpired:
    status = None
  results.check('nested-outputs: closing the last window quits nested with '
                'status 0',
                status == 0, f'status {status}, '
                f'{checks.tail(nested.log_text())}')

def nested_output_checks(results, binary, wl_probe, captures):
  """Nested Shingle with two windows in a headless Shingle."""
  del captures
  outer, nested = start(binary, 'nested-outputs', outputs=2)
  probes = []
  try:
    layout = window_checks(results, outer, nested)
    if layout is not None:
      probes = content_checks(results, outer, nested, wl_probe, layout)
      input_checks(results, outer, nested, layout)
      create_and_close_checks(results, outer, nested)
      error_checks(results, outer, nested)
      for probe in probes:
        probe.stop()
      probes = []
      quit_checks(results, outer, nested)
  finally:
    for probe in probes:
      probe.stop()
    status = nested.stop()
    outer_status = outer.stop()
  if status != 0:
    print(f'--- nested log ---\n{nested.log_text()}--- end ---', flush=True)
  results.check('nested-outputs: SIGTERM exits headless Shingle with '
                'status 0', outer_status == 0, f'status {outer_status}')

def wayland_globals(shingle):
  """The interface names `wayland-info` lists for `shingle`."""
  done = subprocess.run(['wayland-info'], env=shingle.client_environment(),
                        capture_output=True, text=True, timeout=20,
                        stdin=subprocess.DEVNULL)
  names = []
  for line in done.stdout.splitlines():
    if "interface: '" in line:
      names.append(line.split("interface: '", 1)[1].split("'", 1)[0])
  return names

def smoke_checks(results, binary, wl_probe, captures):
  """docs/status.md's nested smoke test, with one window, in the
  namespace."""
  del captures
  outer, nested = start(binary, 'nested-smoke')
  probe = None
  try:
    windows = xw.wait_for(lambda: host_windows(outer), timeout=30) or {}
    results.check('nested-smoke: nested maps one window titled Shingle',
                  list(windows) == ['Shingle'], json.dumps(windows))
    display = nested.call('stipc/get_display')
    socket = nested.wayland_display()
    results.check('nested-smoke: stipc/get_display names the private socket',
                  socket is not None
                  and display.get('wayland') == socket.rsplit('/', 1)[-1],
                  f'{json.dumps(display)} {socket}')
    methods = nested.call('list-methods').get('methods', [])
    results.check(f'nested-smoke: list-methods returns {METHODS} sorted '
                  'methods',
                  len(methods) == METHODS and methods == sorted(methods),
                  str(len(methods)))
    names = wayland_globals(nested)
    missing = [name for name in REQUIRED_GLOBALS if name not in names]
    results.check('nested-smoke: wayland-info lists the core globals',
                  not missing and names.count('wl_output') == 1,
                  f'{len(names)} globals, missing {missing}')
    print(f'INFO nested-smoke: {len(names)} globals', flush=True)
    probe = checks.Client(nested, [wl_probe, 'smoke'], 'smoke',
                          program='wl_probe')
    view = xw.wait_for(lambda: nested.view('smoke'), timeout=15)
    output = nested.call('window-rules/list-outputs')[0]['geometry']

    def placed():
      current = nested.view('smoke')
      focused = nested.call('window-rules/get-focused-view')
      return (current is not None
              and current['geometry'] == output
              and focused.get('info', {}).get('app-id') == 'smoke')
    results.check('nested-smoke: one client maps, is focused and fills its '
                  'tile',
                  view is not None and xw.wait_for(placed, timeout=10),
                  f'{json.dumps(nested.view("smoke"))} output '
                  f'{json.dumps(output)}')
  finally:
    if probe is not None:
      probe.stop()
    status = nested.stop()
    outer_status = outer.stop()
  results.check('nested-smoke: SIGTERM exits nested with status 0',
                status == 0, f'status {status}')
  results.check('nested-smoke: SIGTERM exits headless with status 0',
                outer_status == 0, f'status {outer_status}')

CHECKS = {
    'nested-outputs': nested_output_checks,
    'nested-smoke': smoke_checks,
}
