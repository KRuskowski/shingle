"""Tests for generate.py, on a sample of Wayfire's metadata."""

import os
import tempfile
import unittest
import xml.etree.ElementTree as ElementTree

import generate

# A sample in the shape of wayfire:metadata/*.xml: a plugin with a plain
# option, an escaped default, a grouped option and a dynamic list, and an
# object.
SAMPLE = """<?xml version="1.0"?>
<wayfire>
  <plugin name="vswitch">
    <_short>Viewport Switcher</_short>
    <category>Desktop</category>
    <option name="binding_left" type="activator">
      <_short>Left</_short>
      <default>&lt;super&gt; &lt;alt&gt; KEY_LEFT</default>
    </option>
    <option name="binding_last" type="activator">
      <default></default>
    </option>
    <group>
      <_short>Animation</_short>
      <option name="duration" type="animation">
        <default>300ms circle</default>
        <min>0</min>
      </option>
    </group>
    <option name="workspace_bindings" type="dynamic-list" type-hint="dict">
      <entry prefix="binding_" type="activator"/>
    </option>
  </plugin>
  <object name="output">
    <option name="mode" type="string">
      <default>auto</default>
      <desc><value>off</value></desc>
    </option>
  </object>
</wayfire>
"""
# The checked-in table.
TABLE = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..',
                     'crates', 'shingle-config', 'data',
                     'wayfire-options.tsv')

def sample_options():
  """The options of SAMPLE."""
  return generate.options_of(ElementTree.fromstring(SAMPLE))

class OptionsTest(unittest.TestCase):
  """Reading options out of metadata."""

  def test_every_option_with_its_type_and_default(self):
    self.assertEqual(sample_options(), [
        generate.Option('vswitch', 'binding_left', 'activator',
                        '<super> <alt> KEY_LEFT'),
        generate.Option('vswitch', 'binding_last', 'activator', ''),
        generate.Option('vswitch', 'duration', 'animation', '300ms circle'),
        generate.Option('vswitch', 'workspace_bindings', 'dynamic-list', '',
                        ('binding_',)),
        generate.Option('output', 'mode', 'string', 'auto'),
    ])

  def test_a_default_with_a_line_break_is_refused(self):
    root = ElementTree.fromstring(
        '<wayfire><plugin name="a"><option name="b" type="string">'
        '<default>one\ntwo</default></option></plugin></wayfire>')
    with self.assertRaises(ValueError):
      generate.options_of(root)

  def test_an_option_without_a_default_has_an_empty_one(self):
    root = ElementTree.fromstring(
        '<wayfire><plugin name="a"><option name="b" type="int"/></plugin>'
        '</wayfire>')
    self.assertEqual(generate.options_of(root)[0].default, '')

class TableTest(unittest.TestCase):
  """Writing and reading the table."""

  def write(self, directory, name, text):
    path = os.path.join(directory, name)
    with open(path, 'w') as file:
      file.write(text)
    return path

  def test_collect_sorts_and_merges_duplicate_sections(self):
    with tempfile.TemporaryDirectory() as directory:
      first = self.write(directory, 'a.xml', SAMPLE)
      second = self.write(directory, 'b.xml', SAMPLE)
      options = generate.collect([first, second])
    self.assertEqual([(option.section, option.name) for option in options], [
        ('output', 'mode'),
        ('vswitch', 'binding_last'),
        ('vswitch', 'binding_left'),
        ('vswitch', 'duration'),
        ('vswitch', 'workspace_bindings'),
    ])

  def test_collect_refuses_conflicting_definitions(self):
    with tempfile.TemporaryDirectory() as directory:
      first = self.write(directory, 'a.xml', SAMPLE)
      second = self.write(directory, 'b.xml',
                          SAMPLE.replace('300ms circle', '100'))
      with self.assertRaises(ValueError):
        generate.collect([first, second])

  def test_render_and_parse_round_trip(self):
    options = sample_options()
    text = generate.render(options, 'abc123')
    self.assertIn('WayfireWM/wayfire abc123', text)
    self.assertIn('vswitch\tworkspace_bindings\tdynamic-list\t\tbinding_*\n',
                  text)
    self.assertEqual(generate.parse(text), options)

  def test_an_empty_prefix_survives(self):
    option = generate.Option('autostart', 'autostart', 'dynamic-list', '',
                             ('',))
    self.assertEqual(generate.parse(generate.render([option], 'x')),
                     [option])

  def test_main_writes_the_table(self):
    with tempfile.TemporaryDirectory() as directory:
      self.write(directory, 'vswitch.xml', SAMPLE)
      output = os.path.join(directory, 'table.tsv')
      status = generate.main(['--metadata', directory, '--commit', 'abc',
                              '--output', output])
      with open(output) as file:
        options = generate.parse(file.read())
    self.assertEqual(status, 0)
    self.assertEqual(len(options), 5)

  def test_the_checked_in_table_matches_the_pinned_metadata(self):
    # What the design's survey counted at the pinned commit: 332 options,
    # with Wayfire's defaults for the values WG-2 depends on.
    with open(TABLE) as file:
      text = file.read()
    self.assertIn('669bbe3647e798898982a2ff2cb172b6f626c12b', text)
    options = {(option.section, option.name): option
               for option in generate.parse(text)}
    self.assertEqual(len(options), 332)
    self.assertEqual(options[('core', 'vwidth')].default, '3')
    self.assertEqual(options[('core', 'exit')].default,
                     '<ctrl> <alt> KEY_BACKSPACE')
    self.assertEqual(options[('wm-actions', 'toggle_sticky')].default,
                     'disabled')
    self.assertEqual(options[('animate', 'minimize_animation')].default,
                     'squeezimize')
    self.assertEqual(options[('command', 'bindings')].entries,
                     ('command_', 'binding_'))
    self.assertNotIn('simple-tile',
                     options[('core', 'plugins')].default.split())

if __name__ == '__main__':
  unittest.main()
