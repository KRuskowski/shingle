"""Real-client checks of the lock screen, run inside run.py's namespace.

docs/design/lock-screen.md, "Verification". Offscreen Shingle runs with
two 640x400 outputs and `SHINGLE_RECORD_FRAMES` pointing at a tmpfs, so
that every frame it presents is written as a PPM noting the lock state
and whether a lock surface was drawn. A thread reads each frame as it
appears and deletes it. A `wl_probe --pattern` window shows the four
colour quadrants that a frame must never show unblurred while the lock
hides the clients: no frame then may hold two of the pattern's
neighbouring colours, pure, six pixels apart, and every frame without a
lock surface must be black.

The group `lock` locks with shingle-lock (`--lock`, the debug build by
default) and an invented swaylock config: screenshot, effect-blur,
effect-vignette, clock and test colours for the input's states. It
checks the blurred screenshot and its drift, the input appearing on
typing, the verify and wrong states and the failed-attempt count, Caps
Lock in its own colours with its label, the keyboard layout's label
(the keymap is `us,de`) following a group change, the fade-out after
lock.input_timeout, the drift stopping while DPMS is off and the first
frames after the wake, HEADLESS-2 turned off and on with wlr-randr while
locked, an output created over IPC while locked showing the blurred
capture of HEADLESS-1, the unlock with the test password, a SIGKILLed
lock client leaving the outputs black, the lock binding starting a new
client after that, a grace-period unlock by a client that takes over
another crashed lock, and the lock binding while unlocked, which starts
shingle-lock through lock.command while the clients still show, so that
it captures them, and every output shows the lock within
LOCK_UP_SECONDS of the binding.

PAM is a private /etc/pam.d, bind-mounted in this namespace only, whose
`shingle` service runs a script that accepts only the invented password
ACCEPTED_PHRASE; no real password is used. Signals go through
`run.signal_checked` to children of this process inside the PID
namespace.
"""

import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

import checks
import run

xw = checks.xw
GROUPS = ('lock',)
WIDTH, HEIGHT = 640, 400
OUTPUTS = (
    {'name': 'HEADLESS-1', 'mode': f'{WIDTH}x{HEIGHT}@60'},
    {'name': 'HEADLESS-2', 'mode': f'{WIDTH}x{HEIGHT}@60'},
)
PROBE_SIZE = (320, 240)
# Invented for these checks; letters only, so that STIPC can type it.
ACCEPTED_PHRASE = 'invented'
REFUSED_PHRASE = 'notit'
DRIFT_FPS = 6
INPUT_TIMEOUT = 2
DPMS_SECONDS = 9
# The input's border in each state, chosen apart from the pattern's
# colours: cyan typing, yellow verifying, magenta wrong; the highlights
# orange, which classify as none of them.
SWAYLOCK_CONFIG = """# invented values for the lock checks
screenshot
effect-blur=10x3
effect-vignette=0.6:0.4
clock
show-failed-attempts
indicator-radius=40
indicator-thickness=12
font-size=20
ring-color=00ffff
ring-ver-color=ffff00
ring-wrong-color=ff00ff
inside-color=202020ff
inside-ver-color=202020ff
inside-wrong-color=202020ff
text-color=c0c0c0
key-hl-color=ff8000
bs-hl-color=ff8000
separator-color=804000
indicator-caps-lock
ring-caps-lock-color=8000ff
inside-caps-lock-color=202020ff
caps-lock-key-hl-color=ff8000
caps-lock-bs-hl-color=ff8000
layout-bg-color=202020ff
layout-text-color=00ff80
"""
# The Caps Lock ring and the layout label's text, which classify as none
# of checks.classify's colours.
VIOLET = 'violet'
MINT = 'mint'
LAYOUTS = ('English (US)', 'German')
# An output created over IPC while locked, of another shape than the
# captures.
ADDED_SIZE = (800, 400)
# The PAM check: the token arrives on stdin, NUL-terminated; it sleeps
# so that the verify state is drawn.
PAM_CHECK = f"""#!{sys.executable}
import sys, time
token = sys.stdin.buffer.read().rstrip(b'\\0').rstrip(b'\\n')
time.sleep(1)
sys.exit(0 if token == {ACCEPTED_PHRASE.encode()!r} else 1)
"""
PAM_SERVICE = """auth required pam_exec.so expose_authtok quiet {check}
account required pam_permit.so
"""
# The pattern's neighbouring quadrants, side by side and one above the
# other (checks.PATTERN).
HORIZONTAL_PAIRS = {('red', 'green'), ('green', 'red'), ('blue', 'white'),
                    ('white', 'blue')}
