"""A fake test machine behind a fake SSH runner, for the unit tests.

`FakeMachine` answers the argv lists bringup.py hands its runner: `ssh -G`
with a configured host name, `ssh HOST -- sh -c SCRIPT` by interpreting
SCRIPT against an in-memory home directory, and `scp` by copying between
that and the real local filesystem. It models a native Shingle running on
tty1 closely enough for the step drivers: a clock that the injected sleep
advances, input devices and held keys, wev's log, VT switches, DPMS, the
probe's output and the checked signal. Nothing here starts a process
other than the local commands a test passes through explicitly.
"""

import json
import os
import re
import shlex

import remote

PID = 4242
START = 777
RUN = '.local/state/shingle/bringup/current'
LOG = '.local/state/shingle/native.log'
WRAPPED = re.compile(
    r'^XDG_RUNTIME_DIR=/run/user/\$\(id -u\) WAYLAND_DISPLAY=\S+ (.*)$', re.S)

def stat_line(pid, start, comm='shingle'):
  """A /proc/PID/stat line with start time `start`."""
  fields = ['S'] + ['0'] * 18 + [str(start)] + ['0'] * 10
  return f'{pid} ({comm}) ' + ' '.join(fields)

def png(width, height):
  """The first 24 bytes of a PNG of that size, as text for the fake."""
  return (b'\x89PNG\r\n\x1a\n' + b'\x00\x00\x00\rIHDR' +
          width.to_bytes(4, 'big') + height.to_bytes(4, 'big')).decode(
              'latin-1')

