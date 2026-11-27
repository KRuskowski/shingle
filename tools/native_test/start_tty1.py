#!/usr/bin/env python3
"""Starts native Shingle from the test machine's tty1 console.

ONLY FOR THE DEDICATED TEST MACHINE, typed at its tty1 login, where it
takes the VT, the GPU and the input devices. It refuses to run unless
/etc/shingle-native-test-machine exists, over SSH, inside a graphical
session or on anything but a VT.

Usage:
  start_tty1.py [--step N] [--config NAME] [--client CLIENT] [--frames N]
      [--env VAR=VALUE]... [--trace flips,buffers] [--outputs N]
      [--dry-run] [-- PROGRAM [ARG]...]

--step N takes the config, client and environment that step N of
docs/native-testing.md starts Shingle with (STEP_PRESETS); the other
options override it. CLIENT is wev, foot, probe, probe-tearing, true or
none, or the PROGRAM after `--`. NAME is a file in ~/.config/shingle
(default test.yaml) or a path.

Shingle always runs with `--backend native`, its IPC socket at
$XDG_RUNTIME_DIR/shingle-ipc, XDG_STATE_HOME at ~/.local/state, so the
native backend logs to ~/.local/state/shingle/native.log, and
SHINGLE_TRACE_FLIPS=1 (and SHINGLE_TRACE_BUFFERS=1 with --trace buffers).
Every other file of the run is in ~/.local/state/shingle/bringup/current:
stdout (with WAYLAND_DISPLAY=...), stderr, the client's wev.log or
probe.out, pid (Shingle's PID and its start time, which bringup.py checks
before it signals), run.json and, once Shingle has exited, exit
(`exit=N`, 128 + the signal after one). The previous run, with its
native.log, is moved to bringup/previous/TIMESTAMP first, so no log is
lost. bringup.py drives the steps over SSH and collects these files.
"""

import argparse
import datetime
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys

MARKER = '/etc/shingle-native-test-machine'
KEEP_PREVIOUS = 30
CLIENTS = ('wev', 'foot', 'probe', 'probe-tearing', 'true', 'none')
# The start of each step of docs/native-testing.md that names one; steps
# not listed start with the test config and foot.
STEP_PRESETS = {
    1: {'client': 'wev'},
    3: {'client': 'wev'},
    4: {'client': 'wev'},
    5: {'client': 'wev'},
    6: {'client': 'wev'},
    7: {'client': 'wev'},
    10: {'client': 'foot'},
    11: {'client': 'probe', 'frames': 600},
    12: {'client': 'probe', 'frames': 100000},
    13: {'client': 'probe', 'frames': 100000},
    14: {'client': 'foot'},
    15: {'client': 'foot'},
    **{n: {'config': 'nb9.yaml', 'client': 'foot'} for n in range(17, 23)},
    21: {'config': 'nb9.yaml', 'client': 'probe', 'frames': 100000},
    23: {'client': 'foot', 'trace': ['flips', 'buffers']},
    24: {'client': 'foot', 'trace': ['flips', 'buffers']},
    **{n: {'config': 'mo7-return.yaml', 'client': 'foot'}
       for n in range(26, 36)},
    31: {'config': 'mo7-return.yaml', 'client': 'probe', 'frames': 100000},
    32: {'config': 'mo7-return.yaml', 'client': 'probe', 'frames': 600},
    33: {'config': 'mo7-return.yaml', 'client': 'probe', 'frames': 100000},
    35: {'config': 'mo7-return.yaml', 'client': 'wev'},
    37: {'client': 'foot'},
    38: {'client': 'probe', 'frames': 600},
    39: {'client': 'foot', 'env': ['SHINGLE_SCANOUT=copy']},
    40: {'client': 'probe', 'frames': 100000},
    41: {'client': 'probe', 'frames': 100000},
    43: {'config': 'vswipe.ini', 'client': 'foot'},
    45: {'config': 'nb10.yaml', 'client': 'foot'},
    46: {'config': 'nb10.yaml', 'client': 'foot'},
    47: {'config': 'nb10-vrr.yaml', 'client': 'probe', 'frames': 600},
    48: {'config': 'nb10.yaml', 'client': 'probe-tearing', 'frames': 600},
    49: {'config': 'nb10.yaml', 'client': 'probe', 'frames': 600},
    50: {'config': 'nb10-vrr.yaml', 'client': 'foot'},
    52: {'config': 'devices.ini', 'client': 'foot'},
    53: {'config': 'soak.yaml', 'client': 'true'},
}

