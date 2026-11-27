"""The hardware report of docs/laptop-bringup.md step 2.

`collect` runs the read-only probes of `PROBES` on the test machine,
`drm_info -j` once per card among them, and `render` turns their output
into Markdown with what the GPU family should get: the zero-copy scanout
path of NB-8 (native-testing.md step 37) and the cursor plane of NB-10
(step 45). Where drm_info's JSON is there, the expectation names what the
card itself offers; otherwise it falls back to the family's usual
behaviour, and says so.
"""

import json

# (key, title, command). Every command only reads; none needs root.
# libinput list-devices needs the input group, so /proc/bus/input/devices
# is listed as well, which anyone can read.
PROBES = [
    ('kernel', 'Kernel', 'uname -a'),
    ('os', 'Distribution', 'cat /etc/os-release'),
    ('lspci', 'Display controllers',
     "lspci -nnk | grep -A3 -E 'VGA|3D|Display'"),
    ('cards', 'DRM cards',
     'for c in /sys/class/drm/card[0-9]*; do case $c in *-*) continue;; '
     'esac; n=${c##*/}; '
     'printf "%s driver=%s boot_vga=%s\\n" "$n" '
     '"$(basename "$(readlink -f "$c/device/driver")")" '
     '"$(cat "$c/device/boot_vga" 2>/dev/null || echo -)"; done'),
    ('connectors', 'Connectors and status',
     'for f in /sys/class/drm/card*-*/status; do d=${f%/status}; '
     'printf "%s %s enabled=%s modes=%s\\n" "${d##*/}" "$(cat "$f")" '
     '"$(cat "$d/enabled" 2>/dev/null)" '
     '"$(head -n 1 "$d/modes" 2>/dev/null)"; done'),
    ('dev_dri', '/dev/dri', 'ls -l /dev/dri /dev/dri/by-path 2>&1'),
    ('libinput', 'libinput list-devices', 'libinput list-devices 2>&1'),
    ('input_devices', '/proc/bus/input/devices',
     "grep -E '^(N|P|H):' /proc/bus/input/devices"),
    ('packages', 'Mesa, driver and library versions',
     "dpkg-query -W -f '${Package} ${Version}\\n' 'libgl1-mesa-dri' "
     "'mesa-vulkan-drivers' 'libdrm2' 'libgbm1' 'libinput10' 'libseat1' "
     "'seatd' 'systemd' 'firmware-*' 'nvidia-driver' 'linux-image-*' "
     "2>/dev/null | grep -v ' $'"),
    ('nvidia', 'NVIDIA driver',
     'cat /proc/driver/nvidia/version 2>/dev/null; '
     'printf "nvidia_drm.modeset=%s\\n" '
     '"$(cat /sys/module/nvidia_drm/parameters/modeset 2>/dev/null || '
     'echo absent)"'),
    ('vulkan', 'Vulkan devices',
     'vulkaninfo --summary 2>/dev/null | grep -E '
     "'deviceName|driverName|driverInfo|apiVersion' || "
     'echo "vulkaninfo not installed (vulkan-tools)"'),
    ('seat', 'logind and seatd',
     'loginctl --version 2>&1 | head -n 1; '
     'printf "systemd-logind: %s\\n" '
     '"$(systemctl is-active systemd-logind 2>&1)"; '
     'printf "seatd: %s\\n" "$(systemctl is-active seatd 2>&1)"; '
     'ls -l /run/seatd.sock 2>&1; '
     'systemctl cat seatd 2>/dev/null | grep ExecStart; '
     'loginctl list-sessions --no-legend 2>&1'),
    ('groups', 'Groups', 'id; getent group video input render _seatd seat'),
]
DRM_INFO = ('for c in /dev/dri/card[0-9]*; do echo "== $c"; drm_info -j "$c"; '
            'done')

