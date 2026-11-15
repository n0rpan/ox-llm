"""Read/write compatibility through public HTTP operations and persisted files.

With OXC_DAEMON_COMMAND set, each round trip crosses from the unmodified Python
reference into the selected implementation and back, reusing the same HOME.
"""
import json
import stat
import unittest

from harness import Daemon, PRIMARY, SECONDARY
from test_api import chat
from test_lifecycle import status, wait_until


class FileCompatibilityContract(unittest.TestCase):
    def test_config_round_trip_between_reference_and_selected(self):
        extension = {'version': 1, 'opaque': ['synthetic', 7]}
        with Daemon(configured=True, config_changes={'synthetic_extension': extension,
                    'cache': {'ttl_secs': 86400}}) as daemon:
            daemon.restart(reference=True)
            self.assertEqual(daemon.call('POST', '/_local/select', {'model': SECONDARY})['status'], 200)
            self.assertEqual(daemon.call('POST', '/_local/cache/budget', {'bytes': 0})['status'], 200)
            self.assertEqual(daemon.call('POST', '/_local/mode', {'model': PRIMARY, 'mode': 'deep'})['status'], 200)
            reference = json.loads(daemon.config_path.read_text())
            self.assertEqual(reference['synthetic_extension'], extension)
            self.assertEqual(reference['cache'], {'ttl_secs': 86400, 'disk_budget_bytes': 0})
            self.assertEqual(reference['context_modes'], {'models': {PRIMARY: 'deep'}})
            self.assertEqual(stat.S_IMODE(daemon.config_path.stat().st_mode), 0o600)
            daemon.restart()
            self.assertEqual(status(daemon)['selected_model'], SECONDARY)
            self.assertEqual(status(daemon)['model_limits'][PRIMARY]['mode'], 'deep')
            self.assertEqual(daemon.call('GET', '/_local/cache')['body']['budget_bytes'], 0)
            self.assertEqual(daemon.call('POST', '/_local/select', {'model': PRIMARY})['status'], 200)
            self.assertEqual(daemon.call('POST', '/_local/cache/budget', {'bytes': 4_000_000_000})['status'], 200)
            self.assertEqual(daemon.call('POST', '/_local/mode', {'model': PRIMARY, 'mode': 'fast'})['status'], 200)
            selected = json.loads(daemon.config_path.read_text())
            self.assertEqual(selected['synthetic_extension'], extension)
            self.assertEqual(selected['cache'], {'ttl_secs': 86400, 'disk_budget_bytes': 4_000_000_000})
            self.assertEqual(selected['profiles'], reference['profiles'])
            self.assertEqual(stat.S_IMODE(daemon.config_path.stat().st_mode), 0o600)
            daemon.restart(reference=True)
            self.assertEqual(status(daemon)['selected_model'], PRIMARY)
            self.assertEqual(status(daemon)['model_limits'][PRIMARY]['mode'], 'fast')
            self.assertEqual(daemon.call('GET', '/_local/cache')['body']['budget_bytes'], 4_000_000_000)

    def test_ledger_round_trip_preserves_salt_hashes_schema_and_privacy(self):
        headers = {'X-Session-Id': 'synthetic-session', 'X-Local-AI-Origin': 'lab',
                   'User-Agent': 'conformance/1'}
        payload = chat(messages=[{'role': 'system', 'content': 'Synthetic agent identity.'},
                                 {'role': 'user', 'content': 'Synthetic private prompt.'}])
        with Daemon(configured=True) as daemon:
            daemon.restart(reference=True)
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', payload, headers=headers)['status'], 200)
            reference = wait_until(lambda: daemon.call('GET', '/_local/ledger')['body'].get('rows'))[0]
            directory = daemon.home / '.local/state/oxc/ledger'
            salt_digest = (directory / '.salt').read_bytes()
            self.assertEqual(len(salt_digest), 32)
            self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE((directory / '.salt').stat().st_mode), 0o600)
            files = list(directory.glob('requests-*.jsonl'))
            self.assertEqual(len(files), 1)
            self.assertEqual(stat.S_IMODE(files[0].stat().st_mode), 0o600)
            self.assertRegex(reference['session'], r'^[0-9a-f]{16}$')
            self.assertRegex(reference['agent'], r'^[0-9a-f]{12}$')
            daemon.restart()
            self.assertEqual(daemon.call('GET', '/_local/ledger')['body']['rows'], [reference])
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', payload, headers=headers)['status'], 200)
            rows = wait_until(lambda: (lambda rows: rows if len(rows) == 2 else None)(
                daemon.call('GET', '/_local/ledger')['body'].get('rows', [])))
            selected = rows[0]
            self.assertEqual(rows[1], reference)
            self.assertEqual(set(selected), set(reference))
            for field in ('path', 'model', 'origin', 'client', 'agent', 'n_messages', 'last_role',
                          'n_tools', 'stream', 'max_tokens', 'temperature', 'n_tool_msgs', 'tool_bytes',
                          'session', 'mode', 'status', 'cache_n', 'prompt_n', 'predicted_n',
                          'finish_reason', 'cached_tokens', 'new_tokens', 'ctx_tokens',
                          'output_tokens', 'completeness'):
                with self.subTest(field=field):
                    self.assertEqual(selected[field], reference[field])
            self.assertTrue((directory / '.salt').read_bytes() == salt_digest)
            raw = files[0].read_text()
            for text in ('Synthetic agent identity.', 'Synthetic private prompt.', 'Synthetic engine reply.',
                         'synthetic-session'):
                self.assertNotIn(text, raw)
            daemon.restart(reference=True)
            self.assertEqual(daemon.call('GET', '/_local/ledger')['body']['rows'], [selected, reference])

    def test_api_key_file_is_read_at_request_time_and_unreadable_fails_closed(self):
        # Both values are synthetic test data, never credentials from the caller.
        first, second = 'synthetic-first-credential', 'synthetic-second-credential'
        with Daemon(configured=True) as daemon:
            key = daemon.home / 'api-key'
            config = json.loads(daemon.config_path.read_text())
            config['openai_api'] = {'api_key_file': str(key)}
            daemon.config_path.write_text(json.dumps(config))
            self.assertEqual(daemon.call('GET', '/v1/models')['status'], 401)
            key.write_text(first + '\n')
            key.chmod(0o600)
            self.assertEqual(daemon.call('GET', '/v1/models', headers={'Authorization': 'Bearer ' + first})['status'], 200)
            key.write_text(second + '\n')
            self.assertEqual(daemon.call('GET', '/v1/models', headers={'Authorization': 'Bearer ' + first})['status'], 401)
            self.assertEqual(daemon.call('GET', '/v1/models', headers={'Authorization': 'Bearer ' + second})['status'], 200)
            for path in ('/_local/status', '/_local/models', '/_local/ledger'):
                value = json.dumps(daemon.call('GET', path))
                self.assertNotIn(first, value)
                self.assertNotIn(second, value)
            key.unlink()
            self.assertEqual(daemon.call('GET', '/v1/models', headers={'Authorization': 'Bearer ' + second})['status'], 401)
            self.assertEqual(daemon.events(), [])


if __name__ == '__main__':
    unittest.main()
