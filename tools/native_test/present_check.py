#!/usr/bin/env python3
"""Checks presentation feedback and frame callbacks against page flips.

Usage:
  tools/native_test/present_check.py [--output NAME]
      [--scanout zero-copy|copy] [--tearing] [--vrr] NATIVE_LOG PROBE_OUTPUT

NATIVE_LOG is Shingle's native.log from a run with SHINGLE_TRACE_FLIPS=1,
which has a line `shingle: OUTPUT: page flip seq=S at SEC.NSEC s` for
every flip it presented (`shingle: page flip ...` before MO-7), ending
in ` async` for a flip that did not wait for the vblank (NB-10). With
several outputs lit, each CRTC counts its own vblanks, so `--output NAME`
takes only the flips of the output the probe is on. PROBE_OUTPUT is the
JSON-line output of the present_probe example
(crates/shingle-native/examples/present_probe.rs). The check passes when:

- every `presented` feedback names a logged flip by its sequence and its
  time is within 1 ms of that flip's;
- every frame callback's time is a logged flip's time in milliseconds, at
  or after the commit it answers, and callbacks never go back in time;
- presented sequences strictly increase and carry the flags
  vsync | hw_clock | hw_completion, or with `--tearing` (a probe run with
  `--tearing`, NB-10's async flips) hw_clock | hw_completion without
  vsync;
- with `--vrr`, every presented frame has a refresh of 0, as for an
  output with variable refresh (NB-10);
- with `--scanout zero-copy`, every presented frame also carries
  zero_copy, and with `--scanout copy` none does (NB-8's zero-copy
  scanout, and copy scanout as SHINGLE_SCANOUT=copy forces it).

It prints the number of flips, of presented and discarded feedback, of
frames presented as zero-copy, without vsync and with a refresh of 0,
the largest difference between a feedback
and its flip, and the median time between frame callbacks, which is one
refresh when every frame makes the next vblank.

It only reads two files and is safe anywhere.
"""

import argparse
import json
import re
import sys

FLIP = re.compile(
    r'(?:shingle: )?(?:(\S+): )?page flip seq=(\d+) at (\d+)\.(\d{9}) s')
# wp_presentation_feedback.kind: vsync, hw_clock and hw_completion.
VSYNC = 0x1
HARDWARE = 0x2 | 0x4
FLIP_FLAGS = VSYNC | HARDWARE
# wp_presentation_feedback.kind.zero_copy.
ZERO_COPY = 0x8
TOLERANCE_NS = 1_000_000

def parse_flips(lines, output=None):
  """Returns {sequence: time in ns} from native.log lines, only of the
  flips of `output` if it is given."""
  flips = {}
  for line in lines:
    match = FLIP.search(line)
    if not match:
      continue
    name, seq, sec, nsec = match.groups()
    if output is not None and name != output:
      continue
    flips[int(seq)] = int(sec) * 1_000_000_000 + int(nsec)
  return flips

def parse_probe(lines):
  """Returns the probe's events, one dict per JSON line."""
  events = []
  for line in lines:
    line = line.strip()
    if line.startswith('{'):
      events.append(json.loads(line))
  return events

