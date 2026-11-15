"""Harness learning, metadata-only candidates, persisted primers and control contracts."""
import json
import os
from pathlib import Path
import stat
import sys
import time
import unittest

from harness import Daemon, PRIMARY
from test_api import chat, TOOLS

GOLDENS = Path(__file__).with_name('goldens') / 'harnesses.json'
CASES = ('empty', 'stored', 'learning-off', 'learning-on', 'approve-label-forget',
         'learning-controls', 'prepare', 'compatibility-counters', 'learning-on-stream',
         'memory-only', 'bounded-prefixes', 'bounded-candidate-keys', 'bounded-candidates')
SYSTEM = 'Synthetic harness instructions — λ. ' + 'x' * 3100
PRIVATE = 'Synthetic private user turn excluded from stored primers.'
HEADERS = {'User-Agent': 'SyntheticClient/1.0'}


def normalize(value):
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if not isinstance(value, dict):
        return value
    out = {}
    for key, item in value.items():
        if key in ('first_seen', 'last_used', 'last_ok') and type(item) in (int, float) and item > 1_000_000_000:
            if abs(time.time() - item) > 30:
                raise AssertionError('Invalid registry timestamp')
            out[key] = '<current:unix-seconds>'
        elif key == 'seconds':
            if type(item) not in (int, float) or not 0 <= item <= 30:
                raise AssertionError('Invalid prepare duration')
            out[key] = '<elapsed:seconds>'
        else:
            out[key] = normalize(item)
    return out


def status(daemon):
    reply = daemon.call('GET', '/_local/harnesses')
    if reply['status'] != 200:
        raise AssertionError(reply)
    return reply


def send(daemon, system=SYSTEM, **extra):
    payload = chat(messages=[{'role': 'system', 'content': system}, {'role': 'user', 'content': PRIVATE}], **extra)
    reply = daemon.call('POST', '/v1/chat/completions', payload, HEADERS)
    if reply['status'] != 200:
        raise AssertionError(reply)
    # The response can arrive before ledger/registry finalization releases the
    # request. Wait on the public lifecycle state before reading those records.
    deadline = time.monotonic() + 5
    while daemon.call('GET', '/_local/status')['body']['active_requests']:
        if time.monotonic() >= deadline:
            raise AssertionError('Completed request did not finalize')
        time.sleep(0.01)


def persistence(daemon):
    root = daemon.home / 'state/oxc/harness-prefixes'
    paths = sorted(path for path in root.rglob('*') if path.is_file()) if root.exists() else []
    if any(PRIVATE.encode() in path.read_bytes() for path in paths):
        raise AssertionError('A user turn was persisted in the harness registry')
    if not paths:
        return {'files': []}
    if stat.S_IMODE(root.stat().st_mode) != 0o700:
        raise AssertionError('Registry directory is not private')
    if any(stat.S_IMODE(path.stat().st_mode) != 0o600 for path in paths):
        raise AssertionError('Registry files are not private')
    index = json.loads((root / 'index.json').read_text())
    return {'schema': index['schema'], 'entries': sorted(index['entries']),
            'primers': {path.name: json.loads(path.read_text()) for path in paths if path.parent.name == 'primers'}}


