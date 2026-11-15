"""Black-box CLI contracts. No controller modules are imported by this suite."""
from environment import PYTHON
import json
import os
import re
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GOLDENS = Path(__file__).with_name('goldens') / 'cli.json'
ACTIONS = (
    'panel', 'status', 'ledger', 'storage', 'chat', 'code', 'plan', 'images',
    'load', 'unload', 'moe', 'ornith', 'flash', 'clear-cache', 'storage-delete',
    'harness-prepare', 'harness-forget', 'flash-cache', 'cache', 'mode', 'share',
    'digest', 'update', 'rollback', 'uninstall',
)
CASES = {'help': ['--help'], 'unknown-action': ['not-an-action'],
         'version': ['--version'], 'version-abbreviation': ['--ver'],
         'version-after-action': ['status', '--version'],
         'version-before-unknown': ['--version', 'not-an-action'],
         'version-after-unknown': ['not-an-action', '--version'],
         'version-explicit-error': ['--version=value'],
         'uninstall-version': ['uninstall', '--version'],
         'retired-version': ['dense', '--version'],
         'positional-version': ['--', '--version'],
         'unknown-option': ['--not-an-option'],
         'invalid-limit': ['ledger', '--limit', 'abc'],
         'invalid-mode': ['mode', '--mode', 'invalid'],
         'invalid-pref': ['flash-cache', '--pref', 'invalid']}
for action in ACTIONS:
    CASES[action + '-help'] = [action, '--help']
    CASES[action + '-unknown-option'] = [action, '--not-an-option']


class CliContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.expected = json.loads(GOLDENS.read_text())

    def test_cli_goldens(self):
        self.assertEqual(set(self.expected), set(CASES))
        for name, args in CASES.items():
            with self.subTest(case=name):
                self.assertEqual(invoke(args), self.expected[name])


def normalize_usage(text):
    """Keep argparse's complete usage grammar, independent of version wrapping."""
    return re.sub(r'^usage: ([^\n]*)(?:\n[ \t]+[^\n]+)*',
                  lambda match: 'usage: ' + ' '.join(match.group().split()[1:]), text,
                  flags=re.MULTILINE)


def invoke(args):
    with tempfile.TemporaryDirectory(prefix='oxc-conformance-') as directory:
        home = Path(directory)
        # Fix argv[0] to the public executable name for argparse output. Using a
        # link also exercises Python's installed entry script without importing it.
        executable = home / 'oxc'
        executable.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
        command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [PYTHON, str(executable)]
        # Do not inherit credentials, proxy configuration, XDG paths or controller
        # overrides. These cases terminate in the parser before any HTTP request.
        env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
               'LC_ALL': 'C.UTF-8', 'TERM': 'dumb', 'COLUMNS': '80',
               'PYTHONPATH': str(ROOT / 'oxc/src')}
        for key, subdir in (('XDG_CONFIG_HOME', 'config'), ('XDG_STATE_HOME', 'state'),
                            ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')):
            env[key] = str(home / subdir)
        result = subprocess.run(command + args, env=env, cwd=home,
                                capture_output=True, timeout=10)
        if set(home.iterdir()) != {executable}:
            raise AssertionError('Parser-only invocation wrote installation state')
        return {'exit': result.returncode, 'stdout': normalize_usage(result.stdout.decode()),
                'stderr': normalize_usage(result.stderr.decode())}


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke(args) for name, args in CASES.items()},
                                     indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
