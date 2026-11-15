"""Host-only share-key lifecycle, shown once under a real cross-process lock."""
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import stat
import subprocess
import sys
import time
import unittest

from harness import ROOT, isolated_env
from test_cli_runtime import CANONICAL_HOME, invoke_case

GOLDENS = Path(__file__).with_name('goldens') / 'share-keys.json'
CASES = ('fresh-repeat-rotate', 'json-flag', 'existing-unshown', 'existing-shown', 'invalid-json',
         'invalid-key', 'public-key', 'public-rotate', 'symlink', 'symlink-rotate', 'lock-symlink',
         'existing-parent', 'override', 'relative-override', 'concurrent', 'roundtrip', 'mixed-concurrent')


def capture(name):
    home = CANONICAL_HOME
    env = isolated_env(home)
    env['LOCAL_LLM_CONFIG'] = str(home / 'config/oxc/llm/runtime.json')
    key = home / 'config/oxc/lan-share-key.json'
    if name == 'override':
        key = home / 'synthetic keys/nested/key.json'
        env['OXC_SHARE_KEY_FILE'] = str(key)
    elif name == 'relative-override':
        key = home / 'relative-key.json'
        env['OXC_SHARE_KEY_FILE'] = '  relative-key.json  '
    seeded = name in ('existing-unshown', 'existing-shown', 'invalid-json', 'invalid-key', 'public-key',
                       'public-rotate', 'symlink', 'symlink-rotate')
    if seeded or name in ('lock-symlink', 'existing-parent'):
        key.parent.mkdir(parents=True, mode=0o700)
    if name == 'existing-parent':
        key.parent.chmod(0o755)
    tokens = []

    def identity(value):
        if not isinstance(value, str) or not re.fullmatch(r'lai-[A-Za-z0-9_-]{43}', value):
            raise AssertionError('Invalid generated share key')
        if len(base64.urlsafe_b64decode(value[4:] + '=')) != 32:
            raise AssertionError('Share key lacks 256 bits')
        if value not in tokens:
            tokens.append(value)
        return '<validated:key-' + str(tokens.index(value)) + '>'

    if seeded:
        value = 'lai-' + base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip('=')
        identity(value)
        record = {'key': 'invalid' if name == 'invalid-key' else value, 'created': 1,
                  'shown': name == 'existing-shown', 'unknown': 'synthetic metadata'}
        key.write_text('invalid JSON' if name == 'invalid-json' else json.dumps(record))
        key.chmod(0o644 if name.startswith('public') else 0o600)
        if name.startswith('symlink'):
            key.rename(home / 'key-target')
            key.symlink_to(home / 'key-target')
    if name == 'lock-symlink':
        (home / 'lock-target').write_text('synthetic untouched lock')
        key.with_name('.' + key.name + '.lock').symlink_to(home / 'lock-target')
    entry = home / 'oxc'
    entry.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
    reference = [sys.executable, str(entry)]
    command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else reference

    def snapshot():
        if not key.exists():
            return None
        try:
            record = json.loads(key.read_text())
        except ValueError:
            record = None
        if isinstance(record, dict) and isinstance(record.get('key'), str) and record['key'].startswith('lai-'):
            record['key'] = identity(record['key'])
            created = record.get('created')
            if created != 1:
                if type(created) is not int or abs(created - time.time()) > 30:
                    raise AssertionError('Invalid key creation time')
                record['created'] = '<validated:created>'
        return {'mode': stat.S_IMODE(key.stat().st_mode), 'symlink': key.is_symlink(), 'record': record,
                'parent_mode': stat.S_IMODE(key.parent.stat().st_mode),
                'lock_symlink': key.with_name('.' + key.name + '.lock').is_symlink()}

    def invoke(words, use_reference=False):
        return subprocess.run([*(reference if use_reference else command), 'share', 'key', *words], cwd=home, env=env,
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=10, umask=0o027)

    def normalize(result):
        disk = snapshot()
        stdout, stderr = result.stdout.decode(), result.stderr.decode()
        if stdout:
            value = stdout.rstrip('\n')
            label = identity(value)
            if not isinstance(disk, dict) or disk['record']['key'] != label or disk['record']['shown'] is not True:
                raise AssertionError('Printed key differs from the shown key on disk')
            stdout = label + '\n'
        for token in tokens:
            fingerprint = 'sha256:' + hashlib.sha256(token.encode()).hexdigest()[:12]
            stderr = stderr.replace(fingerprint, '<fingerprint:' + identity(token) + '>')
        return {'exit': result.returncode, 'stdout': stdout, 'stderr': stderr.replace(str(home), '<HOME>'), 'disk': disk}

    if name in ('concurrent', 'mixed-concurrent'):
        with ThreadPoolExecutor(max_workers=2) as executor:
            tasks = [executor.submit(invoke, [], False), executor.submit(invoke, [], name == 'mixed-concurrent')]
            results = [task.result() for task in tasks]
        results.sort(key=lambda result: result.returncode)
        output = [normalize(result) for result in results]
        if [row['exit'] for row in output] != [0, 1]:
            raise AssertionError('Concurrent key reveals did not show exactly once')
    elif name == 'roundtrip':
        output = [normalize(invoke(words, use_reference)) for use_reference, words in (
            (True, []), (False, ['--rotate']), (True, []), (True, ['--rotate']), (False, []))]
    else:
        calls = [[], [], ['--rotate'], []] if name == 'fresh-repeat-rotate' else [
            ['--json'] if name == 'json-flag' else ['--rotate'] if name.endswith('rotate') else []]
        output = [normalize(invoke(words)) for words in calls]
    for target in ('key-target', 'lock-target'):
        path = home / target
        if path.exists():
            value = path.read_text()
            if target == 'key-target':
                value = json.loads(value)
                value['key'] = identity(value['key'])
            output.append({'unchanged_target': target, 'value': value})
    return output


class ShareKeysContract(unittest.TestCase):
    def test_share_key_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
