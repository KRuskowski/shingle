#!/usr/bin/env python3
"""Real-client output-management checks for headless Shingle.

Runs headless Shingle and `wlr-randr` inside an unprivileged user, mount,
network and PID namespace (`unshare -U -r -m -n -p -f --mount-proc`), as
tools/xwayland_test/run.py does: a private directory is bind-mounted
over `/tmp`, `/dev/dri` and `/dev/input` are covered with empty tmpfs
mounts, and the runtime, config, state and home directories are private.
The host's `WAYLAND_DISPLAY`, `WAYLAND_SOCKET`, `DISPLAY` and
`WAYFIRE_SOCKET` are removed from every environment, so no client can
reach the host session, and Shingle runs with `--headless` and Xwayland
off.

Usage, from the repository root and outside any sandbox that forbids
user namespaces:

  cargo build -p shingle-bin -p shingle-lock
  cargo build -p shingle --example wl_probe
  tools/output_test/run.py [--binary target/debug/shingle]
      [--wl-probe target/debug/examples/wl_probe]
      [--lock target/debug/shingle-lock] [--keep] [--checks NAME ...]

It prints one line per check and exits 0 when all pass. The checks are
the parts of NB-9 (docs/design/native-backend.md) that are not
device-specific: the mode list and current mode clients see, custom
modes, tests that change nothing, placement, refusing to turn the only
output off, explicit configured modes with their refresh rate, Wayfire
`[output:NAME]` sections kept apart, and named YAML entries. WG-1
(docs/design/wayfire-gaps.md) adds a start with the WCM-style
crates/shingle-config/testdata/wayfire-wcm.ini: Shingle starts, logs
each of its warnings once with its line, and `wayfire/get-config-option`
returns the imported `core/background_color` and
`simple-tile/inner_gap_size`; since WG-2 `list-config-options` lists
every option of crates/shingle-config/data/wayfire-options.tsv and an
option of a plugin the file does not load reports the file's text.
WG-3 runs `env` through `stipc/run`: under a YAML config the child gets
`XDG_CURRENT_DESKTOP=wayfire`, the configured `XCURSOR_THEME` and
`XCURSOR_SIZE`, Shingle's sockets and no `WAYFIRE_CONFIG_FILE` (a host
one is removed), and a size `set-config-options` changes; under a
Wayfire file it gets the file as `WAYFIRE_CONFIG_FILE` and keeps the
`XDG_CURRENT_DESKTOP` and cursor theme Shingle itself was started with.
Headless
Shingle does not reload its config, so a nested Shingle on lavapipe,
running as a client of a headless one inside the namespace, checks the
configured refresh rate and that a reload applies.

With `--outputs 2` (MO-2, docs/design/multi-output.md) the checks are
the parts of the output registry clients see: `wlr-randr` and
`wayland-info` list both outputs at their automatic and at configured
fixed positions, a head moved with `wlr-randr --pos` moves, outputs
created with `wayfire/create-headless-output` appear to clients and IPC
watchers and go again, a watcher gets `view-mapped` from headless
Shingle, and a `wl_probe` window restored from a session
whose set belongs to `HEADLESS-3` waits on the active output, moves to
`HEADLESS-3` when IPC creates it and back when IPC destroys it. That
needs `cargo build -p shingle --example wl_probe`.

The MO-3 checks (docs/design/multi-output.md) run client protocols
against two outputs, `HEADLESS-2` at scale 2: `grim -o` of each output
in its own physical pixels and `grim -g` across both; a `wl_probe
--layer HEADLESS-2` panel on that output, closed when `wlr-randr
--output HEADLESS-2 --off` turns the output off, and another on an
output IPC created, closed when IPC destroys it; `--on` and `--pos`;
`wl_probe --report-outputs` entering and leaving outputs with their
preferred scales as its output goes and comes back; and `shingle-lock`
(`cargo build -p shingle-lock`) locking both outputs and unlocking
again. For the unlock a private `/etc/pam.d` with a permit-all `shingle`
service is bind-mounted in the namespace, so any typed password passes;
the host's PAM files are not touched.

The MO-4 checks (docs/design/multi-output.md) run focus and moving
windows between two outputs, `HEADLESS-2` at 1024x768, with `wl_probe
--report-outputs` windows: `stipc/move_cursor` onto `HEADLESS-2` leaves
the active output alone and `stipc/feed_button` there makes it active
and focuses its window; `stipc/feed_key` with oswitch's `super+o` and
`super+shift+o` moves focus and the focused window; `configure-view`
with `output_id` moves a tiled, a floating and a fullscreen window with
Wayfire's geometry; and `wsets/set-output-wset` swaps the two outputs'
sets in two calls. IPC watchers see `output-gain-focus`,
`view-set-output`, `view-wset-changed` and `output-wset-changed`.

WG-4 (docs/design/wayfire-gaps.md) checks `zwf_shell_manager_v2` with
a raw-protocol client (wayland_wire.py, wayfire_shell_checks.py):
headless Shingle with two outputs for the global, hotspots, fullscreen
events of a `wl_probe --fullscreen` and `toggle_menu` from STIPC keys
and IPC, and offscreen Shingle on lavapipe for `interactive_move` and
the frontend's `toggle_menu`. `--checks NAME ...` runs only the groups
of those function names, such as `wg4_headless_checks`.

Signals go only to Shingle, wl_probe and shingle-lock, through
`signal_checked`. The pure helpers are tested by `python3 -m unittest`
in this directory (test_run.py).
"""

import argparse
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

def _load_xwayland_harness():
  """tools/xwayland_test/run.py, for its IPC client, results and the
  checked signal helper."""
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..',
                      'xwayland_test', 'run.py')
  spec = importlib.util.spec_from_file_location('xwayland_harness', path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

_HARNESS = _load_xwayland_harness()

def _wayfire_shell_checks():
  """WG-4's checks beside this file, loaded when they run, so that the
  harnesses that load this file need not find them."""
  directory = os.path.dirname(os.path.abspath(__file__))
  if directory not in sys.path:
    sys.path.insert(0, directory)
  import wayfire_shell_checks  # pylint: disable=import-outside-toplevel
  return wayfire_shell_checks
Ipc = _HARNESS.Ipc
Results = _HARNESS.Results
device_fds = _HARNESS.device_fds
parse_ppm = _HARNESS.parse_ppm
signal_checked = _HARNESS.signal_checked
wait_for = _HARNESS.wait_for
hide_host_runtime = _HARNESS.hide_host_runtime
host_uid_environment = _HARNESS.host_uid_environment

INNER_FLAG = '--inner'
HOST_VARIABLES = ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'DISPLAY',
                  'WAYFIRE_SOCKET')
MODE_LINE = re.compile(
    r'^\s+(\d+)x(\d+) px, ([\d.]+) Hz(?: \(([^)]*)\))?$')

def parse_wlr_randr(text):
  """The heads in `wlr-randr` output, by name.

  Each head is a dict with its description, `modes` (a list of
  (width, height, refresh_hz, flags) tuples), `enabled`, `position`
  (x, y), `transform` and `scale`.
  """
  heads = {}
  head = None
  in_modes = False
  for line in text.splitlines():
    if line and not line[0].isspace():
      name, _, description = line.partition(' ')
      head = {'description': description.strip('"'), 'modes': []}
      heads[name] = head
      in_modes = False
      continue
    if head is None:
      continue
    match = MODE_LINE.match(line)
    if in_modes and match:
      width, height, refresh, flags = match.groups()
      head['modes'].append((int(width), int(height), float(refresh),
                            tuple((flags or '').split(', ')) if flags
                            else ()))
      continue
    key, _, value = line.strip().partition(':')
    value = value.strip()
    in_modes = key == 'Modes'
    if key == 'Enabled':
      head['enabled'] = value == 'yes'
    elif key == 'Position':
      x, _, y = value.partition(',')
      head['position'] = (int(x), int(y))
    elif key == 'Transform':
      head['transform'] = value
    elif key == 'Scale':
      head['scale'] = float(value)
    elif key in ('Make', 'Model'):
      head[key.lower()] = value
  return heads

XDG_NAME = re.compile(r"^\s+name: '([^']*)'$")
XDG_POSITION = re.compile(r'^\s+logical_x: (-?\d+), logical_y: (-?\d+)$')
XDG_SIZE = re.compile(r'^\s+logical_width: (\d+), logical_height: (\d+)$')
NAMESPACE_VARIABLE = 'SHINGLE_OUTPUT_TEST_NAMESPACE'

def parse_xdg_outputs(text):
  """The xdg-outputs in `wayland-info` output, by name, as
  (x, y, width, height)."""
  outputs = {}
  name = None
  position = None
  for line in text.splitlines():
    match = XDG_NAME.match(line)
    if match:
      name, position = match.group(1), None
      continue
    match = XDG_POSITION.match(line)
    if match and name is not None:
      position = (int(match.group(1)), int(match.group(2)))
      continue
    match = XDG_SIZE.match(line)
    if match and name is not None and position is not None:
      outputs[name] = (*position, int(match.group(1)), int(match.group(2)))
      name = None
  return outputs

