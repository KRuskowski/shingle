#!/usr/bin/env python3
"""Runs a command on the GPU in a private namespace with a private /dev.

The command runs inside an unprivileged user, mount, network and PID
namespace (`unshare -U -r -m -n -p -f --mount-proc`), as the XWayland
harness does, but with the GPU: a tmpfs covers /dev, and only the basic
nodes (null, zero, full, random, urandom, a private pts and shm), the
render node /dev/dri/renderD128, NVIDIA's nvidia0, nvidiactl, nvidia-uvm
and nvidia-uvm-tools, and udmabuf are bind-mounted into it. With
`--modeset` nvidia-modeset is added too (docs/design/dmabuf.md,
Decisions 3). No card node, no /dev/input and no TTY exist inside, so no
process can open them. A tmpfs covers /run/user/<uid>, which hides the
host's Wayland, D-Bus and PipeWire sockets even by absolute path; the
network namespace hides abstract sockets; /tmp, HOME, XDG_RUNTIME_DIR,
XDG_CONFIG_HOME, XDG_STATE_HOME and XDG_CACHE_HOME are private, and
WAYLAND_DISPLAY, WAYLAND_SOCKET, DISPLAY, WAYFIRE_SOCKET and
DBUS_SESSION_BUS_ADDRESS are removed from the environment.

While the command runs, the device files every process in the namespace
has open are sampled; the run fails if any is outside the allowed set.

Usage, from the repository root and outside any sandbox that forbids user
namespaces:

  tools/gpu_test/run.py [--modeset] [--keep] [--timeout SECONDS]
      -- COMMAND [ARGUMENT ...]
  tools/gpu_test/run.py [--modeset] [--keep] [--binary target/debug/shingle]
      [--wl-probe target/debug/examples/wl_probe] [--captures DIR]
      --checks offscreen|nested|baseline|buffers|dmabuf|browsers|xwayland|
               nested-gpu|nodmabuf|weston|syncobj|nosyncobj|outputs|
               nested-outputs|nested-smoke|wayfire-plugins|wayfire-shell|
               vswipe|decoration|switcher|colours|minimize-focus|
               drag-icon|resize|resize-nested|nested-scaled|
               nested-clipboard|screencast|image-copy-capture|
               ext-data-control|launcher|animate|animate-cost|lock|
               rehearsal|
               first-frame|straddle|apps|
               app-NAME ... [--wayfire-ini PATH]

For example, DB-1's import test:

  cargo test -p shingle-render --test dmabuf_import --no-run
  tools/gpu_test/run.py -- target/debug/deps/dmabuf_import-<hash>

and DB-2's real-client checks of `shingle --backend offscreen`
(checks.py), after `cargo build -p shingle-bin` and
`cargo build -p shingle --example wl_probe`:

  tools/gpu_test/run.py --checks offscreen nested baseline

DB-3's check that buffers replaced behind a FIFO barrier are released is
`--checks buffers`. DB-4's linux-dmabuf checks (dmabuf_checks.py) are
`--checks dmabuf browsers xwayland nested-gpu nodmabuf weston`; for these
groups nvidia-modeset is exposed as with `--modeset`, because NVIDIA's
Vulkan WSI cannot create a swapchain without it. DB-5's syncobj checks
are `--checks syncobj nosyncobj` (after `cargo build -p shingle --example
syncobj_probe`), and `dmabuf`, `xwayland` and `nested-gpu` check syncobj
use too. MO-5's checks of offscreen Shingle with two outputs at
different refresh rates (output_checks.py) are `--checks outputs`, and
MO-6's checks of nested Shingle with a window per output on lavapipe in
a headless Shingle, and of the nested smoke test with one window
(nested_output_checks.py), are `--checks nested-outputs nested-smoke`.
WG-2's checks under a wayfire.ini (wayfire_checks.py) are `--checks
wayfire-plugins wayfire-shell`, WG-5's touchpad workspace swipes
through `stipc/feed_gesture` (vswipe_checks.py) are `--checks vswipe`,
WG-6's server-side frames, drawn and clicked through STIPC
(decoration_checks.py), are `--checks decoration`, and WG-7's Alt+Tab
thumbnails, through STIPC keys (switcher_checks.py), are `--checks
switcher`. `--checks colours` (colour_checks.py) checks that configured
colours show as the bytes Wayfire writes, and `--checks minimize-focus`
(focus_checks.py) that minimizing the focused view leaves the focus
order as Wayfire leaves it; both compare with headless Wayfire, run in
the namespace as an unprivileged user, when `wayfire` starts there.
WG-10's `--checks drag-icon` (drag_icon_checks.py) drags from a
`wl_probe --drag-icon` window with STIPC's pointer and requires captures
to show the icon following the pointer and gone after the drop.
`--checks resize` (resize_checks.py) drags the boundary between `kitty`
and a `wl_probe` with STIPC's pointer, one motion and eight motions per
frame, and reports kitty's configures, buffers and settle time from its
`WAYLAND_DEBUG` log and the configures of a floating probe beside them;
`--checks resize-nested` does the same in nested Shingle on NVIDIA
inside offscreen Shingle.
`--checks nested-scaled` (nested_output_checks.py) runs nested Shingle
in a headless Shingle whose output has scale 1.5 and then 2: nested's
output must take the window's scale and logical size, the host pointer
must land at the same logical point in nested, and a Super+drag through
the headless Shingle must move the window it starts on.
`--checks nested-clipboard` (nested_clipboard_checks.py) copies with
`wl-copy` and pastes with `wl-paste` between a headless Shingle and
nested Shingle in it, both ways, for the clipboard and the primary
selection at 10 bytes and 1 MiB, with a slow reader that must not
stall either compositor, and checks `disable_primary_selection` and
`behavior.host_clipboard: false`.
`--checks screencast` (screencast_checks.py) runs a private D-Bus
session bus, PipeWire, WirePlumber, xdg-desktop-portal and
xdg-desktop-portal-wlr in the namespace against offscreen Shingle and
reads a ScreenCast portal stream of its output with GStreamer's
`pipewiresrc` (after tools/gpu_test/fetch_screencast_deps.py, run once
outside the namespace): pixels, frames only on damage, the hidden and
embedded cursor modes and a DMA-BUF stream. `--checks image-copy-capture`
checks ext-image-copy-capture from a raw-protocol client (capture_wire.py):
output, toplevel and cursor sessions, damage, paint_cursors and a capture
into a GBM DMA-BUF shown in a window, and grim with and without `-c`;
`--checks ext-data-control` copies and pastes through ext-data-control
against wl-copy and wl-paste. `--checks launcher` (launcher_checks.py)
drives shingle-launcher (`--launcher`, the release build by default)
through STIPC keys and pointer: type-to-launch, the in-place edits,
persistence and its startup time. `--checks animate` (animate_checks.py)
captures every animation of Wayfire's animate, slowed to seconds, half
way through, on views of one surface and of several pieces, and the
output's fade-in at start; `--checks animate-cost` times the frames of
eight views closing at once at 4K. `--checks lock`
(lock_checks.py) locks with shingle-lock (`--lock`, the debug build by
default) and checks every presented frame, recorded with
`SHINGLE_RECORD_FRAMES`: no client unblurred from the lock request to
the unlock, black until the lock surface, across DPMS, an output turned
off and on and a killed lock client, the drift, the input's states and
the grace period. `--checks rehearsal --wayfire-ini PATH`
(rehearsal_checks.py) rehearses a real wayfire.ini: a harness-safe copy
under offscreen Shingle and headless Wayfire, every binding it
configures pressed through STIPC in both and the effects compared; the
detailed table is copied to $TMPDIR, and without `--wayfire-ini` the
group is skipped. `--checks first-frame` (first_frame_checks.py) starts
kitty, foot (`SHINGLE_FOOT`) and Firefox beside a tiled probe several
times and reports from their `WAYLAND_DEBUG` logs the time to their
first configure, first buffer and first buffer at the tile's size.
`--checks straddle` (straddle_checks.py) places a `wl_probe --pattern`
across a workspace's edge and captures it on both workspaces, during
vswitch's slide with its gap between the two parts and in Expo's two
cells, clicks the part on the neighbour through STIPC and compares the
captures outside the slide and the focus with headless Wayfire.
`--checks apps`
(app_checks.py) is the
application compatibility sweep: every installed application, one at a
time, through a scripted scenario against offscreen Shingle, and
`app-NAME` runs one of them.

It exits with the command's status, or 1 when a check failed. With
`--checks` it prints one PASS or FAIL line per check and INFO lines for
what is only recorded, and `--captures` saves the settled captures there
as PPM files. Signals go only to processes inside the PID namespace,
through `signal_checked`. The pure helpers are tested by
`python3 -m unittest` in this directory (test_run.py, test_checks.py,
test_dmabuf_checks.py, test_output_checks.py,
test_nested_output_checks.py, test_wayfire_checks.py,
test_vswipe_checks.py, test_decoration_checks.py,
test_switcher_checks.py, test_colour_checks.py, test_focus_checks.py,
test_drag_icon_checks.py, test_resize_checks.py,
test_nested_clipboard_checks.py, test_screencast_checks.py,
test_app_checks.py, test_animate_checks.py, test_lock_checks.py,
test_rehearsal_checks.py, test_first_frame_checks.py,
test_straddle_checks.py).
The work directory under `target/` is removed afterwards unless `--keep`
is given.
"""

