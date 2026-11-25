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

  cargo build -p shingle-bin
  tools/output_test/run.py [--binary target/debug/shingle] [--keep]

It prints one line per check and exits 0 when all pass. The checks are
the parts of NB-9 (docs/design/native-backend.md) that are not
device-specific: the mode list and current mode clients see, custom
modes, tests that change nothing, placement, refusing to turn the only
output off, explicit configured modes with their refresh rate, Wayfire
`[output:NAME]` sections kept apart, and named YAML entries. Headless
Shingle does not reload its config, so a nested Shingle on lavapipe,
running as a client of a headless one inside the namespace, checks the
configured refresh rate and that a reload applies.
Signals go only to Shingle, through `signal_checked`. The pure
helpers are tested by `python3 -m unittest` in this directory
(test_run.py).
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
Ipc = _HARNESS.Ipc
Results = _HARNESS.Results
device_fds = _HARNESS.device_fds
signal_checked = _HARNESS.signal_checked
wait_for = _HARNESS.wait_for

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
               backend=('--headless',), extra_environment=None):
    self.directory = os.path.join('/tmp', name)
    for sub in ('runtime', 'config', 'state', 'home'):
      os.makedirs(os.path.join(self.directory, sub), mode=0o700)
    self.environment = private_environment(self.directory)
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

  def heads(self):
    """What `wlr-randr` lists."""
    status, output = self.randr()
    return parse_wlr_randr(output) if status == 0 else {}

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

def inner(binary):
  """Runs the checks inside the namespace; returns the exit status."""
  work = os.environ['OUTPUT_TEST_WORK']
  private_tmp = os.path.join(work, 'tmp')
  os.makedirs(private_tmp)
  subprocess.run(['mount', '--bind', private_tmp, '/tmp'], check=True)
  for device in ('/dev/dri', '/dev/input'):
    if os.path.isdir(device):
      subprocess.run(['mount', '-t', 'tmpfs', 'none', device], check=True)
  results = Results()
  results.check('/dev/dri is empty in the namespace',
                not os.path.isdir('/dev/dri') or not os.listdir('/dev/dri'))
  results.check('no host display variable is set',
                not any(name in os.environ for name in HOST_VARIABLES))
  for checks in (randr_checks, configured_checks, wayfire_checks,
                 nested_checks):
    try:
      log = checks(results, binary)
    except Exception as error:  # pylint: disable=broad-except
      results.check(f'{checks.__name__} ran', False, repr(error))
      continue
    if not results.all_passed():
      print(f'--- {checks.__name__} log ---\n{log}--- end ---', flush=True)
  passed = sum(1 for _, ok, _ in results.checks if ok)
  print(f'{passed}/{len(results.checks)} checks passed', flush=True)
  return 0 if results.all_passed() else 1

def main():
  """Sets up the work directory and re-runs itself in the namespace."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--keep', action='store_true')
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
  try:
    # The PID namespace confines every signal to processes started in it.
    command = ['unshare', '--user', '--map-root-user', '--mount', '--net',
               '--pid', '--fork', '--mount-proc', '--',
               sys.executable, os.path.abspath(__file__),
               '--binary', binary, INNER_FLAG]
    return subprocess.run(command, env=environment).returncode
  finally:
    if not options.keep:
      shutil.rmtree(work, ignore_errors=True)

if __name__ == '__main__':
  sys.exit(main())