def swap_outputs(session, first, second):
  """`session` with the outputs `first` and `second` swapped in every
  workspace set's `output` and `home`."""
  swap = {first: second, second: first}
  for wset in session['wsets']:
    for key in ('output', 'home'):
      wset[key] = swap.get(wset[key], wset[key])
  return session

def grim_scale(outputs, region):
  """The scale `grim -g` renders `region` (x, y, width, height) at: the
  highest scale of the outputs, (x, y, width, height, scale), it
  overlaps, as grim 1.4 chooses it."""
  x, y, width, height = region
  scales = [scale for ox, oy, ow, oh, scale in outputs
            if ox < x + width and x < ox + ow and oy < y + height
            and y < oy + oh]
  return max(scales, default=1)

def current_mode(head):
  """The (width, height, refresh_hz) of the head's current mode, or None."""
  for width, height, refresh, flags in head['modes']:
    if 'current' in flags:
      return (width, height, round(refresh, 3))
  return None

def private_environment(directory):
  """The environment for Shingle and its clients in `directory`."""
  environment = {key: value for key, value in os.environ.items()
                 if key not in HOST_VARIABLES}
  environment.update({
      'XDG_RUNTIME_DIR': os.path.join(directory, 'runtime'),
      'XDG_CONFIG_HOME': os.path.join(directory, 'config'),
      'XDG_STATE_HOME': os.path.join(directory, 'state'),
      'HOME': os.path.join(directory, 'home'),
  })
  return environment

class Headless:
  """One Shingle run with a config file of its own, headless unless
  `backend` says otherwise."""

  def __init__(self, binary, name, config_text, extension,
               backend=('--headless',), extra_environment=None,
               removed_environment=()):
    self.directory = os.path.join('/tmp', name)
    for sub in ('runtime', 'config', 'state', 'home'):
      os.makedirs(os.path.join(self.directory, sub), mode=0o700)
    self.environment = private_environment(self.directory)
    for variable in removed_environment:
      self.environment.pop(variable, None)
    self.environment.update(extra_environment or {})
    self.runtime = self.environment['XDG_RUNTIME_DIR']
    self.config_path = os.path.join(self.directory, f'shingle.{extension}')
    self.write_config(config_text)
    self.ipc_path = os.path.join(self.runtime, 'ipc')
    self.log_path = os.path.join(self.directory, 'shingle.log')
    self.log = open(self.log_path, 'w')
    self.process = subprocess.Popen(
        [binary, *backend, '--config', self.config_path,
         '--ipc', self.ipc_path],
        env=self.environment, stdout=self.log, stderr=self.log,
        stdin=subprocess.DEVNULL)
    self.ipc = wait_for(self._connect)
    if self.ipc is None:
      raise RuntimeError(f'no IPC socket: {self.log_text()}')

  def _connect(self):
    if self.process.poll() is not None:
      raise RuntimeError(f'shingle exited: {self.log_text()}')
    try:
      return Ipc(self.ipc_path)
    except (FileNotFoundError, ConnectionRefusedError):
      return None

  def write_config(self, text):
    """Writes the config file atomically."""
    temporary = self.config_path + '.new'
    with open(temporary, 'w') as file:
      file.write(text)
    os.replace(temporary, self.config_path)

  def log_text(self):
    """Shingle's output so far."""
    if not self.log.closed:
      self.log.flush()
    with open(self.log_path, errors='replace') as file:
      return file.read()

  def wayland_display(self):
    """The private `WAYLAND_DISPLAY`, as an absolute path."""
    name = wait_for(lambda: next(
        (line.split('=', 1)[1] for line in self.log_text().splitlines()
         if line.startswith('WAYLAND_DISPLAY=')), None))
    return os.path.join(self.runtime, name) if name else None

  def randr(self, *arguments):
    """Runs `wlr-randr` against this Shingle; returns (status, output)."""
    environment = {
        'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
        'XDG_RUNTIME_DIR': self.runtime,
        'WAYLAND_DISPLAY': self.wayland_display(),
    }
    result = subprocess.run(['wlr-randr', *arguments], env=environment,
                            capture_output=True, text=True, timeout=10)
    return result.returncode, result.stdout + result.stderr

  def client_environment(self):
    """The environment for a client of this Shingle."""
    return {
        'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
        'XDG_RUNTIME_DIR': self.runtime,
        'WAYLAND_DISPLAY': self.wayland_display(),
        'HOME': os.path.join(self.directory, 'home'),
        NAMESPACE_VARIABLE: '1',
    }

  def grim(self, *arguments):
    """A capture through `grim -t ppm`, as a `Shot`, or None."""
    try:
      done = subprocess.run(['grim', '-t', 'ppm', *arguments, '-'],
                            env=self.client_environment(),
                            capture_output=True, timeout=30,
                            stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
      return None
    parsed = parse_ppm(done.stdout) if done.returncode == 0 else None
    return Shot(*parsed) if parsed else None

  def client(self, arguments, program, extra_environment=None):
    """Starts a client of this Shingle."""
    environment = self.client_environment()
    environment.update(extra_environment or {})
    return Client(arguments, environment, program)

  def heads(self):
    """What `wlr-randr` lists."""
    status, output = self.randr()
    return parse_wlr_randr(output) if status == 0 else {}

  def xdg_outputs(self):
    """The xdg-outputs `wayland-info` lists, by name."""
    result = subprocess.run(['wayland-info'], env=self.client_environment(),
                            capture_output=True, text=True, timeout=10)
    return parse_xdg_outputs(result.stdout) if result.returncode == 0 else {}

  def positions(self):
    """Each head's position, as `wlr-randr` lists it."""
    return {name: head.get('position') for name, head in self.heads().items()}

  def views(self):
    """The views IPC lists, by app id."""
    return {view['app-id']: view
            for view in self.ipc.call('window-rules/list-views')}

  def output(self):
    """The IPC description of the output."""
    return self.ipc.call('window-rules/list-outputs')[0]

  def stop(self):
    """SIGTERM through `signal_checked`, then waits; returns the status."""
    self.ipc.close()
    if not signal_checked(self.process.pid, signal.SIGTERM, 'shingle'):
      return 'not signalled'
    try:
      return self.process.wait(10)
    except subprocess.TimeoutExpired:
      signal_checked(self.process.pid, signal.SIGKILL, 'shingle')
      self.process.wait()
      return 'timeout'
    finally:
      self.log.close()

PAM_PERMIT = ('auth sufficient pam_permit.so\n'
              'account sufficient pam_permit.so\n')

def grey(pixel):
  """Whether an (r, g, b) pixel is the default background, 0.1 linear or
  89 in sRGB."""
  return all(abs(channel - 89) <= 3 for channel in pixel)

def white(pixel):
  """Whether an (r, g, b) pixel is white."""
  return all(channel >= 250 for channel in pixel)

def red(pixel):
  """Whether an (r, g, b) pixel is clearly red."""
  return pixel[0] > 200 and pixel[1] < 60 and pixel[2] < 60

class Shot:
  """A grim capture as RGB pixels."""

  def __init__(self, width, height, pixels):
    self.size = (width, height)
    self.pixels = pixels

  def pixel(self, x, y):
    """The (r, g, b) at (x, y)."""
    offset = (y * self.size[0] + x) * 3
    return tuple(self.pixels[offset:offset + 3])

class Client:
  """A long-running client of a Shingle, with its stdout and stderr read
  in the background as lines."""

  def __init__(self, arguments, environment, program):
    self.program = program
    self.process = subprocess.Popen(
        arguments, env=environment, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    self.lines = []
    self.lock = threading.Lock()
    self.reader = threading.Thread(target=self._read, daemon=True)
    self.reader.start()

  def _read(self):
    for line in self.process.stdout:
      with self.lock:
        self.lines.append(line.rstrip('\n'))

  def text(self):
    """Everything the client printed so far."""
    with self.lock:
      return '\n'.join(self.lines)

  def events(self):
    """The JSON lines the client printed, parsed; other lines skipped."""
    with self.lock:
      lines = list(self.lines)
    events = []
    for line in lines:
      try:
        events.append(json.loads(line))
      except ValueError:
        pass
    return events

  def wait_event(self, name, timeout=10.0, after=0, **fields):
    """The first `name` event at index `after` or later whose fields
    include `fields`, waiting up to `timeout` seconds."""

    def find():
      return next((event for event in self.events()[after:]
                   if event.get('event') == name
                   and all(event.get(key) == value
                           for key, value in fields.items())), None)
    return wait_for(find, timeout)

  def wait_line(self, text, timeout=10.0):
    """Whether a line containing `text` was printed within `timeout`."""
    return wait_for(lambda: text in self.text(), timeout) or False

  def exited(self, timeout=10.0):
    """Waits for the client to exit; returns its status or None."""
    try:
      return self.process.wait(timeout)
    except subprocess.TimeoutExpired:
      return None

  def stop(self):
    """SIGTERM, then SIGKILL, each through `signal_checked`."""
    if self.process.poll() is None:
      signal_checked(self.process.pid, signal.SIGTERM, self.program)
      if self.exited(5) is None:
        signal_checked(self.process.pid, signal.SIGKILL, self.program)
        self.exited(5)

def yaml_config(output):
  """A YAML config (as JSON) with Xwayland off and `output` as its output
  section."""
  return json.dumps({
      'output': output,
      'xwayland': {'mode': False},
      'session': {'restore': False},
      'workspaces': {'width': 1, 'height': 1},
  })

def geometry(shingle):
  """The output's IPC geometry as (x, y, width, height)."""
  value = shingle.output()['geometry']
  return (value['x'], value['y'], value['width'], value['height'])

def randr_checks(results, binary):
  """wlr-randr against the default headless output."""
  shingle = Headless(binary, 'randr', yaml_config({}), 'yaml')
  try:
    results.check('Shingle holds no GPU, input or TTY descriptor',
                  device_fds(shingle.process.pid) == [],
                  str(device_fds(shingle.process.pid)))
    heads = shingle.heads()
    head = heads.get('HEADLESS-1')
    results.check('wlr-randr lists HEADLESS-1', head is not None,
                  str(list(heads)))
    if head is None:
      return shingle.log_text()
    results.check('the head is described as wlroots does',
                  head['description']
                  == 'Shingle Headless output (HEADLESS-1)',
                  head['description'])
    results.check('its one mode is 1280x720 at 60 Hz, current and preferred',
                  head['modes'] == [(1280, 720, 60.0,
                                     ('preferred', 'current'))],
                  str(head['modes']))
    status, output = shingle.randr('--output', 'HEADLESS-1',
                                   '--custom-mode', '1600x900@50Hz',
                                   '--dryrun')
    results.check('a tested custom mode succeeds and changes nothing',
                  status == 0 and geometry(shingle) == (0, 0, 1280, 720),
                  output.strip())
    status, output = shingle.randr('--output', 'HEADLESS-1',
                                   '--custom-mode', '1600x900@50Hz')
    head = shingle.heads().get('HEADLESS-1', {'modes': []})
    results.check('an applied custom mode resizes the output',
                  status == 0 and geometry(shingle) == (0, 0, 1600, 900),
                  f'{output.strip()} {geometry(shingle)}')
    results.check('and becomes the current mode at its refresh rate',
                  current_mode(head) == (1600, 900, 50.0), str(head['modes']))
    status, output = shingle.randr('--output', 'HEADLESS-1', '--scale', '2',
                                   '--pos', '100,20', '--transform', '90')
    head = shingle.heads().get('HEADLESS-1', {})
    results.check('scale, position and transform are applied',
                  status == 0 and geometry(shingle) == (100, 20, 450, 800)
                  and head.get('scale') == 2.0
                  and head.get('transform') == '90'
                  and head.get('position') == (100, 20),
                  f'{output.strip()} {geometry(shingle)} {head}')
    status, output = shingle.randr('--output', 'HEADLESS-1', '--off')
    results.check('turning the only output off fails and changes nothing',
                  status != 0 and geometry(shingle) == (100, 20, 450, 800)
                  and shingle.heads().get('HEADLESS-1', {}).get('enabled'),
                  output.strip())
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0', status == 0, str(status))
  return shingle.log_text()

def configured_checks(results, binary):
  """Explicit modes from the YAML config: the default entry, then a named
  HEADLESS-1 entry that also places the output."""
  shingle = Headless(binary, 'configured',
                     yaml_config({'mode': '1920x1080@75'}), 'yaml')
  try:
    head = shingle.heads().get('HEADLESS-1', {'modes': []})
    results.check('an explicit configured mode sizes the headless output',
                  geometry(shingle) == (0, 0, 1920, 1080),
                  str(geometry(shingle)))
    results.check('and clients see its refresh rate',
                  current_mode(head) == (1920, 1080, 75.0), str(head['modes']))
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0 (default entry)', status == 0,
                str(status))
  named = json.dumps({
      **json.loads(yaml_config({'mode': '1920x1080@75'})),
      'outputs': [{'name': 'HEADLESS-1', 'mode': '800x600@50', 'x': 10}],
  })
  shingle = Headless(binary, 'named', named, 'yaml')
  try:
    head = shingle.heads().get('HEADLESS-1', {'modes': []})
    results.check('a named HEADLESS-1 entry replaces the default',
                  geometry(shingle) == (10, 0, 800, 600)
                  and current_mode(head) == (800, 600, 50.0)
                  and head.get('position') == (10, 0),
                  f'{geometry(shingle)} {head["modes"]}')
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0 (named entry)', status == 0,
                str(status))
  return shingle.log_text()

