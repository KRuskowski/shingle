"""Tests for rehearsal_checks.py's helpers on an invented wayfire.ini;
nothing here starts a compositor or a client."""

import json
import os
import shlex
import subprocess
import sys
import tempfile
import time
import unittest

import rehearsal_checks as rehearsal

INVENTED = r"""[core]
plugins = command vswitch simple-tile window-rules wm-actions
close_top_view = <super> KEY_X | <alt> KEY_F9
vwidth = 2

[autostart]
bus = busctl --user list
term = foot --app-id probe-term --config ~/invented/foot.ini
later = sh -c 'true; wpctl set-volume invented 0.5'
autostart_wf_shell = false

[command]
binding_term = <super> KEY_ENTER
command_term = foot
binding_nothing = <ctrl> KEY_F12
command_nothing =
release_binding_menu = <super>
command_menu = fuzzel \# not a comment

[output:eDP-9]
mode = 1920x1080@144000

[output:DP-7]
scale = 2.0

[vswitch]
binding_left = <super> <alt> KEY_LEFT
binding_3 = <super> KEY_3
binding_down = none

[window-rules]
first = on created if app_id is "probe-term" then start_on_output "DP-7"
; second = on created if title contains "Editor" then maximize
third = on maximized if app_id is "x" then minimize # trailing

[grid]
slot_c = <super> KEY_KP5

[simple-tile]
tile_by_default = !(app_id is "float-me")
key_focus_left = <super> KEY_LEFT
button_move = <super> BTN_LEFT

[wm-actions]
toggle_fullscreen = <super> KEY_F
minimize = disabled

[wayfire-shell]
toggle_menu = <super>
"""
TYPES = {
    ('core', 'close_top_view'): 'activator',
    ('core', 'vwidth'): 'int',
    ('vswitch', 'binding_left'): 'activator',
    ('vswitch', 'binding_down'): 'activator',
    ('grid', 'slot_c'): 'activator',
    ('simple-tile', 'key_focus_left'): 'key',
    ('simple-tile', 'button_move'): 'button',
    ('simple-tile', 'tile_by_default'): 'string',
    ('wm-actions', 'toggle_fullscreen'): 'activator',
    ('wm-actions', 'minimize'): 'activator',
    ('wayfire-shell', 'toggle_menu'): 'activator',
}

class IniTest(unittest.TestCase):
  """wf-config's reading of the file."""

  def setUp(self):
    self.entries = rehearsal.parse_ini(INVENTED)

  def test_comments_escapes_and_semicolon_lines(self):
    self.assertEqual(rehearsal.option(self.entries, 'command', 'command_menu'),
                     'fuzzel # not a comment')
    self.assertEqual(rehearsal.option(self.entries, 'window-rules', 'third'),
                     'on maximized if app_id is "x" then minimize')
    # wf-config has no `;` comments: the line is an option of that name.
    self.assertIsNotNone(
        rehearsal.option(self.entries, 'window-rules', '; second'))
    self.assertEqual(rehearsal.option(self.entries, 'command',
                                      'command_nothing'), '')

  def test_continuation_lines_join(self):
    entries = rehearsal.parse_ini('[a]\nkey = one \\\n two\n')
    self.assertEqual(entries, [('a', 'key', 'one  two')])

  def test_options_before_a_section_are_dropped(self):
    self.assertEqual(rehearsal.parse_ini('key = 1\n[a]\nb = 2\n'),
                     [('a', 'b', '2')])

  def test_plugins_and_outputs(self):
    self.assertEqual(rehearsal.plugins(self.entries)[:2],
                     ['command', 'vswitch'])
    self.assertEqual(rehearsal.output_mapping(self.entries),
                     {'eDP-9': 'HEADLESS-1', 'DP-7': 'HEADLESS-2'})

  def test_outputs_are_renamed_as_whole_words(self):
    text = rehearsal.rename_outputs('[output:DP-7]\na "DP-7" DP-70 xDP-7',
                                    {'DP-7': 'HEADLESS-2'})
    self.assertEqual(text, '[output:HEADLESS-2]\na "HEADLESS-2" DP-70 xDP-7')

  def test_the_option_table_types_bindings(self):
    types = rehearsal.option_types()
    self.assertEqual(types[('expo', 'toggle')], 'activator')
    self.assertEqual(types[('move', 'activate')], 'button')

