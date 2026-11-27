"""Runs commands on the native test machine over SSH, and refuses others.

bringup.py drives the test machine from the development host through a
`Target`: `run` executes a POSIX sh script there, `put` and `get` copy
files. `SshTarget` does that with the operator's own `ssh` and `scp`, so
the host, user, key and jump hosts all come from ~/.ssh/config; nothing
here stores or asks for a credential (BatchMode makes ssh fail instead of
prompting). Every process goes through an injected runner, so the unit
tests replace SSH with a fake.

`guard` is the safety check every subcommand passes before it touches the
target: it refuses localhost, this host's names and addresses, and a
target whose /etc/machine-id or hostname is this host's, and for device
steps it requires /etc/shingle-native-test-machine on the target.
"""

import dataclasses
import ipaddress
import shlex
import socket
import subprocess

MARKER = '/etc/shingle-native-test-machine'
LOCAL_NAMES = ('localhost', 'localhost.localdomain', 'ip6-localhost',
               'ip6-loopback')
SSH_OPTIONS = ('-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10')

@dataclasses.dataclass
class Result:
  """The outcome of one command."""

  returncode: int
  stdout: str = ''
  stderr: str = ''

  @property
  def ok(self):
    """Whether the command exited with status 0."""
    return self.returncode == 0

class Refused(Exception):
  """The target is not the test machine, or not ready for this command."""

def run_local(argv, timeout=None, cwd=None, env=None):
  """Runs `argv` on this host and returns its `Result`; the default
  runner of every `Target` and of the local build and checks."""
  try:
    completed = subprocess.run(argv, capture_output=True, text=True,
                               timeout=timeout, cwd=cwd, env=env,
                               check=False, errors='replace')
  except subprocess.TimeoutExpired as error:
    return Result(124, error.stdout or '', f'timed out after {timeout} s')
  except OSError as error:
    return Result(127, '', str(error))
  return Result(completed.returncode, completed.stdout, completed.stderr)

class SshTarget:
  """The test machine reached through `ssh HOST`.

  Remote paths that do not start with `/` are relative to the remote
  user's home directory, as for scp.
  """

  def __init__(self, host, runner=run_local, ssh_options=()):
    self.host = host
    self.runner = runner
    self.options = list(SSH_OPTIONS) + list(ssh_options)

  def run(self, script, timeout=60):
    """Runs `script` with `sh -c` in the remote home directory."""
    command = 'sh -c ' + shlex.quote(script)
    return self.runner(['ssh', *self.options, self.host, '--', command],
                       timeout=timeout)

  def put(self, local, remote, timeout=600):
    """Copies the local file `local` to `remote` on the target."""
    return self.runner(['scp', '-q', '-p', *self.options, local,
                        f'{self.host}:{remote}'], timeout=timeout)

  def get(self, remote, local, timeout=600):
    """Copies `remote` from the target to the local path `local`."""
    return self.runner(['scp', '-q', '-p', *self.options,
                        f'{self.host}:{remote}', local], timeout=timeout)

@dataclasses.dataclass
class LocalHost:
  """What identifies the development host this tool runs on."""

  hostname: str
  machine_id: str
  addresses: frozenset

def this_host(machine_id_path='/etc/machine-id'):
  """Describes the host this process runs on."""
  hostname = socket.gethostname()
  try:
    with open(machine_id_path, encoding='utf-8') as file:
      machine_id = file.read().strip()
  except OSError:
    machine_id = ''
  addresses = set()
  for name in {hostname, socket.getfqdn()}:
    try:
      for info in socket.getaddrinfo(name, None):
        addresses.add(info[4][0])
    except OSError:
      pass
  return LocalHost(hostname, machine_id, frozenset(addresses))

def ssh_hostname(host, runner=run_local):
  """The host name ssh would connect to for `host`, after ~/.ssh/config
  (`ssh -G` evaluates the config and connects nowhere)."""
  result = runner(['ssh', '-G', host], timeout=10)
  if result.ok:
    for line in result.stdout.splitlines():
      key, _, value = line.partition(' ')
      if key == 'hostname' and value:
        return value.strip()
  return host.rsplit('@', 1)[-1]

def is_loopback(name):
  """Whether `name` is an IP address of the loopback or unspecified
  range."""
  try:
    address = ipaddress.ip_address(name.strip('[]'))
  except ValueError:
    return False
  return address.is_loopback or address.is_unspecified

def local_name_problem(host, resolved, local):
  """Why `host` (resolving to `resolved`) names this host, or None."""
  for name in {host.rsplit('@', 1)[-1], resolved}:
    lowered = name.lower()
    short = local.hostname.lower().split('.')[0]
    if lowered in LOCAL_NAMES or is_loopback(lowered):
      return f'{name} is this host (loopback)'
    if lowered == local.hostname.lower() or lowered.split('.')[0] == short:
      return f'{name} is this host\'s name'
    if name in local.addresses:
      return f'{name} is an address of this host'
  return None

@dataclasses.dataclass
class TargetInfo:
  """What the guard learnt about the target."""

  hostname: str
  machine_id: str
  user: str
  marker: bool

IDENTIFY = ('hostname; cat /etc/machine-id 2>/dev/null || echo; id -un; '
            f'if [ -e {MARKER} ]; then echo marker; else echo no-marker; fi')

def identify(target):
  """Reads the target's hostname, machine id, user and marker."""
  result = target.run(IDENTIFY, timeout=30)
  if not result.ok:
    raise Refused(f'cannot reach {target.host} over SSH: '
                  f'{result.stderr.strip() or result.returncode}')
  lines = result.stdout.splitlines()
  if len(lines) < 4:
    raise Refused(f'unexpected identification from {target.host}: '
                  f'{result.stdout!r}')
  hostname, machine_id, user, marker = (line.strip() for line in lines[-4:])
  return TargetInfo(hostname, machine_id, user, marker == 'marker')

def guard(target, local, require_marker, runner=run_local):
  """Returns the target's `TargetInfo`, or raises `Refused` when the
  target is this host or, with `require_marker`, lacks the marker of the
  dedicated test machine. Nothing is run on the target before the
  name check has passed."""
  problem = local_name_problem(target.host, ssh_hostname(target.host, runner),
                               local)
  if problem:
    raise Refused(f'refusing {target.host}: {problem}; bringup.py only '
                  'drives the dedicated test machine')
  info = identify(target)
  if local.machine_id and info.machine_id == local.machine_id:
    raise Refused(f'refusing {target.host}: its /etc/machine-id is this '
                  'host\'s')
  if info.hostname.lower() == local.hostname.lower():
    raise Refused(f'refusing {target.host}: its hostname {info.hostname} '
                  'is this host\'s')
  if require_marker and not info.marker:
    raise Refused(f'refusing {target.host}: {MARKER} does not exist there; '
                  f'create it on the test machine only (sudo touch {MARKER})')
  return info
