#!/usr/bin/env python3
"""Real-client XWayland checks for Shingle in a private namespace.

Runs headless Shingle with real Xwayland and X11 clients inside an
unprivileged user, mount, network and PID namespace
(`unshare -U -r -m -n -p -f --mount-proc`).
Inside it a private directory is bind-mounted over `/tmp`, so the X
lock files and `/tmp/.X11-unix` are private, and the abstract X sockets
live in the private network namespace. `/dev/dri` and `/dev/input` are
covered with empty tmpfs mounts, so no process can open the host's GPU or
input devices. The runtime, config, state and home directories are
private too. Nothing reaches the host's X displays or its Wayland
session.

Usage, from the repository root and outside any sandbox that forbids
user namespaces:

  cargo build -p shingle-bin
  cargo build -p shingle-xwayland --example xw_probe
  tools/xwayland_test/run.py [--binary target/debug/shingle]
      [--probe target/debug/examples/xw_probe] [--keep]
      [--only lazy|disabled|eager|running|idle|windows|probe ...]

It prints one line per check and exits 0 when all pass. Every signal it
sends goes to its own children or, through `kill_xwayland`, to a
checked Xwayland PID inside the PID namespace. The pure helpers are
tested by `python3 -m unittest` in this directory (test_run.py). The work
directory under `target/` is removed afterwards unless `--keep` is given.
The checks are XW-2's and XW-3's in docs/design/xwayland.md, section 7.
"""

import argparse
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import threading
import time

INNER_FLAG = '--inner'
IPC_TIMEOUT = 10.0
# The probe client refuses to run without it, so it never opens a window
# on the host's X displays.
NAMESPACE_VARIABLE = 'SHINGLE_XWAYLAND_TEST_NAMESPACE'

def encode_message(message):
  """Frames a JSON message for Wayfire IPC: little-endian length, body."""
  body = json.dumps(message).encode()
  return struct.pack('<I', len(body)) + body

def decode_messages(buffer):
  """Splits complete frames off `buffer`; returns (messages, rest)."""
  messages = []
  while len(buffer) >= 4:
    (length,) = struct.unpack('<I', buffer[:4])
    if len(buffer) < 4 + length:
      break
    messages.append(json.loads(buffer[4:4 + length]))
    buffer = buffer[4 + length:]
  return messages, buffer

def parse_xprop(text):
  """Parses `xprop` output into {property: value text}."""
  properties = {}
  for line in text.splitlines():
    for separator in (' = ', ': '):
      if separator in line:
        name, value = line.split(separator, 1)
        properties[name.split('(')[0].strip()] = value.strip()
        break
  return properties

def session_json(command, working_directory):
  """A saved session with one tiled window launched by `command`."""
  state = {
      'floating': None,
      'fullscreen': False,
      'gaps': {'bottom': 0, 'inner': 0, 'left': 0, 'right': 0, 'top': 0},
      'maximized': False,
      'minimized': False,
      'restore_geometry': None,
      'snapped': None,
      'stacking': 'normal',
      'sticky': False,
      'tiled_edges': 15,
  }
  workspace = {
      'focused': 1,
      'root': {
          'children': [{
              'node': {'kind': 'leaf', 'state': state, 'window': 1},
              'weight': 1,
          }],
          'direction': 'vertical',
      },
      'stack': [1],
      'workspace': {'x': 0, 'y': 0},
  }
  identity = {
      'app_id': 'restored-probe',
      'command': command,
      'launch_group': None,
      'launch_id': None,
      'title': 'Restored probe',
      'working_directory': working_directory,
  }
  return {
      'outputs': [{
          'current_workspace': {'x': 0, 'y': 0},
          'output': 'HEADLESS-1',
          'workarea': {'height': 720, 'width': 1280, 'x': 0, 'y': 0},
          'workspaces': [workspace],
      }],
      'version': 1,
      'windows': [{'id': 1, 'identity': identity, 'parent': None}],
  }

def children_of(pid):
  """The PIDs of `pid`'s children, from /proc."""
  children = []
  try:
    tasks = os.listdir(f'/proc/{pid}/task')
  except FileNotFoundError:
    return children
  for task in tasks:
    try:
      with open(f'/proc/{pid}/task/{task}/children') as file:
        children.extend(int(child) for child in file.read().split())
    except FileNotFoundError:
      pass
  return children

def command_name(pid):
  """The `comm` of `pid`, or None when it is gone."""
  try:
    with open(f'/proc/{pid}/comm') as file:
      return file.read().strip()
  except FileNotFoundError:
    return None

def signal_checked(pid, sig, expected):
  """Sends `sig` to `pid` if it is a live `expected` process.

  Returns whether it did. Refuses any PID that is not an int above 1 or
  whose `/proc/<pid>/comm` is not `expected` (as the kernel truncates it,
  to 15 characters). `-1`, which `stipc/get_xwayland_pid` returns when no
  server runs, would signal every process the caller may signal, and `0`
  its whole process group.
  """
  if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 1:
    return False
  if command_name(pid) != expected[:15]:
    return False
  os.kill(pid, sig)
  return True

def kill_xwayland(pid):
  """SIGKILLs the Xwayland process `pid`; returns whether it did."""
  return signal_checked(pid, signal.SIGKILL, 'Xwayland')

def parse_xwininfo(text):
  """The absolute geometry in `xwininfo -id` output, or None."""
  keys = {
      'Absolute upper-left X': 'x',
      'Absolute upper-left Y': 'y',
      'Width': 'width',
      'Height': 'height',
  }
  geometry = {}
  for line in text.splitlines():
    name, _, value = line.strip().partition(':')
    if name in keys:
      try:
        geometry[keys[name]] = int(value.strip())
      except ValueError:
        return None
  return geometry if len(geometry) == 4 else None