def nested_checks(results, binary):
  """Nested Shingle on lavapipe inside a headless one: clients see the
  configured refresh rate at the size of the window, which the outer
  Shingle decides, and a config reload applies a new scale."""
  outer = Headless(binary, 'outer', yaml_config({}), 'yaml')
  nested_config = json.loads(yaml_config({'mode': '1024x768@75'}))
  nested_config['behavior'] = {'auto_reload': True, 'config_reload_delay': 20}
  nested = None
  try:
    nested = Headless(binary, 'nested', json.dumps(nested_config), 'yaml',
                      backend=('--backend', 'nested'),
                      extra_environment={
                          'WAYLAND_DISPLAY': outer.wayland_display(),
                          'VK_ICD_FILENAMES': _HARNESS.LAVAPIPE,
                      })
    window = wait_for(lambda: outer.ipc.call('window-rules/list-views'),
                      timeout=30)
    size = window[0]['geometry'] if window else {}
    head = nested.heads().get('HEADLESS-1')
    expected = (size.get('width'), size.get('height'), 75.0)
    results.check('nested: the mode is the window at the configured refresh',
                  head is not None and current_mode(head) == expected,
                  f'{head and head["modes"]} window {size}')
    nested_config['output']['scale'] = 2
    nested.write_config(json.dumps(nested_config))
    reloaded = wait_for(
        lambda: nested.heads().get('HEADLESS-1', {}).get('scale') == 2.0)
    logical = (0, 0, size.get('width', 0) // 2, size.get('height', 0) // 2)
    results.check('nested: a reloaded scale is applied to the window size',
                  reloaded and wait_for(lambda: geometry(nested) == logical),
                  f'{nested.heads().get("HEADLESS-1")} {geometry(nested)}')
  finally:
    status = nested.stop() if nested else 'not started'
    outer_status = outer.stop()
  results.check('nested: SIGTERM exits nested with status 0', status == 0,
                str(status))
  results.check('nested: SIGTERM exits headless with status 0',
                outer_status == 0, str(outer_status))
  return nested.log_text() if nested else outer.log_text()

def wayfire_checks(results, binary):
  """Two Wayfire output sections stay apart; the headless one applies."""
  ini = '\n'.join([
      '[core]',
      'xwayland = false',
      '[session]',
      'restore = false',
      '[output:DP-1]',
      'mode = 2560x1440@144',
      'scale = 2',
      '[output:HEADLESS-1]',
      'mode = 1024x768@60',
      'position = 0,0',
      '',
  ])
  shingle = Headless(binary, 'wayfire', ini, 'ini')
  try:
    head = shingle.heads().get('HEADLESS-1', {'modes': [], 'scale': None})
    results.check('the [output:HEADLESS-1] section sizes the output',
                  geometry(shingle) == (0, 0, 1024, 768)
                  and current_mode(head) == (1024, 768, 60.0),
                  f'{geometry(shingle)} {head["modes"]}')
    results.check('the later [output:DP-1] section does not overwrite it',
                  head.get('scale') == 1.0, str(head.get('scale')))
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0 (Wayfire config)', status == 0,
                str(status))
  return shingle.log_text()

WCM_FIXTURE = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), '..', '..', 'crates',
    'shingle-config', 'testdata', 'wayfire-wcm.ini')
# The warnings Shingle gives for WCM_FIXTURE, by line, as the unit test
# a_wcm_written_file_imports_with_the_expected_warnings expects them.
# Scale is not in the fixture's plugin list, so its `pinch in 4` (line
# 332) is not read since WG-2. Since WG-4 the `<super>` of line 61 and
# the `[wayfire-shell]` section of line 412 are read, and since WG-5
# the `[vswipe]` section of line 376 belongs to a plugin Shingle has
# that the list leaves out, which is not read.
WCM_WARNING_LINES = (14, 24, 40, 93, 110, 141, 154, 175, 305, 417, 430)
# Wayfire's options (tools/wayfire_options/generate.py).
OPTION_TABLE = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), '..', '..', 'crates',
    'shingle-config', 'data', 'wayfire-options.tsv')

