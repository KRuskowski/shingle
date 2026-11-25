#!/usr/bin/env python3
"""Sends one Wayfire IPC request to Shingle and prints the reply.

Usage:
  tools/native_test/ipc.py SOCKET METHOD [DATA]

SOCKET is the path given to `shingle --ipc`, METHOD an IPC method such as
`input/list-devices`, and DATA its JSON arguments (default `{}`). The reply
is printed as one line of JSON. Messages are a native-endian u32 length
followed by that many bytes of JSON, as Wayfire's IPC plugin frames them.

This only talks to a socket; it touches no device and is safe anywhere.
"""

import json
import socket
import struct
import sys

def encode_message(message):
  """Frames `message` (JSON-serialisable) for the IPC socket."""
  payload = json.dumps(message).encode()
  return struct.pack('=I', len(payload)) + payload

def decode_message(data):
  """Returns the first framed message in `data` and the bytes after it,
  or None and `data` if it is not complete yet."""
  if len(data) < 4:
    return None, data
  (length,) = struct.unpack('=I', data[:4])
  if len(data) < 4 + length:
    return None, data
  return json.loads(data[4:4 + length]), data[4 + length:]

def request(path, method, data, timeout=5.0):
  """Sends `method` with `data` to the socket at `path` and returns the
  decoded reply."""
  with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.settimeout(timeout)
    connection.connect(path)
    connection.sendall(encode_message({'method': method, 'data': data}))
    received = b''
    while True:
      reply, _ = decode_message(received)
      if reply is not None:
        return reply
      chunk = connection.recv(65536)
      if not chunk:
        raise ConnectionError('the socket closed before the reply')
      received += chunk

def main(argv):
  """Runs the command line in `argv` (without the program name)."""
  if len(argv) not in (2, 3):
    print(__doc__.strip(), file=sys.stderr)
    return 2
  data = json.loads(argv[2]) if len(argv) == 3 else {}
  print(json.dumps(request(argv[0], argv[1], data)))
  return 0

if __name__ == '__main__':
  sys.exit(main(sys.argv[1:]))
