"""Unit tests of drm_info_fixture.py on invented drm_info objects.

They only write into a temporary directory, so they are safe anywhere.
"""

import json
import os
import tempfile
import unittest

import drm_info_fixture

def card():
  """A small drm_info object: an eDP panel and two DP connectors, one
  CRTC and a primary plane, in drm_info's layout."""
  mode = {'clock': 148500, 'hdisplay': 1920, 'hsync_start': 2008,
          'hsync_end': 2052, 'htotal': 2200, 'hskew': 0, 'vdisplay': 1080,
          'vsync_total': 1084, 'vsync_end': 1089, 'vtotal': 1125,
          'vscan': 0, 'vrefresh': 60, 'flags': 10, 'type': 72,
          'name': '1920x1080'}
  edid = {'id': 1, 'type': 16, 'spec': None, 'value': None,
          'data': '00ffffffffffff00serial'}
  bpc = {'id': 2, 'type': 2, 'spec': {'min': 6, 'Max': 12}, 'value': 12}
  return {
      'driver': {'name': 'i915', 'kernel': {'release': '6.12.0'},
                 'caps': {'CURSOR_WIDTH': 256, 'CURSOR_HEIGHT': 256,
                          'ASYNC_PAGE_FLIP': 1, 'PRIME': 3}},
      'connectors': [
          {'id': 10, 'type': 14, 'status': 1, 'phy_width': 310,
           'phy_height': 170, 'subpixel': 1, 'encoders': [11],
           'modes': [mode],
           'properties': {'EDID': edid, 'max bpc': bpc,
                          'CRTC_ID': {'type': 64, 'value': 20}}},
          {'id': 12, 'type': 10, 'status': 2, 'encoders': [11],
           'modes': [], 'properties': {'PATH': {
               'type': 16, 'value': None, 'data': 'mst:12-1'}}},
          {'id': 13, 'type': 10, 'status': 2, 'encoders': [11],
           'modes': [], 'properties': {}},
      ],
      'encoders': [{'id': 11, 'type': 2, 'possible_crtcs': 1}],
      'crtcs': [{'id': 20, 'mode': mode, 'properties': {
          'ACTIVE': {'type': 2, 'spec': {'min': 0, 'Max': 1}, 'value': 1},
          'GAMMA_LUT': {'type': 16, 'value': None}}}],
      'planes': [{'id': 30, 'possible_crtcs': 1, 'crtc_id': 20,
                  'fb_id': 99, 'formats': [875713112],
                  'properties': {
                      'type': {'type': 8, 'value': 1},
                      'IN_FORMATS': {'type': 16, 'value': None, 'data': [
                          {'modifier': 0, 'formats': [875713112]}]}}}],
  }

class TrimTest(unittest.TestCase):
  """What a fixture keeps of a drm_info object."""

  def test_connectors_are_named_by_type_and_order(self):
    fixture = drm_info_fixture.trim(card(), 'test')
    names = [c['name'] for c in fixture['connectors']]
    self.assertEqual(names, ['eDP-1', 'DP-1', 'DP-2'])

  def test_monitor_identities_are_dropped(self):
    fixture = drm_info_fixture.trim(card(), 'test')
    text = json.dumps(fixture)
    self.assertNotIn('serial', text)
    self.assertNotIn('mst:12-1', text)
    props = fixture['connectors'][0]['properties']
    self.assertEqual(props['EDID'], {'value': 0})
    self.assertEqual(fixture['connectors'][1]['properties']['PATH'],
                     {'value': 0})

  def test_ranges_and_values_are_kept(self):
    fixture = drm_info_fixture.trim(card(), 'test')
    props = fixture['connectors'][0]['properties']
    self.assertEqual(props['max bpc'], {'value': 12, 'range': [6, 12]})
    self.assertEqual(props['CRTC_ID'], {'value': 20})
    crtc = fixture['crtcs'][0]
    self.assertEqual(crtc['properties']['GAMMA_LUT'], {'value': 0})
    self.assertEqual(crtc['mode']['vsync_start'], 1084)

  def test_planes_keep_in_formats_and_state(self):
    plane = drm_info_fixture.trim(card(), 'test')['planes'][0]
    self.assertEqual(plane['in_formats'], [[0, [875713112]]])
    self.assertEqual((plane['crtc_id'], plane['fb_id']), (20, 99))
    self.assertEqual(plane['properties']['type'], {'value': 1})

  def test_only_known_caps_are_kept(self):
    caps = drm_info_fixture.trim(card(), 'test')['caps']
    self.assertEqual(caps['CURSOR_WIDTH'], 256)
    self.assertIsNone(caps['ATOMIC_ASYNC_PAGE_FLIP'])
    self.assertNotIn('PRIME', caps)

class LoadTest(unittest.TestCase):
  """Reading drm_info's own output and bringup's."""

  def test_plain_and_keyed_json(self):
    data = card()
    self.assertEqual(drm_info_fixture.load_card(json.dumps(data)), data)
    keyed = json.dumps({'/dev/dri/card1': data})
    self.assertEqual(drm_info_fixture.load_card(keyed), data)

  def test_bringup_sections(self):
    other = card()
    other['driver']['name'] = 'amdgpu'
    text = ('== /dev/dri/card0\n' + json.dumps({'/dev/dri/card0': other})
            + '\n== /dev/dri/card1\n' + json.dumps(card()))
    self.assertEqual(drm_info_fixture.load_card(text)['driver']['name'],
                     'amdgpu')
    chosen = drm_info_fixture.load_card(text, '/dev/dri/card1')
    self.assertEqual(chosen['driver']['name'], 'i915')
    with self.assertRaises(ValueError):
      drm_info_fixture.load_card(text, '/dev/dri/card7')

  def test_main_writes_the_fixture(self):
    with tempfile.TemporaryDirectory() as directory:
      source = os.path.join(directory, 'in.json')
      target = os.path.join(directory, 'out.json')
      with open(source, 'w', encoding='utf-8') as file:
        json.dump(card(), file)
      status = drm_info_fixture.main(['--source', 'a laptop', source,
                                      target])
      self.assertEqual(status, 0)
      with open(target, encoding='utf-8') as file:
        fixture = json.load(file)
    self.assertEqual(fixture['source'], 'a laptop')
    self.assertEqual(fixture['driver'], 'i915')

if __name__ == '__main__':
  unittest.main()
