#!/usr/bin/env python3
"""Drives the native test machine's bring-up from the development host.

Usage:
  tools/native_test/bringup.py report HOST
  tools/native_test/bringup.py check HOST
  tools/native_test/bringup.py deploy HOST [--build-on-target] [--jobs N]
  tools/native_test/bringup.py collect HOST [--scanout zero-copy|copy]
      [--tearing] [--vrr] [--output NAME]
  tools/native_test/bringup.py step N [HOST] [--stop] [--signal SIG]
      [--frames N] [--scanout zero-copy|copy]

HOST is what `ssh HOST` reaches: a name from ~/.ssh/config or user@host.
Nothing here stores a credential; ssh runs with BatchMode, so the key must
be in the agent (`--ssh-option=-oOPTION=VALUE` passes more options to ssh
and scp).

- report: the hardware notes of docs/laptop-bringup.md step 2 (lspci,
  connectors, /dev/dri, input devices, drm_info per card, kernel, Mesa
  and driver versions, logind or seatd) as target/bringup/HOST/report.md,
  with the zero-copy path and cursor plane the GPU family should get.
- check: step 0 of docs/native-testing.md, with the exact command for
  everything missing. It runs no sudo (only `sudo -n -l`, which lists).
- deploy: builds shingle and present_probe here in release and copies
  them, the test tools and the tty1 start script to ~/shingle-bringup if
  the target's glibc and libraries fit, else pushes the commit to
  ~/shingle there with git and builds there.
- collect: copies native.log and the last run's files to
  target/bringup/HOST/runs/, runs present_check.py on every probe output
  and the log checks of bringup_logs.py, and writes summary.md.
- step N: prints native-testing.md's step N and the tty1 command that
  starts Shingle for it; with HOST, drives what SSH can drive while
  Shingle runs on tty1 and appends the result to
  target/bringup/HOST/results.md in the Results format.

Every subcommand refuses localhost and this host (its names, addresses,
/etc/machine-id and hostname); deploy and the driven steps also need
/etc/shingle-native-test-machine on the target.
"""

import argparse
import datetime
import json
import os
import re
import sys
import time

import bringup_logs
import bringup_report
import bringup_setup
import bringup_steps
import remote

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, '..', '..'))

def parse_arguments(argv):
  """Parses `argv` (without the program name)."""
  parser = argparse.ArgumentParser(
      prog='bringup.py', description='Native test machine bring-up.')
  parser.add_argument('--ssh-option', action='append', default=[],
                      help='an extra option for ssh and scp, given as '
                      '--ssh-option=-p2222')
  parser.add_argument('--output-dir', default=os.path.join(
      REPO, 'target', 'bringup'))
  commands = parser.add_subparsers(dest='command', required=True)
  for name in ('report', 'check'):
    commands.add_parser(name).add_argument('host')
  deploy = commands.add_parser('deploy')
  deploy.add_argument('host')
  deploy.add_argument('--build-on-target', action='store_true')
  deploy.add_argument('--jobs', type=int, default=2)
  collect = commands.add_parser('collect')
  collect.add_argument('host')
  collect.add_argument('--scanout', choices=('zero-copy', 'copy'))
  collect.add_argument('--tearing', action='store_true')
  collect.add_argument('--vrr', action='store_true')
  collect.add_argument('--output')
  step = commands.add_parser('step')
  step.add_argument('number', type=int)
  step.add_argument('host', nargs='?')
  step.add_argument('--stop', action='store_true',
                    help='end Shingle with SIGTERM where the step says to')
  step.add_argument('--signal', choices=bringup_steps.SIGNALS)
  step.add_argument('--frames', type=int)
  step.add_argument('--scanout', choices=('zero-copy', 'copy'))
  return parser.parse_args(argv)

def safe_name(host):
  """`host` as a directory name."""
  return re.sub(r'[^A-Za-z0-9._-]', '_', host)

