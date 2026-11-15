"""CLI file readers: private ledger fixtures, bounds and symlink refusal."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest

from environment import ROOT, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'cli-files.json'
CASES = ('empty-directory', 'populated', 'limit-clamped', 'symlink-refused', 'bounded-tail')


def invoke(name):
    with tempfile.TemporaryDirectory(prefix='oxc-cli-files-') as directory:
        home = Path(directory)
        env = isolated_env(home)
        ledger = home / '.local/state/local-ai/ledger'
        ledger.mkdir(parents=True, mode=0o700)
        path = ledger / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')
        row = {'path': '/v1/chat/completions', 'model': 'ornith-35b-a3b-local',
               'harness': '../invalid', 'client': 'synthetic/1', 't': 1.0, 'status': 200,
               'origin': 'organic', 'mode': 'fast', 'stream': False, 'cache_n': 2,
               'prompt_n': 6, 'predicted_n': 3, 'wall_ms': 12.5, 'finish_reason': 'stop',
               'prompt': 'Synthetic private prompt.', 'reply': 'Synthetic private reply.',
               'cache_src': 'private-text-is-not-an-enum', 'resume_src': 'disk'}
        if name == 'symlink-refused':
            target = home / 'owner-file'
            target.write_text(json.dumps(row) + '\n')
            path.symlink_to(target)
        elif name != 'empty-directory':
            prefix = 'synthetic junk' * 50_000 + '\n' if name == 'bounded-tail' else ''
            irrelevant = {'path': '/v1/responses', 'prompt': 'Synthetic ignored value.'}
            path.write_text(prefix + json.dumps(irrelevant) + '\n{invalid\n' + json.dumps(row) + '\n' +
                            json.dumps({**row, 't': 2.0, 'model': 'not-allowed', 'wall_ms': -1}) + '\n')
            path.chmod(0o600)
        executable = home / 'oxc'
        executable.symlink_to(ROOT / 'lab/src/snapdragon_ai_lab/runtime/local-ai')
        command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [sys.executable, str(executable)]
        args = ['ledger', '--json'] + (['--limit', '0'] if name == 'limit-clamped' else [])
        result = subprocess.run(command + args, env=env, cwd=home, capture_output=True, text=True, timeout=5)
        if result.returncode != 0:
            raise AssertionError(result.stderr)
        if any(value in result.stdout for value in ('Synthetic private prompt.', 'Synthetic private reply.',
                                                    'private-text-is-not-an-enum', '../invalid')):
            raise AssertionError('CLI ledger reader leaked a private/non-allowlisted field')
        return {'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}


class CliFileContract(unittest.TestCase):
    def test_file_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
