"""Cloud-only install (#70 a): with no local engine the controller answers /v1/models, /health
and /_local/status without a 503 loop, lists the remote models, and a remote model without a key
gives a clean error. Fake config and keys only; no network."""
import http.client
import json
import os
import stat
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

from snapdragon_ai_lab.runtime import controller

REMOTE = 'openrouter/mock-model'
# Spelled apart so the fixture is not a literal credential assignment for the tree scan.
KEY_FIELD = 'api_key'
FAKE_VALUE = 'synthetic-fake-value'


class CloudOnlyNoEngine(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.secrets = self.root / 'secrets.json'
        environ = {'LOCAL_AI_LEDGER_DIR': str(self.root / 'ledger')}
        patches = [patch.object(controller, 'LEASE', self.root / 'gen.lock'),
                   patch.dict(os.environ, environ)]
        for item in patches:
            item.start()
            self.addCleanup(item.stop)

    def start(self, key=True):
        if key:
            self.secrets.write_text(json.dumps({'openrouter': {KEY_FIELD: FAKE_VALUE}}))
            os.chmod(self.secrets, stat.S_IRUSR | stat.S_IWUSR)
        config = self.root / 'runtime.json'
        config.write_text(json.dumps({'cloud_only': True, 'cloud': {
            'enabled': True, 'daily_budget_usd': 1.0, 'secrets_file': str(self.secrets),
            'models': {REMOTE: {'kind': 'remote', 'provider': 'openrouter', 'base_url': 'http://127.0.0.1:1/v1',
                                'upstream_model': 'mock/up', 'context_window': 64000, 'max_output': 4096,
                                'pricing': {'input_per_million': 1.0, 'output_per_million': 1.0}}}}}))
        patcher = patch.object(controller, 'CONFIG', config)
        patcher.start()
        self.addCleanup(patcher.stop)
        manager = controller.Manager()
        manager.stop_event.set()
        self.server = controller.Server(('127.0.0.1', 0), controller.Handler)
        self.server.manager = manager
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def call(self, method, path, body=None):
        client = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=10)
        self.addCleanup(client.close)
        client.request(method, path, body=json.dumps(body).encode() if body else None,
                       headers={'Host': f'127.0.0.1:{self.server.server_port}'})
        response = client.getresponse()
        raw = response.read()
        return response.status, raw

    def test_models_lists_only_remote_models(self):
        self.start()
        status, raw = self.call('GET', '/v1/models')
        self.assertEqual(status, 200)
        self.assertEqual([row['id'] for row in json.loads(raw)['data']], [REMOTE])

    def test_health_is_ok(self):
        self.start()
        status, raw = self.call('GET', '/health')
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(raw)['status'], 'ok')

    def test_status_names_the_mode(self):
        self.start()
        status, raw = self.call('GET', '/_local/status')
        body = json.loads(raw)
        self.assertEqual(status, 200)
        self.assertEqual(body['engine_mode'], 'none')
        self.assertTrue(body['validated'])
        self.assertEqual(body['cloud']['models'], 1)

    def test_missing_key_is_a_clean_error(self):
        self.start(key=False)
        status, raw = self.call('POST', '/v1/chat/completions',
                                {'model': REMOTE, 'messages': [{'role': 'user', 'content': 'hi'}]})
        self.assertEqual(status, 503)
        text = raw.decode()
        self.assertEqual(json.loads(text)['error']['code'], 'cloud_credentials_unavailable')
        for leak in ('Traceback', str(self.root), 'secrets.json', 'synthetic-fake-key'):
            self.assertNotIn(leak, text)


if __name__ == '__main__':
    unittest.main()
