"""Real-client checks of screen sharing and capture, inside run.py's
namespace (docs/design/screen-sharing.md).

- `screencast`: the ScreenCast portal against offscreen Shingle. A
  private D-Bus session bus, PipeWire, WirePlumber, xdg-desktop-portal
  and xdg-desktop-portal-wlr run inside the namespace, with the system
  bus hidden; xdg-desktop-portal-wlr's chooser is `none` with the output
  named in its config. portal_client.py starts a monitor session through
  the portal's D-Bus API and reads the stream with GStreamer's
  `pipewiresrc`. The stream must show the `wl_probe --pattern` quadrants
  at the view's geometry, send no frames while nothing changes and many
  while a client animates, and show the `wl_probe --cursor` cursor with
  the embedded cursor mode and not with the hidden one. Whether
  xdg-desktop-portal-wlr offers DMA-BUFs, and whether a DMA-BUF stream
  runs, is recorded.
- `image-copy-capture`: ext-image-copy-capture-v1 from a raw-protocol
  client (capture_wire.py): an output session's constraints and frames,
  a later frame waiting for damage, a toplevel session showing only
  that window, `paint_cursors`, a cursor session; and `grim`'s
  wlr-screencopy with and without `-c`.
- `ext-data-control`: ext-data-control-v1 from the raw-protocol client,
  with `wl-copy` and `wl-paste` (which use wlr-data-control) on the other
  side, both ways.

The portal stack needs GStreamer's PipeWire plugin, which this host does
not have installed: fetch_screencast_deps.py extracts Debian's
gstreamer1.0-pipewire package under target/screencast-deps, outside the
namespace, and `pipewiresrc` is loaded from there. Every process here is
a child of this one inside the PID namespace, and signals go through
`run.signal_checked`; the host's session bus, PipeWire and portals are
never reached, since /run/user/<uid> is a private tmpfs and the network
namespace hides abstract sockets.
"""

import json
import os
import subprocess
import time

import capture_wire
import checks
import run

xw = checks.xw
NewId = capture_wire.NewId
GROUPS = ('screencast', 'image-copy-capture', 'ext-data-control')
REPOSITORY = os.path.abspath(os.path.join(os.path.dirname(__file__), '..',
                                          '..'))
GST_PLUGINS = os.path.join(REPOSITORY, 'target', 'screencast-deps', 'root',
                           'usr', 'lib', 'x86_64-linux-gnu',
                           'gstreamer-1.0')
PORTAL = '/usr/libexec/xdg-desktop-portal'
PORTAL_WLR = '/usr/libexec/xdg-desktop-portal-wlr'
PERMISSION_STORE = '/usr/libexec/xdg-permission-store'
PORTAL_CLIENT = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                             'portal_client.py')
# org.freedesktop.portal.ScreenCast cursor modes.
CURSOR_HIDDEN, CURSOR_EMBEDDED, CURSOR_METADATA = 1, 2, 4
# wl_probe --cursor's cursor: yellow, 16x16, hotspot at its centre.
CURSOR_COLOUR = (255, 255, 0)
# The stream's frames while nothing changes, for two seconds: the first
# frame after the pointer stopped may still come.
STATIC_FRAMES = 2
# Frames the stream must carry in two seconds while a client animates at
# the output's 60 Hz, below xdg-desktop-portal-wlr's 30 Hz cap.
ANIMATED_FRAMES = 20

BUS_CONFIG = '''<!DOCTYPE busconfig PUBLIC
 "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={socket}</listen>
  <servicedir>{services}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
'''

# Only the permission store is activatable; every other service is
# started here, so that nothing of the host's is.
PERMISSION_STORE_SERVICE = '''[D-BUS Service]
Name=org.freedesktop.impl.portal.PermissionStore
Exec={binary}
'''

WIREPLUMBER_CONFIG = '''wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
    support.logind = disabled
    support.reserve-device = disabled
  }
}
'''

def portal_wlr_config(output, max_fps=30):
  """xdg-desktop-portal-wlr's config: no chooser, the output by name."""
  return ('[screencast]\n'
          f'output_name={output}\n'
          f'max_fps={max_fps}\n'
          'chooser_type=none\n')

def json_events(text, name=None):
  """The JSON object lines of `text`, those with `event` `name` if given."""
  events = []
  for line in text.splitlines():
    try:
      event = json.loads(line)
    except ValueError:
      continue
    if isinstance(event, dict) and (name is None or event.get('event') == name):
      events.append(event)
  return events

def frames_between(events, start, end):
  """How many `frame` events fall in [start, end) by their time."""
  return sum(1 for event in events
             if event.get('event') == 'frame' and start <= event['t'] < end)

def rgba_capture(data, width, height, stride, pixel_format='RGBA'):
  """An xw.Capture of RGBA or BGRx rows."""
  pixels = bytearray()
  for row in range(height):
    line = data[row * stride:row * stride + width * 4]
    if pixel_format == 'BGRx':
      for column in range(width):
        blue, green, red = line[column * 4:column * 4 + 3]
        pixels += bytes((red, green, blue))
    else:
      for column in range(width):
        pixels += line[column * 4:column * 4 + 3]
  return xw.Capture(width, height, bytes(pixels))

