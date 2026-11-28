"""`check` and `deploy` of bringup.py: step 0 of docs/native-testing.md.

`check` reads the test machine's state and prints, for each item of step
0 that is missing, the exact command that fixes it. It never runs sudo
for real: it only asks `sudo -n -l CMD`, which lists whether CMD may run
without a password and runs nothing.

`deploy` builds `shingle`, `shingle-lock`, `shingle-launcher` and the
present_probe example here in release and copies them, with the test
tools and the tty1 start script, to ~/shingle-bringup on the test
machine, after checking that the binaries' glibc symbol versions and
shared libraries exist there. Otherwise it pushes the commit into
~/shingle on the test machine with git over the same SSH connection and
builds there.
"""

import dataclasses
import datetime
import json
import os
import re
import shlex

from bringup_steps import BASE, TOOLS

REQUIRED_PACKAGES = ['seatd', 'wev', 'python3-evdev', 'wayland-utils',
                     'procps', 'kbd', 'libinput10', 'libseat1', 'libudev1',
                     'libdrm2', 'libgbm1', 'libxkbcommon0', 'libpam0g',
                     'mesa-vulkan-drivers']
LATER_PACKAGES = ['foot', 'grim', 'wlopm', 'wlr-randr', 'edid-decode',
                  'drm-info', 'vulkan-tools', 'mesa-utils-bin',
                  'libinput-tools', 'udev', 'kitty']
BUILD_PACKAGES = ['libwayland-dev', 'libxkbcommon-dev', 'libpam0g-dev',
                  'libinput-dev', 'libseat-dev', 'libudev-dev', 'libdrm-dev',
                  'libgbm-dev', 'pkg-config', 'git']
TEST_CONFIG = 'session: {restore: false}\nxwayland: {mode: false}\n'
SHIPPED_TOOLS = ['ipc.py', 'uinput.py', 'present_check.py', 'start_tty1.py']
# The lock screen and the launcher run from bindings (a Wayfire import
# swaps swaylock and rofi for them), so a session needs them on PATH too.
BINARIES = {'shingle': 'release/shingle',
            'shingle-lock': 'release/shingle-lock',
            'shingle-launcher': 'release/shingle-launcher',
            'present_probe': 'release/examples/present_probe'}
LOAD_LIMIT = 12.0

@dataclasses.dataclass
class Item:
  """One item of `check`."""

  name: str
  ok: bool
  required: bool
  detail: str = ''
  fix: list = dataclasses.field(default_factory=list)

def installed_packages(output):
  """The installed packages in `dpkg-query -W -f '${Package}
  ${db:Status-Abbrev}\\n'` output."""
  installed = set()
  for line in output.splitlines():
    fields = line.split()
    if len(fields) >= 2 and fields[1].startswith('ii'):
      installed.add(fields[0].split(':')[0])
  return installed

def seatd_group(unit):
  """The group seatd's unit runs it for (`-g GROUP`), or video, Debian's
  default."""
  found = re.search(r'ExecStart=.*\s-g\s*(\S+)', unit or '')
  return found.group(1) if found else 'video'

def sudoers_rule(user, home):
  """The sudoers line that lets the driven steps run chvt, fgconsole and
  uinput.py without a password, and nothing else."""
  uinput = f'{home}/{TOOLS}/uinput.py'
  return (f'{user} ALL=(root) NOPASSWD: /usr/bin/chvt, /usr/bin/fgconsole, '
          f'/usr/bin/python3 {uinput} *')

