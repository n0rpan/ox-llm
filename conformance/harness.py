"""Process harness shared by implementation-independent black-box contracts."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import shlex
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
PRIMARY = 'ornith-35b-a3b-local'
SECONDARY = 'qwen3.6-35b-a3b-local'


def isolated_env(home):
    env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
           'LC_ALL': 'C.UTF-8', 'TERM': 'dumb', 'COLUMNS': '80',
           'PYTHONPATH': str(ROOT / 'lab/src')}
    for key, subdir in (('XDG_CONFIG_HOME', 'config'), ('XDG_STATE_HOME', 'state'),
                        ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache'),
                        ('XDG_RUNTIME_DIR', 'run')):
        path = home / subdir
        path.mkdir(mode=0o700, exist_ok=True)
        env[key] = str(path)
    return env


def free_port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def sse_frames(payload):
    frames = []
    for block in payload.replace('\r\n', '\n').split('\n\n'):
        data = '\n'.join(line[5:].lstrip(' ') for line in block.split('\n') if line.startswith('data:'))
        if not data:
            continue
        value = data if data == '[DONE]' else json.loads(data)
        events = [line[6:].strip() for line in block.split('\n') if line.startswith('event:')]
        frames.append({'event': events[-1], 'data': value} if events else value)
    return frames


def normalize_completion(body):
    """Validate and mask only the controller's random completion metadata."""
    rows = body if isinstance(body, list) else [body]
    completion_id = None
    for row in rows:
        if not isinstance(row, dict) or row.get('object') != 'text_completion':
            continue
        value = row.get('id')
        if not isinstance(value, str) or not re.fullmatch(r'cmpl-[0-9a-f]{24}', value):
            raise AssertionError('Invalid generated completion ID: ' + repr(value))
        if completion_id is not None and completion_id != value:
            raise AssertionError('Completion stream changed IDs')
        completion_id = value
        if type(row.get('created')) is not int or abs(row['created'] - time.time()) > 30:
            raise AssertionError('Invalid completion timestamp')
        row['id'], row['created'] = '<generated:cmpl_24hex>', '<current:unix-seconds>'
    return body


def normalize_responses(body):
    ids = {}
    counts = {}

    def visit(value):
        if isinstance(value, list):
            return [visit(item) for item in value]
        if not isinstance(value, dict):
            return value
        out = {}
        for key, item in value.items():
            if key in ('id', 'item_id', 'response_id') and isinstance(item, str) and item.startswith(('resp_', 'msg_', 'rs_', 'fc_')):
                if not re.fullmatch(r'(resp|msg|rs|fc)_[0-9a-f]{24}', item):
                    raise AssertionError('Invalid Responses ID: ' + item)
                prefix = item.split('_', 1)[0]
                if item not in ids:
                    index = counts.get(prefix, 0)
                    counts[prefix] = index + 1
                    ids[item] = '<generated:' + prefix + '_' + str(index) + '>'
                out[key] = ids[item]
            elif key in ('created_at', 'completed_at'):
                if type(item) is not int or abs(item - time.time()) > 30:
                    raise AssertionError('Invalid Responses timestamp: ' + repr(item))
                out[key] = '<current:unix-seconds>'
            else:
                out[key] = visit(item)
        return out
    return visit(body)