class BindingTest(unittest.TestCase):
  """Which options are bindings and how they are pressed."""

  def test_binding_values(self):
    self.assertEqual(rehearsal.parse_binding('<super> KEY_X | <alt> KEY_F9'),
                     [(['super'], 'KEY_X'), (['alt'], 'KEY_F9')])
    self.assertEqual(rehearsal.parse_binding('<super> '), [(['super'], None)])
    self.assertEqual(rehearsal.parse_binding('BTN_LEFT | BTN_RIGHT'),
                     [([], 'BTN_LEFT'), ([], 'BTN_RIGHT')])
    self.assertEqual(rehearsal.parse_binding('swipe up 3'), [([], 'other')])
    for value in ('', 'none', 'disabled', ' none '):
      self.assertEqual(rehearsal.parse_binding(value), [])

  def test_dynamic_list_entries(self):
    self.assertTrue(rehearsal.dynamic_binding('command', 'binding_term'))
    self.assertTrue(rehearsal.dynamic_binding('command',
                                              'release_binding_menu'))
    self.assertFalse(rehearsal.dynamic_binding('command', 'command_term'))
    self.assertTrue(rehearsal.dynamic_binding('vswitch', 'binding_3'))
    self.assertFalse(rehearsal.dynamic_binding('vswitch', 'binding_left'))
    self.assertTrue(rehearsal.dynamic_binding('wsets', 'send_to_wset_2'))

  def test_configured_bindings_and_categories(self):
    found = rehearsal.bindings(rehearsal.parse_ini(INVENTED), TYPES)
    by_label = {binding.label(): binding for binding in found}
    self.assertEqual(
        {label: binding.category for label, binding in by_label.items()},
        {'[core] close_top_view': 'close',
         '[command] binding_term': 'command',
         '[command] binding_nothing': 'command',
         '[command] release_binding_menu': 'command',
         '[vswitch] binding_left': 'vswitch',
         '[vswitch] binding_3': 'vswitch',
         '[grid] slot_c': 'unloaded',
         '[simple-tile] key_focus_left': 'tile-focus',
         '[simple-tile] button_move': 'drag',
         '[wm-actions] toggle_fullscreen': 'toggle',
         '[wayfire-shell] toggle_menu': 'unloaded'})
    self.assertFalse(by_label['[grid] slot_c'].loaded)
    self.assertEqual(len(by_label['[core] close_top_view'].alternatives), 2)

  def test_a_later_value_wins(self):
    entries = rehearsal.parse_ini('[core]\nplugins = expo\n[expo]\n'
                                  'toggle = <super> KEY_A\n'
                                  'toggle = <super> KEY_V\n')
    found = rehearsal.bindings(entries, {('expo', 'toggle'): 'activator'})
    self.assertEqual([binding.alternatives for binding in found],
                     [[(['super'], 'KEY_V')]])

  def test_categories_of_loaded_plugins(self):
    self.assertEqual(rehearsal.category('core', 'exit', True), 'explained')
    self.assertEqual(rehearsal.category('expo', 'select_workspace_2', True),
                     'expo-select')
    self.assertEqual(rehearsal.category('fast-switcher', 'activate', True),
                     'switcher')
    self.assertEqual(rehearsal.category('scale', 'toggle', True), 'toggle')
    self.assertEqual(rehearsal.category('scale', 'toggle', False),
                     'unloaded')
    self.assertEqual(rehearsal.category('invented', 'go', True), 'press')

  def test_scenes(self):
    binding = rehearsal.Binding('simple-tile', 'key_focus_above', '', [],
                                'tile-focus', True)
    scene = rehearsal.scene_for(binding)
    self.assertEqual(scene['focus'], 'rehearsal-b')
    self.assertTrue(scene['layout'])
    self.assertEqual(rehearsal.scene_for(rehearsal.Binding(
        'x', 'y', '', [], 'toggle', True))['presses'], 2)

  def test_a_tap_holds_its_modifiers_around_the_key(self):
    steps = rehearsal.press_steps((['super', 'shift'], 'KEY_O'), 'tap')
    self.assertEqual([(data['key'], data['state']) for _, data in steps],
                     [('KEY_LEFTMETA', True), ('KEY_LEFTSHIFT', True),
                      ('KEY_O', True), ('KEY_O', False),
                      ('KEY_LEFTSHIFT', False), ('KEY_LEFTMETA', False)])

  def test_a_modifier_alone_is_tapped(self):
    steps = rehearsal.press_steps((['super'], None), 'tap')
    self.assertEqual(steps, [
        ('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': True}),
        ('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': False})])

  def test_a_switcher_keeps_its_modifier_a_while(self):
    steps = rehearsal.press_steps((['alt'], 'KEY_TAB'), 'hold')
    self.assertEqual(steps[3], ('sleep', 0.3))
    self.assertEqual(steps[-1][1], {'key': 'KEY_LEFTALT', 'state': False})

  def test_a_drag_moves_with_the_button_held(self):
    steps = rehearsal.press_steps((['super'], 'BTN_LEFT'), 'drag',
                                  (100, 100), (300, 200))
    methods = [step[0] for step in steps]
    self.assertEqual(steps[1], ('stipc/move_cursor', {'x': 100, 'y': 100}))
    self.assertEqual(steps[2], ('stipc/feed_button',
                                {'combo': 'BTN_LEFT', 'mode': 'press'}))
    self.assertIn(('stipc/move_cursor', {'x': 300, 'y': 200}), steps)
    self.assertLess(methods.index('stipc/move_cursor'),
                    len(methods) - methods[::-1].index('stipc/feed_button'))
    self.assertEqual(steps[-1][1], {'key': 'KEY_LEFTMETA', 'state': False})