CHECK_SCRIPT = (
    "dpkg-query -W -f '${Package} ${db:Status-Abbrev}\\n' "
    + ' '.join(REQUIRED_PACKAGES + LATER_PACKAGES + BUILD_PACKAGES)
    + " 2>/dev/null; echo '--'; "
    'echo "$HOME"; id -un; id -nG; echo "--"; '
    'sh -l -c "command -v shingle; command -v present_probe" 2>/dev/null; '
    'echo "--"; '
    f'"$HOME/{BASE}/bin/shingle" --version 2>&1; echo "status=$?"; '
    f'cat "$HOME/{BASE}/DEPLOYED.json" 2>/dev/null; echo; echo "--"; '
    'cat "$HOME/.config/shingle/test.yaml" 2>/dev/null; echo "--"; '
    'systemctl cat seatd 2>/dev/null | grep ExecStart; echo "--"; '
    f'sudo -n -l /usr/bin/chvt 1 >/dev/null 2>&1; echo "chvt=$?"; '
    'sudo -n -l /usr/bin/fgconsole >/dev/null 2>&1; echo "fgconsole=$?"; '
    f'sudo -n -l /usr/bin/python3 "$HOME/{TOOLS}/uinput.py" key KEY_A '
    '>/dev/null 2>&1; echo "uinput=$?"; echo "--"; '
    'systemctl is-active getty@tty1.service 2>&1; '
    'command -v cargo || ls "$HOME/.cargo/bin/cargo" 2>/dev/null; '
    'echo "--"')

def split_sections(text):
  """The `--`-separated sections of CHECK_SCRIPT's output."""
  sections, current = [], []
  for line in text.splitlines():
    if line.strip() == '--':
      sections.append(current)
      current = []
    else:
      current.append(line)
  sections.append(current)
  return sections

def check_items(output, marker):
  """The `Item`s of step 0 from CHECK_SCRIPT's `output`."""
  sections = split_sections(output) + [[]] * 8
  packages, identity, path, deployed, config, seatd, sudo, misc = (
      sections[:8])
  installed = installed_packages('\n'.join(packages))
  home, user, groups = (identity + ['', '', ''])[:3]
  groups = set(groups.split())
  items = []
  missing = [p for p in REQUIRED_PACKAGES if p not in installed]
  items.append(Item('packages of step 0', not missing, True,
                    ', '.join(missing) or 'all installed',
                    [f'sudo apt install {" ".join(missing)}'] if missing
                    else []))
  later = [p for p in LATER_PACKAGES if p not in installed]
  items.append(Item('packages of the later steps', not later, False,
                    ', '.join(later) or 'all installed',
                    [f'sudo apt install {" ".join(later)}'] if later else []))
  items.append(Item(
      'marker /etc/shingle-native-test-machine', marker, True,
      'present' if marker else 'missing: uinput.py, start_tty1.py and every '
      'driven step refuse to run',
      [] if marker else ['sudo touch /etc/shingle-native-test-machine']))
  status = next((line for line in deployed if line.startswith('status=')),
                'status=?')
  version = deployed[0] if deployed else ''
  deploy_ok = status == 'status=0'
  items.append(Item(
      f'a deployed build in ~/{BASE}/bin', deploy_ok, True,
      version if deploy_ok else f'{version} ({status})',
      [] if deploy_ok else ['tools/native_test/bringup.py deploy HOST']))
  on_path = [line for line in path if line.strip()]
  path_ok = len(on_path) == 2 and all(
      line.startswith(f'{home}/') for line in on_path)
  items.append(Item(
      'shingle and present_probe on a login shell\'s PATH', path_ok, True,
      ', '.join(on_path) or 'not found',
      [] if path_ok else [
          'mkdir -p ~/.local/bin, then log in again (Debian\'s ~/.profile '
          'adds it to PATH only if it exists); bringup.py deploy links '
          'both into it']))
  config_text = '\n'.join(config)
  config_ok = 'restore: false' in config_text and 'mode: false' in config_text
  items.append(Item(
      'the test config ~/.config/shingle/test.yaml', config_ok, True,
      'present' if config_ok else 'missing or without restore: false and '
      'xwayland mode: false',
      [] if config_ok else [
          'mkdir -p ~/.config/shingle',
          "printf 'session: {restore: false}\\nxwayland: {mode: false}\\n' "
          '> ~/.config/shingle/test.yaml']))
  group = seatd_group('\n'.join(seatd))
  wanted = [g for g in (group, 'video', 'input') if g not in groups]
  wanted = list(dict.fromkeys(wanted))
  items.append(Item(
      f'groups ({group} for seatd in step 1.4, video for drm_info, input '
      'for libinput list-devices)', not wanted, True,
      f'{user} is in {", ".join(sorted(groups)) or "no groups"}',
      [f'sudo usermod -aG {",".join(wanted)} {user}',
       'then log out and in again (tty1 and SSH)'] if wanted else []))
  rules = dict(line.split('=', 1) for line in sudo if '=' in line)
  denied = [name for name in ('chvt', 'fgconsole', 'uinput')
            if rules.get(name) != '0']
  items.append(Item(
      'sudo without a password for chvt, fgconsole and uinput.py',
      not denied, True,
      'allowed' if not denied else f'not allowed: {", ".join(denied)}',
      [] if not denied else [
          f'echo {shlex.quote(sudoers_rule(user, home))} | '
          'sudo tee /etc/sudoers.d/shingle-bringup',
          'sudo chmod 440 /etc/sudoers.d/shingle-bringup',
          'sudo visudo -c']))
  getty = misc[0].strip() if misc else ''
  items.append(Item('a login on tty1 (getty@tty1)', getty == 'active', False,
                    getty or 'unknown',
                    [] if getty == 'active' else
                    ['sudo systemctl enable --now getty@tty1.service']))
  cargo = misc[1].strip() if len(misc) > 1 else ''
  build_missing = [p for p in BUILD_PACKAGES if p not in installed]
  items.append(Item(
      'a Rust toolchain and build packages, for deploy --build-on-target',
      bool(cargo) and not build_missing, False,
      (cargo or 'no cargo') + (f'; missing {", ".join(build_missing)}'
                               if build_missing else ''),
      ([f'sudo apt install {" ".join(build_missing)}'] if build_missing
       else []) + ([] if cargo else
                   ['curl --proto =https -sSf https://sh.rustup.rs | sh'])))
  return items