class Daemon:
    def __init__(self, configured=False, environ=None, config_changes=None, port=None, home=None,
                 engine_free=False):
        if configured and engine_free:
            raise ValueError('An engine-free fixture cannot install a local engine')
        self.configured = configured
        self.engine_free = engine_free
        self.environ = environ or {}
        self.config_changes = config_changes or {}
        self.request_ids = set()
        self.fixed_port, self.fixed_home = port, home

    def __enter__(self):
        # Match the installed service and keep fixture modes independent of the
        # runner's shell umask. Restore the caller's setting on every exit path.
        self.previous_umask = os.umask(0o077)
        try:
            return self.enter_private()
        except BaseException:
            os.umask(self.previous_umask)
            raise

    def enter_private(self):
        self.temp = None if self.fixed_home else tempfile.TemporaryDirectory(prefix='oxc-http-conformance-')
        self.home = home = Path(self.fixed_home or self.temp.name)
        ports = set()
        while len(ports) < 3:
            ports.add(free_port())
        self.port, self.backend_port, flash_port = sorted(ports)
        if self.fixed_port is not None:
            self.port = self.fixed_port
        self.env = isolated_env(home)
        self.env.update({'LOCAL_LLM_PORT': str(self.port), 'LOCAL_LLM_BACKEND_PORT': str(self.backend_port),
                        'LOCAL_FLASH_BACKEND_PORT': str(flash_port),
                        'LOCAL_LLM_CONFIG': str(home / 'config/local-ai/llm/runtime.json'),
                        'LOCAL_AI_LEASE': str(home / 'state/local-ai/inference.lock'),
                        'LOCAL_AI_ENABLE_OXE': '1', 'LOCAL_LLM_START_SECONDS': '5',
                        'LOCAL_LLM_QUEUE_WAIT_SECONDS': '2',
                        'FAKE_ENGINE_EVENTS': str(home / 'engine-events.jsonl'),
                        'FAKE_ENGINE_GATE': str(home / 'engine-gate'),
                        'FAKE_PROC_VIEW': str(home / 'host/proc')})
        self.env.update(self.environ)
        self.config_path = Path(self.env['LOCAL_LLM_CONFIG'])
        if self.configured:
            self.install_fixture()
        elif self.engine_free:
            # Cloud-only contracts must run without fake models, engine binaries,
            # host-memory fixtures or Snapdragon-specific namespaces. The remote
            # catalog is supplied by the caller after its Python baseline lands.
            self.config_path.parent.mkdir(parents=True, mode=0o700)
            self.config_path.write_text(json.dumps(self.config_changes))
            self.config_path.chmod(0o600)
        command = shlex.split(os.environ['OXC_DAEMON_COMMAND']) if 'OXC_DAEMON_COMMAND' in os.environ else [
            sys.executable, '-m', 'snapdragon_ai_lab.runtime.controller']
        self.namespace = []
        if self.configured:
            self.namespace = self.host_fixture([])
        self.log = (home / 'daemon.log').open('w+')
        self.start(command)
        return self

    def start(self, command):
        self.child_info = self.home / 'bwrap-info.json'
        with self.child_info.open('w') as info:
            wrapped = [*self.namespace[:-1], '--info-fd', str(info.fileno()), '--', *command] if self.namespace else command
            self.process = subprocess.Popen(wrapped, cwd=self.home, env=self.env,
                                            stdout=self.log, stderr=self.log, pass_fds=(info.fileno(),))
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    break
                try:
                    self.call('GET', '/v1/models')
                    return
                except (OSError, http.client.HTTPException):
                    time.sleep(0.02)
            self.log.seek(0)
            raise AssertionError('Daemon failed to start: ' + self.log.read())
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def stop(self):
        if self.process.poll() is None:
            if self.namespace:
                # Signal the actual daemon, so its SIGTERM handler performs the
                # same engine/lease/key teardown as a systemd service stop.
                try:
                    child_pid = json.loads(self.child_info.read_text())['child-pid']
                    os.kill(child_pid, 15)
                except (OSError, ValueError, KeyError):
                    self.process.terminate()
            else:
                self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def restart(self, reference=False):
        self.stop()
        if reference or 'OXC_DAEMON_COMMAND' not in os.environ:
            command = [sys.executable, '-m', 'snapdragon_ai_lab.runtime.controller']
        else:
            command = shlex.split(os.environ['OXC_DAEMON_COMMAND'])
        self.start(command)

    def install_fixture(self):
        # A regular owned executable, hash-pinned exactly as an installed engine is.
        binary = self.home / 'fake-engine'
        source = (ROOT / 'conformance/fixtures/fake_engine.py').read_text()
        # Real controller capability probes inspect usage strings in the pinned
        # executable. Only opt-in fixtures advertise the mock cache routes.
        if self.environ.get('FAKE_ENGINE_STATE_CACHE') == '1':
            source += '\n# supported flags: --state-cache --state-budget-bytes --state-ttl-secs \n'
        if self.environ.get('FAKE_ENGINE_STATE_PRELOAD') == '1':
            source += '\n# oxe-capability state-preload-v1\n'
        binary.write_text('#!' + sys.executable + '\n' + source.split('\n', 1)[1])
        binary.chmod(0o700)
        model = self.home / 'synthetic.gguf'
        model.write_bytes(b'synthetic-model')
        profile = {'ready': True, 'backend_kind': 'oxe', 'model_path': str(model),
                   'sha256': hashlib.sha256(model.read_bytes()).hexdigest(),
                   'expected_bytes': model.stat().st_size, 'server_binary': str(binary),
                   'server_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                   'context_size': 32768, 'threads': 1, 'max_output_tokens': 4,
                   'oxe_args': ['--model', '{model}', '--alias', '{alias}']}
        config = {**profile, 'model_alias': PRIMARY, 'selected_model': PRIMARY,
                  'profiles': {PRIMARY: profile, SECONDARY: {**profile}},
                  **self.config_changes}
        self.config_path.parent.mkdir(parents=True, mode=0o700)
        self.config_path.write_text(json.dumps(config))
        self.config_path.chmod(0o600)

    def events(self):
        path = Path(self.env['FAKE_ENGINE_EVENTS'])
        if not path.exists():
            return []
        # Ignore an unfinished final line while the child is writing it.
        return [json.loads(line) for line in path.read_text().splitlines(keepends=True) if line.endswith('\n')]

    def host_fixture(self, command):
        # A private mount/cgroup namespace supplies the installed service's host
        # contract without changing the runner's cgroup or patching Python. The
        # real memory guard still reads and enforces every supplied counter.
        telemetry = self.home / 'host'
        telemetry.mkdir()
        cgroup = telemetry / 'cgroup'
        cgroup.mkdir()
        for name, value in {
            'memory.swap.max': '0', 'memory.swap.current': '0',
            'memory.max': str(16 * 1024**3), 'memory.high': 'max',
            'memory.events': 'low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0',
            'pids.events': 'max 0',
        }.items():
            (cgroup / name).write_text(value + '\n')
        proc = telemetry / 'proc'
        proc.mkdir()
        for entry in Path('/proc').iterdir():
            if entry.name not in ('meminfo', 'pressure', 'vmstat'):
                (proc / entry.name).symlink_to('/run/oxc-conformance-proc/' + entry.name)
        (proc / 'meminfo').write_text('MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n'
                                         'SwapTotal: 0 kB\nSwapFree: 0 kB\n')
        (proc / 'pressure').mkdir()
        (proc / 'pressure/memory').write_text('some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n'
                                          'full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n')
        (proc / 'vmstat').write_text('oom_kill 0\npswpin 0\npswpout 0\n')
        return ['bwrap', '--die-with-parent', '--unshare-cgroup', '--ro-bind', '/', '/',
                '--dev-bind', '/dev', '/dev',
                # Host root is unmapped in the executor's user namespace. A
                # private, owned ancestor lets the unchanged diagnostic writer
                # enforce its directory-owner checks instead of refusing /tmp.
                '--tmpfs', '/tmp', '--chmod', '0700', '/tmp',
                '--bind', str(self.home), str(self.home),
                '--tmpfs', '/run', '--ro-bind', '/proc', '/run/oxc-conformance-proc', '--bind', str(proc), '/proc',
                '--ro-bind', str(cgroup), '/sys/fs/cgroup',
                '--', *command]

    def wait_event(self, predicate, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for row in self.events():
                if predicate(row):
                    return row
            time.sleep(0.02)
        self.log.flush()
        raise AssertionError('Engine event did not arrive: ' + repr(self.events()))

    def __exit__(self, *_):
        try:
            self.exit_private()
        finally:
            os.umask(self.previous_umask)

    def exit_private(self):
        self.stop()
        # If the daemon crashed, clean up only children whose command still
        # names this temporary fixture (never signal a recycled unrelated PID).
        for row in self.events():
            if row['event'] == 'started':
                pid = row['pid']
                try:
                    command = Path('/proc', str(pid), 'cmdline').read_bytes().split(b'\0')
                    if str(self.home / 'fake-engine').encode() in command:
                        os.killpg(pid, 9)
                except ProcessLookupError:
                    pass
                except FileNotFoundError:
                    pass
        self.log.close()
        if self.temp is not None:
            self.temp.cleanup()

    def call(self, method, path, body=None, headers=None, timeout=5):
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=timeout)
        try:
            if isinstance(body, (dict, list)):
                body = json.dumps(body)
            connection.request(method, path, body=body,
                               headers={'Content-Type': 'application/json',
                                        'X-Request-ID': 'conformance-request', **(headers or {})})
            response = connection.getresponse()
            return self.read_response(response, method, path)
        finally:
            connection.close()

    def call_wire(self, request, method, path):
        # One write prevents a server's early refusal from racing later body writes.
        with socket.create_connection(('127.0.0.1', self.port), timeout=5) as transport:
            transport.sendall(request)
            with http.client.HTTPResponse(transport) as response:
                response.begin()
                return self.read_response(response, method, path)

    def read_response(self, response, method, path):
        payload = response.read().decode()
        request_id = response.getheader('X-Request-ID')
        # Bug-for-bug: Python's CORS preflight uses end_headers directly and
        # does not emit the request ID added by the normal reply helpers.
        missing_preflight_id = method == 'OPTIONS' and response.status == 204 and request_id is None
        if not missing_preflight_id and (not request_id or not re.fullmatch(r'req_[0-9a-f]{32}', request_id)):
            raise AssertionError('Invalid generated request ID: ' + repr(request_id))
        if request_id is not None and request_id in self.request_ids:
            raise AssertionError('Reused request ID')
        if request_id is not None:
            self.request_ids.add(request_id)
        content_type = response.getheader('Content-Type')
        body = None if method == 'HEAD' or response.status == 204 else (
            sse_frames(payload) if content_type and content_type.startswith('text/event-stream') else json.loads(payload))
        result = {'status': response.status, 'content-type': content_type,
                  'x-request-id': None if missing_preflight_id else '<generated:req_32hex>', 'body': normalize_completion(body)}
        for name in ('X-Local-AI-Warning', 'X-Local-AI-Ignored', 'Access-Control-Allow-Origin',
                     'Access-Control-Allow-Headers', 'Access-Control-Allow-Methods',
                     'Access-Control-Allow-Private-Network', 'Access-Control-Max-Age', 'Vary'):
            if response.getheader(name) is not None:
                result[name.lower()] = response.getheader(name)
        if path.startswith('/v1/responses'):
            result['body'] = normalize_responses(result['body'])
        return result