class RuleTest(unittest.TestCase):
  """The window rules and tiling atoms the rehearsal exercises."""

  def test_window_rules(self):
    entries = rehearsal.parse_ini(INVENTED)
    rules = rehearsal.window_rules(entries, {'window-rules'})
    self.assertEqual([(name, probe) for name, probe, _ in rules],
                     [('first', 'probe-term'), ('; second', 'Editor'),
                      ('third', None)])
    self.assertEqual(rehearsal.window_rules(entries, set()), [])

  def test_tile_matcher_atoms(self):
    entries = rehearsal.parse_ini(INVENTED)
    self.assertEqual(rehearsal.tile_matcher_probes(entries, {'simple-tile'}),
                     ['float-me'])
    self.assertEqual(rehearsal.tile_matcher_probes(entries, set()), [])

class CopyTest(unittest.TestCase):
  """The harness-safe copy of the file."""

  def setUp(self):
    self.home = tempfile.TemporaryDirectory()
    os.makedirs(os.path.join(self.home.name, 'invented'))
    with open(os.path.join(self.home.name, 'invented', 'foot.ini'),
              'w') as file:
      file.write('private\n')
    self.text, self.plan = rehearsal.rehearsal_ini(INVENTED, '/tmp/r/x',
                                                   self.home.name)
    self.entries = rehearsal.parse_ini(self.text)

  def tearDown(self):
    self.home.cleanup()

  def test_host_services(self):
    self.assertTrue(rehearsal.host_service('busctl --user list'))
    self.assertTrue(rehearsal.host_service(
        "sh -c 'true; wpctl set-volume invented 0.5'"))
    self.assertTrue(rehearsal.host_service('/usr/bin/systemctl is-active x'))
    self.assertTrue(rehearsal.host_service('gdbus introspect --session'))
    self.assertFalse(rehearsal.host_service('foot --app-id x'))
    self.assertFalse(rehearsal.host_service('mypactl'))

  def test_outputs_are_headless(self):
    sections = {section for section, _, _ in self.entries}
    self.assertIn('output:HEADLESS-1', sections)
    self.assertIn('output:HEADLESS-2', sections)
    self.assertNotIn('output:DP-7', sections)
    self.assertIn('"HEADLESS-2"',
                  rehearsal.option(self.entries, 'window-rules', 'first'))

  def test_plugins_gain_ipc_and_stipc(self):
    self.assertEqual(rehearsal.plugins(self.entries)[-2:], ['ipc', 'stipc'])

  def test_autostart_is_recorded_and_host_services_never_run(self):
    self.assertEqual(self.plan.autostart,
                     [('bus', False), ('term', True), ('later', False)])
    bus = shlex.split(rehearsal.option(self.entries, 'autostart', 'bus'))
    self.assertEqual(bus[:4], ['/tmp/r/x/bin/record', '--no-exec',
                               '/tmp/r/x/records', 'autostart-bus'])
    term = shlex.split(rehearsal.option(self.entries, 'autostart', 'term'))
    self.assertEqual(term[:3], ['/tmp/r/x/bin/record', '/tmp/r/x/records',
                                'autostart-term'])
    self.assertEqual(term[3:5], ['/bin/sh', '-c'])
    self.assertEqual(shlex.split(term[5]),
                     ['foot', '--app-id', 'probe-term', '--config',
                      '/tmp/r/x/stand-ins/1/path-0'])
    self.assertEqual(self.plan.stand_ins,
                     [('/tmp/r/x/stand-ins/1/path-0', 'file')])
    self.assertEqual(rehearsal.option(self.entries, 'autostart',
                                      'autostart_wf_shell'), 'false')

  def test_commands_are_only_recorded(self):
    term = shlex.split(rehearsal.option(self.entries, 'command',
                                        'command_term'))
    self.assertEqual(term, ['/tmp/r/x/bin/record', '--no-exec',
                            '/tmp/r/x/records', 'command_term', '/bin/sh',
                            '-c', 'foot'])
    menu = rehearsal.option(self.entries, 'command', 'command_menu')
    self.assertTrue(menu.endswith("'fuzzel # not a comment'"))
    self.assertEqual(rehearsal.option(self.entries, 'command',
                                      'command_nothing'), '')
    self.assertEqual(rehearsal.option(self.entries, 'command',
                                      'binding_term'), '<super> KEY_ENTER')

  def test_session_files_and_missing_paths(self):
    os.makedirs(os.path.join(self.home.name, 'work'))
    command, made = rehearsal.stand_in_command(
        'kitty --session ~/invented/foot.ini --cwd ~/work ~/gone',
        '/s', self.home.name)
    self.assertEqual(shlex.split(command),
                     ['kitty', '--session', '/s/path-0', '--cwd',
                      '/s/path-1', '~/gone'])
    self.assertEqual(made, [('/s/path-0', 'session'),
                            ('/s/path-1', 'directory')])
    self.assertEqual(rehearsal.stand_in_command('foot', '/s',
                                                self.home.name),
                     ('foot', []))

  def test_tools_are_written(self):
    with tempfile.TemporaryDirectory() as directory:
      plan = rehearsal.Plan()
      plan.stand_ins = [(os.path.join(directory, 's', 'path-0'), 'session'),
                        (os.path.join(directory, 's', 'path-1'),
                         'directory')]
      bin_directory = rehearsal.write_tools(directory, plan)
      with open(os.path.join(directory, 's', 'path-0')) as file:
        self.assertEqual(file.read(), rehearsal.SESSION_STAND_IN)
      self.assertTrue(os.path.isdir(os.path.join(directory, 's', 'path-1')))
      for name in ('record', 'wf-panel', 'wf-background'):
        self.assertTrue(os.access(os.path.join(bin_directory, name),
                                  os.X_OK))

  def test_the_recorder_records_without_running(self):
    with tempfile.TemporaryDirectory() as directory:
      recorder = os.path.join(directory, 'record')
      with open(recorder, 'w') as file:
        file.write(rehearsal.RECORDER)
      subprocess.run([sys.executable, recorder, '--no-exec', directory,
                      'invented', '/bin/false'], check=True,
                     env={'WAYLAND_DISPLAY': 'wayland-9'})
      with open(os.path.join(directory, 'invented.json')) as file:
        record = json.load(file)
      self.assertEqual(record['command'], ['/bin/false'])
      self.assertFalse(record['executed'])
      self.assertEqual(record['environment']['WAYLAND_DISPLAY'],
                       'wayland-9')
      self.assertEqual(sorted(record['process']),
                       ['SigBlk', 'SigIgn', 'nofile'])

  def test_the_recorder_runs_its_command_only_after_its_gate(self):
    with tempfile.TemporaryDirectory() as directory:
      recorder = os.path.join(directory, 'record')
      with open(recorder, 'w') as file:
        file.write(rehearsal.RECORDER)
      ran = os.path.join(directory, 'ran')
      process = subprocess.Popen([sys.executable, recorder, directory,
                                  'gated', '/bin/sh', '-c', f'touch {ran}'])
      try:
        record = os.path.join(directory, 'gated.json')
        for _ in range(200):
          if os.path.exists(record):
            break
          time.sleep(0.02)
        self.assertTrue(os.path.exists(record))
        time.sleep(0.2)
        self.assertFalse(os.path.exists(ran))
        open(os.path.join(directory, 'gated.go'), 'w').close()
        self.assertEqual(process.wait(10), 0)
        self.assertTrue(os.path.exists(ran))
      finally:
        if process.poll() is None:
          process.kill()
          process.wait()

