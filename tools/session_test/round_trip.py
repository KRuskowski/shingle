"""Real-client session round trips for tools/session_test/run.py.

Five `wl_probe` clients in a nested Shingle on lavapipe, itself a client
of a headless Shingle in the harness's namespace, are arranged in a
nested, unevenly weighted split with one floating at an exact rectangle
on the first workspace and one floating on the third. The nested Shingle
is then killed with SIGKILL and started again, and later stopped with
SIGTERM and started again; each restart must relaunch the five clients
from their saved command lines and show the same layouts and views.

With a Shingle built from `master` (`--master-binary`), the same layout
is saved by it as a version 1 session and restored by the Shingle under
test, which must show it unchanged and save it back as version 2.

Every Shingle runs with an explicit `--headless` or `--backend nested`,
and every signal goes through `signal_checked`.
"""

import json
import os
import signal
import time

APPS = ('alpha', 'bravo', 'charlie', 'delta', 'echo')
WORKSPACES = 3
# The rectangles delta (first workspace) and echo (third) float at.
DELTA_FLOATING = {'x': 40, 'y': 30, 'width': 300, 'height': 200}
ECHO_FLOATING = {'x': 100, 'y': 80, 'width': 400, 'height': 300}
# The view fields that must come back unchanged; the PID, focus time and
# activation belong to the run, not the session.
VIEW_FIELDS = ('id', 'app-id', 'title', 'geometry', 'tiled-edges',
               'fullscreen', 'minimized', 'sticky', 'mapped',
               'always-on-top', 'always-on-bottom', 'parent')

def nested_config():
  """The nested Shingle's YAML config, as JSON: sessions restored, three
  workspaces, Xwayland and config reloading off."""
  return json.dumps({
      'xwayland': {'mode': False},
      'session': {'restore': True},
      'workspaces': {'width': WORKSPACES, 'height': 1},
      'behavior': {'auto_reload': False},
  })

def tile_layout(views):
  """`simple-tile/set-layout` data for the first workspace: alpha at 3,
  then bravo over charlie (1 to 2) at 2, then delta at 1."""
  return {
      'wset-index': 1,
      'workspace': {'x': 0, 'y': 0},
      'layout': {'vertical-split': [
          {'view-id': views['alpha'], 'weight': 3},
          {'weight': 2, 'horizontal-split': [
              {'view-id': views['bravo'], 'weight': 1},
              {'view-id': views['charlie'], 'weight': 2},
          ]},
          {'view-id': views['delta'], 'weight': 1},
      ]},
  }

def view_state(views):
  """The fields of `list-views` replies that a session keeps, by id."""
  return sorted(({field: view.get(field) for field in VIEW_FIELDS}
                 for view in views), key=lambda view: view['id'])

def session_trees(session):
  """The workspaces of each output or workspace set of a saved session,
  in order, whichever version wrote it."""
  records = session.get('wsets', session.get('outputs', []))
  return [record.get('workspaces') for record in records]

def session_roots(session):
  """Each workspace's id and root split, per output or set: the tiles,
  weights and leaf states, without the focus and stacking that a restart
  may change."""
  return [[(workspace.get('workspace'), workspace.get('root'))
           for workspace in workspaces or []]
          for workspaces in session_trees(session)]

def session_floating(session, window):
  """The floating rectangle of `window` in a saved session, or None."""
  for workspaces in session_trees(session):
    for workspace in workspaces or []:
      found = _floating(workspace.get('root', {}), window)
      if found:
        return found
  return None

def _floating(split, window):
  for child in split.get('children', []):
    node = child.get('node', {})
    if node.get('kind') == 'leaf' and node.get('window') == window:
      return node.get('state', {}).get('floating')
    if node.get('kind') == 'split':
      found = _floating(node, window)
      if found:
        return found
  return None

def read_session(state):
  """The session saved under the state directory `state`, or None."""
  try:
    with open(os.path.join(state, 'shingle', 'session.json')) as file:
      return json.load(file)
  except (OSError, ValueError):
    return None