def parse_window_list(value):
  """The window ids in an xprop `window id # 0x1, 0x2` value."""
  if '#' not in value:
    return []
  return [int(part.strip(), 16) for part in value.split('#', 1)[1].split(',')
          if part.strip()]

def parse_wm_class(value):
  """The class in an xprop WM_CLASS value `"xterm", "XTerm"`, or ''."""
  parts = [part.strip().strip('"') for part in value.split(',')]
  return parts[1] if len(parts) == 2 else ''

def tiles_fill(geometries, area):
  """Whether the rectangles cover `area` exactly, without overlapping."""
  total = sum(g['width'] * g['height'] for g in geometries)
  if total != area['width'] * area['height']:
    return False
  for index, first in enumerate(geometries):
    for second in geometries[index + 1:]:
      if (first['x'] < second['x'] + second['width']
          and second['x'] < first['x'] + first['width']
          and first['y'] < second['y'] + second['height']
          and second['y'] < first['y'] + first['height']):
        return False
    if (first['x'] < area['x'] or first['y'] < area['y']
        or first['x'] + first['width'] > area['x'] + area['width']
        or first['y'] + first['height'] > area['y'] + area['height']):
      return False
  return True

def same_geometry(first, second):
  """Whether two {x, y, width, height} dictionaries agree."""
  return all(first.get(key) == second.get(key)
             for key in ('x', 'y', 'width', 'height'))

def wait_for(predicate, timeout=10.0, interval=0.05):
  """Polls `predicate` until it returns something true; returns that."""
  deadline = time.monotonic() + timeout
  while True:
    value = predicate()
    if value or time.monotonic() > deadline:
      return value
    time.sleep(interval)

def device_fds(pid):
  """Paths of `pid`'s open descriptors under /dev/dri, /dev/input or TTYs."""
  found = []
  try:
    fds = os.listdir(f'/proc/{pid}/fd')
  except FileNotFoundError:
    return found
  for fd in fds:
    try:
      target = os.readlink(f'/proc/{pid}/fd/{fd}')
    except OSError:
      continue
    if target.startswith(('/dev/dri', '/dev/input', '/dev/tty')):
      found.append(target)
  return found

class Results:
  """Collects check results and prints them as they come."""

  def __init__(self):
    self.checks = []

  def check(self, name, passed, detail=''):
    """Records one check."""
    self.checks.append((name, bool(passed), detail))
    status = 'PASS' if passed else 'FAIL'
    suffix = f' ({detail})' if detail else ''
    print(f'{status} {name}{suffix}', flush=True)
    return passed

  def all_passed(self):
    """Whether every check passed."""
    return all(passed for _, passed, _ in self.checks)

class Ipc:
  """A Wayfire IPC connection."""

  def __init__(self, path):
    self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    self.socket.settimeout(IPC_TIMEOUT)
    self.socket.connect(path)
    self.buffer = b''

  def receive(self, timeout=IPC_TIMEOUT):
    """Waits for the next message."""
    deadline = time.monotonic() + timeout
    while True:
      messages, self.buffer = decode_messages(self.buffer)
      if messages:
        first, rest = messages[0], messages[1:]
        self.buffer = b''.join(encode_message(m) for m in rest) + self.buffer
        return first
      left = deadline - time.monotonic()
      if left <= 0:
        raise TimeoutError('no IPC message')
      self.socket.settimeout(left)
      data = self.socket.recv(65536)
      if not data:
        raise ConnectionError('IPC connection closed')
      self.buffer += data

  def call(self, method, data=None):
    """Sends a request and returns its reply."""
    message = {'method': method}
    if data is not None:
      message['data'] = data
    self.socket.sendall(encode_message(message))
    return self.receive()

  def close(self):
    """Closes the connection."""
    self.socket.close()