def parse_arguments(argv):
  """Parses `argv` (without the program name); a PROGRAM after `--`
  becomes `command`."""
  command = []
  if '--' in argv:
    index = argv.index('--')
    argv, command = argv[:index], argv[index + 1:]
  parser = argparse.ArgumentParser(
      prog='start_tty1.py', description='Starts native Shingle on tty1.')
  parser.add_argument('--step', type=int)
  parser.add_argument('--config')
  parser.add_argument('--client', choices=CLIENTS)
  parser.add_argument('--frames', type=int)
  parser.add_argument('--env', action='append', default=[])
  parser.add_argument('--trace')
  parser.add_argument('--outputs', type=int)
  parser.add_argument('--dry-run', action='store_true')
  arguments = parser.parse_args(argv)
  arguments.command = command
  return arguments

def settings(arguments):
  """Merges the step's preset with the options: returns a dict with
  config, client, frames, env (a list of VAR=VALUE), trace (a list) and
  command."""
  preset = STEP_PRESETS.get(arguments.step, {}) if arguments.step else {}
  merged = {
      'config': preset.get('config', 'test.yaml'),
      'client': preset.get('client', 'foot'),
      'frames': preset.get('frames', 600),
      'env': list(preset.get('env', [])),
      'trace': list(preset.get('trace', ['flips'])),
      'command': list(arguments.command),
      'outputs': arguments.outputs,
  }
  for key in ('config', 'client', 'frames'):
    value = getattr(arguments, key)
    if value is not None:
      merged[key] = value
  merged['env'] += arguments.env
  if arguments.trace is not None:
    merged['trace'] = [t for t in arguments.trace.split(',') if t]
  for variable in merged['env']:
    if '=' not in variable:
      raise ValueError(f'--env {variable} is not VAR=VALUE')
  for trace in merged['trace']:
    if trace not in ('flips', 'buffers'):
      raise ValueError(f'unknown trace {trace}; use flips or buffers')
  return merged

def config_path(name, home):
  """The config file `name` means: a path as given, else a file in
  ~/.config/shingle."""
  if os.sep in name:
    return os.path.expanduser(name)
  return os.path.join(home, '.config', 'shingle', name)

def run_paths(home):
  """The directories and files of the bring-up runs under `home`."""
  state = os.path.join(home, '.local', 'state')
  bringup = os.path.join(state, 'shingle', 'bringup')
  return {
      'state': state,
      'log': os.path.join(state, 'shingle', 'native.log'),
      'current': os.path.join(bringup, 'current'),
      'previous': os.path.join(bringup, 'previous'),
  }

def client_command(merged, current):
  """The `--spawn` arguments for the client, or [] for none."""
  client = merged['client']
  if merged['command']:
    return merged['command']
  if client == 'none':
    return []
  if client == 'wev':
    log = shlex.quote(os.path.join(current, 'wev.log'))
    return ['sh', '-c', f'exec wev > {log} 2>&1']
  if client in ('probe', 'probe-tearing'):
    out = shlex.quote(os.path.join(current, 'probe.out'))
    tearing = '--tearing ' if client == 'probe-tearing' else ''
    return ['sh', '-c',
            f'exec present_probe {tearing}{merged["frames"]} > {out}']
  return [client]

def shingle_command(merged, home, runtime_dir, current):
  """The full `shingle` argv."""
  argv = ['shingle', '--backend', 'native',
          '--config', config_path(merged['config'], home),
          '--ipc', os.path.join(runtime_dir, 'shingle-ipc')]
  if merged['outputs']:
    argv += ['--outputs', str(merged['outputs'])]
  spawn = client_command(merged, current)
  if spawn:
    argv += ['--spawn', *spawn]
  return argv