def near(colour, expected, tolerance=40):
  """Whether `colour` is within `tolerance` of `expected` in each channel."""
  return all(abs(a - b) <= tolerance for a, b in zip(colour, expected))

def mount_private_dbus_dir():
  """Covers /run/dbus with a tmpfs, so that no process here can reach the
  host's system bus; returns whether it did."""
  if not os.path.isdir('/run/dbus'):
    return True
  done = subprocess.run(['mount', '-t', 'tmpfs', '-o', 'mode=0755', 'none',
                         '/run/dbus'], capture_output=True)
  return done.returncode == 0

class Service:
  """A background process of the portal stack, with its output in a log."""

  def __init__(self, directory, name, arguments, environment):
    self.name = name
    self.program = os.path.basename(arguments[0])
    self.log_path = os.path.join(directory, f'{name}.log')
    self.log = open(self.log_path, 'w')
    self.process = subprocess.Popen(arguments, env=environment,
                                    stdout=self.log, stderr=self.log,
                                    stdin=subprocess.DEVNULL)

  def text(self):
    """What it printed so far."""
    if not self.log.closed:
      self.log.flush()
    with open(self.log_path, errors='replace') as file:
      return file.read()

  def alive(self):
    """Whether it still runs."""
    return self.process.poll() is None

  def stop(self):
    """Ends it through `signal_checked`; returns its status."""
    try:
      return checks._stop(self.process, self.program, 5)
    finally:
      self.log.close()

class PortalStack:
  """A private session bus with PipeWire, WirePlumber,
  xdg-desktop-portal-wlr and xdg-desktop-portal on it, for `shingle`."""

  def __init__(self, shingle, output):
    self.directory = os.path.join(shingle.directory, 'portal')
    self.runtime = os.path.join(self.directory, 'runtime')
    config = os.path.join(self.directory, 'config')
    services = os.path.join(self.directory, 'services')
    for path in (self.runtime, config, services,
                 os.path.join(config, 'wireplumber', 'wireplumber.conf.d'),
                 os.path.join(config, 'xdg-desktop-portal'),
                 os.path.join(self.directory, 'home')):
      os.makedirs(path, mode=0o700, exist_ok=True)
    with open(os.path.join(services, 'permission-store.service'),
              'w') as file:
      file.write(PERMISSION_STORE_SERVICE.format(binary=PERMISSION_STORE))
    with open(os.path.join(config, 'wireplumber', 'wireplumber.conf.d',
                           '90-harness.conf'), 'w') as file:
      file.write(WIREPLUMBER_CONFIG)
    with open(os.path.join(config, 'xdg-desktop-portal', 'portals.conf'),
              'w') as file:
      file.write('[preferred]\ndefault=wlr\n')
    self.wlr_config = os.path.join(self.directory, 'xdpw.ini')
    with open(self.wlr_config, 'w') as file:
      file.write(portal_wlr_config(output))
    bus_config = os.path.join(self.directory, 'bus.conf')
    socket_path = os.path.join(self.runtime, 'bus')
    with open(bus_config, 'w') as file:
      file.write(BUS_CONFIG.format(socket=socket_path, services=services))
    self.address = f'unix:path={socket_path}'
    environment = {key: value for key, value in os.environ.items()
                   if key not in run.STRIPPED_VARIABLES}
    environment.update({
        'XDG_RUNTIME_DIR': self.runtime,
        'PIPEWIRE_RUNTIME_DIR': self.runtime,
        'XDG_CONFIG_HOME': config,
        'HOME': os.path.join(self.directory, 'home'),
        'DBUS_SESSION_BUS_ADDRESS': self.address,
        'DBUS_SYSTEM_BUS_ADDRESS': 'unix:path=/nonexistent/system_bus',
        'XDG_CURRENT_DESKTOP': 'wlroots',
        'WAYLAND_DISPLAY': shingle.wayland_display(),
        'GST_PLUGIN_PATH': GST_PLUGINS,
        'GST_REGISTRY': os.path.join(self.directory, 'gst-registry.bin'),
        checks.NAMESPACE_VARIABLE: '1',
    })
    self.environment = environment
    self.services = []
    self.services.append(Service(self.directory, 'dbus', [
        'dbus-daemon', '--nofork', '--nosyslog',
        f'--config-file={bus_config}'], environment))
    if not xw.wait_for(lambda: os.path.exists(socket_path), timeout=10):
      raise RuntimeError(f'no private bus: {self.services[0].text()}')
    self.services.append(Service(self.directory, 'pipewire', ['pipewire'],
                                 environment))
    pipewire_socket = os.path.join(self.runtime, 'pipewire-0')
    if not xw.wait_for(lambda: os.path.exists(pipewire_socket), timeout=10):
      raise RuntimeError(f'no PipeWire: {checks.tail(self.services[1].text())}')
    self.services.append(Service(self.directory, 'wireplumber',
                                 ['wireplumber'], environment))
    self.services.append(Service(self.directory, 'xdpw', [
        PORTAL_WLR, '--loglevel', 'DEBUG', '--config', self.wlr_config,
        '--replace'], environment))
    if not xw.wait_for(lambda: self.has_name(
        'org.freedesktop.impl.portal.desktop.wlr'), timeout=15):
      raise RuntimeError(f'no xdg-desktop-portal-wlr: '
                         f'{checks.tail(self.services[-1].text())}')
    self.services.append(Service(self.directory, 'xdp', [
        PORTAL, '--replace', '--verbose'], environment))
    if not xw.wait_for(lambda: self.has_name(
        'org.freedesktop.portal.Desktop'), timeout=15):
      raise RuntimeError(f'no xdg-desktop-portal: '
                         f'{checks.tail(self.services[-1].text())}')

  def has_name(self, name):
    """Whether `name` has an owner on the private bus."""
    done = subprocess.run(
        ['dbus-send', f'--bus={self.address}', '--print-reply',
         '--dest=org.freedesktop.DBus', '/org/freedesktop/DBus',
         'org.freedesktop.DBus.NameHasOwner', f'string:{name}'],
        capture_output=True, text=True, timeout=10)
    return done.returncode == 0 and 'boolean true' in done.stdout

  def log(self, name):
    """The log of service `name`."""
    return next(service.text() for service in self.services
                if service.name == name)

  def client(self, name, arguments):
    """Starts portal_client.py with `arguments` on the private bus."""
    frames = os.path.join(self.directory, name)
    os.makedirs(frames, exist_ok=True)
    return Service(self.directory, name, [
        'python3', PORTAL_CLIENT, '--frames', frames, *arguments],
        self.environment), frames

  def stop(self):
    """Ends every service, the last started first."""
    for service in reversed(self.services):
      service.stop()