def table_options():
  """The (section, option) pairs of OPTION_TABLE."""
  with open(OPTION_TABLE) as file:
    return {tuple(line.split('\t')[:2]) for line in file
            if line.strip() and not line.startswith('#')}

def config_warnings(log, path):
  """The `line N: ...` config warnings `log` holds for `path`, in order."""
  prefix = f'shingle: {path}: '
  return [line[len(prefix):] for line in log.splitlines()
          if line.startswith(prefix + 'line ')]

def warning_line(warning):
  """The line number of a `line N: ...` warning."""
  return int(warning.split(':', 1)[0].split()[1])

def close_colour(text, expected, tolerance=1e-4):
  """Whether `text`, four numbers, is `expected` within `tolerance`."""
  try:
    values = [float(part) for part in text.split()]
  except (AttributeError, ValueError):
    return False
  return (len(values) == len(expected)
          and all(abs(a - b) <= tolerance for a, b in zip(values, expected)))

def wcm_ini_checks(results, binary):
  """WG-1: a WCM-style wayfire.ini loads, warns once per problem and its
  values reach IPC."""
  with open(WCM_FIXTURE) as file:
    ini = file.read()
  shingle = Headless(binary, 'wcm-ini', ini, 'ini')
  try:
    # Shingle has started and serves IPC, so the file loaded.
    results.check('headless Shingle starts with the WCM-style fixture', True)
    background = shingle.ipc.call('wayfire/get-config-option',
                                  {'option': 'core/background_color'})
    results.check('core/background_color is the escaped \\#1A2B3CFF',
                  close_colour(background.get('value'),
                               (26 / 255, 43 / 255, 60 / 255, 1.0)),
                  str(background))
    gap = shingle.ipc.call('wayfire/get-config-option',
                           {'option': 'simple-tile/inner_gap_size'})
    results.check('simple-tile/inner_gap_size is 7', gap.get('value') == '7',
                  str(gap))
    # WG-2: every option of Wayfire's metadata is listed, and the file's
    # values are what IPC reports.
    listed = shingle.ipc.call('wayfire/list-config-options')
    sections = listed.get('options', {})
    pairs = {(section, option) for section, options in sections.items()
             for option in options}
    expected = table_options()
    results.check('list-config-options lists every option of the table',
                  expected <= pairs and len(expected) == 332,
                  f'{len(expected & pairs)} of {len(expected)} listed')
    toggle = shingle.ipc.call('wayfire/get-config-option',
                              {'option': 'scale/toggle'})
    results.check('get-config-option reports an unloaded plugin\'s '
                  'option as the file wrote it',
                  toggle.get('value') == '<super> KEY_P | pinch in 4'
                  and toggle.get('default') == '<super> KEY_P', str(toggle))
    # Wait past auto_reload_config's delay: a reload would warn again.
    time.sleep(1)
    warnings = config_warnings(shingle.log_text(), shingle.config_path)
    lines = tuple(warning_line(warning) for warning in warnings)
    results.check('each fixture warning is logged once, with its line',
                  lines == WCM_WARNING_LINES, f'{lines}')
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0 (WCM-style config)', status == 0,
                str(status))
  return shingle.log_text()

# The variables WG-3 adds to the client environment, which every run
# removes from Shingle's own environment unless it sets them on purpose.
DESKTOP_VARIABLES = ('XDG_CURRENT_DESKTOP', 'XCURSOR_THEME', 'XCURSOR_SIZE',
                     'WAYFIRE_CONFIG_FILE')
ENV_LINE = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')

def parse_env(text):
  """The variables `env` printed in `text`, by name. Lines without `=`
  continue the previous value, as a value with a newline prints."""
  variables = {}
  name = None
  for line in text.splitlines():
    if ENV_LINE.match(line):
      name, value = line.split('=', 1)
      variables[name] = value
    elif name is not None:
      variables[name] += '\n' + line
  return variables

def child_environment(shingle, name):
  """What a child `stipc/run` starts finds in its environment, by name,
  or None when it did not run."""
  path = os.path.join(shingle.directory, f'{name}.env')
  # Renamed into place, so a partly written file is never read.
  command = f'env > {path}.new && mv {path}.new {path}'
  reply = shingle.ipc.call('stipc/run', {'cmd': command})
  if reply.get('result') != 'ok':
    return None
  if not wait_for(lambda: os.path.exists(path) or None):
    return None
  with open(path) as file:
    return parse_env(file.read())

def environment_mismatches(found, expected):
  """The `name: found != expected` differences of `found` from
  `expected`, where an expected None means the variable is unset."""
  return [f'{name}: {found.get(name)!r} != {value!r}'
          for name, value in expected.items() if found.get(name) != value]

def environment_detail(found, mismatches):
  """The detail of an environment check."""
  if not found:
    return 'the child wrote no environment'
  return '; '.join(mismatches) or 'as expected'

def client_environment_checks(results, binary):
  """WG-3: `stipc/run` children get Wayfire's variables, and Shingle's own
  XDG_CURRENT_DESKTOP when it has one."""
  yaml = json.loads(yaml_config({'mode': '1280x720'}))
  yaml['input'] = {'cursor_theme': 'Adwaita', 'cursor_size': 32}
  host_ini = '/tmp/host-session/wayfire.ini'
  shingle = Headless(binary, 'environment-yaml', json.dumps(yaml), 'yaml',
                     removed_environment=DESKTOP_VARIABLES,
                     extra_environment={'WAYFIRE_CONFIG_FILE': host_ini})
  try:
    found = child_environment(shingle, 'first') or {}
    display = os.path.basename(shingle.wayland_display() or '')
    mismatches = environment_mismatches(found, {
        'XDG_CURRENT_DESKTOP': 'wayfire',
        'XCURSOR_THEME': 'Adwaita',
        'XCURSOR_SIZE': '32',
        'WAYFIRE_CONFIG_FILE': None,
        'WAYLAND_DISPLAY': display,
        'WAYLAND_SOCKET': None,
        'DISPLAY': None,
        'WAYFIRE_SOCKET': shingle.ipc_path,
        '_JAVA_AWT_WM_NONREPARENTING': '1',
    })
    results.check('a YAML run exports wayfire, the cursor and the sockets, '
                  'and no WAYFIRE_CONFIG_FILE', found and not mismatches,
                  environment_detail(found, mismatches))
    reply = shingle.ipc.call('wayfire/set-config-options',
                             {'input/cursor_size': '40'})
    found = child_environment(shingle, 'second') or {}
    results.check('set-config-options changes XCURSOR_SIZE for a later child',
                  reply.get('result') == 'ok'
                  and found.get('XCURSOR_SIZE') == '40',
                  f'{reply} {found.get("XCURSOR_SIZE")!r}')
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0 (environment, YAML)',
                status == 0, str(status))
  log = shingle.log_text()
  ini = '\n'.join([
      '[core]',
      'xwayland = false',
      '[autostart]',
      'autostart_wf_shell = false',
      '[input]',
      'cursor_size = 28',
      '',
  ])
  shingle = Headless(binary, 'environment-ini', ini, 'ini',
                     removed_environment=DESKTOP_VARIABLES,
                     extra_environment={
                         'XDG_CURRENT_DESKTOP': 'GNOME:test',
                         'XCURSOR_THEME': 'inherited-theme',
                     })
  try:
    found = child_environment(shingle, 'first') or {}
    mismatches = environment_mismatches(found, {
        'XDG_CURRENT_DESKTOP': 'GNOME:test',
        'XCURSOR_THEME': 'inherited-theme',
        'XCURSOR_SIZE': '28',
        'WAYFIRE_CONFIG_FILE': shingle.config_path,
    })
    results.check('a Wayfire run keeps the host XDG_CURRENT_DESKTOP and '
                  'theme and exports its file', found and not mismatches,
                  environment_detail(found, mismatches))
  finally:
    status = shingle.stop()
  results.check('SIGTERM exits with status 0 (environment, Wayfire)',
                status == 0, str(status))
  return log + shingle.log_text()

def multi_config(outputs=None, session_path=None):
  """A YAML config (as JSON) with Xwayland off, the named `outputs`
  entries and, with `session_path`, session restore from there."""
  config = json.loads(yaml_config({}))
  if outputs:
    config['outputs'] = outputs
  if session_path:
    config['session'] = {'restore': True, 'path': session_path}
  return json.dumps(config)

def output_ids(shingle):
  """The IPC output ids by name."""
  return {output['name']: output['id']
          for output in shingle.ipc.call('window-rules/list-outputs')}

def watch(shingle, events):
  """A second IPC connection watching `events`."""
  watcher = Ipc(shingle.ipc_path)
  watcher.call('window-rules/events/watch', {'events': events})
  return watcher

