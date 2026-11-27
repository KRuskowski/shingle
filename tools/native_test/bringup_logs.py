"""Checks of native.log and the run files that bringup.py collects.

`scan` reads Shingle's native.log (and its stderr) from a run on the test
machine and returns what the steps look for: the backend, the seat and its
libseat backend, the card, the renderer, every output with its mode and
scanout path, the cursor plane, flips, and every line that signals a
failure (a panic, a refused modeset or flip, a lost GPU, no seat). It only
reads text and is safe anywhere.
"""

import re

SEAT = re.compile(r'seat (\S+) enabled \((\w+) backend\)')
CARD = re.compile(r'using (/dev/dri/card\d+)')
RENDERER = re.compile(r'rendering on (.+)')
OUTPUT = re.compile(r'output (\S+): (\d+)x(\d+) at ([\d.]+) Hz')
SCANOUT = re.compile(r'output (\S+): (zero-copy scanout, modifier .+|'
                     r'copy scanout, because .+|back to copy scanout.*)')
CURSOR = re.compile(r'output (\S+): the cursor (is on the cursor plane|'
                    r'is drawn.*)')
INPUT_ADDED = re.compile(r'input device (\S+) \((.+)\) added')
FLIP = re.compile(r'page flip seq=\d+')
CURSOR_FLIP = re.compile(r'cursor plane flip')
# A failure in any step; each pattern is a line no healthy run writes.
FAILURES = [
    re.compile(p) for p in (
        r'panicked at', r'\bfailed\b', r'the GPU device was lost',
        r'did not finish a frame',
        r'no seat:', r'no DRM device', r'no Vulkan adapter',
        r'no DRM hotplug monitor', r'wl_display\.error',
        r'the GPU failed \d+ times')
]

def scan(log_lines, stderr_lines=()):
  """Summarises a run's native.log and stderr lines."""
  summary = {
      'backend': None, 'seat': None, 'libseat': None, 'card': None,
      'renderer': None, 'outputs': {}, 'scanout': {}, 'cursor': {},
      'input_devices': [], 'flips': 0, 'cursor_flips': 0, 'failures': [],
      'stopped': False, 'paused': 0, 'resumed': 0,
  }
  for line in stderr_lines:
    if 'using the native backend' in line:
      summary['backend'] = 'native'
    elif 'using the nested backend' in line:
      summary['backend'] = 'nested'
  for line in list(log_lines) + list(stderr_lines):
    line = line.rstrip('\n')
    if FLIP.search(line):
      summary['flips'] += 1
      continue
    if CURSOR_FLIP.search(line):
      summary['cursor_flips'] += 1
      continue
    if match := SEAT.search(line):
      summary['seat'], summary['libseat'] = match.groups()
    if match := CARD.search(line):
      summary['card'] = match.group(1)
    if match := RENDERER.search(line):
      summary['renderer'] = match.group(1).strip()
    if match := OUTPUT.search(line):
      name, width, height, hz = match.groups()
      summary['outputs'][name] = (int(width), int(height), float(hz))
    if match := SCANOUT.search(line):
      summary['scanout'][match.group(1)] = match.group(2)
    if match := CURSOR.search(line):
      summary['cursor'][match.group(1)] = match.group(2)
    if match := INPUT_ADDED.search(line):
      summary['input_devices'].append(match.group(2))
    if 'native backend stopped' in line:
      summary['stopped'] = True
    if 'seat disabled; paused' in line:
      summary['paused'] += 1
    if 'seat enabled; resumed' in line:
      summary['resumed'] += 1
    if any(pattern.search(line) for pattern in FAILURES):
      summary['failures'].append(line.strip())
  return summary

def describe(summary):
  """Markdown bullets for a `scan` summary."""
  lines = [
      f'- Backend: {summary["backend"] or "not in stderr"}; seat '
      f'{summary["seat"] or "none"} ({summary["libseat"] or "?"} backend).',
      f'- Card: {summary["card"] or "none"}; renderer: '
      f'{summary["renderer"] or "none"}.',
  ]
  for name, (width, height, hz) in sorted(summary['outputs'].items()):
    scanout = summary['scanout'].get(name, 'no scanout line')
    cursor = summary['cursor'].get(name, 'no cursor line')
    lines.append(f'- Output {name}: {width}x{height} at {hz} Hz; {scanout}; '
                 f'the cursor {cursor}.')
  lines.append(f'- Input devices added: {len(summary["input_devices"])} '
               f'({", ".join(summary["input_devices"]) or "none"}).')
  lines.append(f'- Page flips {summary["flips"]}, cursor plane flips '
               f'{summary["cursor_flips"]}, pauses {summary["paused"]}, '
               f'resumes {summary["resumed"]}; '
               + ('stopped cleanly.' if summary['stopped']
                  else 'no stop line.'))
  if summary['failures']:
    lines.append(f'- FAIL: {len(summary["failures"])} failure lines:')
    lines += [f'  - `{line}`' for line in summary['failures'][:20]]
  else:
    lines.append('- No failure lines.')
  return lines
