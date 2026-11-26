"""Real-client checks of the clipboard nested Shingle shares with its host,
run inside run.py's namespace.

Nested Shingle runs on lavapipe as a client of a headless Shingle, which
plays the host and offers `zwlr_data_control_manager_v1`; neither is ever
a client of the operator's desktop. The group `nested-clipboard` checks,
with `wl-copy --foreground` and `wl-paste --no-newline`:

- that nested binds the host's data-control for both selections;
- for the clipboard and the primary selection, at 10 bytes and 1 MiB,
  that what `wl-copy` offers on the headless Shingle's socket is what
  `wl-paste` reads on nested's, and the reverse, and that the `wl-copy`
  keeps its selection afterwards, which it would lose (and exit) if the
  selection were mirrored back to its own compositor;
- that while a 1 MiB reader in either direction holds its pipe without
  reading, both compositors answer IPC at once and the reader gets all of
  it in the end;
- that under `disable_primary_selection` nested shares only the
  clipboard, and that with `behavior.host_clipboard: false` it shares
  nothing.

Every helper is a child of this process inside the PID namespace and is
stopped through `run.signal_checked`, via the XWayland harness's
`Helper`.
"""

import os
import subprocess
import time

import checks
import nested_output_checks

xw = checks.xw
GROUPS = ('nested-clipboard',)
SELECTIONS = ('clipboard', 'primary')
SIZES = (10, 1 << 20)
# How long a slow reader holds its pipe before reading, in seconds.
STALL = 2.0
# The longest an IPC call may take while a transfer stalls, in seconds.
IPC_LIMIT = 0.5
# How long a check waits for something not to happen, in seconds.
QUIET = 1.5

def wl_options(selection):
  """wl-copy's and wl-paste's options for 'clipboard' or 'primary'."""
  if selection == 'clipboard':
    return []
  if selection == 'primary':
    return ['--primary']
  raise ValueError(selection)

def shared_line(log):
  """The selections nested says it shares and the protocol, from its
  `sharing [...] with the host through NAME` line, or None."""
  for line in log.splitlines():
    if 'sharing [' in line and 'with the host through' in line:
      kinds = line.split('[', 1)[1].split(']', 1)[0]
      names = tuple(kind.strip().lower() for kind in kinds.split(',')
                    if kind.strip())
      return names, line.rsplit(' ', 1)[1]
  return None

def copy(shingle, selection, data):
  """`wl-copy --foreground` offering `data` as `selection` on `shingle`."""
  return xw.Helper(['wl-copy', '--foreground', *wl_options(selection)],
                   shingle.client_environment(), 'wl-copy', data)