def next_event(watcher, name):
  """The next event called `name`, or None within the IPC timeout."""
  try:
    while True:
      event = watcher.receive()
      if event.get('event') == name:
        return event
  except (TimeoutError, ConnectionError):
    return None

def auto_layout_checks(results, binary):
  """Two automatic outputs side by side, a head moved by `wlr-randr`, and
  an output created and destroyed over IPC."""
  shingle = Headless(binary, 'multi-auto', multi_config(), 'yaml',
                     backend=('--headless', '--outputs', '2'))
  try:
    methods = shingle.ipc.call('list-methods').get('methods', [])
    # 81 after MO-2, 83 with MO-4's wsets methods, 85 with MO-6's
    # stipc methods for nested windows, 86 with WG-4's
    # wayfire-shell/toggle_menu and 87 with WG-5's stipc/feed_gesture.
    results.check('MO-6: list-methods returns 87 methods',
                  len(methods) == 87, str(len(methods)))
    positions = shingle.positions()
    results.check('MO-2: wlr-randr lists both outputs at auto positions',
                  positions == {'HEADLESS-1': (0, 0),
                                'HEADLESS-2': (1280, 0)}, str(positions))
    xdg = shingle.xdg_outputs()
    results.check('MO-2: wayland-info shows both xdg-outputs side by side',
                  xdg == {'HEADLESS-1': (0, 0, 1280, 720),
                          'HEADLESS-2': (1280, 0, 1280, 720)}, str(xdg))
    ids = output_ids(shingle)
    results.check('MO-2: IPC gives the outputs ids 1 and 2',
                  ids == {'HEADLESS-1': 1, 'HEADLESS-2': 2}, str(ids))
    status, output = shingle.randr('--output', 'HEADLESS-2', '--pos', '0,720')
    positions = shingle.positions()
    # HEADLESS-2 is fixed now, so the automatic HEADLESS-1 starts at its
    # right edge and its y, as wlroots places it.
    results.check('MO-2: wlr-randr --pos moves the second head',
                  status == 0
                  and positions == {'HEADLESS-1': (1280, 720),
                                    'HEADLESS-2': (0, 720)},
                  f'{output.strip()} {positions}')
    watcher = watch(shingle, ['output-added', 'output-removed',
                              'output-layout-changed'])
    created = shingle.ipc.call('wayfire/create-headless-output',
                               {'width': 640, 'height': 480})
    added = next_event(watcher, 'output-added')
    results.check('MO-2: create-headless-output adds HEADLESS-3 (id 3)',
                  created.get('result') == 'ok'
                  and created['output']['name'] == 'HEADLESS-3'
                  and created['output']['id'] == 3, str(created))
    results.check('MO-2: watchers get output-added for it',
                  added is not None
                  and added['output']['name'] == 'HEADLESS-3', str(added))
    xdg = shingle.xdg_outputs()
    results.check('MO-2: clients see the created output',
                  xdg.get('HEADLESS-3') == (2560, 720, 640, 480)
                  and 'HEADLESS-3' in shingle.heads(), str(xdg))
    destroyed = shingle.ipc.call('wayfire/destroy-headless-output',
                                 {'output': 'HEADLESS-3'})
    removed = next_event(watcher, 'output-removed')
    results.check('MO-2: destroy-headless-output removes it again',
                  destroyed == {'result': 'ok'}
                  and removed is not None
                  and removed['output']['id'] == 3
                  and 'HEADLESS-3' not in wait_for(
                      lambda: shingle.xdg_outputs() or None)
                  and 'HEADLESS-3' not in shingle.heads(),
                  f'{destroyed} {removed}')
    refused = shingle.ipc.call('wayfire/destroy-headless-output',
                               {'output': 'HEADLESS-1'})
    results.check('MO-2: an output IPC did not create is refused',
                  refused.get('error') == 'Output is not a headless output '
                  'created from an IPC command!', str(refused))
    watcher.close()
  finally:
    status = shingle.stop()
  results.check('MO-2: SIGTERM exits with status 0 (auto layout)',
                status == 0, str(status))
  return shingle.log_text()

def fixed_layout_checks(results, binary):
  """Configured fixed positions, with an automatic output after them."""
  outputs = [
      {'name': 'HEADLESS-1', 'position': {'x': 0, 'y': 0}},
      {'name': 'HEADLESS-2', 'position': {'x': -1024, 'y': 200},
       'mode': '1024x768@60'},
  ]
  shingle = Headless(binary, 'multi-fixed', multi_config(outputs), 'yaml',
                     backend=('--headless', '--outputs', '2'))
  try:
    positions = shingle.positions()
    results.check('MO-2: wlr-randr lists configured fixed positions',
                  positions == {'HEADLESS-1': (0, 0),
                                'HEADLESS-2': (-1024, 200)}, str(positions))
    xdg = shingle.xdg_outputs()
    results.check('MO-2: wayland-info shows the fixed xdg-output geometry',
                  xdg == {'HEADLESS-1': (0, 0, 1280, 720),
                          'HEADLESS-2': (-1024, 200, 1024, 768)}, str(xdg))
    created = shingle.ipc.call('wayfire/create-headless-output',
                               {'width': 800, 'height': 600})
    geometry = created.get('output', {}).get('geometry')
    results.check('MO-2: an automatic output goes after the fixed ones',
                  geometry == {'x': 1280, 'y': 0, 'width': 800,
                               'height': 600}, str(created))
  finally:
    status = shingle.stop()
  results.check('MO-2: SIGTERM exits with status 0 (fixed layout)',
                status == 0, str(status))
  return shingle.log_text()

def stop_probe(process):
  """Ends a wl_probe through `signal_checked` and waits for it."""
  if process.poll() is None:
    signal_checked(process.pid, signal.SIGTERM, 'wl_probe')
    try:
      process.wait(5)
    except subprocess.TimeoutExpired:
      signal_checked(process.pid, signal.SIGKILL, 'wl_probe')
      process.wait()

def stop_view_process(view):
  """Ends the wl_probe behind an IPC view, if its pid names one."""
  pid = view.get('pid') if view else None
  if isinstance(pid, int) and signal_checked(pid, signal.SIGTERM,
                                             'wl_probe'):
    wait_for(lambda: not os.path.exists(f'/proc/{pid}'), timeout=5)

def view_output(shingle, app_id):
  """(output-name, output-id) of the view with `app_id`, or None."""
  view = shingle.views().get(app_id)
  return (view['output-name'], view['output-id']) if view else None

def session_checks(results, binary, wl_probe):
  """A window saved on HEADLESS-3 waits on the active output until IPC
  creates HEADLESS-3, moves there, and moves back when it goes."""
  session_path = '/tmp/multi-session.json'
  config = multi_config(session_path=session_path)
  first = Headless(binary, 'multi-save', config, 'yaml',
                   backend=('--headless', '--outputs', '3'),
                   extra_environment={NAMESPACE_VARIABLE: '1'})
  probe = None
  try:
    watcher = watch(first, ['view-mapped'])
    probe = subprocess.Popen(
        [wl_probe, 'mo2-probe'], env=first.client_environment(),
        stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL)
    mapped = wait_for(lambda: view_output(first, 'mo2-probe'))
    results.check('MO-2: the probe maps on the active output',
                  mapped == ('HEADLESS-1', 1), str(mapped))
    event = next_event(watcher, 'view-mapped')
    watcher.close()
    results.check('MO-2: headless sends view-mapped to watchers',
                  event is not None
                  and event['view']['app-id'] == 'mo2-probe'
                  and event['view']['output-name'] == 'HEADLESS-1',
                  str(event))
  finally:
    status = first.stop()
    if probe is not None:
      stop_probe(probe)
      probe.stdin.close()
  results.check('MO-2: SIGTERM exits with status 0 (session saved)',
                status == 0, str(status))
  try:
    with open(session_path) as file:
      session = json.load(file)
  except (OSError, ValueError) as error:
    results.check('MO-2: the session was saved', False, repr(error))
    return first.log_text()
  with open(session_path, 'w') as file:
    json.dump(swap_outputs(session, 'HEADLESS-1', 'HEADLESS-3'), file)
  second = Headless(binary, 'multi-restore', config, 'yaml',
                    backend=('--headless', '--outputs', '2'),
                    extra_environment={NAMESPACE_VARIABLE: '1'})
  view = None
  try:
    waiting = wait_for(lambda: view_output(second, 'mo2-probe'),
                       timeout=20)
    results.check('MO-2: the restored window waits on the active output',
                  waiting == ('HEADLESS-1', 1), str(waiting))
    created = second.ipc.call('wayfire/create-headless-output',
                              {'width': 640, 'height': 480})
    moved = view_output(second, 'mo2-probe')
    results.check('MO-2: it returns to HEADLESS-3 when IPC creates it',
                  created.get('output', {}).get('name') == 'HEADLESS-3'
                  and moved == ('HEADLESS-3', 3), f'{created} {moved}')
    second.ipc.call('wayfire/destroy-headless-output', {'output-id': 3})
    back = view_output(second, 'mo2-probe')
    results.check('MO-2: destroying it moves the window to the active '
                  'output', back == ('HEADLESS-1', 1), str(back))
    view = second.views().get('mo2-probe')
  finally:
    status = second.stop()
    stop_view_process(view)
  results.check('MO-2: SIGTERM exits with status 0 (session restored)',
                status == 0, str(status))
  return second.log_text()

