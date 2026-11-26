"""WG-2's real-client checks of offscreen Shingle under a wayfire.ini, run
inside run.py's namespace.

docs/design/wayfire-gaps.md, WG-2. Shingle reads a synthetic wayfire.ini,
whose `[core] plugins` line decides what is loaded, and input comes
through STIPC:

- `wayfire-plugins`: with `scale` left out of the list, `stipc/feed_key`
  of `super+p` reaches a focused `wl_probe` and no
  `plugin-activation-state-changed` event follows; with `scale` listed
  the event follows and the probe gets no key. With `simple-tile` left
  out, a second `wl_probe` floats and `simple-tile/get-layout` holds no
  view; with it listed both tile.
- `wayfire-shell`: stand-in `wf-panel` and `wf-background` scripts, on
  the PATH of this namespace only, are started once each with
  `autostart_wf_shell = true`; an autostart entry that runs `wf-panel`
  leaves only `wf-background` to the rule; with
  `autostart_wf_shell = false` neither starts. The real wf-shell is never
  run: the stand-ins only record that they started and exit.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import os
import time

import checks

xw = checks.xw
GROUPS = ('wayfire-plugins', 'wayfire-shell')
# evdev code of KEY_P, as wl_probe reports keys.
KEY_P = 25
# What every wayfire.ini here holds besides its plugin list: no X
# display, no reload and no animations to wait for.
BASE_INI = """[core]
plugins = {plugins}
xwayland = false
vwidth = 1
vheight = 1

[workarounds]
auto_reload_config = false

[simple-tile]
animation_duration = 0

[autostart]
autostart_wf_shell = false
"""
# How long a check waits for something that must not happen.
QUIET_SECONDS = 1.0

def ini(plugins, autostart=''):
  """A wayfire.ini loading `plugins`, with `autostart` lines replacing
  the `[autostart]` section's when given."""
  text = BASE_INI.format(plugins=plugins)
  if autostart:
    text = text.replace('autostart_wf_shell = false\n', autostart)
  return text

def layout_views(layout):
  """The view ids anywhere in a `simple-tile/get-layout` layout."""
  if isinstance(layout, dict):
    found = [layout['view-id']] if 'view-id' in layout else []
    for value in layout.values():
      found.extend(layout_views(value))
    return found
  if isinstance(layout, list):
    return [view for item in layout for view in layout_views(item)]
  return []

def tiled_views(shingle):
  """The views of the first workspace's tiling layout."""
  reply = shingle.call('simple-tile/get-layout',
                       {'wset-index': 1, 'workspace': {'x': 0, 'y': 0}})
  return layout_views(reply.get('layout'))

def key_events(probe):
  """The `key` events a probe printed for KEY_P."""
  return [event for event in checks.probe_events(probe.output(), 'key')
          if event.get('key') == KEY_P]

def super_p(shingle):
  """`super+p` through STIPC."""
  for key, pressed in (('KEY_LEFTMETA', True), ('KEY_P', True),
                       ('KEY_P', False), ('KEY_LEFTMETA', False)):
    shingle.call('stipc/feed_key', {'key': key, 'state': pressed})

def activation(watcher, timeout):
  """The next `plugin-activation-state-changed` event, or None."""
  try:
    return watcher.receive(timeout)
  except TimeoutError:
    return None

