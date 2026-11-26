"""The real-client check that minimizing the focused view leaves the
focus order as Wayfire leaves it, run inside run.py's namespace.

Three `wl_probe` clients map as alpha, bravo and charlie, so that
charlie is focused and alpha the least recently. Then IPC's
`wm-actions/set-minimized` minimizes charlie and STIPC taps Alt+Tab
once:

- `minimize-focus`: in offscreen Shingle, bravo takes the focus but
  keeps its `last-focus-timestamp`, so the minimized charlie stays the
  most recently focused, as Wayfire's `seat_t::refocus` gives the focus
  without stamping it (wayfire:src/core/seat/seat.cpp; only
  `set_active_node` stamps); Alt+Tab, whose switcher orders by those
  timestamps, then selects bravo again and leaves charlie minimized.
  Headless Wayfire with the same plugins (wayfire_reference.py) must
  show the same focus, order and outcome.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py and `Wayfire.stop`, to children of this process
inside the PID namespace.
"""

import json
import time

import checks
import wayfire_reference

xw = checks.xw
GROUPS = ('minimize-focus',)
APPS = ('alpha', 'bravo', 'charlie')
INI = """[core]
plugins = switcher wm-actions place
xwayland = false
vwidth = 1
vheight = 1

[workarounds]
auto_reload_config = false

[autostart]
autostart_wf_shell = false
"""
WAYFIRE_INI = """[core]
plugins = ipc ipc-rules stipc switcher wm-actions place
xwayland = false
vwidth = 1
vheight = 1

[autostart]
autostart_wf_shell = false
"""
# What Wayfire 0.9 did on the test host, and what Shingle must do.
EXPECTED = {
    'mapped': ('charlie', ['charlie', 'bravo', 'alpha']),
    'minimized': ('bravo', ['charlie', 'bravo', 'alpha']),
    'switched': ('bravo', True),
}

def by_focus_time(views):
  """The app ids of `views` (`list-views`), most recently focused
  first."""
  ordered = sorted(views, key=lambda view: -view['last-focus-timestamp'])
  return [view['app-id'] for view in ordered]

def focused_app(compositor):
  """The app id of the focused view, or None."""
  reply = compositor.call('window-rules/get-focused-view')
  return (reply.get('info') or {}).get('app-id')

def key(compositor, name, pressed):
  """One `stipc/feed_key`."""
  reply = compositor.call('stipc/feed_key', {'key': name, 'state': pressed})
  if reply.get('result') != 'ok':
    raise RuntimeError(f'stipc/feed_key: {json.dumps(reply)}')

def observe(compositor, wl_probe):
  """Maps the three probes, minimizes charlie and taps Alt+Tab; returns
  what was seen at each step, keyed as EXPECTED, and the clients."""
  clients = []
  seen = {}
  for app_id in APPS:
    clients.append(checks.Client(compositor, [wl_probe, app_id], app_id,
                                 program='wl_probe'))
    xw.wait_for(lambda: compositor.view(app_id) is not None
                and focused_app(compositor) == app_id, timeout=15)
    # Distinct focus timestamps.
    time.sleep(0.2)
  seen['mapped'] = (focused_app(compositor), by_focus_time(compositor.views()))
  charlie = compositor.view('charlie')
  if charlie is None:
    return seen, clients
  compositor.call('wm-actions/set-minimized',
                  {'view_id': charlie['id'], 'state': True})
  xw.wait_for(lambda: compositor.view('charlie')['minimized'], timeout=10)
  xw.wait_for(lambda: focused_app(compositor) is not None, timeout=5)
  seen['minimized'] = (focused_app(compositor),
                       by_focus_time(compositor.views()))
  key(compositor, 'KEY_LEFTALT', True)
  key(compositor, 'KEY_TAB', True)
  key(compositor, 'KEY_TAB', False)
  time.sleep(0.3)
  key(compositor, 'KEY_LEFTALT', False)
  # The switcher focuses its selection as its thumbnails slide back.
  time.sleep(1.5)
  seen['switched'] = (focused_app(compositor),
                      compositor.view('charlie')['minimized'])
  return seen, clients

def minimize_focus_checks(results, binary, wl_probe, captures):
  """Shingle's focus order after a minimize, and Wayfire's."""
  shingle = checks.Shingle(binary, 'minimize-focus', None, ini=INI)
  clients = []
  seen = {}
  try:
    seen, clients = observe(shingle, wl_probe)
    for step, label in (
        ('mapped', 'the probes map, charlie focused last'),
        ('minimized', 'minimizing charlie focuses bravo without making it '
         'the most recently focused'),
        ('switched', 'one Alt+Tab then selects bravo and leaves charlie '
         'minimized')):
      results.check(f'minimize-focus: {label}',
                    seen.get(step) == EXPECTED[step],
                    f'{seen.get(step)}, expected {EXPECTED[step]}')
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    results.check('minimize-focus: SIGTERM exits offscreen Shingle with '
                  'status 0', status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')
  try:
    wayfire = wayfire_reference.Wayfire('minimize-focus-wayfire',
                                        WAYFIRE_INI)
  except RuntimeError as error:
    print(f'INFO minimize-focus: no Wayfire reference: {error}', flush=True)
    return
  clients = []
  try:
    reference, clients = observe(wayfire, wl_probe)
  finally:
    for client in clients:
      client.stop()
    wayfire.stop()
  results.check('minimize-focus: headless Wayfire shows the same focus, '
                'order and Alt+Tab outcome as Shingle',
                reference == seen, f'Wayfire {reference}, Shingle {seen}')

CHECKS = {
    'minimize-focus': minimize_focus_checks,
}