def mo3_config():
  """Two outputs, HEADLESS-2 at scale 2, with Xwayland off."""
  return multi_config([{'name': 'HEADLESS-2', 'scale': 2}])

def enabled_heads(shingle):
  """{name: enabled} of the heads `wlr-randr` lists."""
  return {name: head.get('enabled') for name, head in shingle.heads().items()}

def capture_checks(results, shingle, wl_probe):
  """grim per output and across both, with a layer panel on HEADLESS-2;
  the panel is closed when wlr-randr turns HEADLESS-2 off."""
  first = shingle.grim('-o', 'HEADLESS-1')
  second = shingle.grim('-o', 'HEADLESS-2')
  results.check('MO-3: grim -o HEADLESS-1 captures 1280x720',
                first is not None and first.size == (1280, 720)
                and grey(first.pixel(640, 360)),
                str(first and first.size))
  results.check('MO-3: grim -o HEADLESS-2 captures 2560x1440 at scale 2',
                second is not None and second.size == (2560, 1440)
                and grey(second.pixel(1280, 720)),
                str(second and second.size))
  panel = shingle.client([wl_probe, '--layer', 'HEADLESS-2'], 'wl_probe')
  try:
    configure = panel.wait_event('layer-configure')
    results.check('MO-3: a layer panel on HEADLESS-2 gets its width',
                  configure is not None and configure['width'] == 1280
                  and configure['height'] == 30, str(configure))
    drawn = wait_for(lambda: (lambda shot: shot if shot and white(
        shot.pixel(1280, 10)) else None)(shingle.grim('-o', 'HEADLESS-2')))
    results.check('MO-3: grim -o HEADLESS-2 shows the panel in its pixels',
                  drawn is not None and white(drawn.pixel(2559, 59))
                  and grey(drawn.pixel(1280, 60)),
                  str(drawn and [drawn.pixel(1280, 10),
                                 drawn.pixel(1280, 60)]))
    region = (1270, 0, 20, 40)
    scale = grim_scale([(0, 0, 1280, 720, 1), (1280, 0, 1280, 720, 2)],
                       region)
    across = shingle.grim('-g', '1270,0 20x40')
    results.check('MO-3: grim -g across both outputs joins them',
                  across is not None
                  and across.size == (20 * scale, 40 * scale)
                  and grey(across.pixel(5, 10))
                  and white(across.pixel(30, 10))
                  and grey(across.pixel(30, 70)),
                  str(across and (across.size, across.pixel(5, 10),
                                  across.pixel(30, 10),
                                  across.pixel(30, 70))))
    status, output = shingle.randr('--output', 'HEADLESS-2', '--off')
    closed = panel.wait_event('layer-closed')
    results.check('MO-3: wlr-randr --off turns HEADLESS-2 off',
                  status == 0 and enabled_heads(shingle)
                  == {'HEADLESS-1': True, 'HEADLESS-2': False}
                  and 'HEADLESS-2' not in shingle.xdg_outputs(),
                  f'{output.strip()} {enabled_heads(shingle)}')
    results.check('MO-3: the panel on it is closed, and the probe exits 0',
                  closed is not None and panel.exited() == 0,
                  panel.text()[-300:])
  finally:
    panel.stop()
  status, output = shingle.randr('--output', 'HEADLESS-2', '--on')
  heads = shingle.heads()
  results.check('MO-3: wlr-randr --on turns it on again where it was',
                status == 0 and heads.get('HEADLESS-2', {}).get('enabled')
                and heads['HEADLESS-2'].get('position') == (1280, 0)
                and heads['HEADLESS-2'].get('scale') == 2.0
                and shingle.xdg_outputs().get('HEADLESS-2')
                == (1280, 0, 1280, 720),
                f'{output.strip()} {heads.get("HEADLESS-2")}')

def report_checks(results, shingle, wl_probe):
  """wl_probe --report-outputs enters and leaves outputs, with their
  scales, as its output goes and comes back."""
  probe = shingle.client([wl_probe, 'mo3-probe', '--report-outputs'],
                         'wl_probe')
  try:
    entered = probe.wait_event('output-enter')
    scale = probe.wait_event('preferred-scale')
    results.check('MO-3: the probe enters HEADLESS-1 at scale 1',
                  entered == {'event': 'output-enter',
                              'output': 'HEADLESS-1'}
                  and scale is not None and scale['scale'] == 120,
                  f'{entered} {scale}')
    mark = len(probe.events())
    status, output = shingle.randr('--output', 'HEADLESS-1', '--off')
    left = probe.wait_event('output-leave', after=mark)
    entered = probe.wait_event('output-enter', after=mark)
    scale = probe.wait_event('preferred-scale', after=mark, scale=240)
    buffer_scale = probe.wait_event('buffer-scale', after=mark, factor=2)
    results.check('MO-3: turned off, HEADLESS-1 is left for HEADLESS-2',
                  status == 0 and left is not None
                  and left['output'] == 'HEADLESS-1'
                  and entered is not None
                  and entered['output'] == 'HEADLESS-2',
                  f'{output.strip()} {probe.events()[mark:]}')
    results.check('MO-3: and the probe is told HEADLESS-2\'s scale 2',
                  scale is not None and buffer_scale is not None,
                  str(probe.events()[mark:]))
    mark = len(probe.events())
    status, output = shingle.randr('--output', 'HEADLESS-1', '--on')
    left = probe.wait_event('output-leave', after=mark)
    entered = probe.wait_event('output-enter', after=mark)
    scale = probe.wait_event('preferred-scale', after=mark, scale=120)
    results.check('MO-3: back on, the window returns to HEADLESS-1',
                  status == 0 and left is not None
                  and left['output'] == 'HEADLESS-2'
                  and entered is not None
                  and entered['output'] == 'HEADLESS-1'
                  and scale is not None,
                  f'{output.strip()} {probe.events()[mark:]}')
  finally:
    probe.stop()

def type_text(ipc, text):
  """Types lowercase letters, digits and newlines through STIPC."""
  for character in text:
    key = _HARNESS.KEY_NAMES.get(character, f'KEY_{character.upper()}')
    for pressed in (True, False):
      ipc.call('stipc/feed_key', {'key': key, 'state': pressed})

def lock_checks(results, shingle, lock_binary):
  """shingle-lock covers both outputs, and a typed password unlocks."""
  lock = shingle.client([lock_binary, '--color', 'ff0000'], 'shingle-lock',
                        {'VK_ICD_FILENAMES': _HARNESS.LAVAPIPE})
  try:
    ready = lock.wait_line('opaque lock surfaces committed on 2 output(s)',
                           timeout=30)
    locked = lock.wait_line('compositor reported the session locked')
    results.check('MO-3: shingle-lock covers both outputs and is locked',
                  ready and locked, lock.text()[-400:])
    shots = {name: shingle.grim('-o', name)
             for name in ('HEADLESS-1', 'HEADLESS-2')}
    results.check('MO-3: both outputs show the lock colour',
                  all(shot is not None and red(shot.pixel(10, 10))
                      and red(shot.pixel(shot.size[0] - 10,
                                         shot.size[1] - 10))
                      for shot in shots.values()),
                  str({name: shot and shot.pixel(10, 10)
                       for name, shot in shots.items()}))
    type_text(shingle.ipc, 'secret\n')
    unlocked = lock.wait_line('session unlocked', timeout=20)
    status = lock.exited(10)
    shot = shingle.grim('-o', 'HEADLESS-2')
    results.check('MO-3: a typed password unlocks, and shingle-lock exits 0',
                  unlocked and status == 0 and shot is not None
                  and not red(shot.pixel(10, 10)),
                  f'{status} {lock.text()[-400:]}')
  finally:
    lock.stop()

def created_layer_checks(results, shingle, wl_probe):
  """A panel on an output IPC created is closed when IPC destroys it."""
  created = shingle.ipc.call('wayfire/create-headless-output',
                             {'width': 640, 'height': 480})
  name = created.get('output', {}).get('name')
  panel = shingle.client([wl_probe, '--layer', name or '?'], 'wl_probe')
  try:
    configure = panel.wait_event('layer-configure')
    results.check(f'MO-3: a panel on the created {name} gets its width',
                  configure is not None and configure['width'] == 640,
                  f'{created} {configure} {panel.text()[-200:]}')
    destroyed = shingle.ipc.call('wayfire/destroy-headless-output',
                                 {'output': name})
    closed = panel.wait_event('layer-closed')
    results.check(f'MO-3: destroying {name} closes the panel',
                  destroyed == {'result': 'ok'} and closed is not None
                  and panel.exited() == 0, f'{destroyed} {panel.text()}')
  finally:
    panel.stop()

