"""Real cache inventory with synthetic identities and a held file descriptor."""
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon, PRIMARY

GOLDENS = Path(__file__).with_name('goldens') / 'storage.json'
CASES = ('empty', 'populated', 'in-use', 'symlinks', 'malformed-runtime')
MODEL, SERVER = 'a' * 64, 'b' * 64
ARENA = MODEL + '-synthetic-' + 'e' * 7 + '-' + 'f' * 8 + '.bin'
OTHER = '9' * 64 + '-synthetic-' + 'e' * 7 + '-' + 'f' * 8 + '.bin'


def seed(home, symlinks=False):
    root = home / '.cache/local-ai'
    engine = root / 'snapdragon-engine'
    engine.mkdir(parents=True)
    (engine / ARENA).write_bytes(b'synthetic-arena')
    (engine / OTHER).write_bytes(b'synthetic-unreferenced-arena')
    (engine / 'not-a-cache.bin').write_bytes(b'synthetic-ignored')
    identity = ('model=sha256:' + MODEL + ' server=sha256:' + SERVER).encode()
    prefix = engine / 'prefix' / ('c' * 64 + '.bin')
    prefix.parent.mkdir()
    prefix.write_bytes(b'OXEPFX01' + len(identity).to_bytes(4, 'little') + identity + b'synthetic-state')
    sidecar = engine / 'flash'
    sidecar.mkdir()
    (sidecar / 'synthetic').write_bytes(b'synthetic-sidecar')
    classic = root / 'prefixes' / ('d' * 64 + '.json')
    classic.parent.mkdir()
    classic.write_text(json.dumps({'model_sha256': 'c' * 64, 'server_sha256': 'd' * 64}))
    classic.with_suffix('.bin').write_bytes(b'synthetic-classic-prefix')
    repack = root / 'repack' / ('synthetic.gguf-' + 'e' * 12)
    repack.mkdir(parents=True)
    (repack / 'synthetic').write_bytes(b'synthetic-repack')
    if symlinks:
        (engine / OTHER).unlink()
        (engine / OTHER).symlink_to(engine / ARENA)
        prefix.unlink()
        prefix.symlink_to(engine / ARENA)
        (classic.parent / ('e' * 64 + '.json')).symlink_to(classic)
        (sidecar / 'synthetic').unlink()
        sidecar.rmdir()
        sidecar.symlink_to(repack, target_is_directory=True)
    return engine / OTHER


def run_case(name):
    profile = {'sha256': MODEL, 'server_sha256': SERVER,
               'rust_engine': {'oxe': {'sha256': MODEL, 'server_sha256': SERVER}}}
    with Daemon(configured=True, config_changes={'profiles': {PRIMARY: profile}}) as daemon:
        held = None
        try:
            if name == 'malformed-runtime':
                daemon.config_path.write_text('{invalid synthetic JSON')
            elif name != 'empty':
                path = seed(daemon.home, symlinks=name == 'symlinks')
                if name == 'in-use':
                    held = path.open('rb')
            reply = daemon.call('GET', '/_local/storage')
            if any(row['event'] == 'started' for row in daemon.events()):
                raise AssertionError('Storage inventory loaded an engine')
            return reply
        finally:
            if held is not None:
                held.close()


class StorageContract(unittest.TestCase):
    def test_inventory_goldens(self):
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
