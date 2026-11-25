#!/usr/bin/env python3
"""Virtual input devices for the native test plan (docs/native-testing.md).

ONLY FOR THE DEDICATED TEST MACHINE. A uinput device is a real input
device to every compositor on the machine: run anywhere else, it types
into and moves the pointer of whatever session is there. The script
therefore refuses to run unless /etc/shingle-native-test-machine exists,
a file the operator creates on the test machine only.

Usage (as a user that may write /dev/uinput, usually with sudo):
  uinput.py device KIND [--seconds N]
      Create a KIND device (keyboard, mouse or touch) and keep it for N
      seconds (default 10), for hotplug checks.
  uinput.py key KEY... [--hold SECONDS]
      Press KEYs in order (KEY_LEFTCTRL KEY_LEFTALT KEY_F2 is a chord),
      hold them, then release them in reverse order.
  uinput.py motion DX DY
  uinput.py button BUTTON [--hold SECONDS]
  uinput.py wheel CLICKS
  uinput.py touch X Y [--hold SECONDS]
      X and Y are fractions of the touchscreen, from 0 to 1.

Each command creates its device, waits --settle seconds (default 1) for
libinput to add it, sends the events and prints a line per event with its
CLOCK_MONOTONIC time in milliseconds, the clock libinput stamps events
with, so client timestamps can be compared with it.

Requires python3-evdev.
"""

import argparse
import os
import sys
import time

MARKER = '/etc/shingle-native-test-machine'
VENDOR = 0x1234
PRODUCT = 0x5678
NAMES = {
    'keyboard': 'shingle-test keyboard',
    'mouse': 'shingle-test mouse',
    'touch': 'shingle-test touch',
}
TOUCH_MAX = 4095

def on_test_machine(marker=MARKER):
  """Whether the marker file of the dedicated test machine exists."""
  return os.path.exists(marker)

def monotonic_ms():
  """The CLOCK_MONOTONIC time in milliseconds."""
  return time.clock_gettime(time.CLOCK_MONOTONIC) * 1000.0

def touch_position(fraction):
  """Maps a fraction of the touchscreen to its absolute axis value."""
  if not 0.0 <= fraction <= 1.0:
    raise ValueError(f'{fraction} is not between 0 and 1')
  return round(fraction * TOUCH_MAX)

def parse_arguments(argv):
  """Parses the command line in `argv` (without the program name)."""
  parser = argparse.ArgumentParser(
      description='Virtual input devices for the native test plan.')
  parser.add_argument('--settle', type=float, default=1.0,
                      help='seconds to wait for libinput to add the device')
  commands = parser.add_subparsers(dest='command', required=True)
  device = commands.add_parser('device')
  device.add_argument('kind', choices=sorted(NAMES))
  device.add_argument('--seconds', type=float, default=10.0)
  key = commands.add_parser('key')
  key.add_argument('keys', nargs='+')
  key.add_argument('--hold', type=float, default=0.05)
  motion = commands.add_parser('motion')
  motion.add_argument('dx', type=int)
  motion.add_argument('dy', type=int)
  button = commands.add_parser('button')
  button.add_argument('button')
  button.add_argument('--hold', type=float, default=0.05)
  wheel = commands.add_parser('wheel')
  wheel.add_argument('clicks', type=int)
  touch = commands.add_parser('touch')
  touch.add_argument('x', type=float)
  touch.add_argument('y', type=float)
  touch.add_argument('--hold', type=float, default=0.05)
  return parser.parse_args(argv)

def device_kind(command):
  """The kind of device a command needs."""
  return {
      'key': 'keyboard',
      'motion': 'mouse',
      'button': 'mouse',
      'wheel': 'mouse',
      'touch': 'touch',
  }[command]