class Stream:
  """A running portal_client.py and what it reported."""

  def __init__(self, stack, name, arguments):
    self.service, self.frames = stack.client(name, arguments)
    self.service.program = 'python3'

  def events(self, name=None):
    """Its JSON events so far."""
    return json_events(self.service.text(), name)

  def frame_count(self):
    """How many frames arrived so far."""
    return len(self.events('frame'))

  def latest(self):
    """The latest frame as an xw.Capture, or None."""
    try:
      with open(os.path.join(self.frames, 'latest.json')) as file:
        shape = json.load(file)
      with open(os.path.join(self.frames, 'latest.rgba'), 'rb') as file:
        data = file.read()
    except (FileNotFoundError, ValueError):
      return None
    if len(data) < shape['stride'] * shape['height']:
      return None
    return rgba_capture(data, shape['width'], shape['height'],
                        shape['stride'], shape.get('format', 'RGBA'))

  def frame_after(self, count, timeout=10.0):
    """The latest frame once more than `count` frames arrived."""
    xw.wait_for(lambda: self.frame_count() > count, timeout=timeout)
    time.sleep(0.2)
    return self.latest()

  def stop(self):
    """Ends the client."""
    return self.service.stop()

def check_deps(results):
  """Whether everything the portal stack needs is here."""
  missing = [path for path in (PORTAL, PORTAL_WLR) if not os.path.exists(path)]
  if not os.path.isfile(os.path.join(GST_PLUGINS, 'libgstpipewire.so')):
    missing.append(f'{GST_PLUGINS}/libgstpipewire.so (run '
                   'tools/gpu_test/fetch_screencast_deps.py)')
  return results.check('screencast: the portal stack and pipewiresrc are '
                       'installed', not missing, ', '.join(missing))

def capture_drm_formats(shingle):
  """The `FOURCC:0xMODIFIER` pairs Shingle draws captures into, from an
  ext-image-copy-capture session's `dmabuf_format` events, as
  GStreamer's `drm-format` names them."""
  client = capture_wire.Client(shingle.wayland_display())
  try:
    session = client.session(client.output_source(client.first_output()))
    return [f'{capture_wire.fourcc_name(fourcc)}:{modifier:#018x}'
            for fourcc, modifiers in capture_wire.dmabuf_formats(client,
                                                                 session)
            for modifier in modifiers]
  finally:
    client.close()

def output_name(shingle):
  """The name of Shingle's first output."""
  outputs = shingle.call('window-rules/list-outputs')
  return outputs[0]['name']

