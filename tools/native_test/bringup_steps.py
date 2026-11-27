"""The steps of docs/native-testing.md, printed and, where SSH can, driven.

`instructions` prints a step's text from native-testing.md itself, so it
never drifts from the plan, after the tty1 command that starts Shingle for
it (start_tty1.py's STEP_PRESETS). `DRIVERS` holds the steps that can run
over SSH while Shingle runs on tty1: IPC calls, uinput events, VT switches
with `sudo -n chvt`, DPMS through wlopm, grim, the probe and its check,
and the signals. Each driver checks what the step expects through a
`Session` on the target and writes the outcome into a `Record`, whose
Markdown is the Results format of native-testing.md#results.

Signals go only to the PID that start_tty1.py recorded, and only if that
PID still is a `shingle` with the recorded start time, checked in the
same remote command that sends it (`signal_script`).
"""

import dataclasses
import datetime
import json
import os
import re
import time

import bringup_logs
import start_tty1

BASE = 'shingle-bringup'
TOOLS = f'{BASE}/tools/native_test'
RUN = '.local/state/shingle/bringup/current'
LOG = '.local/state/shingle/native.log'
RUNTIME = '/run/user/$(id -u)'
IPC_SOCKET = f'{RUNTIME}/shingle-ipc'
SIGNALS = ('TERM', 'INT', 'KILL')
STEP_HEADING = re.compile(r'^### Step (\d+): (.*)$')
# Steps that do not start Shingle on tty1, or start it in a way of their
# own (step 8 as root).
NO_START = {0, 2, 8, 9, 16, 25, 36, 44}

class DriveError(Exception):
  """A step cannot go on: Shingle is not running, a tool failed."""

def plan_path():
  """docs/native-testing.md of this checkout."""
  here = os.path.dirname(os.path.abspath(__file__))
  return os.path.join(here, '..', '..', 'docs', 'native-testing.md')

def step_sections(text):
  """{number: (title, body)} of every `### Step N: TITLE` section."""
  sections = {}
  number = None
  for line in text.splitlines():
    match = STEP_HEADING.match(line)
    if match:
      number = int(match.group(1))
      sections[number] = (match.group(2), [])
    elif line.startswith('## ') or line.startswith('### '):
      number = None
    elif number is not None:
      sections[number][1].append(line)
  return {n: (title, '\n'.join(body).strip())
          for n, (title, body) in sections.items()}

def instructions(number, text, host=None):
  """What the operator does for step `number`, as printable text."""
  sections = step_sections(text)
  if number not in sections:
    raise KeyError(f'native-testing.md has no step {number}')
  title, body = sections[number]
  host_arg = host or 'HOST'
  lines = [f'Step {number}: {title}', '']
  if number not in NO_START:
    preset = start_tty1.STEP_PRESETS.get(number)
    lines += ['On the test machine, log in on tty1 as the test user and run',
              '', f'  shingle-tty1 --step {number}', '']
    if preset is None:
      lines += ['(no preset: the test config and foot; add --config, '
                '--client or -- PROGRAM as the step says)', '']
  if number in DRIVERS:
    lines += ['Then, on the development host:', '',
              f'  tools/native_test/bringup.py step {number} {host_arg}', '',
              'drives the parts SSH can and records the result; the parts '
              'that need eyes on the screen or hands on the keyboard are '
              'listed after it.', '']
  elif number not in NO_START:
    lines += ['This step is not driven over SSH; follow it by hand, then '
              f'`tools/native_test/bringup.py collect {host_arg}`.', '']
  lines += ['From docs/native-testing.md:', '', body, '']
  return '\n'.join(lines)

@dataclasses.dataclass
class Entry:
  """One line of a step's result."""

  kind: str
  text: str

class Record:
  """The outcome of one driven step."""

  def __init__(self, number, title):
    self.number = number
    self.title = title
    self.entries = []

  def check(self, ok, text):
    """Records a check; returns `ok`."""
    self.entries.append(Entry('PASS' if ok else 'FAIL', text))
    return ok

  def info(self, text):
    """Records a measured value without judging it."""
    self.entries.append(Entry('INFO', text))

  def manual(self, text):
    """Records what the operator still checks on the screen or keyboard."""
    self.entries.append(Entry('MANUAL', text))

  @property
  def passed(self):
    """Whether no check failed."""
    return all(entry.kind != 'FAIL' for entry in self.entries)

  def markdown(self, date, commit, host, facts):
    """The Results entry: a headline with the date, the commit, the host,
    the libseat backend and the devices, then one bullet per entry."""
    counts = {kind: sum(1 for e in self.entries if e.kind == kind)
              for kind in ('PASS', 'FAIL', 'MANUAL')}
    verdict = 'passed' if self.passed else 'FAILED'
    lines = [
        f'Step {self.number} ({self.title}), {date}, commit {commit}, on '
        f'{host}: {verdict}, {counts["PASS"]} checks passed, '
        f'{counts["FAIL"]} failed, {counts["MANUAL"]} to check by hand. '
        f'{facts}', '']
    lines += [f'- {e.kind}: {e.text}' for e in self.entries]
    return '\n'.join(lines) + '\n'