def protocol_checks(results, binary, wl_probe, lock_binary):
  """MO-3's client protocols against two outputs."""
  shingle = Headless(binary, 'protocols', mo3_config(), 'yaml',
                     backend=('--headless', '--outputs', '2'),
                     extra_environment={NAMESPACE_VARIABLE: '1'})
  try:
    for checks in (capture_checks, report_checks, created_layer_checks):
      checks(results, shingle, wl_probe)
    if lock_binary:
      lock_checks(results, shingle, lock_binary)
    status, output = shingle.randr('--output', 'HEADLESS-2', '--pos', '0,720')
    positions = shingle.positions()
    results.check('MO-3: wlr-randr --pos places HEADLESS-2',
                  status == 0 and positions.get('HEADLESS-2') == (0, 720),
                  f'{output.strip()} {positions}')
  finally:
    status = shingle.stop()
  results.check('MO-3: SIGTERM exits with status 0 (protocols)',
                status == 0, str(status))
  return shingle.log_text()

def focus_config():
  """Two outputs, HEADLESS-2 at 1024x768, with Xwayland off."""
  return multi_config([{'name': 'HEADLESS-2', 'mode': '1024x768@60'}])

def view_info(shingle, view_id):
  """IPC `view-info` of `view_id`, or {}."""
  return shingle.ipc.call('window-rules/view-info',
                          {'id': view_id}).get('info') or {}

def focused_output(shingle):
  """The name of the output `get-focused-output` reports."""
  return shingle.ipc.call('window-rules/get-focused-output').get(
      'info', {}).get('name')

def focused_view_id(shingle):
  """The id of the view `get-focused-view` reports, or None."""
  info = shingle.ipc.call('window-rules/get-focused-view').get('info')
  return info.get('id') if info else None

def stipc_tap(shingle, key, held=()):
  """Taps `key` through STIPC with the `held` keys held."""
  for name in [*held, key]:
    shingle.ipc.call('stipc/feed_key', {'key': name, 'state': True})
  for name in [key, *reversed(held)]:
    shingle.ipc.call('stipc/feed_key', {'key': name, 'state': False})

def drain(watcher):
  """The events a watcher received until it has been quiet for a moment."""
  events = []
  try:
    while True:
      events.append(watcher.receive(timeout=0.5))
  except (TimeoutError, ConnectionError):
    pass
  return events

def map_probe(shingle, wl_probe, app_id):
  """A `wl_probe --report-outputs` window and its IPC view id."""
  probe = shingle.client([wl_probe, app_id, '--report-outputs'], 'wl_probe')
  probe.wait_event('configure')
  view = wait_for(lambda: shingle.views().get(app_id))
  return probe, view['id'] if view else None

def geometry_of(info):
  """A view's IPC geometry as (x, y, width, height)."""
  value = info.get('geometry', {})
  return tuple(value.get(key) for key in ('x', 'y', 'width', 'height'))

def click_checks(results, shingle, probes):
  """Motion alone leaves the active output, a click focuses another."""
  left, right = probes['left'], probes['right']
  shingle.ipc.call('window-rules/focus-view', {'id': left[1]})
  watcher = watch(shingle, ['output-gain-focus'])
  shingle.ipc.call('stipc/move_cursor', {'x': 1800, 'y': 300})
  results.check('MO-4: stipc/move_cursor onto HEADLESS-2 alone keeps '
                'HEADLESS-1 active', focused_output(shingle) == 'HEADLESS-1',
                str(focused_output(shingle)))
  mark = len(right[0].events())
  shingle.ipc.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'full'})
  gained = next_event(watcher, 'output-gain-focus')
  entered = right[0].wait_event('enter', after=mark)
  results.check('MO-4: stipc/feed_button there makes HEADLESS-2 active',
                focused_output(shingle) == 'HEADLESS-2' and gained
                and gained['output']['name'] == 'HEADLESS-2'
                and gained['output']['id'] == 2, str(gained))
  results.check('MO-4: and focuses its window, which gets the keyboard',
                focused_view_id(shingle) == right[1] and entered is not None,
                f'{focused_view_id(shingle)} {right[0].events()[mark:]}')
  watcher.close()

def oswitch_checks(results, shingle, probes):
  """super+o moves focus to the next output, super+shift+o takes the
  focused window along."""
  left = probes['left']
  mark = len(left[0].events())
  stipc_tap(shingle, 'KEY_O', ['KEY_LEFTMETA'])
  entered = left[0].wait_event('enter', after=mark)
  results.check('MO-4: super+o through STIPC focuses HEADLESS-1 and its '
                'window', focused_output(shingle) == 'HEADLESS-1'
                and focused_view_id(shingle) == left[1]
                and entered is not None,
                f'{focused_output(shingle)} {focused_view_id(shingle)}')
  watcher = watch(shingle, ['view-set-output', 'view-wset-changed'])
  mark = len(left[0].events())
  stipc_tap(shingle, 'KEY_O', ['KEY_LEFTMETA', 'KEY_LEFTSHIFT'])
  moved = left[0].wait_event('output-enter', after=mark, output='HEADLESS-2')
  configure = left[0].wait_event('configure', after=mark, width=512)
  events = drain(watcher)
  names = [event.get('event') for event in events]
  info = view_info(shingle, left[1])
  results.check('MO-4: super+shift+o takes the focused window to '
                'HEADLESS-2', focused_output(shingle) == 'HEADLESS-2'
                and focused_view_id(shingle) == left[1]
                and info.get('output-name') == 'HEADLESS-2'
                and moved is not None,
                f'{info.get("output-name")} {left[0].events()[mark:]}')
  results.check('MO-4: where it tiles beside the window there (512 wide)',
                configure is not None and configure['height'] == 768,
                str(left[0].events()[mark:]))
  set_output = next((event for event in events
                     if event.get('event') == 'view-set-output'), {})
  results.check('MO-4: watchers get view-set-output and view-wset-changed',
                'view-wset-changed' in names
                and set_output.get('output', {}).get('name') == 'HEADLESS-2'
                and set_output.get('view', {}).get('id') == left[1], str(names))
  watcher.close()

def configure_view_checks(results, shingle, probes):
  """configure-view with output_id moves a fullscreen, a floating and a
  tiled window with Wayfire's geometry."""
  left, floater, full = probes['left'], probes['floater'], probes['full']
  shingle.ipc.call('wm-actions/set-fullscreen', {'view_id': full[1],
                                                 'state': True})
  full[0].wait_event('configure', width=1280, height=720)
  mark = len(full[0].events())
  reply = shingle.ipc.call('window-rules/configure-view',
                           {'id': full[1], 'output_id': 2})
  configure = full[0].wait_event('configure', after=mark, width=1024,
                                 height=768)
  info = view_info(shingle, full[1])
  results.check('MO-4: a fullscreen window fills HEADLESS-2 (1024x768)',
                reply == {'result': 'ok'} and configure is not None
                and info.get('fullscreen') is True
                and geometry_of(info) == (1280, 0, 1024, 768),
                f'{reply} {geometry_of(info)} {full[0].events()[mark:]}')
  # floater floats at 100,50 on HEADLESS-1; it is scaled by 1024/1280 and
  # 768/720 from HEADLESS-2's corner.
  shingle.ipc.call('window-rules/configure-view', {
      'id': floater[1],
      'geometry': {'x': 100, 'y': 50, 'width': 300, 'height': 200}})
  reply = shingle.ipc.call('window-rules/configure-view',
                           {'id': floater[1], 'output_id': 2})
  info = wait_for(lambda: (lambda value: value if geometry_of(value)
                           == (1360, 53, 300, 200) else None)(
                               view_info(shingle, floater[1])))
  results.check('MO-4: a floating window is scaled into HEADLESS-2 '
                '(100,50 to 1360,53)', reply == {'result': 'ok'}
                and bool(info) and info.get('output-name') == 'HEADLESS-2'
                and floater[0].wait_event('output-enter',
                                          output='HEADLESS-2') is not None,
                f'{reply} {geometry_of(view_info(shingle, floater[1]))}')
  # left is tiled on HEADLESS-2 beside right; HEADLESS-1 is empty now.
  mark = len(left[0].events())
  reply = shingle.ipc.call('window-rules/configure-view',
                           {'id': left[1], 'output_id': 1})
  configure = left[0].wait_event('configure', after=mark, width=1280,
                                 height=720)
  results.check('MO-4: a tiled window joins HEADLESS-1\'s tree (1280x720)',
                reply == {'result': 'ok'} and configure is not None
                and view_info(shingle, left[1]).get('output-name')
                == 'HEADLESS-1', f'{reply} {left[0].events()[mark:]}')
  missing = shingle.ipc.call('window-rules/configure-view',
                             {'id': full[1], 'output_id': 9})
  results.check('MO-4: configure-view refuses an unknown output',
                missing.get('error') == 'output not found', str(missing))

