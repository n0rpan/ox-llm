"""Task 61: malformed config and ledger files must be data, never an exception.

The ledger is append-only JSONL on disk; a torn line, a JSON scalar or a non-finite number is
normal after a crash. `/v1/models` reads runtime.json through the pure alias table. Both are
fuzzed here with a temporary directory, so no controller state is touched.
"""
import json
import math
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from snapdragon_ai_lab.runtime import ctl_models, openai_api, request_ledger


class LedgerRows(unittest.TestCase):
    def test_malformed_rows_are_skipped_and_medians_ignore_them(self):
        with tempfile.TemporaryDirectory() as root:
            day = __import__('time').strftime('%Y%m%d')
            path = Path(root) / ('requests-%s.jsonl' % day)
            lines = [
                b'not json',
                b'[]',
                b'null',
                b'{"status":200,"prompt_n":"8","prompt_ms":10}',
                b'{"status":200,"prompt_n":8,"prompt_ms":0}',
                b'{"status":200,"prompt_n":8,"prompt_ms":10}',
                b'{"status":200,"prompt_n":8,"prompt_ms":10,"queue_wait_ms":"x"}',
                b'{"status":200,"prompt_n":8,"prompt_ms":10,"queue_wait_ms":NaN}',
                b'{"status":200,"prompt_n":8,"prompt_ms":10,"queue_wait_ms":Infinity}',
            ]
            path.write_bytes(b'\n'.join(lines) + b'\n')
            with mock.patch.dict(os.environ, {'LOCAL_AI_LEDGER_DIR': root}):
                value = request_ledger.recent()
            self.assertEqual(value['object'], 'list')
            self.assertTrue(value['available'])
            self.assertEqual(len(value['rows']), 6)  # the dict rows parse; scalars are dropped
            self.assertTrue(all(isinstance(row, dict) for row in value['rows']))
            # NaN/Infinity are not numbers for the medians; a missing value stays None and
            # never raises, and every median that exists is finite.
            for median in value['medians'].values():
                self.assertTrue(median is None or math.isfinite(median))


class ConfigAliases(unittest.TestCase):
    def test_malformed_profiles_are_an_empty_table_not_a_crash(self):
        for profiles in (None, [], 3, 'x'):
            with self.subTest(profiles=profiles):
                value = {'profiles': profiles, 'model_alias': 'm'}
                try:
                    table = openai_api.alias_table(value)
                    resolved = openai_api.resolve_model('local-heavy', value)
                except (TypeError, AttributeError) as error:
                    self.fail(f'profiles={profiles!r} raised {type(error).__name__}: {error}')
                self.assertIn('local-heavy', table)
                self.assertEqual(resolved, 'qwen3.8-flash-next-local')

    def test_manager_config_treats_a_malformed_profiles_value_as_empty(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'runtime.json'
            path.write_text(json.dumps({'model_alias': 'm', 'profiles': 3}))
            with mock.patch.object(ctl_models.ctl, 'CONFIG', path):
                value = ctl_models.ModelsMixin().config()
            self.assertIsInstance(value, dict)
            self.assertEqual(value.get('model_alias'), 'm')


if __name__ == '__main__':
    unittest.main()