def facts_line(summary):
  """The libseat backend and devices of a run, from bringup_logs.scan."""
  outputs = ', '.join(
      f'{name} {w}x{h}@{hz:g}'
      for name, (w, h, hz) in sorted(summary['outputs'].items()))
  return (f'libseat {summary["libseat"] or "unknown"} backend, card '
          f'{summary["card"] or "none"}, renderer '
          f'{summary["renderer"] or "none"}, outputs {outputs or "none"}, '
          f'{len(summary["input_devices"])} input devices.')

def stat_start_time(stat):
  """Field 22 (start time) of a /proc/PID/stat line."""
  return int(stat[stat.rindex(')') + 2:].split()[19])

def signal_script(pid, start, name):
  """A remote sh script that sends SIG`name` to `pid` only if it is a
  `shingle` with start time `start`; its status is 0 when sent, 3 if no
  such process, 4 for another comm, 5 for another start time."""
  if not isinstance(pid, int) or pid <= 1:
    raise DriveError(f'refusing to signal PID {pid!r}')
  if not isinstance(start, int) or start <= 0:
    raise DriveError(f'refusing to signal without a start time ({start!r})')
  if name not in SIGNALS:
    raise DriveError(f'refusing signal {name}')
  return (f'pid={pid}; start={start}; '
          'stat=$(cat /proc/$pid/stat 2>/dev/null) || exit 3; '
          '[ "$(cat /proc/$pid/comm)" = shingle ] || exit 4; '
          'rest=${stat##*) }; '
          '[ "$(echo "$rest" | cut -d" " -f20)" = "$start" ] || exit 5; '
          f'kill -{name} "$pid"')

UINPUT_LINE = re.compile(r'^(.*) at ([\d.]+) ms$')
WEV_LINE = re.compile(r'^\[\s*\d+:\s*(wl_\w+)\]\s*(\w+):(.*)$')

def parse_uinput(text):
  """[(what, CLOCK_MONOTONIC ms)] from uinput.py's output."""
  events = []
  for line in text.splitlines():
    match = UINPUT_LINE.match(line.strip())
    if match:
      events.append((match.group(1), float(match.group(2))))
  return events

def parse_wev(lines):
  """wev's events as dicts: interface, event, time and, where present,
  key, button, state, x and y."""
  events = []
  for line in lines:
    match = WEV_LINE.match(line.strip())
    if not match:
      continue
    interface, event, rest = match.groups()
    item = {'interface': interface, 'event': event}
    for key, pattern in (('time', r'time: (\d+)'), ('key', r'key: (\d+)'),
                         ('button', r'button: (\d+)'),
                         ('state', r'state: (\d+)')):
      found = re.search(pattern, rest)
      if found:
        item[key] = int(found.group(1))
    found = re.search(r'x, y: (-?[\d.]+), (-?[\d.]+)', rest)
    if found:
      item['x'], item['y'] = float(found.group(1)), float(found.group(2))
    events.append(item)
  return events

def expected_wev(what):
  """The wev event a uinput.py line should produce, as a dict to match,
  or None for lines that produce none."""
  match = re.match(r'(KEY_\w+|BTN_\w+) \((\d+)\) (pressed|released)$', what)
  if match:
    code = int(match.group(2))
    state = 1 if match.group(3) == 'pressed' else 0
    if match.group(1).startswith('BTN_'):
      return {'interface': 'wl_pointer', 'event': 'button', 'button': code,
              'state': state}
    return {'interface': 'wl_keyboard', 'event': 'key', 'key': code,
            'state': state}
  if what.startswith('motion '):
    return {'interface': 'wl_pointer', 'event': 'motion'}
  if what.startswith('wheel '):
    return {'interface': 'wl_pointer', 'event': 'axis'}
  if what.startswith('touch down'):
    return {'interface': 'wl_touch', 'event': 'down'}
  if what == 'touch up':
    return {'interface': 'wl_touch', 'event': 'up'}
  return None

def match_times(uinput_events, wev_events):
  """[(what, time difference in ms or None)] for every uinput event that
  should reach wev: the smallest difference to a matching wev event, with
  libinput's millisecond time truncated as wl events carry it."""
  results = []
  for what, ms in uinput_events:
    want = expected_wev(what)
    if want is None:
      continue
    stamp = int(ms) & 0xffffffff
    candidates = [abs(e['time'] - stamp) for e in wev_events
                  if 'time' in e and all(e.get(k) == v
                                         for k, v in want.items())]
    results.append((what, min(candidates) if candidates else None))
  return results

