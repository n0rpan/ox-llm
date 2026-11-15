"""Actual private paid-row contracts, without SDK or provider I/O.

Python uses its Messages translator, reservation, recorder and private writer.
Rust uses Runtime/Call and the same request/usage inputs, not a test accounting
implementation. The shared upstream fixture separately proves provider I/O.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

ROOT = Path(__file__).resolve().parents[3]
GOLDENS = Path(__file__).with_name('paid_rows.json')
CASES = ('cached', 'completion-limit', 'predict-limit', 'body-session',
         'message-session', 'header-priority', 'zero-usage', 'missing-usage',
         'failed-usage', 'cancel-final')
FIELDS = ('model', 'origin', 'agent', 'session', 'n_messages', 'last_role',
          'n_tools', 'n_tool_msgs', 'tool_bytes', 'stream', 'max_tokens',
          'temperature', 'remote', 'remote_provider', 'remote_model', 'status',
          'error', 'completeness', 'prompt_tokens', 'completion_tokens',
          'cached_tokens', 'cache_write_tokens', 'cost_usd', 'settled',
          'usage_missing', 'finish_reason')
PRIVATE = ('Private fixture instructions.', 'Private fixture question λ.',
           'Private fixture tool λ.', 'Private fixture answer.',
           'synthetic-session-header', 'synthetic-session-body', 'fixture_tool')


def inputs(name):
    payload = {'model': 'synthetic-cloud', 'stream': True, 'max_tokens': 1000,
               'temperature': .5, 'messages': [
                   {'role': 'system', 'content': [{'type': 'text', 'text': PRIVATE[0]}]},
                   {'role': 'user', 'content': PRIVATE[1]},
                   {'role': 'tool', 'tool_call_id': 'synthetic-call', 'content': PRIVATE[2]}],
               'tools': [{'type': 'function', 'function': {'name': 'fixture_tool',
                                                         'parameters': {'type': 'object'}}}]}
    session = 'synthetic-session-header'
    if name in ('completion-limit', 'predict-limit'):
        payload['max_completion_tokens' if name == 'completion-limit' else 'n_predict'] = payload.pop('max_tokens')
    if name in ('body-session', 'header-priority'):
        payload['prompt_cache_key'] = 'synthetic-session-body'
    if name in ('body-session', 'message-session'):
        session = None
    usage = {'prompt_tokens': 21, 'completion_tokens': 5,
             'prompt_tokens_details': {'cached_tokens': 7, 'cache_write_tokens': 3}}
    if name == 'zero-usage':
        usage = {'prompt_tokens': 0, 'completion_tokens': 0,
                 'prompt_tokens_details': {'cached_tokens': 0, 'cache_write_tokens': 0}}
    frame = {'choices': [{'delta': {'content': PRIVATE[3]}, 'finish_reason': 'stop'}]}
    if name != 'missing-usage':
        frame['usage'] = usage
    return {'case': name, 'payload': payload, 'session': session, 'frame': frame,
            'fields': FIELDS}


def projection(home, spec):
    directory = home / 'ledger'
    rows = list(directory.glob('requests-*.jsonl'))
    if len(rows) != 1:
        raise AssertionError('Expected exactly one daily file')
    saved = rows[0].read_text()
    parsed = [json.loads(line) for line in saved.splitlines()]
    if len(parsed) != 1 or not saved.endswith('\n'):
        raise AssertionError('Expected exactly one committed row')
    if any(value in saved for value in PRIVATE):
        raise AssertionError('Raw content/session/tool name in private ledger')
    if len(parsed[0]['id']) != 12:
        raise AssertionError('Invalid private request identifier')
    for path in (directory, directory / '.salt', rows[0]):
        info = path.stat()
        if info.st_mode & 0o077 or info.st_uid != os.getuid():
            raise AssertionError('Ledger is not owner-only')
    return {field: parsed[0].get(field) for field in spec['fields']}


def source(spec):
    sys.path.insert(0, str(ROOT / 'oxc/src'))
    from oxc.runtime import cloud, cloud_anthropic, request_ledger
    home = Path(os.environ['HOME'])
    directory = home / 'ledger'
    directory.mkdir(mode=0o700)
    salt = directory / '.salt'
    with os.fdopen(os.open(salt, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), 'wb') as stream:
        stream.write(bytes([1]) * 32)
    os.environ['OXC_LEDGER_DIR'] = str(directory)
    headers = {'X-Local-AI-Origin': ' LAB '}
    if spec['session'] is not None:
        headers['X-Session-Id'] = spec['session']
    entry = request_ledger.Entry('/v1/chat/completions', spec['payload'],
                                 model_alias='synthetic-cloud', headers=headers)
    item = {'provider': 'anthropic', 'upstream_model': 'synthetic-model',
            'context_window': 1024, 'max_output': 32,
            'pricing': cloud._pricing({'pricing': {'input': 2, 'output': 3,
                        'cache_read_per_million': .2, 'cache_write_per_million': 2.5}})}
    params = cloud_anthropic.messages_request(spec['payload'], item)
    hold = cloud.reserve({'cloud': {'daily_budget_usd': 1}}, item, params)
    entry.admitted_now()
    entry.observe(('data: ' + json.dumps(spec['frame']) + '\n\n').encode())
    state = SimpleNamespace(led=entry)
    name = spec['case']
    cloud_anthropic._record(state, item, hold, spec['frame'].get('usage'),
                            name == 'failed-usage',
                            503 if name == 'failed-usage' else 499 if name == 'cancel-final' else 200,
                            'cloud_upstream_error' if name == 'failed-usage' else
                            'client_closed' if name == 'cancel-final' else None,
                            cancelled=(0, 0) if name == 'cancel-final' else None,
                            usage_final=name == 'cancel-final')
    if entry.written is not True or not hold.settled:
        raise AssertionError('Source did not commit before releasing admission')
    return projection(home, spec)


def invoke(name):
    with tempfile.TemporaryDirectory(prefix='oxc-paid-row-') as directory:
        home = '/home/oxc-paid-row'
        env = {'PATH': '/usr/bin:/bin', 'HOME': home, 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
        binary = os.environ.get('OXC_PAID_ROWS_CONTRACT')
        if binary:
            if not Path(binary).is_absolute():
                raise AssertionError('Native driver must be absolute')
            env['OXC_PAID_ROWS_CONTRACT'] = binary
        done = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net', '--ro-bind', '/', '/',
                               '--dev-bind', '/dev', '/dev', '--tmpfs', '/home', '--tmpfs', '/tmp',
                               '--bind', directory, home, '--chdir', home, '--', '/usr/bin/python3',
                               str(Path(__file__).resolve()), '--case', name],
                              env=env, capture_output=True, text=True, timeout=10)
        if done.returncode:
            raise AssertionError(done.stderr)
        return json.loads(done.stdout)


class PaidRows(unittest.TestCase):
    def test_private_paid_row_matches_reference(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_PAID_ROWS_CONTRACT' in os.environ:
            raise SystemExit('Record only Python')
        rows = {name: invoke(name) for name in CASES}
        GOLDENS.write_text('{\n' + ',\n'.join('  ' + json.dumps(name) + ': ' +
                          json.dumps(rows[name], sort_keys=True) for name in sorted(rows)) + '\n}\n')
    elif sys.argv[1:2] == ['--case']:
        spec = inputs(sys.argv[2])
        binary = os.environ.get('OXC_PAID_ROWS_CONTRACT')
        if binary:
            done = subprocess.run([binary], input=json.dumps(spec), text=True, capture_output=True,
                                  env={'PATH': '/usr/bin:/bin', 'HOME': os.environ['HOME']}, timeout=5)
            if done.returncode:
                raise SystemExit(done.stderr)
            actual = json.loads(done.stdout)
            if actual != projection(Path(os.environ['HOME']), spec):
                raise AssertionError('Native return differs from committed private file')
            print(json.dumps(actual))
        else:
            print(json.dumps(source(spec)))
    else:
        unittest.main()