FAMILIES = {
    'i915': ('Intel', 'zero-copy, with an INTEL modifier: Y tiling '
             '(0x0100000000000002) on Gen9 to Gen12, Tile4 on Meteor Lake '
             'and later, or X tiling', '256x256 on every CRTC'),
    'xe': ('Intel (xe)', 'zero-copy, with an INTEL Tile4 or X tiling '
           'modifier', '256x256 on every CRTC'),
    'amdgpu': ('AMD', 'zero-copy, with an AMD GFX9+ modifier (0x02...), '
               'with or without DCC', '64x64 to 256x256 on every CRTC; '
               'amdgpu draws it with the scaling and colour of the plane '
               'below'),
    'radeon': ('AMD (radeon)', 'copy scanout likely: radeon has no '
               'modifiers in IN_FORMATS', 'a legacy cursor plane'),
    'nvidia-drm': ('NVIDIA', 'zero-copy with a block-linear NVIDIA '
                   'modifier (0x030000000060601x), only with '
                   'nvidia_drm.modeset=Y', '256x256 on every CRTC'),
    'nouveau': ('NVIDIA (nouveau)', 'zero-copy with a block-linear '
                'modifier where Mesa renders on the same card',
                '64x64 or 256x256'),
    'simpledrm': ('firmware framebuffer', 'copy scanout: simpledrm has '
                  'only a linear primary plane and no GPU', 'none'),
}
FOURCC_XR24 = 0x34325258
PLANE_TYPES = {0: 'Overlay', 1: 'Primary', 2: 'Cursor'}
MODIFIER_VENDORS = {0: 'LINEAR/NONE', 1: 'INTEL', 2: 'AMD', 3: 'NVIDIA',
                    4: 'SAMSUNG', 5: 'QCOM', 8: 'ARM'}

def collect(target):
  """Runs every probe and drm_info on `target`; returns {key: Result}."""
  results = {key: target.run(command, timeout=60)
             for key, _, command in PROBES}
  results['drm_info'] = target.run(DRM_INFO, timeout=120)
  return results

def parse_cards(text):
  """[{'card', 'driver', 'boot_vga'}] from the `cards` probe."""
  cards = []
  for line in text.splitlines():
    fields = line.split()
    if not fields:
      continue
    card = {'card': fields[0]}
    for field in fields[1:]:
      key, _, value = field.partition('=')
      card[key] = value
    cards.append(card)
  return cards

def parse_drm_info(text):
  """{card node: drm_info JSON object} from the drm_info probe, whose
  output is a `== /dev/dri/cardN` line before each card's JSON."""
  cards = {}
  node, lines = None, []
  for line in text.splitlines() + ['== end']:
    if line.startswith('== '):
      if node is not None:
        try:
          data = json.loads('\n'.join(lines))
        except ValueError:
          data = None
        # drm_info -j keys its object by the node it opened.
        if isinstance(data, dict) and len(data) == 1:
          data = next(iter(data.values()))
        cards[node] = data
      node, lines = line[3:].strip(), []
    else:
      lines.append(line)
  return cards

def prop_value(properties, name):
  """A DRM property's value from drm_info's `properties` object."""
  prop = (properties or {}).get(name)
  if isinstance(prop, dict):
    return prop.get('value')
  return prop

def in_formats(plane):
  """The IN_FORMATS blob of a plane as [(modifier, [fourcc])]."""
  prop = (plane.get('properties') or {}).get('IN_FORMATS')
  if not isinstance(prop, dict):
    return []
  data = prop.get('data')
  if not isinstance(data, list):
    return []
  return [(entry.get('modifier'), entry.get('formats') or [])
          for entry in data if isinstance(entry, dict)]

def modifier_name(modifier):
  """A modifier as VENDOR 0x..., like Shingle's log names it."""
  if not isinstance(modifier, int):
    return str(modifier)
  vendor = MODIFIER_VENDORS.get(modifier >> 56, f'vendor {modifier >> 56}')
  return f'{vendor} {modifier:#018x}'

def summarise_card(data):
  """What one card's drm_info says about NB-8 and NB-10: a dict with
  driver, crtcs, cursor_planes, cursor_size, xr24_modifiers,
  atomic_async, vrr_capable and connected."""
  driver = data.get('driver') or {}
  caps = driver.get('caps') or {}
  planes = data.get('planes') or []
  cursor_planes = 0
  modifiers = []
  for plane in planes:
    kind = prop_value(plane.get('properties'), 'type')
    if isinstance(kind, int):
      kind = PLANE_TYPES.get(kind, kind)
    if kind == 'Cursor':
      cursor_planes += 1
    if kind == 'Primary':
      for modifier, formats in in_formats(plane):
        if FOURCC_XR24 in formats and modifier not in modifiers:
          modifiers.append(modifier)
  connectors = data.get('connectors') or []
  return {
      'driver': driver.get('name'),
      'crtcs': len(data.get('crtcs') or []),
      'cursor_planes': cursor_planes,
      'cursor_size': (caps.get('CURSOR_WIDTH'), caps.get('CURSOR_HEIGHT')),
      'xr24_modifiers': modifiers,
      'atomic_async': caps.get('ATOMIC_ASYNC_PAGE_FLIP'),
      'vrr_capable': [
          c.get('name') or c.get('id') for c in connectors
          if prop_value(c.get('properties'), 'vrr_capable') == 1],
      'connected': [c.get('name') or c.get('id') for c in connectors
                    if c.get('status') in (1, 'connected')],
  }