def format_items(host, items):
  """The text `check` prints."""
  lines = [f'check {host}:']
  for item in items:
    tag = 'ok' if item.ok else ('MISSING' if item.required else 'optional')
    lines.append(f'  {tag:8} {item.name}: {item.detail}')
    for command in item.fix:
      lines.append(f'           {command}')
  required = [item for item in items if item.required and not item.ok]
  lines.append('step 0 is complete' if not required else
               f'{len(required)} required items missing; run the commands '
               'above on the test machine')
  return '\n'.join(lines)

def run_check(target, info):
  """Runs `check`; returns (text, exit status)."""
  result = target.run(CHECK_SCRIPT, timeout=60)
  items = check_items(result.stdout, info.marker)
  required = [item for item in items if item.required and not item.ok]
  return format_items(target.host, items), 1 if required else 0

GLIBC = re.compile(r'GLIBC_(\d+)\.(\d+)')

def glibc_needed(objdump_output):
  """The highest GLIBC_x.y symbol version in `objdump -T` output."""
  versions = [(int(a), int(b)) for a, b in GLIBC.findall(objdump_output)]
  return max(versions, default=None)

def needed_libraries(readelf_output):
  """The NEEDED sonames in `readelf -d` output."""
  return re.findall(r'\(NEEDED\)\s+Shared library: \[([^\]]+)\]',
                    readelf_output)

def parse_glibc_version(text):
  """(major, minor) from `getconf GNU_LIBC_VERSION` (`glibc 2.41`)."""
  found = re.search(r'(\d+)\.(\d+)', text)
  return (int(found.group(1)), int(found.group(2))) if found else None

def ldconfig_sonames(text):
  """The sonames `ldconfig -p` lists."""
  return {line.split()[0] for line in text.splitlines()[1:] if line.strip()}

@dataclasses.dataclass
class Compatibility:
  """Whether binaries built here run on the target."""

  ok: bool
  reasons: list

def compatibility(local, remote_arch, remote_glibc, remote_libraries,
                  local_arch):
  """Compares what the binaries need (`local`: {'glibc': (x, y),
  'libraries': [soname]}) with what the target has."""
  reasons = []
  if remote_arch != local_arch:
    reasons.append(f'the target is {remote_arch}, this host {local_arch}')
  needed = local['glibc']
  if needed is None or remote_glibc is None:
    reasons.append('the glibc versions could not be read')
  elif remote_glibc < needed:
    reasons.append(f'the binaries need GLIBC_{needed[0]}.{needed[1]}, the '
                   f'target has {remote_glibc[0]}.{remote_glibc[1]}')
  missing = sorted(set(local['libraries']) - remote_libraries)
  if missing:
    reasons.append(f'the target lacks {", ".join(missing)}')
  return Compatibility(not reasons, reasons)

