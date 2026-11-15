"""Private zero-spend row contracts; complements live HTTP policy contracts.

The Python peer invokes the actual Anthropic refusal recorder with its actual
private Entry/writer. No SDK, network call, key or Python product edit is used.
The native peer exercises real Runtime admission failures and reads its files.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

ROOT = Path(__file__).resolve().parents[2]
GOLDENS = Path(__file__).with_name('refusal_rows.json')
CASES = {'missing-key': (503, 'cloud_credentials_unavailable'),
         'offline': (403, 'offline'), 'unknown-price': (400, 'cloud_pricing_unavailable'),
         'missing-budget': (403, 'budget_not_configured'),
         'invalid-limit': (400, 'invalid_value'), 'exhausted': (429, 'budget_exceeded'),
         'disabled-ledger': (503, 'cloud_ledger_unavailable')}
FIELDS = ('remote', 'remote_provider', 'model', 'origin', 'status', 'error',
          'cost_usd', 'settled', 'n_messages')


def source(name):
    sys.path.insert(0, str(ROOT / 'oxc/src'))
    from oxc.runtime import cloud_anthropic, request_ledger
    home = Path(os.environ['HOME'])
    os.environ['OXC_LEDGER_DIR'] = str(home / 'ledger')
    status, code = CASES[name]
    payload = {'model': 'synthetic-cloud', 'messages': [{'role': 'user', 'content': 'Synthetic request.'}]}
    entry = None if name == 'disabled-ledger' else request_ledger.Entry(
        '/v1/chat/completions', payload, model_alias='synthetic-cloud',
        headers={'X-Local-AI-Origin': 'lab', 'X-Session-Id': 'synthetic-session'})
    state = SimpleNamespace(led=entry)
    cloud_anthropic._record(state, {'provider': 'anthropic', 'upstream_model': 'synthetic-model'},
                            None, None, False, status, code)
    rows = []
    for path in (home / 'ledger').glob('requests-*.jsonl'):
        rows.extend(json.loads(line) for line in path.read_text().splitlines())
    for row in rows:
        raw = json.dumps(row)
        if 'Synthetic request.' in raw or 'synthetic-session' in raw:
            raise AssertionError('Raw request/session leaked to the ledger')
        if len(row['session']) != 16:
            raise AssertionError('Missing private session hash')
    return {'status': status, 'code': code,
            'rows': [{field: row[field] for field in FIELDS} for row in rows]}


def invoke(name):
    with tempfile.TemporaryDirectory(prefix='oxc-refusal-row-') as directory:
        home = '/home/oxc-refusal-row'
        env = {'PATH': '/usr/bin:/bin', 'HOME': home, 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
        binary = os.environ.get('OXC_REFUSAL_ROWS_CONTRACT')
        if binary:
            if not Path(binary).is_absolute():
                raise AssertionError('Native driver must be absolute')
            env['OXC_REFUSAL_ROWS_CONTRACT'] = binary
        result = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net', '--ro-bind', '/', '/',
                                 '--dev-bind', '/dev', '/dev', '--tmpfs', '/home', '--tmpfs', '/tmp',
                                 '--bind', directory, home, '--chdir', home, '--', '/usr/bin/python3',
                                 str(Path(__file__).resolve()), '--case', name],
                                env=env, capture_output=True, text=True, timeout=10)
        if result.returncode:
            raise AssertionError(result.stderr)
        return json.loads(result.stdout)


class RefusalRows(unittest.TestCase):
    def test_zero_spend_private_remote_rows(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_REFUSAL_ROWS_CONTRACT' in os.environ:
            raise SystemExit('Record only Python')
        GOLDENS.write_text(json.dumps({name: invoke(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        binary = os.environ.get('OXC_REFUSAL_ROWS_CONTRACT')
        if binary:
            done = subprocess.run([binary], input=json.dumps({'case': sys.argv[2]}),
                                  env={'PATH': '/usr/bin:/bin', 'HOME': os.environ['HOME']},
                                  capture_output=True, text=True, timeout=5)
            if done.returncode:
                raise SystemExit(done.stderr)
            print(done.stdout)
        else:
            print(json.dumps(source(sys.argv[2])))
    else:
        unittest.main()
