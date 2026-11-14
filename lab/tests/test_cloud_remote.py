"""Wave D D1/D2: remote catalog routing, the privacy switch, the key boundary and the budget."""
import http.client
import json
import os
import stat
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from snapdragon_ai_lab.runtime import cloud

from mock_cloud import MockOpenRouter
from openai_api_stack import Stack

REMOTE = 'openrouter/mock-model'
UPSTREAM = 'mock/model-upstream'
KEY = 'synthetic-cloud-key-for-mock-only'


def profiles():
    return {'openrouter/mock-model': {
        'kind': 'remote', 'provider': 'openrouter', 'base_url': 'http://127.0.0.1:1/v1',
        'upstream_model': UPSTREAM, 'context_window': 64000, 'max_output': 4096,
        'pricing': {'input_per_million': 1.0, 'output_per_million': 1.0}}}


class CloudCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='cloud-d1-'))
        self.secrets = self.tmp / 'secrets.json'
        self.secrets.write_text(json.dumps({'openrouter': {'api_key': KEY},
                                            'anthropic': {'api_key': KEY}}))
        os.chmod(self.secrets, stat.S_IRUSR | stat.S_IWUSR)
        self.ledger = self.tmp / 'ledger'
        self.upstream = MockOpenRouter()
        self.addCleanup(self.upstream.close)
        self.stack = None

    def tearDown(self):
        if self.stack is not None:
            self.stack.close()

    def start(self, *, enabled=True, budget=1.0, offline=False, models=None, mock=None):
        mock = mock or self.upstream
        table = models if models is not None else profiles()
        for row in table.values():
            row['base_url'] = mock.base + '/v1'
        cloud_config = {'enabled': enabled, 'daily_budget_usd': budget, 'secrets_file': str(self.secrets),
                        'models': table}
        environ = {'LOCAL_AI_LEDGER_DIR': str(self.ledger)}
        if offline:
            environ['OXC_OFFLINE'] = '1'
        self.stack = Stack(mock.port, cloud=cloud_config, environ=environ)
        return self.stack

    def call(self, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection('127.0.0.1', self.stack.port, timeout=15)
        try:
            data = json.dumps(body).encode() if isinstance(body, dict) else body
            connection.request(method, path, body=data, headers=headers or {})
            response = connection.getresponse()
            payload = response.read()
            return response.status, payload
        finally:
            connection.close()

    def chat(self, *, stream=False, model=REMOTE, text='hello'):
        return self.call('POST', '/v1/chat/completions', {
            'model': model, 'messages': [{'role': 'user', 'content': text}], 'stream': stream})

    def rows(self):
        status, body = self.call('GET', '/v1/models')
        self.assertEqual(status, 200)
        return json.loads(body)['data']

    def ledger_text(self, minimum=1):
        deadline = time.monotonic() + 3
        while True:
            paths = sorted(self.ledger.glob('requests-*.jsonl'))
            text = '\n'.join(path.read_text() for path in paths)
            if len(text.splitlines()) >= minimum or time.monotonic() >= deadline:
                return text
            time.sleep(0.01)

    def ledger_rows(self):
        text = self.ledger_text()
        return [json.loads(line) for line in text.splitlines() if line]


class PrivacySwitch(CloudCase):
    def test_cloud_is_off_by_default_and_remote_models_are_not_listed(self):
        self.start(enabled=False)
        self.assertNotIn(REMOTE, [row['id'] for row in self.rows()])
        status, body = self.chat()
        self.assertEqual(status, 404)
        self.assertEqual(json.loads(body)['error']['code'], 'model_not_found')
        self.assertEqual(self.upstream.requests, [])

    def test_enabled_remote_model_is_listed_with_capabilities(self):
        self.start()
        row = next(row for row in self.rows() if row['id'] == REMOTE)
        self.assertTrue(row['remote'])
        self.assertEqual(row['provider'], 'openrouter')
        self.assertEqual(row['context_length'], 64000)
        self.assertEqual(row['context_window'], 64000)
        self.assertEqual(row['max_output_tokens'], 4096)
        caps = row['capabilities']
        self.assertEqual(caps['remote_provider'], 'openrouter')
        self.assertEqual(caps['context_window'], 64000)
        self.assertFalse(caps['prefill'])
        self.assertFalse(caps['cache_hints'])
        self.assertFalse(caps['echo'])

    def test_local_rows_publish_the_canonical_capabilities_too(self):
        self.start()
        local = next(row for row in self.rows() if row['id'] == 'local-default')
        caps = local['capabilities']
        for key in ('remote', 'echo', 'cache_hints', 'cancel', 'prefill', 'context_window'):
            self.assertIn(key, caps)
        self.assertFalse(caps['remote'])
        self.assertGreater(caps['context_window'], 0)
        self.assertEqual(local['context_window'], caps['context_window'])
        status, body = self.call('GET', '/_local/status')
        self.assertEqual(status, 200)
        limits = json.loads(body)['model_limits']['local-default']
        self.assertEqual(limits['capabilities']['context_window'], caps['context_window'])
        remote = json.loads(body)['model_limits'][REMOTE]
        self.assertTrue(remote['capabilities']['remote'])
        self.assertEqual(remote['context_window'], 64000)

    def test_status_exposes_the_switch_but_never_the_key_or_path(self):
        self.start()
        status, body = self.call('GET', '/_local/status')
        self.assertEqual(status, 200)
        value = json.loads(body)
        self.assertTrue(value['cloud']['enabled'])
        self.assertTrue(value['cloud']['providers']['openrouter'])
        self.assertEqual(value['cloud']['daily_budget_usd'], 1.0)
        self.assertNotIn(KEY, body.decode())
        self.assertNotIn(str(self.secrets), body.decode())

    def test_offline_hard_refuses_a_remote_model_before_any_upstream_call(self):
        self.start(offline=True)
        self.assertNotIn(REMOTE, [row['id'] for row in self.rows()])
        status, body = self.chat()
        self.assertEqual(status, 403)
        self.assertEqual(json.loads(body)['error']['code'], 'offline')
        self.assertEqual(self.upstream.requests, [])


class Routing(CloudCase):
    def test_upstream_receives_the_upstream_id_and_mandatory_zdr_prefs(self):
        self.start()
        status, body = self.chat(text='hi')
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)['choices'][0]['message']['content'], 'Cloud reply.')
        self.assertEqual(len(self.upstream.requests), 1)
        sent = self.upstream.requests[0]
        self.assertEqual(sent['path'], '/v1/chat/completions')
        self.assertEqual(sent['body']['model'], UPSTREAM)
        self.assertEqual(sent['body']['provider'], {'zdr': True, 'data_collection': 'deny'})
        self.assertEqual(sent['headers']['authorization'], 'Bearer ' + KEY)
        self.assertEqual(sent['headers']['x-api-key'] if 'x-api-key' in sent['headers'] else None, None)
        self.assertEqual(sent['body']['messages'][0]['content'], 'hi')

    def test_client_provider_prefs_cannot_weaken_the_privacy_requirements(self):
        self.start()
        status, _ = self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': [{'role': 'user', 'content': 'hi'}],
            'provider': {'zdr': False, 'data_collection': 'allow', 'order': ['x']}})
        self.assertEqual(status, 200)
        self.assertEqual(self.upstream.requests[0]['body']['provider'],
                         {'zdr': True, 'data_collection': 'deny', 'order': ['x']})

    def test_key_is_redacted_from_an_upstream_error_and_never_written_to_the_ledger(self):
        bad = MockOpenRouter(status=500, error='provider rejected Authorization: Bearer ' + KEY)
        self.addCleanup(bad.close)
        self.start(mock=bad)
        status, body = self.chat()
        self.assertEqual(status, 500)
        self.assertNotIn(KEY.encode(), body)
        self.assertIn(b'[redacted]', body)
        ledger = self.ledger_text()
        self.assertNotIn(KEY, ledger)
        self.assertNotIn('hello', ledger)
        status, status_body = self.call('GET', '/_local/status')
        self.assertNotIn(KEY.encode(), status_body)
        self.assertNotIn(KEY, ledger)

    def test_streaming_passes_usage_through_and_meters_the_request(self):
        self.start()
        status, body = self.chat(stream=True)
        self.assertEqual(status, 200)
        self.assertIn(b'data: ', body)
        self.assertIn(b'"content": "Cloud reply."', body)
        self.assertIn(b'"usage"', body)
        self.assertTrue(body.rstrip().endswith(b'[DONE]'))
        row = self.ledger_rows()[-1]
        self.assertTrue(row['remote'])
        self.assertEqual(row['remote_provider'], 'openrouter')
        self.assertEqual(row['cost_usd'], 0.000015)
        self.assertEqual(row['prompt_tokens'], 11)
        self.assertEqual(row['completion_tokens'], 4)


