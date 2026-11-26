"""WG-4's real-client checks of wayfire-shell-unstable-v2, run inside
run.py's namespace (docs/design/wayfire-gaps.md, WG-4).

A raw-protocol client (wayland_wire.py) binds `zwf_shell_manager_v2`
of headless Shingle with `--outputs 2` and checks, with input through
STIPC:

- `wayland-info` lists the global at version 3 and `list-methods` has
  `wayfire-shell/toggle_menu`;
- `stipc/move_cursor` to the top edge of `HEADLESS-2` sends its hotspot
  `enter` once the timeout passed and not before, then
  `proximity_changed`, and `leave` when the pointer moves away;
- a `wl_probe --fullscreen` on `HEADLESS-2` sends `enter_fullscreen` to
  that output's `zwf_output_v2` only, and `leave_fullscreen` when it
  closes;
- `stipc/feed_key` of `KEY_LEFTMETA` alone sends `toggle_menu` to the
  active output, with `KEY_A` in between it does not, and the IPC method
  sends it to the output it names.

Headless Shingle has no frontend to run a move, so
`zwf_surface_v2.interactive_move` is checked against offscreen Shingle
on lavapipe in the same namespace: the client maps a floating toplevel,
STIPC presses the button on it, the client asks for the move, and STIPC
motion and the release move the view by the motion. A tap of
`KEY_LEFTMETA` there takes the frontend's path to `toggle_menu`.

Signals go only to Shingle and wl_probe, through run.py's
`signal_checked`.
"""

import json
import subprocess
import time

import wayland_wire

# How long the hotspot waits, and how long a check waits for something
# that must not happen.
HOTSPOT_TIMEOUT_MS = 500
QUIET_SECONDS = 0.5

def shell_config(run):
  """Two outputs side by side, Xwayland off, windows floating."""
  config = json.loads(run.multi_config())
  config['tiling'] = {'tile_by_default': 'none'}
  config['animate'] = {'open_animation': 'none', 'close_animation': 'none'}
  return json.dumps(config)

def output_rects(shingle):
  """Each output's layout rectangle (x, y, width, height), by name."""
  rects = {}
  for output in shingle.ipc.call('window-rules/list-outputs'):
    geometry = output.get('geometry', {})
    rects[output['name']] = tuple(
        geometry.get(key) for key in ('x', 'y', 'width', 'height'))
  return rects

def wayland_info(shingle):
  """What `wayland-info` prints, or ''."""
  try:
    done = subprocess.run(['wayland-info'],
                          env=shingle.client_environment(),
                          capture_output=True, text=True, timeout=10)
  except (OSError, subprocess.TimeoutExpired):
    return ''
  return done.stdout if done.returncode == 0 else ''

def global_version(info, interface):
  """The version `wayland-info` lists for `interface`, or None."""
  for line in info.splitlines():
    if f"'{interface}'" in line and 'version:' in line:
      return int(line.rsplit('version:', 1)[1].split()[0].strip(','))
  return None

def tap_super(shingle, between=None):
  """Presses and releases KEY_LEFTMETA through STIPC, with key `between`
  tapped while it is held."""
  call = shingle.ipc.call
  call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': True})
  if between:
    call('stipc/feed_key', {'key': between, 'state': True})
    call('stipc/feed_key', {'key': between, 'state': False})
  call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': False})

def global_checks(results, shingle):
  """The global in `wayland-info` and the method in `list-methods`."""
  info = wayland_info(shingle)
  version = global_version(info, 'zwf_shell_manager_v2')
  results.check('WG-4: wayland-info lists zwf_shell_manager_v2 version 3',
                version == 3, f'version {version}')
  methods = shingle.ipc.call('list-methods').get('methods', [])
  results.check('WG-4: list-methods has wayfire-shell/toggle_menu (86 '
                'methods)', 'wayfire-shell/toggle_menu' in methods
                and len(methods) == 86, f'{len(methods)} methods')