def shingle_environment(merged, environ, home, bin_dir):
  """Shingle's environment: the login's, with the bring-up variables."""
  env = dict(environ)
  env['XDG_STATE_HOME'] = os.path.join(home, '.local', 'state')
  env['PATH'] = bin_dir + os.pathsep + env.get('PATH', '/usr/bin:/bin')
  if 'flips' in merged['trace']:
    env['SHINGLE_TRACE_FLIPS'] = '1'
  if 'buffers' in merged['trace']:
    env['SHINGLE_TRACE_BUFFERS'] = '1'
  for variable in merged['env']:
    name, _, value = variable.partition('=')
    env[name] = value
  return env

def refusal(environ, marker_exists, tty, running_pid):
  """Why Shingle must not start here, or None."""
  if not marker_exists:
    return (f'{MARKER} does not exist; this is not the native test machine, '
            'so Shingle is not started')
  for variable in ('SSH_CONNECTION', 'SSH_TTY'):
    if environ.get(variable):
      return (f'{variable} is set: run this at the tty1 login of the test '
              'machine, not over SSH')
  for variable in ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'DISPLAY'):
    if environ.get(variable):
      return (f'{variable} is set: this is a graphical session; log in on '
              'tty1 instead')
  if not environ.get('XDG_RUNTIME_DIR'):
    return 'XDG_RUNTIME_DIR is not set; log in on tty1 through logind'
  if not (tty or '').startswith('/dev/tty') or not tty[8:].isdigit():
    return f'standard input is {tty or "not a terminal"}, not a VT'
  if running_pid:
    return (f'Shingle of the last run (PID {running_pid}) still runs; end it '
            'with bringup.py step 7 or kill -TERM first')
  return None

def process_start_time(pid, proc='/proc'):
  """The start time of `pid` in clock ticks (field 22 of its stat), or
  None if it does not exist."""
  try:
    with open(os.path.join(proc, str(pid), 'stat'), encoding='utf-8') as file:
      stat = file.read()
  except OSError:
    return None
  # The comm field may hold spaces and parentheses; the rest follows the
  # last ')'.
  fields = stat[stat.rindex(')') + 2:].split()
  return int(fields[19])

def process_comm(pid, proc='/proc'):
  """The comm of `pid`, or None."""
  try:
    with open(os.path.join(proc, str(pid), 'comm'), encoding='utf-8') as file:
      return file.read().strip()
  except OSError:
    return None

def last_running_pid(current, proc='/proc'):
  """The PID of the last run's Shingle if it still runs, else None."""
  try:
    with open(os.path.join(current, 'pid'), encoding='utf-8') as file:
      pid_text, start_text = file.read().split()[:2]
    pid, start = int(pid_text), int(start_text)
  except (OSError, ValueError):
    return None
  if pid > 1 and process_comm(pid, proc) == 'shingle' and (
      process_start_time(pid, proc) == start):
    return pid
  return None

def archive_previous(paths, now):
  """Moves the last run's files and native.log to previous/TIMESTAMP and
  keeps the newest KEEP_PREVIOUS of them; returns the new directory or
  None if there was nothing to keep."""
  have_run = os.path.isdir(paths['current']) and os.listdir(paths['current'])
  have_log = os.path.exists(paths['log'])
  if not have_run and not have_log:
    return None
  os.makedirs(paths['previous'], exist_ok=True)
  base = os.path.join(paths['previous'], now.strftime('%Y%m%d-%H%M%S'))
  destination, suffix = base, 1
  while os.path.exists(destination):
    destination, suffix = f'{base}-{suffix}', suffix + 1
  if have_run:
    shutil.move(paths['current'], destination)
  else:
    os.makedirs(destination)
  if have_log:
    shutil.move(paths['log'], os.path.join(destination, 'native.log'))
  kept = sorted(os.listdir(paths['previous']))
  for old in kept[:-KEEP_PREVIOUS]:
    shutil.rmtree(os.path.join(paths['previous'], old), ignore_errors=True)
  return destination

