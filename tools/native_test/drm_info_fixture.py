#!/usr/bin/env python3
"""Turns `drm_info -j` output into a fake-card fixture for the unit tests.

Usage:
  tools/native_test/drm_info_fixture.py [--card NODE] [--source TEXT]
      INPUT OUTPUT

INPUT is either one card's drm_info JSON, as drmdb.emersion.fr serves its
snapshots, or the drm_info.json that `bringup.py report` saves, which has
a `== /dev/dri/cardN` line before each card's JSON; `--card` picks the
card there (the first one by default). OUTPUT is written as the trimmed
JSON that crates/shingle-native/src/kms/fake/drm_info.rs loads: the
driver's caps, every connector with its modes and properties, every
encoder, CRTC and plane with their properties and each plane's
IN_FORMATS. Every property keeps its name and value, and range
properties their range, so the fake card refuses what the real driver
would refuse for a missing property or a value out of range.

Monitor identities are left out: every EDID blob and `PATH` string is
dropped (their properties stay, with value 0), so a fixture made from
another person's dump names no monitor serial number. Connector names
are drm_info's when it gives them, else the kernel's TYPE-N with N
counting connectors of that type in order, which is how the kernel
names them on a card that has never lost a DP-MST connector.

It only reads INPUT and writes OUTPUT, and is safe anywhere.
"""

import argparse
import json
import sys

import bringup_report

# The kernel's connector type names (drm_connector.c,
# drm_connector_enum_list).
CONNECTOR_TYPES = {
    0: 'Unknown', 1: 'VGA', 2: 'DVI-I', 3: 'DVI-D', 4: 'DVI-A',
    5: 'Composite', 6: 'SVIDEO', 7: 'LVDS', 8: 'Component', 9: 'DIN',
    10: 'DP', 11: 'HDMI-A', 12: 'HDMI-B', 13: 'TV', 14: 'eDP',
    15: 'Virtual', 16: 'DSI', 17: 'DPI', 18: 'Writeback', 19: 'SPI',
    20: 'USB',
}

# drm_info's property type for a range (DRM_MODE_PROP_RANGE).
RANGE = 2

# Properties whose blobs identify a monitor.
IDENTIFYING = ('EDID', 'PATH')

# The caps the fake card answers.
CAPS = ('CURSOR_WIDTH', 'CURSOR_HEIGHT', 'ASYNC_PAGE_FLIP',
        'ATOMIC_ASYNC_PAGE_FLIP', 'TIMESTAMP_MONOTONIC', 'ADDFB2_MODIFIERS')

MODE_FIELDS = ('clock', 'hdisplay', 'hsync_start', 'hsync_end', 'htotal',
               'hskew', 'vdisplay', 'vsync_start', 'vsync_end', 'vtotal',
               'vscan', 'vrefresh', 'flags', 'type', 'name')

def load_card(text, card=None):
  """One card's drm_info object from `text`: plain JSON, or bringup's
  `== NODE` sections, taking `card` or else the first."""
  if text.lstrip().startswith('=='):
    cards = bringup_report.parse_drm_info(text)
    if card is None:
      card = next(iter(cards), None)
    data = cards.get(card)
    if not isinstance(data, dict):
      raise ValueError(f'no drm_info JSON for {card}')
    return data
  data = json.loads(text)
  # `drm_info -j NODE` keys its object by the node.
  if isinstance(data, dict) and 'connectors' not in data and len(data) == 1:
    data = next(iter(data.values()))
  return data

def trim_mode(mode):
  """A mode with the fields the fake needs; drm_info calls vsync_start
  `vsync_total`."""
  if mode is None:
    return None
  trimmed = {}
  for field in MODE_FIELDS:
    value = mode.get(field)
    if field == 'vsync_start' and value is None:
      value = mode.get('vsync_total')
    trimmed[field] = value
  return trimmed

