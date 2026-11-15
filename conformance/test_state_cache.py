"""Read the real state-cache formats without loading an engine or reading KV bytes."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

from harness import Daemon

GOLDENS = Path(__file__).with_name('goldens') / 'state-cache.json'
CASES = ('empty', 'populated', 'invalid-settings', 'root-outside-home',
         'config-root', 'environment-root', 'symlink-root-outside-home', 'disk-tier-off', 'malformed-runtime')


def seed(home, root=None):
    root = root or home / '.cache/local-ai/snapdragon-engine/state'
    model = root / ('a' * 16)
    block = 'd' * 64
    checksum = 'e' * 16
    hint = 'c' * 32
    files = {
        'identity.json': b'{}',
        'blocks/dd/' + block + '.' + checksum + '.kv': b'synthetic-block',
        'snaps/synthetic.' + checksum + '.st': b'synthetic-snapshot',
        'echo/' + hint + '.jsonl': b'synthetic-echo\n',
        'sessions/synthetic.json': json.dumps({'version': 1, 'blocks': [[block, checksum]],
            'snaps': [{'key': 'synthetic', 'c': checksum}], 'hint': hint,
            'n_tokens': 256, 'atime': 1234, 'pin': False}).encode(),
        'old.pending': b'synthetic-old-pending',
        'new.pending': b'synthetic-new-pending',
        'unknown.txt': b'synthetic-unknown',
    }
    for relative, data in files.items():
        path = model / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    os.utime(model / 'old.pending', (1, 1))
    # All three have store-shaped names, but symlinks must never be followed.
    (root / ('b' * 16)).symlink_to(model, target_is_directory=True)
    (model / 'sessions/symlink.json').symlink_to(model / 'sessions/synthetic.json')
    (model / 'blocks/dd' / (block + '.' + ('f' * 16) + '.kv')).symlink_to(model / 'identity.json')
    opaque = root / ('f' * 16) / 'sessions/unknown.json'
    opaque.parent.mkdir(parents=True)
    opaque.write_bytes(b'{"version":2}')


def run_case(name):
    changes = {'cache': {'disk_budget_bytes': True, 'ttl_secs': 'invalid'}} if name == 'invalid-settings' else {}
    if name == 'root-outside-home':
        changes = {'cache': {'root': '/outside-synthetic-home'}}
    with tempfile.TemporaryDirectory(prefix='oxc-cache-conformance-') as temporary:
        home = Path(temporary)
        override = home / 'alternate-conversations'
        environment = {}
        if name == 'config-root':
            changes = {'cache': {'root': str(override)}}
        elif name == 'environment-root':
            changes = {'cache': {'root': '/outside-synthetic-home'}}
            environment = {'OXC_STATE_CACHE_ROOT': str(override)}
        elif name == 'symlink-root-outside-home':
            (home / 'escape').symlink_to('/outside-synthetic-home', target_is_directory=True)
            changes = {'cache': {'root': str(home / 'escape')}}
        elif name in ('disk-tier-off', 'malformed-runtime'):
            changes = {'cache': {'disk_budget_bytes': 0, 'ttl_secs': 60}}
        with Daemon(configured=True, home=home, config_changes=changes, environ=environment) as daemon:
            if name == 'malformed-runtime':
                daemon.config_path.write_text('{invalid synthetic JSON')
            if name in ('populated', 'invalid-settings'):
                seed(home)
            elif name in ('config-root', 'environment-root'):
                seed(home, override / 'state')
            reply = daemon.call('GET', '/_local/cache')
            if any(row['event'] == 'started' for row in daemon.events()):
                raise AssertionError('Cache usage loaded an engine')
            return reply


class StateCacheContract(unittest.TestCase):
    def test_usage_goldens(self):
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