def exit_status(returncode):
  """A shell's `$?` for a Popen return code: 128 + N after signal N."""
  return 128 - returncode if returncode < 0 else returncode

def deployed_commit(bin_dir):
  """The commit recorded by bringup.py deploy, or None."""
  path = os.path.join(os.path.dirname(bin_dir), 'DEPLOYED.json')
  try:
    with open(path, encoding='utf-8') as file:
      return json.load(file).get('commit')
  except (OSError, ValueError):
    return None

def start(argv, environ, home, popen=subprocess.Popen, now=None,
          marker=MARKER, tty=None, proc='/proc', out=sys.stdout):
  """Runs the start script; returns the exit status. Every side effect
  goes through the arguments, so the unit tests run it on fakes."""
  try:
    arguments = parse_arguments(argv)
    merged = settings(arguments)
  except ValueError as error:
    print(f'start_tty1.py: {error}', file=sys.stderr)
    return 2
  paths = run_paths(home)
  bin_dir = os.path.join(home, 'shingle-bringup', 'bin')
  runtime_dir = environ.get('XDG_RUNTIME_DIR', '')
  argv_shingle = shingle_command(merged, home, runtime_dir, paths['current'])
  env = shingle_environment(merged, environ, home, bin_dir)
  added = sorted(set(env.items()) - set(environ.items()))
  print('environment: ' + ' '.join(f'{k}={shlex.quote(v)}' for k, v in added),
        file=out)
  print('command: ' + shlex.join(argv_shingle), file=out)
  problem = refusal(environ, os.path.exists(marker), tty,
                    last_running_pid(paths['current'], proc))
  if problem:
    print(f'start_tty1.py: {problem}', file=sys.stderr)
    return 1
  if not os.path.exists(config_path(merged['config'], home)):
    print(f'start_tty1.py: no config {config_path(merged["config"], home)}; '
          'bringup.py check prints the command that writes it',
          file=sys.stderr)
    return 1
  if arguments.dry_run:
    return 0
  now = now or datetime.datetime.now()
  previous = archive_previous(paths, now)
  if previous:
    print(f'previous run kept in {previous}', file=out)
  os.makedirs(paths['current'], exist_ok=True)
  os.makedirs(os.path.dirname(paths['log']), exist_ok=True)
  current = paths['current']
  record = {'step': arguments.step, 'argv': argv_shingle,
            'env': dict(added), 'started': now.isoformat(timespec='seconds'),
            'commit': deployed_commit(bin_dir), 'tty': tty}
  with open(os.path.join(current, 'run.json'), 'w', encoding='utf-8') as file:
    json.dump(record, file, indent=1)
  with open(os.path.join(current, 'stdout'), 'w', encoding='utf-8') as stdout, \
       open(os.path.join(current, 'stderr'), 'w', encoding='utf-8') as stderr:
    child = popen(argv_shingle, stdout=stdout, stderr=stderr, env=env,
                  cwd=home)
    start_time = process_start_time(child.pid, proc)
    with open(os.path.join(current, 'pid'), 'w', encoding='utf-8') as file:
      file.write(f'{child.pid} {start_time}\n')
    # The console's own Ctrl+C must not end the script before Shingle;
    # Shingle holds the VT's keyboard while it runs anyway.
    previous_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
      returncode = child.wait()
    finally:
      signal.signal(signal.SIGINT, previous_handler)
  status = exit_status(returncode)
  with open(os.path.join(current, 'exit'), 'w', encoding='utf-8') as file:
    file.write(f'exit={status}\n')
  print(f'exit={status}; files in {current}', file=out)
  return 0

def main():
  """Runs the start script on the real system."""
  try:
    tty = os.ttyname(0)
  except OSError:
    tty = None
  return start(sys.argv[1:], dict(os.environ), os.path.expanduser('~'),
               tty=tty)

if __name__ == '__main__':
  sys.exit(main())
