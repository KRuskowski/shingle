"""Unit tests of bringup.py through a fake SSH runner.

No test connects anywhere: every ssh and scp goes to fake_machine's
FakeMachine, and the only real processes are present_check.py runs on
files in a temporary directory.
"""

import argparse
import contextlib
import io
import json
import os
import tempfile
import unittest

import bringup
import bringup_logs
import bringup_report
import bringup_setup
import bringup_steps
import fake_machine
import remote

LOCAL = remote.LocalHost('devbox', 'a' * 32, frozenset({'10.0.0.5',
                                                        '127.0.1.1'}))

def tool(machine, directory, **kwargs):
  """A Tool on `machine` that writes under `directory`."""
  lines = []
  instance = bringup.Tool(
      runner=fake_machine.copy_runner(machine, remote.run_local),
      local_host=LOCAL, sleep=machine.sleep, out=lines.append, **kwargs)
  return instance, lines

def run_tool(machine, directory, argv, **kwargs):
  """Runs bringup.py `argv` on `machine`; returns (status, output)."""
  instance, lines = tool(machine, directory, **kwargs)
  status = instance.main(['--output-dir', directory, *argv])
  return status, '\n'.join(lines)

class SshTargetTest(unittest.TestCase):
  """The argv of ssh and scp."""

  def test_run_wraps_the_script_in_sh(self):
    calls = []
    target = remote.SshTarget('laptop', lambda argv, **_: calls.append(
        argv) or remote.Result(0), ['-p', '2222'])
    target.run("echo 'a b'")
    self.assertEqual(calls[0], [
        'ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '-p',
        '2222', 'laptop', '--', 'sh -c \'echo \'"\'"\'a b\'"\'"\'\''])

  def test_copies_use_scp_with_the_same_options(self):
    calls = []
    target = remote.SshTarget('laptop', lambda argv, **_: calls.append(
        argv) or remote.Result(0))
    target.put('/x/shingle', 'shingle-bringup/bin/shingle.new')
    target.get('.local/state/shingle/native.log', '/y/native.log')
    self.assertEqual(calls[0][-2:], ['/x/shingle',
                                     'laptop:shingle-bringup/bin/shingle.new'])
    self.assertEqual(calls[1][-2:], ['laptop:.local/state/shingle/native.log',
                                     '/y/native.log'])
    self.assertIn('BatchMode=yes', calls[0])

class GuardTest(unittest.TestCase):
  """The refusal of this host."""

  def test_local_names_are_refused(self):
    for host in ('localhost', 'karl@localhost', '127.0.0.1', '::1',
                 '[::1]', '127.0.1.1', 'devbox', 'DEVBOX.lan', '10.0.0.5',
                 '0.0.0.0'):
      self.assertIsNotNone(remote.local_name_problem(host, host, LOCAL), host)
    self.assertIsNone(remote.local_name_problem('laptop', 'laptop.lan',
                                                LOCAL))

  def test_an_alias_resolving_here_is_refused_before_connecting(self):
    machine = fake_machine.FakeMachine(resolves_to='127.0.0.1')
    target = remote.SshTarget('laptop', machine.runner)
    with self.assertRaisesRegex(remote.Refused, 'loopback'):
      remote.guard(target, LOCAL, False, machine.runner)
    self.assertEqual(machine.scripts, [])

  def test_this_hosts_machine_id_is_refused(self):
    machine = fake_machine.FakeMachine(machine_id=LOCAL.machine_id)
    target = remote.SshTarget('laptop', machine.runner)
    with self.assertRaisesRegex(remote.Refused, 'machine-id'):
      remote.guard(target, LOCAL, False, machine.runner)

  def test_this_hosts_hostname_is_refused(self):
    machine = fake_machine.FakeMachine(hostname='devbox')
    target = remote.SshTarget('laptop', machine.runner)
    with self.assertRaisesRegex(remote.Refused, 'hostname'):
      remote.guard(target, LOCAL, False, machine.runner)

  def test_device_steps_need_the_marker(self):
    machine = fake_machine.FakeMachine(marker=False)
    target = remote.SshTarget('laptop', machine.runner)
    with self.assertRaisesRegex(remote.Refused, 'shingle-native-test'):
      remote.guard(target, LOCAL, True, machine.runner)
    info = remote.guard(target, LOCAL, False, machine.runner)
    self.assertEqual((info.hostname, info.user, info.marker),
                     ('laptop', 'karl', False))

  def test_the_real_development_host_is_refused(self):
    # A target that answers with this very host's identity, as an SSH
    # alias pointing back here would.
    here = remote.this_host()
    if not here.machine_id:
      self.skipTest('no /etc/machine-id here')
    machine = fake_machine.FakeMachine(hostname='elsewhere',
                                       machine_id=here.machine_id)
    target = remote.SshTarget('laptop', machine.runner)
    with self.assertRaises(remote.Refused):
      remote.guard(target, here, True, machine.runner)

  def test_unreachable_targets_are_refused(self):
    def runner(argv, **_):
      if argv[:2] == ['ssh', '-G']:
        return remote.Result(0, 'hostname laptop\n')
      return remote.Result(255, '', 'agent refused operation')
    with self.assertRaisesRegex(remote.Refused, 'agent refused'):
      remote.guard(remote.SshTarget('laptop', runner), LOCAL, False, runner)

  def test_every_subcommand_refuses_this_host(self):
    with tempfile.TemporaryDirectory() as directory:
      for argv in (['report', 'localhost'], ['check', 'devbox'],
                   ['deploy', '127.0.0.1'], ['collect', '::1'],
                   ['step', '7', 'localhost']):
        machine = fake_machine.FakeMachine()
        status, output = run_tool(machine, directory, argv)
        self.assertEqual(status, 3, argv)
        self.assertIn('refusing', output)
        self.assertEqual(machine.scripts, [], argv)

  def test_device_subcommands_refuse_an_unmarked_target(self):
    with tempfile.TemporaryDirectory() as directory:
      for argv in (['deploy', 'laptop'], ['step', '7', 'laptop']):
        machine = fake_machine.FakeMachine(marker=False)
        status, output = run_tool(machine, directory, argv)
        self.assertEqual(status, 3, argv)
        self.assertIn('does not exist there', output)
        self.assertEqual(machine.signalled, [])
        self.assertEqual(machine.puts, [])