def paste(shingle, selection, timeout=10):
  """What `wl-paste -n` reads of `selection` on `shingle`, or None."""
  try:
    done = subprocess.run(['wl-paste', '--no-newline',
                           *wl_options(selection)],
                          env=shingle.client_environment(),
                          capture_output=True, timeout=timeout,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  return done.stdout if done.returncode == 0 else None

def latency(shingle):
  """How long one IPC call of `shingle` takes, in seconds."""
  started = time.monotonic()
  shingle.call('window-rules/list-outputs')
  return time.monotonic() - started

def slow_read(results, label, reader_shingle, writer_shingles, selection,
              data):
  """A `wl-paste` of `selection` on `reader_shingle` whose pipe is not
  read for STALL seconds: both compositors must answer IPC meanwhile,
  and the reader must get `data` in the end."""
  out = os.path.join(reader_shingle.directory, f'slow-{selection}')
  options = ' '.join(wl_options(selection))
  started = time.monotonic()
  reader = xw.Helper(['sh', '-c', f'wl-paste --no-newline {options} | '
                      f'(sleep {STALL}; cat > {out})'],
                     reader_shingle.client_environment(), 'sh')
  try:
    time.sleep(0.5)
    latencies = [latency(shingle) for shingle in writer_shingles
                 for _ in range(5)]
    results.check(f'nested-clipboard: {label}: both compositors answer IPC '
                  'while the reader stalls',
                  max(latencies) < IPC_LIMIT,
                  f'slowest {max(latencies):.3f} s')
    status = reader.wait(30)
    elapsed = time.monotonic() - started
    received = None
    if os.path.exists(out):
      with open(out, 'rb') as file:
        received = file.read()
    results.check(f'nested-clipboard: {label}: the slow reader gets it all',
                  status == 0 and received == data and elapsed >= STALL,
                  f'{xw.compare(received, data)} after {elapsed:.1f} s')
  finally:
    reader.stop()

def transfer(results, source, target, names, selection, size, seed):
  """`wl-copy` on `source`, `wl-paste` on `target`: the data arrives,
  and the owner keeps its selection. `names` are the two sides' names
  for the labels."""
  data = xw.payload(size, seed)
  label = f'{selection} {xw.payload_label(size)} from {names[0]} to ' \
          f'{names[1]}'
  owner = copy(source, selection, data)
  try:
    pasted = xw.wait_paste(lambda: paste(target, selection), data,
                           timeout=15)
    results.check(f'nested-clipboard: {label}', pasted == data,
                  f'{xw.compare(pasted, data)}; wl-copy: '
                  f'{owner.errors()[-200:]!r}')
    time.sleep(QUIET)
    results.check(f'nested-clipboard: {label}: wl-copy keeps the selection, '
                  'which is not mirrored back',
                  owner.process.poll() is None
                  and paste(source, selection) == data,
                  f'wl-copy status {owner.process.poll()}')
    if size >= 1 << 20:
      slow_read(results, label, target, (source, target), selection, data)
  finally:
    owner.stop()

def sharing_checks(results, outer, nested):
  """Both selections, both ways, small and large."""
  line = xw.wait_for(lambda: shared_line(nested.log_text()), timeout=15)
  results.check('nested-clipboard: nested shares both selections through '
                'the host\'s wlr-data-control',
                line == (('clipboard', 'primary'),
                         'zwlr_data_control_manager_v1'),
                str(line))
  seed = 0
  for selection in SELECTIONS:
    for size in SIZES:
      seed += 1
      transfer(results, outer, nested, ('host', 'nested'), selection, size,
               seed)
      seed += 1
      transfer(results, nested, outer, ('nested', 'host'), selection, size,
               seed)

def not_shared(results, label, outer, nested, selection, seed):
  """What either side copies of `selection` does not reach the other."""
  for source, target, names in ((outer, nested, ('host', 'nested')),
                                (nested, outer, ('nested', 'host'))):
    data = xw.payload(10, seed)
    owner = copy(source, selection, data)
    try:
      own = xw.wait_paste(lambda source=source: paste(source, selection),
                          data, timeout=10)
      time.sleep(QUIET)
      crossed = paste(target, selection)
      results.check(f'nested-clipboard: {label}: the {selection} does not '
                    f'cross from {names[0]} to {names[1]}',
                    own == data and crossed != data,
                    f'own {xw.compare(own, data)}, other side '
                    f'{xw.compare(crossed, data)}')
    finally:
      owner.stop()

def disabled_primary_checks(results, binary):
  """`disable_primary_selection`: only the clipboard is shared."""
  inner = nested_output_checks.config()
  inner['behavior'] = dict(inner.get('behavior', {}),
                           disable_primary_selection=True)
  outer, nested = nested_output_checks.start(
      binary, 'nested-clipboard-noprimary', nested_config=inner)
  try:
    line = xw.wait_for(lambda: shared_line(nested.log_text()), timeout=15)
    results.check('nested-clipboard: under disable_primary_selection nested '
                  'shares only the clipboard',
                  line is not None and line[0] == ('clipboard',), str(line))
    transfer(results, outer, nested, ('host', 'nested'), 'clipboard', 10,
             40)
    not_shared(results, 'disable_primary_selection', outer, nested,
               'primary', 41)
  finally:
    status = nested.stop()
    outer_status = outer.stop()
  results.check('nested-clipboard: SIGTERM exits both Shingles without '
                'primary', status == 0 and outer_status == 0,
                f'nested {status}, host {outer_status}')

def turned_off_checks(results, binary):
  """`behavior.host_clipboard: false`: nothing is shared."""
  inner = nested_output_checks.config()
  inner['behavior'] = dict(inner.get('behavior', {}), host_clipboard=False)
  outer, nested = nested_output_checks.start(
      binary, 'nested-clipboard-off', nested_config=inner)
  try:
    time.sleep(QUIET)
    results.check('nested-clipboard: with host_clipboard off nested does not '
                  'bind data-control', shared_line(nested.log_text()) is None,
                  checks.tail(nested.log_text()))
    not_shared(results, 'host_clipboard off', outer, nested, 'clipboard', 50)
  finally:
    status = nested.stop()
    outer_status = outer.stop()
  results.check('nested-clipboard: SIGTERM exits both Shingles with the '
                'bridge off', status == 0 and outer_status == 0,
                f'nested {status}, host {outer_status}')

def nested_clipboard_checks(results, binary, wl_probe, captures):
  """Nested Shingle's host clipboard in a headless Shingle."""
  del wl_probe, captures
  outer, nested = nested_output_checks.start(binary, 'nested-clipboard')
  try:
    sharing_checks(results, outer, nested)
  finally:
    status = nested.stop()
    outer_status = outer.stop()
  if status != 0:
    print(f'--- nested log ---\n{nested.log_text()}--- end ---', flush=True)
  results.check('nested-clipboard: SIGTERM exits both Shingles with status '
                '0', status == 0 and outer_status == 0,
                f'nested {status}, host {outer_status}')
  disabled_primary_checks(results, binary)
  turned_off_checks(results, binary)

CHECKS = {
    'nested-clipboard': nested_clipboard_checks,
}
