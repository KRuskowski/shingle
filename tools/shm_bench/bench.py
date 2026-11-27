"""Pure helpers of the shm upload benchmark: cases, percentiles, the
parsing of probe events, Shingle's buffer trace and `nvidia-smi`, and
the Markdown table.

Nothing here starts a process or sends a signal; run.py does, and
test_bench.py tests this module.
"""

import json
import math

# Buffer sizes and damage of the default cases: `WIDTHxHEIGHT:DAMAGE`,
# DAMAGE `full` or the side of a square.
DEFAULT_CASES = ('256x256:full', '1280x720:full', '1280x720:64',
                 '1920x1080:full', '1920x1080:64')
# The ways Shingle can write shm damage into textures
# (crates/shingle-render/src/staging.rs), chosen by SHINGLE_SHM_STAGING:
# its own staging buffers in system memory, wgpu's
# `Queue::write_texture`, or its own staging buffers in device-local,
# host-visible memory.
VARIANTS = {'staging': '1', 'write_texture': '0',
            'staging_device_local': 'device-local'}
# The variants run unless --variants names others; the third stages in
# the memory write_texture prefers, to tell its memory from its path.
DEFAULT_VARIANTS = ('staging', 'write_texture')
MODES = ('offscreen', 'nested')
# The columns of the table, after the case's labels.
COLUMNS = ('frames', 'latency_p50_ms', 'latency_p90_ms', 'latency_max_ms',
           'upload_p50_us', 'upload_p90_us', 'upload_bytes',
           'cpu_ms_per_frame', 'outer_cpu_ms_per_frame', 'fps')
# The GPU's state while a case ran, from nvidia-smi on the host.
GPU_COLUMNS = ('pstate', 'graphics_mhz', 'pcie_gen', 'bar1_used_mib')

def parse_case(text):
  """`WIDTHxHEIGHT:DAMAGE` as {'width', 'height', 'damage'}, damage None
  for `full` or the side of a square; ValueError when malformed."""
  size, _, damage = text.partition(':')
  width, _, height = size.partition('x')
  width, height = int(width), int(height)
  if width <= 0 or height <= 0:
    raise ValueError(f'bad size in {text!r}')
  if damage in ('', 'full'):
    side = None
  else:
    side = int(damage)
    if side <= 0:
      raise ValueError(f'bad damage in {text!r}')
  return {'width': width, 'height': height, 'damage': side}

def case_label(case):
  """The `WIDTHxHEIGHT:DAMAGE` text of a parsed case."""
  damage = 'full' if case['damage'] is None else str(case['damage'])
  return f'{case["width"]}x{case["height"]}:{damage}'

def probe_arguments(case, frames):
  """The `wl_probe` options that draw `case` for `frames` frames with
  presentation feedback."""
  arguments = ['--animate', str(frames), '--presentation',
               '--size', f'{case["width"]}x{case["height"]}']
  if case['damage'] is not None:
    arguments += ['--damage-square', str(case['damage'])]
  return arguments

def damage_bytes(case):
  """The bytes one frame of `case` damages, 4 a pixel."""
  side = case['damage']
  if side is None:
    return case['width'] * case['height'] * 4
  side = min(side, case['width'], case['height'])
  return side * side * 4

def percentile(values, fraction):
  """The `fraction` percentile of `values` by the nearest rank, or None
  for no values."""
  if not values:
    return None
  ordered = sorted(values)
  rank = max(1, math.ceil(fraction * len(ordered)))
  return ordered[rank - 1]

def events(text, name):
  """The JSON events called `name` among the lines of `text`."""
  found = []
  for line in text.splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get('event') == name:
      found.append(event)
  return found

def latencies_ms(text):
  """The commit-to-presentation latencies, in ms, of the `presented`
  events of a probe's output `text`."""
  return [event['latency_us'] / 1000 for event in events(text, 'presented')
          if isinstance(event.get('latency_us'), int)]

