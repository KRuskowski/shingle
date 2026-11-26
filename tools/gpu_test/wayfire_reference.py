"""Headless Wayfire as a behavioural reference, run inside run.py's
namespace.

`Wayfire` starts the host's `wayfire` (Debian's 0.9 on the test host)
with wlroots' headless backend and its GLES2 renderer on the render
node, under a wayfire.ini of the caller's, with a private runtime
directory and IPC socket. The namespace maps the caller to root, and
Wayfire refuses to run as root, so it runs in a nested user namespace
that maps it back to the caller's own uid (`unshare --user
--map-user`). It offers the parts of checks.Shingle that the reference
checks use: `call`, `views`, `view`, `client_environment` and
`directory` for checks.Client and checks.Captures, and `stop`, which
signals only through `signal_checked`.
"""

import glob
import os
import shutil
import subprocess
import time

import checks

xw = checks.xw

def wayland_socket(runtime):
  """The Wayland socket in `runtime`, or None."""
  sockets = [path for path in glob.glob(os.path.join(runtime, 'wayland-*'))
             if not path.endswith('.lock')]
  return sockets[0] if sockets else None

def unprivileged_uid():
  """The uid Wayfire runs as: the caller's on the host, as run.py
  records it, never 0."""
  uid = os.environ.get('SHINGLE_GPU_TEST_UID', '1000')
  return uid if uid.isdigit() and uid != '0' else '1000'

class Wayfire:
  """One headless Wayfire in /tmp/NAME."""

  def __init__(self, name, ini):
    """Starts Wayfire on `ini`, the text of a wayfire.ini; raises
    RuntimeError with the reason when it does not come up."""
    if shutil.which('wayfire') is None:
      raise RuntimeError('no wayfire on PATH')
    self.directory = os.path.join('/tmp', name)
    self.runtime = os.path.join(self.directory, 'runtime')
    os.makedirs(self.runtime, mode=0o700)
    config_path = os.path.join(self.directory, 'wayfire.ini')
    with open(config_path, 'w') as file:
      file.write(ini)
    self.ipc_path = os.path.join(self.runtime, 'ipc')
    self.environment = {key: value for key, value in os.environ.items()
                        if key not in checks.run.STRIPPED_VARIABLES}
    self.environment.update({
        'XDG_RUNTIME_DIR': self.runtime,
        'HOME': self.directory,
        'WLR_BACKENDS': 'headless',
        'WLR_RENDERER': 'gles2',
        'WLR_RENDER_DRM_DEVICE': '/dev/dri/renderD128',
        'WLR_HEADLESS_OUTPUTS': '1',
        '_WAYFIRE_SOCKET': self.ipc_path,
    })
    self.log_path = os.path.join(self.directory, 'wayfire.log')
    self.log = open(self.log_path, 'w')
    uid = unprivileged_uid()
    self.process = subprocess.Popen(
        ['unshare', '--user', f'--map-user={uid}', f'--map-group={uid}',
         '--', 'wayfire', '-c', config_path],
        env=self.environment, stdout=self.log, stderr=self.log,
        stdin=subprocess.DEVNULL)
    self.socket = xw.wait_for(
        lambda: self.process.poll() is None
        and os.path.exists(self.ipc_path) and wayland_socket(self.runtime),
        timeout=20)
    if not self.socket:
      self.stop()
      raise RuntimeError(
          f'wayfire did not start: {checks.tail(self.log_text(), 2)}')
    self.ipc = xw.Ipc(self.ipc_path)
    # The output's first frame follows its enabling.
    time.sleep(1.0)

  def call(self, method, data=None):
    """One IPC request."""
    return self.ipc.call(method, data)

  def views(self):
    """`window-rules/list-views`."""
    return self.call('window-rules/list-views')

  def view(self, app_id):
    """The one view with `app_id`, or None."""
    views = [view for view in self.views() if view.get('app-id') == app_id]
    return views[0] if len(views) == 1 else None

  def client_environment(self, extra=None):
    """The environment of a Wayland client of this Wayfire."""
    environment = dict(self.environment,
                       WAYLAND_DISPLAY=self.socket)
    environment[checks.NAMESPACE_VARIABLE] = '1'
    environment.update(extra or {})
    return environment

  def log_text(self):
    """Wayfire's output so far."""
    if not self.log.closed:
      self.log.flush()
    with open(self.log_path, errors='replace') as file:
      return file.read()

  def stop(self):
    """SIGTERM through `signal_checked`, then SIGKILL; returns the exit
    status. `unshare` execs Wayfire, so the PID is Wayfire's."""
    if getattr(self, 'ipc', None) is not None:
      self.ipc.close()
    try:
      return checks._stop(self.process, 'wayfire', 10)
    finally:
      self.log.close()
