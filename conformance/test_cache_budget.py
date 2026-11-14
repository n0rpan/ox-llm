"""Persist cache budgets and exercise the real descriptor-anchored eviction path."""
import fcntl
import json
import os
from pathlib import Path
import stat
import sys
import time
import unittest

from harness import Daemon

GOLDENS = Path(__file__).with_name('goldens') / 'cache-budget.json'
CASES = ('empty', 'off-populated', 'expired-shared', 'pins-and-recent', 'ranked-budget',
         'opaque', 'invalid', 'lease-busy', 'invalid-root', 'config-symlink', 'loaded-engine', 'orphan-echo')
MODEL = 'a' * 16
CHECKSUM = 'e' * 16


def seed(daemon):
    root = daemon.home / '.cache/local-ai/snapdragon-engine/state'
    model = root / MODEL
    for name in ('sessions', 'snaps', 'echo', 'blocks/dd', 'blocks/bb'):
        (model / name).mkdir(parents=True, exist_ok=True)
    (model / 'identity.json').write_bytes(b'{}')
    return root, model


def block(model, name, size=None, old=False):
    digest = name * 64
    directory = model / 'blocks' / digest[:2]
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / (digest + '.' + CHECKSUM + '.kv')
    with path.open('wb') as stream:
        stream.write(b'synthetic-block')
        if size is not None:
            stream.truncate(size)  # sparse synthetic KV: the controller reads metadata only
    if old:
        os.utime(path, (1, 1))
    return [digest, CHECKSUM]


def manifest(model, name, blocks, *, atime=1234, pin=False, n_tokens=256, hint='c' * 32):
    value = {'version': 1, 'blocks': blocks, 'snaps': [], 'hint': hint,
             'atime': atime, 'pin': pin, 'n_tokens': n_tokens}
    (model / 'sessions' / (name + '.json')).write_text(json.dumps(value))
    (model / 'echo' / (hint + '.jsonl')).write_bytes(b'synthetic-echo\n')


def inventory(root):
    return {str(path.relative_to(root)): path.stat().st_size
            for path in sorted(root.rglob('*')) if path.is_file() and not path.is_symlink()}


def normalize(value):
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if not isinstance(value, dict):
        return value
    result = {}
    for key, item in value.items():
        if key in ('oldest_atime', 'newest_atime') and type(item) in (int, float) and item > 1_000_000_000:
            if not 0 <= time.time() - item <= 4000:
                raise AssertionError('Invalid recent session timestamp')
            result[key] = '<recent:unix-seconds>'
        else:
            result[key] = normalize(item)
    return result


def run_case(name):
    changes = {'cache': {'ttl_secs': 86400}, 'synthetic_extension': {'opaque': [7, 'synthetic']}}
    if name == 'invalid-root':
        changes['cache']['root'] = '/outside-synthetic-home'
    with Daemon(configured=True, config_changes=changes) as daemon:
        root, model = seed(daemon)
        if name == 'orphan-echo':
            old = model / 'echo/synthetic-old-group.jsonl'
            old.write_bytes(b'synthetic-old-echo\n')
            os.utime(old, (1, 1))
            (model / 'echo/synthetic-recent-group.jsonl').write_bytes(b'synthetic-recent-echo\n')
        if name in ('off-populated', 'expired-shared', 'pins-and-recent', 'ranked-budget',
                    'opaque', 'lease-busy', 'loaded-engine'):
            first = block(model, 'd', size=2_200_000_000 if name == 'ranked-budget' else None)
            manifest(model, 'first', [first], n_tokens=200)
            if name == 'expired-shared':
                manifest(model, 'second', [first], atime=int(time.time()) - 30, hint='f' * 32)
            if name == 'pins-and-recent':
                manifest(model, 'first', [first], pin=True)
                second = block(model, 'b')
                manifest(model, 'second', [second], atime=int(time.time()) - 30, hint='f' * 32)
            if name == 'ranked-budget':
                # Both are past the ten-minute pin but inside the TTL. The
                # cheaper first session loses; the more expensive second stays.
                second = block(model, 'b', size=2_200_000_000)
                manifest(model, 'first', [first], atime=1_000_000_000, n_tokens=200)
                manifest(model, 'second', [second], atime=1_000_000_000, n_tokens=1000, hint='f' * 32)
                config = json.loads(daemon.config_path.read_text())
                config['cache']['ttl_secs'] = 10 * 365 * 86400
                daemon.config_path.write_text(json.dumps(config))
                # A current atime, normalized in public usage, avoids TTL expiry.
                for path in (model / 'sessions').glob('*.json'):
                    value = json.loads(path.read_text())
                    value['atime'] = int(time.time()) - 3600
                    path.write_text(json.dumps(value))
            if name in ('off-populated', 'opaque'):
                block(model, 'b', old=True)
                (model / 'old.pending').write_bytes(b'synthetic-old-pending')
                os.utime(model / 'old.pending', (1, 1))
                (model / 'new.pending').write_bytes(b'synthetic-new-pending')
                (model / 'unknown.txt').write_bytes(b'synthetic-kept')
                if name == 'opaque':
                    (model / 'sessions/opaque.json').write_text('{"version":2}')
                outside = daemon.home / 'synthetic-outside-cache'
                outside.write_bytes(b'synthetic-outside-must-stay')
                (model / 'echo' / ('b' * 32 + '.jsonl')).symlink_to(outside)
                (root / ('b' * 16)).symlink_to(model, target_is_directory=True)
        output = []
        if name == 'invalid':
            for body in ({}, {'bytes': 0, 'extra': 1}, {'bytes': True}, {'bytes': -1}, {'bytes': 1},
                         {'bytes': 4_000_000_000.0}, {'bytes': '4000000000'}, {'bytes': None}, {'bytes': (1 << 50) + 1}):
                output.append(daemon.call('POST', '/_local/cache/budget', body))
        else:
            if name == 'config-symlink':
                target = daemon.home / 'synthetic-config-target'
                daemon.config_path.rename(target)
                target.chmod(0o640)
                daemon.config_path.symlink_to(target)
            if name == 'loaded-engine':
                if daemon.call('POST', '/_local/load', {})['status'] != 200:
                    raise AssertionError('Synthetic engine failed to load')
            descriptor = None
            try:
                if name == 'lease-busy':
                    path = daemon.home / 'state/local-ai/inference.lock'
                    path.parent.mkdir(parents=True, exist_ok=True)
                    descriptor = path.open('a')
                    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                budget = 0 if name in ('off-populated', 'opaque', 'lease-busy', 'loaded-engine') else 4_000_000_000
                output.append(daemon.call('POST', '/_local/cache/budget', {'bytes': budget}))
            finally:
                if descriptor is not None:
                    descriptor.close()
        if name != 'invalid-root':
            output.append(daemon.call('GET', '/_local/cache'))
        config = json.loads(daemon.config_path.read_text())
        output.append({'cache': config['cache'], 'extension': config['synthetic_extension'],
                       'config_mode': stat.S_IMODE(daemon.config_path.stat().st_mode),
                       'config_symlink': daemon.config_path.is_symlink(), 'files': inventory(root)})
        if name in ('off-populated', 'opaque'):
            if outside.read_bytes() != b'synthetic-outside-must-stay':
                raise AssertionError('Cache eviction followed a symlink')
        if name != 'loaded-engine' and any(event['event'] == 'started' for event in daemon.events()):
            raise AssertionError('Cache budget control started an engine')
        return normalize(output)


class CacheBudgetContract(unittest.TestCase):
    def test_budget_goldens(self):
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
