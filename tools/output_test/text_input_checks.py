"""WG-8's real-client checks of text-input-v1, run inside run.py's
namespace (docs/design/wayfire-gaps.md, WG-8).

Raw-protocol clients (wayland_wire.py) of headless Shingle: an
application maps an `xdg_toplevel`, which takes the keyboard focus, and
creates a `zwp_text_input_v1`; a test input method binds
`zwp_input_method_v1`, and later a second one
`zwp_input_method_manager_v2`. The checks:

- the global is announced at version 1;
- `activate` on the focused surface sends `enter`, and the input method
  gets `activate` with a context;
- the application's surrounding text, content type and `commit_state`
  reach the context, with text-input v1's content purpose;
- the input method's preedit and commit string arrive at the
  application with the serial it named, as Wayfire passes them on;
- `deactivate` sends `leave` and `deactivate` to the input method;
- an input-method-v2 client's commit arrives as v1's
  `delete_surrounding_text`, `commit_string` and `preedit_string`.

Signals go only to Shingle, through run.py's `signal_checked`.
"""

import json

import wayland_wire

# text-input v1's `content_purpose.date` and `content_hint.latin`.
PURPOSE_DATE = 9
HINT_LATIN = 0x100

def text_input_config(run):
  """One output, Xwayland off, floating windows, no animations."""
  config = json.loads(run.yaml_config({}))
  config['tiling'] = {'tile_by_default': 'none'}
  config['animate'] = {'open_animation': 'none', 'close_animation': 'none'}
  return json.dumps(config)

def arguments_of(client, object_id, name):
  """The arguments of every `name` event of `object_id`."""
  return [arguments for found, arguments in client.events_of(object_id)
          if found == name]

class App:
  """The application: a focused toplevel with a text input."""

  def __init__(self, display):
    self.client = wayland_wire.Client(display)
    self.seat = self.client.bind('wl_seat', 7)
    self.surface = self.client.toplevel('wg8-text', 200, 100)
    manager = self.client.bind('zwp_text_input_manager_v1', 1)
    self.input = self.client.request(
        manager, 'create_text_input',
        wayland_wire.NewId('zwp_text_input_v1'))
    self.client.roundtrip()

  def request(self, name, *arguments):
    """Sends text input request `name` and waits for Shingle."""
    self.client.request(self.input, name, *arguments)
    self.client.roundtrip()

class MethodV1:
  """A test input method on input-method v1."""

  def __init__(self, display):
    self.client = wayland_wire.Client(display)
    self.method = self.client.bind('zwp_input_method_v1', 1)
    self.client.roundtrip()

  def context(self, timeout=5.0):
    """The context of the last `activate`, waiting for one."""
    self.client.wait_for(self.method, 'activate', timeout=timeout)
    activations = arguments_of(self.client, self.method, 'activate')
    return activations[-1][0] if activations else None

def v1_checks(results, app, method):
  """An input-method-v1 client serving the text input."""
  enters = arguments_of(app.client, app.input, 'enter')
  results.check('WG-8: activate on the focused surface sends enter',
                enters == [[app.surface]], str(app.client.events_of(
                    app.input)))
  context = method.context()
  results.check('WG-8: the input method gets activate with a context',
                context is not None,
                str(method.client.events_of(method.method)))
  if context is None:
    return
  method.client.roundtrip()
  before = len(method.client.events_of(context))
  app.client.request(app.input, 'set_surrounding_text', 'Grüße', 3, 3)
  app.client.request(app.input, 'set_content_type', HINT_LATIN,
                     PURPOSE_DATE)
  app.request('commit_state', 11)
  method.client.wait_for(context, 'commit_state', timeout=5, after=before)
  method.client.pump(0.2)
  events = method.client.events_of(context)[before:]
  results.check('WG-8: surrounding text, content type and commit_state '
                'reach the input-method-v1 context',
                ('surrounding_text', ['Grüße', 3, 3]) in events
                and ('content_type', [HINT_LATIN, PURPOSE_DATE]) in events
                and ('commit_state', [11]) in events, str(events))
  seen = len(app.client.events_of(app.input))
  method.client.request(context, 'preedit_string', 11, 'にほ', 'にほ')
  method.client.request(context, 'commit_string', 11, '日本')
  method.client.roundtrip()
  app.client.wait_for(app.input, 'commit_string', timeout=5, after=seen)
  arrived = app.client.events_of(app.input)[seen:]
  results.check('WG-8: the input method\'s preedit and committed text '
                'arrive at the application with its serial',
                arrived == [('preedit_string', [11, 'にほ', 'にほ']),
                            ('commit_string', [11, '日本'])], str(arrived))
  seen = len(app.client.events_of(app.input))
  app.request('deactivate', app.seat)
  left = app.client.wait_for(app.input, 'leave', timeout=5, after=seen)
  ended = method.client.wait_for(method.method, 'deactivate', timeout=5)
  results.check('WG-8: deactivate sends leave and ends the context',
                left and ended, str(method.client.events_of(method.method)))

def v2_checks(results, display, app):
  """An input-method-v2 client serving the same text input."""
  im = wayland_wire.Client(display)
  try:
    seat = im.bind('wl_seat', 7)
    manager = im.bind('zwp_input_method_manager_v2', 1)
    method = im.request(manager, 'get_input_method', seat,
                        wayland_wire.NewId('zwp_input_method_v2'))
    im.roundtrip()
    seen = len(app.client.events_of(app.input))
    app.request('activate', app.seat, app.surface)
    app.client.request(app.input, 'set_surrounding_text', 'abc', 3, 3)
    app.request('commit_state', 21)
    im.wait_for(method, 'surrounding_text', timeout=5)
    im.pump(0.2)
    names = im.names_of(method)
    results.check('WG-8: input-method v2 is activated for the v1 text '
                  'input and gets its surrounding text',
                  names[:1] == ['activate'] and
                  ['abc', 3, 3] in arguments_of(im, method,
                                                'surrounding_text'),
                  str(im.events_of(method)))
    seen = len(app.client.events_of(app.input))
    im.request(method, 'delete_surrounding_text', 1, 0)
    im.request(method, 'commit_string', 'xyz')
    im.request(method, 'commit', names.count('done'))
    im.roundtrip()
    app.client.wait_for(app.input, 'commit_string', timeout=5, after=seen)
    arrived = app.client.events_of(app.input)[seen:]
    results.check('WG-8: an input-method-v2 commit arrives as v1 '
                  'delete_surrounding_text and commit_string',
                  arrived == [('delete_surrounding_text', [-1, 1]),
                              ('commit_string', [21, 'xyz'])],
                  str(arrived))
  finally:
    im.close()

def headless_checks(results, run, binary):
  """The checks against headless Shingle."""
  shingle = run.Headless(binary, 'text-input-v1', text_input_config(run),
                         'yaml')
  app = method = None
  try:
    display = shingle.wayland_display()
    app = App(display)
    results.check('WG-8: zwp_text_input_manager_v1 is announced at '
                  'version 1',
                  app.client.has_global('zwp_text_input_manager_v1') == 1,
                  str(app.client.has_global('zwp_text_input_manager_v1')))
    method = MethodV1(display)
    app.request('activate', app.seat, app.surface)
    v1_checks(results, app, method)
    method.client.close()
    method = None
    v2_checks(results, display, app)
  finally:
    for client in (app and app.client, method and method.client):
      if client:
        client.close()
    status = shingle.stop()
  results.check('WG-8: SIGTERM exits headless with status 0', status == 0,
                str(status))
  return shingle.log_text()