class ReportTest(unittest.TestCase):
  """The hardware report."""

  DRM = {
      'driver': {'name': 'i915', 'caps': {'CURSOR_WIDTH': 256,
                                          'CURSOR_HEIGHT': 256,
                                          'ATOMIC_ASYNC_PAGE_FLIP': 1}},
      'crtcs': [{}, {}, {}],
      'connectors': [
          {'name': 'eDP-1', 'status': 1,
           'properties': {'vrr_capable': {'value': 1}}},
          {'name': 'HDMI-A-1', 'status': 2, 'properties': {}}],
      'planes': [
          {'properties': {'type': {'value': 1}, 'IN_FORMATS': {
              'value': 40, 'data': [
                  {'modifier': 0, 'formats': [0x34325258]},
                  {'modifier': 0x0100000000000002,
                   'formats': [0x34325258, 0x34325241]},
                  {'modifier': 0x0100000000000006, 'formats': [0x30334241]},
              ]}}},
          {'properties': {'type': {'value': 2}}},
          {'properties': {'type': {'value': 0}}},
      ],
  }

  def test_cards_and_the_boot_vga_one(self):
    cards = bringup_report.parse_cards(
        'card0 driver=nvidia boot_vga=0\ncard1 driver=i915 boot_vga=1\n')
    self.assertEqual(bringup_report.scanout_card(cards)['card'], 'card1')
    self.assertIsNone(bringup_report.scanout_card([]))

  def test_drm_info_is_summarised(self):
    text = '== /dev/dri/card1\n' + json.dumps({'/dev/dri/card1': self.DRM})
    cards = bringup_report.parse_drm_info(text + '\n== /dev/dri/card2\nbad')
    self.assertIsNone(cards['/dev/dri/card2'])
    summary = bringup_report.summarise_card(cards['/dev/dri/card1'])
    self.assertEqual(summary['xr24_modifiers'], [0, 0x0100000000000002])
    self.assertEqual((summary['cursor_planes'], summary['crtcs']), (1, 3))
    self.assertEqual(summary['cursor_size'], (256, 256))
    self.assertEqual(summary['vrr_capable'], ['eDP-1'])
    self.assertEqual(summary['connected'], ['eDP-1'])
    zero, plane = bringup_report.expectation('i915', summary)
    self.assertIn('INTEL 0x0100000000000002', zero)
    self.assertIn('1 cursor planes for 3 CRTCs, 256x256', plane)

  def test_family_defaults_without_drm_info(self):
    zero, plane = bringup_report.expectation('amdgpu')
    self.assertIn('AMD GFX9+', zero)
    self.assertIn('64x64', plane)
    zero, _ = bringup_report.expectation('evdi')
    self.assertIn('unknown', zero)
    linear = dict(self.DRM, planes=[{'properties': {
        'type': {'value': 1},
        'IN_FORMATS': {'data': [{'modifier': 0, 'formats': [0x34325258]}]}}}])
    zero, plane = bringup_report.expectation(
        'simpledrm', bringup_report.summarise_card(linear))
    self.assertIn('only LINEAR', zero)
    self.assertIn('no cursor plane', plane)

  def test_report_subcommand_writes_markdown(self):
    machine = fake_machine.FakeMachine(running=False)
    outputs = {
        'cards': 'card1 driver=i915 boot_vga=1\n',
        'drm_info': '== /dev/dri/card1\n' + json.dumps(
            {'/dev/dri/card1': self.DRM}),
    }

    def answer(match):
      for key, _, command in bringup_report.PROBES:
        if command == match.group(0):
          return remote.Result(0, outputs.get(key, f'{key} output\n'))
      return remote.Result(0, outputs['drm_info'])
    machine.handlers.append((r'^.*$', answer))
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['report', 'laptop'])
      self.assertEqual(status, 0)
      with open(os.path.join(directory, 'laptop', 'report.md'),
                encoding='utf-8') as file:
        report = file.read()
      self.assertTrue(os.path.exists(os.path.join(directory, 'laptop',
                                                  'drm_info.json')))
    self.assertIn('/dev/dri/card1, i915 (boot VGA', report)
    self.assertIn('Atomic async page flips (step 48): yes', report)
    self.assertIn('## libinput list-devices', report)
    self.assertIn('Zero-copy scanout (step 37)', output)
    for script in machine.scripts:
      self.assertNotIn('sudo', script)

