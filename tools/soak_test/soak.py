"""Pure helpers of the soak harness: /proc parsing, the trend test, the
CSV and the Markdown summary, the load gate, latency statistics and the
GPU memory lookup.

Nothing here starts a process or sends a signal; run.py and workload.py
do, and test_soak.py tests this module.
"""

import csv
import io
import math
import statistics

# The 1-minute load above which the workload pauses (a 20-core host
# that other builds share).
LOAD_LIMIT = 12.0
# Samples at the start of a run that the trend test leaves out, while
# caches, pools and the allocator's arenas fill.
WARMUP_SAMPLES = 5
# The fewest samples after the warm-up that the trend test judges.
MIN_SAMPLES = 6
# The fixed columns of the CSV, before the object counts.
BASE_COLUMNS = ('minute', 'elapsed_s', 'process', 'pid', 'rss_kib', 'fds',
                'threads', 'cpu_s', 'gpu_mib', 'namespace_cpu_s', 'load1',
                'paused_s', 'cycles', 'session_writes', 'frames',
                'latency_p50_ms', 'latency_p99_ms', 'latency_max_ms')
# The columns the trend test judges, with their tolerances: growth from
# the first to the last third of a run must exceed both an absolute
# amount and a fraction of the start to be flagged.
TREND_TOLERANCES = {
    'rss_kib': (8192, 0.05),
    'fds': (3, 0.0),
    'threads': (2, 0.0),
    'gpu_mib': (32, 0.05),
}
# Object counts are taken when only the long-lived clients are left, so
# they come back to the same value each time; a few either way are the
# clients and surfaces that happen to be in flight, such as Xwayland's.
COUNT_TOLERANCE = (3, 0.0)
# Counts that follow the workload rather than leaks.
UNJUDGED_COUNTS = ('count_ipc_clients', 'count_frontend_scenes')

def parse_status(text):
  """{'rss_kib', 'threads'} from the text of /proc/PID/status."""
  values = {}
  for line in text.splitlines():
    name, _, rest = line.partition(':')
    if name == 'VmRSS':
      values['rss_kib'] = int(rest.split()[0])
    elif name == 'Threads':
      values['threads'] = int(rest.strip())
  return values

def parse_stat_cpu(text, ticks_per_second, children=False):
  """User plus system CPU seconds from the text of /proc/PID/stat, and
  those of its reaped children too when `children`."""
  # The command may contain spaces and parentheses; the fields after
  # the last ')' are fixed.
  fields = text.rsplit(')', 1)[1].split()
  # utime, stime, cutime and cstime are fields 14 to 17, the 12th to
  # 15th after ')'.
  ticks = int(fields[11]) + int(fields[12])
  if children:
    ticks += int(fields[13]) + int(fields[14])
  return ticks / ticks_per_second

def parse_nspid(text):
  """The PIDs of the NSpid line of /proc/PID/status, outermost first."""
  for line in text.splitlines():
    if line.startswith('NSpid:'):
      return [int(value) for value in line.split()[1:]]
  return []

def parse_ppid(text):
  """The PPid of /proc/PID/status text, or None."""
  for line in text.splitlines():
    if line.startswith('PPid:'):
      return int(line.split()[1])
  return None

def parse_loadavg(text):
  """The 1-minute load average from /proc/loadavg."""
  return float(text.split()[0])

def should_pause(load1, limit=LOAD_LIMIT):
  """Whether the workload waits before its next cycle."""
  return load1 > limit

def parse_nvidia_pids(text):
  """{host PID: used MiB} from `nvidia-smi -q -d PIDS`."""
  used = {}
  pid = None
  for line in text.splitlines():
    name, _, value = line.partition(':')
    name, value = name.strip(), value.strip()
    if name == 'Process ID':
      pid = int(value) if value.isdigit() else None
    elif name == 'Used GPU Memory' and pid is not None:
      number = value.split()[0] if value else ''
      if number.isdigit():
        used[pid] = used.get(pid, 0) + int(number)
      pid = None
  return used

def host_pid(namespace_pid, processes, root):
  """The host PID of the process that is `namespace_pid` inside the PID
  namespace below the host process `root`.

  `processes` maps host PIDs to (ppid, nspid list) as parse_ppid and
  parse_nspid read them. Only descendants of `root` count, so a process
  of another namespace with the same inner PID is never taken.
  """
  for pid, (_, nspid) in processes.items():
    if len(nspid) < 2 or nspid[-1] != namespace_pid:
      continue
    ancestor = pid
    for _ in range(64):
      if ancestor == root:
        return pid
      entry = processes.get(ancestor)
      if entry is None or entry[0] is None or entry[0] <= 1:
        break
      ancestor = entry[0]
  return None