def uploads(text):
  """[(bytes, microseconds)] of the `shm upload N bytes surface=S us=T`
  lines of Shingle's buffer trace in `text`, in order; lines without a
  time are skipped."""
  found = []
  for line in text.splitlines():
    words = line.split()
    try:
      index = words.index('upload')
    except ValueError:
      continue
    if index == 0 or words[index - 1] != 'shm':
      continue
    try:
      size = int(words[index + 1])
      micros = next(int(word[3:]) for word in words if word.startswith('us='))
    except (IndexError, ValueError, StopIteration):
      continue
    found.append((size, micros))
  return found

def summarize(latencies, upload_list, cpu_seconds, outer_cpu_seconds,
              elapsed):
  """One row of the table from a case's measurements: latencies in ms,
  [(bytes, us)] uploads, the measured Shingle's and the outer Shingle's
  CPU seconds (None without one) and the seconds the case took."""
  frames = len(latencies)
  times = [micros for _, micros in upload_list]
  sizes = [size for size, _ in upload_list]

  def per_frame(seconds):
    if seconds is None or not frames:
      return None
    return seconds * 1000 / frames
  return {
      'frames': frames,
      'latency_p50_ms': percentile(latencies, 0.5),
      'latency_p90_ms': percentile(latencies, 0.9),
      'latency_max_ms': max(latencies) if latencies else None,
      'upload_p50_us': percentile(times, 0.5),
      'upload_p90_us': percentile(times, 0.9),
      'upload_bytes': percentile(sizes, 0.5),
      'cpu_ms_per_frame': per_frame(cpu_seconds),
      'outer_cpu_ms_per_frame': per_frame(outer_cpu_seconds),
      'fps': frames / elapsed if elapsed > 0 else None,
  }

def format_value(value):
  """A table cell: '-' for None, one decimal for floats."""
  if value is None:
    return '-'
  if isinstance(value, float):
    return f'{value:.1f}'
  return str(value)

def markdown(rows, columns=COLUMNS):
  """A Markdown table of result `rows`, each with 'mode', 'variant',
  'case' and `columns`."""
  header = ('mode', 'variant', 'case') + tuple(columns)
  lines = ['| ' + ' | '.join(header) + ' |',
           '|' + '---|' * len(header)]
  for row in rows:
    lines.append('| ' + ' | '.join(format_value(row.get(column))
                                   for column in header) + ' |')
  return '\n'.join(lines)

def parse_nvidia_state(text):
  """{'pstate', 'graphics_mhz', 'pcie_gen', 'bar1_used_mib'} from the text
  of `nvidia-smi -q`; missing values are left out."""
  state = {}
  section = None
  for line in text.splitlines():
    stripped = line.strip()
    name, colon, value = stripped.partition(':')
    name, value = name.strip(), value.strip()
    if not colon:
      section = stripped
      continue
    if not value:
      section = name
      continue
    if name == 'Performance State':
      state['pstate'] = value
    elif section == 'Clocks' and name == 'Graphics':
      state['graphics_mhz'] = _number(value)
    elif section == 'PCIe Generation' and name == 'Current':
      state['pcie_gen'] = _number(value)
    elif section == 'BAR1 Memory Usage' and name == 'Used':
      state['bar1_used_mib'] = _number(value)
  return {key: value for key, value in state.items() if value is not None}

def _number(value):
  """The leading integer of `value`, such as 210 of '210 MHz', or None
  when it has none."""
  first = value.split()[0] if value.split() else ''
  try:
    return int(first)
  except ValueError:
    return None

def state_during(samples, start, end):
  """The GPU state of the `samples` (dicts with 'time' and
  parse_nvidia_state's keys) taken from `start` to `end`: the lowest and
  highest of each numeric value as 'LOW-HIGH' (or one value), and the
  performance states seen, joined by '/'. Without a sample in the
  interval, the last one before it counts."""
  inside = [sample for sample in samples
            if start <= sample.get('time', 0) <= end]
  if not inside:
    before = [sample for sample in samples if sample.get('time', 0) < start]
    inside = before[-1:]
  state = {}
  for key in GPU_COLUMNS:
    values = [sample[key] for sample in inside if key in sample]
    if not values:
      continue
    if key == 'pstate':
      state[key] = '/'.join(sorted(set(values)))
    else:
      low, high = min(values), max(values)
      state[key] = str(low) if low == high else f'{low}-{high}'
  return state