class LogsTest(unittest.TestCase):
  """The native.log checks."""

  def test_scan(self):
    log = [
        'shingle: seat seat0 enabled (seatd backend)',
        'shingle: input device event3 (Keyboard) added as seat devices 1',
        'shingle: using /dev/dri/card0',
        'shingle: rendering on AMD Radeon 780M (RADV PHOENIX)',
        'shingle: output eDP-1: 2880x1800 at 120.000 Hz',
        'shingle: output eDP-1: copy scanout, because SHINGLE_SCANOUT=copy '
        'forces copy scanout',
        'shingle: output eDP-1: the cursor is drawn, because colours are '
        'inverted',
        'shingle: eDP-1: page flip seq=1 at 1.000000000 s',
        'shingle: eDP-1: cursor plane flip',
        'shingle: the page flip of eDP-1 failed: EINVAL',
        'shingle: seat disabled; paused',
        'shingle: seat enabled; resumed',
        'shingle: native backend stopped',
    ]
    summary = bringup_logs.scan(log, ['shingle: using the native backend'])
    self.assertEqual((summary['backend'], summary['seat'],
                      summary['libseat'], summary['card']),
                     ('native', 'seat0', 'seatd', '/dev/dri/card0'))
    self.assertEqual(summary['outputs'], {'eDP-1': (2880, 1800, 120.0)})
    self.assertTrue(summary['scanout']['eDP-1'].startswith('copy scanout'))
    self.assertTrue(summary['cursor']['eDP-1'].startswith('is drawn'))
    self.assertEqual((summary['flips'], summary['cursor_flips']), (1, 1))
    self.assertEqual((summary['paused'], summary['resumed']), (1, 1))
    self.assertTrue(summary['stopped'])
    self.assertEqual(summary['failures'],
                     ['shingle: the page flip of eDP-1 failed: EINVAL'])
    text = '\n'.join(bringup_logs.describe(summary))
    self.assertIn('FAIL: 1 failure lines', text)
    self.assertIn('Output eDP-1: 2880x1800 at 120.0 Hz', text)

  def test_panics_are_failures(self):
    summary = bringup_logs.scan(
        ["shingle: panicked at crates/x.rs:1:2:", 'shingle: no seat: x'])
    self.assertEqual(len(summary['failures']), 2)
    self.assertIn('No failure lines.', bringup_logs.describe(
        bringup_logs.scan([]))[-1])

