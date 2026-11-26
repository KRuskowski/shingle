"""DB-4's and DB-5's real-client checks of linux-dmabuf and
linux-drm-syncobj, run inside run.py's namespace.

docs/design/dmabuf.md, section 11, DB-4 and DB-5. Every Shingle here runs
with `SHINGLE_TRACE_BUFFERS=1`, so its log says which path each buffer
took (`dmabuf import`, `dmabuf cached`, `shm upload`, `blocked implicit`,
`blocked syncobj`, `syncobj acquire ready`, `syncobj release`,
`release`), and the checks judge clients by it as well as by captures:

- `dmabuf`: `wayland-info` sees `zwp_linux_dmabuf_v1` version 5 with main
  device 226:128 and every pair of the caps, and
  `wp_linux_drm_syncobj_manager_v1` version 1; `vkcube` runs to its end,
  changes between captures, imports each swapchain image once and
  commits with syncobj acquire and release points; `vkcube` on lavapipe
  stays on `wl_shm`; `es2gears_wayland`, `eglgears_wayland` and `kitty`
  map and draw, and their path is recorded.
- `browsers`: `firefox` and `chromium --ozone-platform=wayland`, one at a
  time, each with a fresh profile in the namespace's private HOME and no
  network, show a local page.
- `xwayland`: `glxgears` through Xwayland draws, and Xwayland's log
  shows glamor on the NVIDIA render node.
- `nested-gpu`: nested Shingle on NVIDIA presents through offscreen
  Shingle's import, and its own shm and GPU clients draw.
- `nodmabuf`: with `SHINGLE_DMABUF=0` there is no global, and DB-2's
  baseline repeats.
- `weston`: `weston-simple-dmabuf-feedback` prints the tranche,
  `weston-simple-dmabuf-egl` draws, and `mpv` plays a test pattern with
  Vulkan and with OpenGL output.
- `syncobj` (DB-5): `syncobj_probe` stalls one toplevel on an acquire
  point nobody signals while its other toplevel keeps drawing; the
  stalled one keeps its last shm content and gets no frame callback,
  and destroying it releases both its DMA-BUFs and signals both their
  release points.
- `nosyncobj` (DB-5): `dmabuf`, `xwayland` and `nested-gpu` again with
  `SHINGLE_SYNCOBJ=0`: no syncobj global, and the clients pass on
  implicit sync.

`xwayland` and `nested-gpu` check syncobj use too: Xwayland's glamor
presents `glxgears`, nested Shingle presents its window and `vkcube`
inside nested Shingle presents, all with acquire and release points.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py, to children of this process inside the PID
namespace.
"""

import json
import os
import re
import subprocess

import checks

xw = checks.xw
TRACE = {'SHINGLE_TRACE_BUFFERS': '1'}
# makedev(226, 128), as wayland-info prints the main device.
RENDER_NODE = 0xE280
FOURCCS = ('AR24', 'XR24', 'AB24', 'XB24')
# What DB-1 found this host's NVIDIA driver samples: six block-linear
# modifiers and LINEAR for every format (docs/status.md).
NVIDIA_MODIFIERS = tuple(0x0300000000606010 + height
                         for height in range(6)) + (0,)
# A page in one colour, for the browsers.
PAGE = ('<!doctype html><html><body style="margin:0;background:#00ff00">'
        '</body></html>')
FIREFOX_PREFERENCES = (
    'browser.shell.checkDefaultBrowser', 'false',
    'browser.aboutwelcome.enabled', 'false',
    'browser.startup.homepage_override.mstone', '"ignore"',
    'datareporting.policy.dataSubmissionEnabled', 'false',
    'toolkit.telemetry.reportingpolicy.firstRun', 'false',
    'app.update.enabled', 'false',
    'browser.tabs.warnOnClose', 'false',
    'browser.sessionstore.resume_from_crash', 'false',
)
# Why EGL clients are only recorded (docs/status.md).
EGL_NOTE = ('NVIDIA EGL cannot create a Wayland EGLDisplay without '
            '/dev/dri/card0, which the namespace hides')
GROUPS = ('dmabuf', 'browsers', 'xwayland', 'nested-gpu', 'nodmabuf',
          'weston', 'syncobj', 'nosyncobj')
# The log line of an offered syncobj global on this host.
SYNCOBJ_LINE = 'linux-drm-syncobj version 1 on render node 226:128'

class Mode:
  """How a group runs: with linux-drm-syncobj, as by default, or with
  `SHINGLE_SYNCOBJ=0`, under /tmp names of its own."""

  def __init__(self, syncobj=True):
    self.syncobj = syncobj
    self.suffix = '' if syncobj else '-nosyncobj'

  def environment(self, extra=None):
    """TRACE, `SHINGLE_SYNCOBJ=0` when off, and `extra`."""
    environment = dict(TRACE)
    if not self.syncobj:
      environment['SHINGLE_SYNCOBJ'] = '0'
    environment.update(extra or {})
    return environment

  def name(self, name):
    """A Shingle's /tmp name in this mode."""
    return name + self.suffix

DEFAULT = Mode()

