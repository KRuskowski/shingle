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
  cargo build -p shingle --example wl_probe
  tools/xwayland_test/run.py [--binary target/debug/shingle]
      [--probe target/debug/examples/xw_probe]
      [--wl-probe target/debug/examples/wl_probe] [--keep]
      [--only lazy|disabled|eager|running|idle|windows|probe|behaviour|
              session|nested|selection|outputs|decoration ...]

It prints one line per check and exits 0 when all pass. Every signal it
sends goes to its own children or, through `kill_xwayland`, to a
checked Xwayland PID inside the PID namespace. The pure helpers are
tested by `python3 -m unittest` in this directory (test_run.py). The work
directory under `target/` is removed afterwards unless `--keep` is given.
The checks are XW-2's to XW-5's in docs/design/xwayland.md, section 7.
Input comes only from STIPC. The `session` group runs Shingle as uid 1000
in a nested user namespace, like the X clients, so that the xterm it
restores can start. The `nested` group runs nested Shingle on
lavapipe (no GPU: `/dev/dri` is empty) as a client of a headless one,
both inside the namespace, and checks pixels in wlr-screencopy captures
of the headless output, which show the frames nested drew. The
`selection` group copies between `xclip` and `wl-copy`/`wl-paste` in both
directions; they too run only inside the namespace, against the private
Shingle, so the operator's clipboard is never touched. The `outputs`
group (MO-3, docs/design/multi-output.md) runs headless Shingle with two
outputs and checks that `xrandr --listmonitors` lists both at their
positions and `xdpyinfo` reports their union as the root, also after
`wlr-randr --pos` moves one. The `decoration` group (WG-6,
docs/design/wayfire-gaps.md) checks that `xterm` gets a server-side
frame and an `xw_probe --no-decorations` window none, tiled and
floating, from IPC's geometries and `xwininfo`.
"""

import argparse
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import re
import sys
import tempfile
import threading
import time

INNER_FLAG = '--inner'
IPC_TIMEOUT = 10.0
# How long Shingle gets to exit after a signal before it is killed. Exit
# syncs the session file to disk, which took more than 10 s while other
# builds loaded the shared disk (fsync, system call 74).
STOP_TIMEOUT = 60.0
# An exit slower than this records where Shingle was waiting.
SLOW_STOP = 5.0
# The probe client refuses to run without it, so it never opens a window
# on the host's X displays.
NAMESPACE_VARIABLE = 'SHINGLE_XWAYLAND_TEST_NAMESPACE'
# When set to a directory, every settled capture is saved there as
# NNN.ppm in the order taken, so that two builds can be compared.
CAPTURES_VARIABLE = 'SHINGLE_XWAYLAND_TEST_CAPTURES'

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

def describe_wait(wchan, syscall):
  """Names what a process waits in from its /proc `wchan` and `syscall`
  files: the kernel function and the system call number."""
  number = syscall.split()[0] if syscall.split() else '?'
  if number == 'running':
    return 'nothing (running)'
  return f'{wchan or "?"}, system call {number}'

def blocked_in(pid):
  """What `pid` waits in now, from /proc, or None when it is gone."""
  try:
    with open(f'/proc/{pid}/wchan') as file:
      wchan = file.read().strip()
    with open(f'/proc/{pid}/syscall') as file:
      syscall = file.read()
  except OSError:
    return None
  return describe_wait(wchan, syscall)

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

XRANDR_MONITOR = re.compile(
    r'^\s*\d+: \S+ (\d+)/\d+x(\d+)/\d+([+-]\d+)([+-]\d+)\s+(\S+)$')

def parse_listmonitors(text):
  """The monitors `xrandr --listmonitors` lists, as (name, (x, y, width,
  height)) in its order."""
  monitors = []
  for line in text.splitlines():
    match = XRANDR_MONITOR.match(line)
    if match:
      width, height, x, y, name = match.groups()
      monitors.append((name, (int(x), int(y), int(width), int(height))))
  return monitors

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

KEY_NAMES = {' ': 'KEY_SPACE', '\n': 'KEY_ENTER', '-': 'KEY_MINUS',
             '.': 'KEY_DOT', '/': 'KEY_SLASH'}

def parse_children(text):
  """The window ids `xwininfo -root -children` lists, in its order, which
  is XQueryTree's: bottom to top."""
  ids = []
  for line in text.splitlines():
    line = line.strip()
    if line.startswith('0x'):
      try:
        ids.append(int(line.split()[0], 16))
      except ValueError:
        pass
  # xwininfo prints children top to bottom; return bottom to top.
  return list(reversed(ids))

def xcursor_image(path, size):
  """(width, height, xhot, yhot) of the image in the Xcursor file `path`
  whose nominal size is nearest `size`, the first on a tie, as the xcursor
  crate picks it; None if the file cannot be read."""
  try:
    with open(path, 'rb') as file:
      data = file.read()
  except OSError:
    return None
  if data[:4] != b'Xcur':
    return None
  (count,) = struct.unpack_from('<I', data, 12)
  best = None
  for index in range(count):
    kind, nominal, position = struct.unpack_from('<III', data, 16 + 12 * index)
    if kind != 0xfffd0002:
      continue
    width, height, xhot, yhot = struct.unpack_from('<IIII', data, position + 16)
    distance = abs(nominal - size)
    if best is None or distance < best[0]:
      best = (distance, (width, height, xhot, yhot))
  return best[1] if best else None

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

# The host user's uid, passed into the namespace, where the user is
# root, so that its /run/user directory can be covered there.
HOST_UID_VARIABLE = 'SHINGLE_TEST_HOST_UID'

def host_uid_environment(environment):
  """`environment` with the host uid for `hide_host_runtime`."""
  environment = dict(environment)
  environment[HOST_UID_VARIABLE] = str(os.getuid())
  return environment