VERTICAL_PAIRS = {('red', 'blue'), ('blue', 'red'), ('green', 'white'),
                  ('white', 'green')}
SHARP_DISTANCE = 8
ROW_STEP = 4
COLUMN_STEP = 2
HIDING = ('pending', 'locked', 'failed')
# How long the lock binding may take until every output shows the lock.
LOCK_UP_SECONDS = 1.0

def parse_frame(data):
  """(info, width, height, RGB bytes) of a recorded frame, or None; info
  holds the header comment's fields."""
  lines = data.split(b'\n', 4)
  if len(lines) < 5 or lines[0] != b'P6' or not lines[1].startswith(b'#'):
    return None
  info = dict(field.split('=', 1)
              for field in lines[1][1:].decode().split() if '=' in field)
  width, height = (int(value) for value in lines[2].split())
  pixels = lines[4]
  if len(pixels) != width * height * 3:
    return None
  return info, width, height, pixels

def pixel(frame, x, y):
  """The (r, g, b) at (x, y) of `frame` (info, width, height, pixels)."""
  _, width, _, pixels = frame
  offset = (y * width + x) * 3
  return tuple(pixels[offset:offset + 3])

def classify(r, g, b):
  """checks.classify, and VIOLET (the Caps Lock ring) and MINT (the
  layout label's text), which it leaves out."""
  name = checks.classify(r, g, b)
  if name is not None:
    return name
  if 90 < r < 170 and g < 60 and b > 200:
    return VIOLET
  if r < 60 and g > 200 and 90 < b < 170:
    return MINT
  return None

def row_classes(frame, y, step=2):
  """classify of every `step`th pixel of row `y`."""
  _, width, _, pixels = frame
  start = y * width * 3
  row = pixels[start:start + width * 3]
  return [classify(row[x * 3], row[x * 3 + 1], row[x * 3 + 2])
          for x in range(0, width, step)]

def sampled_rows(frame):
  """[(y, row_classes)] of every ROW_STEPth row of `frame`."""
  _, _, height, _ = frame
  return [(y, row_classes(frame, y, COLUMN_STEP))
          for y in range(0, height, ROW_STEP)]

def sharp_at(frame, rows=None):
  """Where `frame` shows the probe's pattern unblurred: the first point
  with two of its neighbouring colours, pure, SHARP_DISTANCE pixels
  apart across or down; None when there is none."""
  rows = sampled_rows(frame) if rows is None else rows
  across = SHARP_DISTANCE // COLUMN_STEP
  down = SHARP_DISTANCE // ROW_STEP
  for index, (y, here) in enumerate(rows):
    for column, (left, right) in enumerate(zip(here, here[across:])):
      if (left, right) in HORIZONTAL_PAIRS:
        return (column * COLUMN_STEP, y)
    if index + down < len(rows):
      below = rows[index + down][1]
      for column, (top, bottom) in enumerate(zip(here, below)):
        if (top, bottom) in VERTICAL_PAIRS:
          return (column * COLUMN_STEP, y)
  return None

def sharp_pattern(frame):
  """Whether `frame` shows the pattern unblurred anywhere (sharp_at)."""
  return sharp_at(frame) is not None