class FakeMachine:
  """The fake test machine; `runner` is the injected process runner."""

  def __init__(self, hostname='laptop', machine_id='f' * 32, marker=True,
               resolves_to='laptop.lan', running=True, local=None):
    self.hostname = hostname
    self.machine_id = machine_id
    self.marker = marker
    self.resolves_to = resolves_to
    self.local = local or {}
    self.files = {}
    self.scripts = []
    self.local_calls = []
    self.puts = []
    self.handlers = []
    self.now_ms = 1_000_000.0
    self.vt = 1
    self.dpms = 'on'
    self.devices = [
        {'id': 1, 'name': 'AT Translated Set 2 keyboard', 'type': 'keyboard',
         'vendor': 1, 'product': 1, 'enabled': True},
        {'id': 2, 'name': 'SynPS/2 Synaptics TouchPad', 'type': 'pointer',
         'vendor': 2, 'product': 7, 'enabled': True},
    ]
    self.next_id = 3
    self.plugged = []
    self.held = []
    self.probing = False
    self.cursor_plane = True
    self.signalled = []
    self.ssh_refusal_ok = True
    if running:
      self.start_shingle()

  # The runner.

  def runner(self, argv, timeout=None, cwd=None, env=None):
    """Answers one argv the code under test would run."""
    del timeout, cwd, env
    if argv[:2] == ['ssh', '-G']:
      return remote.Result(0, f'user karl\nhostname {self.resolves_to}\n')
    if argv[0] == 'ssh':
      command = argv[argv.index('--') + 1]
      words = shlex.split(command)
      assert words[:2] == ['sh', '-c'], command
      return self.sh(words[2])
    if argv[0] == 'scp':
      source, destination = argv[-2], argv[-1]
      if ':' in source and not os.path.exists(source):
        return self.scp_get(source.split(':', 1)[1], destination)
      return self.scp_put(source, destination.split(':', 1)[1])
    self.local_calls.append(argv)
    for pattern, answer in self.local.items():
      if re.search(pattern, ' '.join(argv)):
        return answer(argv) if callable(answer) else answer
    return remote.Result(0, '')

  def scp_get(self, path, local):
    """Copies the fake file `path` to the real path `local`."""
    if path not in self.files:
      return remote.Result(1, '', f'scp: {path}: No such file')
    with open(local, 'w', encoding='latin-1') as file:
      file.write(self.files[path])
    return remote.Result(0)

  def scp_put(self, local, path):
    """Records an upload."""
    self.puts.append((local, path))
    self.files[path] = f'uploaded from {local}'
    return remote.Result(0)

  # The fake system.

  def start_shingle(self):
    """Puts a running native Shingle on tty1, as start_tty1.py leaves it."""
    self.files.update({
        f'{RUN}/pid': f'{PID} {START}\n',
        f'/proc/{PID}/comm': 'shingle\n',
        f'/proc/{PID}/stat': stat_line(PID, START) + '\n',
        f'{RUN}/stdout': 'WAYLAND_DISPLAY=wayland-1\n',
        f'{RUN}/stderr': 'shingle: using the native backend (none of '
                         'WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is '
                         'set)\n',
        f'{RUN}/run.json': json.dumps({'step': None, 'argv': ['shingle']}),
        f'{RUN}/wev.log': '',
        f'{RUN}/probe.out': '',
        'shingle-bringup/DEPLOYED.json': json.dumps(
            {'commit': 'abcdef1234567890', 'dirty': False}),
        LOG: '\n'.join([
            'shingle: seat seat0 enabled (logind backend)',
            'shingle: input device event3 (AT Translated Set 2 keyboard) '
            'added as seat devices 1',
            'shingle: input device event7 (SynPS/2 Synaptics TouchPad) '
            'added as seat devices 2',
            'shingle: using /dev/dri/card1',
            'shingle: rendering on Intel(R) Graphics (ADL GT2)',
            'shingle: output eDP-1: 1920x1200 at 60.000 Hz',
            'shingle: output eDP-1: zero-copy scanout, modifier INTEL '
            '0x0100000000000002',
            'shingle: output eDP-1: the cursor is on the cursor plane',
        ]) + '\n',
    })

  def log(self, line):
    """Appends a line to native.log."""
    self.files[LOG] = self.files.get(LOG, '') + f'shingle: {line}\n'

  def wev(self, interface, event, rest):
    """Appends an event to wev's log at the current time."""
    stamp = int(self.now_ms) & 0xffffffff
    self.files[f'{RUN}/wev.log'] += (
        f'[14: {interface:>15}] {event}: serial: 1; time: {stamp}; {rest}\n')

  def sleep(self, seconds):
    """The injected sleep: advances the clock and what depends on it."""
    self.now_ms += seconds * 1000
    for held in list(self.held):
      if held['dropped']:
        self.held.remove(held)
      elif held['release'] <= self.now_ms:
        self.wev('wl_keyboard', 'key', f'key: {held["code"]}; state: 0 '
                 '(released)')
        self.held.remove(held)
    for device in list(self.plugged):
      if device['until'] <= self.now_ms:
        self.plugged.remove(device)
        self.devices.remove(device['entry'])
        self.log(f'input device event9 removed (seat devices '
                 f'{device["entry"]["id"]})')
    if self.probing and self.vt == 1 and self.dpms == 'on' and (
        self.running()):
      lines = int(seconds * 60)
      self.files[f'{RUN}/probe.out'] += '{"event":"frame"}\n' * lines
      self.files[LOG] += 'shingle: eDP-1: page flip seq=1 at 1.0 s\n' * lines

  def running(self):
    """Whether the fake Shingle runs."""
    return f'/proc/{PID}/comm' in self.files

  # The interpreter of the scripts the tools send.

  def sh(self, script):
    """Runs a script the code under test sends over SSH."""
    self.scripts.append(script)
    wrapped = WRAPPED.match(script)
    if wrapped:
      return self.client(wrapped.group(1))
    if script == remote.IDENTIFY:
      marker = 'marker' if self.marker else 'no-marker'
      return remote.Result(
          0, f'{self.hostname}\n{self.machine_id}\nkarl\n{marker}\n')
    for pattern, handler in self.handlers:
      match = re.search(pattern, script, re.S)
      if match:
        return handler(match)
    for pattern, handler in self.builtins():
      match = re.match(pattern, script, re.S)
      if match:
        return handler(match)
    return remote.Result(127, '', f'fake: no handler for {script!r}')

  def builtins(self):
    """The commands the fake knows, as (regex, handler)."""
    return [
        (r'^pid=(\d+); start=(\d+); .*kill -(\w+) "\$pid"$', self.kill),
        (r'^cat (\S+)(?: (\S+))?( 2>/dev/null)?$', self.cat),
        (r'^wc -l < (\S+)', self.wc),
        (r'^tail -n \+(\d+) (\S+)', self.tail),
        (r"^grep -c -E '(.*)' (\S+) 2>/dev/null", self.grep),
        (r'^ls -l /proc/(\d+)/fd', self.fds),
        (r'^ls -1 (\S+)', self.ls),
        (r'^python3 \S+/ipc\.py "[^"]+" (\S+) \'(.*)\'$', self.ipc),
        (r'^(nohup )?sudo -n python3 "\$HOME/\S+/uinput\.py" (.*?)'
         r'( > (\S+) 2>&1 &)?$', self.uinput),
        (r'^sudo -n chvt (\d+)$', self.chvt),
        (r'^sudo -n fgconsole$',
         lambda m: remote.Result(0, f'{self.vt}\n')),
        (r'^echo \$\(\( \$\(date', lambda m: remote.Result(0, '2\n')),
        (r'^export PATH=.*shingle (--backend native)? ?2>&1',
         self.ssh_refusal),
        (r'^basename ', lambda m: remote.Result(0, 'i915\n6.12.0\n')),
    ]

  def cat(self, match):
    """cat of one or two files."""
    paths = [p for p in match.groups()[:2] if p]
    if not all(p in self.files for p in paths):
      return remote.Result(1, ''.join(self.files.get(p, '') for p in paths),
                           'No such file')
    return remote.Result(0, ''.join(self.files[p] for p in paths))

  def wc(self, match):
    """wc -l of a file."""
    text = self.files.get(match.group(1), '')
    return remote.Result(0, f'{text.count(chr(10))}\n')

  def tail(self, match):
    """tail -n +N of a file."""
    lines = self.files.get(match.group(2), '').splitlines(True)
    return remote.Result(0, ''.join(lines[int(match.group(1)) - 1:]))

  def grep(self, match):
    """grep -c -E of a file."""
    pattern = re.compile(match.group(1))
    count = sum(1 for line in self.files.get(match.group(2), '').splitlines()
                if pattern.search(line))
    return remote.Result(0 if count else 1, f'{count}\n')

  def fds(self, match):
    """The descriptors of the fake Shingle."""
    if int(match.group(1)) != PID or not self.running():
      return remote.Result(2, '')
    targets = ['/dev/input/event3', '/dev/input/event7', '/dev/dri/card1',
               'anon_inode:dmabuf', 'anon_inode:dmabuf', 'anon_inode:dmabuf']
    return remote.Result(0, ''.join(
        f'lrwx------ 1 karl karl 64 Nov 28 10:00 {n} -> {t}\n'
        for n, t in enumerate(targets)))

  def ls(self, match):
    """ls -1 of a directory of the fake home."""
    prefix = match.group(1).rstrip('/') + '/'
    names = sorted({p[len(prefix):].split('/')[0] for p in self.files
                    if p.startswith(prefix)})
    return remote.Result(0, ''.join(f'{n}\n' for n in names))

  def ipc(self, match):
    """Wayfire IPC of the fake Shingle."""
    method, data = match.group(1), json.loads(match.group(2))
    if not self.running():
      return remote.Result(1, '', 'ConnectionRefusedError')
    if method == 'input/list-devices':
      return remote.Result(0, json.dumps(self.devices))
    if method == 'input/configure-device':
      for device in self.devices:
        if device['id'] == data['id']:
          device['enabled'] = data['enabled']
          if not data['enabled']:
            self.release_held(lambda held, d=device: held['device'] is d)
      return remote.Result(0, json.dumps({'result': 'ok'}))
    return remote.Result(0, json.dumps({'result': 'ok'}))

  def release_held(self, which):
    """Releases the held keys `which` selects now, as Shingle does."""
    for held in self.held:
      if not held['dropped'] and which(held):
        self.wev('wl_keyboard', 'key', f'key: {held["code"]}; state: 0 '
                 '(released)')
        held['dropped'] = True

  def plug(self, kind, seconds):
    """A uinput device of `kind` for `seconds`."""
    kinds = {'keyboard': 'keyboard', 'mouse': 'pointer', 'touch': 'touch'}
    entry = {'id': self.next_id, 'name': f'shingle-test {kind}',
             'type': kinds[kind], 'vendor': 4660, 'product': 22136,
             'enabled': True}
    self.next_id += 1
    self.devices.append(entry)
    self.plugged.append({'entry': entry,
                         'until': self.now_ms + seconds * 1000})
    self.log(f'input device event9 (shingle-test {kind}) added as seat '
             f'devices {entry["id"]}')
    return entry

  def uinput(self, match):
    """uinput.py: events reach wev at the fake clock's time."""
    args = shlex.split(match.group(2))
    background = match.group(4)
    settle = 1.0
    if args[0] == '--settle':
      settle, args = float(args[1]), args[2:]
    out = []
    when = self.now_ms + 0.4
    codes = {'KEY_A': 30, 'KEY_B': 48, 'KEY_LEFTSHIFT': 42,
             'KEY_LEFTCTRL': 29, 'KEY_LEFTALT': 56, 'KEY_F2': 60,
             'BTN_LEFT': 272}
    command = args[0]
    if command == 'device':
      seconds = float(args[args.index('--seconds') + 1])
      self.plug(args[1], seconds)
      out.append(f'shingle-test {args[1]} present at {when:.3f} ms')
    elif command == 'key':
      hold = float(args[args.index('--hold') + 1]) if '--hold' in args else 0
      names = [a for a in args[1:] if a.startswith('KEY_')]
      keyboard = self.plug('keyboard', settle + hold + 0.2)
      for name in names:
        if name == 'KEY_F2' and 'KEY_LEFTALT' in names:
          self.vt = 2
          self.log('seat disabled; paused')
          continue
        if self.vt == 1:
          self.wev('wl_keyboard', 'key', f'key: {codes[name]}; state: 1 '
                   '(pressed)')
        out.append(f'{name} ({codes[name]}) pressed at {when:.3f} ms')
      for name in names:
        if background and hold:
          self.held.append({'code': codes[name], 'device': keyboard,
                            'release': self.now_ms + hold * 1000,
                            'dropped': False})
        elif self.vt == 1 and name not in ('KEY_LEFTCTRL', 'KEY_LEFTALT'):
          self.wev('wl_keyboard', 'key', f'key: {codes[name]}; state: 0 '
                   '(released)')
          out.append(f'{name} ({codes[name]}) released at {when:.3f} ms')
    elif command == 'motion':
      self.wev('wl_pointer', 'motion', 'x, y: 10.0, 5.0')
      if self.cursor_plane:
        self.log('eDP-1: cursor plane flip')
      else:
        self.log('eDP-1: page flip seq=9 at 1.0 s')
      out.append(f'motion {args[1]} {args[2]} at {when:.3f} ms')
    elif command == 'button':
      for state, word in ((1, 'pressed'), (0, 'released')):
        self.wev('wl_pointer', 'button', f'button: 272 (left), state: '
                 f'{state} ({word})')
        out.append(f'{args[1]} (272) {word} at {when:.3f} ms')
    elif command == 'wheel':
      self.wev('wl_pointer', 'axis', 'axis: 0 (vertical), value: 15.0')
      out.append(f'wheel {args[1]} at {when:.3f} ms')
    elif command == 'touch':
      self.wev('wl_touch', 'down', 'surface: 3; id: 0; x, y: 960.0, 600.0')
      out.append(f'touch down 2048 2048 at {when:.3f} ms')
      self.wev('wl_touch', 'up', 'id: 0')
      out.append(f'touch up at {when:.3f} ms')
    text = ''.join(f'{line}\n' for line in out)
    if background:
      self.files[background] = text
      return remote.Result(0)
    return remote.Result(0, text)

  def chvt(self, match):
    """A VT switch: away pauses and releases held keys."""
    vt = int(match.group(1))
    if vt != 1 and self.vt == 1:
      self.release_held(lambda held: True)
      self.log('seat disabled; paused')
    elif vt == 1 and self.vt != 1:
      self.log('seat enabled; resumed')
    self.vt = vt
    return remote.Result(0)

  def kill(self, match):
    """The checked signal: only the recorded shingle is signalled."""
    pid, start, name = int(match.group(1)), int(match.group(2)), match.group(3)
    if f'/proc/{pid}/stat' not in self.files:
      return remote.Result(3)
    if self.files[f'/proc/{pid}/comm'].strip() != 'shingle':
      return remote.Result(4)
    if int(self.files[f'/proc/{pid}/stat'].split()[21]) != start:
      return remote.Result(5)
    self.signalled.append((pid, name))
    del self.files[f'/proc/{pid}/comm']
    del self.files[f'/proc/{pid}/stat']
    if name == 'KILL':
      self.files[f'{RUN}/exit'] = 'exit=137\n'
    else:
      self.log('native backend stopped')
      self.files[f'{RUN}/exit'] = 'exit=0\n'
    return remote.Result(0)

  def ssh_refusal(self, match):
    """`shingle` started over SSH, as the real binary refuses it."""
    reason = ('was requested on the command line; ...' if match.group(1)
              else 'because none of WAYLAND_DISPLAY, WAYLAND_SOCKET or '
              'DISPLAY is set; ...')
    first = ('shingle: no seat: started over SSH (SSH_CONNECTION is set and '
             'XDG_SEAT is not); run from a VT login or start seatd')
    if not self.ssh_refusal_ok:
      first = 'shingle: using the native backend'
    return remote.Result(0, f'{first}\nshingle: the native backend {reason}'
                         '\nstatus=1\nsame\n')

  def client(self, command):
    """A Wayland client of the fake Shingle: wlopm, grim, wayland-info."""
    if command == 'wlopm':
      return remote.Result(0, f'eDP-1 {self.dpms}\n')
    match = re.match(r"wlopm --(on|off) '\*'", command)
    if match:
      self.dpms = match.group(1)
      self.log(f'output eDP-1 {self.dpms}')
      return remote.Result(0)
    match = re.match(r'grim (\S+)', command)
    if match:
      self.files[match.group(1)] = png(1920, 1200)
      return remote.Result(0)
    if command.startswith('wayland-info'):
      return remote.Result(0, "interface: 'wl_output', version: 4\n")
    return remote.Result(127, '', f'fake: no client {command}')

def copy_runner(machine, real_local):
  """A runner that sends local Python and file commands to `real_local`
  and everything else to `machine`."""
  def runner(argv, timeout=None, cwd=None, env=None):
    if argv and os.path.basename(argv[0]).startswith('python'):
      return real_local(argv, timeout=timeout, cwd=cwd, env=env)
    return machine.runner(argv, timeout=timeout, cwd=cwd, env=env)
  return runner
