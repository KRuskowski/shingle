"""The daily config rehearsal: a real wayfire.ini under offscreen Shingle
and under headless Wayfire, run inside run.py's namespace.

`run.py --checks rehearsal --wayfire-ini PATH` copies PATH into the
namespace's private /tmp and runs offscreen Shingle on it, and then the
host's headless Wayfire on the same file (wayfire_reference.py), each
with one headless output for every `[output:NAME]` section. Without
`--wayfire-ini` the group is skipped with an INFO line.

The copy each compositor reads is made harness-safe first
(`rehearsal_ini`):

- every `[output:NAME]` becomes `[output:HEADLESS-N]`, in file order, and
  NAME is renamed to match wherever else the file names it (window
  rules), so that rules aimed at an output still find one;
- `stipc` and `ipc` join `[core] plugins`, for input and observation;
- each `[autostart]` entry is wrapped in a recorder that writes the
  entry's environment to a record file. An entry that talks to the
  host's services (D-Bus, systemd, portals, audio; `HOST_SERVICES`) is
  only recorded and never runs; any other runs inside the namespace with
  the compositor's private HOME, and a `~/` path in it names a stand-in
  file or directory in the namespace instead of the operator's;
  `wf-panel` and `wf-background` are recording stand-ins on PATH;
- each `[command]` command is replaced by the recorder alone, so that a
  binding's launch is recorded with its environment without starting
  anything.

Then every binding the file configures (`bindings`: the options Wayfire's
metadata types as key, button or activator, in the table
crates/shingle-config/data/wayfire-options.tsv, and the dynamic lists of
command, vswitch, expo and wsets) is pressed through STIPC, in a scene of
`wl_probe` windows set up the same way under both compositors
(`SCENES`), and what changed is compared (`diff`, `same_effect`): the
focused output, view and workspace, each view's output, workspace,
geometry and states, plugin activations, the keys and buttons that
reached the focused client, the launches recorded with their
environment, and wayfire-shell's `toggle_menu`. Each window rule of the
`on created if app_id|title is|contains "X"` kind, and each such atom of
simple-tile's `tile_by_default`, is exercised by a `wl_probe` called X.
Bindings that cannot be compared over IPC, and differences that stay,
are listed in `EXPLAINED` with the reason.

The detailed table (binding, category, result) goes to the file named by
SHINGLE_REHEARSAL_REPORT, which run.py copies to $TMPDIR; the PASS and
FAIL lines name only the section and option. Every signal goes through
`run.signal_checked` with the process's own comm, to processes inside
this PID namespace that started after the compositor did.
"""

import importlib.util
import json
import os
import re
import shlex
import shutil
import signal
import time

import checks
import run
import wayfire_reference

xw = checks.xw

def _load_wayland_wire():
  """tools/output_test/wayland_wire.py, the raw Wayland client, loaded by
  path so that its directory's modules stay out of this one's."""
  path = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..',
                      'output_test', 'wayland_wire.py')
  spec = importlib.util.spec_from_file_location('wayland_wire', path)
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module

wayland_wire = _load_wayland_wire()

GROUPS = ('rehearsal',)
INI_VARIABLE = 'SHINGLE_REHEARSAL_INI'
REPORT_VARIABLE = 'SHINGLE_REHEARSAL_REPORT'
HOME_VARIABLE = 'SHINGLE_REHEARSAL_HOME'
# A regular expression: only the bindings and rules whose label (`[section]
# option`) it matches are exercised, for working on a few of them.
ONLY_VARIABLE = 'SHINGLE_REHEARSAL_ONLY'
OPTION_TABLE = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..',
                            '..', 'crates', 'shingle-config', 'data',
                            'wayfire-options.tsv')
ROOT = '/tmp/rehearsal'
# The evdev names STIPC presses for Wayfire's modifier names.
MODIFIER_KEYS = {'super': 'KEY_LEFTMETA', 'alt': 'KEY_LEFTALT',
                 'ctrl': 'KEY_LEFTCTRL', 'shift': 'KEY_LEFTSHIFT',
                 'altgr': 'KEY_RIGHTALT', 'logo': 'KEY_LEFTMETA'}
# Values that bind nothing.
UNSET = ('', 'none', 'disabled')
# The dynamic lists whose entries are bindings, by section: option name
# prefix and whether the rest must be a number.
DYNAMIC_BINDINGS = {
    'command': (('binding_', False), ('repeatable_binding_', False),
                ('always_binding_', False), ('release_binding_', False)),
    'vswitch': (('binding_', True), ('with_win_', True), ('send_win_', True)),
    'expo': (('select_workspace_', True),),
    'wsets': (('wset_', True), ('send_to_wset_', True)),
}
# Programs that reach the host's session or system services. An autostart
# entry running one is recorded and never run.
HOST_SERVICES = re.compile(
    r'(^|[\s;&|(/])(dbus-update-activation-environment|dbus-launch|'
    r'dbus-send|gdbus|busctl|systemctl|systemd-run|loginctl|'
    r'xdg-desktop-portal[\w-]*|pactl|pacmd|pulseaudio|pipewire[\w-]*|'
    r'wireplumber|wpctl|gsettings|dconf|gnome-keyring[\w-]*|ssh-agent|'
    r'gpg-agent|polkit[\w-]*|udisksctl)(?=$|[\s;&|)])')
# Environment variables a compositor sets or changes for its children,
# compared after `normalized_environment`.
COMPARED_VARIABLES = ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET', 'DISPLAY',
                      'WAYFIRE_SOCKET', 'XDG_CURRENT_DESKTOP',
                      'XCURSOR_THEME', 'XCURSOR_SIZE',
                      '_JAVA_AWT_WM_NONREPARENTING', 'WAYFIRE_CONFIG_FILE')
# The process state a child inherits, compared too.
COMPARED_PROCESS = ('SigBlk', 'SigIgn', 'nofile')
PROBES = ('rehearsal-a', 'rehearsal-b', 'rehearsal-c')
# How long a press may take to settle, and how long nothing may change
# for it to count as settled.
SETTLE_TIMEOUT = 4.0
SETTLE_QUIET = 0.6
# Geometry may differ by this many pixels.
TOLERANCE = 4
# How long autostart clients get to map.
AUTOSTART_TIMEOUT = 60.0
AUTOSTART_QUIET = 8.0
# The record each recorder writes: argv, environment, inherited signal
# mask and dispositions, and the open-files limit. With --no-exec it
# stops there; otherwise it executes the rest of its arguments.
RECORDER = r'''#!/usr/bin/env python3
import json, os, resource, sys
arguments = sys.argv[1:]
execute = arguments[0] != '--no-exec'
if not execute:
  arguments = arguments[1:]
directory, name, command = arguments[0], arguments[1], arguments[2:]
status = {}
with open('/proc/self/status') as file:
  for line in file:
    key, _, value = line.partition(':')
    if key in ('SigBlk', 'SigIgn'):
      status[key] = value.strip()
status['nofile'] = list(resource.getrlimit(resource.RLIMIT_NOFILE))
record = {'name': name, 'command': command, 'environment': dict(os.environ),
          'process': status, 'executed': execute}
path = os.path.join(directory, name + '.json')
with open(path + '.part', 'w') as file:
  json.dump(record, file)
os.rename(path + '.part', path)
if execute and command:
  os.execvp(command[0], command)
'''
# What a stand-in for a kitty session file holds: one window with the
# default shell.
SESSION_STAND_IN = 'launch\n'
# Differences that stay, by (section, option) or (section, '*'), and
# bindings that cannot be compared over IPC.
EXPLAINED = {
    ('core', 'exit'): 'would end the session; not pressed',
}
# Differences that stay, by (section, option): the one observation that
# may differ and why. docs/status.md lists them too.
GRAB_REASON = (
    'Shingle gives the client the release of a modifier whose press it had '
    'before a plugin took the input; Wayfire\'s input grab keeps it')