def hide_host_runtime():
  """Covers the host user's /run/user/<uid> with an empty tmpfs inside
  the mount namespace, so that no process in it reaches the host's
  Wayland, D-Bus or PipeWire sockets even by absolute path. Returns the
  directory, or None when the host has none or no uid was passed."""
  uid = os.environ.get(HOST_UID_VARIABLE)
  if not uid or not uid.isdigit():
    return None
  runtime = f'/run/user/{uid}'
  if not os.path.isdir(runtime):
    return None
  subprocess.run(['mount', '-t', 'tmpfs', '-o', 'mode=0700', 'none',
                  runtime], check=True)
  return runtime

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

  def __init__(self, binary, work, name, config, spawn=None, session=None,
               backend=('--headless',), prefix=(), extra_environment=None):
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
    environment.update(extra_environment or {})
    arguments = [*prefix, binary, *backend, '--config', self.config_path,
                 '--ipc', self.ipc_path]
    if spawn:
      arguments += ['--spawn'] + spawn
    self.log_path = os.path.join(self.directory, 'shingle.log')
    self.log = open(self.log_path, 'w')
    self.stop_seconds = None
    self.stop_blocked = None
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

  def wayland_display(self):
    """The `WAYLAND_DISPLAY` Shingle printed, as an absolute path."""
    name = wait_for(lambda: next(
        (line.split('=', 1)[1] for line in self.log_text().splitlines()
         if line.startswith('WAYLAND_DISPLAY=')), None))
    return os.path.join(self.runtime, name) if name else None

  def wayland_environment(self):
    """The environment for a Wayland client of this Shingle."""
    return {
        'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
        'HOME': os.path.join(self.directory, 'home'),
        'XDG_RUNTIME_DIR': self.runtime,
        'WAYLAND_DISPLAY': self.wayland_display(),
        NAMESPACE_VARIABLE: '1',
    }

  def press(self, key, pressed):
    """`stipc/feed_key` for one evdev key name."""
    return self.call('stipc/feed_key', {'key': key, 'state': pressed})

  def type_text(self, text):
    """Types lowercase letters, digits, spaces and newlines through STIPC."""
    for character in text:
      key = KEY_NAMES.get(character, f'KEY_{character.upper()}')
      self.press(key, True)
      self.press(key, False)

  def click(self, x, y, button='BTN_LEFT'):
    """Moves the STIPC pointer to (x, y) and clicks `button` there."""
    self.call('stipc/move_cursor', {'x': x, 'y': y})
    return self.call('stipc/feed_button', {'combo': button, 'mode': 'full'})

  def focused_view(self):
    """`window-rules/get-focused-view`'s view, or None."""
    info = self.call('window-rules/get-focused-view').get('info')
    return info or None

  def focus_view(self, view_id):
    """`window-rules/focus-view`."""
    return self.call('window-rules/focus-view', {'id': view_id})

  def view_by_id(self, view_id):
    """The listed view with `view_id`, or None."""
    return next((view for view in self.views() if view['id'] == view_id),
                None)

  def root_property(self, name, display):
    """The text of a root window property, as `xprop -root` prints it."""
    _, output = self.x_client(['xprop', '-root', name], display)
    return parse_xprop(output).get(name, '')

  def window_property(self, window, name, display):
    """The text of a window property, as `xprop -id` prints it."""
    _, output = self.x_client(['xprop', '-id', hex(window), name], display)
    return parse_xprop(output).get(name, '')

  def active_window(self, display):
    """`_NET_ACTIVE_WINDOW` as a window id; 0 for None."""
    ids = parse_window_list(self.root_property('_NET_ACTIVE_WINDOW', display))
    return ids[0] if ids else 0

  def x_tree(self, display):
    """The root's children, bottom to top, as `xwininfo -root -children`
    lists them: [(id, override_redirect, viewable, geometry)]."""
    _, output = self.x_client(['xwininfo', '-root', '-children'], display)
    children = []
    for window in parse_children(output):
      _, info = self.x_client(['xwininfo', '-id', hex(window)], display)
      children.append((window, 'Override Redirect State: yes' in info,
                       'Map State: IsViewable' in info,
                       parse_xwininfo(info)))
    return children

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

  def stop(self, sig=signal.SIGTERM, timeout=STOP_TIMEOUT):
    """Sends `sig` and waits; returns the exit status, or 'timeout' once it
    had to be killed. Sets `stop_seconds` to how long the exit took and,
    when it took more than SLOW_STOP seconds, `stop_blocked` to where
    Shingle was waiting by then."""
    self.ipc.close()
    self.process.send_signal(sig)
    started = time.monotonic()
    try:
      try:
        return self.process.wait(min(SLOW_STOP, timeout))
      except subprocess.TimeoutExpired:
        self.stop_blocked = blocked_in(self.process.pid)
      return self.process.wait(max(0.0, timeout - SLOW_STOP))
    except subprocess.TimeoutExpired:
      self.process.kill()
      self.process.wait()
      return 'timeout'
    finally:
      self.stop_seconds = time.monotonic() - started
      self.log.close()

  def stop_detail(self, status):
    """`status` with how long the exit took, for a check's detail."""
    detail = f'{status} after {self.stop_seconds:.1f} s'
    if self.stop_blocked:
      detail += f', at {SLOW_STOP} s blocked in {self.stop_blocked}'
    return detail

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
  results.check('list-methods returns 87 methods', len(methods) == 87,
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
  results.check('lazy: SIGTERM exits with status 0', status == 0,
                shingle.stop_detail(status))
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
  status = shingle.stop()
  results.check('idle: SIGTERM exits with status 0', status == 0,
                shingle.stop_detail(status))
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
  status = shingle.stop()
  results.check('eager: SIGTERM exits with status 0', status == 0,
                shingle.stop_detail(status))
  results.check('eager: SIGTERM leaves no lock or socket',
                not os.path.exists('/tmp/.X0-lock')
                and not os.path.exists('/tmp/.X11-unix/X0'))
  return shingle.log_text()

def running_sigterm_checks(results, binary, work):
  """SIGTERM while Xwayland runs stops it and frees the display."""
  shingle = Shingle(binary, work, 'running', {'xwayland': {'mode': 'true'}})
  pid = shingle.wait_pid(lambda value: value > 0)
  status, _ = shingle.x_client(['xdpyinfo'], ':0')
  results.check('running: xdpyinfo succeeds', status == 0)
  exit_status = shingle.stop()
  results.check('running: SIGTERM exits with status 0', exit_status == 0,
                shingle.stop_detail(exit_status))
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

# Wayfire's default server-side frame (WG-6): 4-pixel borders and a
# 30-pixel title bar.
FRAME_BORDER = 4
FRAME_TITLE = 30

def inside_frame(geometry):
  """The client rectangle inside a default frame at `geometry`."""
  return {
      'x': geometry['x'] + FRAME_BORDER,
      'y': geometry['y'] + FRAME_TITLE + FRAME_BORDER,
      'width': geometry['width'] - 2 * FRAME_BORDER,
      'height': geometry['height'] - FRAME_TITLE - 2 * FRAME_BORDER,
  }

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

    # The X window is the view's base geometry, inside a server-side
    # frame when it has one (WG-6).
    def matches_xwininfo():
      for app_id, _, _ in order:
        view = shingle.view(app_id)
        x_geometry = shingle.x_geometry(ids[app_id], display)
        if not view or not x_geometry:
          return False
        if not same_geometry(view['base-geometry'], x_geometry):
          return False
      return True
    matched = wait_for(matches_xwininfo, timeout=5)
    detail = '; '.join(
        f'{app_id} view '
        f'{json.dumps(shingle.view(app_id)["base-geometry"])} '
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
        shingle.view('XLogo')['geometry'], target) and same_geometry(
            shingle.x_geometry(ids['XLogo'], display) or {},
            inside_frame(target)), timeout=5)
    results.check('windows: configure-view reaches the X geometry, inside '
                  'the frame', reached,
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
    # The probe sees the tile geometry in ConfigureNotify, inside the
    # tile's server-side frame (WG-6), which is the view's base geometry.
    geometry = shingle.view('Held')['base-geometry']

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

def probe_window(client):
  """The X window id the probe reported, or None."""
  event = client.wait_event('window')
  return int(event['id']) if event else None

def has_event(client, name, after=0, timeout=1.0):
  """Whether the client reports `name` at index `after` or later within
  `timeout` seconds."""
  return client.wait_event(name, timeout=timeout, after=after) is not None

def last_event(client, name):
  """The client's latest `name` event, or None."""
  events = [event for event in client.events() if event.get('event') == name]
  return events[-1] if events else None

def probe_query(client, command, name, timeout=5.0):
  """Sends `command` and returns the `name` event it reports."""
  count = len(client.events())
  client.send(command)
  return client.wait_event(name, timeout=timeout, after=count)

def centre(geometry):
  """The centre of an {x, y, width, height} rectangle."""
  return (geometry['x'] + geometry['width'] // 2,
          geometry['y'] + geometry['height'] // 2)

def start_probe(shingle, probe, display, app_id, *options):
  """Starts the probe with class `app_id`, waits for its view, and returns
  (client, X window, view)."""
  client = XClient(shingle, [probe, '--class', f'{app_id.lower()},{app_id}',
                             '--title', app_id, *options], display, 'xw_probe')
  window = probe_window(client)
  view = wait_for(lambda: (shingle.view(app_id) or {}).get('mapped')
                  and shingle.view(app_id))
  return client, window, view

def focused_id(shingle):
  """The ID of the focused view, or None."""
  return (shingle.focused_view() or {}).get('id')

class WaylandClient(XClient):
  """The Wayland probe client, with its stdout read as JSON lines."""

  def __init__(self, shingle, wl_probe, app_id, *options):
    # pylint: disable=super-init-not-called
    self.program = 'wl_probe'
    self.process = subprocess.Popen(
        [wl_probe, app_id, *options], env=shingle.wayland_environment(),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, text=True)
    self.lines = []
    self.lock = threading.Lock()
    self.reader = threading.Thread(target=self._read, daemon=True)
    self.reader.start()

def behaviour_checks(results, binary, work, probe, wl_probe):
  """XW-4: focus, input models, menus, states, dialogs, stacking, rules
  and the root cursor, with STIPC input."""
  del work
  config = {
      'xwayland': {'mode': True},
      'window_rules': ['on created if app_id is "XTerm" then set alpha 0.5'],
      'input': {'cursor_theme': 'Adwaita', 'cursor_size': 24},
  }
  shingle = Shingle(binary, None, 'behaviour', config)
  shingle.wait_pid(lambda value: value > 0)
  results.check('behaviour: Xwayland is ready',
                wait_for(lambda: 'Xwayland ready' in shingle.log_text()))
  display = ':0'
  _, root = shingle.x_client(['xwininfo', '-root'], display)
  results.check('behaviour: the X root window is the output\'s size',
                same_geometry(parse_xwininfo(root) or {}, WORKAREA),
                json.dumps(parse_xwininfo(root)))
  clients = []
  try:
    _behaviour_xterm(results, shingle, display, clients)
    focus_probe = _behaviour_focus(results, shingle, display, clients, probe,
                                   wl_probe)
    _behaviour_models(results, shingle, display, clients, probe)
    _behaviour_dialogs(results, shingle, display, clients, probe)
    _behaviour_states(results, shingle, display, clients, probe)
    _behaviour_stacking(results, shingle, display)
    _behaviour_menus(results, shingle, display)
    _behaviour_override_redirect(results, shingle, display, clients, probe)
    if focus_probe:
      _behaviour_cursor(results, shingle, focus_probe)
  finally:
    for client in clients:
      client.stop()
  results.check('behaviour: SIGTERM exits with status 0', shingle.stop() == 0)
  return shingle.log_text()

def _behaviour_xterm(results, shingle, display, clients):
  """Typed text reaches the focused xterm; the XTerm rule applies."""
  typed = os.path.join(shingle.directory, 'typed')
  script = f'read line; echo "$line" > {typed}; exec sleep 600'
  client, view = start_mapped(
      shingle, AS_USER + ['xterm', '-e', 'sh', '-c', script], display,
      'xterm', 'XTerm')
  clients.append(client)
  results.check('behaviour: xterm maps', bool(view))
  if not view:
    return
  alpha = shingle.call('wf/alpha/get-view-alpha', {'id': view['id']})
  results.check('behaviour: an app_id is "XTerm" window rule applies',
                alpha.get('alpha') == 0.5, json.dumps(alpha))
  results.check('behaviour: the new xterm has the keyboard focus',
                wait_for(lambda: focused_id(shingle) == view['id']))
  window = wait_for(lambda: shingle.active_window(display))
  results.check('behaviour: _NET_ACTIVE_WINDOW names the xterm',
                window and shingle.x_windows(display).get(window) == 'XTerm',
                hex(window or 0))
  # xterm sets up its pty and shell after it maps.
  time.sleep(1)
  shingle.type_text('hello shingle\n')

  def read_typed():
    try:
      with open(typed) as file:
        return file.read()
    except FileNotFoundError:
      return None
  text = wait_for(read_typed, timeout=5)
  results.check('behaviour: STIPC keys reach the focused xterm',
                text == 'hello shingle\n', repr(text))

def _behaviour_focus(results, shingle, display, clients, probe, wl_probe):
  """Keyboard focus moves cleanly between an X and a Wayland window."""
  xprobe, window, view = start_probe(shingle, probe, display, 'FocusProbe')
  clients.append(xprobe)
  results.check('focus: the probe maps and gets FocusIn',
                view and has_event(xprobe, 'focus_in', timeout=5))
  if not view:
    return None
  results.check('focus: _NET_ACTIVE_WINDOW names the probe',
                wait_for(lambda: shingle.active_window(display) == window),
                hex(shingle.active_window(display)))
  state = wait_for(lambda: '_NET_WM_STATE_FOCUSED' in shingle.window_property(
      window, '_NET_WM_STATE', display))
  xterm = next((w for w, c in shingle.x_windows(display).items()
                if c == 'XTerm'), 0)
  xterm_state = shingle.window_property(xterm, '_NET_WM_STATE', display)
  results.check('focus: only the probe has _NET_WM_STATE_FOCUSED',
                state and '_NET_WM_STATE_FOCUSED' not in xterm_state,
                xterm_state)
  focus = probe_query(xprobe, 'focus', 'focus')
  results.check('focus: the X focus is the probe window',
                focus and focus['own'], json.dumps(focus))
  count = len(xprobe.events())
  wayland = WaylandClient(shingle, wl_probe, 'wl-probe')
  clients.append(wayland)
  wl_view = wait_for(lambda: (shingle.view('wl-probe') or {}).get('mapped')
                     and shingle.view('wl-probe'))
  results.check('focus: the Wayland probe maps and gets wl_keyboard.enter',
                wl_view and wayland.wait_event('enter', timeout=5),
                ' | '.join(wayland.lines[-3:]))
  results.check('focus: the X probe gets FocusOut',
                has_event(xprobe, 'focus_out', after=count, timeout=5))
  results.check('focus: _NET_ACTIVE_WINDOW is None for a Wayland window',
                wait_for(lambda: shingle.active_window(display) == 0),
                hex(shingle.active_window(display)))
  focus = probe_query(xprobe, 'focus', 'focus')
  results.check('focus: no X window has the X focus then',
                focus and not focus['own'] and focus['window'] in (0, 1),
                json.dumps(focus))
  # Back to X with a click.
  count = len(xprobe.events())
  wl_count = len(wayland.events())
  view = shingle.view('FocusProbe')
  x, y = centre(view['geometry'])
  shingle.click(x, y)
  pointer = probe_query(xprobe, 'pointer', 'pointer') or {}
  results.check('focus: X sees the pointer where Shingle put it',
                (pointer.get('x'), pointer.get('y')) == (x, y)
                and pointer.get('own'), json.dumps(pointer))
  results.check('focus: a click moves the focus back to the X probe',
                wait_for(lambda: focused_id(shingle) == view['id'])
                and has_event(xprobe, 'focus_in', after=count, timeout=5)
                and has_event(wayland, 'leave', after=wl_count, timeout=5))
  results.check('focus: _NET_ACTIVE_WINDOW names the probe again',
                wait_for(lambda: shingle.active_window(display) == window))
  # And to Wayland with focus-view.
  wl_count = len(wayland.events())
  count = len(xprobe.events())
  shingle.focus_view(wl_view['id'])
  results.check('focus: focus-view moves it to the Wayland probe',
                has_event(wayland, 'enter', after=wl_count, timeout=5)
                and has_event(xprobe, 'focus_out', after=count, timeout=5))
  wayland.stop()
  clients.remove(wayland)
  wait_for(lambda: shingle.view('wl-probe') is None)
  return xprobe

def _behaviour_models(results, shingle, display, clients, probe):
  """Probe windows with each ICCCM input model, and the FocusIn guard."""
  local, _, local_view = start_probe(shingle, probe, display, 'Local',
                                     '--take-focus')
  clients.append(local)
  results.check('models: a locally active window is focused on map and '
                'gets WM_TAKE_FOCUS and FocusIn',
                local_view and wait_for(lambda: focused_id(shingle)
                                        == local_view['id'])
                and has_event(local, 'take_focus', timeout=5)
                and has_event(local, 'focus_in', timeout=5))
  focus = probe_query(local, 'focus', 'focus')
  results.check('models: the locally active window has the X focus',
                focus and focus['own'], json.dumps(focus))
  before = focused_id(shingle)
  glob, _, glob_view = start_probe(shingle, probe, display, 'Global',
                                   '--no-input', '--take-focus',
                                   '--type', 'normal')
  clients.append(glob)
  time.sleep(1)
  results.check('models: a globally active NORMAL window is offered the '
                'focus (WM_TAKE_FOCUS) but not focused on map',
                glob_view and has_event(glob, 'take_focus', timeout=5)
                and focused_id(shingle) == before
                and not has_event(glob, 'focus_in', timeout=0.5),
                json.dumps(shingle.focused_view()))
  none, _, none_view = start_probe(shingle, probe, display, 'NoInput',
                                   '--no-input')
  clients.append(none)
  time.sleep(1)
  results.check('models: a no-input window is not focused on map and gets '
                'neither WM_TAKE_FOCUS nor FocusIn',
                none_view and focused_id(shingle) == before
                and not has_event(none, 'take_focus', timeout=0.5)
                and not has_event(none, 'focus_in', timeout=0.5))
  if not (glob_view and none_view and local_view):
    return
  # Clicks focus them in Shingle, as Wayfire's X toplevels always are.
  count = len(glob.events())
  shingle.click(*centre(shingle.view('Global')['geometry']))
  focused = wait_for(lambda: focused_id(shingle) == glob_view['id'])
  took = has_event(glob, 'take_focus', after=count, timeout=5)
  focus = probe_query(glob, 'focus', 'focus') or {}
  results.check('models: a click focuses the globally active window, which '
                'gets WM_TAKE_FOCUS but no SetInputFocus',
                focused and took and not focus.get('own'), json.dumps(focus))
  shingle.click(*centre(shingle.view('NoInput')['geometry']))
  focused = wait_for(lambda: focused_id(shingle) == none_view['id'])
  focus = probe_query(none, 'focus', 'focus') or {}
  results.check('models: a click focuses the no-input window in Shingle, and '
                'the X focus goes to None', focused and not focus.get('own')
                and focus.get('window') in (0, 1), json.dumps(focus))
  # The FocusIn guard: another client cannot take the X focus.
  shingle.focus_view(local_view['id'])
  wait_for(lambda: (probe_query(local, 'focus', 'focus') or {}).get('own'))
  count = len(none.events())
  none.send('grab_focus')
  has_event(none, 'focus_in', after=count, timeout=3)
  time.sleep(0.5)
  focus = probe_query(local, 'focus', 'focus') or {}
  results.check('models: an X client that sets the focus to itself gets it '
                'taken back (FocusIn guard)',
                focus.get('own') and has_event(none, 'focus_out', after=count,
                                               timeout=3),
                json.dumps(focus))
  for client in (glob, none):
    client.send('destroy')
    client.exited(5)

def _behaviour_dialogs(results, shingle, display, clients, probe):
  """A transient dialog and a fixed-size dialog float as in Wayfire."""
  parent_window = next(w for w, c in shingle.x_windows(display).items()
                       if c == 'FocusProbe')
  dialog, window, view = start_probe(
      shingle, probe, display, 'Dialog', '--transient-for',
      hex(parent_window), '--size', '300x200')
  clients.append(dialog)
  parent = shingle.view('FocusProbe')
  results.check('dialogs: a transient window is a child of its parent',
                view and view['parent'] == parent['id'], json.dumps(view))
  if not view:
    return
  # Its frame (WG-6) is centred on its parent's, as Wayfire centres
  # views, and the X window, 300x200, is inside it.
  geometry = view['geometry']
  pg = parent['geometry']
  framed = {'width': 300 + 2 * FRAME_BORDER,
            'height': 200 + FRAME_TITLE + 2 * FRAME_BORDER}
  expected_x = pg['x'] + (pg['width'] - framed['width']) // 2
  expected_y = pg['y'] + (pg['height'] - framed['height']) // 2
  results.check('dialogs: the transient floats centred on its parent',
                view['tiled-edges'] == 0
                and geometry['width'] == framed['width']
                and geometry['height'] == framed['height']
                and same_geometry(view['base-geometry'],
                                  inside_frame(geometry))
                and abs(geometry['x'] - expected_x) <= 1
                and abs(geometry['y'] - expected_y) <= 1,
                f'{json.dumps(geometry)} parent {json.dumps(pg)}')
  results.check('dialogs: the transient is focused',
                focused_id(shingle) == view['id'])
  results.check('dialogs: xwininfo matches the dialog view',
                wait_for(lambda: same_geometry(
                    shingle.x_geometry(window, display) or {},
                    shingle.view('Dialog')['base-geometry']), timeout=5),
                json.dumps(shingle.x_geometry(window, display)))
  dialog.send('destroy')
  dialog.exited(5)
  fixed, window, view = start_probe(shingle, probe, display, 'Fixed',
                                    '--size', '300x200', '--min', '300x200',
                                    '--max', '300x200', '--type', 'dialog')
  clients.append(fixed)
  # At its size inside its frame (WG-6), which is centred.
  results.check('dialogs: a fixed-size dialog floats at its size',
                view and view['tiled-edges'] == 0
                and view['base-geometry']['width'] == 300
                and view['base-geometry']['height'] == 200
                and same_geometry(view['base-geometry'],
                                  inside_frame(view['geometry']))
                and view['min-size'] == {'width': 300, 'height': 200},
                json.dumps(view))
  if view:
    g = view['geometry']
    results.check('dialogs: it is placed in the centre of the workarea',
                  abs(g['x'] - (WORKAREA['width'] - g['width']) // 2) <= 1
                  and abs(g['y'] - (WORKAREA['height'] - g['height']) // 2)
                  <= 1, json.dumps(g))
  fixed.send('destroy')
  fixed.exited(5)

def _net_wm_state(shingle, window, display):
  """The `_NET_WM_STATE` atoms of `window`."""
  value = shingle.window_property(window, '_NET_WM_STATE', display)
  return [atom.strip() for atom in value.split(',') if atom.strip()]

def _behaviour_states(results, shingle, display, clients, probe):
  """Fullscreen, maximize and minimize from the client and from IPC."""
  client, window, view = start_probe(shingle, probe, display, 'States')
  clients.append(client)
  if not view:
    results.check('states: the probe maps', False)
    return
  view_id = view['id']

  def view_state(key):
    return (shingle.view_by_id(view_id) or {}).get(key)

  def configured_fullscreen():
    event = last_event(client, 'configure') or {}
    return same_geometry(event, WORKAREA)

  def state_has(name, present=True):
    return wait_for(lambda: (f'_NET_WM_STATE_{name}' in _net_wm_state(
        shingle, window, display)) == present, timeout=5)

  def wm_state_after(count, value):
    return wait_for(lambda: any(
        event.get('event') == 'wm_state' and event['value'] == value
        for event in client.events()[count:]), timeout=5)
  # From the client.
  client.send('state add fullscreen')
  results.check('states: _NET_WM_STATE add fullscreen from the client '
                'fullscreens the view and the X window',
                wait_for(lambda: view_state('fullscreen'), timeout=5)
                and wait_for(configured_fullscreen, timeout=5)
                and state_has('FULLSCREEN'))
  client.send('state remove fullscreen')
  results.check('states: remove fullscreen from the client restores it',
                wait_for(lambda: view_state('fullscreen') is False, timeout=5)
                and state_has('FULLSCREEN', False))
  client.send('state add maximized')
  results.check('states: add maximized (both directions) from the client',
                state_has('MAXIMIZED_VERT') and state_has('MAXIMIZED_HORZ'))
  client.send('state remove maximized')
  results.check('states: remove maximized from the client',
                state_has('MAXIMIZED_VERT', False))
  count = len(client.events())
  client.send('iconify')
  results.check('states: WM_CHANGE_STATE Iconic from the client minimizes, '
                'with WM_STATE Iconic',
                wait_for(lambda: view_state('minimized'), timeout=5)
                and state_has('HIDDEN') and wm_state_after(count, 3))
  count = len(client.events())
  shingle.call('wm-actions/set-minimized', {'view_id': view_id,
                                            'state': False})
  results.check('states: set-minimized false over IPC restores it, with '
                'WM_STATE Normal',
                wait_for(lambda: view_state('minimized') is False, timeout=5)
                and state_has('HIDDEN', False) and wm_state_after(count, 1))
  # From IPC.
  shingle.call('wm-actions/set-fullscreen', {'view_id': view_id,
                                             'state': True})
  results.check('states: set-fullscreen over IPC reaches _NET_WM_STATE and '
                'the X geometry',
                state_has('FULLSCREEN') and wait_for(configured_fullscreen,
                                                     timeout=5))
  shingle.call('wm-actions/set-fullscreen', {'view_id': view_id,
                                             'state': False})
  state_has('FULLSCREEN', False)
  shingle.call('wm-actions/toggle_maximize', {'view_id': view_id})
  results.check('states: toggle_maximize over IPC reaches _NET_WM_STATE',
                state_has('MAXIMIZED_VERT') and state_has('MAXIMIZED_HORZ'))
  shingle.call('wm-actions/toggle_maximize', {'view_id': view_id})
  state_has('MAXIMIZED_VERT', False)
  shingle.call('wm-actions/set-minimized', {'view_id': view_id,
                                            'state': True})
  results.check('states: set-minimized over IPC reaches _NET_WM_STATE_HIDDEN',
                state_has('HIDDEN'))
  client.send('state remove hidden')
  results.check('states: removing hidden from the client restores it',
                wait_for(lambda: view_state('minimized') is False, timeout=5))
  # Initial state, set before the map.
  initial, _, initial_view = start_probe(shingle, probe, display, 'Initial',
                                         '--state', 'fullscreen')
  clients.append(initial)
  results.check('states: an initial _NET_WM_STATE fullscreen applies on map',
                initial_view and initial_view['fullscreen'],
                json.dumps(initial_view))
  initial.send('destroy')
  initial.exited(5)
  # _NET_ACTIVE_WINDOW and _NET_CLOSE_WINDOW.
  xterm = shingle.view('XTerm')
  shingle.focus_view(xterm['id'])
  wait_for(lambda: focused_id(shingle) == xterm['id'])
  client.send('activate')
  results.check('states: _NET_ACTIVE_WINDOW from the client focuses it',
                wait_for(lambda: focused_id(shingle) == view_id, timeout=5))
  client.send('close')
  results.check('states: _NET_CLOSE_WINDOW closes it like close-view '
                '(WM_DELETE_WINDOW)',
                client.wait_event('delete', timeout=5) is not None
                and wait_for(lambda: shingle.view_by_id(view_id) is None))

def _behaviour_stacking(results, shingle, display):
  """X stacking and _NET_CLIENT_LIST_STACKING follow Shingle's."""

  def stacking():
    return parse_window_list(shingle.root_property(
        '_NET_CLIENT_LIST_STACKING', display))
  windows = shingle.x_windows(display)
  by_class = {app_id: window for window, app_id in windows.items()}
  for app_id in ('XTerm', 'FocusProbe', 'Local'):
    view = shingle.view(app_id)
    if not view or app_id not in by_class:
      results.check(f'stacking: {app_id} is listed', False)
      return
    shingle.focus_view(view['id'])
    top = wait_for(lambda: stacking() and stacking()[-1] == by_class[app_id],
                   timeout=5)
    tree = [window for window, _, viewable, _ in shingle.x_tree(display)
            if viewable and window in by_class.values()]
    results.check(f'stacking: focusing {app_id} raises it in '
                  '_NET_CLIENT_LIST_STACKING and in X',
                  top and tree and tree[-1] == by_class[app_id],
                  f'stacking {[hex(w) for w in stacking()]}, '
                  f'X {[hex(w) for w in tree]}')
  results.check('stacking: _NET_CLIENT_LIST_STACKING has the same windows as '
                '_NET_CLIENT_LIST',
                sorted(stacking()) == sorted(windows))

def _behaviour_menus(results, shingle, display):
  """xterm's Ctrl+button menu: override-redirect, at the pointer, no
  keyboard focus, gone on release."""
  view = shingle.view('XTerm')
  shingle.focus_view(view['id'])
  wait_for(lambda: focused_id(shingle) == view['id'])
  # The xterm itself, inside its server-side frame (WG-6).
  geometry = view['base-geometry']
  x, y = centre(geometry)
  xterm = next(w for w, c in shingle.x_windows(display).items()
               if c == 'XTerm')
  views_before = len(shingle.views())
  shingle.call('stipc/move_cursor', {'x': x, 'y': y})
  shingle.press('KEY_LEFTCTRL', True)
  shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})

  def menu():
    return next(((window, g) for window, override, viewable, g
                 in shingle.x_tree(display) if override and viewable), None)
  found = wait_for(menu, timeout=5)
  results.check('menus: Ctrl+button maps an override-redirect menu',
                found is not None)
  if found:
    window, menu_geometry = found
    kind = shingle.window_property(window, '_NET_WM_WINDOW_TYPE', display)
    inside = (menu_geometry['x'] - 2 <= x
              <= menu_geometry['x'] + menu_geometry['width'] + 2
              and menu_geometry['y'] - 2 <= y
              <= menu_geometry['y'] + menu_geometry['height'] + 2)
    results.check('menus: the menu appears at the pointer', inside,
                  f'menu {json.dumps(menu_geometry)}, pointer {x},{y}, '
                  f'type {kind or "none"}')
    tree = [w for w, _, viewable, _ in shingle.x_tree(display) if viewable]
    results.check('menus: the menu is above the xterm in X',
                  window in tree and xterm in tree
                  and tree.index(window) > tree.index(xterm))
    results.check('menus: the menu is not a view',
                  len(shingle.views()) == views_before)
    # Long enough for the menu's first buffer, when it is focused.
    time.sleep(1)
    hints = shingle.window_property(window, 'WM_HINTS', display)
    results.check('menus: the X focus and _NET_ACTIVE_WINDOW stay on the '
                  'xterm', shingle.active_window(display) == xterm,
                  hex(shingle.active_window(display)))
    # Wayfire's rule for override-redirect windows: without a menu or
    # tooltip type, and with input (no WM_HINTS), it takes the keyboard.
    results.check('menus: the untyped menu has the Wayland keyboard, as '
                  'Wayfire gives it', focused_id(shingle) is None,
                  f'focused view {focused_id(shingle)}, menu type '
                  f'{kind or "none"}, WM_HINTS {hints or "none"}')
  # Off the menu first, so that the release selects nothing.
  shingle.call('stipc/move_cursor', {'x': geometry['x'] + 2,
                                     'y': geometry['y'] + 2})
  shingle.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'release'})
  shingle.press('KEY_LEFTCTRL', False)
  results.check('menus: the menu disappears on release',
                wait_for(lambda: menu() is None, timeout=5))
  results.check('menus: the keyboard returns to the xterm',
                wait_for(lambda: focused_id(shingle) == view['id'],
                         timeout=5))

