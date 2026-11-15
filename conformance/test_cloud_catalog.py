"""Cloud-only discovery and local-engine refusals on the merged main reference.

Every case runs in a private network namespace. Public-price refreshes therefore
cannot reach the Internet; provider URLs refer only to owned loopback mocks.
"""
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import sys
import unittest

from cli_harness import CANONICAL_HOME, invoke_case
from environment import ROOT
from harness import Daemon

GOLDENS = Path(__file__).with_name('goldens') / 'cloud-catalog.json'
CASES = ('enabled', 'disabled', 'default-off', 'offline', 'old-offline-name-ignored',
         'missing-secrets', 'budget-missing', 'price-missing', 'local-profile-ignored')
PROVIDERS = ('openrouter', 'anthropic', 'openai')
ALIASES = dict(zip(PROVIDERS, ('synthetic-router', 'synthetic-anthropic', 'synthetic-openai')))
KEYS = {provider: 'synthetic-conformance-' + provider + '-key' for provider in PROVIDERS}


def mocks():
    spec = importlib.util.spec_from_file_location('conformance_cloud_mocks', ROOT / 'oxc/tests/mock_cloud.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def normalize(value, home, port):
    if isinstance(value, list):
        return [normalize(item, home, port) for item in value]
    if isinstance(value, dict):
        return {key: '<PORT>' if key == 'port' and type(item) is int and item == port
                else normalize(item, home, port) for key, item in value.items()}
    if isinstance(value, str):
        value = value.replace(str(home), '<HOME>')
        for host in ('127.0.0.1', 'localhost', '[::1]'):
            value = value.replace(f'http://{host}:{port}', f'http://{host}:<PORT>')
    return value


def capture(name):
    home = CANONICAL_HOME
    module = mocks()
    upstreams = {provider: module.MockOpenRouter() for provider in PROVIDERS}
    try:
        secret_file = home / 'synthetic-cloud-secrets.json'
        if name != 'missing-secrets':
            with os.fdopen(os.open(secret_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as stream:
                json.dump({provider: {'api_key': key} for provider, key in KEYS.items()}, stream)
        models = {ALIASES[provider]: {
            'kind': 'remote/' + provider, 'provider': provider,
            'name': 'Synthetic ' + provider, 'base_url': upstreams[provider].base + '/v1',
            'upstream_model': 'synthetic-upstream-' + provider,
            'context_window': 64000, 'max_output': 512,
            'pricing': {'input_per_million': 1.0, 'output_per_million': 2.0},
        } for provider in PROVIDERS}
        config = {'cloud_only': True, 'cloud': {'enabled': name != 'disabled',
                  'daily_budget_usd': 1.0, 'secrets_file': str(secret_file), 'models': models}}
        if name == 'default-off':
            del config['cloud']['enabled']
        if name == 'budget-missing':
            del config['cloud']['daily_budget_usd']
        if name == 'price-missing':
            for item in models.values():
                del item['pricing']
        if name == 'local-profile-ignored':
            config['model_alias'] = 'synthetic-local'
            config['profiles'] = {'synthetic-local': {'backend_kind': 'oxe', 'model_alias': 'synthetic-local',
                                  'model_path': str(home / 'absent-local-model'), 'context_size': 65536}}
        environment = {'OXC_OFFLINE': '1'} if name == 'offline' else (
                       {'LOCAL_AI_OFFLINE': 'yes'} if name == 'old-offline-name-ignored' else {})
        lease = home / 'state/oxc/inference.lock'
        lease.parent.mkdir(parents=True, mode=0o700)
        with lease.open('xb') as owner:
            owner.write(b'synthetic-other-owner\n')
            owner.flush()
            fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
            before = lease.stat()
            with Daemon(engine_free=True, home=home, config_changes=config, environ=environment) as daemon:
                original_config = daemon.config_path.read_bytes()
                calls = {
                    'models': ('GET', '/v1/models'),
                    'health': ('GET', '/health'),
                    'status': ('GET', '/_local/status'),
                    'catalog': ('GET', '/_local/models'),
                    'model': ('GET', '/v1/models/' + ALIASES['openrouter']),
                    'unknown-model': ('GET', '/v1/models/synthetic-unknown'),
                    'load-refused': ('POST', '/_local/load', {}),
                    'preload-control': ('POST', '/_local/preload', {}),
                }
                results = {label: daemon.call(*args) for label, args in calls.items()}
                status = results['status']['body']
                if status['engine_mode'] != 'none' or status['state'] != 'unloaded' or status['loaded_model'] is not None:
                    raise AssertionError('Cloud-only status reports a local engine')
                if status['active_requests'] or daemon.events():
                    raise AssertionError('Cloud-only discovery started local inference')
                for key in ('model', 'selected_model', 'backend', 'context_size'):
                    if status[key] is not None:
                        raise AssertionError('Cloud-only exposes a local model field: ' + key)
                serialized = json.dumps(results)
                if any(key in serialized for key in KEYS.values()) or str(secret_file) in serialized:
                    raise AssertionError('Cloud discovery exposed a secret or its file path')
                if any(upstream.requests for upstream in upstreams.values()):
                    raise AssertionError('Cloud discovery forwarded an inference request')
                if daemon.config_path.read_bytes() != original_config:
                    raise AssertionError('Cloud-only discovery rewrote configuration')
                if (home / 'fake-engine').exists() or (home / 'synthetic.gguf').exists():
                    raise AssertionError('Cloud-only fixture installed a local model/engine')
                if secret_file.exists() and json.loads(secret_file.read_text()) != {
                        provider: {'api_key': key} for provider, key in KEYS.items()}:
                    raise AssertionError('Cloud discovery changed provider credentials')
            after = lease.stat()
            if (before.st_dev, before.st_ino, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_mtime_ns):
                raise AssertionError('Cloud-only startup/discovery mutated another inference lease')
            if lease.read_bytes() != b'synthetic-other-owner\n':
                raise AssertionError('Cloud-only changed foreign lease content')
        return normalize(results, home, daemon.port)
    finally:
        for upstream in upstreams.values():
            upstream.close()


class CloudCatalogContract(unittest.TestCase):
    def test_cloud_only_catalog_without_a_local_engine(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__, timeout=45), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Record cloud contracts only from Python')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__, timeout=45) for name in CASES},
                                     indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
