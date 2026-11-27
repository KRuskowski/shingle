"""Unit tests of start_tty1.py on a fake home, marker, /proc and Popen.

No test starts Shingle: Popen is replaced by a recorder.
"""

import datetime
import io
import json
import os
import shutil
import tempfile
import unittest

import start_tty1

ENV = {'XDG_RUNTIME_DIR': '/run/user/1000', 'PATH': '/usr/bin:/bin',
       'HOME': '/home/karl', 'XDG_SEAT': 'seat0'}

def stat_line(pid, start):
  """A /proc/PID/stat line of a shingle with start time `start`."""
  return f'{pid} (shingle) S ' + ' '.join(['0'] * 18 + [str(start)])

def write_tree(directory, files):
  """Writes {relative path: text} under `directory`."""
  for path, text in files.items():
    full = os.path.join(directory, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, 'w', encoding='utf-8') as file:
      file.write(text)

class FakeChild:
  """A child with `returncode` that exits as soon as it is waited for,
  taking its /proc entry with it."""

  def __init__(self, pid, returncode, proc):
    self.pid = pid
    self.returncode = returncode
    self.proc = proc

  def wait(self):
    """Ends the child; returns its return code."""
    shutil.rmtree(os.path.join(self.proc, str(self.pid)), ignore_errors=True)
    return self.returncode

