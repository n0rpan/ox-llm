"""Owned, isolated Python controller peer for cloud policy contracts."""
import fcntl
import http.client
import importlib.util
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import time
ROOT = Path(__file__).resolve().parents[3]
PYTHON = '/usr/bin/python3'

def config(name):
    return {'cloud_only': True, 'cloud': {'enabled': True, 'daily_budget_usd': 1,
        'models': {'synthetic-cloud': {'kind': 'remote/openrouter',
        'upstream_model': 'synthetic/upstream', 'base_url': 'http://127.0.0.1:1/v1',
        'context_window': 64000, 'max_output': 512, 'name': 'Synthetic cloud model'}}}}

def source(name, home, inspect=None, environment=None):
    spec = importlib.util.spec_from_file_location('cloud_mock', ROOT / 'oxc/tests/mock_cloud.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    upstream = module.MockOpenRouter()
    value = config(name)
    for model in value['cloud']['models'].values():
        if isinstance(model, dict) and model.get('base_url') == 'http://127.0.0.1:1/v1':
            model['base_url'] = upstream.base + '/v1'
    path = home / 'runtime.json'
    path.write_text(json.dumps(value))
    path.chmod(0o600)
    before_config = path.read_bytes()
    lease = home / 'foreign.lock'
    owner = lease.open('xb')
    owner.write(b'synthetic-foreign-owner\n')
    owner.flush()
    fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
    before = lease.stat()
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        port = probe.getsockname()[1]
    env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
           'LC_ALL': 'C.UTF-8', 'PYTHONPATH': str(ROOT / 'oxc/src'),
           'PYTHONDONTWRITEBYTECODE': '1', 'LOCAL_LLM_CONFIG': str(path),
           'LOCAL_LLM_PORT': str(port), 'OXC_LEASE': str(lease),
           'OXC_OFFLINE': '1' if name == 'offline' else '0',
           'XDG_STATE_HOME': str(home / 'state'), 'XDG_CACHE_HOME': str(home / 'cache'),
           'XDG_RUNTIME_DIR': str(home / 'run')}
    env.update(environment or {})

    def get(route):
        peer = http.client.HTTPConnection('127.0.0.1', port, timeout=3)
        try:
            peer.request('GET', route)
            response = peer.getresponse()
            return response.status, json.loads(response.read())
        finally:
            peer.close()

    with (home / 'daemon.log').open('w+') as log:
        process = subprocess.Popen([PYTHON, '-m', 'oxc.runtime.controller'],
                                   env=env, cwd=home, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 8
            while True:
                try:
                    if get('/health')[0] == 200:
                        break
                except OSError:
                    pass
                if process.poll() is not None or time.monotonic() > deadline:
                    log.seek(0)
                    raise AssertionError('Source startup: ' + log.read())
                time.sleep(.02)
            result = {}
            for label, route in (('models', '/v1/models'), ('chooser', '/_local/models'),
                                 ('status', '/_local/status'), ('health', '/health')):
                status, body = get(route)
                if status != 200:
                    raise AssertionError((route, status, body))
                result[label] = body
            status = result['status']
            if any(status[key] is not None for key in
                   ('model', 'selected_model', 'backend', 'context_size', 'loaded_model')):
                raise AssertionError('Cloud-only created local engine state')
            result['status'] = {key: status[key] for key in ('engine_mode', 'model_limits')}
            for label, alias in (('model', 'synthetic-cloud'), ('unknown', 'synthetic-unknown')):
                status, body = get('/v1/models/' + alias)
                if status == 404:
                    if body['error']['code'] != 'model_not_found':
                        raise AssertionError(body)
                    body = None
                elif status != 200:
                    raise AssertionError((status, body))
                result[label] = body
            log.flush()
            log.seek(0)
            lines = [line for line in log.read().splitlines() if line.startswith('oxc: cloud-only mode,')]
            if len(lines) != 1:
                raise AssertionError('Expected one cloud-only startup message')
            result['startup'] = lines[0]
            if inspect:
                result = inspect(port, upstream)
            if path.read_bytes() != before_config or (upstream.requests and inspect is None):
                raise AssertionError('Discovery rewrote config or contacted inference upstream')
        finally:
            process.terminate()
            process.wait(timeout=5)
            upstream.close()
    after = lease.stat()
    if (before.st_dev, before.st_ino, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_mtime_ns):
        raise AssertionError('Cloud-only changed a foreign lease')
    if lease.read_bytes() != b'synthetic-foreign-owner\n':
        raise AssertionError('Cloud-only wrote a foreign lease')
    owner.close()
    return result


def reference(name, script=None):
    with tempfile.TemporaryDirectory(prefix='oxc-cloud-catalog-') as directory:
        env = {'HOME': '/home/oxc-cloud-contract', 'PATH': '/usr/bin:/bin',
               'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
        done = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net',
                               '--ro-bind', '/', '/', '--dev-bind', '/dev', '/dev',
                               '--tmpfs', '/home', '--tmpfs', '/tmp',
                               '--ro-bind', str(ROOT), str(ROOT),
                               '--bind', directory, '/home/oxc-cloud-contract', '--',
                               PYTHON, str(Path(script or __file__).resolve()), '--case', name],
                              env=env, capture_output=True, text=True, timeout=20)
        if done.returncode:
            raise AssertionError(done.stderr)
        return json.loads(done.stdout)