def screencast_checks(results, binary, wl_probe, captures):
  """The ScreenCast portal on a private bus against offscreen Shingle."""
  if not check_deps(results):
    return
  results.check('screencast: the host\'s system bus is hidden',
                mount_private_dbus_dir(), '/run/dbus')
  shingle = checks.Shingle(binary, 'screencast',
                           checks.with_gaps(checks.OUTER_GAPS))
  probe = checks.Client(shingle, [wl_probe, 'pattern', '--pattern',
                                  '--cursor'], 'wl_probe')
  stack = None
  streams = []
  try:
    view = xw.wait_for(lambda: shingle.view('pattern'), timeout=15)
    results.check('screencast: the wl_probe client maps', view is not None,
                  checks.tail(probe.output()))
    if view is None:
      return
    geometry = view['geometry']
    name = output_name(shingle)
    stack = PortalStack(shingle, name)
    results.check('screencast: the private bus, PipeWire, WirePlumber and '
                  'both portals run',
                  all(service.alive() for service in stack.services),
                  ', '.join(service.name for service in stack.services
                            if not service.alive()))
    hidden = Stream(stack, 'hidden', ['--cursor-mode', str(CURSOR_HIDDEN),
                                      '--seconds', '40'])
    streams.append(hidden)
    portal = xw.wait_for(lambda: hidden.events('portal'), timeout=20)
    modes = portal[0]['cursor_modes'] if portal else 0
    results.check(
        'screencast: the portal offers monitors and the hidden and embedded '
        'cursor modes',
        portal and portal[0]['source_types'] & 1
        and modes & CURSOR_HIDDEN and modes & CURSOR_EMBEDDED,
        json.dumps(portal[0]) if portal else checks.tail(hidden.service.text()))
    started = xw.wait_for(lambda: hidden.events('started'), timeout=30)
    results.check('screencast: Start gives a stream of the output\'s size',
                  started and started[0]['size'] == list(checks.OUTPUT),
                  json.dumps(started[0]) if started
                  else checks.tail(hidden.service.text()))
    shot = hidden.frame_after(0, timeout=20)
    caps = hidden.events('caps')
    print(f'INFO screencast: caps {caps[0]["caps"] if caps else None}',
          flush=True)
    wrong = checks.pattern_mismatches(shot, geometry) if shot else None
    results.check(
        'screencast: the stream shows the probe\'s quadrants at its view '
        'geometry',
        shot is not None and (shot.width, shot.height) == checks.OUTPUT
        and not wrong,
        f'geometry {json.dumps(geometry)}, size '
        f'{(shot.width, shot.height) if shot else None}, wrong {wrong}, '
        f'{checks.tail(hidden.service.text(), 3)}')
    gap = (geometry['x'] // 2, geometry['y'] // 2)
    results.check('screencast: the gaps show the background',
                  shot is not None and checks.background_at(shot, *gap),
                  f'{shot.pixel(*gap) if shot else None} at {gap}')
    # The pointer onto the probe, whose cursor the hidden mode leaves out.
    centre = (geometry['x'] + geometry['width'] // 4,
              geometry['y'] + geometry['height'] // 4)
    count = hidden.frame_count()
    shingle.call('stipc/move_cursor', {'x': centre[0], 'y': centre[1]})
    xw.wait_for(lambda: 'cursor_set' in probe.output(), timeout=10)
    shot = hidden.frame_after(count, timeout=5)
    results.check(
        'screencast: the hidden cursor mode leaves the cursor out',
        'cursor_set' in probe.output() and shot is not None
        and checks.classify(*shot.pixel(*centre)) == 'red',
        f'{shot.pixel(*centre) if shot else None} at {centre}, '
        f'{checks.tail(probe.output(), 2)}')
    time.sleep(1.0)
    before = time.monotonic()
    time.sleep(2.0)
    after = time.monotonic()
    static = frames_between(hidden.events(), before, after)
    results.check(
        'screencast: no frames while nothing changes (copy_with_damage)',
        static <= STATIC_FRAMES,
        f'{static} frames in {after - before:.1f} s')
    stack_log = stack.log('xdpw')
    print('INFO screencast: xdg-desktop-portal-wlr '
          + ('offers DMA-BUFs' if 'dmabuf' in stack_log.lower()
             and 'linux_dmabuf' in stack_log else 'did not log DMA-BUF'),
          flush=True)
    embedded = Stream(stack, 'embedded', ['--cursor-mode',
                                          str(CURSOR_EMBEDDED),
                                          '--seconds', '20'])
    streams.append(embedded)
    shot = embedded.frame_after(0, timeout=20)
    results.check(
        'screencast: the embedded cursor mode shows the cursor',
        shot is not None and near(shot.pixel(*centre), CURSOR_COLOUR),
        f'{shot.pixel(*centre) if shot else None} at {centre}, '
        f'{checks.tail(embedded.service.text(), 3)}')
    embedded.stop()
    streams.remove(embedded)
    # A second client that animates: the stream follows its frames.
    count = hidden.frame_count()
    animate = checks.Client(shingle, [wl_probe, 'animate', '--animate',
                                      '600'], 'wl_probe')
    try:
      xw.wait_for(lambda: shingle.view('animate'), timeout=15)
      time.sleep(1.0)
      before = time.monotonic()
      time.sleep(2.0)
      after = time.monotonic()
      moving = frames_between(hidden.events(), before, after)
      results.check('screencast: the stream follows a client that animates',
                    moving >= ANIMATED_FRAMES,
                    f'{moving} frames in {after - before:.1f} s')
    finally:
      animate.stop()
    pairs = capture_drm_formats(shingle)
    print(f'INFO screencast: Shingle draws captures into {", ".join(pairs)}',
          flush=True)
    # One pair, as pipewiresrc 1.4 fails when xdg-desktop-portal-wlr's
    # allocation fixes a modifier other than the first it would pick.
    dmabuf = Stream(stack, 'dmabuf', ['--memory', 'dmabuf', '--seconds', '8',
                                      '--drm-formats', ','.join(pairs[:1])])
    streams.append(dmabuf)
    shot = dmabuf.frame_after(0, timeout=20)
    xw.wait_for(lambda: dmabuf.events('stopped') or dmabuf.events('error'),
                timeout=30)
    frames = dmabuf.events('frame')
    stopped = dmabuf.events('stopped')
    errors = dmabuf.events('error')
    negotiated = [line for line in stack.log('xdpw').splitlines()
                  if 'buffer_type:' in line]
    print(f'INFO screencast: a DMA-BUF stream got {len(frames)} frames, '
          f'{"DMA-BUFs" if frames and frames[0]["dmabuf"] else "not DMA-BUFs"}'
          f', {stopped[0]["unmapped"] if stopped else "?"} not mappable'
          + (f', {errors[0]["text"]}' if errors else '')
          + f'; xdg-desktop-portal-wlr negotiated '
            f'{" | ".join(line.split(" - ", 1)[-1] for line in negotiated)}',
          flush=True)
    results.check(
        'screencast: a DMA-BUF stream negotiates the modifier Shingle '
        'draws into and carries DMA-BUF frames',
        len(frames) >= 1 and all(frame['dmabuf'] for frame in frames)
        and any('buffer_type: 1' in line for line in negotiated),
        f'{len(frames)} frames, {errors[0]["text"] if errors else "no error"}')
    if frames and frames[0]['dmabuf'] and shot is not None:
      wrong = checks.pattern_mismatches(shot, geometry)
      results.check(
          'screencast: a DMA-BUF stream shows the probe\'s quadrants',
          not wrong, f'wrong {wrong}')
    for stream in streams:
      stream.stop()
    streams = []
    errors = [line for line in stack.log('xdpw').splitlines()
              if '[ERROR]' in line]
    results.check('screencast: xdg-desktop-portal-wlr logged no errors',
                  not errors, ' | '.join(errors[-3:]))
    results.check('screencast: Shingle still runs', shingle.alive(),
                  checks.tail(shingle.log_text()))
  finally:
    for stream in streams:
      stream.stop()
    if stack is not None:
      stack.stop()
    probe.stop()
    status = shingle.stop()
    results.check('screencast: SIGTERM exits offscreen Shingle with status 0',
                  status == 0, f'status {status}')

def frame_ready(names):
  """Whether a frame's events are the metadata and `ready`."""
  return names == ['transform', 'damage', 'presentation_time', 'ready']

def argb_capture(data, width, height, stride=None):
  """An xw.Capture of ARGB8888 (BGRA bytes) rows."""
  return rgba_capture(data, width, height, stride or width * 4, 'BGRx')

def corner_colours(shot):
  """The classified colours near the four corners of `shot`."""
  points = ((3, 3), (shot.width - 4, 3), (3, shot.height - 4),
            (shot.width - 4, shot.height - 4))
  return [checks.classify(*shot.pixel(*point)) for point in points]

def image_copy_checks(results, binary, wl_probe, captures):
  """ext-image-copy-capture and wlr-screencopy from raw clients and grim."""
  shingle = checks.Shingle(binary, 'image-copy',
                           checks.with_gaps(checks.OUTER_GAPS))
  probe = checks.Client(shingle, [wl_probe, 'pattern', '--pattern',
                                  '--cursor'], 'wl_probe')
  client = None
  try:
    view = xw.wait_for(lambda: shingle.view('pattern'), timeout=15)
    results.check('image-copy-capture: the wl_probe client maps',
                  view is not None, checks.tail(probe.output()))
    if view is None:
      return
    geometry = view['geometry']
    # The open animation ends first, so that frames show the probe and
    # nothing changes until the pointer moves.
    captures.settled(shingle,
                     lambda shot: not checks.pattern_mismatches(shot, geometry),
                     stable=True)
    client = capture_wire.Client(shingle.wayland_display())
    session = client.session(client.output_source(client.first_output()))
    names = client.names_of(session)
    formats = capture_wire.dmabuf_formats(client, session)
    results.check(
        'image-copy-capture: an output session announces shm and DMA-BUF '
        'formats, the device and the output\'s size',
        names.count('shm_format') == 2 and 'dmabuf_device' in names
        and formats and client.buffer_size(session) == checks.OUTPUT
        and names[-1] == 'done',
        f'{names}, '
        f'{[(capture_wire.fourcc_name(f), len(m)) for f, m in formats]}')
    frame, names, data = client.capture(session, checks.OUTPUT)
    shot = argb_capture(data, *checks.OUTPUT) if data else None
    wrong = checks.pattern_mismatches(shot, geometry) if shot else None
    results.check(
        'image-copy-capture: the first frame is ready at once and shows the '
        'probe\'s quadrants',
        frame_ready(names) and shot is not None and not wrong,
        f'{names}, wrong {wrong}')
    centre = (geometry['x'] + geometry['width'] // 4,
              geometry['y'] + geometry['height'] // 4)
    started = client.start_capture(session, checks.OUTPUT)
    names, _ = client.finish_capture(started, wait=1.5)
    waited = not names
    shingle.call('stipc/move_cursor', {'x': centre[0], 'y': centre[1]})
    xw.wait_for(lambda: 'cursor_set' in probe.output(), timeout=10)
    later, _ = client.finish_capture(started, wait=10)
    results.check(
        'image-copy-capture: the next frame waits until the output changes',
        waited and frame_ready(later),
        f'before the pointer moved {names}, after {later}')
    # The cursor, without and with paint_cursors.
    plain_session = client.session(client.output_source(client.first_output()))
    _, names, data = client.capture(plain_session, checks.OUTPUT)
    plain = argb_capture(data, *checks.OUTPUT) if data else None
    painted_session = client.session(
        client.output_source(client.first_output()),
        capture_wire.PAINT_CURSORS)
    _, names, data = client.capture(painted_session, checks.OUTPUT)
    painted = argb_capture(data, *checks.OUTPUT) if data else None
    results.check(
        'image-copy-capture: the cursor is painted only with paint_cursors',
        painted is not None and near(painted.pixel(*centre), CURSOR_COLOUR)
        and plain is not None
        and checks.classify(*plain.pixel(*centre)) == 'red',
        f'painted {painted.pixel(*centre) if painted else None}, '
        f'plain {plain.pixel(*centre) if plain else None} at {centre}')
    # A cursor session.
    seat = client.bind('wl_seat', 5)
    pointer = client.request(seat, 'get_pointer', NewId('wl_pointer'))
    manager = client.bind('ext_image_copy_capture_manager_v1', 1)
    cursor = client.request(
        manager, 'create_pointer_cursor_session',
        NewId('ext_image_copy_capture_cursor_session_v1'),
        client.output_source(client.first_output()), pointer)
    client.roundtrip()
    cursor_events = client.events_of(cursor)
    cursor_session = client.request(
        cursor, 'get_capture_session',
        NewId('ext_image_copy_capture_session_v1'))
    client.roundtrip()
    size = client.buffer_size(cursor_session)
    _, names, data = client.capture(cursor_session, size or (16, 16))
    image = argb_capture(data, *size) if data and size else None
    results.check(
        'image-copy-capture: a cursor session reports the hotspot and copies '
        'the cursor image',
        [name for name, _ in cursor_events][:1] == ['enter']
        and ('position', list(centre)) in cursor_events
        and ('hotspot', [8, 8]) in cursor_events and size == (16, 16)
        and frame_ready(names) and image is not None
        and near(image.pixel(8, 8), CURSOR_COLOUR),
        f'{cursor_events}, size {size}, {names}, '
        f'{image.pixel(8, 8) if image else None}')
    # One toplevel alone.
    toplevels = client.toplevels()
    source = client.toplevel_source(toplevels['pattern'])
    window = client.session(source)
    size = client.buffer_size(window)
    expected = (geometry['width'], geometry['height'])
    _, names, data = client.capture(window, size or expected)
    shot = argb_capture(data, *size) if data and size else None
    corners = corner_colours(shot) if shot else None
    results.check(
        'image-copy-capture: a toplevel session has the window\'s size and '
        'shows the window alone, without the cursor or the background',
        size == expected and frame_ready(names)
        and corners == ['red', 'green', 'blue', 'white']
        and checks.classify(*shot.pixel(
            centre[0] - geometry['x'], centre[1] - geometry['y'])) == 'red',
        f'size {size} for {expected}, {names}, corners {corners}')
    # wlr-screencopy through grim, with and without the cursor.
    environment = shingle.client_environment()
    shots = {}
    for flag in ('', '-c'):
      done = subprocess.run(['grim', *([flag] if flag else []), '-t', 'ppm',
                             '-'], env=environment, capture_output=True,
                            timeout=20, stdin=subprocess.DEVNULL)
      parsed = xw.parse_ppm(done.stdout) if done.returncode == 0 else None
      shots[flag] = xw.Capture(*parsed) if parsed else None
    results.check(
        'image-copy-capture: grim leaves the cursor out and grim -c draws it',
        all(shots.values())
        and checks.classify(*shots[''].pixel(*centre)) == 'red'
        and near(shots['-c'].pixel(*centre), CURSOR_COLOUR),
        str({flag: shot.pixel(*centre) if shot else None
             for flag, shot in shots.items()}))
    # A DMA-BUF made with GBM, as a GPU client makes one.
    dmabuf_check(results, shingle, captures, client, formats, geometry)
    # The window closes: its session stops.
    probe.stop()
    client.roundtrip()
    stopped = client.wait_for(window, 'stopped', timeout=10)
    _, names, _ = client.capture(window, size or expected, wait=5)
    results.check(
        'image-copy-capture: a toplevel session stops when its window '
        'closes, and its frames fail',
        stopped and names == ['failed'],
        f'{client.names_of(window)}, {names}')
    results.check('image-copy-capture: Shingle still runs and the client '
                  'got no protocol error',
                  shingle.alive() and client.error is None,
                  f'{client.error}, {checks.tail(shingle.log_text())}')
  finally:
    if client is not None:
      client.close()
    probe.stop()
    status = shingle.stop()
    results.check('image-copy-capture: SIGTERM exits offscreen Shingle with '
                  'status 0', status == 0, f'status {status}')

def show_dmabuf(results, shingle, captures, client, buffer, geometry):
  """Shows the captured DMA-BUF `buffer` in a toplevel of `client`, where
  Shingle draws it stretched to the window, and requires grim to show the
  probe's quadrants there, where the capture had them: so the GPU wrote
  them into the DMA-BUF, even where no CPU can map it."""
  surface = client.toplevel('dmabuf-view', 64, 64)
  view = xw.wait_for(lambda: shingle.view('dmabuf-view'), timeout=15)
  if view is None:
    results.check('image-copy-capture: the DMA-BUF viewer maps', False)
    return
  client.request(surface, 'attach', buffer, ('i', 0), ('i', 0))
  client.request(surface, 'damage', ('i', 0), ('i', 0), ('i', 1 << 30),
                 ('i', 1 << 30))
  client.request(surface, 'commit')
  client.roundtrip()
  width, height = checks.OUTPUT

  def wrong(shot):
    shown = xw.wait_for(lambda: shingle.view('dmabuf-view'), timeout=5)
    tile = shown['geometry'] if shown else view['geometry']
    mismatches = []
    for (x, y), (column, row) in checks.quadrant_points(geometry):
      point = (tile['x'] + x * tile['width'] // width,
               tile['y'] + y * tile['height'] // height)
      actual = checks.classify(*shot.pixel(*point))
      if actual != checks.PATTERN[row][column]:
        mismatches.append((point, checks.PATTERN[row][column], actual))
    return mismatches
  shot = captures.settled(shingle, lambda shot: not wrong(shot), stable=True)
  mismatches = wrong(shot) if shot else None
  results.check(
      'image-copy-capture: the DMA-BUF, shown in a window, holds the '
      'probe\'s quadrants where the capture had them',
      shot is not None and not mismatches, f'wrong {mismatches}')

def dmabuf_check(results, shingle, captures, client, formats, geometry):
  """Captures a new output session into a GBM buffer of the first format
  and modifiers Shingle announced, and maps it to look."""
  xr24 = [(fourcc, modifiers) for fourcc, modifiers in formats
          if capture_wire.fourcc_name(fourcc) == 'XR24']
  if not xr24:
    results.check('image-copy-capture: XRGB8888 DMA-BUFs are announced',
                  False, f'{formats}')
    return
  fourcc, modifiers = xr24[0]
  try:
    gbm = capture_wire.Gbm()
  except (OSError, RuntimeError) as error:
    print(f'INFO image-copy-capture: no GBM: {error}', flush=True)
    return
  width, height = checks.OUTPUT
  bo = gbm.create(width, height, fourcc, modifiers)
  try:
    if bo is None:
      print('INFO image-copy-capture: GBM allocated no buffer of the '
            'announced modifiers', flush=True)
      return
    plane = gbm.plane(bo)
    buffer = capture_wire.dmabuf_buffer(client, plane, width, height, fourcc)
    os.close(plane[0])
    # A fresh session's first frame is ready at once.
    fresh = client.session(client.output_source(client.first_output()))
    frame = client.request(fresh, 'create_frame',
                           NewId('ext_image_copy_capture_frame_v1'))
    client.request(frame, 'attach_buffer', buffer)
    client.request(frame, 'damage_buffer', ('i', 0), ('i', 0),
                   ('i', width), ('i', height))
    client.request(frame, 'capture')
    ready = (client.wait_for(frame, 'ready', timeout=10)
             or 'failed' in client.names_of(frame))
    names = client.names_of(frame)
    results.check(
        'image-copy-capture: a frame into a GBM DMA-BUF with modifier '
        f'{plane[3]:#018x} is ready',
        ready and frame_ready(names), f'{names}')
    mapped = gbm.read(bo, width, height)
    if mapped is not None:
      data, stride = mapped
      shot = argb_capture(data, width, height, stride)
      wrong = checks.pattern_mismatches(shot, geometry)
      results.check(
          'image-copy-capture: the mapped DMA-BUF shows the probe\'s '
          'quadrants', not wrong, f'wrong {wrong}')
    else:
      print('INFO image-copy-capture: GBM cannot map the DMA-BUF; it is '
            'shown in a window instead', flush=True)
    show_dmabuf(results, shingle, captures, client, buffer, geometry)
  finally:
    if bo is not None:
      gbm.destroy(bo)
    gbm.close()

def receive(client, offer, mime_type, timeout=10.0):
  """What `offer` holds as `mime_type`, read through a pipe."""
  read, write = os.pipe()
  client.request(offer, 'receive', mime_type, fds=[write])
  os.close(write)
  client.roundtrip()
  chunks = []
  deadline = time.monotonic() + timeout
  os.set_blocking(read, False)
  while time.monotonic() < deadline:
    try:
      chunk = os.read(read, 65536)
    except BlockingIOError:
      client.pump(0.05)
      continue
    if not chunk:
      break
    chunks.append(chunk)
  os.close(read)
  return b''.join(chunks)

def last_offer(client, device, event):
  """The offer of `device`'s last `event` (`selection` or
  `primary_selection`), with its MIME types; (0, []) for none."""
  offers = [arguments[0] for name, arguments in client.events_of(device)
            if name == event]
  offer = offers[-1] if offers else 0
  mime_types = [arguments[0] for name, arguments in client.events_of(offer)
                if name == 'offer'] if offer else []
  return offer, mime_types

def serve_send(client, source, text, timeout=10.0):
  """Answers `source`'s next `send` with `text`; returns its MIME type."""
  seen = len(client.names_of(source))
  if not client.wait_for(source, 'send', timeout=timeout, after=seen):
    return None
  mime_type = [arguments for name, arguments in client.events_of(source)
               if name == 'send'][-1][0]
  fd = client.fds.pop(0)
  os.write(fd, text)
  os.close(fd)
  return mime_type

def ext_data_control_checks(results, binary, wl_probe, captures):
  """ext-data-control from a raw client, against wl-copy and wl-paste."""
  shingle = checks.Shingle(binary, 'ext-data-control', checks.BASE_CONFIG)
  client = None
  copy = None
  try:
    client = capture_wire.Client(shingle.wayland_display())
    results.check('ext-data-control: ext_data_control_manager_v1 is offered',
                  client.has_global('ext_data_control_manager_v1') == 1,
                  f'{client.has_global("ext_data_control_manager_v1")}')
    manager = client.bind('ext_data_control_manager_v1', 1)
    seat = client.bind('wl_seat', 5)
    device = client.request(manager, 'get_data_device',
                            NewId('ext_data_control_device_v1'), seat)
    client.roundtrip()
    names = client.names_of(device)
    results.check(
        'ext-data-control: a device is told at once that both selections '
        'are empty',
        names[:2] == ['selection', 'primary_selection']
        and all(arguments == [0] for _, arguments in client.events_of(device)),
        f'{client.events_of(device)}')
    text = b'copied with wl-copy'
    copy = checks.Client(shingle, ['wl-copy', '--foreground', text.decode()],
                         'wl-copy')
    offered = xw.wait_for(lambda: (client.pump(0.05)
                                   or last_offer(client, device,
                                                 'selection')[0]), timeout=10)
    offer, mime_types = last_offer(client, device, 'selection')
    received = (receive(client, offer, 'text/plain;charset=utf-8')
                if offered else None)
    results.check(
        'ext-data-control: wl-copy\'s selection is offered and reads back',
        offered and 'text/plain;charset=utf-8' in mime_types
        and received == text,
        f'{mime_types}, {received!r}')
    # The raw client's own source, read by wl-paste.
    source = client.request(manager, 'create_data_source',
                            NewId('ext_data_control_source_v1'))
    client.request(source, 'offer', 'text/plain;charset=utf-8')
    client.request(source, 'offer', 'text/plain')
    client.request(device, 'set_selection', source)
    client.roundtrip()
    pasted = []
    environment = shingle.client_environment()
    paste = subprocess.Popen(['wl-paste', '--no-newline'], env=environment,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             stdin=subprocess.DEVNULL)
    mime_type = serve_send(client, source, b'set with ext-data-control')
    try:
      out, _ = paste.communicate(timeout=10)
      pasted.append(out)
    except subprocess.TimeoutExpired:
      checks._stop(paste, 'wl-paste', 2)
    results.check(
        'ext-data-control: wl-paste reads an ext-data-control selection',
        mime_type is not None and pasted == [b'set with ext-data-control'],
        f'{mime_type}, {pasted}')
    results.check(
        'ext-data-control: setting it ends wl-copy\'s selection',
        copy.exited(10) is not None, checks.tail(copy.output()))
    primary = client.request(manager, 'create_data_source',
                             NewId('ext_data_control_source_v1'))
    client.request(primary, 'offer', 'text/plain;charset=utf-8')
    client.request(device, 'set_primary_selection', primary)
    client.roundtrip()
    paste = subprocess.Popen(['wl-paste', '--primary', '--no-newline'],
                             env=environment, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, stdin=subprocess.DEVNULL)
    mime_type = serve_send(client, primary, b'primary')
    try:
      out, _ = paste.communicate(timeout=10)
    except subprocess.TimeoutExpired:
      checks._stop(paste, 'wl-paste', 2)
      out = None
    results.check(
        'ext-data-control: wl-paste --primary reads its primary selection',
        out == b'primary', f'{mime_type}, {out!r}')
    results.check('ext-data-control: no protocol error',
                  client.error is None and shingle.alive(), f'{client.error}')
  finally:
    if copy is not None:
      copy.stop()
    if client is not None:
      client.close()
    status = shingle.stop()
    results.check('ext-data-control: SIGTERM exits offscreen Shingle with '
                  'status 0', status == 0, f'status {status}')

CHECKS = {
    'screencast': screencast_checks,
    'image-copy-capture': image_copy_checks,
    'ext-data-control': ext_data_control_checks,
}
