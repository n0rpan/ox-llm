"""Config bytes, permissions and symlinks through model-free public controls."""
import json
import os
from pathlib import Path
import stat
import sys
import unittest

from harness import Daemon

GOLDENS = Path(__file__).with_name('goldens') / 'config-files.json'
CASES = ('plain', 'unicode', 'group-mode', 'symlink', 'symlink-chain', 'symlink-unicode')


def capture(name):
    config = {'synthetic_extension': {'items': ['synthetic', 7, None, {}, []]},
              'harness_learning': {'enabled': False, 'unknown': 'retained'}}
    if name in ('unicode', 'symlink-unicode'):
        config['synthetic_extension']['\u00e9'] = '\u03bb \U0001f47e \u007f literal \\u00e9'
    with Daemon(engine_free=True, config_changes=config) as daemon:
        link = daemon.config_path
        target = link
        symlink = name.startswith('symlink')
        if symlink:
            target = link.with_name('actual-config.json')
            link.rename(target)
            if name == 'symlink-chain':
                intermediate = link.with_name('config-link.json')
                intermediate.symlink_to(target.name)
                link.symlink_to(intermediate.name)
            else:
                link.symlink_to(target.name)
        mode = 0o640 if name == 'group-mode' or symlink else 0o600
        target.chmod(mode)
        old_file = target.stat()
        old_link = link.lstat()
        reply = daemon.call('POST', '/_local/harness/learning', {'enabled': True, 'min_requests': 3})
        if reply['status'] != 200:
            raise AssertionError(reply)
        result = target.read_bytes()
        expected = {**config, 'harness_learning': {**config['harness_learning'], 'enabled': True, 'min_requests': 3}}
        if json.loads(result) != expected:
            raise AssertionError('Config mutation dropped or changed unrelated fields')
        if stat.S_IMODE(target.stat().st_mode) != mode:
            raise AssertionError('Config replacement changed the owner-selected permission bits')
        if target.stat().st_ino == old_file.st_ino:
            raise AssertionError('Config was not atomically replaced')
        if symlink and (not link.is_symlink() or link.lstat().st_ino != old_link.st_ino):
            raise AssertionError('Config writer replaced the symlink')
        if list(link.parent.glob('.*.tmp')):
            raise AssertionError('Config writer left a temporary file')
        status = daemon.call('GET', '/_local/status')['body']
        if status['loaded_model'] is not None or status['active_requests'] or daemon.events():
            raise AssertionError('Config mutation started local inference')
        return {'reply': reply, 'contents': result.decode(), 'mode': mode, 'symlink': symlink}


class ConfigFileContract(unittest.TestCase):
    def test_config_format_and_file_identity_without_an_engine(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(capture(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Record goldens only from the Python reference')
        GOLDENS.write_text(json.dumps({name: capture(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