def percentile(values, fraction):
  """The `fraction` percentile of `values` by nearest rank, or None."""
  if not values:
    return None
  ordered = sorted(values)
  rank = max(1, math.ceil(fraction * len(ordered)))
  return ordered[rank - 1]

def latency_summary(latencies_ms):
  """(p50, p99, max) of frame latencies in milliseconds, each None when
  there are none."""
  if not latencies_ms:
    return None, None, None
  return (percentile(latencies_ms, 0.5), percentile(latencies_ms, 0.99),
          max(latencies_ms))

def flatten_counts(reply):
  """`stipc/object_counts`'s reply as {'count_NAME': number}, the
  frontend's under `count_frontend_NAME`."""
  counts = {}
  for name, value in sorted((reply or {}).items()):
    if name == 'frontend' and isinstance(value, dict):
      for inner, number in sorted(value.items()):
        if isinstance(number, int):
          counts[f'count_frontend_{inner}'] = number
    elif isinstance(value, int) and not isinstance(value, bool):
      counts[f'count_{name}'] = value
  return counts

def columns_for(rows):
  """BASE_COLUMNS and then every count column any row has, sorted."""
  counts = sorted({key for row in rows for key in row
                   if key.startswith('count_')})
  return list(BASE_COLUMNS) + counts

def csv_text(rows):
  """The rows as CSV text with a header; a missing value is empty."""
  out = io.StringIO()
  writer = csv.DictWriter(out, fieldnames=columns_for(rows),
                          extrasaction='ignore', lineterminator='\n')
  writer.writeheader()
  for row in rows:
    writer.writerow({key: format_value(value) for key, value in row.items()})
  return out.getvalue()

def format_value(value):
  """A CSV cell: floats with three decimals, None empty."""
  if value is None:
    return ''
  if isinstance(value, float):
    return f'{value:.3f}'
  return value

def least_squares_slope(values):
  """The least-squares slope of `values` against their index."""
  count = len(values)
  if count < 2:
    return 0.0
  mean_x = (count - 1) / 2
  mean_y = sum(values) / count
  numerator = sum((index - mean_x) * (value - mean_y)
                  for index, value in enumerate(values))
  denominator = sum((index - mean_x) ** 2 for index in range(count))
  return numerator / denominator