class Session:
  """A driven step's access to the test machine."""

  def __init__(self, target, sleep=time.sleep, local_dir=None):
    self.target = target
    self.sleep = sleep
    self.local_dir = local_dir
    self.background = 0

  def sh(self, script, timeout=60):
    """Runs `script` on the target."""
    return self.target.run(script, timeout=timeout)

  def read(self, path):
    """The text of the remote file `path`, or None."""
    result = self.sh(f'cat {path}')
    return result.stdout if result.ok else None

  def count_lines(self, path):
    """The number of lines of the remote file `path` (0 if missing)."""
    result = self.sh(f'wc -l < {path} 2>/dev/null || echo 0')
    try:
      return int(result.stdout.strip() or 0)
    except ValueError:
      return 0

  def lines_after(self, path, mark):
    """The lines of `path` after its first `mark` lines."""
    result = self.sh(f'tail -n +{mark + 1} {path} 2>/dev/null')
    return result.stdout.splitlines()

  def log(self):
    """The lines of native.log."""
    return (self.read(LOG) or '').splitlines()

  def grep_count(self, pattern, path=LOG):
    """How many lines of `path` match the extended regex `pattern`."""
    result = self.sh(f"grep -c -E '{pattern}' {path} 2>/dev/null || true")
    try:
      return int(result.stdout.strip() or 0)
    except ValueError:
      return 0

  def ipc(self, method, data=None):
    """Calls an IPC method of the running Shingle; returns the reply."""
    payload = json.dumps(data or {}).replace("'", "'\\''")
    result = self.sh(f'python3 {TOOLS}/ipc.py "{IPC_SOCKET}" {method} '
                     f"'{payload}'")
    if not result.ok:
      raise DriveError(f'ipc {method} failed: {result.stderr.strip()}')
    return json.loads(result.stdout)

  def uinput(self, *args, timeout=60):
    """Runs uinput.py with sudo -n (bringup.py check prints the sudoers
    rule it needs); returns its Result."""
    return self.sh('sudo -n python3 "$HOME/' + TOOLS + '/uinput.py" ' +
                   ' '.join(args), timeout=timeout)

  def uinput_background(self, *args):
    """Starts uinput.py in the background; returns its output file."""
    self.background += 1
    out = f'{RUN}/uinput-{self.background}.out'
    self.sh(f'nohup sudo -n python3 "$HOME/{TOOLS}/uinput.py" '
            f'{" ".join(args)} > {out} 2>&1 &')
    return out

  def chvt(self, vt):
    """Switches VT with sudo -n chvt."""
    result = self.sh(f'sudo -n chvt {int(vt)}')
    if not result.ok:
      raise DriveError(f'chvt {vt} failed: {result.stderr.strip()}')

  def fgconsole(self):
    """The active VT's number as text."""
    return self.sh('sudo -n fgconsole').stdout.strip()

  def recorded_pid(self):
    """(pid, start time) of the run's Shingle from start_tty1.py's pid
    file, or None."""
    text = self.read(f'{RUN}/pid')
    try:
      pid_text, start_text = (text or '').split()[:2]
      return int(pid_text), int(start_text)
    except ValueError:
      return None

  def running(self):
    """Whether the recorded Shingle still runs (same comm and start)."""
    recorded = self.recorded_pid()
    if recorded is None:
      return False
    pid, start = recorded
    result = self.sh(f'cat /proc/{pid}/comm /proc/{pid}/stat 2>/dev/null')
    lines = result.stdout.splitlines()
    if not result.ok or len(lines) < 2 or lines[0].strip() != 'shingle':
      return False
    try:
      return stat_start_time(lines[1]) == start
    except (ValueError, IndexError):
      return False

  def signal(self, name):
    """Sends SIG`name` to the recorded Shingle, checked on the target."""
    recorded = self.recorded_pid()
    if recorded is None:
      raise DriveError(f'no PID in {RUN}/pid; was Shingle started with '
                       'shingle-tty1?')
    result = self.sh(signal_script(*recorded, name))
    if not result.ok:
      reason = {3: 'it no longer runs', 4: 'that PID is not shingle',
                5: 'that PID was reused'}.get(result.returncode,
                                              result.stderr.strip())
      raise DriveError(f'not signalled: {reason}')
    return recorded[0]

  def wait_exit(self, timeout=10.0):
    """The run's `exit=N` line once start_tty1.py wrote it, or None."""
    waited = 0.0
    while True:
      text = self.read(f'{RUN}/exit')
      if text:
        return text.strip()
      if waited >= timeout:
        return None
      self.sleep(0.5)
      waited += 0.5

  def wayland(self, script, timeout=60):
    """Runs `script` as a client of the running Shingle."""
    stdout = self.read(f'{RUN}/stdout') or ''
    found = re.search(r'^WAYLAND_DISPLAY=(\S+)$', stdout, re.M)
    if not found:
      raise DriveError('no WAYLAND_DISPLAY in the run\'s stdout')
    return self.sh(f'XDG_RUNTIME_DIR={RUNTIME} '
                   f'WAYLAND_DISPLAY={found.group(1)} {script}',
                   timeout=timeout)

  def fetch(self, remote, name):
    """Copies `remote` into the local results directory; returns the
    local path or None."""
    if not self.local_dir:
      return None
    os.makedirs(self.local_dir, exist_ok=True)
    local = os.path.join(self.local_dir, name)
    return local if self.target.get(remote, local).ok else None

  def fd_targets(self, pid):
    """The targets of the descriptors of `pid`."""
    result = self.sh(f'ls -l /proc/{int(pid)}/fd 2>/dev/null')
    return [line.split(' -> ', 1)[1] for line in result.stdout.splitlines()
            if ' -> ' in line]