def _behaviour_override_redirect(results, shingle, display, clients, probe):
  """Override-redirect probe windows: a typed menu takes the pointer but
  not the keyboard; an untyped one takes the keyboard while X focus stays."""
  focused = focused_id(shingle)
  active = shingle.active_window(display)
  views = len(shingle.views())
  menu = XClient(shingle, [probe, '--class', 'menu,Menu', '--override-redirect',
                           '--type', 'popup_menu', '--position', '300,200',
                           '--size', '120x80', '--color', '00ff00'],
                 display, 'xw_probe')
  clients.append(menu)
  window = probe_window(menu)
  results.check('override-redirect: a popup menu maps where it asked',
                menu.wait_event('mapped') is not None and wait_for(
                    lambda: same_geometry(
                        shingle.x_geometry(window, display) or {},
                        {'x': 300, 'y': 200, 'width': 120, 'height': 80})))
  time.sleep(1)
  results.check('override-redirect: it is no view and takes no focus',
                len(shingle.views()) == views
                and focused_id(shingle) == focused
                and shingle.active_window(display) == active)
  shingle.call('stipc/move_cursor', {'x': 360, 'y': 240})
  pointer = wait_for(lambda: (probe_query(menu, 'pointer', 'pointer') or {})
                     .get('own') and probe_query(menu, 'pointer', 'pointer'),
                     timeout=5)
  results.check('override-redirect: the pointer reaches it',
                bool(pointer), json.dumps(pointer))
  menu.send('destroy')
  menu.exited(5)
  popup = XClient(shingle, [probe, '--class', 'popup,Popup',
                            '--override-redirect', '--position', '500,300',
                            '--size', '120x80'], display, 'xw_probe')
  clients.append(popup)
  popup.wait_event('mapped')
  results.check('override-redirect: an untyped one takes the keyboard, the '
                'X focus staying where it was',
                wait_for(lambda: focused_id(shingle) is None, timeout=5)
                and shingle.active_window(display) == active,
                f'focused {focused_id(shingle)}, '
                f'active {hex(shingle.active_window(display))}')
  popup.send('destroy')
  popup.exited(5)
  results.check('override-redirect: when it goes, the keyboard returns',
                wait_for(lambda: focused_id(shingle) == focused, timeout=5))

