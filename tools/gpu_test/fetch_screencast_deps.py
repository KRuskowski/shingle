#!/usr/bin/env python3
"""Extracts GStreamer's PipeWire plugin for the `screencast` checks.

screencast_checks.py reads the portal's stream with `pipewiresrc`, which
is in Debian's gstreamer1.0-pipewire package. This host does not have it
installed, so this downloads the package with `apt-get download`, as the
unprivileged user, and extracts it with `dpkg-deb -x` under
target/screencast-deps, where the checks load the plugin from through
GST_PLUGIN_PATH. Nothing is installed or changed outside target/. Run it
from the repository root, outside the namespace, which has no network.
"""

import glob
import os
import subprocess
import sys

PACKAGE = 'gstreamer1.0-pipewire'
DIRECTORY = os.path.join('target', 'screencast-deps')

def main():
  """Downloads and extracts the package unless it is there already."""
  root = os.path.join(DIRECTORY, 'root')
  plugin = glob.glob(os.path.join(root, 'usr', 'lib', '*', 'gstreamer-1.0',
                                  'libgstpipewire.so'))
  if plugin:
    print(f'fetch_screencast_deps: {plugin[0]} is there')
    return 0
  os.makedirs(DIRECTORY, exist_ok=True)
  subprocess.run(['apt-get', 'download', PACKAGE], cwd=DIRECTORY, check=True)
  packages = sorted(glob.glob(os.path.join(DIRECTORY, f'{PACKAGE}_*.deb')))
  if not packages:
    print('fetch_screencast_deps: the download left no package',
          file=sys.stderr)
    return 1
  subprocess.run(['dpkg-deb', '-x', packages[-1], root], check=True)
  print(f'fetch_screencast_deps: extracted {packages[-1]} into {root}')
  return 0

if __name__ == '__main__':
  sys.exit(main())