def png_size(path):
  """(width, height) of a PNG file, or None."""
  try:
    with open(path, 'rb') as file:
      head = file.read(24)
  except OSError:
    return None
  if head[:8] != b'\x89PNG\r\n\x1a\n':
    return None
  return int.from_bytes(head[16:20], 'big'), int.from_bytes(head[20:24], 'big')

def summary_of(session):
  """bringup_logs.scan of the run's log and stderr."""
  stderr = (session.read(f'{RUN}/stderr') or '').splitlines()
  return bringup_logs.scan(session.log(), stderr)

def stop(session, record, name, expected):
  """Sends SIG`name` and checks `exit=expected`."""
  pid = session.signal(name)
  status = session.wait_exit()
  record.check(status == f'exit={expected}',
               f'SIG{name} to PID {pid}: {status or "no exit file"} '
               f'(expected exit={expected})')
  return status

def check_log_basics(session, record):
  """The lines every native run starts with; returns the scan summary."""
  summary = summary_of(session)
  record.check(summary['backend'] == 'native',
               f'stderr says the {summary["backend"] or "?"} backend')
  record.check(summary['seat'] == 'seat0',
               f'seat {summary["seat"]} enabled ({summary["libseat"]} '
               'backend)')
  record.check(not summary['failures'],
               f'{len(summary["failures"])} failure lines in native.log'
               + (f': {summary["failures"][0]}' if summary['failures']
                  else ''))
  return summary

def drive_seat(session, record, options):
  """Step 1: the native backend under a seat, devices and descriptors."""
  summary = check_log_basics(session, record)
  record.check(bool(summary['input_devices']),
               f'{len(summary["input_devices"])} input devices added: '
               f'{", ".join(summary["input_devices"])}')
  devices = session.ipc('input/list-devices')
  record.check(bool(devices) and all(d.get('enabled') for d in devices),
               f'input/list-devices lists {len(devices)} devices, all '
               'enabled')
  record.info('devices: ' + '; '.join(
      f'{d.get("name")} ({d.get("type")}, {d.get("vendor")}:'
      f'{d.get("product")})' for d in devices))
  pid = session.recorded_pid()[0]
  fds = session.fd_targets(pid)
  record.check(any(f.startswith('/dev/input/event') for f in fds),
               'Shingle holds /dev/input/event nodes')
  record.info(f'{sum(1 for f in fds if f.startswith("/dev/dri/card"))} '
              'card nodes open (one since NB-7)')
  if options.stop:
    stop(session, record, 'TERM', 0)
  record.manual('repeat under seatd: sudo systemctl start seatd, then '
                'shingle-tty1 --step 1 --env LIBSEAT_BACKEND=seatd')

def drive_ssh_refusal(session, record, options):
  """Step 2: shingle refuses to start over SSH."""
  if session.running():
    raise DriveError('Shingle runs on tty1; step 2 needs nothing running')
  first = ('shingle: no seat: started over SSH (SSH_CONNECTION is set and '
           'XDG_SEAT is not); run from a VT login or start seatd')
  for argv, reason in ((
      '', 'none of WAYLAND_DISPLAY, WAYLAND_SOCKET or DISPLAY is set'),
      ('--backend native', 'was requested on the command line')):
    result = session.sh(
        f'export PATH="$HOME/{BASE}/bin:$PATH"; before=$(ls {RUNTIME}); '
        f'shingle {argv} 2>&1; echo "status=$?"; '
        f'after=$(ls {RUNTIME}); [ "$before" = "$after" ] && echo same')
    lines = result.stdout.splitlines()
    record.check(bool(lines) and lines[0] == first,
                 f'`shingle {argv}` first line: {lines[0] if lines else ""}')
    record.check(len(lines) > 1 and reason in lines[1],
                 f'second line names why native was chosen ({reason})')
    record.check('status=1' in lines, f'`shingle {argv}` exits 1')
    record.check('same' in lines, 'no new socket in the runtime directory')
  record.manual('item 3 (XDG_SEAT=seat0 over SSH) needs seatd stopped and '
                'tty1 on a text console; run it by hand')