class Prefixed:
  """Results whose check names get a prefix, for a group run again."""

  def __init__(self, results, prefix):
    self.results = results
    self.prefix = prefix

  def check(self, name, passed, detail=''):
    """Records one check under the prefix."""
    return self.results.check(self.prefix + name, passed, detail)

def dmabuf_global(text):
  """What `wayland-info`'s `text` says of linux-dmabuf: (version, main
  device, [(fourcc, modifier)] of the default feedback), with None for
  what is missing."""
  version = device = None
  pairs = []
  inside = False
  for line in text.splitlines():
    found = re.match(r"interface: '(\w+)',\s+version:\s+(\d+)", line)
    if found:
      inside = found.group(1) == 'zwp_linux_dmabuf_v1'
      if inside:
        version = int(found.group(2))
      continue
    if not inside:
      continue
    found = re.search(r'main device: (0x[0-9A-Fa-f]+)', line)
    if found:
      device = int(found.group(1), 16)
    found = re.search(r"= '(\w{4})'; (0x[0-9A-Fa-f]+) =", line)
    if found:
      pairs.append((found.group(1), int(found.group(2), 16)))
  return version, device, pairs

def syncobj_global(text):
  """The version of `wp_linux_drm_syncobj_manager_v1` in `wayland-info`'s
  `text`, or None."""
  found = re.search(r"interface: 'wp_linux_drm_syncobj_manager_v1',\s+"
                    r"version:\s+(\d+)", text)
  return int(found.group(1)) if found else None

def expected_pairs():
  """The (fourcc, modifier) pairs DB-1 recorded for this host."""
  return {(fourcc, modifier) for fourcc in FOURCCS
          for modifier in NVIDIA_MODIFIERS}

def trace(text):
  """The buffer trace lines of Shingle's log `text`, as (kind, fields):
  kind is `import`, `cached`, `shm`, `blocked` (implicit),
  `syncobj_wait`, `syncobj_ready`, `syncobj_release` or `release`,
  fields the `key=value` pairs and, for `shm`, `blocked` and
  `syncobj_wait`, the number as `amount`."""
  events = []
  for line in text.splitlines():
    if not line.startswith('shingle: buffer: '):
      continue
    message = line[len('shingle: buffer: '):]
    fields = dict(re.findall(r'(\w+)=(\S+)', message))
    if message.startswith('dmabuf import'):
      kind = 'import'
    elif message.startswith('dmabuf cached'):
      kind = 'cached'
    elif message.startswith('shm upload'):
      kind = 'shm'
      fields['amount'] = int(message.split()[2])
    elif message.startswith('blocked syncobj'):
      kind = 'syncobj_wait'
      fields['amount'] = int(message.split()[2])
    elif message.startswith('blocked'):
      kind = 'blocked'
      fields['amount'] = int(message.split()[2])
    elif message.startswith('syncobj acquire ready'):
      kind = 'syncobj_ready'
    elif message.startswith('syncobj release'):
      kind = 'syncobj_release'
    elif message.startswith('release'):
      kind = 'release'
    else:
      continue
    events.append((kind, fields))
  return events

def summarise(events):
  """Counts of `trace` events: {'imports': {serial: n}, 'cached': set of
  serials, 'shm': n, 'blocked': n, 'releases': n, 'syncobj_waits': n,
  'syncobj_ready': n, 'syncobj_releases': n, 'syncobj_wait_us': [µs]}."""
  summary = {'imports': {}, 'cached': set(), 'shm': 0, 'blocked': 0,
             'releases': 0, 'syncobj_waits': 0, 'syncobj_ready': 0,
             'syncobj_releases': 0, 'syncobj_wait_us': []}
  for kind, fields in events:
    if kind == 'import':
      serial = fields.get('dmabuf')
      summary['imports'][serial] = summary['imports'].get(serial, 0) + 1
    elif kind == 'cached':
      summary['cached'].add(fields.get('dmabuf'))
    elif kind == 'shm':
      summary['shm'] += 1
    elif kind == 'blocked':
      summary['blocked'] += 1
    elif kind == 'release':
      summary['releases'] += 1
    elif kind == 'syncobj_wait':
      summary['syncobj_waits'] += 1
      summary['syncobj_wait_us'].append(fields['amount'])
    elif kind == 'syncobj_ready':
      summary['syncobj_ready'] += 1
    elif kind == 'syncobj_release':
      summary['syncobj_releases'] += 1
  return summary

def describe(summary):
  """One line for a `summarise` result."""
  imports = summary['imports']
  return (f'{len(imports)} buffers imported '
          f'({sum(imports.values())} imports), '
          f'{len(summary["cached"])} used from the cache, '
          f'{summary["shm"]} shm uploads, {summary["blocked"]} fence waits, '
          f'{summary["releases"]} releases, syncobj: '
          f'{summary["syncobj_waits"]} acquire waits '
          f'({median(summary["syncobj_wait_us"])} µs median), '
          f'{summary["syncobj_ready"]} acquires ready at commit, '
          f'{summary["syncobj_releases"]} release points')