import argparse
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading

INNER_FLAG = '--inner'
# Bind-mounted into the private /dev when the host has them.
BASIC_DEVICES = ('null', 'zero', 'full', 'random', 'urandom')
GPU_DEVICES = ('dri/renderD128', 'nvidia0', 'nvidiactl', 'nvidia-uvm',
               'nvidia-uvm-tools', 'udmabuf')
MODESET_DEVICE = 'nvidia-modeset'
STRIPPED_VARIABLES = ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'DISPLAY',
                      'WAYFIRE_SOCKET', 'DBUS_SESSION_BUS_ADDRESS')
# What the private /dev may also hold besides the bound nodes.
PRIVATE_ENTRIES = ('pts', 'ptmx', 'shm', 'fd', 'stdin', 'stdout', 'stderr')
SAMPLE_INTERVAL = 0.05
# The rehearsal group's input file and the table it writes into the work
# directory, which main copies to $TMPDIR before removing the directory
# (rehearsal_checks.INI_VARIABLE and REPORT_VARIABLE).
REHEARSAL_INI_VARIABLE = 'SHINGLE_REHEARSAL_INI'
REHEARSAL_REPORT_VARIABLE = 'SHINGLE_REHEARSAL_REPORT'
REHEARSAL_REPORT = 'rehearsal.md'
# The caller's HOME, which the namespace replaces: a `~/` path of an
# autostart entry gets a stand-in where it names something there.
REHEARSAL_HOME_VARIABLE = 'SHINGLE_REHEARSAL_HOME'
# app_checks.GROUPS: the application sweep, all of it or one app.
APP_GROUPS = ('apps', 'app-kitty', 'app-konsole', 'app-kate', 'app-dolphin',
              'app-okular', 'app-evince', 'app-mpv', 'app-vlc', 'app-gimp',
              'app-inkscape', 'app-libreoffice', 'app-firefox',
              'app-chromium', 'app-google-chrome', 'app-rofi', 'app-dunst')