def trend(values, tolerance, warmup=WARMUP_SAMPLES, min_samples=MIN_SAMPLES):
  """Judges one series sampled once a minute for growth.

  Leaves out `warmup` samples and None values, then compares the median
  of the first third of the rest with that of the last third, and fits a
  least-squares line. Growth is flagged when the medians rose by more
  than `tolerance` (absolute, fraction of the first median), and so did
  the fitted line across the series, so that neither one high sample at
  the end nor noise around a flat line flags it.

  Returns {'judged', 'flagged', 'first', 'last', 'growth',
  'slope_per_hour', 'samples'}.
  """
  series = [value for value in values[warmup:] if value is not None]
  result = {'judged': len(series) >= min_samples, 'flagged': False,
            'first': None, 'last': None, 'growth': None,
            'slope_per_hour': None, 'samples': len(series)}
  if not result['judged']:
    return result
  third = max(1, len(series) // 3)
  first = statistics.median(series[:third])
  last = statistics.median(series[-third:])
  slope = least_squares_slope(series)
  absolute, fraction = tolerance
  limit = max(absolute, fraction * abs(first))
  fitted = slope * (len(series) - 1)
  result.update({
      'first': first,
      'last': last,
      'growth': last - first,
      'slope_per_hour': slope * 60,
      'flagged': last - first > limit and fitted > limit,
  })
  return result

def tolerance_for(column):
  """The trend tolerance of `column`, or None when it is not judged."""
  if column in TREND_TOLERANCES:
    return TREND_TOLERANCES[column]
  if column.startswith('count_') and column not in UNJUDGED_COUNTS:
    return COUNT_TOLERANCE
  return None

def cpu_rates(rows):
  """CPU seconds per minute of wall time between consecutive rows of
  one process; the first row has None."""
  rates = [None]
  for previous, row in zip(rows, rows[1:]):
    seconds = row['elapsed_s'] - previous['elapsed_s']
    used = row['cpu_s'] - previous['cpu_s']
    rates.append(used * 60 / seconds if seconds > 0 else None)
  return rates

def trends(rows):
  """{process: {column: trend}} for every judged column of every
  process in `rows`."""
  by_process = {}
  for row in rows:
    by_process.setdefault(row['process'], []).append(row)
  judged = {}
  for process, process_rows in by_process.items():
    results = {}
    for column in columns_for(process_rows):
      tolerance = tolerance_for(column)
      if tolerance is None:
        continue
      values = [row.get(column) for row in process_rows]
      if all(value is None for value in values):
        continue
      results[column] = trend(values, tolerance)
    judged[process] = results
  return judged

def flagged(judged):
  """[(process, column, trend)] of every flagged series."""
  return [(process, column, result)
          for process, results in sorted(judged.items())
          for column, result in sorted(results.items())
          if result['flagged']]

def mean_cores(rows):
  """Average cores the whole namespace used, from the first to the last
  row's namespace CPU time, or None."""
  usable = [row for row in rows if row.get('namespace_cpu_s') is not None]
  if len(usable) < 2:
    return None
  seconds = usable[-1]['elapsed_s'] - usable[0]['elapsed_s']
  if seconds <= 0:
    return None
  used = usable[-1]['namespace_cpu_s'] - usable[0]['namespace_cpu_s']
  return used / seconds

def summary_markdown(mode, options, rows, judged, failures, info):
  """The run's short Markdown summary.

  `options` are the run's settings as {name: value}, `failures` the
  crash, protocol error and check failure lines, `info` extra
  {name: value} lines such as cycle counts.
  """
  lines = [f'# Soak run: {mode}', '']
  for name, value in options.items():
    lines.append(f'- {name}: {value}')
  for name, value in info.items():
    lines.append(f'- {name}: {value}')
  cores = mean_cores(rows)
  if cores is not None:
    lines.append(f'- mean CPU of everything in the namespace: '
                 f'{cores:.2f} cores')
  growth = flagged(judged)
  verdict = 'FAIL' if failures or growth else 'PASS'
  lines += ['', f'Result: **{verdict}**', '']
  by_process = {}
  for row in rows:
    by_process.setdefault(row['process'], []).append(row)
  for process, process_rows in sorted(by_process.items()):
    first, last = process_rows[0], process_rows[-1]
    lines += [f'## {process}', '',
              '| metric | first sample | last sample | trend first third '
              '| trend last third | per hour | flagged |',
              '|---|---|---|---|---|---|---|']
    for column, result in sorted(judged.get(process, {}).items()):
      lines.append(
          f'| {column} | {format_cell(first.get(column))} '
          f'| {format_cell(last.get(column))} '
          f'| {format_cell(result["first"])} '
          f'| {format_cell(result["last"])} '
          f'| {format_cell(result["slope_per_hour"])} '
          f'| {"yes" if result["flagged"] else "no"} |')
    rates = [rate for rate in cpu_rates(process_rows) if rate is not None]
    if rates:
      lines.append('')
      lines.append(f'CPU: {last["cpu_s"] - first["cpu_s"]:.1f} s over '
                   f'{(last["elapsed_s"] - first["elapsed_s"]) / 60:.1f} '
                   f'min, median {statistics.median(rates):.2f} s a minute, '
                   f'highest {max(rates):.2f}.')
    latencies = [row['latency_p99_ms'] for row in process_rows
                 if row.get('latency_p99_ms') is not None]
    medians = [row['latency_p50_ms'] for row in process_rows
               if row.get('latency_p50_ms') is not None]
    if latencies and process == 'shingle':
      lines.append(f'Frame latency, commit to presentation: median of the '
                   f'per-minute medians {statistics.median(medians):.1f} '
                   f'ms, highest per-minute p99 {max(latencies):.1f} ms.')
    lines.append('')
  if growth:
    lines.append('## Growth')
    lines.append('')
    for process, column, result in growth:
      lines.append(f'- {process} {column}: {format_cell(result["first"])} '
                   f'to {format_cell(result["last"])}')
    lines.append('')
  if failures:
    lines.append('## Failures')
    lines.append('')
    for failure in failures:
      lines.append(f'- {failure}')
    lines.append('')
  return '\n'.join(lines)

def format_cell(value):
  """A summary table cell."""
  if value is None:
    return '-'
  if isinstance(value, float):
    return f'{value:.1f}'
  return str(value)