class Shingle:
  """One headless Shingle run in the work directory."""

  def __init__(self, binary, work, name, config, spawn=None, session=None):
    self.work = work
    # The private /tmp keeps socket paths short enough for sun_path.
    self.directory = os.path.join('/tmp', name)
    self.runtime = os.path.join(self.directory, 'runtime')
    for sub in ('runtime', 'config', 'state', 'home'):
      os.makedirs(os.path.join(self.directory, sub), mode=0o700)
    self.ipc_path = os.path.join(self.runtime, 'ipc')
    session_path = os.path.join(self.directory, 'state', 'session.json')
    config = dict(config)
    config['session'] = {
        'restore': session is not None,
        'path': session_path,
    }
    config.setdefault('workspaces', {'width': 1, 'height': 1})
    if session is not None:
      with open(session_path, 'w') as file:
        json.dump(session, file)
    self.config_path = os.path.join(self.directory, 'shingle.yaml')
    with open(self.config_path, 'w') as file:
      # YAML accepts JSON.
      json.dump(config, file)
    environment = {
        key: value for key, value in os.environ.items()
        if key not in ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'DISPLAY',
                       'WAYFIRE_SOCKET', 'SHINGLE_XWAYLAND')
    }
    environment.update({
        'XDG_RUNTIME_DIR': self.runtime,
        'XDG_CONFIG_HOME': os.path.join(self.directory, 'config'),
        'XDG_STATE_HOME': os.path.join(self.directory, 'state'),
        'HOME': os.path.join(self.directory, 'home'),
    })
    arguments = [binary, '--headless', '--config', self.config_path,
                 '--ipc', self.ipc_path]
    if spawn:
      arguments += ['--spawn'] + spawn
    self.log_path = os.path.join(self.directory, 'shingle.log')
    self.log = open(self.log_path, 'w')
    self.process = subprocess.Popen(
        arguments, env=environment, stdout=self.log, stderr=self.log,
        stdin=subprocess.DEVNULL)
    self.ipc = self._connect()

  def _connect(self):
    """Waits for the IPC socket and connects."""
    deadline = time.monotonic() + IPC_TIMEOUT
    while True:
      if self.process.poll() is not None:
        raise RuntimeError(f'shingle exited: {self.log_text()}')
      try:
        return Ipc(self.ipc_path)
      except (FileNotFoundError, ConnectionRefusedError):
        if time.monotonic() > deadline:
          raise
        time.sleep(0.05)

  def call(self, method, data=None):
    """One IPC request on the main connection."""
    return self.ipc.call(method, data)

  def xwayland_pid(self):
    """`stipc/get_xwayland_pid`."""
    return self.call('stipc/get_xwayland_pid')['pid']

  def wait_pid(self, predicate, timeout=10.0):
    """Polls the Xwayland PID until `predicate` holds; returns it."""
    deadline = time.monotonic() + timeout
    while True:
      pid = self.xwayland_pid()
      if predicate(pid) or time.monotonic() > deadline:
        return pid
      time.sleep(0.05)

  def environment(self, display):
    """The environment for an X client of `display`."""
    return {
        'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
        'HOME': os.path.join(self.directory, 'home'),
        'DISPLAY': display,
        NAMESPACE_VARIABLE: '1',
    }

  def views(self):
    """`window-rules/list-views`."""
    return self.call('window-rules/list-views')

  def view(self, app_id):
    """The one view with `app_id`, or None."""
    views = [view for view in self.views() if view.get('app-id') == app_id]
    return views[0] if len(views) == 1 else None

  def x_windows(self, display):
    """{X window id: class} for `_NET_CLIENT_LIST`, in its order."""
    _, output = self.x_client(['xprop', '-root', '_NET_CLIENT_LIST'], display)
    ids = parse_window_list(parse_xprop(output).get('_NET_CLIENT_LIST', ''))
    windows = {}
    for window in ids:
      _, output = self.x_client(['xprop', '-id', hex(window), 'WM_CLASS'],
                                display)
      windows[window] = parse_wm_class(parse_xprop(output).get('WM_CLASS', ''))
    return windows

  def x_geometry(self, window, display):
    """The absolute geometry `xwininfo -id` reports, or None."""
    _, output = self.x_client(['xwininfo', '-id', hex(window)], display)
    return parse_xwininfo(output)

  def x_client(self, arguments, display, timeout=10):
    """Runs an X client; returns (exit status, output)."""
    try:
      done = subprocess.run(
          arguments, env=self.environment(display), capture_output=True,
          text=True, timeout=timeout, stdin=subprocess.DEVNULL)
      return done.returncode, done.stdout + done.stderr
    except subprocess.TimeoutExpired:
      return 'timeout', ''

  def stop(self, sig=signal.SIGTERM, timeout=10):
    """Sends `sig` and waits; returns the exit status."""
    self.ipc.close()
    self.process.send_signal(sig)
    try:
      return self.process.wait(timeout)
    except subprocess.TimeoutExpired:
      self.process.kill()
      self.process.wait()
      return 'timeout'
    finally:
      self.log.close()

  def log_text(self):
    """Shingle's stdout and stderr so far."""
    if not self.log.closed:
      self.log.flush()
    with open(self.log_path, errors='replace') as file:
      return file.read()