class FakeSession:
  """Views that appear when a gate opens, on a fake clock."""

  def __init__(self, directory, maps):
    self.records = directory
    self.maps = maps
    self.now = 0.0
    self.order = []

  def clock(self):
    return self.now

  def pause(self, seconds):
    self.now += seconds
    for name, count in self.maps.items():
      if (os.path.exists(os.path.join(self.records, f'autostart-{name}.go'))
          and name not in self.order):
        self.order.append(name)
    for name in list(self.maps):
      if not os.path.exists(os.path.join(self.records,
                                         f'autostart-{name}.json')):
        open(os.path.join(self.records, f'autostart-{name}.json'),
             'w').close()

  def snapshot(self):
    views = {}
    for name in self.order:
      for index in range(self.maps[name]):
        views[f'{name}-{index}'] = {}
    return {'views': views}

class OrderTest(unittest.TestCase):
  """The autostart clients start one after another."""

  def test_entries_start_in_file_order_after_the_one_before_mapped(self):
    with tempfile.TemporaryDirectory() as directory:
      session = FakeSession(directory, {'first': 1, 'never': 0,
                                        'second': 2})
      plan = rehearsal.Plan()
      plan.autostart = [('bus', False), ('first', True), ('never', True),
                        ('second', True)]
      released = rehearsal.release_in_order(session, plan, session.clock,
                                            session.pause)
      self.assertEqual(released, ['first', 'never', 'second'])
      self.assertEqual(session.order, ['first', 'never', 'second'])
      self.assertFalse(os.path.exists(os.path.join(directory,
                                                   'autostart-bus.go')))
      # The entry that maps nothing waits out its timeout; the others
      # only their quiet time.
      self.assertLess(session.now, 2 * rehearsal.ENTRY_TIMEOUT)
      self.assertGreater(session.now, rehearsal.ENTRY_TIMEOUT)