def load_average(path='/proc/loadavg'):
  """The 1-minute load of this host."""
  with open(path, encoding='utf-8') as file:
    return float(file.read().split()[0])

def git_state(local, repo):
  """(commit, dirty) of the checkout at `repo`."""
  commit = local(['git', '-C', repo, 'rev-parse', 'HEAD']).stdout.strip()
  dirty = bool(local(['git', '-C', repo, 'status', '--porcelain',
                      '--untracked-files=no']).stdout.strip())
  return commit, dirty

def cargo_builds(jobs):
  """The cargo commands of a release build of the deployed binaries."""
  common = ['cargo', 'build', '--release', '--jobs', str(jobs)]
  return [common + ['-p', 'shingle-bin', '--bin', 'shingle'],
          common + ['-p', 'shingle-lock', '--bin', 'shingle-lock'],
          common + ['-p', 'shingle-launcher', '--bin', 'shingle-launcher'],
          common + ['-p', 'shingle-native', '--example', 'present_probe']]

class DeployError(Exception):
  """Deploying failed; the message says where."""

def build_here(local, repo, target_dir, jobs, load, out=print):
  """Builds the deployed binaries here; returns their paths."""
  if load > LOAD_LIMIT:
    raise DeployError(f'the load is {load:.1f}, above {LOAD_LIMIT:g}; wait '
                      'for other builds to finish')
  for argv in cargo_builds(jobs):
    out('  ' + shlex.join(argv))
    result = local(argv, timeout=7200, cwd=repo)
    if not result.ok:
      raise DeployError(f'{shlex.join(argv)} failed:\n'
                        f'{result.stderr[-2000:]}')
  return {name: os.path.join(target_dir, path)
          for name, path in BINARIES.items()}

def binary_needs(local, paths):
  """What the binaries at `paths` need: their highest glibc symbol
  version and their NEEDED sonames."""
  glibc, libraries = None, set()
  for path in paths:
    objdump = local(['objdump', '-T', path]).stdout
    needed = glibc_needed(objdump)
    if needed and (glibc is None or needed > glibc):
      glibc = needed
    libraries |= set(needed_libraries(local(['readelf', '-d', path]).stdout))
  return {'glibc': glibc, 'libraries': sorted(libraries)}

REMOTE_FACTS = ('uname -m; getconf GNU_LIBC_VERSION; echo "--"; '
                '/sbin/ldconfig -p 2>/dev/null || ldconfig -p')

def remote_facts(target):
  """(arch, glibc, sonames) of the target."""
  result = target.run(REMOTE_FACTS)
  head, _, libraries = result.stdout.partition('--\n')
  lines = head.splitlines() + ['', '']
  return (lines[0].strip(), parse_glibc_version(lines[1]),
          ldconfig_sonames(libraries))

def install_script(commit_record, source_dir=None):
  """The remote script that moves new binaries into place and links them
  into ~/.local/bin. With `source_dir`, the binaries are copied from a
  build there; otherwise from the uploaded `.new` files."""
  lines = [f'set -e; cd "$HOME/{BASE}"']
  for name, path in BINARIES.items():
    if source_dir:
      lines.append(f'cp "$HOME/{source_dir}/target/{path}" bin/{name}.new')
    lines.append(f'chmod 755 bin/{name}.new; mv -f bin/{name}.new '
                 f'bin/{name}')
  lines += [
      'chmod 755 tools/native_test/*.py',
      'mkdir -p "$HOME/.local/bin"',
  ]
  lines += [f'ln -sf "$HOME/{BASE}/bin/{name}" "$HOME/.local/bin/{name}"'
            for name in BINARIES]
  lines += [
      f'ln -sf "$HOME/{TOOLS}/start_tty1.py" "$HOME/.local/bin/shingle-tty1"',
      f'printf %s {shlex.quote(json.dumps(commit_record))} > DEPLOYED.json',
      'bin/shingle --version']
  return '\n'.join(lines)