class XClient:
  """A long-running X client, such as xterm or the probe, with its stdout
  read in the background as lines."""

  def __init__(self, shingle, arguments, display, program):
    self.program = program
    self.process = subprocess.Popen(
        arguments, env=shingle.environment(display), stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    self.lines = []
    self.lock = threading.Lock()
    self.reader = threading.Thread(target=self._read, daemon=True)
    self.reader.start()

  def _read(self):
    for line in self.process.stdout:
      with self.lock:
        self.lines.append(line.rstrip('\n'))

  @property
  def pid(self):
    """The client's PID."""
    return self.process.pid

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

  def wait_event(self, name, timeout=10.0, after=0):
    """Waits for the first `name` event at index `after` or later."""
    def find():
      events = self.events()[after:]
      return next((event for event in events if event.get('event') == name),
                  None)
    return wait_for(find, timeout)

  def send(self, command):
    """Writes one command line to the client's stdin."""
    try:
      self.process.stdin.write(command + '\n')
      self.process.stdin.flush()
    except (BrokenPipeError, ValueError):
      pass

  def exited(self, timeout=10.0):
    """Waits for the client to exit; returns its status or None."""
    try:
      return self.process.wait(timeout)
    except subprocess.TimeoutExpired:
      return None

  def stop(self):
    """Ends the client: SIGTERM, then SIGKILL, each through
    `signal_checked`."""
    if self.process.poll() is None:
      signal_checked(self.pid, signal.SIGTERM, self.program)
      if self.exited(5) is None:
        signal_checked(self.pid, signal.SIGKILL, self.program)
        self.exited(5)
    try:
      self.process.stdin.close()
    except (BrokenPipeError, ValueError):
      pass

def lazy_checks(results, binary, work):
  """Lazy mode: start on connection, X tools, restart rule, SIGTERM."""
  script_out = os.path.join(work, 'lazy-script-out')
  spawn_out = os.path.join(work, 'lazy-spawn-out')
  config = {
      'xwayland': {
          'mode': 'lazy',
          'startup_script': f'echo "$DISPLAY" >> {script_out}',
      },
  }
  spawn = ['sh', '-c', f'echo "${{DISPLAY-unset}}" > {spawn_out}']
  shingle = Shingle(binary, work, 'lazy', config, spawn=spawn)
  methods = shingle.call('list-methods')['methods']
  results.check('list-methods returns 79 methods', len(methods) == 79,
                f'{len(methods)}')
  results.check(
      'list-methods has the XWayland methods',
      'stipc/get_xwayland_display' in methods
      and 'stipc/get_xwayland_pid' in methods)
  display = shingle.call('stipc/get_display')
  name = display.get('xwayland')
  results.check('lazy: stipc/get_display reports :0', name == ':0',
                json.dumps(display))
  reply = shingle.call('stipc/get_xwayland_display')
  results.check('lazy: stipc/get_xwayland_display',
                reply == {'result': 'ok', 'display': ':0'}, json.dumps(reply))
  results.check('lazy: lock and socket bound before any X client',
                os.path.exists('/tmp/.X0-lock')
                and os.path.exists('/tmp/.X11-unix/X0'))
  time.sleep(0.5)
  pid = shingle.xwayland_pid()
  xwayland_children = [child for child in children_of(shingle.process.pid)
                       if command_name(child) == 'Xwayland']
  results.check('lazy: no Xwayland before the first X client',
                pid == -1 and not xwayland_children,
                f'pid {pid}, children {xwayland_children}')
  deadline = time.monotonic() + 5
  while not os.path.exists(spawn_out) and time.monotonic() < deadline:
    time.sleep(0.05)
  with open(spawn_out) as file:
    spawned = file.read().strip()
  results.check('--spawn child sees DISPLAY=:0', spawned == ':0', spawned)

  watcher = Ipc(shingle.ipc_path)
  watched = watcher.call('window-rules/events/watch',
                         {'events': ['xwayland-ready']})
  results.check('xwayland-ready can be watched',
                watched == {'result': 'ok'}, json.dumps(watched))
  started = time.monotonic()
  status, output = shingle.x_client(['xdpyinfo'], ':0')
  results.check('lazy: xdpyinfo on :0 succeeds (first connection)',
                status == 0 and 'name of display:' in output,
                f'status {status}')
  pid = shingle.xwayland_pid()
  results.check('lazy: Xwayland runs after the first connection',
                pid > 0 and command_name(pid) == 'Xwayland', f'pid {pid}')
  try:
    event = watcher.receive()
  except (TimeoutError, ConnectionError) as error:
    event = {'error': str(error)}
  results.check(
      'xwayland-ready event with display and pid',
      event == {'event': 'xwayland-ready', 'display': ':0', 'pid': pid},
      json.dumps(event))
  watcher.close()
  status, output = shingle.x_client(
      ['xprop', '-root', '_NET_SUPPORTED', '_NET_SUPPORTING_WM_CHECK'], ':0')
  properties = parse_xprop(output)
  supported = properties.get('_NET_SUPPORTED', '')
  expected = ['_NET_WM_STATE', '_NET_ACTIVE_WINDOW', '_NET_WM_MOVERESIZE',
              '_NET_WM_STATE_FOCUSED', '_NET_WM_STATE_MODAL',
              '_NET_WM_STATE_FULLSCREEN', '_NET_WM_STATE_MAXIMIZED_VERT',
              '_NET_WM_STATE_MAXIMIZED_HORZ', '_NET_WM_STATE_HIDDEN',
              '_NET_CLIENT_LIST', '_NET_CLIENT_LIST_STACKING']
  listed = [atom.strip() for atom in supported.split(',') if atom.strip()]
  results.check('xprop -root _NET_SUPPORTED lists wlroots\' 11 atoms',
                status == 0 and sorted(listed) == sorted(expected), supported)
  check = properties.get('_NET_SUPPORTING_WM_CHECK', '')
  check_id = check.split('#')[-1].strip() if '#' in check else ''
  results.check('xprop -root _NET_SUPPORTING_WM_CHECK names a window',
                bool(check_id), check)
  if check_id:
    status, output = shingle.x_client(
        ['xprop', '-id', check_id, '_NET_WM_NAME', '_NET_SUPPORTING_WM_CHECK'],
        ':0')
    properties = parse_xprop(output)
    results.check(
        'the check window is named Shingle and points to itself',
        properties.get('_NET_WM_NAME') == '"Shingle"'
        and check_id in properties.get('_NET_SUPPORTING_WM_CHECK', ''),
        output.strip().replace('\n', '; '))
  status, output = shingle.x_client(['xprop', '-root', '_NET_ACTIVE_WINDOW',
                                     '_NET_CLIENT_LIST'], ':0')
  results.check('xprop -root shows the initial EWMH root properties',
                status == 0 and '_NET_ACTIVE_WINDOW' in output,
                output.strip().replace('\n', '; '))
  deadline = time.monotonic() + 5
  while not os.path.exists(script_out) and time.monotonic() < deadline:
    time.sleep(0.05)
  script = open(script_out).read() if os.path.exists(script_out) else ''
  results.check('the startup script ran once with DISPLAY=:0',
                script == ':0\n', repr(script))
  results.check('Xwayland opened no DRM, input or TTY device',
                not device_fds(pid), ', '.join(device_fds(pid)))

  # Kill it after more than 5 s: lazy mode restarts on the next connection.
  time.sleep(max(0.0, 6.5 - (time.monotonic() - started)))
  if not kill_xwayland(pid):
    results.check(f'refusing to kill pid {pid}', False)
    shingle.stop()
    return
  gone = shingle.wait_pid(lambda value: value == -1)
  time.sleep(0.5)
  results.check('lazy: killed after 6 s, no server until a connection',
                gone == -1 and shingle.xwayland_pid() == -1, f'pid {gone}')
  status, output = shingle.x_client(['xdpyinfo'], ':0')
  second = shingle.xwayland_pid()
  results.check('lazy: the next connection restarts Xwayland',
                status == 0 and second > 0 and second != pid,
                f'status {status}, pid {second}')

  # Kill it again within 2 s: no restart, not even lazily.
  if not kill_xwayland(second):
    results.check(f'refusing to kill pid {second}', False)
    shingle.stop()
    return
  gone = shingle.wait_pid(lambda value: value == -1)
  status, _ = shingle.x_client(['xdpyinfo'], ':0', timeout=4)
  third = shingle.xwayland_pid()
  results.check('lazy: killed after 2 s, not restarted',
                gone == -1 and third == -1 and status != 0,
                f'pid {third}, xdpyinfo {status}')

  status = shingle.stop(signal.SIGTERM)
  results.check('lazy: SIGTERM exits with status 0', status == 0, f'{status}')
  results.check('lazy: SIGTERM leaves no lock or socket',
                not os.path.exists('/tmp/.X0-lock')
                and not os.path.exists('/tmp/.X11-unix/X0'),
                ', '.join(os.listdir('/tmp/.X11-unix')))
  log = shingle.log_text()
  results.check('the log shows the restart decisions',
                'restarts on the next X11 connection' in log
                and 'not restarting Xwayland' in log)
  return log

def idle_checks(results, binary, work):
  """Lazy mode: Xwayland exits 10 s after its last client and comes back."""
  shingle = Shingle(binary, work, 'idle', {'xwayland': {'mode': 'lazy'}})
  status, _ = shingle.x_client(['xdpyinfo'], ':0')
  pid = shingle.xwayland_pid()
  results.check('idle: first connection starts Xwayland',
                status == 0 and pid > 0, f'pid {pid}')
  started = time.monotonic()
  gone = shingle.wait_pid(lambda value: value == -1, timeout=40)
  waited = time.monotonic() - started
  results.check('idle: lazy Xwayland exits about 10 s after its last client',
                gone == -1 and 8 < waited < 16, f'{waited:.1f} s')
  status, _ = shingle.x_client(['xdpyinfo'], ':0')
  again = shingle.xwayland_pid()
  results.check('idle: the next connection starts it again',
                status == 0 and again > 0 and again != pid, f'pid {again}')
  results.check('idle: SIGTERM exits with status 0',
                shingle.stop() == 0)
  return shingle.log_text()

def eager_checks(results, binary, work):
  """Eager mode: start before restored clients, restart rule."""
  record = os.path.join(work, 'eager-restored')
  command = ['sh', '-c',
             f'cat /proc/$PPID/task/*/children > {record}; exec sleep 30']
  # Xwayland starts with the compositor, so this is no later than its
  # start; the restart rule counts from there.
  started = time.monotonic()
  shingle = Shingle(binary, work, 'eager', {'xwayland': {'mode': True}},
                    session=session_json(command, work))
  pid = shingle.xwayland_pid()
  results.check('eager: Xwayland runs without any X client',
                pid > 0 and command_name(pid) == 'Xwayland', f'pid {pid}')
  deadline = time.monotonic() + 5
  while not os.path.exists(record) and time.monotonic() < deadline:
    time.sleep(0.05)
  seen = open(record).read().split() if os.path.exists(record) else []
  results.check('eager: Xwayland started before the restored client',
                str(pid) in seen, f'restored client saw {seen}')
  status, output = shingle.x_client(['xdpyinfo'], ':0')
  results.check('eager: xdpyinfo succeeds', status == 0, f'status {status}')
  time.sleep(3)
  results.check('eager: the server stays after its last X client',
                shingle.xwayland_pid() == pid)
  # More than 5 s after its start: restarted at once.
  time.sleep(max(0.0, 6.5 - (time.monotonic() - started)))
  if not kill_xwayland(pid):
    results.check(f'refusing to kill pid {pid}', False)
    shingle.stop()
    return
  second = shingle.wait_pid(lambda value: value > 0 and value != pid)
  results.check('eager: killed after 6 s, restarted at once',
                second > 0 and second != pid
                and command_name(second) == 'Xwayland', f'pid {second}')
  status, _ = shingle.x_client(['xdpyinfo'], ':0')
  results.check('eager: the restarted server serves :0', status == 0)
  if not kill_xwayland(second):
    results.check(f'refusing to kill pid {second}', False)
    shingle.stop()
    return
  gone = shingle.wait_pid(lambda value: value == -1)
  time.sleep(1)
  results.check('eager: killed after 2 s, not restarted',
                gone == -1 and shingle.xwayland_pid() == -1)
  results.check('eager: SIGTERM exits with status 0', shingle.stop() == 0)
  results.check('eager: SIGTERM leaves no lock or socket',
                not os.path.exists('/tmp/.X0-lock')
                and not os.path.exists('/tmp/.X11-unix/X0'))

def running_sigterm_checks(results, binary, work):
  """SIGTERM while Xwayland runs stops it and frees the display."""
  shingle = Shingle(binary, work, 'running', {'xwayland': {'mode': 'true'}})
  pid = shingle.wait_pid(lambda value: value > 0)
  status, _ = shingle.x_client(['xdpyinfo'], ':0')
  results.check('running: xdpyinfo succeeds', status == 0)
  exit_status = shingle.stop()
  results.check('running: SIGTERM exits with status 0', exit_status == 0,
                f'{exit_status}')
  results.check('running: Xwayland is gone after SIGTERM',
                command_name(pid) is None, f'pid {pid}')
  results.check('running: no lock or socket after SIGTERM',
                not os.path.exists('/tmp/.X0-lock')
                and not os.path.exists('/tmp/.X11-unix/X0'))
  return shingle.log_text()

def disabled_checks(results, binary, work):
  """mode false: nothing bound, DISPLAY absent."""
  spawn_out = os.path.join(work, 'disabled-spawn-out')
  spawn = ['sh', '-c', f'echo "${{DISPLAY-unset}}" > {spawn_out}']
  shingle = Shingle(binary, work, 'disabled', {'xwayland': {'mode': False}},
                    spawn=spawn)
  display = shingle.call('stipc/get_display')
  results.check('disabled: stipc/get_display reports no X display',
                display.get('xwayland') == '', json.dumps(display))
  results.check('disabled: stipc/get_xwayland_pid is -1',
                shingle.xwayland_pid() == -1)
  results.check('disabled: nothing bound in /tmp',
                not os.path.exists('/tmp/.X0-lock')
                and not os.listdir('/tmp/.X11-unix'))
  deadline = time.monotonic() + 5
  while not os.path.exists(spawn_out) and time.monotonic() < deadline:
    time.sleep(0.05)
  spawned = open(spawn_out).read().strip()
  results.check('disabled: --spawn child has no DISPLAY',
                spawned == 'unset', spawned)
  results.check('disabled: SIGTERM exits with status 0', shingle.stop() == 0)

WORKAREA = {'x': 0, 'y': 0, 'width': 1280, 'height': 720}
# Runs a program as an ordinary user in a nested user namespace. xterm,
# run as the namespace's root, tries to give its pty to group tty, which
# is not mapped, and exits. `unshare` execs the program, so its PID and
# comm stay the program's.
AS_USER = ['unshare', '--user', '--map-user=1000', '--map-group=1000', '--']

def start_mapped(shingle, arguments, display, program, app_id):
  """Starts an X client and waits until its view is mapped."""
  client = XClient(shingle, arguments, display, program)
  view = wait_for(lambda: (shingle.view(app_id) or {}).get('mapped')
                  and shingle.view(app_id))
  return client, view

def windows_checks(results, binary, work, probe):
  """Managed windows: tiling, xwininfo, list-views, close, configure."""
  del work, probe
  shingle = Shingle(binary, None, 'windows', {'xwayland': {'mode': True}})
  shingle.wait_pid(lambda value: value > 0)
  ready = wait_for(lambda: 'Xwayland ready' in shingle.log_text())
  results.check('windows: Xwayland is ready', ready)
  display = ':0'
  clients = []
  order = [('XTerm', AS_USER + ['xterm', '-e', 'sleep', '600'], 'xterm'),
           ('XEyes', ['xeyes'], 'xeyes'),
           ('XLogo', ['xlogo'], 'xlogo')]
  views = {}
  for app_id, arguments, program in order:
    client, view = start_mapped(shingle, arguments, display, program, app_id)
    clients.append(client)
    views[app_id] = view
    results.check(f'windows: {program} maps as a view with app-id {app_id}',
                  bool(view), json.dumps(shingle.views())
                  + f' exit {client.process.poll()} output: '
                  + ' | '.join(client.lines[-5:]))
  try:
    if not all(views.values()):
      return shingle.log_text()
    # Tiling: in mapping order, filling the workarea.
    time.sleep(0.5)
    listed = {view['app-id']: view for view in shingle.views()}
    geometries = [listed[app_id]['geometry'] for app_id, _, _ in order]
    results.check('windows: three views tile the whole workarea',
                  tiles_fill(geometries, WORKAREA), json.dumps(geometries))
    by_position = sorted(order, key=lambda item: (
        listed[item[0]]['geometry']['x'], listed[item[0]]['geometry']['y']))
    results.check('windows: tiles are in mapping order',
                  [item[0] for item in by_position]
                  == [item[0] for item in order],
                  ', '.join(item[0] for item in by_position))
    # list-views: real PIDs and titles.
    for (app_id, _, program), client in zip(order, clients):
      view = listed[app_id]
      results.check(f'windows: {app_id} has its X client\'s pid',
                    view['pid'] == client.pid
                    and command_name(view['pid']) == program,
                    f'view {view["pid"]}, client {client.pid}')
    results.check('windows: xterm\'s title is listed',
                  bool(listed['XTerm']['title']), listed['XTerm']['title'])
    # _NET_CLIENT_LIST and xwininfo.
    windows = shingle.x_windows(display)
    classes = list(windows.values())
    results.check('windows: _NET_CLIENT_LIST lists them in map order',
                  classes == ['XTerm', 'XEyes', 'XLogo'], json.dumps(classes))
    ids = {app_id: window for window, app_id in windows.items()}

    def matches_xwininfo():
      for app_id, _, _ in order:
        view = shingle.view(app_id)
        x_geometry = shingle.x_geometry(ids[app_id], display)
        if not view or not x_geometry:
          return False
        if not same_geometry(view['geometry'], x_geometry):
          return False
      return True
    matched = wait_for(matches_xwininfo, timeout=5)
    detail = '; '.join(
        f'{app_id} view {json.dumps(shingle.view(app_id)["geometry"])} '
        f'x {json.dumps(shingle.x_geometry(ids[app_id], display))}'
        for app_id, _, _ in order)
    results.check('windows: xwininfo matches view-info for all three',
                  matched, detail)
    _, output = shingle.x_client(
        ['xprop', '-id', hex(ids['XTerm']), 'WM_STATE', 'WM_NAME'], display)
    results.check('windows: xterm has WM_STATE Normal',
                  'Normal' in output, output.strip().replace('\n', '; '))
    # The last window mapped is focused, and only it says so.
    focused = {}
    for app_id, _, _ in order:
      _, output = shingle.x_client(
          ['xprop', '-id', hex(ids[app_id]), '_NET_WM_STATE'], display)
      focused[app_id] = '_NET_WM_STATE_FOCUSED' in output
    results.check('windows: only the focused xlogo has _NET_WM_STATE_FOCUSED',
                  focused == {'XTerm': False, 'XEyes': False, 'XLogo': True},
                  json.dumps(focused))
    # simple-tile/set-layout reaches X geometry.
    xterm, xeyes, xlogo = (listed[app_id]['id'] for app_id, _, _ in order)
    layout = {'horizontal-split': [
        {'weight': 1, 'view-id': xlogo},
        {'weight': 3, 'vertical-split': [
            {'weight': 1, 'view-id': xterm},
            {'weight': 1, 'view-id': xeyes},
        ]},
    ]}
    reply = shingle.call('simple-tile/set-layout', {
        'wset-index': 1, 'workspace': {'x': 0, 'y': 0}, 'layout': layout})
    results.check('windows: simple-tile/set-layout is accepted',
                  reply.get('result') == 'ok', json.dumps(reply))
    changed = wait_for(lambda: not same_geometry(
        shingle.view('XLogo')['geometry'], geometries[2])
        and matches_xwininfo(), timeout=5)
    results.check('windows: set-layout reaches the X geometry',
                  changed, json.dumps(shingle.view('XLogo')['geometry']))
    # configure-view floats xlogo at the given geometry.
    target = {'x': 100, 'y': 120, 'width': 400, 'height': 300}
    reply = shingle.call('window-rules/configure-view',
                         {'id': xlogo, 'geometry': target})
    results.check('windows: configure-view is accepted',
                  reply.get('result') == 'ok', json.dumps(reply))
    reached = wait_for(lambda: same_geometry(
        shingle.x_geometry(ids['XLogo'], display) or {}, target), timeout=5)
    results.check('windows: configure-view reaches the X geometry',
                  reached,
                  json.dumps(shingle.x_geometry(ids['XLogo'], display)))
    # close-view: WM_DELETE_WINDOW for xterm, KillClient for xeyes.
    _, xterm_protocols = shingle.x_client(
        ['xprop', '-id', hex(ids['XTerm']), 'WM_PROTOCOLS'], display)
    _, xeyes_protocols = shingle.x_client(
        ['xprop', '-id', hex(ids['XEyes']), 'WM_PROTOCOLS'], display)
    # Both are Xt applications and list WM_DELETE_WINDOW; the probe
    # checks KillClient for a client without it.
    results.check('windows: xterm and xeyes list WM_DELETE_WINDOW',
                  'WM_DELETE_WINDOW' in xterm_protocols
                  and 'WM_DELETE_WINDOW' in xeyes_protocols,
                  f'{xterm_protocols.strip()}; {xeyes_protocols.strip()}')
    for app_id, client in (('XTerm', clients[0]), ('XEyes', clients[1])):
      view_id = listed[app_id]['id']
      reply = shingle.call('window-rules/close-view', {'id': view_id})
      status = client.exited(10)
      gone = wait_for(lambda: shingle.view(app_id) is None, timeout=5)
      results.check(f'windows: close-view closes {app_id}',
                    reply.get('result') == 'ok' and status is not None
                    and gone, f'reply {json.dumps(reply)}, exit {status}')
    remaining = [view['app-id'] for view in shingle.views()]
    results.check('windows: only xlogo is left', remaining == ['XLogo'],
                  json.dumps(remaining))
  finally:
    for client in clients:
      client.stop()
  results.check('windows: SIGTERM exits with status 0', shingle.stop() == 0)
  return shingle.log_text()

def probe_checks(results, binary, work, probe):
  """The probe client: view-pre-map, ConfigureNotify, races, close."""
  del work
  shingle = Shingle(binary, None, 'probe', {'xwayland': {'mode': True}})
  shingle.wait_pid(lambda value: value > 0)
  wait_for(lambda: 'Xwayland ready' in shingle.log_text())
  display = ':0'
  clients = []
  try:
    # view-pre-map holds an X window until unblock-map.
    watcher = Ipc(shingle.ipc_path)
    watched = watcher.call('window-rules/events/watch',
                           {'events': ['view-pre-map']})
    results.check('probe: view-pre-map can be watched',
                  watched == {'result': 'ok'}, json.dumps(watched))
    held = XClient(shingle, [probe, '--class', 'held,Held'], display,
                   'xw_probe')
    clients.append(held)
    try:
      event = watcher.receive()
    except (TimeoutError, ConnectionError) as error:
      event = {'error': str(error)}
    view = event.get('view', {})
    results.check('probe: view-pre-map names the X window',
                  event.get('event') == 'view-pre-map'
                  and view.get('app-id') == 'Held', json.dumps(event))
    time.sleep(0.5)
    listed = shingle.view('Held')
    results.check('probe: the held window is not mapped',
                  listed is not None and not listed['mapped'],
                  json.dumps(listed))
    reply = watcher.call('window-rules/unblock-map', {'id': view.get('id')})
    results.check('probe: unblock-map is accepted',
                  reply.get('result') == 'ok', json.dumps(reply))
    mapped = wait_for(lambda: (shingle.view('Held') or {}).get('mapped'))
    results.check('probe: unblock-map maps the X window', mapped)
    watcher.close()
    # The probe sees the tile geometry in ConfigureNotify.
    geometry = shingle.view('Held')['geometry']

    def configured_to(client, geometry):
      def find():
        configures = [event for event in client.events()
                      if event.get('event') == 'configure']
        return configures and same_geometry(configures[-1], geometry)
      return find
    results.check('probe: ConfigureNotify reports the tile geometry',
                  wait_for(configured_to(held, geometry)),
                  json.dumps([e for e in held.events()
                              if e.get('event') == 'configure'][-1:]))
    results.check('probe: the view has the probe\'s pid',
                  shingle.view('Held')['pid'] == held.pid)
    # A tiled window asking for another geometry gets its tile back in a
    # synthetic ConfigureNotify.
    count = len(held.events())
    held.send('configure 5 5 100 100')
    synthetic = held.wait_event('configure', after=count)
    results.check('probe: a tiled ConfigureRequest gets the tile back',
                  synthetic is not None and synthetic.get('synthetic')
                  and same_geometry(synthetic, geometry),
                  json.dumps(synthetic))
    # close-view sends WM_DELETE_WINDOW.
    reply = shingle.call('window-rules/close-view',
                         {'id': shingle.view('Held')['id']})
    deleted = held.wait_event('delete')
    results.check('probe: close-view sends WM_DELETE_WINDOW',
                  reply.get('result') == 'ok' and deleted is not None,
                  json.dumps(reply))
    held.exited(5)
    results.check('probe: the deleted window\'s view is gone',
                  wait_for(lambda: shingle.view('Held') is None))
    # An X window destroyed before its surface commits a buffer.
    for attempt in range(5):
      racer = XClient(shingle, [probe, '--class', 'racer,Racer',
                                '--no-background'], display, 'xw_probe')
      clients.append(racer)
      racer.wait_event('window')
      racer.wait_event('mapped')
      racer.send('destroy')
      racer.exited(5)
    time.sleep(1)
    results.check('probe: windows destroyed before their first buffer '
                  'leave no view',
                  all(view['app-id'] != 'Racer' for view in shingle.views()),
                  json.dumps(shingle.views()))
    # Unmapped at once after the map, so the surface goes before or
    # around the serial; then mapped again.
    flicker = XClient(shingle, [probe, '--class', 'flicker,Flicker'],
                      display, 'xw_probe')
    clients.append(flicker)
    flicker.wait_event('mapped')
    cycles = 0
    for _ in range(10):
      # Unmapped as soon as the server reports it mapped, which is when
      # Xwayland creates its surface and announces the serial.
      count = len(flicker.events())
      flicker.send('unmap')
      if not flicker.wait_event('unmapped', after=count):
        break
      count = len(flicker.events())
      flicker.send('map')
      if not flicker.wait_event('mapped', after=count):
        break
      cycles += 1
    count = len(flicker.events())
    flicker.send('unmap')
    flicker.wait_event('unmapped', after=count)
    time.sleep(1)
    results.check('probe: after 10 map-unmap cycles an unmapped window '
                  'leaves no view',
                  cycles == 10 and shingle.view('Flicker') is None,
                  f'{cycles} cycles, {json.dumps(shingle.views())}')
    flicker.send('map')
    remapped = wait_for(lambda: (shingle.view('Flicker') or {}).get('mapped'))
    count = sum(1 for view in shingle.views() if view['app-id'] == 'Flicker')
    results.check('probe: mapped again, it is one view again',
                  remapped and count == 1, f'{count} views')
    flicker.send('destroy')
    flicker.exited(5)
    results.check('probe: destroyed, it leaves no view',
                  wait_for(lambda: shingle.view('Flicker') is None))
    # A window without WM_DELETE_WINDOW is killed by close-view.
    stubborn = XClient(shingle, [probe, '--class', 'stubborn,Stubborn',
                                 '--no-delete'], display, 'xw_probe')
    clients.append(stubborn)
    view = wait_for(lambda: (shingle.view('Stubborn') or {}).get('mapped')
                    and shingle.view('Stubborn'))
    if view:
      shingle.call('window-rules/close-view', {'id': view['id']})
    status = stubborn.exited(10)
    results.check('probe: close-view kills a client without '
                  'WM_DELETE_WINDOW', status is not None
                  and stubborn.wait_event('delete', timeout=0.1) is None,
                  f'exit {status}')
    results.check('probe: no X window is left',
                  wait_for(lambda: shingle.views() == []),
                  json.dumps(shingle.views()))
  finally:
    for client in clients:
      client.stop()
  results.check('probe: SIGTERM exits with status 0', shingle.stop() == 0)
  return shingle.log_text()

GROUPS = ('lazy', 'disabled', 'eager', 'running', 'idle', 'windows', 'probe')

def inner(binary, probe, work, groups):
  """Runs the check `groups` inside the namespace; returns the status."""
  private_tmp = os.path.join(work, 'tmp')
  os.makedirs(private_tmp)
  subprocess.run(['mount', '--bind', private_tmp, '/tmp'], check=True)
  for device in ('/dev/dri', '/dev/input'):
    if os.path.isdir(device):
      subprocess.run(['mount', '-t', 'tmpfs', 'none', device], check=True)
  os.mkdir('/tmp/.X11-unix', 0o1777)
  os.chmod('/tmp/.X11-unix', 0o1777)
  results = Results()
  results.check('the namespace shows an empty /tmp/.X11-unix',
                os.listdir('/tmp/.X11-unix') == [])
  results.check('/dev/dri is empty in the namespace',
                not os.path.isdir('/dev/dri') or not os.listdir('/dev/dri'))
  checks = {
      'lazy': lazy_checks,
      'disabled': disabled_checks,
      'eager': eager_checks,
      'running': running_sigterm_checks,
      'idle': idle_checks,
      'windows': windows_checks,
      'probe': probe_checks,
  }
  for group in groups:
    if group in ('windows', 'probe'):
      log = checks[group](results, binary, work, probe)
    else:
      log = checks[group](results, binary, work)
    if log:
      print(f'--- {group} run log ---\n{log}--- end ---', flush=True)
  passed = sum(1 for _, ok, _ in results.checks if ok)
  print(f'{passed}/{len(results.checks)} checks passed', flush=True)
  return 0 if results.all_passed() else 1

def main():
  """Sets up the work directory and re-runs itself in the namespace."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--probe', default='target/debug/examples/xw_probe')
  parser.add_argument('--keep', action='store_true')
  parser.add_argument('--only', action='append', choices=GROUPS,
                      help='run only this group of checks (repeatable)')
  parser.add_argument(INNER_FLAG, metavar='WORK')
  options = parser.parse_args()
  binary = os.path.abspath(options.binary)
  probe = os.path.abspath(options.probe)
  groups = options.only or list(GROUPS)
  if options.inner:
    return inner(binary, probe, options.inner, groups)
  work = os.path.abspath(os.path.join(
      'target', f'xwayland_test-{os.getpid()}'))
  os.makedirs(work)
  try:
    # The PID namespace confines every signal the checks send to processes
    # started inside it, so a wrong PID can never reach the host session.
    command = ['unshare', '--user', '--map-root-user', '--mount', '--net',
               '--pid', '--fork', '--mount-proc', '--',
               sys.executable, os.path.abspath(__file__),
               '--binary', binary, '--probe', probe, INNER_FLAG, work]
    for group in groups:
      command += ['--only', group]
    return subprocess.run(command).returncode
  finally:
    if not options.keep:
      shutil.rmtree(work, ignore_errors=True)

if __name__ == '__main__':
  sys.exit(main())