def _behaviour_cursor(results, shingle, client):
  """An X window without a cursor of its own shows the root cursor, which
  is the theme's default arrow."""
  view = shingle.view('FocusProbe')
  if not view:
    results.check('cursor: the probe is listed', False)
    return
  expected = xcursor_image('/usr/share/icons/Adwaita/cursors/default', 24)
  x, y = centre(view['geometry'])
  shingle.call('stipc/move_cursor', {'x': x, 'y': y})
  time.sleep(0.5)
  cursor = probe_query(client, 'cursor', 'cursor') or {}
  seen = (cursor.get('width'), cursor.get('height'), cursor.get('xhot'),
          cursor.get('yhot'))
  results.check('cursor: the root cursor is the theme\'s default arrow',
                expected is not None and seen == expected,
                f'seen {seen}, theme {expected}')
def session_checks(results, binary, work, probe):
  """A session saved with xlogo and xterm open restores both into their
  slots. Shingle runs as uid 1000 here, like the X clients, so that the
  xterm it relaunches can set up its pty."""
  del work, probe
  config = {'xwayland': {'mode': True}}
  display = ':0'
  first = Shingle(binary, None, 'session1', config, prefix=AS_USER)
  first.wait_pid(lambda value: value > 0)
  results.check('session: Xwayland is ready',
                wait_for(lambda: 'Xwayland ready' in first.log_text()))
  clients = []
  saved = None
  try:
    # xterm first, on the left: on restore xlogo maps first, and would
    # take the left tile without the placeholders.
    xterm, xterm_view = start_mapped(
        first, AS_USER + ['xterm', '-e', 'sleep', '600'], display, 'xterm',
        'XTerm')
    clients.append(xterm)
    logo, logo_view = start_mapped(first, ['xlogo'], display, 'xlogo',
                                   'XLogo')
    clients.append(logo)
    results.check('session: xlogo and xterm map', logo_view and xterm_view)
    time.sleep(0.5)
    before = {view['app-id']: view for view in first.views()}
  finally:
    status = first.stop()
    for client in clients:
      client.stop()
  results.check('session: the first run exits with status 0', status == 0,
                first.stop_detail(status))
  path = os.path.join(first.directory, 'state', 'session.json')
  try:
    with open(path) as file:
      saved = json.load(file)
  except (OSError, ValueError) as error:
    results.check('session: the session was saved', False, str(error))
    return first.log_text()
  identities = {window['identity']['app_id']: window
                for window in saved.get('windows', [])}
  xterm_identity = identities.get('XTerm', {}).get('identity', {})
  results.check('session: the saved xterm has its X client\'s command',
                xterm_identity.get('command', [])[:1] == ['xterm']
                and 'XLogo' in identities,
                json.dumps(xterm_identity))
  second = Shingle(binary, None, 'session2', config, session=saved,
                   prefix=AS_USER)
  try:
    placeholders = {view['app-id']: view for view in second.views()}
    results.check('session: the restore starts with placeholders',
                  {'XTerm', 'XLogo'} <= set(placeholders),
                  json.dumps(sorted(placeholders)))
    order = []

    def mapped_order():
      for view in second.views():
        if view['mapped'] and view['app-id'] not in order:
          order.append(view['app-id'])
      return len(order) == 2
    wait_for(mapped_order, timeout=20, interval=0.02)
    restored = wait_for(lambda: all(
        (second.view(app_id) or {}).get('mapped')
        for app_id in ('XTerm', 'XLogo')), timeout=20)
    after = {view['app-id']: view for view in second.views()}
    results.check('session: the restored xterm and xlogo map', restored,
                  f'in the order {order}')
    for app_id in ('XTerm', 'XLogo'):
      view = after.get(app_id, {})
      placeholder = placeholders.get(app_id, {})
      results.check(f'session: the restored {app_id} claims its slot',
                    view.get('id') == placeholder.get('id')
                    and same_geometry(view.get('geometry', {}),
                                      before[app_id]['geometry'])
                    and len([v for v in second.views()
                             if v['app-id'] == app_id]) == 1,
                    f'view {view.get("id")} at '
                    f'{json.dumps(view.get("geometry"))}, placeholder '
                    f'{placeholder.get("id")}, before '
                    f'{json.dumps(before[app_id]["geometry"])}')
    pid = after.get('XTerm', {}).get('pid')
    results.check('session: the restored xterm is a new xterm process',
                  isinstance(pid, int) and pid > 1
                  and command_name(pid) == 'xterm', f'pid {pid}')
  finally:
    status = second.stop()
  results.check('session: the restored run exits with status 0', status == 0,
                second.stop_detail(status))
  return second.log_text()