def create(kind):
  """Creates a uinput device of `kind`."""
  # Imported in the functions that need it, so that the pure helpers
  # work without python3-evdev.
  from evdev import AbsInfo, UInput, ecodes
  if kind == 'keyboard':
    keys = [code for code in ecodes.KEY.keys()
            if isinstance(code, int) and 0 < code < ecodes.BTN_MISC]
    capabilities = {ecodes.EV_KEY: keys}
    properties = []
  elif kind == 'mouse':
    capabilities = {
        ecodes.EV_KEY: [ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_MIDDLE],
        ecodes.EV_REL: [ecodes.REL_X, ecodes.REL_Y, ecodes.REL_WHEEL],
    }
    properties = []
  else:
    axis = AbsInfo(value=0, min=0, max=TOUCH_MAX, fuzz=0, flat=0,
                   resolution=10)
    capabilities = {
        ecodes.EV_KEY: [ecodes.BTN_TOUCH],
        ecodes.EV_ABS: [
            (ecodes.ABS_X, axis),
            (ecodes.ABS_Y, axis),
            (ecodes.ABS_MT_SLOT, AbsInfo(0, 0, 9, 0, 0, 0)),
            (ecodes.ABS_MT_TRACKING_ID, AbsInfo(0, 0, 65535, 0, 0, 0)),
            (ecodes.ABS_MT_POSITION_X, axis),
            (ecodes.ABS_MT_POSITION_Y, axis),
        ],
    }
    properties = [ecodes.INPUT_PROP_DIRECT]
  return UInput(capabilities, name=NAMES[kind], vendor=VENDOR,
                product=PRODUCT, input_props=properties)

def code(name):
  """The evdev code of a key or button name such as KEY_A or BTN_LEFT."""
  from evdev import ecodes
  value = ecodes.ecodes.get(name)
  if value is None:
    raise SystemExit(f'uinput.py: unknown key or button {name}')
  return value

def report(what):
  """Prints one event line with its time."""
  print(f'{what} at {monotonic_ms():.3f} ms', flush=True)

def run(arguments, device):
  """Sends the events of `arguments` through `device`."""
  from evdev import ecodes
  if arguments.command == 'device':
    report(f'{NAMES[arguments.kind]} present')
    time.sleep(arguments.seconds)
    report(f'{NAMES[arguments.kind]} removed')
  elif arguments.command == 'key':
    codes = [code(name) for name in arguments.keys]
    for name, value in zip(arguments.keys, codes):
      device.write(ecodes.EV_KEY, value, 1)
      device.syn()
      report(f'{name} ({value}) pressed')
    time.sleep(arguments.hold)
    for name, value in reversed(list(zip(arguments.keys, codes))):
      device.write(ecodes.EV_KEY, value, 0)
      device.syn()
      report(f'{name} ({value}) released')
  elif arguments.command == 'motion':
    device.write(ecodes.EV_REL, ecodes.REL_X, arguments.dx)
    device.write(ecodes.EV_REL, ecodes.REL_Y, arguments.dy)
    device.syn()
    report(f'motion {arguments.dx} {arguments.dy}')
  elif arguments.command == 'button':
    value = code(arguments.button)
    device.write(ecodes.EV_KEY, value, 1)
    device.syn()
    report(f'{arguments.button} ({value}) pressed')
    time.sleep(arguments.hold)
    device.write(ecodes.EV_KEY, value, 0)
    device.syn()
    report(f'{arguments.button} ({value}) released')
  elif arguments.command == 'wheel':
    device.write(ecodes.EV_REL, ecodes.REL_WHEEL, arguments.clicks)
    device.syn()
    report(f'wheel {arguments.clicks}')
  elif arguments.command == 'touch':
    x = touch_position(arguments.x)
    y = touch_position(arguments.y)
    for axis, value in [(ecodes.ABS_MT_SLOT, 0),
                        (ecodes.ABS_MT_TRACKING_ID, 1),
                        (ecodes.ABS_MT_POSITION_X, x),
                        (ecodes.ABS_MT_POSITION_Y, y),
                        (ecodes.ABS_X, x), (ecodes.ABS_Y, y)]:
      device.write(ecodes.EV_ABS, axis, value)
    device.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 1)
    device.syn()
    report(f'touch down {x} {y}')
    time.sleep(arguments.hold)
    device.write(ecodes.EV_ABS, ecodes.ABS_MT_TRACKING_ID, -1)
    device.write(ecodes.EV_KEY, ecodes.BTN_TOUCH, 0)
    device.syn()
    report('touch up')

def main(argv):
  """Runs the command line in `argv` (without the program name)."""
  arguments = parse_arguments(argv)
  if not on_test_machine():
    print(f'uinput.py: {MARKER} does not exist; this is not the native '
          'test machine, so no device is created', file=sys.stderr)
    return 1
  kind = arguments.kind if arguments.command == 'device' else device_kind(
      arguments.command)
  device = create(kind)
  try:
    time.sleep(arguments.settle)
    run(arguments, device)
    # Let libinput read the last events before the device goes.
    time.sleep(0.2)
  finally:
    device.close()
  return 0

if __name__ == '__main__':
  sys.exit(main(sys.argv[1:]))
