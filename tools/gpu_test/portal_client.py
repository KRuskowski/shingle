#!/usr/bin/env python3
"""A ScreenCast portal client that records what the stream shows.

It runs only inside run.py's namespace, started by screencast_checks.py,
on the private session bus in DBUS_SESSION_BUS_ADDRESS, where a private
xdg-desktop-portal and xdg-desktop-portal-wlr serve it; never on the
host's bus.

Through org.freedesktop.portal.ScreenCast it creates a session, selects
one monitor with the cursor mode given, starts it and opens the PipeWire
remote. GStreamer's `pipewiresrc` (from the plugin directory in
GST_PLUGIN_PATH) then reads the stream from that remote, converted to
RGBA in system memory, or, with `--memory dmabuf`, asks for DMA-BUFs:
of the `--drm-formats` pairs, as a GPU consumer does, or else BGRx with
the linear or the implicit modifier, and maps them as they come. It
prints JSON lines on stdout:

- `portal`: the portal's version, source types and cursor modes;
- `started`: the stream's PipeWire node and size, as Start gave them;
- `caps`: the caps negotiated with the stream;
- `frame`: each frame's number, CLOCK_MONOTONIC time in seconds, size,
  pixel format and whether its buffer was a DMA-BUF;
- `error`: what failed, after which it exits with status 1.

The latest frame's rows are written to `latest.rgba` in `--frames DIR`
(atomically, through a rename), with `latest.json` holding its number,
width, height, stride and format, so that the harness can look at what
the stream shows now; `stopped` counts the frames that could not be
mapped. It ends after `--seconds`.
"""

import argparse
import json
import os
import sys
import time

def report(event, **fields):
  """Prints one JSON line."""
  print(json.dumps({'event': event, **fields}), flush=True)

def frame_record(number, width, height, dmabuf):
  """The `frame` event's fields."""
  return {'number': number, 't': time.monotonic(), 'width': width,
          'height': height, 'dmabuf': dmabuf}

def caps_size(structure):
  """(width, height) of a caps structure, or None."""
  found_width, width = structure.get_int('width')
  found_height, height = structure.get_int('height')
  return (width, height) if found_width and found_height else None

def write_latest(directory, number, width, height, stride, data,
                 pixel_format='RGBA'):
  """Writes a frame's rows as latest.rgba and its shape and format
  (`RGBA` or `BGRx`) as latest.json, each through a rename."""
  temporary = os.path.join(directory, 'latest.rgba.tmp')
  with open(temporary, 'wb') as file:
    file.write(data)
  os.replace(temporary, os.path.join(directory, 'latest.rgba'))
  shape = os.path.join(directory, 'latest.json.tmp')
  with open(shape, 'w') as file:
    json.dump({'number': number, 'width': width, 'height': height,
               'stride': stride, 'format': pixel_format}, file)
  os.replace(shape, os.path.join(directory, 'latest.json'))

class Portal:
  """The ScreenCast portal on the session bus, with GLib's main loop."""

  def __init__(self):
    import dbus
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib
    self.dbus = dbus
    self.glib = GLib
    DBusGMainLoop(set_as_default=True)
    self.bus = dbus.SessionBus()
    self.sender = self.bus.get_unique_name()[1:].replace('.', '_')
    portal = self.bus.get_object('org.freedesktop.portal.Desktop',
                                 '/org/freedesktop/portal/desktop')
    self.screencast = dbus.Interface(portal,
                                     'org.freedesktop.portal.ScreenCast')
    self.properties = dbus.Interface(portal,
                                     'org.freedesktop.DBus.Properties')
    self.token = 0

  def property(self, name):
    """A property of the ScreenCast interface."""
    return self.properties.Get('org.freedesktop.portal.ScreenCast', name)

  def _token(self):
    self.token += 1
    return f'shingle{self.token}'

  def call(self, method, *arguments, options=None, timeout=30):
    """Calls a portal method that answers through a Request; returns the
    response's results, or raises with its code."""
    token = self._token()
    options = dict(options or {})
    options['handle_token'] = token
    path = (f'/org/freedesktop/portal/desktop/request/{self.sender}/'
            f'{token}')
    loop = self.glib.MainLoop()
    answer = {}

    def respond(code, results):
      answer['code'] = int(code)
      answer['results'] = results
      loop.quit()
    match = self.bus.add_signal_receiver(
        respond, 'Response', 'org.freedesktop.portal.Request',
        'org.freedesktop.portal.Desktop', path)
    getattr(self.screencast, method)(*arguments, options)
    self.glib.timeout_add_seconds(timeout, loop.quit)
    loop.run()
    match.remove()
    if answer.get('code') != 0:
      raise RuntimeError(f'{method} answered {answer.get("code", "nothing")}')
    return answer['results']