ACCEPTED = {
    ('expo', 'toggle'): ('delivered', GRAB_REASON),
    ('move', 'activate'): ('delivered', GRAB_REASON),
    ('simple-tile', 'button_move'): ('delivered', GRAB_REASON),
    ('simple-tile', 'button_resize'): ('delivered', GRAB_REASON),
    ('resize', 'activate'): (
        'activations',
        'a press without motion on the only tiled view, which has no edge '
        'to drag: Wayfire\'s simple-tile activates for it and changes '
        'nothing, Shingle starts no drag'),
}
# Bindings and matchers whose views may differ in geometry alone, and why.
GEOMETRY_ACCEPTED = {
    ('simple-tile', 'key_toggle'),
    ('simple-tile', 'tile_by_default'),
}
GEOMETRY_REASON = (
    'Shingle configures a new toplevel at its tile size from the first '
    'configure, where Wayfire\'s is 0x0, so a view that floats from the '
    'start or is untiled before it was ever floating takes its tile\'s '
    'size, not the client\'s own at (100, 100)')

def parse_ini(text):
  """wf-config's reading of `text`: [(section, option, value)] in file
  order. An unescaped `#` starts a comment, `\\#` is a `#`, a line ending
  in `\\` continues, and options before any section are dropped
  (crates/shingle-config/src/ini.rs)."""
  logical = []
  pending = ''
  for line in text.split('\n'):
    kept = []
    index = 0
    while index < len(line):
      character = line[index]
      if character == '\\' and line[index + 1:index + 2] == '#':
        kept.append('#')
        index += 2
        continue
      if character == '#':
        break
      kept.append(character)
      index += 1
    stripped = ''.join(kept).rstrip()
    if stripped.endswith('\\') and not stripped.endswith('\\\\'):
      pending += stripped[:-1]
      continue
    logical.append(pending + stripped)
    pending = ''
  if pending:
    logical.append(pending)
  entries = []
  section = None
  for line in logical:
    trimmed = line.strip()
    if not trimmed:
      continue
    if len(trimmed) >= 2 and trimmed[0] == '[' and trimmed[-1] == ']':
      section = trimmed[1:-1]
      continue
    if '=' not in line or section is None:
      continue
    name, value = line.split('=', 1)
    entries.append((section, name.strip(), value.strip()))
  return entries

def option(entries, section, name, default=None):
  """The last value of `section`'s `name` in `entries`, or `default`."""
  found = [value for entry_section, entry_name, value in entries
           if (entry_section, entry_name) == (section, name)]
  return found[-1] if found else default

def plugins(entries):
  """The plugins `[core] plugins` loads."""
  return (option(entries, 'core', 'plugins') or '').split()

def output_names(entries):
  """The NAMEs of the `[output:NAME]` sections, in file order."""
  names = []
  for section, _, _ in entries:
    if section.startswith('output:') and section[7:] not in names:
      names.append(section[7:])
  return names

def output_mapping(entries):
  """{NAME: HEADLESS-N} for the file's outputs."""
  return {name: f'HEADLESS-{index + 1}'
          for index, name in enumerate(output_names(entries))}

def rename_outputs(text, mapping):
  """`text` with each whole-word output name of `mapping` renamed."""
  for old, new in mapping.items():
    text = re.sub(rf'(?<![\w-]){re.escape(old)}(?![\w-])', new, text)
  return text

def option_types(path=OPTION_TABLE):
  """{(section, option): type} from Wayfire's option table."""
  types = {}
  with open(path) as file:
    for line in file:
      if line.startswith('#') or not line.strip():
        continue
      fields = line.rstrip('\n').split('\t')
      if len(fields) >= 3:
        types[(fields[0], fields[1])] = fields[2]
  return types

def parse_binding(value):
  """The alternatives of a binding value: [(modifiers, key)], where key is
  a KEY_ or BTN_ name or None for modifiers alone. Alternatives of other
  kinds (gestures, hotspots) come back with key 'other'."""
  if value.strip().lower() in UNSET:
    return []
  alternatives = []
  for part in value.split('|'):
    tokens = part.split()
    if not tokens or ' '.join(tokens).lower() in UNSET:
      continue
    modifiers = [token[1:-1].lower() for token in tokens
                 if token.startswith('<') and token.endswith('>')]
    rest = [token for token in tokens
            if not (token.startswith('<') and token.endswith('>'))]
    if not rest:
      key = None
    elif len(rest) == 1 and rest[0].startswith(('KEY_', 'BTN_')):
      key = rest[0]
    else:
      key = 'other'
    alternatives.append((modifiers, key))
  return alternatives

def dynamic_binding(section, name):
  """Whether `name` of `section` is an entry of a binding dynamic list."""
  for prefix, numbered in DYNAMIC_BINDINGS.get(section, ()):
    if name.startswith(prefix):
      rest = name[len(prefix):]
      if rest and (rest.isdigit() or not numbered):
        return True
  return False

class Binding:
  """One configured binding: its option, alternatives and category."""

  def __init__(self, section, name, value, alternatives, category, loaded):
    self.section = section
    self.name = name
    self.value = value
    self.alternatives = alternatives
    self.category = category
    self.loaded = loaded

  def label(self):
    """`[section] option`, which is all the PASS and FAIL lines show."""
    return f'[{self.section}] {self.name}'

  def __repr__(self):
    return f'Binding({self.label()}, {self.category})'

def category(section, name, loaded):
  """How a binding is exercised; see SCENES."""
  if not loaded:
    return 'unloaded'
  if (section, '*') in EXPLAINED or (section, name) in EXPLAINED:
    return 'explained'
  table = {
      ('core', 'close_top_view'): 'close',
      ('core', 'focus_buttons'): 'focus-click',
      ('expo', 'toggle'): 'toggle',
      ('move', 'activate'): 'drag',
      ('simple-tile', 'button_move'): 'drag',
      ('resize', 'activate'): 'resize',
      ('simple-tile', 'button_resize'): 'resize',
      ('wayfire-shell', 'toggle_menu'): 'menu',
  }
  if (section, name) in table:
    return table[(section, name)]
  if section == 'command':
    return 'command'
  if section in ('vswitch', 'wsets', 'oswitch'):
    return section
  if section == 'expo':
    return 'expo-select'
  if section in ('switcher', 'fast-switcher'):
    return 'switcher'
  if section == 'simple-tile' and name.startswith('key_focus_'):
    return 'tile-focus'
  if 'toggle' in name:
    return 'toggle'
  return 'press'