class Budget(CloudCase):
    def test_a_missing_cap_fails_closed(self):
        self.start(budget=0)
        status, body = self.chat()
        self.assertEqual(status, 403)
        self.assertEqual(json.loads(body)['error']['code'], 'budget_not_configured')
        self.assertEqual(self.upstream.requests, [])

    def test_the_daily_cap_refuses_the_next_request_and_records_cost(self):
        upstream = MockOpenRouter(usage={'prompt_tokens': 1000, 'completion_tokens': 1000, 'total_tokens': 2000})
        self.addCleanup(upstream.close)
        self.start(budget=0.001, mock=upstream)
        self.assertEqual(self.chat()[0], 200)
        rows = self.ledger_rows()
        self.assertEqual(rows[-1]['cost_usd'], 0.002)
        status, body = self.chat()
        self.assertEqual(status, 429)
        self.assertEqual(json.loads(body)['error']['code'], 'budget_exceeded')
        self.assertEqual(len(upstream.requests), 1)

    def test_spend_is_per_day_and_ignores_local_rows(self):
        directory = self.tmp / 'ledger2'
        directory.mkdir()
        (directory / 'requests-20260101.jsonl').write_text(json.dumps({'remote': True, 'cost_usd': 9.0}) + '\n')
        with patch.dict(os.environ, {'LOCAL_AI_LEDGER_DIR': str(directory)}):
            self.assertEqual(cloud.spend_today(), 0.0)
            (directory / 'requests-20260101.jsonl').write_text(
                json.dumps({'remote': False, 'cost_usd': 9.0}) + '\n' +
                json.dumps({'remote': True, 'cost_usd': 0.25}) + '\n')
            self.assertEqual(cloud.spend_today(day='20260101'), 0.25)


class Secrets(CloudCase):
    def test_a_non_private_secrets_file_is_refused(self):
        os.chmod(self.secrets, 0o644)
        self.start()
        status, body = self.chat()
        self.assertEqual(status, 503)
        self.assertEqual(json.loads(body)['error']['code'], 'cloud_credentials_unavailable')
        self.assertEqual(self.upstream.requests, [])

    def test_a_missing_secrets_file_is_refused_without_a_path(self):
        self.secrets.unlink()
        self.start()
        status, body = self.chat()
        self.assertEqual(status, 503)
        self.assertNotIn(str(self.secrets).encode(), body)
        self.assertNotIn(KEY.encode(), body)


if __name__ == '__main__':
    unittest.main()
