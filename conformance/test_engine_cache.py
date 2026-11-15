"""Cache ownership, private backend controls and preload with an opt-in engine peer."""
import json
import os
from pathlib import Path
import stat
import sys
import time
import unittest

from harness import Daemon, PRIMARY
from test_api import chat
from test_cache_budget import seed, block, manifest, inventory
from test_ledger import seed as seed_ledger
from test_lifecycle import status, wait_until

GOLDENS = Path(__file__).with_name('goldens') / 'engine-cache.json'
CASES = ('owned-controls', 'disk-off', 'override', 'preload', 'preload-no-marker', 'preload-memory', 'faults', 'crashed-owner', 'preload-async')


def normalize(value, home):
    if isinstance(value, list):
        return [normalize(item, home) for item in value]
    if isinstance(value, str):
        return value.replace(str(home), '<home>')
    if not isinstance(value, dict):
        return value
    out = {}
    for key, item in value.items():
        if key == 'age_s' and item is not None:
            if type(item) is not int or not 0 <= item <= 30:
                raise AssertionError('Invalid preload age')
            out[key] = '<elapsed:seconds>'
        else:
            out[key] = normalize(item, home)
    return out


def run_case(name):
    preload = name in ('preload', 'preload-memory', 'preload-async')
    environment = {'FAKE_ENGINE_STATE_CACHE': '1', 'FAKE_ENGINE_STATE_PRELOAD': '1' if preload else '0'}
    if name == 'preload-memory':
        environment['LOCAL_AI_LEDGER'] = '0'
    changes = {'cache': {'disk_budget_bytes': 0 if name == 'disk-off' else 64_000_000_000, 'ttl_secs': 60}}
    with Daemon(configured=True, config_changes=changes, environ=environment) as daemon:
        ledger = seed_ledger(daemon)
        root, model = seed(daemon)
        first = block(model, 'd')
        manifest(model, 'first', [first])
        if name == 'override':
            config = json.loads(daemon.config_path.read_text())
            config['cache']['root'] = str(daemon.home / 'cache-override')
            daemon.config_path.write_text(json.dumps(config))
            root = daemon.home / 'cache-override/state'
        if name == 'preload':
            (ledger / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')).write_text(
                json.dumps({'model': PRIMARY, 'session': 'd' * 16}) + '\n')
        output = [daemon.call('GET', '/_local/cache'), {'preload': status(daemon)['preload']}]
        load = daemon.call('POST', '/_local/preload' if name == 'preload-async' else '/_local/load', {})
        if load['status'] != (202 if name == 'preload-async' else 200):
            raise AssertionError(load)
        output.append(load)
        if preload:
            wait_until(lambda: status(daemon).get('preload', {}).get('state') == 'done')
        output.append({'preload': status(daemon)['preload']})
        output.append(daemon.call('GET', '/_local/cache'))
        before = inventory(root)
        if name == 'owned-controls':
            output.append(daemon.call('POST', '/_local/cache/budget', {'bytes': 4_000_000_000}))
            for body in ({}, {'scope': 'model', 'model': 'a' * 16}, {'scope': 'session', 'session': 'first'},
                         {'scope': 'hint', 'hint': 'synthetic-session'}):
                output.append(daemon.call('POST', '/_local/cache/clear', body))
            output.append(daemon.call('GET', '/_local/cache'))
            if inventory(root) != before:
                raise AssertionError('Controller mutated engine-owned state')
        elif name == 'faults':
            fault = daemon.home / 'engine-cache-fault'
            for mode in ('error', 'malformed', 'oversized'):
                fault.write_text(mode)
                output.append(daemon.call('GET', '/_local/cache'))
                output.append(daemon.call('POST', '/_local/cache/budget', {'bytes': 4_000_000_000}))
                output.append(daemon.call('POST', '/_local/cache/clear', {}))
            fault.unlink()
            output.append(daemon.call('GET', '/_local/cache'))
            output.append({'saved_budget': json.loads(daemon.config_path.read_text())['cache']['disk_budget_bytes']})
            if inventory(root) != before:
                raise AssertionError('Controller trimmed engine-owned state after an upstream failure')
        elif name in ('preload', 'preload-memory'):
            if name == 'preload-memory':
                reply = daemon.call('POST', '/v1/chat/completions', chat(), {'X-Session-Id': 'synthetic-preload-session'})
                if reply['status'] != 200:
                    raise AssertionError(reply)
                wait_until(lambda: status(daemon)['active_requests'] == 0)
                output.append(daemon.call('POST', '/_local/unload', {}))
                output.append(daemon.call('POST', '/_local/load', {}))
                wait_until(lambda: status(daemon).get('preload', {}).get('state') == 'done')
                if list(ledger.glob('requests-*.jsonl')):
                    raise AssertionError('Disabled ledger wrote a request row')
            else:
                output.append(daemon.call('POST', '/_local/load', {}))  # reuse sends no new preload
            output.append({'preload': status(daemon)['preload']})
        elif name == 'crashed-owner':
            pid = next(event['pid'] for event in daemon.events() if event['event'] == 'started')
            if str(daemon.home / 'fake-engine').encode() not in Path('/proc', str(pid), 'cmdline').read_bytes().split(b'\0'):
                raise AssertionError('Refusing to signal an unrelated process')
            os.kill(pid, 9)

            def exited():
                try:
                    return Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()[0] == 'Z'
                except FileNotFoundError:
                    return True

            wait_until(exited)
            reply = daemon.call('POST', '/_local/cache/budget', {'bytes': 0})
            if reply['body'].get('applied_by') != 'controller':
                raise AssertionError('A dead engine retained cache ownership: ' + repr(reply))
            output.append(reply)
            output.append(daemon.call('GET', '/_local/cache'))
        starts = [event.get('state_cache') for event in daemon.events() if event['event'] == 'started']
        output.append({'launches': starts, 'state_mode': stat.S_IMODE(root.stat().st_mode) if root.exists() else None})
        output.append({'backend_posts': [{key: event[key] for key in ('path', 'body')} for event in daemon.events()
                                         if event['event'] == 'request' and event.get('method') == 'POST']})
        return normalize(output, daemon.home)


class EngineCacheContract(unittest.TestCase):
    def test_engine_cache_goldens(self):
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
