"""Scoped cache deletion, shared ownership, private leases and retained primers."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
import hashlib
import hmac
import json
import os
from pathlib import Path
import stat
import sys
import unittest

from harness import Daemon
from test_api import chat
from test_cache_budget import MODEL, CHECKSUM, block, manifest, seed
from test_harnesses import normalize, send, status as harness_status
from test_ledger import SALT, seed as seed_ledger
from test_lifecycle import status, wait_until

GOLDENS = Path(__file__).with_name('goldens') / 'cache-clear.json'
CASES = ('empty', 'all', 'model', 'session-shared', 'hint-shared-owners', 'hint-anonymous',
         'hint-only-owner', 'opaque', 'invalid', 'lease-busy', 'busy', 'legacy-prefix', 'harness-warm', 'manifest-float-version',
         'classic-prefix', 'classic-prefix-symlink', 'classic-prefix-invalid-budget', 'classic-prefix-model-scope')


def hint(name):
    session = hmac.new(SALT, ('h:' + name).encode(), hashlib.sha256).hexdigest()[:16]
    return hashlib.sha256(session.encode()).hexdigest()[:32]


def inventory(root):
    return {'files': {str(path.relative_to(root)): {'bytes': path.stat().st_size,
                     'mode': stat.S_IMODE(path.stat().st_mode),
                     **({'manifest': json.loads(path.read_text())} if path.parent.name == 'sessions'
                        and path.suffix == '.json' and not path.is_symlink() else {})}
                     for path in sorted(root.rglob('*')) if path.is_file() and not path.is_symlink()},
            'directories': [str(path.relative_to(root)) for path in sorted(root.rglob('*'))
                            if path.is_dir() and not path.is_symlink()]}


def run_case(name):
    changes = {'harness_learning': {'enabled': True, 'min_requests': 1}} if name == 'harness-warm' else {}
    with Daemon(configured=True, config_changes=changes) as daemon:
        seed_ledger(daemon)
        root, model = seed(daemon)
        first_hint, second_hint = hint('synthetic-first'), hint('synthetic-second')
        if name != 'empty':
            first = block(model, 'd')
            manifest(model, 'first', [first], hint=first_hint)
            (model / 'snaps' / ('synthetic.' + CHECKSUM + '.st')).write_bytes(b'synthetic-snapshot')
            (model / 'unknown.txt').write_bytes(b'synthetic-kept')
            outside = daemon.home / 'synthetic-outside-cache'
            outside.write_bytes(b'synthetic-outside-kept')
            (model / 'echo' / ('b' * 32 + '.jsonl')).symlink_to(outside)
            if name == 'session-shared':
                manifest(model, 'second', [first], hint=second_hint)
            if name in ('hint-shared-owners', 'hint-anonymous'):
                path = model / 'sessions/first.json'
                value = json.loads(path.read_text())
                value['owners'] = [first_hint, '-' if name == 'hint-anonymous' else second_hint, first_hint]
                path.write_text(json.dumps(value))
                (model / 'echo' / (second_hint + '.jsonl')).write_bytes(b'synthetic-other-owner-echo\n')
            if name == 'opaque':
                (model / 'sessions/opaque.json').write_text('{"version":2}')
            if name == 'manifest-float-version':
                path = model / 'sessions/first.json'
                value = json.loads(path.read_text())
                value['version'] = 1.0  # Python's version comparison accepts this, as it accepts True.
                path.write_text(json.dumps(value))
            if name in ('all', 'model'):
                other = root / ('e' * 16)
                for sub in ('sessions', 'echo'):
                    (other / sub).mkdir(parents=True, exist_ok=True)
                (other / 'identity.json').write_bytes(b'{}')
                second = block(other, 'b')
                manifest(other, 'second', [second], hint=second_hint)
        if name == 'legacy-prefix':
            legacy = root.parent / 'prefix'
            legacy.mkdir(parents=True)
            identity = ('model=sha256:' + 'a' * 64 + ' server=sha256:' + 'b' * 64).encode()
            (legacy / ('c' * 64 + '.bin')).write_bytes(b'OXEPFX01' + len(identity).to_bytes(4, 'little') + identity + b'synthetic-state')
            (legacy / ('d' * 64 + '.bin')).write_bytes(b'synthetic-invalid-prefix')
            (legacy / 'synthetic-unknown.bin').write_bytes(b'synthetic-kept')
        if name.startswith('classic-prefix'):
            classic = daemon.home / 'classic-prefixes'
            classic.mkdir(mode=0o755)
            for suffix in ('bin', 'json', 'pending'):
                path = classic / ('a' * 64 + '.' + suffix)
                path.write_bytes(b'synthetic-classic-state' if suffix != 'json' else b'{}')
                path.chmod(0o600)
            (classic / ('c' * 64 + '.bin')).symlink_to(outside)
            (classic / 'synthetic-unknown.bin').write_bytes(b'synthetic-kept')
            if name == 'classic-prefix-symlink':
                target = classic.with_name('synthetic-classic-target')
                classic.rename(target)
                classic.symlink_to(target, target_is_directory=True)
            config = json.loads(daemon.config_path.read_text())
            config['prefix_cache'] = {'enabled': True, 'directory': str(classic),
                                     'max_bytes': 0 if name == 'classic-prefix-invalid-budget' else 1}
            daemon.config_path.write_text(json.dumps(config))
        output = []
        if name == 'harness-warm':
            send(daemon)
            output.append(harness_status(daemon))
        if name == 'invalid':
            for body in ({'scope': 'unknown'}, {'scope': 'model'}, {'scope': 'model', 'model': '../synthetic'},
                         {'scope': 'session'}, {'scope': 'session', 'session': '../synthetic'},
                         {'scope': 'hint'}, {'scope': 'hint', 'hint': 'invalid /hint'},
                         {'scope': 'all', 'hint': 'synthetic-first'}, {'extra': True}):
                output.append(daemon.call('POST', '/_local/cache/clear', body))
        elif name == 'busy':
            with ThreadPoolExecutor(max_workers=1) as pool:
                request = pool.submit(daemon.call, 'POST', '/v1/chat/completions', chat(
                    messages=[{'role': 'user', 'content': '[hold]'}]))
                try:
                    wait_until(lambda: any(event['event'] == 'held' for event in daemon.events()))
                    output.append(daemon.call('POST', '/_local/cache/clear', {}))
                finally:
                    (daemon.home / 'engine-gate').touch()
                if request.result(timeout=5)['status'] != 200:
                    raise AssertionError('Held synthetic request failed')
            wait_until(lambda: status(daemon)['active_requests'] == 0)
        else:
            body = {'scope': 'model', 'model': MODEL} if name in ('model', 'classic-prefix-model-scope') else \
                   {'scope': 'session', 'session': 'first'} if name in ('session-shared', 'opaque', 'manifest-float-version') else \
                   {'scope': 'hint', 'hint': 'synthetic-first'} if name.startswith('hint-') else {}
            descriptor = None
            try:
                if name == 'lease-busy':
                    path = daemon.home / 'state/oxc/inference.lock'
                    path.parent.mkdir(parents=True, exist_ok=True)
                    descriptor = path.open('a')
                    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                output.append(daemon.call('POST', '/_local/cache/clear', body))
            finally:
                if descriptor is not None:
                    descriptor.close()
        output.append(daemon.call('GET', '/_local/cache'))
        output.append(inventory(root))
        if name == 'legacy-prefix':
            output.append({'legacy_files': {path.name: path.stat().st_size for path in sorted(legacy.iterdir())}})
        if name.startswith('classic-prefix'):
            output.append({'classic_mode': stat.S_IMODE(classic.stat().st_mode),
                           'classic_symlink': classic.is_symlink(),
                           'classic_files': [path.name for path in sorted(classic.iterdir())]})
        if name == 'harness-warm':
            output.append(harness_status(daemon))
        if name != 'empty' and outside.read_bytes() != b'synthetic-outside-kept':
            raise AssertionError('Scoped clear followed a symlink')
        if name not in ('busy', 'harness-warm') and any(event['event'] == 'started' for event in daemon.events()):
            raise AssertionError('Cache clear started an engine')
        return normalize(output)


class CacheClearContract(unittest.TestCase):
    def test_clear_goldens(self):
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