LAVAPIPE = '/usr/share/vulkan/icd.d/lvp_icd.json'

def parse_ppm(data):
  """(width, height, RGB bytes) of a binary PPM, or None."""
  parts = []
  index = 0
  while len(parts) < 4 and index < len(data):
    while index < len(data) and data[index:index + 1].isspace():
      index += 1
    if data[index:index + 1] == b'#':
      while index < len(data) and data[index:index + 1] != b'\n':
        index += 1
      continue
    start = index
    while index < len(data) and not data[index:index + 1].isspace():
      index += 1
    parts.append(data[start:index])
  if len(parts) < 4 or parts[0] != b'P6':
    return None
  width, height, maximum = int(parts[1]), int(parts[2]), int(parts[3])
  if maximum != 255:
    return None
  pixels = data[index + 1:index + 1 + width * height * 3]
  if len(pixels) != width * height * 3:
    return None
  return width, height, pixels

class Capture:
  """A screenshot as RGB pixels."""

  def __init__(self, width, height, pixels):
    self.width = width
    self.height = height
    self.pixels = pixels

  def pixel(self, x, y):
    """The (r, g, b) at (x, y)."""
    offset = (y * self.width + x) * 3
    return tuple(self.pixels[offset:offset + 3])

  def matching(self, predicate, rect=None):
    """(count, bounding box) of the pixels for which `predicate(r, g, b)`
    holds, inside `rect` or everywhere."""
    x0, y0, x1, y1 = (0, 0, self.width, self.height) if rect is None else (
        max(0, rect['x']), max(0, rect['y']),
        min(self.width, rect['x'] + rect['width']),
        min(self.height, rect['y'] + rect['height']))
    count = 0
    box = None
    for y in range(y0, y1):
      row = self.pixels[(y * self.width + x0) * 3:(y * self.width + x1) * 3]
      matched = [index for index in range(0, len(row), 3)
                 if predicate(row[index], row[index + 1], row[index + 2])]
      if not matched:
        continue
      count += len(matched)
      left, right = x0 + matched[0] // 3, x0 + matched[-1] // 3
      box = (left, y, right, y) if box is None else (
          min(box[0], left), box[1], max(box[2], right), y)
    return count, box

def red(r, g, b):
  """A clearly red pixel."""
  return r > 200 and g < 60 and b < 60

def blue(r, g, b):
  """A clearly blue pixel."""
  return b > 200 and r < 60 and g < 60

def green(r, g, b):
  """A clearly green pixel."""
  return g > 200 and r < 60 and b < 60

def dark(r, g, b):
  """A dark pixel, such as text."""
  return r < 100 and g < 100 and b < 100