class CheckTest(unittest.TestCase):
  """check: step 0 and the commands that fix it."""

  def answer(self, packages, groups='karl video input', sudo='0',
             config=bringup_setup.TEST_CONFIG, deployed=True):
    """CHECK_SCRIPT's output for a machine in that state."""
    package_lines = ''.join(f'{p} ii \n' for p in packages)
    version = ('shingle 0.1.0\nstatus=0\n{"commit": "abc"}\n' if deployed
               else 'sh: 1: not found\nstatus=127\n\n')
    path = ('/home/karl/.local/bin/shingle\n'
            '/home/karl/.local/bin/present_probe\n' if deployed else '')
    return (f'{package_lines}--\n/home/karl\nkarl\n{groups}\n--\n{path}--\n'
            f'{version}--\n{config}--\n'
            'ExecStart=/usr/bin/seatd -g video\n--\n'
            f'chvt={sudo}\nfgconsole={sudo}\nuinput={sudo}\n--\n'
            'active\n/home/karl/.cargo/bin/cargo\n--\n')

  def test_a_ready_machine_passes(self):
    everything = (bringup_setup.REQUIRED_PACKAGES +
                  bringup_setup.LATER_PACKAGES + bringup_setup.BUILD_PACKAGES)
    items = bringup_setup.check_items(self.answer(everything), True)
    self.assertTrue(all(item.ok for item in items),
                    [item for item in items if not item.ok])

  def test_missing_items_print_their_commands(self):
    items = bringup_setup.check_items(
        self.answer(['seatd'], groups='karl', sudo='1', config='',
                    deployed=False), False)
    text = bringup_setup.format_items('laptop', items)
    self.assertIn('sudo apt install wev python3-evdev', text)
    self.assertIn('sudo touch /etc/shingle-native-test-machine', text)
    self.assertIn('tools/native_test/bringup.py deploy HOST', text)
    self.assertIn("printf 'session: {restore: false}", text)
    self.assertIn('sudo usermod -aG video,input karl', text)
    self.assertIn('NOPASSWD: /usr/bin/chvt, /usr/bin/fgconsole, '
                  '/usr/bin/python3 /home/karl/shingle-bringup/tools/'
                  'native_test/uinput.py *', text)
    self.assertIn('sudo tee /etc/sudoers.d/shingle-bringup', text)
    self.assertIn('required items missing', text)

  def test_seatd_group_and_packages(self):
    self.assertEqual(bringup_setup.seatd_group(
        'ExecStart=/usr/bin/seatd -g _seatd'), '_seatd')
    self.assertEqual(bringup_setup.seatd_group(''), 'video')
    self.assertEqual(bringup_setup.installed_packages(
        'a ii \nb un \nc:amd64 ii \n'), {'a', 'c'})

  def test_check_subcommand_never_runs_sudo(self):
    machine = fake_machine.FakeMachine(marker=False)
    machine.handlers.append((r'^dpkg-query', lambda m: remote.Result(
        0, self.answer(['seatd'], sudo='1', deployed=False))))
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['check', 'laptop'])
    self.assertEqual(status, 1)
    self.assertIn('bringup.py deploy laptop', output)
    for script in machine.scripts:
      for part in script.split('sudo ')[1:]:
        self.assertTrue(part.startswith('-n -l '), part[:40])