def hotspot_checks(results, shingle, client, shell_outputs, rects):
  """An edge hotspot of HEADLESS-2 enters after its timeout and leaves."""
  x, y, width, _ = rects['HEADLESS-2']
  hotspot = client.request(shell_outputs['HEADLESS-2'], 'create_hotspot',
                           wayland_wire.EDGE_TOP, 10, HOTSPOT_TIMEOUT_MS,
                           wayland_wire.NewId('zwf_hotspot_v2'))
  client.roundtrip()
  start = time.monotonic()
  shingle.ipc.call('stipc/move_cursor', {'x': x + width // 2, 'y': y + 3})
  client.pump(0.3)
  early = client.names_of(hotspot)
  entered = client.wait_for(hotspot, 'enter', timeout=5)
  waited = time.monotonic() - start
  results.check('WG-4: the pointer on the top edge of HEADLESS-2 enters its '
                'hotspot after the 500 ms timeout',
                not early and entered and waited >= 0.45,
                f'early {early} waited {waited:.3f} s')
  client.pump(0.1)
  events = client.events_of(hotspot)
  results.check('WG-4: proximity_changed follows enter with the distance',
                events[1:2] == [('proximity_changed', [3])], str(events))
  count = len(events)
  shingle.ipc.call('stipc/move_cursor', {'x': x + width // 2, 'y': y + 300})
  left = client.wait_for(hotspot, 'leave', timeout=5, after=count)
  results.check('WG-4: moving away from the edge leaves the hotspot', left,
                str(client.events_of(hotspot)))
  # Another output's edge is not this hotspot.
  shingle.ipc.call('stipc/move_cursor', {'x': rects['HEADLESS-1'][2] // 2,
                                         'y': 3})
  client.pump(1.0)
  results.check('WG-4: the top edge of HEADLESS-1 does not enter it',
                client.names_of(hotspot).count('enter') == 1,
                str(client.names_of(hotspot)))

def fullscreen_checks(results, shingle, client, shell_outputs, rects,
                      wl_probe):
  """A fullscreen probe on HEADLESS-2 is announced there only."""
  first, second = shell_outputs['HEADLESS-1'], shell_outputs['HEADLESS-2']
  x, y, width, height = rects['HEADLESS-2']
  # New windows map on the active output; a click makes HEADLESS-2 it.
  shingle.ipc.call('stipc/move_cursor',
                   {'x': x + width // 2, 'y': y + height // 2})
  shingle.ipc.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'full'})
  probe = shingle.client([wl_probe, 'wg4-full', '--fullscreen'], 'wl_probe')
  try:
    requested = probe.wait_event('fullscreen') is not None
    entered = client.wait_for(second, 'enter_fullscreen', timeout=5)
    client.pump(QUIET_SECONDS)
    view = shingle.views().get('wg4-full', {})
    results.check('WG-4: a wl_probe going fullscreen on HEADLESS-2 sends '
                  'enter_fullscreen to that output only',
                  requested and entered and view.get('output-name')
                  == 'HEADLESS-2' and not client.names_of(first),
                  f'{probe.events()} {view.get("output-name")} '
                  f'HEADLESS-1 {client.names_of(first)} '
                  f'HEADLESS-2 {client.names_of(second)}')
  finally:
    probe.stop()
  left = client.wait_for(second, 'leave_fullscreen', timeout=5)
  results.check('WG-4: leave_fullscreen follows when the probe closes',
                left and not client.names_of(first),
                f'HEADLESS-1 {client.names_of(first)} '
                f'HEADLESS-2 {client.names_of(second)}')

def toggle_menu_checks(results, shingle, client, shell_outputs):
  """`toggle_menu` from a STIPC super tap and from the IPC method."""
  first, second = shell_outputs['HEADLESS-1'], shell_outputs['HEADLESS-2']
  active = shingle.ipc.call('window-rules/get-focused-output').get(
      'info', {}).get('name')
  target = shell_outputs.get(active)
  other = first if target == second else second

  def menus(output):
    return client.names_of(output).count('toggle_menu')

  tap_super(shingle)
  arrived = client.wait_for(target, 'toggle_menu', timeout=5)
  client.pump(QUIET_SECONDS)
  results.check('WG-4: STIPC KEY_LEFTMETA alone sends toggle_menu to the '
                'active output', arrived and menus(target) == 1
                and menus(other) == 0,
                f'active {active}: {menus(target)}, other {menus(other)}')
  tap_super(shingle, between='KEY_A')
  client.pump(1.0)
  results.check('WG-4: KEY_LEFTMETA with KEY_A in between sends no '
                'toggle_menu', menus(target) == 1 and menus(other) == 0,
                f'{menus(target)} {menus(other)}')
  ids = shingle.ipc.call('window-rules/list-outputs')
  first_id = next(output['id'] for output in ids
                  if output['name'] == 'HEADLESS-1')
  seen = len(client.names_of(first))
  reply = shingle.ipc.call('wayfire-shell/toggle_menu',
                           {'output_id': first_id})
  arrived = client.wait_for(first, 'toggle_menu', timeout=5, after=seen)
  results.check('WG-4: wayfire-shell/toggle_menu sends toggle_menu to the '
                'output it names', reply == {'result': 'ok'} and arrived,
                f'{reply} {client.names_of(first)}')

def headless_checks(results, run, binary, wl_probe):
  """The checks of headless Shingle with two outputs."""
  shingle = run.Headless(binary, 'wayfire-shell', shell_config(run), 'yaml',
                         backend=('--headless', '--outputs', '2'),
                         extra_environment={run.NAMESPACE_VARIABLE: '1'})
  client = None
  try:
    global_checks(results, shingle)
    client = wayland_wire.Client(shingle.wayland_display())
    shell = client.bind('zwf_shell_manager_v2', 3)
    outputs = client.outputs()
    shell_outputs = {
        name: client.request(shell, 'get_wf_output', output,
                             wayland_wire.NewId('zwf_output_v2'))
        for name, output in outputs.items()}
    client.roundtrip()
    rects = output_rects(shingle)
    results.check('WG-4: the client has a zwf_output_v2 for both outputs',
                  sorted(shell_outputs) == ['HEADLESS-1', 'HEADLESS-2'],
                  str(sorted(shell_outputs)))
    hotspot_checks(results, shingle, client, shell_outputs, rects)
    if wl_probe:
      fullscreen_checks(results, shingle, client, shell_outputs, rects,
                        wl_probe)
    toggle_menu_checks(results, shingle, client, shell_outputs)
  finally:
    if client:
      client.close()
    status = shingle.stop()
  results.check('WG-4: SIGTERM exits headless with status 0', status == 0,
                str(status))
  return shingle.log_text()

def offscreen_checks(results, run, binary):
  """interactive_move and the frontend's toggle_menu, with offscreen
  Shingle on lavapipe."""
  shingle = run.Headless(binary, 'wayfire-shell-offscreen',
                         shell_config(run), 'yaml',
                         backend=('--backend', 'offscreen'),
                         extra_environment={
                             run.NAMESPACE_VARIABLE: '1',
                             'VK_ICD_FILENAMES': run._HARNESS.LAVAPIPE,
                         })
  client = None
  try:
    client = wayland_wire.Client(shingle.wayland_display())
    shell = client.bind('zwf_shell_manager_v2', 3)
    surface = client.toplevel('wg4-move', 200, 150)
    shell_surface = client.request(shell, 'get_wf_surface', surface,
                                   wayland_wire.NewId('zwf_surface_v2'))
    output = client.outputs()['HEADLESS-1']
    shell_output = client.request(shell, 'get_wf_output', output,
                                  wayland_wire.NewId('zwf_output_v2'))
    client.roundtrip()
    view = run.wait_for(lambda: shingle.views().get('wg4-move'))
    before = run.geometry_of(view or {})
    x, y, width, height = before if all(before) else (0, 0, 0, 0)
    centre = (x + width // 2, y + height // 2)
    call = shingle.ipc.call
    call('stipc/move_cursor', {'x': centre[0], 'y': centre[1]})
    call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
    client.request(shell_surface, 'interactive_move')
    client.roundtrip()
    # The frontend takes the move on its next turn.
    client.pump(0.2)
    for step in (1, 2, 3):
      call('stipc/move_cursor', {'x': centre[0] + 50 * step,
                                 'y': centre[1] + 30 * step})
      client.pump(0.1)
    call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'release'})
    expected = (x + 150, y + 90, width, height)
    moved = run.wait_for(lambda: run.geometry_of(
        shingle.views().get('wg4-move', {})) == expected, timeout=5)
    after = run.geometry_of(shingle.views().get('wg4-move', {}))
    results.check('WG-4: interactive_move starts a move that STIPC motion '
                  'and the button release finish',
                  all(before) and moved, f'{before} -> {after}, expected '
                  f'{expected}')
    tap_super(shingle)
    arrived = client.wait_for(shell_output, 'toggle_menu', timeout=5)
    results.check('WG-4: a STIPC super tap sends toggle_menu through the '
                  'frontend (offscreen)', arrived,
                  str(client.names_of(shell_output)))
  finally:
    if client:
      client.close()
    status = shingle.stop()
  results.check('WG-4: SIGTERM exits offscreen with status 0', status == 0,
                str(status))
  return shingle.log_text()