def capture(shingle):
  """A wlr-screencopy capture of `shingle`'s output through grim, or
  None."""
  try:
    done = subprocess.run(['grim', '-t', 'ppm', '-'],
                          env=shingle.wayland_environment(),
                          capture_output=True, timeout=20,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  parsed = parse_ppm(done.stdout) if done.returncode == 0 else None
  return Capture(*parsed) if parsed else None

def encode_ppm(shot):
  """`shot` as a binary PPM."""
  return b'P6\n%d %d\n255\n' % (shot.width, shot.height) + bytes(shot.pixels)

_saved_captures = [0]

def save_capture(shot):
  """Saves `shot` in the directory named by CAPTURES_VARIABLE, if set."""
  directory = os.environ.get(CAPTURES_VARIABLE)
  if not directory or shot is None:
    return
  os.makedirs(directory, exist_ok=True)
  path = os.path.join(directory, f'{_saved_captures[0]:03}.ppm')
  _saved_captures[0] += 1
  with open(path, 'wb') as file:
    file.write(encode_ppm(shot))

def settled_capture(shingle, predicate, timeout=10.0):
  """Captures until `predicate(capture)` holds; returns the last capture,
  which `save_capture` keeps when asked to."""
  last = [None]

  def attempt():
    last[0] = capture(shingle)
    return last[0] is not None and predicate(last[0])
  wait_for(attempt, timeout=timeout, interval=0.3)
  save_capture(last[0])
  return last[0]

def nested_checks(results, binary, work, probe):
  """Pixels, the Unmanaged layer, Scale, Expo and the switcher with X
  windows, in nested Shingle on lavapipe inside a headless Shingle."""
  del work
  # Nested's winit window asks for server-side decorations; without a
  # frame (WG-6) it covers the headless output, as these checks assume.
  outer = Shingle(binary, None, 'outer', {
      'xwayland': {'mode': False},
      'decoration': {'ignore_views': 'all'},
  })
  inner_config = {
      'xwayland': {'mode': True},
      'workspaces': {'width': 2, 'height': 2},
  }
  inner = Shingle(binary, None, 'inner', inner_config,
                  backend=('--backend', 'nested'),
                  extra_environment={
                      'WAYLAND_DISPLAY': outer.wayland_display(),
                      'VK_ICD_FILENAMES': LAVAPIPE,
                  })
  inner.wait_pid(lambda value: value > 0, timeout=30)
  ready = wait_for(lambda: 'Xwayland ready' in inner.log_text(), timeout=30)
  results.check('nested: nested Shingle runs X inside headless Shingle',
                ready and inner.process.poll() is None,
                inner.log_text()[-300:])
  display = ':0'
  clients = []
  try:
    if ready:
      _nested_pixels(results, outer, inner, display, clients, probe)
  finally:
    for client in clients:
      client.stop()
  results.check('nested: SIGTERM exits nested with status 0',
                inner.stop() == 0)
  results.check('nested: SIGTERM exits headless with status 0',
                outer.stop() == 0)
  return inner.log_text()

def _nested_pixels(results, outer, inner, display, clients, probe):
  """The checks inside nested Shingle. Captures are of the headless
  Shingle's output, which shows the frame nested drew (nested's own
  screencopy has the surfaces, not the overviews), and pointer and key
  input for the overviews and the switcher comes from the headless Shingle
  as real wl_pointer and wl_keyboard events on nested's window, which
  covers its output at the same scale."""
  red_client, red_window, red_view = start_probe(
      inner, probe, display, 'Red', '--color', 'ff0000')
  blue_client, blue_window, blue_view = start_probe(
      inner, probe, display, 'Blue', '--color', '0000ff')
  clients += [red_client, blue_client]
  if not (red_view and blue_view):
    results.check('nested: the red and blue probes map', False)
    return
  red_view, blue_view = inner.view('Red'), inner.view('Blue')

  def tiles_drawn(shot):
    return (red(*shot.pixel(*centre(red_view['geometry'])))
            and blue(*shot.pixel(*centre(blue_view['geometry']))))
  shot = settled_capture(outer, tiles_drawn)
  results.check('nested: grim shows each X window at its view geometry',
                shot is not None and tiles_drawn(shot),
                f'red {json.dumps(red_view["geometry"])}, '
                f'blue {json.dumps(blue_view["geometry"])}')
  # The Unmanaged layer: an override-redirect window over the red tile.
  rx, ry = centre(red_view['geometry'])
  menu = XClient(inner, [probe, '--class', 'menu,Menu', '--override-redirect',
                         '--type', 'popup_menu', '--position',
                         f'{rx - 50},{ry - 40}', '--size', '100x80',
                         '--color', '00ff00'], display, 'xw_probe')
  clients.append(menu)
  menu.wait_event('mapped')
  rect = {'x': rx - 50, 'y': ry - 40, 'width': 100, 'height': 80}
  shot = settled_capture(
      inner, lambda shot: shot.matching(green, rect)[0] >= 100 * 80 * 0.9)
  count = shot.matching(green, rect)[0] if shot else 0
  results.check('nested: an override-redirect window is drawn above the '
                'window under it, where X put it',
                count >= 100 * 80 * 0.9, f'{count} green pixels of 8000')
  menu.send('destroy')
  menu.exited(5)
  settled_capture(outer, lambda shot: shot.matching(green)[0] == 0)
  # Scale.
  watcher = Ipc(inner.ipc_path)
  watcher.call('window-rules/events/watch',
               {'events': ['plugin-activation-state-changed']})
  inner.focus_view(blue_view['id'])
  wait_for(lambda: inner.active_window(display) == blue_window)
  inner.call('scale/toggle')
  event = _receive(watcher)
  results.check('nested: scale/toggle activates Scale',
                event.get('plugin') == 'scale' and event.get('state') is True,
                json.dumps(event))

  def smaller(box, view):
    return (box is not None
            and box[2] - box[0] + 1 < view['geometry']['width'] - 20
            and box[3] - box[1] + 1 < view['geometry']['height'] - 20)

  def thumbnails(shot):
    _, red_box = shot.matching(red)
    _, blue_box = shot.matching(blue)
    return smaller(red_box, red_view) and smaller(blue_box, blue_view)
  shot = settled_capture(outer, thumbnails)
  results.check('nested: Scale shows both X windows as thumbnails',
                shot is not None and thumbnails(shot),
                f'red {shot.matching(red)[1] if shot else None}, '
                f'blue {shot.matching(blue)[1] if shot else None}')
  if shot and thumbnails(shot):
    _, box = shot.matching(red)
    x, y = (box[0] + box[2]) // 2, (box[1] + box[3]) // 2
    outer.click(x, y)
    event = _receive(watcher)
    results.check('nested: clicking the red thumbnail ends Scale and '
                  'focuses that X window',
                  event.get('state') is False
                  and wait_for(lambda: focused_id(inner) == red_view['id'])
                  and wait_for(lambda: inner.active_window(display)
                               == red_window),
                  f'{json.dumps(event)}, focused {focused_id(inner)}')
  else:
    inner.call('scale/toggle')
    _receive(watcher)
  settled_capture(outer, tiles_drawn)
  # Expo, with blue on the workspace to the right, since Expo centres
  # every window in its workspace's cell.
  inner.call('vswitch/send-view', {'view-id': blue_view['id'], 'x': 1,
                                   'y': 0})
  inner.call('expo/toggle')
  event = _receive(watcher)
  results.check('nested: expo/toggle activates Expo',
                event.get('plugin') == 'expo' and event.get('state') is True,
                json.dumps(event))

  def wall(shot):
    red_count, red_box = shot.matching(red)
    blue_count, blue_box = shot.matching(blue)
    # Each fills at most its cell, a quarter of the output.
    cell = WORKAREA['width'] * WORKAREA['height'] // 4
    return (0 < red_count <= cell and 0 < blue_count <= cell
            and red_box[2] < 640 and blue_box[0] >= 640
            and red_box[3] < 360 and blue_box[3] < 360)
  shot = settled_capture(outer, wall)
  results.check('nested: Expo shows both X windows on their workspaces',
                shot is not None and wall(shot),
                f'red {shot.matching(red)[1] if shot else None}, '
                f'blue {shot.matching(blue)[1] if shot else None}')
  inner.call('expo/toggle')
  _receive(watcher)
  watcher.close()
  inner.call('vswitch/send-view', {'view-id': blue_view['id'], 'x': 0,
                                   'y': 0})
  red_view, blue_view = inner.view('Red'), inner.view('Blue')
  settled_capture(outer, tiles_drawn)
  # The fast switcher: Alt+Escape moves the focus to the other X window.
  inner.focus_view(blue_view['id'])
  wait_for(lambda: inner.active_window(display) == blue_window)
  for key, pressed in (('KEY_LEFTALT', True), ('KEY_ESC', True),
                       ('KEY_ESC', False), ('KEY_LEFTALT', False)):
    outer.press(key, pressed)
    time.sleep(0.2)
  results.check('nested: the fast switcher (Alt+Escape) moves the focus '
                'between X windows, and the X focus follows',
                wait_for(lambda: focused_id(inner) == red_view['id'])
                and wait_for(lambda: inner.active_window(display)
                             == red_window),
                f'focused {focused_id(inner)}, '
                f'active {hex(inner.active_window(display))}')
  _nested_xterm_menu(results, outer, inner, display, clients)

def _receive(ipc, timeout=10.0):
  """The next IPC message, or an error description."""
  try:
    return ipc.receive(timeout)
  except (TimeoutError, ConnectionError) as error:
    return {'error': str(error)}

def _nested_xterm_menu(results, outer, inner, display, clients):
  """xterm's Ctrl+button menu is drawn above the xterm."""
  client, view = start_mapped(inner, AS_USER + ['xterm', '-e', 'sleep', '600'],
                              display, 'xterm', 'XTerm')
  clients.append(client)
  if not view:
    results.check('nested: xterm maps', False)
    return
  inner.focus_view(view['id'])
  time.sleep(1)
  geometry = inner.view('XTerm')['geometry']
  x, y = centre(geometry)
  inner.call('stipc/move_cursor', {'x': x, 'y': y})
  # The xterm drawn and its open animation over: mostly white.
  area = geometry['width'] * geometry['height']
  before = settled_capture(
      outer, lambda shot: shot.matching(dark, geometry)[0] < area * 0.05)
  inner.press('KEY_LEFTCTRL', True)
  inner.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'press'})
  found = wait_for(lambda: next(
      (g for _, override, viewable, g in inner.x_tree(display)
       if override and viewable), None), timeout=5)
  if found:
    shot = settled_capture(
        inner, lambda shot: shot.matching(dark, found)[0] > 500)
    during = shot.matching(dark, found)[0] if shot else 0
    earlier = before.matching(dark, found)[0] if before else -1
    results.check('nested: xterm\'s menu is drawn above the xterm',
                  during > 500 and during > 4 * max(earlier, 1),
                  f'menu {json.dumps(found)}: {earlier} dark pixels before, '
                  f'{during} with the menu')
  else:
    results.check('nested: xterm\'s Ctrl+button menu maps', False)
  inner.call('stipc/move_cursor', {'x': geometry['x'] + 2,
                                   'y': geometry['y'] + 2})
  inner.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'release'})
  inner.press('KEY_LEFTCTRL', False)

SMALL_PAYLOAD = 10
LARGE_PAYLOAD = 1 << 20
# Above any chunk size xclip picks (a quarter of the maximum request
# size), so that xclip itself must offer it through INCR.
HUGE_PAYLOAD = 8 << 20

def payload(size, seed=0):
  """`size` bytes of printable ASCII lines, different for each `seed`."""
  alphabet = b'abcdefghijklmnopqrstuvwxyz0123456789'
  line = bytes(alphabet[(index * 7 + seed) % len(alphabet)]
               for index in range(63)) + b'\n'
  count = size // len(line) + 1
  data = (line * count)[:size]
  # The seed also leads, so that short payloads differ too.
  return (str(seed).encode() + data)[:size]

def payload_label(size):
  """'10 bytes', '1 MiB' or '8 MiB'."""
  if size >= 1 << 20 and size % (1 << 20) == 0:
    return f'{size >> 20} MiB'
  return f'{size} bytes'

def selection_options(selection):
  """xclip's and wl-copy/wl-paste's options for 'clipboard' or 'primary'."""
  if selection == 'clipboard':
    return ['-selection', 'clipboard'], []
  if selection == 'primary':
    return ['-selection', 'primary'], ['--primary']
  raise ValueError(selection)

def compare(actual, expected):
  """A short description of how `actual` bytes differ from `expected`."""
  if actual == expected:
    return f'{len(actual)} bytes'
  if actual is None:
    return 'nothing'
  same = next((index for index, (a, b) in enumerate(zip(actual, expected))
               if a != b), min(len(actual), len(expected)))
  return (f'{len(actual)} bytes, expected {len(expected)}, '
          f'first difference at {same}')

class Helper:
  """A helper process (xclip, wl-copy, a paste pipeline) fed `data` on
  stdin from a thread, and stopped through `signal_checked`. Its stderr
  goes to a temporary file, read by `errors`."""

  def __init__(self, arguments, environment, program, data=None,
               stdout=subprocess.DEVNULL):
    self.program = program
    self.stderr = tempfile.TemporaryFile()
    self.process = subprocess.Popen(
        arguments, env=environment,
        stdin=subprocess.DEVNULL if data is None else subprocess.PIPE,
        stdout=stdout, stderr=self.stderr)
    self.writer = None
    if data is not None:
      self.writer = threading.Thread(target=self._write, args=(data,),
                                     daemon=True)
      self.writer.start()

  def _write(self, data):
    try:
      self.process.stdin.write(data)
      self.process.stdin.close()
    except (BrokenPipeError, ValueError):
      pass

  @property
  def pid(self):
    """The helper's PID."""
    return self.process.pid

  def errors(self):
    """What the helper wrote to stderr so far."""
    self.stderr.seek(0)
    return self.stderr.read().decode(errors='replace').strip()

  def wait(self, timeout):
    """Waits for the helper to exit; returns its status or None."""
    try:
      return self.process.wait(timeout)
    except subprocess.TimeoutExpired:
      return None

  def stop(self):
    """SIGTERM, then SIGKILL, each through `signal_checked`."""
    if self.process.poll() is None:
      signal_checked(self.pid, signal.SIGTERM, self.program)
      if self.wait(5) is None:
        signal_checked(self.pid, signal.SIGKILL, self.program)
        self.wait(5)

