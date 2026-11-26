#!/usr/bin/env python3
"""Checks presentation feedback and frame callbacks against page flips.

Usage:
  tools/native_test/present_check.py [--output NAME] NATIVE_LOG PROBE_OUTPUT

NATIVE_LOG is Shingle's native.log from a run with SHINGLE_TRACE_FLIPS=1,
which has a line `shingle: OUTPUT: page flip seq=S at SEC.NSEC s` for
every flip it presented (`shingle: page flip ...` before MO-7). With
several outputs lit, each CRTC counts its own vblanks, so `--output NAME`
takes only the flips of the output the probe is on. PROBE_OUTPUT is the
JSON-line output of the present_probe example
(crates/shingle-native/examples/present_probe.rs). The check passes when:

- every `presented` feedback names a logged flip by its sequence and its
  time is within 1 ms of that flip's;
- every frame callback's time is a logged flip's time in milliseconds, at
  or after the commit it answers, and callbacks never go back in time;
- presented sequences strictly increase and carry the flags
  vsync | hw_clock | hw_completion.

It prints the number of flips and of presented and discarded feedback,
the largest difference between a feedback and its flip, and the median
time between frame callbacks, which is one refresh when every frame
makes the next vblank.

It only reads two files and is safe anywhere.
"""

import json
import re
import sys

FLIP = re.compile(
    r'(?:shingle: )?(?:(\S+): )?page flip seq=(\d+) at (\d+)\.(\d{9}) s')
# wp_presentation_feedback.kind: vsync, hw_clock and hw_completion.
FLIP_FLAGS = 0x1 | 0x2 | 0x4
TOLERANCE_NS = 1_000_000
USAGE = ('usage: present_check.py [--output NAME] NATIVE_LOG '
         'PROBE_OUTPUT')

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

def check(flips, events):
  """Returns (problems, statistics) for the probe's events."""
  problems = []
  commits = {}
  flip_ms = {(time // 1_000_000) & 0xffffffff for time in flips.values()}
  last_frame = None
  last_seq = None
  differences = []
  for event in events:
    kind = event.get('event')
    n = event.get('n')
    if kind == 'commit':
      commits[n] = event['monotonic_ns']
    elif kind == 'frame':
      time = event['time_ms']
      if time not in flip_ms:
        problems.append(f'frame {n}: callback at {time} ms is no flip time')
      if n in commits and time < (commits[n] // 1_000_000) & 0xffffffff:
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
      if last_seq is not None and seq <= last_seq:
        problems.append(f'frame {n}: seq {seq} after {last_seq}')
      last_seq = seq
      if event['flags'] & FLIP_FLAGS != FLIP_FLAGS:
        problems.append(f'frame {n}: flags {event["flags"]:#x}')
  frame_times = [e['time_ms'] for e in events if e.get('event') == 'frame']
  intervals = sorted(
      later - earlier for earlier, later in zip(frame_times, frame_times[1:]))
  median = intervals[len(intervals) // 2] if intervals else None
  statistics = {
      'flips': len(flips),
      'presented': sum(1 for e in events if e.get('event') == 'presented'),
      'discarded': sum(1 for e in events if e.get('event') == 'discarded'),
      'max_difference_ns': max(differences, default=0),
      'median_frame_interval_ms': median,
  }
  if not statistics['presented']:
    problems.append('no presented feedback')
  return problems, statistics

def main(argv):
  """Runs the check on the files in `argv`; returns the exit status."""
  output = None
  if len(argv) == 4 and argv[0] == '--output':
    output, argv = argv[1], argv[2:]
  if len(argv) != 2:
    print(USAGE, file=sys.stderr)
    return 2
  with open(argv[0], encoding='utf-8', errors='replace') as log:
    flips = parse_flips(log, output)
  with open(argv[1], encoding='utf-8') as probe:
    events = parse_probe(probe)
  problems, statistics = check(flips, events)
  print(json.dumps(statistics))
  for problem in problems:
    print(problem)
  return 1 if problems else 0

if __name__ == '__main__':
  sys.exit(main(sys.argv[1:]))