def expectation(driver, summary=None):
  """(zero-copy, cursor plane) sentences for a card with `driver` and,
  if parsed, its drm_info `summary`."""
  family, zero_copy, cursor = FAMILIES.get(driver, (
      driver or 'unknown', 'unknown: record what step 37 logs',
      'unknown: record what step 45 logs'))
  zero = f'{family}: {zero_copy}'
  plane = f'{family}: {cursor}'
  if summary:
    modifiers = [m for m in summary['xr24_modifiers'] if m not in (0, None)]
    if summary['xr24_modifiers'] and not modifiers:
      zero = (f'{family}: the primary plane offers XR24 only LINEAR, so '
              'zero-copy needs the renderer to draw linear buffers; copy '
              'scanout is likely')
    elif modifiers:
      names = ', '.join(modifier_name(m) for m in modifiers)
      zero = (f'{family}: zero-copy, with one of the primary plane\'s XR24 '
              f'modifiers that the renderer also lists (step 36.2): {names}')
    width, height = summary['cursor_size']
    if summary['cursor_planes']:
      size = f'{width}x{height}' if width else 'size not reported'
      plane = (f'{family}: the cursor plane, {summary["cursor_planes"]} '
               f'cursor planes for {summary["crtcs"]} CRTCs, {size}')
    else:
      plane = (f'{family}: no cursor plane, so the cursor is drawn into '
               'the frame')
  return zero, plane

def scanout_card(cards):
  """The card Shingle opens: the boot VGA one, else the first."""
  for card in cards:
    if card.get('boot_vga') == '1':
      return card
  return cards[0] if cards else None

def fence(text):
  """`text` in a Markdown code fence."""
  return '```\n' + (text.rstrip() or '(no output)') + '\n```\n'

def render(host, info, results, when):
  """The Markdown report for `host` (whose guard `info` is given)."""
  cards = parse_cards(results['cards'].stdout)
  drm = parse_drm_info(results['drm_info'].stdout)
  chosen = scanout_card(cards)
  lines = [f'# Bring-up report: {host}', '',
           f'Collected {when} by tools/native_test/bringup.py report from '
           f'{info.user}@{info.hostname} (marker '
           f'{"present" if info.marker else "missing"}).', '',
           '## Expected paths', '']
  if not cards:
    lines += ['No DRM card was found, so Shingle has nothing to light.', '']
  for card in cards:
    node = f'/dev/dri/{card["card"]}'
    data = drm.get(node)
    summary = summarise_card(data) if isinstance(data, dict) else None
    zero, plane = expectation(card.get('driver'), summary)
    tag = ' (boot VGA: Shingle opens this one)' if card is chosen else ''
    lines += [f'### {node}, {card.get("driver")}{tag}', '',
              f'- Zero-copy scanout (step 37): {zero}.',
              f'- Cursor plane (step 45): {plane}.']
    if summary:
      async_flip = {1: 'yes', 0: 'no', None: 'not reported'}.get(
          summary['atomic_async'], summary['atomic_async'])
      lines += [
          f'- Atomic async page flips (step 48): {async_flip}.',
          '- VRR-capable connectors (step 47): '
          f'{", ".join(map(str, summary["vrr_capable"])) or "none"}.',
          f'- Connected: {", ".join(map(str, summary["connected"])) or "none"}'
          f'; CRTCs: {summary["crtcs"]}.']
    else:
      lines.append('- drm_info gave no JSON for this card (not installed, '
                   'or the card could not be opened over SSH: the user '
                   'needs the video group), so these are the family\'s '
                   'usual paths, not what the card reports.')
    lines.append('')
  if len(cards) > 1:
    lines += ['Several cards: on a hybrid laptop Shingle scans out on the '
              'boot VGA card; zero-copy also needs the renderer on that '
              'card\'s GPU (step 39.3).', '']
  for key, title, command in PROBES:
    result = results[key]
    lines += [f'## {title}', '', f'`{command}`' if len(command) < 70 else '',
              '', fence(result.stdout + result.stderr)]
  lines += ['## drm_info', '', 'Saved in full as drm_info.json beside this '
            'report.', '']
  return '\n'.join(lines)