# checks.GROUPS, dmabuf_checks.GROUPS, output_checks.GROUPS,
# nested_output_checks.GROUPS, wayfire_checks.GROUPS,
# vswipe_checks.GROUPS, decoration_checks.GROUPS and
# switcher_checks.GROUPS, colour_checks.GROUPS, focus_checks.GROUPS and
# drag_icon_checks.GROUPS, resize_checks.GROUPS,
# nested_clipboard_checks.GROUPS, screencast_checks.GROUPS,
# launcher_checks.GROUPS, animate_checks.GROUPS, lock_checks.GROUPS,
# rehearsal_checks.GROUPS, first_frame_checks.GROUPS and
# straddle_checks.GROUPS, kept here so
# that `--help` works without importing them.
CHECK_GROUPS = ('offscreen', 'nested', 'baseline', 'buffers', 'shm',
                'dmabuf', 'browsers', 'xwayland', 'nested-gpu', 'nodmabuf',
                'weston', 'syncobj', 'nosyncobj', 'outputs', 'nested-outputs',
                'nested-smoke', 'nested-scaled', 'wayfire-plugins',
                'wayfire-shell', 'vswipe', 'decoration', 'switcher',
                'colours', 'minimize-focus', 'drag-icon', 'resize',
                'resize-nested', 'nested-clipboard', 'screencast',
                'image-copy-capture', 'ext-data-control',
                'launcher', 'animate', 'animate-cost', 'lock', 'rehearsal',
                'first-frame', 'straddle') + APP_GROUPS
