"""Filtered status, selected-model dispatch and bounded download metadata peers."""
import json
import os
from pathlib import Path
import sys
import time
import unittest

from harness import PRIMARY, SECONDARY
from test_cli_actions import FLASH, capture
from test_cli_runtime import invoke_case

GOLDENS = Path(__file__).with_name('goldens') / 'cli-status.json'
CASES = ('status-json', 'status-malformed', 'status-stopping', 'status-offline',
         'code-moe', 'mode-deep', 'mode-wrong-model', 'code-selected-unavailable',
         'status-download-paused', 'status-download-ready', 'status-download-unqualified',
         'status-download-verifying', 'status-trial', 'status-trial-duplicate',
         'status-trial-oversized', 'status-trial-non-ascii')


def capture_status(name):
    model = SECONDARY if name == 'code-moe' else FLASH if name in ('mode-wrong-model', 'code-selected-unavailable') else PRIMARY
    chat = {'state': 'ready', 'active_requests': 2, 'idle_unload_seconds': 600,
            'model': PRIMARY, 'selected_model': model, 'prefix_cache': {'enabled': True},
            'backend': 'gpu', 'execution_policy_mode': 'stock', 'capabilities': ['models'],
            'context_size': 32768, 'queued_requests': 1, 'loaded_model': PRIMARY,
            'resident_bytes': 3 * 1024**3, 'idle_unload_remaining_seconds': 123.5,
            'last_error': None, 'error_at': None, 'error_origin': None,
            'harnesses': {'other': {}, 'synthetic': {'last_used': 2, 'prefixes': [
                {'last_used': 1, 'warm': True, 'tools': 2, 'last_cache_n': 128, 'last_prompt_n': 16}]}},
            'preload': {'state': 'done', 'src': 'disk', 'ms': 12.5, 'age_s': 2},
            'context': {'cap': 131072, 'mode': 'deep', 'modes': [
                {'id': 'deep', 'context': 32768, 'compact_at': 26000, 'cap': 131072, 'available': True, 'reason': None}]},
            'flash_cache': {'preference_control': True, 'pref': 'auto', 'tier_gib': 16,
                            'reason': 'preferred', 'margin_gib': 2, 'desktop_floor_gib': 3,
                            'estimated_working_set_gib': 16, 'private': 'SYNTHETIC-PRIVATE-STATUS'},
            'private': 'SYNTHETIC-PRIVATE-STATUS', 'flash_qualification': 'qualified'}
    if name == 'status-malformed':
        chat.update(state='unknown', active_requests=True, idle_unload_seconds=False,
                    backend='unknown', context_size=0, queued_requests=129, resident_bytes=-1,
                    idle_unload_remaining_seconds=-1, context={'mode': 'invalid', 'modes': [{'id': 'invalid'}]},
                    preload={'state': 'unknown'}, flash_cache={'pref': 'private'},
                    last_error={}, error_at=-1, error_origin='unknown', harnesses={'../invalid': {}, 'other': {}},
                    last_failure={'reason': '', 't': -1}, prefix_cache={'enabled': 1})
    if name == 'status-stopping':
        chat.update(state='stopping', stop_pending='  synthetic\x00\x01stop\nreason  ', last_error={'reason': 'synthetic'},
                    last_failure={'reason': 'host_available_emergency', 'alias': PRIMARY, 't': 12.5, 'origin': 'remote'},
                    error_at=10, error_origin='organic')
    aliases = [PRIMARY, SECONDARY] if name == 'code-selected-unavailable' else [PRIMARY, SECONDARY, FLASH]
    responses = {('GET', '/_local/status'): (503, {}) if name == 'status-offline' else (200, chat),
                 ('GET', '/activity'): (200, {'state': 'idle', 'model': 'q8'}),
                 ('GET', '/v1/models'): (200, {'object': 'list', 'data': [{'id': a, 'object': 'model'} for a in aliases]}),
                 ('GET', '/_local/models'): (200, {'data': [
                     {'id': PRIMARY, 'engine': 'rust', 'engine_status': 'production', 'prefix': 'warm',
                      'memory_gib': 16, 'battery_start_percent': 50, 'selected': True, 'default': True,
                      'state': 'ready', 'memory_basis': 'qualified_working_set', 'flash_cache': chat['flash_cache']},
                     {'id': PRIMARY}, {'id': 'unknown'}, {'key': 'moe', 'engine': 'unknown', 'memory_gib': -1}]}),
                 ('POST', '/_local/mode'): (200, {'mode': 'deep'})}

    def setup(home, environment):
        if name.startswith('status-trial'):
            stamp = time.monotonic_ns() // 1000 - 4_000_000
            raw = ('Id=oxc-flash-profile-cache-full-12288.service\nActiveState=active\n'
                   'ActiveEnterTimestampMonotonic=' + str(stamp) + '\n').encode()
            if name == 'status-trial-duplicate':
                raw += b'\n' + raw
            elif name == 'status-trial-oversized':
                raw = b'x' * 8193
            elif name == 'status-trial-non-ascii':
                raw += b'\xff'
            query = home / 'trial-query'
            query.write_bytes(raw)
            environment['OXC_SYSTEMCTL_SHOW_FILE'] = str(query)
        if not name.startswith('status-download'):
            return
        root = home / 'state/oxc/downloads/flash-next'
        root.mkdir(parents=True)
        models = home / 'data/ai-stack/models/llm/Qwen3.8-Flash-Next'
        models.mkdir(parents=True)
        entries = []
        for index, size in enumerate((4, 32 * 1024**2 + 4)):
            identity, digest = 'flash-next-synthetic-' + str(index), str(index) * 64
            path = models / (identity + '.gguf')
            # Sparse synthetic files: only type/path/length metadata is observed.
            with path.open('wb') as stream:
                stream.truncate(size)
            entries.append({'id': identity, 'sha256': digest, 'bytes': size, 'path': str(path)})
            if index == 0 or name in ('status-download-ready', 'status-download-unqualified'):
                marker = {'id': identity, 'sha256': digest, 'bytes': size, 'verified_at': time.time()}
                (root / (identity + '-verified.json')).write_text(json.dumps(marker))
            else:
                waiting = name == 'status-download-verifying'
                marker = {'id': identity, 'sha256': digest, 'total_bytes': size,
                          'chunk_bytes': 32 * 1024**2, 'done': [0, 32 * 1024**2] if waiting else [0],
                          'downloaded_bytes': size if waiting else 32 * 1024**2,
                          'state': 'waiting for verification slot' if waiting else 'downloading'}
                (root / (identity + '-download.json')).write_text(json.dumps(marker))
        manifest = {'repository': 'unsloth/Qwen3.8-Flash-Next-GGUF', 'revision': 'a' * 40,
                    'models': entries, 'total_bytes': sum(row['bytes'] for row in entries)}
        (root / 'manifest.json').write_text(json.dumps(manifest))
        if name == 'status-download-paused':
            (home / 'mock-services.json').write_text(json.dumps({'oxc-flash-download.service': 'inactive'}))
        if name == 'status-download-unqualified':
            chat['flash_qualification'] = 'weights-only'

    action = 'code' if name.startswith('code') else 'mode' if name.startswith('mode') else 'status'
    output = capture(name, action=action, responses=responses, setup=setup,
                     arguments=['--mode', 'deep'] if action == 'mode' else ['--json'] if action == 'status' else [])
    if action == 'status':
        output['status'] = json.loads(output.pop('stdout'))
        if name == 'status-trial':
            elapsed = output['status']['flash_trial_elapsed_seconds']
            if type(elapsed) is not int or not 4 <= elapsed <= 10:
                raise AssertionError('Invalid elapsed trial time')
            output['status']['flash_trial_elapsed_seconds'] = '<validated:elapsed-seconds>'
        if 'SYNTHETIC-PRIVATE-STATUS' in json.dumps(output):
            raise AssertionError('Status retained an unknown private field')
    return output


class CliStatusContract(unittest.TestCase):
    def test_cli_status_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture_status(sys.argv[2])))
    else:
        unittest.main()