class Tool:
  """bringup.py with its processes, host identity, clock and output
  injected, so the unit tests run it on a fake SSH runner."""

  def __init__(self, runner=remote.run_local, local_host=None,
               sleep=time.sleep, now=None, out=print, load=None):
    self.runner = runner
    self.local_host = local_host
    self.sleep = sleep
    self.now = now or datetime.datetime.now
    self.out = out
    self.load = load

  def local(self, argv, timeout=600, cwd=None):
    """Runs a local command."""
    return self.runner(argv, timeout=timeout, cwd=cwd)

  def target(self, arguments, require_marker):
    """The guarded target of `arguments`; returns (target, info)."""
    target = remote.SshTarget(arguments.host, self.runner,
                              arguments.ssh_option)
    local_host = self.local_host or remote.this_host()
    return target, remote.guard(target, local_host, require_marker,
                                self.runner)

  def host_dir(self, arguments, *parts):
    """target/bringup/HOST/PARTS..., created."""
    path = os.path.join(arguments.output_dir, safe_name(arguments.host),
                        *parts)
    os.makedirs(path, exist_ok=True)
    return path

  def present_check(self, args):
    """Runs present_check.py here."""
    return self.local([sys.executable, os.path.join(HERE, 'present_check.py'),
                       *args], timeout=120)

  def report(self, arguments):
    """The `report` subcommand."""
    target, info = self.target(arguments, require_marker=False)
    results = bringup_report.collect(target)
    when = self.now().isoformat(timespec='seconds')
    text = bringup_report.render(arguments.host, info, results, when)
    directory = self.host_dir(arguments)
    with open(os.path.join(directory, 'report.md'), 'w',
              encoding='utf-8') as file:
      file.write(text)
    with open(os.path.join(directory, 'drm_info.json'), 'w',
              encoding='utf-8') as file:
      file.write(results['drm_info'].stdout)
    expected = text.split('## Expected paths', 1)[1].split('\n## ', 1)[0]
    self.out(expected.strip())
    self.out(f'report written to {os.path.join(directory, "report.md")}')
    return 0

  def check(self, arguments):
    """The `check` subcommand."""
    target, info = self.target(arguments, require_marker=False)
    text, status = bringup_setup.run_check(target, info)
    self.out(text.replace('HOST', arguments.host))
    return status

  def deploy(self, arguments):
    """The `deploy` subcommand."""
    target, _ = self.target(arguments, require_marker=True)
    try:
      record = bringup_setup.deploy(target, self.local, REPO, arguments,
                                    load=self.load, now=self.now(),
                                    out=self.out)
    except bringup_setup.DeployError as error:
      self.out(f'bringup.py: deploy failed: {error}')
      return 1
    self.out(f'commit {record["commit"]} built on {record["built_on"]}')
    return 0

  def collect(self, arguments):
    """The `collect` subcommand."""
    target, _ = self.target(arguments, require_marker=False)
    session = bringup_steps.Session(target, self.sleep)
    run = json.loads(session.read(f'{bringup_steps.RUN}/run.json') or '{}')
    stamp = (run.get('started') or self.now().isoformat(
        timespec='seconds')).replace(':', '')
    directory = self.host_dir(arguments, 'runs', stamp)
    names = target.run(f'ls -1 {bringup_steps.RUN} 2>/dev/null').stdout
    for name in names.split():
      target.get(f'{bringup_steps.RUN}/{name}', os.path.join(directory, name))
    log = os.path.join(directory, 'native.log')
    if not target.get(bringup_steps.LOG, log).ok:
      self.out('no native.log on the target')
      return 1
    lines = [f'# Run {stamp} on {arguments.host}', '']
    if run:
      lines += [f'Started for step {run.get("step")} with commit '
                f'{run.get("commit")}: `{" ".join(run.get("argv", []))}`', '']
    with open(log, encoding='utf-8', errors='replace') as file:
      log_lines = file.readlines()
    stderr_path = os.path.join(directory, 'stderr')
    stderr = []
    if os.path.exists(stderr_path):
      with open(stderr_path, encoding='utf-8', errors='replace') as file:
        stderr = file.readlines()
    summary = bringup_logs.scan(log_lines, stderr)
    lines += ['## Log checks', ''] + bringup_logs.describe(summary) + ['']
    exit_path = os.path.join(directory, 'exit')
    if os.path.exists(exit_path):
      with open(exit_path, encoding='utf-8') as file:
        lines += [f'Exit: `{file.read().strip()}`', '']
    status = 1 if summary['failures'] else 0
    probes = sorted(n for n in os.listdir(directory)
                    if n.startswith('probe') and n.endswith('.out'))
    tearing = arguments.tearing or '--tearing' in ' '.join(run.get('argv',
                                                                   []))
    for probe in probes:
      flags = []
      if arguments.scanout:
        flags += ['--scanout', arguments.scanout]
      if tearing:
        flags.append('--tearing')
      if arguments.vrr:
        flags.append('--vrr')
      if arguments.output:
        flags += ['--output', arguments.output]
      result = self.present_check([*flags, log, os.path.join(directory,
                                                             probe)])
      status = status or (1 if result.returncode else 0)
      lines += [f'## present_check.py {" ".join(flags)} {probe}', '',
                f'Status {result.returncode}.', '',
                bringup_report.fence(result.stdout + result.stderr)]
    text = '\n'.join(lines)
    with open(os.path.join(directory, 'summary.md'), 'w',
              encoding='utf-8') as file:
      file.write(text)
    self.out(text)
    self.out(f'collected into {directory}')
    return status

  def step(self, arguments):
    """The `step` subcommand."""
    with open(bringup_steps.plan_path(), encoding='utf-8') as file:
      plan = file.read()
    try:
      text = bringup_steps.instructions(arguments.number, plan,
                                        arguments.host)
    except KeyError as error:
      self.out(f'bringup.py: {error.args[0]}')
      return 2
    if not arguments.host or arguments.number not in bringup_steps.DRIVERS:
      self.out(text)
      return 0
    target, _ = self.target(arguments, require_marker=True)
    stamp = self.now().strftime('%Y%m%d-%H%M%S')
    session = bringup_steps.Session(
        target, self.sleep,
        self.host_dir(arguments, f'step{arguments.number}-{stamp}'))
    deployed = json.loads(session.read(f'{bringup_steps.BASE}/DEPLOYED.json')
                          or '{}')
    commit = (deployed.get('commit') or 'unknown')[:12]
    if deployed.get('dirty'):
      commit += ' (dirty)'
    arguments.local_check = self.present_check
    try:
      record, markdown = bringup_steps.drive(
          arguments.number, session, arguments, plan, arguments.host, commit,
          self.now().date().isoformat())
    except bringup_steps.DriveError as error:
      self.out(f'bringup.py: {error}')
      return 1
    with open(os.path.join(self.host_dir(arguments), 'results.md'), 'a',
              encoding='utf-8') as file:
      file.write(markdown + '\n')
    self.out(markdown)
    return 0 if record.passed else 1

  def main(self, argv):
    """Runs the command line `argv`; returns the exit status."""
    arguments = parse_arguments(argv)
    try:
      return getattr(self, arguments.command)(arguments)
    except remote.Refused as error:
      self.out(f'bringup.py: {error}')
      return 3

if __name__ == '__main__':
  sys.exit(Tool().main(sys.argv[1:]))