class RoundTrip:
  """Nested Shingles started one after another on one state directory,
  inside one headless Shingle, with the harness's helpers."""

  def __init__(self, harness, results, outer, state):
    self.harness = harness
    self.results = results
    self.outer = outer
    self.state = state
    self.runs = 0
    self.probes = []

  def start(self, binary):
    """Starts a nested Shingle from `binary` on the state directory."""
    self.runs += 1
    os.makedirs(self.state, exist_ok=True)
    return self.harness.Headless(
        binary, f'round-trip-{os.path.basename(self.state)}-{self.runs}',
        nested_config(), 'yaml', backend=('--backend', 'nested'),
        extra_environment={
            'WAYLAND_DISPLAY': self.outer.wayland_display(),
            'VK_ICD_FILENAMES': self.harness.LAVAPIPE,
            'XDG_STATE_HOME': self.state,
            # Clients relaunched from the session inherit it, and
            # wl_probe runs only in a harness namespace.
            self.harness.NAMESPACE_VARIABLE: '1',
        })

  def map_clients(self, nested, wl_probe):
    """Starts the five probes; returns their view ids by app id."""
    self.probes = [self.harness.Probe(wl_probe, nested, app)
                   for app in APPS]
    return self.harness.wait_for(
        lambda: self.harness.nested_views(nested, len(APPS)), timeout=30)

  def arrange(self, nested, views):
    """Builds the layout; returns the IPC errors."""
    calls = [
        ('simple-tile/set-layout', tile_layout(views)),
        ('window-rules/configure-view',
         {'id': views['delta'], 'geometry': DELTA_FLOATING}),
        ('vswitch/send-view', {'view-id': views['echo'], 'x': 2, 'y': 0}),
        ('window-rules/configure-view',
         {'id': views['echo'], 'geometry': ECHO_FLOATING}),
    ]
    errors = []
    for method, data in calls:
      reply = nested.ipc.call(method, data)
      if reply.get('result') != 'ok':
        errors.append((method, reply))
    return errors

  def observe(self, nested):
    """What IPC shows once the clients have settled: every workspace's
    `get-layout` and the kept fields of `list-views`."""
    def snapshot():
      layouts = [nested.ipc.call('simple-tile/get-layout', {
          'wset-index': 1, 'workspace': {'x': x, 'y': 0}})
                 for x in range(WORKSPACES)]
      return {'layouts': layouts,
              'views': view_state(nested.ipc.call('window-rules/list-views'))}
    previous = snapshot()
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
      time.sleep(0.3)
      current = snapshot()
      if current == previous:
        return current
      previous = current
    return previous

  def wait_saved(self, views):
    """Waits until the session on disk has the five windows and echo's
    rectangle, and has not changed for half a second."""
    def ready():
      session = read_session(self.state)
      return (session is not None
              and len(session.get('windows', [])) == len(APPS)
              and session_floating(session, views['echo'])
              == ECHO_FLOATING)
    if not self.harness.wait_for(ready, timeout=10):
      return False
    previous = read_session(self.state)
    while True:
      time.sleep(0.5)
      current = read_session(self.state)
      if current == previous:
        return True
      previous = current

  def stop_probes(self):
    """Stops the probes the harness started."""
    for probe in self.probes:
      probe.stop()
    self.probes = []

  def kill(self, nested):
    """SIGKILL through `signal_checked`; returns whether it was sent."""
    nested.ipc.close()
    sent = self.harness.signal_checked(nested.process.pid, signal.SIGKILL,
                                       'shingle')
    nested.process.wait()
    nested.log.close()
    return sent