def bindings(entries, types=None):
  """The bindings `entries` configure, in file order: every option typed
  key, button or activator, and every binding entry of a dynamic list,
  whose value binds something. Command bindings without a command are
  kept (Wayfire binds them all the same)."""
  types = option_types() if types is None else types
  loaded = set(plugins(entries)) | {'core'}
  found = []
  seen = set()
  for section, name, value in entries:
    kind = types.get((section, name))
    if kind not in ('key', 'button', 'activator') and not dynamic_binding(
        section, name):
      continue
    alternatives = parse_binding(value)
    if not alternatives or (section, name) in seen:
      continue
    seen.add((section, name))
    found.append(Binding(section, name, value, alternatives,
                         category(section, name, section in loaded),
                         section in loaded))
  # A later value of the same option is the one Wayfire uses.
  latest = {(section, name): value for section, name, value in entries}
  for binding in found:
    binding.value = latest[(binding.section, binding.name)]
    binding.alternatives = parse_binding(binding.value)
  return found

RULE = re.compile(r'^\s*on\s+created\s+if\s+(app_id|title)\s+(is|contains)\s+'
                  r'"([^"]*)"\s+then\s+(.+?)\s*$')
ATOM = re.compile(r'(app_id|title)\s+(is|contains)\s+"([^"]*)"')

def window_rules(entries, loaded):
  """[(option, probe name or None, reason)] for the window rules: the
  name of the `wl_probe` whose app ID and title match a rule of the
  `on created if app_id|title is|contains "X"` kind, or None with the
  reason it is not exercised. Every option of the section is a rule, as
  Wayfire's dynamic list with an empty prefix reads it."""
  if 'window-rules' not in loaded:
    return []
  rules = []
  for section, name, value in entries:
    if section != 'window-rules':
      continue
    match = RULE.match(value)
    if match:
      rules.append((name, match.group(3), None))
    else:
      rules.append((name, None, 'not an `on created if app_id|title '
                    'is|contains` rule'))
  return rules

def tile_matcher_probes(entries, loaded):
  """The probe names the atoms of simple-tile's `tile_by_default` name."""
  if 'simple-tile' not in loaded:
    return []
  matcher = option(entries, 'simple-tile', 'tile_by_default') or ''
  return list(dict.fromkeys(match.group(3)
                            for match in ATOM.finditer(matcher)))

def host_service(command):
  """Whether an autostart `command` reaches the host's services."""
  return bool(HOST_SERVICES.search(command))

def stand_in_command(command, stand_ins, real_home):
  """`command` with each `~/` path that exists under `real_home`
  replaced by a stand-in path under `stand_ins`; returns (command,
  [(path, kind)]) with kind 'session', 'file' or 'directory'. Nothing of
  the operator's file is read."""
  try:
    tokens = shlex.split(command)
  except ValueError:
    return command, []
  made = []
  replaced = []
  for index, token in enumerate(tokens):
    if not token.startswith('~/'):
      replaced.append(token)
      continue
    real = os.path.join(real_home, token[2:])
    stand_in = os.path.join(stand_ins, f'path-{len(made)}')
    if os.path.isdir(real):
      made.append((stand_in, 'directory'))
    elif os.path.exists(real):
      session = index > 0 and tokens[index - 1] == '--session'
      made.append((stand_in, 'session' if session else 'file'))
    else:
      replaced.append(token)
      continue
    replaced.append(stand_in)
  if not made:
    return command, []
  return shlex.join(replaced), made

def recorded(recorder, records, name, command, execute):
  """The shell command that records `name` into `records` and then, if
  `execute`, runs `command` with /bin/sh."""
  words = [recorder] + ([] if execute else ['--no-exec']) + [records, name]
  if command:
    words += ['/bin/sh', '-c', command]
  return shlex.join(words)

class Plan:
  """What `rehearsal_ini` made of the file for one compositor."""

  def __init__(self):
    self.autostart = []
    self.stand_ins = []
    self.mapping = {}

def rehearsal_ini(text, directory, real_home):
  """The harness-safe copy of the wayfire.ini `text` for a compositor
  whose records go to `directory`/records; returns (text, Plan). See the
  module's docstring."""
  plan = Plan()
  entries = parse_ini(text)
  plan.mapping = output_mapping(entries)
  recorder = os.path.join(directory, 'bin', 'record')
  records = os.path.join(directory, 'records')
  stand_ins = os.path.join(directory, 'stand-ins')
  lines = []
  section = None
  for line in rename_outputs(text, plan.mapping).split('\n'):
    header = re.match(r'^\s*\[([^\]]*)\]\s*$', line)
    if header:
      section = header.group(1)
      lines.append(line)
      continue
    parsed = parse_ini(f'[{section}]\n{line}') if section else []
    if not parsed:
      lines.append(line)
      continue
    _, name, value = parsed[0]
    if section == 'core' and name == 'plugins':
      words = value.split()
      words += [name for name in ('ipc', 'stipc') if name not in words]
      line = f'plugins = {" ".join(words)}'
    elif section == 'autostart' and name not in ('autostart_wf_shell',):
      runs = not host_service(value)
      command, made = stand_in_command(
          value, os.path.join(stand_ins, str(len(plan.autostart))),
          real_home)
      plan.stand_ins += made
      plan.autostart.append((name, runs))
      line = (f'{name} = ' + recorded(recorder, records, f'autostart-{name}',
                                      command, runs).replace('#', '\\#'))
    elif section == 'command' and name.startswith('command_') and value:
      line = f'{name} = ' + recorded(recorder, records, name, value,
                                     False).replace('#', '\\#')
    lines.append(line)
  return '\n'.join(lines), plan

def write_tools(directory, plan):
  """The recorder, the wf-shell stand-ins and `plan`'s stand-in paths in
  `directory`; returns the bin directory for PATH."""
  bin_directory = os.path.join(directory, 'bin')
  records = os.path.join(directory, 'records')
  os.makedirs(bin_directory, exist_ok=True)
  os.makedirs(records, exist_ok=True)
  os.chmod(records, 0o777)
  recorder = os.path.join(bin_directory, 'record')
  with open(recorder, 'w') as file:
    file.write(RECORDER)
  os.chmod(recorder, 0o755)
  for name in ('wf-panel', 'wf-background'):
    path = os.path.join(bin_directory, name)
    with open(path, 'w') as file:
      file.write(f'#!/bin/sh\nexec {shlex.quote(recorder)} --no-exec '
                 f'{shlex.quote(records)} {name}\n')
    os.chmod(path, 0o755)
  for path, kind in plan.stand_ins:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if kind == 'directory':
      os.makedirs(path, exist_ok=True)
    else:
      with open(path, 'w') as file:
        file.write(SESSION_STAND_IN if kind == 'session' else '')
  for root, directories, files in os.walk(directory):
    for name in directories + files:
      path = os.path.join(root, name)
      os.chmod(path, os.stat(path).st_mode | 0o555)
  os.chmod(directory, 0o755)
  return bin_directory

