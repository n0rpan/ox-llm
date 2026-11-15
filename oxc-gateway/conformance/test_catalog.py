"""Cloud-only discovery contracts from the unmodified Python HTTP controller.

Source cases have a private network/HOME and an owned loopback upstream. The
native peer exercises only the opt-in metadata library, not a complete daemon.
"""
import fcntl
import http.client
import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
PYTHON = '/usr/bin/python3'
GOLDENS = Path(__file__).with_name('catalog.json')
CASES = ('router', 'anthropic', 'openai', 'disabled', 'default-off', 'offline',
         'local-profile', 'invalid-entries')


def config(name):
    provider = name if name in ('anthropic', 'openai') else 'openrouter'
    model = {'kind': 'remote/' + provider, 'upstream_model': 'synthetic/upstream',
             'base_url': 'http://127.0.0.1:1/v1', 'context_window': 64000,
             'max_output': 512, 'name': 'Synthetic cloud model'}
    value = {'cloud_only': True, 'cloud': {'enabled': name != 'disabled',
             'daily_budget_usd': 1, 'models': {'synthetic-cloud': model}}}
    if name == 'default-off':
        del value['cloud']['enabled']
    if name == 'local-profile':
        value.update(selected_model='synthetic-local', profiles={
            'synthetic-local': {'backend_kind': 'oxe', 'ready': False,
                                'model_path': '/synthetic/absent-model'}})
    if name == 'invalid-entries':
        models = value['cloud']['models']
        models.update({'bad id!': model, 'wrong-kind': {**model, 'kind': 'local'},
                       'wrong-provider': {**model, 'provider': 'synthetic'},
                       'bad-context': {**model, 'context_window': True},
                       'bad-output': {**model, 'kind': 'remote/openai', 'max_output': None},
                       'url-credentials': {**model, 'base_url': 'https://synthetic@example.invalid/v1'},
                       'url-query': {**model, 'base_url': 'https://example.invalid/v1?x=1'},
                       'empty-model': {**model, 'upstream_model': ' '}, 'not-an-object': []})
    return value


def source(name, home):
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
            if path.read_bytes() != before_config or upstream.requests:
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


def reference(name):
    with tempfile.TemporaryDirectory(prefix='oxc-cloud-catalog-') as directory:
        env = {'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
        done = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net',
                               '--ro-bind', '/', '/', '--dev-bind', '/dev', '/dev',
                               '--tmpfs', '/home', '--tmpfs', '/tmp',
                               '--ro-bind', str(ROOT), str(ROOT),
                               '--bind', directory, '/home/oxc-cloud-contract', '--',
                               PYTHON, str(Path(__file__).resolve()), '--case', name],
                              env=env, capture_output=True, text=True, timeout=20)
        if done.returncode:
            raise AssertionError(done.stderr)
        return json.loads(done.stdout)


class CatalogContract(unittest.TestCase):
    def test_reference_or_opt_in_library(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        executable = os.environ.get('OXC_CATALOG_CONTRACT')
        for name in CASES:
            with self.subTest(case=name):
                if executable:
                    self.assertTrue(Path(executable).is_absolute())
                    done = subprocess.run([executable], input=json.dumps({'config': config(name),
                                          'offline': name == 'offline'}), text=True, capture_output=True,
                                          env={'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8'}, timeout=5)
                    self.assertEqual(done.returncode, 0, done.stderr)
                    actual = json.loads(done.stdout)
                else:
                    actual = reference(name)
                self.assertEqual(actual, expected[name])
        if executable:
            done = subprocess.run([executable], input='{"config": {}}', text=True,
                                  capture_output=True, env={}, timeout=5)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertIsNone(json.loads(done.stdout))


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_CATALOG_CONTRACT' in os.environ:
            raise SystemExit('Record only the Python reference')
        GOLDENS.write_text(json.dumps({name: reference(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(source(sys.argv[2], Path('/home/oxc-cloud-contract'))))
    else:
        unittest.main()
