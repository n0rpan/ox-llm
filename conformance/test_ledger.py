"""Request ledger contracts through HTTP and private on-disk files only."""
import hashlib
import hmac
import http.client
import json
import os
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import re
import stat
import sys
import time
import unittest

from harness import Daemon, PRIMARY
from test_api import chat, completion, TOOLS
from test_lifecycle import status, wait_until

GOLDENS = Path(__file__).with_name('goldens') / 'ledger.json'
CASES = ('empty', 'stored', 'json', 'stream', 'session-hints', 'history-text',
         'history-tools', 'errors', 'disabled', 'invalid-salt', 'retention', 'unwritable')
SALT = bytes(range(32))
SYSTEM = 'Synthetic private agent — λ.'
USER = 'Synthetic private ledger prompt.'
HEADERS = {'User-Agent': 'SyntheticClient/1.2 trailing text', 'X-Session-Id': 'synthetic-session',
           'X-Local-AI-Origin': ' LAB ', 'X-Oxe-Turn-Kind': ' branch_summary '}


def directory(daemon):
    return daemon.home / '.local/state/local-ai/ledger'


def seed(daemon, salt=SALT, mode=0o600):
    root = directory(daemon)
    root.mkdir(parents=True, exist_ok=True)
    root.chmod(0o700)
    (root / '.salt').write_bytes(salt)
    (root / '.salt').chmod(mode)
    return root


def ledger(daemon):
    reply = daemon.call('GET', '/_local/ledger')
    if reply['status'] != 200:
        raise AssertionError(reply)
    return reply['body']


def send(daemon, payload=None, headers=HEADERS, path='/v1/chat/completions', expected=200):
    if payload is None:
        payload = chat(messages=[{'role': 'system', 'content': SYSTEM}, {'role': 'user', 'content': USER}])
    reply = daemon.call('POST', path, payload, headers)
    if reply['status'] != expected:
        raise AssertionError(reply)
    wait_until(lambda: status(daemon)['active_requests'] == 0)
    return reply


def normalize(value):
    ids = {}
    durations = {'ctl_prep_ms', 'ctl_backend_ms', 'queue_wait_ms', 'admit_ms', 'ttfb_ms', 'wall_ms', 'gap_ms'}

    def visit(value):
        if isinstance(value, list):
            return [visit(item) for item in value]
        if not isinstance(value, dict):
            return value
        out = {}
        for key, item in value.items():
            if key in ('id', 'previous_request_id') and isinstance(item, str):
                if not re.fullmatch('[0-9a-f]{12}', item):
                    raise AssertionError('Invalid ledger request id')
                ids.setdefault(item, '<ledger-id:%d>' % len(ids))
                out[key] = ids[item]
            elif key == 't' and type(item) in (int, float) and item > 1_000_000_000:
                if abs(time.time() - item) > 30:
                    raise AssertionError('Invalid ledger timestamp')
                out[key] = '<current:unix-seconds>'
            elif key in durations and item is not None:
                if type(item) not in (int, float) or not 0 <= item <= 30_000:
                    raise AssertionError('Invalid ledger duration: %s' % key)
                out[key] = '<elapsed:milliseconds>'
            else:
                out[key] = visit(item)
        return out

    return visit(value)


def persistence(daemon):
    root = directory(daemon)
    if not root.is_dir():
        return {'files': []}
    files = sorted(root.glob('requests-*.jsonl'))
    raw = b''.join(path.read_bytes() for path in files)
    for text in (SYSTEM, USER, 'Synthetic engine reply.', 'synthetic-session'):
        if text.encode() in raw:
            raise AssertionError('Private request or reply text reached the ledger')
    if files and (stat.S_IMODE(root.stat().st_mode) != 0o700 or
                  any(stat.S_IMODE(path.stat().st_mode) != 0o600 for path in files)):
        raise AssertionError('Ledger files are not private')
    return {'files': [path.name for path in files],
            'rows': [json.loads(line) for path in files for line in path.read_text().splitlines()]}