def all_black(frame, columns=20, rows=12):
  """Whether a grid over `frame` is black."""
  _, width, height, _ = frame
  return all(max(pixel(frame, width * (2 * c + 1) // (2 * columns),
                       height * (2 * r + 1) // (2 * rows))) < 8
             for c in range(columns) for r in range(rows))

def colour_count(frame, name, rows=None):
  """How many sampled pixels of `frame` classify as `name`."""
  rows = sampled_rows(frame) if rows is None else rows
  return sum(classes.count(name) for _, classes in rows)

def reddish(rgb):
  """Whether `rgb` is clearly more red than anything else."""
  r, g, b = rgb
  return r > g + 40 and r > b + 40

def difference(first, second, step=16):
  """The largest channel difference over a grid of two frames."""
  _, width, height, _ = first
  return max(abs(a - b)
             for y in range(0, height, step) for x in range(0, width, step)
             for a, b in zip(pixel(first, x, y), pixel(second, x, y)))

class Frame:
  """One recorded frame's verdicts, when it was read, and the frame if
  kept."""

  def __init__(self, info, frame, keep):
    self.output = info.get('output')
    self.number = int(info.get('frame', -1))
    self.lock = info.get('lock')
    self.lock_surface = info.get('lock_surface') == '1'
    # When Shingle presented it, on CLOCK_MONOTONIC as time.monotonic.
    self.arrived = float(info.get('time', 0))
    rows = sampled_rows(frame)
    self.sharp_point = sharp_at(frame, rows)
    self.sharp = self.sharp_point is not None
    self.black = all_black(frame)
    self.cyan = colour_count(frame, 'cyan', rows)
    self.yellow = colour_count(frame, 'yellow', rows)
    self.magenta = colour_count(frame, 'magenta', rows)
    self.violet = colour_count(frame, VIOLET, rows)
    self.mint = colour_count(frame, MINT, rows)
    self.frame = frame if keep else None

class FrameReader:
  """Reads the recorded frames as they appear, in a thread, and deletes
  them. The last frames of each output are kept whole."""

  def __init__(self, directory, kept=6, evidence=None):
    self.directory = directory
    self.kept = kept
    # Where frames that show a client under the lock are saved.
    self.evidence = evidence
    self.frames = []
    self.lock = threading.Lock()
    self.unreadable = 0
    self._stop = threading.Event()
    self._thread = threading.Thread(target=self._run, daemon=True)
    self._thread.start()

  def _run(self):
    while not self._stop.is_set():
      if not self._read_new():
        time.sleep(0.02)
    self._read_new()

  def _read_new(self):
    try:
      names = sorted(name for name in os.listdir(self.directory)
                     if name.endswith('.ppm'))
    except FileNotFoundError:
      return False
    # By output and number, so that each output's frames are in order.
    for name in names:
      path = os.path.join(self.directory, name)
      try:
        with open(path, 'rb') as file:
          data = file.read()
        os.unlink(path)
      except OSError:
        continue
      parsed = parse_frame(data)
      with self.lock:
        if parsed is None:
          self.unreadable += 1
          continue
        frame = Frame(parsed[0], parsed, True)
        self.frames.append(frame)
        if frame.sharp and frame.lock in HIDING and self.evidence:
          os.makedirs(self.evidence, exist_ok=True)
          name = f'{frame.output}-{frame.number:06}.ppm'
          with open(os.path.join(self.evidence, name), 'wb') as file:
            file.write(data)
        mine = [old for old in self.frames
                if old.output == frame.output and old.frame is not None]
        for old in mine[:-self.kept]:
          old.frame = None
    return bool(names)

  def since(self, moment, output=None):
    """The frames read after `moment`, of `output` if given."""
    with self.lock:
      return [frame for frame in self.frames if frame.arrived > moment
              and (output is None or frame.output == output)]

  def wait(self, predicate, timeout=10.0, moment=0.0):
    """Waits for a frame read after `moment` that `predicate` accepts;
    returns it or None."""
    return xw.wait_for(lambda: next(
        (frame for frame in self.since(moment) if predicate(frame)), None),
                       timeout=timeout, interval=0.05)

  def drain(self, timeout=10.0):
    """Waits until every frame written so far has been read."""
    def empty():
      try:
        return not any(name.endswith('.ppm')
                       for name in os.listdir(self.directory))
      except FileNotFoundError:
        return True
    xw.wait_for(empty, timeout=timeout, interval=0.05)
    # The last one listed may still be in hand.
    time.sleep(0.1)

  def stop(self):
    """Reads what is left and stops."""
    self._stop.set()
    self._thread.join()

def private_pam(base):
  """Bind-mounts a private /etc/pam.d whose `shingle` service accepts
  only ACCEPTED_PHRASE; only this namespace sees it."""
  directory = os.path.join(base, 'pam.d')
  os.makedirs(directory)
  check = os.path.join(base, 'pam-check.py')
  with open(check, 'w') as file:
    file.write(PAM_CHECK)
  os.chmod(check, 0o755)
  with open(os.path.join(directory, 'shingle'), 'w') as file:
    file.write(PAM_SERVICE.format(check=check))
  subprocess.run(['mount', '--bind', directory, '/etc/pam.d'], check=True)

def recording_directory(path):
  """A tmpfs at `path` for the frames, so that they never reach a disk."""
  os.makedirs(path)
  subprocess.run(['mount', '-t', 'tmpfs', '-o', 'size=768m', 'none', path],
                 check=True)

def key_names(text):
  """The evdev names of `text`'s lower-case letters."""
  return [f'KEY_{character.upper()}' for character in text]

class Lock:
  """shingle-lock as a client of `shingle`, with its frames traced."""

  def __init__(self, shingle, binary, arguments, name):
    # The lock section of the config Shingle reads, as Shingle passes it
    # to the lock client it starts itself.
    config = os.path.join(shingle.directory, 'shingle.yaml')
    self.client = checks.Client(
        shingle, [binary, '--shingle-config', config, *arguments], name,
        extra_environment={'SHINGLE_LOCK_TRACE': '1'},
        program='shingle-lock')

  def ready(self, timeout=30):
    """Whether it reported its surfaces committed and locked."""
    return xw.wait_for(lambda: 'shingle-lock: ready' in self.client.output(),
                       timeout=timeout)

  def frames_drawn(self):
    """How many frames its trace reports."""
    return self.client.output().count('shingle-lock: frame ')

  def kill(self):
    """SIGKILL through `run.signal_checked`; returns whether sent."""
    return run.signal_checked(self.client.process.pid, signal.SIGKILL,
                              'shingle-lock')

def lock_config(lock_binary, swaylock):
  """The YAML config, as JSON."""
  config = dict(checks.BASE_CONFIG)
  config.update({
      'input': {'layout': 'us,de'},
      'outputs': [dict(output) for output in OUTPUTS],
      'animate': {'open_animation': 'none', 'close_animation': 'none'},
      'idle': {'dpms_seconds': DPMS_SECONDS},
      'lock': {
          'command': lock_binary,
          'swaylock_config': swaylock,
          'drift': {'fps': DRIFT_FPS, 'amplitude': 12, 'period': 20},
          'input_timeout': INPUT_TIMEOUT,
      },
  })
  return config

def describe(frames):
  """A short account of `frames` for a message."""
  if not frames:
    return 'no frames'
  states = sorted({frame.lock for frame in frames})
  return f'{len(frames)} frames, states {states}'

def type_text(shingle, text):
  """Taps each letter of `text` over STIPC."""
  for name in key_names(text):
    for state in (True, False):
      shingle.call('stipc/feed_key', {'key': name, 'state': state})
      time.sleep(0.03)

def tap(shingle, name):
  """Taps one key over STIPC."""
  for state in (True, False):
    shingle.call('stipc/feed_key', {'key': name, 'state': state})

def lock_checks(results, binary, wl_probe, captures):
  """The lock screen, frame by frame; see the module docstring."""
  del captures
  lock_binary = os.environ.get('SHINGLE_LOCK_BINARY',
                               os.path.abspath('target/debug/shingle-lock'))
  if not os.path.exists(lock_binary):
    results.check('lock: shingle-lock is built', False, lock_binary)
    return
  base = '/tmp/lock-checks'
  os.makedirs(base)
  private_pam(base)
  recorded = os.path.join(base, 'frames')
  recording_directory(recorded)
  swaylock = os.path.join(base, 'swaylock-config')
  with open(swaylock, 'w') as file:
    file.write(SWAYLOCK_CONFIG)
  reader = FrameReader(recorded, evidence=os.path.join(
      os.environ.get('SHINGLE_LOCK_EVIDENCE', base), 'evidence'))
  shingle = checks.Shingle(
      binary, 'lock', lock_config(lock_binary, swaylock),
      backend=('--backend', 'offscreen', '--outputs', '2'),
      extra_environment={'SHINGLE_RECORD_FRAMES': recorded,
                         'SHINGLE_LOCK_TRACE': '1'})
  clients = []
  try:
    probe = checks.Client(
        shingle, [wl_probe, 'lock-pattern', '--pattern', '--size',
                  f'{PROBE_SIZE[0]}x{PROBE_SIZE[1]}'], 'lock-pattern',
        program='wl_probe')
    clients.append(probe)
    view = xw.wait_for(lambda: shingle.view('lock-pattern'), timeout=15)
    shown = reader.wait(lambda frame: frame.sharp and frame.lock == 'unlocked',
                        timeout=15)
    results.check('lock: unlocked frames show the pattern sharp (the '
                  'detector sees it)', shown is not None and view,
                  describe(reader.since(0)))
    geometry = view['geometry'] if view else None
    run_sessions(results, shingle, reader, lock_binary, swaylock, geometry,
                 clients)
  finally:
    for client in clients:
      client.stop()
    status = shingle.stop()
    reader.stop()
    results.check('lock: SIGTERM exits offscreen Shingle with status 0',
                  status == 0,
                  f'status {status}, {checks.tail(shingle.log_text())}')
  frames = reader.since(0)
  hiding = [frame for frame in frames if frame.lock in HIDING]
  sharp = [frame for frame in hiding if frame.sharp]
  results.check('lock: no frame presented while the lock hides the clients '
                'shows the pattern unblurred',
                hiding and not sharp,
                f'{len(hiding)} of {len(frames)} frames hid the clients, '
                f'{len(sharp)} sharp: '
                f'{[(f.output, f.number, f.lock, f.sharp_point)
                    for f in sharp[:5]]}')
  bare = [frame for frame in hiding if not frame.lock_surface]
  lit = [frame for frame in bare if not frame.black]
  results.check('lock: every such frame without a lock surface is black',
                bare and not lit,
                f'{len(bare)} without a lock surface, {len(lit)} not black: '
                f'{[(f.output, f.number, f.lock) for f in lit[:5]]}')
  results.check('lock: every recorded frame was read',
                reader.unreadable == 0, f'{reader.unreadable} unreadable')
  if not sharp:
    shutil.rmtree(base, ignore_errors=True)

def locked_frames(reader, moment, output='HEADLESS-1'):
  """Frames of `output` with a lock surface read after `moment`."""
  return [frame for frame in reader.since(moment, output)
          if frame.lock == 'locked' and frame.lock_surface]

def run_sessions(results, shingle, reader, lock_binary, swaylock, geometry,
                 clients):
  """Three lock clients and the lock binding, one after another."""
  # 1. Lock, look, type, fail, fade, sleep, wake, replug, unlock.
  before = time.monotonic()
  first = Lock(shingle, lock_binary, ['-C', swaylock], 'lock-first')
  clients.append(first.client)
  results.check('lock: shingle-lock reports the session locked',
                first.ready(), checks.tail(first.client.output()))
  shot = reader.wait(lambda frame: frame.lock == 'locked'
                     and frame.output == 'HEADLESS-1' and frame.frame,
                     moment=before)
  if shot is not None and geometry:
    # The red quadrant's centre, blurred: still red.
    x = geometry['x'] + geometry['width'] // 4
    y = geometry['y'] + geometry['height'] // 4
    results.check('lock: the background is the blurred screenshot',
                  reddish(pixel(shot.frame, x, y)) and not shot.sharp,
                  str(pixel(shot.frame, x, y)))
  else:
    results.check('lock: the background is the blurred screenshot', False,
                  describe(reader.since(before)))
  results.check('lock: at rest the input is not shown',
                shot is not None and shot.cyan == 0,
                str(shot.cyan) if shot else '')
  # The drift: frames keep coming at about DRIFT_FPS and differ.
  start = time.monotonic()
  drawn = first.frames_drawn()
  time.sleep(3)
  reader.drain()
  rest = locked_frames(reader, start)
  rate = (first.frames_drawn() - drawn) / 3 / 2
  kept = [frame for frame in rest if frame.frame]
  moved = len(kept) >= 2 and difference(kept[0].frame, kept[-1].frame) > 0
  results.check('lock: the background drifts at about drift.fps',
                moved and DRIFT_FPS * 0.5 <= rate <= DRIFT_FPS * 1.5,
                f'{rate:.1f} frames per second per output, '
                f'{len(rest)} presented, moved {moved}')
  # Typing shows the input.
  moment = time.monotonic()
  type_text(shingle, REFUSED_PHRASE)
  typed = reader.wait(lambda frame: frame.lock == 'locked'
                      and frame.cyan > 20, moment=moment)
  results.check('lock: typing fades in the outlined input',
                typed is not None, describe(reader.since(moment)))
  moment = time.monotonic()
  tap(shingle, 'KEY_ENTER')
  verifying = reader.wait(lambda frame: frame.yellow > 20, moment=moment)
  results.check('lock: Enter shows the verify state',
                verifying is not None, describe(reader.since(moment)))
  wrong = reader.wait(lambda frame: frame.magenta > 20, moment=moment,
                      timeout=10)
  results.check('lock: a wrong password shows the wrong state',
                wrong is not None, describe(reader.since(moment)))
  results.check('lock: and counts the failed attempt',
                xw.wait_for(lambda: '1 failed attempt'
                            in first.client.output(), timeout=5),
                checks.tail(first.client.output()))
  keyboard_checks(results, shingle, reader, first)
  last_key = time.monotonic()
  time.sleep(INPUT_TIMEOUT + 2)
  reader.drain()
  faded = locked_frames(reader, time.monotonic() - 1)
  results.check('lock: the input fades out after lock.input_timeout',
                faded and all(frame.cyan == 0 and frame.magenta == 0
                              for frame in faded),
                f'{describe(faded)}, cyan and magenta '
                f'{[(frame.cyan, frame.magenta) for frame in faded]}')
  # DPMS off after DPMS_SECONDS without input: nothing is drawn.
  time.sleep(max(0.0, last_key + DPMS_SECONDS + 1.0 - time.monotonic()))
  off = time.monotonic()
  drawn = first.frames_drawn()
  time.sleep(3)
  reader.drain()
  presented = reader.since(off)
  results.check('lock: with DPMS off no frame is presented and the drift '
                'stops', not presented and first.frames_drawn() == drawn,
                f'{len(presented)} presented, '
                f'{first.frames_drawn() - drawn} drawn by shingle-lock')
  wake = time.monotonic()
  tap(shingle, 'KEY_LEFTSHIFT')
  woke = reader.wait(lambda frame: True, moment=wake)
  results.check('lock: the first frame after the wake shows the lock, '
                'not the clients',
                woke is not None and woke.lock_surface and not woke.sharp,
                f'{woke.output} {woke.lock} lock surface '
                f'{woke.lock_surface}' if woke else 'no frame')
  results.check('lock: and the drift goes on',
                xw.wait_for(lambda: first.frames_drawn() > drawn + 3,
                            timeout=5),
                str(first.frames_drawn() - drawn))
  # HEADLESS-2 off and on while locked.
  status, output = shingle_randr(shingle, '--output', 'HEADLESS-2', '--off')
  results.check('lock: wlr-randr turns HEADLESS-2 off while locked',
                status == 0, output)
  time.sleep(0.5)
  back = time.monotonic()
  status, output = shingle_randr(shingle, '--output', 'HEADLESS-2', '--on')
  results.check('lock: and on again', status == 0, output)
  again = reader.wait(lambda frame: frame.output == 'HEADLESS-2'
                      and frame.lock_surface, moment=back, timeout=10)
  results.check('lock: the output that came back gets a lock surface, '
                'black until then',
                again is not None and all(
                    frame.black for frame in reader.since(back, 'HEADLESS-2')
                    if frame.arrived < again.arrived),
                describe(reader.since(back, 'HEADLESS-2')))
  own = re.compile(r'\(HEADLESS-2\) shows the capture of output \d+ '
                   r'\(HEADLESS-2\)')
  results.check('lock: and shows its own capture again, found by name',
                xw.wait_for(lambda: len(own.findall(first.client.output()))
                            >= 2, timeout=5),
                checks.tail(first.client.output()))
  added_output_checks(results, shingle, first, geometry)
  # The test password unlocks.
  type_text(shingle, ACCEPTED_PHRASE)
  moment = time.monotonic()
  tap(shingle, 'KEY_ENTER')
  status = first.client.exited(15)
  results.check('lock: the test password unlocks and shingle-lock exits 0',
                status == 0, checks.tail(first.client.output()))
  reader.drain()
  session = sorted((frame for frame in reader.since(before)
                    if frame.arrived < moment),
                   key=lambda frame: frame.arrived)
  hidden = next((index for index, frame in enumerate(session)
                 if frame.lock in HIDING), None)
  shown = [frame for frame in session[hidden or 0:]
           if frame.lock not in HIDING]
  results.check('lock: from the first frame that hides the clients, every '
                'frame hides them until the unlock',
                hidden is not None and not shown,
                f'{len(session)} frames, the first hiding at {hidden}, '
                f'{len(shown)} showing after it')
  shown = reader.wait(lambda frame: frame.lock == 'unlocked' and frame.sharp,
                      moment=moment)
  results.check('lock: unlocked, the pattern shows again', shown is not None,
                describe(reader.since(moment)))

  # 2. A lock client that is killed leaves the outputs black.
  second = Lock(shingle, lock_binary, ['-C', swaylock], 'lock-second')
  clients.append(second.client)
  results.check('lock: a second lock client locks', second.ready(),
                checks.tail(second.client.output()))
  moment = time.monotonic()
  results.check('lock: SIGKILL reaches it', second.kill())
  failed = reader.wait(lambda frame: frame.lock == 'failed', moment=moment)
  results.check('lock: after the crash the session stays locked, black',
                failed is not None and failed.black and not failed.sharp,
                describe(reader.since(moment)))
  # The lock binding starts lock.command again, and its client unlocks.
  moment = time.monotonic()
  lock_binding(shingle)
  restarted = xw.wait_for(lambda: shingle.log_text().count(
      'shingle-lock: ready') >= 1, timeout=30)
  results.check('lock: after the crash the lock binding starts a new lock '
                'client', restarted, checks.tail(shingle.log_text()))
  type_text(shingle, ACCEPTED_PHRASE)
  tap(shingle, 'KEY_ENTER')
  shown = reader.wait(lambda frame: frame.lock == 'unlocked' and frame.sharp,
                      moment=moment, timeout=15)
  between = [frame for frame in reader.since(moment)
             if shown is None or frame.arrived < shown.arrived]
  results.check('lock: which unlocks, the outputs black or locked until '
                'then', shown is not None and all(
                    frame.lock in HIDING for frame in between),
                describe(between))
  second.client.exited(5)
  # Lock again for the grace period's client to take over.
  moment = time.monotonic()
  fourth = Lock(shingle, lock_binary, ['-C', swaylock], 'lock-fourth')
  clients.append(fourth.client)
  results.check('lock: a fourth lock client locks', fourth.ready(),
                checks.tail(fourth.client.output()))
  results.check('lock: SIGKILL reaches it', fourth.kill())
  reader.wait(lambda frame: frame.lock == 'failed', moment=moment)

  # 3. The next client takes over and its grace period unlocks.
  third = Lock(shingle, lock_binary, ['-C', swaylock, '--grace', '5'],
               'lock-third')
  clients.append(third.client)
  results.check('lock: a new client takes over the failed lock',
                third.ready(), checks.tail(third.client.output()))
  moment = time.monotonic()
  tap(shingle, 'KEY_A')
  results.check('lock: a key in the grace period unlocks without a password',
                third.client.exited(10) == 0 and 'grace period unlocks'
                in third.client.output(), checks.tail(third.client.output()))
  shown = reader.wait(lambda frame: frame.lock == 'unlocked' and frame.sharp,
                      moment=moment)
  results.check('lock: and the pattern shows again', shown is not None,
                describe(reader.since(moment)))

  # 4. The lock binding starts lock.command while the clients still show.
  moment = time.monotonic()
  lock_binding(shingle)
  started = xw.wait_for(lambda: shingle.log_text().count(
      'shingle-lock: ready') >= 2, timeout=30)
  starting = [frame for frame in reader.since(moment)
              if frame.lock == 'starting']
  results.check('lock: the lock binding starts shingle-lock, which locks',
                started, checks.tail(shingle.log_text()))
  locked = reader.wait(lambda frame: frame.lock == 'locked'
                       and frame.output == 'HEADLESS-1' and frame.frame,
                       moment=moment)
  if locked is not None and geometry:
    x = geometry['x'] + geometry['width'] // 4
    y = geometry['y'] + geometry['height'] // 4
    blurred = reddish(pixel(locked.frame, x, y))
  else:
    blurred = False
  results.check('lock: it captured the clients while the lock was starting',
                blurred, f'{len(starting)} frames while starting')
  up = xw.wait_for(lambda: lock_up_time(reader, moment), timeout=10)
  timing = [line for line in shingle.log_text().splitlines()
            if 'shingle-lock: timing:' in line]
  results.check(f'lock: the binding\'s lock is up on every output within '
                f'{LOCK_UP_SECONDS} s',
                up is not None and up <= LOCK_UP_SECONDS,
                f'{up:.3f} s; {timing[-1] if timing else "no timing"}'
                if up is not None else 'never')
  type_text(shingle, ACCEPTED_PHRASE)
  moment = time.monotonic()
  tap(shingle, 'KEY_ENTER')
  shown = reader.wait(lambda frame: frame.lock == 'unlocked' and frame.sharp,
                      moment=moment, timeout=15)
  results.check('lock: and the test password unlocks it', shown is not None,
                describe(reader.since(moment)))

def keyboard_checks(results, shingle, reader, lock):
  """Caps Lock and the layout label, while the input shows; ends with
  Caps Lock off, the first layout and the input cleared."""
  # A letter shows the typing state, which the labels belong to.
  moment = time.monotonic()
  type_text(shingle, 'a')
  results.check('lock: typing shows the keyboard layout\'s label, the '
                'keymap having two layouts',
                xw.wait_for(lambda: f'layout label Some("{LAYOUTS[0]}")'
                            in lock.client.output(), timeout=5)
                and reader.wait(lambda frame: frame.lock == 'locked'
                                and frame.mint > 0, moment=moment),
                checks.tail(lock.client.output()))
  moment = time.monotonic()
  tap(shingle, 'KEY_CAPSLOCK')
  caps = reader.wait(lambda frame: frame.lock == 'locked'
                     and frame.violet > 20, moment=moment)
  results.check('lock: Caps Lock turns the input into its Caps Lock '
                'colours (indicator-caps-lock)', caps is not None,
                describe(reader.since(moment)))
  results.check('lock: and shows the Caps Lock label',
                xw.wait_for(lambda: 'caps lock on, layout label '
                            f'Some("{LAYOUTS[0]}"), caps lock label '
                            'Some("Caps Lock")' in lock.client.output(),
                            timeout=5),
                checks.tail(lock.client.output()))
  moment = time.monotonic()
  tap(shingle, 'KEY_CAPSLOCK')
  off = reader.wait(lambda frame: frame.lock == 'locked' and frame.cyan > 20
                    and frame.violet == 0, moment=moment)
  results.check('lock: Caps Lock off again restores the input\'s colours',
                off is not None, describe(reader.since(moment)))
  moment = time.monotonic()
  reply = shingle.call('wayfire/set-keyboard-state', {'layout-index': 1})
  results.check('lock: a group change shows the other layout\'s name',
                reply.get('result') == 'ok' and xw.wait_for(
                    lambda: f'layout label Some("{LAYOUTS[1]}")'
                    in lock.client.output(), timeout=5)
                and reader.wait(lambda frame: frame.lock == 'locked'
                                and frame.mint > 0, moment=moment),
                f'{reply}; {checks.tail(lock.client.output())}')
  shingle.call('wayfire/set-keyboard-state', {'layout-index': 0})
  xw.wait_for(lambda: lock.client.output().count(
      f'layout label Some("{LAYOUTS[0]}")') >= 3, timeout=5)
  tap(shingle, 'KEY_ESC')

def grim_output(shingle, name):
  """A grim capture of output `name` as (info, width, height, RGB
  bytes), or None."""
  try:
    done = subprocess.run(['grim', '-o', name, '-t', 'ppm', '-'],
                          env=shingle.client_environment(),
                          capture_output=True, timeout=20,
                          stdin=subprocess.DEVNULL)
  except subprocess.TimeoutExpired:
    return None
  parsed = xw.parse_ppm(done.stdout) if done.returncode == 0 else None
  return ({}, *parsed) if parsed else None

def covered_point(point, image, target):
  """Where `point` of a capture of `image` size shows on an output of
  `target` size that it covers with its aspect ratio kept, ignoring the
  drift."""
  scale = max(target[0] / image[0], target[1] / image[1])
  return tuple(int((p - i / 2) * scale + t / 2)
               for p, i, t in zip(point, image, target))

def added_output_checks(results, shingle, lock, geometry):
  """An output created while locked shows HEADLESS-1's blurred capture,
  scaled to cover it, and never a client; it is removed again."""
  reply = shingle.call('wayfire/create-headless-output',
                       {'width': ADDED_SIZE[0], 'height': ADDED_SIZE[1]})
  name = (reply.get('output') or {}).get('name')
  results.check('lock: an output created over IPC while locked',
                reply.get('result') == 'ok' and name, str(reply))
  if not name:
    return
  shown = xw.wait_for(lambda: f'({name}) shows the capture of'
                      in lock.client.output(), timeout=10)
  of_first = re.compile(rf'\({re.escape(name)}\) shows the capture of '
                        r'output \d+ \(HEADLESS-1\)')
  results.check(f'lock: {name} shows the capture of HEADLESS-1',
                shown and of_first.search(lock.client.output()),
                checks.tail(lock.client.output()))
  shot = xw.wait_for(lambda: (lambda shot: shot if shot and not all_black(
      shot) else None)(grim_output(shingle, name)), timeout=10)
  if shot is not None and geometry:
    red = (geometry['x'] + geometry['width'] // 4,
           geometry['y'] + geometry['height'] // 4)
    x, y = covered_point(red, (WIDTH, HEIGHT), ADDED_SIZE)
    blurred = reddish(pixel(shot, x, y)) and not sharp_pattern(shot)
    detail = f'{pixel(shot, x, y)} at {(x, y)}'
  else:
    blurred, detail = False, 'no capture'
  results.check(f'lock: {name} shows it blurred, scaled to cover, not '
                'the clients', blurred, detail)
  reply = shingle.call('wayfire/destroy-headless-output', {'output': name})
  results.check(f'lock: {name} removed again', reply.get('result') == 'ok',
                str(reply))

def lock_up_time(reader, moment):
  """Seconds from `moment` until every output has presented a locked
  frame with its lock surface; None if one has not."""
  firsts = {}
  for frame in reader.since(moment):
    if frame.lock == 'locked' and frame.lock_surface:
      firsts.setdefault(frame.output, frame.arrived)
  if len(firsts) < len(OUTPUTS):
    return None
  return max(firsts.values()) - moment

def lock_binding(shingle):
  """Presses Shingle's lock binding, super+F8, over STIPC."""
  shingle.call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': True})
  tap(shingle, 'KEY_F8')
  shingle.call('stipc/feed_key', {'key': 'KEY_LEFTMETA', 'state': False})

def shingle_randr(shingle, *arguments):
  """Runs `wlr-randr` against `shingle`; returns (status, output)."""
  result = subprocess.run(['wlr-randr', *arguments],
                          env=shingle.client_environment(),
                          capture_output=True, text=True, timeout=10,
                          stdin=subprocess.DEVNULL)
  return result.returncode, result.stdout + result.stderr

CHECKS = {
    'lock': lock_checks,
}