def trim_properties(properties):
  """{name: {value[, range]}} of drm_info's properties; blob values
  become 0 and identifying blobs are dropped."""
  trimmed = {}
  for name in sorted(properties or {}):
    prop = properties[name]
    value = prop.get('value') if isinstance(prop, dict) else prop
    if name in IDENTIFYING or not isinstance(value, int):
      value = 0
    entry = {'value': value}
    spec = prop.get('spec') if isinstance(prop, dict) else None
    if (isinstance(prop, dict) and prop.get('type') == RANGE
        and isinstance(spec, dict)):
      entry['range'] = [spec.get('min'), spec.get('Max', spec.get('max'))]
    trimmed[name] = entry
  return trimmed

def connector_names(connectors):
  """Each connector's name, as drm_info gives it or as TYPE-N."""
  counts = {}
  names = []
  for connector in connectors:
    kind = CONNECTOR_TYPES.get(connector.get('type'), 'Unknown')
    counts[kind] = counts.get(kind, 0) + 1
    names.append(connector.get('name') or f'{kind}-{counts[kind]}')
  return names

def trim(data, source):
  """The fixture of drm_info object `data`, which came from `source`."""
  driver = data.get('driver') or {}
  caps = driver.get('caps') or {}
  connectors = data.get('connectors') or []
  fixture = {
      'source': source,
      'driver': driver.get('name'),
      'kernel': (driver.get('kernel') or {}).get('release'),
      'caps': {name: caps.get(name) for name in CAPS},
      'connectors': [],
      'encoders': [],
      'crtcs': [],
      'planes': [],
  }
  for connector, name in zip(connectors, connector_names(connectors)):
    fixture['connectors'].append({
        'id': connector['id'],
        'name': name,
        'type': connector.get('type'),
        'status': connector.get('status'),
        'phy_size': [connector.get('phy_width', 0),
                     connector.get('phy_height', 0)],
        'subpixel': connector.get('subpixel', 0),
        'encoders': connector.get('encoders') or [],
        'modes': [trim_mode(mode) for mode in connector.get('modes') or []],
        'properties': trim_properties(connector.get('properties')),
    })
  for encoder in data.get('encoders') or []:
    fixture['encoders'].append({
        'id': encoder['id'],
        'type': encoder.get('type'),
        'possible_crtcs': encoder.get('possible_crtcs', 0),
    })
  for crtc in data.get('crtcs') or []:
    fixture['crtcs'].append({
        'id': crtc['id'],
        'mode': trim_mode(crtc.get('mode')),
        'properties': trim_properties(crtc.get('properties')),
    })
  for plane in data.get('planes') or []:
    fixture['planes'].append({
        'id': plane['id'],
        'possible_crtcs': plane.get('possible_crtcs', 0),
        'crtc_id': plane.get('crtc_id', 0),
        'fb_id': plane.get('fb_id', 0),
        'formats': plane.get('formats') or [],
        'in_formats': [[modifier, formats] for modifier, formats
                       in bringup_report.in_formats(plane)],
        'properties': trim_properties(plane.get('properties')),
    })
  return fixture

def main(argv):
  """Runs the tool on `argv`; returns the exit status."""
  parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
  parser.add_argument('--card', help='the card node in bringup output')
  parser.add_argument('--source', default='',
                      help='where the dump came from, kept in the fixture')
  parser.add_argument('input')
  parser.add_argument('output')
  args = parser.parse_args(argv)
  with open(args.input, encoding='utf-8') as file:
    text = file.read()
  try:
    data = load_card(text, args.card)
  except ValueError as error:
    print(f'drm_info_fixture: {error}', file=sys.stderr)
    return 1
  fixture = trim(data, args.source or args.input)
  with open(args.output, 'w', encoding='utf-8') as file:
    json.dump(fixture, file, indent=1, sort_keys=True)
    file.write('\n')
  return 0

if __name__ == '__main__':
  sys.exit(main(sys.argv[1:]))
