#!/usr/bin/env python3
"""A FUSE passthrough file system whose fsync is slow.

  slowfs.py BACKING MOUNTPOINT --delay SECONDS --log FILE

Mirrors the directory BACKING at MOUNTPOINT and holds every fsync of a
file for SECONDS before syncing it, as a busy disk does. Directory
fsyncs are not implemented, which the kernel treats as success. Each fsync
appends `begin TIME PATH` and `end TIME PATH` (CLOCK_MONOTONIC seconds)
to FILE, which must lie outside the mount. It runs in the foreground
and single-threaded until it is unmounted.

It needs python3-fuse (libfuse 2) and is only run by
tools/session_test/run.py inside its user and mount namespace, where
the namespace's root may mount FUSE without fusermount.
"""

import argparse
import errno
import os
import sys
import time

import fuse

fuse.fuse_python_api = (0, 2)

def append_event(log_path, event, path):
  """Appends one fsync event line to `log_path`."""
  with open(log_path, 'a') as log:
    log.write(f'{event} {time.monotonic():.6f} {path}\n')

def make_file_class(backing, delay, log_path):
  """The per-open-file class, bound to the backing directory."""

  class SlowFile:
    """One open file of the mirror."""

    def __init__(self, path, flags, *mode):
      self.path = path
      self.fd = os.open(backing + path, flags, *mode)

    def read(self, length, offset):
      """Reads from the backing file."""
      return os.pread(self.fd, length, offset)

    def write(self, buffer, offset):
      """Writes to the backing file."""
      return os.pwrite(self.fd, buffer, offset)

    def fsync(self, datasync):
      """Waits `delay` seconds, then syncs the backing file."""
      append_event(log_path, 'begin', self.path)
      time.sleep(delay)
      if datasync:
        os.fdatasync(self.fd)
      else:
        os.fsync(self.fd)
      append_event(log_path, 'end', self.path)

    def flush(self):
      """Nothing is buffered here."""

    def release(self, flags):
      """Closes the backing file."""
      del flags
      os.close(self.fd)

    def fgetattr(self):
      """The backing file's attributes."""
      return os.fstat(self.fd)

    def ftruncate(self, length):
      """Truncates the backing file."""
      os.ftruncate(self.fd, length)

  return SlowFile

class SlowFs(fuse.Fuse):
  """The mirror of one backing directory."""

  def __init__(self, backing, delay, log_path, *args, **kwargs):
    super().__init__(*args, **kwargs)
    self.backing = backing
    self.file_class = make_file_class(backing, delay, log_path)

  def _real(self, path):
    return self.backing + path

  def getattr(self, path):
    """The backing entry's attributes."""
    return os.lstat(self._real(path))

  def readdir(self, path, offset):
    """The backing directory's entries."""
    del offset
    for name in ('.', '..', *os.listdir(self._real(path))):
      yield fuse.Direntry(name)

  def mkdir(self, path, mode):
    """Creates a backing directory."""
    os.mkdir(self._real(path), mode)

  def rmdir(self, path):
    """Removes a backing directory."""
    os.rmdir(self._real(path))

  def unlink(self, path):
    """Removes a backing file."""
    os.unlink(self._real(path))

  def rename(self, old, new):
    """Renames within the backing directory."""
    os.rename(self._real(old), self._real(new))

  def link(self, old, new):
    """Hard-links within the backing directory."""
    os.link(self._real(old), self._real(new))

  def chmod(self, path, mode):
    """Changes a backing entry's mode."""
    os.chmod(self._real(path), mode)

  def truncate(self, path, length):
    """Truncates a backing file by name."""
    os.truncate(self._real(path), length)

  def utimens(self, path, accessed, modified):
    """Sets a backing entry's times."""
    os.utime(self._real(path), ns=(
        accessed.tv_sec * 10**9 + accessed.tv_nsec,
        modified.tv_sec * 10**9 + modified.tv_nsec))

  def access(self, path, mode):
    """Checks access to a backing entry."""
    if not os.access(self._real(path), mode):
      return -errno.EACCES
    return 0

  def statfs(self):
    """The backing file system's statistics."""
    return os.statvfs(self.backing)

def main():
  """Mounts the mirror and serves it until it is unmounted."""
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument('backing')
  parser.add_argument('mountpoint')
  parser.add_argument('--delay', type=float, required=True)
  parser.add_argument('--log', required=True)
  options = parser.parse_args()
  backing = os.path.abspath(options.backing)
  server = SlowFs(backing, options.delay, os.path.abspath(options.log),
                  usage='slowfs')
  server.multithreaded = False
  server.parse([options.mountpoint, '-f'], errex=1)
  server.main()
  return 0

if __name__ == '__main__':
  sys.exit(main())