def run_case(name):
    changes = {'harness_learning': {'enabled': True, 'min_requests': 2}} if name in (
        'learning-on', 'prepare', 'learning-on-stream', 'memory-only') else {}
    if name == 'bounded-prefixes':
        changes = {'harness_learning': {'enabled': True, 'min_requests': 1}}
    environment = {'OXC_HARNESS_STATE': ''} if name == 'memory-only' else {}
    with Daemon(configured=True, config_changes=changes, environ=environment) as daemon:
        output = []
        if name == 'empty':
            output.append(status(daemon))
        elif name == 'stored':
            root = daemon.home / 'state/oxc/harness-prefixes'
            (root / 'primers').mkdir(parents=True)
            root.chmod(0o700)
            config = json.loads(daemon.config_path.read_text())
            identity = ':'.join(str(config[k]) for k in ('sha256', 'server_sha256', 'context_size'))
            entry = {'model': PRIMARY, 'system_chars': 12, 'tools': 1, 'requests': 2,
                     'last_used': 1234, 'last_cache_n': 2, 'last_prompt_n': 6, 'warm_identity': identity}
            index = {'schema': 1, 'entries': {'a' * 64: {**entry, 'harness': 'synthetic'},
                     'b' * 64: {**entry, 'harness': 'learned-aaaaaaaa', 'last_used': 2345},
                     'c' * 64: {**entry, 'harness': 'other'}},
                     'approved': ['learned-bbbbbbbb'], 'ignored': ['learned-cccccccc'],
                     'labels': {'learned-aaaaaaaa': ' Synthetic <label> / fixture '}}
            (root / 'index.json').write_text(json.dumps(index))
            (root / 'primers' / ('c' * 64 + '.json')).write_text('{}')
            daemon.restart()
            output.append(status(daemon))
            if (root / 'primers' / ('c' * 64 + '.json')).exists():
                raise AssertionError('Legacy unknown-client primer was retained')
        elif name in ('learning-off', 'learning-on', 'prepare', 'learning-on-stream', 'memory-only'):
            options = {'stream': True, 'stream_options': {'include_usage': True}} if name == 'learning-on-stream' else {}
            send(daemon, **options)
            output.append(status(daemon))
            if name != 'learning-off':
                send(daemon, **options)
                output.append(status(daemon))
            output.append(persistence(daemon))
            if name == 'learning-off' and (daemon.home / 'state/oxc/harness-prefixes/index.json').exists():
                raise AssertionError('Unknown-client text was stored without opt-in')
            if name == 'learning-on':
                daemon.restart(reference=True)
                output.append(status(daemon))
                daemon.restart()
                output.append(status(daemon))
            if name == 'prepare':
                harness = next(iter(output[1]['body']['harnesses']))
                before = len(daemon.events())
                output.append(daemon.call('POST', '/_local/harness/prepare', {'harness': harness}))
                output.append([{'path': row['path'], 'body': row['body']} for row in daemon.events()[before:]
                               if row['event'] == 'request' and row['method'] == 'POST'])
                output.append(daemon.call('POST', '/_local/harness/prepare', {'harness': '*'}))
                index_path = daemon.home / 'state/oxc/harness-prefixes/index.json'
                index = json.loads(index_path.read_text())
                for entry in index['entries'].values():
                    entry['warm_identity'] = None
                index_path.write_text(json.dumps(index))
                daemon.restart()
                before = len(daemon.events())
                output.append(daemon.call('POST', '/_local/harness/prepare', {'harness': '*'}))
                output.append([{'path': row['path'], 'body': row['body']} for row in daemon.events()[before:]
                               if row['event'] == 'request' and row['method'] == 'POST'])
            elif name == 'memory-only':
                harness = next(iter(output[1]['body']['harnesses']))
                output.append(daemon.call('POST', '/_local/harness/prepare', {'harness': harness}))
        elif name in ('bounded-prefixes', 'bounded-candidate-keys', 'bounded-candidates'):
            count = {'bounded-prefixes': 5, 'bounded-candidate-keys': 9, 'bounded-candidates': 17}[name]
            for i in range(count):
                # Changing only the tail keeps one learned name but gives a new
                # prefix. Changing the leading text creates separate candidates.
                system = ('Synthetic candidate %d. ' % i + SYSTEM) if name == 'bounded-candidates' else SYSTEM + str(i)
                send(daemon, system=system)
            output.append(status(daemon))
            output.append(persistence(daemon))
        elif name == 'approve-label-forget':
            send(daemon)
            harness = status(daemon)['body']['harness_learning']['candidates'][0]['harness']
            output.append(daemon.call('POST', '/_local/harness/learn', {'harness': harness}))
            send(daemon)
            output.append(daemon.call('POST', '/_local/harness/label', {
                'harness': harness, 'label': ' Synthetic <tool> / label ☃ '}))
            output.append(status(daemon))
            output.append(daemon.call('POST', '/_local/harness/forget', {'harness': harness, 'ignore': True}))
            send(daemon)
            output.append(status(daemon))
            output.append(persistence(daemon))
        elif name == 'learning-controls':
            for body in ({}, {'enabled': 1}, {'enabled': True, 'min_requests': 0},
                         {'enabled': True, 'min_requests': 2}, {'enabled': False}):
                output.append(daemon.call('POST', '/_local/harness/learning', body))
            output.append(daemon.call('POST', '/_local/harness/learn', {'harness': 'learned-ffffffff'}))
            output.append(daemon.call('POST', '/_local/harness/label', {'harness': 'other', 'label': 'synthetic'}))
            output.append(daemon.call('POST', '/_local/harness/forget', {'harness': 42}))
            output.append(daemon.call('POST', '/_local/harness/forget', {}))
            output.append(status(daemon))
        elif name == 'compatibility-counters':
            payloads = [chat(reasoning_effort='high', response_format={'type': 'json_object'},
                temperature=None, cache_prompt=None, tools=TOOLS,
                tool_choice={'type': 'function', 'function': {'name': 'first_tool'}}),
                chat(n=2), chat(messages=[{'role': 'user', 'content': 'synthetic'},
                    {'role': 'system', 'content': 'Synthetic late instruction.'},
                    {'role': 'user', 'content': [{'type': 'image_url', 'image_url': {'url': 'https://example.invalid/synthetic'}}]}]),
                chat(reasoning_effort='synthetic-unknown')]
            for payload in payloads:
                reply = daemon.call('POST', '/v1/chat/completions', payload)
                if reply['status'] not in (200, 400):
                    raise AssertionError(reply)
                output.append(reply)
            output.append(status(daemon))
        return normalize(output)


class HarnessContract(unittest.TestCase):
    def test_harness_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(run_case(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: run_case(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