class DeployTest(unittest.TestCase):
  """deploy: here when compatible, else on the target."""

  READELF = (' 0x1 (NEEDED) Shared library: [libinput.so.10]\n'
             ' 0x1 (NEEDED) Shared library: [libc.so.6]\n')
  LDCONFIG = ('2 libs found in cache\n'
              '\tlibinput.so.10 (libc6,x86-64) => /lib/libinput.so.10\n'
              '\tlibc.so.6 (libc6,x86-64) => /lib/libc.so.6\n')

  def machine(self, glibc='2.41', dirty=False):
    """A target with that glibc, and local answers for the build."""
    local = {
        r'^git .* rev-parse': remote.Result(0, 'abcdef1234567890\n'),
        r'^git .* status': remote.Result(0, ' M x\n' if dirty else ''),
        r'^objdump': remote.Result(0, 'GLIBC_2.34 GLIBC_2.39 GLIBC_2.2.5'),
        r'^readelf': remote.Result(0, self.READELF),
        r'^uname': remote.Result(0, 'x86_64\n'),
    }
    machine = fake_machine.FakeMachine(running=False, local=local)
    machine.handlers += [
        (r'^uname -m; getconf', lambda m: remote.Result(
            0, f'x86_64\nglibc {glibc}\n--\n{self.LDCONFIG}')),
        (r'^(mkdir|\[ -d shingle|set -e)',
         lambda m: remote.Result(0, 'shingle 0.1.0\n')),
    ]
    return machine

  def test_compatibility(self):
    needs = {'glibc': (2, 39), 'libraries': ['libinput.so.10']}
    ok = bringup_setup.compatibility(needs, 'x86_64', (2, 41),
                                     {'libinput.so.10'}, 'x86_64')
    self.assertTrue(ok.ok)
    bad = bringup_setup.compatibility(needs, 'aarch64', (2, 36), set(),
                                      'x86_64')
    self.assertEqual(len(bad.reasons), 3)
    self.assertEqual(bringup_setup.glibc_needed('GLIBC_2.3 GLIBC_2.17'),
                     (2, 17))
    self.assertEqual(bringup_setup.needed_libraries(self.READELF),
                     ['libinput.so.10', 'libc.so.6'])
    self.assertEqual(bringup_setup.ldconfig_sonames(self.LDCONFIG),
                     {'libinput.so.10', 'libc.so.6'})
    self.assertEqual(bringup_setup.parse_glibc_version('glibc 2.41'),
                     (2, 41))

  def test_compatible_target_gets_the_binaries_built_here(self):
    machine = self.machine()
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['deploy', 'laptop'],
                                load=1.0)
    self.assertEqual(status, 0, output)
    builds = [argv for argv in machine.local_calls if argv[0] == 'cargo']
    self.assertEqual(builds, bringup_setup.cargo_builds(2))
    self.assertIn('--release', builds[0])
    uploaded = [path for _, path in machine.puts]
    self.assertIn('shingle-bringup/bin/shingle.new', uploaded)
    self.assertIn('shingle-bringup/bin/present_probe.new', uploaded)
    self.assertIn('shingle-bringup/tools/native_test/start_tty1.py', uploaded)
    install = [s for s in machine.scripts if s.startswith('set -e')][0]
    self.assertIn('ln -sf "$HOME/shingle-bringup/tools/native_test/'
                  'start_tty1.py" "$HOME/.local/bin/shingle-tty1"', install)
    self.assertIn('"commit": "abcdef1234567890"', install)
    self.assertFalse(any(argv[:2] == ['git', '-C'] and 'push' in argv
                         for argv in machine.local_calls))
    self.assertIn('built on development host', output)

  def test_old_glibc_builds_on_the_target_from_a_pushed_commit(self):
    machine = self.machine(glibc='2.36')
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['deploy', 'laptop'],
                                load=1.0)
    self.assertEqual(status, 0, output)
    self.assertIn('need GLIBC_2.39, the target has 2.36', output)
    push = [argv for argv in machine.local_calls if 'push' in argv][0]
    self.assertIn('laptop:shingle', push)
    self.assertIn('abcdef1234567890:refs/heads/bringup', push)
    self.assertTrue(any('BatchMode=yes' in a for a in push))
    build = [s for s in machine.scripts if 'cargo build' in s][0]
    self.assertIn('git checkout -q --detach abcdef1234567890', build)
    self.assertNotIn('shingle-bringup/bin/shingle.new',
                     [path for _, path in machine.puts])
    self.assertIn('built on test machine', output)

  def test_a_dirty_checkout_is_not_built_on_the_target(self):
    machine = self.machine(dirty=True)
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory,
                                ['deploy', 'laptop', '--build-on-target'],
                                load=1.0)
    self.assertEqual(status, 1)
    self.assertIn('uncommitted changes', output)

  def test_a_loaded_host_does_not_build(self):
    machine = self.machine()
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['deploy', 'laptop'],
                                load=13.5)
    self.assertEqual(status, 1)
    self.assertIn('the load is 13.5', output)
    self.assertFalse(any(argv[0] == 'cargo' for argv in machine.local_calls))

class InstructionsTest(unittest.TestCase):
  """step N without a host."""

  def plan(self):
    """docs/native-testing.md."""
    with open(bringup_steps.plan_path(), encoding='utf-8') as file:
      return file.read()

  def test_every_driver_and_preset_names_a_real_step(self):
    import start_tty1
    sections = bringup_steps.step_sections(self.plan())
    for number in list(bringup_steps.DRIVERS) + list(start_tty1.STEP_PRESETS):
      self.assertIn(number, sections)
    self.assertEqual(sorted(sections), list(range(0, 63)))

  def test_instructions_quote_the_plan(self):
    text = bringup_steps.instructions(11, self.plan(), 'laptop')
    self.assertIn('shingle-tty1 --step 11', text)
    self.assertIn('bringup.py step 11 laptop', text)
    self.assertIn('present_check.py', text)
    prepare = bringup_steps.instructions(0, self.plan())
    self.assertNotIn('shingle-tty1 --step 0', prepare)
    self.assertIn('/etc/shingle-native-test-machine', prepare)
    manual = bringup_steps.instructions(43, self.plan())
    self.assertIn('not driven over SSH', manual)
    with self.assertRaises(KeyError):
      bringup_steps.instructions(99, self.plan())

  def test_step_without_host_only_prints(self):
    machine = fake_machine.FakeMachine()
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['step', '13'])
    self.assertEqual(status, 0)
    self.assertIn('Step 13: a VT switch away and back', output)
    self.assertEqual(machine.scripts, [])