def run_case(name):
    environment = {'LOCAL_AI_LEDGER': '0'} if name == 'disabled' else {}
    with Daemon(configured=True, environ=environment) as daemon:
        output = []
        if name == 'empty':
            output.append(ledger(daemon))
            output.append({'exists': directory(daemon).exists(), 'events': daemon.events()})
            return normalize(output)
        root = seed(daemon, mode=0o644 if name == 'invalid-salt' else 0o600)
        if name == 'stored':
            now = time.time()
            rows = [{'t': 10, 'status': 200, 'queue_wait_ms': 1, 'wall_ms': 7,
                     'prompt_n': 10, 'prompt_ms': 2, 'predicted_n': 8, 'predicted_ms': 4},
                    {'t': 11, 'status': 503, 'wall_ms': 999},
                    {'t': 12, 'status': 200, 'queue_wait_ms': 3, 'wall_ms': 9,
                     'prompt_n': 20, 'prompt_ms': 4, 'predicted_n': 10, 'predicted_ms': 5},
                    {'t': 13, 'status': 200, 'queue_wait_ms': True, 'wall_ms': None}]
            today = root / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')
            yesterday = root / ('requests-' + time.strftime('%Y%m%d', time.localtime(now - 86400)) + '.jsonl')
            yesterday.write_text(json.dumps(rows[0]) + '\n')
            today.write_text(json.dumps(rows[1]) + '\nmalformed\n[]\n' +
                             '\n'.join(json.dumps(row) for row in rows[2:]) + '\n')
            output.append(ledger(daemon))
            output.append({'events': daemon.events()})
            return output
        if name == 'retention':
            old = root / 'requests-20000101.jsonl'
            old.write_text('{}\n')
            os.utime(old, (time.time() - 30 * 86400,) * 2)
            unrelated = root / 'synthetic-unrelated.jsonl'
            unrelated.write_text('{}\n')
        if name == 'unwritable':
            root.rename(root.with_name('ledger-saved'))
            root.write_text('synthetic obstruction')
        if name in ('json', 'stream', 'disabled', 'invalid-salt', 'retention', 'unwritable'):
            send(daemon, chat(messages=[{'role': 'system', 'content': SYSTEM},
                 {'role': 'user', 'content': USER}, {'role': 'assistant', 'content': None,
                  'tool_calls': [{'id': 'call-synthetic', 'type': 'function', 'function': {'name': 'first_tool', 'arguments': '{}'}}]},
                 {'role': 'tool', 'tool_call_id': 'call-synthetic', 'content': 'Synthetic tool — λ.'}],
                 tools=TOOLS, stream=name == 'stream', temperature=0.25, max_tokens=7))
            output.append(ledger(daemon))
            if name == 'retention':
                output.append({'old_exists': old.exists(), 'unrelated_exists': unrelated.exists()})
        elif name == 'session-hints':
            for headers, extra in [({'X-Session-Id': 'synthetic-session', 'Session-Id': 'ignored'}, {}),
                                   ({'Session-Id': 'synthetic-session'}, {}),
                                   ({}, {'prompt_cache_key': 'synthetic-session'}),
                                   ({'X-Session-Id': 'x' * 201}, {}), ({}, {}),
                                   ({'X-Local-AI-Origin': 'remote'}, {})]:
                send(daemon, chat(**extra), headers=headers)
            output.append(ledger(daemon))
        elif name in ('history-text', 'history-tools'):
            messages = [{'role': 'system', 'content': SYSTEM}, {'role': 'user', 'content': USER}]
            extra = {'tools': TOOLS} if name == 'history-tools' else {}
            first = send(daemon, chat(messages=messages, **extra))
            reply = first['body']['choices'][0]['message']
            # Every state is compared with the last complete input, not a transient failure.
            send(daemon, chat(messages=messages, **extra))
            messages = [*messages, reply, {'role': 'user', 'content': 'Synthetic next turn.'}]
            send(daemon, chat(messages=messages, **extra))
            edited = json.loads(json.dumps(messages))
            edited[1]['content'] = 'Synthetic edited user turn.'
            send(daemon, chat(messages=edited, **extra))
            send(daemon, chat(messages=edited[:2], **extra))
            edited_reply = json.loads(json.dumps(reply))
            edited_reply['content'] = 'Synthetic edited reply.'
            send(daemon, chat(messages=[*edited[:2], edited_reply, {'role': 'user', 'content': 'Synthetic next turn.'}], **extra))
            output.append(ledger(daemon))
        elif name == 'errors':
            send(daemon)
            send(daemon, chat(n=2), expected=400)  # rejected before a ledger entry exists
            send(daemon, chat(messages=[{'role': 'user', 'content': '[upstream-error]'}]), expected=422)
            send(daemon)
            send(daemon, completion(), path='/v1/completions')
            send(daemon, {'model': PRIMARY, 'input': USER}, path='/v1/responses')
            output.append(ledger(daemon))
        if name != 'stored':
            saved = persistence(daemon)
            # File names contain the current local calendar day; validate before normalizing.
            saved['files'] = ['<today>' if name == 'requests-' + time.strftime('%Y%m%d') + '.jsonl' else name
                              for name in saved['files']]
            output.append(saved)
        if name == 'json':
            row = output[0]['rows'][0]
            expected_session = hmac.new(SALT, b'h:synthetic-session', hashlib.sha256).hexdigest()[:16]
            expected_agent = hmac.new(SALT, b'ledger-agent-v1\0' + SYSTEM.encode(), hashlib.sha256).hexdigest()[:12]
            if row['session'] != expected_session or row['agent'] != expected_agent:
                raise AssertionError('Ledger hashes do not use the private install salt')
        return normalize(output)