def check(flips, events, scanout=None, tearing=False, vrr=False):
  """Returns (problems, statistics) for the probe's events. `scanout` is
  'zero-copy' or 'copy' to require every presented frame to carry the
  zero_copy flag or none to, or None to accept either. With `tearing`
  every presented frame from the first async flip on must be an async
  flip, without vsync, and with `vrr` have a refresh of 0.

  An async flip completes within the current vblank, and the kernel
  reports that vblank's sequence and timestamp for it: frames flipped in
  one vblank share a sequence, and their callbacks can carry a time before
  their commit. With `tearing` neither is a problem. The frames before the
  first async flip went with the vblank; i915 on the Latitude 5450 (Meteor
  Lake) flipped the first two of a probe run that way (2026-11-28), so they
  are counted as `sync_before_async` instead of failing."""
  problems = []
  commits = {}
  flip_ms = {(time // 1_000_000) & 0xffffffff for time in flips.values()}
  last_frame = None
  last_seq = None
  differences = []
  async_seen = False
  sync_before_async = 0
  for event in events:
    kind = event.get('event')
    n = event.get('n')
    if kind == 'commit':
      commits[n] = event['monotonic_ns']
    elif kind == 'frame':
      time = event['time_ms']
      if time not in flip_ms:
        problems.append(f'frame {n}: callback at {time} ms is no flip time')
      if (not tearing and n in commits
          and time < (commits[n] // 1_000_000) & 0xffffffff):
        problems.append(f'frame {n}: callback before its commit')
      if last_frame is not None and time < last_frame:
        problems.append(f'frame {n}: callback went back in time')
      last_frame = time
    elif kind == 'presented':
      seq = event['seq']
      time = event['sec'] * 1_000_000_000 + event['nsec']
      if seq not in flips:
        problems.append(f'frame {n}: presented with seq {seq}, no such flip')
      else:
        difference = abs(time - flips[seq])
        differences.append(difference)
        if difference > TOLERANCE_NS:
          problems.append(
              f'frame {n}: presented {difference} ns from its flip')
      if last_seq is not None and (seq < last_seq
                                   or (seq == last_seq and not tearing)):
        problems.append(f'frame {n}: seq {seq} after {last_seq}')
      last_seq = seq
      flags = event['flags']
      if tearing:
        torn = flags & HARDWARE == HARDWARE and not flags & VSYNC
        if torn:
          async_seen = True
        elif not async_seen:
          sync_before_async += 1
        else:
          problems.append(f'frame {n}: flags {flags:#x}, not an async flip')
      elif flags & FLIP_FLAGS != FLIP_FLAGS:
        problems.append(f'frame {n}: flags {flags:#x}')
      if vrr and event['refresh'] != 0:
        problems.append(
            f'frame {n}: refresh {event["refresh"]}, not 0 for variable '
            'refresh')
      zero_copy = bool(event['flags'] & ZERO_COPY)
      if scanout == 'zero-copy' and not zero_copy:
        problems.append(f'frame {n}: not presented as zero-copy')
      if scanout == 'copy' and zero_copy:
        problems.append(f'frame {n}: presented as zero-copy')
  frame_times = [e['time_ms'] for e in events if e.get('event') == 'frame']
  intervals = sorted(
      later - earlier for earlier, later in zip(frame_times, frame_times[1:]))
  median = intervals[len(intervals) // 2] if intervals else None
  statistics = {
      'flips': len(flips),
      'presented': sum(1 for e in events if e.get('event') == 'presented'),
      'discarded': sum(1 for e in events if e.get('event') == 'discarded'),
      'zero_copy': sum(1 for e in events if e.get('event') == 'presented'
                       and e['flags'] & ZERO_COPY),
      'tearing': sum(1 for e in events if e.get('event') == 'presented'
                     and not e['flags'] & VSYNC),
      'sync_before_async': sync_before_async,
      'variable_refresh': sum(1 for e in events
                              if e.get('event') == 'presented'
                              and e['refresh'] == 0),
      'max_difference_ns': max(differences, default=0),
      'median_frame_interval_ms': median,
  }
  if not statistics['presented']:
    problems.append('no presented feedback')
  return problems, statistics

def parse_args(argv):
  """Returns the parsed arguments, or None after printing the usage."""
  parser = argparse.ArgumentParser(prog='present_check.py')
  parser.add_argument('--output', help='check only this output\'s flips')
  parser.add_argument('--scanout', choices=('zero-copy', 'copy'),
                      help='require every frame to be presented this way')
  parser.add_argument('--tearing', action='store_true',
                      help='require every frame to be an async flip')
  parser.add_argument('--vrr', action='store_true',
                      help='require a refresh of 0 in every frame')
  parser.add_argument('native_log')
  parser.add_argument('probe_output')
  try:
    return parser.parse_args(argv)
  except SystemExit:
    return None

def main(argv):
  """Runs the check on the files in `argv`; returns the exit status."""
  args = parse_args(argv)
  if args is None:
    return 2
  with open(args.native_log, encoding='utf-8', errors='replace') as log:
    flips = parse_flips(log, args.output)
  with open(args.probe_output, encoding='utf-8') as probe:
    events = parse_probe(probe)
  problems, statistics = check(flips, events, args.scanout, args.tearing,
                               args.vrr)
  print(json.dumps(statistics))
  for problem in problems:
    print(problem)
  return 1 if problems else 0

if __name__ == '__main__':
  sys.exit(main(sys.argv[1:]))
