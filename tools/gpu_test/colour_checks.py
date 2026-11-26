"""The real-client check that configured colours reach the screen as the
bytes Wayfire writes for them, run inside run.py's namespace.

crates/shingle-render/src/color.rs. Offscreen Shingle reads a synthetic
wayfire.ini with `[core] background_color = \\#336699FF` and an opaque
`[decoration] active_color = 0.4 0.2 0.6 1.0`, mid-tones that a linear
reading shows much lighter:

- `colours`: a `grim` capture of the empty output shows 0x33, 0x66 and
  0x99 exactly, the bytes Wayfire's GLES2 renderer writes; a
  `wl_probe --decoration server`, floated, is drawn inside a frame of
  102, 51 and 153; and headless Wayfire on the same background_color
  (wayfire_reference.py) shows the same bytes in its own capture.

Signals go through `run.signal_checked`, via the Shingle and Client
helpers of checks.py and `Wayfire.stop`, to children of this process
inside the PID namespace.
"""

import checks
import wayfire_reference

xw = checks.xw
GROUPS = ('colours',)
BACKGROUND = (0x33, 0x66, 0x99)
FRAME = (102, 51, 153)
INI = r"""[core]
plugins = decoration simple-tile move place
background_color = \#336699FF
xwayland = false
vwidth = 1
vheight = 1

[workarounds]
auto_reload_config = false

[simple-tile]
animation_duration = 0
tile_by_default = none

[decoration]
active_color = 0.4 0.2 0.6 1.0

[autostart]
autostart_wf_shell = false
"""
# Wayfire's own run: only IPC, so that nothing but the background shows.
WAYFIRE_INI = r"""[core]
plugins = ipc
background_color = \#336699FF
xwayland = false

[autostart]
autostart_wf_shell = false
"""
GEOMETRY = {'x': 200, 'y': 150, 'width': 400, 'height': 300}

def background_pixels(shot):
  """The output's four corners, where nothing but the background shows."""
  if shot is None:
    return None
  return {shot.pixel(x, y) for x, y in
          ((1, 1), (shot.width - 2, 1), (1, shot.height - 2),
           (shot.width - 2, shot.height - 2))}

def frame_pixels(shot, geometry):
  """Points of the frame's top border and its left and bottom borders."""
  if shot is None:
    return None
  return {shot.pixel(x, y) for x, y in
          ((geometry['x'] + 1, geometry['y'] + 1),
           (geometry['x'] + 1, geometry['y'] + geometry['height'] // 2),
           (geometry['x'] + geometry['width'] // 2,
            geometry['y'] + geometry['height'] - 2))}

def wayfire_background(captures):
  """The corners of headless Wayfire's capture under WAYFIRE_INI, or
  None and the reason there is none."""
  try:
    wayfire = wayfire_reference.Wayfire('colours-wayfire', WAYFIRE_INI)
  except RuntimeError as error:
    return None, str(error)
  try:
    shot = captures.settled(
        wayfire, lambda shot: background_pixels(shot) == {BACKGROUND},
        timeout=10)
    return background_pixels(shot), 'captured'
  finally:
    wayfire.stop()

def colour_checks(results, binary, wl_probe, captures):
  """The background and a frame in their configured bytes, and Wayfire's
  background beside Shingle's."""
  shingle = checks.Shingle(binary, 'colours', None, ini=INI)
  probe = None
  shingle_background = None
  try:
    shot = captures.settled(
        shingle, lambda shot: background_pixels(shot) == {BACKGROUND},
        stable=True)
    shingle_background = background_pixels(shot)
    results.check('colours: grim shows background_color \\#336699FF as the '
                  'bytes 0x33, 0x66, 0x99',
                  shingle_background == {BACKGROUND},
                  f'corners {shingle_background}')
    probe = checks.Client(
        shingle, [wl_probe, 'framed', '--pattern', '--decoration', 'server'],
        'framed', program='wl_probe')
    view = xw.wait_for(lambda: shingle.view('framed'), timeout=15)
    if view is not None:
      shingle.call('window-rules/configure-view',
                   {'id': view['id'], 'geometry': GEOMETRY})
    shot = captures.settled(
        shingle, lambda shot: frame_pixels(shot, GEOMETRY) == {FRAME},
        stable=True)
    results.check('colours: the focused frame shows active_color 0.4 0.2 '
                  '0.6 as the bytes 102, 51, 153',
                  frame_pixels(shot, GEOMETRY) == {FRAME},
                  f'frame {frame_pixels(shot, GEOMETRY)}, view {view}')
  finally:
    if probe is not None:
      probe.stop()
    status = shingle.stop()
    results.check('colours: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')
  reference, reason = wayfire_background(captures)
  if reference is None:
    print(f'INFO colours: no Wayfire reference: {reason}', flush=True)
    return
  results.check('colours: headless Wayfire on GLES2 writes the same '
                'background bytes as Shingle',
                reference == shingle_background == {BACKGROUND},
                f'Wayfire {reference}, Shingle {shingle_background}')

CHECKS = {
    'colours': colour_checks,
}