class StartTest(unittest.TestCase):
  """start_tty1.start on a temporary home."""

  def setUp(self):
    self.directory = tempfile.TemporaryDirectory()
    self.home = os.path.join(self.directory.name, 'home')
    self.proc = os.path.join(self.directory.name, 'proc')
    self.marker = os.path.join(self.directory.name, 'marker')
    write_tree(self.home, {
        '.config/shingle/test.yaml': 'session: {restore: false}\n',
        '.config/shingle/nb10.yaml': 'session: {restore: false}\n',
        'shingle-bringup/DEPLOYED.json': '{"commit": "abc123"}'})
    write_tree(self.proc, {
        '4242/stat': stat_line(4242, 777) + '\n',
        '4242/comm': 'shingle\n'})
    with open(self.marker, 'w', encoding='utf-8'):
      pass
    self.launched = []

  def tearDown(self):
    self.directory.cleanup()

  def popen(self, returncode=0):
    """A Popen that records its call and returns a finished child."""
    def popen(argv, stdout, stderr, env, cwd):
      self.launched.append({'argv': argv, 'env': env, 'cwd': cwd,
                            'stdout': stdout.name, 'stderr': stderr.name})
      write_tree(self.proc, {
          '4242/stat': stat_line(4242, 777) + '\n',
          '4242/comm': 'shingle\n'})
      return FakeChild(4242, returncode, self.proc)
    return popen

  def start(self, argv, env=None, tty='/dev/tty1', returncode=0):
    """Runs start() with the fakes; returns (status, printed)."""
    out = io.StringIO()
    status = start_tty1.start(
        argv, dict(ENV if env is None else env), self.home,
        popen=self.popen(returncode),
        now=datetime.datetime(2026, 11, 28, 10, 0, 0), marker=self.marker,
        tty=tty, proc=self.proc, out=out)
    return status, out.getvalue()

  def current(self, name):
    """The text of a file of the current run."""
    path = os.path.join(start_tty1.run_paths(self.home)['current'], name)
    with open(path, encoding='utf-8') as file:
      return file.read()

  def test_a_step_starts_native_shingle_with_its_preset(self):
    status, printed = self.start(['--step', '48'])
    self.assertEqual(status, 0, printed)
    argv = self.launched[0]['argv']
    current = start_tty1.run_paths(self.home)['current']
    self.assertEqual(argv[:6], [
        'shingle', '--backend', 'native', '--config',
        os.path.join(self.home, '.config/shingle/nb10.yaml'), '--ipc'])
    self.assertEqual(argv[6], '/run/user/1000/shingle-ipc')
    self.assertEqual(argv[7:], [
        '--spawn', 'sh', '-c',
        f'exec present_probe --tearing 600 > {current}/probe.out'])
    env = self.launched[0]['env']
    self.assertEqual(env['XDG_STATE_HOME'],
                     os.path.join(self.home, '.local', 'state'))
    self.assertEqual(env['SHINGLE_TRACE_FLIPS'], '1')
    self.assertNotIn('SHINGLE_TRACE_BUFFERS', env)
    self.assertTrue(env['PATH'].startswith(
        os.path.join(self.home, 'shingle-bringup', 'bin') + ':'))
    self.assertEqual(self.current('pid'), '4242 777\n')
    self.assertEqual(self.current('exit'), 'exit=0\n')
    run = json.loads(self.current('run.json'))
    self.assertEqual((run['step'], run['commit'], run['tty']),
                     (48, 'abc123', '/dev/tty1'))
    self.assertIn('exit=0', printed)

  def test_options_override_the_preset(self):
    status, _ = self.start(['--step', '23', '--client', 'wev', '--env',
                            'SHINGLE_SCANOUT=copy', '--outputs', '2'])
    self.assertEqual(status, 0)
    launched = self.launched[0]
    self.assertEqual(launched['env']['SHINGLE_SCANOUT'], 'copy')
    self.assertEqual(launched['env']['SHINGLE_TRACE_BUFFERS'], '1')
    self.assertIn('--outputs', launched['argv'])
    self.assertIn('exec wev > ', launched['argv'][-1])
    status, _ = self.start(['--trace', '', '--', 'foot', '-e', 'top'])
    self.assertEqual(self.launched[1]['argv'][-4:],
                     ['--spawn', 'foot', '-e', 'top'])
    self.assertNotIn('SHINGLE_TRACE_FLIPS', self.launched[1]['env'])

  def test_signals_give_the_shells_status(self):
    self.start([], returncode=-9)
    self.assertEqual(self.current('exit'), 'exit=137\n')
    self.assertEqual(start_tty1.exit_status(1), 1)
    self.assertEqual(start_tty1.exit_status(-15), 143)

  def test_the_previous_run_and_its_log_are_kept(self):
    paths = start_tty1.run_paths(self.home)
    write_tree(self.home, {
        '.local/state/shingle/native.log': 'old log\n',
        '.local/state/shingle/bringup/current/exit': 'exit=0\n'})
    status, printed = self.start([])
    self.assertEqual(status, 0)
    kept = os.path.join(paths['previous'], '20261128-100000')
    self.assertIn(f'previous run kept in {kept}', printed)
    with open(os.path.join(kept, 'native.log'), encoding='utf-8') as file:
      self.assertEqual(file.read(), 'old log\n')
    self.assertFalse(os.path.exists(paths['log']))
    self.start([])
    self.assertTrue(os.path.isdir(kept + '-1'))

  def test_only_the_newest_runs_are_kept(self):
    paths = start_tty1.run_paths(self.home)
    for n in range(start_tty1.KEEP_PREVIOUS + 3):
      os.makedirs(os.path.join(paths['previous'], f'2026{n:04d}'))
    write_tree(self.home, {'.local/state/shingle/native.log': ''})
    self.start([])
    self.assertEqual(len(os.listdir(paths['previous'])),
                     start_tty1.KEEP_PREVIOUS)

  def test_refusals(self):
    cases = [
        ({'SSH_CONNECTION': '1 2 3 4'}, '/dev/tty1', 'SSH_CONNECTION'),
        ({'WAYLAND_DISPLAY': 'wayland-1'}, '/dev/tty1', 'graphical'),
        ({'DISPLAY': ':0'}, '/dev/tty1', 'graphical'),
        ({}, '/dev/pts/3', 'not a VT'),
        ({}, None, 'not a terminal'),
    ]
    for extra, tty, reason in cases:
      problem = start_tty1.refusal(dict(ENV, **extra), True, tty, None)
      self.assertIn(reason, problem)
    self.assertIn('does not exist', start_tty1.refusal(ENV, False,
                                                       '/dev/tty1', None))
    self.assertIn('XDG_RUNTIME_DIR', start_tty1.refusal(
        {'PATH': '/bin'}, True, '/dev/tty1', None))
    self.assertIn('still runs', start_tty1.refusal(ENV, True, '/dev/tty1',
                                                   4242))
    self.assertIsNone(start_tty1.refusal(ENV, True, '/dev/tty2', None))

  def test_refused_starts_launch_nothing(self):
    os.remove(self.marker)
    status, _ = self.start([])
    self.assertEqual(status, 1)
    with open(self.marker, 'w', encoding='utf-8'):
      pass
    status, _ = self.start([], env=dict(ENV, SSH_TTY='/dev/pts/1'))
    self.assertEqual(status, 1)
    status, _ = self.start(['--config', 'missing.yaml'])
    self.assertEqual(status, 1)
    status, _ = self.start(['--env', 'NOEQUALS'])
    self.assertEqual(status, 2)
    status, _ = self.start(['--dry-run'])
    self.assertEqual(status, 0)
    self.assertEqual(self.launched, [])

  def test_a_running_shingle_is_not_started_twice(self):
    write_tree(self.home, {
        '.local/state/shingle/bringup/current/pid': '4242 777\n'})
    status, _ = self.start([])
    self.assertEqual(status, 1)
    self.assertEqual(self.launched, [])
    current = start_tty1.run_paths(self.home)['current']
    self.assertEqual(start_tty1.last_running_pid(current, self.proc), 4242)
    write_tree(self.home, {
        '.local/state/shingle/bringup/current/pid': '4242 778\n'})
    self.assertIsNone(start_tty1.last_running_pid(current, self.proc))

  def test_this_host_is_refused(self):
    if os.path.exists(start_tty1.MARKER):
      self.skipTest('this is the test machine')
    out = io.StringIO()
    status = start_tty1.start([], dict(ENV), self.home,
                              popen=self.popen(), tty='/dev/tty1',
                              proc=self.proc, out=out)
    self.assertEqual(status, 1)
    self.assertEqual(self.launched, [])

  def test_presets_name_known_clients_and_configs(self):
    for number, preset in start_tty1.STEP_PRESETS.items():
      self.assertIn(preset.get('client', 'foot'), start_tty1.CLIENTS, number)
      start_tty1.settings(start_tty1.parse_arguments(['--step',
                                                      str(number)]))

if __name__ == '__main__':
  unittest.main()