def scale_run(results, binary, wl_probe, plugins, name):
  """Two probes under `plugins`; returns what super+p and tiling did, as
  (key events, activation event, tiled view count, view count)."""
  shingle = checks.Shingle(binary, name, None, ini=ini(plugins))
  probes = []
  try:
    for app_id in ('first', 'second'):
      probes.append(checks.Client(shingle, [wl_probe, app_id], app_id,
                                  program='wl_probe'))
      mapped = xw.wait_for(lambda app_id=app_id: shingle.view(app_id),
                           timeout=15)
      if mapped is None:
        results.check(f'{name}: wl_probe {app_id} maps', False,
                      checks.tail(probes[-1].output()))
        return None
    second = probes[-1]
    xw.wait_for(lambda: checks.probe_events(second.output(), 'enter'),
                timeout=10)
    watcher = xw.Ipc(shingle.ipc_path)
    try:
      watcher.call('window-rules/events/watch',
                   {'events': ['plugin-activation-state-changed']})
      super_p(shingle)
      event = activation(watcher, QUIET_SECONDS * 3)
      xw.wait_for(lambda: key_events(second), timeout=QUIET_SECONDS)
      keys = key_events(second)
      if event is not None and event.get('state') is True:
        shingle.call('scale/toggle')
        activation(watcher, QUIET_SECONDS * 3)
    finally:
      watcher.close()
    views = [view for view in shingle.views() if view.get('mapped', True)]
    return keys, event, len(tiled_views(shingle)), len(views)
  finally:
    for probe in probes:
      probe.stop()
    status = shingle.stop()
    results.check(f'{name}: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

def plugin_checks(results, binary, wl_probe, _captures):
  """super+p and tiling with and without scale and simple-tile."""
  run = scale_run(results, binary, wl_probe, 'vswitch', 'wf-unloaded')
  if run is not None:
    keys, event, tiled, views = run
    results.check(
        'wayfire-plugins: without scale, super+p reaches the wl_probe',
        [event.get('pressed') for event in keys] == [True, False],
        json.dumps(keys))
    results.check(
        'wayfire-plugins: without scale, no plugin-activation-state-changed '
        'follows', event is None, json.dumps(event))
    results.check(
        'wayfire-plugins: without simple-tile the second wl_probe floats '
        'and simple-tile/get-layout is empty',
        views == 2 and tiled == 0, f'{tiled} tiled of {views}')
  run = scale_run(results, binary, wl_probe, 'vswitch scale simple-tile',
                  'wf-loaded')
  if run is not None:
    keys, event, tiled, views = run
    results.check(
        'wayfire-plugins: with scale, super+p activates Scale',
        event is not None and event.get('plugin') == 'scale'
        and event.get('state') is True, json.dumps(event))
    results.check('wayfire-plugins: with scale, the wl_probe gets no key',
                  not keys, json.dumps(keys))
    results.check('wayfire-plugins: with simple-tile both wl_probes tile',
                  views == 2 and tiled == 2, f'{tiled} tiled of {views}')

def write_stand_ins(directory):
  """`wf-panel` and `wf-background` scripts in `directory`/bin that each
  append a line to `directory`/NAME.started and exit. Returns the bin
  directory."""
  bin_directory = os.path.join(directory, 'bin')
  os.makedirs(bin_directory, exist_ok=True)
  for name in ('wf-panel', 'wf-background'):
    path = os.path.join(bin_directory, name)
    marker = os.path.join(directory, f'{name}.started')
    with open(path, 'w') as file:
      file.write(f'#!/bin/sh\necho started >> {marker}\n')
    os.chmod(path, 0o755)
  return bin_directory

def started(directory, name):
  """How often stand-in `name` started."""
  try:
    with open(os.path.join(directory, f'{name}.started')) as file:
      return len(file.read().splitlines())
  except FileNotFoundError:
    return 0

def shell_run(results, binary, name, autostart):
  """Shingle with the `[autostart]` lines `autostart`; returns how often
  each stand-in started, as (wf-panel, wf-background)."""
  # Shingle's directory must not exist before it starts; the stand-ins
  # live beside it in the namespace's private /tmp.
  directory = os.path.join('/tmp', f'{name}-stand-ins')
  bin_directory = write_stand_ins(directory)
  path = f'{bin_directory}:{os.environ.get("PATH", "/usr/bin:/bin")}'
  shingle = checks.Shingle(binary, name, None,
                           ini=ini('autostart', autostart),
                           extra_environment={'PATH': path})
  try:
    xw.wait_for(lambda: started(directory, 'wf-background')
                + started(directory, 'wf-panel') >= 2, timeout=5)
    # Anything started twice would have by now.
    time.sleep(QUIET_SECONDS)
    return started(directory, 'wf-panel'), started(directory, 'wf-background')
  finally:
    status = shingle.stop()
    results.check(f'{name}: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')

def shell_checks(results, binary, _wl_probe, _captures):
  """autostart_wf_shell's three cases with stand-in wf-shell clients."""
  counts = shell_run(results, binary, 'wf-shell-on',
                     'autostart_wf_shell = true\n')
  results.check('wayfire-shell: autostart_wf_shell = true starts wf-panel '
                'and wf-background once each', counts == (1, 1),
                f'started {counts}')
  counts = shell_run(results, binary, 'wf-shell-entry',
                     'autostart_wf_shell = true\npanel = wf-panel\n')
  results.check('wayfire-shell: an autostart entry running wf-panel leaves '
                'only wf-background to the rule', counts == (1, 1),
                f'started {counts}')
  counts = shell_run(results, binary, 'wf-shell-off',
                     'autostart_wf_shell = false\n')
  results.check('wayfire-shell: autostart_wf_shell = false starts neither',
                counts == (0, 0), f'started {counts}')

CHECKS = {
    'wayfire-plugins': plugin_checks,
    'wayfire-shell': shell_checks,
}