class SignalTest(unittest.TestCase):
  """The checked signal."""

  def test_script_refuses_bad_pids_and_signals(self):
    for pid in (-1, 0, 1, '12', None):
      with self.assertRaises(bringup_steps.DriveError):
        bringup_steps.signal_script(pid, 5, 'TERM')
    with self.assertRaises(bringup_steps.DriveError):
      bringup_steps.signal_script(10, 0, 'TERM')
    with self.assertRaises(bringup_steps.DriveError):
      bringup_steps.signal_script(10, 5, 'HUP')
    script = bringup_steps.signal_script(4242, 777, 'TERM')
    self.assertTrue(script.startswith('pid=4242; start=777; '))
    self.assertIn('= shingle ] || exit 4', script)
    self.assertTrue(script.endswith('kill -TERM "$pid"'))

  def test_stat_start_time_handles_odd_comms(self):
    line = fake_machine.stat_line(7, 99, comm='a ) b')
    self.assertEqual(bringup_steps.stat_start_time(line), 99)

  def test_only_the_recorded_shingle_is_signalled(self):
    machine = fake_machine.FakeMachine()
    machine.files[f'/proc/{fake_machine.PID}/comm'] = 'bash\n'
    session = bringup_steps.Session(remote.SshTarget('laptop',
                                                     machine.runner))
    with self.assertRaisesRegex(bringup_steps.DriveError, 'not shingle'):
      session.signal('TERM')
    machine = fake_machine.FakeMachine()
    machine.files[f'/proc/{fake_machine.PID}/stat'] = fake_machine.stat_line(
        fake_machine.PID, 1)
    session = bringup_steps.Session(remote.SshTarget('laptop',
                                                     machine.runner))
    with self.assertRaisesRegex(bringup_steps.DriveError, 'reused'):
      session.signal('KILL')
    self.assertEqual(machine.signalled, [])

class ParserTest(unittest.TestCase):
  """uinput.py's and wev's output."""

  def test_times_are_matched(self):
    printed = bringup_steps.parse_uinput(
        'KEY_A (30) pressed at 1000.400 ms\nKEY_A (30) released at 1003.9 ms'
        '\nshingle-test keyboard present at 5.0 ms\nnoise\n')
    self.assertEqual(printed[0], ('KEY_A (30) pressed', 1000.4))
    events = bringup_steps.parse_wev([
        '[14:     wl_keyboard] key: serial: 2; time: 1000; key: 30; '
        'state: 1 (pressed)',
        '                      sym: a (97), utf8: \'a\'',
        '[14:     wl_keyboard] key: serial: 3; time: 1010; key: 30; '
        'state: 0 (released)',
        '\x1b[1;34m[12:        wl_touch]\x1b[0m down: serial: 4; time: 5; '
        'surface: 3; id: 0; x, y: 640.000000, 360.000000',
    ])
    self.assertEqual(len(events), 3)
    self.assertEqual((events[2]['x'], events[2]['y']), (640.0, 360.0))
    self.assertEqual(bringup_steps.match_times(printed, events), [
        ('KEY_A (30) pressed', 0), ('KEY_A (30) released', 7)])
    self.assertIsNone(bringup_steps.expected_wev('shingle-test x present'))
    self.assertEqual(bringup_steps.expected_wev('BTN_LEFT (272) pressed'),
                     {'interface': 'wl_pointer', 'event': 'button',
                      'button': 272, 'state': 1})

  def test_png_size(self):
    with tempfile.TemporaryDirectory() as directory:
      path = os.path.join(directory, 'a.png')
      with open(path, 'w', encoding='latin-1') as file:
        file.write(fake_machine.png(1920, 1200))
      self.assertEqual(bringup_steps.png_size(path), (1920, 1200))
      self.assertIsNone(bringup_steps.png_size(os.path.join(directory, 'x')))