def start_stream(portal, cursor_mode):
  """Creates, selects and starts a monitor session; returns the session
  handle and the first stream as (node, properties)."""
  results = portal.call('CreateSession', options={
      'session_handle_token': portal.dbus.String(portal._token())})
  session = results['session_handle']
  portal.call('SelectSources', session, options={
      'types': portal.dbus.UInt32(1),
      'multiple': False,
      'cursor_mode': portal.dbus.UInt32(cursor_mode),
  })
  results = portal.call('Start', session, '')
  streams = results.get('streams') or []
  if not streams:
    raise RuntimeError('Start gave no stream')
  node, properties = streams[0]
  return session, int(node), dict(properties)

def run(options):
  """Starts the stream and records it for `options.seconds`."""
  import gi
  gi.require_version('Gst', '1.0')
  from gi.repository import Gst
  Gst.init(None)
  if Gst.ElementFactory.find('pipewiresrc') is None:
    report('error', text='no pipewiresrc in GST_PLUGIN_PATH')
    return 1
  portal = Portal()
  report('portal', version=int(portal.property('version')),
         source_types=int(portal.property('AvailableSourceTypes')),
         cursor_modes=int(portal.property('AvailableCursorModes')))
  session, node, properties = start_stream(portal, options.cursor_mode)
  size = properties.get('size')
  report('started', node=node,
         size=[int(value) for value in size] if size else None)
  remote = portal.screencast.OpenPipeWireRemote(session, {})
  fd = remote.take()
  if options.memory == 'dmabuf' and options.drm_formats:
    # DMA-BUFs of these fourcc:modifier pairs, as a GPU consumer asks.
    caps = ('video/x-raw(memory:DMABuf),format=DMA_DRM,'
            f'drm-format=(string){{{options.drm_formats}}}')
  elif options.memory == 'dmabuf':
    # BGRx DMA-BUFs, which pipewiresrc offers with the linear and the
    # implicit modifier; the frame is mapped as it is.
    caps = 'video/x-raw(memory:DMABuf),format=BGRx'
  else:
    caps = 'video/x-raw,format=RGBA'
  convert = '' if options.memory == 'dmabuf' else 'videoconvert ! '
  pipeline = Gst.parse_launch(
      f'pipewiresrc fd={fd} path={node} always-copy=false ! {convert}{caps} '
      '! appsink name=sink emit-signals=true sync=false max-buffers=2 '
      'drop=true')
  element = pipeline.get_by_name('sink')
  frames = {'count': 0, 'unmapped': 0}

  def record(buffer, caps):
    frames['count'] += 1
    structure = caps.get_structure(0) if caps else None
    width, height = caps_size(structure) if structure else (0, 0)
    dmabuf = bool(caps and caps.get_features(0).contains('memory:DMABuf'))
    pixel_format = structure.get_string('format') if structure else None
    report('frame', **frame_record(frames['count'], width, height, dmabuf),
           format=pixel_format)
    if not options.frames:
      return
    found, mapped = buffer.map(Gst.MapFlags.READ)
    if not found:
      frames['unmapped'] += 1
      return
    try:
      data = bytes(mapped.data)
      stride = len(data) // height if height else width * 4
      write_latest(options.frames, frames['count'], width, height,
                   stride, data, pixel_format)
    finally:
      buffer.unmap(mapped)

  def on_sample(sink):
    sample = sink.emit('pull-sample')
    if sample is not None:
      record(sample.get_buffer(), sample.get_caps())
    return Gst.FlowReturn.OK
  element.connect('new-sample', on_sample)
  pipeline.set_state(Gst.State.PLAYING)
  bus = pipeline.get_bus()
  deadline = time.monotonic() + options.seconds
  reported_caps = False
  status = 0
  while time.monotonic() < deadline:
    message = bus.timed_pop_filtered(
        100 * Gst.MSECOND, Gst.MessageType.ERROR | Gst.MessageType.EOS)
    portal.glib.MainContext.default().iteration(False)
    if not reported_caps:
      pad = element.get_static_pad('sink')
      caps = pad.get_current_caps() if pad else None
      if caps is not None:
        report('caps', caps=caps.to_string())
        reported_caps = True
    if message is None:
      continue
    if message.type == Gst.MessageType.ERROR:
      error, debug = message.parse_error()
      report('error', text=f'{error.message}: {debug}')
      status = 1
    break
  pipeline.set_state(Gst.State.NULL)
  report('stopped', frames=frames['count'], unmapped=frames['unmapped'])
  return status

def main():
  """Parses the options and runs."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('--cursor-mode', type=int, default=1,
                      help='1 hidden, 2 embedded, 4 metadata')
  parser.add_argument('--seconds', type=float, default=10.0)
  parser.add_argument('--memory', choices=('shm', 'dmabuf'), default='shm')
  parser.add_argument('--frames', metavar='DIR')
  parser.add_argument('--drm-formats', metavar='FOURCC:MODIFIER,...',
                      help='with --memory dmabuf, the pairs to ask for')
  options = parser.parse_args()
  try:
    return run(options)
  except Exception as error:  # Reported to the harness, which judges it.
    report('error', text=f'{type(error).__name__}: {error}')
    return 1

if __name__ == '__main__':
  sys.exit(main())