def round_trip_checks(harness, results, binary, wl_probe):
  """A layout survives SIGKILL and a restart, then SIGTERM and a restart."""
  outer = harness.Headless(binary, 'round-trip-outer', json.dumps({
      'output': {'mode': '1280x720@60'},
      'xwayland': {'mode': False},
      'session': {'restore': False},
      'workspaces': {'width': 1, 'height': 1},
  }), 'yaml')
  trip = RoundTrip(harness, results, outer, '/tmp/round-trip-state')
  nested = None
  log = ''
  try:
    nested = trip.start(binary)
    views = trip.map_clients(nested, wl_probe)
    results.check('round trip: five clients map', views, str(views))
    if not views:
      return nested.log_text()
    errors = trip.arrange(nested, views)
    results.check('round trip: the layout is built over IPC', not errors,
                  str(errors))
    before = trip.observe(nested)
    tiled = [view for view in before['views'] if view['tiled-edges']]
    results.check('round trip: three tiles and two floating views',
                  len(tiled) == 3, str(before['views']))
    results.check('round trip: the session is saved in the background',
                  trip.wait_saved(views), str(read_session(trip.state)))
    session = read_session(trip.state) or {}
    results.check('round trip: it is written as version 2 with one set',
                  session.get('version') == 2
                  and [wset.get('wset') for wset in session.get('wsets', [])]
                  == [1], str(session.get('version')))
    results.check('round trip: SIGKILL ends the first run', trip.kill(nested))
    log = nested.log_text()
    nested = None
    trip.stop_probes()
    restarts = (('after SIGKILL', 'SIGTERM'), ('after SIGTERM', None))
    for restart, stop in restarts:
      nested = trip.start(binary)
      restored = harness.wait_for(
          lambda: harness.nested_views(nested, len(APPS)), timeout=30)
      results.check(f'round trip: {restart} the five clients are '
                    'relaunched and claim their windows',
                    restored == views, f'{restored} {views}')
      after = trip.observe(nested)
      results.check(f'round trip: {restart} every layout is identical',
                    after['layouts'] == before['layouts'],
                    f'{after["layouts"]} != {before["layouts"]}')
      results.check(f'round trip: {restart} every view is identical',
                    after['views'] == before['views'],
                    f'{after["views"]} != {before["views"]}')
      if stop:
        status = nested.stop()
        log = nested.log_text()
        nested = None
        results.check('round trip: SIGTERM exits with status 0',
                      status == 0, str(status))
    return log
  finally:
    trip.stop_probes()
    if nested:
      log = nested.log_text()
      nested.stop()
    outer_status = outer.stop()
    results.check('round trip: SIGTERM exits headless with status 0',
                  outer_status == 0, str(outer_status))

def version_1_checks(harness, results, binary, master_binary, wl_probe):
  """A session `master` saved as version 1 restores unchanged and is
  saved back as version 2."""
  outer = harness.Headless(binary, 'version-1-outer', json.dumps({
      'output': {'mode': '1280x720@60'},
      'xwayland': {'mode': False},
      'session': {'restore': False},
      'workspaces': {'width': 1, 'height': 1},
  }), 'yaml')
  trip = RoundTrip(harness, results, outer, '/tmp/version-1-state')
  nested = None
  try:
    nested = trip.start(master_binary)
    views = trip.map_clients(nested, wl_probe)
    results.check('version 1: five clients map in master', views, str(views))
    if not views:
      return nested.log_text()
    errors = trip.arrange(nested, views)
    results.check('version 1: master builds the layout', not errors,
                  str(errors))
    before = trip.observe(nested)
    status = nested.stop()
    nested = None
    trip.stop_probes()
    results.check('version 1: master exits with status 0 on SIGTERM',
                  status == 0, str(status))
    saved = read_session(trip.state) or {}
    results.check('version 1: master saved a version 1 session',
                  saved.get('version') == 1 and 'outputs' in saved
                  and session_floating(saved, views['echo'])
                  == ECHO_FLOATING, json.dumps(saved)[:300])
    nested = trip.start(binary)
    restored = harness.wait_for(
        lambda: harness.nested_views(nested, len(APPS)), timeout=30)
    results.check('version 1: the clients are relaunched and claim their '
                  'windows', restored == views, f'{restored} {views}')
    after = trip.observe(nested)
    results.check('version 1: every layout is as master showed it',
                  after['layouts'] == before['layouts'],
                  f'{after["layouts"]} != {before["layouts"]}')
    results.check('version 1: every view is as master showed it',
                  after['views'] == before['views'],
                  f'{after["views"]} != {before["views"]}')
    status = nested.stop()
    log = nested.log_text()
    nested = None
    results.check('version 1: SIGTERM exits with status 0', status == 0,
                  str(status))
    resaved = read_session(trip.state) or {}
    results.check('version 1: the session is saved back as version 2 with '
                  'the same trees',
                  resaved.get('version') == 2
                  and session_roots(resaved) == session_roots(saved),
                  f'{session_roots(resaved)} != {session_roots(saved)}')
    return log
  finally:
    trip.stop_probes()
    if nested:
      nested.stop()
    outer_status = outer.stop()
    results.check('version 1: SIGTERM exits headless with status 0',
                  outer_status == 0, str(outer_status))