# DB-4's and DB-5's groups whose NVIDIA Vulkan swapchains need
# nvidia-modeset, and nested Shingle's on NVIDIA in `resize-nested`;
# `syncobj` runs no Vulkan client.
MODESET_GROUPS = ('dmabuf', 'browsers', 'xwayland', 'nested-gpu', 'nodmabuf',
                  'weston', 'nosyncobj', 'resize-nested') + APP_GROUPS

def allowed_devices(modeset):
  """The device nodes, relative to /dev, that the namespace exposes."""
  devices = BASIC_DEVICES + GPU_DEVICES
  if modeset:
    devices += (MODESET_DEVICE,)
  return devices

def bindable(devices, exists):
  """The `devices` the host has, by `exists(path)`; the rest are skipped."""
  return [device for device in devices if exists(f'/dev/{device}')]

def is_allowed_path(path, devices):
  """Whether an open file at `path` is allowed inside the namespace.

  Paths outside /dev are always allowed. Under /dev, only the bound
  `devices`, the directories that hold them, the private pts and shm, and
  ptmx are. A directory such as /dev/dri is allowed because the private
  /dev holds nothing in it but the bound nodes, so listing it, as Mesa and
  libdrm do, reaches no other device.
  """
  if not path.startswith('/dev/'):
    return True
  name = path[len('/dev/'):]
  if name in devices or name == 'ptmx':
    return True
  if name in {device.rsplit('/', 1)[0] for device in devices
              if '/' in device}:
    return True
  return name.startswith(('pts/', 'shm/'))

def disallowed(opened, devices):
  """The sorted (command, path) pairs of `opened` not allowed."""
  return sorted({(command, path) for command, path in opened
                 if not is_allowed_path(path, devices)})

def unexpected_dev_entries(entries, devices):
  """Top-level /dev entries that neither a bound device nor private."""
  allowed = {device.split('/')[0] for device in devices}
  allowed.update(PRIVATE_ENTRIES)
  return sorted(set(entries) - allowed)

def clean_environment(environment, work, runtime):
  """The command's environment: host display variables removed, private
  home, runtime, config, state and cache directories."""
  clean = {key: value for key, value in environment.items()
           if key not in STRIPPED_VARIABLES}
  clean.update({
      'HOME': os.path.join(work, 'home'),
      'XDG_RUNTIME_DIR': runtime,
      'XDG_CONFIG_HOME': os.path.join(work, 'config'),
      'XDG_STATE_HOME': os.path.join(work, 'state'),
      'XDG_CACHE_HOME': os.path.join(work, 'cache'),
  })
  return clean

def command_name(pid):
  """The `comm` of `pid`, or None when it is gone."""
  try:
    with open(f'/proc/{pid}/comm') as file:
      return file.read().strip()
  except (FileNotFoundError, ProcessLookupError):
    return None

def signal_checked(pid, sig, expected):
  """Sends `sig` to `pid` if it is a live `expected` process.

  Returns whether it did. Refuses any PID that is not an int above 1 or
  whose `/proc/<pid>/comm` is not `expected` (as the kernel truncates it,
  to 15 characters), so that neither -1 (every process of the user) nor 0
  (the process group) can ever be signalled.
  """
  if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 1:
    return False
  if command_name(pid) != expected[:15]:
    return False
  os.kill(pid, sig)
  return True

def open_devices():
  """(command, path) of every file under /dev open in the namespace."""
  found = []
  for entry in os.listdir('/proc'):
    if not entry.isdigit():
      continue
    try:
      fds = os.listdir(f'/proc/{entry}/fd')
    except OSError:
      continue
    command = command_name(entry) or '?'
    for fd in fds:
      try:
        target = os.readlink(f'/proc/{entry}/fd/{fd}')
      except OSError:
        continue
      if target.startswith('/dev/'):
        found.append((command, target))
  return found

class DeviceSampler:
  """Collects `open_devices()` in a thread until stopped."""

  def __init__(self):
    self.opened = set()
    self._stop = threading.Event()
    self._thread = threading.Thread(target=self._run, daemon=True)

  def start(self):
    """Starts sampling."""
    self._thread.start()

  def stop(self):
    """Takes a last sample and stops."""
    self._stop.set()
    self._thread.join()
    self.opened.update(open_devices())

  def _run(self):
    while not self._stop.wait(SAMPLE_INTERVAL):
      self.opened.update(open_devices())