def wset_indices(shingle):
  """{output name: wset-index} from list-outputs."""
  return {output['name']: output['wset-index']
          for output in shingle.ipc.call('window-rules/list-outputs')}

def wsets_checks(results, shingle, probes):
  """wsets/set-output-wset swaps the two outputs' sets in two calls."""
  left, right = probes['left'], probes['right']
  before = wset_indices(shingle)
  watcher = watch(shingle, ['output-wset-changed'])
  mark = len(right[0].events())
  first = shingle.ipc.call(
      'wsets/set-output-wset',
      {'output-id': 1, 'wset-index': before['HEADLESS-2']})
  after_first = wset_indices(shingle)
  second = shingle.ipc.call(
      'wsets/set-output-wset',
      {'output-id': 2, 'wset-index': before['HEADLESS-1']})
  after = wset_indices(shingle)
  events = drain(watcher)
  results.check('MO-4: set-output-wset gives HEADLESS-1 the set of '
                'HEADLESS-2, which gets a new one', first == {'result': 'ok'}
                and after_first == {'HEADLESS-1': 2, 'HEADLESS-2': 3},
                f'{before} {first} {after_first}')
  results.check('MO-4: and a second call swaps the sets',
                second == {'result': 'ok'}
                and after == {'HEADLESS-1': 2, 'HEADLESS-2': 1}
                and len(shingle.ipc.call('window-rules/list-wsets')) == 2,
                f'{second} {after}')
  entered = right[0].wait_event('output-enter', after=mark,
                                output='HEADLESS-1')
  results.check('MO-4: their windows change outputs with them',
                view_info(shingle, right[1]).get('output-name') == 'HEADLESS-1'
                and view_info(shingle, left[1]).get('output-name')
                == 'HEADLESS-2' and entered is not None,
                f'{right[0].events()[mark:]}')
  results.check('MO-4: watchers get output-wset-changed for each output',
                [event.get('output') for event in events
                 if event.get('event') == 'output-wset-changed']
                == [1, 2, 2],
                str(events))
  watcher.close()

def focus_checks(results, binary, wl_probe):
  """MO-4's focus and moving windows against two outputs."""
  shingle = Headless(binary, 'focus', focus_config(), 'yaml',
                     backend=('--headless', '--outputs', '2'),
                     extra_environment={NAMESPACE_VARIABLE: '1'})
  probes = {}
  try:
    for name in ('right', 'floater', 'full', 'left'):
      probes[name] = map_probe(shingle, wl_probe, f'mo4-{name}')
    results.check('MO-4: four probes map on HEADLESS-1',
                  all(view_id for _, view_id in probes.values()),
                  str({name: view for name, (_, view) in probes.items()}))
    reply = shingle.ipc.call('window-rules/configure-view',
                             {'id': probes['right'][1], 'output_id': 2})
    results.check('MO-4: configure-view puts the right probe on '
                  'HEADLESS-2', reply == {'result': 'ok'}
                  and probes['right'][0].wait_event(
                      'output-enter', output='HEADLESS-2') is not None,
                  f'{reply} {probes["right"][0].events()}')
    for checks in (click_checks, oswitch_checks, configure_view_checks,
                   wsets_checks):
      checks(results, shingle, probes)
  finally:
    for probe, _ in probes.values():
      probe.stop()
    status = shingle.stop()
  results.check('MO-4: SIGTERM exits with status 0 (focus)', status == 0,
                str(status))
  return shingle.log_text()

def private_pam(work):
  """Bind-mounts a private /etc/pam.d whose `shingle` service permits
  everything, for shingle-lock's unlock; only this namespace sees it."""
  directory = os.path.join(work, 'pam.d')
  os.makedirs(directory)
  with open(os.path.join(directory, 'shingle'), 'w') as file:
    file.write(PAM_PERMIT)
  subprocess.run(['mount', '--bind', directory, '/etc/pam.d'], check=True)

def inner(binary):
  """Runs the checks inside the namespace; returns the exit status."""
  work = os.environ['OUTPUT_TEST_WORK']
  private_tmp = os.path.join(work, 'tmp')
  os.makedirs(private_tmp)
  subprocess.run(['mount', '--bind', private_tmp, '/tmp'], check=True)
  for device in ('/dev/dri', '/dev/input'):
    if os.path.isdir(device):
      subprocess.run(['mount', '-t', 'tmpfs', 'none', device], check=True)
  runtime = hide_host_runtime()
  results = Results()
  results.check('/dev/dri is empty in the namespace',
                not os.path.isdir('/dev/dri') or not os.listdir('/dev/dri'))
  results.check('the host runtime directory is hidden',
                runtime is None or os.listdir(runtime) == [])
  results.check('no host display variable is set',
                not any(name in os.environ for name in HOST_VARIABLES))
  wl_probe = os.environ.get('OUTPUT_TEST_WL_PROBE')
  lock_binary = os.environ.get('OUTPUT_TEST_LOCK')
  if lock_binary:
    private_pam(work)
  multi = [auto_layout_checks, fixed_layout_checks]
  if wl_probe:
    def restored_session_checks(results, binary):
      return session_checks(results, binary, wl_probe)

    def mo3_protocol_checks(results, binary):
      return protocol_checks(results, binary, wl_probe, lock_binary)

    def mo4_focus_checks(results, binary):
      return focus_checks(results, binary, wl_probe)
    multi += [restored_session_checks, mo3_protocol_checks, mo4_focus_checks]

  def wg4_headless_checks(results, binary):
    return _wayfire_shell_checks().headless_checks(
        results, sys.modules[__name__], binary, wl_probe)

  def wg4_offscreen_checks(results, binary):
    return _wayfire_shell_checks().offscreen_checks(
        results, sys.modules[__name__], binary)
  groups = (randr_checks, configured_checks, wayfire_checks, wcm_ini_checks,
            client_environment_checks, nested_checks, *multi,
            wg4_headless_checks, wg4_offscreen_checks)
  chosen = [name for name in os.environ.get('OUTPUT_TEST_CHECKS',
                                            '').split(',') if name]
  for checks in groups:
    if chosen and checks.__name__ not in chosen:
      continue
    try:
      log = checks(results, binary)
    except Exception as error:  # pylint: disable=broad-except
      results.check(f'{getattr(checks, "__name__", "checks")} ran', False,
                    repr(error))
      continue
    if not results.all_passed():
      name = getattr(checks, '__name__', 'checks')
      print(f'--- {name} log ---\n{log}--- end ---', flush=True)
  passed = sum(1 for _, ok, _ in results.checks if ok)
  print(f'{passed}/{len(results.checks)} checks passed', flush=True)
  return 0 if results.all_passed() else 1

def main():
  """Sets up the work directory and re-runs itself in the namespace."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--wl-probe', default='target/debug/examples/wl_probe')
  parser.add_argument('--lock', default='target/debug/shingle-lock')
  parser.add_argument('--keep', action='store_true')
  parser.add_argument('--checks', nargs='*', default=[],
                      help='run only these groups, by function name, '
                      'such as wg4_headless_checks')
  parser.add_argument(INNER_FLAG, action='store_true')
  options = parser.parse_args()
  binary = os.path.abspath(options.binary)
  if options.inner:
    return inner(binary)
  if shutil.which('wlr-randr') is None:
    print('wlr-randr is not installed', file=sys.stderr)
    return 2
  work = os.path.abspath(os.path.join('target', f'output_test-{os.getpid()}'))
  os.makedirs(work)
  environment = {key: value for key, value in os.environ.items()
                 if key not in HOST_VARIABLES}
  environment['OUTPUT_TEST_WORK'] = work
  environment['OUTPUT_TEST_CHECKS'] = ','.join(options.checks)
  if os.path.exists(options.wl_probe):
    environment['OUTPUT_TEST_WL_PROBE'] = os.path.abspath(options.wl_probe)
  else:
    print(f'{options.wl_probe} is missing; skipping the session and MO-3 '
          'checks', file=sys.stderr)
  if os.path.exists(options.lock):
    environment['OUTPUT_TEST_LOCK'] = os.path.abspath(options.lock)
  else:
    print(f'{options.lock} is missing; skipping the lock checks',
          file=sys.stderr)
  try:
    # The PID namespace confines every signal to processes started in it.
    command = ['unshare', '--user', '--map-root-user', '--mount', '--net',
               '--pid', '--fork', '--mount-proc', '--',
               sys.executable, os.path.abspath(__file__),
               '--binary', binary, INNER_FLAG]
    return subprocess.run(command,
                          env=host_uid_environment(environment)).returncode
  finally:
    if not options.keep:
      shutil.rmtree(work, ignore_errors=True)

if __name__ == '__main__':
  sys.exit(main())