def copy_x(shingle, display, selection, data):
  """`xclip -i` owning `selection` with `data`, in the foreground."""
  options, _ = selection_options(selection)
  return Helper(['xclip', *options, '-i', '-quiet'],
                shingle.environment(display), 'xclip', data)

def copy_wayland(shingle, selection, data):
  """`wl-copy` offering `data` as `selection`, in the foreground."""
  _, options = selection_options(selection)
  return Helper(['wl-copy', '--foreground', *options],
                shingle.wayland_environment(), 'wl-copy', data)

def paste_x(shingle, display, selection, timeout=10):
  """`xclip -o`'s output for `selection`, or None when it failed."""
  options, _ = selection_options(selection)
  try:
    done = subprocess.run(['xclip', *options, '-o'],
                          env=shingle.environment(display),
                          capture_output=True, timeout=timeout,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  return done.stdout if done.returncode == 0 else None

def paste_wayland(shingle, selection, timeout=10, debug=False):
  """`wl-paste -n`'s output for `selection` (None when it failed) and its
  stderr, which holds the protocol trace with `debug`."""
  _, options = selection_options(selection)
  environment = shingle.wayland_environment()
  if debug:
    environment['WAYLAND_DEBUG'] = '1'
  try:
    done = subprocess.run(['wl-paste', '--no-newline', *options],
                          env=environment, capture_output=True,
                          timeout=timeout, stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None, ''
  output = done.stdout if done.returncode == 0 else None
  return output, done.stderr.decode(errors='replace')

def wait_paste(paste, expected, timeout=10.0):
  """Calls `paste()` until it returns `expected`; returns the last
  output."""
  deadline = time.monotonic() + timeout
  while True:
    output = paste()
    if output == expected or time.monotonic() > deadline:
      return output
    time.sleep(0.1)

def ipc_latency(shingle):
  """How long one IPC call takes, in seconds."""
  started = time.monotonic()
  shingle.call('stipc/get_xwayland_pid')
  return time.monotonic() - started

def selection_checks(results, binary, work, wl_probe):
  """XW-5: the clipboard and primary selection between xclip and
  wl-copy/wl-paste, in both directions, small and large (INCR); that
  wlr-data-control sees X selections; that slow writers and readers stall
  only their own transfer; and `disable_primary_selection`."""
  del work
  shingle = Shingle(binary, None, 'selection', {'xwayland': {'mode': True}})
  shingle.wait_pid(lambda value: value > 0)
  results.check('selection: Xwayland is ready',
                wait_for(lambda: 'Xwayland ready' in shingle.log_text()))
  display = ':0'
  helpers = []
  try:
    seed = 0
    for selection in ('clipboard', 'primary'):
      for size in (SMALL_PAYLOAD, LARGE_PAYLOAD):
        seed += 1
        _selection_x_to_wayland(results, shingle, display, selection,
                                payload(size, seed), helpers)
        seed += 1
        _selection_wayland_to_x(results, shingle, display, selection,
                                payload(size, seed), helpers)
    _selection_x_to_wayland(results, shingle, display, 'clipboard',
                            payload(HUGE_PAYLOAD, 9), helpers)
    _selection_data_control(results, shingle, display, helpers)
    _selection_slow_reader(results, shingle, display, helpers)
    _selection_slow_writer(results, shingle, display, helpers, wl_probe)
  finally:
    for helper in helpers:
      helper.stop()
  results.check('selection: SIGTERM exits with status 0', shingle.stop() == 0)
  log = shingle.log_text()
  return log + _selection_no_primary(results, binary)

def _selection_x_to_wayland(results, shingle, display, selection, data,
                            helpers):
  """xclip owns `selection`; wl-paste reads it."""
  owner = copy_x(shingle, display, selection, data)
  helpers.append(owner)
  pasted = wait_paste(lambda: paste_wayland(shingle, selection)[0], data)
  results.check(f'selection: {selection}, {payload_label(len(data))}, '
                'from xclip -i to wl-paste', pasted == data,
                compare(pasted, data))
  owner.stop()

def _selection_wayland_to_x(results, shingle, display, selection, data,
                            helpers):
  """wl-copy offers `selection`; xclip -o reads it."""
  owner = copy_wayland(shingle, selection, data)
  helpers.append(owner)
  pasted = wait_paste(lambda: paste_x(shingle, display, selection), data)
  detail = compare(pasted, data)
  if pasted != data:
    detail += f'; wl-copy: {owner.errors()[-300:]!r}'
  results.check(f'selection: {selection}, {payload_label(len(data))}, '
                'from wl-copy to xclip -o', pasted == data, detail)
  owner.stop()

def _selection_data_control(results, shingle, display, helpers):
  """wl-paste reads an X selection through wlr-data-control, which needs
  no keyboard focus, as its protocol trace shows."""
  data = payload(SMALL_PAYLOAD, 40)
  owner = copy_x(shingle, display, 'clipboard', data)
  helpers.append(owner)
  wait_paste(lambda: paste_wayland(shingle, 'clipboard')[0], data)
  pasted, trace = paste_wayland(shingle, 'clipboard', debug=True)
  offered = 'zwlr_data_control_offer_v1' in trace and (
      'offer("text/plain;charset=utf-8")' in trace)
  results.check('selection: wlr-data-control offers the X selection',
                offered and pasted == data, compare(pasted, data))
  # The registry lists wl_data_device_manager; no object of it is used.
  results.check('selection: wl-paste used no wl_data_device',
                not re.search(r'wl_data_device(_manager)?@\d', trace))
  owner.stop()

def _selection_slow_reader(results, shingle, display, helpers):
  """A Wayland reader that stops reading 1 MiB from X stalls only its
  own transfer: IPC answers and a primary transfer completes meanwhile."""
  data = payload(LARGE_PAYLOAD, 50)
  owner = copy_x(shingle, display, 'clipboard', data)
  helpers.append(owner)
  wait_paste(lambda: paste_wayland(shingle, 'clipboard')[0], data)
  out = os.path.join(shingle.directory, 'slow-reader')
  started = time.monotonic()
  reader = Helper(['sh', '-c', f'wl-paste --no-newline | '
                   f'(sleep 3; cat > {out})'],
                  shingle.wayland_environment(), 'sh')
  helpers.append(reader)
  time.sleep(0.5)
  latency = ipc_latency(shingle)
  results.check('selection: IPC answers while a slow reader stalls',
                latency < 0.5, f'{latency:.3f} s')
  primary = payload(SMALL_PAYLOAD, 51)
  primary_owner = copy_x(shingle, display, 'primary', primary)
  helpers.append(primary_owner)
  pasted = wait_paste(lambda: paste_wayland(shingle, 'primary')[0], primary,
                      timeout=2.0)
  stalled = reader.process.poll() is None
  results.check('selection: a primary transfer completes while the '
                'clipboard reader stalls', pasted == primary and stalled,
                f'{compare(pasted, primary)}, reader still waiting: '
                f'{stalled}')
  status = reader.wait(20)
  elapsed = time.monotonic() - started
  received = open(out, 'rb').read() if os.path.exists(out) else None
  results.check('selection: the slow reader gets all 1 MiB in the end',
                status == 0 and received == data and elapsed >= 3,
                f'{compare(received, data)} after {elapsed:.1f} s')
  primary_owner.stop()
  owner.stop()

def _selection_slow_writer(results, shingle, display, helpers, wl_probe):
  """A Wayland source that waits 3 s before writing stalls only the X
  reader waiting for it."""
  text = b'slow writer text'
  probe = WaylandClient(shingle, wl_probe, 'slow-copy', '--copy',
                        text.decode(), '--delay-ms', '3000')
  try:
    results.check('selection: the slow writer set the clipboard',
                  probe.wait_event('copied') is not None)
    time.sleep(0.3)
    out_path = os.path.join(shingle.directory, 'slow-writer')
    started = time.monotonic()
    with open(out_path, 'wb') as out:
      options, _ = selection_options('clipboard')
      reader = Helper(['xclip', *options, '-o'],
                      shingle.environment(display), 'xclip', stdout=out)
    helpers.append(reader)
    sent = probe.wait_event('send', timeout=2.0)
    results.check('selection: the X request reaches the Wayland source',
                  sent is not None and
                  sent.get('mime') == 'text/plain;charset=utf-8',
                  json.dumps(sent))
    latency = ipc_latency(shingle)
    results.check('selection: IPC answers while a slow writer stalls',
                  latency < 0.5, f'{latency:.3f} s')
    primary = payload(SMALL_PAYLOAD, 60)
    primary_owner = copy_wayland(shingle, 'primary', primary)
    helpers.append(primary_owner)
    pasted = wait_paste(lambda: paste_x(shingle, display, 'primary'),
                        primary, timeout=2.0)
    stalled = reader.process.poll() is None
    results.check('selection: a primary transfer completes while the '
                  'clipboard writer stalls', pasted == primary and stalled,
                  f'{compare(pasted, primary)}, reader still waiting: '
                  f'{stalled}')
    status = reader.wait(10)
    elapsed = time.monotonic() - started
    received = open(out_path, 'rb').read()
    results.check('selection: the X reader gets the slow text in the end',
                  status == 0 and received == text and elapsed >= 2.5,
                  f'{compare(received, text)} after {elapsed:.1f} s')
    primary_owner.stop()
  finally:
    probe.stop()

def _selection_no_primary(results, binary):
  """With `disable_primary_selection`, PRIMARY is not bridged in either
  direction, and the clipboard still is."""
  config = {
      'xwayland': {'mode': True},
      'behavior': {'disable_primary_selection': True},
  }
  shingle = Shingle(binary, None, 'selection-noprimary', config)
  shingle.wait_pid(lambda value: value > 0)
  results.check('no-primary: Xwayland is ready',
                wait_for(lambda: 'Xwayland ready' in shingle.log_text()))
  display = ':0'
  helpers = []
  try:
    data = payload(SMALL_PAYLOAD, 70)
    owner = copy_x(shingle, display, 'clipboard', data)
    helpers.append(owner)
    pasted = wait_paste(lambda: paste_wayland(shingle, 'clipboard')[0], data)
    results.check('no-primary: the clipboard is still bridged',
                  pasted == data, compare(pasted, data))
    owner.stop()
    data = payload(SMALL_PAYLOAD, 71)
    owner = copy_x(shingle, display, 'primary', data)
    helpers.append(owner)
    on_x = wait_paste(lambda: paste_x(shingle, display, 'primary'), data)
    time.sleep(1.0)
    pasted, _ = paste_wayland(shingle, 'primary')
    results.check('no-primary: an X primary selection stays in X',
                  on_x == data and pasted != data,
                  f'X: {compare(on_x, data)}; Wayland: {pasted!r}')
    owner.stop()
    data = payload(SMALL_PAYLOAD, 72)
    owner = copy_wayland(shingle, 'primary', data)
    helpers.append(owner)
    on_wayland = wait_paste(lambda: paste_wayland(shingle, 'primary')[0],
                            data)
    time.sleep(1.0)
    pasted = paste_x(shingle, display, 'primary')
    results.check('no-primary: a Wayland primary selection stays in Wayland',
                  on_wayland == data and pasted != data,
                  f'Wayland: {compare(on_wayland, data)}; X: {pasted!r}')
    owner.stop()
  finally:
    for helper in helpers:
      helper.stop()
  results.check('no-primary: SIGTERM exits with status 0', shingle.stop() == 0)
  return shingle.log_text()

GROUPS = ('lazy', 'disabled', 'eager', 'running', 'idle', 'windows', 'probe',
          'behaviour', 'session', 'nested', 'selection', 'outputs',
          'decoration')

def decoration_checks(results, binary, work, probe):
  """WG-6: xterm, which sets no _MOTIF_WM_HINTS, gets a server-side frame
  and an X window whose hints turn decorations off does not, in tiles
  and floating."""
  del work
  shingle = Shingle(binary, None, 'decoration', {'xwayland': {'mode': True}})
  shingle.wait_pid(lambda value: value > 0)
  ready = wait_for(lambda: 'Xwayland ready' in shingle.log_text())
  results.check('decoration: Xwayland is ready', ready)
  display = ':0'
  clients = []
  try:
    client, xterm = start_mapped(
        shingle, AS_USER + ['xterm', '-e', 'sleep', '600'], display, 'xterm',
        'XTerm')
    clients.append(client)
    client, _, frameless = start_probe(shingle, probe, display, 'Frameless',
                                       '--no-decorations')
    clients.append(client)
    results.check('decoration: xterm and a probe without decorations map',
                  bool(xterm) and bool(frameless), json.dumps(shingle.views()))
    if not xterm or not frameless:
      return shingle.log_text()
    windows = {app_id: window
               for window, app_id in shingle.x_windows(display).items()}

    def framed(app_id):
      view = shingle.view(app_id)
      x_geometry = shingle.x_geometry(windows[app_id], display) or {}
      return (same_geometry(view['base-geometry'],
                            inside_frame(view['geometry']))
              and same_geometry(x_geometry, view['base-geometry']))

    def unframed(app_id):
      view = shingle.view(app_id)
      x_geometry = shingle.x_geometry(windows[app_id], display) or {}
      return (same_geometry(view['base-geometry'], view['geometry'])
              and same_geometry(x_geometry, view['geometry']))

    def detail(app_id):
      view = shingle.view(app_id)
      return (f'{app_id} geometry {json.dumps(view["geometry"])} base '
              f'{json.dumps(view["base-geometry"])} x '
              f'{json.dumps(shingle.x_geometry(windows[app_id], display))}')
    results.check('decoration: xterm\'s tile is its frame, and its X window '
                  'the inside of the frame',
                  wait_for(lambda: framed('XTerm'), timeout=5),
                  detail('XTerm'))
    results.check('decoration: the probe without decorations fills its tile',
                  wait_for(lambda: unframed('Frameless'), timeout=5),
                  detail('Frameless'))
    target = {'x': 200, 'y': 150, 'width': 500, 'height': 400}
    for app_id in ('XTerm', 'Frameless'):
      shingle.call('window-rules/configure-view',
                   {'id': shingle.view(app_id)['id'], 'geometry': target})
    results.check('decoration: floating, xterm keeps its frame',
                  wait_for(lambda: framed('XTerm') and same_geometry(
                      shingle.view('XTerm')['geometry'], target), timeout=5),
                  detail('XTerm'))
    results.check('decoration: floating, the probe still has none',
                  wait_for(lambda: unframed('Frameless') and same_geometry(
                      shingle.view('Frameless')['geometry'], target),
                           timeout=5), detail('Frameless'))
  finally:
    for client in clients:
      client.stop()
  results.check('decoration: SIGTERM exits with status 0',
                shingle.stop() == 0)
  return shingle.log_text()

def outputs_checks(results, binary, work):
  """MO-3: Xwayland sees both outputs of headless Shingle, at their
  positions, under a root that is their union, also after one moves."""
  config = {
      'xwayland': {'mode': True},
      'outputs': [{'name': 'HEADLESS-2', 'mode': '1024x768@60'}],
  }
  shingle = Shingle(binary, work, 'outputs', config,
                    backend=('--headless', '--outputs', '2'))
  pid = shingle.wait_pid(lambda value: value > 0)
  results.check('outputs: Xwayland runs', pid > 0, f'pid {pid}')

  def monitors():
    status, output = shingle.x_client(['xrandr', '--listmonitors'], ':0')
    if status != 0:
      return None
    return sorted(geometry for _, geometry in parse_listmonitors(output))

  def root():
    status, output = shingle.x_client(['xdpyinfo'], ':0')
    match = re.search(r'dimensions:\s+(\d+)x(\d+) pixels', output)
    return (int(match.group(1)), int(match.group(2))) if match else None
  side_by_side = [(0, 0, 1280, 720), (1280, 0, 1024, 768)]
  seen = wait_for(lambda: monitors() == side_by_side) and monitors()
  results.check('outputs: xrandr --listmonitors lists both at their '
                'positions', seen == side_by_side, str(monitors()))
  results.check('outputs: xdpyinfo reports their union as the root',
                root() == (2304, 768), str(root()))
  done = subprocess.run(
      ['wlr-randr', '--output', 'HEADLESS-2', '--pos', '0,720'],
      env=shingle.wayland_environment(), capture_output=True, text=True,
      timeout=10)
  # HEADLESS-2 is fixed now, so the automatic HEADLESS-1 follows its
  # right edge at its y, as wlroots places it, and the root grows to the
  # bottom-right corner of both (Xwayland's output_get_new_size).
  moved_layout = [(0, 720, 1024, 768), (1024, 720, 1280, 720)]
  moved = wait_for(lambda: monitors() == moved_layout)
  results.check('outputs: a moved output moves in RandR and the root',
                done.returncode == 0 and moved and root() == (2304, 1488),
                f'{done.stdout}{done.stderr} {monitors()} {root()}')
  status = shingle.stop()
  results.check('outputs: SIGTERM exits with status 0', status == 0,
                shingle.stop_detail(status))
  return shingle.log_text()

def inner(binary, probe, wl_probe, work, groups):
  """Runs the check `groups` inside the namespace; returns the status."""
  private_tmp = os.path.join(work, 'tmp')
  os.makedirs(private_tmp)
  subprocess.run(['mount', '--bind', private_tmp, '/tmp'], check=True)
  for device in ('/dev/dri', '/dev/input'):
    if os.path.isdir(device):
      subprocess.run(['mount', '-t', 'tmpfs', 'none', device], check=True)
  os.mkdir('/tmp/.X11-unix', 0o1777)
  os.chmod('/tmp/.X11-unix', 0o1777)
  runtime = hide_host_runtime()
  results = Results()
  results.check('the host runtime directory is hidden',
                runtime is None or os.listdir(runtime) == [])
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
      'behaviour': behaviour_checks,
      'session': session_checks,
      'nested': nested_checks,
      'selection': selection_checks,
      'outputs': outputs_checks,
      'decoration': decoration_checks,
  }
  for group in groups:
    if group == 'behaviour':
      log = checks[group](results, binary, work, probe, wl_probe)
    elif group == 'selection':
      log = checks[group](results, binary, work, wl_probe)
    elif group in ('windows', 'probe', 'session', 'nested', 'decoration'):
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
  parser.add_argument('--wl-probe', default='target/debug/examples/wl_probe')
  parser.add_argument('--keep', action='store_true')
  parser.add_argument('--only', action='append', choices=GROUPS,
                      help='run only this group of checks (repeatable)')
  parser.add_argument(INNER_FLAG, metavar='WORK')
  options = parser.parse_args()
  binary = os.path.abspath(options.binary)
  probe = os.path.abspath(options.probe)
  wl_probe = os.path.abspath(options.wl_probe)
  groups = options.only or list(GROUPS)
  if options.inner:
    return inner(binary, probe, wl_probe, options.inner, groups)
  work = os.path.abspath(os.path.join(
      'target', f'xwayland_test-{os.getpid()}'))
  os.makedirs(work)
  try:
    # The PID namespace confines every signal the checks send to processes
    # started inside it, so a wrong PID can never reach the host session.
    command = ['unshare', '--user', '--map-root-user', '--mount', '--net',
               '--pid', '--fork', '--mount-proc', '--',
               sys.executable, os.path.abspath(__file__),
               '--binary', binary, '--probe', probe, '--wl-probe', wl_probe,
               INNER_FLAG, work]
    for group in groups:
      command += ['--only', group]
    return subprocess.run(
        command, env=host_uid_environment(os.environ)).returncode
  finally:
    if not options.keep:
      shutil.rmtree(work, ignore_errors=True)

if __name__ == '__main__':
  sys.exit(main())