class CompareTest(unittest.TestCase):
  """Snapshots, their differences and the comparison."""

  OUTPUTS = [{'name': 'HEADLESS-1', 'wset-index': 4,
              'geometry': {'x': 0, 'y': 0, 'width': 1000, 'height': 500},
              'workspace': {'x': 1, 'y': 0}}]

  def view(self, app_id, x, **extra):
    view = {'app-id': app_id, 'role': 'toplevel', 'mapped': True,
            'output-name': 'HEADLESS-1', 'wset-index': 4,
            'geometry': {'x': x, 'y': 0, 'width': 500, 'height': 500},
            'tiled-edges': 15, 'fullscreen': False, 'minimized': False,
            'sticky': False, 'layer': 'workspace'}
    view.update(extra)
    return view

  def snapshot(self, views, focused='a'):
    return rehearsal.snapshot_of(
        self.OUTPUTS, views, {'info': {'name': 'HEADLESS-1'}},
        {'info': {'app-id': focused}}, {4: 'wset of HEADLESS-1'})

  def test_views_are_placed_on_workspaces(self):
    snapshot = self.snapshot([self.view('a', 0), self.view('b', 1200)])
    self.assertEqual(snapshot['views']['a']['workspace'], (1, 0))
    self.assertEqual(snapshot['views']['b']['workspace'], (2, 0))
    self.assertEqual(snapshot['views']['a']['wset'], 'wset of HEADLESS-1')
    self.assertEqual(snapshot['wsets'], {'HEADLESS-1': 'wset of HEADLESS-1'})

  def test_unmapped_and_unnamed_views_are_left_out(self):
    snapshot = self.snapshot([self.view('a', 0, mapped=False),
                              self.view('', 0)])
    self.assertEqual(snapshot['views'], {})

  def test_changes(self):
    before = self.snapshot([self.view('a', 0), self.view('b', 500)])
    after = self.snapshot([self.view('a', 0, fullscreen=True)], focused='b')
    self.assertEqual(rehearsal.diff(before, after), {
        'focused-view': ('a', 'b'),
        'view a: fullscreen': (False, True),
        'view b': (before['views']['b'], 'absent')})
    self.assertEqual(rehearsal.diff(before, before), {})

  def test_geometry_within_the_tolerance_matches(self):
    first = {'changes': {'view a: geometry': ((0, 0, 500, 500),
                                              (10, 0, 490, 500))}}
    second = {'changes': {'view a: geometry': ((0, 0, 500, 500),
                                               (12, 1, 488, 500))}}
    third = {'changes': {'view a: geometry': ((0, 0, 500, 500),
                                              (30, 0, 470, 500))}}
    self.assertEqual(rehearsal.same_effect(first, second), (True, []))
    self.assertEqual(rehearsal.same_effect(first, third),
                     (False, ['changes']))
    self.assertEqual(rehearsal.same_effect({}, {'delivered': [1]}),
                     (False, ['delivered']))

  def test_environments_are_compared_by_placeholder(self):
    record = {'environment': {'WAYLAND_DISPLAY': 'wayland-1',
                              'WAYFIRE_SOCKET': '/tmp/w/ipc',
                              'HOME': '/tmp/w/home'},
              'process': {'SigBlk': '0', 'SigIgn': '0', 'nofile': [1, 2]}}
    summary = rehearsal.record_summary(
        record, {'wayland-1': '<wayland socket>',
                 '/tmp/w/ipc': '<ipc socket>'})
    self.assertEqual(summary['environment']['WAYLAND_DISPLAY'],
                     '<wayland socket>')
    self.assertEqual(summary['environment']['WAYFIRE_SOCKET'],
                     '<ipc socket>')
    self.assertIsNone(summary['environment']['DISPLAY'])
    self.assertNotIn('HOME', summary['environment'])
    self.assertEqual(summary['process']['nofile'], [1, 2])
    self.assertIsNone(rehearsal.record_summary(None, {}))

  def test_verdicts(self):
    same = {'changes': {'focused-view': ['a', 'b']}}
    self.assertEqual(rehearsal.verdict('x', 'y', same, same),
                     ('match', None))
    result, reason = rehearsal.verdict('expo', 'toggle', {'delivered': [1]},
                                       {})
    self.assertEqual(result, 'explained')
    self.assertIn('grab', reason)
    self.assertEqual(rehearsal.verdict('x', 'y', {'delivered': [1]}, {})[0],
                     'DIFFERS')
    # A geometry that differs is no longer explained: since
    # `shingle-initial-size` a view floats at its own size, as in Wayfire.
    floated = {'changes': {'view b: tiled': [True, False],
                           'view b: geometry': [[0, 0, 9, 9], [1, 1, 3, 3]]}}
    other = {'changes': {'view b: tiled': [True, False],
                         'view b: geometry': [[0, 0, 9, 9], [5, 5, 2, 2]]}}
    self.assertEqual(rehearsal.verdict('simple-tile', 'key_toggle', floated,
                                       other)[0], 'DIFFERS')

  def test_table_and_counts(self):
    rows = [{'binding': '[a] b', 'value': '<super> KEY_A | KEY_B',
             'category': 'press', 'result': 'match'},
            {'binding': '[a] c', 'category': 'press', 'result': 'DIFFERS',
             'detail': 'x'}]
    text = rehearsal.table(rows)
    self.assertIn('| [a] b | <super> KEY_A \\| KEY_B | press | match |  |',
                  text)
    self.assertEqual(rehearsal.counts(rows), {'match': 1, 'DIFFERS': 1})
    self.assertEqual(rehearsal.describe({}), 'no effect')

if __name__ == '__main__':
  unittest.main()