def keep_report(report, directory, pid):
  """Copies the rehearsal `report` to `directory` as
  shingle-rehearsal-PID.md, readable by its owner only; returns the
  path."""
  kept = os.path.join(directory, f'shingle-rehearsal-{pid}.md')
  with open(report) as source:
    text = source.read()
  descriptor = os.open(kept, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
  with os.fdopen(descriptor, 'w') as file:
    file.write(text)
  return kept

def mount(*arguments):
  """Runs mount(8), failing loudly."""
  subprocess.run(['mount', *arguments], check=True)

def build_dev(work, devices):
  """Replaces /dev with a tmpfs holding only `devices` and private pts and
  shm; returns the devices actually bound."""
  host_dev = os.path.join(work, 'host-dev')
  os.makedirs(host_dev)
  # Recursive, because a user namespace may not bind a mount without the
  # submounts locked onto it; detached again below.
  mount('--rbind', '/dev', host_dev)
  bound = bindable(devices, lambda path: os.path.exists(
      os.path.join(host_dev, path[len('/dev/'):])))
  mount('-t', 'tmpfs', '-o', 'mode=0755', 'none', '/dev')
  for device in bound:
    target = os.path.join('/dev', device)
    os.makedirs(os.path.dirname(target), exist_ok=True)
    open(target, 'w').close()
    mount('--bind', os.path.join(host_dev, device), target)
  subprocess.run(['umount', '--lazy', host_dev], check=True)
  os.rmdir(host_dev)
  os.mkdir('/dev/pts')
  mount('-t', 'devpts', '-o', 'newinstance,ptmxmode=0666', 'devpts',
        '/dev/pts')
  os.symlink('pts/ptmx', '/dev/ptmx')
  os.mkdir('/dev/shm', 0o1777)
  mount('-t', 'tmpfs', '-o', 'mode=1777', 'none', '/dev/shm')
  os.symlink('/proc/self/fd', '/dev/fd')
  for number, name in enumerate(('stdin', 'stdout', 'stderr')):
    os.symlink(f'/proc/self/fd/{number}', f'/dev/{name}')
  return bound

def run_command(command, environment, timeout):
  """Runs `command` and returns its status; SIGKILL after `timeout`."""
  process = subprocess.Popen(command, env=environment)
  try:
    return process.wait(timeout=timeout)
  except subprocess.TimeoutExpired:
    name = os.path.basename(command[0])
    print(f'gpu_test: FAIL timed out after {timeout} s', flush=True)
    signal_checked(process.pid, signal.SIGKILL, name)
    process.wait()
    return 1

def inner(work, command, modeset, timeout, checks=None):
  """Sets up the namespace and runs `command`, or the check groups of
  `checks` ({'groups', 'binary', 'wl_probe', 'captures'}); returns the
  exit status."""
  private_tmp = os.path.join(work, 'tmp')
  os.makedirs(private_tmp)
  mount('--bind', private_tmp, '/tmp')
  runtime = f'/run/user/{os.environ.get("SHINGLE_GPU_TEST_UID", "1000")}'
  if os.path.isdir(runtime):
    mount('-t', 'tmpfs', '-o', 'mode=0700', 'none', runtime)
  else:
    runtime = os.path.join(work, 'runtime')
    os.makedirs(runtime, mode=0o700)
  for name in ('home', 'config', 'state', 'cache'):
    os.makedirs(os.path.join(work, name))
  devices = allowed_devices(modeset)
  bound = build_dev(work, devices)
  failures = []
  extra = unexpected_dev_entries(os.listdir('/dev'), devices)
  if extra:
    failures.append(f'unexpected /dev entries: {extra}')
  if os.path.exists('/dev/dri') and os.listdir('/dev/dri') != ['renderD128']:
    failures.append(f'/dev/dri holds {os.listdir("/dev/dri")}')
  if os.listdir(runtime):
    failures.append(f'{runtime} is not empty')
  print(f'gpu_test: /dev holds {", ".join(bound)}', flush=True)
  if failures:
    for failure in failures:
      print(f'gpu_test: FAIL {failure}', flush=True)
    return 1
  environment = clean_environment(dict(os.environ), work, runtime)
  sampler = DeviceSampler()
  sampler.start()
  if checks is None:
    status = run_command(command, environment, timeout)
  else:
    # The checks start their processes from here, so they inherit it.
    os.environ.clear()
    os.environ.update(environment)
    import checks as real_client_checks
    status = real_client_checks.run_checks(
        checks['groups'], checks['binary'], checks['wl_probe'],
        checks['captures'])
  sampler.stop()
  bad = disallowed(sampler.opened, bound)
  seen = sorted({path for _, path in sampler.opened})
  print(f'gpu_test: devices opened: {", ".join(seen) or "none"}',
        flush=True)
  if bad:
    print(f'gpu_test: FAIL devices outside the allowed set: {bad}',
          flush=True)
    return 1
  print('gpu_test: PASS no process opened a device outside the allowed '
        'set', flush=True)
  return status

def main():
  """Sets up the work directory and re-runs itself in the namespace."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--modeset', action='store_true',
                      help='also expose /dev/nvidia-modeset')
  parser.add_argument('--keep', action='store_true',
                      help='keep the work directory')
  parser.add_argument('--timeout', type=float, default=600.0)
  parser.add_argument('--checks', nargs='+', metavar='GROUP',
                      choices=CHECK_GROUPS,
                      help='run these real-client check groups')
  parser.add_argument('--binary', default='target/debug/shingle')
  parser.add_argument('--wl-probe', default='target/debug/examples/wl_probe')
  parser.add_argument('--launcher', default='target/release/shingle-launcher',
                      help='the shingle-launcher the launcher group runs')
  parser.add_argument('--lock', default='target/debug/shingle-lock',
                      help='the shingle-lock the lock group runs')
  parser.add_argument('--captures', metavar='DIR',
                      help='save settled captures here')
  parser.add_argument('--wayfire-ini', metavar='PATH',
                      help='the wayfire.ini the rehearsal group rehearses')
  parser.add_argument(INNER_FLAG, metavar='WORK')
  parser.add_argument('command', nargs='*')
  options = parser.parse_args()
  if bool(options.checks) == bool(options.command):
    parser.error('give either --checks or a command')
  checks = None
  if options.checks:
    checks = {
        'groups': options.checks,
        'binary': os.path.abspath(options.binary),
        'wl_probe': os.path.abspath(options.wl_probe),
        'captures': (os.path.abspath(options.captures)
                     if options.captures else None),
    }
  # NVIDIA's Vulkan WSI fails to create a swapchain without nvidia-modeset
  # (docs/design/dmabuf.md, DB-4 as built), so DB-4's checks expose it.
  options.modeset = options.modeset or any(
      group in MODESET_GROUPS for group in options.checks or ())
  if options.inner:
    return inner(options.inner, options.command, options.modeset,
                 options.timeout, checks)
  command = list(options.command)
  if command and os.path.sep in command[0]:
    command[0] = os.path.abspath(command[0])
  work = os.path.abspath(os.path.join('target', f'gpu_test-{os.getpid()}'))
  os.makedirs(work)
  try:
    # The PID namespace confines every signal to processes started inside
    # it, so a wrong PID can never reach the host session.
    namespace = ['unshare', '--user', '--map-root-user', '--mount', '--net',
                 '--pid', '--fork', '--mount-proc', '--']
    arguments = [sys.executable, os.path.abspath(__file__),
                 '--timeout', str(options.timeout), INNER_FLAG, work]
    if options.modeset:
      arguments.append('--modeset')
    if checks:
      arguments += ['--checks', *checks['groups'], '--binary',
                    checks['binary'], '--wl-probe', checks['wl_probe']]
      if checks['captures']:
        arguments += ['--captures', checks['captures']]
    environment = dict(os.environ)
    environment['SHINGLE_GPU_TEST_UID'] = str(os.getuid())
    environment['SHINGLE_LAUNCHER_BINARY'] = os.path.abspath(options.launcher)
    environment['SHINGLE_LOCK_BINARY'] = os.path.abspath(options.lock)
    report = os.path.join(work, REHEARSAL_REPORT)
    if options.wayfire_ini:
      environment[REHEARSAL_INI_VARIABLE] = os.path.abspath(
          os.path.expanduser(options.wayfire_ini))
      environment[REHEARSAL_REPORT_VARIABLE] = report
      environment[REHEARSAL_HOME_VARIABLE] = os.path.expanduser('~')
    status = subprocess.run(namespace + arguments + ['--'] + command,
                            env=environment).returncode
    if os.path.exists(report):
      kept = keep_report(report, tempfile.gettempdir(), os.getpid())
      print(f'gpu_test: the rehearsal table is in {kept}', flush=True)
    return status
  finally:
    if not options.keep:
      shutil.rmtree(work, ignore_errors=True)

if __name__ == '__main__':
  sys.exit(main())