class LedgerContract(unittest.TestCase):
    def test_ledger_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(run_case(name), expected[name])

    def test_recent_limit_decoding_clamping_and_duplicate_parameters(self):
        with Daemon(configured=True) as daemon:
            root = seed(daemon)
            path = root / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')
            path.write_text(''.join(json.dumps({'synthetic_index': n}) + '\n' for n in range(10)))
            for query, count in (('', 10), ('?limit=1', 1), ('?limit=0', 1),
                                 ('?limit=201', 10), ('?limit=-1', 10), ('?limit=10000', 10),
                                 ('?limit=0001', 1), ('?limit=%31', 1),
                                 ('?limit=1&limit=2', 2), ('?limit=1&limit=', 1)):
                with self.subTest(query=query):
                    reply = daemon.call('GET', '/_local/ledger' + query)
                    self.assertEqual(reply['status'], 200)
                    self.assertEqual([row['synthetic_index'] for row in reply['body']['rows']],
                                     list(range(9, 9 - count, -1)))
            self.assertEqual(daemon.events(), [])

    def test_engine_hints_are_keyed_and_only_valid_client_hints_survive(self):
        with Daemon(configured=True, environ={'FAKE_ENGINE_HEADERS': '1'}) as daemon:
            seed(daemon)
            send(daemon, headers={**HEADERS, 'X-Oxe-Session': 'synthetic-forged-hint',
                 'X-Oxe-Snapshot': ' KeEp ', 'X-Oxe-Probe': 'synthetic-drop'})
            send(daemon, headers={**HEADERS, 'X-Oxe-Turn-Kind': 'invalid /label',
                 'X-Oxe-Snapshot': 'no', 'X-Oxe-Session': 'synthetic-forged-hint'})
            rows = ledger(daemon)['rows']
            hints = [event['values'] for event in daemon.events() if event['event'] == 'hints']
            self.assertEqual(hints, [
                {'X-Oxe-Session': rows[1]['session'], 'X-Oxe-Turn-Kind': 'branch_summary',
                 'X-Oxe-Snapshot': 'keep', 'X-Oxe-Probe': None},
                {'X-Oxe-Session': rows[0]['session'], 'X-Oxe-Turn-Kind': None,
                 'X-Oxe-Snapshot': None, 'X-Oxe-Probe': None}])
            self.assertNotIn('turn_kind', rows[0])

    def test_bad_salts_have_no_unkeyed_fallback_and_do_not_fail_inference(self):
        for kind in ('short', 'symlink'):
            with self.subTest(kind=kind), Daemon(configured=True) as daemon:
                root = seed(daemon, salt=b'synthetic-short-salt' if kind == 'short' else SALT)
                if kind == 'symlink':
                    (root / '.salt').rename(root / 'synthetic-salt-target')
                    (root / '.salt').symlink_to('synthetic-salt-target')
                send(daemon, chat(messages=[{'role': 'system', 'content': SYSTEM},
                     {'role': 'user', 'content': USER}], tools=TOOLS))
                row = ledger(daemon)['rows'][0]
                for field in ('agent', 'session', 'gap_ms', 'tools_hash'):
                    self.assertIsNone(row[field])
                self.assertNotIn('history_change', row)
                self.assertEqual(row['status'], 200)
                self.assertEqual((root / '.salt').read_bytes(), b'synthetic-short-salt' if kind == 'short' else SALT)
                persistence(daemon)

    def test_header_identity_uses_http_latin1_and_keeps_only_product_token(self):
        with Daemon(configured=True) as daemon:
            seed(daemon)
            send(daemon, headers={'X-Session-Id': 'synthetic-séssion',
                                 'User-Agent': 'SyntheticClient/1.2 é trailing text'})
            row = ledger(daemon)['rows'][0]
            expected = hmac.new(SALT, 'h:synthetic-séssion'.encode(), hashlib.sha256).hexdigest()[:16]
            self.assertEqual(row['session'], expected)
            self.assertEqual(row['client'], 'SyntheticClient/1.2')
            self.assertEqual(row['harness'], 'SyntheticClient')

    def test_ledger_directory_override_relative_and_empty(self):
        for configured in ('synthetic-ledger', ''):
            with self.subTest(directory=configured), Daemon(configured=True, environ={
                    'LOCAL_AI_LEDGER_DIR': configured}) as daemon:
                root = daemon.home / (configured or '.')
                root.mkdir(parents=True, exist_ok=True)
                (root / '.salt').write_bytes(SALT)
                (root / '.salt').chmod(0o600)
                send(daemon)
                rows = ledger(daemon)['rows']
                self.assertEqual(len(rows), 1)
                self.assertRegex(rows[0]['session'], '^[0-9a-f]{16}$')
                self.assertEqual(len(list(root.glob('requests-*.jsonl'))), 1)
                self.assertFalse(directory(daemon).exists())

    def test_recent_reads_existing_file_symlink_like_reference(self):
        # Bug-for-bug: the reader follows an existing daily-file symlink. Salt
        # files have a separate, stricter no-follow contract tested above.
        with Daemon(configured=True) as daemon:
            root = seed(daemon)
            target = daemon.home / 'synthetic-ledger-rows'
            target.write_text('{"synthetic_index":7}\n')
            (root / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')).symlink_to(target)
            self.assertEqual(ledger(daemon)['rows'], [{'synthetic_index': 7}])
            self.assertEqual(daemon.events(), [])

    def test_cancelled_request_is_recorded_and_slot_released(self):
        with Daemon(configured=True) as daemon:
            seed(daemon)
            connection = http.client.HTTPConnection('127.0.0.1', daemon.port, timeout=5)
            connection.request('POST', '/v1/chat/completions', json.dumps(chat(
                messages=[{'role': 'user', 'content': '[hold]'}])),
                {**HEADERS, 'Content-Type': 'application/json'})
            try:
                wait_until(lambda: any(event['event'] == 'held' for event in daemon.events()))
            finally:
                connection.close()
            row = wait_until(lambda: ledger(daemon).get('rows'))[0]
            self.assertEqual(row['status'], 503)
            self.assertEqual(row['error'], 'client_disconnected')
            self.assertEqual(row['completeness'], 'errored')
            self.assertNotIn('ttfb_ms', row)
            self.assertEqual(status(daemon)['active_requests'], 0)
            (daemon.home / 'engine-gate').touch()
            send(daemon)
            self.assertEqual(len(ledger(daemon)['rows']), 2)

    def test_unload_waits_for_completed_request_ledger_bookkeeping(self):
        with Daemon(configured=True) as daemon:
            root = seed(daemon)
            path = root / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')
            os.mkfifo(path, 0o600)
            descriptor = None
            with ThreadPoolExecutor(max_workers=2) as pool:
                try:
                    request = pool.submit(daemon.call, 'POST', '/v1/chat/completions', chat(), HEADERS)
                    self.assertEqual(request.result(timeout=5)['status'], 200)
                    self.assertEqual(status(daemon)['active_requests'], 1)
                    unload = pool.submit(daemon.call, 'POST', '/_local/unload', {})
                    time.sleep(0.1)
                    self.assertFalse(unload.done(), 'Unload refused after forwarding had completed')
                finally:
                    # Always release a writer blocked opening the private FIFO, including
                    # assertion/error paths, before waiting for the process or thread pool.
                    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
                    data = bytearray()

                    def read_record():
                        try:
                            data.extend(os.read(descriptor, 16384))
                        except BlockingIOError:
                            pass
                        return data if data.endswith(b'\n') else None

                    try:
                        wait_until(read_record)
                    finally:
                        os.close(descriptor)
                row = json.loads(data)
                self.assertEqual(row['status'], 200)
                self.assertIsNone(row['error'])
                self.assertEqual(unload.result(timeout=5)['status'], 200)
            self.assertEqual(status(daemon)['state'], 'unloaded')
            self.assertEqual(status(daemon)['active_requests'], 0)


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: run_case(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