def drive_uinput_events(session, record, options):
  """Step 3: uinput events reach wev with libinput's timestamps."""
  wev = f'{RUN}/wev.log'
  mark = session.count_lines(wev)
  printed = []
  for args in (('key', 'KEY_A'), ('motion', '10', '5'),
               ('button', 'BTN_LEFT'), ('wheel', '-1'),
               ('touch', '0.5', '0.5')):
    result = session.uinput(*args)
    if not record.check(result.ok, f'uinput {" ".join(args)}'
                        + ('' if result.ok else f': {result.stderr.strip()}')):
      continue
    printed += parse_uinput(result.stdout)
  session.sleep(0.5)
  events = parse_wev(session.lines_after(wev, mark))
  for what, difference in match_times(printed, events):
    if difference is None:
      record.check(False, f'{what}: no matching wev event')
    else:
      record.check(difference <= 2,
                   f'{what}: wev time {difference} ms from uinput\'s')
  downs = [e for e in events if e.get('event') == 'down' and 'x' in e]
  if downs:
    record.info(f'touch down at ({downs[0]["x"]:g}, {downs[0]["y"]:g}) in '
                'wev\'s window; expected the middle of the output')

def find_device(devices, name):
  """The listed device named `name`, or None."""
  return next((d for d in devices if d.get('name') == name), None)

def drive_hotplug(session, record, options):
  """Step 4: uinput devices appear in and leave input/list-devices."""
  seen_ids = [d.get('id', 0) for d in session.ipc('input/list-devices')]
  for kind, kind_type in (('keyboard', 'keyboard'), ('mouse', 'pointer'),
                          ('touch', 'touch')):
    name = f'shingle-test {kind}'
    session.uinput_background('device', kind, '--seconds', '10')
    session.sleep(3)
    device = find_device(session.ipc('input/list-devices'), name)
    if not record.check(device is not None, f'{name} listed while plugged'):
      continue
    record.check(
        (device.get('vendor'), device.get('product'), device.get('type'),
         device.get('enabled')) == (4660, 22136, kind_type, True),
        f'{name}: vendor {device.get("vendor")}, product '
        f'{device.get("product")}, type {device.get("type")}, enabled '
        f'{device.get("enabled")}')
    record.check(device.get('id', 0) > max(seen_ids, default=0),
                 f'{name} has the new id {device.get("id")}, never reused')
    seen_ids.append(device.get('id', 0))
    session.sleep(9)
    record.check(find_device(session.ipc('input/list-devices'), name) is None,
                 f'{name} gone after it was removed')
  added = session.grep_count('input device event[0-9]+ \\(shingle-test')
  removed = session.grep_count('input device event[0-9]+ removed')
  record.check(added >= 3 and removed >= 3,
               f'the log has {added} added and {removed} removed lines')

def wev_events(session, mark):
  """wev's events after line `mark` of its log."""
  return parse_wev(session.lines_after(f'{RUN}/wev.log', mark))

def key_events(events, key, state):
  """wev's key events of `key` in `state`."""
  return [e for e in events if e.get('event') == 'key'
          and e.get('key') == key and e.get('state') == state]

def drive_configure_device(session, record, options):
  """Step 5: configure-device disables a device and releases its keys."""
  wev = f'{RUN}/wev.log'
  mark = session.count_lines(wev)
  held = session.uinput_background('--settle', '2', 'key', 'KEY_LEFTSHIFT',
                                   '--hold', '20')
  session.sleep(4)
  device = find_device(session.ipc('input/list-devices'),
                       'shingle-test keyboard')
  if not record.check(device is not None, 'the held keyboard is listed'):
    return
  record.check(bool(key_events(wev_events(session, mark), 42, 1)),
               'wev saw Shift (42) pressed')
  disabled_at = session.count_lines(wev)
  reply = session.ipc('input/configure-device',
                      {'id': device['id'], 'enabled': False})
  record.check(reply == {'result': 'ok'}, f'disable replied {reply}')
  session.sleep(0.5)
  record.check(len(key_events(wev_events(session, disabled_at), 42, 0)) == 1,
               'disabling released Shift at once')
  listed = find_device(session.ipc('input/list-devices'),
                       'shingle-test keyboard')
  record.check(listed is not None and listed.get('enabled') is False,
               'the device is listed as disabled')
  before_b = session.count_lines(wev)
  session.uinput('key', 'KEY_B')
  record.check(bool(key_events(wev_events(session, before_b), 48, 1)),
               'KEY_B from a second keyboard still reaches wev')
  session.sleep(18)
  record.check(len(key_events(wev_events(session, disabled_at), 42, 0)) == 1,
               'the real release of Shift after 20 s did not reach wev')
  record.info(f'uinput output: {(session.read(held) or "").strip()[:200]}')
  record.manual('item 5 and 6: re-enable a real keyboard over IPC, and hold '
                'a physical key while it is disabled')