def median(values):
  """The median of `values`, or 0."""
  ordered = sorted(values)
  return ordered[len(ordered) // 2] if ordered else 0

def syncobj_acquires(summary):
  """How many commits came with an acquire point."""
  return summary['syncobj_waits'] + summary['syncobj_ready']

def syncobj_check(results, mode, label, summary):
  """With syncobj on, that `label` committed with syncobj acquire points,
  which Shingle checked and, if unsignalled, waited on, and that their
  release points were signalled; with it off, that none did. Whether a
  commit had to wait depends on the client's timing, so the waits are
  in the detail, not judged."""
  if mode.syncobj:
    results.check(f'{label} commits with syncobj acquire and release '
                  'points',
                  syncobj_acquires(summary) > 0
                  and summary['syncobj_releases'] > 0, describe(summary))
  else:
    results.check(f'{label} uses implicit sync without syncobj points',
                  syncobj_acquires(summary) == 0
                  and summary['syncobj_releases'] == 0, describe(summary))

def syncobj_global_check(results, mode, group, shingle, text):
  """`wayland-info`'s `text` lists the syncobj global when it is on, and
  not when it is off; `group`'s Shingle's log says which."""
  version = syncobj_global(text)
  log = shingle.log_text()
  if mode.syncobj:
    results.check(f'{group}: wayland-info lists '
                  'wp_linux_drm_syncobj_manager_v1 version 1',
                  version == 1 and SYNCOBJ_LINE in log,
                  f'version {version}, {checks.tail(log, 3)}')
  else:
    results.check(f'{group}: SHINGLE_SYNCOBJ=0 offers no '
                  'wp_linux_drm_syncobj_manager_v1 and says so',
                  version is None and 'SHINGLE_SYNCOBJ=0' in log,
                  f'version {version}, {checks.tail(log, 3)}')

def drawn(shot, geometry, background=(89, 89, 89), step=8):
  """The share of sampled pixels in `geometry` that are not the
  `background`."""
  if shot is None or geometry is None:
    return 0.0
  total = changed = 0
  for y in range(geometry['y'], geometry['y'] + geometry['height'], step):
    for x in range(geometry['x'], geometry['x'] + geometry['width'], step):
      if not (0 <= x < shot.width and 0 <= y < shot.height):
        continue
      total += 1
      pixel = shot.pixel(x, y)
      if any(abs(a - b) > 6 for a, b in zip(pixel, background)):
        changed += 1
  return changed / total if total else 0.0

def differs(first, second, geometry, step=4):
  """How many sampled pixels of `geometry` differ between two shots."""
  if first is None or second is None or geometry is None:
    return 0
  count = 0
  for y in range(geometry['y'], geometry['y'] + geometry['height'], step):
    for x in range(geometry['x'], geometry['x'] + geometry['width'], step):
      if not (0 <= x < first.width and 0 <= y < first.height):
        continue
      if first.pixel(x, y) != second.pixel(x, y):
        count += 1
  return count

def colour_share(shot, geometry, name, step=8):
  """The share of sampled pixels of `geometry` that `checks.classify`
  names `name`."""
  if shot is None or geometry is None:
    return 0.0
  total = hits = 0
  for y in range(geometry['y'], geometry['y'] + geometry['height'], step):
    for x in range(geometry['x'], geometry['x'] + geometry['width'], step):
      if 0 <= x < shot.width and 0 <= y < shot.height:
        total += 1
        hits += checks.classify(*shot.pixel(x, y)) == name
  return hits / total if total else 0.0

def firefox_user_js(preferences=FIREFOX_PREFERENCES):
  """The `user.js` of a throwaway Firefox profile."""
  pairs = zip(preferences[::2], preferences[1::2])
  return ''.join(f'user_pref("{name}", {value});\n' for name, value in pairs)

def private_profile(name):
  """A fresh, empty profile directory under the namespace's private HOME
  (run.py makes HOME a directory of its work directory), never the
  operator's."""
  home = os.environ['HOME']
  if not home.startswith(('/tmp/', os.path.abspath('target'))):
    raise RuntimeError(f'HOME {home} is not the namespace\'s private one')
  path = os.path.join(home, f'{name}-profile')
  os.makedirs(path)
  return path

def one_view(shingle):
  """The one mapped view of `shingle`, or None."""
  views = [view for view in shingle.views() if view.get('mapped', True)]
  return views[0] if len(views) == 1 else None

class Run:
  """One GPU client on `shingle`, with the trace it caused."""

  def __init__(self, shingle, name, arguments, extra_environment=None,
               program=None):
    self.shingle = shingle
    self.mark = len(shingle.log_text())
    self.client = checks.Client(shingle, arguments, name,
                                extra_environment, program=program)
    self.view = xw.wait_for(lambda: one_view(shingle), timeout=30)

  def geometry(self):
    """Where the view's client draws, inside a server-side frame if it
    has one (WG-6), or None."""
    return self.view['base-geometry'] if self.view else None

  def summary(self):
    """`summarise` of the trace since the client started."""
    return summarise(trace(self.shingle.log_text()[self.mark:]))

def client_checks(results, outer, captures, name, arguments, environment=None,
                  program=None, settle=20, egl=False):
  """Runs one GPU client that keeps running and checks that it maps,
  draws and imports DMA-BUFs; stops it and returns the run. For an `egl`
  client the path is only recorded: NVIDIA's EGL cannot start its
  Wayland platform without the card node, which the namespace never
  has, so such clients draw with Mesa's software renderer on wl_shm
  here (EGL_NOTE)."""
  run_ = Run(outer, name, arguments, environment, program)
  try:
    results.check(f'dmabuf: {name} maps', run_.view is not None,
                  checks.tail(run_.client.output()))
    geometry = run_.geometry()
    shot = captures.settled(outer, lambda shot: drawn(shot, geometry) > 0.2,
                            timeout=settle)
    results.check(f'dmabuf: {name} draws', drawn(shot, geometry) > 0.2,
                  f'{drawn(shot, geometry):.2f} of its view is drawn, '
                  f'geometry {json.dumps(geometry)}')
    summary = run_.summary()
    print(f'INFO dmabuf: {name}: {describe(summary)}', flush=True)
    if egl:
      print(f'INFO dmabuf: {name}: {EGL_NOTE}; it took '
            f'{"DMA-BUFs" if summary["imports"] else "wl_shm"}', flush=True)
    else:
      results.check(f'dmabuf: {name} draws from imported DMA-BUFs',
                    summary['imports'] and summary['cached'],
                    describe(summary))
  finally:
    run_.client.stop()
  xw.wait_for(lambda: not outer.views(), timeout=15)
  return run_

def wayland_info(shingle):
  """`wayland-info`'s output on `shingle`."""
  done = subprocess.run(['wayland-info'], env=shingle.client_environment(),
                        capture_output=True, text=True, timeout=30,
                        stdin=subprocess.DEVNULL)
  return done.stdout

def dmabuf_checks(results, binary, wl_probe, captures, mode=DEFAULT):
  """wayland-info, vkcube on NVIDIA and lavapipe, the gears and kitty."""
  del wl_probe
  outer = checks.Shingle(binary, mode.name('dmabuf'),
                         checks.with_gaps(checks.OUTER_GAPS),
                         extra_environment=mode.environment())
  info = wayland_info(outer)
  version, device, pairs = dmabuf_global(info)
  syncobj_global_check(results, mode, 'dmabuf', outer, info)
  logged = re.search(r'linux-dmabuf version 5 on render node 226:128, '
                     r'(\d+) format', outer.log_text())
  results.check('dmabuf: wayland-info shows zwp_linux_dmabuf_v1 version 5 '
                'with main device 226:128',
                version == 5 and device == RENDER_NODE,
                f'version {version}, main device {device and hex(device)}')
  results.check(
      'dmabuf: the default feedback lists the caps\' pairs',
      set(pairs) == expected_pairs() and len(pairs) == len(expected_pairs())
      and logged is not None and int(logged.group(1)) == len(pairs),
      f'{len(pairs)} pairs, log {logged and logged.group(0)}')
  print(f'INFO dmabuf: {len(pairs)} pairs: '
        f'{sorted({hex(modifier) for _, modifier in pairs})}', flush=True)
  # vkcube runs to its end by itself.
  run_ = Run(outer, 'vkcube', ['vkcube', '--wsi', 'wayland', '--c', '600'])
  try:
    results.check('dmabuf: vkcube maps', run_.view is not None,
                  checks.tail(run_.client.output()))
    geometry = run_.geometry()
    first = captures.settled(outer, lambda shot: drawn(shot, geometry) > 0.2)
    second = None
    for _ in range(10):
      second = captures.take(outer)
      if differs(first, second, geometry) > 20:
        break
    results.check('dmabuf: captures show the cube changing between frames',
                  drawn(first, geometry) > 0.2
                  and differs(first, second, geometry) > 20,
                  f'{drawn(first, geometry):.2f} drawn, '
                  f'{differs(first, second, geometry)} samples differ')
    status = run_.client.exited(90)
    results.check('dmabuf: vkcube --c 600 runs to the end', status == 0,
                  f'status {status}, {checks.tail(run_.client.output())}')
    summary = run_.summary()
    print(f'INFO dmabuf: vkcube: {describe(summary)}', flush=True)
    imports = summary['imports']
    results.check(
        'dmabuf: vkcube imports each swapchain image once and then uses '
        'the cache',
        imports and max(imports.values()) == 1
        and summary['cached'] and summary['cached'] <= set(imports)
        and summary['shm'] == 0,
        describe(summary))
    syncobj_check(results, mode, 'dmabuf: vkcube', summary)
  finally:
    run_.client.stop()
  xw.wait_for(lambda: not outer.views(), timeout=15)
  run_ = Run(outer, 'vkcube-lavapipe',
             ['vkcube', '--wsi', 'wayland', '--c', '200'],
             {'VK_ICD_FILENAMES': checks.LAVAPIPE}, program='vkcube')
  try:
    run_.client.exited(60)
    summary = run_.summary()
    print(f'INFO dmabuf: vkcube on lavapipe: {describe(summary)}',
          flush=True)
    results.check('dmabuf: vkcube on lavapipe stays on wl_shm',
                  run_.view is not None and summary['shm'] > 0
                  and not summary['imports'],
                  f'{describe(summary)}, {checks.tail(run_.client.output())}')
  finally:
    run_.client.stop()
  xw.wait_for(lambda: not outer.views(), timeout=15)
  for name, arguments, program in (
      ('es2gears_wayland', ['es2gears_wayland'], None),
      ('eglgears_wayland', ['eglgears_wayland'], None),
      ('kitty', ['kitty', '--config', 'NONE', '-o', 'background=#ff0000',
                 'sh', '-c', 'sleep 60'], None)):
    client_checks(results, outer, captures, name, arguments, program=program,
                  egl=True)
  status = outer.stop()
  results.check('dmabuf: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def browser_checks(results, binary, wl_probe, captures):
  """Firefox and Chromium, one after the other, show a green page."""
  del wl_probe
  outer = checks.Shingle(binary, 'browsers', checks.BASE_CONFIG,
                         extra_environment=TRACE)
  page = os.path.join(outer.directory, 'page.html')
  with open(page, 'w') as file:
    file.write(PAGE)
  firefox = private_profile('firefox')
  with open(os.path.join(firefox, 'user.js'), 'w') as file:
    file.write(firefox_user_js())
  chromium = private_profile('chromium')
  # Firefox draws with EGL and so takes wl_shm here (EGL_NOTE); Chromium's
  # GPU process presents DMA-BUFs.
  for name, arguments, environment, program, dmabuf in (
      ('firefox', ['firefox-esr', '--no-remote', '--new-instance',
                   '--profile', firefox, f'file://{page}'],
       {'MOZ_ENABLE_WAYLAND': '1'}, 'firefox-esr', False),
      ('chromium', ['chromium', '--ozone-platform=wayland',
                    f'--user-data-dir={chromium}', '--no-first-run',
                    '--no-default-browser-check', '--no-sandbox',
                    '--password-store=basic', f'file://{page}'],
       {}, 'chromium', True)):
    run_ = Run(outer, name, arguments, environment, program)
    try:
      results.check(f'browsers: {name} maps', run_.view is not None,
                    checks.tail(run_.client.output()))
      geometry = run_.geometry()
      shot = captures.settled(
          outer, lambda shot: colour_share(shot, geometry, 'green') > 0.3,
          timeout=60)
      share = colour_share(shot, geometry, 'green')
      results.check(f'browsers: {name} draws the page', share > 0.3,
                    f'{share:.2f} of its view is the page\'s green')
      summary = run_.summary()
      print(f'INFO browsers: {name}: {describe(summary)}', flush=True)
      if dmabuf:
        results.check(f'browsers: {name} presents DMA-BUFs',
                      summary['imports'] and summary['cached'],
                      describe(summary))
      else:
        print(f'INFO browsers: {name}: {EGL_NOTE}; it took '
              f'{"DMA-BUFs" if summary["imports"] else "wl_shm"}',
              flush=True)
    finally:
      run_.client.stop()
    xw.wait_for(lambda: not outer.views(), timeout=30)
  status = outer.stop()
  results.check('browsers: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def xwayland_wrapper(directory):
  """A script that runs Xwayland with a verbose log, so that glamor's
  device shows; Shingle takes it from SHINGLE_XWAYLAND."""
  path = os.path.join(directory, 'Xwayland')
  with open(path, 'w') as file:
    file.write('#!/bin/sh\nexec /usr/bin/Xwayland "$@" -verbose 3\n')
  os.chmod(path, 0o755)
  return path

def xwayland_devices(shingle):
  """The device files the running Xwayland of `shingle` has open, by
  `stipc/get_xwayland_pid` and /proc; empty when it is not running."""
  pid = shingle.call('stipc/get_xwayland_pid').get('pid', -1)
  if not isinstance(pid, int) or pid <= 1:
    return set()
  devices = set()
  try:
    for fd in os.listdir(f'/proc/{pid}/fd'):
      try:
        target = os.readlink(f'/proc/{pid}/fd/{fd}')
      except OSError:
        continue
      if target.startswith('/dev/'):
        devices.add(target)
  except OSError:
    pass
  return devices

def x_display(shingle):
  """The DISPLAY Shingle printed, or None."""
  return xw.wait_for(lambda: next(
      (line.split('=', 1)[1] for line in shingle.log_text().splitlines()
       if line.startswith('DISPLAY=')), None), timeout=30)

def xwayland_checks(results, binary, wl_probe, captures, mode=DEFAULT):
  """glxgears through Xwayland with glamor on the render node."""
  del wl_probe
  directory = os.path.join('/tmp', mode.name('xwayland-bin'))
  os.makedirs(directory)
  config = dict(checks.BASE_CONFIG)
  config['xwayland'] = {'mode': 'lazy'}
  outer = checks.Shingle(
      binary, mode.name('xwayland'), config,
      extra_environment=mode.environment(
          {'SHINGLE_XWAYLAND': xwayland_wrapper(directory)}))
  display = x_display(outer)
  results.check('xwayland: Shingle binds an X display', display is not None,
                checks.tail(outer.log_text()))
  info = subprocess.run(['glxinfo', '-B'],
                        env=outer.client_environment({'DISPLAY': display}),
                        capture_output=True, text=True, timeout=60,
                        stdin=subprocess.DEVNULL)
  renderer = next((line.strip() for line in info.stdout.splitlines()
                   if 'OpenGL renderer string' in line), info.stderr[-300:])
  print(f'INFO xwayland: glxinfo: {renderer}', flush=True)
  run_ = Run(outer, 'glxgears', ['glxgears'], {'DISPLAY': display})
  try:
    results.check('xwayland: glxgears maps', run_.view is not None,
                  checks.tail(run_.client.output()))
    geometry = run_.geometry()
    shot = captures.settled(outer, lambda shot: drawn(shot, geometry) > 0.2)
    results.check('xwayland: glxgears draws', drawn(shot, geometry) > 0.2,
                  f'{drawn(shot, geometry):.2f} drawn')
    summary = run_.summary()
    print(f'INFO xwayland: glxgears: {describe(summary)}', flush=True)
    results.check('xwayland: Xwayland presents glxgears as DMA-BUFs',
                  summary['imports'] and summary['cached'],
                  describe(summary))
    syncobj_check(results, mode, 'xwayland: Xwayland', summary)
    devices = xwayland_devices(outer)
  finally:
    run_.client.stop()
  log = outer.log_text()
  glamor = [line.strip() for line in log.splitlines()
            if 'glamor' in line.lower() or 'renderD128' in line]
  print(f'INFO xwayland: Xwayland log: {" | ".join(glamor[:8])}',
        flush=True)
  results.check(
      'xwayland: Xwayland initialised glamor on the NVIDIA render node',
      any('glamor' in line.lower() and 'initiali' in line.lower()
          for line in glamor)
      and '/dev/dri/renderD128' in devices and 'NVIDIA' in renderer,
      f'{" | ".join(glamor[:6])}; Xwayland has {sorted(devices)} open; '
      f'{renderer}')
  status = outer.stop()
  results.check('xwayland: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def nested_gpu_checks(results, binary, wl_probe, captures, mode=DEFAULT):
  """Nested Shingle on NVIDIA inside offscreen Shingle, with clients."""
  outer = checks.Shingle(binary, mode.name('outer-gpu'),
                         checks.with_gaps(checks.OUTER_GAPS),
                         extra_environment=mode.environment())
  mark = len(outer.log_text())
  environment = mode.environment({'VK_ICD_FILENAMES': checks.NVIDIA_ICD})
  inner = checks.Shingle(
      binary, mode.name('inner-gpu'), checks.with_gaps(checks.INNER_GAPS),
      backend=('--backend', 'nested'),
      extra_environment=dict(environment,
                             WAYLAND_DISPLAY=outer.wayland_display()))
  window = xw.wait_for(lambda: checks._window_geometry(outer), timeout=30)
  offered = xw.wait_for(
      lambda: 'linux-dmabuf version 5 on render node 226:128'
      in inner.log_text(), timeout=20)
  results.check('nested-gpu: nested Shingle runs on NVIDIA and offers '
                'linux-dmabuf itself',
                window is not None and offered and inner.alive(),
                checks.tail(inner.log_text()))
  probe = checks.Client(inner, [wl_probe, 'nested-pattern', '--pattern'],
                        'wl_probe')
  try:
    view = xw.wait_for(lambda: inner.view('nested-pattern'), timeout=15)
    if view is not None and window is not None:
      offset = (window['x'], window['y'])

      def wrong(shot):
        return checks.pattern_mismatches(shot, view['geometry'],
                                         offset=offset)
      shot = captures.settled(outer, lambda shot: not wrong(shot))
      results.check('nested-gpu: the nested shm client shows in captures '
                    'of the outer output',
                    shot is not None and not wrong(shot),
                    f'wrong {wrong(shot) if shot else None}')
    else:
      results.check('nested-gpu: the nested shm client maps', False,
                    checks.tail(probe.output()))
  finally:
    probe.stop()
  xw.wait_for(lambda: not inner.views(), timeout=15)
  inner_mark = len(inner.log_text())
  # A Vulkan client: EGL ones take wl_shm here (EGL_NOTE).
  gears = checks.Client(inner, ['vkcube', '--wsi', 'wayland'], 'vkcube')
  try:
    view = xw.wait_for(lambda: one_view(inner), timeout=30)
    geometry = None
    if view is not None and window is not None:
      geometry = dict(view['base-geometry'])
      geometry['x'] += window['x']
      geometry['y'] += window['y']
    shot = captures.settled(outer, lambda shot: drawn(shot, geometry) > 0.2)
    inner_summary = summarise(trace(inner.log_text()[inner_mark:]))
    print(f'INFO nested-gpu: vkcube in nested Shingle: '
          f'{describe(inner_summary)}', flush=True)
    results.check('nested-gpu: vkcube in nested Shingle draws from '
                  'imported DMA-BUFs',
                  drawn(shot, geometry) > 0.2 and inner_summary['imports'],
                  f'{drawn(shot, geometry):.2f} drawn, '
                  f'{describe(inner_summary)}')
    syncobj_check(results, mode, 'nested-gpu: vkcube in nested Shingle',
                  inner_summary)
  finally:
    gears.stop()
  outer_summary = summarise(trace(outer.log_text()[mark:]))
  print(f'INFO nested-gpu: offscreen Shingle: {describe(outer_summary)}',
        flush=True)
  # The one shm upload is winit's cursor.
  results.check('nested-gpu: nested Shingle presents through offscreen '
                'Shingle\'s DMA-BUF import',
                outer_summary['imports'] and outer_summary['cached'],
                describe(outer_summary))
  syncobj_check(results, mode, 'nested-gpu: nested Shingle\'s window',
                outer_summary)
  status = inner.stop()
  results.check('nested-gpu: SIGTERM exits nested Shingle with status 0',
                status == 0, f'status {status}')
  status = outer.stop()
  results.check('nested-gpu: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def nodmabuf_checks(results, binary, wl_probe, captures):
  """SHINGLE_DMABUF=0: no global, and DB-2's baseline again."""
  del wl_probe
  outer = checks.Shingle(binary, 'nodmabuf', checks.BASE_CONFIG,
                         extra_environment=dict(TRACE, SHINGLE_DMABUF='0'))
  version, _, _ = dmabuf_global(wayland_info(outer))
  results.check('nodmabuf: SHINGLE_DMABUF=0 offers no zwp_linux_dmabuf_v1 '
                'and says so',
                version is None and 'SHINGLE_DMABUF=0' in outer.log_text(),
                f'version {version}, {checks.tail(outer.log_text(), 3)}')
  vkcube = ['vkcube', '--wsi', 'wayland', '--c', '300']
  summary = checks._record_client(outer, captures, 'vkcube', vkcube, 20)
  print(f'INFO nodmabuf: vkcube: {summary}', flush=True)
  # DB-2's baseline: NVIDIA's WSI finds no present queue, so vkcube picks
  # llvmpipe by itself.
  results.check('nodmabuf: vkcube does not present from NVIDIA, as in the '
                'baseline',
                'llvmpipe' in summary or 'present queue' in summary,
                summary)
  xw.wait_for(lambda: not outer.views(), timeout=10)
  summary = checks._record_client(
      outer, captures, 'es2gears_wayland',
      ['stdbuf', '-oL', 'es2gears_wayland', '-info'], 8,
      program='es2gears_wayland')
  print(f'INFO nodmabuf: es2gears_wayland: {summary}', flush=True)
  results.check('nodmabuf: es2gears_wayland runs on Mesa, not NVIDIA, as '
                'in the baseline',
                'Mesa' in summary and 'NVIDIA' not in summary,
                summary)
  status = outer.stop()
  results.check('nodmabuf: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def mpv_arguments(api):
  """mpv playing six seconds of a test pattern with `api` output."""
  return ['mpv', '--no-config', '--vo=gpu-next', f'--gpu-api={api}',
          '--length=6', '--no-audio', '--keep-open=no',
          'av://lavfi:testsrc=size=640x480:rate=30']

def weston_checks(results, binary, wl_probe, captures):
  """weston's DMA-BUF clients and mpv."""
  del wl_probe
  outer = checks.Shingle(binary, 'weston', checks.BASE_CONFIG,
                         extra_environment=TRACE)
  feedback = checks.Client(outer, ['weston-simple-dmabuf-feedback'],
                           'dmabuf-feedback')
  printed = xw.wait_for(lambda: 'tranche' in feedback.output().lower(),
                        timeout=20)
  feedback.exited(3)
  output = feedback.output()
  print(f'INFO weston: weston-simple-dmabuf-feedback: '
        f'{checks.tail(output, 12)}', flush=True)
  # It looks for the main device's primary node, the card node the
  # namespace hides, and asserts if there is none.
  if 'DRM_NODE_PRIMARY' in output:
    print('INFO weston: weston-simple-dmabuf-feedback needs the main '
          'device\'s card node, which the namespace hides', flush=True)
  else:
    results.check('weston: weston-simple-dmabuf-feedback prints the '
                  'advertised tranche',
                  printed and ('226' in output or 'e280' in output.lower()),
                  checks.tail(output, 4))
  feedback.stop()
  xw.wait_for(lambda: not outer.views(), timeout=15)
  client_checks(results, outer, captures, 'weston-simple-dmabuf-egl',
                ['weston-simple-dmabuf-egl'])
  name = 'mpv --gpu-api=vulkan'
  run_ = Run(outer, 'mpv-vulkan', mpv_arguments('vulkan'), program='mpv')
  try:
    results.check(f'weston: {name} maps', run_.view is not None,
                  checks.tail(run_.client.output()))
    geometry = run_.geometry()
    shot = captures.settled(outer, lambda shot: drawn(shot, geometry) > 0.5)
    results.check(f'weston: {name} plays the test pattern',
                  drawn(shot, geometry) > 0.5,
                  f'{drawn(shot, geometry):.2f} drawn')
    status = run_.client.exited(30)
    summary = run_.summary()
    print(f'INFO weston: {name}: {describe(summary)}, exit {status}',
          flush=True)
    results.check(f'weston: {name} presents DMA-BUFs and exits with 0',
                  summary['imports'] and status == 0,
                  f'{describe(summary)}, status {status}, '
                  f'{checks.tail(run_.client.output())}')
  finally:
    run_.client.stop()
  xw.wait_for(lambda: not outer.views(), timeout=15)
  # mpv's OpenGL output needs EGL or a DRM primary node: only recorded.
  run_ = Run(outer, 'mpv-opengl', mpv_arguments('opengl'), program='mpv')
  status = run_.client.exited(30)
  print(f'INFO weston: mpv --gpu-api=opengl: mapped {run_.view is not None}'
        f', {describe(run_.summary())}, exit {status}, '
        f'{checks.tail(run_.client.output(), 3)}', flush=True)
  run_.client.stop()
  xw.wait_for(lambda: not outer.views(), timeout=15)
  status = outer.stop()
  results.check('weston: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def probe_event(output, name):
  """The first JSON event `name` in a probe's `output`, or None."""
  for line in output.splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and event.get('event') == name:
      return event
  return None

def live_colours(shot, geometry):
  """The share of the live toplevel's view in red and in blue."""
  return (colour_share(shot, geometry, 'red'),
          colour_share(shot, geometry, 'blue'))

def syncobj_checks(results, binary, wl_probe, captures):
  """A toplevel stalled on a never-signalled acquire point stops only
  itself, and destroying it releases its buffers."""
  probe_path = os.path.join(os.path.dirname(wl_probe), 'syncobj_probe')
  outer = checks.Shingle(binary, 'syncobj',
                         checks.with_gaps(checks.OUTER_GAPS),
                         extra_environment=TRACE)
  mark = len(outer.log_text())
  probe = checks.Client(outer, [probe_path, '--seconds', '10'],
                        'syncobj_probe')
  try:
    stalled = xw.wait_for(lambda: probe_event(probe.output(), 'stalled'),
                          timeout=30)
    results.check('syncobj: the probe maps two toplevels and stalls one',
                  stalled is not None, checks.tail(probe.output()))
    stalled_view = outer.view('syncobj-stalled')
    live_view = outer.view('syncobj-live')
    wrong = ['no view']
    if stalled_view is not None and live_view is not None:
      # Once the map animations have ended.
      shot = captures.settled(outer, lambda shot: not checks.pattern_mismatches(
          shot, stalled_view['geometry']))
      wrong = (checks.pattern_mismatches(shot, stalled_view['geometry'])
               if shot else ['no capture'])
    seen = set()
    for _ in range(20):
      shot = captures.take(outer)
      if shot is None or live_view is None:
        break
      red, blue = live_colours(shot, live_view['geometry'])
      if red > 0.8:
        seen.add('red')
      if blue > 0.8:
        seen.add('blue')
      if seen == {'red', 'blue'}:
        break
    results.check('syncobj: the stalled toplevel keeps showing its last '
                  'shm buffer', not wrong, f'wrong {wrong}')
    results.check('syncobj: the other toplevel keeps changing',
                  seen == {'red', 'blue'}, f'seen {sorted(seen)}')
    live = xw.wait_for(lambda: probe_event(probe.output(), 'live'),
                       timeout=30)
    live = live or {}
    results.check('syncobj: the live toplevel got frame callbacks, the '
                  'stalled one none',
                  live.get('frames', 0) >= 20
                  and live.get('stalled_frames') == 0, json.dumps(live))
    results.check('syncobj: nothing was released while stalled',
                  live.get('released') == []
                  and live.get('signalled') == [False, False],
                  json.dumps(live))
    destroyed = xw.wait_for(
        lambda: probe_event(probe.output(), 'destroyed'), timeout=30)
    destroyed = destroyed or {}
    results.check('syncobj: destroying the stalled toplevel releases both '
                  'DMA-BUFs and signals both release points',
                  sorted(destroyed.get('released', []))
                  == ['dmabuf-1', 'dmabuf-2']
                  and destroyed.get('signalled') == [True, True],
                  json.dumps(destroyed))
    status = probe.exited(15)
    results.check('syncobj: the probe exits with 0', status == 0,
                  f'status {status}, {checks.tail(probe.output())}')
  finally:
    probe.stop()
  summary = summarise(trace(outer.log_text()[mark:]))
  print(f'INFO syncobj: {describe(summary)}', flush=True)
  results.check('syncobj: the stalled commits never applied, and their '
                'release points were signalled',
                summary['syncobj_waits'] == 0
                and summary['syncobj_ready'] == 0
                and summary['syncobj_releases'] == 2, describe(summary))
  results.check('syncobj: offscreen Shingle still runs', outer.alive(),
                checks.tail(outer.log_text()))
  status = outer.stop()
  results.check('syncobj: SIGTERM exits offscreen Shingle with status 0',
                status == 0, f'status {status}')

def nosyncobj_checks(results, binary, wl_probe, captures):
  """`dmabuf`, `xwayland` and `nested-gpu` with `SHINGLE_SYNCOBJ=0`."""
  mode = Mode(syncobj=False)
  prefixed = Prefixed(results, 'nosyncobj: ')
  for group in (dmabuf_checks, xwayland_checks, nested_gpu_checks):
    group(prefixed, binary, wl_probe, captures, mode)

CHECKS = {
    'dmabuf': dmabuf_checks,
    'browsers': browser_checks,
    'xwayland': xwayland_checks,
    'nested-gpu': nested_gpu_checks,
    'nodmabuf': nodmabuf_checks,
    'weston': weston_checks,
    'syncobj': syncobj_checks,
    'nosyncobj': nosyncobj_checks,
}