def normalized_environment(environment, known):
  """The COMPARED_VARIABLES of a recorded `environment`, with the values
  in `known` ({value: placeholder}, the compositor's own sockets and
  paths) replaced by their placeholders; a missing one is None."""
  normalized = {}
  for name in COMPARED_VARIABLES:
    value = environment.get(name)
    normalized[name] = known.get(value, value) if value is not None else None
  return normalized

def record_summary(record, known):
  """What is compared of a recorder's record."""
  if record is None:
    return None
  process = record.get('process', {})
  return {
      'environment': normalized_environment(record.get('environment', {}),
                                            known),
      'process': {name: process.get(name) for name in COMPARED_PROCESS},
  }

def workspace_of(view, outputs):
  """The workspace (x, y) a view's geometry lies on, from its output's
  current workspace and size; None if its output is unknown."""
  output = outputs.get(view.get('output-name'))
  if output is None:
    return None
  geometry = view.get('geometry', {})
  width = max(output['geometry']['width'], 1)
  height = max(output['geometry']['height'], 1)
  centre_x = geometry.get('x', 0) + geometry.get('width', 0) // 2
  centre_y = geometry.get('y', 0) + geometry.get('height', 0) // 2
  return (output['workspace']['x'] + centre_x // width,
          output['workspace']['y'] + centre_y // height)

def view_state(view, outputs, wset_names):
  """What is compared of one view (`list-views`)."""
  geometry = view.get('geometry', {})
  return {
      'output': view.get('output-name'),
      'wset': wset_names.get(view.get('wset-index'), view.get('wset-index')),
      'workspace': workspace_of(view, outputs),
      'geometry': tuple(geometry.get(key) for key in
                        ('x', 'y', 'width', 'height')),
      'fullscreen': view.get('fullscreen'),
      'minimized': view.get('minimized'),
      'sticky': view.get('sticky'),
      'tiled': bool(view.get('tiled-edges')),
      'layer': view.get('layer'),
  }

def snapshot_of(outputs, views, focused_output, focused_view, wset_names):
  """The comparable state from IPC replies: `list-outputs`, `list-views`,
  `get-focused-output` and `get-focused-view`. `wset_names` names each
  workspace set by the output it started on, since the compositors
  number them differently."""
  by_name = {output['name']: output for output in outputs}
  mapped = [view for view in views
            if view.get('mapped', True) and view.get('role') == 'toplevel'
            and view.get('app-id')]
  return {
      'focused-output': (focused_output.get('info') or {}).get('name'),
      'focused-view': (focused_view.get('info') or {}).get('app-id'),
      'workspaces': {name: (output['workspace']['x'],
                            output['workspace']['y'])
                     for name, output in by_name.items()},
      'wsets': {name: wset_names.get(output.get('wset-index'),
                                     output.get('wset-index'))
                for name, output in by_name.items()},
      'views': {view['app-id']: view_state(view, by_name, wset_names)
                for view in mapped},
  }

def diff(before, after):
  """What changed between two snapshots: {key: (before, after)}, with
  views as `view APP: FIELD`."""
  changes = {}
  for key in ('focused-output', 'focused-view', 'workspaces', 'wsets'):
    if before.get(key) != after.get(key):
      changes[key] = (before.get(key), after.get(key))
  old_views, new_views = before.get('views', {}), after.get('views', {})
  for app_id in sorted(set(old_views) | set(new_views)):
    old, new = old_views.get(app_id), new_views.get(app_id)
    if old is None or new is None:
      changes[f'view {app_id}'] = (old or 'absent', new or 'absent')
      continue
    for field in old:
      if old[field] != new[field]:
        changes[f'view {app_id}: {field}'] = (old[field], new[field])
  return changes

def close_enough(first, second):
  """Whether two values match, geometry tuples within TOLERANCE."""
  if (isinstance(first, tuple) and isinstance(second, tuple)
      and len(first) == len(second) == 4
      and all(isinstance(value, int) for value in first + second)):
    return all(abs(a - b) <= TOLERANCE for a, b in zip(first, second))
  if isinstance(first, (list, tuple)) and isinstance(second, (list, tuple)):
    return len(first) == len(second) and all(
        close_enough(a, b) for a, b in zip(first, second))
  if isinstance(first, dict) and isinstance(second, dict):
    return first.keys() == second.keys() and all(
        close_enough(first[key], second[key]) for key in first)
  return first == second

def same_effect(shingle, wayfire):
  """Whether two observations (`Session.observe`) match; returns (ok,
  the keys that differ)."""
  keys = sorted(set(shingle) | set(wayfire))
  differing = [key for key in keys
               if not close_enough(shingle.get(key), wayfire.get(key))]
  return not differing, differing

def only_geometry_differs(first, second):
  """Whether two observations differ in view geometry alone."""
  def without_geometry(observation):
    stripped = {}
    for key, value in observation.items():
      if key != 'changes':
        stripped[key] = value
        continue
      changes = {}
      for change, values in value.items():
        if change.endswith(': geometry'):
          continue
        changes[change] = tuple(
            {name: field for name, field in item.items()
             if name != 'geometry'} if isinstance(item, dict) else item
            for item in values)
      stripped[key] = changes
    return stripped
  return close_enough(without_geometry(first), without_geometry(second))

def verdict(section, name, mine, theirs):
  """(result, reason) of one comparison: 'match', 'explained' with the
  reason the difference stays, or 'DIFFERS'."""
  ok, differing = same_effect(mine, theirs)
  if ok:
    return 'match', None
  accepted = ACCEPTED.get((section, name))
  if accepted and differing == [accepted[0]]:
    return 'explained', accepted[1]
  if (section, name) in GEOMETRY_ACCEPTED and only_geometry_differs(
      mine, theirs):
    return 'explained', GEOMETRY_REASON
  return 'DIFFERS', f'differs in {", ".join(differing)}'

def describe(observation):
  """A short text of an observation, for the table."""
  if not observation:
    return 'no effect'
  parts = []
  for key, value in sorted(observation.items()):
    parts.append(f'{key} {json.dumps(value, default=str)}')
  return '; '.join(parts)

def table(rows):
  """The detailed results as Markdown: binding, value, category,
  result and detail."""
  lines = ['| Binding | Value | Category | Result | Detail |',
           '|---|---|---|---|---|']
  for row in rows:
    cells = [row['binding'], row.get('value', ''), row['category'],
             row['result'], row.get('detail', '')]
    lines.append('| ' + ' | '.join(str(cell).replace('|', '\\|')
                                   for cell in cells) + ' |')
  return '\n'.join(lines) + '\n'

def counts(rows):
  """{result: number of rows}, for the summary line."""
  found = {}
  for row in rows:
    found[row['result']] = found.get(row['result'], 0) + 1
  return found

# The probes each category's scene maps (the last one focused), and how a
# binding is pressed in it: 'tap' (modifiers held, the key or button
# tapped), 'hold' (the key tapped, the modifiers released 0.3 s later, as
# a switcher wants), 'drag' (the button held while the pointer moves to
# the other probe), 'resize' (the button held while it moves a quarter of
# the view to the left). `presses` is how often it is pressed, each
# compared on its own; `start` the workspace the scene starts on
# ('centre' of the grid or the first); `layout` a tiling layout.
SCENES = {
    'command': {'probes': 1},
    'close': {'probes': 2},
    'focus-click': {'probes': 2, 'pointer': 'first'},
    'vswitch': {'probes': 1, 'start': 'centre'},
    'wsets': {'probes': 1},
    'oswitch': {'probes': 1},
    'toggle': {'probes': 2, 'presses': 2},
    'expo-select': {'probes': 1, 'before': ('expo', 'toggle')},
    'switcher': {'probes': 3, 'press': 'hold'},
    'drag': {'probes': 2, 'press': 'drag'},
    'resize': {'probes': 2, 'press': 'resize'},
    'tile-focus': {'probes': 3, 'layout': True},
    'menu': {'probes': 1},
    'press': {'probes': 1},
    'unloaded': {'probes': 1},
}
# For key_focus_DIRECTION, the probe focused first in the layout
# [[a / b] | c].
FOCUS_START = {'left': 'rehearsal-c', 'right': 'rehearsal-a',
               'above': 'rehearsal-b', 'below': 'rehearsal-a'}

def scene_for(binding):
  """The scene of `binding`, with key_focus_DIRECTION's starting view."""
  scene = dict(SCENES.get(binding.category, SCENES['press']))
  if binding.category == 'tile-focus':
    direction = binding.name[len('key_focus_'):]
    scene['focus'] = FOCUS_START.get(direction)
  return scene

def press_steps(alternative, mode, centre=None, target=None):
  """The STIPC calls that press one binding alternative: [(method,
  data)] and ('sleep', seconds)."""
  modifiers, key = alternative
  held = [MODIFIER_KEYS.get(modifier, 'KEY_LEFTMETA')
          for modifier in modifiers]
  steps = [('stipc/feed_key', {'key': name, 'state': True}) for name in held]
  button = key is not None and key.startswith('BTN_')

  def tap(state):
    if button:
      return ('stipc/feed_button',
              {'combo': key, 'mode': 'press' if state else 'release'})
    return ('stipc/feed_key', {'key': key, 'state': state})
  if key is None:
    pass
  elif mode in ('drag', 'resize') and button and centre and target:
    steps.append(('stipc/move_cursor', {'x': centre[0], 'y': centre[1]}))
    steps.append(tap(True))
    for index in range(1, 9):
      x = centre[0] + (target[0] - centre[0]) * index // 8
      y = centre[1] + (target[1] - centre[1]) * index // 8
      steps.append(('stipc/move_cursor', {'x': x, 'y': y}))
      steps.append(('sleep', 0.03))
    steps.append(tap(False))
  else:
    steps.append(tap(True))
    steps.append(tap(False))
    if mode == 'hold':
      steps.append(('sleep', 0.3))
  steps += [('stipc/feed_key', {'key': name, 'state': False})
            for name in reversed(held)]
  return steps

def running_processes():
  """The PIDs in this PID namespace now."""
  return {int(entry) for entry in os.listdir('/proc') if entry.isdigit()}

def new_processes(before, ignored):
  """[(pid, comm)] of processes in this PID namespace that were not in
  `before` and are not `ignored`, other than this one."""
  found = []
  for entry in os.listdir('/proc'):
    if not entry.isdigit():
      continue
    pid = int(entry)
    if pid in before or pid in ignored or pid <= 1 or pid == os.getpid():
      continue
    comm = run.command_name(pid)
    if comm:
      found.append((pid, comm))
  return found

class Session:
  """One compositor under the rehearsal copy of the file, with its probe
  windows, records, IPC watcher and wayfire-shell client."""

  def __init__(self, kind, compositor, directory, wl_probe, mapping):
    self.kind = kind
    self.compositor = compositor
    self.records = os.path.join(directory, 'records')
    self.wl_probe = wl_probe
    self.mapping = mapping
    self.probes = {}
    self.ignored = {compositor.process.pid}
    self.before = set()
    self.wset_names = {}
    self.shell = None
    self.shell_outputs = {}
    self.watcher = xw.Ipc(compositor.ipc_path)
    self.watcher.call('window-rules/events/watch',
                      {'events': ['plugin-activation-state-changed']})

  def call(self, method, data=None):
    """One IPC request."""
    return self.compositor.call(method, data)

  def outputs(self):
    """`list-outputs` by name."""
    return {output['name']: output
            for output in self.call('window-rules/list-outputs')}

  def name_wsets(self):
    """Names each workspace set by the output it is on now."""
    self.wset_names = {output.get('wset-index'): f'wset of {name}'
                       for name, output in self.outputs().items()}

  def known_values(self):
    """The compositor's own values a child's environment holds, and their
    placeholders."""
    socket = self.compositor.client_environment().get('WAYLAND_DISPLAY')
    known = {self.compositor.ipc_path: '<ipc socket>',
             os.path.join(self.compositor.directory, 'wayfire.ini'):
             '<config file>'}
    if socket:
      known[socket] = '<wayland socket>'
      known[os.path.basename(socket)] = '<wayland socket>'
    display = self.call('stipc/get_xwayland_display')
    if isinstance(display, dict) and display.get('display'):
      known[display['display']] = '<x display>'
    return known

  def snapshot(self):
    """The comparable state now."""
    return snapshot_of(list(self.outputs().values()),
                       self.call('window-rules/list-views'),
                       self.call('window-rules/get-focused-output'),
                       self.call('window-rules/get-focused-view'),
                       self.wset_names)

  def settle(self, minimum=0.3):
    """Waits until the snapshot stops changing; returns it."""
    time.sleep(minimum)
    deadline = time.monotonic() + SETTLE_TIMEOUT
    last = self.snapshot()
    quiet_since = time.monotonic()
    while time.monotonic() < deadline:
      time.sleep(0.15)
      now = self.snapshot()
      if now != last:
        last, quiet_since = now, time.monotonic()
      elif time.monotonic() - quiet_since >= SETTLE_QUIET:
        break
    return last

  def activations(self):
    """The plugin activations heard since the last call, as sorted unique
    (plugin, state) pairs."""
    found = set()
    while True:
      try:
        event = self.watcher.receive(0.05)
      except TimeoutError:
        break
      if event.get('event') == 'plugin-activation-state-changed':
        found.add((event.get('plugin'), event.get('state')))
    return sorted(found)

  def records_now(self):
    """{name: record} of the records written so far."""
    found = {}
    for name in sorted(os.listdir(self.records)):
      if not name.endswith('.json'):
        continue
      try:
        with open(os.path.join(self.records, name)) as file:
          found[name[:-5]] = json.load(file)
      except (OSError, ValueError):
        continue
    return found

  def connect_shell(self):
    """A raw client bound to zwf_shell_manager_v2, for toggle_menu."""
    socket = self.compositor.client_environment().get('WAYLAND_DISPLAY')
    try:
      client = wayland_wire.Client(socket)
      client.roundtrip()
      versions = [version for interface, version in client.globals.values()
                  if interface == 'zwf_shell_manager_v2']
      if not versions:
        client.close()
        return
      manager = client.bind('zwf_shell_manager_v2', min(versions[0], 2))
      for name, output in client.outputs().items():
        self.shell_outputs[name] = client.request(
            manager, 'get_wf_output', output,
            wayland_wire.NewId('zwf_output_v2'))
      client.roundtrip()
      self.shell = client
    except (OSError, ValueError, KeyError, TimeoutError) as error:
      print(f'INFO rehearsal: {self.kind}: no wayfire-shell client: '
            f'{error}', flush=True)

  def menus(self):
    """{output: toggle_menu events so far}."""
    if self.shell is None:
      return {}
    self.shell.pump(0.2)
    return {name: self.shell.names_of(output).count('toggle_menu')
            for name, output in self.shell_outputs.items()}

  def map_probe(self, app_id):
    """Starts a `wl_probe` called `app_id` and waits for it to map;
    returns its view or None."""
    client = checks.Client(self.compositor,
                           [self.wl_probe, app_id, '--report-buttons'],
                           f'{self.kind}-{app_id}-{len(self.probes)}',
                           program='wl_probe')
    self.probes[app_id] = client
    self.ignored.add(client.process.pid)
    return xw.wait_for(lambda: self.mapped(app_id), timeout=15)

  def mapped(self, app_id):
    """The mapped view called `app_id`, or None."""
    views = [view for view in self.call('window-rules/list-views')
             if view.get('app-id') == app_id and view.get('mapped', True)]
    return views[0] if len(views) == 1 else None

  def probe_events(self, app_id, offset):
    """The key and button events probe `app_id` printed after `offset`
    characters of its output."""
    client = self.probes.get(app_id)
    if client is None:
      return []
    text = client.output()[offset:]
    return [(event['event'], event.get('key', event.get('button')),
             event.get('pressed'))
            for event in checks.probe_events(text, 'key')
            + checks.probe_events(text, 'button')]

  def output_lengths(self):
    """How much each probe printed so far."""
    return {app_id: len(client.output())
            for app_id, client in self.probes.items()}

  def clear(self):
    """Closes every view, ends the probes and any process launched by the
    compositor, and puts every output on its first workspace and
    the pointer on the first output."""
    for view in self.call('window-rules/list-views'):
      if view.get('role') == 'toplevel':
        self.call('window-rules/close-view', {'id': view['id']})
    for client in self.probes.values():
      if client.exited(3) is None:
        client.stop()
      else:
        client.log.close()
    self.probes = {}
    self.end_launched()
    xw.wait_for(lambda: not [view for view in
                             self.call('window-rules/list-views')
                             if view.get('role') == 'toplevel'], timeout=10)
    outputs = self.outputs()
    for output in outputs.values():
      self.call('vswitch/set-workspace',
                {'x': 0, 'y': 0, 'output-id': output['id']})
    self.focus_output(outputs.get('HEADLESS-1')
                      or next(iter(outputs.values())))
    self.activations()

  def focus_output(self, output):
    """Makes `output` the focused one: the pointer moves to its centre
    and clicks its empty background, which focuses an output in both
    compositors."""
    geometry = output['geometry']
    self.move_to((geometry['x'] + geometry['width'] // 2,
                  geometry['y'] + geometry['height'] // 2))
    self.call('stipc/feed_button', {'combo': 'BTN_LEFT', 'mode': 'full'})
    xw.wait_for(lambda: (self.call('window-rules/get-focused-output').get(
        'info') or {}).get('name') == output['name'], timeout=3)

  def end_launched(self):
    """Ends the processes that started after the compositor's start
    (`before`) and are not the compositor, its Xwayland or a probe, by
    their own comm."""
    xwayland = self.call('stipc/get_xwayland_pid')
    if isinstance(xwayland, dict) and isinstance(xwayland.get('pid'), int):
      self.ignored.add(xwayland['pid'])
    launched = [(pid, comm) for pid, comm in
                new_processes(self.before, self.ignored)
                if comm != 'Xwayland']
    for pid, comm in launched:
      run.signal_checked(pid, signal.SIGTERM, comm)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and any(
        run.command_name(pid) == comm for pid, comm in launched):
      time.sleep(0.1)
    for pid, comm in launched:
      run.signal_checked(pid, signal.SIGKILL, comm)

  def setup(self, scene, binding=None):
    """Sets up `scene`; returns whether its probes mapped."""
    self.clear()
    outputs = self.outputs()
    if scene.get('start') == 'centre':
      for output in outputs.values():
        grid = output['workspace']
        self.call('vswitch/set-workspace',
                  {'x': grid['grid_width'] // 2,
                   'y': grid['grid_height'] // 2,
                   'output-id': output['id']})
      time.sleep(0.5)
    for app_id in PROBES[:scene.get('probes', 1)]:
      if self.map_probe(app_id) is None:
        return False
      xw.wait_for(lambda app_id=app_id: (self.call(
          'window-rules/get-focused-view').get('info') or {}).get(
              'app-id') == app_id, timeout=5)
      time.sleep(0.2)
    if scene.get('layout'):
      self.set_layout()
    focus = scene.get('focus')
    if focus:
      view = self.mapped(focus)
      if view:
        self.call('window-rules/focus-view', {'id': view['id']})
    self.settle()
    pointer = self.mapped(PROBES[0] if scene.get('pointer') == 'first'
                          else self.focused_probe())
    if pointer:
      self.move_to(self.centre(pointer))
    return True

  def set_layout(self):
    """Tiles a, b and c as [[a / b] | c] on their workspace."""
    views = {app_id: self.mapped(app_id) for app_id in PROBES}
    if not all(views.values()):
      return
    first = views[PROBES[0]]
    output = self.outputs()[first['output-name']]
    self.call('simple-tile/set-layout', {
        'wset-index': first['wset-index'],
        'workspace': {'x': output['workspace']['x'],
                      'y': output['workspace']['y']},
        'layout': {'vertical-split': [
            {'weight': 1, 'horizontal-split': [
                {'weight': 1, 'view-id': views[PROBES[0]]['id']},
                {'weight': 1, 'view-id': views[PROBES[1]]['id']}]},
            {'weight': 1, 'view-id': views[PROBES[2]]['id']}]}})

  def focused_probe(self):
    """The focused view's app ID."""
    return (self.call('window-rules/get-focused-view').get('info')
            or {}).get('app-id')

  def centre(self, view):
    """The layout coordinates of `view`'s centre."""
    output = self.outputs()[view['output-name']]['geometry']
    geometry = view['geometry']
    return (output['x'] + geometry['x'] + geometry['width'] // 2,
            output['y'] + geometry['y'] + geometry['height'] // 2)

  def move_to(self, point):
    """Moves STIPC's pointer to `point`."""
    self.call('stipc/move_cursor', {'x': point[0], 'y': point[1]})

  def press(self, alternative, mode):
    """Presses one alternative in the current scene."""
    focused = self.mapped(self.focused_probe() or '')
    centre = target = None
    if focused is not None:
      centre = self.centre(focused)
      if mode == 'drag':
        others = [self.mapped(app_id) for app_id in self.probes
                  if app_id != focused['app-id']]
        others = [view for view in others if view]
        target = self.centre(others[0]) if others else (
            centre[0] + 200, centre[1] + 100)
      elif mode == 'resize':
        target = (centre[0] - focused['geometry']['width'] // 4, centre[1])
    for step in press_steps(alternative, mode, centre, target):
      if step[0] == 'sleep':
        time.sleep(step[1])
      else:
        self.call(*step)

  def observe(self, alternative, mode):
    """Presses one alternative and returns what changed."""
    before = self.snapshot()
    focused = before['focused-view']
    lengths = self.output_lengths()
    records = set(self.records_now())
    menus = self.menus()
    self.activations()
    self.press(alternative, mode)
    after = self.settle()
    observation = {'changes': diff(before, after)}
    activations = self.activations()
    if activations:
      observation['activations'] = activations
    if focused in self.probes:
      delivered = self.probe_events(focused, lengths.get(focused, 0))
      if delivered:
        observation['delivered'] = delivered
    known = self.known_values()
    launched = {name: record_summary(record, known)
                for name, record in self.records_now().items()
                if name not in records}
    if launched:
      observation['launched'] = launched
    after_menus = self.menus()
    toggled = {name: count - menus.get(name, 0)
               for name, count in after_menus.items()
               if count != menus.get(name, 0)}
    if toggled:
      observation['menu'] = toggled
    return {key: value for key, value in observation.items()
            if value not in ({}, [], None)}

  def close(self):
    """Ends the probes, the launched processes and the compositor;
    returns its exit status."""
    for client in self.probes.values():
      client.stop()
    self.probes = {}
    try:
      self.end_launched()
    except (OSError, ConnectionError, TimeoutError):
      pass
    if self.shell is not None:
      self.shell.close()
    self.watcher.close()
    return self.compositor.stop()

def binding_runs(session, items):
  """Observes every binding of `items` ([(Binding, alternative index)])
  in `session`; returns {(label, alternative, press): observation}."""
  observed = {}
  for binding, index in items:
    scene = scene_for(binding)
    alternative = binding.alternatives[index]
    if alternative[1] == 'other':
      continue
    try:
      if not session.setup(scene, binding):
        observed[(binding.label(), index, 0)] = {'error': 'probes not mapped'}
        continue
      before = scene.get('before')
      if before:
        pre = [item for item, _ in items
               if (item.section, item.name) == before]
        if pre:
          session.press(pre[0].alternatives[0], 'tap')
          session.settle()
      for press in range(scene.get('presses', 1)):
        observed[(binding.label(), index, press)] = session.observe(
            alternative, scene.get('press', 'tap'))
    except (OSError, ConnectionError, TimeoutError, KeyError) as error:
      observed[(binding.label(), index, 0)] = {'error': str(error)}
  return observed

def rule_runs(session, names):
  """Maps one probe for each of `names` on an empty first output; returns
  {name: the new view's state}."""
  observed = {}
  for name in names:
    try:
      session.setup({'probes': 0})
      before = session.snapshot()
      if session.map_probe(name) is None:
        observed[name] = {'error': 'did not map'}
        continue
      after = session.settle()
      observed[name] = {'changes': diff(before, after)}
    except (OSError, ConnectionError, TimeoutError, KeyError) as error:
      observed[name] = {'error': str(error)}
  return observed

def autostart_run(session, plan):
  """Waits for the autostart clients to map; returns their windows and
  each entry's record."""
  last = None
  quiet_since = started = time.monotonic()
  while time.monotonic() - started < AUTOSTART_TIMEOUT:
    views = sorted(session.snapshot()['views'].items())
    if views != last:
      last, quiet_since = views, time.monotonic()
    elif time.monotonic() - quiet_since > AUTOSTART_QUIET and (
        time.monotonic() - started > 15):
      break
    time.sleep(0.5)
  snapshot = session.snapshot()
  known = session.known_values()
  records = session.records_now()
  entries = {}
  for name, runs in plan.autostart:
    record = records.get(f'autostart-{name}')
    entries[name] = {'recorded': record is not None, 'runs': runs,
                     'record': record_summary(record, known)}
  shell = {name: name in records for name in ('wf-panel', 'wf-background')}
  windows = {app_id: {key: state[key] for key in
                      ('output', 'workspace', 'tiled', 'fullscreen')}
             for app_id, state in snapshot['views'].items()}
  return {'entries': entries, 'shell': shell, 'windows': windows}

def run_session(kind, start, plan, wl_probe, items, rule_names):
  """Starts one compositor with `start()`, runs everything and stops it;
  returns (observations, exit status) or (None, reason)."""
  before = running_processes()
  try:
    compositor = start()
  except (RuntimeError, OSError, ConnectionError, TimeoutError) as error:
    return None, str(error)
  directory = os.path.join(ROOT, kind)
  session = Session(kind, compositor, directory, wl_probe, plan.mapping)
  session.before = before
  result = {}
  try:
    session.name_wsets()
    result['autostart'] = autostart_run(session, plan)
    session.connect_shell()
    result['bindings'] = binding_runs(session, items)
    result['rules'] = rule_runs(session, rule_names)
  except (OSError, ConnectionError, TimeoutError, KeyError) as error:
    result['error'] = str(error)
  finally:
    status = session.close()
  return result, status

def explained(binding):
  """The reason a binding is not compared, or None."""
  return (EXPLAINED.get((binding.section, binding.name))
          or EXPLAINED.get((binding.section, '*')))

def compare(results, items, rules, shingle, wayfire):
  """Compares the two runs; records one check per binding, rule and the
  autostart, and returns the table rows."""
  rows = []
  auto_s = shingle.get('autostart', {})
  auto_w = wayfire.get('autostart', {})
  for name, entry in auto_s.get('entries', {}).items():
    other = auto_w.get('entries', {}).get(name)
    ok = entry == other and entry['recorded']
    rows.append({'binding': f'[autostart] {name}', 'category': 'autostart',
                 'result': 'match' if ok else 'DIFFERS',
                 'detail': ('runs in the namespace' if entry['runs'] else
                            'host service: recorded, not run')
                 + ('' if ok else f'; Shingle {entry}, Wayfire {other}')})
    results.check(f'rehearsal: [autostart] {name} is recorded with the '
                  'environment Wayfire gives it', ok,
                  '' if ok else 'see the table')
  ok = auto_s.get('shell') == auto_w.get('shell')
  rows.append({'binding': '[autostart] autostart_wf_shell',
               'category': 'autostart', 'result':
               'match' if ok else 'DIFFERS',
               'detail': f'Shingle {auto_s.get("shell")}, '
                         f'Wayfire {auto_w.get("shell")}'})
  results.check('rehearsal: autostart_wf_shell starts the same wf-shell '
                'stand-ins', ok, '' if ok else 'see the table')
  ok = close_enough(auto_s.get('windows'), auto_w.get('windows'))
  rows.append({'binding': 'autostart windows', 'category': 'autostart',
               'result': 'match' if ok else 'DIFFERS',
               'detail': f'Shingle {auto_s.get("windows")}'
                         + ('' if ok else
                            f', Wayfire {auto_w.get("windows")}')})
  results.check('rehearsal: the autostart clients map where Wayfire maps '
                'them', ok, '' if ok else 'see the table')
  observed_s = shingle.get('bindings', {})
  observed_w = wayfire.get('bindings', {})
  for binding in rules['explained']:
    rows.append({'binding': binding.label(), 'value': binding.value,
                 'category': binding.category, 'result': 'explained',
                 'detail': explained(binding)})
  for binding, index in items:
    label = binding.label()
    keys = sorted(key for key in set(observed_s) | set(observed_w)
                  if key[0] == label and key[1] == index)
    alternative = binding.alternatives[index]
    if alternative[1] == 'other':
      rows.append({'binding': label, 'value': binding.value,
                   'category': binding.category, 'result': 'explained',
                   'detail': 'a gesture or hotspot, which STIPC cannot '
                             'press'})
      continue
    for key in keys:
      mine, theirs = observed_s.get(key, {}), observed_w.get(key, {})
      result, reason = verdict(binding.section, binding.name, mine, theirs)
      press = f' press {key[2] + 1}' if key[2] else ''
      name = f'{label} alternative {index + 1}{press}'
      rows.append({'binding': name, 'value': binding.value,
                   'category': binding.category, 'result': result,
                   'detail': (describe(mine) if result == 'match' else
                              f'{reason}: Shingle {describe(mine)}; '
                              f'Wayfire {describe(theirs)}')})
      results.check(f'rehearsal: {name} does what Wayfire does', result
                    != 'DIFFERS', reason or '')
  rules_s, rules_w = shingle.get('rules', {}), wayfire.get('rules', {})
  for option_name, probe, reason in rules['window']:
    if probe is None:
      rows.append({'binding': f'[window-rules] {option_name}',
                   'category': 'window-rule', 'result': 'explained',
                   'detail': reason})
      continue
    ok, differing = same_effect(rules_s.get(probe, {}),
                                rules_w.get(probe, {}))
    rows.append({'binding': f'[window-rules] {option_name}',
                 'category': 'window-rule',
                 'result': 'match' if ok else 'DIFFERS',
                 'detail': describe(rules_s.get(probe)) if ok else
                 f'Shingle {describe(rules_s.get(probe))}; Wayfire '
                 f'{describe(rules_w.get(probe))}'})
    results.check(f'rehearsal: [window-rules] {option_name} has the effect '
                  'it has under Wayfire', ok, '' if ok else 'see the table')
  for probe in rules['tile']:
    result, reason = verdict('simple-tile', 'tile_by_default',
                             rules_s.get(probe, {}), rules_w.get(probe, {}))
    rows.append({'binding': f'[simple-tile] tile_by_default atom {probe}',
                 'category': 'tile-matcher', 'result': result,
                 'detail': describe(rules_s.get(probe)) if result == 'match'
                 else f'{reason}: Shingle {describe(rules_s.get(probe))}; '
                 f'Wayfire {describe(rules_w.get(probe))}'})
    results.check('rehearsal: a view matching an atom of tile_by_default '
                  'tiles or floats as under Wayfire', result != 'DIFFERS',
                  reason or '')
  return rows

def prepare_root(text, kind, real_home):
  """The rehearsal copy and its tools for compositor `kind`; returns
  (ini text, Plan, PATH)."""
  directory = os.path.join(ROOT, kind)
  ini, plan = rehearsal_ini(text, directory, real_home)
  bin_directory = write_tools(directory, plan)
  path = f'{bin_directory}:{os.environ.get("PATH", "/usr/bin:/bin")}'
  return ini, plan, path

def rehearsal_checks(results, binary, wl_probe, _captures):
  """The rehearsal of the file in SHINGLE_REHEARSAL_INI."""
  source = os.environ.get(INI_VARIABLE)
  if not source:
    print('INFO rehearsal: skipped: no --wayfire-ini given', flush=True)
    return
  import app_checks
  import screencast_checks
  app_checks.lower_priority()
  if not screencast_checks.mount_private_dbus_dir():
    raise RuntimeError('cannot cover /run/dbus')
  os.makedirs(ROOT, exist_ok=True)
  copy = os.path.join(ROOT, 'source.ini')
  shutil.copyfile(source, copy)
  os.chmod(copy, 0o600)
  with open(copy) as file:
    text = file.read()
  real_home = os.environ.get(HOME_VARIABLE) or os.path.dirname(
      os.path.dirname(os.path.abspath(source)))
  entries = parse_ini(text)
  configured = bindings(entries)
  loaded = set(plugins(entries))
  rules = {
      'window': window_rules(entries, loaded),
      'tile': tile_matcher_probes(entries, loaded),
      'explained': [binding for binding in configured
                    if binding.category == 'explained'],
  }
  only = re.compile(os.environ.get(ONLY_VARIABLE, ''))
  items = [(binding, index) for binding in configured
           if binding.category != 'explained' and only.search(binding.label())
           for index in range(len(binding.alternatives))]
  rules['window'] = [rule for rule in rules['window']
                     if only.search(f'[window-rules] {rule[0]}')]
  rules['tile'] = [probe for probe in rules['tile']
                   if only.search('[simple-tile] tile_by_default')]
  rule_names = list(dict.fromkeys(
      [probe for _, probe, _ in rules['window'] if probe] + rules['tile']))
  outputs = max(len(output_names(entries)), 1)
  print(f'INFO rehearsal: {len(configured)} bindings, '
        f'{len(rules["window"])} window rules, {outputs} outputs',
        flush=True)
  ini_s, plan_s, path_s = prepare_root(text, 'shingle', real_home)
  ini_w, plan_w, path_w = prepare_root(text, 'wayfire', real_home)
  shingle, status = run_session(
      'shingle', lambda: checks.Shingle(
          binary, 'rehearsal-shingle', None,
          backend=('--backend', 'offscreen', '--outputs', str(outputs)),
          extra_environment={'PATH': path_s}, ini=ini_s),
      plan_s, wl_probe, items, rule_names)
  if shingle is None:
    results.check('rehearsal: offscreen Shingle starts on the file', False,
                  status)
    return
  results.check('rehearsal: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')
  wayfire, status = run_session(
      'wayfire', lambda: wayfire_reference.Wayfire(
          'rehearsal-wayfire', ini_w, outputs=outputs,
          extra_environment={'PATH': path_w}),
      plan_w, wl_probe, items, rule_names)
  if wayfire is None:
    print(f'INFO rehearsal: no Wayfire to compare with: {status}',
          flush=True)
    return
  rows = compare(results, items, rules, shingle, wayfire)
  report = os.environ.get(REPORT_VARIABLE)
  if report:
    with open(report, 'w') as file:
      file.write(table(rows))
  summary = ', '.join(f'{count} {result}'
                      for result, count in sorted(counts(rows).items()))
  print(f'INFO rehearsal: {len(rows)} rows: {summary}', flush=True)

CHECKS = {
    'rehearsal': rehearsal_checks,
}