def device_states(devices):
  """{id: (name, enabled)} without the uinput test devices."""
  return {d.get('id'): (d.get('name'), d.get('enabled')) for d in devices
          if not str(d.get('name', '')).startswith('shingle-test')}

def drive_vt_switch_input(session, record, options):
  """Step 6: a VT switch releases held input; switching back restores."""
  mark = session.count_lines(f'{RUN}/wev.log')
  session.uinput_background('--settle', '2', 'key', 'KEY_A', '--hold', '15')
  session.sleep(4)
  before = device_states(session.ipc('input/list-devices'))
  record.check(bool(key_events(wev_events(session, mark), 30, 1)),
               'wev saw KEY_A (30) pressed')
  session.chvt(2)
  session.sleep(2)
  record.check(session.fgconsole() == '2', 'fgconsole says 2')
  record.check(len(key_events(wev_events(session, mark), 30, 0)) == 1,
               'the switch released KEY_A')
  record.check(session.grep_count('seat disabled; paused') >= 1,
               'the log says seat disabled; paused')
  resumed_at = session.count_lines(f'{RUN}/wev.log')
  session.chvt(1)
  session.sleep(2)
  record.check(session.grep_count('seat enabled; resumed') >= 1,
               'the log says seat enabled; resumed')
  after = device_states(session.ipc('input/list-devices'))
  record.check(before == after, 'devices and enabled states unchanged')
  session.uinput('key', 'KEY_B')
  record.check(bool(key_events(wev_events(session, resumed_at), 48, 1)),
               'KEY_B reaches wev after the resume')
  session.sleep(10)
  record.check(not key_events(wev_events(session, resumed_at), 30, 0),
               'the late release of KEY_A did not reach wev')
  chord_at = session.count_lines(f'{RUN}/wev.log')
  session.uinput('key', 'KEY_LEFTCTRL', 'KEY_LEFTALT', 'KEY_F2')
  session.sleep(1)
  record.check(session.fgconsole() == '2', 'Ctrl+Alt+F2 switched to VT 2')
  keys = {e.get('key') for e in wev_events(session, chord_at)
          if e.get('event') == 'key'}
  record.check(29 in keys and 56 in keys and 60 not in keys,
               f'wev saw keys {sorted(k for k in keys if k)}: Ctrl and Alt, '
               'not F2')
  session.chvt(1)
  record.manual('Ctrl+Alt+F2 on the physical keyboard, then Ctrl+Alt+F1 on '
                'tty2, as in item 4')

def drive_signal(default):
  """A driver that ends Shingle with a signal (steps 7, 14 and 51)."""
  def drive(session, record, options):
    name = options.signal or default
    summary = summary_of(session)
    record.info(f'{summary["flips"]} page flips before the signal')
    stop(session, record, name, 137 if name == 'KILL' else 0)
    if name != 'KILL':
      record.check(any('native backend stopped' in line
                       for line in session.log()[-2:]),
                   'the log ends with native backend stopped')
      age = session.sh('echo $(( $(date +%s) - $(stat -c %Y '
                       '.local/state/shingle/session.json) ))').stdout.strip()
      record.check(age.isdigit() and int(age) < 30,
                   f'session.json written {age} s ago')
    record.manual('on tty1: the prompt is back, `echo ok` prints ok, and '
                  'Ctrl+Alt+F2 and Ctrl+Alt+F1 switch VT')
  return drive

def drive_visible(session, record, options):
  """Step 10: a client is visible: card, mode, renderer, grim."""
  summary = check_log_basics(session, record)
  record.check(bool(summary['card']), f'using {summary["card"]}')
  record.check(bool(summary['outputs']),
               f'outputs: {", ".join(summary["outputs"]) or "none"}')
  record.check(bool(summary['renderer']),
               f'rendering on {summary["renderer"]}')
  pid = session.recorded_pid()[0]
  record.check(any(f.startswith('/dev/dri/card')
                   for f in session.fd_targets(pid)),
               'Shingle holds the card node')
  shot = session.wayland(f'grim {RUN}/shot.png')
  local = session.fetch(f'{RUN}/shot.png', f'step{record.number}-shot.png')
  size = png_size(local) if local else None
  modes = [(w, h) for w, h, _ in summary['outputs'].values()]
  record.check(shot.ok and size is not None and (size in modes or not modes),
               f'grim wrote a {size} PNG ({local}); modes {modes}')
  info = session.wayland("wayland-info | grep -A4 \"interface: 'wl_output'\"")
  record.info('wayland-info: ' + ' '.join(info.stdout.split())[:300])
  record.manual('on the screen: the client tiled over the whole output, '
                'sharp, and the pointer moves')
  if options.stop:
    stop(session, record, 'TERM', 0)