def copy_tools(target, here):
  """Uploads the test tools and the start script."""
  for name in SHIPPED_TOOLS:
    result = target.put(os.path.join(here, name), f'{TOOLS}/{name}')
    if not result.ok:
      raise DeployError(f'copying {name} failed: {result.stderr.strip()}')

def deploy(target, local, repo, options, load=None, now=None, out=print):
  """Runs `deploy`; returns the DEPLOYED.json record."""
  here = os.path.dirname(os.path.abspath(__file__))
  commit, dirty = git_state(local, repo)
  now = now or datetime.datetime.now()
  record = {'commit': commit, 'dirty': dirty,
            'deployed': now.isoformat(timespec='seconds')}
  made = target.run(f'mkdir -p "$HOME/{BASE}/bin" "$HOME/{TOOLS}"')
  if not made.ok:
    raise DeployError(f'cannot create ~/{BASE}: {made.stderr.strip()}')
  arch, glibc, sonames = remote_facts(target)
  build_on_target = options.build_on_target
  if not build_on_target:
    target_dir = os.environ.get('CARGO_TARGET_DIR') or os.path.join(
        repo, 'target')
    out(f'building {commit[:12]}{" (dirty)" if dirty else ""} here')
    paths = build_here(local, repo, target_dir, options.jobs,
                       load_average() if load is None else load, out)
    needs = binary_needs(local, paths.values())
    local_arch = local(['uname', '-m']).stdout.strip()
    verdict = compatibility(needs, arch, glibc, sonames, local_arch)
    if verdict.ok:
      out(f'compatible: the target has glibc {glibc} and all '
          f'{len(needs["libraries"])} libraries')
      for name, path in paths.items():
        result = target.put(path, f'{BASE}/bin/{name}.new')
        if not result.ok:
          raise DeployError(f'copying {name} failed: '
                            f'{result.stderr.strip()}')
      record['built_on'] = 'development host'
      copy_tools(target, here)
      installed = target.run(install_script(record))
      if not installed.ok:
        raise DeployError(f'installing failed: {installed.stderr.strip()}')
      out(f'deployed {installed.stdout.strip()}')
      return record
    out('not compatible: ' + '; '.join(verdict.reasons) +
        '; building on the target instead')
  if dirty:
    raise DeployError('the checkout has uncommitted changes; a build on the '
                      'target builds a pushed commit, so commit them first')
  ready = target.run('[ -d shingle/.git ] || git init -q shingle; '
                     'git -C shingle config receive.denyCurrentBranch '
                     'updateInstead')
  if not ready.ok:
    raise DeployError(f'cannot prepare ~/shingle: {ready.stderr.strip()}')
  ssh = 'ssh ' + ' '.join(shlex.quote(o) for o in target.options)
  pushed = local(['git', '-C', repo, '-c', f'core.sshCommand={ssh}', 'push',
                  '--force', f'{target.host}:shingle',
                  f'{commit}:refs/heads/bringup'], timeout=600)
  if not pushed.ok:
    raise DeployError(f'git push to {target.host}:shingle failed: '
                      f'{pushed.stderr.strip()}')
  builds = ' && '.join(shlex.join(argv) for argv in cargo_builds(
      options.jobs))
  out(f'building {commit[:12]} on {target.host}')
  built = target.run(
      'set -e; export PATH="$HOME/.cargo/bin:$PATH"; cd shingle; '
      f'git checkout -q --detach {commit}; nice -n 10 sh -c '
      f'{shlex.quote(builds)}', timeout=7200)
  if not built.ok:
    raise DeployError(f'the build on the target failed:\n'
                      f'{built.stderr[-2000:]}')
  record['built_on'] = 'test machine'
  copy_tools(target, here)
  installed = target.run(install_script(record, source_dir='shingle'))
  if not installed.ok:
    raise DeployError(f'installing failed: {installed.stderr.strip()}')
  out(f'deployed {installed.stdout.strip()}')
  return record