def probe_run(frames=600, flags=7):
  """A native.log and probe output of `frames` frames that pass."""
  log = ['shingle: seat seat0 enabled (logind backend)',
         'shingle: using /dev/dri/card1',
         'shingle: rendering on Intel(R) Graphics',
         'shingle: output eDP-1: 1920x1200 at 60.000 Hz']
  probe = []
  for n in range(frames):
    flip = 100_000_000_000 + n * 16_666_666
    log.append(f'shingle: eDP-1: page flip seq={n + 10} at '
               f'{flip // 10**9}.{flip % 10**9:09d} s')
    probe += [
        json.dumps({'event': 'commit', 'n': n,
                    'monotonic_ns': flip - 10_000_000}),
        json.dumps({'event': 'frame', 'n': n, 'time_ms': flip // 10**6}),
        json.dumps({'event': 'presented', 'n': n, 'sec': flip // 10**9,
                    'nsec': flip % 10**9, 'refresh': 16_666_666,
                    'seq': n + 10, 'flags': flags}),
    ]
  return '\n'.join(log) + '\n', '\n'.join(probe).replace(': ', ':').replace(
      ', ', ',') + '\n'

class DriveTest(unittest.TestCase):
  """Driven steps against the fake machine."""

  def drive(self, number, machine=None, **options):
    """Runs `bringup.py step NUMBER laptop`; returns (status, output,
    machine, results.md)."""
    machine = machine or fake_machine.FakeMachine()
    argv = ['step', str(number), 'laptop']
    for key, value in options.items():
      argv += [f'--{key}'] if value is True else [f'--{key}', str(value)]
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, argv)
      path = os.path.join(directory, 'laptop', 'results.md')
      results = ''
      if os.path.exists(path):
        with open(path, encoding='utf-8') as file:
          results = file.read()
    return status, output, machine, results

  def assert_passed(self, status, output):
    """The step passed and printed no FAIL line."""
    self.assertNotIn('- FAIL', output)
    self.assertEqual(status, 0, output)

  def test_seat_step_and_its_stop(self):
    status, output, machine, results = self.drive(1, stop=True)
    self.assert_passed(status, output)
    self.assertIn('Step 1 (a VT login selects native', results)
    self.assertIn('commit abcdef123456', results)
    self.assertIn('libseat logind backend, card /dev/dri/card1', results)
    self.assertIn('PASS: input/list-devices lists 2 devices', output)
    self.assertEqual(machine.signalled, [(fake_machine.PID, 'TERM')])

  def test_ssh_refusal(self):
    machine = fake_machine.FakeMachine(running=False)
    status, output, _, _ = self.drive(2, machine)
    self.assert_passed(status, output)
    self.assertEqual(output.count('PASS'), 8)
    machine = fake_machine.FakeMachine(running=False)
    machine.ssh_refusal_ok = False
    status, output, _, _ = self.drive(2, machine)
    self.assertEqual(status, 1)
    status, output, _, _ = self.drive(2)
    self.assertIn('needs nothing running', output)

  def test_uinput_events_reach_wev_in_time(self):
    status, output, _, _ = self.drive(3)
    self.assert_passed(status, output)
    self.assertIn('PASS: KEY_A (30) pressed: wev time 0 ms', output)
    self.assertIn('PASS: touch up: wev time 0 ms', output)
    self.assertIn('touch down at (960, 600)', output)

  def test_late_wev_events_fail(self):
    machine = fake_machine.FakeMachine()
    wev = machine.wev
    machine.wev = lambda *a: (setattr(machine, 'now_ms',
                                      machine.now_ms + 16), wev(*a))
    status, output, _, _ = self.drive(3, machine)
    self.assertEqual(status, 1)
    self.assertIn('FAIL: KEY_A (30) pressed: wev time 16 ms', output)

  def test_hotplug(self):
    status, output, _, _ = self.drive(4)
    self.assert_passed(status, output)
    self.assertIn('PASS: shingle-test touch has the new id 5', output)

  def test_configure_device(self):
    status, output, _, _ = self.drive(5)
    self.assert_passed(status, output)
    self.assertIn('PASS: disabling released Shift at once', output)

  def test_configure_device_that_keeps_keys_held_fails(self):
    machine = fake_machine.FakeMachine()
    machine.release_held = lambda which: None
    status, output, _, _ = self.drive(5, machine)
    self.assertEqual(status, 1)
    self.assertIn('FAIL: disabling released Shift at once', output)

  def test_vt_switch_input(self):
    status, output, machine, _ = self.drive(6)
    self.assert_passed(status, output)
    self.assertEqual(machine.vt, 1)

  def test_signals(self):
    for number, extra, expected in ((7, {}, 'TERM'), (14, {}, 'KILL'),
                                    (7, {'signal': 'INT'}, 'INT'),
                                    (51, {}, 'TERM')):
      status, output, machine, _ = self.drive(number, **extra)
      self.assert_passed(status, output)
      self.assertEqual(machine.signalled, [(fake_machine.PID, expected)])
      self.assertIn('MANUAL: on tty1', output)
      if expected == 'KILL':
        self.assertIn('exit=137 (expected exit=137)', output)

  def test_a_step_needs_a_running_shingle(self):
    status, output, machine, _ = self.drive(
        7, fake_machine.FakeMachine(running=False))
    self.assertEqual(status, 1)
    self.assertIn('Shingle is not running', output)
    self.assertEqual(machine.signalled, [])

  def test_visible_client(self):
    status, output, _, _ = self.drive(10)
    self.assert_passed(status, output)
    self.assertIn('grim wrote a (1920, 1200) PNG', output)

  def test_presentation_runs_present_check(self):
    machine = fake_machine.FakeMachine()
    log, probe = probe_run()
    machine.files[fake_machine.LOG] = log
    machine.files[f'{fake_machine.RUN}/probe.out'] = probe
    machine.files[f'{fake_machine.RUN}/stderr'] = (
        'shingle: using the native backend\n')
    status, output, _, _ = self.drive(11, machine)
    self.assert_passed(status, output)
    self.assertIn('PASS: 600 of 600 frames presented', output)
    self.assertIn('median frame interval 17 ms', output)
    status, output, _, _ = self.drive(38, machine)
    self.assertEqual(status, 1)
    self.assertIn('not presented as zero-copy', output)

  def test_dpms(self):
    machine = fake_machine.FakeMachine()
    machine.probing = True
    status, output, _, _ = self.drive(12, machine)
    self.assert_passed(status, output)
    self.assertEqual(machine.dpms, 'on')

  def test_vt_switch_display(self):
    machine = fake_machine.FakeMachine()
    machine.probing = True
    status, output, _, _ = self.drive(13, machine)
    self.assert_passed(status, output)

  def test_zero_copy(self):
    status, output, _, _ = self.drive(37)
    self.assert_passed(status, output)
    self.assertIn('PASS: eDP-1: zero-copy scanout, modifier INTEL', output)
    self.assertIn('driver and kernel: i915 6.12.0', output)

  def test_cursor_plane(self):
    status, output, _, _ = self.drive(45)
    self.assert_passed(status, output)
    self.assertIn('0 frames and 10 cursor plane flips', output)
    machine = fake_machine.FakeMachine()
    machine.cursor_plane = False
    status, output, _, _ = self.drive(45, machine)
    self.assertEqual(status, 1)

  def test_tearing(self):
    machine = fake_machine.FakeMachine()
    log, probe = probe_run(flags=6)
    machine.files[fake_machine.LOG] = log
    machine.files[f'{fake_machine.RUN}/probe.out'] = probe
    status, output, _, _ = self.drive(48, machine)
    self.assertIn('present_check.py --tearing: status 0', output)

class CollectTest(unittest.TestCase):
  """collect."""

  def test_collect_copies_and_checks(self):
    machine = fake_machine.FakeMachine()
    log, probe = probe_run(frames=20)
    machine.files[fake_machine.LOG] = log
    machine.files[f'{fake_machine.RUN}/probe.out'] = probe
    machine.files[f'{fake_machine.RUN}/exit'] = 'exit=0\n'
    machine.files[f'{fake_machine.RUN}/run.json'] = json.dumps(
        {'step': 11, 'started': '2026-11-28T10:00:00', 'commit': 'abc',
         'argv': ['shingle', '--spawn', 'present_probe 20']})
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['collect', 'laptop'])
      run_dir = os.path.join(directory, 'laptop', 'runs',
                             '2026-11-28T100000')
      names = sorted(os.listdir(run_dir))
    self.assertEqual(status, 0, output)
    self.assertIn('native.log', names)
    self.assertIn('probe.out', names)
    self.assertIn('summary.md', names)
    self.assertIn('## present_check.py  probe.out', output)
    self.assertIn('"presented": 20', output)
    self.assertIn('Exit: `exit=0`', output)
    self.assertIn('Started for step 11', output)

  def test_collect_flags_failures(self):
    machine = fake_machine.FakeMachine()
    machine.log('panicked at src/x.rs:1:1:')
    with tempfile.TemporaryDirectory() as directory:
      status, output = run_tool(machine, directory, ['collect', 'laptop'])
    self.assertEqual(status, 1)
    self.assertIn('FAIL: 1 failure lines', output)

class OptionsTest(unittest.TestCase):
  """The command line."""

  def test_arguments(self):
    arguments = bringup.parse_arguments(
        ['--ssh-option=-p2222', 'step', '7', 'laptop', '--signal', 'KILL'])
    self.assertEqual((arguments.number, arguments.host, arguments.signal,
                      arguments.ssh_option), (7, 'laptop', 'KILL',
                                              ['-p2222']))
    with contextlib.redirect_stderr(io.StringIO()):
      with self.assertRaises(SystemExit):
        bringup.parse_arguments(['step', '7', 'laptop', '--signal', 'HUP'])
    self.assertEqual(bringup.safe_name('karl@lap top'), 'karl_lap_top')
    self.assertIsInstance(bringup.parse_arguments(['check', 'x']),
                          argparse.Namespace)

if __name__ == '__main__':
  unittest.main()