def wait_for_probe(session, frames, timeout=60):
  """Waits until the probe printed `frames` presented or discarded
  events, or `timeout` seconds."""
  waited = 0
  while waited < timeout:
    done = session.grep_count('"event":"(presented|discarded)"',
                              f'{RUN}/probe.out')
    if done >= frames:
      return done
    session.sleep(2)
    waited += 2
  return session.grep_count('"event":"(presented|discarded)"',
                            f'{RUN}/probe.out')

def present_check(session, record, local_check, flags, frames):
  """Fetches the log and probe output and runs present_check.py on them
  locally; returns its statistics or None."""
  log = session.fetch(LOG, f'step{record.number}-native.log')
  probe = session.fetch(f'{RUN}/probe.out', f'step{record.number}-probe.out')
  if not (log and probe):
    record.check(False, 'could not fetch native.log and probe.out')
    return None
  result = local_check([*flags, log, probe])
  lines = result.stdout.splitlines()
  try:
    statistics = json.loads(lines[0])
  except (ValueError, IndexError):
    record.check(False, f'present_check.py printed {result.stdout!r}')
    return None
  record.check(result.returncode == 0,
               f'present_check.py {" ".join(flags)}: status '
               f'{result.returncode}' + ''.join(f'; {p}' for p in lines[1:6]))
  record.info(f'statistics: {json.dumps(statistics)}')
  record.check(statistics.get('presented') == frames,
               f'{statistics.get("presented")} of {frames} frames presented')
  record.check(statistics.get('discarded') == 0,
               f'{statistics.get("discarded")} discarded')
  return statistics

def refresh_ms(summary):
  """The frame intervals in ms one refresh of the first output rounds
  to."""
  if not summary['outputs']:
    return set()
  hz = sorted(summary['outputs'].values())[0][2]
  period = 1000.0 / hz
  return {int(period), round(period)}

def drive_presentation(scanout):
  """A driver of the presentation checks (steps 11, 38, 48, 49)."""
  def drive(session, record, options):
    summary = check_log_basics(session, record)
    frames = options.frames or 600
    record.check(wait_for_probe(session, frames) >= frames,
                 f'the probe finished its {frames} frames')
    flags = list(scanout)
    if options.scanout:
      flags = ['--scanout', options.scanout]
    statistics = present_check(session, record, options.local_check, flags,
                               frames)
    if statistics:
      median = statistics.get('median_frame_interval_ms')
      periods = refresh_ms(summary)
      if '--tearing' in flags:
        record.info(f'median frame interval {median} ms with tearing')
      else:
        record.check(median in periods,
                     f'median frame interval {median} ms, one refresh is '
                     f'{sorted(periods)}')
      record.check(statistics.get('max_difference_ns', 1 << 40) < 1_000_000,
                   f'largest feedback difference '
                   f'{statistics.get("max_difference_ns")} ns')
    record.manual('record the mode and GPU; for the CPU comparison of step '
                  '38 read `ps -o times=` before and after the probe')
  return drive

def drive_tearing(session, record, options):
  """Step 48: a tearing client gets async flips."""
  options.frames = options.frames or 600
  drive_presentation(['--tearing'])(session, record, options)
  record.info(f'{session.grep_count("page flip.* async")} async flips')

def drive_dpms(session, record, options):
  """Step 12: DPMS off and on through wlopm."""
  check_log_basics(session, record)
  record.info('wlopm: ' + session.wayland('wlopm').stdout.strip())
  session.wayland("wlopm --off '*'")
  session.sleep(3)
  listed = session.wayland('wlopm').stdout
  record.check(' off' in listed, f'wlopm lists {listed.strip()}')
  record.check(session.grep_count('output [^ ]+ off|display off') >= 1,
               'the log says the output is off')
  first = session.count_lines(f'{RUN}/probe.out')
  session.sleep(2)
  second = session.count_lines(f'{RUN}/probe.out')
  record.check(first == second,
               f'the probe output stays at {first} lines while off '
               f'({second} after 2 s)')
  session.wayland("wlopm --on '*'")
  session.sleep(2)
  record.check(' on' in session.wayland('wlopm').stdout, 'wlopm lists on')
  record.check(session.grep_count('output [^ ]+ on$|display on') >= 1,
               'the log says the output is on')
  record.check(session.count_lines(f'{RUN}/probe.out') > second,
               'the probe output grows again')
  record.manual('the monitor went dark within a few seconds and came back '
                'within a second; a key press while off turns it on')

def drive_vt_display(session, record, options):
  """Step 13: a VT switch away and back resumes with a fresh modeset."""
  check_log_basics(session, record)
  session.chvt(2)
  session.sleep(2)
  record.check(session.grep_count('seat disabled; paused') >= 1,
               'the log says seat disabled; paused')
  first = session.count_lines(f'{RUN}/probe.out')
  session.sleep(2)
  record.check(session.count_lines(f'{RUN}/probe.out') == first,
               'the probe output stops growing while away')
  session.chvt(1)
  session.sleep(2)
  record.check(session.grep_count('seat enabled; resumed') >= 1,
               'the log says seat enabled; resumed')
  record.check(session.grep_count('modeset after resume failed') == 0,
               'no modeset error after the resume')
  record.check(session.count_lines(f'{RUN}/probe.out') > first,
               'the probe output grows again')
  shot = session.wayland(f'grim {RUN}/after.png')
  record.check(shot.ok, 'grim captured the picture after the resume')
  session.fetch(f'{RUN}/after.png', f'step{record.number}-after.png')
  record.manual('item 3: Ctrl+Alt+F2 and Ctrl+Alt+F1 five times quickly; '
                'item 4: a console mode change on tty2')

def drive_zero_copy(session, record, options):
  """Step 37: zero-copy is selected and the picture is right."""
  summary = check_log_basics(session, record)
  for name, line in sorted(summary['scanout'].items()):
    record.check(line.startswith('zero-copy scanout'), f'{name}: {line}')
  if not summary['scanout']:
    record.check(False, 'no scanout line in the log')
  shot = session.wayland(f'grim {RUN}/zero-copy.png')
  record.check(shot.ok, 'grim captured the zero-copy picture')
  session.fetch(f'{RUN}/zero-copy.png', f'step{record.number}-zero-copy.png')
  pid = session.recorded_pid()[0]
  dmabufs = sum(1 for f in session.fd_targets(pid) if 'dmabuf' in f)
  record.check(dmabufs >= 3 * max(1, len(summary['outputs'])),
               f'{dmabufs} DMA-BUF descriptors (3 per zero-copy output)')
  card = (summary['card'] or '/dev/dri/card0').rsplit('/', 1)[1]
  driver = session.sh(f'basename "$(readlink -f /sys/class/drm/{card}/'
                      'device/driver)"; uname -r').stdout.split()
  record.info(f'driver and kernel: {" ".join(driver)}')
  record.manual('on the screen: right colours, no tiling garbage, no '
                'tearing while typing; record any difference from step 10')

def drive_cursor_plane(session, record, options):
  """Step 45: the cursor moves on its plane without drawing frames."""
  summary = check_log_basics(session, record)
  for name, line in sorted(summary['cursor'].items()):
    record.info(f'{name}: the cursor {line}')
  forced = any('SHINGLE_CURSOR_PLANE=0' in line or
               'WLR_NO_HARDWARE_CURSORS' in line for line in session.log())
  frames = session.grep_count('page flip seq')
  moves = session.grep_count('cursor plane flip')
  for _ in range(10):
    session.uinput('--settle', '0.5', 'motion', '20', '10')
  session.sleep(1)
  frames = session.grep_count('page flip seq') - frames
  moves = session.grep_count('cursor plane flip') - moves
  if forced:
    record.check(moves == 0 and frames >= 5,
                 f'forced fallback: {frames} frames, {moves} cursor flips')
  else:
    record.check(frames <= 2 and moves >= 5,
                 f'{frames} frames and {moves} cursor plane flips for 10 '
                 'motions')
  record.manual('the pointer follows smoothly with no trail; the hotspot '
                'is the arrow\'s tip; repeat with --env '
                'SHINGLE_CURSOR_PLANE=0 and WLR_NO_HARDWARE_CURSORS=1')

DRIVERS = {
    1: drive_seat,
    2: drive_ssh_refusal,
    3: drive_uinput_events,
    4: drive_hotplug,
    5: drive_configure_device,
    6: drive_vt_switch_input,
    7: drive_signal('TERM'),
    10: drive_visible,
    11: drive_presentation([]),
    12: drive_dpms,
    13: drive_vt_display,
    14: drive_signal('KILL'),
    37: drive_zero_copy,
    38: drive_presentation(['--scanout', 'zero-copy']),
    45: drive_cursor_plane,
    48: drive_tearing,
    49: drive_presentation(['--scanout', 'zero-copy']),
    51: drive_signal('TERM'),
}

def drive(number, session, options, plan_text, host, commit, today=None):
  """Drives step `number`; returns its Record and Markdown."""
  title = step_sections(plan_text).get(number, ('?', ''))[0]
  record = Record(number, title)
  if number != 2 and not session.running():
    raise DriveError('Shingle is not running from shingle-tty1 on the test '
                     f'machine; start it on tty1 with shingle-tty1 --step '
                     f'{number}')
  run = session.read(f'{RUN}/run.json')
  if number != 2 and run:
    started = json.loads(run).get('step')
    if started not in (None, number):
      record.info(f'Shingle was started for step {started}, not {number}')
  summary = summary_of(session) if number != 2 else bringup_logs.scan([])
  try:
    DRIVERS[number](session, record, options)
  except DriveError as error:
    record.check(False, f'stopped: {error}')
  today = today or datetime.date.today().isoformat()
  return record, record.markdown(today, commit, host, facts_line(summary))
